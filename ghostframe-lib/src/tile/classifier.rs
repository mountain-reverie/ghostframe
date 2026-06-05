//! Per-tile classifier and cost-aware frame-mode decision.
//!
//! See `docs/superpowers/specs/2026-04-28-m3-codec-suite-design.md` Phase M3.0
//! and `docs/specs/ghostframe-initial-spec.md` §4.2 / §4.3.

use super::{CodecState, FrameMode, TileMetrics, TILE_BYTES};

/// Hardcoded encoding-cost model in microseconds per call, plus a bandwidth
/// weighting expressed as bytes per microsecond. Values rounded from the
/// source spec's GPU compute claims; M3.5 retunes from real measurements.
#[derive(Debug, Clone, Copy)]
pub struct CostModel {
    pub solid_us: f32,      // ~0.5  µs (4-byte memcpy)
    pub palrle_us: f32,     // ~5    µs (nibble-pack 1024 px)
    pub bc1_us: f32,        // ~50   µs (Vulkan compute, dispatched)
    pub cdf53_us: f32,      // ~50   µs (Vulkan compute, dispatched)
    pub h264_frame_us: f32, // ~3000 µs (VA-API full-frame encode @ 1080p)
    /// Estimated bytes for a full-frame H.264 emission. Conservative default
    /// (1080p, ~6 Mbit/s @ 60 fps → ~12 KB / frame). Updated alongside
    /// `bytes_per_us` when the source spec §6.5 estimator lands (M4+).
    pub h264_frame_bytes: u32,
    /// Bandwidth weighting; placeholder until source spec §6.5 estimator lands.
    /// Default value matches a typical home-LAN link (100 Mbps → ~12.5 B/µs).
    /// Private — accessed via `bytes_per_us()` per design D17 so the §6.5
    /// estimator wire-up is a one-call change later.
    bytes_per_us: f32,
}

impl Default for CostModel {
    fn default() -> Self {
        // Per-codec µs values retuned from M3.5b bench measurements (see
        // docs/specs/m3-codec-bench-results.md Section 4). The bench
        // measures per-tile `encoder.encode()` cost via criterion; the
        // values below are the median across the 6 content classes,
        // rounded conservatively:
        //
        //   solid_us:    measured 0.015 µs (4-byte memcpy); use 0.05 for
        //                3× headroom on slow runs
        //   palrle_us:   measured 3-12 µs across feasible classes
        //                (gradient/photo/motion return the empty-Vec fast
        //                path at 0.1 µs because >16 colors); use 8.0 as a
        //                conservative upper bound covering flat_ui's
        //                11.7 µs worst case
        //   cdf53_us:    measured 52-149 µs; use 90.0 as a robust median
        //                (motion = 79, text = 73, flat_ui = 96, photo =
        //                149)
        //   bc1_us:      no measured data (BC1 isn't implemented; per
        //                the M3.5b BC1-fate verdict the variant is being
        //                removed). Kept here only until the Codec::Bc1
        //                removal commit follows.
        //   h264_frame_us: per-tile-H264 bench (~750 µs) measures the
        //                dead-code path; the classifier's actual cost is
        //                full-frame H264, not benched here, so the
        //                3000 µs placeholder stays until M4 wires the
        //                §6.5 estimator
        //   h264_frame_bytes / bytes_per_us: unchanged — same §6.5
        //                reasoning
        Self {
            solid_us: 0.05,
            palrle_us: 8.0,
            bc1_us: 50.0,
            cdf53_us: 90.0,
            h264_frame_us: 3000.0,
            h264_frame_bytes: 12_000,
            bytes_per_us: 12.5,
        }
    }
}

impl CostModel {
    /// Bandwidth weighting in B/µs. Currently a hardcoded placeholder
    /// (~12.5 = 100 Mbps). Will be wired to the source-spec §6.5 estimator
    /// in M4+; this single accessor is the swap-in point (per design D17).
    pub fn bytes_per_us(&self) -> f32 {
        self.bytes_per_us
    }

