//! `--lossless-golden --drm-direct` mode.
//!
//! Fully static, fully deterministic codec-mixed test frame designed for
//! whole-frame strict pixel compare in `e2e_lossless_golden_png`.
//!
//! Unlike `solid_per_tile`, which animates a central 64×64 region so the
//! classifier stays in tile-codec mode (and forces the e2e to compare only
//! the four 32×32 corners), this pattern is **100% static** so the e2e
//! can do whole-frame byte-compare against the same `frame_rgba()`
//! generator the server paints. The classifier is bypassed in the e2e
//! via `GHOSTFRAME_FORCE_TILECODEC=1`, so the lack of motion doesn't
//! cause an H.264 fallback.
//!
//! ## Layout
//!
//! Three vertical regions sized in thirds of the scanout width, each
//! snapped down to a 32-pixel boundary so every 32×32 tile lies entirely
//! within one region:
//!
//!   * Left third       — every tile is a single colour (unique_colors=1)
//!                        → classifier picks `CodecState::Solid`.
//!   * Middle third     — every tile draws from a 4-colour sub-palette
//!                        (unique_colors ∈ [2,16])
//!                        → classifier picks `CodecState::PalRle`.
//!   * Right third      — high-entropy gradient (unique_colors > 16)
//!                        → classifier picks `CodecState::Cdf53`.
//!
//! The point is *codec coverage*, not visual appeal. As long as every
//! tile is classified into the intended codec and the bytes are
//! deterministic across runs, the pattern serves its job — it lets
//! `e2e_lossless_golden_png` strict-compare every pixel against
//! `frame_rgba()` and catch first-paint loss on any of the three codec
//! paths.
//!
//! ## Pixel format
//!
//! Return buffer from [`frame_rgba`] is RGBA8 in scanline order
//! (`[R, G, B, A, R, G, B, A, …]`). The alpha byte is forced to
//! `0x00` to match what the WebGPU renderer actually writes to the
//! framebuffer: the DRM scanout is XRGB8888 with `X = 0x00`, and the
//! Solid/PalRle/Cdf53 shaders pass that byte through unchanged to the
//! alpha channel. (Canvas `alphaMode: 'opaque'` masks this visually,
//! but exact-pixel byte-compare must expect alpha = 0x00 — same
//! convention as `solid_per_tile::samples()`.)
//!
//! For DRM scanout (XRGB8888 little-endian, bytes `[B, G, R, X]`) the
//! [`run`] function does the RGBA→XRGB byte swap when copying into the
//! dumb buffer (X stays 0x00).
//!
//! ## Determinism contract
//!
//! `frame_rgba(w, h)` is a pure function of `(w, h)`. Same inputs →
//! identical output bytes, every call, every process, every platform.
//! Three unit tests in this module lock the per-region codec-class
//! property: if a future change breaks the partitioning, the tests
//! fail before the e2e even runs.

use drm::control::Device as ControlDevice;

use crate::drm_direct::{msync_buffer, setup_dumb_scanout};

/// Tile size — matches `ghostframe-lib::tile::TILE_SIZE`. Region
/// boundaries are snapped to multiples of this so every tile lies
/// entirely within one codec zone.
pub const TILE_SIZE: u32 = 32;

/// Side length of each Solid-region block. Must be a multiple of
/// `TILE_SIZE` so each block covers a whole number of tiles.
const SOLID_BLOCK: u32 = 64;

/// Four Solid-region colours (RGBA), arranged checkerboard-style on a
/// `SOLID_BLOCK` grid. All distinct so the four block-types render
/// differently; alpha forced to 0xFF.
const SOLID_PALETTE: [[u8; 4]; 4] = [
    [0xFF, 0x00, 0x00, 0x00], // red
    [0x00, 0xFF, 0x00, 0x00], // green
    [0x00, 0x00, 0xFF, 0x00], // blue
    [0xFF, 0xFF, 0x00, 0x00], // yellow
];

