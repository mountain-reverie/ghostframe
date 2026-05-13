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

    /// Per-design D3 single-client invariant: on new connection accept,
    /// reset all per-slot tracking but preserve slot bytes (warm cache).
    /// Next session may hit `find_matching` on persistent text colors and
    /// avoid re-bundling identical palettes from scratch.
    pub fn on_session_reset(&mut self) {
        self.delivered.clear();
        self.in_flight_carrying.fill(0);
        self.ref_count.fill(0);
        self.free_lru.clear();
        for id in 0..PALETTE_TABLE_SLOTS {
            self.slot_state[id] = if self.entries[id].is_some() {
                SlotState::FreeButCached
            } else {
                SlotState::Empty
            };
            if self.slot_state[id] == SlotState::FreeButCached {
                self.free_lru.push_back(id as u8);
            }
        }
        self.stats_frame = FramePaletteStats::default();
    }

    /// Mark a palette slot as needing rebundling on its next emission.
    /// Called when the client reports a thin payload referenced an
    /// uncached palette_id (decode-error code 3 / ERR_THIN_UNCACHED_PALETTE).
    ///
    /// Clears only the `delivered` bit; the palette bytes stay intact so
    /// subsequent encode passes can still reference them via `acquire_or_allocate`.
    pub fn force_rebundle(&mut self, palette_id: u8) {
        self.delivered.remove(palette_id);
    }

    /// Per-design Section 2 — the 4-way allocation ladder.
    /// Returns the slot id on success; `None` if every slot is `Held`
    /// or `FreeButCached` with no overwrite-eligible candidate.
    pub fn acquire_or_allocate(&mut self, palette: &PaletteEntry) -> Option<u8> {
        // 1. find_matching → reuse
        if let Some(id) = self.find_matching(palette) {
            self.acquire(id);
            return Some(id);
        }
        // 2. find_eligible_free_slot → overwrite
        if let Some(id) = self.find_eligible_free_slot() {
            self.write_bytes(id, palette);
            self.acquire(id);
            return Some(id);
        }
        // 3. find_empty_slot → write
        if let Some(id) = self.find_empty_slot() {
            self.write_bytes(id, palette);
            self.acquire(id);
            return Some(id);
        }
        // 4. fail
        None
    }
}

