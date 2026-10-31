//! A fixed-grid bitmap of damaged tiles, one bit per tile.
//!
//! Replaces a `HashSet<(u32, u32)>`: setting a tile is a shift and an OR, a
//! rectangle costs O(tiles it covers) with no hashing or allocation, and the
//! whole set is `ceil(tiles / 64)` words however many rectangles fed it.

use ghostframe_lib::tile::TILE_SIZE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileBitmap {
    /// The screen size in pixels this bitmap was built for. Compared exactly:
    /// 1920x1080 and 1910x1070 share a 60x34 tile grid but are different
    /// screens.
    px: (u32, u32),
    tiles_w: u32,
    tiles_h: u32,
    bits: Vec<u64>,
}

impl TileBitmap {
    /// An empty bitmap for a `width` x `height` pixel screen.
    pub fn new(width: u32, height: u32) -> Self {
        let tiles_w = width.div_ceil(TILE_SIZE);
        let tiles_h = height.div_ceil(TILE_SIZE);
        let n = (tiles_w as usize) * (tiles_h as usize);
        Self {
            px: (width, height),
            tiles_w,
            tiles_h,
            bits: vec![0; n.div_ceil(64)],
        }
    }

    /// A bitmap with no grid and no allocation; `matches` nothing but a
    /// zero-sized screen.
    pub fn empty() -> Self {
        Self {
            px: (0, 0),
            tiles_w: 0,
            tiles_h: 0,
            bits: Vec::new(),
        }
    }

    /// Clear every bit, keeping the allocation.
    pub fn clear(&mut self) {
        self.bits.fill(0);
    }

    /// Mark every tile. Bits past the grid stay clear so `count` and `iter`
    /// agree.
    pub fn set_all(&mut self) {
        let total = (self.tiles_w as usize) * (self.tiles_h as usize);
        self.bits.fill(u64::MAX);
        let n = self.bits.len();
        if let Some(last) = self.bits.last_mut() {
            let used = total - (n - 1) * 64;
            if used < 64 {
                *last = (1u64 << used) - 1;
            }
        }
    }

    /// Mark the tile rows `lo..hi` (clipped to the grid).
    #[cfg(test)]
    pub fn set_rows(&mut self, lo: u32, hi: u32) {
        for ty in lo..hi.min(self.tiles_h) {
            for tx in 0..self.tiles_w {
                self.set(tx, ty);
            }
        }
    }

    /// `self |= other`; a bitmap of another screen size is ignored.
    pub fn or_with(&mut self, other: &TileBitmap) {
        if self.px == other.px {
            for (a, b) in self.bits.iter_mut().zip(&other.bits) {
                *a |= *b;
            }
        }
    }

    #[cfg(test)]
    /// `self &= !other`; a bitmap of another screen size is ignored.
    pub fn clear_bits_of(&mut self, other: &TileBitmap) {
        if self.px == other.px {
            for (a, b) in self.bits.iter_mut().zip(&other.bits) {
                *a &= !*b;
            }
        }
    }

    /// Become a copy of `other`, reusing the allocation when the size matches.
    pub fn copy_from(&mut self, other: &TileBitmap) {
        if self.px == other.px {
            self.bits.copy_from_slice(&other.bits);
        } else {
            *self = other.clone();
        }
    }

    /// Become the bitmap of `words` for a `width` x `height` screen, reusing
    /// the allocation; `false` when the word count does not fit.
    pub fn load_words(&mut self, width: u32, height: u32, words: &[u64]) -> bool {
        if !self.matches(width, height) {
            *self = Self::new(width, height);
        }
        if self.bits.len() != words.len() {
            return false;
        }
        self.bits.copy_from_slice(words);
        let total = (self.tiles_w as usize) * (self.tiles_h as usize);
        let n = self.bits.len();
        if let Some(last) = self.bits.last_mut() {
            let used = total - (n - 1) * 64;
            if used < 64 {
                *last &= (1u64 << used) - 1;
            }
        }
        true
    }

    /// [`TileBitmap::load_words`] into a fresh bitmap.
    #[cfg(test)]
    pub fn from_words(width: u32, height: u32, words: &[u64]) -> Option<Self> {
        let mut b = Self::new(width, height);
        b.load_words(width, height, words).then_some(b)
    }

    /// The screen size in pixels this bitmap was built for.
    pub fn px(&self) -> (u32, u32) {
        self.px
    }

    /// Grid size in tiles.
    pub fn grid(&self) -> (u32, u32) {
        (self.tiles_w, self.tiles_h)
    }

    /// Whether this bitmap was built for exactly a `width` x `height` pixel
    /// screen. A hint for another size says nothing about this one, even when
    /// the tile grid is the same.
    pub fn matches(&self, width: u32, height: u32) -> bool {
        self.px == (width, height)
    }

    fn index(&self, tx: u32, ty: u32) -> Option<usize> {
        (tx < self.tiles_w && ty < self.tiles_h)
            .then(|| (ty as usize) * (self.tiles_w as usize) + tx as usize)
    }

    /// Mark one tile. Off-grid coordinates are ignored (only on-screen tiles
    /// exist).
    pub fn set(&mut self, tx: u32, ty: u32) {
        if let Some(i) = self.index(tx, ty) {
            self.bits[i / 64] |= 1 << (i % 64);
        }
    }

    pub fn get(&self, tx: u32, ty: u32) -> bool {
        self.index(tx, ty)
            .is_some_and(|i| self.bits[i / 64] >> (i % 64) & 1 == 1)
    }

