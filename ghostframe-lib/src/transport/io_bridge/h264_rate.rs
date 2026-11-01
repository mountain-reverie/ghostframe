//! H.264 rate control, Phase A (design section 4.3): a CBR target that follows
//! the bandwidth estimate, applied by reopening the encoder at an IDR.
//!
//! The ffmpeg encoder cannot change its rate mid-stream without an RC reset,
//! so a new rate means a new encoder, and a new encoder starts with an IDR.
//! That makes the rate a decision about *when to pay for an IDR*, which is the
//! whole of this policy:
//!
//! * The **target** is `clamp(share x estimate, floor, cap)` where the estimate
//!   is `bwe.snapshot().bitrate_bps` smoothed by a 250 ms EWMA. It is not the
//!   model controller's probe-inclusive rate.
//! * A **candidate** target is adopted when it moves more than 10 % from the
//!   last candidate, or every second, and never more than once per 250 ms.
//! * A candidate more than 30 % under the running rate **reopens at once**: the
//!   path has shrunk, the stream is overdriving it, and the IDR that comes with
//!   the reopen is itself cheaper at the lower rate. Every other change (a
//!   rise, or a shallower fall) waits for the next scheduled GOP IDR, where
//!   the IDR is paid for anyway.
//! * At most one reopen per 2 s.
//!
//! [`RatePolicy`] is pure: time and the estimate are arguments. The glue that
//! asks the encoder to reopen, and the harness's scripted CBR encoder, are at
//! the bottom of the file.

use super::*;
use std::time::{Duration, Instant};

/// Share of the estimate the stream may spend (design decision D4). It must
/// stay under the BWE ceiling's `SATURATION`: a sender that spends at least
/// that share of its grant is judged saturated and the ceiling engages, which
/// would shrink the estimate it is following.
pub(crate) const RATE_SHARE: f64 = 0.8;
const _: () = assert!(RATE_SHARE < crate::transport::bwe::SATURATION);

/// The target never goes under this (D4) ...
pub(crate) const RATE_FLOOR_BPS: u32 = 300_000;
/// ... or over this.
pub(crate) const RATE_CAP_BPS: u32 = 50_000_000;

/// The rate the encoder first opens at, before any estimate has been seen.
pub(crate) const RATE_INITIAL_BPS: u32 = 4_000_000;

/// Time constant of the estimate's EWMA.
const EWMA_TAU: Duration = Duration::from_millis(250);
/// A candidate is looked at no more often than this.
const EVAL_MIN_INTERVAL: Duration = Duration::from_millis(250);
/// A candidate is adopted at least this often, however little it moved.
const CANDIDATE_REFRESH: Duration = Duration::from_secs(1);
/// A candidate that moved more than this (fraction) from the last one is
/// adopted at once. It is also the dead band: a running rate within this of
/// the candidate is left alone, so an estimate wandering by a few percent
/// costs no reopen.
const MOVE_FRACTION: f64 = 0.10;
/// A candidate this far under the running rate reopens immediately.
const DROP_FRACTION: f64 = 0.30;
/// Reopens are at least this far apart.
pub(crate) const MIN_REOPEN_INTERVAL: Duration = Duration::from_secs(2);

/// What the policy wants done for the frame about to be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RateStep {
    /// Nothing.
    Keep,
    /// The first rate of the stream: the encoder opens (or already runs) at
    /// it, no reopen.
    Initial(u32),
    /// Reopen the encoder at `bps`; this frame is an IDR. `urgent` marks a
    /// fall of more than 30 % (its IDR is named [`IdrCause::RateDrop`]);
    /// otherwise this is the scheduled GOP IDR taking a new rate.
    Reopen { bps: u32, urgent: bool },
}

/// The pure rate policy. See the module docs.
#[derive(Debug, Clone, Default)]
pub(crate) struct RatePolicy {
    ewma_bps: Option<f64>,
    last_obs: Option<Instant>,
    /// The rate the encoder runs at, as last commanded.
    applied: Option<u32>,
    candidate: Option<u32>,
    candidate_at: Option<Instant>,
    last_eval: Option<Instant>,
    last_reopen: Option<Instant>,
}

