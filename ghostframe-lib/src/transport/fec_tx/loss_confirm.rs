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
//! confirms within [`TTL_MS`] is dropped, as is a budget nobody spends. A
//! verdict a late acknowledgement contradicts is withdrawn.
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

/// How long a verdict waits for a report, and a report's gaps for a verdict, ms.
/// The loss path's horizon is about 600 ms and the client reports every
/// 250 ms, so a real loss is confirmed well inside this; a longer wait lets a
/// stale phantom take a later loss's gap.
pub(crate) const TTL_MS: u64 = 2_000;
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
    held: BTreeMap<i64, (u64, T)>,
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
        self.held.is_empty() && self.budgets.is_empty() && self.pushed_out.is_empty()
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
    pub(crate) fn on_verdict(&mut self, seq: u32, item: T, now_ms: u64) -> Option<T> {
        let u = self.seq.advance(seq);
        if let Some(b) = self
            .budgets
            .iter_mut()
            .find(|b| b.remaining > 0 && b.last >= u)
        {
            b.remaining -= 1;
            self.stats.confirmed += 1;
            return Some(item);
        }
        self.held.insert(u, (now_ms, item));
        while self.held.len() > HELD_CAP {
            if let Some((_, (_, old))) = self.held.pop_first() {
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
            if let Some((_, item)) = self.held.remove(&k) {
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

    /// Drop what has waited [`TTL_MS`]; returns the verdicts no report
    /// confirmed (the pushed-out ones too), for the caller to count.
    pub(crate) fn expire(&mut self, now_ms: u64) -> Vec<T> {
        let mut out = std::mem::take(&mut self.pushed_out);
        let stale: Vec<i64> = self
            .held
            .iter()
            .filter(|(_, (at, _))| at + TTL_MS <= now_ms)
            .map(|(&k, _)| k)
            .collect();
        for k in stale {
            if let Some((_, item)) = self.held.remove(&k) {
                self.stats.unconfirmed += 1;
                out.push(item);
            }
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
            .filter_map(|s| c.on_verdict(s, s, at))
            .collect()
    }

    #[test]
    fn a_stall_that_makes_verdicts_but_no_client_gaps_reports_nothing() {
        let mut c = LossConfirmer::default();
        // 500 phantoms at once, the client has them all: its report has 0 gaps.
        assert!(verdicts(&mut c, 100..600, 1_000).is_empty());
        assert!(c.on_report(700, 0, 1_100).is_empty());
        assert_eq!(c.held_len(), 500);
        let dropped = c.expire(1_000 + TTL_MS);
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
        assert_eq!(c.on_verdict(480, 480, 1_100), Some(480));
        assert_eq!(c.on_verdict(490, 490, 1_150), Some(490));
        // Spent: the third is held. And one past the report's reach is held.
        assert_eq!(c.on_verdict(495, 495, 1_200), None);
        let mut d = LossConfirmer::default();
        assert!(d.on_report(500, 1, 1_000).is_empty());
        assert_eq!(d.on_verdict(501, 501, 1_100), None, "above the report");
        // A budget nobody spends expires, and is counted.
        let mut e: LossConfirmer<u32> = LossConfirmer::default();
        e.on_report(500, 4, 1_000);
        e.expire(1_000 + TTL_MS);
        assert_eq!(e.stats().gaps_unspent, 4);
        assert_eq!(e.on_verdict(480, 480, 1_000 + TTL_MS), None);
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
        let out = c.expire(1_001);
        assert_eq!(out, vec![0, 1, 2, 3, 4]);
    }
}
