//! H.264 decode for the native client: access units in, a dmabuf description
//! out.
//!
//! This crate owns every unsafe decoder call in the client and knows nothing
//! about wgpu. The boundary is [`DmabufPlanes`], a plain struct with no
//! backend types in it. That is deliberate: it lets `ghostframe-client-gpu`
//! be tested with synthetic planes and no decoder, and lets this crate be
//! tested with no GPU surface.
//!
//! # Two backends, one API
//!
//! Exactly one is compiled in, chosen by feature:
//!
//! - **`vaapi`** (default) -- ffmpeg + VA-API. What every x86 host has, and
//!   the only one CI can run.
//! - **`gstreamer-backend`** -- GStreamer, for a machine with no VA-API driver
//!   at all. On the reference hardware (RK3399/rkvdec) that resolves to
//!   `v4l2slh264dec` over the V4L2 Request API, but nothing here says so:
//!   GStreamer picks the element and the device. See
//!   `docs/superpowers/specs/2026-09-25-native-client-gles-v4l2-design.md`.
//!
//! Both present [`decoder::H264Decoder`], [`decoder::HwFrame`],
//! [`decoder::MappedFrame`] and [`probe::h264_decode_available`] with the same
//! shapes, so `ghostframe-client-gpu`'s renderer drives one API and never asks
//! which backend it got. They are compile-time exclusive rather than a runtime
//! choice because they share no code below that surface -- the same reasoning
//! as `ghostframe-client-gpu`'s `vulkan`/`gles` split (design §4.6).
//!
//! **The `gstreamer-backend` needs GStreamer >= 1.24.1**, which is where
//! `v4l2codecs` gained DMA_DRM caps -- the only way to *require* a decoded
//! dmabuf rather than hope for one. Below that floor the decoder silently hands
//! back system memory at some resolutions, which is a full-frame CPU copy per
//! frame and the exact cost the zero-copy design exists to avoid. The `v1_24`
//! features on the gstreamer crates make the floor a link-time fact, and
//! `probe::MIN_GSTREAMER` states it in a log line.
//!
//! This replaced a `cros-codecs` backend carrying five patches against a crate
//! dormant since March 2025. The patches are gone, along with the device
//! selection, frame pooling and plane-offset arithmetic they existed to fix --
//! GStreamer reports the layout instead.

// Exactly one backend, and say so clearly rather than failing with a
// missing-module error twenty lines down.
#[cfg(all(feature = "vaapi", feature = "gstreamer-backend"))]
compile_error!(
    "ghostframe-client-h264: `vaapi` and `gstreamer-backend` are mutually exclusive; they \
     share no code below `H264Decoder`/`HwFrame` (see the crate docs). Pass \
     `--no-default-features --features gstreamer-backend` for the GStreamer backend."
);
#[cfg(not(any(feature = "vaapi", feature = "gstreamer-backend")))]
compile_error!(
    "ghostframe-client-h264: enable exactly one decode backend, `vaapi` (default) or \
     `gstreamer-backend`."
);
#[cfg(feature = "vaapi")]
pub mod decoder;
#[cfg(feature = "gstreamer-backend")]
#[path = "decoder_gst.rs"]
pub mod decoder;

pub mod descriptor;

#[cfg(feature = "vaapi")]
pub mod probe;
#[cfg(feature = "gstreamer-backend")]
#[path = "probe_gst.rs"]
pub mod probe;

// Wherever libavcodec is linked -- the `vaapi` backend, and a `v4l2` build with
// `test-support`, where ffmpeg is the software golden and libx264 the clip
// generator.
#[cfg(any(feature = "vaapi", feature = "test-support"))]
mod ffmpeg_log;

// The oracles' golden: libavcodec's software decoder. `pub` under
// `test-support` for the same reason `testclip` is -- another crate's tests
// comparing hardware output against a software decode should use this one
// rather than write a second, subtly different one -- and reachable from this
// crate's own `#[cfg(test)]` oracles with no feature at all.
#[cfg(any(feature = "test-support", all(test, feature = "vaapi")))]
pub mod software_decode;

// Raw-ioctl device discovery, kept so the capability probe's ground truth stays
// independent of the thing it validates. This is all that survived the move off
// cros-codecs: GStreamer owns device selection, buffer pooling and the plane
// layout now, so the frame type and the decoder plumbing went with it.
#[cfg(feature = "gstreamer-backend")]
pub mod v4l2_device;
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
// Needs ffmpeg (libx264), so "are we testing" is not the only condition: a
// `v4l2` build without `test-support` links no ffmpeg at all, and `cfg(test)`
// alone would try to compile this against a crate that is not there.
#[cfg(any(feature = "test-support", all(test, feature = "vaapi")))]
pub mod testclip;
// The hw-vs-sw decode oracle and the dmabuf-linearity check (spec §7.2).
// Lives in `src/`, not `tests/*.rs`: it tests this crate's own behaviour,
// and a `#[cfg(test)]` module in `src/` can see `testclip` and
// `probe::vainfo_reports_h264_vld` directly with no feature and no
// self-referencing dev-dependency, unlike a separate `tests/*.rs` crate
// unit.
#[cfg(all(test, feature = "vaapi"))]
mod oracle_tests;
// The GStreamer backend's equivalent: hardware decode against a software
// golden, which remains the only test that distinguishes a correct plane layout
// from a nearly-correct one -- GStreamer reports the layout now, but using it
// wrongly is still possible and still produces plausible output.
#[cfg(all(test, feature = "gstreamer-backend", feature = "test-support"))]
mod oracle_tests_gst;

pub use descriptor::{DmabufPlanes, PlaneDesc, DRM_FORMAT_GR88, DRM_FORMAT_NV12, DRM_FORMAT_R8};
pub use probe::h264_decode_available;
// Re-exported so other crates' own test builds can reach the independent
// ground truth every oracle gates on, without reaching through `probe::`.
// See probe.rs for why this must NOT be `h264_decode_available` itself.
//
// `driver_reports_h264_decode` is the backend-neutral name -- `vainfo` on
// VA-API, `VIDIOC_ENUM_FMT` on V4L2 -- so a test in another crate can gate on
// "is there a hardware H.264 decoder here" without knowing which backend it was
// built against.
#[cfg(any(test, feature = "test-support"))]
pub use probe::driver_reports_h264_decode;

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

    /// The GStreamer decode path failed. The `gstreamer-backend` counterpart to
    /// [`Self::Ffmpeg`], and like it, not fatal anywhere in this client: the
    /// capability bit stays clear and the session runs on the tile codecs.
    ///
    /// One variant rather than the two the previous backend had. The split there
    /// was "the device could not be opened" against "decoding failed", and it
    /// mattered because device selection was ours to get wrong. GStreamer
    /// selects the device, so nearly everything that fails now fails in
    /// negotiation, and splitting it would be a distinction without a caller.
    #[error("gstreamer: {0}")]
    Gst(String),
}
