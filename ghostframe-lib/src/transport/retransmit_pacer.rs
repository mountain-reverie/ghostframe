//! Byte allowance for retransmission, refilled by elapsed time.
//!
//! The scheduler's emission is budgeted against the link estimate; replay
//! from the retransmit cache was not. `ReliableTileEmitter::tick` ran on
//! every frame and on every `Event::DatagramsUnblocked` with a bound of 64
//! retransmissions *per call*, and NACKs were re-sent the instant they
//! arrived. Neither had any relation to what the link could carry, so the
//! two together could -- and on a production-scale screen behind 8 Mbit,
//! did -- offer several times the link rate on top of a scheduler that was
//! already filling it. The excess accumulated in quinn's datagram buffer
//! until `clamp_to_quinn_capacity` left the scheduler nothing, and the
//! session never recovered: every acknowledgement was by then queued behind
//! seconds of the server's own retransmissions, so every timer kept firing.
//!
//! This is the missing budget. It is deliberately a separate, time-driven
//! bucket rather than a slice of the per-frame tick budget, because
//! retransmission has to keep working when frames are not arriving at the
//! tick rate -- a capture running at 5 fps, or a frame mode that does not
//! drain the scheduler at all.
//!
//! What keeps the *sum* within the link is the other half:
//! `take_spent_since_tick` reports what retransmission used, and the
//! scheduler's next tick gives that much back out of its own budget.

use std::time::Instant;

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct RetransmitPacer {
    /// May go negative: the emitter's byte bound is soft, so one call can
    /// overshoot by a retransmission, and the debt is repaid before anything
    /// further is allowed.
    tokens: f64,
    last_refill: Option<Instant>,
    spent_since_tick: usize,
}

impl RetransmitPacer {
    pub(crate) fn new() -> Self {
        Self {
            tokens: 0.0,
            last_refill: None,
            spent_since_tick: 0,
        }
    }

    /// Credit the time elapsed since the last refill.
    ///
    /// `tick_budget_bytes` is what the scheduler may send per
    /// `tick_interval_us`, i.e. the link estimate in the scheduler's own
    /// units; `share` is the fraction of that rate retransmission may use.
    ///
    /// Capped at one tick's worth. Idle time must not bank: an allowance
    /// that accumulated across a quiet second would be spent as a burst the
    /// moment a batch of timers came due, which is the overdrive this exists
    /// to prevent, merely delayed.
    pub(crate) fn refill(
        &mut self,
        now: Instant,
        tick_budget_bytes: usize,
        tick_interval_us: f64,
        share: f64,
    ) {
        let per_tick = tick_budget_bytes as f64 * share;
        match self.last_refill {
            // Start with one tick's worth rather than nothing, so the first
            // retransmission of a session is not held for a tick it has, by
            // construction, already waited out.
            None => self.tokens = per_tick,
            Some(prev) => {
                let elapsed_us = now.saturating_duration_since(prev).as_micros() as f64;
                self.tokens =
                    (self.tokens + per_tick * elapsed_us / tick_interval_us).min(per_tick);
            }
        }
        self.last_refill = Some(now);
    }

    /// Bytes that may be retransmitted right now.
    pub(crate) fn available(&self) -> usize {
        if self.tokens <= 0.0 {
            0
        } else {
            self.tokens as usize
        }
    }

    /// Bill `bytes` of retransmission.
    pub(crate) fn spend(&mut self, bytes: usize) {
        self.tokens -= bytes as f64;
        self.spent_since_tick = self.spent_since_tick.saturating_add(bytes);
    }

    /// Bytes retransmitted since the last scheduler tick, reset on read.
    pub(crate) fn take_spent_since_tick(&mut self) -> usize {
        std::mem::take(&mut self.spent_since_tick)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const TICK_US: f64 = 33_333.0;

    #[test]
    fn refills_at_its_share_of_the_tick_budget() {
        let t0 = Instant::now();
        let mut p = RetransmitPacer::new();
        p.refill(t0, 10_000, TICK_US, 0.5);
        p.spend(5_000);
        assert_eq!(p.available(), 0);

        // Half a tick later, half of the half-share is back.
        p.refill(t0 + Duration::from_micros(16_667), 10_000, TICK_US, 0.5);
        let got = p.available();
        assert!((2_499..=2_501).contains(&got), "got {got}");
    }

    /// The property the storm violated: over any stretch of time, what this
    /// lets through is bounded by rate x time, however often it is asked.
    #[test]
    fn asking_more_often_does_not_yield_more() {
        let t0 = Instant::now();
        let mut p = RetransmitPacer::new();
        let mut granted = 0usize;
        // One second, polled every 100 us -- far more often than any tick --
        // by a caller that always takes everything on offer.
        for i in 0..=10_000u64 {
            p.refill(t0 + Duration::from_micros(i * 100), 10_000, TICK_US, 0.5);
            let a = p.available();
            p.spend(a);
            granted += a;
        }
        // 5,000 bytes per 33.333 ms is 150,000 bytes/s, plus the one tick's
        // worth it starts with.
        let ceiling = 150_000 + 5_000 + 10; // +10: the tick is 33,333 us, not 33,333.3
        assert!(granted <= ceiling, "granted {granted} > {ceiling}");
        assert!(granted >= ceiling - 2_000, "granted only {granted}");
    }

    #[test]
    fn idle_time_does_not_bank() {
        let t0 = Instant::now();
        let mut p = RetransmitPacer::new();
        p.refill(t0, 10_000, TICK_US, 0.5);
        p.refill(t0 + Duration::from_secs(60), 10_000, TICK_US, 0.5);
        assert_eq!(p.available(), 5_000, "one tick's worth, not a minute's");
    }

    /// The emitter's bound is soft, so a call can overshoot. The overshoot
    /// is owed, not forgiven.
    #[test]
    fn an_overshoot_is_repaid_before_anything_more_is_allowed() {
        let t0 = Instant::now();
        let mut p = RetransmitPacer::new();
        p.refill(t0, 10_000, TICK_US, 0.5);
        p.spend(15_000); // 10,000 over
        p.refill(t0 + Duration::from_micros(33_333), 10_000, TICK_US, 0.5);
        assert_eq!(p.available(), 0, "one tick repays half the debt");
        p.refill(t0 + Duration::from_micros(3 * 33_333), 10_000, TICK_US, 0.5);
        let got = p.available();
        assert!((4_990..=5_000).contains(&got), "got {got}");
    }

    #[test]
    fn reports_what_was_spent_since_the_last_tick_once() {
        let t0 = Instant::now();
        let mut p = RetransmitPacer::new();
        p.refill(t0, 10_000, TICK_US, 1.0);
        p.spend(1_200);
        p.spend(800);
        assert_eq!(p.take_spent_since_tick(), 2_000);
        assert_eq!(p.take_spent_since_tick(), 0);
    }

    /// The rate follows the budget it is given: when the estimate falls, so
    /// does the allowance, including what was already banked.
    #[test]
    fn a_falling_estimate_shrinks_the_allowance() {
        let t0 = Instant::now();
        let mut p = RetransmitPacer::new();
        p.refill(t0, 100_000, TICK_US, 0.5);
        assert_eq!(p.available(), 50_000);
        p.refill(t0 + Duration::from_micros(1), 2_000, TICK_US, 0.5);
        assert_eq!(p.available(), 1_000);
    }
}