    /// Update the bandwidth weighting (call from §6.5 estimator).
    pub fn set_bytes_per_us(&mut self, value: f32) {
        self.bytes_per_us = value;
    }

    /// Estimated emission size in bytes for a tile assigned to `state`.
    pub fn estimated_tile_bytes(&self, state: CodecState) -> u32 {
        match state {
            CodecState::Skip => 0,
            CodecState::Solid => 4,
            CodecState::PalRle { .. } => 200, // §5.3: typical text 100–200 B; upper bound
            CodecState::Bc1 => 512,
            CodecState::Cdf53 { .. } => 1300, // §4.4: 1.0–1.3 KB to lossless; upper bound
            CodecState::PixelPerfect => 0,
            CodecState::H264 { .. } => TILE_BYTES as u32, // upper bound; classified-as-H264 tile
        }
    }

    /// Estimated encode cost in microseconds for one tile assigned to `state`.
    pub fn estimated_tile_us(&self, state: CodecState) -> f32 {
        match state {
            CodecState::Skip | CodecState::PixelPerfect => 0.0,
            CodecState::Solid => self.solid_us,
            CodecState::PalRle { .. } => self.palrle_us,
            CodecState::Bc1 => self.bc1_us,
            CodecState::Cdf53 { .. } => self.cdf53_us,
            CodecState::H264 { .. } => self.h264_frame_us,
        }
    }
}

/// Live signal vector consumed by `Classifier::decide_frame_mode`.
///
/// Populated by `IoBridge` from `ReceiverFeedback` + QUIC `path_stats`. All
/// fields default to "no information" (`Default` = zeros / `false`). Defaults
/// keep the M3.5-equivalent behavior when no context has been pushed yet.
#[derive(Debug, Clone, Copy, Default)]
pub struct AdaptationContext {
    /// Smoothed throughput estimate, bytes per µs. Sourced from
    /// `min(cwnd/rtt, recent_emit_bytes/window)`. `0.0` means "no estimate
    /// yet" — `Classifier` falls back to its prior `bytes_per_us`.
    pub bytes_per_us: f32,
    /// Smoothed one-way RTT, µs. Sourced from QUIC `path_stats.rtt`.
    pub smoothed_rtt_us: f32,
    /// Smoothed datagram loss rate over the last 5 `ReceiverFeedback` windows.
    pub loss_rate: f32,
    /// Last `ReceiverFeedback`'s suspension flag, OR'd across the last 2
    /// windows to debounce single-window flaps.
    pub suspended: bool,
    /// Monotonic counter incremented on each `set_adaptation_context` call.
    /// Tests assert "classifier saw the new value before decision N".
    pub last_update_seq: u32,
}

#[cfg(test)]
#[path = "classifier_cost_tests.rs"]
mod cost_tests;

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
/// dropping to BC1 (Rule 4). Outside the medium-frequency band the rules
/// apply normally — a tile that drops to low-freq (< 5 Hz) is re-classified
/// fresh per Rules 6-8. (Per-tile hysteresis differs from the frame-mode
/// hysteresis in `Classifier::decide_frame_mode`.)
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
        let frames = match prev {
            CodecState::H264 { frames_in_h264 } => frames_in_h264.saturating_add(1),
            _ => 1,
        };
        return CodecState::H264 {
            frames_in_h264: frames,
        };
    }

    // Rule 3: high freq AND low magnitude ⇒ PalRle if few colors known, else BC1
    if freq > 15.0 && mag <= 0.3 {
        if uc_known && metrics.unique_colors <= 16 {
            return CodecState::PalRle { palette_id: 0 };
        }
        return CodecState::Bc1;
    }

    // Rule 4: medium freq AND currently H264 ⇒ stay H264 (per-tile hysteresis).
    // Rule 5 (medium freq AND not H264) prefers a lossless codec when color
    // info is known (mirrors Rule 3's structure for high-freq + low-mag),
    // and falls back to Bc1 only when color info is unavailable.
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
        return CodecState::Bc1; // Rule 5 fallback
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
        max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
    }
}

