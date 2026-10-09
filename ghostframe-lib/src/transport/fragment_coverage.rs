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
//!
//! ## DEFERRED RETIREMENT
//!
//! Tasks 25-27 migrated the per-emission *retransmit cache* role of this
//! module to `ReliableTileEmitter::RetransmitCache`. What remains here is
//! the per-(frame_seq, tile, pass) **metadata sidecar** that
//! `dispatch_ack_datagram` consumes on ACK: each entry carries
//! `(codec, palette_id, generation)` which drive
//! `scheduler.mark_acked`, `scheduler.record_cdf53_ack`,
//! `palette_table.delivered`, and `palette_table.release` /
//! `in_flight_carrying`.
//!
//! `CacheEntry` in the emitter only stores raw fragment bytes — it has
//! no equivalent sidecar today. Full retirement therefore requires:
//!   1. Widen `CacheEntry` with `codec: Codec`, `palette_id: Option<u8>`,
//!      `generation: u8`.
//!   2. Plumb those fields through `submit_one` / `submit_batch` from
//!      every caller in `drain_scheduler_into_quinn`.
//!   3. Have `dispatch_ack_datagram` consume the metadata from
//!      `RetransmitCache::remove(&key)` instead of `fragment_coverage.take()`.
//!   4. Delete this module + its callers.
//!
//! Tracked as a follow-up to Task 28 in the implementation plan; see
//! `docs/superpowers/plans/2026-06-17-reliable-tile-emitter-plan.md`.

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
    /// Whether this PalRle emission carried the palette inline.
    ///
    /// `PaletteTable::in_flight_carrying` counts emissions that are *carrying*
    /// a palette, and is only incremented for bundled ones. The acknowledgement
    /// path has to know which kind it is acknowledging or the two sides do not
    /// pair: decrementing for a thin emission takes down a count it never put
    /// up, which underflows and — worse — drops a `release` against an
    /// `acquire` that never happened.
    pub palette_bundled: bool,
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

/// One recorded emission, filed under its tile.
struct Recorded {
    frame_seq: u32,
    pass_idx: u8,
    /// Matches this record to its entry in `order`. A key can be taken and
    /// recorded again, leaving an older `order` entry behind; the stamp is
    /// how eviction tells that leftover from the live record.
    stamp: u64,
    coverage: CoverageList,
}

/// LRU-bounded map of `CoverageKey -> CoverageList`. Per-session state.
/// Eviction policy is pure LRU: when capacity is reached, the oldest-inserted
/// entry is dropped (which is equivalent to "the ACK never arrived" — the tile
/// stays in its current codec_state and re-emits on the next dirty cycle).
///
/// Stored tile-major. Both hot operations name a tile: `take` runs once per
/// acknowledged pass and `drop_cdf53_for_tile` once per dirty tile per
/// frame. Keyed flat, each of them scanned the whole map -- so a full-screen
/// change cost tiles x map-size, quadratic in the screen and growing with
/// every unacknowledged frame. A tile holds a handful of records (one per
/// pass in flight), so finding one inside its tile is a short scan.
pub struct FragmentCoverageMap {
    capacity: usize,
    len: usize,
    next_stamp: u64,
    // Insertion-order queue. Front = oldest, back = newest. Removal from
    // the map does not touch it: an entry whose record is gone (or has been
    // replaced under a newer stamp) is skipped when it reaches the front,
    // and `compact_order` bounds how many such entries can pile up.
    order: std::collections::VecDeque<(CoverageKey, u64)>,
    tiles: std::collections::HashMap<(u8, u8), SmallVec<[Recorded; 2]>>,
    #[cfg(test)]
    pub(crate) records_examined: std::cell::Cell<u64>,
}

