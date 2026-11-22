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
//! Repair and control bundles carry no records and are not given a credit.
//! The client's `lost_pre_fec` counts every `wire_seq` gap, repair bundles
//! included (`src_lost_pre_fec` is never set), so a dropped repair bundle is a
//! loss the gate does not excuse: the gate under-excuses there, which is the
//! safe side (the estimator sees a little more loss than the queue explains).
//!
//! # Sequence numbers wrap
//!
//! `bundle_seq` is a `u32` that wraps by design (`reliable_emitter/wire_seq.rs`).
//! Every key here is *unwrapped* against the newest acknowledged sequence
//! (serial-number arithmetic, `i64`), so the ordered maps stay ordered across
//! the wrap and `pop_first` evicts the oldest, not the post-wrap newest.
//!
//! # Credit and reconciliation
//!
//! A report spends only credits for bundles at or below its own
//! `last_bundle_seq` (a loss above it cannot be in it). A report the
//! standing-queue gate skips whole spends the credits in its range too (those
//! losses were skipped anyway) but leaves the later ones. A late
//! acknowledgement of a bundle whose credit was already spent means a loss was
//! excused that was not one: it becomes a *debit* that reduces the next
//! report's excused count.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// The smallest `q` that can be a queue loss, ms.
pub(crate) const Q_FLOOR_MS: i32 = 80;
/// Share of the 10 s peak of `q` a queue loss must have been sent into.
pub(crate) const Q_FRACTION: f64 = 0.8;
/// Window of the base OWD and of the peak, ms.
const WINDOW_MS: u64 = 10_000;
/// A credit not spent in this long is dropped, ms.
pub(crate) const CREDIT_TTL_MS: u64 = 2_000;
/// A spent credit stays reconcilable, and a debit stays owed, this long, ms.
const SPENT_TTL_MS: u64 = 10_000;
/// A `q` above this is a clock step or drift (the OWD is a 16-bit ms count, so
/// anything near 32 s is a wrap), not a queue: the base is restarted, ms.
pub(crate) const Q_SANE_MS: i32 = 5_000;
/// Bundles whose `q` is kept.
const QMAP_CAP: usize = 16_384;
/// Locally discarded bundles remembered.
const LOCAL_CAP: usize = 4_096;
/// Credits (and spent credits) held.
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
    /// Losses of the reports the standing queue skipped whole.
    pub losses_skipped: u64,
    /// Credits withdrawn by a late acknowledgement, and expired unspent.
    pub withdrawn: u64,
    pub expired: u64,
    /// Late acknowledgements of an already spent credit (a debit against the
    /// next report), and how many excused losses they took back.
    pub debits: u64,
    pub debits_paid: u64,
    /// Clock steps seen (a `q` past [`Q_SANE_MS`]).
    pub clock_resets: u64,
    /// Source-bundle verdicts that came after a report whose range already
    /// reached them (the report may have counted them): the race, as seen
    /// from the ledger. Of those, the queue and local ones.
    pub verdicts_behind_report: u64,
    pub credits_behind_report: u64,
}

/// Unwraps `u32` sequence numbers against the newest one acknowledged.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct SeqUnwrap {
    anchor: Option<i64>,
}

impl SeqUnwrap {
    /// `seq` as an `i64` on the anchor's line (within 2^31 of it).
    pub(super) fn at(&self, seq: u32) -> i64 {
        match self.anchor {
            None => i64::from(seq),
            Some(a) => a + i64::from(seq.wrapping_sub(a as u32) as i32),
        }
    }

    /// As [`Self::at`], moving the anchor up to it.
    pub(super) fn advance(&mut self, seq: u32) -> i64 {
        let u = self.at(seq);
        if self.anchor.is_none_or(|a| u > a) {
            self.anchor = Some(u);
        }
        u
    }
}

/// Sequence numbers with the time they were stamped: ordered by sequence for
/// range spends and O(log n) removal, by time for expiry.
#[derive(Debug, Default)]
struct Timed {
    by_seq: BTreeMap<i64, u64>,
    by_time: VecDeque<(u64, i64)>,
}

