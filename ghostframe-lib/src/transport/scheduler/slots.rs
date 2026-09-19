//! Dense per-tile delivery state, indexed `tile_y * cols + tile_x`.
//!
//! Replaces `Scheduler::generations: Vec<u8>` and the
//! `cdf53_passes_acked: HashMap<(u8, u8, u8), u16>` it sat beside. Folding
//! the ACK bitmap into the slot removes the full-map scan `bump_generation`
//! did on every bump, and makes an ACK for a stale generation a comparison
//! rather than a lookup.
//!
//! At most one generation per tile is live — `bump_generation` supersedes
//! the rest — so a single `current_gen` plus one mask is sufficient.

use crate::transport::scheduler::Handle;

/// Cdf53 emits 14 progressive passes; single-pass codecs use index 0.
pub const PASS_SLOTS: usize = 14;

/// Generations are 4 bits on the wire.
const GENERATION_MASK: u8 = 0x0F;

#[derive(Debug, Clone)]
pub struct TileSlot {
    pub current_gen: u8,
    pub acked_mask: u16,
    pub passes: [Option<Handle>; PASS_SLOTS],
}

impl Default for TileSlot {
    fn default() -> Self {
        Self {
            current_gen: 0,
            acked_mask: 0,
            passes: [None; PASS_SLOTS],
        }
    }
}

#[derive(Debug)]
pub struct SlotMap {
    cols: u32,
    rows: u32,
    slots: Vec<TileSlot>,
}

impl SlotMap {
    pub fn new(cols: u32, rows: u32) -> Self {
        Self {
            cols,
            rows,
            slots: vec![TileSlot::default(); (cols as usize) * (rows as usize)],
        }
    }

    pub fn resize(&mut self, cols: u32, rows: u32) {
        self.cols = cols;
        self.rows = rows;
        self.slots = vec![TileSlot::default(); (cols as usize) * (rows as usize)];
    }

    pub fn index(&self, tile_x: u8, tile_y: u8) -> Option<usize> {
        if (tile_x as u32) >= self.cols || (tile_y as u32) >= self.rows {
            return None;
        }
        Some((tile_y as usize) * (self.cols as usize) + (tile_x as usize))
    }

    fn slot(&self, tile_x: u8, tile_y: u8) -> Option<&TileSlot> {
        self.slots.get(self.index(tile_x, tile_y)?)
    }

    fn slot_mut(&mut self, tile_x: u8, tile_y: u8) -> Option<&mut TileSlot> {
        let i = self.index(tile_x, tile_y)?;
        self.slots.get_mut(i)
    }

    pub fn generation(&self, tile_x: u8, tile_y: u8) -> u8 {
        self.slot(tile_x, tile_y).map_or(0, |s| s.current_gen)
    }

    /// Advance the generation and clear all delivery state for the tile.
    /// Returns the new generation.
    pub fn bump_generation(&mut self, tile_x: u8, tile_y: u8) -> u8 {
        match self.slot_mut(tile_x, tile_y) {
            Some(s) => {
                s.current_gen = (s.current_gen.wrapping_add(1)) & GENERATION_MASK;
                s.acked_mask = 0;
                s.current_gen
            }
            None => 0,
        }
    }

    /// Record an acknowledgement. An ack naming a generation other than the
    /// tile's current one is silently ignored: it describes content that has
    /// already been superseded.
    pub fn record_ack(&mut self, tile_x: u8, tile_y: u8, generation: u8, pass_idx: u8) {
        debug_assert!(pass_idx < 16, "pass_idx {pass_idx} out of bitmap range");
        if let Some(s) = self.slot_mut(tile_x, tile_y) {
            if s.current_gen == generation {
                s.acked_mask |= 1u16 << (pass_idx & 0x0F);
            }
        }
    }

    /// True if this acknowledgement is new (not already recorded). Callers
    /// use it to avoid counting duplicates toward the delivery window.
    pub fn is_new_ack(&self, tile_x: u8, tile_y: u8, generation: u8, pass_idx: u8) -> bool {
        match self.slot(tile_x, tile_y) {
            Some(s) if s.current_gen == generation => {
                (s.acked_mask & (1u16 << (pass_idx & 0x0F))) == 0
            }
            _ => false,
        }
    }

    pub fn acked_mask(&self, tile_x: u8, tile_y: u8, generation: u8) -> u16 {
        match self.slot(tile_x, tile_y) {
            Some(s) if s.current_gen == generation => s.acked_mask,
            _ => 0,
        }
    }

    pub fn acked_count(&self, tile_x: u8, tile_y: u8, generation: u8) -> u8 {
        self.acked_mask(tile_x, tile_y, generation).count_ones() as u8
    }

