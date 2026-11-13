// SPDX-License-Identifier: MIT OR Apache-2.0
use crate::GpuError;
// Both backends cache call-invariant lookups here: `ash` function tables and
// memory properties on Vulkan, EGL/GL entry points on GLES.
#[cfg(any(feature = "vulkan", feature = "gles"))]
use std::sync::OnceLock;

/// The EGL instance type `wgpu_hal::gles::AdapterContext::egl_instance` hands
/// back. wgpu-hal keeps its own alias private, so it is spelled out here —
/// which is exactly why `khronos-egl` must be pinned to the version wgpu-hal
/// resolves (see the workspace `Cargo.toml`). A mismatch makes this a
/// different type and the escape hatch unreachable.
#[cfg(feature = "gles")]
pub type EglInstance = khronos_egl::DynamicInstance<khronos_egl::EGL1_4>;

/// Everything an EGL extension call needs: the instance to load function
/// pointers from, the display, and the raw `EGLContext` that
/// `eglCreateImageKHR` wants when its target is a GL texture.
///
/// Handed to `export_gles.rs`/`import_gles.rs` by
/// [`WgpuContext::with_raw_egl`], which holds wgpu-hal's own context lock for
/// the duration — so the GL context is current and no wgpu-internal GL call
/// can interleave.
#[cfg(feature = "gles")]
pub struct EglCtx<'a> {
    pub egl: &'a EglInstance,
    pub display: khronos_egl::Display,
    /// `EGL_NO_CONTEXT` is null; a real context is non-null. Passed straight
    /// through to `eglCreateImageKHR`.
    pub context: *mut std::ffi::c_void,
}

/// A wgpu device that can export its images as dmabufs.
///
/// ## Two tiers of capability, and why we only require the lower one
///
/// Exporting a `VkImage` as a dmabuf needs `VK_KHR_external_memory_fd` and
/// `VK_EXT_external_memory_dma_buf`. Negotiating an *explicit DRM format
/// modifier* — i.e. handing a consumer a tiled buffer and telling it which
/// tiling — additionally needs `VK_EXT_image_drm_format_modifier`.
///
/// wgpu surfaces these as two features. `VULKAN_EXTERNAL_MEMORY_FD` tracks the
/// first extension; `VULKAN_EXTERNAL_MEMORY_DMA_BUF` is set only when all
/// *three* are present (`wgpu-hal-30.0.1/src/vulkan/adapter.rs:1062-1065`).
///
/// Crucially, that second flag is only a *report*. wgpu-hal enables
/// `VK_KHR_external_memory_fd` and `VK_EXT_external_memory_dma_buf` on the
/// device whenever the driver supports them, with no feature gate at all
/// (`adapter.rs:1344-1352`). So a device can export dmabufs while wgpu
/// declines to advertise `VULKAN_EXTERNAL_MEMORY_DMA_BUF`.
///
/// That is not hypothetical: RADV on Polaris (Mesa 26.1.7, RX 480) ships
/// `VK_EXT_external_memory_dma_buf` but not `VK_EXT_image_drm_format_modifier`.
/// Requiring the combined flag would refuse to start on hardware that can
/// export perfectly well in linear tiling.
///
/// So we require only `VULKAN_EXTERNAL_MEMORY_FD` and record explicit-modifier
/// support separately. Without it the export path is limited to
/// `DRM_FORMAT_MOD_LINEAR`, which every consumer can import and which the
/// design already names as the universal fallback.
///
/// ## The escape hatch
///
/// wgpu-hal 30 can *import* a dmabuf (`vulkan::Device::texture_from_dmabuf_fd`)
/// but has no export counterpart, and wgpu itself exposes neither. So
/// `with_raw_device` hands out the underlying `VkDevice` for `export.rs`. That
/// is this crate's only unsafe surface.
pub struct WgpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// Exported images may use an explicitly negotiated tiling rather than
    /// being forced to `DRM_FORMAT_MOD_LINEAR`.
    ///
    /// Sourced per backend, because the capability is spelled differently:
    /// `VK_EXT_image_drm_format_modifier` on Vulkan (reported by wgpu as
    /// `VULKAN_EXTERNAL_MEMORY_DMA_BUF`, which also requires the other two
    /// extensions -- see the type doc above), and
    /// `EGL_EXT_image_dma_buf_import_modifiers` on GLES.
    pub explicit_modifiers: bool,
    /// Lazily-built caches for `import.rs`, which calls
    /// [`WgpuContext::ext_memory_fd`] and [`WgpuContext::memory_properties`]
    /// once per decoded frame (Task 9 imports a fresh dmabuf every frame).
    /// Both underlying calls are call-invariant for this device's lifetime
    /// but not free: `ash::khr::external_memory_fd::Device::new` walks
    /// `vkGetDeviceProcAddr` to build its whole function table, and
    /// `vkGetPhysicalDeviceMemoryProperties` copies a ~520-byte struct.
    /// Built once, on first use, and reused for every import after that.
    ///
    #[cfg(feature = "vulkan")]
    ext_memory_fd: OnceLock<ash::khr::external_memory_fd::Device>,
    #[cfg(feature = "vulkan")]
    mem_properties: OnceLock<ash::vk::PhysicalDeviceMemoryProperties>,
    /// The same thing for GLES, and for the same reason.
    ///
    /// `import_gles` resolves five EGL/GL entry points per imported plane, so
    /// ten per decoded frame if they are loaded per call. `get_proc_address` is
    /// not free, and per-call costs on this hardware have already been measured
    /// hurting this client once -- 178us per `write_texture` turned into an
    /// 11.7x speedup when batched. Loading these once is the cheap version of
    /// that lesson.
    ///
    /// An earlier draft of this comment said the GLES path imports each pool
    /// frame once at setup and so needed no cache. That was the plan in the
    /// design's §6.4; the implementation imports per decoded frame like the
    /// Vulkan path does, because caching textures by fd needs invalidation when
    /// the decoder's pool is rebuilt and fds are recycled -- the defect class
    /// that has already cost this project two debugging sessions. Caching the
    /// function pointers is the part that is safe to do without that.
    #[cfg(feature = "gles")]
    egl_image_fns: OnceLock<crate::egl_ffi::EglImageFns>,
    #[cfg(feature = "gles")]
    gl_tex_fns: OnceLock<crate::egl_ffi::GlTexFns>,
    /// The thread `new` ran on. GL contexts are current per-thread, so every
    /// entry point that touches the device must run here. See
    /// [`WgpuContext::assert_render_thread`].
    #[cfg(feature = "gles")]
    created_on: std::thread::ThreadId,
}

