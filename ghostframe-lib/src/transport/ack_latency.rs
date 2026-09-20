//! Rolling acknowledgement-latency tracker.
//!
//! Feeds the emitter's RTO policy (`reliable_emitter::rto::rto_for_attempt`).
//! The retransmit-storm investigation found the RTO deadline hard-coded at a
//! 50 ms ceiling — in practice 40 ms, since the `smoothed_rtt` input it was
//! derived from was never updated from its 20 ms default — racing a
//! distribution whose *mean* was 29 ms but whose *max* was 103 ms. A fixed
//! constant cannot track that; this tracker exists so the deadline can.
//!
//! # Design
//!
//! A ring buffer of the most recent [`WINDOW`] samples (256 microsecond
//! latencies, 2 KiB total), overwritten oldest-first. `record` is a plain
//! array write plus an index bump — no heap allocation, no locking beyond
//! whatever the caller already holds (`IoBridge` owns one per session and
//! calls it from a single-threaded drain loop).
//!
//! [`AckLatencyTracker::p95`] answers "what deadline should the *next*
//! first-attempt RTO use", not "what happened over the session's entire
//! lifetime" — a cumulative percentile would react in slow motion to a path
//! that got faster or slower minutes ago. A fixed recent window trades that
//! off for responsiveness: 256 samples is a few hundred milliseconds to a
//! few seconds of ACKs at typical tile-pass emission rates, long enough to
//! smooth over ordinary jitter, short enough to move if the path actually
//! changes. Percentile extraction sorts a stack-allocated copy of the
//! window (fixed [`WINDOW`]-sized array, never the heap) — cheap enough to
//! call once per drain (a few dozen times a second at most) even though the
//! sort itself isn't allocation-free.
//!
//! p95 rather than mean or max: the mean is what produced this bug (29 ms
//! mean, 103 ms max, on a distribution a 40 ms deadline still couldn't
//! survive), and the max is a single outlier away from disabling repair
//! for everyone else. p95 tracks the tail that actually matters — "the
//! deadline should survive all but the worst 1-in-20 acknowledgements" —
//! without being hostage to the single worst one.

use std::time::Duration;

/// Samples required before the window's p95 is treated as a measurement.
///
/// Below this, `p95` reports `Duration::ZERO` -- "I do not know yet" -- and
/// the RTO policy substitutes a deliberately pessimistic cold-start deadline
/// rather than its floor.
///
/// The distinction matters because the two failure directions are not
/// symmetric. Guessing *too slow* on an unmeasured path delays a last-resort
/// repair that NACKs and the scheduler's own retry already cover. Guessing
/// *too fast* retransmits everything in flight, and those retransmissions
/// compete with the first paint that is producing the very samples the
/// tracker needs -- the guess makes itself true.
///
/// Measured on the browserless storm scene before this distinction existed:
/// the tracker's first p95 read 93 ms against a true p50 of 280 ms, giving a
/// 186 ms deadline, and all 128 remaining retransmissions fired between
/// 200 ms and 300 ms -- i.e. in the window between the optimistic early
/// deadline and the real latency. The tracker then climbed 93 -> 134 -> 170
/// -> 215 -> 251 -> 256 ms and stopped firing, but the burst had happened.
pub(crate) const MIN_SAMPLES: usize = 32;

/// Number of most-recent samples retained. See the module doc for why 256.
///
/// `pub(crate)`, not `pub`: this is an implementation detail of the ring
/// buffer, not a tunable knob other code should reason about, and every
/// `pub const` in this crate lands in the generated C header (see
/// `AGENTS.md`).
pub(crate) const WINDOW: usize = 256;

/// Ring buffer of the most recent [`WINDOW`] ACK-latency samples
/// (microseconds), with a percentile query. See the module doc for the
/// design rationale.
#[derive(Debug, Clone)]
pub struct AckLatencyTracker {
    samples: [u64; WINDOW],
    /// Index the next `record` will write to.
    next: usize,
    /// Number of valid entries in `samples` (`samples[..len]`), saturating
    /// at `WINDOW` once the buffer has wrapped at least once.
    len: usize,
}

impl Default for AckLatencyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl AckLatencyTracker {
    pub fn new() -> Self {
        Self {
            samples: [0; WINDOW],
            next: 0,
            len: 0,
        }
    }

    /// Record one ACK-latency sample, in microseconds. O(1), no allocation.
    pub fn record(&mut self, latency_us: u64) {
        self.samples[self.next] = latency_us;
        self.next = (self.next + 1) % WINDOW;
        if self.len < WINDOW {
            self.len += 1;
        }
    }

