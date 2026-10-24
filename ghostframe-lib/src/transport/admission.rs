//! The admission bucket: one allowance, billed by every byte class, and the
//! depth gate in front of quinn's datagram FIFO.
//!
//! # Why
//!
//! Nothing used to read how much was already waiting in quinn's datagram
//! queue. The scheduler's budget was a rate per call, retransmission had a
//! half-billed allowance of its own, parity was unbilled, and the queue was
//! 16 MiB deep, so any excess integrated: under 2 % random loss Cubic shrinks
//! the window, the scheduler keeps enqueuing, and the FIFO stood at 2-5 MB
//! with a median enqueue-to-departure lag over a second. The sender's own
//! queue is the one queue it can bound, and it is the only place a stale tile
//! can still be superseded, so the work that cannot be admitted stays in the
//! scheduler instead of in quinn.
//!
//! # What
//!
//! - **One bucket**, refilled by elapsed time at the sending rate `R`. Every
//!   datagram that leaves for quinn is charged its wire bytes (payload,
//!   headers and the WebTransport framing). Parity is charged when it is
//!   emitted. Idle time never banks more than the burst cap
//!   (`max(2 x MTU, R x one tick)`), which is `RetransmitPacer`'s rule, kept.
//! - **Strict class priority**: NACK-requested repair, then retransmission by
//!   timer, then the critical tier, then the refinement tier, then parity. A
//!   class that cannot afford its next datagram stops; the classes below it
//!   do not jump ahead ([`pull_in_priority`]).
//! - **The depth gate**: a datagram is admitted only while quinn's FIFO holds
//!   less than `D_target = max(1 MTU, R x T_q)`, `T_q` = 10 ms and never more
//!   than 14 ms of the rate. The floor is one datagram because a datagram
//!   cannot be split. The check precedes every send, so the FIFO cannot
//!   exceed `D_target` plus the datagram being admitted, by construction.
//!
//! Everything here is pure arithmetic over values the caller passes in, so
//! the rules can be checked without a connection.

use std::time::{Duration, Instant};

/// Standing queue the sender is willing to make at the host, as time at the
/// sending rate. Under one 30 fps frame (Q2, owner).
pub(crate) const QUEUE_TARGET: Duration = Duration::from_millis(10);

/// The most `QUEUE_TARGET` may ever be: `D_target` never exceeds this much of
/// the rate (above the one-datagram floor).
pub(crate) const QUEUE_BOUND: Duration = Duration::from_millis(14);

const _: () = assert!(QUEUE_TARGET.as_micros() <= QUEUE_BOUND.as_micros());

/// How much of the rate may be banked as a burst: one scheduler tick.
pub(crate) const BURST_WINDOW: Duration = Duration::from_micros(33_333);

/// FEC parity is one datagram per this many sources (the group size `K`), so
/// a puller that spends every token on sources would leave the parity those
/// sources owe nothing to be emitted with.
pub(crate) const PARITY_RESERVE_GROUP: usize = crate::transport::reliable_emitter::FEC_GROUP_SIZE_K;

/// The byte classes, in strict priority order: the first is served first, and
/// a class is served only once every class before it has nothing left that it
/// can afford.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum AdmitClass {
    /// A retransmission the client asked for.
    NackRepair,
    /// A retransmission the sender's own timer fired.
    Retransmit,
    /// First transmissions of the critical tier (priority tiles, passes 0-3).
    Critical,
    /// First transmissions of the refinement tier (passes 4 and up).
    Refinement,
    /// FEC parity.
    Parity,
}

impl AdmitClass {
    /// Highest priority first.
    pub(crate) const PRIORITY: [AdmitClass; 5] = [
        AdmitClass::NackRepair,
        AdmitClass::Retransmit,
        AdmitClass::Critical,
        AdmitClass::Refinement,
        AdmitClass::Parity,
    ];
}

