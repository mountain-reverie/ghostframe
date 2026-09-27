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
//! - **`v4l2`** -- `cros-codecs` over the V4L2 Request API, for a
//!   mainline-kernel ARM SoC with a stateless decoder and no VA-API driver at
//!   all. RK3399/rkvdec is the reference; see
//!   `docs/superpowers/specs/2026-09-25-native-client-gles-v4l2-design.md`.
//!
//! Both present [`decoder::H264Decoder`], [`decoder::HwFrame`],
//! [`decoder::MappedFrame`] and [`probe::h264_decode_available`] with the same
//! shapes, so `ghostframe-client-gpu`'s renderer drives one API and never asks
//! which backend it got. They are compile-time exclusive rather than a runtime
//! choice because they share no code below that surface -- the same reasoning
//! as `ghostframe-client-gpu`'s `vulkan`/`gles` split (design §4.6).
//!
//! **The `v4l2` backend needs a patched `cros-codecs`.** Upstream 0.0.6 picks
//! its V4L2 device by scanning for the first node with an OUTPUT mplane queue,
//! which on RK3399 is the hantro *encoder*; the 25-line fix is
//! `tools/hw-probe/v4l2-expbuf-rs/cros-codecs-0.0.6.patch`, and the override it
//! adds is read from `CROS_CODECS_V4L2_DEVICE`. Without both,
//! [`decoder::H264Decoder::with_device`] refuses at startup and the session runs
//! on tile codecs -- degraded, not broken. See [`v4l2_device`].

// Exactly one backend, and say so clearly rather than failing with a
// missing-module error twenty lines down.
#[cfg(all(feature = "vaapi", feature = "v4l2"))]
compile_error!(
    "ghostframe-client-h264: `vaapi` and `v4l2` are mutually exclusive; they share no code \
     below `H264Decoder`/`HwFrame` (see the crate docs). Pass \
     `--no-default-features --features v4l2` for the V4L2 backend."
);
#[cfg(not(any(feature = "vaapi", feature = "v4l2")))]
compile_error!(
    "ghostframe-client-h264: enable exactly one decode backend, `vaapi` (default) or `v4l2`."
);
// `cros-codecs 0.0.6` cannot be built off aarch64 with its `v4l2` feature on:
// `image_processing.rs:15` is `#[cfg(feature = "v4l2")] use std::arch::aarch64::*;`
// -- gated on the feature rather than the architecture -- so its MM21 NEON path
// is unconditional there. Without this the failure is
// `could not find aarch64 in arch`, from a dependency, with nothing pointing at
// the feature that asked for it.
//
// Not a limitation of this backend: the V4L2 Request API is not ARM-specific and
// neither is anything in `v4l2_frame.rs`. Fixing the gate upstream would lift it,
// and it is a candidate patch -- but CI builds cros-codecs from crates.io, so a
// carried patch could not make an x86 build work anyway. `v4l2` is checked on the
// reference machine (`just check-v4l2`) instead.
#[cfg(all(feature = "v4l2", not(target_arch = "aarch64")))]
compile_error!(
    "ghostframe-client-h264: the `v4l2` backend needs aarch64, because \
     cros-codecs 0.0.6 gates its NEON MM21 path on the `v4l2` FEATURE rather than \
     on the target architecture (image_processing.rs:15). Use the default `vaapi` \
     backend on this target."
);

#[cfg(feature = "vaapi")]
pub mod decoder;
#[cfg(feature = "v4l2")]
#[path = "decoder_v4l2.rs"]
pub mod decoder;

pub mod descriptor;

#[cfg(feature = "vaapi")]
pub mod probe;
#[cfg(feature = "v4l2")]
#[path = "probe_v4l2.rs"]
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

// The `v4l2` backend's two halves: the frames the decoder writes into, and the
// raw-ioctl device discovery that keeps the capability probe's ground truth
// independent of `cros-codecs`.
#[cfg(feature = "v4l2")]
pub mod v4l2_device;
#[cfg(feature = "v4l2")]
mod v4l2_frame;
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
// The `v4l2` backend's equivalent: hardware decode against a software golden,
// which is the only test that distinguishes a correct chroma offset from a
// nearly-correct one.
#[cfg(all(test, feature = "v4l2", feature = "test-support"))]
mod oracle_tests_v4l2;

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

    /// The V4L2 stateless decode path failed. The `v4l2` backend's counterpart
    /// to [`Self::Ffmpeg`].
    #[error("v4l2: {0}")]
    V4l2(String),

    /// No usable V4L2 stateless decoder. The `v4l2` backend's counterpart to
    /// [`Self::VaapiUnavailable`], and like it, not fatal anywhere in this
    /// client: the capability bit stays clear and the session runs on the tile
    /// codecs.
    #[error("V4L2 stateless H.264 decode unavailable: {0}")]
    V4l2Unavailable(String),
}
