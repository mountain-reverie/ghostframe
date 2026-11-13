// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-tile classifier and bandwidth-fit frame-mode decision.
//!
//! See `docs/superpowers/specs/2026-04-28-m3-codec-suite-design.md` Phase M3.0
//! and `docs/specs/ghostframe-initial-spec.md` §4.2 / §4.3.

use super::first_pass::{DemandWindow, FirstPassKind, FirstPassModel};
use super::{CodecState, FrameMode, TileMetrics};

/// Live signal vector consumed by `Classifier::decide_frame_mode`.
///
/// Populated by `IoBridge` from `ReceiverFeedback` + QUIC `path_stats` +
/// `ClientCapabilities` (the last from HELLO). Network fields default to
/// "no information" (zeros / `false`); `supports_h264` defaults to `true`
/// because (a) most tests construct `AdaptationContext` directly and don't
/// care about the H.264 capability and (b) the conservative default lives
/// on the `ClientCapabilities` side — `IoBridge` only flips this field to
/// `true` after seeing a HELLO with bit 1 set. Test ergonomics + real
/// behaviour stay aligned.
#[derive(Debug, Clone, Copy)]
pub struct AdaptationContext {
    /// Smoothed throughput estimate, bytes per µs. Sourced from
    /// `min(cwnd/rtt, recent_emit_bytes/window)`. The classifier no longer
    /// reads it (the bandwidth rule uses `bwe_estimate_bps`); it is the
    /// bridge's probe-inclusive model rate, kept for the bridge's own tests
    /// and logs.
    pub bytes_per_us: f32,
    /// Smoothed one-way RTT, µs. Sourced from QUIC `path_stats.rtt`.
    pub smoothed_rtt_us: f32,
    /// Datagram loss rate measured at the sender, from the transmission
    /// ledger.
    pub loss_rate: f32,
    /// Last `ReceiverFeedback`'s suspension flag, OR'd across the last 2
    /// windows to debounce single-window flaps.
    pub suspended: bool,
    /// Monotonic counter incremented on each `set_adaptation_context` call.
    /// Tests assert "classifier saw the new value before decision N".
    pub last_update_seq: u32,
    /// Mirrors `ClientCapabilities::supports_h264`. When `false`, the
    /// classifier never selects `FrameMode::H264` regardless of network
    /// conditions: a Firefox client (no `texture_external` WGSL capability)
    /// silently drops H.264 frames, so falling back to H.264 under load
    /// would produce a frozen stream instead of degraded-but-viewable
    /// tile-codec output.
    pub supports_h264: bool,
    /// The bandwidth *estimate* (goog_cc's `bitrate_bps`), bits per second;
    /// `0` means "no estimate yet". Unlike `bytes_per_us` it is not
    /// probe-inclusive and not a token rate: it is what the headroom guard
    /// reads, so a probe window opening cannot hide a starved link.
    pub bwe_estimate_bps: u64,
    /// The bridge has seen feedback about this session's path: the estimate
    /// is off goog_cc's initial value and moved by at least one sample since
    /// the session reset. Until then the bandwidth rule holds the mode
    /// (`estimate_unknown`). `false` in `Default`.
    pub estimate_known: bool,
    /// The bridge's H.264 transport has stalled (frames in flight beyond the
    /// bound, no frame acknowledged for too long, or the client's reset
    /// requests over their rate limit). The only H264 -> tile path from a
    /// network stall: it beats every other override, and H.264 is not
    /// re-entered until a holdoff (starting at [`H264_STALL_REENTRY_US`], doubling per repeated stall) after it clears.
    pub h264_stalled: bool,
    /// No H.264 encoder can serve the stream: the bridge latched it (openh264
    /// gave up, no library loads) or found the stream outside every
    /// available backend's limits. Unlike `h264_stalled` there is no way back
    /// until the bridge clears it (a process-wide latch is never cleared; a
    /// per-configuration one is, when the picture size changes): the
    /// classifier stays on the lossless tile codec.
    pub h264_encoder_unavailable: bool,
}

impl Default for AdaptationContext {
    fn default() -> Self {
        Self {
            bytes_per_us: 0.0,
            smoothed_rtt_us: 0.0,
            loss_rate: 0.0,
            suspended: false,
            last_update_seq: 0,
            supports_h264: true,
            bwe_estimate_bps: 0,
            estimate_known: false,
            h264_stalled: false,
            h264_encoder_unavailable: false,
        }
    }
}