/// `D_target`: the deepest the datagram FIFO may be before another datagram
/// is refused, in bytes, for a sending rate of `rate` bytes per second and a
/// datagram of `mtu` bytes.
///
/// `max(mtu, rate x QUEUE_TARGET)`. Below `mtu / QUEUE_BOUND` per second one
/// datagram alone is deeper than the bound, which is a physical floor and not
/// a tuning choice: the bound there is "one datagram queued".
pub(crate) fn depth_target(rate: f64, mtu: usize) -> usize {
    let t = (rate.max(0.0) * QUEUE_TARGET.as_secs_f64()) as usize;
    t.max(mtu)
}

/// The largest bucket: `max(2 x MTU, rate x BURST_WINDOW)`. Two datagrams so
/// that one of the largest size can always be afforded and a second follow it.
pub(crate) fn burst_cap(rate: f64, mtu: usize) -> f64 {
    ((2 * mtu) as f64).max(rate.max(0.0) * BURST_WINDOW.as_secs_f64())
}

/// Bytes to pull for new sources given the bucket's `tokens`: all of them
/// but the share the parity those sources owe will cost when it is emitted.
pub(crate) fn source_budget(tokens: usize) -> usize {
    tokens * PARITY_RESERVE_GROUP / (PARITY_RESERVE_GROUP + 1)
}

/// The token bucket. See the module documentation.
#[derive(Debug)]
pub(crate) struct AdmissionBucket {
    /// May go negative only through [`Self::charge`] (H.264 frames, which
    /// have no queue to hold them back); a debt is repaid before anything
    /// further is granted. Never below `-debt_cap`.
    tokens: f64,
    last_refill: Option<Instant>,
    /// The deepest debt [`Self::charge`] may record, as a positive number of
    /// bytes: the burst cap of the last refill (`max(2 x MTU, R x 33 ms)`).
    ///
    /// Why the burst cap and not `D_target`: the cap is the bank's own size,
    /// so a debt never exceeds one bank, and a debt of one bank is repaid in
    /// one `BURST_WINDOW` (33 ms) at `R`, i.e. a burst of ungated sends can
    /// freeze NACK repair, timer repair and refinement for at most one tick
    /// after it ends. `D_target` (10 ms) would be tighter but is below one
    /// frame's worth of debt, which would make every H.264 frame look the
    /// same as no debt at all. Without any floor an H.264 region above the
    /// link rate (8 Mbit/s on 4) grew the debt for as long as the video
    /// played and froze every other class for as long again afterwards.
    ///
    /// The floor bounds the aftermath, not the starvation while the video
    /// plays: a stream above `R` keeps the bucket pinned at the floor and the
    /// gated classes get nothing. That is a bitrate-control matter (H.264 has
    /// no BWE-linked rate yet), not something a bucket can fix.
    debt_cap: f64,
    /// Cumulative tokens credited by [`Self::refill`] at the rate given, before
    /// the bank cap trimmed them. A sender that does not use its grant has
    /// the trimmed part counted here and not in `spent_total`.
    granted_total: f64,
    /// Cumulative tokens granted by [`Self::try_take`], less refunds. Soft
    /// charges ([`Self::charge`]: H.264 frames and control datagrams, which
    /// are not tile transmissions and are never acknowledged by `wire_seq`)
    /// are left out.
    spent_total: f64,
}

impl AdmissionBucket {
    pub(crate) fn new() -> Self {
        Self {
            tokens: 0.0,
            last_refill: None,
            debt_cap: 0.0,
            granted_total: 0.0,
            spent_total: 0.0,
        }
    }

    /// Cumulative tokens the rate has granted (untrimmed). Together with
    /// [`Self::spent_total`], the estimator's measure of whether the sender
    /// used what it was allowed.
    pub(crate) fn granted_total(&self) -> f64 {
        self.granted_total
    }

    /// Cumulative tokens spent on granted sends (net of refunds).
    pub(crate) fn spent_total(&self) -> f64 {
        self.spent_total
    }

