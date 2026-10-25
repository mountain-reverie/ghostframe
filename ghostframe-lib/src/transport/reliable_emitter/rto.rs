//! The probe timeout (PTO) interval of the reliable emitter: the RFC 9002
//! section 6.2 value from the RTT estimator, doubled per backoff step and held
//! to a ceiling. There is no per-pass timer wheel any more (D1 F4 part 2): one
//! connection-wide PTO is the tail probe, and losses are found by the
//! transmission ledger's threshold detector.

use crate::config::EmitterConfig;
use crate::transport::rtt_estimator::RttEstimator;
use std::time::Duration;

/// Maximum RTO backoff. Beyond this, retries fire every `RTO_BACKOFF_MAX`
/// indefinitely. Picked so a single un-delivered tile-pass under sustained
/// loss still retries 12 times per minute — fast enough that recovery is
/// perceptible to the user, slow enough to not flood the link.
pub const RTO_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// The deadline of a transmission that left `attempts` retransmissions ago
/// (0 for an original): the probe timeout of RFC 9002 §6.2 from `rtt`, doubled
/// per backoff step and held to `EmitterConfig::rto_backoff_max`.
///
/// Only the key's own retry count backs it off. The connection's
/// consecutive-PTO count ([`RttEstimator::pto_count`]) does not: an original
/// first sent at the end of a stall would otherwise be armed as late as a
/// retry, and stay so after the ACKs resume. Silence is handled where the timer
/// is evaluated instead: the emitter's PTO timer backs off by the count.
///
/// # History
///
/// This replaced `clamp(2 x p95(ACK latency), 150 ms, 2 s)` (1 s before 32
/// samples), measured from *enqueue*. That formula has no variance term, its
/// sample was censored exactly when it would have been largest (only
/// acknowledged transmissions were measured), and it included the host's own
/// queue. The constants it rested on (`ACK_DEADLINE_FLOOR`, `_COLD_START`,
/// `_CEILING`) were fitted to a live session's 103 ms maximum; a measured
/// estimator with a variance term needs no fitted floor. See D1 F4.
pub fn pto_for_attempt(rtt: &RttEstimator, attempts: u8) -> Duration {
    pto_for_attempt_with(crate::config::emitter_config(), rtt, attempts)
}

/// [`pto_for_attempt`] against an explicit `EmitterConfig`.
pub fn pto_for_attempt_with(cfg: &EmitterConfig, rtt: &RttEstimator, attempts: u8) -> Duration {
    rtt.pto_interval((attempts as u32).min(8))
        .min(cfg.rto_backoff_max)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn est(sample_ms: u64) -> RttEstimator {
        let mut e = RttEstimator::new();
        e.on_sample(Duration::from_millis(sample_ms));
        e
    }

    /// 100 ms sample: srtt 100, rttvar 50, so the PTO is 100 + 200 + 5.
    #[test]
    fn the_first_attempt_deadline_is_the_probe_timeout() {
        assert_eq!(pto_for_attempt(&est(100), 0), 305 * MS);
    }

    /// No seed and no sample: RFC 9002's `kInitialRtt`, not a one-second
    /// guess. (333 + 4 x 166.5 + 5 = 1004 ms.)
    #[test]
    fn no_measurement_at_all_is_the_rfc_initial_rtt() {
        assert_eq!(pto_for_attempt(&RttEstimator::new(), 0), 1004 * MS);
    }

    #[test]
    fn backoff_doubles_per_attempt_and_caps_at_the_ceiling() {
        let e = est(100);
        let base = pto_for_attempt(&e, 0);
        assert_eq!(pto_for_attempt(&e, 1), base * 2);
        assert_eq!(pto_for_attempt(&e, 2), base * 4);
        assert_eq!(pto_for_attempt(&e, 3), base * 8);
        // 305 ms x 2^4 = 4.88 s, x 2^5 would be 9.76 s: capped at 5 s.
        assert_eq!(pto_for_attempt(&e, 5), RTO_BACKOFF_MAX);
        assert_eq!(pto_for_attempt(&e, 99), RTO_BACKOFF_MAX);
    }

    /// Only the retry count backs a deadline off here; the connection's
    /// consecutive-PTO count is applied by the emitter's timer itself.
    #[test]
    fn arming_ignores_the_connections_pto_count() {
        let t0 = std::time::Instant::now();
        let mut e = est(40);
        let base = pto_for_attempt(&e, 0);
        e.on_pto_fire(t0);
        e.on_pto_fire(t0 + base * 2);
        assert_eq!(e.pto_count(), 2);
        assert_eq!(pto_for_attempt(&e, 0), base);
    }

    #[test]
    fn the_ceiling_is_configurable() {
        let cfg = EmitterConfig {
            rto_backoff_max: Duration::from_millis(500),
        };
        assert_eq!(pto_for_attempt_with(&cfg, &est(100), 0), 305 * MS);
        assert_eq!(pto_for_attempt_with(&cfg, &est(100), 1), 500 * MS);
    }
}
