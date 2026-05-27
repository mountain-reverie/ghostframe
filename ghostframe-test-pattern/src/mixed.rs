//! `--mixed` pattern: four content classes painted on disjoint regions of
//! the root window. The same `REGIONS` table is consumed by the renderer
//! and by the e2e test — single source of truth for geometry and the M3
//! codec each region targets.

use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;

use crate::font::{pixel_set, GLYPH_H, GLYPH_W};

/// One region of the mixed pattern.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub name: &'static str,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// Codec the M3 classifier is expected to pick for this region.
    /// Pre-M3 the e2e test only logs this; post-M3 it is asserted against
    /// per-tile codec stats from a future instrumentation channel.
    pub expected_codec: &'static str,
}

/// Default 640×480 layout — split into four 320×240 quadrants.
pub const REGIONS: &[Region] = &[
    Region {
        name: "solid",
        x: 0,
        y: 0,
        w: 320,
        h: 240,
        expected_codec: "Solid",
    },
    Region {
        name: "text",
        x: 320,
        y: 0,
        w: 320,
        h: 240,
        expected_codec: "PalRle",
    },
    Region {
        name: "gradient",
        x: 0,
        y: 240,
        w: 320,
        h: 240,
        expected_codec: "Cdf53",
    },
    Region {
        name: "spinner",
        x: 320,
        y: 240,
        w: 320,
        h: 240,
        expected_codec: "H264",
    },
];

/// Look up a region by name. Panics if not found — REGIONS is a fixed table.
pub fn region(name: &str) -> &'static Region {
    REGIONS
        .iter()
        .find(|r| r.name == name)
        .expect("unknown mixed region")
}

/// Settle time recommended for the e2e test before sampling. The spinner
/// must run long enough for two screenshots to capture different frames.
pub const SETTLE: Duration = Duration::from_secs(7);

/// Paint the static regions (solid, text, gradient) and emit the spinner
/// loop on the calling thread. Never returns — the loop runs forever.
pub fn render_and_spin<C: Connection>(
    conn: &C,
    root: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    paint_static(conn, root)?;
    spinner_loop(conn, root, region("spinner"))
}

fn paint_static<C: Connection>(conn: &C, root: u32) -> Result<(), Box<dyn std::error::Error>> {
    paint_solid(conn, root, region("solid"))?;
    paint_text(conn, root, region("text"))?;
    paint_gradient(conn, root, region("gradient"))?;
    conn.flush()?;
    eprintln!("test-pattern: mixed static regions painted");
    Ok(())
}

fn paint_solid<C: Connection>(
    conn: &C,
    root: u32,
    r: &Region,
) -> Result<(), Box<dyn std::error::Error>> {
    let gc = conn.generate_id()?;
    conn.create_gc(gc, root, &CreateGCAux::new().foreground(0x00FF_0000))?; // red
    conn.poly_fill_rectangle(
        root,
        gc,
        &[Rectangle {
            x: r.x as i16,
            y: r.y as i16,
            width: r.w as u16,
            height: r.h as u16,
        }],
    )?;
    Ok(())
}

fn paint_text<C: Connection>(
    conn: &C,
    root: u32,
    r: &Region,
) -> Result<(), Box<dyn std::error::Error>> {
    // Background fill — near-black so unlit pixels are deterministic.
    let bg_gc = conn.generate_id()?;
    conn.create_gc(bg_gc, root, &CreateGCAux::new().foreground(0x0014_1414))?;
    conn.poly_fill_rectangle(
        root,
        bg_gc,
        &[Rectangle {
            x: r.x as i16,
            y: r.y as i16,
            width: r.w as u16,
            height: r.h as u16,
        }],
    )?;

    // Foreground GC — near-white text.
    let fg_gc = conn.generate_id()?;
    conn.create_gc(fg_gc, root, &CreateGCAux::new().foreground(0x00F5_F5F5))?;

    // Repeat "TEST" three times across the top of the region, with a 4 px
    // top margin and 2 px between rows.
    let text = "TEST TEST TEST";
    let mut rects: Vec<Rectangle> = Vec::with_capacity(text.len() * 24);
    for (n, ch) in text.chars().enumerate() {
        let glyph_x = r.x + 8 + (n as u32) * GLYPH_W;
        let glyph_y = r.y + 8;
        for py in 0..GLYPH_H {
            for px in 0..GLYPH_W {
                if pixel_set(ch, px, py) {
                    rects.push(Rectangle {
                        x: (glyph_x + px) as i16,
                        y: (glyph_y + py) as i16,
                        width: 1,
                        height: 1,
                    });
                }
            }
        }
    }
    conn.poly_fill_rectangle(root, fg_gc, &rects)?;
    Ok(())
}

fn paint_gradient<C: Connection>(
    conn: &C,
    root: u32,
    r: &Region,
) -> Result<(), Box<dyn std::error::Error>> {
    // 4×4 block grid with a diagonal RGB ramp. Each 32×32 capture tile holds
    // 8×8 = 64 distinct block colors, comfortably above the classifier's
    // 16-color threshold, so the gradient region routes to Cdf53 instead of
    // collapsing to PalRle (which a coarse-stripe gradient does).
    let block: u32 = 4;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, root, &CreateGCAux::default())?;
    let mut by = 0;
    while by < r.h {
        let mut bx = 0;
        while bx < r.w {
            let cx = bx / block;
            let cy = by / block;
            let red = ((cx * 3) & 0xFF) as u32;
            let grn = ((cy * 3) & 0xFF) as u32;
            let blu = (((cx + cy) * 2) & 0xFF) as u32;
            let pixel = (red << 16) | (grn << 8) | blu;
            conn.change_gc(gc, &ChangeGCAux::new().foreground(pixel))?;
            conn.poly_fill_rectangle(
                root,
                gc,
                &[Rectangle {
                    x: (r.x + bx) as i16,
                    y: (r.y + by) as i16,
                    width: block as u16,
                    height: block as u16,
                }],
            )?;
            bx += block;
        }
        by += block;
    }
    Ok(())
}

fn spinner_loop<C: Connection>(
    conn: &C,
    root: u32,
    r: &Region,
) -> Result<(), Box<dyn std::error::Error>> {
    let gc = conn.generate_id()?;
    conn.create_gc(gc, root, &CreateGCAux::default())?;
    let mut hue: u8 = 0;
    loop {
        let r_ch = hue;
        let g_ch = hue.wrapping_add(85);
        let b_ch = hue.wrapping_add(170);
        let pixel = ((r_ch as u32) << 16) | ((g_ch as u32) << 8) | (b_ch as u32);
        conn.change_gc(gc, &ChangeGCAux::new().foreground(pixel))?;
        conn.poly_fill_rectangle(
            root,
            gc,
            &[Rectangle {
                x: r.x as i16,
                y: r.y as i16,
                width: r.w as u16,
                height: r.h as u16,
            }],
        )?;
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(100));
        hue = hue.wrapping_add(25);
    }
}
