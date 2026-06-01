//! Per-tile-pass coverage map: records which tile-pass each emitted work item
//! carries so the server can convert per-tile-pass ACKs from the client into
//! per-tile delivery bookkeeping (Cdf53 ACK counter, PalRle palette delivered
//! tracking, etc.).
//!
//! Key: `(frame_seq, tile_x, tile_y, pass_idx)` — unique per tile-pass
//! emission within a frame. This avoids the collision that occurs with a
//! `(frame_seq, frag_idx)` key when multiple single-fragment work items per
//! frame all get frag_idx=0.
//!
//! See `docs/superpowers/specs/2026-06-01-m3.3d-datagram-level-ack-design.md`
//! for the design rationale.

use smallvec::SmallVec;

use crate::transport::protocol::Codec;

/// One tile's payload in a work item. The server records this at emit time
/// and consumes it on ACK arrival in `dispatch_ack_datagram`.
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

/// Inline capacity for the coverage list per key.
/// Sized for typical bundled-PalRle and single-pass-Cdf53 cases without
/// heap allocation.
pub const COVERAGE_INLINE_CAPACITY: usize = 8;

pub type CoverageList = SmallVec<[FragmentCoverage; COVERAGE_INLINE_CAPACITY]>;

/// Coverage map key: `(frame_seq, tile_x, tile_y, pass_idx)`.
/// Unique per tile-pass emission within a frame.
pub type CoverageKey = (u32, u8, u8, u8);

/// Coverage map capacity. Sized to hold all in-flight passes for a full
/// 1920×1080 Cdf53 emission cycle with headroom: 2040 tiles × 14 passes ×
/// 2 frames of RTT ≈ 57,120 entries. Round up to 60,000.
///
/// At 5,000 the map evicts entries faster than ACKs arrive, causing
/// `ack_entry_miss` on more than half of all ACK datagrams and permanently
/// blocking `tile_fully_acked` from returning true.
///
/// TODO(m3.3e): make this a function of `(cols × rows × max_passes × rtt_frames)`
/// instead of a fixed const; at 4K @ 60 fps the worst case grows ~8× further.
pub const FRAGMENT_COVERAGE_CAPACITY: usize = 60_000;

/// LRU-bounded map of `CoverageKey -> CoverageList`. Per-session state.
/// Eviction policy is pure LRU: when capacity is reached, the oldest-inserted
/// entry is dropped (which is equivalent to "the ACK never arrived" — the tile
/// stays in its current codec_state and re-emits on the next dirty cycle).
pub struct FragmentCoverageMap {
    capacity: usize,
    // Insertion-order queue of keys. Front = oldest, back = newest.
    order: std::collections::VecDeque<CoverageKey>,
    entries: std::collections::HashMap<CoverageKey, CoverageList>,
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

    /// Record coverage for a newly-emitted tile-pass work item. If the map is
    /// at capacity, evict the oldest entry.
    pub fn record(&mut self, key: CoverageKey, coverage: CoverageList) {
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
    pub fn take(&mut self, key: CoverageKey) -> Option<CoverageList> {
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

    /// Diagnostic snapshot of all live coverage entries. Used by
    /// `Scheduler::pending_refinement_snapshot` to merge with the
    /// refinement_queue view of in-flight work.
    #[cfg(feature = "cdf53-diag")]
    pub fn snapshot(&self) -> Vec<FragmentCoverage> {
        self.entries
            .values()
            .flat_map(|list| list.iter().copied())
            .collect()
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
        // key: (frame_seq=100, tile_x=1, tile_y=2, pass_idx=0)
        m.record((100, 1, 2, 0), cov.clone());
        let taken = m.take((100, 1, 2, 0)).expect("present");
        assert_eq!(taken.as_slice(), cov.as_slice());
        assert!(m.take((100, 1, 2, 0)).is_none(), "second take returns None");
    }

    #[test]
    fn map_lru_evicts_oldest() {
        let mut m = FragmentCoverageMap::new(3);
        let dummy: CoverageList = smallvec::smallvec![];
        m.record((0, 0, 0, 0), dummy.clone());
        m.record((0, 0, 0, 1), dummy.clone());
        m.record((0, 0, 0, 2), dummy.clone());
        m.record((0, 0, 0, 3), dummy.clone()); // pushes (0,0,0,0) out
        assert!(m.take((0, 0, 0, 0)).is_none(), "oldest entry evicted");
        assert!(m.take((0, 0, 0, 1)).is_some());
        assert!(m.take((0, 0, 0, 2)).is_some());
        assert!(m.take((0, 0, 0, 3)).is_some());
    }

    #[test]
    fn map_take_marks_entry_recent_uses_default_capacity_const() {
        // FRAGMENT_COVERAGE_CAPACITY is the production knob; make sure it's
        // declared and reasonable.
        assert!(FRAGMENT_COVERAGE_CAPACITY >= 1000);
        assert!(FRAGMENT_COVERAGE_CAPACITY <= 100_000);
    }

    #[test]
    fn map_different_passes_same_tile_no_collision() {
        let mut m = FragmentCoverageMap::new(16);
        let make_cov = |pass_idx: u8| -> CoverageList {
            smallvec::smallvec![FragmentCoverage {
                tile_x: 5, tile_y: 3, generation: 1, pass_idx,
                codec: Codec::Cdf53, palette_id: None,
            }]
        };
        // Record all 14 passes of tile (5,3) in frame 42 — no collision.
        for p in 0..14u8 {
            m.record((42, 5, 3, p), make_cov(p));
        }
        assert_eq!(m.len(), 14);
        for p in 0..14u8 {
            let taken = m.take((42, 5, 3, p)).expect("present");
            assert_eq!(taken[0].pass_idx, p);
        }
        assert_eq!(m.len(), 0);
    }
}
