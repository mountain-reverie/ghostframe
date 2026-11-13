// SPDX-License-Identifier: AGPL-3.0-or-later
//! The harness's model of the encoder's rate control: how big a frame is, given
//! the [`RateTarget`] the backend was told.
//!
//! Without a model a scene fixes its frame sizes, so the rate a backend is
//! told changes nothing and a stream under a rate policy proves nothing about
//! rate control. The model is deliberately a *measured* one: every behaviour
//! below is one S2 saw on the real VCE (`spike/h264-ltr/S2-RESULTS.md`, Polaris10,
//! Mesa 26.2.4), and where S2 did not measure something the model says so
//! instead of inventing it. A model of `min(demand, cap)` alone would just
//! encode the hypothesis the policy is built on.
//!
//! | # | Behaviour | S2 source |
//! |---|-----------|-----------|
//! | 1 | VBR: a frame is no bigger than the content demands. Nothing is stuffed (CBR stuffs a static screen, VBR does not). | (a) "CBR stuffs every frame": 25.0 KB per P against 11.9 KB for VBR on frozen content |
//! | 2 | The VBV is a token bucket: capacity `vbv_bits / 8`, refilled by `bps / 8 / fps` a frame, 75 % full at the start. A small VBV cuts the first IDR. | (a) "VBV is settable": HRD 1.2 Mbit, initial fullness 75 % takes the first IDR from 166-198 KB to 103 KB |
//! | 3 | An IDR is sent with the raised VBV (`max(vbv, idr_max_bytes * 8)`) and its HRD re-send carries the initial fullness, so the bucket holds at least 75 % of the raised capacity at an IDR. The P frame after it falls back to the P capacity. | the backend's choice (`rate_commands`); S2 only measured the fullness at open |
//! | 3b | A raised IDR does not fill its ceiling: VBR codes the content, and the content at QP is smaller than the budget. A raised IDR is `idr_fill` (0.6) of what the budget and bucket allow. PROVISIONAL: one scene, one rate (3 Mbit/s: 55.9 and 56.7 KB against a 93.75 KB budget, 0.60x; 26.2 and 26.4 KB with no raise, so the raise helps 2.1x; at 12 Mbit/s 140.9-141.4 KB against 99.4-103.4 KB, 1.4x and 0.38 of the budget: the fraction falls with the rate, the content limits it). The model's own unraised IDR (the 0.1 s VBV cut, 37.5 KB there) is not that small, so the model gives 1.5x, not 2.1x, for the raise. | T4 `h264_direct_the_idr_cap_and_vbv_are_honoured` on the RX 480 (2026-11-05); S2 (a) for the unraised cut |
//! | 4 | `max_frame_bytes` is a ceiling, not a request: a frame is `min(demand, cap)` and never padded up to it. | (b) raising the cap 100 -> 200 KB gave a *smaller* first P (59.6 -> 49.0 KB) |
//! | 5 | The cap is soft. A P frame may exceed it by `cap_overshoot` (1.0: hard; S2 saw 1.08 when the cap is reachable, 1.69 and 2.88 when it is not). An IDR exceeds it by up to 13 %. The excess is debt on the bucket, repaid by the frames after it. | (b) "P within ~8 %... overshoot of 1.7-2.9x when it is not achievable"; IDR -6 %..+13 % |
//! | 6 | A re-send of the rate buffers on a P frame, a rate step or not (the backend starts the HRD at 40 % fullness on it, `P_RESEND_HRD_FULLNESS_PCT`), gives frames of `RESEND_PROFILE` times the average over the next three: 1.00, 0.56, 0.87 (a short dip of about 19 % of the bytes over three frames, no burst). The profile is the same for a pure re-send and for +-10 % steps, and for a rise or a fall, so it does not depend on the step. PROVISIONAL, from T4 `h264_direct_resend_fullness_sweep` on the RX 480 (2026-11-05; settled mean about 1.0 at every fullness): 30 %: 0.71 / 0.41 / 0.81; 40 %: 1.00 (0.99-1.00) / 0.56 / 0.87; 50 %: 1.32 / 0.69 / 0.88; 60 %: 1.40 / 0.92 / 0.97; 75 % (the earlier backend): 1.40 / ~1.08. The burst calibration of the earlier backend (a first frame `step_excess_up` 0.8 / `step_excess_down` 0.7 / `resend_excess` 0.4 over the average, settled on the second frame; T4 `small_rate_steps_first_frame_ratio`: rise 1.76-1.90x, fall 1.56-1.82x, pure re-send 1.40x) stays as [`PcmRateModel::with_burst_resend`], to characterise a backend that re-sends at 75 %. S2 (a) had 1.7-1.8x settling in 3-4 frames at a 100 ms HRD, which `with_s2_transient` still gives. `step_scaling` is the exponent of `(|new - old| / max / 0.75).min(1)` on the burst's excess: 0, flat, is what the data say; 1 is the earlier linear assumption (`with_linear_step_scaling`). | T4 on the RX 480; S2 (a) for the pessimistic reading |
//! | 7 | A re-send of the rate buffers may reset the VCE's buffer level (`resend_resets_level`). S2 did not measure this (it re-sent rarely). Off by default; T4 `h264_direct_a_noisy_rate_sequence_at_4hz_tracks_without_a_burst` measured no burst from re-sends beyond the first-frame transient (worst 1 s window 1.11x the rates told, worst P 0.67x its cap), so it stays off. | not measured: the open question of review item 13 |
//! | 8 | A probe window's rise (the rate goes up by more than 30 % of the new rate) gives frames of `RAISE_PROFILE` times the average at the window's rate: 1.00, 0.36, 0.83 (a mean yield of 0.73 of the bytes asked for; min 0.63, max 0.79 over the runs). Frame 2 is the small one (0.30-0.42). `raise_yield` scales it: 0.86 is the VCE's minimum, the pessimistic gate. | T4 `h264_direct_probe_raise_lever_b_at_40pct`, RX 480, 2026-11-06 |
//! | 9 | The fall that ends a window (the first rate change after a raise, downwards) gives frames of `POST_RAISE_PROFILE`: 0.5, 0.5, then the settled rate. The restore is not free: two frames at half the rate's average. | the same T4 |
//!
//! `idr_fill` (3b) underestimates an IDR whose content is the limit: the VCE
//! gave up to 1.15 x its budget there, where the model gives 0.6 of it.
//!
//! Not modelled, because S2 only observed it on content the model has no
//! picture of: the first P after an IDR being "always big" (b), the QP-51
//! content floor (a), the cap latching only when the RC command is re-issued
//! (the nudge: `rate_commands` and T4 own that, the model sees the target as
//! the backend resolved it), and the 8 P frames of a rolling intra refresh (h).

