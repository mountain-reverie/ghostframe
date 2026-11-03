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
use crate::tile_out::{TileOut, TILE_ROW_BYTES};
use ghostframe_protocol::codec::cdf53::{decode_planes, inverse, CDF53_PASS_COUNT};
use std::collections::HashMap;

/// Write a 1024-triple BGR tile (as `inverse()` returns it) into `out` as
/// RGBA, alpha 255. A row is built on the stack and stored in one copy, so a
/// destination in write-combined memory sees whole 128-byte writes.
pub fn write_bgr_rows(bgr: &[u8], out: &mut TileOut<'_>) {
    assert_eq!(bgr.len(), 1024 * 3, "a tile is 1024 BGR triples");
    for y in 0..32 {
        let mut row = [0u8; TILE_ROW_BYTES];
        for (px, src) in row
            .chunks_exact_mut(4)
            .zip(bgr[y * 96..][..96].chunks_exact(3))
        {
            px.copy_from_slice(&[src[2], src[1], src[0], 255]);
        }
        *out.row_mut(y) = row;
    }
}

#[derive(Clone, Default)]
struct TileEntry {
    generation: u8,
    generation_set: bool,
    /// Decoded 384-byte bit-planes (B,G,R × 128 bytes), one slot per pass.
    planes: [Option<Vec<u8>>; CDF53_PASS_COUNT],
    /// The tile's `present_passes` bitmap, learned from pass 0's payload.
    /// `None` until pass 0 arrives. See `integrate`'s doc comment for how
    /// this interacts with the "contiguous prefix" reconstruction rule.
    present_passes: Option<u16>,
    /// Length of the pass prefix last returned as a picture for this
    /// generation, `None` until one is. Lets `integrate` skip arrivals that
    /// cannot change what is on screen.
    drawn_prefix: Option<usize>,
}

