//! Per-session retransmit cache. Holds emitted fragments by EmitKey for
//! re-emission on RTO / NACK. Unbounded — entries live until ACKed,
//! superseded via `cancel_for_tile`, or the session ends via `clear`.

use crate::transport::reliable_emitter::{EmitKey, CACHE_CAPACITY};
use bytes::Bytes;
use lru::LruCache;
use smallvec::SmallVec;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::time::Instant;

/// Probe cluster a tile-pass was tagged for at emit time (BWE Stage 2.4).
/// `None` for ordinary traffic, which is the overwhelming majority — only
/// passes emitted while `IoBridge`'s `ActiveProbe` window is open carry
/// `Some`. Mirrors `queued_at`: copied through unchanged once set, since
/// `ReliableTileEmitter::tick`'s RTO retransmit path resends the cached
/// bytes directly (`self.queue.push_source`) without going through
/// `submit_one` again, so a retransmitted pass keeps whatever tag (or lack
/// of one) it was originally submitted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeTag {
    /// goog_cc's `ProbeClusterConfig::id`, echoed onto
    /// `PacedPacketInfo::probe_cluster_id`.
    pub id: i32,
    /// From the config's `target_probe_count`; the estimator discards the
    /// cluster if fewer packets than this are acknowledged.
    pub min_probes: i64,
    /// Derived `target_data_rate x target_duration`; the estimator
    /// discards the cluster if fewer bytes than this are acknowledged.
    pub min_bytes: i64,
    /// The active probe's cumulative tagged-byte count *before* this
    /// packet — goog_cc's `probe_cluster_bytes_sent` semantics, not the
    /// cluster's eventual final total. Getting this wrong (e.g. using the
    /// total-after-this-packet, or the window's final count) is silent:
    /// nothing asserts on it downstream, it just skews the estimator's
    /// send-rate calculation.
    pub bytes_sent_before: i64,
}

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub fragments: SmallVec<[Bytes; 2]>,
    pub wire_seqs: SmallVec<[u32; 2]>,
    /// When the underlying `TileWork` became available to the scheduler
    /// (`TileWork::queued_at`), copied through unchanged across
    /// retransmits (unlike `last_sent_at`) — it's the start point for
    /// `queued_at -> ACK` latency (BWE Stage 2.1), which captures
    /// scheduler queueing delay that `last_sent_at -> ACK` (Stage 2.0)
    /// cannot see.
    pub queued_at: Instant,
    pub first_sent_at: Instant,
    pub last_sent_at: Instant,
    pub attempts: u8,
    pub rto_deadline: Instant,
    /// See `ProbeTag`'s doc comment.
    pub probe: Option<ProbeTag>,
}

