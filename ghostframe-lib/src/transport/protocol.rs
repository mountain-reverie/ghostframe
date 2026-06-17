//! Wire format for M1 tile datagrams.
//!
//! Each QUIC datagram carries one fragment of one tile:
//! ```text
//! [DatagramHeader: 16 bytes][TileHeader: 8 bytes][payload: variable]
//! ```
//! The receiver reassembles fragments keyed on `(frame_seq, tile_x, tile_y)`.
//!
//! M0 ping/pong constants are retained for backward compatibility.

use thiserror::Error;

// ---------------------------------------------------------------------------
// M0 constants (kept for io_bridge.rs compatibility)
// ---------------------------------------------------------------------------

pub const PING_PAYLOAD: &[u8; 4] = b"ping";
pub const PONG_PAYLOAD: &[u8; 4] = b"pong";

// ---------------------------------------------------------------------------
// Header size constants
// ---------------------------------------------------------------------------

pub const DATAGRAM_HEADER_SIZE: usize = 16;
pub const TILE_HEADER_SIZE: usize = 8;

/// Sentinel `wire_seq` value meaning "not yet stamped by the emitter".
/// Used at construction sites (`fragment_tile`, `build_parity_datagrams`)
/// to make the convention named and greppable. The `ReliableTileEmitter`
/// overwrites this with a real per-session monotonic value at submit time.
pub const UNSTAMPED_WIRE_SEQ: u32 = 0;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error, PartialEq)]
pub enum ProtocolError {
    #[error("input too short: expected {expected} bytes, got {got}")]
    TooShort { expected: usize, got: usize },

    #[error("unknown codec byte: {0}")]
    UnknownCodec(u8),

    #[error("unknown envelope discriminator byte: {0:#04x}")]
    UnknownEnvelope(u8),

    #[error("invalid length field: count would exceed the protocol maximum")]
    InvalidLength,
}

// ---------------------------------------------------------------------------
// Codec
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    Skip = 0,
    H264 = 1,
    PalRle = 2,
    Solid = 3,
    Raw = 4,
    Cdf53 = 5,
}

impl Codec {
    pub fn from_u8(v: u8) -> Result<Self, ProtocolError> {
        match v {
            0 => Ok(Codec::Skip),
            1 => Ok(Codec::H264),
            2 => Ok(Codec::PalRle),
            3 => Ok(Codec::Solid),
            4 => Ok(Codec::Raw),
            5 => Ok(Codec::Cdf53),
            other => Err(ProtocolError::UnknownCodec(other)),
        }
    }
}

// ---------------------------------------------------------------------------
// DatagramHeader (16 bytes)
//   [0..4]   frame_seq:     u32 BE
//   [4..6]   frag_idx:      u16 BE
//   [6..8]   frag_total:    u16 BE
//   [8..12]  wire_seq:      u32 BE
//   [12..16] timestamp_us:  u32 BE
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatagramHeader {
    pub frame_seq: u32,
    pub frag_idx: u16,
    pub frag_total: u16,
    /// FEC group key, used by the client to deduplicate retransmits and
    /// correlate parity datagrams to their source group.
    ///
    /// Scope: per QUIC session lifetime — restarts at 0 (or any value the
    /// emitter chooses to begin with) on every fresh session. Within a
    /// session it is monotonic: each *new* tile-pass emission gets the
    /// next value; retransmits of the same fragment reuse the original
    /// `wire_seq` so the client can drop duplicates.
    ///
    /// Convention: `0` means "not yet stamped". All call sites that build
    /// a header before the emitter sees it (`fragment_tile`,
    /// `build_parity_datagrams`) initialize `wire_seq = 0`; the
    /// `ReliableTileEmitter` overwrites the field at submit time before
    /// the datagram hits the wire. A `0` observed on the wire therefore
    /// indicates either a bug in the emitter or a synthetic test datagram.
    ///
    /// Wrap behavior: `u32` wrap is benign because the client dedupe
    /// window is much shorter than 2^32 emissions — by the time
    /// `wire_seq` wraps, the previous occupant of any given value has
    /// long left the in-flight set.
    pub wire_seq: u32,
    pub timestamp_us: u32,
}

impl DatagramHeader {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.frame_seq.to_be_bytes());
        buf.extend_from_slice(&self.frag_idx.to_be_bytes());
        buf.extend_from_slice(&self.frag_total.to_be_bytes());
        buf.extend_from_slice(&self.wire_seq.to_be_bytes());
        buf.extend_from_slice(&self.timestamp_us.to_be_bytes());
    }

    pub fn decode(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < DATAGRAM_HEADER_SIZE {
            return Err(ProtocolError::TooShort {
                expected: DATAGRAM_HEADER_SIZE,
                got: data.len(),
            });
        }
        Ok(DatagramHeader {
            frame_seq: u32::from_be_bytes(data[0..4].try_into().unwrap()),
            frag_idx: u16::from_be_bytes(data[4..6].try_into().unwrap()),
            frag_total: u16::from_be_bytes(data[6..8].try_into().unwrap()),
            wire_seq: u32::from_be_bytes(data[8..12].try_into().unwrap()),
            timestamp_us: u32::from_be_bytes(data[12..16].try_into().unwrap()),
        })
    }

    /// Returns `true` if this datagram is a parity (FEC) fragment.
    /// Parity fragments have `frag_idx >= frag_total`.
    pub fn is_parity(&self) -> bool {
        self.frag_idx >= self.frag_total
    }
}

// ---------------------------------------------------------------------------
// TileHeader (8 bytes)
//   [0]    tile_x
//   [1]    tile_y
//   [2]    (codec << 1) | lz4
//   [3]    (generation: u4) << 4 | (pass: u4)
//   [4..8] payload_len: u32 BE
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileHeader {
    pub tile_x: u8,
    pub tile_y: u8,
    pub codec: Codec,
    pub lz4: bool,
    pub generation: u8, // 4 bits effective (0..=15)
    pub pass: u8,       // 4 bits effective (0..=15)
    pub payload_len: u32,
}

