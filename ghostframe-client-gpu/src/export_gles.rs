//! A GL texture whose storage is exported as a dmabuf file descriptor.
//!
//! The GLES counterpart to `export.rs`. Same public surface — `new`,
//! `as_wgpu_texture`, `raw_fd`, `map_read`, and the
//! `width`/`height`/`modifier`/`planes` fields — because `ring.rs`,
//! `renderer.rs` and `tests/gpu_export.rs` are written against one shape and
//! do not know which backend they got. See `lib.rs`'s module wiring.
//!
//! ## Why it allocates instead of exporting
//!
//! `export.rs` allocates a `VkImage` itself because Vulkan lets you ask for
//! exportable memory at creation time and wgpu does not expose that. This
//! module allocates for a different reason, learned the hard way.
//!
//! The obvious GLES approach is the reverse: let wgpu create the texture, then
//! `eglExportDMABUFImageMESA` it. That works, and produces a buffer nothing can
//! use. On Mali-T860 / panfrost / Mesa 24.0.2 a wgpu-created texture exports
//! with modifier `0x0800000000000051`, which decodes as
//! `DRM_FORMAT_MOD_ARM_AFBC(BLOCK_SIZE_16x16 | YTR | SPARSE)` — Arm Frame
//! Buffer Compression. Not a tiling shuffle: *compression*.
//!
//! Against a real server that rendered a repeating green grid with a 16px band
//! at the top and black below — compressed payload and block headers read as
//! raw RGBA, the 16×16 block size, and SPARSE leaving most blocks unwritten.
//! Modifier negotiation did not catch it because the X11 backend offered no
//! preference, and an empty preference list means "library picks", which is
//! indistinguishable from "anything works".
//!
//! **No usage-flag combination avoids it.** Measured across
//! `RENDER_ATTACHMENT|TEXTURE_BINDING|COPY_SRC|COPY_DST`, plus
//! `STORAGE_BINDING`, plus COPY-only, plus `TEXTURE_BINDING|COPY_DST`: every
//! combination that yields a GL texture yields AFBC. (`RENDER_ATTACHMENT` alone
//! yields a renderbuffer, with no name to export at all.)
//!
//! So the buffer is allocated LINEAR through GBM and the texture built around
//! it, which `tools/hw-probe/gbmprobe.c` shows this driver supports:
//!
//! ```text
//! ABGR8888 flags=LINEAR|RENDERING   OK  planes=1 modifier=0x0 stride=2560
//! ```
//!
//! `DRM_FORMAT_ARGB8888` is what `wgpu::TextureFormat::Rgba8Unorm` maps to, so
//! the channel order matches — worth stating, because a linear buffer in the
//! wrong order renders as swapped colours rather than failing.
//!
//! The chain is: `gbm_bo_create(LINEAR|RENDERING)` →
//! `eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT)` →
//! `glEGLImageTargetTexture2DOES` → `texture_from_raw` →
//! `create_texture_from_hal`. The EGL and GL entry points are loaded through
//! `egl.get_proc_address`, which `AdapterContext::egl_instance`'s own doc says
//! handles GL functions too — so this needs no `glow` dependency for four calls.
//!
//! ## Teardown order is load-bearing
//!
//! The texture's storage *is* the `EGLImage`, which *is* the bo. So they must
//! be released in that order, and only once nothing holds the texture — and
//! `as_wgpu_texture` hands out reference-counted clones that can outlive this
//! struct, so tearing down from `Drop` here would be a use-after-free the
//! moment `ring.rs` still held one.
//!
//! [`TextureOwned`] therefore lives in the texture's own drop callback rather
//! than in this struct, making the order correct by construction instead of by
//! convention. That is also why `texture_from_raw` gets `Some(callback)` and not
//! `None`: wgpu must not free the texture out from under the chain.
//!
//! ## Untested paths
//!
//! Single-plane only. A multi-plane linear import would need the PLANE1/2
//! attribute triples, which nothing here produces — `Rgba8Unorm` is one plane.

