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
/// A raw estimate this many times the smoothed one is a jump, not a drift: a
/// probe that found headroom (goog_cc sets the estimate to the probe's result
/// and pulls it back to `1.5 x` the acknowledged rate within a few hundred
/// milliseconds, faster than a 250 ms EWMA would follow it up).
const SPIKE_RATIO: f64 = 1.5;
/// A jump is held (the target is computed from it, not from the smoothed
/// estimate) for this long, enough for the evaluation interval to see it.
const SPIKE_HOLD: Duration = Duration::from_secs(1);
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
/// A candidate this far over the running rate reopens immediately too. A rise
/// that large is a probe having found headroom, and an estimate nobody uses
/// decays back toward 1.5 x the acknowledged rate within a second or two
/// (goog_cc's additive-increase limit), well inside a GOP: waiting for the
/// scheduled IDR would hand the headroom back before the stream ever used it.
const RISE_FRACTION: f64 = 0.30;
/// A rise needs the raw estimate over the line on this many consecutive
/// evaluations (at least [`EVAL_MIN_INTERVAL`] apart): about half a second.
const RISE_CONFIRMATIONS: u32 = 2;
/// Reopens are at least this far apart.
pub(crate) const MIN_REOPEN_INTERVAL: Duration = Duration::from_secs(2);

/// A padding template is a fragment at least this long (a short tail fragment
/// would make a cluster of many tiny datagrams).
pub(crate) const PAD_MIN_TEMPLATE_BYTES: usize = 600;
/// Padding is this much over the cluster's `min_bytes`: the estimator needs
/// `min_bytes` acknowledged, and a few datagrams may be late or lost.
const PAD_MARGIN: f64 = 1.2;
/// Padding for one probe window never exceeds this, whatever the cluster asks.
const PAD_MAX_BYTES: usize = 384 * 1024;
/// Padding for one window is at most this many seconds of the estimate.
const PAD_MAX_ESTIMATE_SECS: f64 = 0.3;
/// No padding this soon after a rate cut.
const PAD_QUIET_AFTER_CUT: Duration = Duration::from_secs(3);
/// Cadence of the application's own probes (goog_cc's ALR probing interval),
/// and the pause between two probes of a chain.
const APP_PROBE_INTERVAL: Duration = Duration::from_secs(5);
const APP_PROBE_CHAIN_GAP: Duration = Duration::from_millis(400);
/// A probe chain goes on while the estimate reached this much of the last
/// target, and at most this many probes long.
const APP_PROBE_CHAIN_RATIO: f64 = 0.7;
const APP_PROBE_CHAIN_MAX: u32 = 4;
/// A cluster of the application's: lasts `APP_PROBE_DURATION`, at least this
/// many packets (goog_cc's ALR probes ask for 5).
const APP_PROBE_DURATION: Duration = Duration::from_millis(100);
const APP_PROBE_MIN_PROBES: i64 = 5;
/// The application's cluster ids start here; goog_cc counts up from 1 and would
/// need a billion probes to meet them.
const APP_PROBE_ID_BASE: i32 = 1_000_000_000;

/// The application's own probing (see `h264_start_app_probe`).
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct AppProbe {
    next_at: Option<Instant>,
    last_target: Option<u64>,
    chain_len: u32,
    counter: u32,
}

/// How long after padding the estimate ceiling stays off: the 100 ms window,
/// the queue the burst builds on the path (up to about 0.3 s on the scenes'
/// links) and the round trip of the acknowledgements.
const PAD_CEILING_HOLD: Duration = Duration::from_millis(1000);
/// An IDR this close is the burst of the window; no padding.
const PAD_IDR_LOOKAHEAD: Duration = Duration::from_millis(150);

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
    /// Reopen at once for a rise of more than 30 % (see [`RISE_FRACTION`]).
    /// Its IDR is [`IdrCause::RateRise`]; it holds nothing (the link is
    /// healthy, the client already has a picture).
    ReopenRise { bps: u32 },
}

