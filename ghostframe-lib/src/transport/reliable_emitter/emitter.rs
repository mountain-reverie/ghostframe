//! Central facade that ties cache, emission queue, allocator, group
//! builder, and RTO wheel into one struct. Per-session.

use crate::transport::protocol::TileParityEnvelope;
use crate::transport::reliable_emitter::cache::{CacheEntry, RetransmitCache};
use crate::transport::reliable_emitter::emission_queue::{Emission, EmissionQueue};
use crate::transport::reliable_emitter::parity::GroupBuilder;
use crate::transport::reliable_emitter::rto::{rto_for_attempt, RtoTimerWheel};
use crate::transport::reliable_emitter::traits::DatagramSender;
use crate::transport::reliable_emitter::wire_seq::WireSeqAllocator;
use crate::transport::reliable_emitter::{EmitKey, FEC_GROUP_SIZE_K, PARITY_INTERLEAVE_OFFSET};
use bytes::Bytes;
use smallvec::smallvec;
use std::time::{Duration, Instant};

pub struct ReliableTileEmitter {
    pub(crate) cache: RetransmitCache,
    pub(crate) alloc: WireSeqAllocator,
    pub(crate) queue: EmissionQueue,
    pub(crate) group: GroupBuilder,
    pub(crate) rto: RtoTimerWheel,
    pub(crate) smoothed_rtt: Duration,
    pub stats: EmitterStats,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct EmitterStats {
    pub source_emitted: u64,
    pub parity_emitted: u64,
    pub rto_fired: u64,
    pub rto_max_retransmits_reached: u64,
    pub ack_hit: u64,
    pub ack_miss: u64,
    pub nack_hit: u64,
    pub nack_miss: u64,
    pub retransmit_attempts_total: u64,
}

impl ReliableTileEmitter {
    pub fn new() -> Self {
        Self {
            cache: RetransmitCache::new(),
            alloc: WireSeqAllocator::new(),
            queue: EmissionQueue::new(),
            group: GroupBuilder::new(FEC_GROUP_SIZE_K),
            rto: RtoTimerWheel::new(),
            smoothed_rtt: Duration::from_millis(20),
            stats: EmitterStats::default(),
        }
    }

    pub fn set_smoothed_rtt(&mut self, rtt: Duration) { self.smoothed_rtt = rtt; }

    /// Submit one tile-pass with a single payload buffer. The buffer is
    /// the body of the source datagram *minus* the DatagramHeader bytes
    /// (the emitter prepends the header internally with the allocated
    /// wire_seq).
    pub fn submit_one(&mut self, key: EmitKey, source_datagram_bytes: Bytes, now: Instant) {
        let wire_seq = self.alloc.allocate();
        let mut bytes = source_datagram_bytes.to_vec();
        if bytes.len() >= 12 {
            bytes[8..12].copy_from_slice(&wire_seq.to_be_bytes());
        }
        // Cache & RTO.
        let entry = CacheEntry {
            fragments: smallvec![Bytes::from(bytes.clone())],
            wire_seqs: smallvec![wire_seq],
            first_sent_at: now,
            last_sent_at: now,
            attempts: 0,
            rto_deadline: now + rto_for_attempt(self.smoothed_rtt, 0),
        };
        self.cache.insert(key, entry);
        self.rto.schedule(key, now + rto_for_attempt(self.smoothed_rtt, 0));
        // Feed group builder; on K-th source build & schedule parity envelope.
        if let Some(result) = self.group.add(wire_seq, &bytes) {
            let envelope = TileParityEnvelope {
                group_first_wire_seq: result.group_first_wire_seq,
                k: result.k,
                parity_idx: 0,
                group_first_payload_len: result.first_len,
                parity_payload: result.parity,
            };
            let mut env_bytes = Vec::new();
            envelope.encode(&mut env_bytes);
            let emit_after = result.group_first_wire_seq.wrapping_add(PARITY_INTERLEAVE_OFFSET);
            self.queue.schedule_parity(emit_after, env_bytes);
        }
        // Enqueue the source itself.
        self.queue.push_source(bytes);
        self.stats.source_emitted += 1;
    }

    pub fn submit_batch(&mut self, items: Vec<(EmitKey, Bytes)>, now: Instant) {
        for (key, bytes) in items {
            self.submit_one(key, bytes, now);
        }
    }