/// Pure classifier: maps current per-tile metrics + previous codec state to a
/// next codec state. Rules from source spec §4.2 evaluated in order.
///
/// Sentinel handling: `unique_colors == UNIQUE_COLORS_UNKNOWN` causes rules
/// 3, 5, 6, and 7 (the colour-count consumers) to skip. `edge_density` is not
/// read by any M3.0 rule — its NaN sentinel exists for forward compatibility
/// with M3.3 rules.
///
/// Hysteresis: when previous state is `H264`, the medium-frequency band
/// (5-15 Hz) holds the tile in `H264 { frames_in_h264: n+1 }` rather than
/// dropping to the Cdf53 fallback (Rule 4). Outside the medium-frequency band
/// the rules apply normally — a tile that drops to low-freq (< 5 Hz) is
/// re-classified fresh per Rules 6-8. (Per-tile hysteresis differs from the
/// frame-mode hysteresis in `Classifier::decide_frame_mode`.)
///
/// **Palette-id placeholder convention.** When this function returns
/// `CodecState::PalRle { palette_id: 0 }`, the `0` is a feasibility
/// placeholder, not a real persistent slot id. `IoBridge::phase_a_palette_allocation`
/// overwrites the field with the real id (or downgrades to `Skip` on
/// allocation failure). See design Section 6.
pub fn classify_tile(metrics: &TileMetrics, prev: &CodecState) -> CodecState {
    // PixelPerfect early-out: the tile has been refined to lossless and is
    // still idle. Skip emission. If the tile becomes dirty, the rules below
    // override (PixelPerfect status was meaningful only while idle).
    if matches!(prev, CodecState::PixelPerfect) && metrics.idle_frames > 0 {
        return CodecState::Skip;
    }

    // Rule 1: idle ⇒ Skip
    if metrics.idle_frames > 0 {
        return CodecState::Skip;
    }

    let freq = metrics.change_freq_hz;
    let mag = metrics.change_magnitude;
    let uc_known = metrics.unique_colors != super::UNIQUE_COLORS_UNKNOWN;

    // Rule 2: high freq AND high magnitude ⇒ H264
    if freq > 15.0 && mag > 0.3 {
        // Not already in H264 => this is the first frame of a new H264 run.
        // The variants are listed rather than caught by `_` because this
        // file is where a catch-all once hid a wrong classifier for three
        // months: a new CodecState silently restarting the counter is
        // exactly the class of defect that cost.
        let frames = match prev {
            CodecState::H264 { frames_in_h264 } => frames_in_h264.saturating_add(1),
            CodecState::PalRle { .. }
            | CodecState::Solid
            | CodecState::Cdf53 { .. }
            | CodecState::PixelPerfect
            | CodecState::Skip => 1,
        };
        return CodecState::H264 {
            frames_in_h264: frames,
        };
    }

    // Rule 3: high freq AND low magnitude ⇒ PalRle if few colors known, else Cdf53
    if freq > 15.0 && mag <= 0.3 {
        // Single-colour tiles (e.g. an X11 wipe-to-black) must take the
        // Solid path even under high-freq churn — otherwise we waste a
        // persistent PaletteTable slot on a count=1 palette that the
        // server then keeps matching against (via find_matching) for
        // every subsequent same-content capture, pinning affected tiles
        // to that all-black palette via thin/indices_raw emissions until
        // a new bundled palette evicts it. Mirrors the Rule 4 / Rule 6
        // single-colour guard.
        if uc_known && metrics.unique_colors <= 1 {
            return CodecState::Solid;
        }
        if uc_known && metrics.unique_colors <= 16 {
            return CodecState::PalRle { palette_id: 0 };
        }
        return CodecState::Cdf53 {
            passes_sent: 0,
            present_passes: 0x3FFF,
        };
    }

    // Rule 4: medium freq AND currently H264 ⇒ stay H264 (per-tile hysteresis).
    // Rule 5 (medium freq AND not H264) prefers a lossless codec when color
    // info is known (mirrors Rule 3's structure for high-freq + low-mag),
    // and falls back to Cdf53 refinement otherwise.
    if (5.0..=15.0).contains(&freq) {
        if let CodecState::H264 { frames_in_h264 } = prev {
            return CodecState::H264 {
                frames_in_h264: frames_in_h264.saturating_add(1),
            };
        }
        if uc_known && metrics.unique_colors <= 1 {
            return CodecState::Solid;
        }
        if uc_known && metrics.unique_colors <= 16 {
            return CodecState::PalRle { palette_id: 0 };
        }
        return CodecState::Cdf53 {
            passes_sent: 0,
            present_passes: 0x3FFF,
        }; // Rule 5 fallback
    }

    // Rule 6: single color ⇒ Solid
    if uc_known && metrics.unique_colors <= 1 {
        return CodecState::Solid;
    }

    // Rule 7: ≤16 colors ⇒ PalRle
    if uc_known && metrics.unique_colors <= 16 {
        return CodecState::PalRle { palette_id: 0 };
    }

    // Rule 8: fallback ⇒ Cdf53 (lossy → refinement). M3.0 emission is Raw.
    CodecState::Cdf53 {
        passes_sent: 0,
        present_passes: 0x3FFF,
    }
}

#[cfg(test)]
#[path = "classifier_classify_tests.rs"]
mod classify_tests;

