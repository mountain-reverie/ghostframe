// SPDX-License-Identifier: AGPL-3.0-or-later
//! The growth gate (D1): while the sender is not saturated, the published
//! estimate rises freely up to what was proved in the last 10 s; above that
//! it is rate-limited to `UNPROBED_GROWTH_PER_S` (3 % a second) unless a probe
//! result lets it through. What a probe cluster measured (our own meter,
//! `probe_meter.rs`) is proved whether or not goog_cc published it: its
//! loss-based controller can cap the target in the very feedback that carries
//! the result, and the gate must not forget the rate when that cap lifts. A
//! rate is proved by what a probe cluster measured or by being published while
//! saturated, never by goog_cc's raw target merely having been published in a
//! feedback that carried probe packets.
//!
//! See the module documentation of `googcc.rs` for why. This file is the rule
//! alone, pure and clock-free, so it can be tested without a controller.

/// How long a proved rate stays proved: a rise back up to it is free for this
/// long. RFC 7661 lets a validated rate stand for minutes, but wireless paths
/// change in seconds; 10 s spans a probe interval (goog_cc's ALR probes are 5 s
/// apart at the soonest) and a typical transient (a cut and its recovery).
pub(crate) const PROVED_WINDOW_US: u64 = 10_000_000;

/// How fast the published estimate may climb above what is proved without a
/// probe: 3 % of itself a second. goog_cc's own drift between probes is about
/// 2 % a second on the P-B scene's ordinary seeds and the stream finds part
/// of its headroom through it (blocking it outright moved the median step-up
/// from 1.1 s to 2.3-5.7 s); the overshoot seeds climbed about 8 % a second
/// from a start-up probe result (2.17 -> 2.87 Mbit/s on a 2 Mbit/s link in
/// 3.75 s).
pub(crate) const UNPROBED_GROWTH_PER_S: f64 = 0.03;

/// The longest gap between two steps that counts towards unprobed growth.
/// Feedback arrives every few milliseconds while the stream flows; a longer
/// gap is an idle or a stall, which proves nothing, and counted in full it
/// would let a 30 s idle publish about 1.9 x at the first feedback after it.
pub(crate) const MAX_GROWTH_STEP_S: f64 = 0.5;

/// What one step of the gate did with the raw estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateOutcome {
    /// Published as it is: the first sample, a decrease, no change, or the
    /// gate was off (saturated: the caller's ceiling bounds it).
    Passed,
    /// A rise up to a rate proved within the window.
    Recovered,
    /// A rise above what is proved, faster than
    /// `UNPROBED_GROWTH_PER_S`: the published value is under the raw one.
    Limited,
}

/// What the gate knows of the sender's saturation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Saturation {
    /// No ceiling reading yet: the gate is off (start-up is not restricted by
    /// a rule that needs history) but nothing is proved, only anchored.
    Unknown,
    /// Not saturated: the gate applies.
    No,
    /// Saturated: the gate is off and what is published is proved. The
    /// reported value is bounded by the reading's ceiling when it has one,
    /// so that bound, not goog_cc's larger raw target, is what is proved.
    Yes { ceiling_bps: Option<u64> },
}

/// The value last published (the anchor) and the rates proved lately: rises
/// are free up to the highest of those, and above it grow by at most
/// `UNPROBED_GROWTH_PER_S` unless a probe says otherwise.
#[derive(Debug, Default, Clone)]
pub(crate) struct GrowthGate {
    anchor_bps: Option<u64>,
    /// When the gate last stepped, for the rate of unprobed growth.
    last_us: Option<u64>,
    /// `(time, rate)` with rates strictly decreasing from front to back: a
    /// sliding maximum, O(1) amortised, bounded by the window's samples.
    proved: std::collections::VecDeque<(u64, u64)>,
}

impl GrowthGate {
    /// Forget the anchor and what was proved: the next sample becomes the
    /// anchor (a restart moves the estimate on purpose, to a rate nothing
    /// here has seen).
    pub(crate) fn reset(&mut self) {
        self.anchor_bps = None;
        self.last_us = None;
        self.proved.clear();
    }

