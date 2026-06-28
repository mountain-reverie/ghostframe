//! Batched-ACK datagram protocol — per-tile-pass acknowledgment (M3.3d rev2).
//!
//! Wire format:
//! ```text
//! [0]      message_type = 0x04
//! [1]      count: u8 (1..=72)
//! [2..]    count × 9 bytes:
//!             [0..4]  frame_seq: u32 little-endian
//!             [4]     tile_x: u8
//!             [5]     tile_y: u8
//!             [6]     pass_idx: u8
//!             [7..9]  arrival_time_ms_lo16: u16 little-endian
//! ```
//!
//! Max count is 72 — `MAX_FRESH_ENTRIES_PER_BATCH (64)` + the
//! client-side `ACK_OVERLAP_COUNT (8)` trailing entries that each
//! batch carries from the previous one as resilience against a single
//! ACK-batch loss. The server MUST accept anything up to that combined
//! limit or the overlap mechanism collapses (overlap-bearing batches
//! get rejected, every retransmit floods MAX_RETRANSMITS without ACK).
//!
//! Each entry acknowledges receipt of the tile-pass payload identified by
//! `(frame_seq, tile_x, tile_y, pass_idx)`. Server uses its
//! `FragmentCoverageMap` to convert per-tile-pass ACKs into per-tile delivery
//! bookkeeping (Cdf53 ACK counter, PalRle palette delivered tracking, etc.).
//!
//! Rev2 change: the M3.3d initial wire format used `(frame_seq, frag_idx)` as
//! the key, which collides for multiple single-fragment work items per frame
//! (all get frag_idx=0). Rev2 switches to `(frame_seq, tile_x, tile_y,
//! pass_idx)` which is unique per tile-pass emission.
//!
//! Pre-release wire break: the previous format (M3.3c) was
//! `message_type=0x02` with per-tile entries `(tile_x, tile_y,
//! packed(gen,pass), reserved)`. Bumping the message-type byte means a
//! stale dev binary mid-rollout fails loud with
//! `AckDecodeError::WrongMsgType(0x02)` rather than silently mis-parsing.

/// ACK envelope wire-format version. Bumped 0x03 → 0x04 in 2026-06-27
/// to add a 2-byte per-entry receiver-arrival-time field (low 16 bits
/// of wall-clock milliseconds) used by the server's bandwidth-estimator
/// hook. Old (0x03) clients/servers are not wire-compatible with new —
/// both sides ship in lockstep.
pub const ACK_BATCH_MSG_TYPE: u8 = 0x04;
/// Maximum number of *fresh* entries the client packs into one batch
/// before flushing (mirrors `MAX_ACK_ENTRIES` in ack.ts). Used for the
/// roundtrip-size tests; the wire-acceptance cap is below.
pub const MAX_FRESH_ENTRIES_PER_BATCH: usize = 64;
/// Trailing overlap count the client appends to each batch (mirrors
/// `ACK_OVERLAP_COUNT` in ack.ts). A single dropped ACK batch
/// therefore needs ACK_OVERLAP_COUNT + 1 consecutive drops to lose
/// any entry.
pub const ACK_OVERLAP_COUNT: usize = 8;
/// Wire-acceptance cap for one ACK batch: fresh + overlap.
pub const MAX_ACK_ENTRIES_PER_BATCH: usize =
    MAX_FRESH_ENTRIES_PER_BATCH + ACK_OVERLAP_COUNT;
