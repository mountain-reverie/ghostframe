// SPDX-License-Identifier: MIT OR Apache-2.0
//! Pre-FEC loss and loss-run measurement from `wire_seq` gaps.
//!
//! Measurement only: nothing here feeds a decision. The server stamps every
//! tile datagram it sends (sources and retransmissions alike) with the next
//! `wire_seq`, so a gap in the sequence the client sees is a datagram that
//! never arrived off the wire. That is loss BEFORE forward error correction,
//! which the server cannot see (its ledger counts a FEC-rebuilt datagram as
//! delivered), plus any datagram the sender's own queue refused or dropped.
//!
//! A gap is only declared a loss once the stream has moved [`REORDER_WINDOW`]
//! sequence numbers past it, so reordering is not loss. A datagram that turns
//! up after its slot was declared lost is counted in `late_after_declared`.

/// Slots newer than this many behind the newest are still allowed to fill in.
pub const REORDER_WINDOW: u32 = 64;
/// Ring size; must exceed [`REORDER_WINDOW`] plus the largest gap handled in place.
const RING: usize = 512;
/// A forward jump larger than this is a new sequence space, not a loss burst:
/// resynchronise without counting it.
///
/// Within one `ClientCore` that does not happen: the server restarts its
/// counters only for a new connection, which is a new core. So a large jump is
/// a blackout, and it must be counted. The sender stops at its congestion
/// window, so a blackout's gap is about one window however long it lasts; it
/// passes 4096 only with a large bandwidth-delay product (about 200 ms at
/// 100 Mbit/s). The bound only stops an implausible value, a corrupt or
/// hostile one, from being read as a loss of billions; 2^20 is about 100 s at
/// 10 000 datagrams/s.
const RESYNC_JUMP: u32 = 1 << 20;

/// Cumulative counters. `run_hist[i]` counts settled loss runs of length
/// 1, 2, 3, 4, 5-8, 9-16, 17-32, >32.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WireLossStats {
    /// Distinct datagrams that arrived off the wire.
    pub received: u64,
    /// Slots declared lost (never arrived within the reorder window).
    pub lost_pre_fec: u64,
    /// Datagrams that arrived out of order but inside the window.
    pub reorder_late: u64,
    /// Datagrams that arrived after their slot was already declared lost.
    pub late_after_declared: u64,
    /// Datagrams rebuilt from parity that had not arrived (real recoveries).
    pub recovered: u64,
    /// Parity "recoveries" of a datagram that had in fact arrived.
    pub dup_rebuilt: u64,
    pub run_hist: [u64; 8],
    /// Longest settled loss run.
    pub max_run: u32,
    /// Losses of the closed runs, the ones whose histogram bucket is written.
    /// `lost_pre_fec - lost_in_runs` is the open run's share. A report reads
    /// this, not `lost_pre_fec`, so that a run's losses and its length reach
    /// the server in the same report. (A run reported before it closes would
    /// send its later losses with no bucket, and the server could not tell
    /// they were part of a long run.)
    pub lost_in_runs: u64,
}

pub(crate) struct WireLossTracker {
    stats: WireLossStats,
    /// Newest sequence number seen.
    newest: Option<u32>,
    /// First sequence number not yet settled.
    settled_next: u32,
    ring: Vec<bool>,
    cur_run: u32,
}

fn bucket(run: u32) -> usize {
    match run {
        1..=4 => run as usize - 1,
        5..=8 => 4,
        9..=16 => 5,
        17..=32 => 6,
        _ => 7,
    }
}

impl WireLossTracker {
    pub fn new() -> Self {
        WireLossTracker {
            stats: WireLossStats::default(),
            newest: None,
            settled_next: 0,
            ring: vec![false; RING],
            cur_run: 0,
        }
    }

    pub fn stats(&self) -> WireLossStats {
        self.stats
    }

    fn slot(&mut self, seq: u32) -> &mut bool {
        &mut self.ring[seq as usize % RING]
    }