    /// Ingest a batch of ACKs from the client. For each key, remove the
    /// corresponding cache entry. A hit (entry was live) bumps `ack_hit`; a
    /// miss (already evicted/cancelled) bumps `ack_miss`. Idempotent: late
    /// ACKs are silent.
    pub fn on_ack(&mut self, keys: &[EmitKey]) {
        for key in keys {
            if self.cache.remove(key).is_some() {
                self.stats.ack_hit += 1;
            } else {
                self.stats.ack_miss += 1;
            }
        }
    }

    /// Drain up to `max_retransmits` due RTO entries. For each due key,
    /// validate against the live cache (silently skip stale entries —
    /// already ACKed or cancelled). Bump `attempts` (used only for RTO
    /// backoff selection), re-enqueue every cached fragment via the
    /// EmissionQueue with the same wire_seqs (so client dedup keys
    /// remain stable), and reschedule a new RTO deadline using
    /// `rto_for_attempt`. Never retires — the entry stays in the cache
    /// until ACKed, cancelled via `cancel_for_tile`, or the session
    /// ends via `clear_cache`.
    ///
    /// Rate-limit rationale: when many entries are simultaneously due
    /// (typical when sustained loss has filled the cache with thousands
    /// of pending passes), firing them all in one `tick()` call hands
    /// quinn far more datagrams than its `datagram_send_buffer` can
    /// absorb, generating `SendDatagramError::Blocked` on the rejected
    /// ones (they're silently dropped from the wire — the cache still
    /// has the entry, so RTO will fire it again, perpetuating a storm
    /// where the link can never settle). Capping at a small number per
    /// call converts that storm into orderly drip-feed: heap entries
    /// not popped this tick stay due, and the next call pops the next
    /// batch. Pairs with the `Event::DatagramsUnblocked` continuation
    /// in `IoBridge::run` which calls this again whenever quinn frees
    /// up datagram-buffer space.
    pub fn tick(&mut self, now: Instant, max_retransmits: usize) {
        let mut count = 0;
        while count < max_retransmits {
            let Some(key) = self.rto.pop_due(now) else { break; };
            let Some(entry) = self.cache.get_mut(&key) else {
                // Already ACKed or cancelled — stale heap entry; skip silently.
                continue;
            };
            // Bump attempts, re-enqueue every cached fragment, reschedule RTO.
            entry.attempts += 1;
            entry.last_sent_at = now;
            let new_rto = rto_for_attempt(self.smoothed_rtt, entry.attempts);
            entry.rto_deadline = now + new_rto;
            let frags: Vec<Vec<u8>> = entry.fragments.iter().map(|b| b.to_vec()).collect();
            // [H3-DIAG] log every actual retransmit. Captures the EmitKey
            // routing tuple + a fingerprint of the first fragment so a
            // stale-bundle replay (e.g. a PalRle bundled payload whose slot
            // id has since been reassigned to a different palette) shows
            // up as: same (tile_x, tile_y) replayed across many frame_seqs
            // with the same fingerprint, while the live palette has moved on.
            let attempts = entry.attempts;
            let frag_count = frags.len();
            let (fp, codec_byte, palette_id_byte, flags_byte) = first_fragment_fp(&frags);
            tracing::trace!(
                target: "emitter.h3",
                frame_seq = key.frame_seq,
                tile_x = key.tile_x,
                tile_y = key.tile_y,
                pass_idx = key.pass_idx,
                attempts = attempts,
                frag_count = frag_count,
                codec = codec_byte,
                palrle_flags = flags_byte,
                palette_id = palette_id_byte,
                fp = format_args!("{:08x}", fp),
                "[H3-SRV] retransmit"
            );
            for bytes in frags {
                self.queue.push_source(bytes);
            }
            self.rto.schedule(key, now + new_rto);
            self.stats.rto_fired += 1;
            self.stats.retransmit_attempts_total += 1;
            count += 1;
        }
    }

    /// Number of cache entries currently held — i.e. unACKed tile-passes
    /// awaiting either an ACK, a `cancel_for_tile`, or session end.
    /// Diagnostic accessor for the cumulative-emit log.
    pub fn pending_cache_entries(&self) -> usize {
        self.cache.len()
    }

