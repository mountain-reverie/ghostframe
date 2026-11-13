// SPDX-License-Identifier: MIT OR Apache-2.0
//! One upload slab on the Vulkan backend: a `VK_IMAGE_TILING_LINEAR` image in
//! host-visible, host-coherent memory that stays mapped for its whole life.
//! The CPU writes tiles into the mapping and the GPU samples the same image,
//! so the slab shares `rgba_slab_tiles.wgsl` with the GLES one.
//!
//! ## First Vulkan run: what to look at
//!
//! Run `cargo test -p ghostframe-client-gpu --test gpu_tileready` with
//! `VK_LAYER_KHRONOS_validation` enabled.
//! `slab_route_puts_every_pixel_where_it_belongs` is the detector: a slab's
//! first batch coming out black or garbage means the host writes were lost to a
//! layout transition.
//!
//! The contents are protected by one thing: `UploadPool` transitions every slab
//! to its sampled state (`TextureUses::RESOURCE`) with a submit and a wait
//! *before* the decode thread gets a slot, so the transition that would
//! otherwise run at the first flush -- after the first batch was written, and
//! allowed to discard it -- has already happened, on an image that holds
//! nothing yet. (The image is also created `PREINITIALIZED`, but wgpu tracks
//! the texture as `UNINITIALIZED`, so its barrier starts from `UNDEFINED`
//! whatever the creation layout; that setting does not protect anything and is
//! kept only because it is the honest layout for a host-written image.) From
//! then on the image sits in `SHADER_READ_ONLY_OPTIMAL` while the CPU writes
//! it. Host access in that layout is outside what the specification promises
//! (it allows host access in `GENERAL` and `PREINITIALIZED` only) but works on
//! the drivers that matter (Mesa, NVIDIA). If a driver objects, the
//! specification-clean alternative is to bind the slab as a read-only storage
//! image and keep it in `GENERAL` for life; that is not implemented here.
//!
//! **This file has not run on hardware.** The development machine for this
//! change has no Vulkan driver; it compiles under the default feature set and
//! is written from the spec and from `export.rs`/`import.rs`, whose
//! allocation and wrapping it follows. Everything that can go wrong at
//! creation (a driver with no linear sampled RGBA8, no host-coherent type,
//! too small a maximum extent) returns an error, and the caller then runs
//! without slabs. What cannot be checked from here is the per-flush cost and
//! the layout behaviour described above.
//!
//! Why an image and not a buffer: a buffer would need a second shader
//! variant, and wgpu-hal 30 has no `buffer_from_raw` to wrap one that was
//! allocated here anyway (`import.rs` hit the same wall).
//!
//! Why `HOST_COHERENT` and not an explicit flush: with it, CPU writes become
//! visible to the device at the next queue submission with no
//! `vkFlushMappedMemoryRanges`, so [`SlabMap::begin_cpu_write`] and
//! [`SlabMap::end_cpu_write`] have nothing to do, and the decode thread need
//! not know which backend it is on.

use crate::wgpu_ctx::WgpuContext;
use crate::GpuError;
use ash::vk;
use std::sync::Arc;

/// See `upload_slab_gles.rs`: both backends name the same wgpu format.
pub(crate) const SLAB_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const SLAB_FORMAT_VK: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// The image and its memory, freed together when the last holder drops.
///
/// Held by the texture's drop callback and by [`SlabMap`]. The texture's
/// callback only runs once wgpu is finished with the image on the GPU; the
/// map is what a decode thread may keep past the renderer. Whichever goes
/// last frees, so a late decode thread writes to live memory, not freed.
///
/// `_device` keeps the `VkDevice` alive: a `wgpu::Device` is reference counted
/// and destroys the Vulkan device when its last clone goes, and this value can
/// be dropped on the decode thread after the render thread has exited.
struct VkOwner {
    device: ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    _keepalive: wgpu::Device,
}