use super::backend::RateTarget;

/// The HRD's initial fullness, a fraction of the buffer (S2 a, the value the
/// backend sends).
const INITIAL_FULLNESS: f64 = 0.75;
/// The relative step `|new - old| / max(new, old)` of S2's x4 and x0.25, the
/// size at which the step's excess is taken as measured.
const STEP_MEASURED: f64 = 0.75;
/// The excess of the first frame after a rise over the new average frame
/// (T4 `small_rate_steps`: 1.76-1.90x, all sizes). Provisional.
const STEP_EXCESS_UP: f64 = 0.8;
/// The same for a fall (1.56-1.82x). Provisional.
const STEP_EXCESS_DOWN: f64 = 0.7;
/// The excess of a re-send at the same rate (1.40x over 8). Provisional.
const RESEND_EXCESS: f64 = 0.4;
/// The frames after a P re-send of the rate buffers, as multiples of the
/// average frame, under the backend's 40 % HRD fullness (T4
/// `h264_direct_resend_fullness_sweep`, 2026-11-05: 1.00 / 0.56 / 0.87, the
/// same for a pure re-send and for +-10 % steps). Provisional.
const RESEND_PROFILE: [f64; 3] = [1.0, 0.56, 0.87];
/// The frames of a probe window's rate (a rise of more than
/// [`RAISE_STEP_MIN`]) as multiples of the average frame at the new rate: 1.00,
/// then 0.36 (0.30-0.42 on the VCE) and 0.83 (0.6-1.0), a mean of 0.73 (min
/// 0.63, max 0.79) against a 33.3 kB window-rate frame. T4
/// `h264_direct_probe_raise_lever_b_at_40pct` on the RX 480, 2026-11-06. The
/// 40 % re-send's own profile (above) is 0.81 and has a frame 2 of 0.56; the
/// window's rise is a bigger step and the second frame is smaller.
const RAISE_PROFILE: [f64; 3] = [1.0, 0.36, 0.83];
/// The frames after the window, when the rate goes back down: a dip of about
/// 0.5 x the average for two frames (T4 lever B), then the settled rate.
const POST_RAISE_PROFILE: [f64; 3] = [0.5, 0.5, 1.0];
/// The frames over which the excess decays to nothing (T4: frame 2 on is back
/// at 0.9-1.05x). Provisional.
const STEP_SETTLE_FRAMES: u32 = 1;
/// S2's reading, used by [`PcmRateModel::with_s2_transient`]: 0.8 both ways,
/// settling over four frames.
const S2_STEP_EXCESS: f64 = 0.8;
const S2_SETTLE_FRAMES: u32 = 4;
/// A raised IDR is this fraction of what its budget and bucket allow (see
/// behaviour 3b). Provisional.
const IDR_FILL: f64 = 0.6;
/// An IDR may exceed its cap by this much (S2 b: +13 % at a 25 KB cap).
const IDR_CAP_OVERSHOOT: f64 = 1.13;