    /// Credit the time elapsed since the last refill at `rate` bytes per
    /// second, capped at [`burst_cap`]. A falling rate shrinks what was
    /// already banked, so a window cut takes effect at once and not after the
    /// bucket has drained.
    ///
    /// The first refill starts the bucket full, so the first datagram of a
    /// session is not held for a tick it has, by construction, already
    /// waited out.
    pub(crate) fn refill(&mut self, now: Instant, rate: f64, mtu: usize) {
        let cap = burst_cap(rate, mtu);
        self.debt_cap = cap;
        match self.last_refill {
            None => self.tokens = cap,
            Some(prev) => {
                let dt = now.saturating_duration_since(prev).as_secs_f64();
                self.granted_total += rate.max(0.0) * dt;
                self.tokens = (self.tokens + rate.max(0.0) * dt).min(cap);
            }
        }
        // A falling rate shrinks the debt allowed as well as the bank.
        self.tokens = self.tokens.max(-cap);
        self.last_refill = Some(now);
    }

    /// Whole bytes that may be granted right now.
    pub(crate) fn available(&self) -> usize {
        if self.tokens <= 0.0 {
            0
        } else {
            self.tokens as usize
        }
    }

    /// Grant `bytes` if they are all affordable. Never partial and never into
    /// debt: this is the hard grant the rate bound is stated for.
    pub(crate) fn try_take(&mut self, bytes: usize) -> bool {
        if self.tokens >= bytes as f64 {
            self.tokens -= bytes as f64;
            self.spent_total += bytes as f64;
            true
        } else {
            false
        }
    }

    /// Give back a grant whose send was refused downstream.
    pub(crate) fn refund(&mut self, bytes: usize) {
        self.tokens += bytes as f64;
        self.spent_total -= bytes as f64;
    }

    /// Bill `bytes` that were sent without asking (an H.264 frame). May drive
    /// the bucket negative, down to `-debt_cap` and no further; the debt is
    /// repaid by the refill.
    pub(crate) fn charge(&mut self, bytes: usize) {
        self.tokens = (self.tokens - bytes as f64).max(-self.debt_cap);
    }

    /// Bytes of debt currently recorded (zero when the bucket is in credit).
    #[cfg(test)]
    pub(crate) fn debt(&self) -> usize {
        if self.tokens < 0.0 {
            (-self.tokens) as usize
        } else {
            0
        }
    }

    /// How long until `bytes` are affordable at `rate`; zero if they already
    /// are, `None` if the rate is zero.
    pub(crate) fn time_until(&self, bytes: usize, rate: f64) -> Option<Duration> {
        let short = bytes as f64 - self.tokens;
        if short <= 0.0 {
            Some(Duration::ZERO)
        } else if rate > 0.0 {
            Some(Duration::from_secs_f64(short / rate))
        } else {
            None
        }
    }
}

/// Why a datagram was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admit {
    /// Admitted and charged.
    Go,
    /// quinn's FIFO already holds `D_target`.
    Depth,
    /// The bucket cannot afford it.
    Tokens,
}

/// The rule every datagram bound for quinn passes, parity included: the
/// depth gate first (so a blocked datagram costs nothing), then the charge for
/// its `wire_len` bytes. `class` is not consulted: nothing is exempt from the
/// bucket, and a parity datagram is charged exactly as a source is.
pub(crate) fn admit_one(
    bucket: &mut AdmissionBucket,
    _class: AdmitClass,
    wire_len: usize,
    depth: usize,
    d_target: usize,
) -> Admit {
    if depth >= d_target {
        Admit::Depth
    } else if bucket.try_take(wire_len) {
        Admit::Go
    } else {
        Admit::Tokens
    }
}