impl TileHeader {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(self.tile_x);
        buf.push(self.tile_y);
        buf.push(((self.codec as u8) << 1) | (self.lz4 as u8));
        buf.push(((self.generation & 0x0F) << 4) | (self.pass & 0x0F));
        buf.extend_from_slice(&self.payload_len.to_be_bytes());
    }

    pub fn decode(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < TILE_HEADER_SIZE {
            return Err(ProtocolError::TooShort {
                expected: TILE_HEADER_SIZE,
                got: data.len(),
            });
        }
        let packed = data[2];
        let codec = Codec::from_u8(packed >> 1)?;
        let lz4 = (packed & 1) != 0;
        let gen_pass = data[3];
        Ok(TileHeader {
            tile_x: data[0],
            tile_y: data[1],
            codec,
            lz4,
            generation: gen_pass >> 4,
            pass: gen_pass & 0x0F,
            payload_len: u32::from_be_bytes(data[4..8].try_into().unwrap()),
        })
    }
}

// ---------------------------------------------------------------------------
// fragment_tile
// ---------------------------------------------------------------------------

/// Identifying inputs that get stamped into every fragment of one tile.
///
/// Bundles the per-tile fields that fill in the [`DatagramHeader`] (minus
/// `frag_idx` / `frag_total`, which are computed during fragmentation) and
/// the [`TileHeader`] (minus `payload_len`, also computed).
#[derive(Clone, Copy, Debug)]
pub struct TileFragmentInputs {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub codec: Codec,
    pub generation: u8,
    pub pass: u8,
    pub timestamp_us: u32,
}

/// Fragments a tile payload into MTU-sized datagrams.
///
/// Each datagram = [DatagramHeader (12 B)][TileHeader (8 B)][payload_fragment].
/// Empty payload (e.g. Skip codec) → single datagram with no payload bytes.
pub fn fragment_tile(
    inputs: &TileFragmentInputs,
    payload: &[u8],
    max_fragment_payload: usize,
) -> Vec<Vec<u8>> {
    assert!(
        max_fragment_payload > 0,
        "max_fragment_payload must be > 0; got 0 (datagram size too small to fit headers)"
    );

    // Determine fragments: at least one, even for empty payload.
    let chunks: Vec<&[u8]> = if payload.is_empty() {
        vec![&[]]
    } else {
        payload.chunks(max_fragment_payload).collect()
    };

    let frag_total = chunks.len() as u16;

    chunks
        .into_iter()
        .enumerate()
        .map(|(idx, chunk)| {
            let dh = DatagramHeader {
                frame_seq: inputs.frame_seq,
                frag_idx: idx as u16,
                frag_total,
                // Emitter stamps the real value at submit time.
                wire_seq: UNSTAMPED_WIRE_SEQ,
                timestamp_us: inputs.timestamp_us,
            };
            let th = TileHeader {
                tile_x: inputs.tile_x,
                tile_y: inputs.tile_y,
                codec: inputs.codec,
                lz4: false,
                generation: inputs.generation,
                pass: inputs.pass,
                payload_len: payload.len() as u32,
            };
            let mut buf = Vec::with_capacity(DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE + chunk.len());
            dh.encode(&mut buf);
            th.encode(&mut buf);
            buf.extend_from_slice(chunk);
            buf
        })
        .collect()
}

// ---------------------------------------------------------------------------
// decode_tile_datagram
// ---------------------------------------------------------------------------

/// Decodes a datagram into its DatagramHeader, TileHeader, and payload slice.
pub fn decode_tile_datagram(
    data: &[u8],
) -> Result<(DatagramHeader, TileHeader, &[u8]), ProtocolError> {
    let dh = DatagramHeader::decode(data)?;
    let rest = &data[DATAGRAM_HEADER_SIZE..];
    let th = TileHeader::decode(rest)?;
    let payload = &rest[TILE_HEADER_SIZE..];
    Ok((dh, th, payload))
}

// ---------------------------------------------------------------------------
// build_parity_datagrams
// ---------------------------------------------------------------------------

/// Build parity datagrams for a tile's source fragments.
///
/// `frag_total` is the number of source fragments (from `fragment_tile`).
/// `parities` is the output of `fec::generate_parity`: `(group_start, parity_payload)` pairs.
///
/// Parity datagrams have `frag_idx` starting at `frag_total` (so they sort after source fragments)
/// and `frag_total` set to the source fragment count (so the receiver knows how many source
/// fragments to expect).
pub fn build_parity_datagrams(
    frame_seq: u32,
    tile_x: u8,
    tile_y: u8,
    codec: Codec,
    timestamp_us: u32,
    frag_total: u16,
    parities: &[(u16, Vec<u8>)],
) -> Vec<Vec<u8>> {
    parities
        .iter()
        .enumerate()
        .map(|(parity_idx, (_group_start, parity_payload))| {
            let dh = DatagramHeader {
                frame_seq,
                frag_idx: frag_total + parity_idx as u16,
                frag_total,
                // Emitter stamps the real value at submit time.
                wire_seq: UNSTAMPED_WIRE_SEQ,
                timestamp_us,
            };
            let th = TileHeader {
                tile_x,
                tile_y,
                codec,
                lz4: false,
                generation: 0,
                pass: 0,
                payload_len: 0, // not meaningful for parity
            };
            let mut buf =
                Vec::with_capacity(DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE + parity_payload.len());
            dh.encode(&mut buf);
            th.encode(&mut buf);
            buf.extend_from_slice(parity_payload);
            buf
        })
        .collect()
}

// ---------------------------------------------------------------------------
// max_fragment_payload
// ---------------------------------------------------------------------------

/// Returns the maximum tile payload bytes that fit in one datagram of `max_datagram_size`.
pub fn max_fragment_payload(max_datagram_size: usize) -> usize {
    max_datagram_size.saturating_sub(DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE)
}

// ---------------------------------------------------------------------------
// build_frame_dimensions_datagram
// ---------------------------------------------------------------------------

/// Sentinel tile coordinates marking a control message that carries the
/// current frame dimensions rather than pixel data. Tile coords are `u8`;
/// 0xFF (255) is structurally impossible at any sensible resolution
/// (would imply >8000 px width), so the receiver can route on the sentinel.
pub const FRAME_DIMENSIONS_SENTINEL_X: u8 = 0xFF;
pub const FRAME_DIMENSIONS_SENTINEL_Y: u8 = 0xFF;

