// SPDX-License-Identifier: MIT OR Apache-2.0
//! One upload slab on the GLES backend: a LINEAR GBM buffer the CPU writes
//! through a persistent mapping and the GPU samples as a texture.
//!
//! The chain is `export_gles.rs`'s, reused: `gbm_bo_create(LINEAR|RENDERING)`
//! -> `eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT)` ->
//! `glEGLImageTargetTexture2DOES` -> `texture_from_raw` ->
//! `create_texture_from_hal`. Read that module's doc for why the buffer must be
//! allocated rather than exported (a wgpu-created texture is AFBC-compressed
//! on panfrost, and nothing can write compressed memory from the CPU), and for
//! the teardown order, which [`TextureOwned`] carries over unchanged.
//!
//! What differs:
//!
//! - **Format.** `DRM_FORMAT_ABGR8888` is bytes R,G,B,A in memory, which is
//!   `Rgba8Unorm`. The export uses ARGB8888/`Bgra8Unorm` because X11 reads it
//!   that way; a slab is only ever read by our own shader, so the tiles'
//!   native byte order goes in and no swizzle exists anywhere.
//! - **A CPU mapping.** The dmabuf is `mmap`ed once, here, and the mapping
//!   outlives the texture (see [`SlabMap`]). On panfrost a dmabuf mapping is
//!   write-combined, which is why the decoder writes whole 128-byte rows.
//! - **The texture is sampled, not rendered to.** Usage is `TEXTURE_BINDING`
//!   and the initial state `RESOURCE`, as in `import_gles.rs`. (wgpu-core does
//!   not clear a texture made by `create_texture_from_hal`, so this is about
//!   telling its usage tracker the truth, not about protecting the contents.)
//!
//! Whether panfrost samples this directly or converts it behind our back is
//! measured, not assumed: see `tests/upload_bench.rs` and the
//! `PAN_MESA_DEBUG=perf` notes in the commit that introduced this file.

use crate::dmabuf::{
    cpu_access_hooks_are_noops, dma_buf_sync, drm_driver_name, DMA_BUF_SYNC_END,
    DMA_BUF_SYNC_START, DMA_BUF_SYNC_WRITE, DRM_FORMAT_MOD_LINEAR,
};
use crate::egl_ffi::{
    EglImageFns, GlTexFns, EGL_DMA_BUF_PLANE0_FD_EXT, EGL_DMA_BUF_PLANE0_OFFSET_EXT,
    EGL_DMA_BUF_PLANE0_PITCH_EXT, EGL_HEIGHT, EGL_LINUX_DMA_BUF_EXT, EGL_LINUX_DRM_FOURCC_EXT,
    EGL_NONE_I, EGL_WIDTH, GL_LINEAR, GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_TEXTURE_MIN_FILTER,
};
use crate::egl_ffi::{EglImageKhr, PfnEglDestroyImageKhr, PfnGlDeleteTextures};
use crate::export::{
    gbm_bo_create, gbm_bo_destroy, gbm_bo_get_fd, gbm_bo_get_modifier, gbm_bo_get_stride,
    gbm_create_device, gbm_device_destroy, GbmBo, GbmDevice, GBM_BO_USE_LINEAR,
    GBM_BO_USE_RENDERING, RENDER_NODE,
};
use crate::wgpu_ctx::{EglCtx, WgpuContext};
use crate::GpuError;
use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;

/// `DRM_FORMAT_ABGR8888`, `fourcc_code('A','B','2','4')`: bytes R,G,B,A.
const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241;

/// The pixel format of a slab, as wgpu names it. Must be the same bytes as
/// [`DRM_FORMAT_ABGR8888`].
pub(crate) const SLAB_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// The CPU's view of a slab.
///
/// Owned by an `Arc` that the decode thread's allocator and the pool both
/// hold, so the mapping lives until the last of them is gone. It does not
/// depend on the texture: the mapping holds its own reference to the dmabuf,
/// so destroying the bo and EGLImage underneath it (the texture's drop
/// callback does) leaves the mapped pages valid. A decode thread that outlives
/// the renderer therefore writes to memory nobody reads, never to freed memory.
pub(crate) struct SlabMap {
    ptr: *mut u8,
    len: usize,
    stride: usize,
    fd: OwnedFd,
}

