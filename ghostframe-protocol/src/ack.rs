//! Batched-ACK datagram protocol — per-tile-pass acknowledgment (M3.3d rev2).
//!
//! Wire format:
//! ```text
//! [0]      message_type = 0x06
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
//!
//! Message-type lineage: `0x02 -> 0x03 -> 0x04 -> 0x06`. `0x05` is
//! deliberately skipped -- it was briefly assigned here in 2026-09-14 and
//! collided with `TILE_NACK_ENVELOPE = 0x05` (both are client->server
//! datagrams dispatched by the same `classify_inbound`), which would have
//! silently routed every ACK batch into the NACK handler. Do not reuse
//! `0x05` for this format.

/// ACK envelope wire-format version. Bumped 0x04 → 0x06 in 2026-09 to move
/// from millisecond to microsecond arrival timestamps: goog_cc was
/// discarding the majority of probe measurements because sub-millisecond
/// inter-packet arrival spacing was unrepresentable in the old format. Old
/// (0x04) clients/servers are not wire-compatible with new — both sides
/// ship in lockstep.
///
/// `0x05` was skipped: it was assigned first and collided with
/// `TILE_NACK_ENVELOPE` (`protocol.rs`), silently routing every ACK batch
/// into the NACK handler. See `protocol::tests::
/// inbound_message_discriminators_do_not_collide` for the regression test.
pub const ACK_BATCH_MSG_TYPE: u8 = 0x06;
/// Maximum number of *fresh* entries the client packs into one batch
/// before flushing (mirrors `MAX_ACK_ENTRIES` in ack.ts). This is also the
/// wire-acceptance cap for `count_fresh`, enforced independently by both
/// `decode` and `AckBatch::new`.
pub const MAX_FRESH_ENTRIES_PER_BATCH: usize = 64;
/// Trailing overlap count the client appends to each batch (mirrors
/// `ACK_OVERLAP_COUNT` in ack.ts). A single dropped ACK batch
/// therefore needs ACK_OVERLAP_COUNT + 1 consecutive drops to lose
/// any entry. This is also the wire-acceptance cap for `count_overlap`,
/// enforced independently by both `decode` and `AckBatch::new`.
pub const ACK_OVERLAP_COUNT: usize = 8;
/// Combined size of a full-fresh, full-overlap batch. Informational only:
/// nothing checks this sum directly, because it holds automatically from
/// `count_fresh` and `count_overlap` each being capped independently (by
/// `MAX_FRESH_ENTRIES_PER_BATCH` and `ACK_OVERLAP_COUNT` respectively).
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
    #[error("wrong message type: expected 0x06, got 0x{0:02x}")]
    WrongMsgType(u8),
    #[error("invalid {section} count: {count} exceeds the cap of {cap}")]
    InvalidCount {
        section: &'static str,
        count: u8,
        cap: usize,
    },
    #[error("ack batch has no fresh and no overlap entries")]
    Empty,
    #[error("ack batch wrong length: got {got} bytes, need exactly {want}")]
    WrongLength { got: usize, want: usize },
    /// The wire bytes parsed structurally (right message type, counts
    /// within cap, right length), but the resulting batch fails the same
    /// invariants `AckBatch::new` enforces -- e.g. a wire-legal batch whose
    /// `base_arrival_us` disagrees with `fresh[0]`'s own reconstructed
    /// delta, since the wire carries them as two separate fields that a
    /// well-behaved encoder keeps in sync (delta_0 == 0) but a decoder
    /// cannot assume of arbitrary bytes. Rejecting this here means "if an
    /// `AckBatch` exists, it encodes" holds for both constructors, not just
    /// `new`.
    #[error("decoded batch fails its own encoding invariants: {0}")]
    Unencodable(AckEncodeError),
}

