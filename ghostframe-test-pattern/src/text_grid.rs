//! `--text-grid` pattern: paint a known ASCII string at known coordinates.
//!
//! The rendered text and pixel coords are part of the public surface of this
//! module so the e2e test imports them directly — there is exactly one source
//! of truth for "where does ink land on the X server".

use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;

use crate::font::{pixel_set, GLYPH_H, GLYPH_W};

/// Background colour painted on the root before drawing glyphs.
pub const BG_PIXEL: u32 = 0x0014_1414; // near-black RGB(0x14,0x14,0x14)
/// Glyph "ink" colour.
pub const FG_PIXEL: u32 = 0x00F5_F5F5; // near-white RGB(0xF5,0xF5,0xF5)

/// Top-left of the rendered text block, in root-window pixels.
pub const ORIGIN_X: u32 = 80;
pub const ORIGIN_Y: u32 = 100;

/// String drawn on the root window. Letters chosen to fit the bundled font.
pub const TEXT: &str = "GHOSTFRAME / TILE / TEST";

/// Pixels of inter-character spacing (no spacing between glyph cells —
/// the font itself includes a 1-px gutter on the right of each glyph).
pub const CHAR_SPACING: u32 = 0;

/// Returns the (x, y) of the top-left of the n-th character in TEXT.
pub fn char_origin(n: usize) -> (u32, u32) {
    let x = ORIGIN_X + (n as u32) * (GLYPH_W + CHAR_SPACING);
    (x, ORIGIN_Y)
}

/// Returns the absolute pixel coordinate of glyph-local pixel (px, py)
/// inside the n-th character of TEXT.
pub fn glyph_pixel(n: usize, px: u32, py: u32) -> (u32, u32) {
    let (ox, oy) = char_origin(n);
    (ox + px, oy + py)
}

/// Hand-picked sample positions. The e2e test asserts contrast across these
/// pairs: each ink pixel paired with a same-row background pixel.
#[derive(Debug)]
pub struct SamplePair {
    pub ink: (u32, u32),
    pub bg: (u32, u32),
}

/// 4 ink/bg sample pairs spread across the rendered string. Hard-coded
/// rather than computed so a regression in the font table or origin is
/// caught here, not papered over.
///
/// Positions chosen on this string ("GHOSTFRAME / TILE / TEST"):
///   index 0  = 'G' — second pixel of top arc:  glyph-px (1,0) is ink,
///                    leftmost pixel glyph-px (0,0) is bg.
///   index 7  = 'A' — apex row glyph-px (2,0) is ink (row0=0b001100),
///                    leftmost pixel glyph-px (0,0) is bg.
///   index 14 = 'I' — second pixel of top serif: glyph-px (1,0) is ink
///                    (row0=0b011110), leftmost glyph-px (0,1) is bg.
///   index 20 = 'T' — crossbar glyph-px (1,0) is ink (row0=0b111111),
///                    body-below glyph-px (0,1) is bg (row1=0b001100).
pub const SAMPLES: &[SamplePair] = &[
    SamplePair {
        ink: (ORIGIN_X + 1, ORIGIN_Y + 0),
        bg: (ORIGIN_X + 0, ORIGIN_Y + 0),
    },
    SamplePair {
        ink: (ORIGIN_X + 7 * GLYPH_W + 2, ORIGIN_Y + 0),
        bg: (ORIGIN_X + 7 * GLYPH_W + 0, ORIGIN_Y + 0),
    },
    SamplePair {
        ink: (ORIGIN_X + 14 * GLYPH_W + 1, ORIGIN_Y + 0),
        bg: (ORIGIN_X + 14 * GLYPH_W + 0, ORIGIN_Y + 1),
    },
    SamplePair {
        ink: (ORIGIN_X + 20 * GLYPH_W + 1, ORIGIN_Y + 0),
        bg: (ORIGIN_X + 20 * GLYPH_W + 0, ORIGIN_Y + 1),
    },
];

/// Paint the root window background and glyphs. Returns once the X server
/// has flushed every operation.
pub fn render<C: Connection>(conn: &C, root: u32) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Background.
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().background_pixel(BG_PIXEL),
    )?;
    conn.clear_area(false, root, 0, 0, 0, 0)?;

    // 2. Foreground GC.
    let gc = conn.generate_id()?;
    conn.create_gc(gc, root, &CreateGCAux::new().foreground(FG_PIXEL))?;

    // 3. Build a list of rectangles to fill — one per ink pixel.
    let mut rects: Vec<Rectangle> = Vec::with_capacity(TEXT.len() * 24);
    for (n, ch) in TEXT.chars().enumerate() {
        for py in 0..GLYPH_H {
            for px in 0..GLYPH_W {
                if pixel_set(ch, px, py) {
                    let (ax, ay) = glyph_pixel(n, px, py);
                    rects.push(Rectangle {
                        x: ax as i16,
                        y: ay as i16,
                        width: 1,
                        height: 1,
                    });
                }
            }
        }
    }

    // 4. Issue the fills in a single request.
    conn.poly_fill_rectangle(root, gc, &rects)?;
    conn.flush()?;

    eprintln!(
        "test-pattern: text-grid rendered ({} ink pixels)",
        rects.len()
    );
    Ok(())
}
