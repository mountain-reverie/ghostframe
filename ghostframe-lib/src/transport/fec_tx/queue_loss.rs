// SPDX-License-Identifier: AGPL-3.0-or-later
//! The ledger loss gate: which of the losses a LOSS_REPORT will count were
//! made by the stream's own queue or by the sender itself, not by the path.
//!
//! Pure and sans-IO: every call takes its time (ms on any monotonic clock).
//!
//! # Why
//!
//! The estimator (`adapt.rs`) engages on loss it cannot excuse. The only
//! excuse it had was `bwe.standing_queue()` read when the report arrives, and
//! a report arrives about 600 ms after the burst that overflowed the buffer,
//! when the queue has drained. At 2 Mbit/s with a 150 ms buffer the first
//! ~60 kB frame of a session overflows it (31 tail drops), and quinn's MTU
//! black-hole detector then misreads that burst as a black hole and discards
//! the full-size datagrams still queued (`connection/mod.rs:1767-1775`,
//! `drop_oversized`): losses of the sender's own making.
//!
//! # Rule
//!
//! Each acknowledged bundle gives `q = OWD - min OWD over the last 10 s`
//! (OWD from the bundle's departure stamp and the client's
//! `arrival_time_ms_lo16`; the clock offset is unknown but constant, which a
//! windowed minimum removes). A bundle the ledger judges lost is a **queue
//! loss** when the nearest acknowledged bundle before it (by `bundle_seq`) had
//! `q >= max(Q_FLOOR, F x q_max)`, `q_max` the largest `q` of the last 10 s: it
//! was sent into a queue that was at (or near) the deepest this link has been
//! seen to hold. A bundle quinn discarded locally is a **local** loss.
//! Either gives the next LOSS_REPORT a credit of one loss to excuse
//! ([`QueueLossClassifier::excuse`]); a credit expires after
//! [`CREDIT_TTL_MS`], so a stale one cannot hide later random loss, and a
//! verdict a late acknowledgement contradicts is withdrawn.
//!
//! Measured (`docs/specs/fec-v2/sim-rev6/FINDINGS.md`, section 15): overflow
//! losses all have `q_before >= 100 ms`; i.i.d. losses after the first 2 s
//! have it in 0 % of cases (`>= 50 ms` in 0.5-5 %, `>= 80 ms` in 0-1.4 %).
//! The floor is 80 ms, not 50: at 50 ms a 5 % i.i.d. link at 10 Mbit/s had 3.5 %
//! of its random losses classed as queue losses, at 80 ms none, and the
//! 150 ms start-up overflow was covered either way (its bar is `0.8 x q_max`,
//! about 124 ms). `F` between 0.5 and 0.8 changed nothing; at 1.0 only a third
//! of the overflow qualified.
//!
//! Repair and control bundles carry no records and are not counted: the
//! credit is in source bundles, like the estimator's `lost_pre_fec`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// The smallest `q` that can be a queue loss, ms.
pub(crate) const Q_FLOOR_MS: i32 = 80;
/// Share of the 10 s peak of `q` a queue loss must have been sent into.
pub(crate) const Q_FRACTION: f64 = 0.8;
/// Window of the base OWD and of the peak, ms.
const WINDOW_MS: u64 = 10_000;
/// A credit not spent in this long is dropped, ms.
pub(crate) const CREDIT_TTL_MS: u64 = 2_000;
/// Bundles whose `q` is kept.
const QMAP_CAP: usize = 16_384;
/// Locally discarded bundles remembered.
const LOCAL_CAP: usize = 4_096;
/// Credits held.
const CREDIT_CAP: usize = 4_096;

/// What the classifier made of one loss verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LossClass {
    /// Sent into a queue at its deepest: the stream's own doing.
    Queue,
    /// Discarded by quinn before it reached the wire.
    Local,
    /// Neither: path loss as far as this can tell.
    Path,
    /// Carries no source records (repair, control): not counted.
    Ignored,
}