    /// Mark every tile a pixel rectangle touches.
    ///
    /// A negative origin is clamped to 0 without shrinking the extent, so a
    /// rectangle straddling the screen edge over-approximates by the clipped
    /// amount. Over-approximating costs a redundant tile; under-approximating
    /// would drop a changed tile.
    pub fn set_rect(&mut self, x: i16, y: i16, width: u16, height: u16) {
        let x = x.max(0) as u32;
        let y = y.max(0) as u32;
        let tx_end = (x + u32::from(width)).div_ceil(TILE_SIZE).min(self.tiles_w);
        let ty_end = (y + u32::from(height))
            .div_ceil(TILE_SIZE)
            .min(self.tiles_h);
        for ty in (y / TILE_SIZE)..ty_end {
            for tx in (x / TILE_SIZE)..tx_end {
                self.set(tx, ty);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|&w| w == 0)
    }

    pub fn count(&self) -> usize {
        self.bits.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Set tiles in row-major order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        let w = self.tiles_w;
        let total = (self.tiles_w as usize) * (self.tiles_h as usize);
        self.bits
            .iter()
            .enumerate()
            .flat_map(|(wi, &word)| {
                let mut word = word;
                std::iter::from_fn(move || {
                    if word == 0 {
                        return None;
                    }
                    let b = word.trailing_zeros() as usize;
                    word &= word - 1;
                    Some(wi * 64 + b)
                })
            })
            .filter(move |&i| i < total)
            .map(move |i| ((i % w as usize) as u32, (i / w as usize) as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_count_and_iteration_agree() {
        let mut b = TileBitmap::new(256, 256); // 8x8 tiles
        assert!(b.is_empty());
        b.set(7, 7);
        b.set(0, 0);
        b.set(3, 2);
        b.set(3, 2);
        b.set(8, 0); // off grid: ignored
        assert_eq!(b.count(), 3);
        assert!(b.get(3, 2) && !b.get(2, 3) && !b.get(8, 0));
        assert_eq!(b.iter().collect::<Vec<_>>(), vec![(0, 0), (3, 2), (7, 7)]);
    }

    #[test]
    fn whole_bitmap_operations_respect_the_grid() {
        let mut a = TileBitmap::new(70, 33); // 3x2 = 6 tiles
        a.set_all();
        assert_eq!(a.count(), 6, "set_all must not light bits past the grid");
        let mut b = TileBitmap::new(70, 33);
        b.set_rows(1, 5);
        assert_eq!(b.iter().collect::<Vec<_>>(), vec![(0, 1), (1, 1), (2, 1)]);
        a.clear_bits_of(&b);
        assert_eq!(a.count(), 3);
        a.or_with(&b);
        assert_eq!(a.count(), 6);
        let mut c = TileBitmap::new(70, 33);
        c.copy_from(&b);
        assert_eq!(c, b);
        // Words from the wire: stray high bits are masked, a wrong length refused.
        let w = TileBitmap::from_words(70, 33, &[u64::MAX]).unwrap();
        assert_eq!(w.count(), 6);
        assert!(TileBitmap::from_words(70, 33, &[0, 0]).is_none());
        // Another screen size is never merged.
        let mut d = TileBitmap::new(66, 40);
        d.or_with(&b);
        assert!(d.is_empty());
    }

    /// The library's feedback bits and this bitmap must agree on the layout
    /// (`y * cols + x`); a transposed or off-by-one layout would turn every
    /// verdict into misses elsewhere.
    #[test]
    fn the_library_feedback_layout_is_this_bitmaps_layout() {
        use ghostframe_lib::{FeedbackKind, FrameFeedback};
        // 100x70 px: 4 x 3 tiles; flat index = y * 4 + x.
        let mut want = TileBitmap::new(100, 70);
        for (x, y) in [(0, 0), (3, 0), (1, 2), (3, 2)] {
            want.set(x, y);
        }
        let flat: Vec<u32> = want.iter().map(|(x, y)| y * 4 + x).collect();
        let fb = FrameFeedback::from_dirty(1, 100, 70, &flat);
        let FeedbackKind::Dirty(words) = fb.kind else {
            panic!("not saturated");
        };
        assert_eq!(TileBitmap::from_words(100, 70, &words), Some(want));
    }

    #[test]
    fn a_partial_edge_tile_exists_and_the_grid_is_clipped() {
        // 70 px = 3 tile columns (the third partial).
        let mut b = TileBitmap::new(70, 33);
        assert_eq!(b.grid(), (3, 2));
        b.set_rect(0, 0, 4000, 4000);
        assert_eq!(b.count(), 6, "a huge rect clips to the grid");
        assert!(b.matches(70, 33) && !b.matches(129, 33));
        // Same 3x2 grid, different screen.
        assert_eq!(b.grid(), TileBitmap::new(66, 40).grid());
        assert!(!b.matches(66, 40));
    }

    #[test]
    fn rect_boundaries_match_the_tile_grid() {
        let tiles = |x, y, w, h| {
            let mut b = TileBitmap::new(256, 256);
            b.set_rect(x, y, w, h);
            b.iter().collect::<Vec<_>>()
        };
        assert_eq!(tiles(0, 0, 32, 32), vec![(0, 0)]);
        assert_eq!(tiles(31, 0, 2, 1), vec![(0, 0), (1, 0)]);
        assert_eq!(tiles(-10, 0, 40, 1), vec![(0, 0), (1, 0)]);
        assert!(tiles(32, 32, 0, 0).is_empty());
    }
}