/// Build a single frame-dimensions datagram.
///
/// Format: standard tile datagram with `(tile_x, tile_y) = (0xFF, 0xFF)`,
/// `codec = Codec::Skip`, and an 8-byte payload `[width: u32 BE][height: u32 BE]`.
///
/// Always fits in a single datagram (8-byte payload, 32-byte total).
pub fn build_frame_dimensions_datagram(
    frame_seq: u32,
    timestamp_us: u32,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&width.to_be_bytes());
    payload.extend_from_slice(&height.to_be_bytes());

    let inputs = TileFragmentInputs {
        frame_seq: frame_seq | TILE_DATAGRAM_FLAG,
        tile_x: FRAME_DIMENSIONS_SENTINEL_X,
        tile_y: FRAME_DIMENSIONS_SENTINEL_Y,
        codec: Codec::Skip,
        generation: 0,
        pass: 0,
        timestamp_us,
    };
    let datagrams = fragment_tile(&inputs, &payload, /* max_fragment_payload */ 8);
    debug_assert_eq!(datagrams.len(), 1, "frame dimensions must fit one datagram");
    datagrams.into_iter().next().unwrap()
}

// ---------------------------------------------------------------------------
// Discriminator: tile vs frame datagrams
// ---------------------------------------------------------------------------

/// Bit 31 of frame_seq distinguishes tile datagrams from frame datagrams.
/// Frame datagrams: bit 31 = 0. Tile datagrams: bit 31 = 1.
pub const TILE_DATAGRAM_FLAG: u32 = 1 << 31;

/// Returns true if the datagram is a tile-level datagram (bit 31 of frame_seq = 1).
pub fn is_tile_datagram(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    let first_u32 = u32::from_be_bytes(data[0..4].try_into().unwrap());
    (first_u32 & TILE_DATAGRAM_FLAG) != 0
}

// ---------------------------------------------------------------------------
// FrameHeader (14 bytes)
//   [0..4]  frame_seq:     u32 BE (bit 31 always 0)
//   [4..6]  frag_idx:      u16 BE
//   [6..8]  frag_total:    u16 BE
//   [8..12] timestamp_us:  u32 BE
//   [12]    flags:         u8  (bit 0 = is_keyframe)
//   [13]    reserved:      u8
// ---------------------------------------------------------------------------

pub const FRAME_HEADER_SIZE: usize = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub frame_seq: u32,
    pub frag_idx: u16,
    pub frag_total: u16,
    pub timestamp_us: u32,
    pub flags: u8,
    pub reserved: u8,
}

impl FrameHeader {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.frame_seq.to_be_bytes());
        buf.extend_from_slice(&self.frag_idx.to_be_bytes());
        buf.extend_from_slice(&self.frag_total.to_be_bytes());
        buf.extend_from_slice(&self.timestamp_us.to_be_bytes());
        buf.push(self.flags);
        buf.push(self.reserved);
    }

    pub fn decode(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < FRAME_HEADER_SIZE {
            return Err(ProtocolError::TooShort {
                expected: FRAME_HEADER_SIZE,
                got: data.len(),
            });
        }
        Ok(FrameHeader {
            frame_seq: u32::from_be_bytes(data[0..4].try_into().unwrap()),
            frag_idx: u16::from_be_bytes(data[4..6].try_into().unwrap()),
            frag_total: u16::from_be_bytes(data[6..8].try_into().unwrap()),
            timestamp_us: u32::from_be_bytes(data[8..12].try_into().unwrap()),
            flags: data[12],
            reserved: data[13],
        })
    }

    /// Returns `true` if this datagram is a parity (FEC) fragment.
    /// Parity fragments have `frag_idx >= frag_total`.
    pub fn is_parity(&self) -> bool {
        self.frag_idx >= self.frag_total
    }

    /// Returns `true` if the keyframe flag (bit 0) is set.
    pub fn is_keyframe(&self) -> bool {
        (self.flags & 0x01) != 0
    }
}

// ---------------------------------------------------------------------------
// fragment_frame
// ---------------------------------------------------------------------------

/// Fragments a full-frame H.264 payload into MTU-sized datagrams.
///
/// Each datagram = [FrameHeader (14 B)][payload_fragment].
/// Empty payload → single datagram with no payload bytes.
pub fn fragment_frame(
    frame_seq: u32,
    timestamp_us: u32,
    is_keyframe: bool,
    payload: &[u8],
    max_fragment_payload: usize,
) -> Vec<Vec<u8>> {
    assert!(
        max_fragment_payload > 0,
        "max_fragment_payload must be > 0; got 0 (datagram size too small to fit headers)"
    );

    let flags: u8 = if is_keyframe { 0x01 } else { 0x00 };

    let chunks: Vec<&[u8]> = if payload.is_empty() {
        vec![&[]]
    } else {
        payload.chunks(max_fragment_payload).collect()
    };

    let frag_total = chunks.len() as u16;

    chunks
        .into_iter()
        .enumerate()
        .map(|(idx, chunk)| {
            let fh = FrameHeader {
                frame_seq,
                frag_idx: idx as u16,
                frag_total,
                timestamp_us,
                flags,
                reserved: 0,
            };
            let mut buf = Vec::with_capacity(FRAME_HEADER_SIZE + chunk.len());
            fh.encode(&mut buf);
            buf.extend_from_slice(chunk);
            buf
        })
        .collect()
}

// ---------------------------------------------------------------------------
// decode_frame_datagram
// ---------------------------------------------------------------------------

/// Decodes a frame datagram into its FrameHeader and payload slice.
pub fn decode_frame_datagram(data: &[u8]) -> Result<(FrameHeader, &[u8]), ProtocolError> {
    let fh = FrameHeader::decode(data)?;
    let payload = &data[FRAME_HEADER_SIZE..];
    Ok((fh, payload))
}

// ---------------------------------------------------------------------------
// max_frame_fragment_payload
// ---------------------------------------------------------------------------

/// Returns the maximum frame payload bytes that fit in one datagram of `max_datagram_size`.
pub fn max_frame_fragment_payload(max_datagram_size: usize) -> usize {
    max_datagram_size.saturating_sub(FRAME_HEADER_SIZE)
}

