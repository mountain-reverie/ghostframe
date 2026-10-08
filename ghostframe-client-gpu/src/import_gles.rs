//! Importing a decoded NV12 dmabuf as GL textures.
//!
//! The GLES counterpart to [`crate::import`]. Both turn one `DmabufPlanes`
//! into two textures — luma and interleaved chroma — that
//! `pipelines/nv12.rs` samples to convert into the framebuffer.
//!
//! Mechanically this is the same four calls [`crate::export_gles`] already
//! makes, run on a buffer somebody else allocated: `eglCreateImageKHR` with
//! `EGL_LINUX_DMA_BUF_EXT`, `glEGLImageTargetTexture2DOES` onto a fresh
//! texture name, then `wgpu_hal::gles::Device::texture_from_raw` and
//! `create_texture_from_hal`. Twice, once per plane.
//!
//! ## Two images over one fd
//!
//! NV12 is one buffer holding two planes, so both `EGLImage`s are created from
//! the same fd at different offsets:
//!
//! | plane | fourcc | size | offset / pitch |
//! | --- | --- | --- | --- |
//! | luma | `DRM_FORMAT_R8` | `width` x `height` | `planes.luma` |
//! | chroma | `DRM_FORMAT_GR88` | `chroma_width` x `chroma_height` | `planes.chroma` |
//!
//! `GR88` is two bytes per texel, so the chroma image's *width in texels* is
//! half the luma width while its *pitch in bytes* is the same — which is why
//! the pitch comes from the descriptor rather than being computed from the
//! width. Getting that backwards produces a sheared chroma plane and a picture
//! with correct luminance and smeared colour.
//!
//! ## The fd is borrowed, and that is the whole lifetime story
//!
//! Unlike Vulkan's `vkImportMemoryFdKHR`, **EGL does not take ownership of the
//! fd** — it takes its own reference to the underlying buffer. So there is no
//! `dup()` here, and no fd to close: `DmabufPlanes::fd` stays the decoder's,
//! valid for as long as the `MappedFrame` that produced it.
//!
//! What EGL *does* keep a reference to is the buffer, not a snapshot of its
//! contents. The decoder's pool recycles that buffer as soon as the frame is
//! released, and the next decoded picture is written straight into it — under a
//! texture that is still perfectly valid and now shows the wrong frame. Keeping
//! the frame alive until the draw sampling it has retired is therefore the
//! caller's job, and `renderer.rs`'s `blit_h264_frame` documents at length what
//! it does and does not guarantee there. That analysis was written for
//! amdgpu/RADV and explicitly asks a future hardware target to check the
//! implicit-fencing assumption rather than inherit it; see §"Fencing" there.
//!
//! ## Why the EGLImage outlives the texture here
//!
//! The opposite of the export direction. Exporting, the texture is just a
//! handle on a buffer GBM owns, so the image can go once the texture is bound.
//! Importing, **the texture's storage *is* the image** — destroying the image
//! while a texture references it is a use-after-free. So both live in the
//! texture's drop callback, released in the one safe order: texture first, then
//! the image whose storage it was.

use std::ffi::c_void;

use ghostframe_client_h264::{DmabufPlanes, DRM_FORMAT_GR88, DRM_FORMAT_R8};

use crate::egl_ffi::{
    EglImageFns, EglImageKhr, GlTexFns, PfnEglDestroyImageKhr, PfnGlDeleteTextures,
    EGL_DMA_BUF_PLANE0_FD_EXT, EGL_DMA_BUF_PLANE0_OFFSET_EXT, EGL_DMA_BUF_PLANE0_PITCH_EXT,
    EGL_HEIGHT, EGL_LINUX_DMA_BUF_EXT, EGL_LINUX_DRM_FOURCC_EXT, EGL_NONE_I, EGL_WIDTH, GL_LINEAR,
    GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_TEXTURE_MIN_FILTER,
};
use crate::wgpu_ctx::{EglCtx, WgpuContext};
use crate::GpuError;

/// `DRM_FORMAT_MOD_LINEAR`.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID` — "the producer declined to say".
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Whether this backend can import a decoded NV12 dmabuf.
///
/// `true` since the import below exists. `ghostframe-client-native` reads this
/// when deciding whether to advertise H.264 — decode and import became
/// available independently on this port, and advertising a codec this build can
/// decode but not display is a black window on first paint.
pub const NV12_IMPORT_IMPLEMENTED: bool = true;

