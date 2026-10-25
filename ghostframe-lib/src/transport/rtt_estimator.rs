//! RFC 9002 §5 round-trip estimator and the §6.2 probe timeout (PTO) it
//! feeds, for the server's tile transmissions (D1 F4, part 1).
//!
//! # Samples
//!
//! One per acknowledged `wire_seq`: acknowledgement receipt minus the
//! transmission's *departure* from quinn's FIFO (`TransmissionLedger`'s
//! `depart_us`, F2). A `wire_seq` names exactly one transmission (PR #90), so
//! an acknowledgement is never ambiguous between an original and its
//! retransmission and Karn's rule is not needed. Departure, not enqueue: the
//! host's own FIFO is not part of the path.
//!
//! The wire carries no acknowledgement delay, so the sample is not adjusted
//! for it (RFC 9002 §5.3 subtracts a peer-reported `ack_delay`); the client's
//! flush (`ACK_FLUSH`, 5 ms) is instead *added* to the PTO as
//! `max_ack_delay`, which is where RFC 9002 §6.2.1 puts it.
//!
//! # Cost
//!
//! O(1) per sample, no allocation, nothing that grows with the session.
//!
//! # The timeout
//!
//! `PTO = srtt + max(4 x rttvar, 1 ms) + max_ack_delay`, doubled for every
//! consecutive PTO and reset by any acknowledgement. Before the first sample
//! it is computed from RFC 9002 §6.2.2's `kInitialRtt` (333 ms: `srtt = 333`,
//! `rttvar = 166.5`, PTO about 1 s).
//!
//! Deliberately **not** seeded from quinn's handshake RTT, which the design
//! suggested. Quinn measures the transport round trip (a few ms on a LAN);
//! the timer waits for an *application* acknowledgement, which comes after the
//! client's 5 ms flush and its event loop (a live session measured an ACK p95
//! of 38.6 ms on a path with a QUIC RTT of a few ms). A seed that small gives a
//! PTO near 8 ms, and before the first ACK there is nothing for the
//! hold-back to hold on, so every first-paint pass would fire.

use std::time::{Duration, Instant};

/// The client's acknowledgement flush deadline
/// (`ghostframe_client_core::ack_batcher`, 5 ms): the longest an
/// acknowledgement can be held back on purpose. RFC 9002's `max_ack_delay`.
pub(crate) const MAX_ACK_DELAY: Duration = Duration::from_millis(5);

/// RFC 9002 §6.2: the variance term never drops below the timer
/// granularity, which is 1 ms.
const GRANULARITY: Duration = Duration::from_millis(1);

/// RFC 9002 §6.2.2 `kInitialRtt`, used only when neither a sample nor a seed
/// exists (a unit test, or a connection that has not finished its handshake).
const INITIAL_RTT: Duration = Duration::from_millis(333);

/// Backoff exponent cap. `2^16` of any RTT already exceeds every ceiling
/// the caller applies; the cap only keeps the shift defined.
const MAX_BACKOFF_EXP: u32 = 16;

/// Smoothed round-trip time, its variance, the minimum, and the PTO backoff.
#[derive(Debug, Clone)]
pub struct RttEstimator {
    min_rtt: Option<Duration>,
    srtt: Duration,
    rttvar: Duration,
    latest_rtt: Duration,
    /// Whether `srtt` came from a measurement (`false`: a seed or nothing).
    sampled: bool,
    samples: u64,
    max_ack_delay: Duration,
    /// Consecutive PTOs since the last acknowledgement.
    pto_count: u32,
    /// A fire counts toward `pto_count` only once per PTO interval: the
    /// per-key timer wheel can pop many due keys in one tick, but RFC 9002
    /// has one timer per connection and one expiry is one PTO.
    next_count_at: Option<Instant>,
}