/// What the content costs and how the encoder limits it. See the module docs
/// for what each behaviour is and where it was measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PcmRateModel {
    /// What the content would cost with no limit, bytes.
    pub demand_p_bytes: usize,
    pub demand_idr_bytes: usize,
    /// The scene's frame rate (the bucket refills once per frame).
    pub fps: u32,
    /// `false`: the bps-only baseline of the first B6 commit, a VBR bucket of
    /// `bps / 10` and nothing else (no cap, no VBV from the target, no IDR
    /// budget, no step transient). `true`: the whole [`RateTarget`].
    pub honour_target: bool,
    /// A P frame may be this many times the smaller of the cap and the bucket
    /// (behaviour 5). 1.0 is a hard cap.
    pub cap_overshoot: f64,
    /// Behaviour 7: a change of the target (or an IDR's HRD) refills the
    /// bucket to its initial fullness.
    pub resend_resets_level: bool,
    /// Behaviour 6: the excess of the first frame after a rise / a fall, in
    /// average frames, and the frames it takes to settle.
    pub step_excess_up: f64,
    pub step_excess_down: f64,
    /// The excess of a re-send at the same rate (a changed cap, the nudge).
    pub resend_excess: f64,
    pub settle_frames: u32,
    /// Behaviour 6: `false`, the backend as it is, the frames after a P re-send
    /// following `RESEND_PROFILE` whatever the step; `true`, the burst
    /// calibration (`step_excess_*`, `resend_excess`, `settle_frames`).
    pub burst_resend: bool,
    /// Behaviour 6: the exponent of the relative step size on the excess. 0:
    /// every step gives the full excess; 1: linear in the step.
    pub step_scaling: f64,
    /// Behaviour 3b: the fraction of its ceiling a raised IDR reaches.
    pub idr_fill: f64,
    /// Behaviour 8, the probe raise: the frames of a big rise of the rate (a
    /// step of more than [`RAISE_STEP_MIN`] of the new rate, which is what a
    /// probe window's rate is) follow [`RAISE_PROFILE`] (1.00 / 0.36 / 0.83, a
    /// mean yield of 0.73 of the window's bytes) times this. 1.0: as measured
    /// on the VCE (T4 `h264_direct_probe_raise_lever_b_at_40pct`: mean 0.73,
    /// min 0.63, max 0.79). 0.86 gives the VCE's minimum (0.63), the
    /// pessimistic gate; 0.7 is more pessimistic still. The 75 %-fullness
    /// spike of the earlier backend gave 51.0 / 28.9 / 29.9 kB for a 33.3 kB
    /// window-rate frame, 1.10 of the 100 kB asked for.
    pub raise_yield: f64,
}

/// A rise of the target this large (fraction of the new rate) is the probe
/// raise, for [`PcmRateModel::raise_yield`].
pub(crate) const RAISE_STEP_MIN: f64 = 0.30;

impl PcmRateModel {
    /// The bps-only baseline (see [`honour_target`](Self::honour_target)).
    pub fn bps_only(demand_p_bytes: usize, demand_idr_bytes: usize, fps: u32) -> Self {
        Self {
            demand_p_bytes,
            demand_idr_bytes,
            fps,
            honour_target: false,
            cap_overshoot: 1.0,
            resend_resets_level: false,
            step_excess_up: STEP_EXCESS_UP,
            step_excess_down: STEP_EXCESS_DOWN,
            resend_excess: RESEND_EXCESS,
            settle_frames: STEP_SETTLE_FRAMES,
            burst_resend: false,
            step_scaling: 0.0,
            idr_fill: IDR_FILL,
            raise_yield: 1.0,
        }
    }

    /// The whole target, a hard P cap, the level persisting across re-sends.
    pub fn s2(demand_p_bytes: usize, demand_idr_bytes: usize, fps: u32) -> Self {
        Self {
            honour_target: true,
            ..Self::bps_only(demand_p_bytes, demand_idr_bytes, fps)
        }
    }

    /// A soft P cap: frames up to `factor` times what the cap and bucket allow.
    pub fn with_cap_overshoot(self, factor: f64) -> Self {
        Self {
            cap_overshoot: factor.max(1.0),
            ..self
        }
    }

    /// S2's transient for every step, even a 10 % one: 1.8x on the first
    /// frame, settling in four (the pessimistic reading, before the VCE was
    /// measured on the rate path).
    pub fn with_s2_transient(self) -> Self {
        Self {
            step_excess_up: S2_STEP_EXCESS,
            step_excess_down: S2_STEP_EXCESS,
            resend_excess: 0.0,
            settle_frames: S2_SETTLE_FRAMES,
            burst_resend: true,
            step_scaling: 0.0,
            ..self
        }
    }

    /// The burst calibration of the earlier backend, which re-sent the HRD at
    /// 75 % fullness: the first frame after a re-send is 1.8x (a rise), 1.7x
    /// (a fall) or 1.4x (the same rate) the average, settled on the second.
    pub fn with_burst_resend(self) -> Self {
        Self {
            burst_resend: true,
            ..self
        }
    }

    /// The excess scales linearly with the size of the step (the earlier
    /// assumption), up to the size measured.
    pub fn with_linear_step_scaling(self) -> Self {
        Self {
            step_scaling: 1.0,
            ..self
        }
    }

    /// The excess scales with the relative step to the power `exponent`.
    pub fn with_step_scaling(self, exponent: f64) -> Self {
        Self {
            step_scaling: exponent.max(0.0),
            ..self
        }
    }

    /// A big rise of the rate (a probe raise) yields only this fraction of the
    /// profile's bytes (behaviour 8); `1.0` is the profile as measured.
    pub fn with_raise_yield(self, raise_yield: f64) -> Self {
        Self {
            raise_yield: raise_yield.clamp(0.0, 1.0),
            ..self
        }
    }

    /// Re-sending the rate buffers refills the bucket (behaviour 7).
    pub fn with_resend_reset(self) -> Self {
        Self {
            resend_resets_level: true,
            ..self
        }
    }
}

/// Which limit decided a frame's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Limit {
    /// The content: nothing limited it.
    Demand,
    /// The frame cap (`max_frame_bytes` / `idr_max_bytes`).
    Cap,
    /// The VBV bucket.
    Bucket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameOutcome {
    pub limit: Limit,
    /// The frame is bigger than its cap (a soft cap let it through).
    pub over_cap: bool,
    /// The frame is inside a rate step's transient.
    pub in_transient: bool,
    /// The frame's size, and the average frame at the rate it was sent under.
    pub bytes: usize,
    pub allowed_bytes: usize,
}

