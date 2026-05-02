//! Per-tile classifier and cost-aware frame-mode decision.
//!
//! See `docs/superpowers/specs/2026-04-28-m3-codec-suite-design.md` Phase M3.0
//! and `docs/specs/ghostframe-initial-spec.md` §4.2 / §4.3.

use super::{CodecState, TileMetrics, TILE_BYTES};

/// Hardcoded encoding-cost model in microseconds per call, plus a bandwidth
/// weighting expressed as bytes per microsecond. Values rounded from the
/// source spec's GPU compute claims; M3.5 retunes from real measurements.
#[derive(Debug, Clone, Copy)]
pub struct CostModel {
    pub solid_us: f32,         // ~0.5  µs (4-byte memcpy)
    pub palrle_us: f32,        // ~5    µs (nibble-pack 1024 px)
    pub bc1_us: f32,           // ~50   µs (Vulkan compute, dispatched)
    pub cdf53_us: f32,         // ~50   µs (Vulkan compute, dispatched)
    pub h264_frame_us: f32,    // ~3000 µs (VA-API full-frame encode @ 1080p)
    /// Estimated bytes for a full-frame H.264 emission. Conservative default
    /// (1080p, ~6 Mbit/s @ 60 fps → ~12 KB / frame). Updated alongside
    /// `bytes_per_us` when the source spec §6.5 estimator lands (M4+).
    pub h264_frame_bytes: u32,
    /// Bandwidth weighting; placeholder until source spec §6.5 estimator lands.
    /// Default value matches a typical home-LAN link (100 Mbps → ~12.5 B/µs).
    pub bytes_per_us: f32,
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
    /// Estimated emission size in bytes for a tile assigned to `state`.
    pub fn estimated_tile_bytes(&self, state: CodecState) -> u32 {
        match state {
            CodecState::Skip => 0,
            CodecState::Solid => 4,
            CodecState::PalRle { .. } => 200,            // §5.3: typical text 100–200 B; upper bound
            CodecState::Bc1 => 512,
            CodecState::Cdf53 { .. } => 1300,            // §4.4: 1.0–1.3 KB to lossless; upper bound
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
mod cost_tests {
    use super::*;

    #[test]
    fn default_is_self_consistent() {
        let c = CostModel::default();
        // H.264 should dominate per-tile codec costs.
        assert!(c.h264_frame_us > c.solid_us * 1000.0);
        assert!(c.bytes_per_us > 0.0);
    }

    #[test]
    fn skip_and_pixel_perfect_cost_zero() {
        let c = CostModel::default();
        assert_eq!(c.estimated_tile_us(CodecState::Skip), 0.0);
        assert_eq!(c.estimated_tile_us(CodecState::PixelPerfect), 0.0);
        assert_eq!(c.estimated_tile_bytes(CodecState::Skip), 0);
        assert_eq!(c.estimated_tile_bytes(CodecState::PixelPerfect), 0);
    }
}

/// Pure classifier: maps current per-tile metrics + previous codec state to a
/// next codec state. Rules from source spec §4.2 evaluated in order; sentinel
/// fields (`UNIQUE_COLORS_UNKNOWN`, NaN `edge_density`) cause rules consulting
/// them to skip.
///
/// Hysteresis: while in `H264`, stay in H264 unless `change_freq_hz < 5.0`.
/// (Per-tile hysteresis differs from the frame-mode hysteresis in
/// `Classifier::decide_frame_mode`.)
pub fn classify_tile(metrics: &TileMetrics, prev: &CodecState) -> CodecState {
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
        return CodecState::H264 { frames_in_h264: frames };
    }

    // Rule 3: high freq AND low magnitude ⇒ PalRle if few colors known, else BC1
    if freq > 15.0 && mag <= 0.3 {
        if uc_known && metrics.unique_colors <= 16 {
            return CodecState::PalRle { palette_id: 0 };
        }
        return CodecState::Bc1;
    }

    // Rule 4: medium freq AND currently H264 ⇒ stay H264 (per-tile hysteresis)
    if (5.0..=15.0).contains(&freq) {
        if let CodecState::H264 { frames_in_h264 } = prev {
            return CodecState::H264 { frames_in_h264: frames_in_h264.saturating_add(1) };
        }
        return CodecState::Bc1;
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
    CodecState::Cdf53 { passes_sent: 0, max_passes: 9 }
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    fn metrics(freq: f32, mag: f32, idle: u32, unique_colors: u16) -> TileMetrics {
        TileMetrics {
            change_freq_hz: freq,
            change_magnitude: mag,
            idle_frames: idle,
            unique_colors,
            edge_density: super::super::EDGE_DENSITY_UNKNOWN,
            codec_state: CodecState::Skip,
        }
    }

    #[test]
    fn idle_tile_classified_as_skip() {
        let m = metrics(0.0, 0.0, 1, super::super::UNIQUE_COLORS_UNKNOWN);
        assert_eq!(classify_tile(&m, &CodecState::Solid), CodecState::Skip);
    }

    #[test]
    fn high_freq_high_magnitude_picks_h264() {
        let m = metrics(60.0, 0.5, 0, super::super::UNIQUE_COLORS_UNKNOWN);
        let next = classify_tile(&m, &CodecState::Skip);
        assert!(matches!(next, CodecState::H264 { .. }));
    }

    #[test]
    fn h264_state_increments_frames_counter() {
        let m = metrics(60.0, 0.5, 0, super::super::UNIQUE_COLORS_UNKNOWN);
        let next = classify_tile(&m, &CodecState::H264 { frames_in_h264: 7 });
        assert_eq!(next, CodecState::H264 { frames_in_h264: 8 });
    }

    #[test]
    fn high_freq_low_magnitude_few_colors_picks_palrle() {
        let m = metrics(60.0, 0.05, 0, 4);
        assert_eq!(
            classify_tile(&m, &CodecState::Skip),
            CodecState::PalRle { palette_id: 0 },
        );
    }

    #[test]
    fn high_freq_low_magnitude_many_colors_picks_bc1() {
        let m = metrics(60.0, 0.05, 0, 200);
        assert_eq!(classify_tile(&m, &CodecState::Skip), CodecState::Bc1);
    }

    #[test]
    fn high_freq_low_magnitude_sentinel_unique_colors_picks_bc1() {
        let m = metrics(60.0, 0.05, 0, super::super::UNIQUE_COLORS_UNKNOWN);
        assert_eq!(classify_tile(&m, &CodecState::Skip), CodecState::Bc1);
    }

    #[test]
    fn medium_freq_holds_h264_via_hysteresis() {
        let m = metrics(8.0, 0.05, 0, super::super::UNIQUE_COLORS_UNKNOWN);
        let next = classify_tile(&m, &CodecState::H264 { frames_in_h264: 3 });
        assert_eq!(next, CodecState::H264 { frames_in_h264: 4 });
    }

    #[test]
    fn medium_freq_no_prior_h264_picks_bc1() {
        let m = metrics(8.0, 0.05, 0, super::super::UNIQUE_COLORS_UNKNOWN);
        assert_eq!(classify_tile(&m, &CodecState::Skip), CodecState::Bc1);
    }

    #[test]
    fn low_freq_single_color_picks_solid() {
        let m = metrics(2.0, 0.05, 0, 1);
        assert_eq!(classify_tile(&m, &CodecState::Skip), CodecState::Solid);
    }

    #[test]
    fn low_freq_few_colors_picks_palrle() {
        let m = metrics(2.0, 0.05, 0, 8);
        assert_eq!(
            classify_tile(&m, &CodecState::Skip),
            CodecState::PalRle { palette_id: 0 },
        );
    }

    #[test]
    fn low_freq_sentinel_unique_colors_falls_through_to_cdf53() {
        let m = metrics(2.0, 0.05, 0, super::super::UNIQUE_COLORS_UNKNOWN);
        let next = classify_tile(&m, &CodecState::Skip);
        assert!(matches!(next, CodecState::Cdf53 { .. }));
    }
}