use crate::dmabuf::{
    choose_modifier, dma_buf_sync, DMA_BUF_SYNC_END, DMA_BUF_SYNC_READ, DMA_BUF_SYNC_START,
    DRM_FORMAT_MOD_LINEAR,
};
use crate::egl_ffi::{
    EglImageFns, EglImageKhr, GlTexFns, PfnEglDestroyImageKhr, PfnGlDeleteTextures,
    EGL_DMA_BUF_PLANE0_FD_EXT, EGL_DMA_BUF_PLANE0_OFFSET_EXT, EGL_DMA_BUF_PLANE0_PITCH_EXT,
    EGL_HEIGHT, EGL_LINUX_DMA_BUF_EXT, EGL_LINUX_DRM_FOURCC_EXT, EGL_NONE_I, EGL_WIDTH, GL_LINEAR,
    GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_TEXTURE_MIN_FILTER,
};
use crate::wgpu_ctx::{EglCtx, WgpuContext};
use crate::GpuError;
use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Re-exported so `ring.rs` and external consumers reach it through
/// `export::PlaneLayout` whichever backend is active. See `export.rs`.
pub use crate::dmabuf::PlaneLayout;

/// Format every exported image uses, matching `export.rs`'s `FORMAT_WGPU`.
/// Both backends must agree: the consumer reads the buffer according to this
/// layout, and a mismatch silently reinterprets bytes.
///
/// BGRA, not RGBA, because X11/DRI3 infers a dmabuf's layout from depth and
/// bpp through Mesa's fixed table -- depth 24 / bpp 32 means XRGB8888, i.e.
/// bytes B,G,R,X -- and no depth means RGBA. Exporting RGBA put red and blue
/// the wrong way round, so a blue desktop rendered orange. `Framebuffer` stays
/// `rgba8unorm` (WebGPU has no bgra8unorm storage format, and the compute
/// shaders write to it as a storage texture); the conversion happens in the
/// export blit, which is a render pass for exactly this reason.
const FORMAT_WGPU: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

// --- EGL bits `khronos-egl` does not wrap -------------------------------
//
// `khronos-egl` covers core EGL only; KHR_image_base and MESA_image_dma_buf_
// export are extensions, so their entry points are loaded by name and called
// through transmuted function pointers. The signatures below are from
// `EGL_KHR_image_base` and `EGL_MESA_image_dma_buf_export`.

// --- libgbm, for allocating a LINEAR buffer we can render into -----------
//
// Hand-declared rather than pulling the `gbm` crate: this needs five entry
// points, and the crate brings `drm` with it. Same call this file's own
// `dmabuf.rs` sibling makes for DMA_BUF_IOCTL_SYNC -- a handful of FFI
// declarations is cheaper than a dependency in a library others link.

#[repr(C)]
struct GbmDevice {
    _opaque: [u8; 0],
}
#[repr(C)]
struct GbmBo {
    _opaque: [u8; 0],
}

#[link(name = "gbm")]
extern "C" {
    fn gbm_create_device(fd: i32) -> *mut GbmDevice;
    fn gbm_device_destroy(dev: *mut GbmDevice);
    fn gbm_bo_create(
        dev: *mut GbmDevice,
        width: u32,
        height: u32,
        format: u32,
        flags: u32,
    ) -> *mut GbmBo;
    fn gbm_bo_destroy(bo: *mut GbmBo);
    fn gbm_bo_get_fd(bo: *mut GbmBo) -> i32;
    fn gbm_bo_get_stride(bo: *mut GbmBo) -> u32;
    fn gbm_bo_get_modifier(bo: *mut GbmBo) -> u64;
}

/// `GBM_BO_USE_RENDERING` -- the buffer will be a render/copy target.
const GBM_BO_USE_RENDERING: u32 = 1 << 2;
/// `GBM_BO_USE_LINEAR` -- the whole point. Without it panfrost picks AFBC.
const GBM_BO_USE_LINEAR: u32 = 1 << 4;

/// `DRM_FORMAT_ARGB8888`, `fourcc_code('A','R','2','4')`.
///
/// This is what [`FORMAT_WGPU`] (`Bgra8Unorm`) maps to -- a 32-bit word of
/// 0xAARRGGBB, i.e. bytes B,G,R,A in memory. The two must agree: a buffer in
/// the wrong channel order renders as swapped colours rather than failing,
/// which is a much worse way to find out.
const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;