impl Default for RttEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl RttEstimator {
    pub fn new() -> Self {
        Self {
            min_rtt: None,
            srtt: INITIAL_RTT,
            rttvar: INITIAL_RTT / 2,
            latest_rtt: Duration::ZERO,
            sampled: false,
            samples: 0,
            max_ack_delay: MAX_ACK_DELAY,
            pto_count: 0,
            next_count_at: None,
        }
    }

    /// Whether any real sample has been taken.
    pub fn has_sample(&self) -> bool {
        self.sampled
    }

    /// One RTT sample (RFC 9002 §5.3, with no acknowledgement-delay
    /// adjustment; see the module doc). Also an acknowledgement: it resets
    /// the PTO backoff.
    pub fn on_sample(&mut self, rtt: Duration) {
        self.latest_rtt = rtt;
        self.samples += 1;
        self.min_rtt = Some(self.min_rtt.map_or(rtt, |m| m.min(rtt)));
        if !self.sampled {
            // §5.3: the first sample initialises both.
            self.srtt = rtt;
            self.rttvar = rtt / 2;
            self.sampled = true;
        } else {
            // rttvar = 3/4 rttvar + 1/4 |srtt - sample|
            // srtt   = 7/8 srtt   + 1/8 sample
            let dev = self.srtt.abs_diff(rtt);
            self.rttvar = (self.rttvar * 3 + dev) / 4;
            self.srtt = (self.srtt * 7 + rtt) / 8;
        }
        self.on_ack();
    }

    /// Any acknowledgement: the path is delivering, so the backoff ends
    /// (RFC 9002 §6.2.1).
    pub fn on_ack(&mut self) {
        self.pto_count = 0;
        self.next_count_at = None;
    }

    /// A PTO expired at `now`. Counts once per interval, however many due
    /// keys the caller pops in the same tick.
    pub fn on_pto_fire(&mut self, now: Instant) {
        if self.next_count_at.is_some_and(|t| now < t) {
            return;
        }
        self.pto_count = self.pto_count.saturating_add(1);
        self.next_count_at = Some(now + self.pto_interval(0));
    }

    pub fn srtt(&self) -> Duration {
        self.srtt
    }
    pub fn rttvar(&self) -> Duration {
        self.rttvar
    }
    pub fn min_rtt(&self) -> Option<Duration> {
        self.min_rtt
    }
    pub fn latest_rtt(&self) -> Duration {
        self.latest_rtt
    }
    pub fn samples(&self) -> u64 {
        self.samples
    }
    pub fn pto_count(&self) -> u32 {
        self.pto_count
    }

