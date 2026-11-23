// SPDX-License-Identifier: AGPL-3.0-or-later
//! Losses the client has confirmed.
//!
//! Pure and sans-IO: every call takes its time (ms on any monotonic clock).
//!
//! # Why
//!
//! The ledger declares a transmission lost when the acknowledgements that
//! should have named it have not (a packet or time threshold). When the ack
//! path stalls, hundreds are declared at once for datagrams the client holds:
//! phantom losses. Handed to goog_cc's loss-based estimator they read as
//! congestion and pin the estimate to its floor. Only the receiver knows what
//! it did not get, and it says so in its LOSS_REPORT: `lost_pre_fec`, the
//! number of `wire_seq` gaps it still had after its reorder window, and
//! `last_bundle_seq`, the newest sequence the report reaches.
//!
//! The report is a count, not a list, and it is filed when the client closes
//! the gap, which can be after the range that holds the sequence, so it names
//! no sequence. What it does give is a **budget**: a report with `n` gaps up to
//! `last` confirms at most `n` of the declared losses at or below `last`.
//!
//! # Rule
//!
//! A verdict is held. It is released (for goog_cc to be told of) when a budget
//! covers it: an earlier report's unspent gaps (`last >= seq`), or the next
//! report's. Whichever of the two comes second finds the other waiting. Of the
//! verdicts a report covers the oldest go first. A held verdict nobody
//! confirms is dropped once it is [`TTL_MS`] old *and* [`MIN_NEWER_SENT`]
//! newer sequences have been sent since it (or at [`MAX_AGE_MS`] whatever was
//! sent); a budget nobody spends is dropped after [`TTL_MS`]. A verdict a late
//! acknowledgement contradicts is withdrawn.
//!
//! The property that matters: the losses released never exceed the gaps the
//! client reported. A stall, which makes verdicts for datagrams the client
//! has, adds none to that count and so releases none. Which verdict a gap is
//! spent on is a guess (a stall's phantoms can take a real loss's gap), but the
//! number goes to goog_cc exactly as the client counted it.
//!
//! Sequence numbers wrap; the keys are unwrapped against the newest seen.

use super::queue_loss::SeqUnwrap;
use std::collections::{BTreeMap, VecDeque};

/// The shortest time a verdict waits for a report, and how long a report's
/// gaps wait for a verdict, ms. The loss path's horizon is about 600 ms and
/// the client reports every 250 ms, so at a normal rate a real loss is
/// confirmed well inside this; a longer wait lets a stale phantom take a
/// later loss's gap.
///
/// That holds only while the stream is fast. The client counts a gap after
/// [`CLIENT_REORDER_WINDOW`] newer sequences have arrived, so at a low rate
/// (the 300 kbit/s floor is about 30 datagrams/s) the confirmation comes
/// seconds after the loss. A held verdict is therefore also kept until
/// [`MIN_NEWER_SENT`] sequences have been sent past it.
///
/// A budget keeps the plain time rule: it is spent by the ledger's verdicts,
/// which come from acknowledgements and the loss path's own timers, not from
/// the client's reorder window, and a budget that waits for long is how a
/// stale phantom takes a later loss's gap.
pub(crate) const TTL_MS: u64 = 2_000;
/// The client's reorder window: the number of newer `wire_seq`s it must have
/// received before it counts a gap
/// (`ghostframe-client-core/src/wire_loss.rs`, `REORDER_WINDOW`). This is a
/// copy; the two must agree.
const CLIENT_REORDER_WINDOW: i64 = 64;
/// Sequences that must have been sent past a held verdict before it may
/// expire: twice the client's window, so a report that needs the window to
/// fill has had it fill, and a further window of margin for the datagrams the
/// network drops on the way.
pub(crate) const MIN_NEWER_SENT: i64 = 2 * CLIENT_REORDER_WINDOW;
/// The most a verdict waits whatever was sent (an idle stream sends nothing,
/// so the sequence rule alone would hold it for ever), ms.
pub(crate) const MAX_AGE_MS: u64 = 10_000;
/// Verdicts held.
const HELD_CAP: usize = 4_096;
/// Reports whose gaps are unspent.
const BUDGET_CAP: usize = 64;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LossConfirmStats {
    /// Verdicts a report's gaps confirmed.
    pub confirmed: u64,
    /// Verdicts no report confirmed (expired or pushed out): phantoms.
    pub unconfirmed: u64,
    /// Verdicts a late acknowledgement withdrew while they were held.
    pub withdrawn: u64,
    /// Gaps a report named that no verdict took in time.
    pub gaps_unspent: u64,
}