    fn close_run(&mut self) {
        if self.cur_run > 0 {
            self.stats.run_hist[bucket(self.cur_run)] += 1;
            self.stats.lost_in_runs += u64::from(self.cur_run);
            self.stats.max_run = self.stats.max_run.max(self.cur_run);
            self.cur_run = 0;
        }
    }

    /// Losses settled in the run still open (0 when none).
    pub(crate) fn open_run(&self) -> u32 {
        self.cur_run
    }

    /// Close the open run at its true length, for a caller that cannot wait
    /// for the stream to settle it: settle the slots after the settled ones
    /// that are lost, up to the first that arrived, then close. Every slot of
    /// the run is known by then (the newest slot always arrived, so the run
    /// ends at or before it), so its losses and its bucket enter the
    /// statistics together at the length it really has. It is never cut in
    /// two because time passed: the settled head and the unsettled tail are
    /// one run. A datagram that turns up later in a slot settled here is
    /// counted like any other too-old arrival (`late_after_declared`).
    pub(crate) fn close_open_run(&mut self) {
        if self.cur_run == 0 {
            return;
        }
        while self.has_unsettled() && !*self.slot(self.settled_next) {
            self.settle_one();
        }
        self.close_run();
    }

    /// Slots from `settled_next` through the newest are still waiting out the
    /// reorder window.
    pub(crate) fn has_unsettled(&self) -> bool {
        self.newest
            .is_some_and(|n| (n.wrapping_sub(self.settled_next) as i32) >= 0)
    }

    /// Settle everything through the newest slot now, closing the run it ends.
    /// For a caller that has heard nothing for long enough that no datagram of
    /// the window is still in flight; a late arrival into a slot settled here
    /// is a too-old arrival (`late_after_declared`), as after any settle.
    pub(crate) fn settle_all(&mut self) {
        while self.has_unsettled() {
            self.settle_one();
        }
    }

    /// Settle the slot at `settled_next`: lost if it never arrived.
    fn settle_one(&mut self) {
        let s = self.settled_next;
        if *self.slot(s) {
            self.close_run();
        } else {
            self.stats.lost_pre_fec += 1;
            self.cur_run += 1;
        }
        *self.slot(s) = false;
        self.settled_next = s.wrapping_add(1);
    }

    /// Settle every slot older than the reorder window behind `newest`.
    fn settle(&mut self, newest: u32) {
        while (newest.wrapping_sub(self.settled_next) as i32) > REORDER_WINDOW as i32 {
            self.settle_one();
        }
    }

    /// A forward jump of `d` too large to clear slot by slot: the ring would
    /// wrap over the slots still inside the reorder window (and over the
    /// jump's own slot), so datagrams that did arrive would be counted lost.
    /// Settle what is pending behind `newest` as it stands, count the gap
    /// older than the new window by arithmetic, and restart the ring.
    fn jump(&mut self, newest: u32, seq: u32, d: u32) {
        while (newest.wrapping_sub(self.settled_next) as i32) >= 0 {
            self.settle_one();
        }
        // Slots newest+1 .. seq-REORDER_WINDOW-1 never arrived.
        let lost = (d - 1 - REORDER_WINDOW) as u64;
        self.stats.lost_pre_fec += lost;
        self.cur_run = self
            .cur_run
            .saturating_add(lost.min(u32::MAX as u64) as u32);
        self.ring.iter_mut().for_each(|b| *b = false);
        self.settled_next = seq.wrapping_sub(REORDER_WINDOW);
        *self.slot(seq) = true;
        self.newest = Some(seq);
        self.stats.received += 1;
    }

