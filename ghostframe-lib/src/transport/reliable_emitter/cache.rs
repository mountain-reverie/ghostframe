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

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub fragments: SmallVec<[Bytes; 2]>,
    pub wire_seqs: SmallVec<[u32; 2]>,
    pub first_sent_at: Instant,
    pub last_sent_at: Instant,
    pub attempts: u8,
    pub rto_deadline: Instant,
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
            first_sent_at: now,
            last_sent_at: now,
            attempts: 0,
            rto_deadline: now,
        }
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
