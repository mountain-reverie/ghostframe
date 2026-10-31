//! Row bands: a slot's grab set as full-width runs of pixel rows
//! (design 3.1, "Grab primitive").
//!
//! `ShmGetImage` writes a rectangle packed at `shmoffset`, not at its place
//! in a larger image, so a rectangle cannot land in a persistent frame. A
//! full-width band `y0..y1` is contiguous at `y0 * stride`, so it can. The
//! grab set is a tile bitmap; this module turns it into bands, or says that
//! one full grab is the better request.

use ghostframe_lib::tile::TILE_SIZE;

use crate::capture::TileBitmap;

/// Bands whose gap is at most this many tile rows are merged: the rows
/// between are grabbed too (cheap, contiguous) and a request is saved.
pub const MERGE_GAP_TILE_ROWS: u32 = 2;

/// Most requests one tick sends. All are pipelined, but each is work for the
/// X server and a reply to read; beyond this the smallest gaps are merged.
pub const MAX_BANDS: usize = 16;

/// Above this share of the screen's rows (percent) one full grab replaces the
/// bands.
///
/// 50 %: past it the bands save under half the copy yet still pay one
/// request each, and a full grab is atomic where separate band requests can
/// tear (design 3.1, M1). Typing, a cursor and the sweep slice (1/12 of the
/// rows) stay far below it; scrolling and video sit above it.
pub const FULL_GRAB_ABOVE_PCT: u32 = 50;

/// A run of pixel rows `y0..y1`, full width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Band {
    pub y0: u32,
    pub y1: u32,
}

impl Band {
    pub fn rows(&self) -> u32 {
        self.y1 - self.y0
    }
}

/// What to grab into a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grab {
    /// The whole root, one request.
    Full,
    /// These bands (possibly none: the slot is already current).
    Bands(Bands),
}

/// Up to [`MAX_BANDS`] bands, inline: no allocation per tick.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Bands {
    spans: [Band; MAX_BANDS],
    n: usize,
}

impl std::fmt::Debug for Bands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl Bands {
    pub fn as_slice(&self) -> &[Band] {
        &self.spans[..self.n]
    }

    /// Pixel rows covered.
    pub fn rows(&self) -> u32 {
        self.as_slice().iter().map(Band::rows).sum()
    }
}

/// The bands that cover `mask` on a screen `height` pixels tall.
pub fn plan(mask: &TileBitmap, height: u32) -> Grab {
    // Runs of tile rows with a set tile. `iter` is row-major, so rows come in
    // order and the scan costs the words of the bitmap plus its set tiles.
    let mut runs = [(0u32, 0u32); MAX_BANDS + 1];
    let mut n = 0usize;
    let mut last_row = None;
    for (_, ty) in mask.iter() {
        if last_row == Some(ty) {
            continue;
        }
        last_row = Some(ty);
        if n > 0 && runs[n - 1].1 == ty {
            runs[n - 1].1 = ty + 1;
            continue;
        }
        // Merge across a small gap as we go.
        if n > 0 && ty - runs[n - 1].1 <= MERGE_GAP_TILE_ROWS {
            runs[n - 1].1 = ty + 1;
            continue;
        }
        if n == runs.len() {
            // Too many separate runs: fold the smallest gap away.
            merge_smallest_gap(&mut runs, &mut n);
        }
        runs[n] = (ty, ty + 1);
        n += 1;
    }
    while n > MAX_BANDS {
        merge_smallest_gap(&mut runs, &mut n);
    }
    let mut out = Bands {
        spans: [Band::default(); MAX_BANDS],
        n,
    };
    for (i, &(lo, hi)) in runs[..n].iter().enumerate() {
        out.spans[i] = Band {
            y0: (lo * TILE_SIZE).min(height),
            y1: (hi * TILE_SIZE).min(height),
        };
    }
    if u64::from(out.rows()) * 100 > u64::from(height) * u64::from(FULL_GRAB_ABOVE_PCT) {
        Grab::Full
    } else {
        Grab::Bands(out)
    }
}

