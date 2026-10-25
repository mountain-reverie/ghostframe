//! RTO timer wheel: min-heap of (deadline, EmitKey). The emitter's tick()
//! pops entries whose deadline ≤ now, validates each against the live
//! cache, and retransmits.

use crate::config::EmitterConfig;
use crate::transport::reliable_emitter::EmitKey;
use crate::transport::rtt_estimator::RttEstimator;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtoEntry {
    pub deadline: Instant,
    pub key: EmitKey,
}

impl Ord for RtoEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline.cmp(&other.deadline)
    }
}
impl PartialOrd for RtoEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub struct RtoTimerWheel {
    /// Min-heap by deadline (`Reverse` wraps the max-heap default).
    heap: BinaryHeap<Reverse<RtoEntry>>,
}

impl Default for RtoTimerWheel {
    fn default() -> Self {
        Self::new()
    }
}

impl RtoTimerWheel {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
        }
    }

    pub fn schedule(&mut self, key: EmitKey, deadline: Instant) {
        self.heap.push(Reverse(RtoEntry { deadline, key }));
    }

    /// Pop the next entry whose deadline ≤ now. Returns None when no entry
    /// is yet due. Callers re-validate the returned key against the live
    /// cache before retransmitting.
    pub fn pop_due(&mut self, now: Instant) -> Option<EmitKey> {
        // EXPERIMENT: RTO timer disabled. Cache + NACK path untouched.
        // Cached in `config`; this runs on every scheduler tick, so the
        // previous per-call `env::var` was a hot-path read as well as a
        // violation of the single-env-read-site rule.
        if crate::config::no_rto() {
            return None;
        }
        let Reverse(top) = self.heap.peek()?;
        if top.deadline > now {
            return None;
        }
        let Reverse(entry) = self.heap.pop().unwrap();
        Some(entry.key)
    }

    /// Is the earliest deadline already past? May name an entry that has
    /// since left the cache -- `pop_due` callers validate, and so this is an
    /// upper bound on "work is waiting", which is all a diagnostic needs.
    pub fn has_due(&self, now: Instant) -> bool {
        if crate::config::no_rto() {
            return false;
        }
        self.heap
            .peek()
            .is_some_and(|Reverse(top)| top.deadline <= now)
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}

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
/// is evaluated instead, by [`pto_resume_for_attempt`].
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

/// How long after the newest acknowledgement a held-back pass may fire: the
/// probe timeout backed off by the larger of the key's retries and the
/// connection's consecutive PTOs, so that during silence the passes of a dead
/// path are spaced out together rather than each firing at its own first
/// deadline. The count falls to zero with the first acknowledgement.
pub fn pto_resume_for_attempt(rtt: &RttEstimator, attempts: u8) -> Duration {
    let cfg = crate::config::emitter_config();
    rtt.pto_interval((attempts as u32).max(rtt.pto_count()).min(8))
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

    /// The connection's consecutive-PTO count backs off the *resume* time of a
    /// held pass but never the deadline an original is armed with, and an
    /// acknowledgement ends it.
    #[test]
    fn a_silent_path_backs_off_the_resume_time_not_the_arming_deadline() {
        let t0 = Instant::now();
        let mut e = est(40);
        let base = pto_for_attempt(&e, 0);
        e.on_pto_fire(t0);
        e.on_pto_fire(t0 + base * 2);
        assert_eq!(e.pto_count(), 2);
        assert_eq!(pto_for_attempt(&e, 0), base, "arming ignores the count");
        assert_eq!(pto_resume_for_attempt(&e, 0), base * 4);
        // The key's own retry count wins when it is the larger.
        assert_eq!(pto_resume_for_attempt(&e, 3), base * 8);
        e.on_ack();
        assert_eq!(pto_resume_for_attempt(&e, 0), base);
    }

    #[test]
    fn the_ceiling_is_configurable() {
        let cfg = EmitterConfig {
            rto_backoff_max: Duration::from_millis(500),
        };
        assert_eq!(pto_for_attempt_with(&cfg, &est(100), 0), 305 * MS);
        assert_eq!(pto_for_attempt_with(&cfg, &est(100), 1), 500 * MS);
    }

    #[test]
    fn heap_pops_in_deadline_order() {
        let mut w = RtoTimerWheel::new();
        let t0 = Instant::now();
        let k1 = EmitKey::new(1, 0, 0, 0);
        let k2 = EmitKey::new(2, 0, 0, 0);
        let k3 = EmitKey::new(3, 0, 0, 0);
        w.schedule(k1, t0 + Duration::from_millis(50));
        w.schedule(k2, t0 + Duration::from_millis(10));
        w.schedule(k3, t0 + Duration::from_millis(30));
        // At t0, none due
        assert_eq!(w.pop_due(t0), None);
        // At t0+15ms, only k2 due
        assert_eq!(w.pop_due(t0 + Duration::from_millis(15)), Some(k2));
        assert_eq!(w.pop_due(t0 + Duration::from_millis(15)), None);
        // At t0+35ms, k3 due
        assert_eq!(w.pop_due(t0 + Duration::from_millis(35)), Some(k3));
        // At t0+100ms, k1 due
        assert_eq!(w.pop_due(t0 + Duration::from_millis(100)), Some(k1));
    }
}