/// Encoding refuses rather than silently mangles a batch that violates any
/// of `AckBatch`'s invariants:
/// - at least one fresh or overlap entry (`Empty` otherwise),
/// - fresh and overlap section lengths within their wire caps,
///   `MAX_FRESH_ENTRIES_PER_BATCH` and `ACK_OVERLAP_COUNT` (`TooManyEntries`
///   otherwise),
/// - fresh entries non-decreasing, mod 2^32, from `entries[0]`
///   (`FreshEntryOutOfOrder` otherwise), and
/// - each fresh entry within 65,535us of `entries[0]` -- comfortably more
///   than the client's flush interval, so real fresh spans never approach
///   it and reaching this is a caller bug (`FreshDeltaOverflow` otherwise).
///
/// `AckBatch::new` checks all four up front, so a batch that exists always
/// encodes; `decode` upholds them structurally too. Silently clamping or
/// truncating any of these would feed the estimator a wrong arrival time,
/// or reinterpret the fresh/overlap boundary, which is the class of defect
/// this format exists to end.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AckEncodeError {
    #[error("fresh entry {index} is {delta_us}us from the batch base, over the u16 limit")]
    FreshDeltaOverflow { index: usize, delta_us: u32 },
    /// Fresh entries must arrive in non-decreasing order (mod 2^32) from
    /// `entries[0]`, because the per-batch base is `entries[0]`'s own
    /// timestamp and every other fresh entry's delta is computed forward
    /// from it. A backwards entry produces a `delta_us` near `u32::MAX`
    /// (a huge apparent forward span) rather than the small backwards step
    /// it actually is; reporting the true `behind_us` here instead of
    /// routing it through `FreshDeltaOverflow` avoids describing a 1us
    /// reordering as if it were a 71-minute clock jump.
    #[error(
        "fresh entry {index} is {behind_us}us behind the batch base -- fresh entries must be \
         non-decreasing (mod 2^32) from entries[0]"
    )]
    FreshEntryOutOfOrder { index: usize, behind_us: u32 },
    #[error("{count} {section} entries exceeds the cap of {cap}")]
    TooManyEntries {
        section: &'static str,
        count: usize,
        cap: usize,
    },
    /// A batch with zero fresh and zero overlap entries would encode as
    /// `count_fresh=0, count_overlap=0`, which `decode` correctly refuses
    /// as carrying nothing to acknowledge. Refusing it here too keeps
    /// encode/decode symmetric: anything `try_encode` emits, `decode` must
    /// accept.
    #[error("batch has no fresh and no overlap entries")]
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckEntry {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
    /// Client's wall-clock receive time for this pass, wrapped to
    /// microseconds mod 2^32 (~71.6 minutes). `u32`, not `u64`: only the
    /// low 32 bits are ever meaningful on the wire, and a `u64` field here
    /// previously let a caller pass an un-wrapped wall-clock timestamp
    /// that silently truncated to the wrong value on encode (a real
    /// `1757000000000000`-style timestamp decoded back as `1893650432`,
    /// with no error). Making the type match the wire's actual width
    /// makes that truncation unrepresentable instead of just undocumented.
    /// Absolute clock skew doesn't matter because the BWE consumer looks
    /// at *relative* arrival differences between packets in the same
    /// batch.
    pub arrival_us: u32,
}

/// One ACK batch, split into a *fresh* section (newly-received tile-passes,
/// encoded as small deltas from a per-batch base) and an *overlap* section
/// (replayed from previous batches, encoded as absolute timestamps since
/// they may be arbitrarily old). Build one via `new`, which validates every
/// invariant up front -- including that fresh entries are non-decreasing,
/// mod 2^32, from the first fresh entry, since that entry's timestamp
/// becomes the batch's base; `decode` builds directly from wire bytes,
/// which already satisfy these structurally. Iterate uniformly over both
/// sections via `entries()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckBatch {
    entries: Vec<AckEntry>,
    fresh_count: usize,
}

impl AckBatch {
    /// Build a batch from its fresh and overlap sections, validating all of
    /// `AckBatch`'s invariants (see `AckEncodeError`) before this value can
    /// exist. `fresh` must be non-decreasing, mod 2^32, from `fresh[0]`.
    pub fn new(fresh: Vec<AckEntry>, overlap: Vec<AckEntry>) -> Result<Self, AckEncodeError> {
        let fresh_count = fresh.len();
        let mut entries = fresh;
        entries.extend(overlap);
        let batch = AckBatch {
            entries,
            fresh_count,
        };
        batch.validate()?;
        Ok(batch)
    }

    /// The freshly-received tile-passes in this batch.
    pub fn fresh(&self) -> &[AckEntry] {
        &self.entries[..self.fresh_count]
    }

    /// The entries replayed from previous batches.
    pub fn overlap(&self) -> &[AckEntry] {
        &self.entries[self.fresh_count..]
    }

    /// All entries, fresh followed by overlap, for uniform iteration.
    pub fn entries(&self) -> &[AckEntry] {
        &self.entries
    }

