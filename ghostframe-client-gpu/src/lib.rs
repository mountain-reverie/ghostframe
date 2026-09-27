//! GPU decode and dmabuf export for the native ghostframe client.
//!
//! ## Catch-all match arms
//!
//! `clippy::wildcard_enum_match_arm` is on for non-test code in this crate.
//! A `_ =>` arm that silently absorbs unknown variants has twice hidden real
//! defects here: a wrong classifier that stood for three months, and inbound
//! routing that fed every non-NACK datagram to the ACK parser. Listing the
//! variants makes adding one a compile error at every site that must decide
//! about it.
//!
//! Wildcards that fail *loudly* (`other => panic!(...)` in a test) are fine,
//! which is why the lint is scoped to `not(test)`. A wildcard that is
//! genuinely required -- a foreign enum we do not control -- takes a local
//! `#[allow]` with a reason, so every exception is a decision on the record.
#![cfg_attr(not(test), warn(clippy::wildcard_enum_match_arm))]

// Exactly one backend, selected by feature. See this crate's `Cargo.toml` for
// why the choice is compile-time rather than runtime.
#[cfg(all(feature = "vulkan", feature = "gles"))]
compile_error!(
    "ghostframe-client-gpu: features `vulkan` and `gles` are mutually exclusive. \
     The two export paths share no code -- Vulkan allocates an exportable VkImage \
     through ash, GLES calls eglExportDMABUFImageMESA through khronos-egl -- so a \
     build with both would link both sets of bindings. Pick one: \
     `--features gles --no-default-features`, or the default `vulkan`."
);
#[cfg(not(any(feature = "vulkan", feature = "gles")))]
compile_error!(
    "ghostframe-client-gpu: no backend selected. Enable `vulkan` (the default) or \
     `gles`. Building with neither would compile a crate that cannot create a \
     device at all."
);

pub mod coalesce;
pub mod config;
pub mod dirty;
// dmabuf plane layout, modifier negotiation and DMA_BUF_IOCTL_SYNC -- about the
// kernel, not about any graphics API, so shared by both backends.
pub mod dmabuf;
// `export`/`import` name one module each, whichever backend is active. The
// `#[path]` attributes let both files claim the same module name, so
// `renderer.rs`, `ring.rs` and the `tests/gpu_*.rs` suite are written against
// `crate::export::ExportedImage` without caring which one they got. Each
// backend's `ExportedImage` must therefore keep the same surface: `new`,
// `as_wgpu_texture`, `raw_fd`, `map_read`, and the `width`/`height`/`modifier`/
// `planes` fields (see `ring.rs`, which is the strictest consumer).
#[cfg(feature = "vulkan")]
#[path = "export.rs"]
pub mod export;
#[cfg(feature = "gles")]
#[path = "export_gles.rs"]
pub mod export;
pub mod framebuffer;
#[cfg(feature = "vulkan")]
#[path = "import.rs"]
pub mod import;
#[cfg(feature = "gles")]
#[path = "import_gles.rs"]
pub mod import;
// Re-exported at the crate root because the caller that needs it is
// `ghostframe-client-native`'s capability decision, which has no other business
// inside `import`. See the const's own doc: decode and import became available
// independently on the GLES/V4L2 port, so "can this build display H.264" is a
// question with two answers to combine.
pub use import::NV12_IMPORT_IMPLEMENTED;
// The CPU reference is consumed only by Task 8's oracle test (a separate
// crate unit under `tests/`, so `#[cfg(test)]` alone would not reach it --
// hence the `test-support` feature) and has no reason to exist in a release
// build otherwise. Same precedent as `ghostframe-client-h264`'s own
// `test-support` gate on `testclip` (see that crate's `Cargo.toml`), and the
// same defect this branch already fixed once for that crate in 21a0221:
// making this feature default-on would put an oracle-only module into every
// release build of every crate that forgets `default-features = false`.
#[cfg(any(test, feature = "test-support"))]
pub mod nv12_reference;
// Task 8's NV12 shader oracle. Lives here, not `tests/*.rs`, for the same
// reason `ghostframe-client-h264`'s `oracle_tests.rs` does: it needs
// `nv12_reference`, which only a `#[cfg(test)]` build of this crate's own
// unit tests can see without a feature flag or a self-referencing
// dev-dependency (both rejected -- see this module's own doc).
#[cfg(test)]
mod nv12_oracle_tests;
pub mod pipelines;
pub mod renderer;
pub mod ring;
// Needs no GPU (see the module doc): a syntax/type error in any WGSL under
// `shaders/client/` would otherwise reach `master` green, since naga only
// parses a shader at `create_shader_module` time, which CI's GPU-less
// runners never call.
#[cfg(test)]
mod shader_validation;
pub mod testdata;
pub mod wgpu_ctx;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GpuError {
    #[cfg(feature = "vulkan")]
    #[error("vulkan: {0}")]
    Vulkan(String),

