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
}

// ---------------------------------------------------------------------------
// TileHeader (8 bytes)
//   [0]    tile_x
//   [1]    tile_y
//   [2]    (codec << 1) | lz4
//   [3]    generation
//   [4..8] payload_len: u32 BE
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileHeader {
    pub tile_x: u8,
    pub tile_y: u8,
    pub codec: Codec,
    pub lz4: bool,
    pub generation: u8,
    pub payload_len: u32,
}

impl TileHeader {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(self.tile_x);
        buf.push(self.tile_y);
        buf.push(((self.codec as u8) << 1) | (self.lz4 as u8));
        buf.push(self.generation);
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
        Ok(TileHeader {
            tile_x: data[0],
            tile_y: data[1],
            codec,
            lz4,
            generation: data[3],
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
// max_fragment_payload
// ---------------------------------------------------------------------------

/// Returns the maximum tile payload bytes that fit in one datagram of `max_datagram_size`.
pub fn max_fragment_payload(max_datagram_size: usize) -> usize {
    max_datagram_size.saturating_sub(DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE)
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
            generation: 42,
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
}
