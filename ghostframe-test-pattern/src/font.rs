//! Hand-rolled 6×8 monospace bitmap font, just enough characters for the
//! `--text-grid` test pattern. Each glyph is 8 rows of u8; bit 5 is the
//! left-most pixel ("ink"), bit 0 is the right-most pixel.

pub const GLYPH_W: u32 = 6;
pub const GLYPH_H: u32 = 8;

pub fn glyph(c: char) -> &'static [u8; 8] {
    match c {
        ' ' => &[0,0,0,0,0,0,0,0],
        '/' => &[
            0b000001,
            0b000010,
            0b000010,
            0b000100,
            0b000100,
            0b001000,
            0b010000,
            0b000000,
        ],
        'A' => &[
            0b001100,
            0b010010,
            0b100001,
            0b100001,
            0b111111,
            0b100001,
            0b100001,
            0b000000,
        ],
        'E' => &[
            0b111111,
            0b100000,
            0b100000,
            0b111110,
            0b100000,
            0b100000,
            0b111111,
            0b000000,
        ],
        'F' => &[
            0b111111,
            0b100000,
            0b100000,
            0b111110,
            0b100000,
            0b100000,
            0b100000,
            0b000000,
        ],
        'G' => &[
            0b011110,
            0b100001,
            0b100000,
            0b100111,
            0b100001,
            0b100001,
            0b011110,
            0b000000,
        ],
        'H' => &[
            0b100001,
            0b100001,
            0b100001,
            0b111111,
            0b100001,
            0b100001,
            0b100001,
            0b000000,
        ],
        'I' => &[
            0b011110,
            0b001100,
            0b001100,
            0b001100,
            0b001100,
            0b001100,
            0b011110,
            0b000000,
        ],
        'L' => &[
            0b100000,
            0b100000,
            0b100000,
            0b100000,
            0b100000,
            0b100000,
            0b111111,
            0b000000,
        ],
        'M' => &[
            0b100001,
            0b110011,
            0b101101,
            0b100001,
            0b100001,
            0b100001,
            0b100001,
            0b000000,
        ],
        'O' => &[
            0b011110,
            0b100001,
            0b100001,
            0b100001,
            0b100001,
            0b100001,
            0b011110,
            0b000000,
        ],
        'R' => &[
            0b111110,
            0b100001,
            0b100001,
            0b111110,
            0b100100,
            0b100010,
            0b100001,
            0b000000,
        ],
        'S' => &[
            0b011111,
            0b100000,
            0b100000,
            0b011110,
            0b000001,
            0b000001,
            0b111110,
            0b000000,
        ],
        'T' => &[
            0b111111,
            0b001100,
            0b001100,
            0b001100,
            0b001100,
            0b001100,
            0b001100,
            0b000000,
        ],
        // Anything we didn't ship a glyph for renders as a solid block so a
        // typo in the test string fails loudly rather than silently blanking.
        _ => &[
            0b111111,
            0b111111,
            0b111111,
            0b111111,
            0b111111,
            0b111111,
            0b111111,
            0b000000,
        ],
    }
}

/// Returns true if pixel (px, py) inside a `GLYPH_W × GLYPH_H` glyph box is "ink".
pub fn pixel_set(c: char, px: u32, py: u32) -> bool {
    if px >= GLYPH_W || py >= GLYPH_H {
        return false;
    }
    let row = glyph(c)[py as usize];
    let bit = 1u8 << (GLYPH_W - 1 - px);
    (row & bit) != 0
}