/// Enter H.264 when the first-pass demand stays above this share of the
/// estimate. Below the 0.8 the H.264 rate targets, so H.264 has headroom.
pub(crate) const ENTER_SHARE: f64 = 0.70;
/// Leave H.264 when the demand stays below this share of the estimate.
pub(crate) const EXIT_SHARE: f64 = 0.40;
/// Leaving H.264 repaints the whole screen; leave only if that repaint's
/// first pass drains within this long at the estimate.
pub(crate) const REPAINT_DRAIN_MAX_US: u64 = 1_000_000;
/// After a repaint is requested, a frame covering most of the screen is the
/// repaint, not demand, for this long.
pub(crate) const REPAINT_GRACE_US: u64 = 1_000_000;

/// Headroom guard: the bandwidth *estimate* below which the classifier
/// forces H264 regardless of the bandwidth rule, held for
/// [`HEADROOM_ENTER_HOLD_US`] before it engages. Below ~2 Mbit/s the tile
/// codec cannot keep up with the frame rate.
pub(crate) const HEADROOM_ENTER_BPS: u64 = 2_000_000;
/// The estimate above which an engaged headroom guard lets go, held for
/// [`HEADROOM_EXIT_HOLD_US`]. Between the two the guard keeps its state, so
/// an estimate wandering around 2 Mbit/s cannot flap the mode.
pub(crate) const HEADROOM_EXIT_BPS: u64 = 3_000_000;
pub(crate) const HEADROOM_ENTER_HOLD_US: u64 = 500_000;
pub(crate) const HEADROOM_EXIT_HOLD_US: u64 = 1_000_000;
/// Consecutive evaluations a guard condition must also hold for (the
/// preemption-immune half of the hybrid dwell, as for the cost path).
pub(crate) const HEADROOM_MIN_EVALUATIONS: u32 = 3;
/// After `h264_stalled` clears, H264 may not be re-entered (by any path) for
/// this long.
pub(crate) const H264_STALL_REENTRY_US: u64 = 2_000_000;
/// Each stall that follows a re-entry doubles the holdoff, up to this.
pub(crate) const H264_STALL_REENTRY_MAX_US: u64 = 30_000_000;
/// H.264 that runs this long without a stall resets the holdoff to
/// [`H264_STALL_REENTRY_US`].
pub(crate) const H264_STALL_HEALTHY_RESET_US: u64 = 10_000_000;
/// An idle screen (no dirty tiles) this long releases an engaged headroom
/// guard whose estimate is back above the enter threshold: tiny P frames
/// cannot raise the estimate, so on a static screen it would never leave.
pub(crate) const HEADROOM_IDLE_RELEASE_US: u64 = 2_000_000;

/// Smoothed loss-rate threshold above which the classifier hard-overrides
/// to H264, when the override is enabled (`GHOSTFRAME_LOSS_OVERRIDE`). It is
/// off by default: with every H.264 fragment a ledger transmission and one
/// lost fragment killing a frame, "H264 rides through loss better" no longer
/// holds, and the threshold has not been re-measured.
pub(crate) const LOSS_OVERRIDE_THRESHOLD: f32 = 0.10;

/// The headroom guard's own state. Kept apart from [`ClassifierHysteresis`]
/// because the guard's branches must not be reset by (or reset) the cost
/// path's counters.
#[derive(Debug, Clone, Copy, Default)]
struct GuardState {
    /// The guard is engaged (forcing H264).
    headroom_active: bool,
    /// When the estimate first went (and has stayed) below the enter
    /// threshold / above the exit threshold, and for how many evaluations.
    below_since_us: Option<u64>,
    below_evals: u32,
    above_since_us: Option<u64>,
    above_evals: u32,
    /// The last evaluation at which `h264_stalled` was set.
    stalled_at_us: Option<u64>,
    /// Stalls since H.264 last ran healthily for
    /// [`H264_STALL_HEALTHY_RESET_US`]; the holdoff doubles with each.
    stall_level: u32,
    /// The holdoff of the stall in force, µs.
    holdoff_us: u64,
    /// When H.264 started running without a stall (None while not in H264).
    healthy_since_us: Option<u64>,
    /// When the dirty set last became (and stayed) empty.
    idle_since_us: Option<u64>,
}

/// Hysteresis state for `Classifier::decide_frame_mode_at`.
///
/// Hybrid design: a transition requires BOTH a wall-clock dwell AND
/// a streak of consecutive calls where the condition holds. The streak
/// counter is preemption-immune (doesn't advance during pauses); the
/// wall-clock dwell enforces frame-rate-independent timing at high FPS.
/// Both must be satisfied to flip — fixes the 7e3e041 regression where
/// docker scheduling pauses caused wall-clock-only dwell to spuriously
/// elapse past the threshold despite few actual calls.
#[derive(Debug, Clone, Copy, Default)]
struct ClassifierHysteresis {
    /// Wall-clock µs at which the enter condition first started holding,
    /// or `None` if it isn't holding now.
    enter_started_us: Option<u64>,
    /// Wall-clock µs at which the exit condition first started holding,
    /// or `None` if it isn't holding now.
    exit_started_us: Option<u64>,
    /// Consecutive calls where the enter condition has held. Reset to 0
    /// on the first call where enter_now=false.
    enter_streak: u32,
    /// Consecutive calls where the exit condition has held.
    exit_streak: u32,
}