// SAFETY: `ptr` is a `MAP_SHARED` mapping owned by this value and unmapped
// once, in `Drop`. The mapping is plain memory that any thread may address;
// who writes which bytes when is the pool's slot protocol, not this type's.
unsafe impl Send for SlabMap {}
unsafe impl Sync for SlabMap {}

impl SlabMap {
    /// First byte of the slab. Row `y` starts at `y * stride()`.
    pub(crate) fn base(&self) -> *mut u8 {
        self.ptr
    }

    /// Bytes between rows, as the driver allocated them.
    pub(crate) fn stride(&self) -> usize {
        self.stride
    }

    /// Mapped length: `stride() * height`.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Open a CPU write window (`DMA_BUF_IOCTL_SYNC` START|WRITE). Writes made
    /// before the matching [`SlabMap::end_cpu_write`] are what the GPU sees
    /// after it.
    pub(crate) fn begin_cpu_write(&self) -> Result<(), GpuError> {
        // SAFETY: `fd` is the live dmabuf this value maps.
        unsafe { dma_buf_sync(self.fd.as_raw_fd(), DMA_BUF_SYNC_START | DMA_BUF_SYNC_WRITE) }
    }

    /// Close the window opened by [`SlabMap::begin_cpu_write`].
    pub(crate) fn end_cpu_write(&self) -> Result<(), GpuError> {
        // SAFETY: as in `begin_cpu_write`.
        unsafe { dma_buf_sync(self.fd.as_raw_fd(), DMA_BUF_SYNC_END | DMA_BUF_SYNC_WRITE) }
    }
}

impl Drop for SlabMap {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are the mapping `alloc_import_wrap` created and
        // nothing has unmapped; `fd` closes itself afterwards.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// The render node and GBM device every slab of a pool is allocated from.
/// Shared by an `Arc` so each slab's texture teardown can outlive the pool and
/// the device is destroyed once, after the last bo.
struct GbmShared {
    gbm: *mut GbmDevice,
    /// gbm borrows this fd, so it must outlive the device; field order drops
    /// it after `Drop::drop` has destroyed the device.
    drm_fd: OwnedFd,
}

// SAFETY: `gbm` is an opaque libgbm handle used only to create bos (from the
// render thread) and destroyed once, in `Drop`; libgbm's device is usable from
// any thread for these calls.
unsafe impl Send for GbmShared {}
unsafe impl Sync for GbmShared {}

impl Drop for GbmShared {
    fn drop(&mut self) {
        // SAFETY: `gbm` was created by `SlabSource::new` and is destroyed only
        // here, after every bo made from it (each holds an `Arc` to this).
        unsafe { gbm_device_destroy(self.gbm) };
    }
}

/// What one slab's texture releases when wgpu is done with it, in the one safe
/// order: GL texture, the image whose storage it was, the bo the image
/// imported, then (last `Arc`) the device. Like `export_gles::TextureOwned`
/// but sharing the GBM device.
struct SlabOwned {
    delete_textures: PfnGlDeleteTextures,
    destroy_image: PfnEglDestroyImageKhr,
    display_ptr: *mut c_void,
    gl_name: u32,
    image: EglImageKhr,
    bo: *mut GbmBo,
    _gbm: Arc<GbmShared>,
}

// SAFETY: as `TextureOwned`: EGL/GBM handles valid process-wide, released once
// by whoever owns the drop callback; `Sync` because the callback type demands
// it and `release` takes `self`, leaving no shared-reference path.
unsafe impl Send for SlabOwned {}
unsafe impl Sync for SlabOwned {}

impl SlabOwned {
    fn release(self) {
        // SAFETY: every handle was created in `alloc_import_wrap` and is
        // released exactly once, here. `glDeleteTextures` wants the GL context
        // current; wgpu runs this from its texture cleanup, where it is, and
        // if not the worst case is a leaked texture name.
        unsafe {
            (self.delete_textures)(1, &self.gl_name);
            (self.destroy_image)(self.display_ptr, self.image);
            gbm_bo_destroy(self.bo);
        }
    }
}

/// Where a pool's slabs come from: one render node and GBM device.
pub(crate) struct SlabSource {
    gbm: Arc<GbmShared>,
}

impl SlabSource {
    pub(crate) fn new() -> Result<Self, GpuError> {
        let drm_fd = OwnedFd::from(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(RENDER_NODE)
                .map_err(|e| GpuError::Egl(format!("open {RENDER_NODE} for GBM: {e}")))?,
        );
        // SAFETY: `drm_fd` is a live render node; gbm borrows it and
        // `GbmShared` keeps it open for the device's life.
        let gbm = unsafe { gbm_create_device(drm_fd.as_raw_fd()) };
        if gbm.is_null() {
            return Err(GpuError::Egl(format!(
                "gbm_create_device({RENDER_NODE}) failed"
            )));
        }
        Ok(SlabSource {
            gbm: Arc::new(GbmShared { gbm, drm_fd }),
        })
    }