/// A decoded NV12 frame as two GL textures.
///
/// Mirrors [`crate::import::ImportedNv12`] so `renderer.rs` compiles against
/// one shape. Each texture owns its `EGLImage` through its own drop callback,
/// so cloning one out of here and dropping this value is safe — the same
/// property the export path relies on.
#[derive(Debug)]
pub struct ImportedNv12 {
    luma: wgpu::Texture,
    chroma: wgpu::Texture,
}

impl ImportedNv12 {
    pub fn luma(&self) -> &wgpu::Texture {
        &self.luma
    }

    pub fn chroma(&self) -> &wgpu::Texture {
        &self.chroma
    }
}

/// What one imported plane's texture must release, and in what order.
struct PlaneOwned {
    delete_textures: PfnGlDeleteTextures,
    destroy_image: PfnEglDestroyImageKhr,
    display_ptr: *mut c_void,
    gl_name: u32,
    image: EglImageKhr,
}

// SAFETY: the raw pointers are EGL handles, not Rust-visible memory, and are
// valid process-wide rather than per-thread. `Send` because wgpu may run the
// drop callback from a different thread than the one that built it; `Sync`
// because `wgpu_hal::DropCallback` requires it, and soundly here because
// `release` takes `self` by value — the handles can only be released once, by
// whichever thread owns the callback, with no shared-reference path to them.
unsafe impl Send for PlaneOwned {}
unsafe impl Sync for PlaneOwned {}

impl PlaneOwned {
    /// Texture first, then the image whose storage it was. The reverse order is
    /// a use-after-free; see the module doc.
    fn release(self) {
        // SAFETY: both handles were created in `import_plane` and are released
        // exactly once, here. `glDeleteTextures` wants the GL context current,
        // and wgpu runs this from its own texture cleanup where it is; were it
        // ever not, the worst outcome is a leaked texture NAME, because
        // `eglDestroyImageKHR` does not depend on GL.
        unsafe {
            (self.delete_textures)(1, &self.gl_name);
            (self.destroy_image)(self.display_ptr, self.image);
        }
    }
}

/// Import a decoded NV12 dmabuf as two wgpu textures.
pub fn import_nv12(ctx: &WgpuContext, planes: &DmabufPlanes) -> Result<ImportedNv12, GpuError> {
    check_modifier(ctx, planes)?;
    check_extents(planes)?;

    ctx.with_raw_egl(|egl_ctx| {
        let egl = ctx.egl_image_fns(egl_ctx)?;
        let gl = ctx.gl_tex_fns(egl_ctx)?;

        let luma = import_plane(
            ctx,
            egl_ctx,
            egl,
            gl,
            planes,
            Plane {
                label: "ghostframe-h264-luma",
                fourcc: planes.fourcc_luma,
                width: planes.width,
                height: planes.height,
                offset: planes.luma.offset,
                pitch: planes.luma.pitch,
                format: wgpu::TextureFormat::R8Unorm,
            },
        )?;
        let chroma = import_plane(
            ctx,
            egl_ctx,
            egl,
            gl,
            planes,
            Plane {
                label: "ghostframe-h264-chroma",
                fourcc: planes.fourcc_chroma,
                // Texels, not bytes: `GR88` is two bytes per texel, so this is
                // half the luma width while the pitch below is unchanged.
                width: planes.chroma_width(),
                height: planes.chroma_height(),
                offset: planes.chroma.offset,
                pitch: planes.chroma.pitch,
                format: wgpu::TextureFormat::Rg8Unorm,
            },
        )?;
        Ok(ImportedNv12 { luma, chroma })
    })
    .ok_or_else(|| {
        GpuError::Egl(
            "wgpu is not running on the GLES backend, so there is no EGL display to \
             import a dmabuf through"
                .to_string(),
        )
    })?
}

/// Everything one plane's import needs that differs between the two.
struct Plane {
    label: &'static str,
    fourcc: u32,
    width: u32,
    height: u32,
    offset: u64,
    pitch: u64,
    format: wgpu::TextureFormat,
}