    /// Ingest a batch of NACKs from the client. For each (key, frag_idx):
    /// - Cache miss → bump `nack_miss` and continue.
    /// - Out-of-range frag_idx → silently skip.
    /// - Otherwise re-emit just that one fragment via the queue, bump
    ///   `attempts`, advance `last_sent_at`, bump `nack_hit` and
    ///   `retransmit_attempts_total`.
    ///
    /// The RTO heap entry for this key stays in place; `tick` will see
    /// the bumped `attempts` and re-fire with the capped backoff. No
    /// active heap removal is required.
    pub fn on_nack(&mut self, entries: &[(EmitKey, u8)]) {
        for &(key, frag_idx) in entries {
            let now = self.smoothed_rtt_clock_now();
            let Some(entry) = self.cache.get_mut(&key) else {
                self.stats.nack_miss += 1;
                continue;
            };
            let Some(frag) = entry.fragments.get(frag_idx as usize) else {
                continue;
            };
            let bytes = frag.to_vec();
            entry.attempts += 1;
            entry.last_sent_at = now;
            // Release the entry borrow before touching self.queue / self.stats.
            let _ = entry;
            self.queue.push_source(bytes);
            self.stats.nack_hit += 1;
            self.stats.retransmit_attempts_total += 1;
        }
    }

    pub fn cancel_for_tile(&mut self, tile_x: u8, tile_y: u8) {
        self.cache.cancel_for_tile(tile_x, tile_y);
        // RTO heap entries are left in place; validation-on-pop in tick() will
        // drop them when the cache lookup misses.
    }

    /// Drop every cached entry. Called from `IoBridge::Event::ConnectionLost`
    /// — see `RetransmitCache::clear` doc.
    pub fn clear_cache(&mut self) {
        self.cache.clear();
        // RTO heap entries will validate against the empty cache on pop
        // and silently skip; no need to flush the heap.
    }

    pub fn has_cache_entries_for_tile(&self, tile_x: u8, tile_y: u8) -> bool {
        self.cache.has_entries_for_tile(tile_x, tile_y)
    }

    /// Internal helper — captures the current time so on_nack's caller
    /// doesn't have to pass an Instant. Task 22 wires a real Clock through
    /// the emitter constructor; this stub keeps the signature stable until
    /// then.
    fn smoothed_rtt_clock_now(&self) -> Instant {
        Instant::now()
    }

