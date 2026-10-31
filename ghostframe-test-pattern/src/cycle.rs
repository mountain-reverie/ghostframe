//! `--x11-sink` + `--mode-switch-cycle SECS` mode.
//!
//! Loops painting alternating static/motion content into the root window
//! through [`crate::x11_sink`]: a solid static half, then a motion half that
//! repaints every `motion_tick`.
//!
//! Pixel format: XRGB8888 little-endian, `[B, G, R, X]`.

use std::thread;
use std::time::{Duration, Instant};

use crate::x11_sink::Scanout;

/// Background colour for the static half — packed XRGB8888 little-endian, so
/// in memory bytes are `[B=0xA0, G=0x40, R=0x20, X=0x00]`. Matches the
/// X11-based mode_switch's intent (steel-blue-ish).
const STATIC_BG_PIXEL: u32 = 0x00_20_40_A0;
/// Default motion paint interval. MUST be < capture interval so every captured frame
/// sees a fresh paint and Vulkan SAD reports dirty tiles. With CAPTURE_FPS=30
/// (33 ms), 50 ms paints would leave every-other capture seeing no change
/// (empty dirty → classifier's `enter_streak` resets → never reaches sustain
/// → mode never flips to H264). 16 ms (~60 Hz paint) safely beats any
/// reasonable capture rate.
pub const MOTION_TICK: Duration = Duration::from_millis(16);
/// Band height matches `ghostframe-lib::tile::TILE_SIZE` (32 px) so every
/// motion-band exactly covers one tile row in the capture grid. Kept in sync
/// manually since `ghostframe-test-pattern` has no `ghostframe-lib`
/// dependency.
const BAND_H: u32 = 32;

/// What the motion half paints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Motion {
    /// One flat colour per tile row, recoloured every tick. Every tile is
    /// dirty every frame, and every tile is `Solid`: four bytes each. This
    /// is motion the tile codec is *right* to keep -- a whole screen of it
    /// costs less than one H.264 frame -- so nothing but a network override
    /// should move the classifier off TileCodec while it plays.
    #[default]
    Bands,
    /// A scrolling per-pixel gradient: every tile is dirty every frame and
    /// holds far more than 16 colours, so each classifies as `Cdf53`. A
    /// screenful of those per frame is what H.264 exists for, and the cost
    /// comparison is expected to enter it.
    Video,
}

/// What the static half paints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Static {
    /// One flat colour: every tile is `Solid` and nothing is left to refine.
    #[default]
    Solid,
    /// The `Motion::Video` frame at phase 0, held still: every tile holds
    /// far more than 16 colours, so each classifies as `Cdf53` and the
    /// encoder has refinement passes to send while the half lasts.
    Gradient,
}

/// Run the static/motion paint loop on the root window.
pub fn run(
    half_cycle: Duration,
    static_half: Static,
    motion: Motion,
    motion_tick: Duration,
    motion_region: Option<(u32, u32)>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut scanout = Scanout::connect(None)?;
    // Pre-fill with the static background before the first capture sees any pixels.
    paint_static(&mut scanout, static_half);
    eprintln!("cycle: root painted — entering paint loop");

    let cycle = half_cycle * 2;
    let mut motion_phase: u8 = 0;

    loop {
        let cycle_start = Instant::now();

        // Static half: paint the buffer once.
        paint_static(&mut scanout, static_half);
        thread::sleep(half_cycle);

        // Motion half: rewrite tile-aligned bands of shifting colours every
        // `motion_tick` so the capture grid sees fresh dirty tiles every frame.
        let motion_deadline = cycle_start + cycle;
        while Instant::now() < motion_deadline {
            match motion {
                Motion::Bands => paint_motion(&mut scanout, motion_phase, motion_region),
                Motion::Video => paint_video(&mut scanout, motion_phase),
            }
            motion_phase = motion_phase.wrapping_add(7);
            thread::sleep(motion_tick);
        }
    }
}

fn paint_static(scanout: &mut Scanout, static_half: Static) {
    match static_half {
        Static::Solid => fill_solid(scanout, STATIC_BG_PIXEL),
        Static::Gradient => paint_video(scanout, 0),
    }
}

/// Fill the frame with a single packed-XRGB pixel value.
fn fill_solid(scanout: &mut Scanout, pixel: u32) {
    let row_pitch = scanout.pitch as usize;
    let row_active = (scanout.mode_w as usize) * 4;
    let height = scanout.mode_h as usize;
    let mut map = scanout.map();
    let bytes = map.as_mut();
    let pixel_bytes = pixel.to_le_bytes();
    for y in 0..height {
        let row_start = y * row_pitch;
        let row = &mut bytes[row_start..row_start + row_active];
        for chunk in row.chunks_exact_mut(4) {
            chunk.copy_from_slice(&pixel_bytes);
        }
    }
}