/// Serve the classes in strict priority with `budget` bytes. `pull(class,
/// remaining)` takes what it can of its class's work against `remaining`
/// bytes and returns the bytes it took, which may overshoot by its last
/// datagram. A class is asked only while budget remains, so a class that used
/// the budget up, or overshot it, leaves nothing for the classes below.
pub(crate) fn pull_in_priority(
    budget: usize,
    classes: &[AdmitClass],
    mut pull: impl FnMut(AdmitClass, usize) -> usize,
) -> usize {
    let mut remaining = budget;
    let mut spent = 0usize;
    for &c in classes {
        if remaining == 0 {
            break;
        }
        let took = pull(c, remaining);
        spent = spent.saturating_add(took);
        remaining = remaining.saturating_sub(took);
    }
    spent
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::VecDeque;

    /// A queue of datagram lengths per class, drained head first: the model
    /// the priority property is stated against.
    type ClassQueues = [VecDeque<usize>; 5];

    const MTU: usize = 1_200;

    fn at(t0: Instant, us: u64) -> Instant {
        t0 + Duration::from_micros(us)
    }

    #[test]
    fn the_first_refill_starts_the_bucket_full() {
        let mut b = AdmissionBucket::new();
        b.refill(Instant::now(), 1_000_000.0, MTU);
        assert_eq!(b.available(), 33_333);
    }

    #[test]
    fn a_falling_rate_shrinks_what_was_banked() {
        let t0 = Instant::now();
        let mut b = AdmissionBucket::new();
        b.refill(t0, 10_000_000.0, MTU);
        assert!(b.available() > 300_000);
        b.refill(at(t0, 1), 100_000.0, MTU);
        assert_eq!(b.available(), 3_333, "the cut applies to the bank too");
    }

    #[test]
    fn a_charge_beyond_the_bank_is_a_debt_repaid_before_anything_is_granted() {
        let t0 = Instant::now();
        let mut b = AdmissionBucket::new();
        b.refill(t0, 1_000_000.0, MTU);
        b.charge(50_000); // 16,667 over the 33,333 banked
        assert_eq!(b.available(), 0);
        assert!(!b.try_take(1));
        b.refill(at(t0, 16_000), 1_000_000.0, MTU);
        assert_eq!(b.available(), 0, "16 ms repays 16,000 of the debt");
        b.refill(at(t0, 18_000), 1_000_000.0, MTU);
        assert!(b.available() > 0);
    }

    /// An ungated stream above the rate cannot dig the debt deeper than one
    /// bank, so the aftermath is one `BURST_WINDOW` and not as long as the
    /// stream played. Removing the floor in `charge` makes this fail.
    #[test]
    fn debt_is_floored_at_one_bank_so_the_aftermath_is_bounded() {
        let t0 = Instant::now();
        let rate = 500_000.0; // 4 Mbit/s
        let mut b = AdmissionBucket::new();
        b.refill(t0, rate, MTU);
        let cap = burst_cap(rate, MTU) as usize;
        // 8 Mbit/s of H.264 for 10 s, one frame per 33.3 ms.
        let mut t_us = 0u64;
        for _ in 0..300 {
            t_us += 33_333;
            b.refill(at(t0, t_us), rate, MTU);
            b.charge(33_333); // 1 MB/s x 33.3 ms
            assert!(b.debt() <= cap, "debt {} beyond one bank {cap}", b.debt());
        }
        assert!(b.debt() > 0, "the stream must have built a debt");
        // The video stops. Repair is granted again within one burst window.
        t_us += 2 * BURST_WINDOW.as_micros() as u64;
        b.refill(at(t0, t_us), rate, MTU);
        assert!(
            b.try_take(MTU),
            "10 s of overspend must not outlast the stream by more than a tick"
        );
    }

    #[test]
    fn a_grant_is_whole_or_nothing() {
        let mut b = AdmissionBucket::new();
        b.refill(Instant::now(), 0.0, MTU); // cap = 2 x MTU
        assert!(b.try_take(2_000));
        assert!(!b.try_take(500), "400 left is not 500");
        assert_eq!(b.available(), 400);
    }

    #[test]
    fn time_until_is_the_shortfall_over_the_rate() {
        let mut b = AdmissionBucket::new();
        b.refill(Instant::now(), 1_000_000.0, MTU);
        b.charge(33_333);
        assert_eq!(b.time_until(0, 1e6), Some(Duration::ZERO));
        let d = b.time_until(1_200, 1_000_000.0).unwrap();
        assert!((d.as_secs_f64() - 0.0012).abs() < 1e-6, "{d:?}");
        assert_eq!(b.time_until(1_200, 0.0), None);
    }

    /// A parity datagram is charged exactly as a source is. Skipping the
    /// charge for `Parity` in `admit_one` makes this fail.
    #[test]
    fn parity_spends_tokens() {
        for class in AdmitClass::PRIORITY {
            let mut b = AdmissionBucket::new();
            b.refill(Instant::now(), 1_000_000.0, MTU);
            let before = b.available();
            assert_eq!(admit_one(&mut b, class, 1_000, 0, 10_000), Admit::Go);
            assert_eq!(before - b.available(), 1_000, "{class:?} must be charged");
        }
    }

    #[test]
    fn a_blocked_datagram_costs_nothing() {
        let mut b = AdmissionBucket::new();
        b.refill(Instant::now(), 1_000_000.0, MTU);
        let before = b.available();
        assert_eq!(
            admit_one(&mut b, AdmitClass::Critical, 1_000, 5_000, 5_000),
            Admit::Depth,
            "the gate is depth >= D_target, so equality refuses"
        );
        assert_eq!(b.available(), before);
        let mut empty = AdmissionBucket::new();
        empty.refill(Instant::now(), 0.0, MTU);
        empty.charge(2_400);
        assert_eq!(
            admit_one(&mut empty, AdmitClass::Critical, 1_000, 0, 5_000),
            Admit::Tokens
        );
    }

    #[test]
    fn the_depth_target_is_ten_milliseconds_with_a_one_datagram_floor() {
        // 2 Mbit/s: 2.5 kB. 100 Mbit/s: 125 kB.
        assert_eq!(depth_target(250_000.0, MTU), 2_500);
        assert_eq!(depth_target(12_500_000.0, MTU), 125_000);
        // Below 120 kB/s the floor is one datagram.
        assert_eq!(depth_target(50_000.0, MTU), MTU);
        assert_eq!(depth_target(0.0, MTU), MTU);
    }

    #[test]
    fn the_parity_reserve_is_one_part_in_k_plus_one() {
        assert_eq!(source_budget(11_000), 10_000);
        assert_eq!(source_budget(0), 0);
    }

    /// Strict priority on a fixed example, so a failure is readable: with
    /// budget for 2 of the 3 nack datagrams, nothing below is touched.
    #[test]
    fn a_class_that_cannot_finish_leaves_nothing_to_the_classes_below() {
        let mut q: ClassQueues = Default::default();
        q[0].extend([1_000, 1_000, 1_000]);
        q[2].extend([100, 100]);
        let mut served: Vec<AdmitClass> = Vec::new();
        pull_in_priority(2_000, &AdmitClass::PRIORITY, |c, mut remaining| {
            let mut took = 0;
            while remaining > 0 {
                let Some(len) = q[c as usize].pop_front() else {
                    break;
                };
                served.push(c);
                took += len;
                remaining = remaining.saturating_sub(len);
            }
            took
        });
        assert_eq!(served, [AdmitClass::NackRepair; 2]);
        assert_eq!(q[0].len(), 1);
        assert_eq!(q[2].len(), 2);
    }

    proptest! {
        /// Over any window, the bytes granted are at most `R x window` plus
        /// the largest burst the bucket could hold when the window opened,
        /// however `R` moves.
        ///
        /// Demand saturates the bucket: every step asks, one datagram at a
        /// time, until the bucket refuses (bounded at 4,000 asks so a bucket
        /// that never refuses still ends, and fails). Rates stay where the
        /// bucket binds: at the top (4 MB/s x 40 ms = 160 kB) the demand of
        /// 4,000 x len is still more than a step can refill. A refill that
        /// credits 1.2x the rate, or a `try_take` that always grants, fails
        /// this.
        #[test]
        fn granted_bytes_are_bounded_by_rate_times_window_plus_burst(
            steps in proptest::collection::vec(
                (0u64..40_000, 0u32..4_000_000, 200usize..=1_200),
                2..60,
            ),
        ) {
            let t0 = Instant::now();
            let mut b = AdmissionBucket::new();
            let mut t_us = 0u64;
            // (time, rate in force over the interval ending here, granted at this time)
            let mut log: Vec<(u64, f64, usize)> = Vec::new();
            for &(dt, rate, len) in &steps {
                t_us += dt;
                let rate = f64::from(rate);
                b.refill(at(t0, t_us), rate, MTU);
                let mut granted = 0usize;
                for _ in 0..4_000 {
                    if !b.try_take(len) {
                        break;
                    }
                    granted += len;
                }
                log.push((t_us, rate, granted));
            }
            for i in 0..log.len() {
                // The most the bank could hold when the window opened.
                let cap_i = burst_cap(log[i].1, MTU);
                let mut got = 0usize;
                let mut allowed = 0.0f64;
                for j in i..log.len() {
                    got += log[j].2;
                    if j > i {
                        // The interval (j-1, j] refilled at j's rate.
                        allowed += log[j].1 * (log[j].0 - log[j - 1].0) as f64 / 1e6;
                    }
                    prop_assert!(
                        got as f64 <= allowed + cap_i + 1e-6,
                        "granted {got} over window {i}..={j} > {allowed} + {cap_i}"
                    );
                }
            }
        }

        /// A quiet spell banks nothing beyond the cap: however long the idle,
        /// the next instant's grants are at most one burst.
        #[test]
        fn idle_time_does_not_bank(
            idle_us in 0u64..3_600_000_000,
            rate in 0u32..50_000_000,
        ) {
            let t0 = Instant::now();
            let rate = f64::from(rate);
            let mut b = AdmissionBucket::new();
            b.refill(t0, rate, MTU);
            let _ = b.try_take(b.available());
            b.refill(at(t0, idle_us), rate, MTU);
            prop_assert!(b.available() as f64 <= burst_cap(rate, MTU));
        }

        /// `D_target` is at least a datagram and at most the larger of a
        /// datagram and 14 ms of the rate, for every rate.
        #[test]
        fn the_depth_target_never_exceeds_its_bound(
            rate in 0.0f64..1e10,
            mtu in 100usize..9_000,
        ) {
            let d = depth_target(rate, mtu);
            prop_assert!(d >= mtu);
            let bound = (rate * QUEUE_BOUND.as_secs_f64()) as usize;
            prop_assert!(d <= mtu.max(bound), "{d} > max({mtu}, {bound})");
        }

        /// Classes are served in strict priority: a class below another is
        /// served only if every class above it was emptied or the budget was
        /// spent, and the order of service is the priority order.
        #[test]
        fn classes_are_served_in_strict_priority(
            lens in proptest::collection::vec(proptest::collection::vec(1usize..=1_200, 0..8), 5),
            budget in 0usize..12_000,
        ) {
            let mut q: ClassQueues = Default::default();
            for (i, l) in lens.iter().enumerate() {
                q[i].extend(l.iter().copied());
            }
            let mut order: Vec<AdmitClass> = Vec::new();
            let spent = pull_in_priority(budget, &AdmitClass::PRIORITY, |c, mut remaining| {
                let mut took = 0;
                while remaining > 0 {
                    let Some(len) = q[c as usize].pop_front() else { break };
                    order.push(c);
                    took += len;
                    remaining = remaining.saturating_sub(len);
                }
                took
            });
            // Service order never goes back up the priority list.
            prop_assert!(order.windows(2).all(|w| w[0] <= w[1]), "{order:?}");
            // A served class below an unserved-but-nonempty one is a jump.
            if let Some(&lowest) = order.last() {
                for c in AdmitClass::PRIORITY.iter().filter(|&&c| c < lowest) {
                    prop_assert!(
                        q[*c as usize].is_empty(),
                        "{lowest:?} was served while {c:?} still had {} waiting",
                        q[*c as usize].len()
                    );
                }
            }
            // Soft budget: at most one datagram over.
            prop_assert!(spent < budget + 1_200 || budget == 0 && spent == 0);
        }
    }
}