// ---------------------------------------------------------------------------
// build_frame_parity_datagram
// ---------------------------------------------------------------------------

/// Builds a parity datagram for a frame's source fragments.
///
/// The parity datagram uses `frag_idx = frag_total` to signal it is a parity fragment.
/// `frag_total` is the number of source fragments (from `fragment_frame`).
pub fn build_frame_parity_datagram(
    frame_seq: u32,
    timestamp_us: u32,
    is_keyframe: bool,
    frag_total: u16,
    parity_payload: &[u8],
) -> Vec<u8> {
    let flags: u8 = if is_keyframe { 0x01 } else { 0x00 };
    let fh = FrameHeader {
        frame_seq,
        frag_idx: frag_total, // signals parity
        frag_total,
        timestamp_us,
        flags,
        reserved: 0,
    };
    let mut buf = Vec::with_capacity(FRAME_HEADER_SIZE + parity_payload.len());
    fh.encode(&mut buf);
    buf.extend_from_slice(parity_payload);
    buf
}

// ---------------------------------------------------------------------------
// NACK message (sent by client on bidi stream)
// ---------------------------------------------------------------------------

/// NACK message size: frame_seq (4) + frag_idx (2) = 6 bytes.
pub const NACK_SIZE: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NackMessage {
    pub frame_seq: u32,
    pub frag_idx: u16,
}

impl NackMessage {
    pub fn encode(&self) -> [u8; NACK_SIZE] {
        let mut buf = [0u8; NACK_SIZE];
        buf[0..4].copy_from_slice(&self.frame_seq.to_be_bytes());
        buf[4..6].copy_from_slice(&self.frag_idx.to_be_bytes());
        buf
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < NACK_SIZE {
            return None;
        }
        Some(NackMessage {
            frame_seq: u32::from_be_bytes(data[0..4].try_into().unwrap()),
            frag_idx: u16::from_be_bytes(data[4..6].try_into().unwrap()),
        })
    }
}

// ---------------------------------------------------------------------------
// TILE_PARITY envelope (0x04)
//
// Wire format (spec §5.2):
//   [0]       discriminator:              u8  (= TILE_PARITY_ENVELOPE = 0x04)
//   [1..5]    group_first_wire_seq:       u32 BE
//   [5]       k:                          u8  (number of source datagrams covered)
//   [6]       parity_idx:                 u8  (0-indexed within group's R parities)
//   [7..9]    group_first_payload_len:    u16 BE
//   [9..]     parity_payload                  (XOR of K sources, left-padded)
// ---------------------------------------------------------------------------

/// Envelope discriminator byte for the tile-FEC parity datagram.
pub const TILE_PARITY_ENVELOPE: u8 = 0x04;

/// Header of a parity datagram (envelope `0x04`). Covers K source datagrams
/// starting at `group_first_wire_seq`. The `parity_payload` is the XOR of
/// the K sources' full byte buffers, left-padded to the longest source's
/// length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileParityEnvelope {
    pub group_first_wire_seq: u32,
    pub k: u8,
    /// Index of this parity within the group's R parities. v1 always emits
    /// R=1 parity per group, so `parity_idx` is always 0.
    pub parity_idx: u8,
    /// Length of the first source datagram in the group, used by the
    /// decoder to extract a recovered payload of the right size.
    pub group_first_payload_len: u16,
    pub parity_payload: Vec<u8>,
}

/// Size of the fixed-length header preceding `parity_payload`.
pub const TILE_PARITY_HEADER_SIZE: usize = 1 + 4 + 1 + 1 + 2; // = 9

impl TileParityEnvelope {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(TILE_PARITY_ENVELOPE);
        buf.extend_from_slice(&self.group_first_wire_seq.to_be_bytes());
        buf.push(self.k);
        buf.push(self.parity_idx);
        buf.extend_from_slice(&self.group_first_payload_len.to_be_bytes());
        buf.extend_from_slice(&self.parity_payload);
    }

    pub fn decode(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < TILE_PARITY_HEADER_SIZE {
            return Err(ProtocolError::TooShort {
                expected: TILE_PARITY_HEADER_SIZE,
                got: data.len(),
            });
        }
        if data[0] != TILE_PARITY_ENVELOPE {
            return Err(ProtocolError::UnknownEnvelope(data[0]));
        }
        let group_first_wire_seq = u32::from_be_bytes(data[1..5].try_into().unwrap());
        let k = data[5];
        let parity_idx = data[6];
        let group_first_payload_len = u16::from_be_bytes(data[7..9].try_into().unwrap());
        let parity_payload = data[TILE_PARITY_HEADER_SIZE..].to_vec();
        Ok(Self {
            group_first_wire_seq,
            k,
            parity_idx,
            group_first_payload_len,
            parity_payload,
        })
    }
}

// ---------------------------------------------------------------------------
// TILE_NACK envelope (0x05)
//
// Wire format (spec §5.3):
//   [0]       discriminator: u8  (= TILE_NACK_ENVELOPE = 0x05)
//   [1]       count:         u8  (number of entries, ≤ TILE_NACK_MAX_ENTRIES)
//   [2..]     count × NackEntry, each 8 bytes:
//             [0..4] frame_seq u32 LE | [4] tile_x | [5] tile_y |
//             [6] pass_idx | [7] frag_idx
//
// `frame_seq` is LE on the wire to match the ACK envelope's existing
// convention.
// ---------------------------------------------------------------------------

/// Envelope discriminator byte for the per-fragment NACK datagram
/// (client → server).
pub const TILE_NACK_ENVELOPE: u8 = 0x05;

/// Size on the wire of a single NACK entry.
pub const TILE_NACK_ENTRY_SIZE: usize = 8;

/// Maximum number of NACK entries per datagram. Mirrors the ACK envelope's
/// cap so a NackBatcher can use the same chunking pattern.
pub const TILE_NACK_MAX_ENTRIES: usize = 64;

/// One missing fragment reported by the client.
///
/// `frame_seq` is encoded LE on the wire to match the ACK envelope's
/// existing convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileNackEntry {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
    pub frag_idx: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileNackEnvelope {
    pub entries: Vec<TileNackEntry>,
}

