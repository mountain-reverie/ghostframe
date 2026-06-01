//! Round-robin tile-work scheduler with batched-ACK driven retry.
//!
//! Per M3.1 umbrella, this is the single point of dispatch for every tile-codec
//! emission. M3.1 ships single-pass codecs only; the `pass_idx`/`total_passes`
//! fields are reserved for M3.3 progressive refinement.

use crate::transport::protocol::Codec;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    Pending,
    InFlight,
    Acked,
    Superseded,
}

/// Returned by `Scheduler::bump_generation_collecting` when a TileWork is
/// superseded. Carries the bare minimum the IoBridge needs to update
/// palette-table state without retaining payload ownership.
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
    /// FIFO of pending/in-flight priority tile work (single-pass codec emissions).
    /// Renamed from `queue` in M3.3a to distinguish from `refinement_queue`.
    priority_queue: VecDeque<TileWork>,
    /// FIFO of multi-pass Cdf53 refinement work. Drained in pass-major order
    /// by `drain_refinement_pass_major`.
    refinement_queue: VecDeque<TileWork>,
    /// QUIC RTT estimate used to drive 2×RTT retry. Updated by `set_rtt`.
    rtt: Duration,
    /// Fraction of tick budget allocated to refinement passes (default 0.2).
    /// Adjusted adaptively by `maybe_adjust_refinement_fraction`.
    refinement_bandwidth_fraction: f32,
    /// Count of work items emitted in the current delivery window.
    delivery_window_emitted: u32,
    /// Count of ACKed items in the current delivery window.
    delivery_window_acked: u32,
    /// Consecutive rounds with delivery rate below LOW_DELIVERY threshold.
    low_delivery_rounds: u32,
    /// Consecutive rounds with delivery rate at or above HIGH_DELIVERY threshold.
    high_delivery_rounds: u32,
    /// Per-(tile, gen) count of refinement-queue passes that have transitioned
    /// to `WorkState::Acked`. Used by `tile_fully_acked` to signal the
    /// PixelPerfect transition without scanning the queue on each query.
    /// Cleared for old gens on `bump_generation`/`bump_generation_collecting`.
    cdf53_passes_acked: HashMap<(u8, u8, u8), u8>,
}

impl Scheduler {
    pub fn new(cols: u32, rows: u32) -> Self {
        Self {
            cols,
            rows,
            generations: vec![0; (cols as usize) * (rows as usize)],
            priority_queue: VecDeque::new(),
            refinement_queue: VecDeque::new(),
            rtt: Duration::from_millis(20),
            refinement_bandwidth_fraction: 0.2,
            delivery_window_emitted: 0,
            delivery_window_acked: 0,
            low_delivery_rounds: 0,
            high_delivery_rounds: 0,
            cdf53_passes_acked: HashMap::new(),
        }
    }

    pub fn resize(&mut self, cols: u32, rows: u32) {
        self.cols = cols;
        self.rows = rows;
        self.generations = vec![0; (cols as usize) * (rows as usize)];
        self.priority_queue.clear();
        self.refinement_queue.clear();
        self.cdf53_passes_acked.clear();
    }

    /// Drain all pending and in-flight work. Generations are preserved so
    /// any late ACKs from the prior session still no-op cleanly (their
    /// matching work is gone). Called on session reconnect to prevent stale
    /// work from polluting the new client's first tick.
    pub fn clear(&mut self) {
        self.priority_queue.clear();
        self.refinement_queue.clear();
        // Per-(tile, gen) ACK counters are kept for the same reason
        // `generations` is kept — a late ACK on a stale (tile, gen) is a
        // safe no-op, and the counter is harmless until the next
        // bump_generation for that tile clears it.
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
        self.priority_queue.len()
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
        self.priority_queue.push_back(work);
    }

    #[cfg(test)]
    pub fn peek_for_test(&self) -> Vec<TileWork> {
        self.priority_queue.iter().cloned().collect()
    }

    #[cfg(test)]
    pub fn cdf53_passes_acked_for_test(&self, tile_x: u8, tile_y: u8, generation: u8) -> u8 {
        self.cdf53_passes_acked
            .get(&(tile_x, tile_y, generation))
            .copied()
            .unwrap_or(0)
    }

