//! Import an externally produced dmabuf as wgpu textures.
//!
//! The mirror of [`crate::export`], and subject to one constraint that
//! shapes everything here: **wgpu-hal 30 has `texture_from_raw` but no
//! `buffer_from_raw`**, so an imported dmabuf must become a `VkImage`. For a
//! `VK_IMAGE_TILING_LINEAR` image the driver picks the row pitch, and
//! without `VK_EXT_image_drm_format_modifier` -- which this hardware does
//! not expose -- there is no way to tell Vulkan the pitch the producer used.
//!
//! So the pitch is *checked*, not assumed: `vkGetImageSubresourceLayout`
//! reports what the driver chose, and a disagreement fails the import. A
//! wrong pitch does not produce obviously broken output; it produces a
//! sheared image that looks like a decode bug and costs a day.
//!
//! ## The imported fd is not ours to close
//!
//! `vkAllocateMemory` with `VkImportMemoryFdInfoKHR` "transfers ownership of
//! the file descriptor from the application to the Vulkan implementation"
//! (`VK_KHR_external_memory_fd`). That is not a lazily-deferred transfer on
//! this driver: measured here, RADV/amdgpu closes the exact fd number passed
//! in synchronously, inside the `vkAllocateMemory` call that imports it (a
//! DRM PRIME import dups the buffer at the kernel level and the userspace fd
//! is done). Holding onto the duplicated fd in an `OwnedFd` field past that
//! point -- as if `ImportedNv12` still owned it -- means its `Drop` later
//! calls `close()` on an fd the driver already closed, which Rust's
//! `OwnedFd` treats as a double-close and aborts the process
//! (`std::os::fd::owned`'s `debug_assert_fd_is_open`). That is exactly what
//! this module's first working version did, and how this was found. So the
//! dup'd fd is `mem::forget`-ten immediately after a successful
//! `allocate_memory`, not stored.

use crate::wgpu_ctx::WgpuContext;
use crate::GpuError;
use ash::vk;
use ghostframe_client_h264::{DmabufPlanes, PlaneDesc};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// The two textures an NV12 dmabuf becomes.
pub struct ImportedNv12 {
    pub luma: wgpu::Texture,
    pub chroma: wgpu::Texture,
    images: Vec<vk::Image>,
    memory: vk::DeviceMemory,
    device: ash::Device,
}

impl Drop for ImportedNv12 {
    fn drop(&mut self) {
        // SAFETY: images first, then the memory they were bound to -- freeing
        // memory under a live image is the invalid order. Both were created
        // here and are owned solely by this struct.
        unsafe {
            for image in self.images.drain(..) {
                self.device.destroy_image(image, None);
            }
            self.device.free_memory(self.memory, None);
        }
    }
}

/// Import `planes` as two single-plane textures: luma as `R8Unorm`, chroma as
/// `Rg8Unorm` at half resolution.
///
/// The fd is duplicated, so the caller keeps ownership of theirs and may drop
/// the mapped frame as soon as this returns.
pub fn import_nv12(ctx: &WgpuContext, planes: &DmabufPlanes) -> Result<ImportedNv12, GpuError> {
    // Three cases, and the middle one is the one that matters here.
    //
    // LINEAR: import, nothing to check.
    //
    // INVALID: the producer declined to say. On AMD pre-GFX9 that is the ONLY
    // value the driver can report -- modifiers begin at GFX9, so radeonsi has
    // no vocabulary for "linear" either (spec §7.1). Refusing on INVALID would
    // make this whole function dead code on exactly the hardware it was
    // written for. Spec §7.2 measured those surfaces byte-for-byte against
    // `av_hwframe_transfer_data` at 640x480 and 1920x1080 and found them
    // linear, so we proceed -- and the `check_pitch` below is what keeps that
    // from being an assumption: it compares the pitch the driver actually
    // chose against the descriptor's, and refuses the import on disagreement.
    //
    // Any other modifier is a real tiled layout, which needs
    // VK_EXT_image_drm_format_modifier to import. Say so precisely rather than
    // failing deeper in with a confusing Vulkan error.
    const DRM_FORMAT_MOD_LINEAR: u64 = 0;
    const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
    match planes.modifier {
        DRM_FORMAT_MOD_LINEAR => {}
        DRM_FORMAT_MOD_INVALID => {
            tracing::debug!(
                "dmabuf reports no modifier; attempting a linear import, \
                 guarded by the pitch check"
            );
        }
        m if !ctx.explicit_modifiers => {
            return Err(GpuError::Vulkan(format!(
                "dmabuf has modifier 0x{m:016x} but this adapter lacks \
                 VK_EXT_image_drm_format_modifier, so only LINEAR (0) or \
                 INVALID (unspecified) can be imported"
            )));
        }
        _ => {}
    }

    ctx.with_raw(|instance, device, phys| import_inner(instance, device, phys, &ctx.device, planes))
        .ok_or_else(|| GpuError::Vulkan("wgpu is not running on the Vulkan backend".to_string()))?
}

