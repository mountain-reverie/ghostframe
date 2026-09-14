//! Batched-ACK datagram sender. Buffers per-tile-pass ACK entries; flushes
//! on either >= MAX_FRESH_ENTRIES_PER_BATCH entries OR 5ms since the first
//! queued entry. Ports `ghostframe-web-client/src/ack.ts`, but time is
//! injected (`now_us: u64`) instead of using `setTimeout`/wall clock, so the
//! caller (ClientCore) drives the deadline via `poll_timeout`/`on_timeout`.

use std::collections::VecDeque;

use ghostframe_protocol::ack::{
    AckBatch, AckEntry, ACK_OVERLAP_COUNT, MAX_FRESH_ENTRIES_PER_BATCH,
};

/// Flush deadline: 5ms in microseconds after the first entry of a pending
/// batch is queued (mirrors `FLUSH_INTERVAL_MS` in ack.ts).
pub const FLUSH_INTERVAL_US: u64 = 5_000;

/// How many recent fresh entries to retain for overlap purposes (4x
/// ACK_OVERLAP_COUNT, mirroring `maxRecent` in ack.ts).
const MAX_RECENT: usize = ACK_OVERLAP_COUNT * 4;

/// Longest span of fresh entries one batch may cover, bounded by the u16
/// microsecond delta the wire format gives each fresh entry. Normally
/// FLUSH_INTERVAL_US (5 ms) closes a batch long before this, but nothing
/// guarantees `on_timeout` is called on time -- a stalled event loop can
/// accumulate entries across a much longer window, and a batch spanning more
/// than this cannot be encoded at all.
const MAX_FRESH_SPAN_US: u64 = u16::MAX as u64;

pub struct AckBatcher {
    entries: Vec<AckEntry>,
    recent: VecDeque<AckEntry>,
    deadline_us: Option<u64>,
    /// `now_us` of the oldest entry in `entries`, i.e. when the pending
    /// batch was opened. Used to detect a span that would overflow the
    /// wire format's u16 delta (see `MAX_FRESH_SPAN_US`) before it happens,
    /// independent of whether `on_timeout` fires on schedule.
    first_pending_us: Option<u64>,
}

impl AckBatcher {
    pub fn new() -> Self {
        AckBatcher {
            entries: Vec::new(),
            recent: VecDeque::new(),
            deadline_us: None,
            first_pending_us: None,
        }
    }

    /// Queues entry; returns Some(encoded datagram) when either the
    /// fresh-entry cap or the fresh-span guard forces an immediate flush.
    ///
    /// The span guard exists because nothing but `on_timeout` normally
    /// closes a batch before it grows too old to encode, and nothing
    /// guarantees `on_timeout` is called on time: a stalled event loop (a
    /// slow frame, a GC pause, a backgrounded tab) can let entries pile up
    /// via `add` alone until their span exceeds what the wire format's u16
    /// delta can express. Rather than let that reach `AckBatch::new` as a
    /// `FreshDeltaOverflow`, close the stale batch here and start a fresh
    /// one with this entry -- splitting preserves every ACK, where dropping
    /// entries to fit would not.
    pub fn add(&mut self, entry: AckEntry, now_us: u64) -> Option<Vec<u8>> {
        let stall_flush = match self.first_pending_us {
            Some(first_us) if now_us.saturating_sub(first_us) > MAX_FRESH_SPAN_US => self.flush(),
            _ => None,
        };

        self.entries.push(entry);
        if self.first_pending_us.is_none() {
            self.first_pending_us = Some(now_us);
        }

        if self.entries.len() >= MAX_FRESH_ENTRIES_PER_BATCH {
            // A stall flush, if any, already happened above and left
            // `entries` with just the one entry we pushed after it, so the
            // cap (64) can't also be hit in this same call -- the two flush
            // triggers never fire together, so returning this one can't
            // silently drop the other.
            return self.flush();
        }

        if self.deadline_us.is_none() {
            self.deadline_us = Some(now_us + FLUSH_INTERVAL_US);
        }

        stall_flush
    }

    /// Earliest deadline (µs) at which `on_timeout` must be called, if any.
    pub fn poll_timeout(&self) -> Option<u64> {
        self.deadline_us
    }

    /// Flushes if `now_us` has reached the pending deadline.
    pub fn on_timeout(&mut self, now_us: u64) -> Option<Vec<u8>> {
        match self.deadline_us {
            Some(deadline) if now_us >= deadline => self.flush(),
            _ => None,
        }
    }

    /// Flushes any pending fresh entries plus the overlap tail. Returns None
    /// if there are no fresh entries queued.
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        self.deadline_us = None;
        self.first_pending_us = None;
        if self.entries.is_empty() {
            return None;
        }

        let fresh: Vec<AckEntry> = std::mem::take(&mut self.entries);
        let overlap: Vec<AckEntry> = self
            .recent
            .iter()
            .rev()
            .take(ACK_OVERLAP_COUNT)
            .rev()
            .copied()
            .collect();

