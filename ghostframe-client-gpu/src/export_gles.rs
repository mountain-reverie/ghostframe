//! A GL texture whose storage is exported as a dmabuf file descriptor.
//!
//! The GLES counterpart to `export.rs`. Same public surface — `new`,
//! `as_wgpu_texture`, `raw_fd`, `map_read`, and the
//! `width`/`height`/`modifier`/`planes` fields — because `ring.rs`,
//! `renderer.rs` and `tests/gpu_export.rs` are written against one shape and
//! do not know which backend they got. See `lib.rs`'s module wiring.
//!
//! ## The order is inverted relative to Vulkan, deliberately
//!
//! `export.rs` allocates a `VkImage` itself and then wraps it as a wgpu
//! texture, because Vulkan lets you ask for exportable memory at creation
//! time and wgpu does not expose that.
//!
//! EGL has no such requirement: `eglExportDMABUFImageMESA` exports whatever a
//! given `EGLImage` is backed by, and `eglCreateImageKHR` will build an
//! `EGLImage` from an *existing* GL texture. So here wgpu creates the texture
//! normally and the export happens afterwards. That means wgpu owns the
//! texture's lifetime, which removes the whole class of double-free hazard
//! `export.rs` has to document around its no-op drop callback.
//!
//! ## The EGLImage is destroyed immediately
//!
//! Once `eglExportDMABUFImageMESA` has returned an fd, that fd holds its own
//! reference to the underlying buffer object — the `EGLImage` was only ever a
//! handle used to ask for it. So it is destroyed at the end of `new` rather
//! than kept alive in the struct, which is what lets `ExportedImage` need no
//! `Drop` impl at all: wgpu frees the texture, `OwnedFd` closes the fd.
//!
//! ## What this exports is TILED, and that has consequences
//!
//! Measured on Mali-T860 / panfrost / Mesa 24.0.2: a wgpu-created texture
//! exports as one plane with modifier **`0x0800000000000051`** — vendor `0x08`
//! is `DRM_FORMAT_MOD_VENDOR_ARM`, so this is ARM block tiling, not
//! `DRM_FORMAT_MOD_LINEAR`. Its reported *stride* is nonetheless the linear
//! stride (256 bytes for 64px RGBA), which makes a row-major CPU read look
//! like it succeeded while returning scrambled pixels.
//!
//! Two consequences, both real:
//!
//! 1. [`ExportedImage::map_read`] refuses unless the modifier is linear. A
//!    silently-wrong readback is worse than an error, and this one fooled three
//!    of `tests/gpu_export.rs`'s assertions before the check existed.
//! 2. **A consumer that cannot import ARM tiling gets nothing usable.** A GPU
//!    consumer told the modifier is fine — that is what modifiers are for, and
//!    `ring.rs` already carries it through to `PublishedFrame`. But a consumer
//!    that needs linear (a CPU reader, or a compositor without the ARM
//!    modifier) cannot be served by this path at all, because the export
//!    reports the texture's existing tiling rather than choosing one.
//!
//! Serving a linear-only consumer means inverting this module back to the
//! Vulkan pattern: allocate the buffer LINEAR up front and render into it,
//! instead of exporting whatever wgpu allocated. `tools/hw-probe/gbmprobe.c`
//! shows GBM on this driver *can* allocate `XRGB8888` with
//! `GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING`, reporting modifier `0x0` — so
//! the allocation side is available; the missing piece is importing it as an
//! `EGLImage` and wrapping it via `wgpu_hal::gles::Device::texture_from_raw`,
//! which is the same machinery `import_gles.rs` needs. See the GLES/V4L2
//! design doc.
//!
//! ## Untested paths
//!
//! The multi-plane branch is written from the spec and has not run — this
//! driver reports one plane. Treat a report of trouble there as plausible,
//! not surprising.

use crate::dmabuf::{
    choose_modifier, dma_buf_sync, DMA_BUF_SYNC_END, DMA_BUF_SYNC_READ, DMA_BUF_SYNC_START,
    DRM_FORMAT_MOD_LINEAR,
};
use crate::wgpu_ctx::{EglCtx, WgpuContext};
use crate::GpuError;
use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Re-exported so `ring.rs` and external consumers reach it through
/// `export::PlaneLayout` whichever backend is active. See `export.rs`.
pub use crate::dmabuf::PlaneLayout;

