//! Per-datagram coverage map: records which tiles each emitted datagram
//! carries so the server can convert datagram-level ACKs from the client
//! into per-tile delivery bookkeeping (Cdf53 ACK counter, PalRle palette
//! delivered tracking, etc.).
//!
//! See `docs/superpowers/specs/2026-06-01-m3.3d-datagram-level-ack-design.md`
//! for the design rationale.

use smallvec::SmallVec;

use crate::transport::protocol::Codec;

/// One tile's payload in a datagram. The server records this at emit time
/// (only for the FINAL fragment of a multi-fragment payload) and consumes
/// it on ACK arrival in `dispatch_ack_datagram`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FragmentCoverage {
    pub tile_x: u8,
    pub tile_y: u8,
    pub generation: u8,
    /// 0 for non-Cdf53 codecs (sentinel; only meaningful for Cdf53).
    pub pass_idx: u8,
    pub codec: Codec,
    /// Some(_) only for PalRle; drives `PaletteTable.delivered` tracking.
    pub palette_id: Option<u8>,
}

/// Inline capacity for the coverage list per (frame_seq, frag_idx) key.
/// Sized for typical bundled-PalRle and single-pass-Cdf53 cases without
/// heap allocation.
pub const COVERAGE_INLINE_CAPACITY: usize = 8;

pub type CoverageList = SmallVec<[FragmentCoverage; COVERAGE_INLINE_CAPACITY]>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_coverage_struct_holds_all_fields() {
        let c = FragmentCoverage {
            tile_x: 5,
            tile_y: 7,
            generation: 3,
            pass_idx: 11,
            codec: Codec::Cdf53,
            palette_id: None,
        };
        assert_eq!(c.tile_x, 5);
        assert_eq!(c.tile_y, 7);
        assert_eq!(c.generation, 3);
        assert_eq!(c.pass_idx, 11);
        assert_eq!(c.codec, Codec::Cdf53);
        assert_eq!(c.palette_id, None);
    }

    #[test]
    fn fragment_coverage_palrle_carries_palette_id() {
        let c = FragmentCoverage {
            tile_x: 0,
            tile_y: 0,
            generation: 1,
            pass_idx: 0,
            codec: Codec::PalRle,
            palette_id: Some(42),
        };
        assert_eq!(c.palette_id, Some(42));
    }
}
