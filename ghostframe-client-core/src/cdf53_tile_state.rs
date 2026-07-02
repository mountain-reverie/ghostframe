//! CPU replacement for `webgpu/cdf53.ts:342-455`: per-tile CDF53 bit-plane
//! accumulation and RGBA reconstruction.
//!
//! ## `decode_passes` contract reconciliation
//!
//! `codec::cdf53::decode_passes` accepts `&[&[u8]]` of *raw RLE-encoded pass
//! payloads* (each with the `[u16 BE len][rle]` × 3-channel framing) and
//! internally RLE-decodes them before accumulating sign/magnitude bits.
//! `PrevalidatedCdf53` (Task 8), however, already stores *decoded* 384-byte
//! bit-planes (RLE already stripped, split into 3×128-byte B/G/R planes).
//!
//! Rather than keep a second, redundant copy of the raw payload bytes
//! alongside the decoded planes (or re-RLE-encode the decoded planes just to
//! satisfy `decode_passes`'s signature), this module reimplements the same
//! sign/magnitude accumulation + SPIHT midpoint-reconstruction algorithm
//! directly over decoded bit-planes in [`reconstruct_coefficients`]. This is
//! the simplest correct option: it operates on the data we actually have
//! (already-decoded planes), needs no extra storage, and is a small,
//! self-contained mirror of `decode_passes`'s core loop (RLE decoding
//! removed since our input is already decoded).

use crate::cdf53_prevalidate::PrevalidatedCdf53;
use ghostframe_protocol::codec::cdf53::{
    inverse, CDF53_CHANNELS, CDF53_COEFFS_PER_CHANNEL, CDF53_PASS_COUNT, CDF53_TOTAL_COEFFS,
};
use std::collections::HashMap;

/// Per-channel bit-plane byte size = 1024 bits / 8 = 128 bytes.
const BIT_PLANE_BYTES_PER_CHANNEL: usize = CDF53_COEFFS_PER_CHANNEL / 8;

#[derive(Clone)]
struct TileEntry {
    generation: u8,
    generation_set: bool,
    /// Decoded 384-byte bit-planes (B,G,R × 128 bytes), one slot per pass.
    planes: [Option<Vec<u8>>; CDF53_PASS_COUNT],
}

impl Default for TileEntry {
    fn default() -> Self {
        TileEntry {
            generation: 0,
            generation_set: false,
            planes: Default::default(),
        }
    }
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

        let coefficients = reconstruct_coefficients(&tile.planes[..prefix_len]);
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

/// Reassemble coefficients from a contiguous prefix of decoded bit-planes
/// (index 0 = sign plane, 1..13 = magnitude planes bit 12 down to bit 0).
/// Mirrors `codec::cdf53::decode_passes`'s accumulation + SPIHT midpoint
/// correction, operating on already-decoded planes instead of RLE payloads.
fn reconstruct_coefficients(planes: &[Option<Vec<u8>>]) -> Vec<i16> {
    let k = planes.len();
    let mut coefficients = vec![0i16; CDF53_TOTAL_COEFFS];
    let mut magnitudes = vec![0u32; CDF53_TOTAL_COEFFS];
    let mut signs = vec![false; CDF53_TOTAL_COEFFS];

    for (pass_idx, plane_opt) in planes.iter().enumerate() {
        let plane = match plane_opt {
            Some(p) => p,
            None => break, // contiguous prefix guarantees this shouldn't happen
        };
        for ch in 0..CDF53_CHANNELS {
            let channel_offset = ch * CDF53_COEFFS_PER_CHANNEL;
            let plane_offset = ch * BIT_PLANE_BYTES_PER_CHANNEL;
            for i in 0..CDF53_COEFFS_PER_CHANNEL {
                let byte = plane[plane_offset + i / 8];
                let bit = (byte >> (i % 8)) & 1;
                if bit != 0 {
                    if pass_idx == 0 {
                        signs[channel_offset + i] = true;
                    } else {
                        let bit_pos = 13 - pass_idx;
                        magnitudes[channel_offset + i] |= 1u32 << bit_pos;
                    }
                }
            }
        }
    }

    // Same midpoint reconstruction rule as decode_passes: unknown low bits
    // of significant coefficients are set to the midpoint of the unknown
    // range rather than 0, for K in [2, CDF53_PASS_COUNT).
    let midpoint: u32 = if k >= 2 && k < CDF53_PASS_COUNT {
        let unknown_bits = CDF53_PASS_COUNT - k;
        1u32 << (unknown_bits - 1)
    } else {
        0
    };

    for i in 0..CDF53_TOTAL_COEFFS {
        let mut mag_u32 = magnitudes[i];
        if mag_u32 != 0 && midpoint != 0 {
            mag_u32 = mag_u32.saturating_add(midpoint);
        }
        let mag = mag_u32.min(i16::MAX as u32) as i16;
        coefficients[i] = if signs[i] { -mag } else { mag };
    }

    coefficients
}
