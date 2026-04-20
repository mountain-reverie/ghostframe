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

    // Keep the process alive so the container doesn't exit.
    while conn.wait_for_event().is_ok() {}

    Ok(())
}
