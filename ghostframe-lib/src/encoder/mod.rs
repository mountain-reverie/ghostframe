//! Tile encoding: converts raw BGRA tile data into compressed payloads.

pub mod h264_vaapi;

mod nal_parser;
mod vaapi_device;

pub use ghostframe_protocol::codec::cdf53;
pub use ghostframe_protocol::codec::pal_rle;
pub use ghostframe_protocol::codec::solid;

use crate::transport::protocol::Codec;

/// Result of encoding a single tile.
pub struct EncodedTile {
    pub codec: Codec,
    pub payload: Vec<u8>,
}
