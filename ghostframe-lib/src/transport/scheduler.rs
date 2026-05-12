//! Round-robin tile-work scheduler with batched-ACK driven retry.
//!
//! Per M3.1 umbrella, this is the single point of dispatch for every tile-codec
//! emission. M3.1 ships single-pass codecs only; the `pass_idx`/`total_passes`
//! fields are reserved for M3.3 progressive refinement.

use crate::transport::protocol::Codec;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    Pending,
    InFlight,
    Acked,
    Superseded,
}

#[derive(Debug, Clone)]
pub struct TileWork {
    pub tile_x: u8,
    pub tile_y: u8,
    pub generation: u8,    // 4 bits effective
    pub pass_idx: u8,      // 4 bits effective; 0 for single-pass codecs
    pub total_passes: u8,
    pub codec: Codec,
    pub payload: Vec<u8>,
    pub queued_at: Instant,
    pub last_sent_at: Option<Instant>,
    pub state: WorkState,
}

impl TileWork {
    /// Test-only constructor that fills timing fields with `Instant::now()`.
    #[cfg(test)]
    pub fn raw_for_test(tile_x: u8, tile_y: u8, generation: u8, payload: Vec<u8>) -> Self {
        Self {
            tile_x,
            tile_y,
            generation,
            pass_idx: 0,
            total_passes: 1,
            codec: Codec::Raw,
            payload,
            queued_at: Instant::now(),
            last_sent_at: None,
            state: WorkState::Pending,
        }
    }
}

pub struct Scheduler {
    cols: u32,
    rows: u32,
    /// Per-tile generation counter, indexed `y * cols + x`. 4-bit wrap.
    generations: Vec<u8>,
    /// FIFO of pending/in-flight tile work. M3.1 is single-pass so FIFO is
    /// equivalent to round-robin; M3.3 will revisit this when multi-pass
    /// refinement starts mixing fresh work with refinement passes.
    queue: VecDeque<TileWork>,
    /// QUIC RTT estimate used to drive 2×RTT retry. Updated by `set_rtt`.
    rtt: Duration,
    /// Reserved for M3.3 refinement bandwidth partitioning. Unused in M3.1.
    #[allow(dead_code)]
    refinement_fraction: f32,
}

impl Scheduler {
    pub fn new(cols: u32, rows: u32) -> Self {
        Self {
            cols,
            rows,
            generations: vec![0; (cols as usize) * (rows as usize)],
            queue: VecDeque::new(),
            rtt: Duration::from_millis(20),
            refinement_fraction: 0.2,
        }
    }

    pub fn resize(&mut self, cols: u32, rows: u32) {
        self.cols = cols;
        self.rows = rows;
        self.generations = vec![0; (cols as usize) * (rows as usize)];
        self.queue.clear();
    }

