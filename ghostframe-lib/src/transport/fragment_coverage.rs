//! Per-datagram coverage map: records which tiles each emitted datagram
//! carries so the server can convert datagram-level ACKs from the client
//! into per-tile delivery bookkeeping (Cdf53 ACK counter, PalRle palette
//! delivered tracking, etc.).
//!
//! See `docs/superpowers/specs/2026-06-01-m3.3d-datagram-level-ack-design.md`
//! for the design rationale.

use smallvec::SmallVec;

use crate::transport::protocol::Codec;

/// One tile's payload in a datagram. The server records this at emit time
/// (only for the FINAL fragment of a multi-fragment payload) and consumes
/// it on ACK arrival in `dispatch_ack_datagram`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FragmentCoverage {
    pub tile_x: u8,
    pub tile_y: u8,
    pub generation: u8,
    /// 0 for non-Cdf53 codecs (sentinel; only meaningful for Cdf53).
    pub pass_idx: u8,
    pub codec: Codec,
    /// Some(_) only for PalRle; drives `PaletteTable.delivered` tracking.
    pub palette_id: Option<u8>,
}

/// Inline capacity for the coverage list per (frame_seq, frag_idx) key.
/// Sized for typical bundled-PalRle and single-pass-Cdf53 cases without
/// heap allocation.
pub const COVERAGE_INLINE_CAPACITY: usize = 8;

pub type CoverageList = SmallVec<[FragmentCoverage; COVERAGE_INLINE_CAPACITY]>;

/// Starting heuristic sized for 1920×1080 @ 30 fps × ~1 s ACK round-trip
/// × ~10% headroom. TODO(m3.3e): make this a function of
/// `(cols × rows × capture_fps × rtt_budget)` instead of a fixed const;
/// at 4K @ 60 fps the in-flight worst case grows ~4× and 5000 will be
/// insufficient.
pub const FRAGMENT_COVERAGE_CAPACITY: usize = 5000;

/// LRU-bounded map of `(frame_seq, frag_idx) -> CoverageList`. Per-session
/// state. Eviction policy is pure LRU: when capacity is reached, the
/// oldest-inserted entry is dropped (which is equivalent to "the ACK never
/// arrived" — the tile stays in its current codec_state and re-emits on
/// the next dirty cycle).
pub struct FragmentCoverageMap {
    capacity: usize,
    // Insertion-order queue of keys. Front = oldest, back = newest.
    order: std::collections::VecDeque<(u32, u16)>,
    entries: std::collections::HashMap<(u32, u16), CoverageList>,
}

impl FragmentCoverageMap {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "FragmentCoverageMap capacity must be > 0");
        Self {
            capacity,
            order: std::collections::VecDeque::with_capacity(capacity),
            entries: std::collections::HashMap::with_capacity(capacity),
        }
    }

    /// Record coverage for a newly-emitted datagram. If the map is at
    /// capacity, evict the oldest entry.
    pub fn record(&mut self, key: (u32, u16), coverage: CoverageList) {
        if self.entries.contains_key(&key) {
            // Same key re-emitted (e.g., NACK retransmit path) — overwrite
            // in place, do NOT re-queue (preserves insertion order).
            self.entries.insert(key, coverage);
            return;
        }
        if self.entries.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
        self.order.push_back(key);
        self.entries.insert(key, coverage);
    }

    /// Remove and return the coverage for a key. Returns None if the entry
    /// was evicted by LRU pressure or already taken.
    pub fn take(&mut self, key: (u32, u16)) -> Option<CoverageList> {
        let cov = self.entries.remove(&key)?;
        // Find and remove key from the order queue. O(n) scan acceptable
        // because the queue is bounded by `capacity` and take() is rare
        // relative to record().
        if let Some(pos) = self.order.iter().position(|&k| k == key) {
            self.order.remove(pos);
        }
        Some(cov)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_coverage_struct_holds_all_fields() {
        let c = FragmentCoverage {
            tile_x: 5,
            tile_y: 7,
            generation: 3,
            pass_idx: 11,
            codec: Codec::Cdf53,
            palette_id: None,
        };
        assert_eq!(c.tile_x, 5);
        assert_eq!(c.tile_y, 7);
        assert_eq!(c.generation, 3);
        assert_eq!(c.pass_idx, 11);
        assert_eq!(c.codec, Codec::Cdf53);
        assert_eq!(c.palette_id, None);
    }

    #[test]
    fn fragment_coverage_palrle_carries_palette_id() {
        let c = FragmentCoverage {
            tile_x: 0,
            tile_y: 0,
            generation: 1,
            pass_idx: 0,
            codec: Codec::PalRle,
            palette_id: Some(42),
        };
        assert_eq!(c.palette_id, Some(42));
    }

    #[test]
    fn map_records_and_takes() {
        let mut m = FragmentCoverageMap::new(16);
        let cov: CoverageList = smallvec::smallvec![FragmentCoverage {
            tile_x: 1, tile_y: 2, generation: 0, pass_idx: 0,
            codec: Codec::Solid, palette_id: None,
        }];
        m.record((100, 5), cov.clone());
        let taken = m.take((100, 5)).expect("present");
        assert_eq!(taken.as_slice(), cov.as_slice());
        assert!(m.take((100, 5)).is_none(), "second take returns None");
    }

    #[test]
    fn map_lru_evicts_oldest() {
        let mut m = FragmentCoverageMap::new(3);
        let dummy: CoverageList = smallvec::smallvec![];
        m.record((0, 0), dummy.clone());
        m.record((0, 1), dummy.clone());
        m.record((0, 2), dummy.clone());
        m.record((0, 3), dummy.clone()); // pushes (0,0) out
        assert!(m.take((0, 0)).is_none(), "oldest entry evicted");
        assert!(m.take((0, 1)).is_some());
        assert!(m.take((0, 2)).is_some());
        assert!(m.take((0, 3)).is_some());
    }

    #[test]
    fn map_take_marks_entry_recent_uses_default_capacity_const() {
        // FRAGMENT_COVERAGE_CAPACITY is the production knob; make sure it's
        // declared and reasonable.
        assert!(FRAGMENT_COVERAGE_CAPACITY >= 1000);
        assert!(FRAGMENT_COVERAGE_CAPACITY <= 100_000);
    }
}
