//! Wire format for M1 tile datagrams.
//!
//! Each QUIC datagram carries one fragment of one tile:
//! ```text
//! [DatagramHeader: 12 bytes][TileHeader: 8 bytes][payload: variable]
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

pub const DATAGRAM_HEADER_SIZE: usize = 12;
pub const TILE_HEADER_SIZE: usize = 8;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error, PartialEq)]
pub enum ProtocolError {
    #[error("input too short: expected {expected} bytes, got {got}")]
    TooShort { expected: usize, got: usize },

    #[error("unknown codec byte: {0}")]
    UnknownCodec(u8),
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
    Bc1 = 3,
    Solid = 4,
    Raw = 5,
    Cdf53 = 6,
}

impl Codec {
    pub fn from_u8(v: u8) -> Result<Self, ProtocolError> {
        match v {
            0 => Ok(Codec::Skip),
            1 => Ok(Codec::H264),
            2 => Ok(Codec::PalRle),
            3 => Ok(Codec::Bc1),
            4 => Ok(Codec::Solid),
            5 => Ok(Codec::Raw),
            6 => Ok(Codec::Cdf53),
            other => Err(ProtocolError::UnknownCodec(other)),
        }
    }
}

// ---------------------------------------------------------------------------
// DatagramHeader (12 bytes)
//   [0..4]  frame_seq:     u32 BE
//   [4..6]  frag_idx:      u16 BE
//   [6..8]  frag_total:    u16 BE
//   [8..12] timestamp_us:  u32 BE
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatagramHeader {
    pub frame_seq: u32,
    pub frag_idx: u16,
    pub frag_total: u16,
    pub timestamp_us: u32,
}

impl DatagramHeader {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.frame_seq.to_be_bytes());
        buf.extend_from_slice(&self.frag_idx.to_be_bytes());
        buf.extend_from_slice(&self.frag_total.to_be_bytes());
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
            timestamp_us: u32::from_be_bytes(data[8..12].try_into().unwrap()),
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

/// Fragments a tile payload into MTU-sized datagrams.
///
/// Each datagram = [DatagramHeader (12 B)][TileHeader (8 B)][payload_fragment].
/// Empty payload (e.g. Skip codec) → single datagram with no payload bytes.
pub fn fragment_tile(
    frame_seq: u32,
    tile_x: u8,
    tile_y: u8,
    codec: Codec,
    payload: &[u8],
    timestamp_us: u32,
    max_fragment_payload: usize,
) -> Vec<Vec<u8>> {
    // NOTE: TileHeader constructed here uses generation=0, pass=0. Sites that
    // need real generation/pass values byte-patch `dg[DATAGRAM_HEADER_SIZE + 3]`
    // after this returns (see io_bridge.rs::process_frame_cpu and
    // process_frame_gpu). If you change the byte [3] format, update both
    // patch sites.
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
                frame_seq,
                frag_idx: idx as u16,
                frag_total,
                timestamp_us,
            };
            let th = TileHeader {
                tile_x,
                tile_y,
                codec,
                lz4: false,
                generation: 0,
                pass: 0,
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
pub fn decode_tile_datagram(data: &[u8]) -> Result<(DatagramHeader, TileHeader, &[u8]), ProtocolError> {
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
            let mut buf = Vec::with_capacity(
                DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE + parity_payload.len(),
            );
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
        let short = vec![0u8; 11];
        let result = DatagramHeader::decode(&short);
        assert_eq!(
            result,
            Err(ProtocolError::TooShort {
                expected: DATAGRAM_HEADER_SIZE,
                got: 11
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
        // Raw=5, lz4=true → packed byte = (5 << 1) | 1 = 11
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
        assert_eq!(buf[2], 11u8);

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
        let datagrams = fragment_tile(1, 2, 3, Codec::Raw, &payload, 5000, 1200);
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
        let datagrams = fragment_tile(7, 0, 0, Codec::H264, &payload, 999, max_frag);

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
        let datagrams = fragment_tile(0, 0, 0, Codec::Skip, &[], 0, 1200);
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
            frame_seq: 1, frag_idx: 2, frag_total: 8, timestamp_us: 0,
        };
        assert!(!source.is_parity());

        // Parity fragment: frag_idx >= frag_total
        let parity = DatagramHeader {
            frame_seq: 1, frag_idx: 8, frag_total: 8, timestamp_us: 0,
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
        let source_dgs = fragment_tile(7, 2, 3, Codec::H264, &payload, 999, max_frag);
        let frag_total = source_dgs.len();
        assert_eq!(frag_total, 4); // ceil(4096/1200) = 4

        // Extract source payloads (bytes after headers)
        let source_payloads: Vec<&[u8]> = source_dgs.iter().map(|dg| {
            &dg[DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE..]
        }).collect();

        // Generate parity
        let parities = fec::generate_parity(&source_payloads, k);
        assert_eq!(parities.len(), 1); // 4 frags / K=4 = 1 parity group

        // Build parity datagrams using the helper
        let parity_dgs = build_parity_datagrams(
            7, 2, 3, Codec::H264, 999,
            frag_total as u16,
            &parities,
        );
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
        let (group_start, group_len, _xor_data) = fec::decode_parity_payload(parity_payload).unwrap();
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
        assert!(!is_tile_datagram(&frame_buf), "frame datagram must have bit 31 = 0");

        // Tile datagram: bit 31 of frame_seq = 1 (TILE_DATAGRAM_FLAG set)
        let dh = DatagramHeader {
            frame_seq: TILE_DATAGRAM_FLAG | 42,
            frag_idx: 0,
            frag_total: 1,
            timestamp_us: 0,
        };
        let mut tile_buf = Vec::new();
        dh.encode(&mut tile_buf);
        // Pad with a tile header's worth of zeros so is_tile_datagram only checks first 4 bytes
        tile_buf.extend_from_slice(&[0u8; TILE_HEADER_SIZE]);
        assert!(is_tile_datagram(&tile_buf), "tile datagram must have bit 31 = 1");
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
        let nack = NackMessage { frame_seq: 42, frag_idx: 3 };
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
}
