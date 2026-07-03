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
const FLUSH_INTERVAL_US: u64 = 5_000;

/// How many recent fresh entries to retain for overlap purposes (4x
/// ACK_OVERLAP_COUNT, mirroring `maxRecent` in ack.ts).
const MAX_RECENT: usize = ACK_OVERLAP_COUNT * 4;

pub struct AckBatcher {
    entries: Vec<AckEntry>,
    recent: VecDeque<AckEntry>,
    deadline_us: Option<u64>,
}

impl AckBatcher {
    pub fn new() -> Self {
        AckBatcher {
            entries: Vec::new(),
            recent: VecDeque::new(),
            deadline_us: None,
        }
    }

    /// Queues entry; returns Some(encoded datagram) when the fresh-entry cap
    /// forces an immediate flush.
    pub fn add(&mut self, entry: AckEntry, now_us: u64) -> Option<Vec<u8>> {
        self.entries.push(entry);
        if self.entries.len() >= MAX_FRESH_ENTRIES_PER_BATCH {
            return self.flush();
        }
        if self.deadline_us.is_none() {
            self.deadline_us = Some(now_us + FLUSH_INTERVAL_US);
        }
        None
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

        let mut all_entries = fresh.clone();
        all_entries.extend(overlap);

        let batch = AckBatch {
            entries: all_entries,
        };
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