/// 16-colour global palette for the PalRle middle region. Each tile
/// uses 4 colours sub-selected by `(tile_x, tile_y)` so:
///   - unique_colors per tile is exactly 4 (∈ [2, 16] → Rule 7 PalRle)
///   - the overall middle region cycles through all 16 palette colours
const PALRLE_PALETTE: [[u8; 4]; 16] = [
    [0x00, 0x00, 0x00, 0x00],
    [0x10, 0x10, 0x10, 0x00],
    [0x20, 0x20, 0x20, 0x00],
    [0x30, 0x30, 0x30, 0x00],
    [0xFF, 0x40, 0x40, 0x00],
    [0x40, 0xFF, 0x40, 0x00],
    [0x40, 0x40, 0xFF, 0x00],
    [0xFF, 0xFF, 0x40, 0x00],
    [0xFF, 0x40, 0xFF, 0x00],
    [0x40, 0xFF, 0xFF, 0x00],
    [0x80, 0x80, 0x80, 0x00],
    [0xC0, 0xC0, 0xC0, 0x00],
    [0xFF, 0x80, 0x00, 0x00],
    [0x00, 0x80, 0xFF, 0x00],
    [0x80, 0x00, 0xFF, 0x00],
    [0xFF, 0xFF, 0xFF, 0x00],
];

/// Snap `v` down to a multiple of [`TILE_SIZE`]. Used to align region
/// boundaries so no tile straddles two codec zones.
fn snap_tile(v: u32) -> u32 {
    v & !(TILE_SIZE - 1)
}

/// Compute the x-coordinate where the Solid region ends / PalRle region
/// begins. Snapped to [`TILE_SIZE`].
pub fn region_boundary_solid_palrle(width: u32) -> u32 {
    snap_tile(width / 3)
}

/// Compute the x-coordinate where the PalRle region ends / Cdf53 region
/// begins. Snapped to [`TILE_SIZE`].
pub fn region_boundary_palrle_cdf53(width: u32) -> u32 {
    snap_tile((width * 2) / 3)
}

/// Generate the full deterministic codec-mixed test frame as
/// `width * height * 4` bytes of RGBA8 in scanline order.
///
/// **Pure function** of `(width, height)`: same inputs → identical bytes
/// every call. The e2e test calls this server-side at build time and
/// compares against the rendered canvas readback byte-by-byte.
pub fn frame_rgba(width: u32, height: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];
    let boundary_left = region_boundary_solid_palrle(width);
    let boundary_right = region_boundary_palrle_cdf53(width);

    for y in 0..height {
        for x in 0..width {
            let off = ((y * width + x) * 4) as usize;
            let rgba = if x < boundary_left {
                // ── Left third: Solid region ────────────────────────
                // Pick one of 4 colours by SOLID_BLOCK checkerboard.
                let bx = x / SOLID_BLOCK;
                let by = y / SOLID_BLOCK;
                // Two-bit index from (bx parity, by parity) → 4 colours
                // arranged in a 2×2 super-grid that visually checkers.
                let idx = ((bx & 1) | ((by & 1) << 1)) as usize;
                SOLID_PALETTE[idx]
            } else if x < boundary_right {
                // ── Middle third: PalRle region ─────────────────────
                // Pick a 4-colour sub-palette per tile, then choose one
                // of the 4 by a 2×2 sub-block pattern inside the tile.
                let tx = x / TILE_SIZE;
                let ty = y / TILE_SIZE;
                // Deterministic 4-colour sub-palette indices for this
                // tile. (tx * 5 + ty * 3) mixes coords without going
                // anywhere near needing >16 colours per tile.
                let base = ((tx.wrapping_mul(5)).wrapping_add(ty.wrapping_mul(3))) as usize;
                let pal_idx = [
                    base % 16,
                    (base + 4) % 16,
                    (base + 8) % 16,
                    (base + 12) % 16,
                ];
                // Sub-tile position (0..32) → which of 4 colours.
                let sub_x = (x % TILE_SIZE) / (TILE_SIZE / 2);
                let sub_y = (y % TILE_SIZE) / (TILE_SIZE / 2);
                let which = (sub_x | (sub_y << 1)) as usize;
                PALRLE_PALETTE[pal_idx[which]]
            } else {
                // ── Right third: Cdf53 region ───────────────────────
                // High-entropy per-pixel gradient — every 32×32 tile
                // has well over 16 unique colours (verified by the
                // `lossless_golden_cdf53_region_yields_cdf53_tiles`
                // unit test below).
                let r = ((x.wrapping_mul(17)).wrapping_add(y.wrapping_mul(31))) as u8;
                let g = ((x.wrapping_mul(13)).wrapping_add(y.wrapping_mul(7))) as u8;
                let b = ((x.wrapping_mul(29)).wrapping_add(y.wrapping_mul(11))) as u8;
                [r, g, b, 0x00]
            };
            buf[off] = rgba[0];
            buf[off + 1] = rgba[1];
            buf[off + 2] = rgba[2];
            buf[off + 3] = rgba[3];
        }
    }
    buf
}