    /// A datagram with this `wire_seq` arrived off the wire.
    pub fn on_received(&mut self, seq: u32) {
        let Some(newest) = self.newest else {
            self.newest = Some(seq);
            self.settled_next = seq;
            *self.slot(seq) = true;
            self.stats.received += 1;
            return;
        };
        let d = seq.wrapping_sub(newest) as i32;
        if d > 0 {
            if d as u32 > RESYNC_JUMP {
                self.close_run();
                self.ring.iter_mut().for_each(|b| *b = false);
                self.newest = Some(seq);
                self.settled_next = seq;
                *self.slot(seq) = true;
                self.stats.received += 1;
                return;
            }
            if d as usize + REORDER_WINDOW as usize + 1 >= RING {
                self.jump(newest, seq, d as u32);
                return;
            }
            // Slots between are gaps until they fill in. Clear them before the
            // ring wraps over stale marks.
            for s in 1..d as u32 {
                *self.slot(newest.wrapping_add(s)) = false;
            }
            *self.slot(seq) = true;
            self.newest = Some(seq);
            self.stats.received += 1;
            self.settle(seq);
            return;
        }
        // Not newer: reordered, duplicated, or after its slot was settled.
        if (seq.wrapping_sub(self.settled_next) as i32) < 0 {
            self.stats.late_after_declared += 1;
            self.stats.received += 1;
            return;
        }
        if *self.slot(seq) {
            return; // duplicate
        }
        *self.slot(seq) = true;
        self.stats.received += 1;
        self.stats.reorder_late += 1;
    }