    /// Whether writes through this source's slabs must be bracketed with
    /// `DMA_BUF_IOCTL_SYNC`. `driver` replaces the detected kernel driver
    /// name (tests; see [`crate::dmabuf::cpu_access_hooks_are_noops`]).
    pub(crate) fn needs_cpu_sync(&self, driver: Option<&str>) -> bool {
        let detected = driver
            .is_none()
            .then(|| drm_driver_name(self.gbm.drm_fd.as_raw_fd()))
            .flatten();
        !cpu_access_hooks_are_noops(driver.or(detected.as_deref()))
    }

    pub(crate) fn alloc(
        &self,
        ctx: &WgpuContext,
        width: u32,
        height: u32,
    ) -> Result<SlabAlloc, GpuError> {
        ctx.with_raw_egl(|egl_ctx| self.alloc_import_wrap(ctx, egl_ctx, width, height))
            .ok_or_else(|| {
                GpuError::Egl(
                    "wgpu is not running on the GLES backend, so there is no EGL \
                     display to import a dmabuf through"
                        .to_string(),
                )
            })?
    }
}

/// A slab's GPU texture and the CPU mapping of the same memory.
pub(crate) struct SlabAlloc {
    pub(crate) texture: wgpu::Texture,
    pub(crate) map: SlabMap,
}

impl SlabSource {
    fn alloc_import_wrap(
        &self,
        ctx: &WgpuContext,
        egl_ctx: &EglCtx<'_>,
        width: u32,
        height: u32,
    ) -> Result<SlabAlloc, GpuError> {
        let egl_fns = EglImageFns::load(egl_ctx)?;
        let gl = GlTexFns::load(egl_ctx)?;
        let dpy = egl_ctx.display.as_ptr();

        let gbm = self.gbm.gbm;
        let fail = |bo: *mut GbmBo, e: GpuError| -> GpuError {
            if !bo.is_null() {
                // SAFETY: `bo` is non-null and released once.
                unsafe { gbm_bo_destroy(bo) };
            }
            e
        };

        // SAFETY: `gbm` is live; format and flags are constants.
        let bo = unsafe {
            gbm_bo_create(
                gbm,
                width,
                height,
                DRM_FORMAT_ABGR8888,
                GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING,
            )
        };
        if bo.is_null() {
            return Err(fail(
                std::ptr::null_mut(),
                GpuError::Egl(format!(
                    "gbm_bo_create {width}x{height} ABGR8888 LINEAR|RENDERING failed"
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
                bo,
                GpuError::Egl("gbm_bo_get_fd returned no fd".to_string()),
            ));
        }
        // SAFETY: a fresh fd owned by nobody else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        if modifier != DRM_FORMAT_MOD_LINEAR {
            // A tiled or compressed slab cannot be written row by row.
            return Err(fail(
                bo,
                GpuError::Egl(format!(
                    "GBM gave modifier 0x{modifier:016x} for a LINEAR slab; \
                     the CPU cannot write it"
                )),
            ));
        }

        let len = stride as usize * height as usize;
        // SAFETY: `fd` is a live dmabuf at least `len` bytes long (stride *
        // height as GBM reports them). The mapping is unmapped by `SlabMap`.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(fail(bo, GpuError::Io(std::io::Error::last_os_error())));
        }
        let map = SlabMap {
            ptr: ptr.cast(),
            len,
            stride: stride as usize,
            fd,
        };

        // Import as an EGLImage; see `export_gles.rs` for why the modifier
        // attributes are omitted.
        let attribs: [i32; 13] = [
            EGL_WIDTH,
            width as i32,
            EGL_HEIGHT,
            height as i32,
            EGL_LINUX_DRM_FOURCC_EXT,
            DRM_FORMAT_ABGR8888 as i32,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            map.fd.as_raw_fd(),
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            0,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            stride as i32,
            EGL_NONE_I,
        ];
        // SAFETY: `dpy` is live for this borrow; `attribs` is EGL_NONE-
        // terminated and describes the buffer behind `map.fd`. EGL does not
        // take ownership of the fd.
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
                bo,
                GpuError::Egl(format!(
                    "eglCreateImageKHR failed for a {width}x{height} linear \
                     ABGR8888 slab, stride {stride}"
                )),
            ));
        }

        // SAFETY: the GL context is current -- `with_raw_egl` holds wgpu-hal's
        // context lock for this call. `name` is written before it is read.
        let gl_name = unsafe {
            let mut name: u32 = 0;
            (gl.gen_textures)(1, &mut name);
            if name == 0 {
                (egl_fns.destroy_image)(dpy, image);
                return Err(fail(
                    bo,
                    GpuError::Egl("glGenTextures returned 0".to_string()),
                ));
            }
            (gl.bind_texture)(GL_TEXTURE_2D, name);
            (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            (gl.image_target_texture_2d)(GL_TEXTURE_2D, image);
            (gl.bind_texture)(GL_TEXTURE_2D, 0);
            name
        };

        let owned = SlabOwned {
            delete_textures: gl.delete_textures,
            destroy_image: egl_fns.destroy_image,
            display_ptr: dpy,
            gl_name,
            image,
            bo,
            _gbm: Arc::clone(&self.gbm),
        };
        let texture = Self::wrap_gl_texture(ctx, gl_name, width, height, owned);
        tracing::info!(
            width,
            height,
            stride,
            "allocated a LINEAR upload slab via GBM"
        );
        Ok(SlabAlloc { texture, map })
    }

    fn wrap_gl_texture(
        ctx: &WgpuContext,
        gl_name: u32,
        width: u32,
        height: u32,
        owned: SlabOwned,
    ) -> wgpu::Texture {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let name = std::num::NonZeroU32::new(gl_name).expect("checked non-zero by the caller");
        // SAFETY: `gl_name` is a live GL texture whose storage is `owned`'s
        // EGLImage and matches the descriptor. `Some(..)` rather than `None`
        // because wgpu must NOT free the texture itself; the callback releases
        // the whole chain in the one safe order.
        let hal_texture = unsafe {
            ctx.device
                .as_hal::<wgpu_hal::api::Gles>()
                .map(|hal_device| {
                    hal_device.texture_from_raw(
                        name,
                        &wgpu_hal::TextureDescriptor {
                            label: Some("ghostframe-upload-slab"),
                            size,
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: wgpu::TextureDimension::D2,
                            format: SLAB_FORMAT,
                            usage: wgpu::TextureUses::RESOURCE,
                            memory_flags: wgpu_hal::MemoryFlags::empty(),
                            view_formats: Vec::new(),
                        },
                        Some(Box::new(move || owned.release())),
                    )
                })
                .expect("with_raw_egl already established this is the GLES backend")
        };
        // SAFETY: `hal_texture` was just built from a live GL texture matching
        // the descriptor. The state is `RESOURCE`: the memory holds pixels the
        // CPU wrote, and the tracker should start from the state the texture
        // is actually read in. (wgpu-core never clears a hal-created texture,
        // so this affects barriers only.)
        unsafe {
            ctx.device.create_texture_from_hal::<wgpu_hal::api::Gles>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("ghostframe-upload-slab"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: SLAB_FORMAT,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::TextureUses::RESOURCE,
            )
        }
    }
}