    fn validate(&self) -> Result<(), AckEncodeError> {
        let fresh = self.fresh();
        let overlap = self.overlap();
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
        if let Some(first) = fresh.first() {
            for (i, e) in fresh.iter().enumerate() {
                let delta_us = e.arrival_us.wrapping_sub(first.arrival_us);
                if delta_us > u32::MAX / 2 {
                    // e.arrival_us is *behind* the base mod 2^32: wrapping
                    // subtraction makes a small backwards step look like a
                    // huge forward one, so report the true magnitude.
                    return Err(AckEncodeError::FreshEntryOutOfOrder {
                        index: i,
                        behind_us: delta_us.wrapping_neg(),
                    });
                }
                if delta_us > u16::MAX as u32 {
                    return Err(AckEncodeError::FreshDeltaOverflow { index: i, delta_us });
                }
            }
        }
        Ok(())
    }

    /// Encode this batch.
    ///
    /// # Panics
    ///
    /// Panics if the batch violates one of the invariants `AckBatch::new`
    /// checks: see `AckEncodeError`. This cannot happen for a batch built
    /// via `new` or `decode`; use `try_encode` if the batch's provenance is
    /// something else.
    pub fn encode(&self) -> Vec<u8> {
        self.try_encode()
            .expect("AckBatch must satisfy AckBatch::new's invariants -- see AckEncodeError")
    }

    pub fn try_encode(&self) -> Result<Vec<u8>, AckEncodeError> {
        self.validate()?;
        let fresh = self.fresh();
        let overlap = self.overlap();
        let base_arrival_us: u32 = fresh.first().map(|e| e.arrival_us).unwrap_or(0);

        let mut out = Vec::with_capacity(
            ACK_HEADER_SIZE
                + fresh.len() * ACK_FRESH_ENTRY_SIZE
                + overlap.len() * ACK_OVERLAP_ENTRY_SIZE,
        );
        out.push(ACK_BATCH_MSG_TYPE);
        out.push(fresh.len() as u8); // safe: validate() proved <= MAX_FRESH_ENTRIES_PER_BATCH
        out.push(overlap.len() as u8); // safe: validate() proved <= ACK_OVERLAP_COUNT
        out.extend_from_slice(&base_arrival_us.to_le_bytes());

        for e in fresh {
            // safe: validate() proved this delta fits in u16.
            let delta = e.arrival_us.wrapping_sub(base_arrival_us) as u16;
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
            out.extend_from_slice(&e.arrival_us.to_le_bytes());
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
            return Err(AckDecodeError::InvalidCount {
                section: "fresh",
                count: count_fresh,
                cap: MAX_FRESH_ENTRIES_PER_BATCH,
            });
        }
        if count_overlap as usize > ACK_OVERLAP_COUNT {
            return Err(AckDecodeError::InvalidCount {
                section: "overlap",
                count: count_overlap,
                cap: ACK_OVERLAP_COUNT,
            });
        }
        if count_fresh == 0 && count_overlap == 0 {
            return Err(AckDecodeError::Empty);
        }
        let base_arrival_us = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);

        let need = ACK_HEADER_SIZE
            + (count_fresh as usize) * ACK_FRESH_ENTRY_SIZE
            + (count_overlap as usize) * ACK_OVERLAP_ENTRY_SIZE;
        if data.len() != need {
            return Err(AckDecodeError::WrongLength {
                got: data.len(),
                want: need,
            });
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
            let arrival_us = base_arrival_us.wrapping_add(delta as u32);
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
                u32::from_le_bytes([data[off + 7], data[off + 8], data[off + 9], data[off + 10]]);
            entries.push(AckEntry {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
                arrival_us,
            });
        }