impl Timed {
    fn insert(&mut self, seq: i64, at_ms: u64) {
        self.by_seq.insert(seq, at_ms);
        self.by_time.push_back((at_ms, seq));
        while self.by_seq.len() > CREDIT_CAP {
            self.pop_oldest();
        }
    }

    fn remove(&mut self, seq: i64) -> bool {
        self.by_seq.remove(&seq).is_some()
    }

    /// The oldest live entry goes (stale queue slots are skipped).
    fn pop_oldest(&mut self) -> bool {
        while let Some((at, seq)) = self.by_time.pop_front() {
            if self.by_seq.get(&seq) == Some(&at) {
                self.by_seq.remove(&seq);
                return true;
            }
        }
        false
    }

    /// Drop entries older than `ttl`; returns how many were live.
    fn expire(&mut self, now_ms: u64, ttl: u64) -> u64 {
        let mut n = 0;
        while let Some(&(at, seq)) = self.by_time.front() {
            if at + ttl > now_ms {
                break;
            }
            self.by_time.pop_front();
            if self.by_seq.get(&seq) == Some(&at) {
                self.by_seq.remove(&seq);
                n += 1;
            }
        }
        n
    }

    /// Remove up to `n` entries with sequence at or below `max_seq`, lowest
    /// first; returns the sequences removed.
    fn take_upto(&mut self, max_seq: i64, n: usize) -> Vec<(i64, u64)> {
        let keys: Vec<i64> = self
            .by_seq
            .range(..=max_seq)
            .take(n)
            .map(|(&k, _)| k)
            .collect();
        keys.into_iter()
            .filter_map(|k| self.by_seq.remove(&k).map(|at| (k, at)))
            .collect()
    }
}

