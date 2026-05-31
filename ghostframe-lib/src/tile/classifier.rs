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
        Self {
            solid_us: 0.5,
            palrle_us: 5.0,
            bc1_us: 50.0,
            cdf53_us: 50.0,
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

/// Hysteresis state for `Classifier::decide_frame_mode`.
#[derive(Debug, Clone, Copy, Default)]
struct ClassifierHysteresis {
    /// Frames in a row the enter condition has held (cost OR motion fast-path).
    enter_streak: u32,
    /// Frames in a row the exit condition has held (cost-only).
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
    pub enter_sustain_frames: u32,     // default 3
    pub exit_sustain_frames: u32,      // default 30
    state: ClassifierHysteresis,
}

impl Default for Classifier {
    fn default() -> Self {
        Self {
            cost: CostModel::default(),
            enter_factor: 1.3,
            exit_factor: 0.6,
            motion_tile_threshold: 0.20,
            motion_tile_min_absolute: 8,
            enter_sustain_frames: 3,
            exit_sustain_frames: 30,
            state: ClassifierHysteresis::default(),
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

    /// Apply per-tile rules across all dirty tiles, then decide whole-frame
    /// mode based on cost + sustained-motion fast-path with hysteresis.
    ///
    /// `tentative_states` is the per-dirty-tile output of `classify_tile()`.
    /// `prev_mode` is what the previous frame emitted.
    ///
    /// Empty `tentative_states` is valid — it represents a frame with no dirty
    /// tiles. In H264 mode, this contributes toward the exit-sustain counter
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
    pub fn decide_frame_mode(
        &mut self,
        tentative_states: &[CodecState],
        prev_mode: FrameMode,
    ) -> FrameMode {
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
        let h264_cost =
            self.cost.h264_frame_us + (self.cost.h264_frame_bytes as f32) / bytes_per_us;

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

        match prev_mode {
            FrameMode::TileCodec => {
                if enter_now {
                    self.state.enter_streak = self.state.enter_streak.saturating_add(1);
                    self.state.exit_streak = 0;
                    if self.state.enter_streak >= self.enter_sustain_frames {
                        self.state.enter_streak = 0;
                        return FrameMode::H264;
                    }
                } else {
                    self.state.enter_streak = 0;
                }
                FrameMode::TileCodec
            }
            FrameMode::H264 => {
                if exit_now {
                    self.state.exit_streak = self.state.exit_streak.saturating_add(1);
                    self.state.enter_streak = 0;
                    if self.state.exit_streak >= self.exit_sustain_frames {
                        self.state.exit_streak = 0;
                        return FrameMode::TileCodec;
                    }
                } else {
                    self.state.exit_streak = 0;
                }
                FrameMode::H264
            }
        }
    }
}

#[cfg(test)]
#[path = "classifier_decide_tests.rs"]
mod decide_tests;
