//! A `VkImage` whose memory is exported as a dmabuf file descriptor.
//!
//! ## Two export paths
//!
//! Which path runs is decided entirely by [`WgpuContext::explicit_modifiers`]
//! (see that type's doc comment for why the flag exists):
//!
//! - **LINEAR** (`explicit_modifiers == false`): there is no
//!   `VK_EXT_image_drm_format_modifier`, so there is no modifier
//!   negotiation at all. The image is created with plain
//!   `vk::ImageTiling::LINEAR`, and its one plane is read back with the
//!   ordinary `vk::ImageAspectFlags::COLOR` aspect -- the aspect Vulkan
//!   defines for non-modifier images. The reported modifier is always
//!   `DRM_FORMAT_MOD_LINEAR` (0), because that is the only tiling this path
//!   can produce.
//! - **Explicit modifier** (`explicit_modifiers == true`): the image is
//!   created with `vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT`, chained with
//!   `VkImageDrmFormatModifierListCreateInfoEXT` narrowed to exactly the
//!   modifier [`ExportedImage::choose_modifier`] picked (a driver given
//!   several candidates is free to choose among them; narrowing to one
//!   removes that ambiguity). Plane layouts on this path use
//!   `vk::ImageAspectFlags::MEMORY_PLANE_0_EXT`, *not* `COLOR` --
//!   `COLOR` on a `DRM_FORMAT_MODIFIER_EXT` image returns a zero stride,
//!   the classic symptom of reading the layout off the wrong aspect. The
//!   driver's actual choice is then confirmed with
//!   `vkGetImageDrmFormatModifierPropertiesEXT`, since narrowing the create
//!   list to one modifier is a request, not a guarantee.
//!
//! This machine (AMD RX 480 / RADV / Mesa 26.1.7) ships
//! `VK_EXT_external_memory_dma_buf` but not `VK_EXT_image_drm_format_modifier`,
//! so only the LINEAR path is exercised by `tests/gpu_export.rs` here. The
//! explicit-modifier path is written from the spec and the sibling LINEAR
//! path but has not run against real hardware; treat a report of trouble
//! there as plausible, not surprising.

use crate::wgpu_ctx::WgpuContext;
use crate::GpuError;
use ash::vk;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Format every exported image uses. Single-plane, so the explicit-modifier
/// path's plane-count handling never has more than one plane to deal with.
const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Byte layout of one dmabuf plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneLayout {
    pub offset: u64,
    pub stride: u64,
}

/// A `VkImage` whose memory is exported as a dmabuf.
///
/// `Drop` destroys the image, then frees the memory, in that order (freeing
/// memory while still bound to a live image would be the invalid sequence).
/// The dmabuf fd is owned exactly once, by the `OwnedFd`, which closes it on
/// drop.
pub struct ExportedImage {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    pub planes: Vec<PlaneLayout>,
    fd: OwnedFd,
    // Kept only so `Drop` can call `destroy_image`/`free_memory`. Not
    // exposed: callers reach the device through `WgpuContext` for
    // everything else.
    device: ash::Device,
}

impl std::fmt::Debug for ExportedImage {
    // `ash::Device` has no `Debug` impl, so it is omitted here rather than
    // deriving.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportedImage")
            .field("image", &self.image)
            .field("memory", &self.memory)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("modifier", &self.modifier)
            .field("planes", &self.planes)
            .field("fd", &self.fd.as_raw_fd())
            .finish()
    }
}

impl ExportedImage {
    /// `preferred` is the consumer's modifier list, most-preferred first.
    /// Empty means "library picks", which prefers `DRM_FORMAT_MOD_LINEAR`
    /// because it is the one every consumer can import.
    pub fn new(
        ctx: &WgpuContext,
        width: u32,
        height: u32,
        preferred: &[u64],
    ) -> Result<Self, GpuError> {
        ctx.with_raw(|instance, device, phys| {
            if ctx.explicit_modifiers {
                Self::create_explicit(instance, device, phys, width, height, preferred)
            } else {
                Self::create_linear(instance, device, phys, width, height, preferred)
            }
        })
        .ok_or_else(|| GpuError::Vulkan("wgpu is not running on the Vulkan backend".to_string()))?
    }