/// The pure rate policy. See the module docs.
#[derive(Debug, Clone, Default)]
pub(crate) struct RatePolicy {
    ewma_bps: Option<f64>,
    last_obs: Option<Instant>,
    /// The latest raw observation (not smoothed, not held).
    last_raw_bps: f64,
    /// Consecutive evaluations in which the raw estimate alone put the target
    /// over the rise line. A rise reopen needs [`RISE_CONFIRMATIONS`].
    rise_streak: u32,
    /// A jump of the raw estimate, and when its hold ends.
    spike: Option<(f64, Instant)>,
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
    pub(crate) fn applied(&self) -> Option<u32> {
        self.applied
    }

    /// A new session: the estimate starts over; the encoder keeps its rate.
    pub(crate) fn reset_session(&mut self) {
        self.ewma_bps = None;
        self.last_obs = None;
        self.rise_streak = 0;
        self.spike = None;
        self.candidate = None;
        self.candidate_at = None;
        self.last_eval = None;
    }

    fn target(&self, now: Instant) -> u32 {
        let mut e = self.ewma_bps.unwrap_or(0.0);
        if let Some((v, until)) = self.spike {
            if now < until {
                e = e.max(v);
            }
        }
        let t = (RATE_SHARE * e).clamp(f64::from(RATE_FLOOR_BPS), f64::from(RATE_CAP_BPS));
        t as u32
    }

