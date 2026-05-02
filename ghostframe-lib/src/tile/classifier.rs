//! Per-tile classifier and cost-aware frame-mode decision.
//!
//! See `docs/superpowers/specs/2026-04-28-m3-codec-suite-design.md` Phase M3.0
//! and `docs/specs/ghostframe-initial-spec.md` §4.2 / §4.3.

use super::{CodecState, TILE_BYTES};

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