/// Tunable decision parameters; defaults reflect the design doc M3.0 values.
#[derive(Debug, Clone)]
pub struct Classifier {
    /// Wall-clock µs the enter condition must hold before transitioning
    /// to H264. Default 500_000 µs. Hybrid: this
    /// AND `enter_sustain_frames_min` must both be satisfied to flip.
    pub enter_sustain_micros: u64,
    /// Wall-clock µs the exit condition must hold before transitioning
    /// back to TileCodec. Default 1_000_000 µs.
    /// Hybrid: this AND `exit_sustain_frames_min` must both be satisfied.
    pub exit_sustain_micros: u64,
    /// Minimum consecutive calls (frame-rate-independent floor) where
    /// enter condition holds. Default 3. Required ALONGSIDE
    /// `enter_sustain_micros` to flip — preemption-immune component.
    pub enter_sustain_frames_min: u32,
    /// Minimum consecutive calls where exit condition holds. Default 30.
    pub exit_sustain_frames_min: u32,
    state: ClassifierHysteresis,
    guard: GuardState,
    adaptation_context: AdaptationContext,
    /// Calibrated first-pass size per tile.
    first_pass: FirstPassModel,
    /// First-pass bytes of the last second of frames.
    demand: DemandWindow,
    /// The demand at the last decision, bit/s (logged).
    last_demand_bps: u64,
    /// Tiles on the screen, and the predicted first-pass bytes of
    /// repainting all of them (0 when unknown or not in H.264).
    screen_tiles: u32,
    repaint_bytes: u64,
    /// A repaint was requested; until then a near-full-screen frame is not
    /// demand.
    repaint_grace_until_us: Option<u64>,
    /// The next frame is one slow-start cushion repaint (see
    /// `note_repaint_frame`); cleared by the next decision.
    repaint_frame_pending: bool,
    /// `ENTER_SHARE` / `EXIT_SHARE`, or their `ClassifierConfig` overrides.
    enter_share: f64,
    exit_share: f64,
    /// M3.7a env-var override: when `Some`, `decide_inner` uses this
    /// instead of `LOSS_OVERRIDE_THRESHOLD`. Populated from
    /// `ClassifierConfig` in `Classifier::new`.
    loss_override_threshold_override: Option<f32>,
    /// `GHOSTFRAME_LOSS_OVERRIDE`: whether `loss_override` is evaluated at all.
    loss_override_enabled: bool,
    /// M3.7b env-var override for the headroom guard's enter threshold. Populated
    /// from `ClassifierConfig` in `Classifier::new`.
    headroom_min_bpus_override: Option<f32>,
    /// Mode pin (`GHOSTFRAME_FORCE_FRAME_MODE`): when `Some`, `decide_inner` returns this mode every
    /// frame, bypassing all hysteresis, hard overrides, and cost paths.
    /// Lets tests isolate end-to-end behaviour of a specific codec path
    /// (e.g. H.264 colorspace) without depending on the M3.6 mode-switch
    /// policy firing on its own. Populated from
    /// `ClassifierConfig` in `Classifier::new` —
    /// accepts `"h264"` or `"tile"`. `None` unless configured.
    force_frame_mode_override: Option<FrameMode>,
    /// Last emitted mode (debounces the `mode.decision` event to fire
    /// only on transitions). `None` before the first decision.
    last_emitted_mode: Option<FrameMode>,
    /// Reference instant used by `decide_frame_mode` to convert
    /// monotonic time into µs for `decide_frame_mode_at`. Set in
    /// `Default` to `Instant::now()` at Classifier construction.
    /// Tests calling `decide_frame_mode_at` directly bypass this.
    epoch: std::time::Instant,
}

/// Production-inert defaults, with **no** environment read: equivalent to
/// `Classifier::new(ClassifierConfig::default())`.
///
/// The env-reading version of this impl was the last ambient-configuration
/// path in the library — every override now arrives explicitly through
/// `ClassifierConfig`, sourced from `LibConfig::from_env()` at the executable
/// boundary. Kept because tests use `Classifier { ..Default::default() }`
/// struct-update syntax.
impl Default for Classifier {
    fn default() -> Self {
        Self::new(crate::config::ClassifierConfig::default())
    }
}

impl Classifier {
    /// Build a classifier from an explicit configuration. Overrides come
    /// from `config` rather than from process-global environment reads, so
    /// callers (including tests) no longer need an environment lock
    /// to avoid racing other threads' `std::env::set_var` calls.
    pub fn new(config: crate::config::ClassifierConfig) -> Self {
        Self {
            enter_sustain_micros: 500_000,
            exit_sustain_micros: 1_000_000,
            enter_sustain_frames_min: 3,
            exit_sustain_frames_min: 30,
            state: ClassifierHysteresis::default(),
            guard: GuardState::default(),
            adaptation_context: AdaptationContext::default(),
            first_pass: FirstPassModel::default(),
            demand: DemandWindow::default(),
            last_demand_bps: 0,
            screen_tiles: 0,
            repaint_bytes: 0,
            repaint_grace_until_us: None,
            repaint_frame_pending: false,
            enter_share: config.enter_share.map_or(ENTER_SHARE, f64::from),
            exit_share: config.exit_share.map_or(EXIT_SHARE, f64::from),
            loss_override_threshold_override: config.loss_override_threshold,
            loss_override_enabled: config.loss_override_enabled,
            headroom_min_bpus_override: config.headroom_min_bpus,
            force_frame_mode_override: config.force_frame_mode,
            last_emitted_mode: None,
            epoch: std::time::Instant::now(),
        }
    }
}

