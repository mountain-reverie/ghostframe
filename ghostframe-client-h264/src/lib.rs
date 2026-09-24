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
// `cfg(test)` covers this crate's own unit tests (decoder::tests and
// oracle_tests both use `gradient_clip`) without a self-referencing
// dev-dependency on the `test-support` feature -- that idiom compiled the
// lib twice under `--all-targets` and let `crate::H264Error` and
// `ghostframe_client_h264::H264Error` collide as distinct types.
//
// The `test-support` feature stays for everyone else: other crates'
// integration tests (`ghostframe-client-gpu`, `ghostframe-e2e`) need
// `gradient_clip` from a different crate unit's test build, where
// `#[cfg(test)]` on the defining crate would never be active. A cargo
// feature is the mechanism that reaches across the crate-unit boundary
// while still keeping nine panicking paths out of THIS crate's own release
// build by default -- see `Cargo.toml`, where `test-support` is off by
// default for exactly that reason.
#[cfg(any(test, feature = "test-support"))]
pub mod testclip;
// The hw-vs-sw decode oracle and the dmabuf-linearity check (spec §7.2).
// Lives in `src/`, not `tests/*.rs`: it tests this crate's own behaviour,
// and a `#[cfg(test)]` module in `src/` can see `testclip` and
// `probe::vainfo_reports_h264_vld` directly with no feature and no
// self-referencing dev-dependency, unlike a separate `tests/*.rs` crate
// unit.
#[cfg(test)]
mod oracle_tests;

pub use descriptor::{DmabufPlanes, PlaneDesc, DRM_FORMAT_GR88, DRM_FORMAT_NV12, DRM_FORMAT_R8};
pub use probe::vaapi_h264_decode_available;
// Re-exported so other crates' own test builds can reach the independent
// ground truth every oracle gates on, without reaching through `probe::`.
// See probe.rs for why this must NOT be `vaapi_h264_decode_available`
// itself.
#[cfg(any(test, feature = "test-support"))]
pub use probe::vainfo_reports_h264_vld;

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
