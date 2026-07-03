//! CPU replacement for `webgpu/cdf53.ts:342-455`: per-tile CDF53 bit-plane
//! accumulation and RGBA reconstruction.
//!
//! ## `decode_passes` contract reconciliation
//!
//! `codec::cdf53::decode_passes` accepts `&[&[u8]]` of *raw RLE-encoded pass
//! payloads* (each with the `[u16 BE len][rle]` × 3-channel framing) and
//! internally RLE-decodes them before delegating to
//! `codec::cdf53::decode_planes` for the sign/magnitude accumulation.
//! `PrevalidatedCdf53` (Task 8), however, already stores *decoded* 384-byte
//! bit-planes (RLE already stripped, split into 3×128-byte B/G/R planes), so
//! this module calls `decode_planes` directly with those buffers rather than
//! re-RLE-encoding them just to satisfy `decode_passes`'s signature.

use crate::cdf53_prevalidate::PrevalidatedCdf53;
use ghostframe_protocol::codec::cdf53::{decode_planes, inverse, CDF53_PASS_COUNT};
use std::collections::HashMap;

#[derive(Clone, Default)]
struct TileEntry {
    generation: u8,
    generation_set: bool,
    /// Decoded 384-byte bit-planes (B,G,R × 128 bytes), one slot per pass.
    planes: [Option<Vec<u8>>; CDF53_PASS_COUNT],
}

/// Per-(tile_x, tile_y) CDF53 progressive-pass accumulator.
pub struct Cdf53TileState {
    tiles: HashMap<(u8, u8), TileEntry>,
}

impl Cdf53TileState {
    pub fn new() -> Self {
        Cdf53TileState {
            tiles: HashMap::new(),
        }
    }

    /// Accumulate one accepted pass for tile `(tile_x, tile_y)`.
    ///
    /// A generation change relative to the tile's currently-held state
    /// discards all previously stored planes for that tile before storing
    /// the new pass (stale-generation rule) — the new generation never
    /// blends with the old one.
    ///
    /// Reconstructs from the contiguous prefix of received passes starting
    /// at pass 0 (a gap at pass K means passes > K are held but unused until
    /// the gap fills). Returns the freshly reconstructed 4096-byte RGBA tile
    /// (BGR from `inverse()` converted to RGB, plus alpha 255).
    pub fn integrate(&mut self, tile_x: u8, tile_y: u8, entry: &PrevalidatedCdf53) -> Vec<u8> {
        let tile = self.tiles.entry((tile_x, tile_y)).or_default();

        if !tile.generation_set || tile.generation != entry.generation {
            tile.generation = entry.generation;
            tile.generation_set = true;
            tile.planes = Default::default();
        }

        let idx = entry.pass_idx as usize;
        if idx < CDF53_PASS_COUNT {
            tile.planes[idx] = Some(entry.bit_planes.clone());
        }

        // Contiguous prefix of received passes starting at 0.
        let mut prefix_len = 0usize;
        while prefix_len < CDF53_PASS_COUNT && tile.planes[prefix_len].is_some() {
            prefix_len += 1;
        }

        let plane_refs: Vec<&[u8]> = tile.planes[..prefix_len]
            .iter()
            .map(|p| p.as_deref().expect("contiguous prefix"))
            .collect();
        let coefficients = decode_planes(&plane_refs);
        let bgr = inverse(&coefficients);

        let mut rgba = vec![0u8; 1024 * 4];
        for px in 0..1024 {
            rgba[px * 4] = bgr[px * 3 + 2]; // R
            rgba[px * 4 + 1] = bgr[px * 3 + 1]; // G
            rgba[px * 4 + 2] = bgr[px * 3]; // B
            rgba[px * 4 + 3] = 255; // A
        }
        rgba
    }

    /// Clear all per-tile accumulation state.
    pub fn reset(&mut self) {
        self.tiles.clear();
    }
}

impl Default for Cdf53TileState {
    fn default() -> Self {
        Self::new()
    }
}