impl Classifier {
    /// Clear hysteresis state so the next `decide_frame_mode` starts fresh.
    /// Call this when a new client session connects (so prior session's
    /// streak counters don't influence the decision for the new client).
    /// Clears the demand window, repaint state and hysteresis; keeps the
    /// first-pass calibration, which describes this server's content, not
    /// the client, and is reset only when the process starts.
    pub fn reset(&mut self) {
        self.state = ClassifierHysteresis::default();
        self.guard = GuardState::default();
        self.demand.clear();
        self.last_demand_bps = 0;
        self.repaint_bytes = 0;
        self.repaint_grace_until_us = None;
        self.repaint_frame_pending = false;
    }

    /// Push a live `AdaptationContext` from the transport layer.
    pub fn set_adaptation_context(&mut self, ctx: AdaptationContext) {
        self.adaptation_context = ctx;
    }

    /// Snapshot of the most recent `AdaptationContext` pushed via
    /// `set_adaptation_context`.
    pub fn adaptation_context(&self) -> AdaptationContext {
        self.adaptation_context
    }

    /// The screen's tile count and the predicted first-pass bytes of
    /// repainting it (see `first_pass::repaint_state`). The bridge calls this
    /// every frame; `repaint_bytes` may be 0 outside H.264.
    pub fn set_screen(&mut self, screen_tiles: u32, repaint_bytes: u64) {
        self.screen_tiles = screen_tiles;
        self.repaint_bytes = repaint_bytes;
    }

    /// A full-screen repaint was requested at `now_us` (session start, the
    /// H.264 exit, a stuck-entry re-dirty): for `REPAINT_GRACE_US` a frame
    /// dirtying at least 90 % of the screen is that repaint, not demand.
    pub fn note_repaint(&mut self, now_us: u64) {
        self.repaint_grace_until_us = Some(now_us.saturating_add(REPAINT_GRACE_US));
    }

    /// The next frame is a repaint: one slow-start cushion frame. Unlike
    /// `note_repaint` it opens no window; the next decision treats its frame as
    /// zero demand if it dirties at least 90 % of the screen, then forgets,
    /// whatever the frame was. Routine baseline invalidations during H.264 mark
    /// frames this way, so they cannot hide a second of real motion.
    pub fn note_repaint_frame(&mut self) {
        self.repaint_frame_pending = true;
    }

    /// The last decision's first-pass demand over the window, bit/s.
    #[cfg(test)]
    pub(crate) fn last_demand_bps(&self) -> u64 {
        self.last_demand_bps
    }

    /// When the current repaint grace ends, if one was noted.
    #[cfg(test)]
    pub(crate) fn repaint_grace_until_us(&self) -> Option<u64> {
        self.repaint_grace_until_us
    }

    /// One emission batch's first-pass bytes of `kind` (see
    /// `FirstPassModel::observe`).
    pub fn observe_first_pass(&mut self, kind: FirstPassKind, bytes: u64, tiles: u32) {
        self.first_pass.observe(kind, bytes, tiles);
    }

    /// The calibrated per-tile model, for the bridge's repaint estimate.
    pub fn first_pass(&self) -> FirstPassModel {
        self.first_pass
    }

    /// Decide the whole-frame mode on bandwidth, with wall-clock hysteresis.
    ///
    /// `now_us` is the current wall-clock time in microseconds (monotonic or
    /// system time). `tentative_states` is the per-dirty-tile output of
    /// `classify_tile()`. `prev_mode` is what the previous frame emitted.
    ///
    /// Empty `tentative_states` is valid: a frame with no dirty tiles adds no
    /// demand.
    pub fn decide_frame_mode_at(
        &mut self,
        now_us: u64,
        tentative_states: &[CodecState],
        prev_mode: FrameMode,
    ) -> FrameMode {
        let (next_mode, reason) = self.decide_inner(now_us, tentative_states, prev_mode);
        if self.last_emitted_mode != Some(next_mode) {
            let ctx = self.adaptation_context;
            tracing::info!(
                target: "ghostframe::bench",
                event = "mode.decision",
                from = ?prev_mode,
                to = ?next_mode,
                reason = reason,
                smoothed_rtt_us = ctx.smoothed_rtt_us,
                loss_rate = ctx.loss_rate,
                suspended = ctx.suspended,
                h264_stalled = ctx.h264_stalled,
                bwe_estimate_bps = ctx.bwe_estimate_bps,
                first_pass_demand_bps = self.last_demand_bps,
                estimate_known = ctx.estimate_known,
                fp_solid_bytes = self.first_pass.tile_bytes_of(FirstPassKind::Solid).round() as u32,
                fp_palrle_bytes = self.first_pass.tile_bytes_of(FirstPassKind::PalRle).round() as u32,
                fp_cdf53_bytes = self.first_pass.tile_bytes_of(FirstPassKind::Cdf53).round() as u32,
                repaint_drain_ms = self.repaint_drain_us().min(u64::MAX / 2) / 1000,
                "mode decision"
            );
            self.last_emitted_mode = Some(next_mode);
        }
        next_mode
    }