/// Format every exported image uses, matching `export.rs`'s `FORMAT_WGPU`.
/// Both backends must agree: the consumer is told a fourcc derived from this,
/// and a mismatch would silently reinterpret bytes.
const FORMAT_WGPU: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

// --- EGL bits `khronos-egl` does not wrap -------------------------------
//
// `khronos-egl` covers core EGL only; KHR_image_base and MESA_image_dma_buf_
// export are extensions, so their entry points are loaded by name and called
// through transmuted function pointers. The signatures below are from
// `EGL_KHR_image_base` and `EGL_MESA_image_dma_buf_export`.

/// `EGL_GL_TEXTURE_2D_KHR`, from `EGL_KHR_gl_image`.
const EGL_GL_TEXTURE_2D_KHR: u32 = 0x30B1;

type EglImageKhr = *mut c_void;

type PfnEglCreateImageKhr = unsafe extern "system" fn(
    dpy: *mut c_void,
    ctx: *mut c_void,
    target: u32,
    buffer: *mut c_void,
    attrib_list: *const i32,
) -> EglImageKhr;

type PfnEglDestroyImageKhr = unsafe extern "system" fn(dpy: *mut c_void, image: EglImageKhr) -> u32;

type PfnEglExportDmabufImageQueryMesa = unsafe extern "system" fn(
    dpy: *mut c_void,
    image: EglImageKhr,
    fourcc: *mut i32,
    num_planes: *mut i32,
    modifiers: *mut u64,
) -> u32;

type PfnEglExportDmabufImageMesa = unsafe extern "system" fn(
    dpy: *mut c_void,
    image: EglImageKhr,
    fds: *mut i32,
    strides: *mut i32,
    offsets: *mut i32,
) -> u32;

/// The four extension entry points, loaded once per export.
///
/// Loaded rather than cached on `WgpuContext` because export happens a fixed
/// number of times at pool setup, not per frame — `get_proc_address` on the
/// hot path would be worth avoiding, but there is no hot path here.
struct EglExportFns {
    create_image: PfnEglCreateImageKhr,
    destroy_image: PfnEglDestroyImageKhr,
    query: PfnEglExportDmabufImageQueryMesa,
    export: PfnEglExportDmabufImageMesa,
}

impl EglExportFns {
    fn load(egl_ctx: &EglCtx<'_>) -> Result<Self, GpuError> {
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
                query: std::mem::transmute::<extern "system" fn(), PfnEglExportDmabufImageQueryMesa>(
                    get(egl_ctx, "eglExportDMABUFImageQueryMESA")?,
                ),
                export: std::mem::transmute::<extern "system" fn(), PfnEglExportDmabufImageMesa>(
                    get(egl_ctx, "eglExportDMABUFImageMESA")?,
                ),
            }
        })
    }
}

/// A wgpu texture whose storage is exported as a dmabuf.
///
/// No `Drop`: wgpu owns the texture and frees it when the last clone goes
/// away; `OwnedFd` closes the dmabuf. The `EGLImage` used to perform the
/// export is already destroyed by the time this value exists (see the module
/// doc).
#[derive(Debug)]
pub struct ExportedImage {
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    pub planes: Vec<PlaneLayout>,
    /// Held so the GL texture outlives the export. `wgpu::Texture` is
    /// reference-counted, so [`ExportedImage::as_wgpu_texture`] hands out a
    /// clone rather than re-wrapping anything.
    texture: wgpu::Texture,
    fd: OwnedFd,
}

