//! Central facade that ties cache, emission queue, allocator, group
//! builder, and RTO wheel into one struct. Per-session.

use crate::transport::protocol::TileParityEnvelope;
use crate::transport::reliable_emitter::cache::{CacheEntry, RetransmitCache};
use crate::transport::reliable_emitter::emission_queue::{Emission, EmissionQueue};
use crate::transport::reliable_emitter::parity::GroupBuilder;
use crate::transport::reliable_emitter::rto::{rto_for_attempt, RtoTimerWheel};
use crate::transport::reliable_emitter::traits::DatagramSender;
use crate::transport::reliable_emitter::wire_seq::WireSeqAllocator;
use crate::transport::reliable_emitter::{EmitKey, FEC_GROUP_SIZE_K, MAX_RETRANSMITS, PARITY_INTERLEAVE_OFFSET};
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

    /// Drain due RTO entries. For each due key, validate against the live
    /// cache (silently skip stale entries — already ACKed or cancelled). If
    /// the entry hasn't hit MAX_RETRANSMITS, bump `attempts`, re-enqueue every
    /// cached fragment via the EmissionQueue with the same wire_seqs (so client
    /// dedup keys remain stable), and reschedule a new RTO deadline. At
    /// MAX_RETRANSMITS, retire — drop the cache entry and bump
    /// `rto_max_retransmits_reached`.
    pub fn tick(&mut self, now: Instant) {
        while let Some(key) = self.rto.pop_due(now) {
            let Some(entry) = self.cache.get_mut(&key) else {
                // Already ACKed or cancelled — stale heap entry; skip silently.
                continue;
            };
            if entry.attempts >= MAX_RETRANSMITS {
                // Retire — drop from cache.
                self.cache.remove(&key);
                self.stats.rto_max_retransmits_reached += 1;
                continue;
            }
            // Bump attempts, re-enqueue every cached fragment, reschedule RTO.
            entry.attempts += 1;
            entry.last_sent_at = now;
            let new_rto = rto_for_attempt(self.smoothed_rtt, entry.attempts);
            entry.rto_deadline = now + new_rto;
            let frags: Vec<Vec<u8>> = entry.fragments.iter().map(|b| b.to_vec()).collect();
            for bytes in frags {
                self.queue.push_source(bytes);
            }
            self.rto.schedule(key, now + new_rto);
            self.stats.rto_fired += 1;
            self.stats.retransmit_attempts_total += 1;
        }
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
        e.tick(t1);
        e.drain(&mut sender, t1);
        assert_eq!(sender.sent.len(), 2);
        let entry = e.cache.get(&key).unwrap();
        assert_eq!(entry.attempts, 1);
        assert!(entry.last_sent_at > entry_first_sent);
        assert_eq!(e.stats.rto_fired, 1);
        assert_eq!(e.stats.retransmit_attempts_total, 1);
    }

    #[test]
    fn tick_stops_at_max_retransmits() {
        let mut e = ReliableTileEmitter::new();
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0);
        e.drain(&mut sender, t0);
        let mut tn = t0;
        for _ in 0..MAX_RETRANSMITS {
            tn += Duration::from_millis(500);
            e.tick(tn);
            e.drain(&mut sender, tn);
        }
        // After MAX_RETRANSMITS retries: 1 original + MAX_RETRANSMITS = 5 total
        assert_eq!(sender.sent.len(), 1 + MAX_RETRANSMITS as usize);
        // One more tick after the budget — nothing more emitted.
        tn += Duration::from_millis(500);
        e.tick(tn);
        e.drain(&mut sender, tn);
        assert_eq!(sender.sent.len(), 1 + MAX_RETRANSMITS as usize);
        assert_eq!(e.stats.rto_max_retransmits_reached, 1);
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
        e.tick(t1);
        e.drain(&mut sender, t1);
        assert_eq!(sender.sent.len(), 1, "no retransmit after ACK");
        assert_eq!(e.stats.rto_fired, 0);
    }
}