impl TileNackEnvelope {
    /// Encode all entries (panics in debug if `entries.len() > 64`; use
    /// `encode_clamped` when the source list may be longer).
    pub fn encode(&self, buf: &mut Vec<u8>) {
        debug_assert!(self.entries.len() <= TILE_NACK_MAX_ENTRIES);
        Self::write(buf, &self.entries);
    }

    /// Encode at most `TILE_NACK_MAX_ENTRIES` entries; returns the number
    /// actually written.
    pub fn encode_clamped(&self, buf: &mut Vec<u8>) -> usize {
        let n = self.entries.len().min(TILE_NACK_MAX_ENTRIES);
        Self::write(buf, &self.entries[..n]);
        n
    }

    fn write(buf: &mut Vec<u8>, entries: &[TileNackEntry]) {
        buf.push(TILE_NACK_ENVELOPE);
        buf.push(entries.len() as u8);
        for e in entries {
            buf.extend_from_slice(&e.frame_seq.to_le_bytes());
            buf.push(e.tile_x);
            buf.push(e.tile_y);
            buf.push(e.pass_idx);
            buf.push(e.frag_idx);
        }
    }

    pub fn decode(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < 2 {
            return Err(ProtocolError::TooShort {
                expected: 2,
                got: data.len(),
            });
        }
        if data[0] != TILE_NACK_ENVELOPE {
            return Err(ProtocolError::UnknownEnvelope(data[0]));
        }
        let count = data[1] as usize;
        if count > TILE_NACK_MAX_ENTRIES {
            return Err(ProtocolError::InvalidLength);
        }
        let expected = 2 + count * TILE_NACK_ENTRY_SIZE;
        if data.len() < expected {
            return Err(ProtocolError::TooShort {
                expected,
                got: data.len(),
            });
        }
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let off = 2 + i * TILE_NACK_ENTRY_SIZE;
            entries.push(TileNackEntry {
                frame_seq: u32::from_le_bytes(data[off..off + 4].try_into().unwrap()),
                tile_x: data[off + 4],
                tile_y: data[off + 5],
                pass_idx: data[off + 6],
                frag_idx: data[off + 7],
            });
        }
        Ok(Self { entries })
    }
}

// ---------------------------------------------------------------------------
// Inbound classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundKind {
    Empty,
    Unknown,
    Hello,         // 0x01
    AckBatchV1,    // 0x02 (deprecated, still routed)
    AckBatch,      // 0x03
    TileParity,    // 0x04
    TileNack,      // 0x05
    FrameFragment, // first byte 0x10..=0x7F
    TileFragment,  // first byte 0x80..=0xFF (TILE_DATAGRAM_FLAG bit set)
}