pub struct RetransmitCache {
    entries: HashMap<EmitKey, CacheEntry>,
    lru: LruCache<EmitKey, ()>,
    pub stats: CacheStats,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CacheStats {
    pub lru_eviction: u64,
}

impl Default for RetransmitCache {
    fn default() -> Self {
        Self::new()
    }
}

impl RetransmitCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            lru: LruCache::new(NonZeroUsize::new(CACHE_CAPACITY).unwrap()),
            stats: CacheStats::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Total wire bytes held across all unACKed entries.
    ///
    /// This is the session's in-flight data: an entry lives here from the
    /// moment its fragments go out until the ACK arrives (or the tile is
    /// cancelled/superseded). goog_cc's congestion-window pushback reads the
    /// equivalent number as `TransportPacketsFeedback::data_in_flight`, and
    /// fed zero it has nothing to push back against.
    pub fn bytes_outstanding(&self) -> usize {
        self.entries
            .values()
            .flat_map(|e| e.fragments.iter())
            .map(|f| f.len())
            .sum()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert a fresh entry. Cache has no upper bound — entries live
    /// until ACKed (`remove`), the tile is superseded
    /// (`cancel_for_tile`), or the session ends (`clear`). This is the
    /// delivery-guarantee invariant: an un-ACKed tile-pass is never
    /// silently dropped. Memory is bounded in practice by the number of
    /// in-flight unacked passes — typically a few thousand under normal
    /// conditions, up to ~50 K (~25 MB at 500 B per pass) during a
    /// 1920×1080 first-paint burst over a high-RTT link.
    pub fn insert(&mut self, key: EmitKey, entry: CacheEntry) {
        // LRU is retained for compatibility with the public `stats.lru_eviction`
        // field (still useful for spotting genuine ACK-failure cliffs in
        // the future) but never triggers eviction now.
        self.lru.put(key, ());
        self.entries.insert(key, entry);
    }

    pub fn get(&self, key: &EmitKey) -> Option<&CacheEntry> {
        self.entries.get(key)
    }

    pub fn get_mut(&mut self, key: &EmitKey) -> Option<&mut CacheEntry> {
        // Touch LRU so accessed entries don't evict.
        let _ = self.lru.get(key);
        self.entries.get_mut(key)
    }

    pub fn remove(&mut self, key: &EmitKey) -> Option<CacheEntry> {
        self.lru.pop(key);
        self.entries.remove(key)
    }

    /// Drop every entry matching `(tile_x, tile_y)` across all frame_seq /
    /// pass_idx. Used by bump_generation supersession.
    pub fn cancel_for_tile(&mut self, tile_x: u8, tile_y: u8) {
        let drop: Vec<EmitKey> = self
            .entries
            .keys()
            .filter(|k| k.tile_x == tile_x && k.tile_y == tile_y)
            .copied()
            .collect();
        for k in &drop {
            self.lru.pop(k);
            self.entries.remove(k);
        }
    }

    /// Drop every cached entry. Invoked from `IoBridge::Event::ConnectionLost`
    /// — when the WebTransport session ends, the un-ACKed passes are no
    /// longer deliverable and must not occupy memory or fire spurious
    /// retransmits for the next-connecting client.
    pub fn clear(&mut self) {
        self.entries.clear();
        // LruCache has no clear(); rebuild it.
        self.lru = LruCache::new(NonZeroUsize::new(CACHE_CAPACITY).unwrap());
    }

    /// Returns true iff at least one cache entry matches `(tile_x, tile_y)`
    /// across any frame_seq / pass_idx. Used by io_bridge's stuck-tile
    /// resweep to skip tiles the emitter is still actively retransmitting.
    pub fn has_entries_for_tile(&self, tile_x: u8, tile_y: u8) -> bool {
        self.entries
            .keys()
            .any(|k| k.tile_x == tile_x && k.tile_y == tile_y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smallvec::smallvec;

    fn mk_entry(now: Instant) -> CacheEntry {
        CacheEntry {
            fragments: smallvec![Bytes::from(vec![1, 2, 3])],
            wire_seqs: smallvec![0],
            queued_at: now,
            first_sent_at: now,
            last_sent_at: now,
            attempts: 0,
            rto_deadline: now,
            probe: None,
        }
    }

    /// `bytes_outstanding` is what reaches goog_cc as
    /// `TransportPacketsFeedback::data_in_flight`. A cache that reported
    /// zero — as this code path did until 2026-09-14, where the value was
    /// hardcoded — leaves the controller's congestion-window pushback with
    /// nothing to act on, so it silently never engages.
    #[test]
    fn bytes_outstanding_counts_unacked_fragments_and_drains_on_ack() {
        let mut c = RetransmitCache::new();
        let now = Instant::now();
        assert_eq!(c.bytes_outstanding(), 0, "an empty cache holds nothing");

        let k1 = EmitKey::new(1, 0, 0, 0);
        let k2 = EmitKey::new(1, 0, 1, 0);
        c.insert(k1, mk_entry(now)); // 3 bytes
        c.insert(k2, mk_entry(now)); // 3 bytes
        assert_eq!(
            c.bytes_outstanding(),
            6,
            "both entries' fragments are in flight"
        );

        // A multi-fragment entry must count every fragment, not just the first.
        let k3 = EmitKey::new(2, 0, 0, 0);
        c.insert(
            k3,
            CacheEntry {
                fragments: smallvec![Bytes::from(vec![0; 10]), Bytes::from(vec![0; 5])],
                wire_seqs: smallvec![1, 2],
                ..mk_entry(now)
            },
        );
        assert_eq!(c.bytes_outstanding(), 21);

        c.remove(&k3);
        c.remove(&k2);
        assert_eq!(c.bytes_outstanding(), 3, "ACKed entries stop counting");
        c.remove(&k1);
        assert_eq!(c.bytes_outstanding(), 0, "a drained cache holds nothing");
    }

    #[test]
    fn insert_lookup_remove_roundtrip() {
        let mut c = RetransmitCache::new();
        let k = EmitKey::new(1, 2, 3, 4);
        let now = Instant::now();
        c.insert(k, mk_entry(now));
        assert!(c.get(&k).is_some());
        assert_eq!(c.len(), 1);
        assert!(c.remove(&k).is_some());
        assert!(c.is_empty());
    }

    #[test]
    fn cancel_for_tile_drops_matching_only() {
        let mut c = RetransmitCache::new();
        let now = Instant::now();
        c.insert(EmitKey::new(1, 5, 5, 0), mk_entry(now));
        c.insert(EmitKey::new(2, 5, 5, 0), mk_entry(now));
        c.insert(EmitKey::new(1, 5, 6, 0), mk_entry(now));
        c.insert(EmitKey::new(1, 6, 5, 0), mk_entry(now));
        c.cancel_for_tile(5, 5);
        assert!(c.get(&EmitKey::new(1, 5, 5, 0)).is_none());
        assert!(c.get(&EmitKey::new(2, 5, 5, 0)).is_none());
        assert!(c.get(&EmitKey::new(1, 5, 6, 0)).is_some());
        assert!(c.get(&EmitKey::new(1, 6, 5, 0)).is_some());
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn has_entries_for_tile_reflects_live_entries_only() {
        let now = Instant::now();
        let mut c = RetransmitCache::new();
        let k = EmitKey {
            frame_seq: 7,
            tile_x: 3,
            tile_y: 4,
            pass_idx: 0,
        };
        assert!(!c.has_entries_for_tile(3, 4));
        c.insert(k, mk_entry(now));
        assert!(c.has_entries_for_tile(3, 4));
        assert!(!c.has_entries_for_tile(3, 5));
        c.remove(&k);
        assert!(!c.has_entries_for_tile(3, 4));
    }

    #[test]
    fn cache_grows_past_capacity_without_eviction() {
        let now = Instant::now();
        let mut c = RetransmitCache::new();
        // Insert 2× CACHE_CAPACITY entries — none should be evicted.
        let target = CACHE_CAPACITY * 2;
        for i in 0..(target as u32) {
            let k = EmitKey {
                frame_seq: i,
                tile_x: 0,
                tile_y: 0,
                pass_idx: 0,
            };
            c.insert(k, mk_entry(now));
        }
        assert_eq!(c.len(), target);
        assert_eq!(
            c.stats.lru_eviction, 0,
            "no LRU eviction — entries stay until cancel/clear"
        );
        // The very first inserted entry must still be present.
        let first = EmitKey {
            frame_seq: 0,
            tile_x: 0,
            tile_y: 0,
            pass_idx: 0,
        };
        assert!(c.get(&first).is_some());
    }

    #[test]
    fn clear_drops_all_entries() {
        let now = Instant::now();
        let mut c = RetransmitCache::new();
        for i in 0..100u32 {
            let k = EmitKey {
                frame_seq: i,
                tile_x: 0,
                tile_y: 0,
                pass_idx: 0,
            };
            c.insert(k, mk_entry(now));
        }
        assert_eq!(c.len(), 100);
        c.clear();
        assert_eq!(c.len(), 0);
        assert!(c.is_empty());
    }
}