fn relative_move(from: u32, to: u32) -> f64 {
    (f64::from(to) - f64::from(from)).abs() / f64::from(from.max(1))
}

impl RatePolicy {
    /// The rate the encoder was last told to run at.
    #[cfg(test)]
    pub(crate) fn applied(&self) -> Option<u32> {
        self.applied
    }

    /// A new session: the estimate starts over; the encoder keeps its rate.
    pub(crate) fn reset_session(&mut self) {
        self.ewma_bps = None;
        self.last_obs = None;
        self.candidate = None;
        self.candidate_at = None;
        self.last_eval = None;
    }

    fn target(&self) -> u32 {
        let e = self.ewma_bps.unwrap_or(0.0);
        let t = (RATE_SHARE * e).clamp(f64::from(RATE_FLOOR_BPS), f64::from(RATE_CAP_BPS));
        t as u32
    }

    fn observe(&mut self, now: Instant, estimate_bps: u64) {
        let est = estimate_bps as f64;
        match (self.ewma_bps, self.last_obs) {
            (Some(e), Some(t)) => {
                let dt = now.saturating_duration_since(t).as_secs_f64();
                let alpha = 1.0 - (-dt / EWMA_TAU.as_secs_f64()).exp();
                self.ewma_bps = Some(e + alpha * (est - e));
            }
            _ => self.ewma_bps = Some(est),
        }
        self.last_obs = Some(now);
    }

    fn reopen_allowed(&self, now: Instant) -> bool {
        self.last_reopen
            .is_none_or(|t| now.saturating_duration_since(t) >= MIN_REOPEN_INTERVAL)
    }

    /// Fold in the current estimate and say what the encoder should do for
    /// the frame about to be encoded. `at_gop` is whether that frame is the
    /// scheduled GOP IDR.
    pub(crate) fn step(&mut self, now: Instant, estimate_bps: u64, at_gop: bool) -> RateStep {
        self.observe(now, estimate_bps);
        let Some(applied) = self.applied else {
            let t = self.target();
            self.applied = Some(t);
            self.candidate = Some(t);
            self.candidate_at = Some(now);
            self.last_eval = Some(now);
            return RateStep::Initial(t);
        };
        if self
            .last_eval
            .is_none_or(|t| now.saturating_duration_since(t) >= EVAL_MIN_INTERVAL)
        {
            self.last_eval = Some(now);
            let target = self.target();
            let adopt = match (self.candidate, self.candidate_at) {
                (Some(c), Some(at)) => {
                    relative_move(c, target) > MOVE_FRACTION
                        || now.saturating_duration_since(at) >= CANDIDATE_REFRESH
                }
                _ => true,
            };
            if adopt {
                self.candidate = Some(target);
                self.candidate_at = Some(now);
            }
        }
        let cand = self.candidate.unwrap_or(applied);
        if relative_move(applied, cand) <= MOVE_FRACTION || !self.reopen_allowed(now) {
            return RateStep::Keep;
        }
        let dropped = f64::from(cand) < f64::from(applied) * (1.0 - DROP_FRACTION);
        if dropped || at_gop {
            self.applied = Some(cand);
            self.last_reopen = Some(now);
            return RateStep::Reopen {
                bps: cand,
                urgent: dropped,
            };
        }
        RateStep::Keep
    }
}

// ---------------------------------------------------------------------------
// The scripted CBR encoder of the browserless harness.
// ---------------------------------------------------------------------------

/// The harness's stand-in for a CBR encoder (see
/// [`InjectedH264::cbr`](super::InjectedH264)): frame sizes follow the rate the
/// bridge last set through [`Control::H264Rate`], an IDR is `idr_over_p` P
/// frames, and a reopen (or the GOP, or a pending IDR request) makes the next
/// frame an IDR. It takes exactly the controls the real backend does, so the
/// scenes exercise the real policy.
#[derive(Debug, Clone, Copy)]
pub(super) struct ScriptedCbr {
    fps: u32,
    idr_over_p: u32,
    rate_bps: u32,
    gop_frames: u32,
    reopen: bool,
    /// Frames since the last IDR, that IDR included; 0 before the first frame.
    since_idr: u32,
}