        // AckBatch::new's invariants all hold by construction here, so this
        // can't fail:
        // - fresh count <= MAX_FRESH_ENTRIES_PER_BATCH (64): `add` flushes
        //   as soon as the cap is reached, before it can be exceeded.
        // - overlap count <= ACK_OVERLAP_COUNT: bounded by `take` above.
        // - fresh span <= MAX_FRESH_SPAN_US (u16::MAX us): `add`'s stall
        //   guard flushes any batch that would grow older than this before
        //   adding an entry that would push it over.
        // - fresh non-decreasing mod 2^32 from fresh[0]: entries are pushed
        //   in arrival order and the span guard keeps every batch's
        //   lifetime far under 2^32us, so there's no wraparound to reorder.
        // - non-empty: the `is_empty` check above returns early otherwise.
        let batch = AckBatch::new(fresh.clone(), overlap)
            .expect("AckBatcher upholds every AckBatch::new invariant -- see comment above");
        let out = batch.encode();

        // Track only FRESH entries in recent; cap memory at MAX_RECENT.
        self.recent.extend(fresh);
        while self.recent.len() > MAX_RECENT {
            self.recent.pop_front();
        }

        Some(out)
    }
}

impl Default for AckBatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn entry(frame_seq: u32, now_us: u64) -> AckEntry {
        AckEntry {
            frame_seq,
            tile_x: 1,
            tile_y: 2,
            pass_idx: 0,
            arrival_us: (now_us & 0xFFFF_FFFF) as u32,
        }
    }

    #[test]
    fn flush_reports_the_fresh_count() {
        let mut b = AckBatcher::new();

        // First batch: 3 fresh, no overlap yet.
        for i in 0..3u32 {
            assert!(b
                .add(entry(i, 1_000 + i as u64), 1_000 + i as u64)
                .is_none());
        }
        let d1 = b.flush().expect("3 pending entries flush");
        let batch1 = AckBatch::decode(&d1).expect("valid batch");
        assert_eq!(batch1.fresh().len(), 3);
        assert_eq!(batch1.overlap().len(), 0);

        // Second batch: far enough in the future that the first batch's
        // fresh entries (now "recent") are more than a u16 delta behind
        // the new base -- exactly the case overlap's absolute encoding
        // exists for.
        let far_future = 1_000_000u64;
        for i in 0..2u32 {
            assert!(b
                .add(entry(100 + i, far_future + i as u64), far_future + i as u64)
                .is_none());
        }
        let d2 = b.flush().expect("2 pending entries flush");
        let batch2 = AckBatch::decode(&d2).expect("valid batch");
        assert_eq!(batch2.fresh().len(), 2);
        assert_eq!(batch2.overlap().len(), 3);

        // The overlap entries' arrival_us must survive exactly, even though
        // they predate the new base by far more than a u16 could express
        // as a delta (~1_000_000us apart vs u16::MAX ~65_535us).
        let overlap_arrivals: HashSet<u32> =
            batch2.overlap().iter().map(|e| e.arrival_us).collect();
        for i in 0..3u32 {
            assert!(
                overlap_arrivals.contains(&(1_000 + i)),
                "expected overlap to carry arrival_us {} exactly",
                1_000 + i
            );
        }
    }

    #[test]
    fn a_stalled_event_loop_splits_the_batch_instead_of_overflowing() {
        let mut b = AckBatcher::new();

        // Simulate a stalled event loop: entries keep arriving via `add`
        // alone, at ever-increasing timestamps, without `on_timeout` ever
        // being called. Steps of 10_000us so that after ~7 entries the
        // span from the first entry exceeds MAX_FRESH_SPAN_US (65_535us).
        let count = 20u32;
        let step_us = 10_000u64;

        let mut emitted_datagrams = Vec::new();
        for i in 0..count {
            let now_us = i as u64 * step_us;
            if let Some(dg) = b.add(entry(i, now_us), now_us) {
                emitted_datagrams.push(dg);
            }
        }
        // Flush whatever's left pending at the end (stands in for the
        // eventual on_timeout/shutdown flush).
        if let Some(dg) = b.flush() {
            emitted_datagrams.push(dg);
        }

        assert!(
            !emitted_datagrams.is_empty(),
            "expected the span guard to force at least one flush"
        );
        // Splitting, not just one final flush: with a 200_000us total span
        // and a 65_535us cap, more than one datagram must have been
        // produced during `add` itself.
        assert!(
            emitted_datagrams.len() >= 2,
            "expected the stall to split into multiple batches, got {}",
            emitted_datagrams.len()
        );

        let mut seen_frame_seqs: HashSet<u32> = HashSet::new();
        for dg in &emitted_datagrams {
            let batch = AckBatch::decode(dg).expect("every emitted datagram must decode");
            for e in batch.fresh() {
                seen_frame_seqs.insert(e.frame_seq);
            }
        }

        let expected: HashSet<u32> = (0..count).collect();
        assert_eq!(
            seen_frame_seqs, expected,
            "no entry may be lost when a stalled batch splits"
        );
    }

    #[test]
    fn microsecond_precision_survives_the_round_trip() {
        let mut b = AckBatcher::new();
        assert!(b.add(entry(1, 1_000), 1_000).is_none());
        assert!(b.add(entry(2, 2_500), 2_500).is_none());
        let d = b.flush().expect("2 pending entries flush");

        let batch = AckBatch::decode(&d).expect("valid batch");
        assert_eq!(batch.fresh().len(), 2);
        let a = batch.fresh()[0].arrival_us;
        let bb = batch.fresh()[1].arrival_us;
        // 1500us apart: indistinguishable at millisecond resolution, which
        // is the entire point of this change.
        assert_eq!(bb - a, 1_500);
    }
}
