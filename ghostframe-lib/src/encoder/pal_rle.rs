//! PalRLE tile codec — palette-indexed nibble-packed run-length encoding.
//!
//! Server-side state: `PaletteTable` carries persistent 256-slot lifecycle
//! (ref counts, `delivered` BitSet, `in_flight_carrying` counters) under
//! the M3.2a single-client invariant. See
//! `docs/superpowers/specs/2026-05-13-palrle-codec-design.md`.

use std::collections::VecDeque;

/// Maximum colors in a PalRLE palette. Tiles with more unique colors fall
/// through the classifier to Cdf53.
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
    ///   Combined with the always-required `ref_count == 0` precondition.
    pub fn overwrite_eligible(&self, id: u8) -> bool {
        if self.ref_count[id as usize] != 0 {
            return false;
        }
        self.delivered.contains(id) || self.in_flight_carrying[id as usize] == 0
    }

    /// Replace the slot's bytes and reset the per-slot tracking state.
    /// Caller must have verified `overwrite_eligible` (debug assert here).
    pub fn write_bytes(&mut self, id: u8, palette: &PaletteEntry) {
        debug_assert!(
            self.overwrite_eligible(id) || self.slot_state[id as usize] == SlotState::Empty
        );
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
        self.free_lru.iter().find(|&&id| self.slot_state[id as usize] == SlotState::FreeButCached
                && self.overwrite_eligible(id)).copied()
    }

    /// Per-design D3 single-client invariant: on new connection accept,
    /// reset all per-slot tracking but preserve slot bytes (warm cache).
    /// Next session may hit `find_matching` on persistent text colors and
    /// avoid re-bundling identical palettes from scratch.
    pub fn on_session_reset(&mut self, preserve_delivered: bool) {
        if !preserve_delivered {
            self.delivered.clear();
        }
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
        // 1. find_matching → reuse existing slot
        if let Some(id) = self.find_matching(palette) {
            self.acquire(id);
            return Some(id);
        }
        // 2. find_empty_slot → write to truly-fresh slot.
        //    Preserves existing FreeButCached entries for future
        //    find_matching hits; avoids LRU thrashing on small palette sets.
        if let Some(id) = self.find_empty_slot() {
            self.write_bytes(id, palette);
            self.acquire(id);
            return Some(id);
        }
        // 3. find_eligible_free_slot → evict oldest FreeButCached only when
        //    no truly-empty slot exists.
        if let Some(id) = self.find_eligible_free_slot() {
            self.write_bytes(id, palette);
            self.acquire(id);
            return Some(id);
        }
        // 4. fail (caller falls back to Codec::Raw for the tile)
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

/// Build the M3.2b `indices_raw` PalRLE wire payload: flags (bit 1) +
/// palette_id + raw 512-byte 4-bit-packed indices (no RLE expansion).
///
/// This is the "thin equivalent" of the bundled/thin paths but skips the
/// nibble-RLE encoding step entirely. The client must support `indices_raw`
/// (advertised via HELLO bit 0) for the server to emit this variant.
pub fn encode_pal_rle_payload_indices_raw(packed_indices: &[u8; 512], palette_id: u8) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(514);
    out.push(0x02); // flags: bit 1 = indices_raw, bit 0 = not bundled
    out.push(palette_id);
    out.extend_from_slice(packed_indices);
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
        let mut p = PaletteEntry { count, ..Default::default() };
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
    Truncated {
        needed: usize,
        offset: usize,
        got: usize,
    },

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
#[path = "pal_rle_tests.rs"]
mod tests;
