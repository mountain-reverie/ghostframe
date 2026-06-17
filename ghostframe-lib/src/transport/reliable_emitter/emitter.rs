//! Central facade that ties cache, emission queue, allocator, group
//! builder, and RTO wheel into one struct. Per-session.

use crate::transport::reliable_emitter::cache::{CacheEntry, RetransmitCache};
use crate::transport::reliable_emitter::emission_queue::{Emission, EmissionQueue};
use crate::transport::reliable_emitter::parity::GroupBuilder;
use crate::transport::reliable_emitter::rto::{rto_for_attempt, RtoTimerWheel};
use crate::transport::reliable_emitter::traits::DatagramSender;
use crate::transport::reliable_emitter::wire_seq::WireSeqAllocator;
use crate::transport::reliable_emitter::{EmitKey, FEC_GROUP_SIZE_K};
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
        // Stamp wire_seq into the source bytes' DatagramHeader at offset 8..12 BE.
        let mut bytes = source_datagram_bytes.to_vec();
        if bytes.len() >= 12 {
            bytes[8..12].copy_from_slice(&wire_seq.to_be_bytes());
        }
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
        self.queue.push_source(bytes);
        self.stats.source_emitted += 1;
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
}