/// The render node GBM allocates from. Matches `ghostframe-client-h264`'s
/// `RENDER_NODE`; a machine with several GPUs and a client on the wrong one is
/// a problem neither has solved yet.
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// A LINEAR dmabuf, allocated through GBM, wrapped as a wgpu texture.
///
/// The buffer is allocated first and the texture built around it, rather than
/// exporting whatever wgpu allocated -- see the module doc for why that is
/// forced on this backend.
///
/// ## Lifetimes
///
/// Everything that must outlive the texture is owned by the texture's own drop
/// callback, not by this struct's `Drop`. `as_wgpu_texture` hands out
/// reference-counted clones, so a clone can outlive this value; tearing the
/// `EGLImage` or the bo down from here would be a use-after-free the moment
/// `ring.rs` still held one. Letting wgpu run the teardown when it is genuinely
/// finished with the texture makes the order correct by construction rather
/// than by convention.
pub struct ExportedImage {
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    pub planes: Vec<PlaneLayout>,
    texture: wgpu::Texture,
    fd: OwnedFd,
}

impl std::fmt::Debug for ExportedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportedImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("modifier", &self.modifier)
            .field("planes", &self.planes)
            .field("fd", &self.fd.as_raw_fd())
            .finish()
    }
}

/// Resources the texture's drop callback owns and releases, in order.
struct TextureOwned {
    delete_textures: PfnGlDeleteTextures,
    destroy_image: PfnEglDestroyImageKhr,
    display_ptr: *mut c_void,
    gl_name: u32,
    image: EglImageKhr,
    bo: *mut GbmBo,
    gbm: *mut GbmDevice,
    drm_fd: OwnedFd,
}

// SAFETY: the raw pointers are EGL/GBM handles, not Rust-visible memory, and
// are valid process-wide rather than per-thread.
//
// `Send` because wgpu may run the drop callback on a different thread than the
// one that built it, and the callback touches no `!Send` Rust state.
//
// `Sync` because `wgpu_hal::DropCallback` requires it. It is sound here for a
// stronger reason than "the handles are shareable": `release` takes `self` by
// value, so the handles can only be released once, by whichever thread owns the
// callback. There is no shared-reference path to them at all.
unsafe impl Send for TextureOwned {}
unsafe impl Sync for TextureOwned {}

impl TextureOwned {
    /// Release in the one order that is safe: GL texture, then the image whose
    /// storage it was, then the bo the image imported, then the device.
    fn release(self) {
        // SAFETY: every handle was created in `ExportedImage::new` and is
        // released exactly once, here. `glDeleteTextures` needs the GL context
        // current; wgpu runs this from its own texture cleanup, where it is.
        // Should it ever not be, the worst outcome is a leaked texture NAME --
        // not a use-after-free -- because the image and bo teardown below do
        // not depend on GL.
        unsafe {
            (self.delete_textures)(1, &self.gl_name);
            (self.destroy_image)(self.display_ptr, self.image);
            gbm_bo_destroy(self.bo);
            gbm_device_destroy(self.gbm);
        }
        drop(self.drm_fd);
    }
}

impl ExportedImage {
    /// `preferred` is the consumer's modifier list, most-preferred first;
    /// empty means "library picks". See [`crate::dmabuf::choose_modifier`].
    ///
    /// `host_visible` is accepted for signature parity with the Vulkan backend
    /// and ignored: a GBM `LINEAR` buffer is CPU-mappable either way, so
    /// [`ExportedImage::map_read`] works without asking.
    pub fn new(
        ctx: &WgpuContext,
        width: u32,
        height: u32,
        preferred: &[u64],
        _host_visible: bool,
    ) -> Result<Self, GpuError> {
        // Everything happens inside `with_raw_egl`: the allocation, the import
        // and the wrap all need the EGL/GL context, and keeping them in one
        // scope is what lets each failure path clean up what it allocated with
        // ordinary `?` instead of a hand-rolled unwind.
        ctx.with_raw_egl(|egl_ctx| Self::alloc_import_wrap(ctx, egl_ctx, width, height, preferred))
            .ok_or_else(|| {
                GpuError::Egl(
                    "wgpu is not running on the GLES backend, so there is no EGL \
                     display to import a dmabuf through"
                        .to_string(),
                )
            })?
    }

