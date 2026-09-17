//! Batched-ACK datagram protocol — per-*transmission* acknowledgment (rev3).
//!
//! Wire format:
//! ```text
//! [0]      message_type = 0x06
//! [1]      count: u8 (1..=72)
//! [2..]    count × 6 bytes:
//!             [0..4]  wire_seq: u32 little-endian
//!             [4..6]  arrival_time_ms_lo16: u16 little-endian
//! ```
//!
//! Max count is 72 — `MAX_FRESH_ENTRIES_PER_BATCH (64)` + the
//! client-side `ACK_OVERLAP_COUNT (8)` trailing entries that each
//! batch carries from the previous one as resilience against a single
//! ACK-batch loss. The server MUST accept anything up to that combined
//! limit or the overlap mechanism collapses (overlap-bearing batches
//! get rejected, every retransmit floods MAX_RETRANSMITS without ACK).
//!
//! Each entry acknowledges one **transmission**, named by the `wire_seq` the
//! server stamped into the datagram header. The server maps it back to the
//! tile-pass it carried, so `FragmentCoverageMap` and the rest of the delivery
//! bookkeeping (Cdf53 ACK counter, PalRle palette tracking, retransmit
//! cancellation) are unchanged.
//!
//! Rev3 change, and the reason for it: rev2's key
//! `(frame_seq, tile_x, tile_y, pass_idx)` named *content*, not a
//! transmission. Two consequences, both of which were worked around rather
//! than fixed. An acknowledgement could not say which of several sends of the
//! same pass it referred to, which is why a Karn-style filter was tried and
//! reverted after it discarded 99% of timing samples on a congested link. And
//! a transmission whose content was superseded before acknowledgement simply
//! vanished from the accounting, because the retransmit cache that held it is
//! emptied on supersession — leaving goog_cc seeing a 0.4-3% loss fraction on
//! a link shedding ~7% by bytes, and a 4x overestimate of a degraded link
//! standing.
//!
//! `wire_seq` is unique per transmission (retransmissions allocate their own),
//! so both problems are structural rather than patched.
//!
//! Rev2 for the record: the M3.3d initial wire format used
//! `(frame_seq, frag_idx)`, which collides for multiple single-fragment work
//! items per frame (all get frag_idx=0).
//!
//! Pre-release wire break: the previous format (M3.3c) was
//! `message_type=0x02` with per-tile entries `(tile_x, tile_y,
//! packed(gen,pass), reserved)`. Bumping the message-type byte means a
//! stale dev binary mid-rollout fails loud with
//! `AckDecodeError::WrongMsgType(0x02)` rather than silently mis-parsing.

/// ACK envelope wire-format version. Bumped 0x04 → 0x06 in 2026-09-17 when
/// entries switched from naming a tile-pass to naming a transmission.
/// 0x05 is skipped: it is `TILE_NACK_ENVELOPE`, and an ACK batch landing in
/// the NACK handler would be silently dropped by the wrong decoder — a
/// collision this project has already shipped once and caught late.
///
/// Old clients/servers are not wire-compatible with new; both sides ship in
/// lockstep, and the bumped byte makes a stale binary fail loud with
/// `WrongMsgType` rather than mis-parse.
pub const ACK_BATCH_MSG_TYPE: u8 = 0x06;
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
pub const MAX_ACK_ENTRIES_PER_BATCH: usize = MAX_FRESH_ENTRIES_PER_BATCH + ACK_OVERLAP_COUNT;
pub const ACK_ENTRY_SIZE: usize = 6;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AckDecodeError {
    #[error("ack batch too short ({0} bytes)")]
    TooShort(usize),
    #[error("wrong message type: expected 0x06, got 0x{0:02x}")]
    WrongMsgType(u8),
    #[error("invalid entry count: {0} (must be 1..=72)")]
    InvalidCount(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckEntry {
    /// The `wire_seq` the server stamped into this datagram's header. Unique
    /// per transmission, so it identifies *which send* arrived rather than
    /// merely which content.
    pub wire_seq: u32,
    /// Low 16 bits of the client's wall-clock millisecond receive time
    /// for this transmission. 65.5 s wrap; absolute clock skew doesn't matter
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
            out.extend_from_slice(&e.wire_seq.to_le_bytes());
            // Bytes [4..6]: arrival_time_ms_lo16, little-endian to match the
            // rest of the entry's little-endian fields.
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
            let wire_seq =
                u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
            let arrival_time_ms_lo16 = (data[off + 4] as u16) | ((data[off + 5] as u16) << 8);
            entries.push(AckEntry {
                wire_seq,
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
                wire_seq: 0x1234_5678,
                arrival_time_ms_lo16: 0xBEEF,
            }],
        };
        let bytes = batch.encode();
        assert_eq!(bytes[0], ACK_BATCH_MSG_TYPE, "msg type = 0x06");
        assert_eq!(bytes[0], 0x06);
        assert_eq!(bytes[1], 1, "count = 1");
        assert_eq!(bytes[2..6], [0x78, 0x56, 0x34, 0x12], "wire_seq LE");
        assert_eq!(bytes[6..8], [0xEF, 0xBE], "arrival LE");
        let decoded = AckBatch::decode(&bytes).expect("valid batch");
        assert_eq!(decoded.entries, batch.entries);
    }

    #[test]
    fn batch_at_max_capacity_fits_under_mtu() {
        let entries: Vec<_> = (0..MAX_ACK_ENTRIES_PER_BATCH)
            .map(|i| AckEntry {
                wire_seq: i as u32,
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
    fn decode_rejects_the_superseded_tile_pass_format() {
        // rev2 (0x04) named a tile-pass rather than a transmission. A stale
        // peer still speaking it must fail loud here rather than have its
        // 9-byte entries mis-read as 6-byte ones, which would decode into
        // plausible-looking nonsense.
        let data = vec![0x04, 1, 0x78, 0x56, 0x34, 0x12, 3, 7, 13, 0, 0];
        let err = AckBatch::decode(&data).expect_err("must reject rev2");
        assert!(matches!(err, AckDecodeError::WrongMsgType(0x04)));
    }

    #[test]
    fn the_msg_type_does_not_collide_with_the_nack_envelope() {
        // 0x05 is TILE_NACK_ENVELOPE. An ACK batch numbered into it routes to
        // the NACK handler and is silently dropped by the wrong decoder —
        // this project shipped exactly that collision once and caught it late.
        assert_ne!(ACK_BATCH_MSG_TYPE, crate::protocol::TILE_NACK_ENVELOPE);
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
    fn ack_envelope_carries_arrival_time() {
        let entries = vec![
            AckEntry {
                wire_seq: 0x1234_5678,
                arrival_time_ms_lo16: 0xABCD,
            },
            AckEntry {
                wire_seq: 0x9999_AAAA,
                arrival_time_ms_lo16: 0x0010,
            },
        ];
        let batch = AckBatch {
            entries: entries.clone(),
        };
        let bytes = batch.encode();
        // 2 header bytes + 2 entries × 6 bytes = 14 bytes
        assert_eq!(bytes.len(), 14);
        assert_eq!(bytes[0], ACK_BATCH_MSG_TYPE);
        assert_eq!(bytes[0], 0x06, "msg_type bumped to 0x06");
        assert_eq!(bytes[1], 2);

        let decoded = AckBatch::decode(&bytes).expect("round-trip");
        assert_eq!(decoded.entries, entries);
    }
}
