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
//! | 4 | `max_frame_bytes` is a ceiling, not a request: a frame is `min(demand, cap)` and never padded up to it. | (b) raising the cap 100 -> 200 KB gave a *smaller* first P (59.6 -> 49.0 KB) |
//! | 5 | The cap is soft. A P frame may exceed it by `cap_overshoot` (1.0: hard; S2 saw 1.08 when the cap is reachable, 1.69 and 2.88 when it is not). An IDR exceeds it by up to 13 %. The excess is debt on the bucket, repaid by the frames after it. | (b) "P within ~8 %... overshoot of 1.7-2.9x when it is not achievable"; IDR -6 %..+13 % |
//! | 6 | A rate step is not instant: the first frame after a large step is `1.8x` the new average and the excess decays to nothing over four frames, for a rise and for a fall. S2 only measured x0.25 and x4 (a relative step `|new - old| / max` of 0.75); for a smaller step the excess is scaled linearly (a 10 % step: 0.1 x 0.8 of an average frame). `full_step_transient` takes S2 literally and gives 1.8x to every step. | (a) VBR x0.25: 21.0, 14.8, 16.3, 13.2 KB against 12.5 KB new average (1.7x first, settles in 3-4); x4 with a 100 ms HRD: 86 KB against 50 KB (1.7x), "settles in 3-4 frames". The scaling is an assumption. |
//! | 7 | A re-send of the rate buffers may reset the VCE's buffer level (`resend_resets_level`). S2 did not measure this (it re-sent rarely). Off by default; T4 `h264_direct_noisy_rate_sequence_tracks_without_a_burst` measures it. | not measured: the open question of review item 13 |
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
/// The excess of the first frame after a rate step over the new average frame
/// (S2 a: 0.7 to 0.8 in both directions with a 100 ms HRD), for a relative
/// step of [`STEP_MEASURED`].
const STEP_EXCESS: f64 = 0.8;
/// The relative step `|new - old| / max(new, old)` of S2's x4 and x0.25.
const STEP_MEASURED: f64 = 0.75;
/// The frames over which the step's excess decays to nothing (S2 a: "settles
/// in 3-4 frames").
const STEP_SETTLE_FRAMES: u32 = 4;
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
    /// Behaviour 6 at S2's size for every step, however small (the pessimistic
    /// reading), instead of scaled with the step.
    pub full_step_transient: bool,
}

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
            full_step_transient: false,
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

    /// Every rate step gets S2's 1.8x first frame, even a 10 % one.
    pub fn with_full_step_transient(self) -> Self {
        Self {
            full_step_transient: true,
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
        let stepped = self.last.is_some_and(|l| l.bps != rate.bps);
        if let (true, Some(l)) = (stepped, self.last) {
            let (old, new) = (f64::from(l.bps), bps);
            let rel = (new - old).abs() / old.max(new).max(1.0);
            self.step_excess = if m.full_step_transient {
                STEP_EXCESS
            } else {
                STEP_EXCESS * (rel / STEP_MEASURED).min(1.0)
            };
        }
        self.frames_since_step = if stepped {
            0
        } else if self.last.is_none() {
            // The first rate of a stream is no step.
            STEP_SETTLE_FRAMES
        } else {
            self.frames_since_step.saturating_add(1)
        };
        let target_changed = self.last.is_some_and(|l| l != *rate);
        self.last = Some(*rate);
        let in_transient = m.honour_target
            && self.frames_since_step < STEP_SETTLE_FRAMES
            && self.step_excess > 0.0;
        let factor = if in_transient {
            1.0 + self.step_excess
                * (1.0 - f64::from(self.frames_since_step) / f64::from(STEP_SETTLE_FRAMES))
        } else {
            1.0
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
        let limit_bytes = room * overshoot;
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

    /// Behaviour 6: x4 gives 1.8x on the first frame and settles in four.
    #[test]
    fn a_rate_step_gives_one_point_eight_on_the_first_frame_and_settles_in_four() {
        let m = model();
        let low = RateTarget::steady(3_000_000, FPS);
        let high = RateTarget::steady(12_000_000, FPS);
        let mut st = RateState::default();
        p_frames(&mut st, &m, &low, 60);
        let (up, stats) = p_frames(&mut st, &m, &high, 8);
        let ratio: Vec<f64> = up.iter().map(|&b| b as f64 / avg(high.bps)).collect();
        for (i, want) in [1.8, 1.6, 1.4, 1.2, 1.0].iter().enumerate() {
            assert!((ratio[i] - want).abs() < 0.03, "frame {i}: {ratio:?}");
        }
        assert!(
            ratio[5..].iter().all(|r| (r - 1.0).abs() < 0.03),
            "{ratio:?}"
        );
        assert_eq!(stats.frames_in_transient, 4, "premise: the transient ran");
        // The same on the way down: the first frame is above the new average.
        let (down, _) = p_frames(&mut st, &m, &low, 8);
        let first = down[0] as f64 / avg(low.bps);
        assert!((first - 1.8).abs() < 0.2, "{first}");
        assert!(down[7] as f64 / avg(low.bps) < 1.05);
        // A 10 % step is a tenth of that excess, unless S2 is read literally.
        let small = RateTarget::steady(13_200_000, FPS);
        let first = |m: &PcmRateModel| {
            let mut st = RateState::default();
            p_frames(&mut st, m, &high, 60);
            p_frames(&mut st, m, &small, 1).0[0] as f64 / avg(small.bps)
        };
        assert!((first(&m) - 1.0).abs() < 0.12, "{}", first(&m));
        assert!(
            (first(&m.with_full_step_transient()) - 1.8).abs() < 0.03,
            "{}",
            first(&m.with_full_step_transient())
        );
        // The bps-only baseline has no transient.
        let b = PcmRateModel::bps_only(300_000, 200_000, FPS);
        let mut st = RateState::default();
        p_frames(&mut st, &b, &low, 60);
        let (flat, _) = p_frames(&mut st, &b, &high, 8);
        assert!(
            (flat[2] as f64 / avg(high.bps) - 1.0).abs() < 0.03,
            "{flat:?}"
        );
    }

    /// Behaviour 2 and 3: a plain small VBV cuts the first IDR, the raised
    /// one holds the budget at about three quarters of it.
    #[test]
    fn a_small_vbv_cuts_the_idr_and_the_raised_one_holds_the_budget() {
        let m = model();
        let rate = RateTarget::steady(6_000_000, FPS);
        let mut st = RateState::default();
        let (idr, o) = st.frame_bytes(&m, true, &rate);
        let budget = rate.idr_max_bytes as f64;
        assert!(
            idr as f64 > 0.7 * budget && idr as f64 <= budget * IDR_CAP_OVERSHOOT,
            "{idr} for a budget of {budget}"
        );
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
        assert!(cut < idr / 2, "the VBV cut the IDR: {cut} against {idr}");
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
}