#[derive(Debug, Clone, Copy)]
struct Budget {
    /// Newest sequence the report reaches.
    last: i64,
    remaining: u16,
    at_ms: u64,
}

/// See the module comment. `T` is what the caller reports once confirmed.
#[derive(Debug)]
pub(crate) struct LossConfirmer<T> {
    seq: SeqUnwrap,
    /// Per verdict: when it was held, the newest sequence sent at that
    /// moment, and the item.
    held: BTreeMap<i64, (u64, i64, T)>,
    /// `(held at, key)` in the order they were held, for `expire` to stop at
    /// the first verdict that may not expire yet. An entry whose verdict was
    /// confirmed, withdrawn or pushed out is skipped when it reaches the front.
    by_time: VecDeque<(u64, i64)>,
    budgets: VecDeque<Budget>,
    /// Verdicts pushed out by the cap, handed back by the next `expire`.
    pushed_out: Vec<T>,
    stats: LossConfirmStats,
}

impl<T> Default for LossConfirmer<T> {
    fn default() -> Self {
        Self {
            seq: SeqUnwrap::default(),
            held: BTreeMap::new(),
            by_time: VecDeque::new(),
            budgets: VecDeque::new(),
            pushed_out: Vec::new(),
            stats: LossConfirmStats::default(),
        }
    }
}

impl<T> LossConfirmer<T> {
    pub(crate) fn stats(&self) -> LossConfirmStats {
        self.stats
    }

    pub(crate) fn held_len(&self) -> usize {
        self.held.len()
    }

    /// Nothing held, no budget, nothing pushed out: `expire` has no work.
    pub(crate) fn is_idle(&self) -> bool {
        self.held.is_empty()
            && self.by_time.is_empty()
            && self.budgets.is_empty()
            && self.pushed_out.is_empty()
    }

    /// A different connection is live: its sequence numbers are not these.
    /// The counters stay.
    pub(crate) fn reset_session(&mut self) {
        *self = Self {
            stats: self.stats,
            ..Self::default()
        };
    }

    /// The ledger judged `seq` lost. Returns `item` if a report's unspent gap
    /// already covers it, else holds it.
    ///
    /// `newest_sent` is the newest `wire_seq` sent at this moment: the
    /// [`MIN_NEWER_SENT`] wait counts from it, not from the verdict's own
    /// sequence (see `expire`).
    pub(crate) fn on_verdict(
        &mut self,
        seq: u32,
        item: T,
        now_ms: u64,
        newest_sent: Option<u32>,
    ) -> Option<T> {
        let u = self.seq.advance(seq);
        let newest_at = newest_sent.map_or(u, |n| self.seq.advance(n).max(u));
        if let Some(b) = self
            .budgets
            .iter_mut()
            .find(|b| b.remaining > 0 && b.last >= u)
        {
            b.remaining -= 1;
            self.stats.confirmed += 1;
            return Some(item);
        }
        self.held.insert(u, (now_ms, newest_at, item));
        self.by_time.push_back((now_ms, u));
        while self.held.len() > HELD_CAP {
            if let Some((_, (_, _, old))) = self.held.pop_first() {
                self.stats.unconfirmed += 1;
                self.pushed_out.push(old);
            }
        }
        None
    }