    /// Drain emissions to a sender until the queue is empty.
    pub fn drain<S: DatagramSender>(&mut self, sender: &mut S, now: Instant) {
        let next = self.alloc.peek();
        while let Some(emission) = self.queue.pop(next, now) {
            match emission {
                Emission::Source(bytes) => sender.send(&bytes),
                Emission::Parity(bytes) => {
                    sender.send(&bytes);
                    self.stats.parity_emitted += 1;
                }
            }
        }
    }
}

/// [H3-DIAG] FNV-1a fingerprint of the first fragment's bytes, plus the
/// peeked codec / palrle-flags / palette_id bytes from a tile datagram
/// layout: 16-byte DatagramHeader + 8-byte TileHeader + payload.
///
/// Returns (fp, codec_byte, palette_id_byte, palrle_flags_byte). For
/// non-PalRle codecs the palette_id / flags bytes are meaningless but
/// still surfaced to keep the log shape stable.
fn first_fragment_fp(frags: &[Vec<u8>]) -> (u32, u8, u8, u8) {
    let Some(f) = frags.first() else {
        return (0, 0xFF, 0xFF, 0xFF);
    };
    // FNV-1a over the whole fragment except the timestamp field at [12..16]
    // (which the io_bridge stamps lazily and we don't want to compare).
    let mut h: u32 = 0x811c_9dc5;
    for (i, &b) in f.iter().enumerate() {
        if (12..16).contains(&i) { continue; }
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    // TileHeader.codec lives at [18] as (codec << 1) | lz4.
    let codec_byte = f.get(18).copied().map(|b| b >> 1).unwrap_or(0xFF);
    // PalRle payload starts at [24]: [24]=flags, [25]=palette_id.
    let flags_byte = f.get(24).copied().unwrap_or(0xFF);
    let pid_byte = f.get(25).copied().unwrap_or(0xFF);
    (h, codec_byte, pid_byte, flags_byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::reliable_emitter::traits::testing::CollectSender;

    fn fake_source(seq: u32, frag_idx: u16, payload: u8) -> Bytes {
        // 16-byte DatagramHeader + 8-byte TileHeader + 1-byte payload
        let mut v = vec![0u8; 25];
        // frame_seq with TILE_DATAGRAM_FLAG (bit 31)
        let fs = 0x8000_0000 | seq;
        v[0..4].copy_from_slice(&fs.to_be_bytes());
        v[4..6].copy_from_slice(&frag_idx.to_be_bytes());
        v[6..8].copy_from_slice(&1u16.to_be_bytes());
        // wire_seq at 8..12 left as 0 — emitter will overwrite
        v[12..16].copy_from_slice(&0u32.to_be_bytes());  // timestamp
        v[16] = 0; v[17] = 0; v[18] = 0; v[19] = 0; // tile/codec/etc
        v[20..24].copy_from_slice(&1u32.to_be_bytes()); // payload_len
        v[24] = payload;
        Bytes::from(v)
    }

    #[test]
    fn submit_one_then_drain_emits_one_source() {
        let mut e = ReliableTileEmitter::new();
        let mut sender = CollectSender::default();
        let now = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0xAA), now);
        e.drain(&mut sender, now);
        assert_eq!(sender.sent.len(), 1);
        assert_eq!(e.stats.source_emitted, 1);
        assert!(e.cache.get(&key).is_some());
    }

    #[test]
    fn submit_one_stamps_wire_seq_into_datagram_bytes() {
        let mut e = ReliableTileEmitter::new();
        let mut sender = CollectSender::default();
        let now = Instant::now();
        e.submit_one(EmitKey::new(1, 0, 0, 0), fake_source(1, 0, 0), now);
        e.submit_one(EmitKey::new(2, 0, 0, 0), fake_source(2, 0, 0), now);
        e.drain(&mut sender, now);
        let s0 = &sender.sent[0];
        let s1 = &sender.sent[1];
        let ws0 = u32::from_be_bytes(s0[8..12].try_into().unwrap());
        let ws1 = u32::from_be_bytes(s1[8..12].try_into().unwrap());
        assert_eq!(ws0, 0);
        assert_eq!(ws1, 1);
    }

    #[test]
    fn submit_k_sources_emits_one_parity_after_offset() {
        use crate::transport::reliable_emitter::FEC_GROUP_SIZE_K;
        let mut e = ReliableTileEmitter::new();
        let mut sender = CollectSender::default();
        let now = Instant::now();
        // Submit K sources for group 0, then K sources for group 1 (so the
        // offset-interleaved parity for group 0 is reachable).
        for i in 0..(FEC_GROUP_SIZE_K as u32 * 2) {
            e.submit_one(EmitKey::new(i, 0, 0, 0), fake_source(i, 0, 0), now);
        }
        // Drain past wire_seq 20 by submitting one more source so allocator's
        // peek advances.
        e.submit_one(EmitKey::new(99, 0, 0, 0), fake_source(99, 0, 0), now);
        e.drain(&mut sender, now);
        // At least one parity datagram should have been emitted
        assert_eq!(e.stats.parity_emitted, 1, "exactly one parity for first K sources");
        // Total emitted: 2K+1 source + 1 parity
        assert_eq!(sender.sent.len(), (FEC_GROUP_SIZE_K as u32 * 2 + 1 + 1) as usize);
        // The parity datagram starts with TILE_PARITY_ENVELOPE (0x04)
        let parities: Vec<&Vec<u8>> = sender.sent.iter().filter(|b| b[0] == 0x04).collect();
        assert_eq!(parities.len(), 1);
    }

    #[test]
    fn on_ack_removes_cache_entry_and_bumps_ack_hit() {
        let mut e = ReliableTileEmitter::new();
        let mut sender = CollectSender::default();
        let now = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), now);
        e.drain(&mut sender, now);
        assert!(e.cache.get(&key).is_some());
        e.on_ack(&[key]);
        assert!(e.cache.get(&key).is_none());
        assert_eq!(e.stats.ack_hit, 1);
        assert_eq!(e.stats.ack_miss, 0);
    }

    #[test]
    fn on_ack_for_unknown_key_bumps_ack_miss() {
        let mut e = ReliableTileEmitter::new();
        e.on_ack(&[EmitKey::new(99, 0, 0, 0)]);
        assert_eq!(e.stats.ack_miss, 1);
        assert_eq!(e.stats.ack_hit, 0);
    }

    #[test]
    fn tick_retransmits_when_rto_expires() {
        let mut e = ReliableTileEmitter::new();
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0);
        e.drain(&mut sender, t0);
        assert_eq!(sender.sent.len(), 1);
        let entry_first_sent = e.cache.get(&key).unwrap().first_sent_at;
        // Advance past RTO; tick should retransmit.
        let t1 = t0 + Duration::from_millis(60);
        e.tick(t1, usize::MAX);
        e.drain(&mut sender, t1);
        assert_eq!(sender.sent.len(), 2);
        let entry = e.cache.get(&key).unwrap();
        assert_eq!(entry.attempts, 1);
        assert!(entry.last_sent_at > entry_first_sent);
        assert_eq!(e.stats.rto_fired, 1);
        assert_eq!(e.stats.retransmit_attempts_total, 1);
    }

    #[test]
    fn tick_keeps_retransmitting_after_many_attempts() {
        // Retirement was removed: the emitter keeps firing RTO retransmits
        // indefinitely until ACKed or cancelled. Verify that after N ticks
        // the entry is still present and rto_max_retransmits_reached stays 0.
        let mut e = ReliableTileEmitter::new();
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0);
        e.drain(&mut sender, t0);
        let mut tn = t0;
        for _ in 0..10 {
            tn += Duration::from_millis(500);
            e.tick(tn, usize::MAX);
            e.drain(&mut sender, tn);
        }
        // Entry must still be in the cache — no retirement.
        assert!(e.cache.get(&key).is_some(), "entry must not be retired");
        // rto_max_retransmits_reached stays 0 since we never retire.
        assert_eq!(e.stats.rto_max_retransmits_reached, 0);
    }

    #[test]
    fn tick_skips_already_acked_entries() {
        let mut e = ReliableTileEmitter::new();
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0);
        e.drain(&mut sender, t0);
        e.on_ack(&[key]);
        let t1 = t0 + Duration::from_millis(60);
        e.tick(t1, usize::MAX);
        e.drain(&mut sender, t1);
        assert_eq!(sender.sent.len(), 1, "no retransmit after ACK");
        assert_eq!(e.stats.rto_fired, 0);
    }

    #[test]
    fn on_nack_reemits_specific_fragment_only() {
        let mut e = ReliableTileEmitter::new();
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0);
        e.drain(&mut sender, t0);
        assert_eq!(sender.sent.len(), 1);
        // NACK for frag_idx=0 (the only one)
        e.on_nack(&[(key, 0u8)]);
        e.drain(&mut sender, t0);
        assert_eq!(sender.sent.len(), 2);
        assert_eq!(e.stats.nack_hit, 1);
        assert_eq!(e.cache.get(&key).unwrap().attempts, 1);
    }

    #[test]
    fn on_nack_for_unknown_key_bumps_nack_miss() {
        let mut e = ReliableTileEmitter::new();
        e.on_nack(&[(EmitKey::new(99, 0, 0, 0), 0u8)]);
        assert_eq!(e.stats.nack_miss, 1);
    }

    #[test]
    fn cancel_for_tile_clears_all_gens_and_blocks_retransmit() {
        let mut e = ReliableTileEmitter::new();
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let k1 = EmitKey::new(1, 5, 5, 0);
        let k2 = EmitKey::new(2, 5, 5, 1);
        let k3 = EmitKey::new(1, 5, 6, 0);  // different tile
        e.submit_one(k1, fake_source(1, 0, 0), t0);
        e.submit_one(k2, fake_source(2, 0, 0), t0);
        e.submit_one(k3, fake_source(3, 0, 0), t0);
        e.drain(&mut sender, t0);
        e.cancel_for_tile(5, 5);
        assert!(e.cache.get(&k1).is_none());
        assert!(e.cache.get(&k2).is_none());
        assert!(e.cache.get(&k3).is_some());
        // Tick past RTO — no retransmit for cancelled, retransmit for k3.
        let t1 = t0 + Duration::from_millis(60);
        e.tick(t1, usize::MAX);
        e.drain(&mut sender, t1);
        assert_eq!(sender.sent.len(), 4, "3 initial + 1 retransmit for k3 only");
    }

    #[test]
    fn on_nack_re_emits_without_budget_cap() {
        // Retirement and NACK budget cap were removed: every NACK produces a
        // re-emission as long as the cache entry is live.
        let mut e = ReliableTileEmitter::new();
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0);
        e.drain(&mut sender, t0);
        let nacks = 10usize;
        for _ in 0..nacks {
            e.on_nack(&[(key, 0u8)]);
            e.drain(&mut sender, t0);
        }
        // 1 original + nacks re-emits — all NACKs produce emissions now.
        assert_eq!(sender.sent.len(), 1 + nacks);
        assert_eq!(e.stats.nack_hit, nacks as u64);
        // Entry is still live.
        assert!(e.cache.get(&key).is_some());
    }

    #[test]
    fn submit_batch_processes_all_items() {
        let mut e = ReliableTileEmitter::new();
        let mut sender = CollectSender::default();
        let now = Instant::now();
        let items: Vec<(EmitKey, Bytes)> = (0..5)
            .map(|i| (EmitKey::new(i, 0, 0, 0), fake_source(i, 0, 0)))
            .collect();
        e.submit_batch(items, now);
        e.drain(&mut sender, now);
        assert_eq!(sender.sent.len(), 5);
        assert_eq!(e.stats.source_emitted, 5);
    }

    #[test]
    fn tick_keeps_retrying_indefinitely_until_acked() {
        let mut e = ReliableTileEmitter::new();
        let key = EmitKey::new(1, 0, 0, 0);
        let now = Instant::now();
        e.submit_one(key, bytes::Bytes::from(vec![0u8; 16]), now);
        // Drain the initial emission so the queue is empty.
        let mut sink: Vec<Vec<u8>> = Vec::new();
        struct Sink<'a>(&'a mut Vec<Vec<u8>>);
        impl<'a> crate::transport::reliable_emitter::traits::DatagramSender for Sink<'a> {
            fn send(&mut self, dg: &[u8]) {
                self.0.push(dg.to_vec());
            }
        }
        e.drain(&mut Sink(&mut sink), now);
        let initial_sends = sink.len();
        assert!(initial_sends >= 1, "initial submit produced at least one send");

        // Advance time well past the old MAX_RETRANSMITS retirement window
        // and tick many times. Every tick should keep the entry alive (no
        // retirement) and continue scheduling retransmits.
        let mut t = now;
        for _ in 0..30 {
            t += std::time::Duration::from_secs(5);
            e.tick(t, usize::MAX);
            e.drain(&mut Sink(&mut sink), t);
        }
        // Entry must still be live in the cache.
        assert!(
            e.has_cache_entries_for_tile(0, 0),
            "cache entry must survive arbitrarily many RTO firings (no retirement)"
        );
        // ACK the entry — cache entry must disappear.
        e.on_ack(&[key]);
        assert!(
            !e.has_cache_entries_for_tile(0, 0),
            "ACK must remove the entry from the cache"
        );
    }

    #[test]
    fn tick_respects_max_retransmits_budget() {
        // Submit 1000 entries with distinct EmitKeys (so each gets its own
        // RTO heap entry). Advance time past all their initial RTOs.
        // tick(t, budget) must fire at most `budget` retransmits per call,
        // leaving the rest still due in the heap for the next call.
        let mut e = ReliableTileEmitter::new();
        let now = Instant::now();
        for i in 0..1000u32 {
            let k = EmitKey::new(i, 0, 0, 0);
            e.submit_one(k, bytes::Bytes::from(vec![0u8; 16]), now);
        }
        // Drain the initial 1000 submissions.
        struct Sink<'a>(&'a mut Vec<Vec<u8>>);
        impl<'a> crate::transport::reliable_emitter::traits::DatagramSender for Sink<'a> {
            fn send(&mut self, dg: &[u8]) {
                self.0.push(dg.to_vec());
            }
        }
        let mut sink: Vec<Vec<u8>> = Vec::new();
        e.drain(&mut Sink(&mut sink), now);
        sink.clear();

        // Advance past the first RTO deadline of every entry.
        let t1 = now + std::time::Duration::from_secs(1);

        // First tick with budget=64: must fire exactly 64.
        let before = e.stats.rto_fired;
        e.tick(t1, 64);
        e.drain(&mut Sink(&mut sink), t1);
        assert_eq!(
            e.stats.rto_fired - before,
            64,
            "first tick must fire exactly the budget"
        );
        assert_eq!(sink.len(), 64, "exactly 64 retransmits drained to wire");

        // Second tick at same instant: another 64 should fire (936 still due).
        sink.clear();
        let before = e.stats.rto_fired;
        e.tick(t1, 64);
        e.drain(&mut Sink(&mut sink), t1);
        assert_eq!(e.stats.rto_fired - before, 64, "second tick fires another batch");
        assert_eq!(sink.len(), 64);

        // After many ticks at budget=64, all 1000 entries should be popped
        // (each pop reschedules its RTO into the future, so subsequent ticks
        // at t1 yield None). 1000 / 64 ≈ 16 ticks needed.
        for _ in 0..20 {
            e.tick(t1, 64);
        }
        // All 1000 entries' next RTOs are in the future — heap has nothing due.
        // pending_cache_entries reflects un-ACKed work, still 1000.
        assert_eq!(e.pending_cache_entries(), 1000);
    }
}
