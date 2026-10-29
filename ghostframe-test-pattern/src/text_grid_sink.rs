//! `--text-grid --x11-sink` mode.
//!
//! Paints `text_grid::TEXT` glyphs pixel-exactly into the root window
//! through [`crate::x11_sink`]. The X11 `--text-grid` path draws with a core
//! font instead, whose rendering does not match `font.rs` bit for bit; the
//! e2e tests that assert exact glyph pixels need this one.
//!
//! The text is static: it is painted once, in one frame, so a capture never
//! sees the background without the glyphs.

use std::thread;
use std::time::Duration;

use crate::font::{pixel_set, GLYPH_H, GLYPH_W};
use crate::text_grid::{char_origin, BG_PIXEL, FG_PIXEL, TEXT};
use crate::x11_sink::Scanout;

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut scanout = Scanout::connect(None)?;
    eprintln!(
        "text_grid_sink: scanout {}x{} active — painting once then entering keepalive loop",
        scanout.mode_w, scanout.mode_h
    );

    let pitch = scanout.pitch as usize;
    let width = scanout.mode_w;
    let height = scanout.mode_h;

    // ── One-time paint ──────────────────────────────────────────────────────
    {
        let mut map = scanout.map();
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
        // `map` drops here, which draws the frame into root.
    }

    eprintln!("text_grid_sink: initial paint complete — entering keepalive loop");

    // The root window keeps what was drawn; nothing to refresh.
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
