//! Tile encoding: converts raw BGRA tile data into compressed payloads.

pub mod cdf53;
pub mod h264_vaapi;
pub mod pal_rle;
pub mod solid;

mod nal_parser;
mod vaapi_device;

use crate::transport::protocol::Codec;

/// Result of encoding a single tile.
pub struct EncodedTile {
    pub codec: Codec,
    pub payload: Vec<u8>,
}
