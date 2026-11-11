// SPDX-License-Identifier: MIT OR Apache-2.0
//! ACK batching constants shared by the client's ACK v4 batcher.
//!
//! The per-transmission ACK batch of earlier revisions (message type 0x06,
//! entries naming a `wire_seq`) is gone: with the rev 6 framing the client
//! acknowledges bundles (`crate::fec::feedback::BundleAckBatch`, 0x08), and
//! the server classifies a leading 0x06 as a superseded revision
//! (`crate::protocol::InboundKind::AckBatchV1`) and drops it.

/// Maximum number of *fresh* entries the client packs into one batch
/// before flushing.
pub const MAX_FRESH_ENTRIES_PER_BATCH: usize = 64;
/// Trailing overlap count the client appends to each batch. A single dropped
/// ACK batch therefore needs ACK_OVERLAP_COUNT + 1 consecutive drops to lose
/// any entry.
pub const ACK_OVERLAP_COUNT: usize = 8;