    /// Returns the last `FrameMode` for which a `mode.decision` event
    /// was emitted, or `None` if no decision has been made yet.
    pub fn last_emitted_mode(&self) -> Option<FrameMode> {
        self.last_emitted_mode
    }

    /// The headroom guard with hysteresis, on the bandwidth estimate: engages
    /// after the estimate has stayed below the enter threshold for
    /// [`HEADROOM_ENTER_HOLD_US`], lets go after it has stayed above the exit
    /// threshold for [`HEADROOM_EXIT_HOLD_US`], and keeps its state in
    /// between. Both holds also need [`HEADROOM_MIN_EVALUATIONS`] consecutive
    /// evaluations. An unknown estimate (0) changes nothing and restarts both
    /// holds. Returns whether the guard is engaged.
    fn headroom_guard_engaged(&mut self, now_us: u64, estimate_bps: u64, idle_us: u64) -> bool {
        // An overridden enter threshold brings the exit threshold along at the
        // 3:2 ratio of the defaults.
        let (enter_bps, exit_bps) = match self.headroom_min_bpus_override {
            Some(bpus) => {
                let enter = (f64::from(bpus) * 8e6) as u64;
                (enter, enter.saturating_add(enter / 2))
            }
            None => (HEADROOM_ENTER_BPS, HEADROOM_EXIT_BPS),
        };
        let g = &mut self.guard;
        let known = estimate_bps > 0;
        // A static screen cannot raise the estimate (a P frame of a few
        // hundred bytes is no probe): once the estimate is back above the
        // enter threshold and nothing has changed for a while, let go.
        if g.headroom_active
            && known
            && estimate_bps >= enter_bps
            && idle_us >= HEADROOM_IDLE_RELEASE_US
        {
            g.headroom_active = false;
            g.below_since_us = None;
            g.below_evals = 0;
            g.above_since_us = None;
            g.above_evals = 0;
            return false;
        }
        if known && estimate_bps < enter_bps {
            g.above_since_us = None;
            g.above_evals = 0;
            if !g.headroom_active {
                let since = *g.below_since_us.get_or_insert(now_us);
                g.below_evals = g.below_evals.saturating_add(1);
                if now_us.saturating_sub(since) >= HEADROOM_ENTER_HOLD_US
                    && g.below_evals >= HEADROOM_MIN_EVALUATIONS
                {
                    g.headroom_active = true;
                    g.below_since_us = None;
                    g.below_evals = 0;
                }
            }
        } else if known && estimate_bps > exit_bps {
            g.below_since_us = None;
            g.below_evals = 0;
            if g.headroom_active {
                let since = *g.above_since_us.get_or_insert(now_us);
                g.above_evals = g.above_evals.saturating_add(1);
                if now_us.saturating_sub(since) >= HEADROOM_EXIT_HOLD_US
                    && g.above_evals >= HEADROOM_MIN_EVALUATIONS
                {
                    g.headroom_active = false;
                    g.above_since_us = None;
                    g.above_evals = 0;
                }
            }
        } else {
            g.below_since_us = None;
            g.below_evals = 0;
            g.above_since_us = None;
            g.above_evals = 0;
        }
        g.headroom_active
    }

    /// Pure decision logic returning `(mode, reason)`. Each branch
    /// returns a static `&'static str` naming the cause:
    /// "headroom_guard", "loss_override", "suspension", "bandwidth_short",
    /// "bandwidth_fits", "estimate_unknown" or "hysteresis_clamp".
    pub(crate) fn decide_inner(
        &mut self,
        now_us: u64,
        tentative_states: &[CodecState],
        prev_mode: FrameMode,
    ) -> (FrameMode, &'static str) {
        let frame_bytes: f64 = tentative_states
            .iter()
            .map(|s| self.first_pass.state_bytes(*s))
            .sum();
        let in_grace = self.repaint_grace_until_us.is_some_and(|t| now_us < t);
        let marked = std::mem::take(&mut self.repaint_frame_pending);
        let is_repaint = (in_grace || marked)
            && self.screen_tiles > 0
            && (tentative_states.len() as u64) * 10 >= u64::from(self.screen_tiles) * 9;
        self.last_demand_bps = self
            .demand
            .push(now_us, if is_repaint { 0.0 } else { frame_bytes });
        let out = self.decide_core(now_us, tentative_states, prev_mode);
        // H.264 that has run for a while without a stall earns the short
        // re-entry holdoff back.
        if out.0 == FrameMode::H264 && out.1 != "h264_stalled" {
            let since = *self.guard.healthy_since_us.get_or_insert(now_us);
            if now_us.saturating_sub(since) >= H264_STALL_HEALTHY_RESET_US {
                self.guard.stall_level = 0;
            }
        } else {
            self.guard.healthy_since_us = None;
        }
        out
    }