    pub fn bump_generation(&mut self, tile_x: u8, tile_y: u8) -> u8 {
        let idx = (tile_y as usize) * (self.cols as usize) + (tile_x as usize);
        let new_gen = (self.generations[idx] + 1) & 0x0F;
        self.generations[idx] = new_gen;
        // Any queued work for this tile is now stale; mark Superseded so the
        // next tick drops it. Acked entries don't matter (already done).
        for work in self.priority_queue.iter_mut().chain(self.refinement_queue.iter_mut()) {
            if work.tile_x == tile_x
                && work.tile_y == tile_y
                && !matches!(work.state, WorkState::Superseded | WorkState::Acked)
            {
                work.state = WorkState::Superseded;
            }
        }
        // Drop any per-(tile, gen) ACK counters for the old gen — they're
        // no longer relevant once the gen advances.
        self.cdf53_passes_acked.retain(|&(tx, ty, _g), _| !(tx == tile_x && ty == tile_y));
        new_gen
    }

    #[cfg(test)]
    pub fn queue_states_for_test(&self) -> Vec<(u8, u8, WorkState)> {
        self.priority_queue
            .iter()
            .map(|w| (w.tile_x, w.tile_y, w.state))
            .collect()
    }

    /// Increment the per-(tile, generation) Cdf53 ACK counter. Called from
    /// the coverage-ACK handler in `IoBridge::dispatch_ack_datagram` for
    /// each `FragmentCoverage` entry whose `codec == Codec::Cdf53`. Drives
    /// the PixelPerfect transition via `tile_fully_acked`.
    pub fn record_cdf53_ack(&mut self, tile_x: u8, tile_y: u8, generation: u8) {
        let counter = self.cdf53_passes_acked
            .entry((tile_x, tile_y, generation))
            .or_insert(0);
        *counter = counter.saturating_add(1);
    }

