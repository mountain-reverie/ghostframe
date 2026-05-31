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

        // Static half: three horizontal stripes that exercise different
        // classifier rules. Repaints once per cycle, then sleeps.
        //
        //   STRIPE 1 (top third):    solid steel-blue (#204080)  → CodecState::Solid
        //   STRIPE 2 (middle third): 8-color vertical bars       → CodecState::PalRle
        //   STRIPE 3 (bottom third): diagonal RGB gradient       → CodecState::Cdf53
        //
        // Each stripe height is rounded down to a multiple of TILE_SIZE
        // (BAND_H=32 px) so stripes line up on tile boundaries — no
        // straddling tiles classify ambiguously.
        let stripe_h = (height / 3 / BAND_H as u32) * BAND_H as u32;
        let stripe1_y = 0i16;
        let stripe2_y = stripe_h as i16;
        let stripe3_y = (stripe_h * 2) as i16;
        let stripe3_h = (height - stripe_h * 2) as u32;

        // Stripe 1 — solid.
        conn.change_gc(
            gc,
            &ChangeGCAux::default().foreground(STATIC_BG_PIXEL),
        )?;
        conn.poly_fill_rectangle(
            root,
            gc,
            &[Rectangle { x: 0, y: stripe1_y, width: width as u16, height: stripe_h as u16 }],
        )?;

        // Stripe 2 — 8 vertical bars of distinct colors (≤16 colors → PalRle).
        const PALRLE_COLORS: [u32; 8] = [
            0x00_00_00_00, // black
            0x00_FF_FF_FF, // white
            0x00_FF_00_00, // red
            0x00_00_FF_00, // green
            0x00_00_00_FF, // blue
            0x00_FF_FF_00, // yellow
            0x00_FF_00_FF, // magenta
            0x00_00_FF_FF, // cyan
        ];
        let bar_w = (width / PALRLE_COLORS.len() as u32) as u16;
        for (i, color) in PALRLE_COLORS.iter().enumerate() {
            conn.change_gc(gc, &ChangeGCAux::default().foreground(*color))?;
            conn.poly_fill_rectangle(
                root,
                gc,
                &[Rectangle {
                    x: (i as u16 * bar_w) as i16,
                    y: stripe2_y,
                    width: bar_w,
                    height: stripe_h as u16,
                }],
            )?;
        }

        // Stripe 3 — diagonal gradient (high-color → Cdf53). Per-row solid
        // colors approximate the gradient cheaply via X11 (no put_image
        // needed since exact per-pixel values aren't required for the test;
        // the classifier just needs > 16 colors).
        for row in 0..stripe3_h {
            let py = stripe3_y as u32 + row;
            let r = (py.wrapping_mul(3) & 0xFF) as u32;
            let g = (py.wrapping_mul(5) & 0xFF) as u32;
            let b = (py.wrapping_mul(7) & 0xFF) as u32;
            let color = (r << 16) | (g << 8) | b;
            conn.change_gc(gc, &ChangeGCAux::default().foreground(color))?;
            conn.poly_fill_rectangle(
                root,
                gc,
                &[Rectangle {
                    x: 0,
                    y: py as i16,
                    width: width as u16,
                    height: 1,
                }],
            )?;
        }

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
                conn.poly_fill_rectangle(
                    root,
                    gc,
                    &[Rectangle {
                        x: 0,
                        y: y as i16,
                        width: width as u16,
                        height: BAND_H,
                    }],
                )?;
            }
            conn.flush()?;
            motion_phase = motion_phase.wrapping_add(7);
            thread::sleep(MOTION_TICK);
        }
    }
}
