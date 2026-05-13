//! X11 test pattern app for E2E tests.
//!
//! Draws deterministic color patterns on the X11 root window so the E2E test
//! can verify pixel-level correctness of the capture + transport pipeline.
//!
//! When `--solid-red` is used, sets the root window background to red and
//! clears it.  This guarantees `XGetImage` on the root returns red pixels
//! regardless of compositor/WM presence.

use ghostframe_test_pattern::{mixed, text_grid};

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

    /// Draw a known monospace string at fixed coordinates for the e2e
    /// text-clarity test. Mutually compatible with --solid-red (text-grid
    /// is rendered last, after solid-red has cleared the background).
    #[arg(long)]
    text_grid: bool,

    /// Draw the four-region mixed pattern (solid + text + gradient + spinner).
    /// Spawns the spinner loop forever; mutually exclusive with --solid-red,
    /// --spinner, --text-grid (those are ignored when --mixed is set).
    #[arg(long)]
    mixed: bool,

    /// Cycle between static and motion content every `SECS` seconds (so the
    /// full cycle is `2 * SECS`). Drives `e2e_mode_switch` to verify the
    /// classifier flips between H264 and TileCodec modes.
    #[arg(long)]
    mode_switch_cycle: Option<u64>,

    /// Draw `count` sequential text-like regions, each with a distinct
    /// 4-color palette, cycling over ~5 seconds. Drives e2e_palette_eviction.
    #[arg(long)]
    palette_churn: Option<u32>,

    /// Run as a DRM master and paint directly to `/dev/dri/card<N>`'s scanout
    /// instead of speaking X11. Required for environments where Xorg holds
    /// the DRM master and prevents framebuffer-update propagation (e.g.
    /// VKMS). Mutually exclusive with the X11 modes; combine with
    /// `--mode-switch-cycle`.
    #[arg(long)]
    drm_direct: bool,

    /// DRM device path for `--drm-direct`. Defaults to `/dev/dri/card0`.
    #[arg(long, default_value = "/dev/dri/card0")]
    drm_device: String,

    /// (Ignored — kept for CLI compat.) Window width.
    #[arg(long, default_value = "640")]
    width: u16,

    /// (Ignored — kept for CLI compat.) Window height.
    #[arg(long, default_value = "480")]
    height: u16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // DRM-direct path: take DRM master and paint straight to scanout.
    // Must run before any X11 connect attempt — there is no X server in
    // this configuration.
    if args.drm_direct {
        let secs = args
            .mode_switch_cycle
            .ok_or_else(|| "--drm-direct requires --mode-switch-cycle SECS".to_string())?;
        let half = std::time::Duration::from_secs(secs);
        return ghostframe_test_pattern::drm_direct::run(&args.drm_device, half);
    }

    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    if let Some(count) = args.palette_churn {
        return ghostframe_test_pattern::palette_churn::run(&conn, root, count);
    }

    if let Some(secs) = args.mode_switch_cycle {
        let half = std::time::Duration::from_secs(secs);
        return ghostframe_test_pattern::mode_switch::run(&conn, root, half);
    }

    if args.mixed {
        // Mutually exclusive — render four regions and run the spinner forever.
        return mixed::render_and_spin(&conn, root);
    }

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

    if args.text_grid {
        text_grid::render(&conn, root)?;
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

            std::thread::sleep(std::time::Duration::from_millis(100));
            hue = hue.wrapping_add(25);
        }
    }

    // Keep the process alive so the container doesn't exit.
    while conn.wait_for_event().is_ok() {}

    Ok(())
}