impl ScriptedCbr {
    pub(super) fn new(cfg: InjectedCbr) -> Self {
        Self {
            fps: cfg.fps.max(1),
            idr_over_p: cfg.idr_over_p.max(1),
            rate_bps: RATE_INITIAL_BPS,
            gop_frames: 0,
            reopen: false,
            since_idr: 0,
        }
    }

    pub(super) fn apply(&mut self, c: Control) {
        if let Control::H264Rate {
            bps,
            gop_frames,
            reopen,
        } = c
        {
            self.rate_bps = bps;
            self.gop_frames = gop_frames;
            self.reopen |= reopen;
        }
    }

    /// The next frame: `(is_idr, payload bytes)`. `requested` is an IDR the
    /// bridge asked for.
    pub(super) fn next_frame(&mut self, requested: bool) -> (bool, usize) {
        let gop_due = self.gop_frames > 0 && self.since_idr >= self.gop_frames;
        let is_idr = requested || self.reopen || gop_due || self.since_idr == 0;
        self.reopen = false;
        self.since_idr = if is_idr { 1 } else { self.since_idr + 1 };
        let p = (u64::from(self.rate_bps) / 8 / u64::from(self.fps)) as usize;
        (
            is_idr,
            if is_idr {
                p * self.idr_over_p as usize
            } else {
                p
            },
        )
    }
}

// ---------------------------------------------------------------------------
// Glue.
// ---------------------------------------------------------------------------

impl IoBridge {
    /// Send a control to the encoder (the frame worker's backend) and to the
    /// harness's scripted encoder, if there is one.
    pub(super) fn h264_control(&mut self, control: Control) {
        if let Some(s) = self.h264.scripted.as_mut() {
            s.apply(control);
        }
        self.backend_apply(control);
    }

    /// Run the rate policy for a frame the gate has let through. `gop_frames`
    /// is the GOP in frames.
    pub(super) fn h264_rate_step(&mut self, now: Instant, gop_frames: u32) {
        let estimate = self.bwe.snapshot().bitrate_bps;
        let at_gop = gop_frames > 0 && self.h264.frames_since_idr.is_some_and(|n| n >= gop_frames);
        let step = self.h264.rate.step(now, estimate, at_gop);
        self.h264_apply_rate_step(step, estimate, gop_frames);
    }

