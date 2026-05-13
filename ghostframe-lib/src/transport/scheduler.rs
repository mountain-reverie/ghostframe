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

/// Returned by `Scheduler::on_ack` and `Scheduler::bump_generation_collecting`
/// when a TileWork is resolved (ACKed or superseded). Carries the bare minimum
/// the IoBridge needs to update palette-table state without retaining payload
/// ownership.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedTileWork {
    pub tile_x: u8,
    pub tile_y: u8,
    pub generation: u8,
    pub pass: u8,
    pub codec: crate::transport::protocol::Codec,
    /// For `Codec::PalRle`, the persistent palette id (payload byte [1]).
    /// For other codecs, `None`.
    pub palette_id: Option<u8>,
    /// Whether the resolved work was an ACK (true) or a supersession (false).
    pub via_ack: bool,
}

#[derive(Debug, Clone)]
pub struct TileWork {
    pub tile_x: u8,
    pub tile_y: u8,
    pub generation: u8, // 4 bits effective
    pub pass_idx: u8,   // 4 bits effective; 0 for single-pass codecs
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

    /// Drain all pending and in-flight work. Generations are preserved so
    /// any late ACKs from the prior session still no-op cleanly (their
    /// matching work is gone). Called on session reconnect to prevent stale
    /// work from polluting the new client's first tick.
    pub fn clear(&mut self) {
        self.queue.clear();
    }

