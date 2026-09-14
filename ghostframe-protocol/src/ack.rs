//! Batched-ACK datagram protocol — per-tile-pass acknowledgment (M3.3d rev2).
//!
//! Wire format:
//! ```text
//! [0]      message_type = 0x05
//! [1]      count_fresh: u8    (0..=MAX_FRESH_ENTRIES_PER_BATCH)
//! [2]      count_overlap: u8  (0..=ACK_OVERLAP_COUNT)
//! [3..7]   base_arrival_us: u32 little-endian
//! [7..]    count_fresh × 9 bytes (fresh entries):
//!             [0..4]  frame_seq: u32 little-endian
//!             [4]     tile_x: u8
//!             [5]     tile_y: u8
//!             [6]     pass_idx: u8
//!             [7..9]  arrival_delta_us: u16 little-endian
//!                     (microseconds after base_arrival_us, wrapping in
//!                     32-bit space)
//! [..]     count_overlap × 11 bytes (overlap entries):
//!             [0..4]  frame_seq: u32 little-endian
//!             [4]     tile_x: u8
//!             [5]     tile_y: u8
//!             [6]     pass_idx: u8
//!             [7..11] arrival_us: u32 little-endian (absolute, low 32
//!                     bits of wall-clock microseconds)
//! ```
//!
//! Two sections, two encodings, because fresh and overlap entries have
//! genuinely different temporal shapes:
//!
//! - **Fresh** entries are newly-received tile-passes. The client flushes
//!   a batch every `FLUSH_INTERVAL_US` (5ms), so all fresh entries in one
//!   batch span at most 5ms — comfortably inside a `u16` microsecond delta
//!   from a per-batch base (65.535ms of range). This is also what fixes the
//!   bug this format exists to fix: sub-millisecond arrival spacing was
//!   unrepresentable in the old millisecond wire format, so goog_cc computed
//!   0 or near-0 intervals for probe packets that actually arrived
//!   microseconds apart, and rejected the majority of probe measurements
//!   ("invalid send/receive interval", "receive/send ratio too high").
//! - **Overlap** entries are up to `ACK_OVERLAP_COUNT` entries replayed from
//!   previous batches, as resilience against a lost ACK batch. These can be
//!   arbitrarily old — seconds, on a sparse link — so they carry an absolute
//!   (low-32-bit) microsecond timestamp instead of a delta. An earlier
//!   design dropped overlap entries too old to encode as a delta; that was
//!   rejected because overlap exceeds 65ms of age precisely when traffic is
//!   sparse, which is precisely when a lost previous batch matters most.
//!   Nothing is ever dropped to make a batch encode.
//!
//! Max fresh count is `MAX_FRESH_ENTRIES_PER_BATCH (64)`; max overlap count
//! is `ACK_OVERLAP_COUNT (8)`. The server MUST accept anything up to those
//! caps or the overlap mechanism collapses (overlap-bearing batches get
//! rejected, every retransmit floods MAX_RETRANSMITS without ACK).
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

/// ACK envelope wire-format version. Bumped 0x04 → 0x05 in 2026-09 to move
/// from millisecond to microsecond arrival timestamps: goog_cc was
/// discarding the majority of probe measurements because sub-millisecond
/// inter-packet arrival spacing was unrepresentable in the old format. Old
/// (0x04) clients/servers are not wire-compatible with new — both sides
/// ship in lockstep.
pub const ACK_BATCH_MSG_TYPE: u8 = 0x05;
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
/// `[msg_type][count_fresh][count_overlap][base_arrival_us: u32]`
pub const ACK_HEADER_SIZE: usize = 1 + 1 + 1 + 4;
/// `[frame_seq: u32][tile_x][tile_y][pass_idx][arrival_delta_us: u16]`
pub const ACK_FRESH_ENTRY_SIZE: usize = 9;
/// `[frame_seq: u32][tile_x][tile_y][pass_idx][arrival_us: u32]`
pub const ACK_OVERLAP_ENTRY_SIZE: usize = 11;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AckDecodeError {
    #[error("ack batch too short ({0} bytes)")]
    TooShort(usize),
    #[error("wrong message type: expected 0x05, got 0x{0:02x}")]
    WrongMsgType(u8),
    #[error("invalid entry count: {0}")]
    InvalidCount(u8),
}

