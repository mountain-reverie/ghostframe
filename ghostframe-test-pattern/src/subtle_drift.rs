//! M3.7a subtle-drift mode: paint the configured `tile_pattern` class to
//! the root window, then every `drift_ms` milliseconds shift the bitmap
//! by 1 pixel along the X axis (wrapping). Generates dirty events for
//! the classifier without changing the perceptual content of pure scenes.

use std::time::Duration;

use crate::tile_pattern;
use crate::x11_sink::Scanout;

pub fn run(class_name: &str, drift_ms: u64) -> Result<(), Box<dyn std::error::Error>> {
    run_inner(class_name, drift_ms, false)
}

/// Shift exactly once after `delay_ms`, then leave the screen alone forever.
pub fn run_once(class_name: &str, delay_ms: u64) -> Result<(), Box<dyn std::error::Error>> {
    run_inner(class_name, delay_ms, true)
}

fn run_inner(
    class_name: &str,
    drift_ms: u64,
    once: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if drift_ms == 0 {
        // 0 ⇒ disable drift, fall through to the existing static behavior.
        return tile_pattern::run(class_name);
    }

    let mut scanout = Scanout::connect(None)?;
    let width = scanout.mode_w as usize;
    let height = scanout.mode_h as usize;
    let pitch = scanout.pitch as usize;
    eprintln!(
        "subtle-drift: scanout {}x{} active — class {:?}, drift every {}ms",
        width, height, class_name, drift_ms
    );

    // Initial paint.
    {
        let mut map = scanout.map();
        let bytes = map.as_mut();
        tile_pattern::fill_with_tile_pattern(class_name, width as u32, height as u32, pitch, bytes);
    }

    // `drift_ms` with `once` set means: wait, shift exactly once, then stop
    // touching the buffer. That is the "a dialog opened on a settled desktop"
    // shape -- one burst of dirty tiles followed by silence -- which neither
    // a never-changing screen nor a never-stopping drift can produce.
    //
    // Drift loop: shift bytes left by 4 (one BGRX pixel) per row, wrapping
    // the leftmost pixel to the right edge; each frame is drawn into root
    // when `map` drops.
    loop {
        std::thread::sleep(Duration::from_millis(drift_ms));
        let mut map = scanout.map();
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

        // The frame is drawn into root when `map` drops, so drop it before
        // the quiet forever below, or the one burst is never drawn.
        drop(map);
        if once {
            eprintln!("subtle-drift: single burst applied, screen is now quiet for good");
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
    }
}
