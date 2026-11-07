//! Tile encoding: converts raw BGRA tile data into compressed payloads.

// Wired into the frame worker and the bridge (`io_bridge/h264_refs.rs`). What
// is still unused in a production build is the test and oracle surface of the
// syntax model.
#[allow(dead_code)]
pub(crate) mod h264;
// The PCM-patch source of the browserless harness (design T3).
#[cfg(feature = "browserless-harness")]
pub use h264::harness as h264_harness;

pub use ghostframe_protocol::codec::cdf53;
pub use ghostframe_protocol::codec::pal_rle;
pub use ghostframe_protocol::codec::solid;

use crate::transport::protocol::Codec;

/// Result of encoding a single tile.
pub struct EncodedTile {
    pub codec: Codec,
    pub payload: Vec<u8>,
}
