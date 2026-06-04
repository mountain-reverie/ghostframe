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

    /// Paint four 32x32 corner tiles in fixed colours (RED/GREEN/BLUE/YELLOW)
    /// plus a central 64x64 motion region. Combine with `--drm-direct` to
    /// paint to the DRM dumb-buffer scanout. Drives `e2e_solid_per_tile_pixels`.
    #[arg(long)]
    solid_per_tile: bool,

    /// Paint a four-tile PalRle test scene (checkerboard / horizontal
    /// stripes / vertical stripes / 2×2 blocks) with a known 2-color
    /// palette. Combine with `--drm-direct` to paint to the DRM
    /// dumb-buffer scanout. Drives `e2e_palrle_exact_pixels`.
    #[arg(long)]
    palrle_exact: bool,

    /// Paint a full-screen diagonal RGB gradient. Each 32×32 tile has >16
    /// unique colors so the classifier picks the high-color path
    /// (Raw fallback, or Cdf53 when GHOSTFRAME_ENABLE_CDF53 is set).
    /// Combine with `--drm-direct` to paint to the DRM dumb-buffer scanout.
    /// Drives the M3.3a e2e tests.
    #[arg(long)]
    gradient: bool,

    /// Fill the full screen by repeating a 32×32 fixture tile for one of the
    /// six M3.5 Layer B content classes: solid, flat_ui, text, gradient,
    /// photo, motion. Requires `--drm-direct`.
    ///
    /// Example: `--drm-direct --tile-pattern flat_ui`
    #[arg(long, value_name = "CLASS")]
    tile_pattern: Option<String>,

    /// M3.7a: shift the rendered tile_pattern by 1 px every N ms to
    /// generate dirty events for the classifier under pure-scene
    /// content. Requires --drm-direct and --tile-pattern.
    #[arg(long)]
    subtle_drift: Option<u64>,

    /// (Ignored — kept for CLI compat.) Window width.
    #[arg(long, default_value = "640")]
    width: u16,

    /// (Ignored — kept for CLI compat.) Window height.
    #[arg(long, default_value = "480")]
    height: u16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // DRM-direct palrle-exact mode: paint four 32×32 PalRle test tiles
    // with known 2-color patterns. Drives `e2e_palrle_exact_pixels`
    // (M3.2c B1 follow-up).
    if args.drm_direct && args.palrle_exact {
        return ghostframe_test_pattern::palrle_exact::run(&args.drm_device);
    }

    // DRM-direct solid-per-tile mode: paint fixed corner tiles + a small
    // central motion region so the classifier sees `unique_colors == 1`
    // per corner and picks `CodecState::Solid`. Drives
    // `e2e_solid_per_tile_pixels` (M3.2c W3/B3).
    if args.drm_direct && args.solid_per_tile {
        return ghostframe_test_pattern::solid_per_tile::run(&args.drm_device);
    }

    // DRM-direct gradient mode: full-screen high-color gradient for M3.3a
    // Cdf53 e2e tests. No X11 path; gradient is DRM-only.
    if args.drm_direct && args.gradient {
        return ghostframe_test_pattern::gradient::run(&args.drm_device);
    }

    // DRM-direct tile-pattern mode: full-screen ContentClass fixture tile
    // repeated to cover the scanout. Drives M3.5 Layer B pure-scene benches.
    // With --subtle-drift <ms>, shifts the bitmap 1 px/X per tick to generate
    // dirty events for the classifier (M3.7a bench).
    if args.drm_direct {
        if let Some(ref class_name) = args.tile_pattern {
            if let Some(drift_ms) = args.subtle_drift {
                return ghostframe_test_pattern::subtle_drift::run(
                    &args.drm_device,
                    class_name,
                    drift_ms,
                );
            }
            return ghostframe_test_pattern::tile_pattern::run(
                &args.drm_device,
                class_name,
            );
        }
    }

    // DRM-direct text-grid mode: paint text glyphs directly to scanout,
    // bypassing Xorg. Required for E2E tests where Xorg-on-VKMS yields
    // >16 unique colours per tile and PalRle never wins classification.
    // See docs/superpowers/specs/2026-05-15-m3.2c-verification-design.md.
    if args.drm_direct && args.text_grid {
        return ghostframe_test_pattern::text_grid_drm::run(&args.drm_device);
    }

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