/// Cumulative counters (the log line and the harness).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct QueueLossStats {
    /// Source-bundle verdicts classed as queue losses.
    pub queue_losses: u64,
    /// Source-bundle verdicts that were a local discard.
    pub local_losses: u64,
    /// Datagrams quinn discarded locally (every kind), and black-hole events.
    pub local_discards: u64,
    pub black_holes: u64,
    /// Losses a LOSS_REPORT did not charge to the path.
    pub losses_excused: u64,
    /// Credits withdrawn by a late acknowledgement, and expired unspent.
    pub withdrawn: u64,
    pub expired: u64,
    /// Source-bundle verdicts that came after a report whose range already
    /// reached them (the report may have counted them): the race, as seen
    /// from the ledger. Of those, the queue and local ones.
    pub verdicts_behind_report: u64,
    pub credits_behind_report: u64,
}

#[derive(Debug, Clone, Copy)]
struct Credit {
    seq: u32,
    at_ms: u64,
}

/// See the module comment.
#[derive(Debug, Default)]
pub(crate) struct QueueLossClassifier {
    reference: Option<u16>,
    /// Sliding minimum of OWD: `(time, owd)`, owd increasing.
    base: VecDeque<(u64, i32)>,
    /// Sliding maximum of `q`: `(time, q)`, q decreasing.
    peak: VecDeque<(u64, i32)>,
    q: BTreeMap<u32, i32>,
    /// Bundles quinn discarded locally.
    local: BTreeSet<u32>,
    credit: VecDeque<Credit>,
    /// `last_bundle_seq` of the newest report folded.
    last_report_seq: Option<u32>,
    stats: QueueLossStats,
}

impl QueueLossClassifier {
    pub(crate) fn stats(&self) -> QueueLossStats {
        self.stats
    }

    /// A different connection is live: the old one's sequence numbers and
    /// clock reference mean nothing. The counters stay.
    pub(crate) fn reset_session(&mut self) {
        *self = Self {
            stats: self.stats,
            ..Self::default()
        };
    }

    /// The threshold now, ms.
    pub(crate) fn threshold_ms(&self) -> i32 {
        let q_max = self.peak.front().map_or(0, |&(_, q)| q);
        Q_FLOOR_MS.max((Q_FRACTION * f64::from(q_max)) as i32)
    }

    /// A live acknowledgement of `seq`, which left at `depart_ms` (the ledger's
    /// departure stamp) and reached the client at `arrival_ms_lo16`. Returns
    /// its `q`.
    pub(crate) fn on_ack(
        &mut self,
        seq: u32,
        arrival_ms_lo16: u16,
        depart_ms: u64,
        now_ms: u64,
    ) -> i32 {
        let owd_lo16 = arrival_ms_lo16.wrapping_sub((depart_ms & 0xFFFF) as u16);
        let r = *self.reference.get_or_insert(owd_lo16);
        let owd = i32::from(owd_lo16.wrapping_sub(r) as i16);
        while self.base.back().is_some_and(|&(_, o)| o >= owd) {
            self.base.pop_back();
        }
        self.base.push_back((now_ms, owd));
        while self
            .base
            .front()
            .is_some_and(|&(t, _)| t + WINDOW_MS < now_ms)
        {
            self.base.pop_front();
        }
        let q = owd - self.base.front().map_or(owd, |&(_, o)| o);
        self.trim_peak(now_ms);
        while self.peak.back().is_some_and(|&(_, p)| p <= q) {
            self.peak.pop_back();
        }
        self.peak.push_back((now_ms, q));
        self.q.insert(seq, q);
        while self.q.len() > QMAP_CAP {
            self.q.pop_first();
        }
        q
    }

    /// quinn discarded `seqs` (source and other bundles) before they reached
    /// the wire; `datagrams` is how many datagrams it discarded in all.
    pub(crate) fn on_local_discard(&mut self, seqs: &[u32], datagrams: u64) {
        self.stats.black_holes += 1;
        self.stats.local_discards += datagrams;
        self.local.extend(seqs.iter().copied());
        while self.local.len() > LOCAL_CAP {
            self.local.pop_first();
        }
    }

