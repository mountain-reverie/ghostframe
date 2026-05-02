//! Per-tile metrics storage parallel to `DirtyTracker`.
//!
//! Owns one `TileMetrics` per tile in `cols × rows` row-major order. The
//! caller is responsible for invoking `record_frame()` after every dirty-tile
//! detection cycle so `idle_frames` and `change_freq_hz` stay current.

use super::TileMetrics;

pub struct MetricsTracker {
    cols: u32,
    rows: u32,
    metrics: Vec<TileMetrics>,
}

impl MetricsTracker {
    pub fn new(cols: u32, rows: u32) -> Self {
        let count = (cols * rows) as usize;
        Self {
            cols,
            rows,
            metrics: vec![TileMetrics::default(); count],
        }
    }

    pub fn cols(&self) -> u32 { self.cols }
    pub fn rows(&self) -> u32 { self.rows }

    pub fn resize(&mut self, cols: u32, rows: u32) {
        self.cols = cols;
        self.rows = rows;
        self.metrics.clear();
        self.metrics.resize((cols * rows) as usize, TileMetrics::default());
    }

    pub fn reset(&mut self) {
        for m in &mut self.metrics {
            *m = TileMetrics::default();
        }
    }

    pub fn metrics(&self) -> &[TileMetrics] {
        &self.metrics
    }

    pub fn get(&self, tile_x: u32, tile_y: u32) -> &TileMetrics {
        &self.metrics[self.idx(tile_x, tile_y)]
    }

    pub fn get_mut(&mut self, tile_x: u32, tile_y: u32) -> &mut TileMetrics {
        let i = self.idx(tile_x, tile_y);
        &mut self.metrics[i]
    }

    fn idx(&self, tile_x: u32, tile_y: u32) -> usize {
        debug_assert!(
            tile_x < self.cols && tile_y < self.rows,
            "tile ({tile_x},{tile_y}) out of bounds for {}×{} grid",
            self.cols, self.rows,
        );
        (tile_y * self.cols + tile_x) as usize
    }

    /// Update `idle_frames` and `change_freq_hz` for every tile based on
    /// whether it appears in `dirty`.
    ///
    /// EMA: `freq = freq * 0.9 + (changed ? 60.0 : 0.0) * 0.1`.
    /// `idle_frames` reset to 0 for dirty tiles, otherwise incremented.
    pub fn record_frame(&mut self, dirty: &[(u32, u32)]) {
        let cols = self.cols;
        let mut is_dirty = vec![false; self.metrics.len()];
        for &(tx, ty) in dirty {
            if tx < cols && ty < self.rows {
                is_dirty[(ty * cols + tx) as usize] = true;
            }
        }
        for (i, m) in self.metrics.iter_mut().enumerate() {
            let dirty_now = is_dirty[i];
            let target = if dirty_now { 60.0 } else { 0.0 };
            m.change_freq_hz = m.change_freq_hz * 0.9 + target * 0.1;
            if dirty_now {
                m.idle_frames = 0;
            } else {
                m.idle_frames = m.idle_frames.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_initializes_grid_with_defaults() {
        let t = MetricsTracker::new(3, 2);
        assert_eq!(t.metrics().len(), 6);
        for m in t.metrics() {
            assert_eq!(m.idle_frames, 0);
            assert_eq!(m.change_freq_hz, 0.0);
        }
    }

    #[test]
    fn resize_resets_grid() {
        let mut t = MetricsTracker::new(2, 2);
        t.get_mut(0, 0).idle_frames = 17;
        t.resize(3, 3);
        assert_eq!(t.metrics().len(), 9);
        assert_eq!(t.get(0, 0).idle_frames, 0);
    }

    #[test]
    fn record_frame_resets_idle_for_dirty_tiles() {
        let mut t = MetricsTracker::new(2, 2);
        t.get_mut(1, 0).idle_frames = 5;
        t.record_frame(&[(1, 0)]);
        assert_eq!(t.get(1, 0).idle_frames, 0);
    }

    #[test]
    fn record_frame_increments_idle_for_clean_tiles() {
        let mut t = MetricsTracker::new(2, 2);
        t.record_frame(&[]);
        t.record_frame(&[]);
        assert_eq!(t.get(0, 0).idle_frames, 2);
    }

    #[test]
    fn record_frame_ema_climbs_for_persistently_dirty_tile() {
        let mut t = MetricsTracker::new(1, 1);
        for _ in 0..30 {
            t.record_frame(&[(0, 0)]);
        }
        // freq → 60 - 60*(0.9^30) ≈ 57.5. Bounded range catches inverted-alpha
        // bugs (alpha=0.9 climb would give exactly 60.0).
        let f = t.get(0, 0).change_freq_hz;
        assert!(f > 50.0 && f < 59.0, "EMA climb out of expected range: {f}");
    }

    #[test]
    fn record_frame_ema_decays_for_clean_tile() {
        let mut t = MetricsTracker::new(1, 1);
        // Prime to 60 Hz then go clean
        for _ in 0..30 { t.record_frame(&[(0, 0)]); }
        for _ in 0..30 { t.record_frame(&[]); }
        assert!(t.get(0, 0).change_freq_hz < 5.0);
    }

    #[test]
    fn record_frame_ignores_out_of_range_dirty_coords() {
        let mut t = MetricsTracker::new(2, 2);
        t.record_frame(&[(99, 99)]); // out of range
        // No panic, all tiles treated as clean.
        for m in t.metrics() {
            assert_eq!(m.idle_frames, 1);
        }
    }
}
