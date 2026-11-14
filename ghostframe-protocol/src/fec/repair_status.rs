// SPDX-License-Identifier: MIT OR Apache-2.0
//! Records of the reliable client -> server repair-status stream:
//! `[type u8][len u16 LE][body]`, `len` <= 1020. Any violation is a protocol
//! error; the caller closes the stream.

pub const FEC_STATUS_RECOVERED: u8 = 0x10;
pub const FEC_STATUS_CONTENT_REJECTED: u8 = 0x11;
pub const FEC_STATUS_DECLINED: u8 = 0x12;
pub const FEC_STATUS_MAX_RECORD_BODY: usize = 1020;
/// First byte of the repair-status bidi stream, before the first record. The
/// server routes every client-opened bidi stream to its feedback handler, so
/// this tag tells it which stream it is looking at. No feedback-stream message
/// type is >= 0x80, so a feedback stream never begins with it.
pub const FEC_REPAIR_STATUS_STREAM_TAG: u8 = 0xF5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclineReason {
    Pruned = 1,
    Evicted = 2,
    InvalidSymbol = 3,
    LengthBound = 4,
    HeaderMismatch = 5,
}

impl DeclineReason {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Pruned),
            2 => Some(Self::Evicted),
            3 => Some(Self::InvalidSymbol),
            4 => Some(Self::LengthBound),
            5 => Some(Self::HeaderMismatch),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusRecord {
    Recovered(Vec<u32>),
    ContentRejected(Vec<(u32, u8)>),
    Declined {
        block_id: u16,
        reason: DeclineReason,
        k: u8,
        missing: Vec<u8>,
    },
}

impl StatusRecord {
    /// Panics if the body would exceed `FEC_STATUS_MAX_RECORD_BODY` or a list is empty.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut body = Vec::new();
        let kind = match self {
            Self::Recovered(v) => {
                assert!(!v.is_empty());
                for s in v {
                    body.extend_from_slice(&s.to_le_bytes());
                }
                FEC_STATUS_RECOVERED
            }
            Self::ContentRejected(v) => {
                assert!(!v.is_empty());
                for (s, idx) in v {
                    body.extend_from_slice(&s.to_le_bytes());
                    body.push(*idx);
                }
                FEC_STATUS_CONTENT_REJECTED
            }
            Self::Declined {
                block_id,
                reason,
                k,
                missing,
            } => {
                debug_assert_eq!(missing.len(), usize::from(*k).div_ceil(8));
                debug_assert!(
                    (usize::from(*k)..missing.len() * 8)
                        .all(|i| missing[i / 8] >> (i % 8) & 1 == 0),
                    "bit set beyond k"
                );
                body.extend_from_slice(&block_id.to_le_bytes());
                body.push(*reason as u8);
                body.push(*k);
                body.extend_from_slice(missing);
                FEC_STATUS_DECLINED
            }
        };
        assert!(body.len() <= FEC_STATUS_MAX_RECORD_BODY);
        out.push(kind);
        out.extend_from_slice(&(body.len() as u16).to_le_bytes());
        out.extend_from_slice(&body);
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum StatusError {
    #[error("unknown record type 0x{0:02x}")]
    UnknownType(u8),
    #[error("bad length {len} for record type 0x{kind:02x}")]
    BadLength { kind: u8, len: usize },
    #[error("bad DECLINED record: {0}")]
    BadDecline(&'static str),
}

fn check_len(kind: u8, len: usize) -> Result<(), StatusError> {
    let ok = match kind {
        FEC_STATUS_RECOVERED => len.is_multiple_of(4),
        FEC_STATUS_CONTENT_REJECTED => len.is_multiple_of(5),
        // 4 + ceil(k/8), k in 1..=255
        _ => (5..=36).contains(&len),
    };
    if len == 0 || len > FEC_STATUS_MAX_RECORD_BODY || !ok {
        return Err(StatusError::BadLength { kind, len });
    }
    Ok(())
}

fn decode_body(kind: u8, body: &[u8]) -> Result<StatusRecord, StatusError> {
    match kind {
        FEC_STATUS_RECOVERED => Ok(StatusRecord::Recovered(
            body.chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        )),
        FEC_STATUS_CONTENT_REJECTED => Ok(StatusRecord::ContentRejected(
            body.chunks_exact(5)
                .map(|c| (u32::from_le_bytes([c[0], c[1], c[2], c[3]]), c[4]))
                .collect(),
        )),
        _ => {
            let block_id = u16::from_le_bytes([body[0], body[1]]);
            let reason =
                DeclineReason::from_u8(body[2]).ok_or(StatusError::BadDecline("reason"))?;
            let k = body[3];
            if k == 0 {
                return Err(StatusError::BadDecline("k is zero"));
            }
            let k_us = usize::from(k);
            let missing = &body[4..];
            if missing.len() != k_us.div_ceil(8) {
                return Err(StatusError::BadDecline("bitmap length"));
            }
            if (k_us..missing.len() * 8).any(|i| missing[i / 8] >> (i % 8) & 1 != 0) {
                return Err(StatusError::BadDecline("bit set beyond k"));
            }
            Ok(StatusRecord::Declined {
                block_id,
                reason,
                k,
                missing: missing.to_vec(),
            })
        }
    }
}

#[derive(Debug, Default)]
pub struct StatusReader {
    buf: Vec<u8>,
    failed: Option<StatusError>,
}

impl StatusReader {
    /// Append stream bytes; return every complete record, keep a partial tail.
    ///
    /// After an error the reader is failed for good: every later call returns
    /// the same error without touching the buffer. Records decoded earlier in
    /// the failing call are discarded; the caller must close the stream.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<StatusRecord>, StatusError> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        match self.push_inner(bytes) {
            Ok(v) => Ok(v),
            Err(e) => {
                self.failed = Some(e.clone());
                Err(e)
            }
        }
    }

