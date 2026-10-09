//! Per-session retransmit cache. Holds emitted fragments by EmitKey for
//! re-emission on RTO / NACK. Unbounded — entries live until ACKed,
//! superseded via `cancel_for_tile`, or the session ends via `clear`.

use crate::transport::reliable_emitter::{EmitKey, CACHE_CAPACITY};
use bytes::Bytes;
use lru::LruCache;
use smallvec::SmallVec;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

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
    /// `(tile_x, tile_y, pass_idx) -> most recently emitted EmitKey`.
    ///
    /// A NACK cannot name a transmission. An ACK echoes the `frame_seq` of a
    /// datagram the client *received*, so its `EmitKey` matches exactly; a
    /// NACK names a pass the client never got, so it has no `frame_seq` to
    /// echo and sends the tile's last-known one instead. Refinement passes
    /// for one tile are emitted across different frames, so that value is
    /// almost never the frame the missing pass was cached under -- which is
    /// why `nack_hit` read exactly 0 in the field, not merely low.
    ///
    /// This index restores the content identity a NACK actually carries.
    /// Last-write-wins: emission is chronological, so the newest entry for a
    /// `(tile, pass)` is the one worth resending.
    by_content: HashMap<(u8, u8, u8), EmitKey>,
    /// `(tile_x, tile_y) -> every cached EmitKey for that tile`.
    ///
    /// `cancel_for_tile` runs once per dirty tile per frame. Finding a
    /// tile's entries by scanning `entries` made a full-screen change cost
    /// tiles x cache-size key comparisons -- quadratic in the screen, since
    /// the cache holds a few entries per tile -- which is what made tile
    /// mode take tens of milliseconds a frame at 1080p and seconds at 4K.
    ///
    /// A tile holds one entry per unacknowledged pass, so the per-tile list
    /// is a handful of keys and removing one from it is a short scan.
    by_tile: HashMap<(u8, u8), SmallVec<[EmitKey; 4]>>,
    /// Running total behind `bytes_outstanding`, which is read on every ACK
    /// datagram and used to sum the whole cache each time. Fragments are
    /// immutable once cached, so the total only moves on insert and removal.
    bytes: usize,
    #[cfg(test)]
    pub(crate) keys_examined: std::cell::Cell<u64>,
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
            by_content: HashMap::new(),
            by_tile: HashMap::new(),
            bytes: 0,
            #[cfg(test)]
            keys_examined: std::cell::Cell::new(0),
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
        self.bytes
    }

    fn entry_bytes(entry: &CacheEntry) -> usize {
        entry.fragments.iter().map(|f| f.len()).sum()
    }

    /// Take `key` out of every index and return its entry. The one place an
    /// entry leaves the cache, so the indexes cannot drift from `entries`.
    fn evict(&mut self, key: &EmitKey) -> Option<CacheEntry> {
        let entry = self.entries.remove(key)?;
        self.bytes -= Self::entry_bytes(&entry);
        self.lru.pop(key);
        self.forget_content(key);
        let tile = (key.tile_x, key.tile_y);
        if let Some(keys) = self.by_tile.get_mut(&tile) {
            #[cfg(test)]
            self.keys_examined
                .set(self.keys_examined.get() + keys.len() as u64);
            keys.retain(|k| k != key);
            if keys.is_empty() {
                self.by_tile.remove(&tile);
            }
        }
        Some(entry)
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
        self.by_content
            .insert((key.tile_x, key.tile_y, key.pass_idx), key);
        self.bytes += Self::entry_bytes(&entry);
        match self.entries.insert(key, entry) {
            // Same key cached again: already indexed under its tile.
            Some(replaced) => self.bytes -= Self::entry_bytes(&replaced),
            None => self
                .by_tile
                .entry((key.tile_x, key.tile_y))
                .or_default()
                .push(key),
        }
    }

    /// The newest cached `EmitKey` for this tile-pass, whatever frame it was
    /// emitted in.
    ///
    /// Used only as a NACK fallback: see `by_content`'s doc comment for why a
    /// NACK's `frame_seq` cannot be trusted.
    pub fn lookup_content(&self, tile_x: u8, tile_y: u8, pass_idx: u8) -> Option<EmitKey> {
        self.by_content.get(&(tile_x, tile_y, pass_idx)).copied()
    }

    /// Drop the content index entry for `key`, but only if it still points at
    /// `key`.
    ///
    /// A newer emission of the same tile-pass has already repointed it, and
    /// clearing that would lose the live entry's only content-addressable
    /// route. Leaving a stale pointer would be worse: the fallback would hand
    /// `on_nack` a key with no cache entry, and the retransmission would be
    /// silently skipped.
    fn forget_content(&mut self, key: &EmitKey) {
        let slot = (key.tile_x, key.tile_y, key.pass_idx);
        if self.by_content.get(&slot) == Some(key) {
            self.by_content.remove(&slot);
        }
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
        self.evict(key)
    }

    /// Drop every entry matching `(tile_x, tile_y)` across all frame_seq /
    /// pass_idx. Used by bump_generation supersession.
    pub fn cancel_for_tile(&mut self, tile_x: u8, tile_y: u8) {
        let Some(keys) = self.by_tile.remove(&(tile_x, tile_y)) else {
            return;
        };
        #[cfg(test)]
        self.keys_examined
            .set(self.keys_examined.get() + keys.len() as u64);
        for k in &keys {
            if let Some(entry) = self.entries.remove(k) {
                self.bytes -= Self::entry_bytes(&entry);
            }
            self.lru.pop(k);
            self.forget_content(k);
        }
    }

    /// Drop every cached entry. Invoked from `IoBridge::Event::ConnectionLost`
    /// — when the WebTransport session ends, the un-ACKed passes are no
    /// longer deliverable and must not occupy memory or fire spurious
    /// retransmits for the next-connecting client.
    /// Remove every entry first sent longer ago than `older_than`, returning
    /// their keys.
    ///
    /// Not an attempt cap. An attempt cap was tried and removed in 0aa97de for
    /// a good reason: under sustained backpressure it abandoned passes that
    /// were still legitimately in flight. This is the aggregate bound that
    /// commit was missing — its arithmetic ("a single stuck pass retries 12
    /// times per minute") is right for one entry and wrong for thousands, and
    /// nothing bounded the total.
    ///
    /// An entry this old is not "in flight but slow". Its payload is a
    /// snapshot of the screen from `older_than` ago, so re-encoding the tile
    /// is strictly better than continuing to retransmit it: the caller pairs
    /// this with a forced re-dirty, and the fresh content supersedes it
    /// through the same path a normal screen change would take.
    pub fn retire_older_than(&mut self, now: Instant, older_than: Duration) -> Vec<EmitKey> {
        let stale: Vec<EmitKey> = self
            .entries
            .iter()
            .filter(|(_, e)| now.saturating_duration_since(e.first_sent_at) > older_than)
            .map(|(k, _)| *k)
            .collect();
        for k in &stale {
            self.evict(k);
        }
        stale
    }

    pub fn clear(&mut self) {
        self.by_content.clear();
        self.by_tile.clear();
        self.bytes = 0;
        self.entries.clear();
        // LruCache has no clear(); rebuild it.
        self.lru = LruCache::new(NonZeroUsize::new(CACHE_CAPACITY).unwrap());
    }

    /// Returns true iff at least one cache entry matches `(tile_x, tile_y)`
    /// across any frame_seq / pass_idx. Used by io_bridge's stuck-tile
    /// resweep to skip tiles the emitter is still actively retransmitting.
    pub fn has_entries_for_tile(&self, tile_x: u8, tile_y: u8) -> bool {
        self.by_tile.contains_key(&(tile_x, tile_y))
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

    /// `cancel_for_tile` runs once per dirty tile per frame, so what it
    /// costs per call decides whether a full-screen change is linear or
    /// quadratic in the screen. It must touch the tile's own entries only.
    #[test]
    fn cancelling_a_tile_does_not_examine_other_tiles_entries() {
        fn examined_with_other_tiles(others: u32) -> u64 {
            let now = Instant::now();
            let mut c = RetransmitCache::new();
            for pass in 0..3u8 {
                c.insert(EmitKey::new(1, 0, 0, pass), mk_entry(now));
            }
            for i in 0..others {
                let (x, y) = (1 + (i % 200) as u8, (i / 200) as u8);
                c.insert(EmitKey::new(1, x, y, 0), mk_entry(now));
            }
            c.keys_examined.set(0);
            c.cancel_for_tile(0, 0);
            assert_eq!(c.len(), others as usize, "only tile (0,0) is cancelled");
            c.keys_examined.get()
        }

        let few = examined_with_other_tiles(4);
        let many = examined_with_other_tiles(16_000);
        // Absolute as well as relative: a counter stuck at zero would pass
        // the comparison alone.
        assert_eq!(few, 3, "the tile's three entries, and nothing else");
        assert_eq!(
            many, few,
            "cancel_for_tile examined {many} keys with 16,000 other tiles \
             cached versus {few} with 4 -- it is scanning the cache"
        );
    }

    /// Same property for the acknowledgement path, which removes one entry.
    #[test]
    fn removing_an_entry_does_not_examine_other_tiles_entries() {
        let now = Instant::now();
        let mut c = RetransmitCache::new();
        for i in 0..16_000u32 {
            let (x, y) = ((i % 200) as u8, (i / 200) as u8);
            c.insert(EmitKey::new(1, x, y, 0), mk_entry(now));
            c.insert(EmitKey::new(1, x, y, 1), mk_entry(now));
        }
        c.keys_examined.set(0);
        assert!(c.remove(&EmitKey::new(1, 7, 7, 1)).is_some());
        assert_eq!(c.keys_examined.get(), 2, "the tile's own two entries");
    }

    /// Every way an entry can leave has to leave every index. A tile index
    /// that kept a departed key would report the tile as still held -- and
    /// the stranded-tile detector skips exactly those tiles, forever.
    #[test]
    fn the_tile_index_and_byte_total_follow_every_way_out() {
        let t0 = Instant::now();
        let mut c = RetransmitCache::new();
        let acked = EmitKey::new(1, 1, 1, 0);
        let retired = EmitKey::new(1, 2, 2, 0);
        let cancelled = EmitKey::new(1, 3, 3, 0);
        let kept = EmitKey::new(1, 4, 4, 0);
        c.insert(acked, mk_entry(t0 + Duration::from_secs(100)));
        c.insert(retired, mk_entry(t0));
        c.insert(cancelled, mk_entry(t0 + Duration::from_secs(100)));
        c.insert(kept, mk_entry(t0 + Duration::from_secs(100)));
        // Caching the same key again replaces it; it must not be counted or
        // indexed twice.
        c.insert(kept, mk_entry(t0 + Duration::from_secs(100)));
        assert_eq!(c.bytes_outstanding(), 12);

        c.remove(&acked);
        assert_eq!(
            c.retire_older_than(t0 + Duration::from_secs(101), Duration::from_secs(50)),
            vec![retired]
        );
        c.cancel_for_tile(3, 3);

        for gone in [acked, retired, cancelled] {
            assert!(!c.has_entries_for_tile(gone.tile_x, gone.tile_y));
            assert!(c.lookup_content(gone.tile_x, gone.tile_y, 0).is_none());
        }
        assert!(c.has_entries_for_tile(4, 4));
        assert_eq!(c.len(), 1);
        assert_eq!(c.bytes_outstanding(), 3);

        c.remove(&kept);
        assert!(
            !c.has_entries_for_tile(4, 4),
            "one removal clears a twice-cached key"
        );
        assert_eq!(c.bytes_outstanding(), 0);

        c.insert(kept, mk_entry(t0));
        c.clear();
        assert!(!c.has_entries_for_tile(4, 4));
        assert_eq!(c.bytes_outstanding(), 0);
    }
}