    /// Returns true iff all `max_passes` passes for the given (tile, gen)
    /// have transitioned to `WorkState::Acked`. O(1) lookup. Used by
    /// `IoBridge` to signal `CodecState::PixelPerfect` once a tile is
    /// fully refined.
    pub fn tile_fully_acked(&self, tile_x: u8, tile_y: u8, generation: u8, max_passes: u8) -> bool {
        self.cdf53_passes_acked
            .get(&(tile_x, tile_y, generation))
            .map_or(false, |&c| c >= max_passes)
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
        for entry in self.priority_queue.iter_mut().chain(self.refinement_queue.iter_mut()) {
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
        self.priority_queue.retain(|w| w.state != WorkState::Superseded);
        self.refinement_queue.retain(|w| w.state != WorkState::Superseded);
        (new_gen, resolved)
    }

    /// Drain the queues per the M3.3a scheduling rule:
    ///
    /// - Drop `Superseded` and `Acked` entries (terminal states) from both queues.
    /// - Partition `budget_bytes` between priority and refinement work using
    ///   `refinement_bandwidth_fraction`. Empty-queue repurposing: if one queue
    ///   is empty the other receives the full budget.
    /// - Priority queue is drained with soft-cap semantics (same as M3.1): the
    ///   item that crosses the budget is still returned; the loop stops before
    ///   the next eligible item.
    /// - Refinement queue is drained in pass-major order under a hard budget cap.
    ///
    /// Pass `usize::MAX` to disable budgeting (M3.1 call sites do this).
    pub fn tick(&mut self, budget_bytes: usize) -> Vec<TileWork> {
        let fraction = self.refinement_bandwidth_fraction;
        let mut refinement_budget = (budget_bytes as f32 * fraction) as usize;
        let mut priority_budget = budget_bytes.saturating_sub(refinement_budget);

        // Empty-queue repurposing.
        if self.refinement_queue.is_empty() {
            priority_budget = budget_bytes;
            refinement_budget = 0;
        } else if self.priority_queue.is_empty() {
            refinement_budget = budget_bytes;
            priority_budget = 0;
        }

        let mut emitted = Vec::new();
        let rtt = self.rtt;
        Self::drain_priority_queue(&mut self.priority_queue, priority_budget, rtt, &mut emitted);
        Self::drain_refinement_pass_major(&mut self.refinement_queue, refinement_budget, &mut emitted);

        self.delivery_window_emitted = self.delivery_window_emitted.saturating_add(emitted.len() as u32);
        self.maybe_adjust_refinement_fraction();

        emitted
    }

    fn drain_priority_queue(
        queue: &mut VecDeque<TileWork>,
        budget: usize,
        rtt: Duration,
        out: &mut Vec<TileWork>,
    ) {
        let now = Instant::now();
        let retry_after = 2 * rtt;

        // Drop terminal-state entries first.
        queue.retain(|w| !matches!(w.state, WorkState::Superseded | WorkState::Acked));

        let mut spent = 0usize;
        for work in queue.iter_mut() {
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
            if spent >= budget {
                break;
            }
            work.state = WorkState::InFlight;
            work.last_sent_at = Some(now);
            spent = spent.saturating_add(work.payload.len());
            out.push(work.clone());
        }
    }

    /// Drain refinement queue in pass-major order: every tile's pass 0 before
    /// any tile's pass 1.
    fn drain_refinement_pass_major(
        queue: &mut VecDeque<TileWork>,
        budget: usize,
        out: &mut Vec<TileWork>,
    ) {
        // Drop terminal-state entries first. Mirrors drain_priority_queue.
        // Without this, plain bump_generation (which only marks Superseded
        // without retain) leaves stale Pending->Superseded entries that
        // re-emit on the next tick with old gen values — corrupting the
        // client integrator (which clears its tile state on gen change).
        queue.retain(|w| !matches!(w.state, WorkState::Superseded | WorkState::Acked));

        let mut spent = 0usize;
        loop {
            let min_pass = match queue.iter().map(|w| w.pass_idx).min() {
                Some(p) => p,
                None => break,
            };
            let mut still_at_this_pass = false;
            let mut idx = 0;
            while idx < queue.len() {
                if queue[idx].pass_idx == min_pass {
                    let cost = queue[idx].payload.len();
                    if spent + cost > budget {
                        return;
                    }
                    let mut work = queue.remove(idx).unwrap();
                    work.last_sent_at = Some(Instant::now());
                    work.state = WorkState::InFlight;
                    spent += cost;
                    out.push(work);
                    still_at_this_pass = true;
                } else {
                    idx += 1;
                }
            }
            if !still_at_this_pass { break; }
        }
    }

    fn maybe_adjust_refinement_fraction(&mut self) {
        const ADJUST_AFTER_ROUNDS: u32 = 10;
        const LOW_DELIVERY: f32 = 0.5;
        const HIGH_DELIVERY: f32 = 0.8;
        const MIN_FRACTION: f32 = 0.05;
        const MAX_FRACTION: f32 = 0.2;

        // Empty window = no signal. Don't tick either streak counter and don't
        // reset the window; we may accumulate a partial round if work resumes
        // before the next tick. The window will reset naturally once work flows.
        if self.delivery_window_emitted == 0 {
            return;
        }

        let rate = self.delivery_window_acked as f32 / self.delivery_window_emitted as f32;

        if rate < LOW_DELIVERY {
            self.low_delivery_rounds += 1;
            self.high_delivery_rounds = 0;
            if self.low_delivery_rounds >= ADJUST_AFTER_ROUNDS {
                self.refinement_bandwidth_fraction =
                    (self.refinement_bandwidth_fraction * 0.5).max(MIN_FRACTION);
                self.low_delivery_rounds = 0;
            }
        } else if rate >= HIGH_DELIVERY {
            self.high_delivery_rounds += 1;
            self.low_delivery_rounds = 0;
            if self.high_delivery_rounds >= ADJUST_AFTER_ROUNDS {
                self.refinement_bandwidth_fraction =
                    (self.refinement_bandwidth_fraction * 2.0).min(MAX_FRACTION);
                self.high_delivery_rounds = 0;
            }
        } else {
            self.low_delivery_rounds = self.low_delivery_rounds.saturating_sub(1);
            self.high_delivery_rounds = self.high_delivery_rounds.saturating_sub(1);
        }

        self.delivery_window_emitted = 0;
        self.delivery_window_acked = 0;
    }

    /// Diagnostic accessor: current refinement-bandwidth fraction.
    pub fn current_refinement_fraction(&self) -> f32 {
        self.refinement_bandwidth_fraction
    }

    /// Enqueue all passes for a refinement tile. Each pass becomes one
    /// TileWork with pass_idx 0..passes.len()-1 and total_passes = passes.len().
    pub fn enqueue_refinement(
        &mut self,
        tile_x: u8,
        tile_y: u8,
        gen: u8,
        passes: Vec<Vec<u8>>,
    ) {
        let total = passes.len() as u8;
        for (pass_idx, payload) in passes.into_iter().enumerate() {
            self.refinement_queue.push_back(TileWork {
                tile_x,
                tile_y,
                generation: gen,
                pass_idx: pass_idx as u8,
                total_passes: total,
                codec: Codec::Cdf53,
                payload,
                queued_at: Instant::now(),
                last_sent_at: None,
                state: WorkState::Pending,
            });
        }
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

    #[test]
    fn refinement_pass_major_round_robin() {
        let mut sch = Scheduler::new(32, 24);
        // 2 tiles × 4 passes each (4 passes instead of 14 for brevity).
        let passes_a: Vec<Vec<u8>> = (0..4).map(|i| vec![b'A', i as u8]).collect();
        let passes_b: Vec<Vec<u8>> = (0..4).map(|i| vec![b'B', i as u8]).collect();
        sch.enqueue_refinement(0, 0, 0, passes_a);
        sch.enqueue_refinement(1, 0, 0, passes_b);

        // Drain in tick() calls; assert pass-major order:
        //   (0,0,p0), (1,0,p0), (0,0,p1), (1,0,p1), ...
        // Budget=3 with 2-byte payloads: repurposing gives refinement_budget=3;
        // `>` check emits 1 item (spent=2, 2+2=4>3 stops), ensuring one item per tick.
        let mut emitted: Vec<(u8, u8, u8)> = Vec::new();
        for _ in 0..8 {
            let mut got = sch.tick(3);
            assert!(!got.is_empty(), "tick should emit work while queue is non-empty");
            for w in got.drain(..) {
                emitted.push((w.tile_x, w.tile_y, w.pass_idx));
            }
        }
        assert_eq!(emitted, vec![
            (0,0,0), (1,0,0),
            (0,0,1), (1,0,1),
            (0,0,2), (1,0,2),
            (0,0,3), (1,0,3),
        ]);
    }

    #[test]
    fn refinement_fraction_default_is_20_percent() {
        let sch = Scheduler::new(8, 8);
        assert!((sch.current_refinement_fraction() - 0.2).abs() < 1e-6);
    }

    #[test]
    fn refinement_fraction_halves_on_sustained_low_delivery() {
        let mut sch = Scheduler::new(8, 8);
        for i in 0..200 {
            sch.enqueue_refinement(0, 0, 0, vec![vec![i as u8]; 14]);
        }
        // tick(250) drains ~250 items per call → queue stays non-empty for 10+
        // ticks, so every round has emissions + no acks → rate=0 → halving fires
        // on the 10th low-delivery round.
        for _ in 0..10 {
            let _ = sch.tick(250);
        }
        assert!((sch.current_refinement_fraction() - 0.1).abs() < 1e-6,
            "expected 0.1 after one halving; got {}", sch.current_refinement_fraction());
    }

    #[test]
    fn empty_priority_queue_lets_refinement_use_full_budget() {
        let mut sch = Scheduler::new(8, 8);
        for i in 0..5 {
            sch.enqueue_refinement(i, 0, 0, vec![vec![0u8; 100]; 14]);
        }
        let emitted = sch.tick(1000);
        assert!(emitted.len() >= 10,
            "expected ≥10 refinement passes when priority empty; got {}", emitted.len());
    }

    #[test]
    fn bump_generation_cancels_refinement_work() {
        let mut sch = Scheduler::new(8, 8);
        sch.enqueue_refinement(3, 4, 0, vec![vec![0u8; 50]; 14]);
        assert_eq!(sch.refinement_queue.len(), 14);
        let _new_gen = sch.bump_generation(3, 4);
        let superseded_count = sch.refinement_queue.iter()
            .filter(|w| w.state == WorkState::Superseded)
            .count();
        assert_eq!(superseded_count, 14,
            "expected all 14 refinement passes for the tile to be Superseded after bump_generation");
    }

    #[test]
    fn tile_fully_acked_returns_true_after_all_passes_acked() {
        let mut s = Scheduler::new(4, 4);
        assert!(!s.tile_fully_acked(2, 3, 1, 14));
        for _pass in 0..13u8 {
            s.record_cdf53_ack(2, 3, 1);
            assert!(
                !s.tile_fully_acked(2, 3, 1, 14),
                "should not be fully_acked before all 14 acks"
            );
        }
        s.record_cdf53_ack(2, 3, 1);
        assert!(s.tile_fully_acked(2, 3, 1, 14));
    }

    #[test]
    fn tile_fully_acked_isolates_per_tile_and_gen() {
        let mut s = Scheduler::new(4, 4);
        for _pass in 0..14u8 {
            s.record_cdf53_ack(0, 0, 1);
        }
        assert!(s.tile_fully_acked(0, 0, 1, 14));
        assert!(!s.tile_fully_acked(1, 0, 1, 14));
        assert!(!s.tile_fully_acked(0, 0, 2, 14)); // different gen
    }

    #[test]
    fn bump_generation_drops_old_gen_ack_counter() {
        let mut s = Scheduler::new(4, 4);
        for _pass in 0..14u8 {
            s.record_cdf53_ack(0, 0, 1);
        }
        assert!(s.tile_fully_acked(0, 0, 1, 14));
        let _ = s.bump_generation(0, 0);
        assert!(!s.tile_fully_acked(0, 0, 1, 14), "old gen counter should be dropped");
        assert!(!s.tile_fully_acked(0, 0, 2, 14), "new gen has no acks yet");
    }

    #[test]
    fn bump_generation_only_clears_target_tile() {
        let mut s = Scheduler::new(4, 4);
        for _pass in 0..14u8 {
            s.record_cdf53_ack(0, 0, 1);
            s.record_cdf53_ack(1, 1, 1);
        }
        assert!(s.tile_fully_acked(0, 0, 1, 14));
        assert!(s.tile_fully_acked(1, 1, 1, 14));

        // Bump only (0, 0). Tile (1, 1)'s counter must survive.
        let _ = s.bump_generation(0, 0);
        assert!(!s.tile_fully_acked(0, 0, 1, 14), "target tile cleared");
        assert!(s.tile_fully_acked(1, 1, 1, 14), "non-target tile preserved");
    }

    #[test]
    fn record_cdf53_ack_increments_per_tile_gen_counter() {
        let mut s = Scheduler::new(4, 4);
        assert!(!s.tile_fully_acked(2, 3, 1, 14));
        for _ in 0..14 {
            s.record_cdf53_ack(2, 3, 1);
        }
        assert!(s.tile_fully_acked(2, 3, 1, 14));
    }

    #[test]
    fn record_cdf53_ack_isolates_per_tile_and_gen() {
        let mut s = Scheduler::new(4, 4);
        for _ in 0..14 {
            s.record_cdf53_ack(0, 0, 1);
        }
        assert!(s.tile_fully_acked(0, 0, 1, 14));
        assert!(!s.tile_fully_acked(1, 0, 1, 14), "different tile");
        assert!(!s.tile_fully_acked(0, 0, 2, 14), "different gen");
    }

    #[test]
    fn drain_refinement_skips_superseded_after_bump() {
        // Regression: drain_refinement_pass_major used to lack the
        // Superseded/Acked retain that drain_priority_queue has, so plain
        // bump_generation (which marks Superseded without retain) would let
        // stale entries re-emit on the next tick with old gen — corrupting
        // the client integrator. This locks the retain at the top of
        // drain_refinement_pass_major.
        let mut s = Scheduler::new(4, 4);

        // Bump once so generations[(0,0)] = 1 (matching what io_bridge's
        // Phase B does before enqueue_refinement). Then enqueue gen=1.
        let gen1 = s.bump_generation(0, 0);
        assert_eq!(gen1, 1);
        let passes: Vec<Vec<u8>> = (0..14).map(|i| vec![i as u8]).collect();
        s.enqueue_refinement(0, 0, gen1, passes.clone());

        // Plain bump_generation again — marks all gen=1 Pending entries as
        // Superseded but does NOT retain them out of the queue.
        let gen2 = s.bump_generation(0, 0);
        assert_eq!(gen2, 2);

        // Enqueue gen=2 passes for the same tile.
        s.enqueue_refinement(0, 0, gen2, passes);

        // Tick: should ONLY emit gen=2 entries, not the Superseded gen=1.
        let resolved = s.tick(usize::MAX);
        let emitted_old_gen: Vec<_> = resolved
            .iter()
            .filter(|w| w.tile_x == 0 && w.tile_y == 0 && w.generation == gen1)
            .collect();
        assert!(
            emitted_old_gen.is_empty(),
            "Superseded gen=1 entries must not re-emit; got {} stale emissions",
            emitted_old_gen.len(),
        );
        let emitted_new_gen: Vec<_> = resolved
            .iter()
            .filter(|w| w.tile_x == 0 && w.tile_y == 0 && w.generation == gen2)
            .collect();
        assert_eq!(emitted_new_gen.len(), 14, "all 14 new-gen passes emit");
    }
}