    /// A restore puts the estimate back to a rate the path carried before a
    /// transient (`BaseRise`): that rate is the anchor and is proved, and
    /// what was proved before stays. Clearing it here made the recovery after
    /// goog_cc's next cut probe-only: `p8e` sat at 0.6 x the link for 12 s.
    pub(crate) fn restore(&mut self, now_us: u64, bps: u64) {
        self.anchor_bps = Some(bps);
        self.last_us = Some(now_us);
        self.prove(now_us, bps);
    }

    /// A probe cluster delivered `bps` (measured by `ProbeMeter`): that rate
    /// is proved for the window, whatever goog_cc published in the same
    /// feedback. The anchor stays: only a published value moves it.
    pub(crate) fn note_probe_result(&mut self, now_us: u64, bps: u64) {
        self.prove(now_us, bps);
    }

    /// The anchor, `None` before the first sample.
    #[cfg(test)]
    pub(crate) fn anchor_bps(&self) -> Option<u64> {
        self.anchor_bps
    }

    /// The highest rate proved within the window ending at `now_us`.
    pub(crate) fn proved_max_bps(&mut self, now_us: u64) -> Option<u64> {
        let from = now_us.saturating_sub(PROVED_WINDOW_US);
        while self.proved.front().is_some_and(|&(t, _)| t < from) {
            self.proved.pop_front();
        }
        self.proved.front().map(|&(_, v)| v)
    }

    fn prove(&mut self, now_us: u64, bps: u64) {
        while self.proved.back().is_some_and(|&(_, v)| v <= bps) {
            self.proved.pop_back();
        }
        self.proved.push_back((now_us, bps));
    }

