//! X11 test pattern app for E2E tests.
//!
//! Draws deterministic color patterns on the X11 root window so the E2E test
//! can verify pixel-level correctness of the capture + transport pipeline.
//!
//! When `--solid-red` is used, sets the root window background to red and
//! clears it.  This guarantees `XGetImage` on the root returns red pixels
//! regardless of compositor/WM presence.

use clap::Parser;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;

#[derive(Parser)]
struct Args {
    /// Fill the root window with solid red.
    #[arg(long)]
    solid_red: bool,

    /// Draw a cycling-color region that changes every frame (simulates motion).
    #[arg(long)]
    spinner: bool,

    /// (Ignored — kept for CLI compat.) Window width.
    #[arg(long, default_value = "640")]
    width: u16,

    /// (Ignored — kept for CLI compat.) Window height.
    #[arg(long, default_value = "480")]
    height: u16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    if args.solid_red {
        // Paint the root window background red.
        // Using ChangeWindowAttributes + ClearArea guarantees the root's own
        // pixel buffer contains the colour — XGetImage will always see it.
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().background_pixel(0x00FF_0000),
        )?;
        conn.clear_area(false, root, 0, 0, 0, 0)?; // 0,0 = full window
        conn.flush()?;
        eprintln!("test-pattern: root window painted red");
    }

    if args.spinner {
        let gc = conn.generate_id()?;
        conn.create_gc(gc, root, &CreateGCAux::default())?;

        let mut hue: u8 = 0;
        loop {
            // Cycle colors in a 64x64 region at position (100, 100)
            let r = hue;
            let g = hue.wrapping_add(85);
            let b = hue.wrapping_add(170);
            let pixel = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);

            conn.change_gc(gc, &ChangeGCAux::new().foreground(pixel))?;
            // Draw a filled rectangle at (100, 100), size 64x64
            conn.poly_fill_rectangle(root, gc, &[Rectangle {
                x: 100, y: 100, width: 64, height: 64,
            }])?;
            conn.flush()?;

            std::thread::sleep(std::time::Duration::from_millis(100));
            hue = hue.wrapping_add(25);
        }
    }

    // Keep the process alive so the container doesn't exit.
    while conn.wait_for_event().is_ok() {}

    Ok(())
}
