//! The loss rate the classifier's `loss_override` acts on, measured by the
//! sender.
//!
//! It used to come from the client's `ReceiverFeedback`, whose
//! `datagrams_lost` counts fragments missing from a tile the client saw
//! *part* of. A tile that fits in one datagram has no part to see: when it
//! is lost the client never learns it existed. Measured: 15% injected loss
//! on a screen of single-datagram tiles reported `lost = 0`, and the
//! override it was meant to trip never fired once.
//!
//! The transmission ledger already knows the fate of every datagram the
//! emitter sent, one verdict each -- acknowledged, or outstanding past the
//! loss horizon. This is that, as a rate.
//!
//! Outcomes are filed under the time their datagram was *sent*, not the
//! time the verdict came in. An acknowledgement arrives one round trip
//! after the send and a loss is declared a whole horizon after it, so
//! counting by verdict time shows the tail of every burst as nothing but
//! losses.
//!
//! And the rate reads only sends old enough that their verdict is final.
//! The ledger's horizon is set for the congestion controller, which wants
//! its answer early and can absorb a wrong one: it declares a loss after as
//! little as 60 ms. A browser that is busy for 150 ms acknowledges four
//! whole frames late, and all of them are "lost" for a moment first.
//! Measured on a loss-free link: 5248 of 29184 transmissions expired that
//! way, in two episodes, reading as 83% and 12% loss. `loss_override`
//! bypasses hysteresis, so it must not act on a verdict that is about to be
//! withdrawn. Hence [`SETTLE`], and `retract` for the acknowledgement that
//! arrives inside it.

use std::collections::VecDeque;

/// How long after its send a transmission's verdict is treated as final.
/// The ledger's own upper bound on the loss horizon: past this, it would
/// have given up on the datagram under any path conditions.
pub const SETTLE: std::time::Duration = std::time::Duration::from_millis(600);

/// How much send time one reading covers.
const WINDOW_US: u64 = 1_000_000;
/// Outcomes are pooled per this much send time.
const BUCKET_US: u64 = 10_000;
/// Fewer verdicts than this in the window is not a measurement. At a true
/// 15% loss, 100 samples put one standard deviation at 3.6 points.
const MIN_SAMPLES: u32 = 100;
/// How long the last real measurement stands in while there is too little
/// traffic to make another.
///
/// It has to stand for a while: H.264 frames do not pass through the
/// emitter, so the moment `loss_override` engages this window starves, and
/// a rate that fell to zero with it would hand the classifier straight back
/// to the mode that was losing tiles. It must not stand forever either, or
/// one bad second would pin a session in H.264 for good. Five seconds, then
/// the classifier is free to go back and find out.
const HOLD_US: u64 = 5_000_000;

#[derive(Debug, Clone, Copy)]
struct Bucket {
    index: u64,
    delivered: u32,
    lost: u32,
}

#[derive(Debug, Default)]
pub struct LossWindow {
    /// Ascending by `index`.
    buckets: VecDeque<Bucket>,
    /// The newest stamp seen, unwrapped: the sender's stamps are `u32`
    /// microseconds and wrap every 71.6 minutes.
    clock_us: u64,
    /// The last rate measured from enough samples, and when.
    held: Option<(f32, u64)>,
}

