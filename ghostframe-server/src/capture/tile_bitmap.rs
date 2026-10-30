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

    #[cfg(test)]
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

    #[cfg(test)]
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