impl Drop for VkOwner {
    fn drop(&mut self) {
        // SAFETY: `image` and `memory` were created together by
        // `SlabAlloc::new` and are freed only here, image first (memory must
        // not be freed while a live image is bound to it). Freeing the memory
        // unmaps it. `_keepalive` guarantees `device` is still a live device.
        unsafe {
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// The CPU's view of a slab. See `upload_slab_gles.rs` for the contract.
pub(crate) struct SlabMap {
    ptr: *mut u8,
    len: usize,
    stride: usize,
    _owner: Arc<VkOwner>,
}

// SAFETY: `ptr` points into persistently mapped device memory that `_owner`
// keeps alive; it is plain memory any thread may address. Who writes which
// bytes when is the pool's slot protocol.
unsafe impl Send for SlabMap {}
unsafe impl Sync for SlabMap {}

impl SlabMap {
    pub(crate) fn base(&self) -> *mut u8 {
        self.ptr
    }

    pub(crate) fn stride(&self) -> usize {
        self.stride
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Nothing to do: the memory is host-coherent.
    pub(crate) fn begin_cpu_write(&self) -> Result<(), GpuError> {
        Ok(())
    }

    /// Nothing to do: the memory is host-coherent.
    pub(crate) fn end_cpu_write(&self) -> Result<(), GpuError> {
        Ok(())
    }
}

/// Where a pool's slabs come from. Nothing is shared between Vulkan slabs; the
/// type exists so both backends present the same `SlabSource` to the pool (the
/// GLES one shares a GBM device).
pub(crate) struct SlabSource;

impl SlabSource {
    pub(crate) fn new() -> Result<Self, GpuError> {
        Ok(SlabSource)
    }

    /// Vulkan slabs are host-coherent device memory, not a dma-buf: there is
    /// nothing to bracket.
    pub(crate) fn needs_cpu_sync(&self, _driver: Option<&str>) -> bool {
        false
    }

    pub(crate) fn alloc(
        &self,
        ctx: &WgpuContext,
        width: u32,
        height: u32,
    ) -> Result<SlabAlloc, GpuError> {
        SlabAlloc::new(ctx, width, height)
    }
}

/// A slab's GPU texture and the CPU mapping of the same memory.
pub(crate) struct SlabAlloc {
    pub(crate) texture: wgpu::Texture,
    pub(crate) map: SlabMap,
}

impl SlabAlloc {
    pub(crate) fn new(ctx: &WgpuContext, width: u32, height: u32) -> Result<Self, GpuError> {
        let (owner, ptr, len, stride) = ctx
            .with_raw(|instance, device, phys| {
                Self::create(instance, device, phys, &ctx.device, width, height)
            })
            .ok_or_else(|| {
                GpuError::Vulkan("wgpu is not running on the Vulkan backend".to_string())
            })??;
        let texture = Self::wrap(ctx, &owner, width, height)?;
        tracing::info!(
            width,
            height,
            stride,
            "allocated a host-visible upload slab"
        );
        Ok(SlabAlloc {
            texture,
            map: SlabMap {
                ptr,
                len,
                stride,
                _owner: owner,
            },
        })
    }

    /// Create the image, back it with host-visible coherent memory, bind and
    /// map it. Returns the owner, the mapped pointer, the mapped length and
    /// the row pitch.
    fn create(
        instance: &ash::Instance,
        device: &ash::Device,
        phys: vk::PhysicalDevice,
        keepalive: &wgpu::Device,
        width: u32,
        height: u32,
    ) -> Result<(Arc<VkOwner>, *mut u8, usize, usize), GpuError> {
        // Linear tiling is optional for sampled images; ask rather than hope.
        // SAFETY: `phys` is a valid physical device from `with_raw`; this is a
        // read-only property query.
        let props = unsafe {
            instance.get_physical_device_image_format_properties(
                phys,
                SLAB_FORMAT_VK,
                vk::ImageType::TYPE_2D,
                vk::ImageTiling::LINEAR,
                vk::ImageUsageFlags::SAMPLED,
                vk::ImageCreateFlags::empty(),
            )
        }
        .map_err(|e| {
            GpuError::Vulkan(format!(
                "a LINEAR sampled RGBA8 image is not supported here: {e}"
            ))
        })?;
        if props.max_extent.width < width || props.max_extent.height < height {
            return Err(GpuError::Vulkan(format!(
                "LINEAR images top out at {}x{}, a slab is {width}x{height}",
                props.max_extent.width, props.max_extent.height
            )));
        }

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(SLAB_FORMAT_VK)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            // PREINITIALIZED, not UNDEFINED: the first layout transition out of
            // UNDEFINED may discard contents, and the CPU writes into this
            // image before the GPU ever touches it. PREINITIALIZED keeps them.
            .initial_layout(vk::ImageLayout::PREINITIALIZED);
        // SAFETY: `image_info` describes a valid 2D image on a live device.
        let image = unsafe { device.create_image(&image_info, None) }
            .map_err(|e| GpuError::Vulkan(format!("create_image (slab): {e}")))?;

        // From here every failure must destroy what exists. `VkOwner` does it
        // once it holds both; before that the two error paths below do.
        // SAFETY: `image` is valid and was just created on `device`.
        let req = unsafe { device.get_image_memory_requirements(image) };
        // SAFETY: `phys` is valid; read-only query.
        let mem_props = unsafe { instance.get_physical_device_memory_properties(phys) };
        let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let find = |flags: vk::MemoryPropertyFlags| {
            (0..mem_props.memory_type_count).find(|&i| {
                req.memory_type_bits & (1 << i) != 0
                    && mem_props.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
        };
        // Device-local host-visible only where that heap is large (UMA, or a
        // discrete GPU with resizable BAR). The classic 256 MiB BAR window is
        // scarce and write-combined for the CPU; plain host memory is the better
        // home for tens of MiB of slabs there.
        const LARGE_HEAP: u64 = 256 << 20;
        let big_device_local = find(want | vk::MemoryPropertyFlags::DEVICE_LOCAL).filter(|&i| {
            mem_props.memory_heaps[mem_props.memory_types[i as usize].heap_index as usize].size
                > LARGE_HEAP
        });
        let Some(type_index) = big_device_local.or_else(|| find(want)) else {
            // SAFETY: `image` is unused and unbound.
            unsafe { device.destroy_image(image, None) };
            return Err(GpuError::Vulkan(
                "no HOST_VISIBLE | HOST_COHERENT memory type fits a linear image".to_string(),
            ));
        };
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(type_index);
        // SAFETY: `alloc` asks for `req.size` bytes of a type `image` accepts.
        let memory = match unsafe { device.allocate_memory(&alloc, None) } {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: as above.
                unsafe { device.destroy_image(image, None) };
                return Err(GpuError::Vulkan(format!("allocate_memory (slab): {e}")));
            }
        };
        let owner = Arc::new(VkOwner {
            device: device.clone(),
            image,
            memory,
            _keepalive: keepalive.clone(),
        });

        // SAFETY: `image` and `memory` are live and sized for each other.
        unsafe { device.bind_image_memory(image, memory, 0) }
            .map_err(|e| GpuError::Vulkan(format!("bind_image_memory (slab): {e}")))?;
        // SAFETY: `memory` is host-visible, unmapped, and the range is within
        // it. It stays mapped until `VkOwner` frees it.
        let ptr = unsafe { device.map_memory(memory, 0, req.size, vk::MemoryMapFlags::empty()) }
            .map_err(|e| GpuError::Vulkan(format!("map_memory (slab): {e}")))?;
        // SAFETY: `image` is bound; the subresource names its only plane.
        let layout = unsafe {
            device.get_image_subresource_layout(
                image,
                vk::ImageSubresource {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    array_layer: 0,
                },
            )
        };
        let (stride, offset) = (layout.row_pitch as usize, layout.offset as usize);
        let len = stride * height as usize;
        if offset + len > req.size as usize {
            return Err(GpuError::Vulkan(
                "linear image layout runs past its memory".to_string(),
            ));
        }
        // SAFETY: `offset` is inside the mapping (checked above).
        let base = unsafe { ptr.cast::<u8>().add(offset) };
        Ok((owner, base, len, stride))
    }

    /// Wrap the image as a wgpu texture. The texture borrows it: its drop
    /// callback holds an `Arc<VkOwner>`, which is what finally destroys it.
    ///
    /// The initial state is `UNINITIALIZED`, so wgpu's first transition starts
    /// from `UNDEFINED` (the image was created `PREINITIALIZED`, and wgpu-hal
    /// has no way to be told). That transition is made harmless by performing
    /// it up front in `UploadPool::with_slabs`; see the module doc.
    fn wrap(
        ctx: &WgpuContext,
        owner: &Arc<VkOwner>,
        width: u32,
        height: u32,
    ) -> Result<wgpu::Texture, GpuError> {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let keep = Arc::clone(owner);
        // SAFETY: `owner.image` is a live VkImage bound to memory `owner`
        // keeps alive. The drop callback owns an `Arc` to it, so wgpu-hal never
        // frees the image (`TextureMemory::External`) and it outlives every GPU
        // use: the callback runs only when wgpu is done with the texture.
        let hal_texture = unsafe {
            ctx.device
                .as_hal::<wgpu_hal::api::Vulkan>()
                .map(|hal_device| {
                    hal_device.texture_from_raw(
                        owner.image,
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
                        Some(Box::new(move || drop(keep))),
                        wgpu_hal::vulkan::TextureMemory::External,
                    )
                })
        }
        .ok_or_else(|| GpuError::Vulkan("wgpu is not running on the Vulkan backend".to_string()))?;

        // SAFETY: `hal_texture` was just built from a valid VkImage matching
        // this descriptor.
        Ok(unsafe {
            ctx.device.create_texture_from_hal::<wgpu_hal::api::Vulkan>(
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
                wgpu::TextureUses::UNINITIALIZED,
            )
        })
    }
}
