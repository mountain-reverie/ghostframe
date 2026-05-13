//! PalRLE tile codec — palette-indexed nibble-packed run-length encoding.
//!
//! Server-side state: `PaletteTable` carries persistent 256-slot lifecycle
//! (ref counts, `delivered` BitSet, `in_flight_carrying` counters) under
//! the M3.2a single-client invariant. See
//! `docs/superpowers/specs/2026-05-13-palrle-codec-design.md`.

use std::collections::VecDeque;

/// Maximum colors in a PalRLE palette. Tiles with more unique colors fall
/// through the classifier to BC1/Cdf53.
pub const MAX_PALETTE_COUNT: usize = 16;

/// Persistent palette table capacity.
pub const PALETTE_TABLE_SLOTS: usize = 256;

/// Canonical palette entry. Colors are BGRA-ascending sorted (matching the
/// canonical sort appended to `tile_analysis.comp`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PaletteEntry {
    pub colors: [[u8; 4]; MAX_PALETTE_COUNT],
    pub count: u8,
}

/// Per-slot lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Empty,
    Held,
    FreeButCached,
}

/// Per-frame palette-table activity, surfaced via tracing.
#[derive(Debug, Default, Clone, Copy)]
pub struct FramePaletteStats {
    pub reused_or_allocated: u32,
    pub fell_back_to_raw: u32,
}

/// Bitset over `PALETTE_TABLE_SLOTS` slots. Plain `[bool; 256]` form chosen
/// for simplicity (32 bytes) — no external crate needed.
#[derive(Debug, Clone, Copy)]
pub struct PaletteIdBitSet {
    bits: [bool; PALETTE_TABLE_SLOTS],
}

impl PaletteIdBitSet {
    pub fn new() -> Self {
        Self {
            bits: [false; PALETTE_TABLE_SLOTS],
        }
    }
    pub fn contains(&self, id: u8) -> bool {
        self.bits[id as usize]
    }
    pub fn insert(&mut self, id: u8) {
        self.bits[id as usize] = true;
    }
    pub fn remove(&mut self, id: u8) {
        self.bits[id as usize] = false;
    }
    pub fn clear(&mut self) {
        self.bits.fill(false);
    }
}

impl Default for PaletteIdBitSet {
    fn default() -> Self {
        Self::new()
    }
}

/// Persistent 256-slot palette table. Single-client invariant — owned
/// directly by `IoBridge`, no per-session wrapping (see design D3).
pub struct PaletteTable {
    pub entries: [Option<PaletteEntry>; PALETTE_TABLE_SLOTS],
    pub slot_state: [SlotState; PALETTE_TABLE_SLOTS],
    pub ref_count: [u32; PALETTE_TABLE_SLOTS],
    pub delivered: PaletteIdBitSet,
    pub in_flight_carrying: [u32; PALETTE_TABLE_SLOTS],
    pub free_lru: VecDeque<u8>,
    pub stats_frame: FramePaletteStats,
}

impl PaletteTable {
    pub fn new() -> Self {
        Self {
            entries: [None; PALETTE_TABLE_SLOTS],
            slot_state: [SlotState::Empty; PALETTE_TABLE_SLOTS],
            ref_count: [0; PALETTE_TABLE_SLOTS],
            delivered: PaletteIdBitSet::new(),
            in_flight_carrying: [0; PALETTE_TABLE_SLOTS],
            free_lru: VecDeque::new(),
            stats_frame: FramePaletteStats::default(),
        }
    }

    /// Linear scan over (Held ∪ FreeButCached) slots. Returns the first id
    /// whose `entries[id]` byte-equals `palette` (sorted-set equality —
    /// canonical sort is required upstream).
    pub fn find_matching(&self, palette: &PaletteEntry) -> Option<u8> {
        for id in 0..PALETTE_TABLE_SLOTS {
            if self.slot_state[id] == SlotState::Empty {
                continue;
            }
            if let Some(e) = &self.entries[id] {
                if e == palette {
                    return Some(id as u8);
                }
            }
        }
        None
    }