impl ExportedImage {
    /// `preferred` is the consumer's modifier list, most-preferred first;
    /// empty means "library picks". See [`crate::dmabuf::choose_modifier`].
    ///
    /// `host_visible` is accepted for signature parity with the Vulkan backend
    /// and **ignored**. There is no EGL equivalent: whether an exported dmabuf
    /// can be `mmap`ed is a property of how the driver allocated it, not
    /// something the export API lets you request. [`ExportedImage::map_read`]
    /// therefore may fail on this backend where it would succeed on Vulkan,
    /// and it is a diagnostic path only.
    pub fn new(
        ctx: &WgpuContext,
        width: u32,
        height: u32,
        preferred: &[u64],
        host_visible: bool,
    ) -> Result<Self, GpuError> {
        if host_visible {
            tracing::debug!(
                "host_visible ignored on the GLES backend: EGL cannot request \
                 CPU-mappable storage for an exported dmabuf"
            );
        }

        // wgpu creates and owns the texture. Usage mirrors what the Vulkan
        // path's wrapped image is used for: the framebuffer blits into it
        // (RENDER_ATTACHMENT), the pipelines sample it (TEXTURE_BINDING), and
        // COPY_SRC keeps a wgpu-side readback possible for tests.
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ghostframe-export"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT_WGPU,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let gl_name = gl_texture_name(&texture)?;

        let (fd, modifier, planes) = ctx
            .with_raw_egl(|egl_ctx| Self::export_texture(egl_ctx, gl_name, preferred))
            .ok_or_else(|| {
                GpuError::Egl("wgpu is not running on the GLES backend".to_string())
            })??;

        tracing::info!(
            width,
            height,
            modifier,
            planes = planes.len(),
            "exported dmabuf via EGL_MESA_image_dma_buf_export"
        );

        Ok(Self {
            width,
            height,
            modifier,
            planes,
            texture,
            fd,
        })
    }

    /// Wrap the GL texture in an `EGLImage`, export it, destroy the image.
    fn export_texture(
        egl_ctx: &EglCtx<'_>,
        gl_name: u32,
        preferred: &[u64],
    ) -> Result<(OwnedFd, u64, Vec<PlaneLayout>), GpuError> {
        let fns = EglExportFns::load(egl_ctx)?;
        let dpy = egl_ctx.display.as_ptr();

        // SAFETY: `dpy`/`egl_ctx.context` are live EGL handles for the
        // duration of `with_raw_egl`'s borrow; `gl_name` is a live GL texture
        // owned by wgpu. The GL context is current because `with_raw_egl`
        // holds wgpu-hal's context lock.
        let image = unsafe {
            (fns.create_image)(
                dpy,
                egl_ctx.context,
                EGL_GL_TEXTURE_2D_KHR,
                gl_name as usize as *mut c_void,
                std::ptr::null(),
            )
        };
        if image.is_null() {
            return Err(GpuError::Egl(
                "eglCreateImageKHR returned EGL_NO_IMAGE_KHR for the export texture".to_string(),
            ));
        }

        // Everything from here must destroy `image` before returning, so the
        // body is a closure and the destroy runs on both paths.
        let result = Self::query_and_export(&fns, dpy, image, preferred);

        // SAFETY: `image` is the handle just created and not yet destroyed.
        // Destroying it does not invalidate an fd already exported from it --
        // that fd holds its own reference (see the module doc).
        let ok = unsafe { (fns.destroy_image)(dpy, image) };
        if ok == 0 {
            // Not fatal to a successful export, but it leaks an EGLImage per
            // buffer, which over a long session is worth seeing.
            tracing::warn!("eglDestroyImageKHR failed for an exported image");
        }
        result
    }

