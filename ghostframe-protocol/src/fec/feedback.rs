//! Client -> server FEC feedback datagrams: ACK v4 (0x08, names a
//! `bundle_seq`), LOSS_REPORT (0x07) and REPAIR_REQUEST (0x0B).
//! All integers are little-endian.

pub const FEC_LOSS_REPORT_MSG_TYPE: u8 = 0x07;
pub const FEC_ACK_V4_MSG_TYPE: u8 = 0x08;
pub const FEC_REPAIR_REQUEST_MSG_TYPE: u8 = 0x0B;
pub const FEC_LOSS_REPORT_LEN: usize = 64;
pub const FEC_MAX_REPAIR_REQUEST_ENTRIES: usize = 16;
pub const FEC_MAX_ACK_V4_ENTRIES: usize = 72;
const LOSS_REPORT_VERSION: u8 = 1;
const REPAIR_REQUEST_VERSION: u8 = 1;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FeedbackError {
    #[error("datagram too short: {0} bytes")]
    TooShort(usize),
    #[error("wrong message type 0x{0:02x}")]
    WrongMsgType(u8),
    #[error("unsupported version {0}")]
    BadVersion(u8),
    #[error("invalid entry count {0}")]
    InvalidCount(u8),
    #[error("{0} trailing bytes")]
    TrailingBytes(usize),
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundleAck {
    pub bundle_seq: u32,
    pub arrival_time_ms_lo16: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleAckBatch {
    pub entries: Vec<BundleAck>,
}

impl BundleAckBatch {
    /// Panics if `entries` is empty or longer than 72.
    pub fn encode(&self) -> Vec<u8> {
        assert!((1..=FEC_MAX_ACK_V4_ENTRIES).contains(&self.entries.len()));
        let mut out = Vec::with_capacity(2 + 6 * self.entries.len());
        out.push(FEC_ACK_V4_MSG_TYPE);
        out.push(self.entries.len() as u8);
        for e in &self.entries {
            out.extend_from_slice(&e.bundle_seq.to_le_bytes());
            out.extend_from_slice(&e.arrival_time_ms_lo16.to_le_bytes());
        }
        out
    }

    pub fn decode(dg: &[u8]) -> Result<Self, FeedbackError> {
        if dg.len() < 2 {
            return Err(FeedbackError::TooShort(dg.len()));
        }
        if dg[0] != FEC_ACK_V4_MSG_TYPE {
            return Err(FeedbackError::WrongMsgType(dg[0]));
        }
        let n = dg[1];
        if n == 0 || usize::from(n) > FEC_MAX_ACK_V4_ENTRIES {
            return Err(FeedbackError::InvalidCount(n));
        }
        let want = 2 + 6 * usize::from(n);
        if dg.len() < want {
            return Err(FeedbackError::TooShort(dg.len()));
        }
        if dg.len() > want {
            return Err(FeedbackError::TrailingBytes(dg.len() - want));
        }
        let entries = dg[2..]
            .chunks_exact(6)
            .map(|c| BundleAck {
                bundle_seq: u32_at(c, 0),
                arrival_time_ms_lo16: u16_at(c, 4),
            })
            .collect();
        Ok(Self { entries })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LossReport {
    pub interval_ms: u16,
    pub first_bundle_seq: u32,
    pub last_bundle_seq: u32,
    pub received: u16,
    pub lost_pre_fec: u16,
    pub recovered: u16,
    pub residual_lost: u16,
    pub repair_rx: u16,
    pub repair_used: u16,
    pub repair_wasted: u16,
    pub loss_run_hist: [u16; 8],
    pub reorder_max: u16,
    pub recovery_p95_ms: u16,
    pub rx_buffer_drops: u16,
    pub recovery_allowance_ms: u16,
    pub inter_arrival_us: u16,
    pub blocks_unrecoverable: u16,
    pub repair_requests_sent: u16,
    pub repair_late: u16,
    pub repair_skipped_deadline: u16,
    pub src_lost_pre_fec: u16,
    pub h264_frames_lost: u16,
}

impl LossReport {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(FEC_LOSS_REPORT_LEN);
        o.push(FEC_LOSS_REPORT_MSG_TYPE);
        o.push(LOSS_REPORT_VERSION);
        o.extend_from_slice(&self.interval_ms.to_le_bytes());
        o.extend_from_slice(&self.first_bundle_seq.to_le_bytes());
        o.extend_from_slice(&self.last_bundle_seq.to_le_bytes());
        for v in [
            self.received,
            self.lost_pre_fec,
            self.recovered,
            self.residual_lost,
            self.repair_rx,
            self.repair_used,
            self.repair_wasted,
        ] {
            o.extend_from_slice(&v.to_le_bytes());
        }
        for v in self.loss_run_hist {
            o.extend_from_slice(&v.to_le_bytes());
        }
        for v in [
            self.reorder_max,
            self.recovery_p95_ms,
            self.rx_buffer_drops,
            self.recovery_allowance_ms,
            self.inter_arrival_us,
            self.blocks_unrecoverable,
            self.repair_requests_sent,
            self.repair_late,
            self.repair_skipped_deadline,
            self.src_lost_pre_fec,
            self.h264_frames_lost,
        ] {
            o.extend_from_slice(&v.to_le_bytes());
        }
        debug_assert_eq!(o.len(), FEC_LOSS_REPORT_LEN);
        o
    }

    /// Accepts any datagram of at least 64 bytes; trailing bytes belong to a
    /// later version and are ignored.
    pub fn decode(dg: &[u8]) -> Result<Self, FeedbackError> {
        if dg.len() < FEC_LOSS_REPORT_LEN {
            return Err(FeedbackError::TooShort(dg.len()));
        }
        if dg[0] != FEC_LOSS_REPORT_MSG_TYPE {
            return Err(FeedbackError::WrongMsgType(dg[0]));
        }
        if dg[1] != LOSS_REPORT_VERSION {
            return Err(FeedbackError::BadVersion(dg[1]));
        }
        let mut loss_run_hist = [0u16; 8];
        for (i, h) in loss_run_hist.iter_mut().enumerate() {
            *h = u16_at(dg, 26 + 2 * i);
        }
        Ok(Self {
            interval_ms: u16_at(dg, 2),
            first_bundle_seq: u32_at(dg, 4),
            last_bundle_seq: u32_at(dg, 8),
            received: u16_at(dg, 12),
            lost_pre_fec: u16_at(dg, 14),
            recovered: u16_at(dg, 16),
            residual_lost: u16_at(dg, 18),
            repair_rx: u16_at(dg, 20),
            repair_used: u16_at(dg, 22),
            repair_wasted: u16_at(dg, 24),
            loss_run_hist,
            reorder_max: u16_at(dg, 42),
            recovery_p95_ms: u16_at(dg, 44),
            rx_buffer_drops: u16_at(dg, 46),
            recovery_allowance_ms: u16_at(dg, 48),
            inter_arrival_us: u16_at(dg, 50),
            blocks_unrecoverable: u16_at(dg, 52),
            repair_requests_sent: u16_at(dg, 54),
            repair_late: u16_at(dg, 56),
            repair_skipped_deadline: u16_at(dg, 58),
            src_lost_pre_fec: u16_at(dg, 60),
            h264_frames_lost: u16_at(dg, 62),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepairAsk {
    pub block_id: u16,
    pub have: u8,
    pub missing: u8,
    pub first_new_idx: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairRequest {
    pub entries: Vec<RepairAsk>,
}

impl RepairRequest {
    /// Panics if `entries` is empty or longer than 16.
    pub fn encode(&self) -> Vec<u8> {
        assert!((1..=FEC_MAX_REPAIR_REQUEST_ENTRIES).contains(&self.entries.len()));
        let mut o = Vec::with_capacity(3 + 5 * self.entries.len());
        o.push(FEC_REPAIR_REQUEST_MSG_TYPE);
        o.push(REPAIR_REQUEST_VERSION);
        o.push(self.entries.len() as u8);
        for e in &self.entries {
            o.extend_from_slice(&e.block_id.to_le_bytes());
            o.extend_from_slice(&[e.have, e.missing, e.first_new_idx]);
        }
        o
    }

    pub fn decode(dg: &[u8]) -> Result<Self, FeedbackError> {
        if dg.len() < 3 {
            return Err(FeedbackError::TooShort(dg.len()));
        }
        if dg[0] != FEC_REPAIR_REQUEST_MSG_TYPE {
            return Err(FeedbackError::WrongMsgType(dg[0]));
        }
        if dg[1] != REPAIR_REQUEST_VERSION {
            return Err(FeedbackError::BadVersion(dg[1]));
        }
        let n = dg[2];
        if n == 0 || usize::from(n) > FEC_MAX_REPAIR_REQUEST_ENTRIES {
            return Err(FeedbackError::InvalidCount(n));
        }
        let want = 3 + 5 * usize::from(n);
        if dg.len() < want {
            return Err(FeedbackError::TooShort(dg.len()));
        }
        if dg.len() > want {
            return Err(FeedbackError::TrailingBytes(dg.len() - want));
        }
        let entries = dg[3..]
            .chunks_exact(5)
            .map(|c| RepairAsk {
                block_id: u16_at(c, 0),
                have: c[2],
                missing: c[3],
                first_new_idx: c[4],
            })
            .collect();
        Ok(Self { entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn ack_v4_layout_and_roundtrip() {
        let b = BundleAckBatch {
            entries: vec![BundleAck {
                bundle_seq: 0x0403_0201,
                arrival_time_ms_lo16: 0x0605,
            }],
        };
        let dg = b.encode();
        assert_eq!(dg, vec![0x08, 1, 1, 2, 3, 4, 5, 6]);
        assert_eq!(BundleAckBatch::decode(&dg), Ok(b));
    }

    #[test]
    fn ack_v4_rejects() {
        assert_eq!(
            BundleAckBatch::decode(&[0x08]),
            Err(FeedbackError::TooShort(1))
        );
        assert_eq!(
            BundleAckBatch::decode(&[0x06, 1, 0, 0, 0, 0, 0, 0]),
            Err(FeedbackError::WrongMsgType(0x06))
        );
        assert_eq!(
            BundleAckBatch::decode(&[0x08, 0]),
            Err(FeedbackError::InvalidCount(0))
        );
        assert_eq!(
            BundleAckBatch::decode(&[0x08, 73]),
            Err(FeedbackError::InvalidCount(73))
        );
        assert_eq!(
            BundleAckBatch::decode(&[0x08, 1, 0, 0, 0, 0, 0]),
            Err(FeedbackError::TooShort(7))
        );
        assert_eq!(
            BundleAckBatch::decode(&[0x08, 1, 0, 0, 0, 0, 0, 0, 9]),
            Err(FeedbackError::TrailingBytes(1))
        );
    }

    #[test]
    fn loss_report_layout_and_roundtrip() {
        let r = LossReport {
            interval_ms: 250,
            first_bundle_seq: 10,
            last_bundle_seq: 137,
            received: 120,
            lost_pre_fec: 8,
            recovered: 6,
            residual_lost: 2,
            loss_run_hist: [5, 1, 0, 0, 0, 0, 0, 1],
            h264_frames_lost: 3,
            ..Default::default()
        };
        let dg = r.encode();
        assert_eq!(dg.len(), FEC_LOSS_REPORT_LEN);
        assert_eq!(&dg[..4], &[0x07, 1, 250, 0]);
        assert_eq!(&dg[26..28], &5u16.to_le_bytes());
        assert_eq!(&dg[62..64], &3u16.to_le_bytes());
        assert_eq!(LossReport::decode(&dg), Ok(r.clone()));
        let mut longer = dg.clone();
        longer.extend_from_slice(&[0xEE; 6]);
        assert_eq!(
            LossReport::decode(&longer),
            Ok(r),
            "trailing bytes are a later version's fields"
        );
        assert_eq!(
            LossReport::decode(&dg[..63]),
            Err(FeedbackError::TooShort(63))
        );
        let mut v2 = dg;
        v2[1] = 2;
        assert_eq!(LossReport::decode(&v2), Err(FeedbackError::BadVersion(2)));
    }

    #[test]
    fn repair_request_layout_and_rejects() {
        let q = RepairRequest {
            entries: vec![RepairAsk {
                block_id: 0x0201,
                have: 8,
                missing: 2,
                first_new_idx: 3,
            }],
        };
        let dg = q.encode();
        assert_eq!(dg, vec![0x0B, 1, 1, 1, 2, 8, 2, 3]);
        assert_eq!(RepairRequest::decode(&dg), Ok(q));
        assert_eq!(
            RepairRequest::decode(&[0x0B, 1, 0]),
            Err(FeedbackError::InvalidCount(0))
        );
        assert_eq!(
            RepairRequest::decode(&[0x0B, 1, 17]),
            Err(FeedbackError::InvalidCount(17))
        );
        assert_eq!(
            RepairRequest::decode(&[0x0B, 2, 1, 0, 0, 0, 0, 0]),
            Err(FeedbackError::BadVersion(2))
        );
    }

    proptest! {
        #[test]
        fn decoders_never_panic(dg in proptest::collection::vec(any::<u8>(), 0..600)) {
            let _ = BundleAckBatch::decode(&dg);
            let _ = LossReport::decode(&dg);
            let _ = RepairRequest::decode(&dg);
        }
    }
}