impl WgpuContext {
    /// `base` with the buffer-size limits set to what `adapter` offers (more
    /// than the portable defaults on desktop GPUs; asking for more than the
    /// adapter has would fail `request_device`). Nothing else changes, so
    /// shaders are still checked against the portable limits.
    fn with_adapter_buffer_limits(base: wgpu::Limits, adapter: &wgpu::Limits) -> wgpu::Limits {
        wgpu::Limits {
            max_storage_buffer_binding_size: adapter.max_storage_buffer_binding_size,
            max_buffer_size: adapter.max_buffer_size,
            ..base
        }
    }

    /// Whether the device runs on the GL/GLES backend (panfrost and kin),
    /// where a long submit trips the driver's job watchdog.
    pub fn is_gl(&self) -> bool {
        self.adapter.get_info().backend == wgpu::Backend::Gl
    }

    #[cfg(feature = "vulkan")]
    pub fn new() -> Result<Self, GpuError> {
        // NB: wgpu 30 takes the descriptor by value. `InstanceDescriptor`
        // has no `Default` impl (unlike `DeviceDescriptor`); the
        // constructor for "no display handle" is
        // `new_without_display_handle`.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            ..Default::default()
        }))
        .map_err(|_| GpuError::NoAdapter)?;

        // Require fd export up front. A client that cannot export at all
        // should refuse to start rather than hand its consumer nothing.
        if !adapter
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_FD)
        {
            return Err(GpuError::AdapterCannotExport {
                adapter: adapter.get_info().name,
            });
        }

        // Explicit modifiers are a bonus, not a requirement. See the type doc.
        let explicit_modifiers = adapter
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF);
        if !explicit_modifiers {
            tracing::info!(
                adapter = %adapter.get_info().name,
                "VK_EXT_image_drm_format_modifier unavailable; \
                 exported buffers will be DRM_FORMAT_MOD_LINEAR only"
            );
        }

        let device_desc = wgpu::DeviceDescriptor {
            label: Some("ghostframe-client"),
            // Storage writes to the BGRA export let the compute export copy
            // touch only the damaged tiles; without it the ring keeps the
            // render-pass blit. Asked for only where the adapter has it, so it
            // never costs a device that lacks it.
            required_features: wgpu::Features::VULKAN_EXTERNAL_MEMORY_FD
                | (adapter.features() & wgpu::Features::BGRA8UNORM_STORAGE),
            // downlevel_defaults caps maxComputeInvocationsPerWorkgroup at
            // the portable 256 that palrle_decode.wgsl was designed around,
            // so a limit the browser would not have is not silently
            // available here. It also caps maxStorageBuffersPerShaderStage
            // at 4, which is BELOW the WebGPU spec's own default of 8; the
            // web client's cdf53_integrate.wgsl binds 7, and a few native
            // pipelines bind more than 4, so raise that one limit back up to
            // the spec default.
            //
            // The storage binding size is raised to what this adapter offers
            // (downlevel says 128 MiB, which would send an 8K frame's CDF 5/3
            // plane store -- 174 MB -- to the CPU on a GPU that could hold it).
            required_limits: wgpu::Limits {
                max_storage_buffers_per_shader_stage: 8,
                ..Self::with_adapter_buffer_limits(
                    wgpu::Limits::downlevel_defaults(),
                    &adapter.limits(),
                )
            },
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
            ..Default::default()
        };
        // wgpu-hal does not enable `VK_KHR_external_semaphore_fd`, which the
        // export fence (`fence.rs`) needs, so open the device through its hook
        // when the driver has it. Any trouble there is not worth refusing to
        // start over: the plain path is what the client ran on before, and the
        // ring falls back to waiting on the GPU without the extension.
        let custom = if crate::config::PublishConfig::from_env().plain_device_open {
            None
        } else {
            Self::open_with_semaphore_fd(&adapter, &device_desc)
        };
        let (device, queue) = match custom {
            Some(pair) => pair,
            None => pollster::block_on(adapter.request_device(&device_desc))
                .map_err(|e| GpuError::Vulkan(format!("request_device: {e}")))?,
        };

        Ok(WgpuContext {
            instance,
            adapter,
            device,
            queue,
            explicit_modifiers,
            #[cfg(feature = "vulkan")]
            ext_memory_fd: OnceLock::new(),
            #[cfg(feature = "vulkan")]
            mem_properties: OnceLock::new(),
            #[cfg(feature = "gles")]
            egl_image_fns: OnceLock::new(),
            #[cfg(feature = "gles")]
            gl_tex_fns: OnceLock::new(),
        })
    }

    /// Open the device like `Adapter::request_device` would, with
    /// `VK_KHR_external_semaphore_fd` (and its dependency) added. `None` if
    /// anything about that looks doubtful, so the caller can take the plain path.
    ///
    /// `create_device_from_hal` skips the request-time checks `request_device`
    /// makes, so they are repeated here first. The extension list is checked
    /// against the driver before the hal open, because a `vkCreateDevice` that
    /// reports a missing extension is a panic in wgpu-hal, not an error; the
    /// whole attempt is also wrapped in `catch_unwind` so nothing on this
    /// optional path can take the client down.
    #[cfg(feature = "vulkan")]
    fn open_with_semaphore_fd(
        adapter: &wgpu::Adapter,
        desc: &wgpu::DeviceDescriptor<'_>,
    ) -> Option<(wgpu::Device, wgpu::Queue)> {
        if !adapter.features().contains(desc.required_features)
            || !desc.required_limits.check_limits(&adapter.limits())
        {
            return None;
        }
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Self::open_with_semaphore_fd_inner(adapter, desc)
        }));
        match attempt {
            Ok(opened) => opened,
            Err(_) => {
                tracing::warn!("opening the device with external-semaphore support panicked");
                None
            }
        }
    }

    #[cfg(feature = "vulkan")]
    fn open_with_semaphore_fd_inner(
        adapter: &wgpu::Adapter,
        desc: &wgpu::DeviceDescriptor<'_>,
    ) -> Option<(wgpu::Device, wgpu::Queue)> {
        let wanted = [
            ash::khr::external_semaphore::NAME,
            ash::khr::external_semaphore_fd::NAME,
        ];
        // SAFETY: the borrowed hal adapter lives to the end of this block; it
        // is only read, and used to open one device.
        let open = {
            let hal_adapter = unsafe { adapter.as_hal::<wgpu_hal::api::Vulkan>() }?;
            let instance = hal_adapter.shared_instance().raw_instance();
            // SAFETY: a read-only query on the adapter's own physical device.
            let supported = unsafe {
                instance.enumerate_device_extension_properties(hal_adapter.raw_physical_device())
            }
            .ok()?;
            let has = |name: &std::ffi::CStr| {
                supported
                    .iter()
                    .any(|p| p.extension_name_as_c_str() == Ok(name))
            };
            if !wanted.iter().all(|name| has(name)) {
                return None;
            }
            // SAFETY: the callback only appends extensions the driver listed
            // as supported, which is all the hal contract permits.
            unsafe {
                hal_adapter.open_with_callback(
                    desc.required_features,
                    &desc.required_limits,
                    &desc.memory_hints,
                    Some(Box::new(
                        move |args: wgpu_hal::vulkan::CreateDeviceCallbackArgs<'_, '_, '_>| {
                            for name in wanted {
                                if !args.extensions.contains(&name) {
                                    args.extensions.push(name);
                                }
                            }
                        },
                    )),
                )
            }
            .ok()?
        };
        // SAFETY: `open` was just opened from this adapter with the features and
        // limits `desc` carries, which the caller checked the adapter supports.
        unsafe { adapter.create_device_from_hal::<wgpu_hal::api::Vulkan>(open, desc) }.ok()
    }

    /// GLES/EGL context, for GPUs with no Vulkan driver at all.
    ///
    /// Must be called on the thread that will own rendering: an EGL context is
    /// current per-thread, and wgpu-hal's gles backend takes an internal lock
    /// around its GL calls that makes a cross-thread violation look like it
    /// works under light load. The thread id is recorded here and checked by
    /// [`WgpuContext::assert_render_thread`].
    #[cfg(feature = "gles")]
    pub fn new() -> Result<Self, GpuError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::GL,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            ..Default::default()
        }))
        .map_err(|_| GpuError::NoAdapter)?;

        let adapter_name = adapter.get_info().name;
        {
            let info = adapter.get_info();
            tracing::info!(
                adapter = %info.name,
                driver = %info.driver,
                driver_info = %info.driver_info,
                backend = ?info.backend,
                "GPU adapter selected"
            );
        }

        // Ask EGL directly what it can do. wgpu has no feature bit for
        // `EGL_MESA_image_dma_buf_export` -- it is a MESA extension wgpu never
        // calls -- so the capability check has to go around wgpu, not through
        // it.
        let (can_export, explicit_modifiers) = {
            // SAFETY: the borrowed hal adapter does not outlive this block, and
            // nothing here mutates or destroys anything wgpu owns -- both calls
            // are read-only EGL queries.
            let hal_adapter = unsafe { adapter.as_hal::<wgpu_hal::api::Gles>() }
                .ok_or_else(|| GpuError::Egl("adapter is not the GLES backend".into()))?;
            let ctx = hal_adapter.adapter_context();
            let egl = ctx.egl_instance().ok_or_else(|| {
                GpuError::Egl(
                    "no EGL instance on this adapter (externally created context?), \
                     so dmabuf export is unreachable"
                        .into(),
                )
            })?;
            let display = ctx
                .raw_display()
                .copied()
                .ok_or_else(|| GpuError::Egl("no EGLDisplay on this adapter".into()))?;
            let exts = egl
                .query_string(Some(display), khronos_egl::EXTENSIONS)
                .map_err(|e| GpuError::Egl(format!("query EGL_EXTENSIONS: {e}")))?
                .to_string_lossy()
                .into_owned();
            tracing::debug!(%exts, "EGL display extensions");
            (
                exts.split_whitespace()
                    .any(|e| e == "EGL_MESA_image_dma_buf_export"),
                exts.split_whitespace()
                    .any(|e| e == "EGL_EXT_image_dma_buf_import_modifiers"),
            )
        };

        // Same rule as the Vulkan path: a client that cannot export at all
        // should refuse to start rather than hand its consumer nothing.
        if !can_export {
            return Err(GpuError::AdapterCannotExport {
                adapter: adapter_name,
            });
        }
        if !explicit_modifiers {
            tracing::info!(
                adapter = %adapter_name,
                "EGL_EXT_image_dma_buf_import_modifiers unavailable; \
                 exported buffers will be DRM_FORMAT_MOD_LINEAR only"
            );
        }

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ghostframe-client"),
            // Nothing to require: dmabuf export here is an EGL extension, not
            // a wgpu feature, and it was already checked above.
            required_features: wgpu::Features::empty(),
            // NOT `downlevel_defaults()` unmodified. That asks for 256
            // invocations per workgroup, and GLES 3.1's guaranteed minimum is
            // 128 -- Mali-T860 reports exactly 128, so requesting 256 fails
            // `request_device` outright, before any shader runs, and the error
            // names the limit rather than the shader.
            //
            // 128 here and
            // `shader_validation.rs::MAX_WORKGROUP_INVOCATIONS` must agree:
            // this is what the device is asked for, that is what the shaders
            // are checked against. The storage-buffer bumps are the same as the
            // Vulkan path's and for the same reasons.
            required_limits: wgpu::Limits {
                max_storage_buffers_per_shader_stage: 8,
                max_compute_invocations_per_workgroup: 128,
                max_compute_workgroup_size_x: 128,
                max_compute_workgroup_size_y: 128,
                ..Self::with_adapter_buffer_limits(
                    wgpu::Limits::downlevel_defaults(),
                    &adapter.limits(),
                )
            },
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
            ..Default::default()
        }))
        .map_err(|e| GpuError::Egl(format!("request_device: {e}")))?;

        Ok(WgpuContext {
            instance,
            adapter,
            device,
            queue,
            explicit_modifiers,
            egl_image_fns: OnceLock::new(),
            gl_tex_fns: OnceLock::new(),
            created_on: std::thread::current().id(),
        })
    }

    /// The `EGL_KHR_image_base` entry points, loaded once per context.
    ///
    /// Takes an `&EglCtx` rather than loading from `self` because
    /// `get_proc_address` needs the EGL instance, which only exists inside
    /// [`Self::with_raw_egl`]'s borrow.
    #[cfg(feature = "gles")]
    pub(crate) fn egl_image_fns(
        &self,
        egl_ctx: &EglCtx<'_>,
    ) -> Result<&crate::egl_ffi::EglImageFns, GpuError> {
        if let Some(fns) = self.egl_image_fns.get() {
            return Ok(fns);
        }
        let fns = crate::egl_ffi::EglImageFns::load(egl_ctx)?;
        // `set` losing a race is fine: both values are the same function
        // pointers, and the loser is dropped.
        let _ = self.egl_image_fns.set(fns);
        Ok(self
            .egl_image_fns
            .get()
            .expect("just set, or set by a racing caller"))
    }

    /// The GL entry points for binding an `EGLImage` to a texture, loaded once
    /// per context. See [`Self::egl_image_fns`].
    #[cfg(feature = "gles")]
    pub(crate) fn gl_tex_fns(
        &self,
        egl_ctx: &EglCtx<'_>,
    ) -> Result<&crate::egl_ffi::GlTexFns, GpuError> {
        if let Some(fns) = self.gl_tex_fns.get() {
            return Ok(fns);
        }
        let fns = crate::egl_ffi::GlTexFns::load(egl_ctx)?;
        let _ = self.gl_tex_fns.set(fns);
        Ok(self
            .gl_tex_fns
            .get()
            .expect("just set, or set by a racing caller"))
    }

    /// Panic in debug builds if called off the thread that created the context.
    ///
    /// An EGL context is current per-thread. wgpu-hal's gles backend guards its
    /// own GL calls with a mutex, so touching the device from another thread
    /// may appear to work and then fail under different timing -- the worst
    /// kind of bug to find later. `ghostframe-client-native` already confines
    /// GPU work to its render thread; this makes that an assertion rather than
    /// a convention.
    #[cfg(feature = "gles")]
    pub fn assert_render_thread(&self) {
        debug_assert_eq!(
            std::thread::current().id(),
            self.created_on,
            "WgpuContext used off the thread that created it. EGL contexts are \
             current per-thread; this is undefined behaviour that happens to \
             work sometimes."
        );
    }

    /// Run `f` with the EGL instance, display and context backing this device,
    /// holding wgpu-hal's context lock so the GL context is current.
    ///
    /// The GLES counterpart to [`WgpuContext::with_raw_device`], and for the
    /// same reason: wgpu can import a dmabuf but cannot export one, so
    /// `export_gles.rs` loads `eglExportDMABUFImageMESA` itself.
    ///
    /// The lock matters beyond currency. `eglCreateImageKHR` with a
    /// `EGL_GL_TEXTURE_2D_KHR` target reads GL state, and wgpu-hal serialises
    /// all of its own GL work behind this same lock; taking it here is what
    /// stops an export interleaving with a wgpu command submission on the
    /// same context. The glow handle it yields is deliberately not passed on —
    /// `f` has no business issuing GL commands, only EGL ones.
    ///
    /// Returns `None` if the backend is not GLES or the context was created
    /// externally (`Adapter::new_external`), in which case there is no EGL
    /// display to export through.
    #[cfg(feature = "gles")]
    pub fn with_raw_egl<R>(&self, f: impl FnOnce(&EglCtx<'_>) -> R) -> Option<R> {
        self.assert_render_thread();
        // SAFETY: the borrowed hal device does not outlive this call, and we
        // never destroy anything wgpu owns -- we only create EGLImages (and
        // destroy those same ones) and export fds for resources we allocated.
        let hal_dev = unsafe { self.device.as_hal::<wgpu_hal::api::Gles>() }?;
        let adapter_ctx = hal_dev.context();
        let egl = adapter_ctx.egl_instance()?;
        let display = adapter_ctx.raw_display().copied()?;
        let context = adapter_ctx.raw_context();
        let _gl_lock = adapter_ctx.lock();
        Some(f(&EglCtx {
            egl,
            display,
            context,
        }))
    }

    /// Cached `VK_KHR_external_memory_fd` device-extension function table.
    /// See the field doc on [`WgpuContext::ext_memory_fd`]'s storage for why
    /// this is cached rather than rebuilt on every call.
    #[cfg(feature = "vulkan")]
    pub(crate) fn ext_memory_fd(
        &self,
        instance: &ash::Instance,
        device: &ash::Device,
    ) -> &ash::khr::external_memory_fd::Device {
        self.ext_memory_fd
            .get_or_init(|| ash::khr::external_memory_fd::Device::new(instance, device))
    }

    /// Cached `vkGetPhysicalDeviceMemoryProperties` result.
    #[cfg(feature = "vulkan")]
    pub(crate) fn memory_properties(
        &self,
        instance: &ash::Instance,
        phys: ash::vk::PhysicalDevice,
    ) -> ash::vk::PhysicalDeviceMemoryProperties {
        *self.mem_properties.get_or_init(|| {
            // SAFETY: `instance` and `phys` are live handles borrowed from
            // `with_raw` for the duration of this call; this is a read-only
            // property query with no side effects to sequence against.
            unsafe { instance.get_physical_device_memory_properties(phys) }
        })
    }

    /// Run `f` with the raw Vulkan device and physical device.
    ///
    /// Returns `None` if the backend is not Vulkan. Used only by the
    /// export path, which wgpu does not provide.
    #[cfg(feature = "vulkan")]
    pub fn with_raw_device<R>(
        &self,
        f: impl FnOnce(&ash::Device, ash::vk::PhysicalDevice) -> R,
    ) -> Option<R> {
        // SAFETY: the borrowed device must not outlive `self`, and we must
        // never destroy anything wgpu owns -- we only allocate our own images.
        let hal_dev = unsafe { self.device.as_hal::<wgpu_hal::api::Vulkan>() }?;
        Some(f(hal_dev.raw_device(), hal_dev.raw_physical_device()))
    }

    /// Run `f` with the raw Vulkan instance, device, and physical device.
    ///
    /// `export.rs` needs the `ash::Instance` too, to construct the
    /// `VK_KHR_external_memory_fd` / `VK_EXT_image_drm_format_modifier`
    /// extension function-pointer tables, which `ash`'s device-extension
    /// wrappers require at construction time even though every call after
    /// that goes through the device.
    ///
    /// Returns `None` if the backend is not Vulkan.
    #[cfg(feature = "vulkan")]
    pub fn with_raw<R>(
        &self,
        f: impl FnOnce(&ash::Instance, &ash::Device, ash::vk::PhysicalDevice) -> R,
    ) -> Option<R> {
        // SAFETY: the borrowed instance/device must not outlive `self`, and
        // we must never destroy anything wgpu owns -- we only allocate our
        // own images bound to memory we export. (One deliberate exception to
        // "must not outlive": `upload_slab`'s `VkOwner` keeps an `ash::Device`
        // clone past this call, and a `wgpu::Device` clone alongside it so the
        // VkDevice stays alive for as long as that clone is used.)
        let hal_inst = unsafe { self.instance.as_hal::<wgpu_hal::api::Vulkan>() }?;
        let hal_dev = unsafe { self.device.as_hal::<wgpu_hal::api::Vulkan>() }?;
        Some(f(
            hal_inst.shared_instance().raw_instance(),
            hal_dev.raw_device(),
            hal_dev.raw_physical_device(),
        ))
    }
}