/// Encoding refuses rather than silently mangles a malformed or
/// out-of-range batch. This cannot happen with correctly-constructed
/// input (`fresh_count <= entries.len()`, section lengths within their
/// caps, and fresh entries within `FLUSH_INTERVAL_US` of the batch base),
/// so reaching any of these is a caller bug — and silently clamping or
/// truncating would feed the estimator a wrong arrival time, or reinterpret
/// the fresh/overlap boundary, which is the class of defect this format
/// exists to end.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AckEncodeError {
    #[error("fresh entry {index} is {delta_us}us from the batch base, over the u16 limit")]
    FreshDeltaOverflow { index: usize, delta_us: u64 },
    #[error("fresh_count {fresh_count} exceeds {entries} entries")]
    FreshCountExceedsEntries { fresh_count: usize, entries: usize },
    #[error("{count} {section} entries exceeds the cap of {cap}")]
    TooManyEntries {
        section: &'static str,
        count: usize,
        cap: usize,
    },
    /// A batch with zero fresh and zero overlap entries would encode as
    /// `count_fresh=0, count_overlap=0`, which `decode` correctly refuses
    /// (`AckDecodeError::InvalidCount(0)`) as carrying nothing to
    /// acknowledge. Refusing it here too keeps encode/decode symmetric:
    /// anything `try_encode` emits, `decode` must accept.
    #[error("batch has no fresh and no overlap entries")]
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckEntry {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
    /// Client's wall-clock receive time for this pass, in microseconds,
    /// wrapped to the low 32 bits. Absolute clock skew doesn't matter
    /// because the BWE consumer looks at *relative* arrival differences
    /// between packets; the wrap-width just needs to exceed any interval
    /// goog_cc cares about, which 32-bit microseconds (~71.6 minutes) does.
    pub arrival_us: u64,
}

/// One ACK batch. `entries[..fresh_count]` are freshly-received tile-passes
/// (encoded as small deltas from a per-batch base); `entries[fresh_count..]`
/// are overlap entries replayed from previous batches (encoded as absolute
/// timestamps, since they may be arbitrarily old). Kept as a single vector
/// so consumers can iterate uniformly over all entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckBatch {
    pub entries: Vec<AckEntry>,
    pub fresh_count: usize,
}

impl AckBatch {
    /// Encode this batch, panicking if the batch is malformed or a fresh
    /// entry's delta from the batch base overflows `u16`. Use `try_encode`
    /// to handle those cases instead.
    pub fn encode(&self) -> Vec<u8> {
        self.try_encode().expect("AckBatch must be well-formed: fresh_count <= entries.len(), section counts within their caps, and fresh entries within FLUSH_INTERVAL_US of the batch base")
    }