impl LossWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Extend a wrapping `u32` stamp to the `u64` timeline, relative to the
    /// newest stamp seen so far.
    fn unwrap(&mut self, stamp_us: u32) -> u64 {
        let delta = stamp_us.wrapping_sub(self.clock_us as u32) as i32;
        let at = self.clock_us.saturating_add_signed(delta as i64);
        self.clock_us = self.clock_us.max(at);
        at
    }

    /// File the verdict on one transmission sent at `sent_us`.
    pub fn record(&mut self, sent_us: u32, lost: bool) {
        let index = self.unwrap(sent_us) / BUCKET_US;
        // Verdicts arrive nearly in send order, so the bucket is at or near
        // the back.
        let at = self
            .buckets
            .iter()
            .rposition(|b| b.index <= index)
            .map_or(0, |i| i + 1);
        let slot = if at > 0 && self.buckets[at - 1].index == index {
            at - 1
        } else {
            self.buckets.insert(
                at,
                Bucket {
                    index,
                    delivered: 0,
                    lost: 0,
                },
            );
            at
        };
        let bucket = &mut self.buckets[slot];
        if lost {
            bucket.lost += 1;
        } else {
            bucket.delivered += 1;
        }
    }

    /// Withdraw a loss verdict: the transmission sent at `sent_us` was
    /// acknowledged after all. Late, but delivered.
    pub fn retract(&mut self, sent_us: u32) {
        let index = self.unwrap(sent_us) / BUCKET_US;
        if let Some(bucket) = self.buckets.iter_mut().rfind(|b| b.index == index) {
            if bucket.lost > 0 {
                bucket.lost -= 1;
                bucket.delivered += 1;
            }
        }
    }

    /// The loss rate at `now_us`, over transmissions sent at least
    /// [`SETTLE`] ago.
    pub fn rate(&mut self, now_us: u32) -> f32 {
        let now = self.unwrap(now_us);
        // Sends at or after `settled` may still have their verdict changed.
        let settled = now.saturating_sub(SETTLE.as_micros() as u64) / BUCKET_US;
        let oldest = settled.saturating_sub(WINDOW_US / BUCKET_US);
        while self.buckets.front().is_some_and(|b| b.index < oldest) {
            self.buckets.pop_front();
        }
        let (delivered, lost) = self
            .buckets
            .iter()
            .take_while(|b| b.index < settled)
            .fold((0u32, 0u32), |(d, l), b| (d + b.delivered, l + b.lost));
        if delivered + lost >= MIN_SAMPLES {
            let rate = lost as f32 / (delivered + lost) as f32;
            self.held = Some((rate, now));
            return rate;
        }
        match self.held {
            Some((rate, at)) if now.saturating_sub(at) <= HOLD_US => rate,
            _ => {
                self.held = None;
                0.0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTLE_US: u32 = SETTLE.as_micros() as u32;

    /// Send one datagram every `step_us` over `[from_us, to_us)`, losing
    /// every `lose_every`-th (0 = none), and file all the verdicts.
    fn send(w: &mut LossWindow, from_us: u32, to_us: u32, step_us: u32, lose_every: u32) {
        let mut n = 0u32;
        let mut t = from_us;
        while t < to_us {
            n += 1;
            w.record(t, lose_every != 0 && n.is_multiple_of(lose_every));
            t += step_us;
        }
    }

    #[test]
    fn reports_the_fraction_of_transmissions_that_were_lost() {
        let mut w = LossWindow::new();
        send(&mut w, 0, 1_000_000, 1_000, 5);
        let rate = w.rate(1_000_000 + SETTLE_US);
        assert!((rate - 0.20).abs() < 0.01, "got {rate}");
    }

    /// The case the client's own count could not see: every tile is a single
    /// datagram, so nothing is ever *partially* received. The verdicts are
    /// all this type is told, so it cannot tell the difference -- which is
    /// the point.
    #[test]
    fn a_clean_link_reads_zero() {
        let mut w = LossWindow::new();
        send(&mut w, 0, 1_000_000, 1_000, 0);
        assert_eq!(w.rate(1_000_000 + SETTLE_US), 0.0);
    }

    /// A verdict on a recent send can still change, either way.
    #[test]
    fn unsettled_sends_are_not_read() {
        let mut w = LossWindow::new();
        // An old second at 20% loss, fully settled.
        send(&mut w, 0, 1_000_000, 1_000, 5);
        // Then a burst the ledger has just declared wholly lost.
        send(&mut w, 1_000_000, 1_000_000 + SETTLE_US, 100, 1);
        let rate = w.rate(1_000_000 + SETTLE_US);
        assert!((rate - 0.20).abs() < 0.01, "got {rate}");
    }

    /// The loss-free link that read as 83% loss: a client busy for longer
    /// than the ledger's horizon acknowledges whole frames late. Each was
    /// declared lost, then acknowledged; by the time the window reads those
    /// sends, nothing is missing.
    #[test]
    fn a_late_acknowledgement_withdraws_the_loss() {
        let mut w = LossWindow::new();
        send(&mut w, 0, 1_000_000, 1_000, 1);
        for t in (0..1_000_000u32).step_by(1_000) {
            w.retract(t);
        }
        assert_eq!(w.rate(1_000_000 + SETTLE_US), 0.0);
        // Nothing to withdraw twice.
        w.retract(0);
        assert_eq!(w.rate(1_000_000 + SETTLE_US), 0.0);
    }

    /// The tail of a burst, the other way round: its acknowledgements were
    /// filed long ago and its losses are only now being declared. Filed
    /// under verdict time that reads as a window of nothing but losses;
    /// filed under send time it is the same 2% it always was.
    #[test]
    fn late_verdicts_join_the_sends_they_belong_to() {
        let mut w = LossWindow::new();
        let mut losses = Vec::new();
        let mut n = 0u32;
        for t in (0..1_000_000u32).step_by(100) {
            n += 1;
            if n.is_multiple_of(50) {
                losses.push(t);
            } else {
                w.record(t, false);
            }
        }
        for t in losses {
            w.record(t, true);
        }
        let rate = w.rate(1_000_000 + SETTLE_US);
        assert!((rate - 0.02).abs() < 0.001, "got {rate}");
    }

    #[test]
    fn a_handful_of_verdicts_is_not_a_measurement() {
        let mut w = LossWindow::new();
        // 99 sends, all lost.
        send(&mut w, 0, 99_000, 1_000, 1);
        assert_eq!(w.rate(99_000 + SETTLE_US), 0.0);
    }

    /// `loss_override` moves the session to H.264, whose frames never reach
    /// the ledger. The measurement that caused it has to outlive the traffic
    /// that produced it, and then let go.
    #[test]
    fn the_last_measurement_stands_through_a_quiet_spell_then_lapses() {
        let mut w = LossWindow::new();
        send(&mut w, 0, 1_000_000, 1_000, 5);
        let measured_at = 1_000_000 + SETTLE_US;
        let measured = w.rate(measured_at);
        assert!(measured > 0.19);

        // The window has slid clean off the traffic; the rate still stands.
        let quiet = measured_at + 3_000_000;
        assert_eq!(w.rate(quiet), measured);
        let still = measured_at + HOLD_US as u32;
        assert_eq!(w.rate(still), measured);

        assert_eq!(w.rate(still + 1), 0.0);
    }

    #[test]
    fn old_traffic_ages_out_of_the_reading() {
        let mut w = LossWindow::new();
        send(&mut w, 0, 1_000_000, 1_000, 5);
        send(&mut w, 1_000_000, 2_100_000, 1_000, 0);
        assert_eq!(w.rate(2_100_000 + SETTLE_US), 0.0);
        assert!(w.buckets.len() <= (WINDOW_US / BUCKET_US) as usize + 10);
    }

    /// The sender's stamp wraps at 2^32 us. A window straddling the wrap
    /// must read as one second of traffic, not as two ends of 71 minutes.
    #[test]
    fn the_window_reads_across_the_stamp_wrap() {
        let mut w = LossWindow::new();
        let start = u32::MAX - 500_000;
        // Walk the clock up to the wrap first, as a live session would.
        w.rate(start);
        let mut n = 0u32;
        for i in 0..1_000u32 {
            n += 1;
            w.record(start.wrapping_add(i * 1_000), n.is_multiple_of(5));
        }
        let now = start.wrapping_add(1_000_000 + SETTLE_US);
        let rate = w.rate(now);
        assert!((rate - 0.20).abs() < 0.01, "got {rate}");
    }
}