    /// The ledger judged `seq` lost. `has_records` is whether it carried
    /// source content. A queue or local loss earns the next report a credit.
    pub(crate) fn on_verdict(&mut self, seq: u32, has_records: bool, now_ms: u64) -> LossClass {
        self.trim_peak(now_ms);
        let local = self.local.remove(&seq);
        if !has_records {
            return LossClass::Ignored;
        }
        let class = if local {
            LossClass::Local
        } else if self
            .q
            .range(..seq)
            .next_back()
            .is_some_and(|(_, &q)| q >= self.threshold_ms())
        {
            LossClass::Queue
        } else {
            LossClass::Path
        };
        let behind = self.last_report_seq.is_some_and(|l| seq <= l);
        self.stats.verdicts_behind_report += u64::from(behind);
        if class != LossClass::Path {
            if class == LossClass::Queue {
                self.stats.queue_losses += 1;
            } else {
                self.stats.local_losses += 1;
            }
            self.stats.credits_behind_report += u64::from(behind);
            self.expire(now_ms);
            self.credit.push_back(Credit { seq, at_ms: now_ms });
            while self.credit.len() > CREDIT_CAP {
                self.credit.pop_front();
            }
        }
        class
    }

    /// A late acknowledgement of `seq`: its verdict was wrong, withdraw it.
    pub(crate) fn on_late_ack(&mut self, seq: u32) {
        self.local.remove(&seq);
        if let Some(i) = self.credit.iter().position(|c| c.seq == seq) {
            self.credit.remove(i);
            self.stats.withdrawn += 1;
        }
    }

    /// A LOSS_REPORT with `lost` losses, its range ending at `last_seq`: how
    /// many of them the path is not charged for. The credit is spent.
    pub(crate) fn excuse(&mut self, lost: u16, last_seq: u32, now_ms: u64) -> u16 {
        self.expire(now_ms);
        self.last_report_seq = Some(self.last_report_seq.map_or(last_seq, |l| l.max(last_seq)));
        let n = lost.min(u16::try_from(self.credit.len()).unwrap_or(u16::MAX));
        self.credit.drain(..usize::from(n));
        n
    }

    /// Note that `n` losses were charged off (the estimator did not skip the
    /// report whole).
    pub(crate) fn note_excused(&mut self, n: u16) {
        self.stats.losses_excused += u64::from(n);
    }

    fn trim_peak(&mut self, now_ms: u64) {
        while self
            .peak
            .front()
            .is_some_and(|&(t, _)| t + WINDOW_MS < now_ms)
        {
            self.peak.pop_front();
        }
    }

    fn expire(&mut self, now_ms: u64) {
        while self
            .credit
            .front()
            .is_some_and(|c| c.at_ms + CREDIT_TTL_MS <= now_ms)
        {
            self.credit.pop_front();
            self.stats.expired += 1;
        }
    }