    pub fn try_encode(&self) -> Result<Vec<u8>, AckEncodeError> {
        if self.fresh_count > self.entries.len() {
            return Err(AckEncodeError::FreshCountExceedsEntries {
                fresh_count: self.fresh_count,
                entries: self.entries.len(),
            });
        }
        let fresh = &self.entries[..self.fresh_count];
        let overlap = &self.entries[self.fresh_count..];
        if fresh.is_empty() && overlap.is_empty() {
            return Err(AckEncodeError::Empty);
        }
        if fresh.len() > MAX_FRESH_ENTRIES_PER_BATCH {
            return Err(AckEncodeError::TooManyEntries {
                section: "fresh",
                count: fresh.len(),
                cap: MAX_FRESH_ENTRIES_PER_BATCH,
            });
        }
        if overlap.len() > ACK_OVERLAP_COUNT {
            return Err(AckEncodeError::TooManyEntries {
                section: "overlap",
                count: overlap.len(),
                cap: ACK_OVERLAP_COUNT,
            });
        }
        let base_arrival_us: u32 = fresh.first().map(|e| e.arrival_us as u32).unwrap_or(0);

        let mut out = Vec::with_capacity(
            ACK_HEADER_SIZE
                + fresh.len() * ACK_FRESH_ENTRY_SIZE
                + overlap.len() * ACK_OVERLAP_ENTRY_SIZE,
        );
        out.push(ACK_BATCH_MSG_TYPE);
        out.push(fresh.len() as u8);
        out.push(overlap.len() as u8);
        out.extend_from_slice(&base_arrival_us.to_le_bytes());

        for (i, e) in fresh.iter().enumerate() {
            let delta_us = (e.arrival_us as u32).wrapping_sub(base_arrival_us) as u64;
            let delta: u16 = delta_us
                .try_into()
                .map_err(|_| AckEncodeError::FreshDeltaOverflow { index: i, delta_us })?;
            out.extend_from_slice(&e.frame_seq.to_le_bytes());
            out.push(e.tile_x);
            out.push(e.tile_y);
            out.push(e.pass_idx);
            out.extend_from_slice(&delta.to_le_bytes());
        }

        for e in overlap {
            out.extend_from_slice(&e.frame_seq.to_le_bytes());
            out.push(e.tile_x);
            out.push(e.tile_y);
            out.push(e.pass_idx);
            out.extend_from_slice(&(e.arrival_us as u32).to_le_bytes());
        }

        Ok(out)
    }

    pub fn decode(data: &[u8]) -> Result<Self, AckDecodeError> {
        if data.is_empty() {
            return Err(AckDecodeError::TooShort(0));
        }
        if data[0] != ACK_BATCH_MSG_TYPE {
            return Err(AckDecodeError::WrongMsgType(data[0]));
        }
        if data.len() < ACK_HEADER_SIZE {
            return Err(AckDecodeError::TooShort(data.len()));
        }
        let count_fresh = data[1];
        let count_overlap = data[2];
        if count_fresh as usize > MAX_FRESH_ENTRIES_PER_BATCH {
            return Err(AckDecodeError::InvalidCount(count_fresh));
        }
        if count_overlap as usize > ACK_OVERLAP_COUNT {
            return Err(AckDecodeError::InvalidCount(count_overlap));
        }
        if count_fresh == 0 && count_overlap == 0 {
            return Err(AckDecodeError::InvalidCount(0));
        }
        let base_arrival_us = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);

        let need = ACK_HEADER_SIZE
            + (count_fresh as usize) * ACK_FRESH_ENTRY_SIZE
            + (count_overlap as usize) * ACK_OVERLAP_ENTRY_SIZE;
        if data.len() < need {
            return Err(AckDecodeError::TooShort(data.len()));
        }

        let mut entries = Vec::with_capacity(count_fresh as usize + count_overlap as usize);
        for i in 0..(count_fresh as usize) {
            let off = ACK_HEADER_SIZE + i * ACK_FRESH_ENTRY_SIZE;
            let frame_seq =
                u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
            let tile_x = data[off + 4];
            let tile_y = data[off + 5];
            let pass_idx = data[off + 6];
            let delta = u16::from_le_bytes([data[off + 7], data[off + 8]]);
            let arrival_us = base_arrival_us.wrapping_add(delta as u32) as u64;
            entries.push(AckEntry {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
                arrival_us,
            });
        }

        let overlap_base = ACK_HEADER_SIZE + (count_fresh as usize) * ACK_FRESH_ENTRY_SIZE;
        for i in 0..(count_overlap as usize) {
            let off = overlap_base + i * ACK_OVERLAP_ENTRY_SIZE;
            let frame_seq =
                u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
            let tile_x = data[off + 4];
            let tile_y = data[off + 5];
            let pass_idx = data[off + 6];
            let arrival_us =
                u32::from_le_bytes([data[off + 7], data[off + 8], data[off + 9], data[off + 10]])
                    as u64;
            entries.push(AckEntry {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
                arrival_us,
            });
        }