    /// Allocate a LINEAR dmabuf through GBM, import it as an `EGLImage`, bind
    /// that to a GL texture, and wrap the texture for wgpu.
    fn alloc_import_wrap(
        ctx: &WgpuContext,
        egl_ctx: &EglCtx<'_>,
        width: u32,
        height: u32,
        preferred: &[u64],
    ) -> Result<Self, GpuError> {
        let egl_fns = EglImageFns::load(egl_ctx)?;
        let gl = GlTexFns::load(egl_ctx)?;
        let dpy = egl_ctx.display.as_ptr();

        // --- 1. LINEAR dmabuf from GBM ---------------------------------
        let drm_fd = OwnedFd::from(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(RENDER_NODE)
                .map_err(|e| GpuError::Egl(format!("open {RENDER_NODE} for GBM: {e}")))?,
        );

        // SAFETY: `drm_fd` is a live render node. gbm_create_device borrows the
        // fd rather than taking it, which is why `drm_fd` is kept alive in
        // `TextureOwned` below.
        let gbm = unsafe { gbm_create_device(drm_fd.as_raw_fd()) };
        if gbm.is_null() {
            return Err(GpuError::Egl(format!(
                "gbm_create_device({RENDER_NODE}) failed"
            )));
        }
        // From here on, every early return must release what is held. There are
        // few enough paths to do it by hand, and a guard type would have to be
        // disarmed on the success path anyway since ownership moves into the
        // texture's drop callback.
        let fail = |gbm: *mut GbmDevice, bo: *mut GbmBo, e: GpuError| -> GpuError {
            // SAFETY: each handle is non-null when passed and released once.
            unsafe {
                if !bo.is_null() {
                    gbm_bo_destroy(bo);
                }
                gbm_device_destroy(gbm);
            }
            e
        };

        // SAFETY: `gbm` is live; format and flags are constants.
        let bo = unsafe {
            gbm_bo_create(
                gbm,
                width,
                height,
                DRM_FORMAT_ARGB8888,
                GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING,
            )
        };
        if bo.is_null() {
            return Err(fail(
                gbm,
                std::ptr::null_mut(),
                GpuError::Egl(format!(
                    "gbm_bo_create {width}x{height} ABGR8888 LINEAR|RENDERING failed \
                     (tools/hw-probe/gbmprobe.c reports whether this driver can)"
                )),
            ));
        }

        // SAFETY: `bo` is live. gbm_bo_get_fd returns a NEW fd the caller owns.
        let (raw_fd, stride, modifier) = unsafe {
            (
                gbm_bo_get_fd(bo),
                gbm_bo_get_stride(bo),
                gbm_bo_get_modifier(bo),
            )
        };
        if raw_fd < 0 {
            return Err(fail(
                gbm,
                bo,
                GpuError::Egl("gbm_bo_get_fd returned no fd".to_string()),
            ));
        }
        // SAFETY: a fresh fd owned by nobody else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        // The driver decides the modifier, so this is a compatibility check
        // against the consumer rather than a negotiation -- but it goes through
        // the shared helper so preference semantics cannot drift per backend.
        let supported = [modifier];
        if choose_modifier(&supported, preferred).is_none() {
            return Err(fail(
                gbm,
                bo,
                GpuError::NoCommonModifier {
                    device: supported.to_vec(),
                    requested: preferred.to_vec(),
                },
            ));
        }
        if modifier != DRM_FORMAT_MOD_LINEAR {
            // Not fatal -- a GPU consumer told this modifier can still import
            // it -- but it means GBM ignored GBM_BO_USE_LINEAR, and CPU
            // readback and linear-only consumers will not work.
            tracing::warn!(
                modifier = format!("0x{modifier:016x}"),
                "GBM honoured GBM_BO_USE_LINEAR with a non-linear modifier"
            );
        }

        // --- 2. Import it as an EGLImage -------------------------------
        //
        // EGL_NO_CONTEXT: a dmabuf import builds the image from the buffer, not
        // from a GL object, so no context participates. Modifier attributes are
        // deliberately omitted -- the buffer is linear, and passing them without
        // EGL_EXT_image_dma_buf_import_modifiers is an error, not a no-op.
        let attribs: [i32; 13] = [
            EGL_WIDTH,
            width as i32,
            EGL_HEIGHT,
            height as i32,
            EGL_LINUX_DRM_FOURCC_EXT,
            DRM_FORMAT_ARGB8888 as i32,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            fd.as_raw_fd(),
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            0,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            stride as i32,
            EGL_NONE_I,
        ];
        // SAFETY: `dpy` is live for this borrow; `attribs` is EGL_NONE-
        // terminated and describes the buffer behind `fd`. EGL does not take
        // ownership of the fd.
        let image = unsafe {
            (egl_fns.create_image)(
                dpy,
                std::ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null_mut(),
                attribs.as_ptr(),
            )
        };
        if image.is_null() {
            return Err(fail(
                gbm,
                bo,
                GpuError::Egl(format!(
                    "eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT) failed for a \
                     {width}x{height} linear ABGR8888 dmabuf, stride {stride}"
                )),
            ));
        }

        // --- 3. Bind it to a GL texture --------------------------------
        //
        // SAFETY: the GL context is current -- `with_raw_egl` holds wgpu-hal's
        // context lock for this call. `name` is written before it is read.
        let gl_name = unsafe {
            let mut name: u32 = 0;
            (gl.gen_textures)(1, &mut name);
            if name == 0 {
                (egl_fns.destroy_image)(dpy, image);
                return Err(fail(
                    gbm,
                    bo,
                    GpuError::Egl("glGenTextures returned 0".to_string()),
                ));
            }
            (gl.bind_texture)(GL_TEXTURE_2D, name);
            // An EGLImage-backed texture has no mipmaps, so the default
            // mipmap-based MIN_FILTER leaves it incomplete -- which some
            // drivers surface only as a silent no-op.
            (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            (gl.image_target_texture_2d)(GL_TEXTURE_2D, image);
            (gl.bind_texture)(GL_TEXTURE_2D, 0);
            name
        };

        // --- 4. Wrap as a wgpu texture, handing it the teardown chain ---
        let texture = Self::wrap_gl_texture(
            ctx,
            gl_name,
            width,
            height,
            TextureOwned {
                delete_textures: gl.delete_textures,
                destroy_image: egl_fns.destroy_image,
                display_ptr: dpy,
                gl_name,
                image,
                bo,
                gbm,
                drm_fd,
            },
        );

        tracing::info!(
            width,
            height,
            modifier = format!("0x{modifier:016x}"),
            stride,
            "allocated a LINEAR dmabuf via GBM and bound it as the export texture"
        );

        Ok(Self {
            width,
            height,
            modifier,
            planes: vec![PlaneLayout {
                offset: 0,
                stride: stride as u64,
            }],
            texture,
            fd,
        })
    }

    /// Wrap a GL texture name as a `wgpu::Texture`, giving `owned` to its drop
    /// callback so the teardown order is correct by construction.
    fn wrap_gl_texture(
        ctx: &WgpuContext,
        gl_name: u32,
        width: u32,
        height: u32,
        owned: TextureOwned,
    ) -> wgpu::Texture {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let name = std::num::NonZeroU32::new(gl_name).expect("checked non-zero by the caller");
        // COPY_DST because `Framebuffer::blit_full` reaches the export through
        // `copy_texture_to_texture`; COPY_SRC so a wgpu-side readback stays
        // possible. Deliberately NOT RENDER_ATTACHMENT or TEXTURE_BINDING --
        // nothing renders into or samples the export, and the Vulkan backend
        // wraps with exactly this pair.
        //
        // SAFETY: `gl_name` is a live GL texture whose storage is `owned`'s
        // EGLImage and matches `desc`. `Some(..)` rather than `None` because
        // wgpu must NOT free the texture itself -- the callback releases the
        // whole chain in the one safe order (see `TextureOwned::release`).
        let hal_texture = unsafe {
            ctx.device
                .as_hal::<wgpu_hal::api::Gles>()
                .map(|hal_device| {
                    hal_device.texture_from_raw(
                        name,
                        &wgpu_hal::TextureDescriptor {
                            label: Some("ghostframe-exported-image"),
                            size,
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: wgpu::TextureDimension::D2,
                            format: FORMAT_WGPU,
                            usage: wgpu::TextureUses::COPY_SRC | wgpu::TextureUses::COLOR_TARGET,
                            memory_flags: wgpu_hal::MemoryFlags::empty(),
                            view_formats: Vec::new(),
                        },
                        Some(Box::new(move || owned.release())),
                    )
                })
                .expect("with_raw_egl already established this is the GLES backend")
        };

        // SAFETY: `hal_texture` was just built from a live GL texture matching
        // `desc`. UNINITIALIZED is accurate -- the dmabuf holds whatever GBM
        // left there, and wgpu's tracker must be told that rather than assume.
        unsafe {
            ctx.device.create_texture_from_hal::<wgpu_hal::api::Gles>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("ghostframe-exported-image"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: FORMAT_WGPU,
                    usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                },
                wgpu::TextureUses::UNINITIALIZED,
            )
        }
    }

