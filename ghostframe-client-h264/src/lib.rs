//! H.264 decode for the native client: ffmpeg + VA-API in, a dmabuf
//! description out.
//!
//! This crate owns every unsafe ffmpeg call in the client and knows nothing
//! about wgpu. The boundary is [`DmabufPlanes`], a plain struct with no
//! ffmpeg types in it. That is deliberate: it lets `ghostframe-client-gpu`
//! be tested with synthetic planes and no decoder, and lets this crate be
//! tested with no GPU surface.

pub mod decoder;
pub mod descriptor;
pub mod probe;

pub use descriptor::{DmabufPlanes, PlaneDesc};
pub use probe::vaapi_h264_decode_available;

#[derive(Debug, thiserror::Error)]
pub enum H264Error {
    #[error("ffmpeg: {0}")]
    Ffmpeg(String),

    /// VA-API could not be opened. Not fatal anywhere in this client: the
    /// capability bit stays clear and the session runs on the tile codecs.
    #[error("VA-API unavailable: {0}")]
    VaapiUnavailable(String),

    #[error("decoded frame is {got_w}x{got_h}, expected {want_w}x{want_h}")]
    SizeMismatch {
        got_w: u32,
        got_h: u32,
        want_w: u32,
        want_h: u32,
    },

    #[error("unexpected DRM descriptor: {0}")]
    Descriptor(String),
}
