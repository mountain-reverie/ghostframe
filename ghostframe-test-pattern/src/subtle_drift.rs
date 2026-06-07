//! M3.7a subtle-drift mode: paint the configured `tile_pattern` class to
//! the DRM scanout, then every `drift_ms` milliseconds shift the bitmap
//! by 1 pixel along the X axis (wrapping). Generates dirty events for
//! the classifier without changing the perceptual content of pure scenes.

use std::time::Duration;

use drm::control::Device as ControlDevice;

use crate::drm_direct::{msync_buffer, setup_dumb_scanout};
use crate::tile_pattern;

pub fn run(
    card_path: &str,
    class_name: &str,
    drift_ms: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    if drift_ms == 0 {
        // 0 ⇒ disable drift, fall through to the existing static behavior.
        return tile_pattern::run(card_path, class_name);
    }

    let mut scanout = setup_dumb_scanout(card_path)?;
    let width = scanout.mode_w as usize;
    let height = scanout.mode_h as usize;
    let pitch = scanout.pitch as usize;
    eprintln!(
        "subtle-drift: scanout {}x{} active — class {:?}, drift every {}ms",
        width, height, class_name, drift_ms
    );

    // Initial paint.
    {
        let mut map = scanout
            .card
            .map_dumb_buffer(&mut scanout.db)
            .map_err(|e| format!("map_dumb_buffer: {e}"))?;
        let bytes = map.as_mut();
        tile_pattern::fill_with_tile_pattern(
            class_name,
            scanout.mode_w,
            scanout.mode_h,
            pitch,
            bytes,
        );
        msync_buffer(bytes);
    }

    // Drift loop: shift bytes left by 4 (one BGRX pixel) per row, wrapping
    // the leftmost pixel to the right edge. msync each frame.
    loop {
        std::thread::sleep(Duration::from_millis(drift_ms));
        let mut map = scanout
            .card
            .map_dumb_buffer(&mut scanout.db)
            .map_err(|e| format!("map_dumb_buffer: {e}"))?;
        let bytes = map.as_mut();
        for row in 0..height {
            let start = row * pitch;
            let row_bytes = width * 4; // BGRX = 4 bytes per pixel
            if row_bytes < 4 || start + row_bytes > bytes.len() {
                break;
            }
            // Save leftmost pixel, shift, restore at right edge.
            let mut leftmost = [0u8; 4];
            leftmost.copy_from_slice(&bytes[start..start + 4]);
            bytes.copy_within(start + 4..start + row_bytes, start);
            bytes[start + row_bytes - 4..start + row_bytes].copy_from_slice(&leftmost);
        }
        msync_buffer(bytes);
    }
}
