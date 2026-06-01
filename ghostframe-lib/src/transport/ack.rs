//! Batched-ACK datagram protocol — per-datagram acknowledgment (M3.3d).
//!
//! Wire format:
//! ```text
//! [0]      message_type = 0x03
//! [1]      count: u8 (1..=64)
//! [2..]    count × 6 bytes:
//!             [0..4]  frame_seq: u32 little-endian
//!             [4..6]  frag_idx:  u16 little-endian
//! ```
//!
//! Each entry acknowledges receipt of the datagram identified by
//! `(frame_seq, frag_idx)`. Server uses its `FragmentCoverageMap` to
//! convert datagram-level ACKs into per-tile delivery bookkeeping
//! (Cdf53 ACK counter, PalRle palette delivered tracking, etc.).
//!
//! Pre-release wire break: the previous format (M3.3c) was
//! `message_type=0x02` with per-tile entries `(tile_x, tile_y,
//! packed(gen,pass), reserved)`. Bumping the message-type byte means a
//! stale dev binary mid-rollout fails loud with
//! `AckDecodeError::WrongMsgType(0x02)` rather than silently mis-parsing.

pub const ACK_BATCH_MSG_TYPE: u8 = 0x03;
pub const MAX_ACK_ENTRIES_PER_BATCH: usize = 64;
pub const ACK_ENTRY_SIZE: usize = 6;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AckDecodeError {
    #[error("ack batch too short ({0} bytes)")]
    TooShort(usize),
    #[error("wrong message type: expected 0x03, got 0x{0:02x}")]
    WrongMsgType(u8),
    #[error("invalid entry count: {0} (must be 1..=64)")]
    InvalidCount(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckEntry {
    pub frame_seq: u32,
    pub frag_idx: u16,
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
            out.extend_from_slice(&e.frag_idx.to_le_bytes());
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
            let frame_seq = u32::from_le_bytes([
                data[off], data[off + 1], data[off + 2], data[off + 3],
            ]);
            let frag_idx = u16::from_le_bytes([data[off + 4], data[off + 5]]);
            entries.push(AckEntry { frame_seq, frag_idx });
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
            entries: vec![AckEntry { frame_seq: 0x1234_5678, frag_idx: 0x9ABC }],
        };
        let bytes = batch.encode();
        assert_eq!(bytes[0], ACK_BATCH_MSG_TYPE, "msg type = 0x03");
        assert_eq!(bytes[0], 0x03);
        assert_eq!(bytes[1], 1, "count = 1");
        assert_eq!(bytes[2..6], [0x78, 0x56, 0x34, 0x12], "frame_seq LE");
        assert_eq!(bytes[6..8], [0xBC, 0x9A], "frag_idx LE");
        let decoded = AckBatch::decode(&bytes).expect("valid batch");
        assert_eq!(decoded.entries, batch.entries);
    }

    #[test]
    fn batch_at_max_capacity_fits_under_mtu() {
        let entries: Vec<_> = (0..MAX_ACK_ENTRIES_PER_BATCH)
            .map(|i| AckEntry { frame_seq: i as u32, frag_idx: 0 })
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
        // Pre-release wire break: clients still sending old format must fail
        // loud rather than silently mis-parse 4-byte entries as 6-byte ones.
        let data = vec![0x02, 1, 3, 4, 0x21, 0];
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
        // header says count=2 (needs 12 bytes), only 6 bytes of entry data
        let mut data = vec![ACK_BATCH_MSG_TYPE, 2];
        data.extend_from_slice(&[0u8; 6]);
        assert!(matches!(
            AckBatch::decode(&data),
            Err(AckDecodeError::TooShort(_))
        ));
    }
}
