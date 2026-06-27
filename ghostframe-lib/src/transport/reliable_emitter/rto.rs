//! RTO timer wheel: min-heap of (deadline, EmitKey). The emitter's tick()
//! pops entries whose deadline ≤ now, validates each against the live
//! cache, and retransmits.

use crate::transport::reliable_emitter::{EmitKey, BASE_RTO_MS, RTO_BACKOFF_FACTOR};
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

impl RtoTimerWheel {
    pub fn new() -> Self {
        Self { heap: BinaryHeap::new() }
    }

    pub fn schedule(&mut self, key: EmitKey, deadline: Instant) {
        self.heap.push(Reverse(RtoEntry { deadline, key }));
    }

    /// Pop the next entry whose deadline ≤ now. Returns None when no entry
    /// is yet due. Callers re-validate the returned key against the live
    /// cache before retransmitting.
    pub fn pop_due(&mut self, now: Instant) -> Option<EmitKey> {
        let Some(Reverse(top)) = self.heap.peek() else {
            return None;
        };
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

/// Compute the RTO for a given attempt number (0 = first transmission's
/// RTO; 1, 2, ... = backoff for subsequent retries).
///
/// Returns `min(base * 2^attempts, RTO_BACKOFF_MAX)` where
/// `base ∈ [25ms, BASE_RTO_MS=50ms]` derived from smoothed RTT.
/// Once the cap is reached, steady-state retries fire every 5 s.
pub fn rto_for_attempt(smoothed_rtt: Duration, attempts: u8) -> Duration {
    let base = (smoothed_rtt * 2).max(Duration::from_millis(25));
    let base = base.min(Duration::from_millis(BASE_RTO_MS));
    let shift = attempts.min(8) as u32;
    let backoff = base
        .checked_mul(RTO_BACKOFF_FACTOR.pow(shift))
        .unwrap_or(RTO_BACKOFF_MAX);
    backoff.min(RTO_BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rto_first_attempt_high_rtt_caps_at_50ms() {
        let r = rto_for_attempt(Duration::from_millis(100), 0);
        assert_eq!(r, Duration::from_millis(50));
    }

    #[test]
    fn rto_first_attempt_low_rtt_floors_at_25ms() {
        let r = rto_for_attempt(Duration::from_millis(1), 0);
        assert_eq!(r, Duration::from_millis(25));
    }

    #[test]
    fn rto_backoff_doubles_per_attempt() {
        let r0 = rto_for_attempt(Duration::from_millis(100), 0);
        let r1 = rto_for_attempt(Duration::from_millis(100), 1);
        let r2 = rto_for_attempt(Duration::from_millis(100), 2);
        let r3 = rto_for_attempt(Duration::from_millis(100), 3);
        assert_eq!(r0, Duration::from_millis(50));
        assert_eq!(r1, Duration::from_millis(100));
        assert_eq!(r2, Duration::from_millis(200));
        assert_eq!(r3, Duration::from_millis(400));
    }

    #[test]
    fn rto_backoff_caps_at_5_seconds() {
        // attempt 99 must never exceed 5 s.
        let r = rto_for_attempt(Duration::from_millis(100), 99);
        assert_eq!(r, Duration::from_secs(5));
        // intermediate attempts still double until the cap.
        let r4 = rto_for_attempt(Duration::from_millis(100), 4);
        assert_eq!(r4, Duration::from_millis(800));
        let r5 = rto_for_attempt(Duration::from_millis(100), 5);
        assert_eq!(r5, Duration::from_millis(1600));
        let r6 = rto_for_attempt(Duration::from_millis(100), 6);
        assert_eq!(r6, Duration::from_millis(3200));
        // attempt 7 would compute 6400ms, but cap is 5000ms.
        let r7 = rto_for_attempt(Duration::from_millis(100), 7);
        assert_eq!(r7, Duration::from_secs(5));
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
