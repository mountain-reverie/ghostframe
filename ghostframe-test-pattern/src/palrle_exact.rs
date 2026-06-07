//! `--palrle-exact --drm-direct` mode.
//!
//! Paint-once scene with four 32×32 PalRle test tiles, each with a
//! distinct 2-color pattern. Drives `e2e_palrle_exact_pixels` (M3.2c
//! B1 follow-up). The four tiles are at tile coords (5,5), (10,5),
//! (5,10), (10,10) — well-separated so dirty detection treats each
//! independently. Background is dark grey (BG_PIXEL = 0x141414).
//!
//! ## Pixel format
//!
//! XRGB8888 little-endian: bytes in memory are `[B, G, R, X]`. Pack
//! via `u32::to_le_bytes()` on `0x00RR_GGBB` literals. X stays 0x00
//! (don't-care) per the project convention — the production
//! Solid/PalRle shaders pass X through to the framebuffer alpha
//! channel unchanged, so `samples()` reports `[R, G, B, 0x00]` per
//! sample point.
//!
//! ## Patterns
//!
//! Each tile uses the same 2-color palette: A = red (0xFF0000),
//! B = blue (0x0000FF). The per-pixel formula varies:
//!
//! | Tile (tile_x, tile_y) | Formula (px, py = 0..31 within tile)            |
//! |-----------------------|--------------------------------------------------|
//! | (5, 5)  checkerboard  | `if (px + py) % 2 == 0 { A } else { B }`         |
//! | (10, 5) hstripes      | `if py % 2 == 0 { A } else { B }`                |
//! | (5, 10) vstripes      | `if px % 2 == 0 { A } else { B }`                |
//! | (10, 10) 2×2 blocks   | `if (px/2 + py/2) % 2 == 0 { A } else { B }`     |

use std::thread;
use std::time::Duration;

use drm::control::Device as ControlDevice;

use crate::drm_direct::{msync_buffer, setup_dumb_scanout};

/// Dark-grey background. Packed XRGB8888 → bytes `[0x14, 0x14, 0x14, 0x00]`.
const BG_PIXEL: u32 = 0x0014_1414;

/// Test palette colour A: pure red. Packed XRGB8888 little-endian.
const COLOR_A: u32 = 0x00FF_0000;

/// Test palette colour B: pure blue. Packed XRGB8888 little-endian.
const COLOR_B: u32 = 0x0000_00FF;

const TILE: u32 = 32;
const T_CHECKER: (u32, u32) = (5, 5);
const T_HSTRIPES: (u32, u32) = (10, 5);
const T_VSTRIPES: (u32, u32) = (5, 10);
const T_BLOCKS2X2: (u32, u32) = (10, 10);

/// One classifier-PalRle sample location plus the expected RGBA value
/// the WebGPU client's `__readPixel` returns at that coordinate.
///
/// Expected RGBA = BGRA→RGBA swizzle of the wire bytes, alpha byte
/// passed through unchanged. The test pattern paints with X=0x00, so
/// the alpha component reads back as 0x00. (Canvas alphaMode='opaque'
/// makes this visually identical to alpha=0xFF.)
pub struct ExactSample {
    pub x: u32,
    pub y: u32,
    pub expected_rgba: [u8; 4],
}

const RGBA_RED: [u8; 4] = [0xFF, 0x00, 0x00, 0x00];
const RGBA_BLUE: [u8; 4] = [0x00, 0x00, 0xFF, 0x00];