    fn push_inner(&mut self, bytes: &[u8]) -> Result<Vec<StatusRecord>, StatusError> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut pos = 0;
        while self.buf.len() - pos >= 3 {
            let kind = self.buf[pos];
            let len = usize::from(u16::from_le_bytes([self.buf[pos + 1], self.buf[pos + 2]]));
            if !matches!(
                kind,
                FEC_STATUS_RECOVERED | FEC_STATUS_CONTENT_REJECTED | FEC_STATUS_DECLINED
            ) {
                return Err(StatusError::UnknownType(kind));
            }
            check_len(kind, len)?;
            if self.buf.len() - pos < 3 + len {
                break;
            }
            out.push(decode_body(kind, &self.buf[pos + 3..pos + 3 + len])?);
            pos += 3 + len;
        }
        self.buf.drain(..pos);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(r: &StatusRecord) -> Vec<u8> {
        let mut v = Vec::new();
        r.encode(&mut v);
        v
    }

    #[test]
    fn layouts() {
        assert_eq!(
            enc(&StatusRecord::Recovered(vec![0x0403_0201])),
            vec![0x10, 4, 0, 1, 2, 3, 4]
        );
        assert_eq!(
            enc(&StatusRecord::ContentRejected(vec![(7, 2)])),
            vec![0x11, 5, 0, 7, 0, 0, 0, 2]
        );
        let d = StatusRecord::Declined {
            block_id: 0x0201,
            reason: DeclineReason::Pruned,
            k: 10,
            missing: vec![0b0000_0101, 0b0000_0010],
        };
        assert_eq!(enc(&d), vec![0x12, 6, 0, 1, 2, 1, 10, 0b101, 0b10]);
    }

    #[test]
    fn reader_handles_any_split() {
        let recs = vec![
            StatusRecord::Recovered(vec![1, 2, 3]),
            StatusRecord::Declined {
                block_id: 9,
                reason: DeclineReason::Evicted,
                k: 3,
                missing: vec![0b100],
            },
            StatusRecord::ContentRejected(vec![(5, 0), (6, 1)]),
        ];
        let mut stream = Vec::new();
        for r in &recs {
            r.encode(&mut stream);
        }
        for cut in 0..=stream.len() {
            let mut rd = StatusReader::default();
            let mut got = rd.push(&stream[..cut]).unwrap();
            got.extend(rd.push(&stream[cut..]).unwrap());
            assert_eq!(got, recs, "split at {cut}");
        }
    }

    #[test]
    fn reader_rejects() {
        let mut rd = StatusReader::default();
        assert_eq!(
            rd.push(&[0x13, 4, 0, 0, 0, 0, 0]),
            Err(StatusError::UnknownType(0x13))
        );
        let mut rd = StatusReader::default();
        assert_eq!(
            rd.push(&[0x10, 0, 0]),
            Err(StatusError::BadLength { kind: 0x10, len: 0 })
        );
        let mut rd = StatusReader::default();
        assert_eq!(
            rd.push(&[0x10, 5, 0]),
            Err(StatusError::BadLength { kind: 0x10, len: 5 })
        );
        let mut rd = StatusReader::default();
        assert_eq!(
            rd.push(&[0x10, 0x00, 0x04]),
            Err(StatusError::BadLength {
                kind: 0x10,
                len: 1024
            })
        );
        let mut rd = StatusReader::default();
        // k = 3 needs 1 bitmap byte; a set bit 3 is beyond k
        assert!(matches!(
            rd.push(&[0x12, 5, 0, 0, 0, 1, 3, 0b1000]),
            Err(StatusError::BadDecline(_))
        ));
        let mut rd = StatusReader::default();
        assert!(
            matches!(
                rd.push(&[0x12, 5, 0, 0, 0, 9, 3, 0]),
                Err(StatusError::BadDecline(_))
            ),
            "reason 9"
        );
        let mut rd = StatusReader::default();
        assert!(
            matches!(
                rd.push(&[0x12, 6, 0, 0, 0, 1, 3, 0, 0]),
                Err(StatusError::BadDecline(_))
            ),
            "len != 4 + ceil(k/8)"
        );
    }

    #[test]
    fn reader_is_sticky_failed() {
        let mut rd = StatusReader::default();
        let e = rd.push(&[0x13, 4, 0]).unwrap_err();
        assert_eq!(e, StatusError::UnknownType(0x13));
        let mut ok = Vec::new();
        StatusRecord::Recovered(vec![1]).encode(&mut ok);
        assert_eq!(rd.push(&ok), Err(e));
    }

    #[test]
    fn declined_len_bound_is_immediate() {
        let mut rd = StatusReader::default();
        assert_eq!(
            rd.push(&[0x12, 37, 0]),
            Err(StatusError::BadLength {
                kind: 0x12,
                len: 37
            })
        );
    }
}