fn import_plane(
    ctx: &WgpuContext,
    egl_ctx: &EglCtx<'_>,
    egl: &EglImageFns,
    gl: &GlTexFns,
    planes: &DmabufPlanes,
    plane: Plane,
) -> Result<wgpu::Texture, GpuError> {
    let dpy = egl_ctx.display.as_ptr();

    // `EGL_NO_CONTEXT`: the image is created from a dmabuf, not from a GL
    // object, so no context participates. Modifier attributes are omitted
    // deliberately -- `check_modifier` has already established the buffer is
    // linear, and passing them without `EGL_EXT_image_dma_buf_import_modifiers`
    // is an error rather than a no-op.
    let attribs: [i32; 13] = [
        EGL_WIDTH,
        plane.width as i32,
        EGL_HEIGHT,
        plane.height as i32,
        EGL_LINUX_DRM_FOURCC_EXT,
        plane.fourcc as i32,
        EGL_DMA_BUF_PLANE0_FD_EXT,
        planes.fd,
        EGL_DMA_BUF_PLANE0_OFFSET_EXT,
        plane.offset as i32,
        EGL_DMA_BUF_PLANE0_PITCH_EXT,
        plane.pitch as i32,
        EGL_NONE_I,
    ];

    // SAFETY: `dpy` is live for this borrow; `attribs` is EGL_NONE-terminated
    // and describes a region of the buffer behind `planes.fd`, which the caller
    // keeps open for the duration. EGL takes its own reference and does not
    // take the fd.
    let image = unsafe {
        (egl.create_image)(
            dpy,
            std::ptr::null_mut(),
            EGL_LINUX_DMA_BUF_EXT,
            std::ptr::null_mut(),
            attribs.as_ptr(),
        )
    };
    if image.is_null() {
        return Err(GpuError::Egl(format!(
            "eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT) failed for the {} plane: \
             {}x{} fourcc 0x{:08x}, offset {}, pitch {}",
            plane.label, plane.width, plane.height, plane.fourcc, plane.offset, plane.pitch
        )));
    }

    // SAFETY: the GL context is current -- `with_raw_egl` holds wgpu-hal's
    // context lock across this whole call. `name` is written before it is read.
    let gl_name = unsafe {
        let mut name: u32 = 0;
        (gl.gen_textures)(1, &mut name);
        if name == 0 {
            (egl.destroy_image)(dpy, image);
            return Err(GpuError::Egl(format!(
                "glGenTextures returned 0 for the {} plane",
                plane.label
            )));
        }
        (gl.bind_texture)(GL_TEXTURE_2D, name);
        // GL_LINEAR on both: `nv12.wgsl` samples chroma at half resolution, so
        // the filter is doing real work rather than being a default. NEAREST
        // here would pixelate colour on any non-integer chroma sample.
        (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        (gl.image_target_texture_2d)(GL_TEXTURE_2D, image);
        name
    };

    let owned = PlaneOwned {
        delete_textures: gl.delete_textures,
        destroy_image: egl.destroy_image,
        display_ptr: dpy,
        gl_name,
        image,
    };

    Ok(wrap_texture(ctx, &plane, gl_name, owned))
}

fn wrap_texture(
    ctx: &WgpuContext,
    plane: &Plane,
    gl_name: u32,
    owned: PlaneOwned,
) -> wgpu::Texture {
    let size = wgpu::Extent3d {
        width: plane.width,
        height: plane.height,
        depth_or_array_layers: 1,
    };
    let name = std::num::NonZeroU32::new(gl_name).expect("checked non-zero above");

    // SAFETY: `gl_name` is a live GL texture whose storage is `owned`'s
    // EGLImage and matches `desc`. `Some(callback)` rather than `None` because
    // wgpu must NOT free the texture itself -- the callback releases the
    // texture and then the image, which is the only safe order.
    let hal_texture = unsafe {
        ctx.device
            .as_hal::<wgpu_hal::api::Gles>()
            .map(|hal_device| {
                hal_device.texture_from_raw(
                    name,
                    &wgpu_hal::TextureDescriptor {
                        label: Some(plane.label),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: plane.format,
                        // RESOURCE because the shader samples these; COPY_SRC
                        // for parity with the Vulkan import, where it is what
                        // lets `gpu_import`'s oracle read the planes back and
                        // prove they landed at the descriptor's offsets rather
                        // than merely having the right dimensions. Nothing
                        // writes to them either way -- the decoder owns the
                        // pixels.
                        usage: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC,
                        memory_flags: wgpu_hal::MemoryFlags::empty(),
                        view_formats: Vec::new(),
                    },
                    Some(Box::new(move || owned.release())),
                )
            })
            .expect("with_raw_egl already established this is the GLES backend")
    };

    // SAFETY: `hal_texture` was just built from a live GL texture matching
    // `desc`. The initial state is `RESOURCE` rather than `UNINITIALIZED`
    // because the decoder has already written the pixels -- telling wgpu the
    // texture is uninitialised would license it to clear what we came to read.
    unsafe {
        ctx.device.create_texture_from_hal::<wgpu_hal::api::Gles>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some(plane.label),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: plane.format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            },
            wgpu::TextureUses::RESOURCE,
        )
    }
}

