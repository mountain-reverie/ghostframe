//! Sequential palette-churn pattern for e2e_palette_eviction.
//!
//! Cycles through `count` distinct 4-color palettes, drawing each in a
//! 64×64 region for several frames before moving to the next. Used to
//! exercise the server-side PaletteTable's reuse-and-overwrite logic.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;

pub fn run<C: Connection>(
    conn: &C,
    root: Window,
    count: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let gc = conn.generate_id()?;
    conn.create_gc(gc, root, &CreateGCAux::default())?;

    let frames_per_region = 5;
    let frame_delay = std::time::Duration::from_millis(16);

    // Optional per-iter diagnostic — gated on env var so e2e log output
    // stays quiet by default but can be enabled when chasing a regression.
    // Set GHOSTFRAME_PALCHURN_TRACE=1 to surface per-iter progress + a
    // "completed" marker on clean loop exit.
    let trace_enabled = std::env::var("GHOSTFRAME_PALCHURN_TRACE")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some();

    for n in 0..count {
        let palette = [
            ((n.wrapping_mul(17) & 0xFF) as u8, 0, 0),
            (0, (n.wrapping_mul(31) & 0xFF) as u8, 0),
            (0, 0, (n.wrapping_mul(53) & 0xFF) as u8),
            (255, 255, 255),
        ];
        // [PALCHURN-DIAG] iteration heartbeat — every iter so we can
        // tell from container logs whether the pattern process is alive
        // and how far through `count` it got before any X11 error.
        if trace_enabled {
            eprintln!(
                "[PALCHURN] iter={n} palette=[({},0,0),(0,{},0),(0,0,{}),(255,255,255)]",
                palette[0].0, palette[1].1, palette[2].2
            );
        }

        for _frame in 0..frames_per_region {
            // Paint a 64×64 region at (100, 100) using a 4-color tile pattern.
            for (q, &(r, g, b)) in palette.iter().enumerate() {
                let pixel = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
                conn.change_gc(gc, &ChangeGCAux::new().foreground(pixel))?;
                let dx = (q % 2) as i16;
                let dy = (q / 2) as i16;
                conn.poly_fill_rectangle(
                    root,
                    gc,
                    &[Rectangle {
                        x: 100 + dx * 32,
                        y: 100 + dy * 32,
                        width: 32,
                        height: 32,
                    }],
                )?;
            }
            conn.flush()?;
            std::thread::sleep(frame_delay);
        }

        // Wipe to black to let the dirty tracker mark the region for re-classification.
        conn.change_gc(gc, &ChangeGCAux::new().foreground(0))?;
        conn.poly_fill_rectangle(
            root,
            gc,
            &[Rectangle {
                x: 100,
                y: 100,
                width: 64,
                height: 64,
            }],
        )?;
        conn.flush()?;
        std::thread::sleep(frame_delay);
    }

    // [PALCHURN-DIAG] log clean completion (loop exited normally).
    if trace_enabled {
        eprintln!("[PALCHURN] completed {count} iterations cleanly");
    }

    // Final region stays visible — the test sample-reads pixels here.
    Ok(())
}
