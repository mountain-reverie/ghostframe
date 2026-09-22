use crate::GpuError;

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
    /// `VK_EXT_image_drm_format_modifier` is available, so exported images may
    /// use an explicitly negotiated tiling. When false, exports must be
    /// `DRM_FORMAT_MOD_LINEAR`.
    pub explicit_modifiers: bool,
}

impl WgpuContext {
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
        .map_err(|_| GpuError::NoVulkanAdapter)?;

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

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ghostframe-client"),
            required_features: wgpu::Features::VULKAN_EXTERNAL_MEMORY_FD,
            // downlevel_defaults caps maxComputeInvocationsPerWorkgroup at
            // the portable 256 that palrle_decode.wgsl was designed around,
            // so a limit the browser would not have is not silently
            // available here.
            required_limits: wgpu::Limits::downlevel_defaults(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
            ..Default::default()
        }))
        .map_err(|e| GpuError::Vulkan(format!("request_device: {e}")))?;

        Ok(WgpuContext {
            instance,
            adapter,
            device,
            queue,
            explicit_modifiers,
        })
    }

    /// Run `f` with the raw Vulkan device and physical device.
    ///
    /// Returns `None` if the backend is not Vulkan. Used only by the
    /// export path, which wgpu does not provide.
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
    pub fn with_raw<R>(
        &self,
        f: impl FnOnce(&ash::Instance, &ash::Device, ash::vk::PhysicalDevice) -> R,
    ) -> Option<R> {
        // SAFETY: the borrowed instance/device must not outlive `self`, and
        // we must never destroy anything wgpu owns -- we only allocate our
        // own images bound to memory we export.
        let hal_inst = unsafe { self.instance.as_hal::<wgpu_hal::api::Vulkan>() }?;
        let hal_dev = unsafe { self.device.as_hal::<wgpu_hal::api::Vulkan>() }?;
        Some(f(
            hal_inst.shared_instance().raw_instance(),
            hal_dev.raw_device(),
            hal_dev.raw_physical_device(),
        ))
    }
}