/// Refuse a tiled buffer rather than importing it as if it were linear.
///
/// Mirrors [`crate::import`]'s reasoning, with one difference worth noting: on
/// this hardware the producer *does* report the modifier. GStreamer's
/// `drm-format` comes back as `NV12:0x0` from the decoder itself, so `LINEAR`
/// here is an answer rather than the inference the VA-API path had to make.
/// `INVALID` is still tolerated for producers that decline to say.
fn check_modifier(ctx: &WgpuContext, planes: &DmabufPlanes) -> Result<(), GpuError> {
    match planes.modifier {
        DRM_FORMAT_MOD_LINEAR => Ok(()),
        DRM_FORMAT_MOD_INVALID => {
            tracing::debug!("dmabuf reports no modifier; importing as linear");
            Ok(())
        }
        m if !ctx.explicit_modifiers => Err(GpuError::Egl(format!(
            "dmabuf has modifier 0x{m:016x} but this driver lacks \
             EGL_EXT_image_dma_buf_import_modifiers, so a tiled layout cannot be \
             described to it"
        ))),
        m => Err(GpuError::Egl(format!(
            "dmabuf has modifier 0x{m:016x}; importing a tiled NV12 buffer is not \
             implemented -- the decoder is expected to produce LINEAR"
        ))),
    }
}

/// Both planes must lie inside the buffer they claim to be part of.
///
/// `DmabufPlanes` is also built by hand in tests, and an importer that trusts
/// `offset`/`pitch` without checking them hands EGL a region past the end of
/// the allocation. EGL may or may not notice.
fn check_extents(planes: &DmabufPlanes) -> Result<(), GpuError> {
    // The descriptor's own fourccs are passed straight to EGL, so they must be
    // the per-plane pair and not the composed `DRM_FORMAT_NV12`. `DmabufPlanes`
    // normalises both accepted shapes to R8 + GR88, so this should be
    // unreachable -- but a composed fourcc here would describe plane 0 as a
    // whole two-plane image, which EGL would accept and sample wrongly rather
    // than reject.
    if planes.fourcc_luma != DRM_FORMAT_R8 || planes.fourcc_chroma != DRM_FORMAT_GR88 {
        return Err(GpuError::Egl(format!(
            "expected per-plane fourccs R8 (0x{DRM_FORMAT_R8:08x}) and GR88 \
             (0x{DRM_FORMAT_GR88:08x}), got 0x{:08x} and 0x{:08x}",
            planes.fourcc_luma, planes.fourcc_chroma
        )));
    }
    for (label, desc, rows) in [
        ("luma", planes.luma, u64::from(planes.height)),
        ("chroma", planes.chroma, u64::from(planes.chroma_height())),
    ] {
        let bytes = desc
            .pitch
            .checked_mul(rows)
            .ok_or_else(|| GpuError::Egl(format!("{label} pitch * rows overflows")))?;
        let end = desc
            .offset
            .checked_add(bytes)
            .ok_or_else(|| GpuError::Egl(format!("{label} offset + extent overflows")))?;
        if end > planes.size {
            return Err(GpuError::Egl(format!(
                "{label} plane extends to byte {end}, past the dmabuf's {} bytes",
                planes.size
            )));
        }
    }
    if planes.luma.pitch < u64::from(planes.width) {
        return Err(GpuError::Egl(format!(
            "luma pitch {} is narrower than the frame's {} pixels",
            planes.luma.pitch, planes.width
        )));
    }
    Ok(())
}