/// What the model did to the scene's frames, for the scene's premises.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PcmModelStats {
    pub frames: u64,
    pub idr_frames: u64,
    /// Frames the content alone decided.
    pub frames_demand_limited: u64,
    /// Frames the cap decided (`frames_capped`).
    pub frames_capped: u64,
    /// Frames the VBV bucket decided (`frames_bucket_empty`).
    pub frames_bucket_empty: u64,
    /// Frames bigger than their cap (a soft cap).
    pub frames_over_cap: u64,
    /// Frames inside a rate step's transient.
    pub frames_in_transient: u64,
    /// Bytes the model gave the frames, and what the rates told allow
    /// (`bps / 8 / fps` a frame): their ratio is how far the stream ran over
    /// (or under) its target.
    pub bytes: u64,
    pub allowed_bytes: u64,
}

impl PcmModelStats {
    pub(crate) fn note(&mut self, is_idr: bool, o: FrameOutcome) {
        self.frames += 1;
        self.idr_frames += u64::from(is_idr);
        match o.limit {
            Limit::Demand => self.frames_demand_limited += 1,
            Limit::Cap => self.frames_capped += 1,
            Limit::Bucket => self.frames_bucket_empty += 1,
        }
        self.frames_over_cap += u64::from(o.over_cap);
        self.frames_in_transient += u64::from(o.in_transient);
        self.bytes += o.bytes as u64;
        self.allowed_bytes += o.allowed_bytes as u64;
    }
}

/// The model's state: the bucket level (bytes; negative while a soft cap's
/// excess is being repaid), the last target, and frames since the rate moved.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RateState {
    level: Option<f64>,
    last: Option<RateTarget>,
    frames_since_step: u32,
    /// The first frame's excess of the step in progress, in average frames.
    step_excess: f64,
    /// The step in progress is a big rise (see [`RAISE_STEP_MIN`]).
    raise_step: bool,
    /// The step in progress is the fall that ends a raise window.
    post_raise_step: bool,
    /// The last step was a big rise: the next fall is its window ending.
    after_raise: bool,
}

