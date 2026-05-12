//! Batched-ACK datagram protocol for tile-codec retransmission control.
//!
//! Wire format:
//! ```text
//! [0]      message_type = 0x02
//! [1]      count: u8 (1..=64)
//! [2..]    count × 4 bytes:
//!             [0] tile_x
//!             [1] tile_y
//!             [2] (generation << 4) | pass
//!             [3] reserved (0)
//! ```

pub const ACK_BATCH_MSG_TYPE: u8 = 0x02;
pub const MAX_ACK_ENTRIES_PER_BATCH: usize = 64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AckDecodeError {
    #[error("ack batch too short ({0} bytes)")]
    TooShort(usize),
    #[error("wrong message type: expected 0x02, got 0x{0:02x}")]
    WrongMsgType(u8),
    #[error("invalid entry count: {0} (must be 1..=64)")]
    InvalidCount(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckEntry {
    pub tile_x: u8,
    pub tile_y: u8,
    pub generation: u8, // 4 bits
    pub pass: u8,       // 4 bits
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckBatch {
    pub entries: Vec<AckEntry>,
}

impl AckBatch {
    pub fn encode(&self) -> Vec<u8> {
        debug_assert!((1..=MAX_ACK_ENTRIES_PER_BATCH).contains(&self.entries.len()));
        let mut out = Vec::with_capacity(2 + self.entries.len() * 4);
        out.push(ACK_BATCH_MSG_TYPE);
        out.push(self.entries.len() as u8);
        for e in &self.entries {
            out.push(e.tile_x);
            out.push(e.tile_y);
            out.push(((e.generation & 0x0F) << 4) | (e.pass & 0x0F));
            out.push(0); // reserved
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
        let need = 2 + (count as usize) * 4;
        if data.len() < need {
            return Err(AckDecodeError::TooShort(data.len()));
        }
        let mut entries = Vec::with_capacity(count as usize);
        for i in 0..(count as usize) {
            let off = 2 + i * 4;
            let packed = data[off + 2];
            entries.push(AckEntry {
                tile_x: data[off],
                tile_y: data[off + 1],
                generation: packed >> 4,
                pass: packed & 0x0F,
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
                tile_x: 3,
                tile_y: 4,
                generation: 2,
                pass: 1,
            }],
        };
        let bytes = batch.encode();
        assert_eq!(bytes[0], ACK_BATCH_MSG_TYPE);
        assert_eq!(bytes[1], 1);
        assert_eq!(bytes[2], 3);
        assert_eq!(bytes[3], 4);
        assert_eq!(bytes[4], (2 << 4) | 1);
        assert_eq!(bytes[5], 0);
        let decoded = AckBatch::decode(&bytes).expect("valid batch");
        assert_eq!(decoded.entries, batch.entries);
    }

    #[test]
    fn batch_at_max_capacity_fits_under_mtu() {
        let entries: Vec<_> = (0..64)
            .map(|i| AckEntry {
                tile_x: i as u8,
                tile_y: 0,
                generation: 0,
                pass: 0,
            })
            .collect();
        let batch = AckBatch { entries };
        let bytes = batch.encode();
        assert_eq!(bytes.len(), 1 + 1 + 64 * 4);
        assert!(bytes.len() < 1200);
        let decoded = AckBatch::decode(&bytes).expect("valid batch");
        assert_eq!(decoded.entries.len(), 64);
    }

    #[test]
    fn decode_rejects_wrong_msg_type() {
        let data = vec![0x01, 1, 0, 0, 0, 0];
        assert!(AckBatch::decode(&data).is_err());
    }

    #[test]
    fn decode_rejects_count_zero() {
        let data = vec![ACK_BATCH_MSG_TYPE, 0];
        assert!(AckBatch::decode(&data).is_err());
    }

    #[test]
    fn decode_rejects_count_over_64() {
        let data = vec![ACK_BATCH_MSG_TYPE, 65, 0, 0, 0, 0];
        assert!(AckBatch::decode(&data).is_err());
    }

    #[test]
    fn decode_rejects_truncated_entries() {
        // count=2 but only 1 entry worth of bytes follows
        let data = vec![ACK_BATCH_MSG_TYPE, 2, 1, 2, 3, 4];
        assert!(AckBatch::decode(&data).is_err());
    }

    #[test]
    fn decode_rejects_too_short_for_header() {
        assert!(AckBatch::decode(&[]).is_err());
        assert!(AckBatch::decode(&[ACK_BATCH_MSG_TYPE]).is_err());
    }
}
