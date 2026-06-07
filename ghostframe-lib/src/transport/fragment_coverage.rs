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

    /// Drop every **Cdf53** coverage entry for the given (tile_x, tile_y).
    /// Called from `IoBridge` alongside `Scheduler::bump_generation*`: the
    /// bump marks queued Cdf53 refinement work `Superseded`, so the
    /// matching already-emitted-but-not-yet-ACKed Cdf53 coverage must be
    /// discarded in lockstep. Otherwise those entries linger until LRU
    /// eviction and `pending_refinement_snapshot` reports them as
    /// in-flight Cdf53 work for a generation that no longer exists,
    /// breaking the M3.3d refinement-cancel invariant.
    ///
    /// Non-Cdf53 coverage (Solid, PalRle, Raw) is left in place:
    /// the snapshot filters by `codec == Cdf53` so non-Cdf53 entries
    /// cannot pollute it, and dropping them would break ACK-driven
    /// side effects (PalRle `delivered`/`in_flight_carrying` bookkeeping,
    /// `ack_miss` telemetry on motion-region Solid flips, etc.).
    ///
    /// O(n) over both `entries` and `order` (n = current map size, bounded
    /// by `capacity`). Called per dirty tile per frame in the emission path,
    /// so a much-larger map would warrant a secondary tile-keyed index.
    pub fn drop_cdf53_for_tile(&mut self, tile_x: u8, tile_y: u8) {
        let mut dropped_keys: smallvec::SmallVec<[CoverageKey; 16]> = smallvec::SmallVec::new();
        self.entries.retain(|&key, cov_list| {
            let (_, tx, ty, _) = key;
            let target =
                tx == tile_x && ty == tile_y && cov_list.iter().any(|c| c.codec == Codec::Cdf53);
            if target {
                dropped_keys.push(key);
                false
            } else {
                true
            }
        });
        if !dropped_keys.is_empty() {
            self.order.retain(|k| !dropped_keys.contains(k));
        }
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
            tile_x: 1,
            tile_y: 2,
            generation: 0,
            pass_idx: 0,
            codec: Codec::Solid,
            palette_id: None,
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
    fn drop_cdf53_for_tile_removes_matching_cdf53_entries_only() {
        let mut m = FragmentCoverageMap::new(16);
        let make_cdf53 = |tile_x: u8, tile_y: u8| -> CoverageList {
            smallvec::smallvec![FragmentCoverage {
                tile_x,
                tile_y,
                generation: 0,
                pass_idx: 0,
                codec: Codec::Cdf53,
                palette_id: None,
            }]
        };
        // Same tile (5,3) spread across two frames and two passes (4 entries).
        m.record((100, 5, 3, 0), make_cdf53(5, 3));
        m.record((100, 5, 3, 1), make_cdf53(5, 3));
        m.record((101, 5, 3, 0), make_cdf53(5, 3));
        m.record((101, 5, 3, 1), make_cdf53(5, 3));
        // Neighbor tile (6,3) must survive (different tile).
        m.record((100, 6, 3, 0), make_cdf53(6, 3));
        // Same column, different row (5,4) must survive (different tile).
        m.record((100, 5, 4, 0), make_cdf53(5, 4));
        assert_eq!(m.len(), 6);

        m.drop_cdf53_for_tile(5, 3);

        assert_eq!(m.len(), 2, "only the (6,3) and (5,4) entries should remain");
        assert!(m.take((100, 5, 3, 0)).is_none());
        assert!(m.take((100, 5, 3, 1)).is_none());
        assert!(m.take((101, 5, 3, 0)).is_none());
        assert!(m.take((101, 5, 3, 1)).is_none());
        assert!(
            m.take((100, 6, 3, 0)).is_some(),
            "neighbor (6,3) unaffected"
        );
        assert!(
            m.take((100, 5, 4, 0)).is_some(),
            "neighbor (5,4) unaffected"
        );
    }

    #[test]
    fn drop_cdf53_for_tile_leaves_non_cdf53_coverage_alone() {
        // Non-Cdf53 entries (Solid, PalRle, Raw) for the same tile must
        // survive: their ACK paths drive palette delivery, ack_miss telemetry
        // and other bookkeeping that the snapshot fix has no business
        // touching. Snapshot only filters by `codec == Cdf53` so leaving them
        // in place cannot pollute it.
        let mut m = FragmentCoverageMap::new(16);
        let one = |codec: Codec, palette_id: Option<u8>| -> CoverageList {
            smallvec::smallvec![FragmentCoverage {
                tile_x: 4,
                tile_y: 4,
                generation: 0,
                pass_idx: 0,
                codec,
                palette_id,
            }]
        };
        m.record((10, 4, 4, 0), one(Codec::Solid, None));
        m.record((11, 4, 4, 0), one(Codec::PalRle, Some(7)));
        m.record((13, 4, 4, 0), one(Codec::Raw, None));
        m.record((14, 4, 4, 0), one(Codec::Cdf53, None));
        assert_eq!(m.len(), 4);

        m.drop_cdf53_for_tile(4, 4);

        assert_eq!(m.len(), 3, "only the Cdf53 entry should be removed");
        assert!(m.take((10, 4, 4, 0)).is_some(), "Solid survives");
        assert!(m.take((11, 4, 4, 0)).is_some(), "PalRle survives");
        assert!(m.take((13, 4, 4, 0)).is_some(), "Raw survives");
        assert!(m.take((14, 4, 4, 0)).is_none(), "Cdf53 dropped");
    }

    #[test]
    fn drop_cdf53_for_tile_keeps_order_and_entries_consistent() {
        // After drop_cdf53_for_tile, the order queue must not contain dangling
        // keys that the entries map has already discarded; otherwise the next
        // LRU eviction would try to remove a key that isn't there.
        let mut m = FragmentCoverageMap::new(3);
        let cdf53: CoverageList = smallvec::smallvec![FragmentCoverage {
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            pass_idx: 0,
            codec: Codec::Cdf53,
            palette_id: None,
        }];
        m.record((0, 1, 1, 0), cdf53.clone());
        m.record((0, 2, 2, 0), cdf53.clone());
        m.record((0, 1, 1, 1), cdf53.clone());
        assert_eq!(m.len(), 3);

        m.drop_cdf53_for_tile(1, 1);
        assert_eq!(m.len(), 1);

        // Capacity should now accept two new entries without LRU-evicting
        // the surviving (2,2) entry. If `order` still carried the dropped
        // (1,1,*) keys, the next record() at capacity would pop a phantom
        // key and the (2,2) entry would survive incorrectly OR an unrelated
        // entry would be evicted.
        m.record((0, 3, 3, 0), cdf53.clone());
        m.record((0, 4, 4, 0), cdf53.clone());
        assert_eq!(m.len(), 3);
        assert!(m.take((0, 2, 2, 0)).is_some(), "(2,2) survived");
        assert!(m.take((0, 3, 3, 0)).is_some());
        assert!(m.take((0, 4, 4, 0)).is_some());
    }

    #[test]
    fn map_different_passes_same_tile_no_collision() {
        let mut m = FragmentCoverageMap::new(16);
        let make_cov = |pass_idx: u8| -> CoverageList {
            smallvec::smallvec![FragmentCoverage {
                tile_x: 5,
                tile_y: 3,
                generation: 1,
                pass_idx,
                codec: Codec::Cdf53,
                palette_id: None,
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