fn import_inner(
    instance: &ash::Instance,
    device: &ash::Device,
    phys: vk::PhysicalDevice,
    wgpu_device: &wgpu::Device,
    planes: &DmabufPlanes,
) -> Result<ImportedNv12, GpuError> {
    // Duplicate: vkImportMemoryFdKHR takes ownership of the fd it is given,
    // while the caller's `MappedFrame` still owns the original.
    // SAFETY: `planes.fd` is a live fd owned by the caller for the duration
    // of this call.
    let dup = unsafe { libc::dup(planes.fd) };
    if dup < 0 {
        return Err(GpuError::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: `dup` is a fresh fd this process now owns exclusively.
    let owned = unsafe { OwnedFd::from_raw_fd(dup) };

    let luma = create_linear_image(device, planes.width, planes.height, vk::Format::R8_UNORM)?;
    let chroma = match create_linear_image(
        device,
        planes.chroma_width(),
        planes.chroma_height(),
        vk::Format::R8G8_UNORM,
    ) {
        Ok(i) => i,
        Err(e) => {
            // SAFETY: `luma` was created above and nothing else holds it.
            unsafe { device.destroy_image(luma, None) };
            return Err(e);
        }
    };

    let result = bind_and_wrap(
        instance,
        device,
        phys,
        wgpu_device,
        planes,
        luma,
        chroma,
        owned,
    );
    if result.is_err() {
        // SAFETY: on the error path neither image was handed to an
        // `ImportedNv12`, so this is the only owner.
        unsafe {
            device.destroy_image(chroma, None);
            device.destroy_image(luma, None);
        }
    }
    result
}

fn create_linear_image(
    device: &ash::Device,
    width: u32,
    height: u32,
    format: vk::Format,
) -> Result<vk::Image, GpuError> {
    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::LINEAR)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external);

    // SAFETY: `info` is fully populated and `device` is live.
    unsafe { device.create_image(&info, None) }
        .map_err(|e| GpuError::Vulkan(format!("create_image (import, {format:?}): {e}")))
}

#[allow(clippy::too_many_arguments)]
fn bind_and_wrap(
    instance: &ash::Instance,
    device: &ash::Device,
    phys: vk::PhysicalDevice,
    wgpu_device: &wgpu::Device,
    planes: &DmabufPlanes,
    luma: vk::Image,
    chroma: vk::Image,
    fd: OwnedFd,
) -> Result<ImportedNv12, GpuError> {
    let ext_mem_fd = ash::khr::external_memory_fd::Device::new(instance, device);

    // What memory types can back this fd?
    let mut fd_props = vk::MemoryFdPropertiesKHR::default();
    // SAFETY: `fd` is a live dmabuf fd; `fd_props` is a valid out-param.
    unsafe {
        ext_mem_fd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            fd.as_raw_fd(),
            &mut fd_props,
        )
    }
    .map_err(|e| GpuError::Vulkan(format!("get_memory_fd_properties: {e}")))?;

    // SAFETY: both images are live.
    let luma_req = unsafe { device.get_image_memory_requirements(luma) };
    let chroma_req = unsafe { device.get_image_memory_requirements(chroma) };
    let type_bits =
        fd_props.memory_type_bits & luma_req.memory_type_bits & chroma_req.memory_type_bits;

    let mem_type_index = find_memory_type_index(instance, phys, type_bits).ok_or_else(|| {
        GpuError::Vulkan("no memory type can back this dmabuf and both plane images".to_string())
    })?;

    // The chroma image binds at a nonzero offset into the same allocation,
    // which Vulkan only permits at a multiple of its alignment.
    if !planes.chroma.offset.is_multiple_of(chroma_req.alignment) {
        return Err(GpuError::Vulkan(format!(
            "chroma plane offset {} is not a multiple of the required alignment {}",
            planes.chroma.offset, chroma_req.alignment
        )));
    }

    // `planes.size` is what the driver reported for the dmabuf object.
    // Reconstructing it as `chroma.offset + chroma.pitch * chroma_height()`
    // looks equivalent and is not: any padding past the last chroma row, or an
    // alignment-driven taller chroma plane, makes the guess short, and a short
    // `vkAllocateMemory` surfaces as an opaque import failure with nothing
    // pointing back to the arithmetic.
    let total = planes.size;

    let mut import_info = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(fd.as_raw_fd());
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(total)
        .memory_type_index(mem_type_index)
        .push_next(&mut import_info);

    // SAFETY: `alloc_info` imports exactly the fd above, sized to cover both
    // planes. On success Vulkan owns the fd.
    let memory = unsafe { device.allocate_memory(&alloc_info, None) }
        .map_err(|e| GpuError::Vulkan(format!("allocate_memory (import): {e}")))?;

    // Ownership of `fd` just passed to the Vulkan implementation (module
    // doc: "the imported fd is not ours to close"). `mem::forget` releases
    // it from `OwnedFd`'s bookkeeping without running `close()` on it --
    // closing it here would be a double-close, since RADV already closed
    // this exact fd number as part of the `allocate_memory` call above.
    std::mem::forget(fd);

    // SAFETY: images and memory are live and sized for each other.
    unsafe { device.bind_image_memory(luma, memory, planes.luma.offset) }.map_err(|e| {
        // SAFETY: nothing is bound yet, so freeing is sound.
        unsafe { device.free_memory(memory, None) };
        GpuError::Vulkan(format!("bind_image_memory (luma): {e}"))
    })?;
    // SAFETY: as above.
    unsafe { device.bind_image_memory(chroma, memory, planes.chroma.offset) }.map_err(|e| {
        // SAFETY: freeing memory with `luma` still bound is sound only
        // because `luma` is destroyed by the caller's error path before this
        // function's memory is reused; destroy it here first to keep the
        // documented order (images, then memory).
        unsafe {
            device.destroy_image(luma, None);
            device.free_memory(memory, None);
        }
        GpuError::Vulkan(format!("bind_image_memory (chroma): {e}"))
    })?;

    // THE CHECK. The driver chose these pitches; the producer chose the ones
    // in `planes`. If they differ, every row after the first reads from the
    // wrong offset and the image shears.
    check_pitch(device, luma, "luma", planes.luma)?;
    check_pitch(device, chroma, "chroma", planes.chroma)?;

    let luma_tex = wrap_texture(
        wgpu_device,
        luma,
        planes.width,
        planes.height,
        wgpu::TextureFormat::R8Unorm,
        "ghostframe-imported-luma",
    )?;
    let chroma_tex = wrap_texture(
        wgpu_device,
        chroma,
        planes.chroma_width(),
        planes.chroma_height(),
        wgpu::TextureFormat::Rg8Unorm,
        "ghostframe-imported-chroma",
    )?;

    Ok(ImportedNv12 {
        luma: luma_tex,
        chroma: chroma_tex,
        images: vec![luma, chroma],
        memory,
        device: device.clone(),
    })
}