    /// `srtt + max(4 x rttvar, 1 ms) + max_ack_delay`, doubled `backoff`
    /// times. The caller passes the larger of its own retry count and
    /// [`Self::pto_count`] and applies its ceiling.
    pub fn pto_interval(&self, backoff: u32) -> Duration {
        let base = self.srtt + (self.rttvar * 4).max(GRANULARITY) + self.max_ack_delay;
        base.checked_mul(1u32 << backoff.min(MAX_BACKOFF_EXP))
            .unwrap_or(Duration::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    /// The §5.3 recurrences worked by hand for a fixed sample list, in
    /// microseconds so no step rounds.
    ///
    /// ```text
    /// s1 = 100 ms  : srtt = 100,      rttvar = 50
    /// s2 = 200 ms  : dev = 100        rttvar = (150 + 100) / 4 = 62.5
    ///                                 srtt   = (700 + 200) / 8 = 112.5
    /// s3 =  60 ms  : dev = 52.5       rttvar = (187.5 + 52.5) / 4 = 60
    ///                                 srtt   = (787.5 + 60) / 8 = 105.9375
    /// ```
    #[test]
    fn srtt_and_rttvar_follow_the_rfc_9002_recurrence() {
        let mut e = RttEstimator::new();
        e.on_sample(ms(100));
        assert_eq!(e.srtt(), ms(100));
        assert_eq!(e.rttvar(), ms(50), "first sample: rttvar = sample / 2");
        assert_eq!(e.min_rtt(), Some(ms(100)));

        e.on_sample(ms(200));
        assert_eq!(e.rttvar(), Duration::from_micros(62_500));
        assert_eq!(e.srtt(), Duration::from_micros(112_500));

        e.on_sample(ms(60));
        assert_eq!(e.rttvar(), ms(60));
        assert_eq!(e.srtt(), Duration::from_nanos(105_937_500));
        assert_eq!(e.min_rtt(), Some(ms(60)));
        assert_eq!(e.latest_rtt(), ms(60));
    }

    /// PTO = srtt + max(4 x rttvar, 1 ms) + max_ack_delay, against the same
    /// hand-worked values: 100 + 4 x 50 + 5 after the first sample.
    #[test]
    fn the_pto_is_srtt_plus_four_rttvar_plus_max_ack_delay() {
        let mut e = RttEstimator::new();
        e.on_sample(ms(100));
        assert_eq!(e.pto_interval(0), ms(100 + 4 * 50 + 5));
        e.on_sample(ms(200));
        // 112.5 + 4 x 62.5 + 5
        assert_eq!(e.pto_interval(0), Duration::from_micros(367_500));
    }

    /// With a constant RTT the variance decays toward zero and the PTO
    /// toward `srtt + 1 ms + max_ack_delay`: the granularity floor is what
    /// the `max(.., 1 ms)` is for.
    #[test]
    fn a_perfectly_steady_path_floors_the_variance_term_at_one_millisecond() {
        let mut e = RttEstimator::new();
        for _ in 0..400 {
            e.on_sample(ms(20));
        }
        assert!(e.rttvar() * 4 < MS, "premise: 4 x rttvar below the floor");
        assert_eq!(e.pto_interval(0), ms(20) + MS + MAX_ACK_DELAY);
    }

    /// Doubles per consecutive PTO, and any acknowledgement ends it.
    #[test]
    fn backoff_doubles_per_pto_and_resets_on_any_ack() {
        let t0 = Instant::now();
        let mut e = RttEstimator::new();
        e.on_sample(ms(40));
        let base = e.pto_interval(0);
        assert_eq!(e.pto_interval(1), base * 2);
        assert_eq!(e.pto_interval(3), base * 8);

        e.on_pto_fire(t0);
        assert_eq!(e.pto_count(), 1);
        // A second fire inside the same interval is the same expiry.
        e.on_pto_fire(t0 + base / 2);
        assert_eq!(e.pto_count(), 1, "one expiry, however many keys it popped");
        // Past the interval, the next expiry is a consecutive PTO.
        e.on_pto_fire(t0 + base * 2);
        assert_eq!(e.pto_count(), 2);
        e.on_pto_fire(t0 + base * 2 + base * 4);
        assert_eq!(e.pto_count(), 3);

        e.on_ack();
        assert_eq!(e.pto_count(), 0, "any acknowledgement ends the backoff");
        // And the counter is armed again: the next fire counts at once.
        e.on_pto_fire(t0 + base * 2 + base * 4 + MS);
        assert_eq!(e.pto_count(), 1);

        // A sample is an acknowledgement.
        e.on_sample(ms(40));
        assert_eq!(e.pto_count(), 0);
    }

    /// Before the first sample the estimator stands on RFC 9002's initial RTT
    /// (333 ms, PTO 333 + 4 x 166.5 + 5 = 1004 ms), and the first real sample
    /// replaces it outright, with no blend.
    #[test]
    fn the_initial_rtt_stands_until_the_first_sample_and_is_then_replaced() {
        let mut e = RttEstimator::new();
        assert!(!e.has_sample());
        assert_eq!(e.pto_interval(0), ms(1004));
        e.on_sample(ms(10));
        assert!(e.has_sample());
        assert_eq!(
            e.srtt(),
            ms(10),
            "the sample replaces the default, no blend"
        );
        assert_eq!(e.rttvar(), ms(5));
    }
}
