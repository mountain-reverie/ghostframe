//! RTO timer wheel: min-heap of (deadline, EmitKey). The emitter's tick()
//! pops entries whose deadline ≤ now, validates each against the live
//! cache, and retransmits.

use crate::transport::reliable_emitter::{EmitKey, RTO_BACKOFF_FACTOR};
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
        if std::env::var("GHOSTFRAME_NO_RTO").is_ok_and(|v| v == "1") {
            return None;
        }
        let Reverse(top) = self.heap.peek()?;
        if top.deadline > now {
            return None;
        }
        let Reverse(entry) = self.heap.pop().unwrap();
        Some(entry.key)
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

/// Floor for the first-attempt ACK deadline (`base`, below).
///
/// Set above the 103 ms *max* observed on the live session that motivated
/// this policy (mean 29 ms, max 103 ms over 6517 samples) — not just the
/// mean, and not the p95 either, because a floor between the p95 and the
/// max would still race the tail on every session whose distribution
/// happens to look like that one. Without this floor a quiet or
/// just-started tracker (`AckLatencyTracker::p95()` returns
/// `Duration::ZERO` before its first sample) would derive a `base` of
/// zero, doubled to zero, which is the exact failure mode this change
/// exists to remove.
pub const ACK_DEADLINE_FLOOR: Duration = Duration::from_millis(150);

/// First-attempt deadline used while the latency tracker has no trustworthy
/// measurement (`AckLatencyTracker::p95()` returns `Duration::ZERO` until
/// `MIN_SAMPLES` acknowledgements have arrived).
///
/// Deliberately pessimistic, and deliberately well above the floor. The two
/// failure directions are not symmetric: guessing too slow delays a
/// last-resort repair that NACKs and the scheduler's 2xRTT retry already
/// cover, while guessing too fast retransmits everything in flight -- and
/// those retransmissions compete with the first paint that is generating the
/// very samples the tracker is waiting for, so the optimistic guess makes
/// itself true.
///
/// One second is longer than any acknowledgement latency observed on either
/// a real tailnet path (max 103 ms) or the browserless harness (max 317 ms),
/// so the cold-start window costs at most one second of delayed repair on a
/// path that genuinely drops its opening datagrams.
pub const ACK_DEADLINE_COLD_START: Duration = Duration::from_secs(1);

/// Ceiling for the first-attempt ACK deadline (`base`, below).
///
/// The RTO is a last resort — client NACKs handle real loss immediately,
/// and the scheduler's own 2xRTT `InFlight` retry covers the one case a
/// receiver can't see (a tile whose only datagram is lost, then static).
/// So a slow RTO is safe. But an unbounded one is not: a single
/// pathological `ack_p95` measurement (a session-start spike before the
/// tracker has filled, or a genuinely broken path) must not push the
/// deadline out far enough to functionally disable the RTO's own
/// contribution to recovery. 2 s is far above any latency this system is
/// designed to tolerate, so it only ever bites the pathological case.
pub const ACK_DEADLINE_CEILING: Duration = Duration::from_secs(2);