    /// Number of samples currently held (`<= WINDOW`).
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// p95 latency over the current window, via the nearest-rank method
    /// (the 95th-smallest-of-100 convention): `rank = ceil(0.95 * n)`,
    /// 1-indexed, clamped into range.
    ///
    /// `Duration::ZERO` until [`MIN_SAMPLES`] have been recorded, meaning
    /// "no trustworthy measurement yet" rather than "zero latency".
    /// `rto_for_attempt` answers that with a pessimistic cold-start deadline
    /// — see [`MIN_SAMPLES`] for why erring slow is the safe direction.
    ///
    /// Sorts a stack-allocated copy of the window (fixed-size array, not a
    /// `Vec`) — see the module doc for why that's an acceptable query-time
    /// cost.
    pub fn p95(&self) -> Duration {
        if self.len < MIN_SAMPLES {
            return Duration::ZERO;
        }
        let mut buf = [0u64; WINDOW];
        buf[..self.len].copy_from_slice(&self.samples[..self.len]);
        let sorted = &mut buf[..self.len];
        sorted.sort_unstable();

        let n = self.len as u64;
        // ceil(0.95 * n), 1-indexed rank into the sorted sample.
        let rank = (95 * n).div_ceil(100);
        let idx = (rank.saturating_sub(1) as usize).min(self.len - 1);
        Duration::from_micros(sorted[idx])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tracker_reports_zero() {
        let t = AckLatencyTracker::new();
        assert_eq!(t.p95(), Duration::ZERO);
        assert_eq!(t.len(), 0);
        assert!(t.is_empty());
    }

    #[test]
    fn a_single_sample_is_not_yet_a_measurement() {
        let mut t = AckLatencyTracker::new();
        t.record(12_345);
        assert_eq!(
            t.p95(),
            Duration::ZERO,
            "one sample is not a distribution; reporting it would let a single \
             fast acknowledgement set the deadline for everything behind it"
        );
        assert_eq!(t.len(), 1, "but it is still retained");
    }

    /// The threshold is a real boundary, not decoration: one sample short
    /// reports nothing, and the next sample turns the window into a
    /// measurement.
    #[test]
    fn p95_becomes_a_measurement_exactly_at_min_samples() {
        let mut t = AckLatencyTracker::new();
        for _ in 0..MIN_SAMPLES - 1 {
            t.record(40_000);
        }
        assert_eq!(
            t.p95(),
            Duration::ZERO,
            "one sample short of the threshold is still no measurement"
        );
        t.record(40_000);
        assert_eq!(t.p95(), Duration::from_micros(40_000));
    }

    #[test]
    fn p95_of_100_samples_is_the_95th_smallest() {
        // 94 samples at 50ms, 6 samples at 80ms: sorted ascending, index 94
        // (0-based, the 95th element) is the first 80ms entry. This is the
        // exact fixture the RTO integration test in rto.rs relies on to
        // assert the derived deadline lands at 160ms.
        let mut t = AckLatencyTracker::new();
        for _ in 0..94 {
            t.record(50_000);
        }
        for _ in 0..6 {
            t.record(80_000);
        }
        assert_eq!(t.p95(), Duration::from_millis(80));
    }

    #[test]
    fn window_evicts_oldest_samples() {
        let mut t = AckLatencyTracker::new();
        // Fill the window with a high value...
        for _ in 0..WINDOW {
            t.record(500_000);
        }
        assert_eq!(t.p95(), Duration::from_micros(500_000));
        // ...then fully overwrite it with a low one. If eviction didn't
        // work, some 500ms samples would still be in the window and p95
        // would not equal the low value.
        for _ in 0..WINDOW {
            t.record(1_000);
        }
        assert_eq!(t.len(), WINDOW);
        assert_eq!(t.p95(), Duration::from_micros(1_000));
    }

    #[test]
    fn partial_window_uses_only_recorded_samples() {
        let mut t = AckLatencyTracker::new();
        // Fill to the threshold with a low value, then add three higher ones,
        // so the window is still partial (< WINDOW) but is a measurement.
        for _ in 0..MIN_SAMPLES {
            t.record(10_000);
        }
        t.record(20_000);
        t.record(30_000);
        t.record(40_000);
        let n = MIN_SAMPLES + 3;
        assert_eq!(t.len(), n, "a partial window must not be padded to WINDOW");
        // rank = ceil(0.95 * n) over [10ms x MIN_SAMPLES, 20ms, 30ms, 40ms].
        let expected = {
            let mut v: Vec<u64> = vec![10_000; MIN_SAMPLES];
            v.extend([20_000, 30_000, 40_000]);
            v.sort_unstable();
            let rank = (95 * n as u64).div_ceil(100) as usize;
            v[rank.saturating_sub(1).min(n - 1)]
        };
        assert_eq!(t.p95(), Duration::from_micros(expected));
    }

    #[test]
    fn p95_is_order_independent() {
        // Percentile must not depend on arrival order, only on the
        // multiset of values currently in the window.
        let mut a = AckLatencyTracker::new();
        let mut b = AckLatencyTracker::new();
        let vals_a = [10_000u64, 90_000, 20_000, 80_000, 30_000];
        let vals_b = [80_000u64, 30_000, 10_000, 90_000, 20_000];
        for v in vals_a {
            a.record(v);
        }
        for v in vals_b {
            b.record(v);
        }
        assert_eq!(a.p95(), b.p95());
    }
}