        let batch = AckBatch {
            entries,
            fresh_count: count_fresh as usize,
        };
        // The wire parsed structurally, but base_arrival_us and fresh[0]'s
        // delta are two independent fields on the wire -- a well-behaved
        // encoder keeps delta_0 == 0, but arbitrary bytes need not. Without
        // this, a hand-crafted (or third-party) datagram could decode into
        // an AckBatch that panics if ever re-encoded, which would make the
        // "if it exists, it encodes" guarantee true only for `new`.
        batch.validate().map_err(AckDecodeError::Unencodable)?;
        Ok(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(frame_seq: u32, arrival_us: u32) -> AckEntry {
        AckEntry {
            frame_seq,
            tile_x: 1,
            tile_y: 2,
            pass_idx: 3,
            arrival_us,
        }
    }

    // --- construction and basic round-trip ---------------------------

    #[test]
    fn batch_roundtrip_single_entry() {
        let batch = AckBatch::new(
            vec![AckEntry {
                frame_seq: 0x1234_5678,
                tile_x: 3,
                tile_y: 7,
                pass_idx: 13,
                arrival_us: 0,
            }],
            vec![],
        )
        .unwrap();
        let bytes = batch.encode();
        assert_eq!(bytes[0], ACK_BATCH_MSG_TYPE, "msg type = 0x06");
        assert_eq!(bytes[0], 0x06);
        assert_eq!(bytes[1], 1, "count_fresh = 1");
        assert_eq!(bytes[2], 0, "count_overlap = 0");
        assert_eq!(bytes[7..11], [0x78, 0x56, 0x34, 0x12], "frame_seq LE");
        assert_eq!(bytes[11], 3, "tile_x");
        assert_eq!(bytes[12], 7, "tile_y");
        assert_eq!(bytes[13], 13, "pass_idx");
        let decoded = AckBatch::decode(&bytes).expect("valid batch");
        assert_eq!(decoded.entries(), batch.entries());
    }

    #[test]
    fn new_rejects_a_fresh_count_over_the_cap_without_touching_overlap() {
        let fresh: Vec<_> = (0..MAX_FRESH_ENTRIES_PER_BATCH + 1)
            .map(|i| entry(i as u32, i as u32))
            .collect();
        assert_eq!(
            AckBatch::new(fresh, vec![]),
            Err(AckEncodeError::TooManyEntries {
                section: "fresh",
                count: MAX_FRESH_ENTRIES_PER_BATCH + 1,
                cap: MAX_FRESH_ENTRIES_PER_BATCH,
            })
        );
    }

    #[test]
    fn new_rejects_an_overlap_count_over_the_cap() {
        let overlap: Vec<_> = (0..ACK_OVERLAP_COUNT + 1)
            .map(|i| entry(i as u32, i as u32))
            .collect();
        assert_eq!(
            AckBatch::new(vec![entry(0, 0)], overlap),
            Err(AckEncodeError::TooManyEntries {
                section: "overlap",
                count: ACK_OVERLAP_COUNT + 1,
                cap: ACK_OVERLAP_COUNT,
            })
        );
    }

    #[test]
    fn accessors_split_fresh_and_overlap_and_entries_is_the_concatenation() {
        let fresh = vec![entry(1, 1_000), entry(2, 1_500)];
        let overlap = vec![entry(3, 500_000)];
        let batch = AckBatch::new(fresh.clone(), overlap.clone()).unwrap();
        assert_eq!(batch.fresh(), fresh.as_slice());
        assert_eq!(batch.overlap(), overlap.as_slice());
        assert_eq!(batch.entries(), [fresh, overlap].concat().as_slice());
    }

    #[test]
    fn batch_at_max_capacity_fits_under_mtu() {
        let fresh: Vec<_> = (0..MAX_FRESH_ENTRIES_PER_BATCH)
            .map(|i| entry(i as u32, i as u32))
            .collect();
        let overlap: Vec<_> = (0..ACK_OVERLAP_COUNT)
            .map(|i| entry((1000 + i) as u32, i as u32))
            .collect();
        let batch = AckBatch::new(fresh, overlap).unwrap();
        let bytes = batch.encode();
        assert_eq!(
            bytes.len(),
            ACK_HEADER_SIZE
                + MAX_FRESH_ENTRIES_PER_BATCH * ACK_FRESH_ENTRY_SIZE
                + ACK_OVERLAP_COUNT * ACK_OVERLAP_ENTRY_SIZE
        );
        assert!(bytes.len() < 1200, "must fit under typical MTU");
        let decoded = AckBatch::decode(&bytes).expect("valid batch");
        assert_eq!(decoded.entries().len(), MAX_ACK_ENTRIES_PER_BATCH);
    }

    #[test]
    fn worst_case_batch_fits_the_documented_size() {
        let fresh: Vec<AckEntry> = (0..MAX_FRESH_ENTRIES_PER_BATCH)
            .map(|i| entry(i as u32, 1_000 + i as u32))
            .collect();
        let overlap: Vec<AckEntry> = (0..ACK_OVERLAP_COUNT)
            .map(|i| entry(900 + i as u32, 10 + i as u32))
            .collect();
        let b = AckBatch::new(fresh, overlap).unwrap();
        assert_eq!(b.encode().len(), 671);
        assert_eq!(AckBatch::decode(&b.encode()).unwrap(), b);
    }

    // --- fresh section: delta encoding, endianness, ordering ---------

    #[test]
    fn single_fresh_entry_round_trips_with_zero_delta() {
        let b = AckBatch::new(vec![entry(7, 1_234_567)], vec![]).unwrap();
        let bytes = b.encode();
        assert_eq!(bytes[0], ACK_BATCH_MSG_TYPE);
        assert_eq!(bytes[1], 1, "count_fresh");
        assert_eq!(bytes[2], 0, "count_overlap");
        assert_eq!(AckBatch::decode(&bytes).unwrap(), b);
    }

    #[test]
    fn entries_sharing_a_timestamp_encode_zero_deltas() {
        let b = AckBatch::new(
            vec![entry(1, 9_000), entry(2, 9_000), entry(3, 9_000)],
            vec![],
        )
        .unwrap();
        let bytes = b.encode();
        for i in 0..3 {
            let off = ACK_HEADER_SIZE + i * ACK_FRESH_ENTRY_SIZE;
            assert_eq!((bytes[off + 7], bytes[off + 8]), (0, 0), "entry {i}");
        }
        assert_eq!(AckBatch::decode(&bytes).unwrap(), b);
    }

    #[test]
    fn fresh_delta_bytes_are_little_endian() {
        // An asymmetric delta (0x0102 = 258, whose LE and BE byte order
        // differ) so a swapped byte order fails this test instead of
        // passing it: the zero-delta test above is endian-invariant by
        // construction and cannot catch this.
        let b = AckBatch::new(vec![entry(1, 0), entry(2, 0x0102)], vec![]).unwrap();
        let bytes = b.encode();
        let off = ACK_HEADER_SIZE + ACK_FRESH_ENTRY_SIZE;
        assert_eq!(
            &bytes[off + 7..off + 9],
            &0x0102u16.to_le_bytes(),
            "fresh delta is little-endian"
        );
        assert_eq!(AckBatch::decode(&bytes).unwrap(), b);
    }

    #[test]
    fn a_fresh_span_of_exactly_u16_max_still_encodes() {
        let b = AckBatch::new(
            vec![entry(1, 1_000), entry(2, 1_000 + u16::MAX as u32)],
            vec![],
        )
        .unwrap();
        assert_eq!(AckBatch::decode(&b.encode()).unwrap(), b);
    }

    #[test]
    fn a_fresh_span_beyond_u16_max_is_refused_rather_than_truncated() {
        // Comfortably beyond any real flush interval, so reaching this is a
        // caller bug. Silently truncating would feed the estimator a wrong
        // arrival time, which is the class of defect this format exists to end.
        let fresh = vec![entry(1, 1_000), entry(2, 1_000 + u16::MAX as u32 + 1)];
        assert!(matches!(
            AckBatch::new(fresh, vec![]),
            Err(AckEncodeError::FreshDeltaOverflow { index: 1, .. })
        ));
    }

    #[test]
    fn an_out_of_order_fresh_entry_gets_its_own_honest_error() {
        // entries[1] arrives 1us *before* entries[0]. wrapping_sub makes
        // that look like a ~71-minute forward span (u32::MAX), which is
        // exactly the wrong story to tell a reader -- there was no clock
        // jump, just two entries out of order. FreshEntryOutOfOrder must
        // fire instead of FreshDeltaOverflow, reporting the true 1us.
        let fresh = vec![entry(1, 1_000), entry(2, 999)];
        assert_eq!(
            AckBatch::new(fresh, vec![]),
            Err(AckEncodeError::FreshEntryOutOfOrder {
                index: 1,
                behind_us: 1,
            })
        );
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
            arrival_us: u32,
        ) -> AckEntry {
            AckEntry {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
                arrival_us,
            }
        }
        let b = AckBatch::new(
            vec![varied(1, 10, 20, 1, 1_000), varied(2, 11, 21, 2, 1_500)],
            vec![varied(3, 12, 22, 3, 2_000_000)],
        )
        .unwrap();
        assert_eq!(AckBatch::decode(&b.encode()).unwrap(), b);
    }