impl RateState {
    /// The size of the next access unit under `rate`, and what limited it.
    pub(crate) fn frame_bytes(
        &mut self,
        m: &PcmRateModel,
        is_idr: bool,
        rate: &RateTarget,
    ) -> (usize, FrameOutcome) {
        let fps = f64::from(m.fps.max(1));
        let bps = f64::from(rate.bps);
        let demand = if is_idr {
            m.demand_idr_bytes
        } else {
            m.demand_p_bytes
        } as f64;

        // The rate stepped (behaviour 6).
        let settle = if m.burst_resend {
            m.settle_frames
        } else {
            RESEND_PROFILE.len() as u32
        };
        let resent = self.last.is_some_and(|l| l.bps == rate.bps && l != *rate);
        let stepped = resent || self.last.is_some_and(|l| l.bps != rate.bps);
        if resent {
            self.step_excess = m.resend_excess;
            self.raise_step = false;
            self.post_raise_step = false;
        } else if let (true, Some(l)) = (stepped, self.last) {
            let (old, new) = (f64::from(l.bps), bps);
            let rel = (new - old).abs() / old.max(new).max(1.0);
            let base = if new > old {
                m.step_excess_up
            } else {
                m.step_excess_down
            };
            self.step_excess = base * (rel / STEP_MEASURED).min(1.0).powf(m.step_scaling);
            self.raise_step = new > old && rel >= RAISE_STEP_MIN;
            // The fall that follows a raise ends its window: the encoder dips.
            // Any fall right after a big rise gets the dip, a spike's give-back
            // included: the model cannot tell them apart from the rates, and a
            // give-back is a fall from a probe-window-sized rate like the
            // window's own end.
            self.post_raise_step = new < old && self.after_raise;
            self.after_raise = self.raise_step;
        }
        self.frames_since_step = if stepped {
            0
        } else if self.last.is_none() {
            // The first rate of a stream is no step.
            settle
        } else {
            self.frames_since_step.saturating_add(1)
        };
        let target_changed = self.last.is_some_and(|l| l != *rate);
        self.last = Some(*rate);
        let in_transient = m.honour_target
            && self.frames_since_step < settle
            && (!m.burst_resend || self.step_excess > 0.0);
        let factor = if in_transient && !m.burst_resend {
            let profile = if self.raise_step {
                &RAISE_PROFILE
            } else if self.post_raise_step {
                &POST_RAISE_PROFILE
            } else {
                &RESEND_PROFILE
            };
            profile[self.frames_since_step as usize]
        } else if in_transient {
            1.0 + self.step_excess
                * (1.0 - f64::from(self.frames_since_step) / f64::from(m.settle_frames.max(1)))
        } else {
            1.0
        };
        let factor = if in_transient && self.raise_step {
            factor * m.raise_yield
        } else {
            factor
        };

        // The bucket and the cap this frame is sent under (behaviours 2, 3).
        let (capacity, cap, overshoot) = if m.honour_target {
            let (vbv_bits, cap_bytes) = if is_idr {
                (
                    rate.vbv_bits.max(rate.idr_max_bytes.saturating_mul(8)),
                    rate.idr_max_bytes,
                )
            } else {
                (rate.vbv_bits, rate.max_frame_bytes)
            };
            let overshoot = if is_idr {
                IDR_CAP_OVERSHOOT
            } else {
                m.cap_overshoot
            };
            (
                f64::from(vbv_bits) / 8.0,
                (cap_bytes > 0).then_some(f64::from(cap_bytes)),
                overshoot,
            )
        } else {
            (bps / 10.0 / 8.0, None, 1.0)
        };
        let fresh = capacity * INITIAL_FULLNESS;
        let mut level = self.level.unwrap_or(fresh);
        if m.honour_target && (is_idr || (m.resend_resets_level && target_changed)) {
            level = if m.resend_resets_level {
                fresh
            } else {
                level.max(fresh)
            };
        }
        level = (level + bps / 8.0 / fps * factor).min(capacity);

        let room = cap.map_or(level, |c| c.min(level)).max(0.0);
        // A raised IDR is content-limited, short of its ceiling (3b).
        let fill = if is_idr && m.honour_target && cap.is_some() {
            m.idr_fill
        } else {
            1.0
        };
        let limit_bytes = room * overshoot * fill;
        let size = demand.min(limit_bytes);
        let limit = if size >= demand {
            Limit::Demand
        } else if cap.is_some_and(|c| c <= level) {
            Limit::Cap
        } else {
            Limit::Bucket
        };
        // A soft cap's excess is debt, bounded so one run of overshoots cannot
        // starve the stream for ever.
        self.level = Some((level - size).max(-capacity));
        (
            size as usize,
            FrameOutcome {
                limit,
                over_cap: cap.is_some_and(|c| size > c),
                in_transient,
                bytes: size as usize,
                allowed_bytes: (bps / 8.0 / fps) as usize,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: u32 = 30;

    fn model() -> PcmRateModel {
        PcmRateModel::s2(300_000, 200_000, FPS)
    }

    fn avg(bps: u32) -> f64 {
        f64::from(bps) / 8.0 / f64::from(FPS)
    }

    /// `n` P frames at `rate`, with the stats they produced.
    fn p_frames(
        st: &mut RateState,
        m: &PcmRateModel,
        rate: &RateTarget,
        n: usize,
    ) -> (Vec<usize>, PcmModelStats) {
        let mut stats = PcmModelStats::default();
        let sizes = (0..n)
            .map(|_| {
                let (b, o) = st.frame_bytes(m, false, rate);
                stats.note(false, o);
                b
            })
            .collect();
        (sizes, stats)
    }

    /// Behaviour 8: a probe raise (a rate step of over 30 %) yields the profile
    /// times `raise_yield`; a small step, or a fall, is not touched by it; the
    /// frames after the transient are the settled rate whatever the yield.
    #[test]
    fn a_raise_yields_the_profile_scaled_and_only_a_big_rise_is_scaled() {
        let (base, raised) = (2_500_000u32, 8_000_000u32);
        // Content that fills any bucket: the frames are bucket-limited.
        let run = |m: PcmRateModel, from: u32, to: u32| {
            let mut st = RateState::default();
            p_frames(&mut st, &m, &RateTarget::steady(from, FPS), 90);
            let (sizes, _) = p_frames(&mut st, &m, &RateTarget::steady(to, FPS), 3);
            let (after, _) = p_frames(&mut st, &m, &RateTarget::steady(to, FPS), 20);
            (sizes, after)
        };
        let full = run(model(), base, raised).0;
        let pess = run(model().with_raise_yield(0.7), base, raised).0;
        let total = |v: &[usize]| v.iter().sum::<usize>() as f64;
        assert!(
            total(&full) > avg(raised) * 1.5,
            "premise: a raise gives bytes: {full:?}"
        );
        let ratio = total(&pess) / total(&full);
        assert!(
            (0.65..=0.75).contains(&ratio),
            "yield 0.7 gives about 0.7 of the bytes: {ratio:.2} ({pess:?} vs {full:?})"
        );
        // Not a big rise: untouched.
        let small_full = run(model(), base, base + 200_000);
        let small_pess = run(model().with_raise_yield(0.7), base, base + 200_000);
        assert_eq!(small_full, small_pess, "a 8 % step is no probe raise");
        let fall_full = run(model(), raised, base);
        let fall_pess = run(model().with_raise_yield(0.7), raised, base);
        assert_eq!(fall_full, fall_pess, "a fall is no raise");
        // The settled frames after the transient are the same.
        assert_eq!(
            run(model(), base, raised).1,
            run(model().with_raise_yield(0.7), base, raised).1
        );
    }

    #[test]
    fn a_demanding_stream_is_held_to_the_rate() {
        let rate = RateTarget::steady(3_000_000, FPS);
        let mut st = RateState::default();
        let (sizes, _) = p_frames(&mut st, &model(), &rate, 120);
        let tail: usize = sizes[30..].iter().sum();
        let want = avg(3_000_000) * 90.0;
        assert!(
            (tail as f64 - want).abs() <= want * 0.02,
            "{tail} vs {want}"
        );
        assert!(sizes[0] > sizes[119], "the first frames spend the bucket");
    }

    /// Behaviour 1: a static screen costs what it costs, however big the rate.
    #[test]
    fn content_cheaper_than_the_rate_is_never_stuffed() {
        let m = PcmRateModel::s2(3_000, 200_000, FPS);
        let rate = RateTarget::steady(6_000_000, FPS);
        let mut st = RateState::default();
        let (sizes, stats) = p_frames(&mut st, &m, &rate, 60);
        assert!(sizes.iter().all(|&s| s == 3_000), "{sizes:?}");
        assert_eq!(
            stats.frames_demand_limited, 60,
            "premise: nothing limited it"
        );
    }

    /// Behaviour 4: the cap is a ceiling. Idle time fills the bucket to its
    /// capacity (one cap, here), so the burst after it is exactly the cap.
    #[test]
    fn the_cap_bounds_a_burst_and_is_never_a_request() {
        let rate = RateTarget::steady(6_000_000, FPS);
        let cheap = PcmRateModel::s2(500, 200_000, FPS);
        let mut st = RateState::default();
        p_frames(&mut st, &cheap, &rate, 30);
        let (burst, stats) = p_frames(&mut st, &model(), &rate, 1);
        assert_eq!(burst[0], rate.max_frame_bytes as usize);
        assert_eq!(stats.frames_capped, 1, "premise: the cap decided");
        // A cap far above the content is not reached: no padding up to it.
        let mut big = rate;
        big.max_frame_bytes *= 100;
        big.vbv_bits *= 100;
        let mut st = RateState::default();
        let (sizes, _) = p_frames(&mut st, &cheap, &big, 10);
        assert!(sizes.iter().all(|&s| s == 500), "{sizes:?}");
    }

    /// Behaviour 5: a soft cap lets a frame through, and the excess is repaid.
    #[test]
    fn a_soft_cap_overshoots_by_the_factor_and_pays_it_back() {
        let rate = RateTarget::steady(6_000_000, FPS);
        let cap = rate.max_frame_bytes as f64;
        let cheap = PcmRateModel::s2(500, 200_000, FPS);
        let soft = model().with_cap_overshoot(1.7);
        let mut st = RateState::default();
        p_frames(&mut st, &cheap, &rate, 30);
        let (sizes, stats) = p_frames(&mut st, &soft, &rate, 40);
        assert_eq!(sizes[0], (cap * 1.7) as usize, "{sizes:?}");
        assert!(stats.frames_over_cap >= 1, "premise: a frame went over");
        assert!(sizes.iter().all(|&s| s as f64 <= cap * 1.7 + 1.0));
        // Past the burst the stream is back at the rate, not above it.
        let tail: usize = sizes[20..].iter().sum();
        let want = avg(rate.bps) * 20.0;
        assert!((tail as f64) <= want * 1.02, "{tail} vs {want}");
        // And a hard cap never goes over.
        let mut st = RateState::default();
        p_frames(&mut st, &cheap, &rate, 30);
        let (_, hard) = p_frames(&mut st, &model(), &rate, 40);
        assert_eq!(hard.frames_over_cap, 0);
    }

    /// Behaviour 6: a P re-send of the rate buffers, a fall, a small step or the same
    /// rate with only the cap moved, gives frames of 1.00, 0.56 and 0.87 times
    /// the average and is settled on the fourth (T4
    /// `h264_direct_resend_fullness_sweep`, HRD at 40 %).
    #[test]
    fn a_resend_gives_the_measured_profile_and_settles_in_three() {
        let m = model();
        let low = RateTarget::steady(3_000_000, FPS);
        let high = RateTarget::steady(12_000_000, FPS);
        let mut nudged = high;
        nudged.max_frame_bytes += 1;
        let small = RateTarget::steady(13_200_000, FPS);
        let steady_then = |from: &RateTarget, to: &RateTarget| {
            let mut st = RateState::default();
            p_frames(&mut st, &m, from, 60);
            let (sizes, stats) = p_frames(&mut st, &m, to, 8);
            let r: Vec<f64> = sizes.iter().map(|&b| b as f64 / avg(to.bps)).collect();
            (r, stats)
        };
        // A fall, a 10 % step and a pure re-send all give the profile (a big
        // rise is a probe window's, and has its own: behaviour 8).
        for (from, to) in [(&high, &low), (&high, &small), (&high, &nudged)] {
            let (r, stats) = steady_then(from, to);
            for (k, want) in RESEND_PROFILE.iter().enumerate() {
                assert!((r[k] - want).abs() < 0.03, "{from:?} -> {to:?}: {r:?}");
            }
            assert!(
                r[3..].iter().all(|x| (x - 1.0).abs() < 0.03),
                "settled by the fourth frame: {r:?}"
            );
            assert_eq!(stats.frames_in_transient, 3, "premise: the profile ran");
        }
        // The same target again is no re-send.
        let mut st = RateState::default();
        p_frames(&mut st, &m, &high, 60);
        p_frames(&mut st, &m, &nudged, 3);
        let (same, _) = p_frames(&mut st, &m, &nudged, 2);
        assert!((same[0] as f64 / avg(high.bps) - 1.0).abs() < 0.03);
        // No burst anywhere in it, and the dip is the cost (about 19 % of
        // three frames' bytes).
        let sum: f64 = RESEND_PROFILE.iter().sum();
        assert!(RESEND_PROFILE.iter().all(|&f| f <= 1.0));
        assert!((1.0 - sum / 3.0 - 0.19).abs() < 0.01, "{sum}");
        // The bps-only baseline has no transient.
        let b = PcmRateModel::bps_only(300_000, 200_000, FPS);
        let mut st = RateState::default();
        p_frames(&mut st, &b, &low, 60);
        let (flat, _) = p_frames(&mut st, &b, &high, 8);
        assert!(
            (flat[0] as f64 / avg(high.bps) - 1.0).abs() < 0.03,
            "{flat:?}"
        );
    }

    /// Behaviour 6, the earlier backend's calibration, kept to characterise a
    /// re-send at 75 % fullness: 1.8x (a rise) or 1.7x (a fall) on the first
    /// frame, whatever its size, settled on the second; 1.4x at the same rate.
    #[test]
    fn the_burst_calibration_gives_one_burst_frame_and_settles_in_one() {
        let m = model().with_burst_resend();
        let low = RateTarget::steady(3_000_000, FPS);
        let high = RateTarget::steady(12_000_000, FPS);
        let mut st = RateState::default();
        p_frames(&mut st, &m, &low, 60);
        let (up, stats) = p_frames(&mut st, &m, &high, 8);
        let ratio: Vec<f64> = up.iter().map(|&b| b as f64 / avg(high.bps)).collect();
        assert!((ratio[0] - 1.8).abs() < 0.03, "{ratio:?}");
        assert!(
            ratio[1..].iter().all(|r| (r - 1.0).abs() < 0.03),
            "{ratio:?}"
        );
        assert_eq!(stats.frames_in_transient, 1, "premise: the transient ran");
        let (down, _) = p_frames(&mut st, &m, &low, 8);
        let first = down[0] as f64 / avg(low.bps);
        assert!((first - 1.7).abs() < 0.03, "{first}");
        assert!(down[1] as f64 / avg(low.bps) < 1.05);
        // The size of the step does not matter (a 10 % step), unless asked to.
        let small = RateTarget::steady(13_200_000, FPS);
        let first = |m: &PcmRateModel| {
            let mut st = RateState::default();
            p_frames(&mut st, m, &high, 60);
            p_frames(&mut st, m, &small, 1).0[0] as f64 / avg(small.bps)
        };
        assert!((first(&m) - 1.8).abs() < 0.03, "{}", first(&m));
        let lin = first(&m.with_linear_step_scaling());
        assert!((lin - 1.0).abs() < 0.12, "{lin}");
        // S2's reading settles in four frames.
        let mut st = RateState::default();
        p_frames(&mut st, &m.with_s2_transient(), &low, 60);
        let (slow, _) = p_frames(&mut st, &m.with_s2_transient(), &high, 6);
        assert!(slow[3] as f64 / avg(high.bps) > 1.15, "S2 settles in four");
        // A re-send at the same rate (only the cap moved) is a 1.4x frame.
        let mut st = RateState::default();
        p_frames(&mut st, &m, &high, 60);
        let mut nudged = high;
        nudged.max_frame_bytes += 1;
        let (re, stats) = p_frames(&mut st, &m, &nudged, 3);
        assert!((re[0] as f64 / avg(high.bps) - 1.4).abs() < 0.03, "{re:?}");
        assert!((re[1] as f64 / avg(high.bps) - 1.0).abs() < 0.03, "{re:?}");
        assert_eq!(stats.frames_in_transient, 1);
    }

    /// Behaviour 2, 3 and 3b: a plain small VBV cuts the first IDR, the raised
    /// one is bigger and reaches `idr_fill` of the budget, not all of it.
    #[test]
    fn a_small_vbv_cuts_the_idr_and_the_raised_one_holds_the_budget() {
        let m = model();
        let rate = RateTarget::steady(6_000_000, FPS);
        let mut st = RateState::default();
        let (idr, o) = st.frame_bytes(&m, true, &rate);
        let budget = rate.idr_max_bytes as f64;
        assert!(
            idr as f64 > 0.5 * budget && idr as f64 <= budget * IDR_CAP_OVERSHOOT * IDR_FILL,
            "{idr} for a budget of {budget}"
        );
        // A model with a full ceiling reaches the budget (the earlier reading).
        let full = PcmRateModel { idr_fill: 1.0, ..m };
        let (filled, _) = RateState::default().frame_bytes(&full, true, &rate);
        assert!(filled as f64 > 0.7 * budget && filled > idr, "{filled}");
        assert!(idr > rate.max_frame_bytes as usize, "the raise is real");
        assert_ne!(o.limit, Limit::Demand, "premise: the budget decided");
        // Without the budget the IDR is cut by the 0.1 s VBV (S2 a).
        let mut plain = rate;
        plain.idr_max_bytes = 0;
        let mut st = RateState::default();
        let (cut, _) = st.frame_bytes(&m, true, &plain);
        assert!(
            (cut as f64) < 0.9 * f64::from(plain.vbv_bits) / 8.0 + avg(plain.bps),
            "{cut}"
        );
        assert!(
            idr as f64 > 1.3 * cut as f64,
            "the raise is worth less than 1.3x: {cut} against {idr}"
        );
    }

    /// Behaviour 7: a changed target leaves the level alone, unless the model
    /// is told a re-send refills it.
    #[test]
    fn a_resend_resets_the_level_only_when_asked_to() {
        let after_nudge = |m: PcmRateModel| {
            let cheap = PcmRateModel {
                demand_p_bytes: 500,
                ..m
            };
            let rate = RateTarget::steady(6_000_000, FPS);
            let mut st = RateState::default();
            // Idle fills the bucket, a run of big frames drains it...
            p_frames(&mut st, &cheap, &rate, 30);
            p_frames(&mut st, &m, &rate, 5);
            // ...and a cap-only change (a re-send at the same rate).
            let mut nudged = rate;
            nudged.max_frame_bytes += 1;
            p_frames(&mut st, &m, &nudged, 1).0[0]
        };
        let keeps = after_nudge(model());
        let resets = after_nudge(model().with_resend_reset());
        assert!(resets > keeps * 2, "kept {keeps}, reset {resets}");
    }

    /// The property that holds for any content and any sequence of rates a
    /// policy that applies at most every 250 ms can produce: in any one second
    /// the bytes are at most the rate, plus what the bucket held, plus the
    /// soft cap's debt, plus the step transients and the IDR's raised fullness.
    #[test]
    fn bytes_in_any_second_are_bounded_by_rate_vbv_cap_and_transients() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for overshoot in [1.0, 1.7] {
            let m = model().with_cap_overshoot(overshoot);
            let mut st = RateState::default();
            let mut rate = RateTarget::steady(4_000_000, FPS);
            // (bytes, step, idr) per frame.
            let mut frames: Vec<(f64, bool, bool)> = Vec::new();
            let mut max_rate = 0u32;
            let (mut max_vbv, mut max_idr_vbv) = (0.0f64, 0.0f64);
            for i in 0..1800 {
                let step = i > 0 && i % 8 == 0 && next() % 3 == 0;
                if step {
                    rate = RateTarget::steady(500_000 + (next() % 11_000_000) as u32, FPS);
                }
                let idr = i == 0 || next() % 90 == 0;
                max_rate = max_rate.max(rate.bps);
                max_vbv = max_vbv.max(f64::from(rate.vbv_bits) / 8.0);
                max_idr_vbv =
                    max_idr_vbv.max(f64::from(rate.vbv_bits.max(rate.idr_max_bytes * 8)) / 8.0);
                // Demand wanders between a static screen and far above the rate.
                let m = PcmRateModel {
                    demand_p_bytes: [500, 4_000, 300_000][(next() % 3) as usize],
                    ..m
                };
                let (b, _) = st.frame_bytes(&m, idr, &rate);
                frames.push((b as f64, step, idr));
            }
            let max_avg = avg(max_rate);
            // Capacity, the debt a soft cap leaves, the transients of the steps
            // in the window (two average frames each, at the largest rate), the
            // raised fullness each IDR adds.
            for w in frames.windows(FPS as usize) {
                let bytes: f64 = w.iter().map(|f| f.0).sum();
                let steps = w.iter().filter(|f| f.1).count() as f64;
                let idrs = w.iter().filter(|f| f.2).count() as f64;
                let bound = f64::from(max_rate) / 8.0
                    + max_vbv * overshoot
                    + steps * 2.0 * max_avg
                    + idrs * (INITIAL_FULLNESS * max_idr_vbv * IDR_CAP_OVERSHOOT);
                assert!(
                    bytes <= bound,
                    "overshoot {overshoot}: {bytes} bytes in a second, bound {bound}"
                );
            }
            assert!(
                frames.iter().filter(|f| f.1).count() > 20 && frames.iter().any(|f| f.2),
                "premise: the sequence stepped and made IDRs"
            );
        }
    }

    /// Behaviour 8: the frames of a probe window's rise follow the T4 profile
    /// (1.00 / 0.36 / 0.83, a mean of 0.73), and the fall that ends it dips to
    /// half for two frames, then is back at the rate's average.
    #[test]
    fn a_raise_window_follows_the_measured_profile_and_its_end_dips() {
        let (base, raised) = (2_500_000u32, 8_000_000u32);
        let m = model();
        let mut st = RateState::default();
        p_frames(&mut st, &m, &RateTarget::steady(base, FPS), 90);
        let (win, _) = p_frames(&mut st, &m, &RateTarget::steady(raised, FPS), 3);
        let (after, _) = p_frames(&mut st, &m, &RateTarget::steady(base, FPS), 6);
        let rel = |v: &[usize], avg: f64| v.iter().map(|&b| b as f64 / avg).collect::<Vec<_>>();
        let w = rel(&win, avg(raised));
        for (got, want) in w.iter().zip(RAISE_PROFILE) {
            assert!((got - want).abs() < 0.05, "window frames {w:?}");
        }
        let mean = w.iter().sum::<f64>() / 3.0;
        assert!((0.70..=0.76).contains(&mean), "mean yield {mean:.2}");
        let a = rel(&after, avg(base));
        assert!(
            a[0] < 0.6 && a[1] < 0.6,
            "two frames at about half after the window: {a:?}"
        );
        assert!(
            a[3..].iter().all(|&x| (x - 1.0).abs() < 0.05),
            "then the rate's average: {a:?}"
        );
        // A fall that does not follow a raise is the plain re-send profile.
        let mut st = RateState::default();
        p_frames(&mut st, &m, &RateTarget::steady(raised, FPS), 90);
        let (down, _) = p_frames(&mut st, &m, &RateTarget::steady(base, FPS), 3);
        let d = rel(&down, avg(base));
        assert!((d[1] - 0.56).abs() < 0.05, "a plain fall: {d:?}");
    }
}
