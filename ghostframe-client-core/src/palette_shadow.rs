//! CPU-side mirror of which palette slots have been delivered (i.e., the
//! client has uploaded their colors to the GPU `palette_atlas` buffer).
//!
//! Port of `ghostframe-web-client/src/palette_shadow.ts`. The actual
//! palette bytes live in the GPU atlas; we only need to know "is slot N
//! populated?" to validate thin PalRLE payloads before dispatching to the
//! GPU.

/// Tracks, per palette slot (0..=255), whether it has been populated and
/// with how many colors (1..=16).
pub struct PaletteShadow {
    // counts[i] == 0 means slot i is not populated; > 0 means populated
    // with that many colors. Using a sentinel 0 (vs a separate boolean
    // array) keeps the data structure flat.
    counts: [u8; 256],
}

impl PaletteShadow {
    pub fn new() -> Self {
        PaletteShadow { counts: [0u8; 256] }
    }

    pub fn has(&self, id: u8) -> bool {
        self.counts[id as usize] != 0
    }

    pub fn count(&self, id: u8) -> u8 {
        self.counts[id as usize]
    }

    /// Records that palette slot `id` is populated with `count` colors.
    ///
    /// Invalid counts (0 or > 16) are silently ignored. Only valid counts
    /// in the range `1..=16` update the shadow.
    pub fn put(&mut self, id: u8, count: u8) {
        if (1..=16).contains(&count) {
            self.counts[id as usize] = count;
        }
    }

    pub fn clear(&mut self) {
        self.counts = [0u8; 256];
    }
}

impl Default for PaletteShadow {
    fn default() -> Self {
        Self::new()
    }
}