/// See the module comment.
#[derive(Debug, Default)]
pub(crate) struct QueueLossClassifier {
    reference: Option<u16>,
    /// Sliding minimum of OWD: `(time, owd)`, owd increasing.
    base: VecDeque<(u64, i32)>,
    /// Sliding maximum of `q`: `(time, q)`, q decreasing.
    peak: VecDeque<(u64, i32)>,
    seq: SeqUnwrap,
    q: BTreeMap<i64, i32>,
    /// Bundles quinn discarded locally.
    local: BTreeSet<i64>,
    credit: Timed,
    /// Credits a report spent, kept to reconcile a late acknowledgement.
    spent: Timed,
    /// Excused losses a late acknowledgement then showed were not losses.
    debt: u32,
    debt_at_ms: u64,
    /// `last_bundle_seq` of the newest report folded.
    last_report_seq: Option<i64>,
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
        let mut q = self.push_owd(owd, now_ms);
        if q > Q_SANE_MS {
            // A clock step or drift, not a queue: forget the window and
            // classify nothing on it.
            self.stats.clock_resets += 1;
            self.base.clear();
            self.peak.clear();
            q = self.push_owd(owd, now_ms);
        }
        self.trim_peak(now_ms);
        while self.peak.back().is_some_and(|&(_, p)| p <= q) {
            self.peak.pop_back();
        }
        self.peak.push_back((now_ms, q));
        let u = self.seq.advance(seq);
        self.q.insert(u, q);
        while self.q.len() > QMAP_CAP {
            self.q.pop_first();
        }
        q
    }

    /// Add `owd` to the sliding minimum; returns the `q` it gives.
    fn push_owd(&mut self, owd: i32, now_ms: u64) -> i32 {
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
        owd - self.base.front().map_or(owd, |&(_, o)| o)
    }

    /// quinn discarded `seqs` (source and other bundles) before they reached
    /// the wire; `datagrams` is how many datagrams it discarded in all.
    pub(crate) fn on_local_discard(&mut self, seqs: &[u32], datagrams: u64) {
        self.stats.black_holes += 1;
        self.stats.local_discards += datagrams;
        for &s in seqs {
            let u = self.seq.at(s);
            self.local.insert(u);
        }
        while self.local.len() > LOCAL_CAP {
            self.local.pop_first();
        }
    }

    /// What `on_verdict` would class `seq` as, without touching any state:
    /// no credit is issued, a local mark is not consumed, and the counters do
    /// not move. Source-carrying or not, the question is the same (is it a
    /// loss the stream's own queue or quinn's own discard explains), which is
    /// what the bandwidth estimator is asked. The threshold reads only the
    /// window's entries still in date at `now_ms`.
    pub(crate) fn peek_class(&self, seq: u32, now_ms: u64) -> LossClass {
        let u = self.seq.at(seq);
        if self.local.contains(&u) {
            return LossClass::Local;
        }
        let q_max = self
            .peak
            .iter()
            .find(|&&(t, _)| t + WINDOW_MS >= now_ms)
            .map_or(0, |&(_, q)| q);
        let threshold = Q_FLOOR_MS.max((Q_FRACTION * f64::from(q_max)) as i32);
        if self
            .q
            .range(..u)
            .next_back()
            .is_some_and(|(_, &q)| q >= threshold)
        {
            LossClass::Queue
        } else {
            LossClass::Path
        }
    }

    /// The ledger judged `seq` lost. `has_records` is whether it carried
    /// source content. A queue or local loss earns a report a credit.
    pub(crate) fn on_verdict(&mut self, seq: u32, has_records: bool, now_ms: u64) -> LossClass {
        self.trim_peak(now_ms);
        let u = self.seq.at(seq);
        let local = self.local.remove(&u);
        if !has_records {
            return LossClass::Ignored;
        }
        let class = if local {
            LossClass::Local
        } else if self
            .q
            .range(..u)
            .next_back()
            .is_some_and(|(_, &q)| q >= self.threshold_ms())
        {
            LossClass::Queue
        } else {
            LossClass::Path
        };
        let behind = self.last_report_seq.is_some_and(|l| u <= l);
        self.stats.verdicts_behind_report += u64::from(behind);
        if class != LossClass::Path {
            if class == LossClass::Queue {
                self.stats.queue_losses += 1;
            } else {
                self.stats.local_losses += 1;
            }
            self.stats.credits_behind_report += u64::from(behind);
            self.expire(now_ms);
            self.credit.insert(u, now_ms);
        }
        class
    }

    /// A late acknowledgement of `seq`: its verdict was wrong. An unspent
    /// credit is withdrawn; a spent one excused a loss that was not one, and
    /// is owed back against the next report.
    pub(crate) fn on_late_ack(&mut self, seq: u32, now_ms: u64) {
        let u = self.seq.at(seq);
        self.local.remove(&u);
        if self.credit.remove(u) {
            self.stats.withdrawn += 1;
        } else if self.spent.remove(u) {
            self.stats.debits += 1;
            if self.debt == 0 {
                self.debt_at_ms = now_ms;
            }
            self.debt = self.debt.saturating_add(1);
        }
    }

    /// A LOSS_REPORT with `lost` losses, its range ending at `last_seq`: how
    /// many of them the path is not charged for. Only credits for bundles at
    /// or below `last_seq` are spent. `skipped`: the standing-queue gate skips
    /// the whole report, so those credits are consumed (the losses are out of
    /// the estimate already) but nothing is excused or owed.
    pub(crate) fn excuse(&mut self, lost: u16, last_seq: u32, now_ms: u64, skipped: bool) -> u16 {
        self.expire(now_ms);
        self.spent.expire(now_ms, SPENT_TTL_MS);
        if self.debt > 0 && self.debt_at_ms + SPENT_TTL_MS <= now_ms {
            self.debt = 0;
        }
        let last = self.seq.at(last_seq);
        self.last_report_seq = Some(self.last_report_seq.map_or(last, |l| l.max(last)));
        let taken = self.credit.take_upto(last, usize::from(lost));
        if skipped {
            self.stats.losses_skipped += u64::from(lost);
            return 0;
        }
        let n = u16::try_from(taken.len()).unwrap_or(u16::MAX);
        for (seq, _) in taken {
            self.spent.insert(seq, now_ms);
        }
        let pay = u32::from(n).min(self.debt);
        self.debt -= pay;
        self.stats.debits_paid += u64::from(pay);
        let excused = n - pay as u16;
        self.stats.losses_excused += u64::from(excused);
        excused
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
        self.stats.expired += self.credit.expire(now_ms, CREDIT_TTL_MS);
    }

    #[cfg(test)]
    pub(crate) fn credit_len(&self) -> usize {
        self.credit.by_seq.len()
    }

    #[cfg(test)]
    pub(crate) fn q_len(&self) -> usize {
        self.q.len()
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
    fn peek_class_agrees_with_the_verdict_and_changes_nothing() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, 0, 0, 1_000);
        ack(&mut c, 10, 120, 1_100);
        ack(&mut c, 20, 0, 1_200);
        c.on_local_discard(&[30], 1);
        let before = (c.stats(), c.credit_len());
        for (seq, want) in [
            (15, LossClass::Queue),
            (25, LossClass::Path),
            (0, LossClass::Path),
            (30, LossClass::Local),
        ] {
            assert_eq!(c.peek_class(seq, 1_300), want, "peek {seq}");
        }
        assert_eq!(before, (c.stats(), c.credit_len()), "a peek is read-only");
        // The local mark survives the peek, so the verdict still sees it.
        assert_eq!(c.on_verdict(30, true, 1_300), LossClass::Local);
        // A peak out of the window no longer raises the threshold.
        assert_eq!(c.peek_class(15, 1_100 + WINDOW_MS + 1), LossClass::Queue);
        let mut d = QueueLossClassifier::default();
        ack(&mut d, 0, 0, 1_000);
        ack(&mut d, 10, 400, 1_100);
        ack(&mut d, 20, 100, 1_200);
        assert_eq!(d.peek_class(25, 1_300), LossClass::Path, "100 < 0.8 x 400");
        assert_eq!(
            d.peek_class(25, 1_100 + WINDOW_MS + 1),
            LossClass::Queue,
            "the 400 ms peak aged out"
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
        assert_eq!(c.excuse(3, 10, 1_200, false), 3, "a report of 3 spends 3");
        assert_eq!(c.credit_len(), 1);
        assert_eq!(c.excuse(7, 20, 1_300, false), 1, "min(credit, lost)");
        assert_eq!(c.excuse(7, 30, 1_400, false), 0, "spent");
    }

    #[test]
    fn a_credit_expires_so_it_cannot_hide_later_random_loss() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, 0, 0, 900);
        ack(&mut c, 0, 150, 1_000);
        c.on_verdict(1, true, 1_100);
        assert_eq!(c.excuse(1, 10, 1_100 + CREDIT_TTL_MS, false), 0);
        assert_eq!(c.stats().expired, 1);
    }

    #[test]
    fn a_late_ack_withdraws_the_credit() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, 0, 0, 900);
        ack(&mut c, 0, 150, 1_000);
        c.on_verdict(1, true, 1_100);
        c.on_verdict(2, true, 1_100);
        c.on_late_ack(1, 1_150);
        assert_eq!(c.stats().withdrawn, 1);
        assert_eq!(c.excuse(5, 10, 1_200, false), 1);
    }

    #[test]
    fn a_late_ack_after_the_credit_was_spent_is_owed_against_the_next_report() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, 0, 0, 900);
        ack(&mut c, 0, 150, 1_000);
        for s in 1..=3 {
            c.on_verdict(s, true, 1_100);
        }
        assert_eq!(c.excuse(2, 10, 1_200, false), 2, "seqs 1 and 2 excused");
        // Seq 1 turns out to have arrived: that excused loss was a real one.
        c.on_late_ack(1, 1_250);
        assert_eq!(c.stats().debits, 1);
        assert_eq!(c.stats().withdrawn, 0);
        // The next report's excused count pays it back: 1 credit left, owed 1.
        assert_eq!(c.excuse(5, 20, 1_300, false), 0);
        assert_eq!(c.stats().debits_paid, 1);
        // Paid once only.
        c.on_verdict(4, true, 1_320);
        assert_eq!(c.excuse(5, 30, 1_400, false), 1);
        // An ack for something never spent is no debit.
        c.on_late_ack(99, 1_500);
        assert_eq!(c.stats().debits, 1);
    }

    #[test]
    fn a_report_spends_only_credits_inside_its_range() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, 0, 0, 900);
        ack(&mut c, 0, 150, 1_000);
        for s in [5, 6, 50, 51] {
            c.on_verdict(s, true, 1_100);
        }
        // A report that reaches bundle 10 cannot hold 50 or 51.
        assert_eq!(c.excuse(4, 10, 1_200, false), 2);
        assert_eq!(c.credit_len(), 2);
        // A skipped report consumes its range's credits and excuses nothing,
        // the later ones wait.
        c.on_verdict(7, true, 1_210);
        assert_eq!(c.excuse(4, 10, 1_220, true), 0);
        assert_eq!(c.credit_len(), 2, "7 was consumed, 50 and 51 remain");
        assert_eq!(c.stats().losses_skipped, 4);
        assert_eq!(c.excuse(4, 60, 1_300, false), 2);
    }

    #[test]
    fn the_classifier_works_across_the_sequence_wrap() {
        let mut c = QueueLossClassifier::default();
        let start = u32::MAX - 20;
        // 100 acked bundles straddling the wrap, the deep queue all along.
        ack(&mut c, start.wrapping_sub(1), 0, 900);
        for i in 0..100u32 {
            ack(&mut c, start.wrapping_add(i), 150, 1_000 + u64::from(i));
        }
        assert_eq!(c.q_len(), 101, "nothing evicted by the wrap");
        // A loss after the wrap finds its pre-wrap and post-wrap neighbours.
        assert_eq!(c.on_verdict(5, true, 1_200), LossClass::Queue);
        assert_eq!(c.on_verdict(u32::MAX - 3, true, 1_200), LossClass::Queue);
        // A report whose last_bundle_seq is post-wrap spends a pre-wrap credit
        // and one on the other side of it.
        assert_eq!(c.excuse(5, 10, 1_300, false), 2);
        let s = c.stats();
        assert_eq!(s.queue_losses, 2);
        // `verdicts_behind_report` follows the wrap too: seq 6 is below 10.
        c.on_verdict(6, true, 1_400);
        assert_eq!(c.stats().verdicts_behind_report, 1);
    }

    #[test]
    fn the_cap_evicts_the_oldest_across_the_wrap_not_the_newest() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, u32::MAX - 10, 0, 900);
        let n = (QMAP_CAP + 100) as u32;
        for i in 0..n {
            ack(&mut c, (u32::MAX - 10).wrapping_add(i), 150, 1_000);
        }
        assert_eq!(c.q_len(), QMAP_CAP);
        // The newest bundle is still there: its successor finds it.
        let next = (u32::MAX - 10).wrapping_add(n);
        assert_eq!(c.on_verdict(next, true, 1_100), LossClass::Queue);
    }

    #[test]
    fn a_clock_step_restarts_the_window_instead_of_excusing_everything() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, 0, 0, 1_000);
        // The client's clock steps 6 s: the next OWD reads 6 s over the base.
        let q = c.on_ack(1, ((1_100 + 6_000u64) & 0xFFFF) as u16, 1_100 - 20, 1_100);
        assert_eq!(q, 0, "not a 6 s queue");
        assert_eq!(c.stats().clock_resets, 1);
        assert_eq!(c.threshold_ms(), Q_FLOOR_MS, "the peak did not take it");
        assert_eq!(c.on_verdict(2, true, 1_200), LossClass::Path);
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
        assert_eq!(c.excuse(9, 10, 1_200, false), 1);
        // A discarded bundle that is then acked late is withdrawn.
        c.on_late_ack(4, 1_250);
        assert_eq!(c.on_verdict(4, true, 1_300), LossClass::Path);
    }

    #[test]
    fn a_verdict_after_the_report_that_reached_it_is_counted() {
        let mut c = QueueLossClassifier::default();
        ack(&mut c, 0, 0, 900);
        ack(&mut c, 0, 150, 1_000);
        assert_eq!(
            c.excuse(2, 40, 1_100, false),
            0,
            "nothing judged yet: the race lost"
        );
        assert_eq!(c.on_verdict(30, true, 1_150), LossClass::Queue);
        assert_eq!(c.stats().verdicts_behind_report, 1);
        assert_eq!(c.stats().credits_behind_report, 1);
        // The credit waits for the next report.
        assert_eq!(c.excuse(2, 50, 1_400, false), 1);
    }
}
