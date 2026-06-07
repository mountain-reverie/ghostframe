//! `--solid-per-tile --drm-direct` mode.
//!
//! Paints a fixed scene into a DRM dumb buffer so the classifier sees
//! `unique_colors == 1` per corner tile and picks `CodecState::Solid`,
//! while a central motion region keeps the tile-codec pipeline engaged so
//! the classifier does NOT fall back to the whole-frame H.264 encoder.
//!
//! ## Layout
//!
//! Coordinates use raw scanout pixels (mode_w × mode_h). The four corner
//! 32×32 regions are painted with fixed colours:
//!
//!   * top-left      `RED`     (0xFF0000)
//!   * top-right     `GREEN`   (0x00FF00)
//!   * bottom-left   `BLUE`    (0x0000FF)
//!   * bottom-right  `YELLOW`  (0xFFFF00)
//!
//! Everything else is `BG_PIXEL = 0x141414` (the same near-black used by
//! `text_grid_drm`). A 64×64 motion region centred on the screen cycles
//! colours every ~16ms so capture frames see fresh dirty in the central
//! tiles and the classifier stays in tile-codec mode.
//!
//! ## Pixel format
//!
//! XRGB8888 little-endian — bytes in memory are `[B, G, R, X]`. Use
//! `u32::to_le_bytes()` on a packed `0x00RR_GGBB` literal. The e2e test's
//! `__readPixel()` returns non-premultiplied RGBA8 with alpha forced to
//! 0xFF, so [`samples`] reports `[R, G, B, 0xFF]` per corner.
//!
//! ## Paint scheduling
//!
//! Background + corners are painted ONCE at startup, then never touched
//! again. Corner pixel bytes therefore remain byte-identical frame-to-frame,
//! so the SAD-based dirty detector reports them as unchanged → classifier
//! sees zero motion → enters Solid steady-state. Only the central motion
//! region is repainted per tick (16ms cadence — same constant as
//! `drm_direct::MOTION_TICK`).

use std::thread;
use std::time::{Duration, Instant};

use drm::control::Device as ControlDevice;

use crate::drm_direct::{msync_buffer, setup_dumb_scanout};

/// Dark-grey background. Packed XRGB8888 → bytes `[0x14, 0x14, 0x14, 0x00]`.
/// X (high byte) stays 0x00 to match the rest of the codebase's "X is don't
/// care" convention. The client framebuffer ends up with alpha=0x00 in this
/// channel; canvas `alphaMode: 'opaque'` masks that visually, but exact-pixel
/// assertions must expect alpha=0x00 to match what the production shader
/// actually writes (no alpha-force in the Solid/PalRle pipelines).
const BG_PIXEL: u32 = 0x0014_1414;

/// Corner colours, packed XRGB8888 little-endian.
const RED_PIXEL: u32 = 0x00FF_0000;
const GREEN_PIXEL: u32 = 0x0000_FF00;
const BLUE_PIXEL: u32 = 0x0000_00FF;
const YELLOW_PIXEL: u32 = 0x00FF_FF00;

/// Side length of each corner tile (matches `ghostframe-lib::tile::TILE_SIZE`).
const CORNER_SIZE: u32 = 32;
/// Side length of the central motion region.
const MOTION_SIZE: u32 = 64;
/// Motion-region repaint cadence. MUST stay below capture-frame interval so
/// every captured frame sees fresh bytes in the central tiles — see the
/// note in `drm_direct.rs` for the same constant.
const MOTION_TICK: Duration = Duration::from_millis(16);

/// One classifier-Solid sample location plus the colour the WebGPU client's
/// `__readPixel` is expected to return at that coordinate.
///
/// The expected RGBA matches what the production Solid/PalRle shaders write
/// to the framebuffer: BGRA→RGBA swizzle of the wire bytes, alpha byte
/// passed through unchanged. The test pattern paints with X=0x00, so the
/// alpha channel reads back as 0x00. (Canvas alphaMode='opaque' makes this
/// visually identical to alpha=0xFF.)
pub struct CornerSample {
    pub x: u32,
    pub y: u32,
    pub expected_rgba: [u8; 4],
}

/// Returns the four corner sample positions and their expected RGBA values.
/// The e2e test compares these against `__readPixel(x, y)` on the rendered
/// canvas after the Solid tiles have been transported over WebTransport and
/// decoded by the WebGPU client.
pub fn samples(width: u32, height: u32) -> Vec<CornerSample> {
    let bx = width.saturating_sub(16);
    let by = height.saturating_sub(16);
    vec![
        CornerSample {
            x: 16,
            y: 16,
            expected_rgba: [0xFF, 0x00, 0x00, 0x00],
        }, // top-left RED
        CornerSample {
            x: bx,
            y: 16,
            expected_rgba: [0x00, 0xFF, 0x00, 0x00],
        }, // top-right GREEN
        CornerSample {
            x: 16,
            y: by,
            expected_rgba: [0x00, 0x00, 0xFF, 0x00],
        }, // bottom-left BLUE
        CornerSample {
            x: bx,
            y: by,
            expected_rgba: [0xFF, 0xFF, 0x00, 0x00],
        }, // bottom-right YELLOW
    ]
}