        Ok(AckBatch {
            entries,
            fresh_count: count_fresh as usize,
        })
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
                arrival_us: 0,
            }],
            fresh_count: 1,
        };
        let bytes = batch.encode();
        assert_eq!(bytes[0], ACK_BATCH_MSG_TYPE, "msg type = 0x05");
        assert_eq!(bytes[0], 0x05);
        assert_eq!(bytes[1], 1, "count_fresh = 1");
        assert_eq!(bytes[2], 0, "count_overlap = 0");
        assert_eq!(bytes[7..11], [0x78, 0x56, 0x34, 0x12], "frame_seq LE");
        assert_eq!(bytes[11], 3, "tile_x");
        assert_eq!(bytes[12], 7, "tile_y");
        assert_eq!(bytes[13], 13, "pass_idx");
        let decoded = AckBatch::decode(&bytes).expect("valid batch");
        assert_eq!(decoded.entries, batch.entries);
    }

    #[test]
    fn batch_at_max_capacity_fits_under_mtu() {
        let mut entries: Vec<_> = (0..MAX_FRESH_ENTRIES_PER_BATCH)
            .map(|i| AckEntry {
                frame_seq: i as u32,
                tile_x: 0,
                tile_y: 0,
                pass_idx: 0,
                arrival_us: i as u64,
            })
            .collect();
        entries.extend((0..ACK_OVERLAP_COUNT).map(|i| AckEntry {
            frame_seq: (1000 + i) as u32,
            tile_x: 0,
            tile_y: 0,
            pass_idx: 0,
            arrival_us: i as u64,
        }));
        let batch = AckBatch {
            entries,
            fresh_count: MAX_FRESH_ENTRIES_PER_BATCH,
        };
        let bytes = batch.encode();
        assert_eq!(
            bytes.len(),
            ACK_HEADER_SIZE
                + MAX_FRESH_ENTRIES_PER_BATCH * ACK_FRESH_ENTRY_SIZE
                + ACK_OVERLAP_COUNT * ACK_OVERLAP_ENTRY_SIZE
        );
        assert!(bytes.len() < 1200, "must fit under typical MTU");
        let decoded = AckBatch::decode(&bytes).expect("valid batch");
        assert_eq!(decoded.entries.len(), MAX_ACK_ENTRIES_PER_BATCH);
    }

    #[test]
    fn decode_rejects_old_msg_type_0x02() {
        let data = vec![0x02, 1, 0, 0, 0, 0, 0];
        let err = AckBatch::decode(&data).expect_err("must reject 0x02");
        assert!(matches!(err, AckDecodeError::WrongMsgType(0x02)));
    }

    #[test]
    fn decode_rejects_count_zero() {
        let data = vec![ACK_BATCH_MSG_TYPE, 0, 0, 0, 0, 0, 0];
        assert!(matches!(
            AckBatch::decode(&data),
            Err(AckDecodeError::InvalidCount(0))
        ));
    }

    #[test]
    fn decode_rejects_truncated_entry_payload() {
        // header says count_fresh=2 (needs 2*9=18 more bytes), only 9 supplied
        let mut data = vec![ACK_BATCH_MSG_TYPE, 2, 0];
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&[0u8; 9]);
        assert!(matches!(
            AckBatch::decode(&data),
            Err(AckDecodeError::TooShort(_))
        ));
    }

    fn entry(frame_seq: u32, arrival_us: u64) -> AckEntry {
        AckEntry {
            frame_seq,
            tile_x: 1,
            tile_y: 2,
            pass_idx: 3,
            arrival_us,
        }
    }

    #[test]
    fn single_fresh_entry_round_trips_with_zero_delta() {
        let b = AckBatch {
            entries: vec![entry(7, 1_234_567)],
            fresh_count: 1,
        };
        let bytes = b.encode();
        assert_eq!(bytes[0], ACK_BATCH_MSG_TYPE);
        assert_eq!(bytes[1], 1, "count_fresh");
        assert_eq!(bytes[2], 0, "count_overlap");
        assert_eq!(AckBatch::decode(&bytes).unwrap(), b);
    }

    #[test]
    fn entries_sharing_a_timestamp_encode_zero_deltas() {
        let b = AckBatch {
            entries: vec![entry(1, 9_000), entry(2, 9_000), entry(3, 9_000)],
            fresh_count: 3,
        };
        let bytes = b.encode();
        for i in 0..3 {
            let off = ACK_HEADER_SIZE + i * ACK_FRESH_ENTRY_SIZE;
            assert_eq!((bytes[off + 7], bytes[off + 8]), (0, 0), "entry {i}");
        }
        assert_eq!(AckBatch::decode(&bytes).unwrap(), b);
    }

    #[test]
    fn a_fresh_span_of_exactly_u16_max_still_encodes() {
        let b = AckBatch {
            entries: vec![entry(1, 1_000), entry(2, 1_000 + u16::MAX as u64)],
            fresh_count: 2,
        };
        assert_eq!(AckBatch::decode(&b.encode()).unwrap(), b);
    }

    #[test]
    fn a_fresh_span_beyond_u16_max_is_refused_rather_than_truncated() {
        // Cannot happen with FLUSH_INTERVAL_US = 5_000, so reaching this is a
        // caller bug. Silently truncating would feed the estimator a wrong
        // arrival time, which is the class of defect this format exists to end.
        let b = AckBatch {
            entries: vec![entry(1, 1_000), entry(2, 1_000 + u16::MAX as u64 + 1)],
            fresh_count: 2,
        };
        assert!(matches!(
            b.try_encode(),
            Err(AckEncodeError::FreshDeltaOverflow { index: 1, .. })
        ));
    }

    #[test]
    fn empty_batch_is_refused_because_decode_would_refuse_it_too() {
        // decode() correctly rejects count_fresh=0 && count_overlap=0 as
        // InvalidCount(0). If try_encode() were to emit that batch anyway,
        // encode() and decode() would disagree about what's valid -- caught
        // by the arbitrary_batches_round_trip_or_are_refused property below.
        let b = AckBatch {
            entries: vec![],
            fresh_count: 0,
        };
        assert_eq!(b.try_encode(), Err(AckEncodeError::Empty));
    }

    #[test]
    fn fresh_count_exceeding_entries_len_is_refused_rather_than_panicking() {
        // Fields are public with no constructor, so this is trivially
        // constructible by a caller bug. A `try_` function must not panic.
        let b = AckBatch {
            entries: vec![entry(1, 1_000)],
            fresh_count: 2,
        };
        assert_eq!(
            b.try_encode(),
            Err(AckEncodeError::FreshCountExceedsEntries {
                fresh_count: 2,
                entries: 1,
            })
        );
    }

    #[test]
    fn too_many_fresh_entries_is_refused_rather_than_truncating_the_count_byte() {
        // count_fresh is a u8 on the wire; 256 fresh entries would silently
        // encode as count_fresh=0, producing a structurally wrong batch.
        let fresh_count = MAX_FRESH_ENTRIES_PER_BATCH + 1;
        let entries: Vec<_> = (0..fresh_count)
            .map(|i| entry(i as u32, i as u64))
            .collect();
        let b = AckBatch {
            entries,
            fresh_count,
        };
        assert_eq!(
            b.try_encode(),
            Err(AckEncodeError::TooManyEntries {
                section: "fresh",
                count: fresh_count,
                cap: MAX_FRESH_ENTRIES_PER_BATCH,
            })
        );
    }

    #[test]
    fn too_many_overlap_entries_is_refused_rather_than_truncating_the_count_byte() {
        let overlap_count = ACK_OVERLAP_COUNT + 1;
        let mut entries = vec![entry(0, 0)];
        entries.extend((0..overlap_count).map(|i| entry(100 + i as u32, i as u64)));
        let b = AckBatch {
            entries,
            fresh_count: 1,
        };
        assert_eq!(
            b.try_encode(),
            Err(AckEncodeError::TooManyEntries {
                section: "overlap",
                count: overlap_count,
                cap: ACK_OVERLAP_COUNT,
            })
        );
    }

    #[test]
    fn an_overlap_entry_seconds_old_round_trips_exactly() {
        let b = AckBatch {
            entries: vec![entry(1, 5_000_000), entry(2, 1_000)],
            fresh_count: 1,
        };
        assert_eq!(AckBatch::decode(&b.encode()).unwrap(), b);
    }

    #[test]
    fn base_and_overlap_section_bytes_match_the_documented_offsets() {
        // A self-consistent encoder/decoder pair could put base_arrival_us
        // at the wrong offset, or scramble the overlap field order, and
        // still pass every round-trip-only test. Pin the wire bytes down.
        let fresh = AckEntry {
            frame_seq: 0x1111_2222,
            tile_x: 9,
            tile_y: 8,
            pass_idx: 7,
            arrival_us: 5_000_000,
        };
        let overlap = AckEntry {
            frame_seq: 0xAAAA_BBBB,
            tile_x: 44,
            tile_y: 55,
            pass_idx: 66,
            arrival_us: 0x0102_0304,
        };
        let b = AckBatch {
            entries: vec![fresh, overlap],
            fresh_count: 1,
        };
        let bytes = b.encode();

        // [3..7]: base_arrival_us = the (only) fresh entry's arrival_us.
        assert_eq!(
            &bytes[3..7],
            &5_000_000u32.to_le_bytes(),
            "base_arrival_us LE at [3..7]"
        );

        // Overlap section starts right after the header + 1 fresh entry.
        let off = ACK_HEADER_SIZE + ACK_FRESH_ENTRY_SIZE;
        assert_eq!(
            &bytes[off..off + 4],
            &0xAAAA_BBBBu32.to_le_bytes(),
            "overlap frame_seq LE"
        );
        assert_eq!(bytes[off + 4], 44, "overlap tile_x");
        assert_eq!(bytes[off + 5], 55, "overlap tile_y");
        assert_eq!(bytes[off + 6], 66, "overlap pass_idx");
        assert_eq!(
            &bytes[off + 7..off + 11],
            &0x0102_0304u32.to_le_bytes(),
            "overlap arrival_us LE (absolute, not a delta)"
        );

        assert_eq!(AckBatch::decode(&bytes).unwrap(), b);
    }

    #[test]
    fn per_entry_tile_fields_are_not_collapsed_to_the_first_entrys() {
        // The shared `entry()` test helper hardcodes tile_x/tile_y/pass_idx
        // to 1/2/3 for every call, so a bug that wrote entry[0]'s tile
        // fields into every slot would pass unnoticed by every other test
        // in this file. Vary them explicitly across fresh and overlap.
        fn varied(
            frame_seq: u32,
            tile_x: u8,
            tile_y: u8,
            pass_idx: u8,
            arrival_us: u64,
        ) -> AckEntry {
            AckEntry {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
                arrival_us,
            }
        }
        let b = AckBatch {
            entries: vec![
                varied(1, 10, 20, 1, 1_000),
                varied(2, 11, 21, 2, 1_500),
                varied(3, 12, 22, 3, 2_000_000),
            ],
            fresh_count: 2,
        };
        assert_eq!(AckBatch::decode(&b.encode()).unwrap(), b);
    }

    #[test]
    fn an_overlap_entry_ten_minutes_old_round_trips_exactly() {
        let ten_min_us = 600_000_000u64;
        let b = AckBatch {
            entries: vec![entry(1, ten_min_us + 1_000), entry(2, 1_000)],
            fresh_count: 1,
        };
        assert_eq!(AckBatch::decode(&b.encode()).unwrap(), b);
    }

    #[test]
    fn a_batch_straddling_the_u32_wrap_decodes_exactly() {
        let base = u32::MAX as u64 - 10;
        let b = AckBatch {
            entries: vec![entry(1, base), entry(2, base + 100)],
            fresh_count: 2,
        };
        let decoded = AckBatch::decode(&b.encode()).unwrap();
        assert_eq!(decoded.entries[0].arrival_us, base);
        // Wrapped into the low end of 32-bit space; the server's sequence-space
        // unwrapper is what restores monotonicity.
        assert_eq!(decoded.entries[1].arrival_us, 89);
    }

    #[test]
    fn counts_over_their_caps_are_rejected() {
        let mut bytes = vec![
            ACK_BATCH_MSG_TYPE,
            (MAX_FRESH_ENTRIES_PER_BATCH + 1) as u8,
            0,
        ];
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            AckBatch::decode(&bytes),
            Err(AckDecodeError::InvalidCount(_))
        ));

        let mut bytes = vec![ACK_BATCH_MSG_TYPE, 1, (ACK_OVERLAP_COUNT + 1) as u8];
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            AckBatch::decode(&bytes),
            Err(AckDecodeError::InvalidCount(_))
        ));
    }

    #[test]
    fn a_buffer_truncated_mid_entry_is_rejected() {
        let b = AckBatch {
            entries: vec![entry(1, 1_000), entry(2, 2_000)],
            fresh_count: 2,
        };
        let bytes = b.encode();
        for cut in 1..bytes.len() {
            assert!(
                AckBatch::decode(&bytes[..cut]).is_err(),
                "truncating to {cut} bytes must be rejected, not read past the end"
            );
        }
    }

    #[test]
    fn an_old_format_batch_is_rejected_loudly() {
        let stale = vec![0x04u8, 1, 0, 0, 0, 0];
        assert_eq!(
            AckBatch::decode(&stale),
            Err(AckDecodeError::WrongMsgType(0x04))
        );
    }

    #[test]
    fn worst_case_batch_fits_the_documented_size() {
        let mut entries: Vec<AckEntry> = (0..MAX_FRESH_ENTRIES_PER_BATCH)
            .map(|i| entry(i as u32, 1_000 + i as u64))
            .collect();
        entries.extend((0..ACK_OVERLAP_COUNT).map(|i| entry(900 + i as u32, 10 + i as u64)));
        let b = AckBatch {
            entries,
            fresh_count: MAX_FRESH_ENTRIES_PER_BATCH,
        };
        assert_eq!(b.encode().len(), 671);
        assert_eq!(AckBatch::decode(&b.encode()).unwrap(), b);
    }

    use proptest::prelude::*;

    fn any_entry() -> impl Strategy<Value = AckEntry> {
        (
            any::<u32>(),
            any::<u8>(),
            any::<u8>(),
            any::<u8>(),
            0u64..=(u32::MAX as u64),
        )
            .prop_map(
                |(frame_seq, tile_x, tile_y, pass_idx, arrival_us)| AckEntry {
                    frame_seq,
                    tile_x,
                    tile_y,
                    pass_idx,
                    arrival_us,
                },
            )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]
        #[test]
        fn arbitrary_batches_round_trip_or_are_refused(
            entries in prop::collection::vec(any_entry(), 0..100),
            fresh_count_raw in 0usize..200,
        ) {
            // Keep fresh_count within [0, entries.len()] so most cases
            // exercise the round-trip path rather than only the
            // FreshCountExceedsEntries guard; section-cap and delta-budget
            // refusals are still reachable from the arbitrary lengths and
            // arbitrary timestamps above.
            let fresh_count = fresh_count_raw % (entries.len() + 1);
            let batch = AckBatch { entries, fresh_count };
            match batch.try_encode() {
                // Refusal is an acceptable outcome for out-of-range input;
                // the property is "round-trip exactly, or refuse" -- never
                // silently mutate.
                Err(_) => {}
                Ok(bytes) => {
                    let decoded = AckBatch::decode(&bytes)
                        .expect("anything try_encode emits must be decodable");
                    prop_assert_eq!(decoded, batch);
                }
            }
        }
    }
}