/// Apply the Cdf53 emission gates to a classifier-produced [`CodecState`].
///
/// If the state is [`CodecState::Cdf53`] and either gate is closed, downgrade
/// to the pre-M3.3a high-color fallback ([`CodecState::Bc1`] — io_bridge maps
/// this to `Codec::Raw` on the wire, preserving M3.2 behavior). Non-Cdf53
/// states pass through unchanged.
///
/// Gates:
/// - `cdf53_enabled`: server-side env flag `GHOSTFRAME_ENABLE_CDF53`.
/// - `client_supports_cdf53`: per-session HELLO `supports_cdf53` capability bit.
pub fn gate_codec_state(
    state: CodecState,
    cdf53_enabled: bool,
    client_supports_cdf53: bool,
) -> CodecState {
    if matches!(state, CodecState::Cdf53 { .. }) && !(cdf53_enabled && client_supports_cdf53) {
        // Downgrade to Bc1: the Rule-5 / pre-M3.3a high-color fallback.
        // io_bridge's `_ => (Codec::Raw, tile_data)` arm already handles Bc1
        // as Raw emission, so no wire-format change occurs.
        CodecState::Bc1
    } else {
        state
    }
}

#[cfg(test)]
#[path = "classifier_classify_tests.rs"]
mod classify_tests;

/// Per-tile bias added to the H264 comparand when refinement passes are
/// outstanding. Pulls the cost comparison toward TileCodec when there's
/// PixelPerfect work left to deliver and bandwidth headroom permits.
/// Retuned in M3.6c from the bandwidth × scene matrix.
pub const REFINEMENT_BIAS_PER_TILE_US: f32 = 5.0;

/// Minimum bandwidth (bytes per µs) below which the classifier
/// hard-overrides to H264 regardless of the cost comparison. 0.25 B/µs
/// ≈ 2 Mbps — below this, individual tile-codec emissions can't keep up
/// with frame-rate. Retuned in M3.6c.
pub const HEADROOM_MIN_BYTES_PER_US: f32 = 0.25;

/// Smoothed loss-rate threshold above which the classifier hard-overrides
/// to H264. 10 % datagram loss makes per-tile reassembly + refinement
/// unworkable; H264 full-frame's larger I-frame interval rides through
/// better. Retuned in M3.6c.
pub const LOSS_OVERRIDE_THRESHOLD: f32 = 0.10;

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
    pub cost: CostModel,
    pub enter_factor: f32,             // default 1.3
    pub exit_factor: f32,              // default 0.6
    pub motion_tile_threshold: f32,    // default 0.20 (fraction of dirty tiles)
    pub motion_tile_min_absolute: u32, // default 8 (absolute floor)
    /// Wall-clock µs the enter condition must hold before transitioning
    /// to H264. Default 50_000 µs (≈ 3 frames at 60 fps). Hybrid: this
    /// AND `enter_sustain_frames_min` must both be satisfied to flip.
    pub enter_sustain_micros: u64,
    /// Wall-clock µs the exit condition must hold before transitioning
    /// back to TileCodec. Default 500_000 µs (≈ 30 frames at 60 fps).
    /// Hybrid: this AND `exit_sustain_frames_min` must both be satisfied.
    pub exit_sustain_micros: u64,
    /// Minimum consecutive calls (frame-rate-independent floor) where
    /// enter condition holds. Default 3. Required ALONGSIDE
    /// `enter_sustain_micros` to flip — preemption-immune component.
    pub enter_sustain_frames_min: u32,
    /// Minimum consecutive calls where exit condition holds. Default 30.
    pub exit_sustain_frames_min: u32,
    state: ClassifierHysteresis,
    adaptation_context: AdaptationContext,
    refinement_deficit_tiles: u32,
    /// M3.7a env-var override: when `Some`, `decide_inner` uses this
    /// instead of `REFINEMENT_BIAS_PER_TILE_US`. Populated from
    /// `GHOSTFRAME_TEST_REFINEMENT_BIAS_US` in `Default::default()` under
    /// `cfg(any(test, feature = "test-loss-injection"))`. `None` in
    /// production builds (env var ignored).
    #[cfg(any(test, feature = "test-loss-injection"))]
    refinement_bias_us_override: Option<f32>,
    /// M3.7a env-var override: when `Some`, `decide_inner` uses this
    /// instead of `LOSS_OVERRIDE_THRESHOLD`. Populated from
    /// `GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD` in `Default::default()`
    /// under `cfg(any(test, feature = "test-loss-injection"))`.
    #[cfg(any(test, feature = "test-loss-injection"))]
    loss_override_threshold_override: Option<f32>,
    /// M3.7b env-var override for HEADROOM_MIN_BYTES_PER_US. Populated
    /// from `GHOSTFRAME_TEST_HEADROOM_MIN_BPUS` in `Default::default()`.
    /// Cfg-gated under `test-loss-injection`.
    #[cfg(any(test, feature = "test-loss-injection"))]
    headroom_min_bpus_override: Option<f32>,
    /// Last emitted mode (debounces the `mode.decision` event to fire
    /// only on transitions). `None` before the first decision.
    last_emitted_mode: Option<FrameMode>,
    /// Reference instant used by `decide_frame_mode` to convert
    /// monotonic time into µs for `decide_frame_mode_at`. Set in
    /// `Default` to `Instant::now()` at Classifier construction.
    /// Tests calling `decide_frame_mode_at` directly bypass this.
    epoch: std::time::Instant,
}