    #[cfg(test)]
    pub(crate) fn credit_len(&self) -> usize {
        self.credit.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ack `seq` with a queueing delay of `q` ms over a 20 ms base at time
    /// `t` (departure stamped at `t - 20 - q`, arrival at `t`).
    fn ack(c: &mut QueueLossClassifier, seq: u32, q: i32, t: u64) -> i32 {
        let dep = t - 20 - q as u64;
        c.on_ack(seq, (t & 0xFFFF) as u16, dep, t)
    }

    #[test]
    fn q_is_the_delay_over_the_windowed_minimum() {
        let mut c = QueueLossClassifier::default();
        assert_eq!(ack(&mut c, 0, 0, 1_000), 0);
        assert_eq!(ack(&mut c, 1, 80, 1_100), 80);
        // The base is the minimum of the last 10 s: it falls out of the
        // window and a later, larger floor takes its place.
        assert_eq!(ack(&mut c, 2, 30, 12_000), 0);
    }

    #[test]
    fn the_floor_and_the_peak_set_the_threshold() {
        let mut c = QueueLossClassifier::default();
        assert_eq!(c.threshold_ms(), Q_FLOOR_MS);
        ack(&mut c, 0, 0, 1_000);
        ack(&mut c, 1, Q_FLOOR_MS - 1, 1_100);
        assert_eq!(
            c.on_verdict(2, true, 1_200),
            LossClass::Path,
            "below the floor"
        );
        ack(&mut c, 3, Q_FLOOR_MS, 1_300);
        assert_eq!(
            c.on_verdict(4, true, 1_400),
            LossClass::Queue,
            "at the floor"
        );
        // A deeper queue seen lately lifts the bar to 0.8 of it.
        ack(&mut c, 5, 200, 1_500);
        ack(&mut c, 6, 100, 1_600);
        assert_eq!(c.threshold_ms(), 160);
        assert_eq!(c.on_verdict(7, true, 1_700), LossClass::Path, "100 < 160");
        ack(&mut c, 8, 170, 1_800);
        assert_eq!(c.on_verdict(9, true, 1_900), LossClass::Queue);
        // The peak is the last 10 s: 12 s later the bar is back at the floor.
        ack(&mut c, 20, 0, 13_000);
        assert_eq!(c.threshold_ms(), Q_FLOOR_MS);
    }

    #[test]
    fn the_neighbour_is_the_nearest_acked_bundle_before_it() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, 0, 0, 1_000);
        ack(&mut c, 10, 120, 1_100);
        ack(&mut c, 20, 0, 1_200);
        // 15's neighbour before is 10 (deep), 25's is 20 (shallow); an ack
        // after a loss does not speak for it.
        assert_eq!(c.on_verdict(15, true, 1_300), LossClass::Queue);
        assert_eq!(c.on_verdict(25, true, 1_300), LossClass::Path);
        assert_eq!(
            c.on_verdict(0, true, 1_300),
            LossClass::Path,
            "no neighbour"
        );
    }

    #[test]
    fn repair_and_control_bundles_are_ignored() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, 0, 120, 1_000);
        assert_eq!(c.on_verdict(5, false, 1_100), LossClass::Ignored);
        assert_eq!(c.credit_len(), 0);
        assert_eq!(c.stats().queue_losses, 0);
    }

    #[test]
    fn a_report_spends_the_credit_up_to_what_it_lost() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, 0, 0, 900);
        ack(&mut c, 0, 150, 1_000);
        for s in 1..=4 {
            assert_eq!(c.on_verdict(s, true, 1_100), LossClass::Queue);
        }
        assert_eq!(c.excuse(3, 10, 1_200), 3, "a report of 3 spends 3");
        assert_eq!(c.credit_len(), 1);
        assert_eq!(c.excuse(7, 20, 1_300), 1, "min(credit, lost)");
        assert_eq!(c.excuse(7, 30, 1_400), 0, "spent");
    }

    #[test]
    fn a_credit_expires_so_it_cannot_hide_later_random_loss() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, 0, 0, 900);
        ack(&mut c, 0, 150, 1_000);
        c.on_verdict(1, true, 1_100);
        assert_eq!(c.excuse(1, 10, 1_100 + CREDIT_TTL_MS), 0);
        assert_eq!(c.stats().expired, 1);
    }

    #[test]
    fn a_late_ack_withdraws_the_credit() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, 0, 0, 900);
        ack(&mut c, 0, 150, 1_000);
        c.on_verdict(1, true, 1_100);
        c.on_verdict(2, true, 1_100);
        c.on_late_ack(1);
        assert_eq!(c.stats().withdrawn, 1);
        assert_eq!(c.excuse(5, 10, 1_200), 1);
        // A late ack of a verdict already spent does nothing.
        c.on_late_ack(2);
        assert_eq!(c.stats().withdrawn, 1);
    }

    #[test]
    fn a_local_discard_is_a_local_loss_whatever_the_queue_said() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, 0, 0, 1_000);
        c.on_local_discard(&[3, 4], 5);
        assert_eq!(c.on_verdict(3, true, 1_100), LossClass::Local);
        assert_eq!(c.on_verdict(5, true, 1_100), LossClass::Path);
        assert_eq!(c.stats().local_discards, 5);
        assert_eq!(c.stats().black_holes, 1);
        assert_eq!(c.excuse(9, 10, 1_200), 1);
        // A discarded bundle that is then acked late is withdrawn.
        c.on_late_ack(4);
        assert_eq!(c.on_verdict(4, true, 1_300), LossClass::Path);
    }

    #[test]
    fn a_verdict_after_the_report_that_reached_it_is_counted() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, 0, 0, 900);
        ack(&mut c, 0, 150, 1_000);
        assert_eq!(
            c.excuse(2, 40, 1_100),
            0,
            "nothing judged yet: the race lost"
        );
        assert_eq!(c.on_verdict(30, true, 1_150), LossClass::Queue);
        assert_eq!(c.stats().verdicts_behind_report, 1);
        assert_eq!(c.stats().credits_behind_report, 1);
        // The credit waits for the next report.
        assert_eq!(c.excuse(2, 50, 1_400), 1);
    }
}