    /// Increment `ref_count[id]`; promote `FreeButCached` → `Held` if it was
    /// the entry-into-use transition. No-op for `Empty` (caller bug — debug
    /// assert).
    pub fn acquire(&mut self, id: u8) {
        debug_assert!(self.slot_state[id as usize] != SlotState::Empty);
        if self.slot_state[id as usize] == SlotState::FreeButCached {
            self.slot_state[id as usize] = SlotState::Held;
            // Remove from free_lru if present.
            self.free_lru.retain(|&x| x != id);
        }
        self.ref_count[id as usize] = self.ref_count[id as usize].saturating_add(1);
    }

    /// Decrement `ref_count[id]`; drop `Held` → `FreeButCached` at zero and
    /// push onto `free_lru` tail.
    pub fn release(&mut self, id: u8) {
        debug_assert!(self.slot_state[id as usize] == SlotState::Held);
        self.ref_count[id as usize] = self.ref_count[id as usize].saturating_sub(1);
        if self.ref_count[id as usize] == 0 {
            self.slot_state[id as usize] = SlotState::FreeButCached;
            self.free_lru.push_back(id);
        }
    }

    /// A slot's bytes can be safely overwritten when no in-flight tile
    /// could observe a mismatch between the bytes the server sent and the
    /// bytes currently in the slot. Two sufficient conditions, per design D4:
    /// - `delivered[id] == true` (client has rendered using these bytes)
    /// - `in_flight_carrying[id] == 0` AND `delivered[id] == false`
    ///   (bytes never reached the wire)
    /// Combined with the always-required `ref_count == 0` precondition.
    pub fn overwrite_eligible(&self, id: u8) -> bool {
        if self.ref_count[id as usize] != 0 {
            return false;
        }
        self.delivered.contains(id) || self.in_flight_carrying[id as usize] == 0
    }

    /// Replace the slot's bytes and reset the per-slot tracking state.
    /// Caller must have verified `overwrite_eligible` (debug assert here).
    pub fn write_bytes(&mut self, id: u8, palette: &PaletteEntry) {
        debug_assert!(self.overwrite_eligible(id) || self.slot_state[id as usize] == SlotState::Empty);
        self.entries[id as usize] = Some(*palette);
        self.slot_state[id as usize] = SlotState::Held;
        self.ref_count[id as usize] = 0;
        self.delivered.remove(id);
        self.in_flight_carrying[id as usize] = 0;
        // Make sure stale free_lru entries don't linger.
        self.free_lru.retain(|&x| x != id);
    }

    /// First Empty slot by id ascending. None if every slot is non-Empty.
    pub fn find_empty_slot(&self) -> Option<u8> {
        for id in 0..PALETTE_TABLE_SLOTS {
            if self.slot_state[id] == SlotState::Empty {
                return Some(id as u8);
            }
        }
        None
    }

    /// Oldest FreeButCached slot that passes `overwrite_eligible`, in LRU
    /// age order. Ineligible entries are skipped but not removed — they may
    /// become eligible later when their in-flight ACK arrives.
    pub fn find_eligible_free_slot(&self) -> Option<u8> {
        // We walk in age order without mutating self — find first hit.
        for &id in &self.free_lru {
            if self.slot_state[id as usize] == SlotState::FreeButCached
                && self.overwrite_eligible(id)
            {
                return Some(id);
            }
        }
        None
    }
}

impl Default for PaletteTable {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PalRleDecodeError {
    #[error("payload too short: needed {needed} bytes at offset {offset}, got {got}")]
    Truncated { needed: usize, offset: usize, got: usize },

    #[error("thin payload references uncached palette id {0}")]
    UncachedPalette(u8),

    #[error("palette index {index} >= count {count}")]
    IndexOutOfRange { index: u8, count: u8 },

    #[error("decoded {decoded} pixels, expected 1024")]
    PixelCountMismatch { decoded: u32 },

    #[error("palette count {0} exceeds MAX_PALETTE_COUNT")]
    PaletteCountOutOfRange(u8),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_entry_default_is_empty() {
        let p = PaletteEntry::default();
        assert_eq!(p.count, 0);
        assert_eq!(p.colors[0], [0, 0, 0, 0]);
    }

