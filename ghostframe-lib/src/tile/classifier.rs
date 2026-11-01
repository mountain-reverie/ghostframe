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
            cdf53_us: 90.0,
            h264_frame_us: 3000.0,
            h264_frame_bytes: 12_000,
            bytes_per_us: 12.5,
        }
    }
}

impl CostModel {
    /// The compiled-in model with any `CostModelConfig` overrides applied.
    /// With `CostModelConfig::default()` this is exactly `CostModel::default()`.
    pub fn from_config(config: &crate::config::CostModelConfig) -> Self {
        let base = Self::default();
        Self {
            solid_us: config.solid_us.unwrap_or(base.solid_us),
            palrle_us: config.palrle_us.unwrap_or(base.palrle_us),
            cdf53_us: config.cdf53_us.unwrap_or(base.cdf53_us),
            h264_frame_us: config.h264_frame_us.unwrap_or(base.h264_frame_us),
            h264_frame_bytes: config.h264_frame_bytes.unwrap_or(base.h264_frame_bytes),
            bytes_per_us: config.bytes_per_us.unwrap_or(base.bytes_per_us),
        }
    }

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
            CodecState::Cdf53 { .. } => self.cdf53_us,
            CodecState::H264 { .. } => self.h264_frame_us,
        }
    }
}

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
    /// `min(cwnd/rtt, recent_emit_bytes/window)`. `0.0` means "no estimate
    /// yet" — `Classifier` falls back to its prior `bytes_per_us`.
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
    /// The bridge's H.264 transport has stalled (frames in flight beyond the
    /// bound, no frame acknowledged for too long, or the client's reset
    /// requests over their rate limit). The only H264 -> tile path from a
    /// network stall: it beats every other override, and H.264 is not
    /// re-entered until a holdoff (starting at [`H264_STALL_REENTRY_US`], doubling per repeated stall) after it clears.
    pub h264_stalled: bool,
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
            h264_stalled: false,
        }
    }
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

/// Per-tile bias added to the H264 comparand when refinement passes are
/// outstanding. Pulls the cost comparison toward TileCodec when there's
/// PixelPerfect work left to deliver and bandwidth headroom permits.
/// Retuned in M3.6c from the bandwidth × scene matrix.
pub(crate) const REFINEMENT_BIAS_PER_TILE_US: f32 = 5.0;

/// Headroom guard: the bandwidth *estimate* below which the classifier
/// forces H264 regardless of the cost comparison, held for
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
    guard: GuardState,
    adaptation_context: AdaptationContext,
    refinement_deficit_tiles: u32,
    /// M3.7a env-var override: when `Some`, `decide_inner` uses this
    /// instead of `REFINEMENT_BIAS_PER_TILE_US`. Populated from
    /// `ClassifierConfig` in `Classifier::new`; `None` unless configured.
    refinement_bias_us_override: Option<f32>,
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
            cost: CostModel::from_config(&config.cost),
            enter_factor: 1.3,
            exit_factor: 0.6,
            motion_tile_threshold: 0.20,
            motion_tile_min_absolute: 8,
            enter_sustain_micros: 50_000,
            exit_sustain_micros: 500_000,
            enter_sustain_frames_min: 3,
            exit_sustain_frames_min: 30,
            state: ClassifierHysteresis::default(),
            guard: GuardState::default(),
            adaptation_context: AdaptationContext::default(),
            refinement_deficit_tiles: 0,
            refinement_bias_us_override: config.refinement_bias_us,
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
    pub fn reset(&mut self) {
        self.state = ClassifierHysteresis::default();
        self.guard = GuardState::default();
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

    /// Number of tiles currently in `Cdf53 { passes_sent < present_passes.count_ones() }`.
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
                h264_stalled = ctx.h264_stalled,
                bwe_estimate_bps = ctx.bwe_estimate_bps,
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
    /// "headroom_guard", "loss_override", "suspension", "cost_comparison",
    /// or "hysteresis_clamp".
    fn decide_inner(
        &mut self,
        now_us: u64,
        tentative_states: &[CodecState],
        prev_mode: FrameMode,
    ) -> (FrameMode, &'static str) {
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
        let per_tile_bias = self
            .refinement_bias_us_override
            .unwrap_or(REFINEMENT_BIAS_PER_TILE_US);
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
                    let micros_elapsed =
                        now_us.saturating_sub(*started) >= self.enter_sustain_micros;
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
                    let micros_elapsed =
                        now_us.saturating_sub(*started) >= self.exit_sustain_micros;
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