fn check_pitch(
    device: &ash::Device,
    image: vk::Image,
    what: &str,
    plane: PlaneDesc,
) -> Result<(), GpuError> {
    let subresource = vk::ImageSubresource {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        array_layer: 0,
    };
    // SAFETY: `image` is live, bound, and linear-tiled, which is the only
    // tiling for which this query is defined.
    let layout = unsafe { device.get_image_subresource_layout(image, subresource) };
    if layout.row_pitch != plane.pitch {
        return Err(GpuError::Vulkan(format!(
            "{what} plane pitch mismatch: the dmabuf says {}, the driver's linear \
             image wants {}. Importing anyway would shear the image. Use the CPU \
             copy path for this frame.",
            plane.pitch, layout.row_pitch
        )));
    }
    Ok(())
}

fn wrap_texture(
    wgpu_device: &wgpu::Device,
    image: vk::Image,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    label: &'static str,
) -> Result<wgpu::Texture, GpuError> {
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    // SAFETY: `image` is a live VkImage bound to memory `ImportedNv12` owns.
    // The no-op drop callback keeps ownership here rather than letting
    // wgpu-hal destroy the image, and `TextureMemory::External` tells it the
    // memory is not its to free -- `ImportedNv12::drop` does both.
    // `UNINITIALIZED` is the image's true layout: it was just created.
    let hal_texture = unsafe {
        wgpu_device
            .as_hal::<wgpu_hal::api::Vulkan>()
            .map(|hal_device| {
                hal_device.texture_from_raw(
                    image,
                    &wgpu_hal::TextureDescriptor {
                        label: Some(label),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC,
                        memory_flags: wgpu_hal::MemoryFlags::empty(),
                        view_formats: Vec::new(),
                    },
                    Some(Box::new(|| {})),
                    wgpu_hal::vulkan::TextureMemory::External,
                )
            })
    }
    .ok_or_else(|| GpuError::Vulkan("wgpu is not running on the Vulkan backend".to_string()))?;

    // SAFETY: `hal_texture` was just built from a valid VkImage matching this
    // descriptor, and `UNINITIALIZED` reflects its true current layout.
    Ok(unsafe {
        wgpu_device.create_texture_from_hal::<wgpu_hal::api::Vulkan>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            },
            wgpu::TextureUses::UNINITIALIZED,
        )
    })
}

fn find_memory_type_index(
    instance: &ash::Instance,
    phys: vk::PhysicalDevice,
    type_bits: u32,
) -> Option<u32> {
    // SAFETY: `phys` is a live physical device from `WgpuContext::with_raw`.
    let props = unsafe { instance.get_physical_device_memory_properties(phys) };
    (0..props.memory_type_count).find(|i| type_bits & (1 << i) != 0)
}