    #[test]
    fn slot_state_variants_distinct() {
        assert_ne!(SlotState::Empty, SlotState::Held);
        assert_ne!(SlotState::Held, SlotState::FreeButCached);
    }

    fn make_palette(colors: &[[u8; 4]]) -> PaletteEntry {
        let mut e = PaletteEntry::default();
        for (i, c) in colors.iter().enumerate() {
            e.colors[i] = *c;
        }
        e.count = colors.len() as u8;
        e
    }

    #[test]
    fn empty_table_has_no_matches() {
        let t = PaletteTable::new();
        let p = make_palette(&[[10, 20, 30, 255]]);
        assert_eq!(t.find_matching(&p), None);
    }

    #[test]
    fn acquire_promotes_free_but_cached_to_held() {
        let mut t = PaletteTable::new();
        let p = make_palette(&[[10, 20, 30, 255], [40, 50, 60, 255]]);
        // Manually inject for this test — full allocate ladder comes in Task 4.
        t.entries[7] = Some(p);
        t.slot_state[7] = SlotState::FreeButCached;
        t.acquire(7);
        assert_eq!(t.slot_state[7], SlotState::Held);
        assert_eq!(t.ref_count[7], 1);
    }

    #[test]
    fn release_drops_to_free_but_cached_at_zero() {
        let mut t = PaletteTable::new();
        let p = make_palette(&[[1, 2, 3, 255]]);
        t.entries[3] = Some(p);
        t.slot_state[3] = SlotState::FreeButCached;
        t.acquire(3);
        t.acquire(3);
        t.release(3);
        assert_eq!(t.slot_state[3], SlotState::Held);
        assert_eq!(t.ref_count[3], 1);
        t.release(3);
        assert_eq!(t.slot_state[3], SlotState::FreeButCached);
        assert_eq!(t.ref_count[3], 0);
        assert!(t.free_lru.contains(&3), "release at ref_count=0 must push id to free_lru");
    }

    #[test]
    fn acquire_removes_id_from_free_lru() {
        let mut t = PaletteTable::new();
        let p1 = make_palette(&[[1, 1, 1, 255]]);
        let p2 = make_palette(&[[2, 2, 2, 255]]);
        // Two FreeButCached slots in free_lru.
        t.entries[10] = Some(p1);
        t.slot_state[10] = SlotState::FreeButCached;
        t.free_lru.push_back(10);
        t.entries[20] = Some(p2);
        t.slot_state[20] = SlotState::FreeButCached;
        t.free_lru.push_back(20);
        // Re-acquire slot 10; it must leave free_lru while 20 stays.
        t.acquire(10);
        assert!(!t.free_lru.contains(&10), "acquired slot must be removed from free_lru");
        assert!(t.free_lru.contains(&20), "other free slot must remain");
        assert_eq!(t.slot_state[10], SlotState::Held);
        assert_eq!(t.ref_count[10], 1);
    }

    #[test]
    fn find_matching_hits_held_slots() {
        let mut t = PaletteTable::new();
        let p = make_palette(&[[10, 20, 30, 255], [40, 50, 60, 255]]);
        t.entries[5] = Some(p);
        t.slot_state[5] = SlotState::Held;
        assert_eq!(t.find_matching(&p), Some(5));
    }

    #[test]
    fn find_matching_hits_free_but_cached_slots() {
        let mut t = PaletteTable::new();
        let p = make_palette(&[[10, 20, 30, 255]]);
        t.entries[9] = Some(p);
        t.slot_state[9] = SlotState::FreeButCached;
        assert_eq!(t.find_matching(&p), Some(9));
    }

    #[test]
    fn find_matching_ignores_empty_slots() {
        let mut t = PaletteTable::new();
        let p = make_palette(&[[10, 20, 30, 255]]);
        // entries[4] left None; slot_state[4] is Empty.
        t.entries[4] = None;
        t.slot_state[4] = SlotState::Empty;
        assert_eq!(t.find_matching(&p), None);
    }

