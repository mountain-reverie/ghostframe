//! Per-session retransmit cache. Holds emitted fragments by EmitKey for
//! re-emission on RTO / NACK. Bounded by LRU.

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

impl RetransmitCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            lru: LruCache::new(NonZeroUsize::new(CACHE_CAPACITY).unwrap()),
            stats: CacheStats::default(),
        }
    }

    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    /// Insert a fresh entry. If the cache is at capacity, evicts the LRU
    /// entry and bumps `stats.lru_eviction`.
    pub fn insert(&mut self, key: EmitKey, entry: CacheEntry) {
        if self.entries.len() >= CACHE_CAPACITY {
            if let Some((evicted, _)) = self.lru.pop_lru() {
                self.entries.remove(&evicted);
                self.stats.lru_eviction = self.stats.lru_eviction.saturating_add(1);
            }
        }
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
    fn lru_eviction_bumps_stat_when_at_capacity() {
        let mut c = RetransmitCache::new();
        let now = Instant::now();
        for i in 0..(CACHE_CAPACITY as u32) {
            c.insert(EmitKey::new(i, 0, 0, 0), mk_entry(now));
        }
        assert_eq!(c.stats.lru_eviction, 0);
        c.insert(EmitKey::new(99999, 0, 0, 0), mk_entry(now));
        assert_eq!(c.stats.lru_eviction, 1);
        assert_eq!(c.len(), CACHE_CAPACITY);
    }
}