/// Colour of pixel `(x, y)` in the `Motion::Video` frame at `phase`, packed
/// XRGB8888. Red follows x and green follows y, so any 32x32 tile holds 1024
/// distinct colours; all three channels slide with `phase`, so no pixel
/// keeps its value from one frame to the next.
pub fn video_pixel(x: u32, y: u32, phase: u8) -> u32 {
    let p = phase as u32;
    let r = (x + p) & 0xFF;
    let g = (y + 2 * p) & 0xFF;
    let b = ((x + y) / 2 + 3 * p) & 0xFF;
    (r << 16) | (g << 8) | b
}

/// Paint the `Motion::Video` frame for `motion_phase`.
fn paint_video(scanout: &mut Scanout, motion_phase: u8) {
    let row_pitch = scanout.pitch as usize;
    let row_active = (scanout.mode_w as usize) * 4;
    let height = scanout.mode_h;
    let mut map = scanout.map();
    let bytes = map.as_mut();
    for y in 0..height {
        let row_start = (y as usize) * row_pitch;
        let row = &mut bytes[row_start..row_start + row_active];
        for (x, chunk) in row.chunks_exact_mut(4).enumerate() {
            chunk.copy_from_slice(&video_pixel(x as u32, y, motion_phase).to_le_bytes());
        }
    }
}

/// Paint horizontal bands of shifting colours, each `BAND_H` rows tall.
///
/// `region` is a `(tiles_wide, tiles_high)` rectangle at the top left to
/// confine the motion to; `None` repaints the whole frame. A region small
/// enough to fit one frame's send budget lets a test observe every changed
/// tile being sent in the frame that changed it.
fn paint_motion(scanout: &mut Scanout, motion_phase: u8, region: Option<(u32, u32)>) {
    let row_pitch = scanout.pitch as usize;
    let (region_w, region_h) = region.map_or((u32::MAX, u32::MAX), |(w, h)| {
        (w.saturating_mul(BAND_H), h.saturating_mul(BAND_H))
    });
    let row_active = (scanout.mode_w.min(region_w) as usize) * 4;
    let height = scanout.mode_h.min(region_h);
    let mut map = scanout.map();
    let bytes = map.as_mut();
    let mut band_idx: u8 = 0;
    let mut y = 0u32;
    while y < height {
        let r = motion_phase.wrapping_mul(3).wrapping_add(band_idx);
        let g = motion_phase.wrapping_mul(5).wrapping_add(band_idx ^ 0x55);
        let b = motion_phase.wrapping_mul(7).wrapping_add(band_idx ^ 0xAA);
        // XRGB8888 little-endian: bytes [B, G, R, X]
        let pixel = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        let pixel_bytes = pixel.to_le_bytes();

        let band_end = (y + BAND_H).min(height);
        for row in y..band_end {
            let row_start = (row as usize) * row_pitch;
            let row_slice = &mut bytes[row_start..row_start + row_active];
            for chunk in row_slice.chunks_exact_mut(4) {
                chunk.copy_from_slice(&pixel_bytes);
            }
        }

        y += BAND_H;
        band_idx = band_idx.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::video_pixel;
    use std::collections::HashSet;

    /// The two properties the classifier is supposed to react to. Sixteen is
    /// the PalRle palette limit: at or under it a tile is cheap again and the
    /// pattern stops being a reason to enter H.264.
    #[test]
    fn video_motion_gives_every_tile_many_colours_and_changes_every_pixel() {
        for phase in [0u8, 7, 14, 249] {
            for (tx, ty) in [(0u32, 0u32), (31, 23), (59, 33)] {
                let colours: HashSet<u32> = (0..32)
                    .flat_map(|dy| (0..32).map(move |dx| (dx, dy)))
                    .map(|(dx, dy)| video_pixel(tx * 32 + dx, ty * 32 + dy, phase))
                    .collect();
                assert!(colours.len() > 16, "tile ({tx},{ty}) phase {phase}");
            }
            // The paint loop advances the phase by 7 per tick.
            let next = phase.wrapping_add(7);
            for (x, y) in [(0u32, 0u32), (1919, 1079), (640, 400)] {
                assert_ne!(video_pixel(x, y, phase), video_pixel(x, y, next));
            }
        }
    }
}