fn merge_smallest_gap(runs: &mut [(u32, u32); MAX_BANDS + 1], n: &mut usize) {
    debug_assert!(*n >= 2);
    let i = (1..*n)
        .min_by_key(|&i| runs[i].0 - runs[i - 1].1)
        .expect("at least two runs");
    runs[i - 1].1 = runs[i].1;
    runs.copy_within(i + 1..*n, i);
    *n -= 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = T * 10;
    const T: u32 = TILE_SIZE;
    const H: u32 = T * 64; // 64 tile rows

    fn mask(rows: &[u32]) -> TileBitmap {
        let mut m = TileBitmap::new(W, H);
        for &r in rows {
            m.set(r % 3, r);
        }
        m
    }

    fn bands(g: Grab) -> Vec<(u32, u32)> {
        match g {
            Grab::Bands(b) => b.as_slice().iter().map(|b| (b.y0, b.y1)).collect(),
            Grab::Full => panic!("expected bands"),
        }
    }

    #[test]
    fn an_empty_mask_grabs_nothing() {
        assert_eq!(bands(plan(&mask(&[]), H)), vec![]);
    }

    #[test]
    fn adjacent_rows_are_one_band_and_a_small_gap_is_merged() {
        assert_eq!(bands(plan(&mask(&[4, 5, 6]), H)), vec![(4 * T, 7 * T)]);
        // Gap of 2 tile rows (rows 6 and 7 between 5 and 8): merged.
        assert_eq!(bands(plan(&mask(&[4, 5, 8]), H)), vec![(4 * T, 9 * T)]);
        // Gap of 3: kept apart.
        assert_eq!(
            bands(plan(&mask(&[4, 5, 9]), H)),
            vec![(4 * T, 6 * T), (9 * T, 10 * T)]
        );
    }

    #[test]
    fn the_last_band_is_clipped_to_a_partial_tile_row() {
        let h = T * 5 + 7;
        let mut m = TileBitmap::new(W, h);
        m.set(0, 5);
        assert_eq!(bands(plan(&m, h)), vec![(5 * T, h)]);
    }

    #[test]
    fn past_the_fraction_one_full_grab_replaces_the_bands() {
        // 33 isolated-ish rows out of 64 is above 50 %.
        let rows: Vec<u32> = (0..64).step_by(2).collect(); // gap 1 -> merged to all
        assert_eq!(plan(&mask(&rows), H), Grab::Full);
        let few: Vec<u32> = (0..30).collect();
        assert!(matches!(plan(&mask(&few), H), Grab::Bands(_)));
        let over: Vec<u32> = (0..33).collect();
        assert_eq!(plan(&mask(&over), H), Grab::Full);
    }

    #[test]
    fn the_band_count_is_bounded_and_every_set_row_stays_covered() {
        // Rows 0, 4, 8, ... : 16 isolated runs with gaps of 3 plus a few more
        // up to 20 runs; the cap forces merges. Keep it under the fraction by
        // using a tall screen.
        let h = T * 400;
        let mut m = TileBitmap::new(W, h);
        let rows: Vec<u32> = (0..20).map(|i| i * 4 + (i % 3)).collect();
        for &r in &rows {
            m.set(0, r);
        }
        let g = plan(&m, h);
        let b = bands(g);
        assert!(b.len() <= MAX_BANDS, "{} bands", b.len());
        for &r in &rows {
            let (y0, y1) = (r * T, r * T + T);
            assert!(b.iter().any(|&(a, z)| a <= y0 && y1 <= z), "row {r} lost");
        }
        // Bands are disjoint and ordered.
        assert!(b.windows(2).all(|w| w[0].1 <= w[1].0));
    }

    /// Property: whatever the mask, bands are ordered, disjoint, inside the
    /// screen, and cover every set tile (or the plan is Full).
    #[test]
    fn bands_cover_any_mask() {
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        for _ in 0..500 {
            let h = T * 200 + (next() % u64::from(T)) as u32;
            let mut m = TileBitmap::new(W, h);
            let k = next() % 40;
            for _ in 0..k {
                m.set((next() % 10) as u32, (next() % 201) as u32);
            }
            let Grab::Bands(b) = plan(&m, h) else {
                continue;
            };
            let b = b.as_slice();
            assert!(b.len() <= MAX_BANDS);
            assert!(b.windows(2).all(|w| w[0].y1 <= w[1].y0));
            assert!(b.iter().all(|x| x.y0 < x.y1 && x.y1 <= h));
            for (_, ty) in m.iter() {
                let (y0, y1) = (ty * T, ((ty + 1) * T).min(h));
                assert!(
                    b.iter().any(|x| x.y0 <= y0 && y1 <= x.y1),
                    "tile row {ty} lost"
                );
            }
        }
    }
}