pub const ACK_ENTRY_SIZE: usize = 9;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AckDecodeError {
    #[error("ack batch too short ({0} bytes)")]
    TooShort(usize),
    #[error("wrong message type: expected 0x04, got 0x{0:02x}")]
    WrongMsgType(u8),
    #[error("invalid entry count: {0} (must be 1..=72)")]
    InvalidCount(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckEntry {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
    /// Low 16 bits of the client's wall-clock millisecond receive time
    /// for this pass. 65.5 s wrap; absolute clock skew doesn't matter
    /// because the BWE consumer looks at *relative* arrival differences
    /// between packets in the same envelope.
    pub arrival_time_ms_lo16: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckBatch {
    pub entries: Vec<AckEntry>,
}

impl AckBatch {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.entries.len() * ACK_ENTRY_SIZE);
        out.push(ACK_BATCH_MSG_TYPE);
        out.push(self.entries.len() as u8);
        for e in &self.entries {
            out.extend_from_slice(&e.frame_seq.to_le_bytes());
            out.push(e.tile_x);
            out.push(e.tile_y);
            out.push(e.pass_idx);
            // Bytes [7..9]: arrival_time_ms_lo16, little-endian to match
            // the rest of the entry's little-endian fields.
            out.push((e.arrival_time_ms_lo16 & 0xFF) as u8);
            out.push(((e.arrival_time_ms_lo16 >> 8) & 0xFF) as u8);
        }
        out
    }

    pub fn decode(data: &[u8]) -> Result<Self, AckDecodeError> {
        if data.len() < 2 {
            return Err(AckDecodeError::TooShort(data.len()));
        }
        if data[0] != ACK_BATCH_MSG_TYPE {
            return Err(AckDecodeError::WrongMsgType(data[0]));
        }
        let count = data[1];
        if count == 0 || count as usize > MAX_ACK_ENTRIES_PER_BATCH {
            return Err(AckDecodeError::InvalidCount(count));
        }
        let need = 2 + (count as usize) * ACK_ENTRY_SIZE;
        if data.len() < need {
            return Err(AckDecodeError::TooShort(data.len()));
        }
        let mut entries = Vec::with_capacity(count as usize);
        for i in 0..(count as usize) {
            let off = 2 + i * ACK_ENTRY_SIZE;
            let frame_seq =
                u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
            let tile_x = data[off + 4];
            let tile_y = data[off + 5];
            let pass_idx = data[off + 6];
            let arrival_time_ms_lo16 =
                (data[off + 7] as u16) | ((data[off + 8] as u16) << 8);
            entries.push(AckEntry {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
                arrival_time_ms_lo16,
            });
        }
        Ok(AckBatch { entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_roundtrip_single_entry() {
        let batch = AckBatch {
            entries: vec![AckEntry {
                frame_seq: 0x1234_5678,
                tile_x: 3,
                tile_y: 7,
                pass_idx: 13,
                arrival_time_ms_lo16: 0,
            }],
        };
        let bytes = batch.encode();
        assert_eq!(bytes[0], ACK_BATCH_MSG_TYPE, "msg type = 0x04");
        assert_eq!(bytes[0], 0x04);
        assert_eq!(bytes[1], 1, "count = 1");
        assert_eq!(bytes[2..6], [0x78, 0x56, 0x34, 0x12], "frame_seq LE");
        assert_eq!(bytes[6], 3, "tile_x");
        assert_eq!(bytes[7], 7, "tile_y");
        assert_eq!(bytes[8], 13, "pass_idx");
        let decoded = AckBatch::decode(&bytes).expect("valid batch");
        assert_eq!(decoded.entries, batch.entries);
    }

    #[test]
    fn batch_at_max_capacity_fits_under_mtu() {
        let entries: Vec<_> = (0..MAX_ACK_ENTRIES_PER_BATCH)
            .map(|i| AckEntry {
                frame_seq: i as u32,
                tile_x: 0,
                tile_y: 0,
                pass_idx: 0,
                arrival_time_ms_lo16: 0,
            })
            .collect();
        let batch = AckBatch { entries };
        let bytes = batch.encode();
        assert_eq!(bytes.len(), 2 + MAX_ACK_ENTRIES_PER_BATCH * ACK_ENTRY_SIZE);
        assert!(bytes.len() < 1200, "must fit under typical MTU");
        let decoded = AckBatch::decode(&bytes).expect("valid batch");
        assert_eq!(decoded.entries.len(), MAX_ACK_ENTRIES_PER_BATCH);
    }

    #[test]
    fn decode_rejects_old_msg_type_0x02() {
        let data = vec![0x02, 1, 3, 4, 0x21, 0, 0];
        let err = AckBatch::decode(&data).expect_err("must reject 0x02");
        assert!(matches!(err, AckDecodeError::WrongMsgType(0x02)));
    }

    #[test]
    fn decode_rejects_count_zero() {
        let data = vec![ACK_BATCH_MSG_TYPE, 0];
        assert!(matches!(
            AckBatch::decode(&data),
            Err(AckDecodeError::InvalidCount(0))
        ));
    }

    #[test]
    fn decode_rejects_truncated_entry_payload() {
        // header says count=2 (needs 20 bytes), only 9 bytes of entry data
        let mut data = vec![ACK_BATCH_MSG_TYPE, 2];
        data.extend_from_slice(&[0u8; 9]);
        assert!(matches!(
            AckBatch::decode(&data),
            Err(AckDecodeError::TooShort(_))
        ));
    }

    #[test]
    fn ack_envelope_v4_carries_arrival_time() {
        let entries = vec![
            AckEntry {
                frame_seq: 0x1234_5678,
                tile_x: 7,
                tile_y: 9,
                pass_idx: 3,
                arrival_time_ms_lo16: 0xABCD,
            },
            AckEntry {
                frame_seq: 0x9999_AAAA,
                tile_x: 1,
                tile_y: 2,
                pass_idx: 13,
                arrival_time_ms_lo16: 0x0010,
            },
        ];
        let batch = AckBatch { entries: entries.clone() };
        let bytes = batch.encode();
        // 2 header bytes + 2 entries × 9 bytes = 20 bytes
        assert_eq!(bytes.len(), 20);
        assert_eq!(bytes[0], ACK_BATCH_MSG_TYPE);
        assert_eq!(bytes[0], 0x04, "msg_type bumped to 0x04");
        assert_eq!(bytes[1], 2);

        let decoded = AckBatch::decode(&bytes).expect("round-trip");
        assert_eq!(decoded.entries, entries);
    }
}