    fn decide_core(
        &mut self,
        now_us: u64,
        tentative_states: &[CodecState],
        prev_mode: FrameMode,
    ) -> (FrameMode, &'static str) {
        // No encoder can produce H.264 at all. This beats even the mode pin:
        // a pinned H.264 with no encoder would classify every frame as H.264
        // and discard it (a frozen screen). The stall machinery has nothing to
        // say about a stream that does not exist.
        if self.adaptation_context.h264_encoder_unavailable {
            self.state = ClassifierHysteresis::default();
            return (FrameMode::TileCodec, "h264_encoder_unavailable");
        }
        // Mode pin: when GHOSTFRAME_FORCE_FRAME_MODE (or the older
        // GHOSTFRAME_TEST_FORCE_FRAME_MODE) is set, return that mode every
        // call without running the cost model or hysteresis. Lets tests
        // isolate one codec path end-to-end.
        if let Some(forced) = self.force_frame_mode_override {
            self.state = ClassifierHysteresis::default();
            return (forced, "test_force");
        }
        // Hard overrides: bypass hysteresis entirely. This is the user-
        // visible "the link can't support tile-codec emission" signal —
        // dwell would just delay an inevitable degradation. Same applies
        // to loss-override and suspension-override added in Task 9. We
        // accept the resulting mode-switch count uptick as the cost of
        // fast recovery; M3.6c's Table 9 verifies it's not pathological.
        let ctx = self.adaptation_context;

        // Client-capability override (HELLO bit 1, `supports_h264`).
        // Evaluated BEFORE the network-driven H.264 forces because none of
        // them — headroom_guard, loss_override, suspension — are useful
        // when the client physically cannot decode H.264. The Firefox path
        // (WebGPU without texture_external) lands here.
        if !ctx.supports_h264 {
            self.state = ClassifierHysteresis::default();
            return (FrameMode::TileCodec, "h264_unsupported");
        }

        // An H.264 transport stall beats every other override, including
        // the ones that would force H264: it is the only way out of H.264
        // when the path stops carrying it. Re-entry (by any path) waits for
        // the flag to have been clear for `H264_STALL_REENTRY_US`.
        // The guard's timers run through a stall and its holdoff (its result
        // is discarded there): an estimate that recovered while the session
        // was in tile must release the guard on time, not when H.264 returns.
        if tentative_states.is_empty() {
            self.guard.idle_since_us.get_or_insert(now_us);
        } else {
            self.guard.idle_since_us = None;
        }
        let idle_us = self
            .guard
            .idle_since_us
            .map_or(0, |t| now_us.saturating_sub(t));
        let guard_engaged = self.headroom_guard_engaged(now_us, ctx.bwe_estimate_bps, idle_us);

        if ctx.h264_stalled {
            // A stall that ends a stretch of H.264 doubles the holdoff
            // (2 s, 4 s, ... 30 s); one that restarts the holdoff already
            // running does not.
            if prev_mode == FrameMode::H264 {
                self.guard.stall_level = self.guard.stall_level.saturating_add(1);
                self.guard.holdoff_us = H264_STALL_REENTRY_US
                    .saturating_mul(1u64 << (self.guard.stall_level - 1).min(16))
                    .min(H264_STALL_REENTRY_MAX_US);
            } else if self.guard.holdoff_us == 0 {
                self.guard.holdoff_us = H264_STALL_REENTRY_US;
            }
            self.state = ClassifierHysteresis::default();
            self.guard.stalled_at_us = Some(now_us);
            return (FrameMode::TileCodec, "h264_stalled");
        }
        if self
            .guard
            .stalled_at_us
            .is_some_and(|t| now_us.saturating_sub(t) < self.guard.holdoff_us)
        {
            self.state = ClassifierHysteresis::default();
            return (FrameMode::TileCodec, "h264_stalled_holdoff");
        }

        if guard_engaged {
            return (FrameMode::H264, "headroom_guard");
        }
        let loss_threshold = self
            .loss_override_threshold_override
            .unwrap_or(LOSS_OVERRIDE_THRESHOLD);
        let loss_force_h264 = self.loss_override_enabled && ctx.loss_rate > loss_threshold;
        if loss_force_h264 {
            self.state = ClassifierHysteresis::default();
            return (FrameMode::H264, "loss_override");
        }
        let suspension_force_h264 = ctx.suspended;
        if suspension_force_h264 {
            self.state = ClassifierHysteresis::default();
            return (FrameMode::H264, "suspension");
        }

        // Bandwidth path (design 2026-11-07): H.264 only when the tile codec's
        // first pass does not fit the estimate.
        if !ctx.estimate_known || ctx.bwe_estimate_bps == 0 {
            self.state = ClassifierHysteresis::default();
            return (prev_mode, "estimate_unknown");
        }
        let estimate = ctx.bwe_estimate_bps as f64;
        let demand = self.last_demand_bps as f64;
        let enter_now = demand > self.enter_share * estimate;
        let exit_now =
            demand < self.exit_share * estimate && self.repaint_drain_us() <= REPAINT_DRAIN_MAX_US;

        // Hybrid hysteresis: a transition requires BOTH the wall-clock
        // dwell AND the consecutive-call streak. The streak is preemption-
        // immune (no calls during a pause = no advance); the wall-clock
        // floor prevents over-eager flips at high FPS. See 7e3e041
        // regression in `feedback_classifier_hysteresis_micros_regression`.
        match prev_mode {
            FrameMode::TileCodec => {
                if enter_now {
                    let started = self.state.enter_started_us.get_or_insert(now_us);
                    self.state.enter_streak = self.state.enter_streak.saturating_add(1);
                    self.state.exit_started_us = None;
                    self.state.exit_streak = 0;
                    let micros_elapsed =
                        now_us.saturating_sub(*started) >= self.enter_sustain_micros;
                    let frames_elapsed = self.state.enter_streak >= self.enter_sustain_frames_min;
                    if micros_elapsed && frames_elapsed {
                        self.state.enter_started_us = None;
                        self.state.enter_streak = 0;
                        return (FrameMode::H264, "bandwidth_short");
                    }
                } else {
                    self.state.enter_started_us = None;
                    self.state.enter_streak = 0;
                }
                (FrameMode::TileCodec, "hysteresis_clamp")
            }
            FrameMode::H264 => {
                if exit_now {
                    let started = self.state.exit_started_us.get_or_insert(now_us);
                    self.state.exit_streak = self.state.exit_streak.saturating_add(1);
                    self.state.enter_started_us = None;
                    self.state.enter_streak = 0;
                    let micros_elapsed =
                        now_us.saturating_sub(*started) >= self.exit_sustain_micros;
                    let frames_elapsed = self.state.exit_streak >= self.exit_sustain_frames_min;
                    if micros_elapsed && frames_elapsed {
                        self.state.exit_started_us = None;
                        self.state.exit_streak = 0;
                        return (FrameMode::TileCodec, "bandwidth_fits");
                    }
                } else {
                    self.state.exit_started_us = None;
                    self.state.exit_streak = 0;
                }
                (FrameMode::H264, "hysteresis_clamp")
            }
        }
    }