impl FragmentCoverageMap {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "FragmentCoverageMap capacity must be > 0");
        Self {
            capacity,
            len: 0,
            next_stamp: 0,
            order: std::collections::VecDeque::new(),
            tiles: std::collections::HashMap::new(),
            #[cfg(test)]
            records_examined: std::cell::Cell::new(0),
        }
    }

    /// Record coverage for a newly-emitted tile-pass work item. If the map is
    /// at capacity, evict the oldest entry.
    pub fn record(&mut self, key: CoverageKey, coverage: CoverageList) {
        let (frame_seq, tile_x, tile_y, pass_idx) = key;
        let records = self.tiles.entry((tile_x, tile_y)).or_default();
        #[cfg(test)]
        self.records_examined
            .set(self.records_examined.get() + records.len() as u64);
        if let Some(r) = records
            .iter_mut()
            .find(|r| r.frame_seq == frame_seq && r.pass_idx == pass_idx)
        {
            // Same key re-emitted (e.g., NACK retransmit path) — overwrite
            // in place, do NOT re-queue (preserves insertion order).
            r.coverage = coverage;
            return;
        }
        let stamp = self.next_stamp;
        self.next_stamp += 1;
        records.push(Recorded {
            frame_seq,
            pass_idx,
            stamp,
            coverage,
        });
        self.len += 1;
        self.order.push_back((key, stamp));
        while self.len > self.capacity {
            let Some((oldest, stamp)) = self.order.pop_front() else {
                break;
            };
            self.remove_where(oldest, |r| r.stamp == stamp);
        }
        self.compact_order();
    }

    /// Remove the record for `key` if `matches` accepts it.
    fn remove_where(
        &mut self,
        key: CoverageKey,
        matches: impl Fn(&Recorded) -> bool,
    ) -> Option<CoverageList> {
        let (frame_seq, tile_x, tile_y, pass_idx) = key;
        let records = self.tiles.get_mut(&(tile_x, tile_y))?;
        #[cfg(test)]
        self.records_examined
            .set(self.records_examined.get() + records.len() as u64);
        let pos = records
            .iter()
            .position(|r| r.frame_seq == frame_seq && r.pass_idx == pass_idx && matches(r))?;
        let taken = records.swap_remove(pos);
        if records.is_empty() {
            self.tiles.remove(&(tile_x, tile_y));
        }
        self.len -= 1;
        Some(taken.coverage)
    }

    /// Rebuild `order` without its dead entries once they outnumber the
    /// live ones. Each rebuild is paid for by the removals that made it
    /// necessary, so the amortised cost per operation stays constant.
    fn compact_order(&mut self) {
        if self.order.len() <= 2 * self.len + 64 {
            return;
        }
        let tiles = &self.tiles;
        self.order
            .retain(|&((frame_seq, tile_x, tile_y, pass_idx), stamp)| {
                tiles.get(&(tile_x, tile_y)).is_some_and(|records| {
                    records.iter().any(|r| {
                        r.stamp == stamp && r.frame_seq == frame_seq && r.pass_idx == pass_idx
                    })
                })
            });
    }

    /// Remove and return the coverage for a key. Returns None if the entry
    /// was evicted by LRU pressure or already taken.
    pub fn take(&mut self, key: CoverageKey) -> Option<CoverageList> {
        let taken = self.remove_where(key, |_| true);
        if taken.is_some() {
            self.compact_order();
        }
        taken
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
    /// Costs the tile's own records, not the map: see the type's doc.
    pub fn drop_cdf53_for_tile(&mut self, tile_x: u8, tile_y: u8) {
        let Some(records) = self.tiles.get_mut(&(tile_x, tile_y)) else {
            return;
        };
        #[cfg(test)]
        self.records_examined
            .set(self.records_examined.get() + records.len() as u64);
        let before = records.len();
        records.retain(|r| !r.coverage.iter().any(|c| c.codec == Codec::Cdf53));
        self.len -= before - records.len();
        if records.is_empty() {
            self.tiles.remove(&(tile_x, tile_y));
        }
        self.compact_order();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Diagnostic snapshot of all live coverage entries. Used by
    /// `Scheduler::pending_refinement_snapshot` to merge with the
    /// refinement_queue view of in-flight work.
    #[cfg(feature = "cdf53-diag")]
    pub fn snapshot(&self) -> Vec<FragmentCoverage> {
        self.tiles
            .values()
            .flat_map(|records| records.iter())
            .flat_map(|r| r.coverage.iter().copied())
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
            palette_bundled: false,
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
            palette_bundled: false,
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
            palette_bundled: false,
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
        const { assert!(FRAGMENT_COVERAGE_CAPACITY >= 1000) };
        const { assert!(FRAGMENT_COVERAGE_CAPACITY <= 100_000) };
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
                palette_bundled: false,
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
                palette_bundled: palette_id.is_some(),
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
            palette_bundled: false,
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
                palette_bundled: false,
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

    fn cov(codec: Codec) -> CoverageList {
        smallvec::smallvec![FragmentCoverage {
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            pass_idx: 0,
            codec,
            palette_id: None,
            palette_bundled: false,
        }]
    }

    /// A map holding one frame of a 4K screen, plus `passes` records on
    /// tile (0, 0).
    fn loaded(other_tiles: u32, passes: u8) -> FragmentCoverageMap {
        let mut m = FragmentCoverageMap::new(FRAGMENT_COVERAGE_CAPACITY);
        for pass in 0..passes {
            m.record((1, 0, 0, pass), cov(Codec::Cdf53));
        }
        for i in 0..other_tiles {
            let (x, y) = (1 + (i % 200) as u8, (i / 200) as u8);
            m.record((1, x, y, 0), cov(Codec::Solid));
        }
        m.records_examined.set(0);
        m
    }

    /// `drop_cdf53_for_tile` runs once per dirty tile per frame. Scanning
    /// the map there made a full-screen change quadratic in the screen.
    #[test]
    fn dropping_a_tile_does_not_examine_other_tiles_records() {
        let examined = |others| {
            let mut m = loaded(others, 3);
            m.drop_cdf53_for_tile(0, 0);
            assert_eq!(m.len(), others as usize);
            m.records_examined.get()
        };
        let (few, many) = (examined(4), examined(16_000));
        assert_eq!(few, 3, "the tile's three records, and nothing else");
        assert_eq!(
            many, few,
            "drop_cdf53_for_tile examined {many} records with 16,000 other \
             tiles recorded versus {few} with 4 -- it is scanning the map"
        );
    }

    /// `take` runs once per acknowledged pass, and used to search the whole
    /// insertion queue for the key it had just removed.
    #[test]
    fn taking_a_record_does_not_examine_other_tiles_records() {
        let examined = |others| {
            let mut m = loaded(others, 3);
            assert!(m.take((1, 0, 0, 1)).is_some());
            m.records_examined.get()
        };
        let (few, many) = (examined(4), examined(16_000));
        assert_eq!(few, 3);
        assert_eq!(many, few, "take is scanning the map");
    }

    /// The insertion queue is not edited on removal, so something has to
    /// stop it growing: a session that records and acknowledges forever
    /// while one old record stays unacknowledged at the front must not
    /// accumulate a queue entry per acknowledgement.
    #[test]
    fn the_insertion_queue_stays_bounded_under_record_and_take() {
        let mut m = FragmentCoverageMap::new(1_000);
        m.record((0, 9, 9, 0), cov(Codec::Solid)); // never acknowledged
        for seq in 1..=50_000u32 {
            m.record((seq, 1, 1, 0), cov(Codec::Solid));
            assert!(m.take((seq, 1, 1, 0)).is_some());
        }
        assert_eq!(m.len(), 1);
        assert!(
            m.order.len() <= 2 * m.len() + 64,
            "{} queue entries for {} live records",
            m.order.len(),
            m.len()
        );
        assert!(m.take((0, 9, 9, 0)).is_some(), "the old record survived");
    }

    /// A key taken and recorded again leaves its first queue entry behind.
    /// When that leftover reaches the front it must be skipped, not used to
    /// evict the newer record that happens to carry the same key.
    #[test]
    fn a_stale_queue_entry_does_not_evict_the_record_that_reused_its_key() {
        let mut m = FragmentCoverageMap::new(3);
        m.record((0, 1, 1, 0), cov(Codec::Solid));
        m.record((0, 2, 2, 0), cov(Codec::Solid));
        assert!(m.take((0, 1, 1, 0)).is_some());
        m.record((0, 1, 1, 0), cov(Codec::Solid)); // newest, same key
        m.record((0, 3, 3, 0), cov(Codec::Solid));
        // At capacity: the next record evicts the oldest *live* one, (2,2).
        m.record((0, 4, 4, 0), cov(Codec::Solid));
        assert_eq!(m.len(), 3);
        assert!(m.take((0, 2, 2, 0)).is_none(), "(2,2) was the oldest");
        assert!(
            m.take((0, 1, 1, 0)).is_some(),
            "the re-recorded key survived"
        );
    }
}