impl TileEntry {
    /// Length of the contiguous prefix of resolved passes starting at 0.
    fn prefix_len(&self) -> usize {
        self.planes.iter().take_while(|p| p.is_some()).count()
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

    /// Forget tiles for which `keep(x, y)` is false.
    pub fn retain_tiles(&mut self, keep: impl Fn(u8, u8) -> bool) {
        self.tiles.retain(|&(x, y), _| keep(x, y));
    }

    /// Accumulate one accepted pass for tile `(tile_x, tile_y)`.
    ///
    /// A generation change relative to the tile's currently-held state
    /// discards all previously stored planes for that tile before storing
    /// the new pass (stale-generation rule) — the new generation never
    /// blends with the old one.
    ///
    /// Reconstructs from the contiguous prefix of *resolved* passes starting
    /// at pass 0, where a pass counts as resolved once either its plane has
    /// actually arrived, or the tile's `present_passes` bitmap (known once
    /// pass 0 arrives) says it was never going to be sent -- sparse encoding
    /// skips bit-planes that are all-zero, and an all-zero plane is exactly
    /// what "never sent" should decode as, so treating it as immediately
    /// resolved is lossless. A gap at pass K that IS marked present in the
    /// bitmap (or the bitmap isn't known yet) means passes > K are held but
    /// unused until the gap fills.
    ///
    /// Returns the reconstructed 4096-byte RGBA tile (BGR from `inverse()`
    /// converted to RGB, plus alpha 255) only when there is something new to
    /// draw, `None` otherwise:
    ///
    /// - The prefix must carry picture information: a non-zero coefficient,
    ///   or every pass resolved. Pass 0 holds only signs, and `inverse` has
    ///   no level shift, so a prefix with no magnitude bits reconstructs as
    ///   black. Returning that painted every new version of a tile black
    ///   before its content arrived -- a visible flash on each update. With
    ///   `None` the tile keeps its previous content until the new one has
    ///   substance. A tile that really is black is complete at pass 0 when
    ///   sparse-encoded, so it is still drawn.
    /// - The prefix must have grown since the last picture of this
    ///   generation. A duplicate, or a pass held behind a gap, leaves the
    ///   picture unchanged; uploading it again only costs a tile upload.
    pub fn integrate(
        &mut self,
        tile_x: u8,
        tile_y: u8,
        entry: &PrevalidatedCdf53,
    ) -> Option<Vec<u8>> {
        let bgr = self.integrate_bgr(tile_x, tile_y, entry)?;
        let mut rgba = vec![0u8; 1024 * 4];
        let mut out = TileOut::new(&mut rgba, TILE_ROW_BYTES);
        write_bgr_rows(&bgr, &mut out);
        Some(rgba)
    }

    /// [`Self::integrate`] up to, and not including, the pixel format: the
    /// tile as `inverse()` made it, 1024 BGR triples, for
    /// [`write_bgr_rows`] to put wherever the pixels are wanted.
    ///
    /// [`Self::accumulate`] then [`Self::reconstruct`], so a caller that is
    /// behind can accumulate several passes of a tile and reconstruct once.
    pub fn integrate_bgr(
        &mut self,
        tile_x: u8,
        tile_y: u8,
        entry: &PrevalidatedCdf53,
    ) -> Option<Vec<u8>> {
        if !self.accumulate(tile_x, tile_y, entry) {
            return None;
        }
        self.reconstruct(tile_x, tile_y)
    }

    /// The cheap half of [`Self::integrate_bgr`]: store the pass, apply the
    /// generation rule and the `present_passes` backfill, but run neither
    /// `decode_planes` nor `inverse`. Returns whether the contiguous prefix
    /// differs from the one last drawn, i.e. whether
    /// [`Self::reconstruct`] may have a picture to give. A `true` is not a
    /// promise (the black-flash rule can still decline), a `false` is: the
    /// picture on screen is already this one.
    pub fn accumulate(&mut self, tile_x: u8, tile_y: u8, entry: &PrevalidatedCdf53) -> bool {
        let tile = self.tiles.entry((tile_x, tile_y)).or_default();

        if !tile.generation_set || tile.generation != entry.generation {
            tile.generation = entry.generation;
            tile.generation_set = true;
            tile.planes = Default::default();
            tile.present_passes = None;
            tile.drawn_prefix = None;
        }

        let idx = entry.pass_idx as usize;
        if idx < CDF53_PASS_COUNT {
            tile.planes[idx] = Some(entry.bit_planes.clone());
        }

        // Pass 0 just taught us (or re-confirmed) which passes exist for
        // this generation. Backfill every known-absent slot that hasn't
        // actually received data with an explicit all-zero plane -- the
        // same content an "empty" pass would have produced had it been
        // sent -- so the contiguous-prefix scan below doesn't stall on a
        // pass that is never coming.
        if let Some(present) = entry.present_passes {
            tile.present_passes = Some(present);
            for i in 0..CDF53_PASS_COUNT {
                if present & (1u16 << i) == 0 && tile.planes[i].is_none() {
                    tile.planes[i] = Some(vec![0u8; 384]);
                }
            }
        }

        tile.drawn_prefix != Some(tile.prefix_len())
    }

    /// The expensive half of [`Self::integrate_bgr`]: the picture for what
    /// [`Self::accumulate`] has stored so far, under the rules in
    /// [`Self::integrate`]'s doc comment. `None` when the prefix has not grown
    /// since the last picture, or carries no magnitude bits yet.
    pub fn reconstruct(&mut self, tile_x: u8, tile_y: u8) -> Option<Vec<u8>> {
        let tile = self.tiles.get_mut(&(tile_x, tile_y))?;
        let prefix_len = tile.prefix_len();
        if tile.drawn_prefix == Some(prefix_len) {
            return None;
        }

        let plane_refs: Vec<&[u8]> = tile.planes[..prefix_len]
            .iter()
            .map(|p| p.as_deref().expect("contiguous prefix"))
            .collect();
        let coefficients = decode_planes(&plane_refs);
        if prefix_len < CDF53_PASS_COUNT && coefficients.iter().all(|&c| c == 0) {
            return None;
        }
        tile.drawn_prefix = Some(prefix_len);
        Some(inverse(&coefficients))
    }

    /// Forget one tile's planes, for a new generation that reuses the
    /// 4-bit number the tile holds (which `integrate` cannot tell apart).
    pub fn forget_tile(&mut self, tile_x: u8, tile_y: u8) {
        self.tiles.remove(&(tile_x, tile_y));
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