/// Returns the 16 sample points the e2e asserts on. 4 samples per
/// tile × 4 tiles. Each tile slot resolves to absolute pixel coords
/// via `tile.x * TILE + px`.
pub fn samples() -> Vec<ExactSample> {
    let (cx, cy) = (T_CHECKER.0 * TILE, T_CHECKER.1 * TILE);
    let (hx, hy) = (T_HSTRIPES.0 * TILE, T_HSTRIPES.1 * TILE);
    let (vx, vy) = (T_VSTRIPES.0 * TILE, T_VSTRIPES.1 * TILE);
    let (bx, by) = (T_BLOCKS2X2.0 * TILE, T_BLOCKS2X2.1 * TILE);
    vec![
        // Checkerboard — (px+py)%2 == 0 → A.
        ExactSample {
            x: cx + 0,
            y: cy + 0,
            expected_rgba: RGBA_RED,
        }, // (0,0) even → A
        ExactSample {
            x: cx + 1,
            y: cy + 0,
            expected_rgba: RGBA_BLUE,
        }, // (1,0) odd  → B
        ExactSample {
            x: cx + 0,
            y: cy + 1,
            expected_rgba: RGBA_BLUE,
        }, // (0,1) odd  → B
        ExactSample {
            x: cx + 1,
            y: cy + 1,
            expected_rgba: RGBA_RED,
        }, // (1,1) even → A
        // Horizontal stripes — py%2 == 0 → A.
        ExactSample {
            x: hx + 0,
            y: hy + 0,
            expected_rgba: RGBA_RED,
        }, // row 0 → A
        ExactSample {
            x: hx + 0,
            y: hy + 1,
            expected_rgba: RGBA_BLUE,
        }, // row 1 → B
        ExactSample {
            x: hx + 31,
            y: hy + 0,
            expected_rgba: RGBA_RED,
        }, // row 0 right end → A
        ExactSample {
            x: hx + 31,
            y: hy + 1,
            expected_rgba: RGBA_BLUE,
        }, // row 1 right end → B
        // Vertical stripes — px%2 == 0 → A.
        ExactSample {
            x: vx + 0,
            y: vy + 0,
            expected_rgba: RGBA_RED,
        }, // col 0 → A
        ExactSample {
            x: vx + 1,
            y: vy + 0,
            expected_rgba: RGBA_BLUE,
        }, // col 1 → B
        ExactSample {
            x: vx + 0,
            y: vy + 31,
            expected_rgba: RGBA_RED,
        }, // col 0 bottom → A
        ExactSample {
            x: vx + 1,
            y: vy + 31,
            expected_rgba: RGBA_BLUE,
        }, // col 1 bottom → B
        // 2×2 blocks — (px/2 + py/2)%2 == 0 → A.
        ExactSample {
            x: bx + 0,
            y: by + 0,
            expected_rgba: RGBA_RED,
        }, // block (0,0) → A
        ExactSample {
            x: bx + 1,
            y: by + 0,
            expected_rgba: RGBA_RED,
        }, // still block (0,0) → A
        ExactSample {
            x: bx + 2,
            y: by + 0,
            expected_rgba: RGBA_BLUE,
        }, // block (1,0) → B
        ExactSample {
            x: bx + 0,
            y: by + 2,
            expected_rgba: RGBA_BLUE,
        }, // block (0,1) → B
    ]
}

/// Paint the scene once, then enter a keepalive loop that re-maps the
/// dumb buffer and msyncs without writing — the pattern is static.
pub fn run(card_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut scanout = setup_dumb_scanout(card_path)?;
    let width = scanout.mode_w;
    let height = scanout.mode_h;
    let pitch = scanout.pitch as usize;
    eprintln!(
        "palrle_exact: scanout {}x{} active — painting four PalRle test tiles",
        width, height
    );

    {
        let mut map = scanout
            .card
            .map_dumb_buffer(&mut scanout.db)
            .map_err(|e| format!("map_dumb_buffer: {e}"))?;
        let bytes = map.as_mut();

        // Background fill — every active pixel set to BG_PIXEL.
        let bg_bytes = BG_PIXEL.to_le_bytes();
        let row_active = width as usize * 4;
        for y in 0..height as usize {
            let row_start = y * pitch;
            let row = &mut bytes[row_start..row_start + row_active];
            for chunk in row.chunks_exact_mut(4) {
                chunk.copy_from_slice(&bg_bytes);
            }
        }

        // Paint each test tile.
        paint_tile(bytes, pitch, width, height, T_CHECKER, |px, py| {
            (px + py) % 2 == 0
        });
        paint_tile(bytes, pitch, width, height, T_HSTRIPES, |_px, py| {
            py % 2 == 0
        });
        paint_tile(bytes, pitch, width, height, T_VSTRIPES, |px, _py| {
            px % 2 == 0
        });
        paint_tile(bytes, pitch, width, height, T_BLOCKS2X2, |px, py| {
            (px / 2 + py / 2) % 2 == 0
        });

        msync_buffer(bytes);
    }

    eprintln!("palrle_exact: scene painted — entering keepalive loop");

    let keepalive = Duration::from_millis(33);
    loop {
        thread::sleep(keepalive);
        let mut map = scanout
            .card
            .map_dumb_buffer(&mut scanout.db)
            .map_err(|e| format!("map_dumb_buffer keepalive: {e}"))?;
        msync_buffer(map.as_mut());
    }
}

/// Paint a single 32×32 test tile at (tile_x*32, tile_y*32). `is_a`
/// returns true when the pixel at offset (px, py) within the tile
/// should be COLOR_A; false → COLOR_B.
fn paint_tile<F: Fn(u32, u32) -> bool>(
    bytes: &mut [u8],
    pitch: usize,
    width: u32,
    height: u32,
    tile: (u32, u32),
    is_a: F,
) {
    let a_bytes = COLOR_A.to_le_bytes();
    let b_bytes = COLOR_B.to_le_bytes();
    let tile_x0 = tile.0 * TILE;
    let tile_y0 = tile.1 * TILE;
    for py in 0..TILE {
        let ay = tile_y0 + py;
        if ay >= height {
            return;
        }
        for px in 0..TILE {
            let ax = tile_x0 + px;
            if ax >= width {
                continue;
            }
            let off = (ay as usize) * pitch + (ax as usize) * 4;
            let pixel = if is_a(px, py) { &a_bytes } else { &b_bytes };
            bytes[off..off + 4].copy_from_slice(pixel);
        }
    }
}