    fn query_and_export(
        fns: &EglExportFns,
        dpy: *mut c_void,
        image: EglImageKhr,
        preferred: &[u64],
    ) -> Result<(OwnedFd, u64, Vec<PlaneLayout>), GpuError> {
        let mut fourcc: i32 = 0;
        let mut num_planes: i32 = 0;
        let mut modifier: u64 = 0;
        // SAFETY: all three out-params are live locals of the right types per
        // the EGL_MESA_image_dma_buf_export spec.
        let ok = unsafe { (fns.query)(dpy, image, &mut fourcc, &mut num_planes, &mut modifier) };
        if ok == 0 {
            return Err(GpuError::Egl(
                "eglExportDMABUFImageQueryMESA failed".to_string(),
            ));
        }
        if num_planes < 1 {
            return Err(GpuError::Egl(format!(
                "eglExportDMABUFImageQueryMESA reported {num_planes} planes; \
                 an exported image must have at least one"
            )));
        }

        // The driver tells us the modifier rather than being asked for one:
        // the texture already exists with whatever tiling it was created with.
        // So this is a compatibility check against the consumer's list, not a
        // negotiation -- but it uses the same helper as the Vulkan path so the
        // preference semantics cannot drift between backends.
        let supported = [modifier];
        let chosen = choose_modifier(&supported, preferred).ok_or(GpuError::NoCommonModifier {
            device: supported.to_vec(),
            requested: preferred.to_vec(),
        })?;
        debug_assert_eq!(chosen, modifier, "choose_modifier over a 1-element set");

        let n = num_planes as usize;
        let mut fds = vec![-1i32; n];
        let mut strides = vec![0i32; n];
        let mut offsets = vec![0i32; n];
        // SAFETY: the three buffers are each `num_planes` long, which is what
        // the spec says the driver writes.
        let ok = unsafe {
            (fns.export)(
                dpy,
                image,
                fds.as_mut_ptr(),
                strides.as_mut_ptr(),
                offsets.as_mut_ptr(),
            )
        };
        if ok == 0 {
            return Err(GpuError::Egl("eglExportDMABUFImageMESA failed".to_string()));
        }

        // One fd per plane is exported. This crate's format is single-plane
        // RGBA, and the rest of the pipeline (`DmabufPlanes`, `ring.rs`,
        // the window backends) carries exactly one fd -- so close any extras
        // rather than leaking them, and say so.
        for extra in fds.iter().skip(1) {
            if *extra >= 0 {
                // SAFETY: an fd the driver just handed us and nothing else owns.
                unsafe { libc::close(*extra) };
            }
        }
        if n > 1 {
            tracing::warn!(
                planes = n,
                "exported image has more than one plane; only plane 0's fd is \
                 carried and the rest were closed. A multi-plane export is not \
                 expected for {FORMAT_WGPU:?}."
            );
        }

        if fds[0] < 0 {
            return Err(GpuError::Egl(
                "eglExportDMABUFImageMESA succeeded but returned no fd for plane 0".to_string(),
            ));
        }
        // SAFETY: `fds[0]` is a fresh dmabuf fd owned by nobody else; `OwnedFd`
        // takes sole ownership and closes it on drop.
        let fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };

        let planes = (0..n)
            .map(|i| PlaneLayout {
                offset: offsets[i].max(0) as u64,
                stride: strides[i].max(0) as u64,
            })
            .collect();

        Ok((fd, modifier, planes))
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

/// The GL texture name behind a wgpu texture.
///
/// `wgpu_hal::gles::Texture::inner` is public, so this needs no patch — but it
/// is an enum, and only the `Texture` variant has a name to export. A
/// renderbuffer or a default-framebuffer texture cannot be turned into an
/// `EGLImage`, and wgpu chooses between them from the usage flags, so getting
/// this wrong would show up as a confusing EGL error rather than here.
fn gl_texture_name(texture: &wgpu::Texture) -> Result<u32, GpuError> {
    // SAFETY: the borrow does not outlive this function and nothing here
    // mutates or destroys wgpu state -- it reads the texture's identity.
    let hal = unsafe { texture.as_hal::<wgpu_hal::api::Gles>() }
        .ok_or_else(|| GpuError::Egl("texture is not a GLES texture".to_string()))?;
    // `TextureInner` is wgpu-hal's enum, not ours, so the wildcard is the
    // exception `lib.rs`'s module doc allows: listing its variants would break
    // this build every time wgpu-hal adds a backing kind we do not care about,
    // and every non-`Texture` kind is handled identically here -- there is no
    // per-variant decision to be forced.
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "foreign enum (wgpu_hal::gles::TextureInner); all non-Texture \
                  kinds are one case and new ones should join it, not break the build"
    )]
    match &hal.inner {
        wgpu_hal::gles::TextureInner::Texture { raw, .. } => Ok(raw.0.get()),
        other => Err(GpuError::Egl(format!(
            "export texture is backed by {other:?}, not a GL texture, so it \
             cannot be wrapped in an EGLImage. This is a usage-flag problem in \
             ExportedImage::new, not a driver limitation."
        ))),
    }
}