impl Default for Classifier {
    fn default() -> Self {
        Self {
            cost: CostModel::default(),
            enter_factor: 1.3,
            exit_factor: 0.6,
            motion_tile_threshold: 0.20,
            motion_tile_min_absolute: 8,
            enter_sustain_micros: 50_000,
            exit_sustain_micros: 500_000,
            enter_sustain_frames_min: 3,
            exit_sustain_frames_min: 30,
            state: ClassifierHysteresis::default(),
            adaptation_context: AdaptationContext::default(),
            refinement_deficit_tiles: 0,
            #[cfg(any(test, feature = "test-loss-injection"))]
            refinement_bias_us_override: std::env::var("GHOSTFRAME_TEST_REFINEMENT_BIAS_US")
                .ok()
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|v| *v > 0.0),
            #[cfg(any(test, feature = "test-loss-injection"))]
            loss_override_threshold_override: std::env::var("GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD")
                .ok()
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|v| *v > 0.0 && *v <= 1.0),
            #[cfg(any(test, feature = "test-loss-injection"))]
            headroom_min_bpus_override: std::env::var("GHOSTFRAME_TEST_HEADROOM_MIN_BPUS")
                .ok()
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|v| *v > 0.0),
            last_emitted_mode: None,
            epoch: std::time::Instant::now(),
        }
    }
}

impl Classifier {
    /// Clear hysteresis state so the next `decide_frame_mode` starts fresh.
    /// Call this when a new client session connects (so prior session's
    /// streak counters don't influence the decision for the new client).
    pub fn reset(&mut self) {
        self.state = ClassifierHysteresis::default();
    }

    /// Push a live `AdaptationContext` from the transport layer.
    ///
    /// When `ctx.bytes_per_us > 0.0`, that value is mirrored to
    /// `cost.bytes_per_us` so the existing M3.5 cost comparison
    /// transparently uses it. Zero / negative `bytes_per_us` is treated as
    /// "no estimate yet" and leaves the existing `cost.bytes_per_us`
    /// unchanged.
    ///
    /// M3.6a wires every field but `decide_frame_mode` only consumes
    /// `bytes_per_us`. M3.6b extends the consumer.
    pub fn set_adaptation_context(&mut self, ctx: AdaptationContext) {
        self.adaptation_context = ctx;
        if ctx.bytes_per_us > 0.0 {
            self.cost.set_bytes_per_us(ctx.bytes_per_us);
        }
    }

    /// Snapshot of the most recent `AdaptationContext` pushed via
    /// `set_adaptation_context`.
    pub fn adaptation_context(&self) -> AdaptationContext {
        self.adaptation_context
    }