/// Compute the RTO for a given attempt number (0 = first transmission's
/// RTO; 1, 2, ... = backoff for subsequent retries).
///
/// `ack_p95` is the emitter's measured ACK-latency p95 (see
/// [`crate::transport::ack_latency::AckLatencyTracker`]), fed in via
/// `ReliableTileEmitter::set_ack_deadline`. The base deadline is
/// `clamp(ack_p95 * 2, ACK_DEADLINE_FLOOR, ACK_DEADLINE_CEILING)`: doubling
/// the measured p95 gives real headroom over the tail instead of racing
/// it, and the floor/ceiling bound a bad or absent measurement (see their
/// doc comments). Returns `min(base * 2^attempts, RTO_BACKOFF_MAX)` — once
/// the cap is reached, steady-state retries fire every 5 s.
///
/// # History
///
/// Before this, `base` was derived from `smoothed_rtt` — which
/// `set_smoothed_rtt` never actually updated, since nothing called it —
/// and hard-capped at a 50 ms ceiling (`BASE_RTO_MS`), in practice reached
/// from a 20 ms constructor default. Measured on a live session, 2m45s
/// uptime, on a link that dropped nothing: 29 ms mean / 103 ms max ACK
/// latency (6517 samples) against a 40 ms deadline fired `rto_fired=33685`
/// times and retransmitted 1.7x the actual picture
/// (`retransmit_attempts_total=44207` against 26578 fresh emissions).
/// Confirmed independently in the browserless harness by sweeping the
/// first-attempt deadline on a lossless scene: 40 ms -> 320
/// retransmissions, 200 ms -> 128, 300 ms -> 0. See
/// `a_lossless_link_with_a_real_rtt_does_not_retransmit` in
/// `browserless_runner.rs` for the regression gate.
pub fn rto_for_attempt(ack_p95: Option<Duration>, attempts: u8) -> Duration {
    // `None` is the tracker saying it has no trustworthy measurement yet,
    // which is a different fact from a fast path. Assume the worst until
    // told otherwise; see `ACK_DEADLINE_COLD_START` for why slow is the safe
    // direction to be wrong in.
    let base = match ack_p95 {
        Some(p95) => (p95 * 2).clamp(ACK_DEADLINE_FLOOR, ACK_DEADLINE_CEILING),
        None => ACK_DEADLINE_COLD_START,
    };
    let shift = attempts.min(8) as u32;
    let backoff = base
        .checked_mul(RTO_BACKOFF_FACTOR.pow(shift))
        .unwrap_or(RTO_BACKOFF_MAX);
    backoff.min(RTO_BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    // These four tests encode the RTO policy itself and are the policy's
    // documentation as much as its verification — see `rto_for_attempt`'s
    // doc comment for the full history of why the policy looks like this.

    #[test]
    fn rto_first_attempt_floors_at_150ms() {
        // A genuinely measured but tiny p95 must not be chased: the floor
        // sits above the 103 ms max measured in production, which is exactly
        // the distribution a lower floor raced and lost.
        let r = rto_for_attempt(Some(Duration::from_millis(1)), 0);
        assert_eq!(r, ACK_DEADLINE_FLOOR);
    }

    /// `Duration::ZERO` is the tracker saying "I have no measurement", not
    /// "the path is instant". Answering it with the floor is what left 128
    /// retransmissions on the browserless storm scene: the first p95 read
    /// 93 ms against a true p50 of 280 ms, and every one of those fires
    /// landed between the optimistic deadline and the real latency.
    #[test]
    fn no_measurement_yet_gets_the_pessimistic_cold_start_deadline() {
        let r = rto_for_attempt(None, 0);
        assert_eq!(r, ACK_DEADLINE_COLD_START);
        assert!(
            ACK_DEADLINE_COLD_START > ACK_DEADLINE_FLOOR,
            "an unmeasured path must be assumed slower than a measured fast one"
        );
    }

    #[test]
    fn rto_first_attempt_ceiling_caps_pathological_measurement() {
        // A single pathological ack_p95 (a start-of-session spike, or a
        // genuinely broken path) must not disable the RTO's contribution
        // to recovery by pushing the deadline out indefinitely.
        let r = rto_for_attempt(Some(Duration::from_secs(5)), 0);
        assert_eq!(r, ACK_DEADLINE_CEILING);
    }

    #[test]
    fn rto_backoff_doubles_per_attempt() {
        // ack_p95 = 100ms -> base = clamp(200ms, 150ms, 2s) = 200ms,
        // comfortably inside floor/ceiling so doubling is visible
        // untouched by either clamp.
        let r0 = rto_for_attempt(Some(Duration::from_millis(100)), 0);
        let r1 = rto_for_attempt(Some(Duration::from_millis(100)), 1);
        let r2 = rto_for_attempt(Some(Duration::from_millis(100)), 2);
        let r3 = rto_for_attempt(Some(Duration::from_millis(100)), 3);
        assert_eq!(r0, Duration::from_millis(200));
        assert_eq!(r1, Duration::from_millis(400));
        assert_eq!(r2, Duration::from_millis(800));
        assert_eq!(r3, Duration::from_millis(1600));
    }

    #[test]
    fn rto_backoff_caps_at_5_seconds() {
        // Same base = 200ms as the doubling test above.
        let r4 = rto_for_attempt(Some(Duration::from_millis(100)), 4);
        assert_eq!(r4, Duration::from_millis(3200));
        // attempt 5 would compute 6400ms, but the cap is 5000ms.
        let r5 = rto_for_attempt(Some(Duration::from_millis(100)), 5);
        assert_eq!(r5, Duration::from_secs(5));
        // attempt 99 must never exceed 5 s regardless of shift saturation.
        let r99 = rto_for_attempt(Some(Duration::from_millis(100)), 99);
        assert_eq!(r99, Duration::from_secs(5));
    }

    #[test]
    fn ack_deadline_tracks_measured_p95() {
        // The whole point of this policy: the deadline moves with what the
        // link actually does, instead of sitting at a fixed constant. Same
        // fixture as `ack_latency::tests::p95_of_100_samples_is_the_95th_smallest`.
        use crate::transport::ack_latency::AckLatencyTracker;
        let mut t = AckLatencyTracker::new();
        for _ in 0..94 {
            t.record(50_000); // 50ms
        }
        for _ in 0..6 {
            t.record(80_000); // 80ms
        }
        assert_eq!(t.p95(), Some(Duration::from_millis(80)));
        let deadline = rto_for_attempt(t.p95(), 0);
        assert_eq!(deadline, Duration::from_millis(160));
    }

    #[test]
    fn ack_deadline_floors_when_measurement_is_tiny() {
        use crate::transport::ack_latency::AckLatencyTracker;
        use crate::transport::ack_latency::MIN_SAMPLES;
        let mut t = AckLatencyTracker::new();
        // Enough samples to count as a measurement, all of them tiny.
        for _ in 0..MIN_SAMPLES {
            t.record(1_000); // 1ms
        }
        let deadline = rto_for_attempt(t.p95(), 0);
        assert_eq!(deadline, ACK_DEADLINE_FLOOR);

        // Below the threshold the same samples are not a measurement, and
        // the policy must err slow instead.
        let mut cold = AckLatencyTracker::new();
        for _ in 0..MIN_SAMPLES - 1 {
            cold.record(1_000);
        }
        assert_eq!(rto_for_attempt(cold.p95(), 0), ACK_DEADLINE_COLD_START);
    }

    #[test]
    fn ack_deadline_floor_is_below_ceiling() {
        assert!(ACK_DEADLINE_FLOOR < ACK_DEADLINE_CEILING);
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