/// Paint the scene once, then loop forever repainting only the central
/// motion region every `MOTION_TICK`.
pub fn run(card_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut scanout = setup_dumb_scanout(card_path)?;
    let width = scanout.mode_w;
    let height = scanout.mode_h;
    let pitch = scanout.pitch as usize;
    eprintln!(
        "solid_per_tile: scanout {}x{} active — painting background + corners once",
        width, height
    );

    // ── One-time paint: background + four corner tiles ──────────────────────
    {
        let mut map = scanout
            .card
            .map_dumb_buffer(&mut scanout.db)
            .map_err(|e| format!("map_dumb_buffer: {e}"))?;
        let bytes = map.as_mut();

        // Background fill.
        let bg_bytes = BG_PIXEL.to_le_bytes();
        let row_active = width as usize * 4;
        for y in 0..height as usize {
            let row_start = y * pitch;
            let row = &mut bytes[row_start..row_start + row_active];
            for chunk in row.chunks_exact_mut(4) {
                chunk.copy_from_slice(&bg_bytes);
            }
        }

        // Corner tiles. Use saturating arithmetic so very-small modes still
        // produce something sensible.
        let corner = CORNER_SIZE.min(width).min(height);
        let tl_x = 0u32;
        let tl_y = 0u32;
        let tr_x = width.saturating_sub(corner);
        let tr_y = 0u32;
        let bl_x = 0u32;
        let bl_y = height.saturating_sub(corner);
        let br_x = width.saturating_sub(corner);
        let br_y = height.saturating_sub(corner);

        fill_rect(
            bytes, pitch, width, height, tl_x, tl_y, corner, corner, RED_PIXEL,
        );
        fill_rect(
            bytes,
            pitch,
            width,
            height,
            tr_x,
            tr_y,
            corner,
            corner,
            GREEN_PIXEL,
        );
        fill_rect(
            bytes, pitch, width, height, bl_x, bl_y, corner, corner, BLUE_PIXEL,
        );
        fill_rect(
            bytes,
            pitch,
            width,
            height,
            br_x,
            br_y,
            corner,
            corner,
            YELLOW_PIXEL,
        );

        msync_buffer(bytes);
        // `map` drops here.
    }

    eprintln!("solid_per_tile: corners painted — entering motion loop");

    // ── Motion loop: only repaint the central 64×64 region ─────────────────
    let motion_size = MOTION_SIZE.min(width).min(height);
    let motion_x = width.saturating_sub(motion_size) / 2;
    let motion_y = height.saturating_sub(motion_size) / 2;
    let start = Instant::now();

    loop {
        {
            let mut map = scanout
                .card
                .map_dumb_buffer(&mut scanout.db)
                .map_err(|e| format!("map_dumb_buffer motion: {e}"))?;
            let bytes = map.as_mut();

            // 2-color flip at 33ms cadence (≈30Hz). Both colours form stable
            // 1-color palettes; the same two palette slots get re-used every
            // cycle, so palette_table.delivered stays set for both across the
            // e2e test's page-reload. Required by e2e_decode_error_thin_uncached
            // (A5/B6) — see
            // docs/superpowers/specs/2026-05-17-decode-error-thin-uncached-design.md.
            const FLIP_RED: u32 = 0x00FF_0000;
            const FLIP_BLUE: u32 = 0x0000_00FF;
            let pixel = if (start.elapsed().as_millis() / 33) % 2 == 0 {
                FLIP_RED
            } else {
                FLIP_BLUE
            };

            fill_rect(
                bytes,
                pitch,
                width,
                height,
                motion_x,
                motion_y,
                motion_size,
                motion_size,
                pixel,
            );

            msync_buffer(bytes);
        }
        thread::sleep(MOTION_TICK);
    }
}

/// Fill a rectangle inside `bytes` with the packed-XRGB pixel value `pixel`.
/// Clips the rectangle to the scanout dimensions; no-op if it lies entirely
/// outside.
fn fill_rect(
    bytes: &mut [u8],
    pitch: usize,
    width: u32,
    height: u32,
    rx: u32,
    ry: u32,
    rw: u32,
    rh: u32,
    pixel: u32,
) {
    let x0 = rx.min(width);
    let y0 = ry.min(height);
    let x1 = rx.saturating_add(rw).min(width);
    let y1 = ry.saturating_add(rh).min(height);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let pixel_bytes = pixel.to_le_bytes();
    for y in y0..y1 {
        let row_start = (y as usize) * pitch + (x0 as usize) * 4;
        let row_end = row_start + ((x1 - x0) as usize) * 4;
        let row = &mut bytes[row_start..row_end];
        for chunk in row.chunks_exact_mut(4) {
            chunk.copy_from_slice(&pixel_bytes);
        }
    }
}