    pub fn raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }

    /// First consumer preference the device also supports; if `preferred`
    /// is empty, LINEAR when available, else the device's first.
    fn choose_modifier(supported: &[u64], preferred: &[u64]) -> Option<u64> {
        if preferred.is_empty() {
            if supported.contains(&DRM_FORMAT_MOD_LINEAR) {
                return Some(DRM_FORMAT_MOD_LINEAR);
            }
            return supported.first().copied();
        }
        preferred.iter().copied().find(|m| supported.contains(m))
    }

    fn create_linear(
        instance: &ash::Instance,
        device: &ash::Device,
        phys: vk::PhysicalDevice,
        width: u32,
        height: u32,
        preferred: &[u64],
    ) -> Result<Self, GpuError> {
        let supported = [DRM_FORMAT_MOD_LINEAR];
        let modifier = Self::choose_modifier(&supported, preferred).ok_or_else(|| {
            GpuError::NoCommonModifier {
                device: supported.to_vec(),
                requested: preferred.to_vec(),
            }
        })?;

        tracing::info!(
            width,
            height,
            modifier,
            "exporting dmabuf image via LINEAR tiling \
             (VK_EXT_image_drm_format_modifier unavailable)"
        );

        let mut ext_image_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image_info = vk::ImageCreateInfo::default()
            .push_next(&mut ext_image_info)
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        // SAFETY: `image_info` describes a valid 2D image; `device` is the
        // live raw device handed out by `WgpuContext::with_raw`.
        let image = unsafe { device.create_image(&image_info, None) }
            .map_err(|e| GpuError::Vulkan(format!("create_image (linear): {e}")))?;

        Self::finish_export(
            instance,
            device,
            phys,
            image,
            width,
            height,
            modifier,
            vk::ImageAspectFlags::COLOR,
        )
    }

    fn create_explicit(
        instance: &ash::Instance,
        device: &ash::Device,
        phys: vk::PhysicalDevice,
        width: u32,
        height: u32,
        preferred: &[u64],
    ) -> Result<Self, GpuError> {
        let supported = Self::enumerate_supported_modifiers(instance, phys);
        let modifier = Self::choose_modifier(&supported, preferred).ok_or_else(|| {
            GpuError::NoCommonModifier {
                device: supported.clone(),
                requested: preferred.to_vec(),
            }
        })?;

        tracing::info!(
            width,
            height,
            modifier,
            supported = ?supported,
            "exporting dmabuf image via explicit DRM format modifier \
             (untested on real hardware as of this writing -- this project's \
             only Vulkan device so far lacks VK_EXT_image_drm_format_modifier)"
        );

        let chosen = [modifier];
        let mut modifier_list_info =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&chosen);
        let mut ext_image_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image_info = vk::ImageCreateInfo::default()
            .push_next(&mut ext_image_info)
            .push_next(&mut modifier_list_info)
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        // SAFETY: `image_info` describes a valid 2D image restricted to the
        // one modifier we already confirmed the device supports.
        let image = unsafe { device.create_image(&image_info, None) }
            .map_err(|e| GpuError::Vulkan(format!("create_image (explicit modifier): {e}")))?;

        // Narrowing the create list to one modifier is a request, not a
        // guarantee -- confirm what the driver actually picked.
        let modifier_ext = ash::ext::image_drm_format_modifier::Device::new(instance, device);
        let mut actual_props = vk::ImageDrmFormatModifierPropertiesEXT::default();
        // SAFETY: `image` was just created successfully above.
        let confirm = unsafe {
            modifier_ext.get_image_drm_format_modifier_properties(image, &mut actual_props)
        };
        if let Err(e) = confirm {
            // SAFETY: `image` is ours and has not been handed to any caller.
            unsafe { device.destroy_image(image, None) };
            return Err(GpuError::Vulkan(format!(
                "get_image_drm_format_modifier_properties: {e}"
            )));
        }
        if actual_props.drm_format_modifier != modifier {
            // SAFETY: as above.
            unsafe { device.destroy_image(image, None) };
            return Err(GpuError::Vulkan(format!(
                "driver reported modifier {} but {modifier} was requested",
                actual_props.drm_format_modifier
            )));
        }

        Self::finish_export(
            instance,
            device,
            phys,
            image,
            width,
            height,
            modifier,
            vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
        )
    }

    /// Enumerate the DRM format modifiers RADV/the driver reports as
    /// supported for [`FORMAT`], via the two-call pattern: first query the
    /// count, then fill a buffer of that size.
    fn enumerate_supported_modifiers(
        instance: &ash::Instance,
        phys: vk::PhysicalDevice,
    ) -> Vec<u64> {
        let mut count_query = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut count_probe = vk::FormatProperties2::default().push_next(&mut count_query);
        // SAFETY: `count_probe` is a valid out-param; `phys` came from
        // `WgpuContext::with_raw` and is alive for the call's duration.
        unsafe { instance.get_physical_device_format_properties2(phys, FORMAT, &mut count_probe) };

        let count = count_query.drm_format_modifier_count as usize;
        let mut props = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
        let mut fill_query = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut props);
        let mut fill_probe = vk::FormatProperties2::default().push_next(&mut fill_query);
        // SAFETY: as above; `props` has exactly `count` elements, matching
        // what the first call reported.
        unsafe { instance.get_physical_device_format_properties2(phys, FORMAT, &mut fill_probe) };

        props.iter().map(|p| p.drm_format_modifier).collect()
    }

    /// Allocate export-capable memory for `image`, bind it, export its fd,
    /// and read back the plane-0 layout with `aspect_mask` (which differs
    /// between the two tiling paths -- see the module doc).
    ///
    /// On any failure after `image` is created, destroys `image` (and frees
    /// memory, if allocation got that far) before returning, so callers
    /// never leak a partially-constructed export on the error path.
    #[allow(clippy::too_many_arguments)]
    fn finish_export(
        instance: &ash::Instance,
        device: &ash::Device,
        phys: vk::PhysicalDevice,
        image: vk::Image,
        width: u32,
        height: u32,
        modifier: u64,
        aspect_mask: vk::ImageAspectFlags,
    ) -> Result<Self, GpuError> {
        match Self::finish_export_inner(
            instance,
            device,
            phys,
            image,
            width,
            height,
            modifier,
            aspect_mask,
        ) {
            Ok(exported) => Ok(exported),
            Err(e) => {
                // SAFETY: `image` was created by the caller of `finish_export`
                // and has not been handed to anything else yet.
                unsafe { device.destroy_image(image, None) };
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_export_inner(
        instance: &ash::Instance,
        device: &ash::Device,
        phys: vk::PhysicalDevice,
        image: vk::Image,
        width: u32,
        height: u32,
        modifier: u64,
        aspect_mask: vk::ImageAspectFlags,
    ) -> Result<Self, GpuError> {
        // SAFETY: `image` is a valid, just-created image on `device`.
        let mem_req = unsafe { device.get_image_memory_requirements(image) };
        let mem_type_index = Self::find_memory_type_index(
            instance,
            phys,
            mem_req.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .ok_or_else(|| {
            GpuError::Vulkan("no memory type supports this exportable image".to_string())
        })?;

        let mut export_alloc_info = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated_alloc_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_req.size)
            .memory_type_index(mem_type_index)
            .push_next(&mut dedicated_alloc_info)
            .push_next(&mut export_alloc_info);

        // SAFETY: `alloc_info` requests memory sized for `image`'s exact
        // requirements, dedicated to `image`, exportable as a dmabuf.
        let memory = unsafe { device.allocate_memory(&alloc_info, None) }
            .map_err(|e| GpuError::Vulkan(format!("allocate_memory (export): {e}")))?;

        match Self::bind_and_export(
            instance,
            device,
            image,
            memory,
            width,
            height,
            modifier,
            aspect_mask,
        ) {
            Ok(exported) => Ok(exported),
            Err(e) => {
                // SAFETY: `memory` was just allocated above and is not yet
                // bound to anything this struct's `Drop` would free.
                unsafe { device.free_memory(memory, None) };
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_and_export(
        instance: &ash::Instance,
        device: &ash::Device,
        image: vk::Image,
        memory: vk::DeviceMemory,
        width: u32,
        height: u32,
        modifier: u64,
        aspect_mask: vk::ImageAspectFlags,
    ) -> Result<Self, GpuError> {
        // SAFETY: `image` and `memory` are both live and were sized/allocated
        // for each other by the caller.
        unsafe { device.bind_image_memory(image, memory, 0) }
            .map_err(|e| GpuError::Vulkan(format!("bind_image_memory: {e}")))?;

        let ext_mem_fd = ash::khr::external_memory_fd::Device::new(instance, device);
        let get_fd_info = vk::MemoryGetFdInfoKHR::default()
            .memory(memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // SAFETY: `memory` was allocated with `ExportMemoryAllocateInfo`
        // requesting exactly `DMA_BUF_EXT`, matching `handle_type` here.
        let raw_fd = unsafe { ext_mem_fd.get_memory_fd(&get_fd_info) }
            .map_err(|e| GpuError::Vulkan(format!("get_memory_fd: {e}")))?;

        // SAFETY: `vkGetMemoryFdKHR` transfers ownership of a new fd to the
        // caller on success (Vulkan spec, "Each call ... must create a new
        // file descriptor"); nothing else in this process holds it, so it is
        // sound to take exclusive ownership here.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        let subresource = vk::ImageSubresource {
            aspect_mask,
            mip_level: 0,
            array_layer: 0,
        };
        // SAFETY: `image` is bound and valid; `subresource` names its only
        // plane (both export paths use single-plane R8G8B8A8_UNORM).
        let layout = unsafe { device.get_image_subresource_layout(image, subresource) };

        Ok(ExportedImage {
            image,
            memory,
            width,
            height,
            modifier,
            planes: vec![PlaneLayout {
                offset: layout.offset,
                stride: layout.row_pitch,
            }],
            fd,
            device: device.clone(),
        })
    }

    fn find_memory_type_index(
        instance: &ash::Instance,
        phys: vk::PhysicalDevice,
        type_bits: u32,
        preferred_flags: vk::MemoryPropertyFlags,
    ) -> Option<u32> {
        // SAFETY: `phys` is a valid physical device handle from `with_raw`.
        let props = unsafe { instance.get_physical_device_memory_properties(phys) };

        (0..props.memory_type_count)
            .find(|&i| {
                type_bits & (1 << i) != 0
                    && props.memory_types[i as usize]
                        .property_flags
                        .contains(preferred_flags)
            })
            .or_else(|| (0..props.memory_type_count).find(|&i| type_bits & (1 << i) != 0))
    }
}

impl Drop for ExportedImage {
    fn drop(&mut self) {
        // SAFETY: `image` and `memory` are owned exclusively by this struct
        // and were created/allocated together in `new`. Destroying the
        // image before freeing its memory is the order Vulkan requires
        // (memory must not be freed while still bound to a live image).
        // `fd` needs no action here -- `OwnedFd`'s own `Drop` closes it.
        unsafe {
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_preference_prefers_linear() {
        assert_eq!(ExportedImage::choose_modifier(&[7, 0, 9], &[]), Some(0));
    }

    #[test]
    fn empty_preference_falls_back_to_first_when_no_linear() {
        assert_eq!(ExportedImage::choose_modifier(&[7, 9], &[]), Some(7));
    }

    #[test]
    fn consumer_preference_order_wins_over_device_order() {
        assert_eq!(ExportedImage::choose_modifier(&[7, 9], &[9, 7]), Some(9));
    }

    #[test]
    fn no_overlap_is_none() {
        assert_eq!(ExportedImage::choose_modifier(&[7, 9], &[1, 2]), None);
    }
}