    // --- overlap section: absolute timestamps, arbitrary age ---------

    #[test]
    fn overlap_only_batch_encodes_and_round_trips() {
        // decode() supports count_fresh == 0 && count_overlap > 0; nothing
        // upstream of this test asserted try_encode() actually produces
        // that shape rather than refusing every overlap-only batch.
        let overlap = vec![entry(1, 1_000), entry(2, 2_000), entry(3, 3_000)];
        let b =
            AckBatch::new(vec![], overlap).expect("fresh_count=0 with overlap-only must succeed");
        let bytes = b.encode();
        assert_eq!(bytes[1], 0, "count_fresh");
        assert_eq!(bytes[2], 3, "count_overlap");
        assert_eq!(AckBatch::decode(&bytes).unwrap(), b);
    }

    #[test]
    fn an_overlap_entry_seconds_old_round_trips_exactly() {
        let b = AckBatch::new(vec![entry(1, 5_000_000)], vec![entry(2, 1_000)]).unwrap();
        assert_eq!(AckBatch::decode(&b.encode()).unwrap(), b);
    }

    #[test]
    fn an_overlap_entry_ten_minutes_old_round_trips_exactly() {
        let ten_min_us: u32 = 600_000_000;
        let b = AckBatch::new(vec![entry(1, ten_min_us + 1_000)], vec![entry(2, 1_000)]).unwrap();
        assert_eq!(AckBatch::decode(&b.encode()).unwrap(), b);
    }