    /// Number of tiles currently in `Cdf53 { passes_sent < max_passes }`.
    /// Caller (io_bridge) updates this each frame from the per-tile state.
    /// Used by M3.6b's refinement-deficit bias term in `decide_frame_mode`.
    pub fn set_refinement_deficit_tiles(&mut self, count: u32) {
        self.refinement_deficit_tiles = count;
    }

    /// Apply per-tile rules across all dirty tiles, then decide whole-frame
    /// mode based on cost + sustained-motion fast-path with wall-clock hysteresis.
    ///
    /// `now_us` is the current wall-clock time in microseconds (monotonic or
    /// system time). `tentative_states` is the per-dirty-tile output of
    /// `classify_tile()`. `prev_mode` is what the previous frame emitted.
    ///
    /// Empty `tentative_states` is valid — it represents a frame with no dirty
    /// tiles. In H264 mode, this contributes toward the exit-sustain timer
    /// (cost is 0, below exit threshold).
    ///
    /// **M3.0 caveat:** `MetricsTracker::record_frame` doesn't populate
    /// `change_magnitude` (the value comes from GPU SAD output that lands in
    /// M3.1+). With magnitude permanently 0.0, `classify_tile` never returns
    /// H264 from Rule 2 on real frames — so the motion fast-path's
    /// `motion_tile_min_absolute` and `motion_tile_threshold` thresholds are
    /// dormant in production and the cost path is the only entry trigger.
    /// Unit tests construct H264 tile states directly to exercise the
    /// fast-path; this stays exercised by tests until M3.1+ wires real
    /// magnitude.
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
                bytes_per_us = ctx.bytes_per_us,
                smoothed_rtt_us = ctx.smoothed_rtt_us,
                loss_rate = ctx.loss_rate,
                suspended = ctx.suspended,
                refinement_deficit_tiles = self.refinement_deficit_tiles,
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