    /// One sample at `now_us`. Returns what to publish and what happened.
    ///
    /// `raw_bps` is goog_cc's target rate. `saturation`: see [`Saturation`];
    /// saturated, the sender used its grant, which the estimate ceiling
    /// already handles, so the gate is off. `feedback`: this step is made on a
    /// feedback. Only a feedback earns unprobed growth credit and advances the
    /// gate's clock; any other step (an admission pass, a reseed) applies
    /// decreases and saturation only, so an ack blackout cannot accrue
    /// credit.
    ///
    /// A rate is proved by being published while saturated or by a probe's
    /// measured rate (`note_probe_result`), even when goog_cc published less.
    pub(crate) fn step(
        &mut self,
        now_us: u64,
        raw_bps: u64,
        saturation: Saturation,
        feedback: bool,
    ) -> (u64, GateOutcome) {
        // A gap with no feedback (an idle) is no evidence of room: at most
        // `MAX_GROWTH_STEP_S` of growth credit per step.
        let dt_s = if feedback {
            self.last_us
                .map_or(0.0, |t| now_us.saturating_sub(t) as f64 / 1e6)
                .min(MAX_GROWTH_STEP_S)
        } else {
            0.0
        };
        if feedback {
            self.last_us = Some(now_us);
        }
        match saturation {
            Saturation::Yes { ceiling_bps } => {
                // The caller bounds what it reports by the ceiling (and reads
                // the unbounded value to follow it), so raw is returned; the
                // bounded value is what is actually in force, and is the
                // one anchored and proved.
                let bounded = ceiling_bps.map_or(raw_bps, |c| raw_bps.min(c));
                self.anchor_bps = Some(bounded);
                self.prove(now_us, bounded);
                return (raw_bps, GateOutcome::Passed);
            }
            Saturation::Unknown => {
                self.anchor_bps = Some(raw_bps);
                return (raw_bps, GateOutcome::Passed);
            }
            Saturation::No => {}
        }
        let Some(anchor) = self.anchor_bps else {
            self.anchor_bps = Some(raw_bps);
            return (raw_bps, GateOutcome::Passed);
        };
        if raw_bps <= anchor {
            self.anchor_bps = Some(raw_bps);
            return (raw_bps, GateOutcome::Passed);
        }
        // Above what is proved: at most
        // `UNPROBED_GROWTH_PER_S` of the published value a second.
        let grown = anchor as f64 * (1.0 + UNPROBED_GROWTH_PER_S * dt_s);
        let proved = self.proved_max_bps(now_us).unwrap_or(0);
        if raw_bps <= proved {
            self.anchor_bps = Some(raw_bps);
            (raw_bps, GateOutcome::Recovered)
        } else if raw_bps as f64 <= grown {
            self.anchor_bps = Some(raw_bps);
            (raw_bps, GateOutcome::Passed)
        } else {
            let cap = (grown as u64).max(proved).max(anchor);
            self.anchor_bps = Some(cap);
            (cap, GateOutcome::Limited)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use GateOutcome::*;
    use Saturation::No;
    const YES: Saturation = Saturation::Yes { ceiling_bps: None };

    const S: u64 = 1_000_000;
    /// The rule's rate, written out here so that changing the constant fails
    /// these tests instead of moving them along with it.
    const RATE: f64 = 0.03;

    /// What the rule allows `from` to grow to in `dt_us`.
    /// The limit over one step: `RATE` a second, a step counted for at most
    /// half a second (literals, so a change to either constant fails here).
    fn lim(from: u64, dt_us: u64) -> u64 {
        (from as f64 * (1.0 + RATE * (dt_us as f64 / 1e6).min(0.5))) as u64
    }

    #[test]
    fn an_idle_gap_earns_no_more_than_half_a_second_of_growth() {
        let mut g = GrowthGate::default();
        g.step(0, 2_000_000, No, true);
        // 30 s without feedback, then goog_cc's target reads 4 Mbit/s.
        assert_eq!(
            g.step(30 * S, 4_000_000, No, true),
            (lim(2_000_000, 30 * S), Limited)
        );
        // Half a second of 3 %/s: 1.5 %, not 90 %.
        assert!(lim(2_000_000, 30 * S).abs_diff(2_030_000) <= 1);
    }

    #[test]
    fn a_restore_proves_its_rate_and_keeps_what_was_proved() {
        let mut g = GrowthGate::default();
        g.step(0, 5_000_000, YES, true); // proved while saturated
        g.restore(S, 4_000_000);
        // goog_cc cuts after the restore, then climbs back: free up to the
        // higher of the two proved rates, rate-limited above it.
        assert_eq!(g.step(2 * S, 1_000_000, No, true), (1_000_000, Passed));
        assert_eq!(g.step(3 * S, 4_000_000, No, true), (4_000_000, Recovered));
        assert_eq!(g.step(4 * S, 5_000_000, No, true), (5_000_000, Recovered));
        assert_eq!(
            g.step(5 * S, 6_000_000, No, true),
            (lim(5_000_000, S), Limited)
        );
        // The restored rate outlives the older proof by its own window: the
        // 5 Mbit/s proof is gone, a rise from 1 Mbit/s is free to the 4.
        assert_eq!(g.step(10 * S + 1, 1_000_000, No, true), (1_000_000, Passed));
        assert_eq!(g.proved_max_bps(10 * S + 2), Some(4_000_000));
        assert_eq!(
            g.step(10 * S + 2, 5_000_000, No, true),
            (4_000_000, Limited)
        );
    }

    /// Seed 0xfb1002: a probe measured 11 Mbit/s while goog_cc's loss
    /// controller capped the target at 5.9; when the cap lifted the raw
    /// returned to about 10 with no probe in the feedback.
    #[test]
    fn a_probe_measurement_is_proved_even_when_the_target_was_capped() {
        let mut g = GrowthGate::default();
        g.step(0, 5_900_000, No, true);
        g.note_probe_result(S, 11_000_000);
        assert_eq!(g.anchor_bps(), Some(5_900_000), "the anchor does not move");
        assert_eq!(g.proved_max_bps(S), Some(11_000_000));
        // Without the proof this would be Limited at about 5.9 x 1.015.
        assert_eq!(g.step(2 * S, 10_000_000, No, true), (10_000_000, Recovered));
        // Above the measured rate the limit applies again.
        assert_eq!(g.step(3 * S, 12_000_000, No, true), (11_000_000, Limited));
        // And the proof expires with the window.
        let late = S + PROVED_WINDOW_US + 1;
        assert_eq!(g.proved_max_bps(late), None);
    }

    #[test]
    fn the_first_sample_is_the_anchor() {
        let mut g = GrowthGate::default();
        assert_eq!(g.step(0, 2_000_000, No, true), (2_000_000, Passed));
        assert_eq!(g.anchor_bps(), Some(2_000_000));
    }

    #[test]
    fn an_unprobed_rise_above_anything_proved_grows_at_the_limit() {
        let mut g = GrowthGate::default();
        g.step(0, 2_000_000, No, true);
        let mut expect = 2_000_000;
        for i in 1..=4u64 {
            expect = lim(expect, S);
            // Raw is far above: the published value follows the limit.
            assert_eq!(g.step(i * S, 3_000_000, No, true), (expect, Limited));
        }
        assert_eq!(g.anchor_bps(), Some(expect));
        assert!(expect > 2_000_000 && (expect as f64) < 2_000_000.0 * (RATE * 4.0).exp());
    }

    /// A rise slower than the limit is not touched.
    #[test]
    fn an_unprobed_rise_slower_than_the_limit_passes_untouched() {
        let mut g = GrowthGate::default();
        g.step(0, 2_000_000, No, true);
        // 2 % a second in steps a tenth of a second apart, as feedback comes.
        let step = S / 10;
        let mut raw = 2_000_000u64;
        for i in 1..=60u64 {
            raw = (raw as f64 * 1.002) as u64;
            assert!(raw <= lim(g.anchor_bps().unwrap(), step), "premise");
            assert_eq!(g.step(i * step, raw, No, true), (raw, Passed));
        }
        assert_eq!(g.proved_max_bps(6 * S), None, "nothing was proved");
    }

    /// The same rise, steps a tenth of a second apart, is limited by time,
    /// not by the number of steps.
    #[test]
    fn the_limit_is_per_second_not_per_step() {
        let mut g = GrowthGate::default();
        g.step(0, 2_000_000, No, true);
        let mut last = 0;
        for i in 1..=30u64 {
            last = g.step(i * S / 10, 9_000_000, No, true).0;
        }
        // 3 s of 3 %/s, compounded per step: under the continuous bound.
        assert!(last > lim(2_000_000, 2 * S));
        assert!((last as f64) < 2_000_000.0 * (RATE * 3.0).exp());
    }

    #[test]
    fn a_probe_result_lets_a_rise_up_to_it_through_and_proves_it() {
        let mut g = GrowthGate::default();
        g.step(0, 2_000_000, No, true);
        g.note_probe_result(S, 2_400_000);
        assert_eq!(g.step(S, 2_400_000, No, true), (2_400_000, Recovered));
        assert_eq!(g.proved_max_bps(S), Some(2_400_000));
        // Above the probed rate, no probe: the limit from it, not a jump.
        assert_eq!(
            g.step(2 * S, 2_500_000, No, true),
            (lim(2_400_000, S), Limited)
        );
    }

    #[test]
    fn a_decrease_passes_at_once_and_lowers_the_anchor() {
        let mut g = GrowthGate::default();
        g.step(0, 2_000_000, No, true);
        assert_eq!(g.step(S, 1_500_000, No, true), (1_500_000, Passed));
        assert_eq!(g.anchor_bps(), Some(1_500_000));
    }

    /// After a cut, the way back up to a proved rate is free.
    #[test]
    fn recovery_after_a_decrease_up_to_a_proved_rate_is_free() {
        let mut g = GrowthGate::default();
        g.step(0, 1_000_000, No, true);
        g.note_probe_result(S, 2_000_000); // proved by a probe
        g.step(S, 2_000_000, No, true);
        g.step(2 * S, 1_200_000, No, true); // cut
        assert_eq!(g.step(3 * S, 1_700_000, No, true), (1_700_000, Recovered));
        assert_eq!(g.step(4 * S, 2_000_000, No, true), (2_000_000, Recovered));
    }

    #[test]
    fn a_rise_above_the_proved_maximum_is_rate_limited() {
        let mut g = GrowthGate::default();
        g.step(0, 1_000_000, No, true);
        g.note_probe_result(S, 2_000_000);
        g.step(S, 2_000_000, No, true);
        g.step(2 * S, 1_200_000, No, true);
        // Raw jumps past the proved 2 Mbit/s: the climb is free to it (the
        // limit from 1.2 would give less), and no further than the limit.
        assert!(lim(1_200_000, S) < 2_000_000, "premise");
        assert_eq!(g.step(3 * S, 2_600_000, No, true), (2_000_000, Limited));
        assert_eq!(
            g.step(4 * S, 2_600_000, No, true),
            (lim(2_000_000, S), Limited)
        );
    }

    #[test]
    fn proved_rates_expire_after_the_window() {
        let mut g = GrowthGate::default();
        g.step(0, 1_000_000, No, true);
        g.note_probe_result(S, 2_000_000);
        g.step(S, 2_000_000, No, true);
        g.step(2 * S, 1_200_000, No, true);
        let late = S + PROVED_WINDOW_US + 1;
        assert_eq!(g.proved_max_bps(late), None);
        // Nothing proved: only the limit from the anchor, over the whole gap.
        let expect = lim(1_200_000, late - 2 * S);
        assert!(expect < 1_700_000, "premise: the limit binds");
        assert_eq!(g.step(late, 1_700_000, No, true), (expect, Limited));
    }

    #[test]
    fn a_saturated_publish_is_proved_and_the_gate_is_off() {
        let mut g = GrowthGate::default();
        g.step(0, 2_000_000, No, true);
        assert_eq!(g.step(S, 2_800_000, YES, true), (2_800_000, Passed));
        assert_eq!(g.proved_max_bps(S), Some(2_800_000));
        // Unsaturated again: a fall and the way back are free to 2.8, and
        // above it the limit applies.
        g.step(2 * S, 1_500_000, No, true);
        assert_eq!(g.step(3 * S, 2_800_000, No, true), (2_800_000, Recovered));
        assert_eq!(
            g.step(4 * S, 2_950_000, No, true),
            (lim(2_800_000, S), Limited)
        );
    }

    #[test]
    fn a_reset_clears_the_anchor_and_what_was_proved() {
        let mut g = GrowthGate::default();
        g.step(0, 2_000_000, YES, true);
        g.reset();
        assert_eq!(g.anchor_bps(), None);
        assert_eq!(g.proved_max_bps(0), None);
        assert_eq!(g.step(0, 3_000_000, No, true), (3_000_000, Passed));
        // The time of the last step went with it: no growth credit.
        assert_eq!(g.step(0, 9_000_000, No, true), (3_000_000, Limited));
    }

    /// Whatever the raw series does, with no evidence or saturation the
    /// published value is never above raw and never above what the limit
    /// allows from the previous one; the proved deque stays empty.
    #[test]
    fn without_evidence_the_published_value_never_outgrows_the_limit() {
        let mut g = GrowthGate::default();
        let (mut limited, mut prev, mut first) = (0, 0u64, 0u64);
        for i in 0..400u64 {
            let raw = 1_000_000 + (i * 37_000) % 1_700_000;
            let (p, o) = g.step(i * 100_000, raw, No, true);
            if i == 0 {
                first = raw;
            } else {
                assert!(p <= raw, "i={i}");
                assert!(p <= lim(prev, 100_000).max(prev), "i={i} p={p} prev={prev}");
                assert!(p as f64 <= first as f64 * (RATE * i as f64 / 10.0).exp() + 1.0);
            }
            prev = p;
            limited += (o == Limited) as u32;
            assert!(g.proved.is_empty());
        }
        assert!(
            limited > 0,
            "the limit never bound, the test proved nothing"
        );
    }

    /// goog_cc's raw target drifts to 2.8 Mbit/s over a link of about 2.2
    /// while a probe cluster is only partly acknowledged; the cluster that
    /// completes reads 2.0. Raw must neither be published past the limit nor
    /// proved: only the measured 2.0 is.
    #[test]
    fn a_probe_result_below_a_drifted_raw_does_not_prove_raw() {
        let mut g = GrowthGate::default();
        g.step(0, 2_200_000, No, true);
        let (p, o) = g.step(S, 2_800_000, No, true);
        assert_eq!((p, o), (lim(2_200_000, S), Limited));
        g.note_probe_result(S, 2_000_000);
        let (p, o) = g.step(S + 1, 2_800_000, No, true);
        assert!(p < 2_800_000 && o == Limited, "{p} {o:?}");
        assert_eq!(g.proved_max_bps(S + 1), Some(2_000_000));
        // Nor is a later climb back to the drifted raw free.
        g.step(2 * S, 1_500_000, No, true);
        let (p, o) = g.step(3 * S, 2_800_000, No, true);
        assert!(p < 2_800_000 && o == Limited, "{p} {o:?}");
    }

    /// Saturated, what is published is bounded by the reading's ceiling, so
    /// the ceiling is what is anchored and proved, not goog_cc's larger raw.
    #[test]
    fn a_saturated_step_proves_the_ceiling_bounded_value() {
        let mut g = GrowthGate::default();
        let sat = Saturation::Yes {
            ceiling_bps: Some(2_000_000),
        };
        assert_eq!(g.step(0, 3_000_000, sat, true), (3_000_000, Passed));
        assert_eq!(g.anchor_bps(), Some(2_000_000));
        assert_eq!(g.proved_max_bps(0), Some(2_000_000));
        // Raw under the ceiling is published as it is.
        assert_eq!(g.step(S, 1_800_000, sat, true), (1_800_000, Passed));
        // Unsaturated, the climb back to the raw that was never published is
        // not free.
        let (p, o) = g.step(2 * S, 3_000_000, Saturation::No, true);
        assert!(p < 3_000_000 && o == Limited, "{p} {o:?}");
    }

    /// Before the first ceiling reading the gate is off, but what goog_cc
    /// reaches is only anchored, never proved.
    #[test]
    fn with_no_reading_yet_the_gate_is_off_and_proves_nothing() {
        let mut g = GrowthGate::default();
        assert_eq!(
            g.step(0, 2_000_000, Saturation::Unknown, true),
            (2_000_000, Passed)
        );
        assert_eq!(
            g.step(S, 6_000_000, Saturation::Unknown, true),
            (6_000_000, Passed)
        );
        assert_eq!(g.anchor_bps(), Some(6_000_000));
        assert_eq!(g.proved_max_bps(S), None);
    }

    /// An ack blackout: admission passes step the gate every few
    /// milliseconds with no feedback. They must not accrue growth credit.
    #[test]
    fn steps_without_feedback_earn_no_growth() {
        let mut g = GrowthGate::default();
        g.step(0, 2_000_000, No, true);
        for i in 1..=300u64 {
            let (p, o) = g.step(i * S / 10, 3_000_000, No, false);
            assert_eq!((p, o), (2_000_000, Limited), "i={i}");
        }
        assert_eq!(g.anchor_bps(), Some(2_000_000));
        // The next feedback, 30 s on, earns at most half a second.
        assert_eq!(
            g.step(30 * S, 3_000_000, No, true),
            (lim(2_000_000, 30 * S), Limited)
        );
        // A decrease still passes without feedback.
        assert_eq!(g.step(31 * S, 1_000_000, No, false), (1_000_000, Passed));
    }
}