    pub fn raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }

    /// The wgpu texture backing this export.
    ///
    /// Unlike the Vulkan backend this does not construct anything: wgpu
    /// created the texture, so this is a reference-count bump. The `device`
    /// argument exists only to keep the signature identical across backends.
    pub fn as_wgpu_texture(&self, _device: &wgpu::Device) -> Result<wgpu::Texture, GpuError> {
        Ok(self.texture.clone())
    }

    /// `mmap` the dmabuf and read it back as row-major pixels.
    /// Diagnostic/test path only.
    ///
    /// **Refuses on a non-linear modifier, and that is the common case here.**
    /// Measured on Mali-T860 / panfrost / Mesa 24.0.2: a wgpu-created texture
    /// exports with modifier `0x0800000000000051` — vendor `0x08` is
    /// `DRM_FORMAT_MOD_VENDOR_ARM`, i.e. ARM block tiling. Its *stride* is the
    /// linear stride (256 for 64px RGBA), so a row-major read looks like it
    /// worked and returns scrambled pixels. That silent-wrong outcome is worse
    /// than an error, so the modifier is checked rather than trusted.
    ///
    /// Also more likely to fail outright than on Vulkan even when linear: see
    /// the `host_visible` note on [`ExportedImage::new`].
    pub fn map_read(&self) -> Result<Vec<u8>, GpuError> {
        if self.modifier != DRM_FORMAT_MOD_LINEAR {
            return Err(GpuError::Egl(format!(
                "cannot CPU-read an export with modifier 0x{:016x}: the buffer is \
                 tiled (vendor 0x{:02x}), so a row-major read would return \
                 plausible-looking but wrong pixels rather than failing. A GPU \
                 consumer importing this dmabuf WITH that modifier sees correct \
                 pixels; only this diagnostic path needs linear. Getting a linear \
                 export needs the buffer allocated LINEAR up front (GBM) and \
                 rendered into, rather than exported from a wgpu-created \
                 texture -- see the GLES/V4L2 design doc.",
                self.modifier,
                (self.modifier >> 56) & 0xff,
            )));
        }
        let stride = self
            .planes
            .first()
            .map(|p| p.stride)
            .filter(|s| *s > 0)
            .ok_or_else(|| {
                GpuError::Egl("exported image has no usable plane 0 stride".to_string())
            })?;
        let offset = self.planes[0].offset;
        let len = (stride * self.height as u64 + offset) as usize;
        let fd = self.fd.as_raw_fd();

        // SAFETY: `fd` is a live dmabuf fd this struct owns.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(GpuError::Io(std::io::Error::last_os_error()));
        }

        // SAFETY: `fd` is a live dmabuf fd. The START/END pair brackets CPU
        // access so the driver can flush or invalidate caches around it;
        // reading without it can return stale bytes on non-coherent hardware,
        // which is exactly the class of machine this backend targets.
        let sync = unsafe { dma_buf_sync(fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ) };
        let out = sync.map(|()| {
            // SAFETY: `ptr` is a valid mapping of `len` readable bytes.
            let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
            bytes[offset as usize..].to_vec()
        });
        // SAFETY: as above; END closes the access window opened by START.
        let _ = unsafe { dma_buf_sync(fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ) };
        // SAFETY: `ptr`/`len` are the mapping just created and not yet unmapped.
        unsafe { libc::munmap(ptr, len) };
        out
    }
}
