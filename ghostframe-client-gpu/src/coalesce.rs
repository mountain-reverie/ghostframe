//! Merge a [`DirtyGrid`] into as few damage rectangles as possible.
//!
//! The host's presentation path pays per damage rect handed to it, so
//! coalescing adjacent dirty tiles into larger rectangles matters. The
//! algorithm is two passes: horizontal runs per row, then a vertical merge
//! of runs that line up exactly with the row above. Cheap, and good
//! enough on the access pattern ghostframe produces (damage clusters
//! rather than scattered single tiles).

use crate::dirty::DirtyGrid;
use std::collections::HashMap;

/// A rectangle in TILE units. Convert to pixels with [`Rect::to_pixels`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// Scale to pixel coordinates, clamped to the framebuffer. The right
    /// and bottom edges need clamping because the tile grid is a ceil
    /// division: 1080 is 33.75 tiles, so row 33 is only 24 pixels tall.
    pub fn to_pixels(self, fb_width: u32, fb_height: u32) -> Rect {
        const T: u32 = 32;
        let x = self.x * T;
        let y = self.y * T;
        Rect {
            x,
            y,
            w: (self.w * T).min(fb_width.saturating_sub(x)),
            h: (self.h * T).min(fb_height.saturating_sub(y)),
        }
    }
}

/// A rectangle still accumulating height as identical runs stack up
/// across consecutive rows.
struct OpenRect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// Merge dirty tiles into as few rectangles as possible.
///
/// Two passes: horizontal runs per row, then merge vertically adjacent
/// runs that share an x-extent. Emits in row-major order of each rect's
/// origin, so output is deterministic regardless of iteration/hashing
/// order used internally.
pub fn coalesce(grid: &DirtyGrid) -> Vec<Rect> {
    let cols = grid.cols();
    let rows = grid.rows();

    // Rects still open for vertical extension, keyed by (x, w): the only
    // two fields a continuing run must match. Every entry here was
    // created (or last extended) at the immediately preceding row, so a
    // key match always satisfies `open.y + open.h == y` by construction.
    let mut open: HashMap<(u32, u32), OpenRect> = HashMap::new();
    let mut closed: Vec<Rect> = Vec::new();

    for y in 0..rows {
        let runs = horizontal_runs(grid, y, cols);

        let mut next_open: HashMap<(u32, u32), OpenRect> = HashMap::with_capacity(runs.len());
        for (x, w) in runs {
            let key = (x, w);
            match open.remove(&key) {
                Some(mut r) => {
                    debug_assert_eq!(
                        r.y + r.h,
                        y,
                        "an open rect must only be queued for the row immediately below it"
                    );
                    r.h += 1;
                    next_open.insert(key, r);
                }
                None => {
                    next_open.insert(key, OpenRect { x, y, w, h: 1 });
                }
            }
        }

        // Anything still in `open` did not continue into this row: close it.
        for (_, r) in open.drain() {
            closed.push(Rect {
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
            });
        }

        open = next_open;
    }

    for (_, r) in open.drain() {
        closed.push(Rect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
        });
    }

    // Internal order comes from hash-map iteration; sort so output is
    // deterministic and, per the doc comment, row-major by origin.
    closed.sort_unstable_by_key(|r| (r.y, r.x));
    closed
}

/// Contiguous runs of dirty tiles in row `y`, as `(start_x, width)`.
fn horizontal_runs(grid: &DirtyGrid, y: u32, cols: u32) -> Vec<(u32, u32)> {
    let mut runs = Vec::new();
    let mut x = 0;
    while x < cols {
        if grid.get(x, y) {
            let start = x;
            while x < cols && grid.get(x, y) {
                x += 1;
            }
            runs.push((start, x - start));
        } else {
            x += 1;
        }
    }
    runs
}