    /// EGL/GLES equivalent of [`GpuError::Vulkan`]: a driver call failed with
    /// no more specific variant to carry it. Separate rather than a shared
    /// `Backend(String)` so the prefix in the message still tells a reader
    /// which stack produced it without them having to know how the binary was
    /// built.
    #[cfg(feature = "gles")]
    #[error("egl: {0}")]
    Egl(String),

    /// No usable adapter at all -- missing loader, no ICD, or a broken driver
    /// install. Distinct from [`GpuError::AdapterCannotExport`] so the message
    /// does not send someone hunting for extension support when the real
    /// problem is that the driver is not working.
    ///
    /// The message names the backend this binary was built for, because "no
    /// adapter found" on a machine that demonstrably has a working GPU is
    /// otherwise deeply confusing -- the usual cause is a client built for the
    /// wrong one.
    #[cfg_attr(
        feature = "vulkan",
        error("no Vulkan adapter found (is a Vulkan driver installed?)")
    )]
    #[cfg_attr(
        feature = "gles",
        error("no GLES adapter found (is a GL/EGL driver installed?)")
    )]
    NoAdapter,

    /// An adapter exists but cannot export memory as a file descriptor, so it
    /// can never produce a dmabuf. The extension names are spelled out because
    /// they are greppable against `vulkaninfo` / `eglinfo` output.
    #[cfg_attr(
        feature = "vulkan",
        error(
            "adapter {adapter:?} lacks VK_KHR_external_memory_fd \
             (wgpu Features::VULKAN_EXTERNAL_MEMORY_FD), so it cannot export a dmabuf"
        )
    )]
    #[cfg_attr(
        feature = "gles",
        error(
            "adapter {adapter:?} lacks EGL_MESA_image_dma_buf_export, \
             so it cannot export a dmabuf"
        )
    )]
    AdapterCannotExport { adapter: String },

    /// The device and the consumer share no DRM format modifier.
    ///
    /// Both lists are carried because modifier debugging is precisely "which
    /// set did each side offer", and an error that omits them sends the reader
    /// back to reproduce it with logging on.
    #[error(
        "no DRM format modifier supported by both this device ({device:?}) \
         and the consumer ({requested:?})"
    )]
    NoCommonModifier {
        device: Vec<u64>,
        requested: Vec<u64>,
    },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// [`import::check_pitch`]'s guard fired: the driver's linear image did
    /// not agree with the dmabuf descriptor's row pitch for one plane. The
    /// only error this module produces that a caller is meant to *handle*
    /// (fall back to the CPU copy path) rather than propagate, so it gets
    /// its own variant instead of sharing [`GpuError::Vulkan`]'s free-form
    /// string -- a caller reacting to that would otherwise have to
    /// substring-match "pitch mismatch".
    #[error(
        "{plane} plane pitch mismatch: the dmabuf says {dmabuf}, the driver's \
         linear image wants {driver}. Importing anyway would shear the image. \
         Use the CPU copy path for this frame."
    )]
    PitchMismatch {
        plane: &'static str,
        dmabuf: u64,
        driver: u64,
    },

    /// `export_buffers == 0` was requested. An export ring with no buffers
    /// can never hand `publish` a free one, so the client would connect
    /// successfully and then never show a frame -- a failure mode much
    /// harder to notice than a rejected config.
    #[error("export_buffers must be at least 1 (got 0); a client with zero export buffers can never publish a frame")]
    NoExportBuffers,
}