    /// How long repainting the screen's first pass takes at the estimate, µs.
    fn repaint_drain_us(&self) -> u64 {
        let est = self.adaptation_context.bwe_estimate_bps;
        if est == 0 {
            return u64::MAX;
        }
        ((self.repaint_bytes as f64) * 8.0 * 1e6 / est as f64) as u64
    }

    /// Apply per-tile rules across all dirty tiles, then decide whole-frame
    /// mode on bandwidth with hysteresis.
    ///
    /// This is a thin wrapper around [`decide_frame_mode_at`] that reads
    /// `self.epoch`'s wall-clock elapsed time. `IoBridge` (the only
    /// production caller as of the browserless-harness work) calls
    /// `decide_frame_mode_at` directly with a `now_us` derived from its own
    /// `now_std()` epoch instead, so that frame-mode hysteresis tracks
    /// tokio's (possibly paused) clock rather than this struct's own
    /// `Instant::now()`-seeded one. This wrapper is kept only for callers
    /// that don't care about deterministic/virtual timing (currently just
    /// unit tests in this file and `classifier_decide_tests.rs`) — anything
    /// that needs to run under `tokio::time::pause()` must call
    /// `decide_frame_mode_at` directly with an explicit `now_us`.
    pub fn decide_frame_mode(
        &mut self,
        tentative_states: &[CodecState],
        prev_mode: FrameMode,
    ) -> FrameMode {
        // Monotonic source — `SystemTime` would skew under NTP and freeze
        // hysteresis during the skew window. `Instant::elapsed` is
        // guaranteed non-decreasing on every supported platform.
        //
        // Deliberately wall-clock, not virtual: this wrapper exists for
        // callers that never run under `tokio::time::pause()` (see doc
        // comment above), so reading `self.epoch`'s real elapsed time here
        // is correct and intentional, not the bug `clippy::disallowed_methods`
        // usually catches in this codebase.
        #[allow(
            clippy::disallowed_methods,
            reason = "self.epoch is stamped with a real std::time::Instant at construction, never from now_std(), so .elapsed() compares wall against wall and cannot saturate. Callers may run under tokio::time::pause() -- io_bridge's negative-control test does so deliberately -- but that is exactly the stale-dwell behaviour this wrapper is being contrasted against there. Production reads the bridge clock via decide_frame_mode_at()"
        )]
        let now_us = self.epoch.elapsed().as_micros() as u64;
        self.decide_frame_mode_at(now_us, tentative_states, prev_mode)
    }
}

#[cfg(test)]
#[path = "classifier_decide_tests.rs"]
mod decide_tests;