    /// Act on what the policy decided. Split from [`Self::h264_rate_step`] so a
    /// test can hand it a decision without steering the estimator.
    pub(super) fn h264_apply_rate_step(&mut self, step: RateStep, estimate: u64, gop_frames: u32) {
        match step {
            RateStep::Keep => {}
            RateStep::Initial(bps) => {
                self.h264.stats.rate_target_bps = bps;
                self.h264_control(Control::H264Rate {
                    bps,
                    gop_frames,
                    reopen: false,
                });
            }
            RateStep::Reopen { bps, urgent } => {
                self.h264.stats.rate_target_bps = bps;
                if urgent {
                    // The reopen makes this frame an IDR. It is a recovery
                    // IDR in every way that matters and gets the P hold (see
                    // `send_h264_frame`, which holds for every cause but
                    // `GopPeriodic`): the link has just shrunk, so this is the
                    // burst most likely to lose a fragment, and the P frames
                    // behind it would be undecodable without it and would
                    // only deepen the queue it has to cross.
                    self.expect_h264_idr(IdrCause::RateDrop);
                    self.h264.stats.rate_reopens_drop += 1;
                } else {
                    // The scheduled IDR taking a new rate: no hold, like any
                    // periodic IDR, and counted as one.
                    self.h264.stats.rate_reopens_gop += 1;
                }
                tracing::info!(
                    target: LOG_TARGET,
                    bps,
                    urgent,
                    estimate_bps = estimate,
                    "H.264 rate: reopening the encoder"
                );
                self.h264_control(Control::H264Rate {
                    bps,
                    gop_frames,
                    reopen: true,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBIT: u64 = 1_000_000;

    fn ms(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    /// Drive the policy at 60 Hz for `ms_len` from `from_ms`, a constant
    /// estimate, returning every non-Keep step with its time.
    fn run(
        p: &mut RatePolicy,
        t0: Instant,
        from_ms: u64,
        ms_len: u64,
        est: u64,
        at_gop: impl Fn(u64) -> bool,
    ) -> Vec<(u64, RateStep)> {
        let mut out = Vec::new();
        let mut t = from_ms;
        while t < from_ms + ms_len {
            let s = p.step(ms(t0, t), est, at_gop(t));
            if s != RateStep::Keep {
                out.push((t, s));
            }
            t += 16;
        }
        out
    }

    #[test]
    fn the_share_is_under_the_bwe_ceilings_saturation() {
        // Also a compile-time assertion; this names it in the test report and
        // pins the value the design chose.
        const { assert!(RATE_SHARE < crate::transport::bwe::SATURATION) };
        assert_eq!(RATE_SHARE, 0.8);
    }

    #[test]
    fn the_first_step_sets_the_initial_rate_from_the_share_of_the_estimate() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        assert_eq!(
            p.step(t0, 10 * MBIT, false),
            RateStep::Initial(8_000_000),
            "0.8 x 10 Mbit/s"
        );
        assert_eq!(p.applied(), Some(8_000_000));
    }

    #[test]
    fn the_target_is_clamped_to_the_floor_and_the_cap() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        assert_eq!(
            p.step(t0, 100_000, false),
            RateStep::Initial(RATE_FLOOR_BPS)
        );
        let mut p = RatePolicy::default();
        assert_eq!(
            p.step(t0, 10_000 * MBIT, false),
            RateStep::Initial(RATE_CAP_BPS)
        );
    }

    #[test]
    fn a_steady_estimate_never_reopens() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        let steps = run(&mut p, t0, 0, 20_000, 10 * MBIT, |_| true);
        assert_eq!(steps.len(), 1, "only the initial rate: {steps:?}");
    }

    #[test]
    fn an_estimate_wandering_inside_the_dead_band_costs_no_reopen() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        p.step(t0, 10 * MBIT, true);
        // +-8 % square wave every 500 ms, at a GOP boundary on every frame:
        // the only thing standing between it and a reopen is the 10 % band.
        let mut reopens = 0;
        for i in 0..1200u64 {
            let est = if (i / 30) % 2 == 0 {
                10_800_000
            } else {
                9_200_000
            };
            if matches!(
                p.step(ms(t0, 16 * (i + 1)), est, true),
                RateStep::Reopen { .. }
            ) {
                reopens += 1;
            }
        }
        assert_eq!(reopens, 0);
    }

    #[test]
    fn the_ewma_smooths_a_step_with_a_250_ms_time_constant() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        p.step(t0, 10 * MBIT, false);
        // One observation 250 ms after a step to 20 Mbit/s: 1 - 1/e = 63.2 %
        // of the way.
        p.observe(ms(t0, 250), 20 * MBIT);
        let e = p.ewma_bps.unwrap();
        let want = 10e6 + (1.0 - (-1.0f64).exp()) * 10e6;
        assert!((e - want).abs() < 1.0, "{e} vs {want}");
        // And a long gap converges on the new value.
        p.observe(ms(t0, 10_250), 20 * MBIT);
        assert!((p.ewma_bps.unwrap() - 20e6).abs() < 1e3);
    }

    #[test]
    fn a_drop_over_30_percent_reopens_at_once_and_is_urgent() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        p.step(t0, 10 * MBIT, false);
        // The estimate falls to 5 Mbit/s (target 4 Mbit/s, half the 8 running).
        // Not at a GOP: only the drop rule can reopen.
        let steps = run(&mut p, t0, 16, 4_000, 5 * MBIT, |_| false);
        assert_eq!(steps.len(), 1, "{steps:?}");
        let (at, s) = steps[0];
        let RateStep::Reopen { bps, urgent } = s else {
            panic!("{s:?}");
        };
        assert!(urgent);
        // The candidate follows the smoothed estimate, so it can be adopted
        // while the EWMA is still on its way down; it is never under the
        // final target nor over the running rate's 70 %.
        assert!((4_000_000..=5_600_000).contains(&bps), "{bps}");
        // Within the EWMA plus the evaluation interval: the smoothed
        // estimate crosses the 30 % line (8 -> 5.6) after ~0.3 tau, then the
        // next evaluation.
        assert!(at <= 600, "reopened at {at} ms");
    }