    /// Pure decision logic returning `(mode, reason)`. Each branch
    /// returns a static `&'static str` naming the cause:
    /// "headroom_guard", "loss_override", "suspension", "cost_comparison",
    /// or "hysteresis_clamp".
    fn decide_inner(
        &mut self,
        now_us: u64,
        tentative_states: &[CodecState],
        prev_mode: FrameMode,
    ) -> (FrameMode, &'static str) {
        // Hard overrides: bypass hysteresis entirely. This is the user-
        // visible "the link can't support tile-codec emission" signal —
        // dwell would just delay an inevitable degradation. Same applies
        // to loss-override and suspension-override added in Task 9. We
        // accept the resulting mode-switch count uptick as the cost of
        // fast recovery; M3.6c's Table 9 verifies it's not pathological.
        let ctx = self.adaptation_context;
        let headroom_threshold = {
            #[cfg(any(test, feature = "test-loss-injection"))]
            { self.headroom_min_bpus_override.unwrap_or(HEADROOM_MIN_BYTES_PER_US) }
            #[cfg(not(any(test, feature = "test-loss-injection")))]
            { HEADROOM_MIN_BYTES_PER_US }
        };
        let headroom_force_h264 =
            ctx.bytes_per_us > 0.0 && ctx.bytes_per_us < headroom_threshold;
        if headroom_force_h264 {
            self.state = ClassifierHysteresis::default();
            return (FrameMode::H264, "headroom_guard");
        }
        let loss_threshold = {
            #[cfg(any(test, feature = "test-loss-injection"))]
            { self.loss_override_threshold_override.unwrap_or(LOSS_OVERRIDE_THRESHOLD) }
            #[cfg(not(any(test, feature = "test-loss-injection")))]
            { LOSS_OVERRIDE_THRESHOLD }
        };
        let loss_force_h264 = ctx.loss_rate > loss_threshold;
        if loss_force_h264 {
            self.state = ClassifierHysteresis::default();
            return (FrameMode::H264, "loss_override");
        }
        let suspension_force_h264 = ctx.suspended;
        if suspension_force_h264 {
            self.state = ClassifierHysteresis::default();
            return (FrameMode::H264, "suspension");
        }

        // Cost path. H264-classified tiles are excluded from the per-tile µs
        // sum because `estimated_tile_us(H264)` returns the full-frame VA-API
        // cost (`h264_frame_us`), which can't be summed per-tile coherently —
        // those tiles are accounted for separately by the motion fast-path.
        // All tiles' byte costs ARE summed so any non-empty dirty set carries
        // non-zero tile-codec cost, keeping the deadband effective.
        let non_h264_us: f32 = tentative_states
            .iter()
            .filter(|s| !matches!(s, CodecState::H264 { .. }))
            .map(|s| self.cost.estimated_tile_us(*s))
            .sum();
        let all_tile_bytes: u32 = tentative_states
            .iter()
            .map(|s| self.cost.estimated_tile_bytes(*s))
            .sum();
        let bytes_per_us = self.cost.bytes_per_us();
        let tile_codec_cost = non_h264_us + (all_tile_bytes as f32) / bytes_per_us;
        let per_tile_bias = {
            #[cfg(any(test, feature = "test-loss-injection"))]
            { self.refinement_bias_us_override.unwrap_or(REFINEMENT_BIAS_PER_TILE_US) }
            #[cfg(not(any(test, feature = "test-loss-injection")))]
            { REFINEMENT_BIAS_PER_TILE_US }
        };
        let refinement_bias_us = self.refinement_deficit_tiles as f32 * per_tile_bias;
        let h264_cost = self.cost.h264_frame_us
            + (self.cost.h264_frame_bytes as f32) / bytes_per_us
            + refinement_bias_us;

        // Motion fast-path: count tentatively-H.264 tiles
        let h264_tile_count = tentative_states
            .iter()
            .filter(|s| matches!(s, CodecState::H264 { .. }))
            .count() as u32;
        let dirty_count = tentative_states.len() as u32;
        let motion_fraction = h264_tile_count as f32 / dirty_count.max(1) as f32;

        let cost_enter = tile_codec_cost > h264_cost * self.enter_factor;
        let motion_enter = h264_tile_count >= self.motion_tile_min_absolute
            && motion_fraction > self.motion_tile_threshold;
        let enter_now = cost_enter || motion_enter;
        let exit_now = tile_codec_cost < h264_cost * self.exit_factor;

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
                    let micros_elapsed = now_us.saturating_sub(*started) >= self.enter_sustain_micros;
                    let frames_elapsed = self.state.enter_streak >= self.enter_sustain_frames_min;
                    if micros_elapsed && frames_elapsed {
                        self.state.enter_started_us = None;
                        self.state.enter_streak = 0;
                        return (FrameMode::H264, "cost_comparison");
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
                    let micros_elapsed = now_us.saturating_sub(*started) >= self.exit_sustain_micros;
                    let frames_elapsed = self.state.exit_streak >= self.exit_sustain_frames_min;
                    if micros_elapsed && frames_elapsed {
                        self.state.exit_started_us = None;
                        self.state.exit_streak = 0;
                        return (FrameMode::TileCodec, "cost_comparison");
                    }
                } else {
                    self.state.exit_started_us = None;
                    self.state.exit_streak = 0;
                }
                (FrameMode::H264, "hysteresis_clamp")
            }
        }
    }

    /// Apply per-tile rules across all dirty tiles, then decide whole-frame
    /// mode based on cost + sustained-motion fast-path with hysteresis.
    ///
    /// This is a thin wrapper around [`decide_frame_mode_at`] that reads the
    /// current monotonic time. Production code should call this method. Tests
    /// that need deterministic timing should call `decide_frame_mode_at`
    /// directly with an explicit `now_us`.
    pub fn decide_frame_mode(
        &mut self,
        tentative_states: &[CodecState],
        prev_mode: FrameMode,
    ) -> FrameMode {
        // Monotonic source — `SystemTime` would skew under NTP and freeze
        // hysteresis during the skew window. `Instant::elapsed` is
        // guaranteed non-decreasing on every supported platform.
        let now_us = self.epoch.elapsed().as_micros() as u64;
        self.decide_frame_mode_at(now_us, tentative_states, prev_mode)
    }
}

#[cfg(test)]
#[path = "classifier_decide_tests.rs"]
mod decide_tests;
