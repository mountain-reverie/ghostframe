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
/// A forward jump larger than this is a new sequence space (session change),
/// not a loss burst: resynchronise without counting it.
const RESYNC_JUMP: u32 = 4096;

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
            self.stats.max_run = self.stats.max_run.max(self.cur_run);
            self.cur_run = 0;
        }
    }

    /// Settle every slot older than the reorder window behind `newest`.
    fn settle(&mut self, newest: u32) {
        while (newest.wrapping_sub(self.settled_next) as i32) > REORDER_WINDOW as i32 {
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
        t.on_received(1_000_000);
        for s in 1_000_001..1_000_300 {
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
}