    /// A datagram with this `wire_seq` was rebuilt from parity. Real only if it
    /// had not arrived (still unmarked, or already settled as lost).
    pub fn on_rebuilt(&mut self, seq: u32) -> bool {
        let settled = (seq.wrapping_sub(self.settled_next) as i32) < 0;
        let in_window = self.newest.is_some_and(|n| {
            (n.wrapping_sub(seq) as i32) >= 0 && n.wrapping_sub(seq) < RING as u32
        });
        let arrived = !settled && in_window && *self.slot(seq);
        if arrived {
            self.stats.dup_rebuilt += 1;
            false
        } else {
            self.stats.recovered += 1;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deliver `0..n` skipping every seq in `lost`, in order.
    fn run(n: u32, lost: &[u32]) -> WireLossTracker {
        let mut t = WireLossTracker::new();
        for s in 0..n {
            if !lost.contains(&s) {
                t.on_received(s);
            }
        }
        t
    }

    #[test]
    fn counts_exactly_the_injected_losses_and_their_run_lengths() {
        // Runs of 1, 2, 3, 6 and 20 separated by received datagrams.
        let mut lost = vec![10];
        lost.extend(20..22);
        lost.extend(30..33);
        lost.extend(40..46);
        lost.extend(60..80);
        let t = run(400, &lost);
        let s = t.stats();
        assert_eq!(s.lost_pre_fec, lost.len() as u64);
        assert_eq!(s.run_hist, [1, 1, 1, 0, 1, 0, 1, 0]);
        assert_eq!(s.max_run, 20);
        assert!(s.received > 0);
    }

    /// Every datagram up to `last` arrives, then the stream jumps `d` ahead
    /// and continues in order: exactly `d - 1` were lost, whatever `d` is.
    fn jumped(last: u32, d: u32) -> WireLossStats {
        let mut t = WireLossTracker::new();
        for s in 0..=last {
            t.on_received(s);
        }
        let to = last + d;
        for s in to..to + 300 {
            t.on_received(s);
        }
        t.stats()
    }

    #[test]
    fn a_jump_counts_exactly_the_skipped_datagrams_at_any_size() {
        for d in [
            2, 65, 100, 440, 446, 447, 448, 449, 511, 512, 513, 1000, 4096, 4097, 60_000,
        ] {
            let s = jumped(99, d);
            assert_eq!(s.lost_pre_fec, (d - 1) as u64, "jump of {d}");
            assert_eq!(s.max_run, d - 1, "run for a jump of {d}");
            assert_eq!(s.received, 100 + 300, "received across a jump of {d}");
        }
    }

    #[test]
    fn a_jump_does_not_lose_the_datagrams_still_in_the_reorder_window() {
        // 0..=99 arrived; 100 was skipped, 101 arrived (pending in window),
        // then a 600-datagram blackout.
        let mut t = WireLossTracker::new();
        for s in (0..=99).chain([101]) {
            t.on_received(s);
        }
        let to = 101 + 600;
        for s in to..to + 300 {
            t.on_received(s);
        }
        // 100 plus the 599 between 102 and 700.
        assert_eq!(t.stats().lost_pre_fec, 1 + 599);
    }

    #[test]
    fn a_blackout_is_loss_not_a_resync() {
        // The old tracker read a 5000 jump as a new stream and counted none.
        assert_eq!(jumped(99, 5000).lost_pre_fec, 4999);
    }

    #[test]
    fn an_implausible_jump_is_a_resync() {
        assert_eq!(jumped(99, RESYNC_JUMP + 1).lost_pre_fec, 0);
        // The largest jump still read as a blackout. Without it the line above
        // holds for a resync bound of any size.
        assert_eq!(
            jumped(99, RESYNC_JUMP).lost_pre_fec,
            (RESYNC_JUMP - 1) as u64
        );
    }

    #[test]
    fn a_jump_across_the_u32_wrap_is_counted_exactly() {
        let mut t = WireLossTracker::new();
        let start = u32::MAX - 200;
        for i in 0..100 {
            t.on_received(start.wrapping_add(i));
        }
        let to = start.wrapping_add(99).wrapping_add(1000);
        for i in 0..300 {
            t.on_received(to.wrapping_add(i));
        }
        let s = t.stats();
        assert_eq!((s.lost_pre_fec, s.received), (999, 400));
    }

    #[test]
    fn reordering_inside_the_window_is_not_loss() {
        let mut t = WireLossTracker::new();
        for s in 0..10 {
            t.on_received(s);
        }
        t.on_received(12); // 10, 11 skipped for now
        t.on_received(11);
        t.on_received(10);
        for s in 13..300 {
            t.on_received(s);
        }
        let s = t.stats();
        assert_eq!(s.lost_pre_fec, 0);
        assert_eq!(s.reorder_late, 2);
    }

    #[test]
    fn a_datagram_after_its_slot_was_declared_lost_is_counted_separately() {
        let mut t = WireLossTracker::new();
        for s in 0..300 {
            if s != 5 {
                t.on_received(s);
            }
        }
        assert_eq!(t.stats().lost_pre_fec, 1);
        t.on_received(5);
        assert_eq!(t.stats().late_after_declared, 1);
    }

    #[test]
    fn a_rebuilt_datagram_is_a_recovery_only_if_it_had_not_arrived() {
        let mut t = WireLossTracker::new();
        for s in 0..100 {
            if s != 50 {
                t.on_received(s);
            }
        }
        assert!(t.on_rebuilt(50), "50 never arrived");
        assert!(!t.on_rebuilt(51), "51 arrived");
        let s = t.stats();
        assert_eq!((s.recovered, s.dup_rebuilt), (1, 1));
    }

    #[test]
    fn a_huge_forward_jump_resynchronises_instead_of_counting_a_burst() {
        let mut t = run(200, &[]);
        t.on_received(5_000_000);
        for s in 5_000_001..5_000_300 {
            t.on_received(s);
        }
        assert_eq!(t.stats().lost_pre_fec, 0);
    }

    #[test]
    fn wraparound_is_handled() {
        let mut t = WireLossTracker::new();
        let start = u32::MAX - 100;
        for i in 0..400u32 {
            if i != 120 {
                t.on_received(start.wrapping_add(i));
            }
        }
        assert_eq!(t.stats().lost_pre_fec, 1);
    }

    /// `lost_in_runs` is the losses of closed runs: an open run's losses are
    /// counted as lost but are not in a run, and arrive with it when it closes.
    #[test]
    fn lost_in_runs_waits_for_the_run_to_close_whatever_its_length() {
        for len in [10u32, 40, 100] {
            let mut t = WireLossTracker::new();
            for s in 0..100 {
                t.on_received(s);
            }
            // 100..100+len lost; the stream carries on until just short of the
            // run's close, then past it.
            let mut open_seen = 0;
            for s in (100 + len)..(100 + len + 200) {
                t.on_received(s);
                let st = t.stats();
                if st.lost_pre_fec > 0 && st.lost_in_runs == 0 {
                    open_seen += 1;
                    assert_eq!(st.run_hist, [0; 8], "len {len}: not in the histogram yet");
                }
            }
            assert!(
                open_seen > 0 || len <= 1,
                "len {len}: observed the run open"
            );
            let st = t.stats();
            assert_eq!(
                (st.lost_pre_fec, st.lost_in_runs),
                (u64::from(len), u64::from(len))
            );
            assert_eq!(st.run_hist.iter().sum::<u64>(), 1, "one run, counted once");
        }
    }

    #[test]
    fn an_open_run_closed_early_is_one_run_at_its_true_length() {
        let mut t = WireLossTracker::new();
        for s in 0..100 {
            t.on_received(s);
        }
        // 100..=199 lost, 230 sent: 65 of the lost settled, the run open, the
        // other 35 waiting in the reorder window behind it.
        for s in 200..230 {
            t.on_received(s);
        }
        assert_eq!(t.open_run(), 65);
        let before = t.stats();
        assert_eq!((before.lost_in_runs, before.run_hist), (0, [0; 8]));
        t.close_open_run();
        let st = t.stats();
        assert_eq!(
            (st.lost_in_runs, st.run_hist[7]),
            (100, 1),
            "one run of 100"
        );
        assert_eq!(st.lost_pre_fec, 100);
        assert_eq!(t.open_run(), 0);
        t.close_open_run();
        assert_eq!(t.stats(), st, "closing nothing changes nothing");
        // Traffic carries on: nothing of the run is counted again.
        for s in 230..400 {
            t.on_received(s);
        }
        let st = t.stats();
        assert_eq!((st.lost_pre_fec, st.lost_in_runs), (100, 100));
        assert_eq!(st.run_hist.iter().sum::<u64>(), 1);
    }

    #[test]
    fn settle_all_closes_a_gap_that_never_opened_a_run() {
        let mut t = WireLossTracker::new();
        for s in 0..100 {
            t.on_received(s);
        }
        // 100..120 lost (20), then 10 arrivals: all inside the reorder window.
        for s in 120..130 {
            t.on_received(s);
        }
        assert_eq!((t.open_run(), t.stats().lost_pre_fec), (0, 0));
        t.settle_all();
        let st = t.stats();
        assert_eq!(
            (st.lost_pre_fec, st.lost_in_runs, st.run_hist[6]),
            (20, 20, 1)
        );
        assert!(!t.has_unsettled());
        // A datagram of the gap turning up now is a too-old arrival: the
        // loss stays counted, the arrival is counted once.
        let received = st.received;
        t.on_received(110);
        let st = t.stats();
        assert_eq!(
            (st.late_after_declared, st.lost_pre_fec, st.received),
            (1, 20, received + 1)
        );
        // New traffic continues cleanly.
        for s in 130..300 {
            t.on_received(s);
        }
        let st = t.stats();
        assert_eq!((st.lost_pre_fec, st.run_hist.iter().sum::<u64>()), (20, 1));
    }

    #[test]
    fn a_jump_run_is_in_the_histogram_once_it_closes() {
        let mut t = WireLossTracker::new();
        for s in 0..100 {
            t.on_received(s);
        }
        t.on_received(100 + 1000);
        let st = t.stats();
        // 936 of the 1000 settled; the run is open.
        assert_eq!(st.lost_pre_fec, 1001 - 1 - REORDER_WINDOW as u64);
        assert_eq!((st.lost_in_runs, st.run_hist), (0, [0; 8]));
        for s in 1101..1300 {
            t.on_received(s);
        }
        let st = t.stats();
        assert_eq!((st.lost_pre_fec, st.lost_in_runs), (1000, 1000));
        assert_eq!(st.run_hist, [0, 0, 0, 0, 0, 0, 0, 1]);
    }
}