    pub fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
    }

    pub fn cols(&self) -> u32 {
        self.cols
    }
    pub fn rows(&self) -> u32 {
        self.rows
    }
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

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
        self.queue
            .iter()
            .map(|w| (w.tile_x, w.tile_y, w.state))
            .collect()
    }

    /// Match (tile_x, tile_y, generation, pass) against in-flight queue entries
    /// and mark each match as `Acked`. Returns the resolved entries (codec,
    /// palette_id, etc.) so callers can update downstream state.
    ///
    /// Only entries with `WorkState::InFlight` are eligible. Entries in other
    /// states (`Pending`, `Acked`, `Superseded`) are ignored. Stale-generation
    /// or stale-pass ACKs match no entry and produce an empty vector.
    #[must_use = "ResolvedTileWork is needed by IoBridge to update palette delivery state"]
    pub fn on_ack(&mut self, tile_x: u8, tile_y: u8, generation: u8, pass: u8) -> Vec<ResolvedTileWork> {
        let mut resolved: Vec<ResolvedTileWork> = Vec::new();
        for entry in self.queue.iter_mut() {
            if entry.tile_x == tile_x
                && entry.tile_y == tile_y
                && entry.generation == generation
                && entry.pass_idx == pass
                && entry.state == WorkState::InFlight
            {
                entry.state = WorkState::Acked;
                let palette_id = if entry.codec == Codec::PalRle {
                    debug_assert!(
                        entry.payload.len() >= 2,
                        "PalRle TileWork has malformed payload: {} bytes",
                        entry.payload.len()
                    );
                    entry.payload.get(1).copied()
                } else {
                    None
                };
                resolved.push(ResolvedTileWork {
                    tile_x,
                    tile_y,
                    generation,
                    pass,
                    codec: entry.codec,
                    palette_id,
                    via_ack: true,
                });
            }
        }
        self.queue.retain(|w| w.state != WorkState::Acked);
        resolved
    }

    /// Like `bump_generation` but returns the work that was superseded so
    /// callers can update downstream state (e.g. `PaletteTable::in_flight_carrying`).
    ///
    /// Safe to call `bump_generation` internally after pre-marking entries
    /// `Superseded`: `bump_generation`'s loop skips entries already in
    /// `Superseded` or `Acked` state. If that guard ever changes, this
    /// function's safety needs re-verification.
    #[must_use = "ResolvedTileWork is needed by IoBridge to update palette delivery state on supersession"]
    pub fn bump_generation_collecting(&mut self, tile_x: u8, tile_y: u8)
        -> (u8, Vec<ResolvedTileWork>)
    {
        let mut resolved: Vec<ResolvedTileWork> = Vec::new();
        for entry in self.queue.iter_mut() {
            if entry.tile_x == tile_x
                && entry.tile_y == tile_y
                && entry.state != WorkState::Acked
                && entry.state != WorkState::Superseded
            {
                let palette_id = if entry.codec == Codec::PalRle {
                    debug_assert!(
                        entry.payload.len() >= 2,
                        "PalRle TileWork has malformed payload: {} bytes",
                        entry.payload.len()
                    );
                    entry.payload.get(1).copied()
                } else {
                    None
                };
                resolved.push(ResolvedTileWork {
                    tile_x,
                    tile_y,
                    generation: entry.generation,
                    pass: entry.pass_idx,
                    codec: entry.codec,
                    palette_id,
                    via_ack: false,
                });
                entry.state = WorkState::Superseded;
            }
        }
        let new_gen = self.bump_generation(tile_x, tile_y);
        self.queue.retain(|w| w.state != WorkState::Superseded);
        (new_gen, resolved)
    }

    /// Drain the queue per the M3.1 single-pass scheduling rule:
    ///
    /// - Drop `Superseded` and `Acked` entries (terminal states).
    /// - Return `Pending` items, and `InFlight` items past their `2 × rtt`
    ///   retry threshold. Returned items are promoted to `InFlight` with
    ///   `last_sent_at = now`.
    ///
    /// `budget_bytes` is a **soft cap**: the first tile whose cumulative
    /// payload size crosses `budget_bytes` is still returned; the loop
    /// stops before the next eligible tile. Pass `usize::MAX` to disable
    /// budgeting (the M3.1 call site does this; M3.3 will pass real values
    /// once refinement competes with fresh tiles for bandwidth).
    pub fn tick(&mut self, budget_bytes: usize) -> Vec<TileWork> {
        let now = Instant::now();
        let retry_after = 2 * self.rtt;

        // First pass: drop terminal-state entries.
        self.queue
            .retain(|w| !matches!(w.state, WorkState::Superseded | WorkState::Acked));

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
        assert!(states
            .iter()
            .any(|(x, y, st)| *x == 1 && *y == 2 && *st == WorkState::Superseded));
        assert!(states
            .iter()
            .any(|(x, y, st)| *x == 3 && *y == 0 && *st == WorkState::Pending));
    }

    #[test]
    fn bump_generation_supersedes_regardless_of_work_generation() {
        let mut s = Scheduler::new(4, 4);
        s.enqueue(TileWork::raw_for_test(1, 2, 3, vec![1]));
        // The queued work is at gen=3; bumping any gen on that tile invalidates it.
        s.bump_generation(1, 2);
        let states = s.queue_states_for_test();
        assert!(states
            .iter()
            .any(|(x, y, st)| *x == 1 && *y == 2 && *st == WorkState::Superseded));
    }

    #[test]
    fn enqueue_normalizes_state_to_pending_and_refreshes_queued_at() {
        let mut s = Scheduler::new(4, 4);
        let stale_instant = Instant::now() - Duration::from_secs(10);
        let before_enqueue = Instant::now();
        s.enqueue(TileWork {
            tile_x: 1,
            tile_y: 2,
            generation: 0,
            pass_idx: 0,
            total_passes: 1,
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
        assert!(
            w.queued_at >= before_enqueue,
            "queued_at must be refreshed by enqueue"
        );
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
        s.set_rtt(Duration::from_millis(5));
        s.enqueue(TileWork::raw_for_test(0, 0, 0, vec![1]));
        let first = s.tick(usize::MAX);
        assert_eq!(first.len(), 1);

        // Immediately ticking again — within 2×RTT, no retry.
        let second = s.tick(usize::MAX);
        assert!(second.is_empty());

        std::thread::sleep(Duration::from_millis(15));
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
        assert_eq!(
            out.len(),
            2,
            "budget allows whole tiles up to and including the one that crosses the cap"
        );
        assert_eq!(
            s.queue_len(),
            3,
            "all three are still queued; tick doesn't drop InFlight until ACK or supersede"
        );
    }

    #[test]
    fn on_ack_clears_matching_inflight_work() {
        let mut s = Scheduler::new(4, 4);
        s.enqueue(TileWork::raw_for_test(1, 2, 0, vec![1]));
        let _ = s.tick(usize::MAX); // promote to InFlight
        let _ = s.on_ack(1, 2, 0, 0);
        // Next tick should drop it.
        let out = s.tick(usize::MAX);
        assert!(out.is_empty());
        assert_eq!(s.queue_len(), 0);
    }

    #[test]
    fn on_ack_with_wrong_gen_is_a_noop() {
        let mut s = Scheduler::new(4, 4);
        s.set_rtt(Duration::from_millis(50)); // long RTT so next tick won't retry
        s.enqueue(TileWork::raw_for_test(1, 2, 5, vec![1]));
        let _ = s.tick(usize::MAX);
        let _ = s.on_ack(1, 2, 4, 0); // wrong gen
        let next = s.tick(usize::MAX); // not yet 2×RTT — empty
        assert!(next.is_empty());
        // The work is still queued.
        assert_eq!(s.queue_len(), 1);
    }

    #[test]
    fn on_ack_unknown_tile_does_not_panic() {
        let mut s = Scheduler::new(4, 4);
        let _ = s.on_ack(7, 7, 0, 0);
        assert_eq!(s.queue_len(), 0);
    }

    #[test]
    fn clear_drains_queue_but_preserves_generations() {
        let mut s = Scheduler::new(4, 4);
        s.bump_generation(1, 2); // gen = 1
        s.bump_generation(1, 2); // gen = 2
        s.enqueue(TileWork::raw_for_test(1, 2, 2, vec![1, 2]));
        s.enqueue(TileWork::raw_for_test(0, 0, 0, vec![3, 4]));
        assert_eq!(s.queue_len(), 2);
        s.clear();
        assert_eq!(s.queue_len(), 0);
        // Generations survive so stale-gen ACKs from prior session are still
        // distinguishable from current-gen work in any new tile we enqueue.
        assert_eq!(s.generation_for(1, 2), 2);
        // A stale ACK against the cleared work is a noop (no matching entry).
        let _ = s.on_ack(1, 2, 2, 0);
        assert_eq!(s.queue_len(), 0);
    }

    #[test]
    fn on_ack_does_not_touch_superseded_work() {
        let mut s = Scheduler::new(4, 4);
        s.enqueue(TileWork::raw_for_test(0, 0, 0, vec![1]));
        s.bump_generation(0, 0); // marks Superseded
        let _ = s.on_ack(0, 0, 0, 0); // would match by tile_x/y/gen/pass — but work is Superseded
                              // The work stays Superseded (not promoted to Acked). Tick still drops it
                              // via the Superseded retain path.
        let out = s.tick(usize::MAX);
        assert!(out.is_empty());
        assert_eq!(s.queue_len(), 0);
    }

    #[test]
    fn on_ack_returns_resolved_work_for_palrle_tile() {
        use crate::transport::protocol::Codec;
        let mut s = Scheduler::new(2, 2);
        s.enqueue(TileWork {
            tile_x: 0, tile_y: 0,
            generation: 0, pass_idx: 0, total_passes: 1,
            codec: Codec::PalRle,
            payload: vec![0x01u8, 7, 0, 0, 0, 0],
            queued_at: std::time::Instant::now(),
            last_sent_at: None,
            state: WorkState::Pending,
        });
        let _ = s.tick(usize::MAX);

        let resolved = s.on_ack(0, 0, 0, 0);
        assert_eq!(resolved.len(), 1);
        let r = resolved[0];
        assert_eq!(r.tile_x, 0);
        assert_eq!(r.tile_y, 0);
        assert_eq!(r.codec, Codec::PalRle);
        assert_eq!(r.palette_id, Some(7));
        assert!(r.via_ack);
    }

    #[test]
    fn on_ack_for_solid_tile_has_none_palette_id() {
        use crate::transport::protocol::Codec;
        let mut s = Scheduler::new(2, 2);
        s.enqueue(TileWork {
            tile_x: 0, tile_y: 1,
            generation: 0, pass_idx: 0, total_passes: 1,
            codec: Codec::Solid,
            payload: vec![10, 20, 30, 255],
            queued_at: std::time::Instant::now(),
            last_sent_at: None,
            state: WorkState::Pending,
        });
        let _ = s.tick(usize::MAX);
        let resolved = s.on_ack(0, 1, 0, 0);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].codec, Codec::Solid);
        assert_eq!(resolved[0].palette_id, None);
        assert!(resolved[0].via_ack);
    }

    #[test]
    fn bump_generation_collecting_returns_superseded_palrle_work() {
        use crate::transport::protocol::Codec;
        let mut s = Scheduler::new(2, 2);
        s.enqueue(TileWork {
            tile_x: 1, tile_y: 0,
            generation: 0, pass_idx: 0, total_passes: 1,
            codec: Codec::PalRle,
            payload: vec![0x01u8, 9, 0, 0, 0, 0],
            queued_at: std::time::Instant::now(),
            last_sent_at: None,
            state: WorkState::Pending,
        });
        let _ = s.tick(usize::MAX);
        let (new_gen, resolved) = s.bump_generation_collecting(1, 0);
        assert_eq!(new_gen, 1); // generation incremented
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].codec, Codec::PalRle);
        assert_eq!(resolved[0].palette_id, Some(9));
        assert!(!resolved[0].via_ack);
    }
}