    pub fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
    }

    pub fn cols(&self) -> u32 { self.cols }
    pub fn rows(&self) -> u32 { self.rows }
    pub fn queue_len(&self) -> usize { self.queue.len() }

    pub fn generation_for(&self, tile_x: u8, tile_y: u8) -> u8 {
        let idx = (tile_y as usize) * (self.cols as usize) + (tile_x as usize);
        self.generations.get(idx).copied().unwrap_or(0)
    }

    pub fn enqueue(&mut self, mut work: TileWork) {
        debug_assert!((work.tile_x as u32) < self.cols, "tile_x out of bounds");
        debug_assert!((work.tile_y as u32) < self.rows, "tile_y out of bounds");
        work.queued_at = Instant::now();
        work.last_sent_at = None;
        work.state = WorkState::Pending;
        self.queue.push_back(work);
    }

    #[cfg(test)]
    pub fn peek_for_test(&self) -> Vec<TileWork> {
        self.queue.iter().cloned().collect()
    }

    pub fn bump_generation(&mut self, tile_x: u8, tile_y: u8) -> u8 {
        let idx = (tile_y as usize) * (self.cols as usize) + (tile_x as usize);
        let new_gen = (self.generations[idx] + 1) & 0x0F;
        self.generations[idx] = new_gen;
        // Any queued work for this tile is now stale; mark Superseded so the
        // next tick drops it. Acked entries don't matter (already done).
        for work in self.queue.iter_mut() {
            if work.tile_x == tile_x
                && work.tile_y == tile_y
                && !matches!(work.state, WorkState::Superseded | WorkState::Acked)
            {
                work.state = WorkState::Superseded;
            }
        }
        new_gen
    }

    #[cfg(test)]
    pub fn queue_states_for_test(&self) -> Vec<(u8, u8, WorkState)> {
        self.queue.iter().map(|w| (w.tile_x, w.tile_y, w.state)).collect()
    }

    /// Drain the queue per the M3.1 single-pass scheduling rule:
    /// - Drop `Superseded` and `Acked` entries.
    /// - Return `Pending` and `InFlight`-past-retry items, up to `budget_bytes`.
    /// Returned items are promoted to `InFlight` with `last_sent_at = now`.
    pub fn tick(&mut self, budget_bytes: usize) -> Vec<TileWork> {
        let now = Instant::now();
        let retry_after = 2 * self.rtt;

        // First pass: drop terminal-state entries.
        self.queue.retain(|w| !matches!(w.state, WorkState::Superseded | WorkState::Acked));

        let mut out = Vec::new();
        let mut spent = 0usize;
        for work in self.queue.iter_mut() {
            let eligible = match work.state {
                WorkState::Pending => true,
                WorkState::InFlight => work
                    .last_sent_at
                    .map(|t| now.duration_since(t) >= retry_after)
                    .unwrap_or(true),
                _ => false,
            };
            if !eligible {
                continue;
            }
            if spent >= budget_bytes {
                break;
            }
            work.state = WorkState::InFlight;
            work.last_sent_at = Some(now);
            spent = spent.saturating_add(work.payload.len());
            out.push(work.clone());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sized_scheduler_has_empty_queue_and_zeroed_generations() {
        let s = Scheduler::new(5, 4);
        assert_eq!(s.queue_len(), 0);
        assert_eq!(s.generation_for(2, 1), 0);
        assert_eq!(s.cols(), 5);
        assert_eq!(s.rows(), 4);
    }

    #[test]
    fn resize_clears_state() {
        let mut s = Scheduler::new(2, 2);
        s.enqueue(TileWork::raw_for_test(0, 0, 0, vec![1, 2, 3]));
        assert_eq!(s.queue_len(), 1);
        s.resize(8, 6);
        assert_eq!(s.queue_len(), 0);
        assert_eq!(s.generation_for(7, 5), 0);
    }

    #[test]
    fn bump_generation_increments_and_wraps_at_16() {
        let mut s = Scheduler::new(4, 4);
        assert_eq!(s.bump_generation(1, 2), 1);
        assert_eq!(s.bump_generation(1, 2), 2);
        for _ in 0..14 {
            s.bump_generation(1, 2);
        }
        // Total of 16 bumps from 0 → wraps back to 0.
        assert_eq!(s.generation_for(1, 2), 0);
    }

    #[test]
    fn bump_generation_supersedes_matching_in_flight_work() {
        let mut s = Scheduler::new(4, 4);
        s.enqueue(TileWork::raw_for_test(1, 2, 0, vec![1, 2, 3]));
        s.enqueue(TileWork::raw_for_test(3, 0, 0, vec![4, 5, 6]));
        s.bump_generation(1, 2);
        // (1,2) work superseded; (3,0) work untouched.
        let states = s.queue_states_for_test();
        assert!(states.iter().any(|(x, y, st)| *x == 1 && *y == 2 && *st == WorkState::Superseded));
        assert!(states.iter().any(|(x, y, st)| *x == 3 && *y == 0 && *st == WorkState::Pending));
    }

    #[test]
    fn bump_generation_supersedes_regardless_of_work_generation() {
        let mut s = Scheduler::new(4, 4);
        s.enqueue(TileWork::raw_for_test(1, 2, 3, vec![1]));
        // The queued work is at gen=3; bumping any gen on that tile invalidates it.
        s.bump_generation(1, 2);
        let states = s.queue_states_for_test();
        assert!(states.iter().any(|(x, y, st)| *x == 1 && *y == 2 && *st == WorkState::Superseded));
    }

    #[test]
    fn enqueue_normalizes_state_to_pending_and_refreshes_queued_at() {
        let mut s = Scheduler::new(4, 4);
        let stale_instant = Instant::now() - Duration::from_secs(10);
        let before_enqueue = Instant::now();
        s.enqueue(TileWork {
            tile_x: 1, tile_y: 2,
            generation: 0, pass_idx: 0, total_passes: 1,
            codec: Codec::Raw,
            payload: vec![1, 2, 3],
            queued_at: stale_instant,
            last_sent_at: Some(stale_instant),
            state: WorkState::Acked, // wrong state — enqueue must normalize
        });
        let queued = s.peek_for_test();
        assert_eq!(queued.len(), 1);
        let w = &queued[0];
        assert_eq!(w.state, WorkState::Pending);
        assert!(w.last_sent_at.is_none());
        assert!(w.queued_at >= before_enqueue, "queued_at must be refreshed by enqueue");
    }

    #[test]
    fn tick_returns_pending_and_promotes_to_inflight() {
        let mut s = Scheduler::new(4, 4);
        s.enqueue(TileWork::raw_for_test(0, 0, 0, vec![1, 2, 3]));
        s.enqueue(TileWork::raw_for_test(1, 0, 0, vec![4, 5, 6]));
        let out = s.tick(usize::MAX);
        assert_eq!(out.len(), 2);
        for w in &s.peek_for_test() {
            assert_eq!(w.state, WorkState::InFlight);
            assert!(w.last_sent_at.is_some());
        }
    }

    #[test]
    fn tick_drops_superseded_entries() {
        let mut s = Scheduler::new(4, 4);
        s.enqueue(TileWork::raw_for_test(0, 0, 0, vec![1]));
        s.bump_generation(0, 0); // marks the work Superseded
        let out = s.tick(usize::MAX);
        assert!(out.is_empty());
        assert_eq!(s.queue_len(), 0);
    }

    #[test]
    fn tick_retries_inflight_after_2x_rtt() {
        let mut s = Scheduler::new(4, 4);
        s.set_rtt(Duration::from_millis(1));
        s.enqueue(TileWork::raw_for_test(0, 0, 0, vec![1]));
        let first = s.tick(usize::MAX);
        assert_eq!(first.len(), 1);

        // Immediately ticking again — within 2×RTT, no retry.
        let second = s.tick(usize::MAX);
        assert!(second.is_empty());

        std::thread::sleep(Duration::from_millis(3));
        let third = s.tick(usize::MAX);
        assert_eq!(third.len(), 1, "InFlight work should retry after 2×RTT");
    }

    #[test]
    fn tick_respects_budget_bytes() {
        let mut s = Scheduler::new(4, 4);
        s.enqueue(TileWork::raw_for_test(0, 0, 0, vec![0; 100]));
        s.enqueue(TileWork::raw_for_test(1, 0, 0, vec![0; 100]));
        s.enqueue(TileWork::raw_for_test(2, 0, 0, vec![0; 100]));
        let out = s.tick(150);
        // First (100) fits; second (cumulative 200) crosses 150 but is returned
        // as a whole tile (we allow up to and including the tile that crosses the cap).
        // Third never gets eligibility because the budget check breaks the loop.
        assert_eq!(out.len(), 2, "budget allows whole tiles up to and including the one that crosses the cap");
        assert_eq!(s.queue_len(), 3, "all three are still queued; tick doesn't drop InFlight until ACK or supersede");
    }
}
