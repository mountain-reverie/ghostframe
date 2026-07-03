//! Batched-NACK datagram sender. Buffers per-fragment NACK entries; flushes
//! on either >= 64 entries OR 5ms since the first queued entry. Ports
//! `ghostframe-web-client/src/nack.ts`, but time is injected (`now_us: u64`)
//! instead of using `setTimeout`/wall clock, so the caller (ClientCore) drives
//! the deadline via `poll_timeout`/`on_timeout`.

use ghostframe_protocol::protocol::{TileNackEntry, TileNackEnvelope};

/// Flush deadline: 5ms in microseconds after the first entry of a pending
/// batch is queued (mirrors `NACK_BATCH_FLUSH_MS` in nack.ts).
const FLUSH_INTERVAL_US: u64 = 5_000;

/// Maximum entries per NACK batch.
const NACK_BATCH_MAX: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NackEntry {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
    pub frag_idx: u8,
}

pub struct NackBatcher {
    entries: Vec<NackEntry>,
    deadline_us: Option<u64>,
}

impl NackBatcher {
    pub fn new() -> Self {
        NackBatcher {
            entries: Vec::new(),
            deadline_us: None,
        }
    }

    /// Queues entry; returns Some(encoded datagram) when the entry cap
    /// forces an immediate flush.
    pub fn add(&mut self, entry: NackEntry, now_us: u64) -> Option<Vec<u8>> {
        self.entries.push(entry);
        if self.entries.len() >= NACK_BATCH_MAX {
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

    /// Flushes any pending entries. Returns None if there are no entries queued.
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        self.deadline_us = None;
        if self.entries.is_empty() {
            return None;
        }

        let entries: Vec<TileNackEntry> = self
            .entries
            .drain(..)
            .map(|e| TileNackEntry {
                frame_seq: e.frame_seq,
                tile_x: e.tile_x,
                tile_y: e.tile_y,
                pass_idx: e.pass_idx,
                frag_idx: e.frag_idx,
            })
            .collect();

        let envelope = TileNackEnvelope { entries };
        let mut out = Vec::new();
        envelope.encode(&mut out);

        Some(out)
    }
}

impl Default for NackBatcher {
    fn default() -> Self {
        Self::new()
    }
}
