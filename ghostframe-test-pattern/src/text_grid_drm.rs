//! `--text-grid --drm-direct` mode.
//!
//! Paints `text_grid::TEXT` glyphs into a DRM dumb buffer (XRGB8888
//! little-endian), bypassing Xorg. Required for E2E tests under
//! Xorg-on-VKMS where the capture path yields > 16 unique colours per
//! tile and PalRle never wins classification. See M3.2c W1 design notes.

use std::thread;
use std::time::Duration;

use drm::control::Device as ControlDevice;

use crate::drm_direct::{msync_buffer, setup_dumb_scanout};
use crate::font::{pixel_set, GLYPH_H, GLYPH_W};
use crate::text_grid::{char_origin, BG_PIXEL, FG_PIXEL, TEXT};

/// Repaint interval — content is static (text doesn't change frame-to-frame),
/// so 30 Hz is sufficient: each capture sees a freshly-written buffer, the
/// SAD detector reports the tile as unchanged and skip-codec kicks in.
/// drm_direct.rs's MOTION_TICK (16 ms) is for dynamic content where every
/// capture needs new pixels; we don't need that here.
const REPAINT_INTERVAL: Duration = Duration::from_millis(33); // ~30 Hz

pub fn run(card_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut scanout = setup_dumb_scanout(card_path)?;
    eprintln!(
        "text_grid_drm: scanout {}x{} active — entering paint loop",
        scanout.mode_w, scanout.mode_h
    );

    let pitch = scanout.pitch as usize;
    let width = scanout.mode_w;
    let height = scanout.mode_h;

    loop {
        let mut map = scanout
            .card
            .map_dumb_buffer(&mut scanout.db)
            .map_err(|e| format!("map_dumb_buffer: {e}"))?;
        let bytes = map.as_mut();

        // Background fill — pack BG_PIXEL (0x00141414) into every active pixel.
        let bg_bytes = BG_PIXEL.to_le_bytes();
        let row_active = width as usize * 4;
        for y in 0..height as usize {
            let row_start = y * pitch;
            let row = &mut bytes[row_start..row_start + row_active];
            for chunk in row.chunks_exact_mut(4) {
                chunk.copy_from_slice(&bg_bytes);
            }
        }

        // Foreground glyphs.
        let fg_bytes = FG_PIXEL.to_le_bytes();
        for (n, ch) in TEXT.chars().enumerate() {
            let (gx0, gy0) = char_origin(n);
            for py in 0..GLYPH_H {
                for px in 0..GLYPH_W {
                    if pixel_set(ch, px, py) {
                        let ax = (gx0 + px) as usize;
                        let ay = (gy0 + py) as usize;
                        if ax < width as usize && ay < height as usize {
                            let off = ay * pitch + ax * 4;
                            bytes[off..off + 4].copy_from_slice(&fg_bytes);
                        }
                    }
                }
            }
        }

        msync_buffer(bytes);
        drop(map); // explicit drop before sleeping so the kernel sees the writes promptly

        thread::sleep(REPAINT_INTERVAL);
    }
}