    #[test]
    fn a_batch_straddling_the_u32_wrap_preserves_the_delta() {
        // arrival_us is already wrapped 32-bit space (that's the whole
        // point of the field being u32, not u64), so a caller whose wall
        // clock crosses the wrap must pass the already-wrapped value.
        // There is no consumer yet that unwraps this into a monotonic
        // sequence -- whoever eventually reads `arrival_us` must treat it
        // as a wrapping 32-bit counter, the same way TCP sequence numbers
        // are compared. This test only proves the *delta* survives the
        // wrap; it does not, and cannot, prove anything about ordering
        // across it.
        let base: u32 = u32::MAX - 10;
        let b = AckBatch::new(
            vec![entry(1, base), entry(2, base.wrapping_add(100))],
            vec![],
        )
        .unwrap();
        let decoded = AckBatch::decode(&b.encode()).unwrap();
        assert_eq!(decoded.fresh()[0].arrival_us, base);
        assert_eq!(
            decoded.fresh()[1].arrival_us,
            89,
            "89 = (u32::MAX - 10 + 100) mod 2^32"
        );
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
        let b = AckBatch::new(vec![fresh], vec![overlap]).unwrap();
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

    // --- emptiness ------------------------------------------------------

    #[test]
    fn empty_batch_is_refused_because_decode_would_refuse_it_too() {
        // decode() correctly rejects count_fresh=0 && count_overlap=0 as
        // AckDecodeError::Empty. If try_encode() were to emit that batch
        // anyway, encode() and decode() would disagree about what's valid.
        assert_eq!(AckBatch::new(vec![], vec![]), Err(AckEncodeError::Empty));
    }

    // --- decode-side rejection ------------------------------------------

    #[test]
    fn decode_rejects_old_msg_type_0x02() {
        let data = vec![0x02, 1, 0, 0, 0, 0, 0];
        let err = AckBatch::decode(&data).expect_err("must reject 0x02");
        assert!(matches!(err, AckDecodeError::WrongMsgType(0x02)));
    }

    #[test]
    fn decode_rejects_count_zero() {
        let data = vec![ACK_BATCH_MSG_TYPE, 0, 0, 0, 0, 0, 0];
        assert_eq!(AckBatch::decode(&data), Err(AckDecodeError::Empty));
    }

    #[test]
    fn decode_rejects_truncated_entry_payload() {
        // header says count_fresh=2 (needs 2*9=18 more bytes), only 9 supplied
        let mut data = vec![ACK_BATCH_MSG_TYPE, 2, 0];
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&[0u8; 9]);
        assert_eq!(
            AckBatch::decode(&data),
            Err(AckDecodeError::WrongLength { got: 16, want: 25 })
        );
    }