    fn full_mask(max_passes: u8) -> u16 {
        if max_passes >= 16 {
            0xFFFF
        } else {
            (1u16 << max_passes) - 1
        }
    }

    pub fn fully_acked(&self, tile_x: u8, tile_y: u8, generation: u8, max_passes: u8) -> bool {
        let needed = Self::full_mask(max_passes);
        (self.acked_mask(tile_x, tile_y, generation) & needed) == needed
    }

    pub fn unacked_mask(&self, tile_x: u8, tile_y: u8, generation: u8, max_passes: u8) -> u16 {
        Self::full_mask(max_passes) & !self.acked_mask(tile_x, tile_y, generation)
    }

    pub fn handle(&self, tile_x: u8, tile_y: u8, pass_idx: u8) -> Option<Handle> {
        self.slot(tile_x, tile_y)?
            .passes
            .get(pass_idx as usize)
            .copied()
            .flatten()
    }

    pub fn set_handle(&mut self, tile_x: u8, tile_y: u8, pass_idx: u8, h: Option<Handle>) {
        if let Some(s) = self.slot_mut(tile_x, tile_y) {
            if let Some(cell) = s.passes.get_mut(pass_idx as usize) {
                *cell = h;
            }
        }
    }

    /// Drop all queued handles but keep generations and ACK state, matching
    /// `Scheduler::clear`'s documented contract: a late ACK on a stale
    /// (tile, gen) must remain a safe no-op.
    pub fn clear_handles(&mut self) {
        for s in self.slots.iter_mut() {
            s.passes = [None; PASS_SLOTS];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_is_row_major() {
        let map = SlotMap::new(4, 3);
        assert_eq!(map.index(0, 0), Some(0));
        assert_eq!(map.index(3, 0), Some(3));
        assert_eq!(map.index(0, 1), Some(4));
        assert_eq!(map.index(3, 2), Some(11));
        assert_eq!(map.index(4, 0), None, "x out of range");
        assert_eq!(map.index(0, 3), None, "y out of range");
    }

    #[test]
    fn acking_sets_only_that_pass() {
        let mut map = SlotMap::new(2, 2);
        map.record_ack(1, 1, 0, 3);
        assert_eq!(map.acked_mask(1, 1, 0), 0b1000);
        map.record_ack(1, 1, 0, 0);
        assert_eq!(map.acked_mask(1, 1, 0), 0b1001);
    }

    #[test]
    fn an_ack_for_a_stale_generation_is_ignored() {
        let mut map = SlotMap::new(2, 2);
        map.record_ack(0, 0, 0, 1);
        map.bump_generation(0, 0);
        assert_eq!(map.acked_mask(0, 0, 1), 0, "bump clears the mask");
        map.record_ack(0, 0, 0, 2);
        assert_eq!(
            map.acked_mask(0, 0, 1),
            0,
            "an ack naming the old generation must not touch the new one"
        );
    }

    #[test]
    fn bump_advances_the_generation_and_wraps_at_four_bits() {
        let mut map = SlotMap::new(1, 1);
        assert_eq!(map.generation(0, 0), 0);
        for expected in 1..=15u8 {
            assert_eq!(map.bump_generation(0, 0), expected);
        }
        assert_eq!(map.bump_generation(0, 0), 0, "4-bit generation wraps");
    }

    #[test]
    fn fully_acked_needs_every_pass_below_max() {
        let mut map = SlotMap::new(1, 1);
        for p in 0..13u8 {
            map.record_ack(0, 0, 0, p);
        }
        assert!(!map.fully_acked(0, 0, 0, 14));
        map.record_ack(0, 0, 0, 13);
        assert!(map.fully_acked(0, 0, 0, 14));
    }

    #[test]
    fn unacked_mask_is_the_complement_below_max() {
        let mut map = SlotMap::new(1, 1);
        assert_eq!(map.unacked_mask(0, 0, 0, 14), 0x3FFF);
        map.record_ack(0, 0, 0, 0);
        assert_eq!(map.unacked_mask(0, 0, 0, 14), 0x3FFE);
    }

    #[test]
    fn handles_are_stored_and_retrieved_per_pass() {
        let mut map = SlotMap::new(1, 1);
        let h = crate::transport::scheduler::Handle {
            index: 7,
            version: 1,
        };
        map.set_handle(0, 0, 2, Some(h));
        assert_eq!(map.handle(0, 0, 2), Some(h));
        assert_eq!(map.handle(0, 0, 3), None, "other passes are unaffected");
        map.set_handle(0, 0, 2, None);
        assert_eq!(map.handle(0, 0, 2), None);
    }
}