pub fn classify_inbound(data: &[u8]) -> InboundKind {
    let Some(&first) = data.first() else { return InboundKind::Empty };
    match first {
        0x01 => InboundKind::Hello,
        0x02 => InboundKind::AckBatchV1,
        0x03 => InboundKind::AckBatch,
        TILE_PARITY_ENVELOPE => InboundKind::TileParity,
        TILE_NACK_ENVELOPE => InboundKind::TileNack,
        b if b < 0x10 => InboundKind::Unknown,
        b if b < 0x80 => InboundKind::FrameFragment,
        _ => InboundKind::TileFragment,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagram_header_roundtrip() {
        let original = DatagramHeader {
            frame_seq: 0xDEAD_BEEF,
            frag_idx: 3,
            frag_total: 7,
            wire_seq: 0xCAFE_F00D,
            timestamp_us: 123_456_789,
        };
        let mut buf = Vec::new();
        original.encode(&mut buf);
        assert_eq!(buf.len(), DATAGRAM_HEADER_SIZE);
        let decoded = DatagramHeader::decode(&buf).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn datagram_header_decode_short_input() {
        let short = vec![0u8; 15];
        let result = DatagramHeader::decode(&short);
        assert_eq!(
            result,
            Err(ProtocolError::TooShort {
                expected: DATAGRAM_HEADER_SIZE,
                got: 15
            })
        );
    }

    #[test]
    fn tile_header_roundtrip() {
        let original = TileHeader {
            tile_x: 5,
            tile_y: 3,
            codec: Codec::H264,
            lz4: false,
            generation: 10,
            pass: 0,
            payload_len: 99_999,
        };
        let mut buf = Vec::new();
        original.encode(&mut buf);
        assert_eq!(buf.len(), TILE_HEADER_SIZE);
        let decoded = TileHeader::decode(&buf).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn tile_header_codec_lz4_packing() {
        // Raw=4, lz4=true → packed byte = (4 << 1) | 1 = 9
        let th = TileHeader {
            tile_x: 0,
            tile_y: 0,
            codec: Codec::Raw,
            lz4: true,
            generation: 0,
            pass: 0,
            payload_len: 0,
        };
        let mut buf = Vec::new();
        th.encode(&mut buf);
        assert_eq!(buf[2], 9u8);

        let decoded = TileHeader::decode(&buf).unwrap();
        assert_eq!(decoded.codec, Codec::Raw);
        assert!(decoded.lz4);
    }

    #[test]
    fn tile_header_gen_pass_packing_roundtrip() {
        let original = TileHeader {
            tile_x: 1,
            tile_y: 2,
            codec: Codec::Solid,
            lz4: false,
            generation: 5,
            pass: 9,
            payload_len: 4,
        };
        let mut buf = Vec::new();
        original.encode(&mut buf);
        // Byte [3] must be (5 << 4) | 9 = 0x59
        assert_eq!(buf[3], 0x59);
        let decoded = TileHeader::decode(&buf).unwrap();
        assert_eq!(decoded.generation, 5);
        assert_eq!(decoded.pass, 9);
        assert_eq!(decoded, original);
    }

    #[test]
    fn tile_header_decode_max_nibble_values() {
        // Decoding a byte with gen=15, pass=15 yields the max values.
        let buf = vec![0, 0, 0, 0xFF, 0, 0, 0, 4];
        let decoded = TileHeader::decode(&buf).unwrap();
        assert_eq!(decoded.generation, 15);
        assert_eq!(decoded.pass, 15);
    }

    #[test]
    fn tile_header_encode_clips_generation_and_pass_to_4_bits() {
        // generation=0x1F and pass=0x1F should both clip to 0x0F on the wire.
        let h = TileHeader {
            tile_x: 0,
            tile_y: 0,
            codec: Codec::Raw,
            lz4: false,
            generation: 0x1F,
            pass: 0x1F,
            payload_len: 0,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        assert_eq!(buf[3], 0xFF, "both nibbles must clip to 0x0F");
        let decoded = TileHeader::decode(&buf).unwrap();
        assert_eq!(decoded.generation, 15);
        assert_eq!(decoded.pass, 15);
    }

    #[test]
    fn tile_header_legacy_generation_zero_decodes_as_pass_zero() {
        // Existing M2 wire format with generation=0 must decode as gen=0, pass=0.
        let buf = vec![3, 4, 0, 0x00, 0, 0, 0, 0];
        let decoded = TileHeader::decode(&buf).unwrap();
        assert_eq!(decoded.generation, 0);
        assert_eq!(decoded.pass, 0);
    }

    #[test]
    fn encode_tile_datagram_single_fragment() {
        let payload = vec![0xABu8; 100];
        let datagrams = fragment_tile(
            &TileFragmentInputs {
                frame_seq: 1,
                tile_x: 2,
                tile_y: 3,
                codec: Codec::Raw,
                generation: 0,
                pass: 0,
                timestamp_us: 5000,
            },
            &payload,
            1200,
        );
        assert_eq!(datagrams.len(), 1);

        let dg = &datagrams[0];
        let (dh, th, frag_payload) = decode_tile_datagram(dg).unwrap();

        assert_eq!(dh.frame_seq, 1);
        assert_eq!(dh.frag_idx, 0);
        assert_eq!(dh.frag_total, 1);
        assert_eq!(dh.timestamp_us, 5000);

        assert_eq!(th.tile_x, 2);
        assert_eq!(th.tile_y, 3);
        assert_eq!(th.codec, Codec::Raw);
        assert_eq!(th.payload_len, 100);

        assert_eq!(frag_payload, payload.as_slice());
    }

    #[test]
    fn fragment_tile_multiple_fragments() {
        let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let max_frag = 1200;
        let datagrams = fragment_tile(
            &TileFragmentInputs {
                frame_seq: 7,
                tile_x: 0,
                tile_y: 0,
                codec: Codec::H264,
                generation: 0,
                pass: 0,
                timestamp_us: 999,
            },
            &payload,
            max_frag,
        );

        // ceil(4096 / 1200) = 4
        assert_eq!(datagrams.len(), 4);

        // Verify each datagram has the right frag_idx and frag_total.
        for (i, dg) in datagrams.iter().enumerate() {
            let (dh, th, _) = decode_tile_datagram(dg).unwrap();
            assert_eq!(dh.frame_seq, 7);
            assert_eq!(dh.frag_idx, i as u16);
            assert_eq!(dh.frag_total, 4);
            assert_eq!(th.payload_len, 4096);
        }

        // Reassemble and verify matches original.
        let mut reassembled = Vec::new();
        for dg in &datagrams {
            let (_, _, frag) = decode_tile_datagram(dg).unwrap();
            reassembled.extend_from_slice(frag);
        }
        assert_eq!(reassembled, payload);
    }

    #[test]
    fn fragment_tile_empty_payload_skip_codec() {
        let datagrams = fragment_tile(
            &TileFragmentInputs {
                frame_seq: 0,
                tile_x: 0,
                tile_y: 0,
                codec: Codec::Skip,
                generation: 0,
                pass: 0,
                timestamp_us: 0,
            },
            &[],
            1200,
        );
        assert_eq!(datagrams.len(), 1);

        let (dh, th, frag_payload) = decode_tile_datagram(&datagrams[0]).unwrap();
        assert_eq!(dh.frag_total, 1);
        assert_eq!(th.codec, Codec::Skip);
        assert_eq!(th.payload_len, 0);
        assert!(frag_payload.is_empty());
    }

    #[test]
    fn is_parity_datagram() {
        // Source fragment: frag_idx < frag_total
        let source = DatagramHeader {
            frame_seq: 1,
            frag_idx: 2,
            frag_total: 8,
            wire_seq: 0,
            timestamp_us: 0,
        };
        assert!(!source.is_parity());

        // Parity fragment: frag_idx >= frag_total
        let parity = DatagramHeader {
            frame_seq: 1,
            frag_idx: 8,
            frag_total: 8,
            wire_seq: 0,
            timestamp_us: 0,
        };
        assert!(parity.is_parity());
    }

    #[test]
    fn fragment_tile_with_parity_roundtrip() {
        use crate::transport::fec;

        let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let max_frag = 1200;
        let k = 4;

        // Generate source datagrams
        let source_dgs = fragment_tile(
            &TileFragmentInputs {
                frame_seq: 7,
                tile_x: 2,
                tile_y: 3,
                codec: Codec::H264,
                generation: 0,
                pass: 0,
                timestamp_us: 999,
            },
            &payload,
            max_frag,
        );
        let frag_total = source_dgs.len();
        assert_eq!(frag_total, 4); // ceil(4096/1200) = 4

        // Extract source payloads (bytes after headers)
        let source_payloads: Vec<&[u8]> = source_dgs
            .iter()
            .map(|dg| &dg[DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE..])
            .collect();

        // Generate parity
        let parities = fec::generate_parity(&source_payloads, k);
        assert_eq!(parities.len(), 1); // 4 frags / K=4 = 1 parity group

        // Build parity datagrams using the helper
        let parity_dgs =
            build_parity_datagrams(7, 2, 3, Codec::H264, 999, frag_total as u16, &parities);
        assert_eq!(parity_dgs.len(), 1);

        // Decode the parity datagram
        let (dh, th, parity_payload) = decode_tile_datagram(&parity_dgs[0]).unwrap();
        assert!(dh.is_parity());
        assert_eq!(dh.frag_idx, frag_total as u16); // first parity index
        assert_eq!(dh.frag_total, frag_total as u16); // source fragment count preserved
        assert_eq!(th.codec, Codec::H264);
        assert_eq!(th.tile_x, 2);
        assert_eq!(th.tile_y, 3);

        // Verify parity payload has the parity header
        let (group_start, group_len, _xor_data) =
            fec::decode_parity_payload(parity_payload).unwrap();
        assert_eq!(group_start, 0);
        assert_eq!(group_len, 4);
    }

    #[test]
    fn frame_header_roundtrip() {
        let original = FrameHeader {
            frame_seq: 0x0000_1234,
            frag_idx: 2,
            frag_total: 5,
            timestamp_us: 987_654_321,
            flags: 0x01,
            reserved: 0,
        };
        let mut buf = Vec::new();
        original.encode(&mut buf);
        assert_eq!(buf.len(), FRAME_HEADER_SIZE);
        let decoded = FrameHeader::decode(&buf).unwrap();
        assert_eq!(decoded, original);
        assert!(decoded.is_keyframe());
        assert!(!decoded.is_parity());
    }

    #[test]
    fn frame_header_discriminator() {
        // Frame datagram: bit 31 of frame_seq = 0
        let fh = FrameHeader {
            frame_seq: 42,
            frag_idx: 0,
            frag_total: 1,
            timestamp_us: 0,
            flags: 0,
            reserved: 0,
        };
        let mut frame_buf = Vec::new();
        fh.encode(&mut frame_buf);
        assert!(
            !is_tile_datagram(&frame_buf),
            "frame datagram must have bit 31 = 0"
        );

        // Tile datagram: bit 31 of frame_seq = 1 (TILE_DATAGRAM_FLAG set)
        let dh = DatagramHeader {
            frame_seq: TILE_DATAGRAM_FLAG | 42,
            frag_idx: 0,
            frag_total: 1,
            wire_seq: 0,
            timestamp_us: 0,
        };
        let mut tile_buf = Vec::new();
        dh.encode(&mut tile_buf);
        // Pad with a tile header's worth of zeros so is_tile_datagram only checks first 4 bytes
        tile_buf.extend_from_slice(&[0u8; TILE_HEADER_SIZE]);
        assert!(
            is_tile_datagram(&tile_buf),
            "tile datagram must have bit 31 = 1"
        );
    }

    #[test]
    fn fragment_frame_single() {
        let payload = vec![0xCDu8; 500];
        let datagrams = fragment_frame(10, 8000, true, &payload, 1200);
        assert_eq!(datagrams.len(), 1);

        let (fh, frag_payload) = decode_frame_datagram(&datagrams[0]).unwrap();
        assert_eq!(fh.frame_seq, 10);
        assert_eq!(fh.frag_idx, 0);
        assert_eq!(fh.frag_total, 1);
        assert_eq!(fh.timestamp_us, 8000);
        assert!(fh.is_keyframe());
        assert!(!fh.is_parity());
        assert_eq!(frag_payload, payload.as_slice());
    }

    #[test]
    fn fragment_frame_multiple() {
        let payload: Vec<u8> = (0u8..=255).cycle().take(5000).collect();
        let max_frag = 1200;
        let datagrams = fragment_frame(99, 12345, false, &payload, max_frag);

        // ceil(5000 / 1200) = 5
        assert_eq!(datagrams.len(), 5);

        for (i, dg) in datagrams.iter().enumerate() {
            let (fh, _) = decode_frame_datagram(dg).unwrap();
            assert_eq!(fh.frame_seq, 99);
            assert_eq!(fh.frag_idx, i as u16);
            assert_eq!(fh.frag_total, 5);
            assert_eq!(fh.timestamp_us, 12345);
            assert!(!fh.is_keyframe());
        }

        // Reassemble and verify matches original.
        let mut reassembled = Vec::new();
        for dg in &datagrams {
            let (_, frag) = decode_frame_datagram(dg).unwrap();
            reassembled.extend_from_slice(frag);
        }
        assert_eq!(reassembled, payload);
    }

    #[test]
    fn nack_message_roundtrip() {
        let nack = NackMessage {
            frame_seq: 42,
            frag_idx: 3,
        };
        let encoded = nack.encode();
        assert_eq!(encoded.len(), NACK_SIZE);
        let decoded = NackMessage::decode(&encoded).unwrap();
        assert_eq!(decoded, nack);
    }

    #[test]
    fn nack_message_short_input() {
        assert!(NackMessage::decode(&[0u8; 5]).is_none());
        assert!(NackMessage::decode(&[0u8; 6]).is_some());
    }

    #[test]
    fn datagram_header_includes_wire_seq() {
        // Header must be 16 bytes (was 12 before wire_seq was added). This
        // assert catches accidental shrinkage just as well as growth.
        assert_eq!(DATAGRAM_HEADER_SIZE, 16, "header changed size");

        // Mid-range value: catches the obvious encode/decode mistakes.
        let h = DatagramHeader {
            frame_seq: TILE_DATAGRAM_FLAG | 42,
            frag_idx: 1,
            frag_total: 3,
            wire_seq: 0xDEADBEEF,
            timestamp_us: 1_000_000,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        assert_eq!(buf.len(), DATAGRAM_HEADER_SIZE);
        let parsed = DatagramHeader::decode(&buf).expect("decode");
        assert_eq!(parsed.wire_seq, 0xDEADBEEF);
        assert_eq!(parsed.timestamp_us, 1_000_000);
        assert_eq!(parsed.frame_seq, TILE_DATAGRAM_FLAG | 42);

        // wire_seq = 0 (the "not yet stamped" sentinel) round-trips exactly.
        // If `wire_seq` and `timestamp_us` were swapped at offset 8/12 this
        // would still pass — so the next case nails the offsets down.
        let h_zero = DatagramHeader {
            frame_seq: 0,
            frag_idx: 0,
            frag_total: 1,
            wire_seq: 0,
            timestamp_us: 7,
        };
        let mut buf_zero = Vec::new();
        h_zero.encode(&mut buf_zero);
        let parsed_zero = DatagramHeader::decode(&buf_zero).expect("decode");
        assert_eq!(parsed_zero.wire_seq, 0);
        assert_eq!(parsed_zero.timestamp_us, 7);
        // Verify offsets: wire_seq lives at bytes 8..12, timestamp_us at 12..16.
        // With wire_seq=0 and timestamp_us=7 these slices must differ; an
        // off-by-two encode would smear the 7 into the wire_seq slot.
        assert_eq!(&buf_zero[8..12], &[0, 0, 0, 0]);
        assert_eq!(&buf_zero[12..16], &[0, 0, 0, 7]);

        // wire_seq = u32::MAX (the wrap-edge value) round-trips exactly.
        let h_max = DatagramHeader {
            frame_seq: 1,
            frag_idx: 0,
            frag_total: 1,
            wire_seq: u32::MAX,
            timestamp_us: u32::MAX,
        };
        let mut buf_max = Vec::new();
        h_max.encode(&mut buf_max);
        let parsed_max = DatagramHeader::decode(&buf_max).expect("decode");
        assert_eq!(parsed_max.wire_seq, u32::MAX);
        assert_eq!(parsed_max.timestamp_us, u32::MAX);
    }

    #[test]
    fn frame_dimensions_datagram_roundtrip() {
        let dg = build_frame_dimensions_datagram(
            /*frame_seq*/ 42, /*ts*/ 1000, /*width*/ 1920, /*height*/ 1080,
        );
        // Total: 16 (DatagramHeader) + 8 (TileHeader) + 8 (payload) = 32 bytes.
        assert_eq!(dg.len(), 32);

        let (dh, th, payload) = decode_tile_datagram(&dg).expect("decode failed");
        // Tile-datagram flag must be set in frame_seq.
        assert_ne!(
            dh.frame_seq & TILE_DATAGRAM_FLAG,
            0,
            "TILE_DATAGRAM_FLAG must be set"
        );
        assert_eq!(dh.frame_seq & !TILE_DATAGRAM_FLAG, 42);
        assert_eq!(dh.frag_idx, 0);
        assert_eq!(dh.frag_total, 1);
        assert_eq!(dh.timestamp_us, 1000);

        assert_eq!(th.tile_x, FRAME_DIMENSIONS_SENTINEL_X);
        assert_eq!(th.tile_y, FRAME_DIMENSIONS_SENTINEL_Y);
        assert_eq!(th.codec, Codec::Skip);
        assert_eq!(th.payload_len, 8);

        assert_eq!(payload.len(), 8);
        let w = u32::from_be_bytes(payload[0..4].try_into().unwrap());
        let h = u32::from_be_bytes(payload[4..8].try_into().unwrap());
        assert_eq!(w, 1920);
        assert_eq!(h, 1080);
    }

    #[test]
    fn tile_parity_envelope_roundtrip() {
        let parity_payload = vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        let envelope = TileParityEnvelope {
            group_first_wire_seq: 1000,
            k: 10,
            parity_idx: 0,
            group_first_payload_len: 512,
            parity_payload: parity_payload.clone(),
        };
        let mut buf = Vec::new();
        envelope.encode(&mut buf);
        assert_eq!(buf[0], TILE_PARITY_ENVELOPE);
        let parsed = TileParityEnvelope::decode(&buf).expect("decode");
        assert_eq!(parsed.group_first_wire_seq, 1000);
        assert_eq!(parsed.k, 10);
        assert_eq!(parsed.parity_idx, 0);
        assert_eq!(parsed.group_first_payload_len, 512);
        assert_eq!(parsed.parity_payload, parity_payload);
    }

    #[test]
    fn tile_parity_envelope_rejects_wrong_discriminator() {
        let mut buf = vec![0x99, 0, 0, 0, 0, 10, 0, 0, 0];
        assert!(TileParityEnvelope::decode(&buf).is_err());
        buf[0] = TILE_PARITY_ENVELOPE;
        // still too short for header
        assert!(TileParityEnvelope::decode(&buf[..3]).is_err());
    }

    #[test]
    fn tile_nack_envelope_roundtrip() {
        let entries = vec![
            TileNackEntry { frame_seq: 100, tile_x: 5, tile_y: 7, pass_idx: 3, frag_idx: 1 },
            TileNackEntry { frame_seq: 100, tile_x: 5, tile_y: 7, pass_idx: 4, frag_idx: 0 },
        ];
        let env = TileNackEnvelope { entries: entries.clone() };
        let mut buf = Vec::new();
        env.encode(&mut buf);
        assert_eq!(buf[0], TILE_NACK_ENVELOPE);
        assert_eq!(buf[1], 2);
        let parsed = TileNackEnvelope::decode(&buf).expect("decode");
        assert_eq!(parsed.entries, entries);
    }

    #[test]
    fn tile_nack_envelope_caps_at_64_entries() {
        let entries: Vec<_> = (0..70)
            .map(|i| TileNackEntry { frame_seq: i, tile_x: 0, tile_y: 0, pass_idx: 0, frag_idx: 0 })
            .collect();
        let env = TileNackEnvelope { entries };
        let mut buf = Vec::new();
        let written = env.encode_clamped(&mut buf);
        assert_eq!(written, 64, "encode_clamped writes at most 64 entries");
        let parsed = TileNackEnvelope::decode(&buf).unwrap();
        assert_eq!(parsed.entries.len(), 64);
    }

    #[test]
    fn classify_envelope_routes_by_first_byte() {
        // Tile fragment: bit 31 of frame_seq set
        let mut tile_dg = vec![0u8; DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE];
        tile_dg[0] = 0x80;  // TILE_DATAGRAM_FLAG high byte
        assert_eq!(classify_inbound(&tile_dg), InboundKind::TileFragment);

        // Frame fragment: bit 31 clear
        let mut frame_dg = vec![0u8; FRAME_HEADER_SIZE];
        frame_dg[0] = 0x10;
        assert_eq!(classify_inbound(&frame_dg), InboundKind::FrameFragment);

        // Envelopes
        assert_eq!(classify_inbound(&[0x03]), InboundKind::AckBatch);
        assert_eq!(classify_inbound(&[TILE_PARITY_ENVELOPE]), InboundKind::TileParity);
        assert_eq!(classify_inbound(&[TILE_NACK_ENVELOPE]), InboundKind::TileNack);
        assert_eq!(classify_inbound(&[0x09]), InboundKind::Unknown);
        assert_eq!(classify_inbound(&[]), InboundKind::Empty);
    }
}