    #[test]
    fn trailing_garbage_bytes_are_rejected() {
        // Previously `data.len() < need`, which silently accepted trailing
        // bytes beyond a structurally complete batch.
        let b = AckBatch::new(vec![entry(1, 1_000)], vec![]).unwrap();
        let mut bytes = b.encode();
        bytes.push(0xFF);
        assert!(matches!(
            AckBatch::decode(&bytes),
            Err(AckDecodeError::WrongLength { .. })
        ));
    }

    #[test]
    fn decode_rejects_a_wire_legal_batch_whose_reconstructed_fresh_is_out_of_order() {
        // Hand-craft a datagram that parses structurally (right msg type,
        // counts in range, right length) but whose fresh section is not
        // something try_encode() could ever have produced: base_arrival_us
        // and fresh[0]'s own delta are two independent wire fields, and a
        // well-behaved encoder always writes delta_0 == 0 -- but arbitrary
        // bytes need not. With base=1_000_000 and deltas [5000, 0], this
        // reconstructs to fresh = [1_005_000, 1_000_000], which is out of
        // order. Before this test, decode() built that AckBatch anyway; it
        // would panic the moment anything tried to re-encode it.
        let mut data = vec![ACK_BATCH_MSG_TYPE, 2, 0];
        data.extend_from_slice(&1_000_000u32.to_le_bytes());
        // fresh[0]: frame_seq=1, tile 1/2/3, delta=5000
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&[1, 2, 3]);
        data.extend_from_slice(&5000u16.to_le_bytes());
        // fresh[1]: frame_seq=2, tile 1/2/3, delta=0
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(&[1, 2, 3]);
        data.extend_from_slice(&0u16.to_le_bytes());
        assert_eq!(data.len(), ACK_HEADER_SIZE + 2 * ACK_FRESH_ENTRY_SIZE);

        assert_eq!(
            AckBatch::decode(&data),
            Err(AckDecodeError::Unencodable(
                AckEncodeError::FreshEntryOutOfOrder {
                    index: 1,
                    behind_us: 5000,
                }
            ))
        );
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
            Err(AckDecodeError::InvalidCount {
                section: "fresh",
                ..
            })
        ));

        let mut bytes = vec![ACK_BATCH_MSG_TYPE, 1, (ACK_OVERLAP_COUNT + 1) as u8];
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            AckBatch::decode(&bytes),
            Err(AckDecodeError::InvalidCount {
                section: "overlap",
                ..
            })
        ));
    }

    #[test]
    fn a_buffer_truncated_mid_entry_is_rejected() {
        let b = AckBatch::new(vec![entry(1, 1_000), entry(2, 2_000)], vec![]).unwrap();
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

    // --- property: round-trip exactly, or refuse -------------------------

    use proptest::prelude::*;

    fn any_entry() -> impl Strategy<Value = AckEntry> {
        (
            any::<u32>(),
            any::<u8>(),
            any::<u8>(),
            any::<u8>(),
            any::<u32>(),
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
            fresh in prop::collection::vec(any_entry(), 0..70),
            overlap in prop::collection::vec(any_entry(), 0..12),
        ) {
            // Compute the documented preconditions independently of the
            // implementation, so the property has a positive direction
            // too: an encoder that refuses *everything* satisfies
            // "round-trip or refuse" just as well as a correct one, unless
            // something here also asserts success when success is due.
            let within_caps =
                fresh.len() <= MAX_FRESH_ENTRIES_PER_BATCH && overlap.len() <= ACK_OVERLAP_COUNT;
            let non_empty = !(fresh.is_empty() && overlap.is_empty());
            let ordering_ok = match fresh.first() {
                None => true,
                Some(first) => fresh.iter().all(|e| {
                    e.arrival_us.wrapping_sub(first.arrival_us) <= u16::MAX as u32
                }),
            };
            let should_succeed = within_caps && non_empty && ordering_ok;

            match AckBatch::new(fresh, overlap) {
                Err(_) => {
                    prop_assert!(
                        !should_succeed,
                        "new() refused a batch meeting every documented precondition"
                    );
                }
                Ok(batch) => {
                    prop_assert!(
                        should_succeed,
                        "new() accepted a batch violating a documented precondition"
                    );
                    let bytes = batch.encode();
                    let decoded = AckBatch::decode(&bytes)
                        .expect("anything encode emits must be decodable");
                    prop_assert_eq!(decoded, batch);
                }
            }
        }
    }
}