    #[test]
    fn overwrite_eligible_requires_zero_ref_count() {
        let mut t = PaletteTable::new();
        let p = make_palette(&[[1, 2, 3, 255]]);
        t.entries[2] = Some(p);
        t.slot_state[2] = SlotState::Held;
        t.ref_count[2] = 1;
        assert!(!t.overwrite_eligible(2), "ref_count > 0 must block overwrite");
    }

    #[test]
    fn overwrite_eligible_passes_when_delivered() {
        let mut t = PaletteTable::new();
        t.entries[3] = Some(make_palette(&[[1, 2, 3, 255]]));
        t.slot_state[3] = SlotState::FreeButCached;
        t.delivered.insert(3);
        assert!(t.overwrite_eligible(3));
    }

    #[test]
    fn overwrite_eligible_passes_when_never_sent() {
        let mut t = PaletteTable::new();
        t.entries[4] = Some(make_palette(&[[1, 2, 3, 255]]));
        t.slot_state[4] = SlotState::FreeButCached;
        // delivered = false, in_flight_carrying = 0  → never-sent case.
        assert!(t.overwrite_eligible(4));
    }

    #[test]
    fn overwrite_eligible_blocked_when_in_flight_not_delivered() {
        let mut t = PaletteTable::new();
        t.entries[5] = Some(make_palette(&[[1, 2, 3, 255]]));
        t.slot_state[5] = SlotState::FreeButCached;
        t.in_flight_carrying[5] = 1;
        assert!(!t.overwrite_eligible(5));
    }

    #[test]
    fn write_bytes_replaces_entry_and_resets_to_held() {
        let mut t = PaletteTable::new();
        let new_pal = make_palette(&[[9, 9, 9, 255], [10, 10, 10, 255]]);
        t.write_bytes(11, &new_pal);
        assert_eq!(t.entries[11], Some(new_pal));
        assert_eq!(t.slot_state[11], SlotState::Held);
        assert_eq!(t.ref_count[11], 0);
        assert!(!t.delivered.contains(11));
        assert_eq!(t.in_flight_carrying[11], 0);
    }

    #[test]
    fn find_empty_slot_returns_lowest_empty() {
        let mut t = PaletteTable::new();
        // Mark slot 0 as Held to force scan past it.
        t.entries[0] = Some(make_palette(&[[1, 1, 1, 255]]));
        t.slot_state[0] = SlotState::Held;
        assert_eq!(t.find_empty_slot(), Some(1));
    }

    #[test]
    fn find_empty_slot_returns_none_when_full() {
        let mut t = PaletteTable::new();
        for id in 0..PALETTE_TABLE_SLOTS {
            t.slot_state[id] = SlotState::Held;
            t.entries[id] = Some(make_palette(&[[id as u8, 0, 0, 255]]));
        }
        assert_eq!(t.find_empty_slot(), None);
    }

    #[test]
    fn find_eligible_free_slot_picks_lru_head() {
        let mut t = PaletteTable::new();
        for id in [7u8, 13, 22] {
            t.entries[id as usize] = Some(make_palette(&[[id, 0, 0, 255]]));
            t.slot_state[id as usize] = SlotState::FreeButCached;
            t.delivered.insert(id); // make all overwrite-eligible
            t.free_lru.push_back(id);
        }
        assert_eq!(t.find_eligible_free_slot(), Some(7));
    }

    #[test]
    fn find_eligible_free_slot_skips_ineligible_entries() {
        let mut t = PaletteTable::new();
        // 7: in_flight, not delivered → ineligible
        t.entries[7] = Some(make_palette(&[[7, 0, 0, 255]]));
        t.slot_state[7] = SlotState::FreeButCached;
        t.in_flight_carrying[7] = 1;
        t.free_lru.push_back(7);
        // 13: delivered → eligible
        t.entries[13] = Some(make_palette(&[[13, 0, 0, 255]]));
        t.slot_state[13] = SlotState::FreeButCached;
        t.delivered.insert(13);
        t.free_lru.push_back(13);
        assert_eq!(t.find_eligible_free_slot(), Some(13));
    }
}
