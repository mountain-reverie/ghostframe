//! `--mode-switch-cycle SECS` pattern.
//!
//! Alternates between two halves of a `2 × SECS`-second cycle:
//!   - Static half: fill the root window with a fixed solid colour, stay still.
//!   - Motion half: redraw the entire root window with shifting noise every
//!     ~50 ms so every tile becomes dirty every frame.
//!
//! The e2e test polls per-mode datagram counts at known offsets within the
//! cycle to confirm the classifier moved into H264 mode during motion and back
//! to TileCodec during static.

use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;

/// Background colour for the static half — dark steel-blue (#204080), format: 00RRGGBB.
const STATIC_BG_PIXEL: u32 = 0x00_20_40_80;
const MOTION_TICK: Duration = Duration::from_millis(50);
/// Band height for motion drawing. Matches `ghostframe-lib::tile::TILE_SIZE` (32 px) so
/// every motion-band exactly covers one tile row in the capture grid. Kept in sync
/// manually since `ghostframe-test-pattern` has no `ghostframe-lib` dependency.
const BAND_H: u16 = 32;

pub fn run<C: Connection>(
    conn: &C,
    root: u32,
    half_cycle: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let geom = conn.get_geometry(root)?.reply()?;
    let width = geom.width as u32;
    let height = geom.height as u32;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, root, &CreateGCAux::default())?;

    let mut motion_phase: u8 = 0;
    let cycle = half_cycle * 2;

    loop {
        let cycle_start = Instant::now();

        // Static half: paint solid background once and idle.
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().background_pixel(STATIC_BG_PIXEL),
        )?;
        conn.clear_area(false, root, 0, 0, 0, 0)?;
        conn.flush()?;

        thread::sleep(half_cycle);

        // Motion half: tile the screen with shifting colour bands every tick.
        let motion_deadline = cycle_start + cycle;
        while Instant::now() < motion_deadline {
            for (band_idx, y) in (0..height as u16).step_by(BAND_H as usize).enumerate() {
                let idx = band_idx as u8;
                let r = motion_phase.wrapping_mul(3).wrapping_add(idx);
                let g = motion_phase.wrapping_mul(5).wrapping_add(idx ^ 0x55);
                let b = motion_phase.wrapping_mul(7).wrapping_add(idx ^ 0xAA);
                let pixel = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
                conn.change_gc(gc, &ChangeGCAux::new().foreground(pixel))?;
                conn.poly_fill_rectangle(root, gc, &[Rectangle {
                    x: 0, y: y as i16, width: width as u16, height: BAND_H,
                }])?;
            }
            conn.flush()?;
            motion_phase = motion_phase.wrapping_add(7);
            thread::sleep(MOTION_TICK);
        }
    }
}