    #[test]
    fn a_drop_under_30_percent_waits_for_the_gop_idr() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        p.step(t0, 10 * MBIT, false);
        // 10 -> 7.5 Mbit/s: target 6 Mbit/s, 25 % under 8: over the 10 % band,
        // under the 30 % line.
        let steps = run(&mut p, t0, 16, 3_000, 7_500_000, |_| false);
        assert!(steps.is_empty(), "no GOP, no reopen: {steps:?}");
        let steps = run(&mut p, t0, 3_016, 100, 7_500_000, |_| true);
        let RateStep::Reopen { bps, urgent } = steps[0].1 else {
            panic!("{steps:?}");
        };
        assert!(!urgent);
        // The EWMA approaches the estimate asymptotically; 3 s is 12 tau.
        assert!(bps.abs_diff(6_000_000) < 1_000, "{bps}");
    }

    #[test]
    fn an_increase_waits_for_the_gop_idr_and_then_takes_it() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        p.step(t0, 5 * MBIT, false);
        let steps = run(&mut p, t0, 16, 6_000, 10 * MBIT, |_| false);
        assert!(steps.is_empty(), "a rise reopens only at a GOP: {steps:?}");
        let steps = run(&mut p, t0, 6_016, 100, 10 * MBIT, |_| true);
        assert_eq!(steps.len(), 1, "{steps:?}");
        assert_eq!(steps[0].0, 6_016, "at the first GOP frame");
        let RateStep::Reopen { bps, urgent } = steps[0].1 else {
            panic!("{steps:?}");
        };
        assert!(!urgent);
        assert!(bps.abs_diff(8_000_000) < 1_000, "{bps}");
    }

    #[test]
    fn at_most_one_reopen_per_two_seconds() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        p.step(t0, 100 * MBIT / 10, true);
        // The estimate alternates between 10 and 3 Mbit/s every 500 ms, a GOP
        // boundary on every frame: only the 2 s limit holds the reopens back.
        let mut reopen_times = Vec::new();
        for i in 0..1000u64 {
            let t = 16 * (i + 1);
            let est = if (t / 500) % 2 == 0 {
                10 * MBIT
            } else {
                3 * MBIT
            };
            if let RateStep::Reopen { .. } = p.step(ms(t0, t), est, true) {
                reopen_times.push(t);
            }
        }
        assert!(reopen_times.len() >= 4, "premise: {reopen_times:?}");
        for w in reopen_times.windows(2) {
            assert!(
                w[1] - w[0] >= 2_000,
                "reopens {} ms apart: {reopen_times:?}",
                w[1] - w[0]
            );
        }
    }

    #[test]
    fn a_drop_held_back_by_the_limit_is_made_when_the_limit_passes() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        p.step(t0, 20 * MBIT, true);
        // First reopen: a fall to 10 Mbit/s.
        let first = run(&mut p, t0, 16, 1_500, 10 * MBIT, |_| false);
        assert_eq!(first.len(), 1, "{first:?}");
        let t1 = first[0].0;
        // Then a fall to 2 Mbit/s right after: urgent, but inside the 2 s.
        // The smoothed estimate is still falling when the first candidate is
        // adopted, so a second reopen may follow; what must hold is that
        // none comes early, and that the first is made the moment the limit
        // allows, not later (it was pending, not forgotten).
        let later = run(&mut p, t0, 1_532, 6_000, 2 * MBIT, |_| false);
        assert!(!later.is_empty(), "premise: the drop is followed");
        assert!(
            later[0].0 >= t1 + 2_000 && later[0].0 <= t1 + 2_000 + 16,
            "reopened at {} after {t1}",
            later[0].0
        );
        assert!(matches!(later[0].1, RateStep::Reopen { urgent: true, .. }));
        let all: Vec<u64> = std::iter::once(t1)
            .chain(later.iter().map(|x| x.0))
            .collect();
        for w in all.windows(2) {
            assert!(w[1] - w[0] >= 2_000, "{all:?}");
        }
        // And it settles: the last reopen is at 0.8 x the 2 Mbit/s estimate.
        let RateStep::Reopen { bps, .. } = later.last().unwrap().1 else {
            unreachable!()
        };
        assert!(bps.abs_diff(1_600_000) < 200_000, "{bps}");
    }

    #[test]
    fn candidates_are_not_looked_at_faster_than_every_250_ms() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        p.step(t0, 10 * MBIT, false);
        let evals = p.last_eval;
        // 100 steps inside 240 ms of the first.
        for i in 1..=15u64 {
            p.step(ms(t0, i * 16), MBIT, false);
        }
        assert_eq!(p.last_eval, evals, "no evaluation before 250 ms");
        p.step(ms(t0, 260), MBIT, false);
        assert_ne!(p.last_eval, evals);
    }

    #[test]
    fn a_small_drift_is_adopted_after_a_second_and_accumulates_into_a_reopen() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        p.step(t0, 10 * MBIT, true);
        // 3 % per second down: no single move reaches 10 %, but the candidate
        // refreshes every second, so the running rate is eventually 10 % off.
        let mut reopened = None;
        for i in 1..=1500u64 {
            let t = 16 * i;
            let est = (10e6 * 0.97f64.powf(t as f64 / 1000.0)) as u64;
            if let RateStep::Reopen { urgent, .. } = p.step(ms(t0, t), est, true) {
                assert!(!urgent);
                reopened = Some(t);
                break;
            }
        }
        // 0.97^s < 0.9 needs s > 3.46 s, then the next 1 s refresh.
        let t = reopened.expect("the drift must be followed");
        assert!((3_400..=5_000).contains(&t), "{t}");
    }

    #[test]
    fn a_new_session_restarts_the_estimate_but_keeps_the_rate() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        p.step(t0, 10 * MBIT, false);
        p.reset_session();
        assert_eq!(p.applied(), Some(8_000_000));
        assert!(p.ewma_bps.is_none() && p.candidate.is_none());
    }

    #[test]
    fn the_scripted_cbr_encoder_follows_the_rate_and_reopens_on_request() {
        let mut s = ScriptedCbr::new(InjectedCbr {
            fps: 30,
            idr_over_p: 5,
            gop_frames: None,
        });
        s.apply(Control::H264Rate {
            bps: 2_400_000,
            gop_frames: 4,
            reopen: false,
        });
        // 2.4 Mbit/s at 30 fps = 10 000 bytes per P frame.
        assert_eq!(s.next_frame(false), (true, 50_000), "first frame is an IDR");
        assert_eq!(s.next_frame(false), (false, 10_000));
        assert_eq!(s.next_frame(false), (false, 10_000));
        assert_eq!(s.next_frame(false), (false, 10_000));
        assert_eq!(s.next_frame(false), (true, 50_000), "GOP of 4");
        s.apply(Control::H264Rate {
            bps: 1_200_000,
            gop_frames: 4,
            reopen: true,
        });
        assert_eq!(
            s.next_frame(false),
            (true, 25_000),
            "reopen: IDR at the new rate"
        );
        assert_eq!(s.next_frame(false), (false, 5_000));
        assert_eq!(s.next_frame(true), (true, 25_000), "a requested IDR");
    }
}