/// Paint the lossless-golden frame once into the DRM dumb buffer,
/// then hold the scanout indefinitely.
///
/// The frame is **fully static** — there is no motion loop. The e2e
/// test relies on `GHOSTFRAME_FORCE_TILECODEC=1` to bypass the
/// classifier's H.264 forced-start so the cdf53 path actually runs.
pub fn run(card_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut scanout = setup_dumb_scanout(card_path)?;
    let width = scanout.mode_w;
    let height = scanout.mode_h;
    let pitch = scanout.pitch as usize;
    eprintln!(
        "lossless_golden: scanout {}x{} active — painting static codec-mixed frame",
        width, height
    );

    // Generate the canonical RGBA frame, then copy into the XRGB8888
    // dumb buffer with the R/B swap that XRGB requires.
    let rgba = frame_rgba(width, height);

    {
        let mut map = scanout
            .card
            .map_dumb_buffer(&mut scanout.db)
            .map_err(|e| format!("map_dumb_buffer: {e}"))?;
        let bytes = map.as_mut();
        let row_active = width as usize * 4;
        for y in 0..height as usize {
            let row_start = y * pitch;
            let dst_row = &mut bytes[row_start..row_start + row_active];
            let src_row_start = y * width as usize * 4;
            let src_row = &rgba[src_row_start..src_row_start + row_active];
            // RGBA (source) → XRGB little-endian = bytes [B, G, R, X].
            for (dst, src) in dst_row.chunks_exact_mut(4).zip(src_row.chunks_exact(4)) {
                dst[0] = src[2]; // B
                dst[1] = src[1]; // G
                dst[2] = src[0]; // R
                dst[3] = 0x00; // X don't-care
            }
        }
        msync_buffer(bytes);
    }

    let solid_w = region_boundary_solid_palrle(width);
    let palrle_w = region_boundary_palrle_cdf53(width) - solid_w;
    let cdf53_w = width - region_boundary_palrle_cdf53(width);
    eprintln!(
        "lossless_golden: regions painted — Solid={}px / PalRle={}px / Cdf53={}px (snapped to {}-px tiles)",
        solid_w, palrle_w, cdf53_w, TILE_SIZE
    );
    eprintln!("lossless_golden: holding scanout");

    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Default test dimensions — matches the canonical 1920×1080
    /// scanout most VKMS configurations report. Boundary math:
    /// 1920/3 = 640 (already 32-aligned), 1920*2/3 = 1280 (32-aligned).
    const W: u32 = 1920;
    const H: u32 = 1080;

    /// Count unique RGBA tuples inside the 32×32 tile whose top-left
    /// corner is `(tile_px, tile_py)`. Helper for the codec-region
    /// classification checks.
    fn count_unique_in_tile(buf: &[u8], width: u32, tile_px: u32, tile_py: u32) -> usize {
        let mut s: HashSet<[u8; 4]> = HashSet::new();
        for yy in 0..TILE_SIZE {
            for xx in 0..TILE_SIZE {
                let off = (((tile_py + yy) * width + (tile_px + xx)) * 4) as usize;
                s.insert([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
            }
        }
        s.len()
    }

    #[test]
    fn lossless_golden_is_deterministic() {
        let a = frame_rgba(W, H);
        let b = frame_rgba(W, H);
        assert_eq!(a.len(), (W * H * 4) as usize);
        assert_eq!(a, b, "frame_rgba is not deterministic across calls");
    }

    #[test]
    fn lossless_golden_is_deterministic_off_dim() {
        // Non-canonical dimensions — still pure.
        let a = frame_rgba(1024, 768);
        let b = frame_rgba(1024, 768);
        assert_eq!(a, b);
    }

    #[test]
    fn lossless_golden_solid_region_yields_solid_tiles() {
        let f = frame_rgba(W, H);
        let boundary = region_boundary_solid_palrle(W);
        // Iterate every 32×32 tile that lies entirely inside the Solid
        // region (tile_x * 32 + 32 ≤ boundary).
        let tiles_x = boundary / TILE_SIZE;
        let tiles_y = H / TILE_SIZE;
        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                let n = count_unique_in_tile(&f, W, tx * TILE_SIZE, ty * TILE_SIZE);
                assert_eq!(
                    n,
                    1,
                    "Solid tile ({}, {}) at px ({}, {}) has {} unique colours \
                     — classifier would NOT pick CodecState::Solid",
                    tx,
                    ty,
                    tx * TILE_SIZE,
                    ty * TILE_SIZE,
                    n
                );
            }
        }
    }

    #[test]
    fn lossless_golden_palrle_region_yields_palrle_tiles() {
        let f = frame_rgba(W, H);
        let bx0 = region_boundary_solid_palrle(W);
        let bx1 = region_boundary_palrle_cdf53(W);
        let tx0 = bx0 / TILE_SIZE;
        let tx1 = bx1 / TILE_SIZE;
        let tiles_y = H / TILE_SIZE;
        for ty in 0..tiles_y {
            for tx in tx0..tx1 {
                let n = count_unique_in_tile(&f, W, tx * TILE_SIZE, ty * TILE_SIZE);
                assert!(
                    (2..=16).contains(&n),
                    "PalRle tile ({}, {}) at px ({}, {}) has {} unique colours \
                     — classifier would NOT pick CodecState::PalRle (needs ∈ [2,16])",
                    tx,
                    ty,
                    tx * TILE_SIZE,
                    ty * TILE_SIZE,
                    n
                );
            }
        }
    }

    #[test]
    fn lossless_golden_cdf53_region_yields_cdf53_tiles() {
        let f = frame_rgba(W, H);
        let bx1 = region_boundary_palrle_cdf53(W);
        let tx0 = bx1 / TILE_SIZE;
        let tx_end = W / TILE_SIZE;
        let tiles_y = H / TILE_SIZE;
        for ty in 0..tiles_y {
            for tx in tx0..tx_end {
                let n = count_unique_in_tile(&f, W, tx * TILE_SIZE, ty * TILE_SIZE);
                assert!(
                    n > 16,
                    "Cdf53 tile ({}, {}) at px ({}, {}) has {} unique colours \
                     — classifier would NOT pick CodecState::Cdf53 (needs > 16)",
                    tx,
                    ty,
                    tx * TILE_SIZE,
                    ty * TILE_SIZE,
                    n
                );
            }
        }
    }

    #[test]
    fn lossless_golden_region_boundaries_are_tile_aligned() {
        // Regression: a boundary not divisible by TILE_SIZE means tiles
        // straddle two codec zones, breaking the per-region classifier
        // property the e2e test depends on.
        assert_eq!(region_boundary_solid_palrle(W) % TILE_SIZE, 0);
        assert_eq!(region_boundary_palrle_cdf53(W) % TILE_SIZE, 0);
        // Off-canonical width too.
        assert_eq!(region_boundary_solid_palrle(1024) % TILE_SIZE, 0);
        assert_eq!(region_boundary_palrle_cdf53(1024) % TILE_SIZE, 0);
    }
}
