//! Tile encoding: converts raw BGRA tile data into compressed payloads.

pub mod h264_vaapi;
pub mod solid;

use crate::transport::protocol::Codec;

/// Result of encoding a single tile.
pub struct EncodedTile {
    pub codec: Codec,
    pub payload: Vec<u8>,
}