    fn observe(&mut self, now: Instant, estimate_bps: u64) {
        let est = estimate_bps as f64;
        match (self.ewma_bps, self.last_obs) {
            (Some(e), Some(t)) => {
                let dt = now.saturating_duration_since(t).as_secs_f64();
                if est > e * SPIKE_RATIO {
                    let until = now + SPIKE_HOLD;
                    self.spike = Some((est.max(self.spike.map_or(0.0, |s| s.0)), until));
                }
                let alpha = 1.0 - (-dt / EWMA_TAU.as_secs_f64()).exp();
                self.ewma_bps = Some(e + alpha * (est - e));
            }
            _ => self.ewma_bps = Some(est),
        }
        self.last_obs = Some(now);
        self.last_raw_bps = est;
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
            let t = self.target(now);
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
            // A rise must be seen in the raw estimate on consecutive
            // evaluations, not only in the held spike: goog_cc pulls a probe's
            // result back within a few hundred milliseconds when nothing uses
            // it, and a stream raised on a blip is cut again a probe later (a
            // RateRise and a RateDrop for one probe chain).
            let raw_target = (RATE_SHARE * self.last_raw_bps)
                .clamp(f64::from(RATE_FLOOR_BPS), f64::from(RATE_CAP_BPS));
            if raw_target > f64::from(applied) * (1.0 + RISE_FRACTION) {
                self.rise_streak += 1;
            } else {
                self.rise_streak = 0;
            }
            let target = self.target(now);
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
        let rose = f64::from(cand) > f64::from(applied) * (1.0 + RISE_FRACTION)
            && self.rise_streak >= RISE_CONFIRMATIONS;
        if dropped || rose || at_gop {
            self.applied = Some(cand);
            self.last_reopen = Some(now);
            if rose {
                return RateStep::ReopenRise { bps: cand };
            }
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
/// frames, and a reopen (or a pending IDR request, the GOP's included) makes
/// the next frame an IDR. It takes exactly the controls the real backend does, so the
/// scenes exercise the real policy.
#[derive(Debug, Clone, Copy)]
pub(super) struct ScriptedCbr {
    fps: u32,
    idr_over_p: u32,
    rate_bps: u32,
    reopen: bool,
    started: bool,
}

impl ScriptedCbr {
    pub(super) fn new(cfg: InjectedCbr) -> Self {
        Self {
            fps: cfg.fps.max(1),
            idr_over_p: cfg.idr_over_p.max(1),
            rate_bps: RATE_INITIAL_BPS,
            reopen: false,
            started: false,
        }
    }

    pub(super) fn apply(&mut self, c: Control) {
        if let Control::H264Rate { bps, fps, reopen } = c {
            self.rate_bps = bps;
            self.fps = fps.max(1);
            self.reopen |= reopen;
        }
    }

    /// The next frame: `(is_idr, payload bytes)`. `requested` is an IDR the
    /// bridge asked for. As the real encoder it makes one on no initiative of
    /// its own but its first frame and a fresh open.
    pub(super) fn next_frame(&mut self, requested: bool) -> (bool, usize) {
        let is_idr = requested || self.reopen || !self.started;
        self.reopen = false;
        self.started = true;
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

    /// The probe machinery of a stream that is no longer in charge (it left
    /// H.264 mode, or the session ended): forget the app-probe cadence and the
    /// padding in flight.
    pub(super) fn h264_reset_probing(&mut self) {
        self.h264.app_probe = AppProbe::default();
        self.reliable_emitter.pads.clear();
    }

    /// Whether padding may be queued now (design: bounded, never in the way).
    /// Not while the H.264 queue holds anything but the frame just queued
    /// (`own_bytes`), and not within [`PAD_QUIET_AFTER_CUT`] of a rate cut: the
    /// path is shrinking, probing it is the wrong moment, and it is also when
    /// the extra queue would cost frames.
    fn h264_pad_allowed(&self, now: Instant, own_bytes: usize) -> bool {
        if self
            .h264
            .last_cut_at
            .is_some_and(|t| now.saturating_duration_since(t) < PAD_QUIET_AFTER_CUT)
        {
            return false;
        }
        self.reliable_emitter
            .queued_h264_bytes()
            .saturating_sub(own_bytes)
            == 0
    }

    /// Start a probe cluster of ghostframe's own, if one is due.
    ///
    /// goog_cc's periodic ALR probing needs its ALR detector to call the
    /// sender application-limited, and a CBR stream at 0.8 x its estimate is
    /// above that detector's usage line, so it never asks. The stream *is*
    /// application-limited in the sense that matters (it chose not to send
    /// more), so the application probes: a cluster at twice the estimate every
    /// [`APP_PROBE_INTERVAL`], and, while a probe keeps succeeding (the
    /// estimate reached 0.7 x its target), the next one straight after, as
    /// goog_cc chains its own. The cluster has an id from a range goog_cc never
    /// issues ([`APP_PROBE_ID_BASE`]); goog_cc's probe estimator measures any
    /// cluster id it finds on acknowledged packets (`probe_bitrate_estimator.rs`
    /// `handle_probe_and_estimate_bitrate`, keyed by the packet's id alone),
    /// and applies the result like one it asked for.
    fn h264_start_app_probe(&mut self, now: Instant) {
        let est = self.bwe.snapshot().bitrate_bps;
        let p = &mut self.h264.app_probe;
        let due = match p.next_at {
            None => {
                // First look: the start-up ladder is goog_cc's.
                p.next_at = Some(now + APP_PROBE_INTERVAL);
                false
            }
            Some(t) => now >= t,
        };
        if !due {
            return;
        }
        // A chain goes on while the last probe's target was (nearly) reached.
        let chaining = p
            .last_target
            .is_some_and(|t| est as f64 >= APP_PROBE_CHAIN_RATIO * t as f64)
            && p.chain_len < APP_PROBE_CHAIN_MAX;
        if p.last_target.is_some() && !chaining {
            // The chain ended: the next probe is an interval from now.
            p.last_target = None;
            p.chain_len = 0;
            p.next_at = Some(now + APP_PROBE_INTERVAL);
            return;
        }
        let target = est.saturating_mul(2);
        let duration = APP_PROBE_DURATION;
        let min_bytes = (target as f64 / 8.0 * duration.as_secs_f64()) as i64;
        let id = APP_PROBE_ID_BASE + (p.counter % 1_000_000) as i32;
        p.counter += 1;
        p.last_target = Some(target);
        p.chain_len += 1;
        p.next_at = Some(now + APP_PROBE_CHAIN_GAP);
        self.h264.stats.app_probes += 1;
        self.open_probe_cluster(now, id, target, duration, APP_PROBE_MIN_PROBES, min_bytes);
    }

    /// Fill a probe window for a frame that was just sent, without forcing
    /// an IDR (design 4.3). `own_bytes` is the wire size of that frame.
    ///
    /// A window is claimed by a burst. If an IDR is due within the window it
    /// is the burst and nothing is done here. Otherwise a window is opened now
    /// (goog_cc's own request first, else the application's, see
    /// [`Self::h264_start_app_probe`]) and filled with copies of an already
    /// acknowledged P-frame fragment under new `wire_seq`s, flagged as padding:
    /// the client acknowledges them on receipt and discards them, so they are
    /// measured and cost the picture nothing. They are billed as probe, never
    /// counted as frame data, never retransmitted, bounded by the cluster
    /// (`PAD_MARGIN` over `min_bytes`, `PAD_MAX_BYTES` overall) and sent at
    /// the probe rate (the admission rate is the window's while it is open).
    pub(super) fn h264_probe_pad(&mut self, now: Instant, is_idr: bool, own_bytes: usize) {
        if self.h264.rate.applied().is_none() {
            return;
        }
        let Some(template) = self.h264.pad_template.clone() else {
            return;
        };
        if self.active_probe.is_none() && !is_idr {
            if !self.h264_pad_allowed(now, own_bytes) {
                return;
            }
            let gop = self.h264.gop;
            let idr_soon = self.h264.pending_idr.is_some()
                || self
                    .h264
                    .last_idr_at
                    .is_some_and(|t| now.saturating_duration_since(t) + PAD_IDR_LOOKAHEAD >= gop);
            if idr_soon {
                return;
            }
            self.open_probe_window_for_burst(now);
            if self.active_probe.is_none() {
                self.h264_start_app_probe(now);
            }
        }
        // An open window, however it was claimed (an IDR, or just above), is
        // topped up to the cluster's size: an IDR smaller than the cluster
        // would otherwise end the window under-filled and waste the probe.
        let Some(p) = self.active_probe.as_mut().filter(|p| now < p.ends_at) else {
            return;
        };
        let len = template.len() as i64;
        let want_bytes = (p.min_bytes as f64 * PAD_MARGIN) as i64;
        if p.bytes_sent >= want_bytes && p.packets_sent >= p.min_probes {
            return;
        }
        let by_bytes = (want_bytes - p.bytes_sent + len - 1) / len;
        let by_count = p.min_probes - p.packets_sent;
        // Never more padding than a fraction of a second of the estimate: a
        // probe cluster is `rate x 100 ms`, and a start-up ladder rung at 6 or
        // 12 Mbit/s would otherwise queue seconds of padding on a thin path
        // (a 1 Mbit/s link was measured skipping a third more frames). A
        // cluster too big for that is left under-filled and abandoned, as a
        // burst too small for it always was.
        let est_bytes = (self.bwe.snapshot().bitrate_bps / 8) as f64;
        let by_estimate = (est_bytes * PAD_MAX_ESTIMATE_SECS) as usize;
        let cap = (PAD_MAX_BYTES.min(by_estimate) as i64 / len).max(1);
        let n = by_bytes.max(by_count).clamp(1, cap);
        let mut bytes = 0u64;
        for _ in 0..n {
            let tag = crate::transport::reliable_emitter::cache::ProbeTag {
                id: p.id,
                min_probes: p.min_probes,
                min_bytes: p.min_bytes,
                bytes_sent_before: p.bytes_sent,
            };
            p.bytes_sent = p.bytes_sent.saturating_add(len);
            p.packets_sent = p.packets_sent.saturating_add(1);
            bytes += self.reliable_emitter.submit_h264_pad(&template, tag, now) as u64;
        }
        // The burst spends the whole grant before any of it is acknowledged:
        // keep the estimate ceiling from reading that as a saturated, silent
        // path for the window plus what the queue it builds and a round trip
        // can add.
        self.bwe.hold_ceiling(now, PAD_CEILING_HOLD);
        self.h264.stats.probe_padding_windows += 1;
        self.h264.stats.probe_padding_datagrams += n as u64;
        self.h264.stats.probe_padding_bytes += bytes;
    }

    /// Run the rate policy for a frame the gate has let through. `gop` is the
    /// time between IDRs, `fps` the frame rate the encoder is opened for.
    ///
    /// The GOP is the bridge's: the frame is the scheduled IDR when `gop` has
    /// passed since the last IDR of any cause (a loss or rate IDR restarts the
    /// clock). A rate change that wants an IDR takes that one; without one the
    /// bridge asks the encoder for it, so the encoder needs no GOP of its own.
    pub(super) fn h264_rate_step(&mut self, now: Instant, gop: Duration, fps: u32) {
        let estimate = self.bwe.snapshot().bitrate_bps;
        let at_gop = self
            .h264
            .last_idr_at
            .is_some_and(|t| now.saturating_duration_since(t) >= gop);
        let step = self.h264.rate.step(now, estimate, at_gop);
        let reopens = matches!(step, RateStep::Reopen { .. } | RateStep::ReopenRise { .. });
        self.h264_apply_rate_step(step, estimate, fps);
        if at_gop && !reopens && self.h264.pending_idr.is_none() {
            self.request_h264_idr(IdrCause::GopPeriodic, now);
        }
    }

    /// Act on what the policy decided. Split from [`Self::h264_rate_step`] so a
    /// test can hand it a decision without steering the estimator.
    pub(super) fn h264_apply_rate_step(&mut self, step: RateStep, estimate: u64, fps: u32) {
        match step {
            RateStep::Keep => {}
            RateStep::Initial(bps) => {
                self.h264.stats.rate_target_bps = bps;
                self.h264_control(Control::H264Rate {
                    bps,
                    fps,
                    reopen: false,
                });
            }
            RateStep::ReopenRise { bps } => {
                self.h264.stats.rate_target_bps = bps;
                self.expect_h264_idr(IdrCause::RateRise);
                self.h264.stats.rate_reopens_rise += 1;
                tracing::info!(
                    target: LOG_TARGET,
                    bps,
                    estimate_bps = estimate,
                    "H.264 rate: reopening the encoder for a rise"
                );
                self.h264_control(Control::H264Rate {
                    bps,
                    fps,
                    reopen: true,
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
                    self.h264.last_cut_at = Some(now_std());
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
                    fps,
                    reopen: true,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::io_bridge::now_std;

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
        // +20 %: over the 10 % band, under the 30 % that reopens at once.
        let steps = run(&mut p, t0, 16, 6_000, 6 * MBIT, |_| false);
        assert!(steps.is_empty(), "a rise reopens only at a GOP: {steps:?}");
        let steps = run(&mut p, t0, 6_016, 100, 6 * MBIT, |_| true);
        assert_eq!(steps.len(), 1, "{steps:?}");
        assert_eq!(steps[0].0, 6_016, "at the first GOP frame");
        let RateStep::Reopen { bps, urgent } = steps[0].1 else {
            panic!("{steps:?}");
        };
        assert!(!urgent);
        assert!(bps.abs_diff(4_800_000) < 1_000, "{bps}");
    }

    #[test]
    fn a_rise_over_30_percent_reopens_at_once_without_waiting_for_the_gop() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        p.step(t0, 5 * MBIT, false);
        let steps = run(&mut p, t0, 16, 3_000, 10 * MBIT, |_| false);
        assert_eq!(steps.len(), 1, "{steps:?}");
        let RateStep::ReopenRise { bps } = steps[0].1 else {
            panic!("{steps:?}");
        };
        assert!(bps.abs_diff(8_000_000) < 1_000, "{bps}");
        assert!(steps[0].0 <= 1_200, "reopened at {} ms", steps[0].0);
    }

    #[test]
    fn a_jump_that_lasts_is_followed_a_blip_is_not_and_a_decay_gets_no_hold() {
        let t0 = Instant::now();
        let follow = |len_ms: u64| {
            // A probe lifts the estimate 4x for `len_ms` and goog_cc pulls it back.
            let mut p = RatePolicy::default();
            p.step(t0, 2 * MBIT, false);
            let mut reopened = false;
            for i in 1..=200u64 {
                let t = 16 * i;
                let est = if (400..400 + len_ms).contains(&t) {
                    8 * MBIT
                } else {
                    2 * MBIT
                };
                if matches!(p.step(ms(t0, t), est, false), RateStep::ReopenRise { .. }) {
                    reopened = true;
                }
            }
            reopened
        };
        assert!(follow(900), "a jump that lasts must be followed");
        assert!(!follow(200), "a blip shorter than two evaluations is not");
        // A decay is not a jump: it gets no hold, so the fall is acted on as
        // fast as the smoothing allows (the 30 % test above bounds it).
        let mut p = RatePolicy::default();
        p.step(t0, 10 * MBIT, false);
        p.step(ms(t0, 16), 5 * MBIT, false);
        assert!(p.spike.is_none(), "a fall is no spike");
    }

    #[test]
    fn at_most_one_reopen_per_two_seconds() {
        let t0 = Instant::now();
        let mut p = RatePolicy::default();
        p.step(t0, 100 * MBIT / 10, true);
        // The estimate alternates between 10 and 3 Mbit/s every 1.5 s, a GOP
        // boundary on every frame: only the 2 s limit holds the reopens back.
        let mut reopen_times = Vec::new();
        for i in 0..1000u64 {
            let t = 16 * (i + 1);
            let est = if (t / 1_500) % 2 == 0 {
                10 * MBIT
            } else {
                3 * MBIT
            };
            if let RateStep::Reopen { .. } | RateStep::ReopenRise { .. } =
                p.step(ms(t0, t), est, true)
            {
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
            gop: None,
        });
        s.apply(Control::H264Rate {
            bps: 2_400_000,
            fps: 30,
            reopen: false,
        });
        // 2.4 Mbit/s at 30 fps = 10 000 bytes per P frame.
        assert_eq!(s.next_frame(false), (true, 50_000), "first frame is an IDR");
        // And no initiative of its own after it: the GOP is the bridge's.
        for _ in 0..500 {
            assert_eq!(s.next_frame(false), (false, 10_000));
        }
        s.apply(Control::H264Rate {
            bps: 1_200_000,
            fps: 30,
            reopen: true,
        });
        assert_eq!(
            s.next_frame(false),
            (true, 25_000),
            "reopen: IDR at the new rate"
        );
        assert_eq!(s.next_frame(false), (false, 5_000));
        assert_eq!(s.next_frame(true), (true, 25_000), "a requested IDR");
        // A frame rate change rescales the budget.
        s.apply(Control::H264Rate {
            bps: 1_200_000,
            fps: 60,
            reopen: false,
        });
        assert_eq!(s.next_frame(false), (false, 2_500));
    }

    // ---- the bridge: application probes and padding -------------------------

    async fn bridge() -> IoBridge {
        let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
        let server = crate::transport::quic::QuicServer::new().expect("QuicServer::new");
        IoBridge::new_with_stream_for_test(ours, server)
    }

    /// A bridge with the rate policy running and a padding template.
    async fn padding_bridge() -> IoBridge {
        let mut b = bridge().await;
        b.h264_apply_rate_step(RateStep::Initial(2_000_000), 2_000_000, 30);
        b.h264.rate.step(now_std(), 2 * MBIT, false);
        b.h264.pad_template = Some(bytes::Bytes::from(vec![0u8; 1200]));
        b
    }

    #[tokio::test(start_paused = true)]
    async fn the_application_probes_at_twice_the_estimate_every_five_seconds() {
        let mut b = padding_bridge().await;
        let est = b.bwe.snapshot().bitrate_bps;
        // The first look arms the cadence; the start-up ladder is goog_cc's.
        b.h264_probe_pad(now_std(), false, 0);
        assert!(b.active_probe.is_none() && b.h264.stats.app_probes == 0);
        tokio::time::advance(APP_PROBE_INTERVAL - Duration::from_millis(1)).await;
        b.h264_probe_pad(now_std(), false, 0);
        assert!(b.active_probe.is_none(), "not yet");
        tokio::time::advance(Duration::from_millis(2)).await;
        b.h264_probe_pad(now_std(), false, 0);
        let p = b.active_probe.expect("the cluster is open");
        assert!(
            p.id >= 1_000_000_000,
            "an id goog_cc never issues: {}",
            p.id
        );
        assert_eq!(p.target_rate_bps, est * 2);
        assert_eq!(p.min_probes, APP_PROBE_MIN_PROBES);
        assert_eq!(
            p.min_bytes,
            (p.target_rate_bps as f64 / 8.0 * APP_PROBE_DURATION.as_secs_f64()) as i64
        );
        assert_eq!(b.h264.stats.app_probes, 1);
        // Filled with padding, tagged for this cluster, never past the cluster
        // asked for (1.2 x min_bytes plus one datagram).
        assert!(b.h264.stats.probe_padding_datagrams as i64 >= p.min_probes);
        assert!(
            p.bytes_sent >= p.min_bytes,
            "{} of {}",
            p.bytes_sent,
            p.min_bytes
        );
        assert!(
            p.bytes_sent <= (p.min_bytes as f64 * PAD_MARGIN) as i64 + 1200,
            "bounded by the cluster: {} of {}",
            p.bytes_sent,
            p.min_bytes
        );
        assert_eq!(
            b.reliable_emitter.pads.len() as u64,
            b.h264.stats.probe_padding_datagrams
        );
        // Never counted as frame data.
        assert_eq!(b.h264.stats.frames_sent, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_probe_chain_goes_on_while_the_estimate_follows_and_ends_when_it_does_not() {
        let mut b = padding_bridge().await;
        let est = b.bwe.snapshot().bitrate_bps;
        b.h264_probe_pad(now_std(), false, 0); // arms
        tokio::time::advance(APP_PROBE_INTERVAL).await;
        b.h264_probe_pad(now_std(), false, 0);
        assert_eq!(b.h264.stats.app_probes, 1);
        // The window closes; the estimate did not move (it is under 0.7 x the
        // 2 x target): the chain ends, the next probe is an interval away.
        b.active_probe = None;
        b.reliable_emitter.purge_queued_h264(|_| true); // the pads went out
        tokio::time::advance(APP_PROBE_CHAIN_GAP).await;
        b.h264_probe_pad(now_std(), false, 0);
        assert_eq!(b.h264.stats.app_probes, 1, "the chain ended");
        tokio::time::advance(APP_PROBE_INTERVAL).await;
        b.h264_probe_pad(now_std(), false, 0);
        assert_eq!(
            b.h264.stats.app_probes, 2,
            "an interval after the chain ended"
        );
        // And a chain that succeeded goes on straight away: pretend the
        // estimate reached the last target by lowering the bar to it.
        let last = b.h264.app_probe.last_target.expect("a target");
        assert!(last >= est * 2);
        b.active_probe = None;
        b.h264.app_probe.last_target = Some(est); // the estimate "reached" it
        b.reliable_emitter.purge_queued_h264(|_| true);
        tokio::time::advance(APP_PROBE_CHAIN_GAP).await;
        b.h264_probe_pad(now_std(), false, 0);
        assert_eq!(b.h264.stats.app_probes, 3, "the chain went on");
    }

    #[tokio::test(start_paused = true)]
    async fn padding_keeps_quiet_behind_a_backlog_and_after_a_rate_cut() {
        let mut b = padding_bridge().await;
        b.h264_probe_pad(now_std(), false, 0); // arms
        tokio::time::advance(APP_PROBE_INTERVAL).await;
        let t = now_std();
        // A backlog: an H.264 fragment of another frame still queued.
        let dgs = crate::transport::protocol::fragment_frame(9, 0, false, &[1u8; 800], 1200);
        b.reliable_emitter.submit_h264_fragment(
            crate::transport::reliable_emitter::h264::H264Submit {
                frame_seq: 9,
                frag_idx: 0,
                is_idr: false,
                bytes: bytes::Bytes::from(dgs[0].clone()),
                queued_at: t,
                probe: None,
            },
            t,
        );
        let own = 0; // this frame's own bytes: none, so the queue is a backlog
        b.h264_probe_pad(t, false, own);
        assert!(b.active_probe.is_none(), "behind a backlog");
        // The queue holds only the frame just sent: allowed.
        let queued = b.reliable_emitter.queued_h264_bytes();
        // ... but not right after a rate cut.
        b.h264.last_cut_at = Some(t);
        b.h264_probe_pad(t, false, queued);
        assert!(b.active_probe.is_none(), "within 3 s of a cut");
        tokio::time::advance(PAD_QUIET_AFTER_CUT).await;
        b.h264_probe_pad(now_std(), false, queued);
        assert!(b.active_probe.is_some(), "quiet again");
        assert_eq!(b.h264.stats.app_probes, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_idr_due_within_the_window_is_the_burst_and_is_topped_up_not_padded_around() {
        let mut b = padding_bridge().await;
        b.h264_probe_pad(now_std(), false, 0); // arms
        tokio::time::advance(APP_PROBE_INTERVAL).await;
        // The scheduled IDR is 100 ms away (the bridge's clock, not frames).
        b.h264.gop = Duration::from_secs(3);
        b.h264.last_idr_at = Some(now_std() - Duration::from_millis(2_900));
        b.h264_probe_pad(now_std(), false, 0);
        assert!(
            b.active_probe.is_none(),
            "the IDR is the burst: no window opened on a P frame"
        );
        // An IDR that claimed a window smaller than the cluster is topped up.
        let t = now_std();
        b.open_probe_cluster(t, 77, 4_000_000, APP_PROBE_DURATION, 5, 50_000);
        b.h264_probe_pad(t, true, 3_000);
        let p = b.active_probe.expect("still open");
        assert!(
            p.bytes_sent >= p.min_bytes,
            "topped up: {} of {}",
            p.bytes_sent,
            p.min_bytes
        );
    }

    #[tokio::test(start_paused = true)]
    async fn leaving_h264_mode_or_the_session_forgets_the_probing_and_the_padding() {
        let mut b = padding_bridge().await;
        b.h264_probe_pad(now_std(), false, 0);
        tokio::time::advance(APP_PROBE_INTERVAL).await;
        b.h264_probe_pad(now_std(), false, 0);
        assert!(!b.reliable_emitter.pads.is_empty());
        b.h264_reset_probing();
        assert!(b.reliable_emitter.pads.is_empty());
        assert!(
            b.h264.app_probe.next_at.is_none(),
            "the cadence starts over"
        );
    }

    /// A pad's acknowledgement proves the path delivers, not that frames do:
    /// it must not count as H.264 ack progress for the stall flag's
    /// no-progress rule (`last_ack_progress`), nor mark the session as one that
    /// acknowledges H.264 (`acked_any`). A real fragment's does.
    #[tokio::test(start_paused = true)]
    async fn a_pad_acknowledgement_is_no_h264_ack_progress() {
        let mut b = padding_bridge().await;
        let now = now_std();
        let pad_tx = crate::transport::transmission_ledger::Transmission {
            emit_us: 0,
            wire_bytes: 1200,
            content: crate::transport::reliable_emitter::Content::Pad(5),
            depart_us: None,
            dep_idx: None,
        };
        b.apply_h264_acks(&[(pad_tx.content, pad_tx, 0, false)], now);
        assert!(b.h264.last_ack_progress.is_none(), "a pad is no progress");
        assert!(!b.h264.acked_any);
        let frag_tx = crate::transport::transmission_ledger::Transmission {
            content: crate::transport::reliable_emitter::Content::H264Frag {
                frame_seq: 1,
                frag_idx: 0,
            },
            ..pad_tx
        };
        b.apply_h264_acks(&[(frag_tx.content, frag_tx, 0, false)], now);
        assert_eq!(b.h264.last_ack_progress, Some(now), "a fragment is");
        assert!(b.h264.acked_any);
    }

    /// Leaving H.264 mode lets go of the rate side in one place
    /// (`abandon_h264_inflight`): padding, the app-probe cadence, the rate
    /// policy's estimate and the GOP clock. The encoder keeps its rate.
    #[tokio::test(start_paused = true)]
    async fn leaving_h264_mode_resets_padding_probing_policy_and_gop_clock_in_one_place() {
        let mut b = padding_bridge().await;
        b.h264_probe_pad(now_std(), false, 0); // arms
        tokio::time::advance(APP_PROBE_INTERVAL).await;
        b.h264_probe_pad(now_std(), false, 0);
        assert!(
            !b.reliable_emitter.pads.is_empty(),
            "premise: padding in flight"
        );
        b.h264.last_idr_at = Some(now_std());
        let applied = b.h264.rate.applied();
        assert!(applied.is_some());
        b.abandon_h264_inflight(7, now_std());
        assert!(b.reliable_emitter.pads.is_empty());
        assert!(b.h264.app_probe.next_at.is_none() && b.h264.app_probe.last_target.is_none());
        assert!(b.h264.last_idr_at.is_none());
        assert_eq!(b.h264.rate.applied(), applied, "the encoder keeps its rate");
        assert!(b.h264.rate.ewma_bps.is_none(), "the estimate starts over");
    }
}
