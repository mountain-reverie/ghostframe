//! Importing a decoded NV12 dmabuf as GL textures. **Not yet implemented.**
//!
//! The GLES counterpart to `import.rs`, and the one piece of the GLES backend
//! deliberately left unbuilt: it has nothing to be tested against yet.
//!
//! ## Why this is a stub and not code
//!
//! `import_nv12` has exactly one caller — `renderer.rs`'s H.264 branch — and
//! that branch only runs when the session negotiated H.264, which requires a
//! working hardware decoder. On the hardware this backend exists for
//! (RK3399 / Mali-T860) that decoder is rkvdec behind the V4L2 Request API,
//! and wiring it up is Tasks 3–6 of the GLES/V4L2 plan, still outstanding.
//!
//! Until then `probe.rs` reports no H.264, the server falls back to tile
//! codecs — which is what every session did before M3 and is the dominant
//! path for desktop content — and nothing calls this. Writing the import now
//! would mean ~250 lines of unsafe EGL/GL FFI that no test could exercise,
//! which this codebase has learned to distrust: see AGENTS.md on tests that
//! pass while proving nothing.
//!
//! So it fails loudly instead. A session that somehow negotiates H.264 on this
//! backend gets a clear error naming the missing work, rather than a black
//! window or a silent fallback.
//!
//! ## What implementing it looks like
//!
//! The mechanism is settled, only untested. Per NV12 plane — luma as
//! `DRM_FORMAT_R8` at full size, chroma as `DRM_FORMAT_GR88` at half size:
//!
//! 1. `eglCreateImageKHR(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL,
//!    attribs)` where attribs carry `EGL_WIDTH`, `EGL_HEIGHT`,
//!    `EGL_LINUX_DRM_FOURCC_EXT`, `EGL_DMA_BUF_PLANE0_FD_EXT`,
//!    `_OFFSET_EXT`, `_PITCH_EXT`, and — only when
//!    [`WgpuContext::explicit_modifiers`] is set —
//!    `EGL_DMA_BUF_PLANE0_MODIFIER_{LO,HI}_EXT`. Passing modifier attributes
//!    without `EGL_EXT_image_dma_buf_import_modifiers` is an error, not a
//!    no-op, which is why that flag is tracked on the context.
//! 2. `glGenTextures` + `glBindTexture(GL_TEXTURE_2D)` +
//!    `glEGLImageTargetTexture2DOES(GL_TEXTURE_2D, image)`. That entry point is
//!    a GL extension, so it loads through `egl.get_proc_address` like the
//!    export side's MESA functions — `AdapterContext::egl_instance`'s own doc
//!    covers both.
//! 3. `wgpu_hal::gles::Device::texture_from_raw(name, &desc, drop_callback)`
//!    then `wgpu::Texture::from_hal`. The drop callback must delete the GL
//!    texture *and* `eglDestroyImageKHR` the image — unlike the export
//!    direction, here the `EGLImage` must outlive the texture, because the
//!    texture's storage *is* the image.
//! 4. The caller must `dup()` the incoming fd per plane before step 1, for the
//!    reason `DmabufPlanes`' own field doc spells out at length: the importer
//!    must not take ownership of an fd the decoder still owns.
//!
//! Two things to get right that the Vulkan path already learned:
//!
//! - **Import once per pool frame, not once per decoded frame.** The design's
//!   §6.4 has ghostframe allocating the decoder's output pool itself, so each
//!   dmabuf is known up front and can be imported at setup and looked up by
//!   index thereafter. That is strictly less work than `import.rs` does today
//!   and it removes the fd-lifetime hazard entirely.
//! - **Check the pitch.** `import.rs` carries [`crate::GpuError::PitchMismatch`]
//!   because a driver's idea of a linear image's row pitch need not match the
//!   dmabuf's, and importing anyway shears the image. GLES has the same
//!   exposure; that error variant is backend-neutral and should be reused
//!   rather than re-invented.

use crate::wgpu_ctx::WgpuContext;
use crate::GpuError;
use ghostframe_client_h264::DmabufPlanes;

/// A decoded NV12 frame as two GL textures.
///
/// Mirrors `import.rs`'s type so `renderer.rs` compiles against one shape. It
/// is uninhabited on this backend: nothing can construct it until
/// [`import_nv12`] is implemented, which is what keeps `luma`/`chroma` honest
/// rather than returning placeholder textures.
#[derive(Debug)]
pub struct ImportedNv12 {
    /// Uninhabited: `ImportedNv12` cannot be constructed on this backend.
    /// `renderer.rs` still needs the methods below to type-check, and this is
    /// what lets them be written as unreachable rather than as a lie.
    never: std::convert::Infallible,
}

impl ImportedNv12 {
    pub fn luma(&self) -> &wgpu::Texture {
        match self.never {}
    }

    pub fn chroma(&self) -> &wgpu::Texture {
        match self.never {}
    }
}

/// Import a decoded NV12 dmabuf. Always fails on the GLES backend — see the
/// module doc for why, and for what implementing it involves.
pub fn import_nv12(_ctx: &WgpuContext, _planes: &DmabufPlanes) -> Result<ImportedNv12, GpuError> {
    Err(GpuError::Egl(
        "NV12 dmabuf import is not implemented on the GLES backend yet, so \
         hardware H.264 cannot be displayed. This session should not have \
         negotiated H.264: probe.rs reports no decoder on this backend, and the \
         server falls back to tile codecs. Reaching here means that negotiation \
         is wrong, not that the frame is bad. See GLES/V4L2 plan tasks 3-6."
            .to_string(),
    ))
}
