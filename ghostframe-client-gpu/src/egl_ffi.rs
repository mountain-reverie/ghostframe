//! The EGL and GL entry points for moving a dmabuf in or out of a GL texture.
//!
//! Shared by [`crate::export_gles`] and [`crate::import_gles`], which use the
//! same four-call mechanism in opposite directions:
//!
//! - **export** allocates a buffer through GBM, imports it as an `EGLImage`,
//!   binds that to a texture, and hands the fd to a consumer;
//! - **import** takes a decoder's fd, makes an `EGLImage` per NV12 plane, and
//!   binds each to a texture.
//!
//! Neither links these symbols. `eglCreateImageKHR`, `eglDestroyImageKHR` and
//! `glEGLImageTargetTexture2DOES` are extensions, and `AdapterContext::egl_instance`'s
//! own doc says its `get_proc_address` resolves "GL and EGL extension
//! functions" -- so going through EGL for the core GL calls too keeps them all
//! on one mechanism, and avoids a `glow` dependency for five functions.
//!
//! **Loading cost differs sharply between the two callers.** Export happens a
//! fixed number of times at pool setup, so loading per call is free. Import
//! happens once per decoded frame, where `get_proc_address` five times per
//! frame is exactly the per-call cost this codebase has already measured
//! hurting it elsewhere -- so `import_gles` caches these on the context
//! instead. See [`WgpuContext::egl_gl_fns`](crate::wgpu_ctx::WgpuContext).

use std::ffi::c_void;

use crate::wgpu_ctx::EglCtx;
use crate::GpuError;

pub(crate) type EglImageKhr = *mut c_void;

pub(crate) type PfnEglCreateImageKhr = unsafe extern "system" fn(
    dpy: *mut c_void,
    ctx: *mut c_void,
    target: u32,
    buffer: *mut c_void,
    attrib_list: *const i32,
) -> EglImageKhr;

pub(crate) type PfnEglDestroyImageKhr =
    unsafe extern "system" fn(dpy: *mut c_void, image: EglImageKhr) -> u32;

/// The two `EGL_KHR_image_base` entry points.
///
/// `export_gles` loads these per call, which is free at pool setup;
/// `import_gles` takes them from the context cache, because it runs per frame.
pub(crate) struct EglImageFns {
    pub(crate) create_image: PfnEglCreateImageKhr,
    pub(crate) destroy_image: PfnEglDestroyImageKhr,
}

impl EglImageFns {
    pub(crate) fn load(egl_ctx: &EglCtx<'_>) -> Result<Self, GpuError> {
        // SAFETY of the transmutes: each name is transmuted to the signature
        // the extension spec gives for that exact entry point. A driver that
        // exports the name with a different signature would be non-conformant.
        // `get_proc_address` returning None is handled as an error rather than
        // unwrapped -- `WgpuContext::new` checked the extension string, but a
        // driver that advertises an extension and then fails to resolve one of
        // its functions is a real (if broken) configuration, and a clear error
        // beats a null-pointer call.
        fn get(egl_ctx: &EglCtx<'_>, name: &str) -> Result<extern "system" fn(), GpuError> {
            egl_ctx.egl.get_proc_address(name).ok_or_else(|| {
                GpuError::Egl(format!(
                    "EGL_MESA_image_dma_buf_export is advertised but {name} \
                     does not resolve"
                ))
            })
        }
        Ok(unsafe {
            Self {
                create_image: std::mem::transmute::<extern "system" fn(), PfnEglCreateImageKhr>(
                    get(egl_ctx, "eglCreateImageKHR")?,
                ),
                destroy_image: std::mem::transmute::<extern "system" fn(), PfnEglDestroyImageKhr>(
                    get(egl_ctx, "eglDestroyImageKHR")?,
                ),
            }
        })
    }
}
// --- EGL dmabuf import (EGL_EXT_image_dma_buf_import) -------------------

pub(crate) const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
pub(crate) const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
pub(crate) const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
pub(crate) const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
pub(crate) const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
pub(crate) const EGL_WIDTH: i32 = 0x3057;
pub(crate) const EGL_HEIGHT: i32 = 0x3056;
pub(crate) const EGL_NONE_I: i32 = 0x3038;

// --- the three GL calls needed to bind an EGLImage to a texture ---------

pub(crate) const GL_TEXTURE_2D: u32 = 0x0DE1;
pub(crate) const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
pub(crate) const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
pub(crate) const GL_LINEAR: i32 = 0x2601;

pub(crate) type PfnGlGenTextures = unsafe extern "system" fn(n: i32, textures: *mut u32);
pub(crate) type PfnGlBindTexture = unsafe extern "system" fn(target: u32, texture: u32);
pub(crate) type PfnGlDeleteTextures = unsafe extern "system" fn(n: i32, textures: *const u32);
pub(crate) type PfnGlTexParameteri = unsafe extern "system" fn(target: u32, pname: u32, param: i32);
pub(crate) type PfnGlEglImageTargetTexture2DOes =
    unsafe extern "system" fn(target: u32, image: EglImageKhr);

/// The GL entry points needed to bind an `EGLImage` to a texture.
///
/// Loaded through `egl.get_proc_address`, not linked: `glEGLImageTargetTexture2DOES`
/// is a GLES extension, and `AdapterContext::egl_instance`'s own doc says that
/// loader handles "GL and EGL extension functions". Going through EGL for the
/// core calls too keeps them all on one mechanism, and avoids a `glow`
/// dependency for four functions.
pub(crate) struct GlTexFns {
    pub(crate) gen_textures: PfnGlGenTextures,
    pub(crate) bind_texture: PfnGlBindTexture,
    pub(crate) delete_textures: PfnGlDeleteTextures,
    pub(crate) tex_parameteri: PfnGlTexParameteri,
    pub(crate) image_target_texture_2d: PfnGlEglImageTargetTexture2DOes,
}

impl GlTexFns {
    pub(crate) fn load(egl_ctx: &EglCtx<'_>) -> Result<Self, GpuError> {
        fn get(egl_ctx: &EglCtx<'_>, name: &str) -> Result<extern "system" fn(), GpuError> {
            egl_ctx.egl.get_proc_address(name).ok_or_else(|| {
                GpuError::Egl(format!("{name} does not resolve; cannot bind an EGLImage"))
            })
        }
        // SAFETY of the transmutes: each name is transmuted to the signature
        // the GL/GLES spec gives for that exact entry point.
        Ok(unsafe {
            Self {
                gen_textures: std::mem::transmute::<extern "system" fn(), PfnGlGenTextures>(get(
                    egl_ctx,
                    "glGenTextures",
                )?),
                bind_texture: std::mem::transmute::<extern "system" fn(), PfnGlBindTexture>(get(
                    egl_ctx,
                    "glBindTexture",
                )?),
                delete_textures: std::mem::transmute::<extern "system" fn(), PfnGlDeleteTextures>(
                    get(egl_ctx, "glDeleteTextures")?,
                ),
                tex_parameteri: std::mem::transmute::<extern "system" fn(), PfnGlTexParameteri>(
                    get(egl_ctx, "glTexParameteri")?,
                ),
                image_target_texture_2d: std::mem::transmute::<
                    extern "system" fn(),
                    PfnGlEglImageTargetTexture2DOes,
                >(get(
                    egl_ctx,
                    "glEGLImageTargetTexture2DOES",
                )?),
            }
        })
    }
}
