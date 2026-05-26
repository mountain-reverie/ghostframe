//! `--gradient --drm-direct` mode.
//!
//! Full-screen diagonal RGB gradient. Each 32×32 tile contains >16 unique
//! colors, forcing the classifier into the high-color path (Raw fallback,
//! or Cdf53 when GHOSTFRAME_ENABLE_CDF53 is set).
//!
//! Pixel formula at (x, y):
//!   B = (x * 3) & 0xFF
//!   G = (y * 3) & 0xFF
//!   R = ((x + y) * 2) & 0xFF
//!
//! For any 32×32 tile, this varies across both x and y, yielding hundreds
//! of unique BGR triples — comfortably above the 16-color PalRle threshold.
//!
//! Pixel format: XRGB8888 little-endian — bytes are [B, G, R, X].

use drm::control::Device as ControlDevice;

use crate::drm_direct::{msync_buffer, setup_dumb_scanout};

/// Paint the gradient once, then sleep forever (matches the
/// `solid_per_tile` "paint once, hold scanout" pattern but without
/// the motion loop — Cdf53 emission tests don't need motion).
pub fn run(card_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut scanout = setup_dumb_scanout(card_path)?;
    let width = scanout.mode_w;
    let height = scanout.mode_h;
    let pitch = scanout.pitch as usize;
    eprintln!(
        "gradient: scanout {}x{} active — painting diagonal RGB gradient",
        width, height
    );

    {
        let mut map = scanout
            .card
            .map_dumb_buffer(&mut scanout.db)
            .map_err(|e| format!("map_dumb_buffer: {e}"))?;
        let bytes = map.as_mut();

        let row_active = width as usize * 4;
        for y in 0..height as usize {
            let row_start = y * pitch;
            let row = &mut bytes[row_start..row_start + row_active];
            for (x, chunk) in row.chunks_exact_mut(4).enumerate() {
                let b = (x.wrapping_mul(3)) as u8;
                let g = (y.wrapping_mul(3)) as u8;
                let r = (x.wrapping_add(y).wrapping_mul(2)) as u8;
                chunk[0] = b;
                chunk[1] = g;
                chunk[2] = r;
                chunk[3] = 0xFF;
            }
        }

        msync_buffer(bytes);
        // `map` drops here, returning the buffer to the DRM master.
    }

    eprintln!("gradient: scene painted — holding scanout");

    // Hold the scanout indefinitely so the capture pipeline keeps reading
    // the painted gradient. Match the existing `solid_per_tile` pattern's
    // "loop forever" shape, but without motion — sleep instead of repainting.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}