impl Default for PaletteTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Walk 1024 nibbles in `packed_indices` row-major (low nibble of byte 0
/// = pixel 0, high nibble of byte 0 = pixel 1, …) and emit nibble-packed
/// RLE bytes: `(index << 4) | (run_len - 1)`. Max run is 16; longer runs
/// emit multiple bytes.
pub fn encode_pal_rle_indices(packed_indices: &[u8; 512]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(64); // best case (single-color tile)
    let mut cur_idx: u8 = packed_indices[0] & 0x0F;
    let mut cur_run: u8 = 0;

    for &byte in packed_indices.iter() {
        for idx in [byte & 0x0F, byte >> 4] {
            if idx == cur_idx && cur_run < 16 {
                cur_run += 1;
            } else {
                out.push((cur_idx << 4) | (cur_run - 1));
                cur_idx = idx;
                cur_run = 1;
            }
        }
    }
    out.push((cur_idx << 4) | (cur_run - 1));
    out
}

/// Build the full PalRle wire payload: flags + palette_id +
/// optional bundled palette block + nibble-packed RLE bytes.
pub fn encode_pal_rle_payload(
    packed_indices: &[u8; 512],
    palette: &PaletteEntry,
    palette_id: u8,
    bundled: bool,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(if bundled { 256 } else { 128 });
    out.push(if bundled { 0x01 } else { 0x00 }); // flags
    out.push(palette_id);
    if bundled {
        out.push(palette.count);
        for i in 0..palette.count as usize {
            out.extend_from_slice(&palette.colors[i]);
        }
    }
    out.extend(encode_pal_rle_indices(packed_indices));
    out
}

/// Returned bundle from `decode_pal_rle` — pixel data plus, if the payload
/// was bundled, the (palette_id, palette) the client should cache.
#[derive(Debug)]
pub struct DecodedPalRle {
    pub pixels: Vec<u8>, // 1024 × 4 BGRA bytes = 4096 bytes
    pub updated_palette: Option<(u8, PaletteEntry)>,
}

/// Server-side parity decoder. Mirrors `ghostframe-web-client/src/decoder.ts`
/// `decodePalRle`. Used in roundtrip tests; not on the runtime hot path.
pub fn decode_pal_rle(
    payload: &[u8],
    cached_palette: Option<&PaletteEntry>,
) -> Result<DecodedPalRle, PalRleDecodeError> {
    let needed = |n: usize, off: usize| -> Result<(), PalRleDecodeError> {
        if payload.len() < off + n {
            Err(PalRleDecodeError::Truncated {
                needed: n,
                offset: off,
                got: payload.len().saturating_sub(off),
            })
        } else {
            Ok(())
        }
    };

    needed(2, 0)?;
    let flags = payload[0];
    let palette_id = payload[1];
    let bundled = (flags & 0x01) != 0;

    let mut cursor = 2usize;
    let palette_owned: Option<PaletteEntry> = if bundled {
        needed(1, cursor)?;
        let count = payload[cursor];
        cursor += 1;
        if count as usize > MAX_PALETTE_COUNT || count == 0 {
            return Err(PalRleDecodeError::PaletteCountOutOfRange(count));
        }
        needed(count as usize * 4, cursor)?;
        let mut p = PaletteEntry::default();
        p.count = count;
        for i in 0..count as usize {
            p.colors[i].copy_from_slice(&payload[cursor..cursor + 4]);
            cursor += 4;
        }
        Some(p)
    } else {
        None
    };

    // Resolve the palette ref we'll use for RLE expansion.
    let palette: &PaletteEntry = match (&palette_owned, cached_palette) {
        (Some(p), _) => p,
        (None, Some(p)) => p,
        (None, None) => return Err(PalRleDecodeError::UncachedPalette(palette_id)),
    };

    // Decode RLE: each byte = (idx<<4) | (run-1). Total pixels must = 1024.
    let mut pixels = vec![0u8; 1024 * 4];
    let mut pixel_idx: u32 = 0;
    while cursor < payload.len() {
        let b = payload[cursor];
        cursor += 1;
        let idx = (b >> 4) & 0x0F;
        let run_len = ((b & 0x0F) as u32) + 1;
        if idx >= palette.count {
            return Err(PalRleDecodeError::IndexOutOfRange {
                index: idx,
                count: palette.count,
            });
        }
        let mut color = palette.colors[idx as usize];
        // Per design Section 7: force alpha=255 when source is 0 (BGRX quirk).
        if color[3] == 0 {
            color[3] = 255;
        }
        if pixel_idx + run_len > 1024 {
            return Err(PalRleDecodeError::PixelCountMismatch {
                decoded: pixel_idx + run_len,
            });
        }
        for _ in 0..run_len {
            let off = (pixel_idx as usize) * 4;
            pixels[off..off + 4].copy_from_slice(&color);
            pixel_idx += 1;
        }
    }
    if pixel_idx != 1024 {
        return Err(PalRleDecodeError::PixelCountMismatch { decoded: pixel_idx });
    }

    let updated_palette = palette_owned.map(|p| (palette_id, p));
    Ok(DecodedPalRle {
        pixels,
        updated_palette,
    })
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

    #[error("palette count {0} is invalid (must be 1..=16)")]
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

    #[test]
    fn acquire_or_allocate_returns_match_when_present() {
        let mut t = PaletteTable::new();
        let p = make_palette(&[[1, 2, 3, 255]]);
        t.entries[42] = Some(p);
        t.slot_state[42] = SlotState::FreeButCached;
        let id = t.acquire_or_allocate(&p).unwrap();
        assert_eq!(id, 42);
        assert_eq!(t.slot_state[42], SlotState::Held);
        assert_eq!(t.ref_count[42], 1);
    }

    #[test]
    fn acquire_or_allocate_uses_eligible_free_slot_when_no_match() {
        let mut t = PaletteTable::new();
        // Pre-populate slot 9 with a different palette, FreeButCached + delivered.
        let old = make_palette(&[[1, 2, 3, 255]]);
        t.entries[9] = Some(old);
        t.slot_state[9] = SlotState::FreeButCached;
        t.delivered.insert(9);
        t.free_lru.push_back(9);

        // Mark all earlier slots as Held so find_empty_slot returns one >= 10
        // and the 4-way ladder picks step 2 (find_eligible_free_slot).
        for id in 0..9 {
            t.slot_state[id] = SlotState::Held;
            t.entries[id] = Some(make_palette(&[[id as u8, 7, 7, 255]]));
        }
        // Slots 10..256 are Empty; find_empty would normally return 10.
        // But ladder step 2 fires before step 3 — so slot 9 wins.
        let new_pal = make_palette(&[[99, 99, 99, 255]]);
        let id = t.acquire_or_allocate(&new_pal).unwrap();
        assert_eq!(id, 9);
        assert_eq!(t.entries[9], Some(new_pal));
        assert!(!t.delivered.contains(9));
        assert_eq!(t.slot_state[9], SlotState::Held);
        assert_eq!(t.ref_count[9], 1);
        assert!(!t.free_lru.contains(&9));
    }

    #[test]
    fn acquire_or_allocate_uses_empty_slot_when_no_match_no_eligible() {
        let mut t = PaletteTable::new();
        let p = make_palette(&[[5, 5, 5, 255]]);
        let id = t.acquire_or_allocate(&p).unwrap();
        assert_eq!(id, 0); // first empty
        assert_eq!(t.entries[0], Some(p));
        assert_eq!(t.slot_state[0], SlotState::Held);
        assert_eq!(t.ref_count[0], 1);
    }

    #[test]
    fn acquire_or_allocate_returns_none_when_full_and_all_ineligible() {
        let mut t = PaletteTable::new();
        for id in 0..PALETTE_TABLE_SLOTS {
            let p = make_palette(&[[id as u8, 0, 0, 255]]);
            t.entries[id] = Some(p);
            t.slot_state[id] = SlotState::Held;
            t.ref_count[id] = 1;
        }
        let new_pal = make_palette(&[[200, 200, 200, 255]]);
        assert_eq!(t.acquire_or_allocate(&new_pal), None);
    }

    #[test]
    fn on_session_reset_preserves_bytes_clears_tracking() {
        let mut t = PaletteTable::new();
        let p = make_palette(&[[1, 2, 3, 255]]);
        t.entries[7] = Some(p);
        t.slot_state[7] = SlotState::Held;
        t.ref_count[7] = 3;
        t.delivered.insert(7);
        t.in_flight_carrying[7] = 2;

        t.on_session_reset();

        assert_eq!(t.entries[7], Some(p), "bytes preserved (warm cache)");
        assert_eq!(t.slot_state[7], SlotState::FreeButCached);
        assert_eq!(t.ref_count[7], 0);
        assert!(!t.delivered.contains(7));
        assert_eq!(t.in_flight_carrying[7], 0);
    }

    #[test]
    fn on_session_reset_keeps_empty_slots_empty() {
        let mut t = PaletteTable::new();
        // slot 99 untouched
        t.on_session_reset();
        assert_eq!(t.slot_state[99], SlotState::Empty);
        assert!(t.entries[99].is_none());
    }

    #[test]
    fn on_session_reset_rebuilds_free_lru_with_cached_slots() {
        let mut t = PaletteTable::new();
        let p1 = make_palette(&[[1, 1, 1, 255]]);
        let p2 = make_palette(&[[2, 2, 2, 255]]);
        t.entries[5] = Some(p1);
        t.slot_state[5] = SlotState::Held;
        t.entries[8] = Some(p2);
        t.slot_state[8] = SlotState::FreeButCached;

        t.on_session_reset();

        // Both 5 and 8 become FreeButCached; free_lru contains both.
        assert!(t.free_lru.contains(&5));
        assert!(t.free_lru.contains(&8));
    }

    #[test]
    fn find_matching_still_hits_after_reset() {
        let mut t = PaletteTable::new();
        let p = make_palette(&[[10, 20, 30, 255]]);
        t.entries[42] = Some(p);
        t.slot_state[42] = SlotState::Held;
        t.delivered.insert(42);
        t.in_flight_carrying[42] = 1;

        t.on_session_reset();
        // Warm cache still finds the palette by bytes.
        assert_eq!(t.find_matching(&p), Some(42));
    }

    fn make_indices_4bit(indices: &[u8]) -> [u8; 512] {
        // Packs `indices` (one nibble each, low nibble of byte 0 first) into 512 bytes.
        assert!(indices.len() <= 1024);
        let mut out = [0u8; 512];
        for (i, &idx) in indices.iter().enumerate() {
            let byte = i / 2;
            let shift = (i % 2) * 4;
            out[byte] |= (idx & 0x0F) << shift;
        }
        out
    }

    #[test]
    fn rle_all_same_color_compresses_to_64_bytes() {
        // 1024 pixels all index 5 → 64 runs of length 16 → 64 RLE bytes.
        let indices = vec![5u8; 1024];
        let packed = make_indices_4bit(&indices);
        let rle = encode_pal_rle_indices(&packed);
        assert_eq!(rle.len(), 64);
        for &b in &rle {
            assert_eq!(b, (5 << 4) | 15, "each byte should be (idx 5, run 16)");
        }
    }

    #[test]
    fn rle_alternating_colors_emits_one_byte_per_pixel() {
        // Alternating 0, 1, 0, 1, ... — every pixel is a run-of-1.
        let indices: Vec<u8> = (0..1024).map(|i| (i % 2) as u8).collect();
        let packed = make_indices_4bit(&indices);
        let rle = encode_pal_rle_indices(&packed);
        assert_eq!(rle.len(), 1024);
        assert_eq!(rle[0], 0x00);
        assert_eq!(rle[1], 0x10);
        assert_eq!(rle[2], 0x00);
    }

    #[test]
    fn rle_run_caps_at_16() {
        let mut indices = vec![3u8; 17];
        indices.extend(vec![0u8; 1024 - 17]);
        let packed = make_indices_4bit(&indices);
        let rle = encode_pal_rle_indices(&packed);
        assert_eq!(rle[0], 0x3F);
        assert_eq!(rle[1], 0x30);
    }

    #[test]
    fn rle_pair_pattern_correct_offsets() {
        let mut indices = vec![0u8, 0, 0, 1, 1];
        indices.extend(vec![0u8; 1024 - 5]);
        let packed = make_indices_4bit(&indices);
        let rle = encode_pal_rle_indices(&packed);
        assert_eq!(rle[0], (0 << 4) | 2, "run of 3 zeros = len 3 → encoded 2");
        assert_eq!(rle[1], (1 << 4) | 1, "run of 2 ones  = len 2 → encoded 1");
    }

    #[test]
    fn payload_bundled_layout_has_flag_id_count_palette_then_rle() {
        let palette = make_palette(&[[10, 20, 30, 255], [40, 50, 60, 255]]);
        let indices = vec![0u8; 1024];
        let packed = make_indices_4bit(&indices);
        let payload = encode_pal_rle_payload(&packed, &palette, 7, true);

        assert_eq!(payload[0] & 0x01, 0x01, "bundle flag set");
        assert_eq!(payload[1], 7, "palette id");
        assert_eq!(payload[2], 2, "count = 2");
        // Colors block: 2 entries × 4 bytes = 8 bytes starting at offset 3.
        assert_eq!(&payload[3..7], &[10, 20, 30, 255]);
        assert_eq!(&payload[7..11], &[40, 50, 60, 255]);
        // RLE bytes after offset 11.
        assert!(payload.len() > 11);
    }

    #[test]
    fn payload_thin_layout_skips_palette_block() {
        let palette = make_palette(&[[10, 20, 30, 255]]);
        let indices = vec![0u8; 1024];
        let packed = make_indices_4bit(&indices);
        let payload = encode_pal_rle_payload(&packed, &palette, 42, false);

        assert_eq!(payload[0] & 0x01, 0x00, "bundle flag clear");
        assert_eq!(payload[1], 42, "palette id");
        // RLE starts immediately at offset 2.
        // All-same → 64 bytes RLE → total 66 bytes.
        assert_eq!(payload.len(), 66);
    }

    #[test]
    fn decode_bundled_extracts_palette() {
        let palette = make_palette(&[[1, 2, 3, 255], [4, 5, 6, 255]]);
        let indices: Vec<u8> = (0..1024).map(|i| (i % 2) as u8).collect();
        let packed = make_indices_4bit(&indices);
        let payload = encode_pal_rle_payload(&packed, &palette, 9, true);

        let result = decode_pal_rle(&payload, None).unwrap();
        // Pixel 0 → palette[0] = [1,2,3,255]; pixel 1 → palette[1].
        assert_eq!(&result.pixels[0..4], &[1, 2, 3, 255]);
        assert_eq!(&result.pixels[4..8], &[4, 5, 6, 255]);
        assert_eq!(result.updated_palette, Some((9, palette)));
    }

    #[test]
    fn decode_thin_requires_cached_palette() {
        let palette = make_palette(&[[10, 20, 30, 255]]);
        let indices = vec![0u8; 1024];
        let packed = make_indices_4bit(&indices);
        let payload = encode_pal_rle_payload(&packed, &palette, 5, false);

        // No cached palette → error.
        let err = decode_pal_rle(&payload, None).unwrap_err();
        assert_eq!(err, PalRleDecodeError::UncachedPalette(5));

        // With cached palette → ok.
        let result = decode_pal_rle(&payload, Some(&palette)).unwrap();
        assert_eq!(&result.pixels[0..4], &[10, 20, 30, 255]);
        assert!(result.updated_palette.is_none());
    }

    #[test]
    fn decode_rejects_index_out_of_range() {
        // Build a payload by hand: thin flag, palette_id=0, then a byte (idx=3, run=0)
        // when the cached palette only has 2 entries.
        let palette = make_palette(&[[1, 2, 3, 255], [4, 5, 6, 255]]);
        let mut payload = vec![0x00u8, 0]; // thin, id=0
        // 1024 indices = 1024 pixels worth. (idx=3, run=0) covers 1 pixel. Need 1023 more.
        payload.push((3 << 4) | 0);
        payload.push((0 << 4) | 15); // run of 16
        for _ in 0..((1024 - 1 - 16) / 16) {
            payload.push((0 << 4) | 15);
        }
        // Remainder: (1024 - 1 - 16 - (62*16)) = 1024 - 1 - 16 - 992 = 15
        payload.push((0 << 4) | 14);
        let err = decode_pal_rle(&payload, Some(&palette)).unwrap_err();
        assert!(matches!(err, PalRleDecodeError::IndexOutOfRange { .. }));
    }

    #[test]
    fn encode_decode_roundtrip_exact_pixels() {
        let palette = make_palette(&[
            [10, 20, 30, 255],
            [40, 50, 60, 255],
            [70, 80, 90, 255],
        ]);
        // 1024 pixels with arbitrary palette indices.
        let indices: Vec<u8> = (0..1024).map(|i| (i % 3) as u8).collect();
        let packed = make_indices_4bit(&indices);
        let payload = encode_pal_rle_payload(&packed, &palette, 11, true);
        let result = decode_pal_rle(&payload, None).unwrap();
        for pixel in 0..1024 {
            let expected = palette.colors[indices[pixel] as usize];
            let off = pixel * 4;
            assert_eq!(&result.pixels[off..off + 4], &expected, "pixel {}", pixel);
        }
    }

    use proptest::prelude::*;

    fn arb_palette() -> impl Strategy<Value = PaletteEntry> {
        (1u8..=16).prop_flat_map(|count| {
            prop::collection::vec(any::<[u8; 4]>(), count as usize..=count as usize)
                .prop_map(move |cols| {
                    let mut p = PaletteEntry::default();
                    for (i, c) in cols.iter().enumerate() {
                        p.colors[i] = *c;
                    }
                    p.count = count;
                    p
                })
        })
    }

    fn arb_indices_against(count: u8) -> impl Strategy<Value = [u8; 512]> {
        prop::collection::vec(0u8..count, 1024..=1024).prop_map(|v| {
            let mut out = [0u8; 512];
            for (i, &idx) in v.iter().enumerate() {
                let byte = i / 2;
                let shift = (i % 2) * 4;
                out[byte] |= (idx & 0x0F) << shift;
            }
            out
        })
    }

    #[test]
    fn force_rebundle_clears_delivered_bit() {
        let mut t = PaletteTable::new();
        let pal = PaletteEntry { count: 1, colors: [[10, 20, 30, 255]; 16] };
        // Force a slot to delivered state.
        t.write_bytes(5, &pal);
        t.delivered.insert(5);
        assert!(t.delivered.contains(5));

        t.force_rebundle(5);

        assert!(!t.delivered.contains(5),
            "force_rebundle must clear the delivered bit so the next emission re-bundles");
        // The palette content stays put — we only cleared the delivered flag.
        assert_eq!(t.entries[5].as_ref().unwrap().count, 1);
        assert_eq!(t.entries[5].as_ref().unwrap().colors[0], [10, 20, 30, 255]);
    }

    #[test]
    fn force_rebundle_no_op_on_undelivered() {
        let mut t = PaletteTable::new();
        // Slot 7 is empty / undelivered.
        assert!(!t.delivered.contains(7));
        t.force_rebundle(7);
        // Should be a no-op, no panic.
        assert!(!t.delivered.contains(7));
    }

    proptest! {
        #[test]
        fn proptest_encode_decode_bundled_roundtrip(
            (palette, packed) in arb_palette().prop_flat_map(|p| {
                let count = p.count;
                arb_indices_against(count).prop_map(move |packed| (p, packed))
            })
        ) {
            let payload = encode_pal_rle_payload(&packed, &palette, 0, true);
            let dec = decode_pal_rle(&payload, None).unwrap();

            // Reconstruct expected pixels via direct index lookup.
            let mut expected = vec![0u8; 1024 * 4];
            for pixel in 0..1024usize {
                let byte = packed[pixel / 2];
                let idx = if pixel % 2 == 0 { byte & 0x0F } else { (byte >> 4) & 0x0F };
                let mut color = palette.colors[idx as usize];
                if color[3] == 0 { color[3] = 255; }
                expected[pixel * 4..pixel * 4 + 4].copy_from_slice(&color);
            }
            prop_assert_eq!(dec.pixels, expected);
            prop_assert_eq!(dec.updated_palette, Some((0u8, palette)));
        }

        #[test]
        fn proptest_palette_table_acquire_release_balanced(
            ops in prop::collection::vec(0u8..32u8, 1..50)
        ) {
            let mut t = PaletteTable::new();
            // Pre-allocate slots 0..32 with synthetic palettes.
            for id in 0..32u8 {
                let mut p = PaletteEntry::default();
                p.colors[0] = [id, 0, 0, 255];
                p.count = 1;
                t.entries[id as usize] = Some(p);
                t.slot_state[id as usize] = SlotState::FreeButCached;
                t.free_lru.push_back(id);
                t.delivered.insert(id);
            }
            let mut acquired: Vec<u8> = Vec::new();
            for op in ops {
                t.acquire(op);
                acquired.push(op);
            }
            // Release in reverse order; ref_counts should reach zero everywhere.
            for &id in acquired.iter().rev() {
                t.release(id);
            }
            for id in 0..32 {
                prop_assert_eq!(t.ref_count[id], 0);
                prop_assert_eq!(t.slot_state[id], SlotState::FreeButCached);
            }
        }
    }
}
