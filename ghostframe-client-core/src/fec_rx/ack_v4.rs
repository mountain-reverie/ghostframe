// SPDX-License-Identifier: MIT OR Apache-2.0
//! ACK v4 batcher: flush at 64 fresh entries or 5 ms after the first queued
//! one, last 8 fresh entries appended as overlap; entries name `bundle_seq` and
//! the batch is a `BundleAckBatch` (0x08).

use std::collections::VecDeque;

use ghostframe_protocol::ack::{ACK_OVERLAP_COUNT, MAX_FRESH_ENTRIES_PER_BATCH};
use ghostframe_protocol::fec::feedback::{BundleAck, BundleAckBatch};

/// Flush deadline: 5ms in microseconds after the first entry of a pending
/// batch is queued.
pub const FLUSH_INTERVAL_US: u64 = 5_000;

/// How many recent fresh entries to retain for overlap purposes (4x
/// `ACK_OVERLAP_COUNT`).
pub(crate) const MAX_RECENT: usize = ACK_OVERLAP_COUNT * 4;

pub struct AckV4Batcher {
    entries: Vec<BundleAck>,
    recent: VecDeque<BundleAck>,
    deadline_us: Option<u64>,
}

impl AckV4Batcher {
    pub fn new() -> Self {
        AckV4Batcher {
            entries: Vec::new(),
            recent: VecDeque::new(),
            deadline_us: None,
        }
    }

    /// Queues an entry; returns the encoded datagram when the fresh-entry cap
    /// forces an immediate flush.
    pub fn add(&mut self, entry: BundleAck, now_us: u64) -> Option<Vec<u8>> {
        self.entries.push(entry);
        if self.entries.len() >= MAX_FRESH_ENTRIES_PER_BATCH {
            return self.flush();
        }
        if self.deadline_us.is_none() {
            self.deadline_us = Some(now_us + FLUSH_INTERVAL_US);
        }
        None
    }

    pub fn poll_timeout(&self) -> Option<u64> {
        self.deadline_us
    }

    pub fn on_timeout(&mut self, now_us: u64) -> Option<Vec<u8>> {
        match self.deadline_us {
            Some(d) if now_us >= d => self.flush(),
            _ => None,
        }
    }

    /// Flushes the fresh entries plus the overlap tail; `None` if nothing is queued.
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        self.deadline_us = None;
        if self.entries.is_empty() {
            return None;
        }
        let fresh = std::mem::take(&mut self.entries);
        let mut all = fresh.clone();
        all.extend(
            self.recent
                .iter()
                .rev()
                .take(ACK_OVERLAP_COUNT)
                .rev()
                .copied(),
        );
        let out = BundleAckBatch { entries: all }.encode();
        self.recent.extend(fresh);
        while self.recent.len() > MAX_RECENT {
            self.recent.pop_front();
        }
        Some(out)
    }
}

impl Default for AckV4Batcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack(n: u32) -> BundleAck {
        BundleAck {
            bundle_seq: n,
            arrival_time_ms_lo16: n as u16,
        }
    }

    #[test]
    fn sixty_four_adds_flush_once_with_sixty_four_entries() {
        let mut b = AckV4Batcher::new();
        for i in 0..63 {
            assert!(b.add(ack(i), 0).is_none());
        }
        let dg = b.add(ack(63), 0).expect("64th add flushes");
        let batch = BundleAckBatch::decode(&dg).unwrap();
        assert_eq!(batch.entries.len(), 64);
        assert_eq!(batch.entries[0], ack(0));
        assert_eq!(batch.entries[63], ack(63));
        assert!(b.poll_timeout().is_none());
        assert!(b.flush().is_none());
    }

    #[test]
    fn the_deadline_flushes_a_partial_batch() {
        let mut b = AckV4Batcher::new();
        assert!(b.add(ack(1), 1_000).is_none());
        assert!(b.add(ack(2), 2_000).is_none());
        assert_eq!(b.poll_timeout(), Some(1_000 + FLUSH_INTERVAL_US));
        assert!(b.on_timeout(1_000 + FLUSH_INTERVAL_US - 1).is_none());
        let dg = b.on_timeout(1_000 + FLUSH_INTERVAL_US).expect("flush");
        let batch = BundleAckBatch::decode(&dg).unwrap();
        assert_eq!(batch.entries, vec![ack(1), ack(2)]);
    }

    #[test]
    fn the_second_batch_carries_eight_overlap_entries_from_the_first() {
        let mut b = AckV4Batcher::new();
        for i in 0..64 {
            b.add(ack(i), 0);
        }
        b.add(ack(100), 10);
        let dg = b.on_timeout(10 + FLUSH_INTERVAL_US).unwrap();
        let batch = BundleAckBatch::decode(&dg).unwrap();
        assert_eq!(batch.entries.len(), 1 + ACK_OVERLAP_COUNT);
        assert_eq!(batch.entries[0], ack(100));
        let tail: Vec<u32> = batch.entries[1..].iter().map(|e| e.bundle_seq).collect();
        assert_eq!(tail, (56..64).collect::<Vec<u32>>());
    }

    #[test]
    fn overlap_is_capped_at_eight_after_twenty_single_entry_flushes() {
        let mut b = AckV4Batcher::new();
        for i in 0..20u32 {
            b.add(ack(i), 0);
            b.flush();
        }
        b.add(ack(100), 0);
        let batch = BundleAckBatch::decode(&b.flush().unwrap()).unwrap();
        assert_eq!(batch.entries.len(), 1 + ACK_OVERLAP_COUNT);
        assert_eq!(batch.entries[0], ack(100));
        let tail: Vec<u32> = batch.entries[1..].iter().map(|e| e.bundle_seq).collect();
        assert_eq!(tail, (12..20).collect::<Vec<u32>>());
    }

    #[test]
    fn the_first_batch_has_no_overlap_and_an_empty_flush_sends_nothing() {
        let mut b = AckV4Batcher::new();
        assert!(b.flush().is_none());
        b.add(ack(42), 0);
        let batch = BundleAckBatch::decode(&b.flush().unwrap()).unwrap();
        assert_eq!(batch.entries, vec![ack(42)]);
        assert!(b.flush().is_none(), "nothing queued after a flush");
    }
}