    /// A LOSS_REPORT reached `last_seq` and counted `gaps`. Returns the held
    /// verdicts it confirms, oldest first; any gaps left wait for a verdict.
    pub(crate) fn on_report(&mut self, last_seq: u32, gaps: u16, now_ms: u64) -> Vec<T> {
        let last = self.seq.advance(last_seq);
        let keys: Vec<i64> = self
            .held
            .range(..=last)
            .take(usize::from(gaps))
            .map(|(&k, _)| k)
            .collect();
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            if let Some((_, _, item)) = self.held.remove(&k) {
                out.push(item);
            }
        }
        self.stats.confirmed += out.len() as u64;
        let remaining = gaps - out.len() as u16;
        if remaining > 0 {
            self.budgets.push_back(Budget {
                last,
                remaining,
                at_ms: now_ms,
            });
            while self.budgets.len() > BUDGET_CAP {
                if let Some(b) = self.budgets.pop_front() {
                    self.stats.gaps_unspent += u64::from(b.remaining);
                }
            }
        }
        out
    }

    /// `seq` was acknowledged after it was judged lost: it was not lost.
    pub(crate) fn withdraw(&mut self, seq: u32) -> bool {
        let u = self.seq.at(seq);
        let gone = self.held.remove(&u).is_some();
        self.stats.withdrawn += u64::from(gone);
        gone
    }

    /// Drop the verdicts that have waited long enough (see the module
    /// comment); `newest_sent` is the newest `wire_seq` sent so far. Returns the
    /// verdicts no report confirmed (the pushed-out ones too), for the caller to
    /// count.
    ///
    /// Only the front of the time queue is looked at, so a turn with nothing to
    /// drop costs O(1). A verdict held behind one that may not expire yet waits
    /// for it, at most until [`MAX_AGE_MS`].
    pub(crate) fn expire(&mut self, now_ms: u64, newest_sent: Option<u32>) -> Vec<T> {
        let mut out = std::mem::take(&mut self.pushed_out);
        let newest = newest_sent.map(|n| self.seq.at(n));
        while let Some(&(at, key)) = self.by_time.front() {
            let Some(&(a, newest_at, _)) = self.held.get(&key) else {
                self.by_time.pop_front();
                continue;
            };
            if a != at {
                self.by_time.pop_front();
                continue;
            }
            let age = now_ms.saturating_sub(at);
            // Counted from what had been sent when the verdict was made. From
            // the verdict's own sequence, a long run satisfies it itself: the
            // run's later datagrams are `newer`, so a verdict at the start of
            // a 344-datagram outage was "passed" at once, and expired at the
            // TTL before the run closed (the client counts a run's gaps only
            // when it ends, 65 arrivals after the last lost one).
            let passed = newest.is_some_and(|n| n - newest_at >= MIN_NEWER_SENT);
            let may_expire = age >= MAX_AGE_MS || (age >= TTL_MS && passed);
            if !may_expire {
                break;
            }
            self.by_time.pop_front();
            if let Some((_, _, item)) = self.held.remove(&key) {
                self.stats.unconfirmed += 1;
                out.push(item);
            }
        }
        // Confirmations and withdrawals leave their entries behind.
        if self.by_time.len() > 2 * self.held.len() + 64 {
            let held = &self.held;
            self.by_time
                .retain(|(at, k)| held.get(k).is_some_and(|(a, _, _)| a == at));
        }
        while self
            .budgets
            .front()
            .is_some_and(|b| b.at_ms + TTL_MS <= now_ms)
        {
            if let Some(b) = self.budgets.pop_front() {
                self.stats.gaps_unspent += u64::from(b.remaining);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdicts(
        c: &mut LossConfirmer<u32>,
        seqs: impl IntoIterator<Item = u32>,
        at: u64,
    ) -> Vec<u32> {
        seqs.into_iter()
            .filter_map(|s| c.on_verdict(s, s, at, None))
            .collect()
    }

    #[test]
    fn a_stall_that_makes_verdicts_but_no_client_gaps_reports_nothing() {
        let mut c = LossConfirmer::default();
        // 500 phantoms at once, the client has them all: its report has 0 gaps.
        assert!(verdicts(&mut c, 100..600, 1_000).is_empty());
        assert!(c.on_report(700, 0, 1_100).is_empty());
        assert_eq!(c.held_len(), 500);
        let dropped = c.expire(1_000 + TTL_MS, Some(800));
        assert_eq!(dropped.len(), 500, "unconfirmed, so dropped");
        let s = c.stats();
        assert_eq!((s.confirmed, s.unconfirmed), (0, 500));
    }

    #[test]
    fn what_is_released_never_exceeds_what_the_client_counted() {
        let mut c = LossConfirmer::default();
        let mut released = verdicts(&mut c, 100..400, 1_000);
        released.extend(c.on_report(399, 7, 1_100));
        released.extend(c.on_report(399, 3, 1_350));
        released.extend(verdicts(&mut c, 400..450, 1_400));
        released.extend(c.on_report(449, 2, 1_500));
        assert_eq!(released.len(), 7 + 3 + 2);
        assert_eq!(c.stats().confirmed, 12);
        // Oldest first.
        assert_eq!(&released[..7], &[100, 101, 102, 103, 104, 105, 106]);
    }

    #[test]
    fn a_report_before_its_verdicts_waits_for_them() {
        let mut c = LossConfirmer::default();
        assert!(c.on_report(500, 2, 1_000).is_empty());
        // Covered by the report's gaps: released at once.
        assert_eq!(c.on_verdict(480, 480, 1_100, None), Some(480));
        assert_eq!(c.on_verdict(490, 490, 1_150, None), Some(490));
        // Spent: the third is held. And one past the report's reach is held.
        assert_eq!(c.on_verdict(495, 495, 1_200, None), None);
        let mut d = LossConfirmer::default();
        assert!(d.on_report(500, 1, 1_000).is_empty());
        assert_eq!(
            d.on_verdict(501, 501, 1_100, None),
            None,
            "above the report"
        );
        // A budget nobody spends expires, and is counted.
        let mut e: LossConfirmer<u32> = LossConfirmer::default();
        e.on_report(500, 4, 1_000);
        e.expire(1_000 + TTL_MS, None);
        assert_eq!(e.stats().gaps_unspent, 4);
        assert_eq!(e.on_verdict(480, 480, 1_000 + TTL_MS, None), None);
    }

    #[test]
    fn a_late_acknowledgement_withdraws_a_held_verdict() {
        let mut c = LossConfirmer::default();
        verdicts(&mut c, 10..14, 1_000);
        assert!(c.withdraw(11));
        assert!(!c.withdraw(11));
        assert_eq!(c.on_report(20, 4, 1_100), vec![10, 12, 13]);
        assert_eq!(c.stats().withdrawn, 1);
    }

    #[test]
    fn sequence_numbers_wrap() {
        let mut c = LossConfirmer::default();
        let near = u32::MAX - 2;
        let held = verdicts(&mut c, [near, near + 1, near + 2, 0, 1], 1_000);
        assert!(held.is_empty());
        // The report reaches seq 0 across the wrap: it covers the three before.
        assert_eq!(c.on_report(0, 4, 1_100), vec![near, near + 1, near + 2, 0]);
        assert_eq!(c.held_len(), 1);
    }

    #[test]
    fn the_cap_pushes_the_oldest_out_and_hands_it_back() {
        let mut c = LossConfirmer::default();
        verdicts(&mut c, 0..(HELD_CAP as u32 + 5), 1_000);
        assert_eq!(c.held_len(), HELD_CAP);
        let out = c.expire(1_001, None);
        assert_eq!(out, vec![0, 1, 2, 3, 4]);
    }

    /// 30 datagrams/s: the client's window of 64 takes over two seconds to fill,
    /// so its report for a loss comes 2.4 s after the verdict. The old rule (a
    /// plain 2 s wait) had dropped the verdict by then.
    #[test]
    fn at_a_low_rate_a_loss_whose_report_is_late_is_still_confirmed() {
        const RATE: u64 = 30;
        let mut c = LossConfirmer::default();
        assert!(c.on_verdict(100, 100u32, 1_000, None).is_none());
        let mut dropped = Vec::new();
        for t in (1_000..3_400).step_by(33) {
            let sent = 100 + (RATE * (t - 1_000) / 1_000) as u32;
            dropped.extend(c.expire(t, Some(sent)));
        }
        assert!(dropped.is_empty(), "dropped before its report: {dropped:?}");
        assert_eq!(c.on_report(172, 1, 3_400), vec![100]);
        assert_eq!(c.stats().confirmed, 1);
        assert_eq!(c.stats().unconfirmed, 0);
    }

    #[test]
    fn a_verdict_expires_only_when_old_enough_and_passed_by_enough_sequences() {
        let mut c = LossConfirmer::default();
        c.on_verdict(100, 100u32, 1_000, None);
        // Old enough, not passed.
        assert!(c.expire(1_000 + TTL_MS, Some(100 + 127)).is_empty());
        // Passed, not old enough.
        assert!(c.expire(1_000 + TTL_MS - 1, Some(100 + 128)).is_empty());
        // Both.
        assert_eq!(c.expire(1_000 + TTL_MS, Some(100 + 128)), vec![100]);
        // Nothing sent since: goes at the hard maximum, not before.
        c.on_verdict(300, 300, 5_000, None);
        assert!(c.expire(5_000 + MAX_AGE_MS - 1, Some(300)).is_empty());
        assert_eq!(c.expire(5_000 + MAX_AGE_MS, Some(300)), vec![300]);
        assert_eq!(c.stats().unconfirmed, 2);
        assert!(c.is_idle());
    }

    #[test]
    fn expiry_survives_a_sequence_wrap() {
        let mut c = LossConfirmer::default();
        let near = u32::MAX - 10;
        c.on_verdict(near, near, 1_000, None);
        // 128 newer sent: the newest is across the wrap.
        let newest = near.wrapping_add(128);
        assert!(newest < 200);
        assert_eq!(c.expire(1_000 + TTL_MS, Some(newest)), vec![near]);
    }

    #[test]
    fn a_confirmed_verdict_does_not_hold_up_expiry_behind_it() {
        let mut c = LossConfirmer::default();
        c.on_verdict(10, 10u32, 1_000, None);
        c.on_verdict(11, 11, 1_100, None);
        assert_eq!(c.on_report(10, 1, 1_200), vec![10]);
        assert_eq!(c.expire(1_100 + TTL_MS, Some(11 + 128)), vec![11]);
    }

    /// An outage as the sender sees it: its window (`gap` datagrams) goes out
    /// within 200 ms, the stream then stalls for `outage_ms`, and resumes at
    /// `rate` datagrams/s. The ledger judges each lost one 600 ms after its
    /// send. The client counts the run when it closes, 65 arrivals after the
    /// stream resumes (the reorder window and one). Returns what the report
    /// finds still held, and what expired before it.
    ///
    /// Counted from the verdict's own sequence, the run's later datagrams are
    /// "newer", so a verdict at the start of the run was "passed" the moment it
    /// was made, and all but the last 128 went at the TTL: 216 of 344.
    fn outage_heard(gap: u32, rate: u64, outage_ms: u64) -> (usize, u32) {
        let mut c = LossConfirmer::default();
        let first = 1_000u32;
        let burst_ms = 200u64;
        let sent_at = |t: u64| -> u32 {
            if t < burst_ms {
                first + (u64::from(gap) * t / burst_ms) as u32
            } else if t < outage_ms {
                first + gap
            } else {
                first + gap + (rate * (t - outage_ms) / 1_000) as u32
            }
        };
        let close_ms = outage_ms + 65 * 1_000 / rate + 20;
        let mut dropped = 0u32;
        let mut next = 0u32;
        let mut t = 0u64;
        while t <= close_ms {
            // Datagram `next` left at `next / gap * burst`; judged 600 ms later.
            while next < gap && u64::from(next) * burst_ms / u64::from(gap) + 600 <= t {
                let _ = c.on_verdict(first + next, first + next, t, Some(sent_at(t)));
                next += 1;
            }
            dropped += c.expire(t, Some(sent_at(t))).len() as u32;
            t += 10;
        }
        let released = c
            .on_report(first + gap, gap.min(u16::MAX as u32) as u16, close_ms)
            .len();
        (released, dropped)
    }

    #[test]
    fn goog_cc_hears_an_outage_whose_run_closes_late() {
        // 344 lost, the run closing 65 arrivals after the stream resumes: at the
        // 30 datagrams/s floor that is 2.2 s later, after the TTL.
        for rate in [30u64, 300, 3_000] {
            let (released, dropped) = outage_heard(344, rate, 4_000);
            assert_eq!(
                dropped, 0,
                "rate {rate}: verdicts dropped before the report"
            );
            assert_eq!(
                released, 344,
                "rate {rate}: goog_cc heard {released} of 344"
            );
        }
    }

    #[test]
    fn the_wait_counts_from_the_stream_at_the_verdicts_time() {
        let mut c = LossConfirmer::default();
        // A verdict for 100 made when 400 had been sent: 128 newer sent after
        // it means 528, not 228.
        c.on_verdict(100, 100u32, 1_000, Some(400));
        assert!(c.expire(1_000 + TTL_MS, Some(527)).is_empty());
        assert_eq!(c.expire(1_000 + TTL_MS, Some(528)), vec![100]);
    }
}
