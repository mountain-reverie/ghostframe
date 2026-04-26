//! GPU-accelerated dirty tile detection via Vulkan compute (SAD shader).
//!
//! [`GpuFrameProcessor`] imports each frame as a DMA-BUF VkImage, runs the
//! `tile_sad` compute shader that computes per-tile Sum of Absolute Differences
//! against the previous frame, then reads back ~8 KB of SAD scores to decide
//! which 32×32-pixel tiles changed.
//!
//! This replaces the CPU-based dirty tracker for the M2 zero-copy GPU pipeline.

use ash::vk;
use std::ffi::CStr;
use std::io;

/// Pixels-per-tile dimension (matches shader local_size_x/y).
const TILE_SIZE: u32 = 32;

/// SAD score above which a tile is considered dirty.
/// Chosen to ignore sub-pixel rounding noise while catching any visible change.
const SAD_THRESHOLD: u32 = 64;

// ---------------------------------------------------------------------------
// Public struct
// ---------------------------------------------------------------------------

/// Vulkan compute-based dirty tile tracker.
///
/// Call [`diff`][GpuFrameProcessor::diff] each frame with the DMA-BUF fd,
/// resolution and stride.  The returned `Vec<u32>` contains the flat tile
/// indices (`tile_y * cols + tile_x`) of every tile whose SAD score exceeded
/// the threshold.
pub struct GpuFrameProcessor {
    _entry: ash::Entry,
    instance: ash::Instance,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    #[allow(dead_code)]
    queue_family_index: u32,
    command_pool: vk::CommandPool,
    shader_module: vk::ShaderModule,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    prev_image: Option<PrevFrame>,
    sad_buffer: vk::Buffer,
    sad_memory: vk::DeviceMemory,
    sad_ptr: *mut u32,
    max_tiles: u32,
    last_width: u32,
    last_height: u32,
}

struct PrevFrame {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    /// Current Vulkan image layout — tracks transitions across frames.
    layout: vk::ImageLayout,
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
}

// Safety: GpuFrameProcessor is used from a single tokio task; Vulkan objects are
// not thread-safe on their own but we never share them across threads.
unsafe impl Send for GpuFrameProcessor {}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

impl GpuFrameProcessor {
    /// Create a new `GpuFrameProcessor` capable of tracking up to `max_tiles` tiles.
    pub fn new(max_tiles: u32) -> Result<Self, Box<dyn std::error::Error>> {
        unsafe { Self::new_inner(max_tiles) }
    }

    unsafe fn new_inner(max_tiles: u32) -> Result<Self, Box<dyn std::error::Error>> {
        // --- Entry ---
        let entry = ash::Entry::load()?;

        // --- Instance ---
        let app_info = vk::ApplicationInfo::default()
            .application_name(c"ghostframe")
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_name(c"ghostframe")
            .api_version(vk::make_api_version(0, 1, 1, 0));

        let instance_ci = vk::InstanceCreateInfo::default().application_info(&app_info);
        let instance = entry.create_instance(&instance_ci, None)?;

        // --- Physical device ---
        let phys_devices = instance.enumerate_physical_devices()?;
        if phys_devices.is_empty() {
            return Err("no Vulkan physical devices found".into());
        }

        let required_device_exts: [&CStr; 2] = [
            ash::khr::external_memory_fd::NAME,
            ash::ext::external_memory_dma_buf::NAME,
        ];

        let mut chosen = None;
        for &pdev in &phys_devices {
            let supported = instance.enumerate_device_extension_properties(pdev)?;
            let has_all = required_device_exts.iter().all(|req| {
                supported
                    .iter()
                    .any(|ext| CStr::from_ptr(ext.extension_name.as_ptr()) == *req)
            });
            if has_all {
                chosen = Some(pdev);
                break;
            }
        }

        let physical_device =
            chosen.ok_or("no Vulkan device supports DMA-BUF import extensions")?;

        // --- Queue family: prefer COMPUTE-only, fall back to GRAPHICS|COMPUTE ---
        let queue_families =
            instance.get_physical_device_queue_family_properties(physical_device);

        let queue_family_index = queue_families
            .iter()
            .position(|qf| {
                qf.queue_flags.contains(vk::QueueFlags::COMPUTE)
                    && !qf.queue_flags.contains(vk::QueueFlags::GRAPHICS)
            })
            .or_else(|| {
                queue_families
                    .iter()
                    .position(|qf| qf.queue_flags.contains(vk::QueueFlags::COMPUTE))
            })
            .ok_or("no compute-capable queue family")? as u32;

        // --- Logical device ---
        let queue_priorities = [1.0_f32];
        let queue_ci = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&queue_priorities)];

        let ext_names_raw: Vec<*const i8> =
            required_device_exts.iter().map(|e| e.as_ptr()).collect();

        let device_ci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_ci)
            .enabled_extension_names(&ext_names_raw);

        let device = instance.create_device(physical_device, &device_ci, None)?;
        let queue = device.get_device_queue(queue_family_index, 0);

        // --- Command pool ---
        let pool_ci = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family_index)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = device.create_command_pool(&pool_ci, None)?;

        // --- Shader module ---
        let spv = include_bytes!("shaders/tile_sad.spv");
        // SPIR-V words must be u32-aligned; ash handles alignment internally via
        // make_spirv_raw but we use the raw slice approach here.
        let spv_words = ash::util::read_spv(&mut std::io::Cursor::new(spv.as_slice()))?;
        let shader_module_ci = vk::ShaderModuleCreateInfo::default().code(&spv_words);
        let shader_module = device.create_shader_module(&shader_module_ci, None)?;

        // --- Descriptor set layout ---
        // binding 0: STORAGE_IMAGE (current frame)
        // binding 1: STORAGE_IMAGE (prev frame)
        // binding 2: STORAGE_BUFFER (SAD output)
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let dsl_ci = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        let descriptor_set_layout = device.create_descriptor_set_layout(&dsl_ci, None)?;

        // --- Pipeline layout ---
        // Push constants: 3 x u32 = 12 bytes (frame_width, frame_height, cols)
        let push_range = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(12)];
        let pipeline_layout_ci = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(std::slice::from_ref(&descriptor_set_layout))
            .push_constant_ranges(&push_range);
        let pipeline_layout = device.create_pipeline_layout(&pipeline_layout_ci, None)?;

        // --- Compute pipeline ---
        let entry_name = c"main";
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(shader_module)
            .name(entry_name);

        let compute_ci = vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(pipeline_layout);

        let pipelines = device
            .create_compute_pipelines(vk::PipelineCache::null(), &[compute_ci], None)
            .map_err(|(_, e)| e)?;
        let pipeline = pipelines[0];

        // --- Descriptor pool ---
        // Allocate enough for 1 set per frame with FREE_DESCRIPTOR_SET so we can
        // free individual sets after each frame.
        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_IMAGE,
                descriptor_count: 2,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 1,
            },
        ];
        let dp_ci = vk::DescriptorPoolCreateInfo::default()
            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        let descriptor_pool = device.create_descriptor_pool(&dp_ci, None)?;

        // --- SAD output buffer ---
        // max_tiles * 4 bytes, HOST_VISIBLE | HOST_COHERENT, persistently mapped
        let sad_buf_size = (max_tiles * 4) as vk::DeviceSize;
        let buf_ci = vk::BufferCreateInfo::default()
            .size(sad_buf_size)
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let sad_buffer = device.create_buffer(&buf_ci, None)?;
        let buf_reqs = device.get_buffer_memory_requirements(sad_buffer);
        let mem_props = instance.get_physical_device_memory_properties(physical_device);
        let sad_mem_type = find_memory_type(
            &mem_props,
            buf_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or("no host-visible memory type for SAD buffer")?;

        let sad_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(buf_reqs.size)
            .memory_type_index(sad_mem_type);
        let sad_memory = device.allocate_memory(&sad_alloc, None)?;
        device.bind_buffer_memory(sad_buffer, sad_memory, 0)?;

        // Persistently mapped pointer to SAD values
        let sad_ptr = device.map_memory(
            sad_memory,
            0,
            sad_buf_size,
            vk::MemoryMapFlags::empty(),
        )? as *mut u32;

        Ok(Self {
            _entry: entry,
            instance,
            physical_device,
            device,
            queue,
            queue_family_index,
            command_pool,
            shader_module,
            pipeline,
            pipeline_layout,
            descriptor_set_layout,
            descriptor_pool,
            prev_image: None,
            sad_buffer,
            sad_memory,
            sad_ptr,
            max_tiles,
            last_width: 0,
            last_height: 0,
        })
    }
}

// ---------------------------------------------------------------------------
// diff()
// ---------------------------------------------------------------------------

impl GpuFrameProcessor {
    /// Compare the DMA-BUF frame at `fd` against the previous frame.
    ///
    /// Returns flat tile indices of dirty tiles.  On the first call (no
    /// previous frame), all tiles are returned as dirty.
    ///
    /// The `fd` is **not** consumed; this function dups it internally.
    pub fn diff(
        &mut self,
        fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
        unsafe { self.diff_inner(fd, width, height, stride) }
    }

    unsafe fn diff_inner(
        &mut self,
        fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
        let cols = width.div_ceil(TILE_SIZE);
        let rows = height.div_ceil(TILE_SIZE);
        let tile_count = cols * rows;

        if tile_count > self.max_tiles {
            return Err(format!(
                "tile_count {tile_count} exceeds max_tiles {}",
                self.max_tiles
            )
            .into());
        }

        // Drop prev_image if resolution changed
        if width != self.last_width || height != self.last_height {
            if let Some(prev) = self.prev_image.take() {
                self.destroy_prev_frame(prev);
            }
            self.last_width = width;
            self.last_height = height;
        }

        // --- Import DMA-BUF as VkImage ---
        let current = self.import_dmabuf(fd, width, height, stride)?;

        // --- First frame: store as prev, return all tiles dirty ---
        if self.prev_image.is_none() {
            self.prev_image = Some(current);
            let all_dirty: Vec<u32> = (0..tile_count).collect();
            return Ok(all_dirty);
        }

        let prev = self.prev_image.as_ref().unwrap();

        // --- Allocate descriptor set ---
        let set_layouts = [self.descriptor_set_layout];
        let ds_alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&set_layouts);
        let descriptor_sets = self.device.allocate_descriptor_sets(&ds_alloc_info)?;
        let ds = descriptor_sets[0];

        // Update descriptor set:
        // binding 0 = current_frame, binding 1 = prev_frame, binding 2 = SAD buffer
        let current_image_info = [vk::DescriptorImageInfo::default()
            .image_view(current.view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let prev_image_info = [vk::DescriptorImageInfo::default()
            .image_view(prev.view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let sad_buffer_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.sad_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];

        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&current_image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&prev_image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&sad_buffer_info),
        ];
        self.device.update_descriptor_sets(&writes, &[]);

        // --- Command buffer ---
        let cmd_alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cmd_bufs = self.device.allocate_command_buffers(&cmd_alloc)?;
        let cmd = cmd_bufs[0];

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        self.device.begin_command_buffer(cmd, &begin_info)?;

        // Transition both images PREINITIALIZED -> GENERAL
        let subresource_range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let barriers = [
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::PREINITIALIZED)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(current.image)
                .subresource_range(subresource_range)
                .src_access_mask(vk::AccessFlags::HOST_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ),
            vk::ImageMemoryBarrier::default()
                .old_layout(prev.layout)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(prev.image)
                .subresource_range(subresource_range)
                .src_access_mask(if prev.layout == vk::ImageLayout::PREINITIALIZED {
                    vk::AccessFlags::HOST_WRITE
                } else {
                    vk::AccessFlags::SHADER_READ
                })
                .dst_access_mask(vk::AccessFlags::SHADER_READ),
        ];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::HOST,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &barriers,
        );

        // Bind pipeline and descriptor set
        self.device
            .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.pipeline_layout,
            0,
            &[ds],
            &[],
        );

        // Push constants: [frame_width, frame_height, cols]
        let push_data: [u32; 3] = [width, height, cols];
        let push_bytes = std::slice::from_raw_parts(
            push_data.as_ptr() as *const u8,
            std::mem::size_of_val(&push_data),
        );
        self.device.cmd_push_constants(
            cmd,
            self.pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            push_bytes,
        );

        // Dispatch one workgroup per tile
        self.device.cmd_dispatch(cmd, cols, rows, 1);

        // Memory barrier: SHADER_WRITE -> HOST_READ on SAD buffer
        let buf_barrier = [vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.sad_buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE)];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            &buf_barrier,
            &[],
        );

        self.device.end_command_buffer(cmd)?;

        // --- Submit and wait ---
        let fence_ci = vk::FenceCreateInfo::default();
        let fence = self.device.create_fence(&fence_ci, None)?;
        let submit_info = vk::SubmitInfo::default().command_buffers(&cmd_bufs);
        self.device.queue_submit(self.queue, &[submit_info], fence)?;
        self.device
            .wait_for_fences(&[fence], true, u64::MAX)?;
        self.device.destroy_fence(fence, None);
        self.device.free_command_buffers(self.command_pool, &cmd_bufs);

        // --- Read SAD values ---
        let sad_slice = std::slice::from_raw_parts(self.sad_ptr, tile_count as usize);
        let dirty: Vec<u32> = (0..tile_count)
            .filter(|&i| sad_slice[i as usize] > SAD_THRESHOLD)
            .collect();

        // --- Swap prev_image ---
        // Destroy old prev, promote current to prev.
        // Current was transitioned to GENERAL during the dispatch.
        let old_prev = self.prev_image.take().unwrap();
        self.destroy_prev_frame(old_prev);
        let mut current = current;
        current.layout = vk::ImageLayout::GENERAL;
        self.prev_image = Some(current);

        // --- Free descriptor set ---
        self.device
            .free_descriptor_sets(self.descriptor_pool, &[ds])?;

        Ok(dirty)
    }

    /// Import a DMA-BUF fd as a VkImage + VkImageView.
    unsafe fn import_dmabuf(
        &self,
        fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<PrevFrame, Box<dyn std::error::Error>> {
        let size = (stride * height) as vk::DeviceSize;

        // Dup the fd: Vulkan takes ownership of the duplicated fd.
        let dup_fd = libc::dup(fd);
        if dup_fd < 0 {
            return Err(io::Error::last_os_error().into());
        }

        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dup_fd);

        let mem_props = self
            .instance
            .get_physical_device_memory_properties(self.physical_device);

        let image_mem_type = find_memory_type(
            &mem_props,
            u32::MAX,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .or_else(|| find_memory_type(&mem_props, u32::MAX, vk::MemoryPropertyFlags::empty()))
        .ok_or("no suitable memory type for DMA-BUF import")?;

        let mut alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(size)
            .memory_type_index(image_mem_type);
        alloc_info = alloc_info.push_next(&mut import_info);

        let memory = self.device.allocate_memory(&alloc_info, None)?;

        // Create VkImage backed by imported memory.
        // STORAGE usage so the compute shader can use imageLoad.
        let mut ext_mem_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::STORAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::PREINITIALIZED)
            .push_next(&mut ext_mem_info);

        let image = self.device.create_image(&image_ci, None)?;
        self.device.bind_image_memory(image, memory, 0)?;

        // Create VkImageView
        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .components(vk::ComponentMapping {
                r: vk::ComponentSwizzle::IDENTITY,
                g: vk::ComponentSwizzle::IDENTITY,
                b: vk::ComponentSwizzle::IDENTITY,
                a: vk::ComponentSwizzle::IDENTITY,
            })
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        let view = self.device.create_image_view(&view_ci, None)?;

        Ok(PrevFrame {
            image,
            memory,
            view,
            layout: vk::ImageLayout::PREINITIALIZED,
            width,
            height,
        })
    }

    /// Destroy all Vulkan resources held by a `PrevFrame`.
    unsafe fn destroy_prev_frame(&self, frame: PrevFrame) {
        self.device.destroy_image_view(frame.view, None);
        self.device.destroy_image(frame.image, None);
        self.device.free_memory(frame.memory, None);
    }
}

// ---------------------------------------------------------------------------
// Drop
// ---------------------------------------------------------------------------

impl Drop for GpuFrameProcessor {
    fn drop(&mut self) {
        unsafe {
            self.device.device_wait_idle().ok();

            if let Some(prev) = self.prev_image.take() {
                self.destroy_prev_frame(prev);
            }

            self.device.unmap_memory(self.sad_memory);
            self.device.destroy_buffer(self.sad_buffer, None);
            self.device.free_memory(self.sad_memory, None);

            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);
            self.device
                .destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device.destroy_shader_module(self.shader_module, None);
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find a memory type index matching the given type filter and property flags.
fn find_memory_type(
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    type_filter: u32,
    required_flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..mem_props.memory_type_count).find(|&i| {
        let suitable = (type_filter & (1 << i)) != 0;
        let has_flags = mem_props.memory_types[i as usize]
            .property_flags
            .contains(required_flags);
        suitable && has_flags
    })
}

/// Create a memfd of `size` bytes filled with `pixel` repeated, return the fd.
///
/// Helper shared by tests.
#[cfg(test)]
unsafe fn make_memfd(width: u32, height: u32, pixel: [u8; 4]) -> std::os::unix::io::RawFd {
    let stride = width * 4;
    let size = (stride * height) as usize;
    let name = std::ffi::CString::new("ghost-test").unwrap();
    let fd = libc::memfd_create(name.as_ptr(), 0);
    assert!(fd >= 0, "memfd_create failed");
    libc::ftruncate(fd, size as i64);

    let ptr = libc::mmap(
        std::ptr::null_mut(),
        size,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        fd,
        0,
    );
    assert_ne!(ptr, libc::MAP_FAILED);
    let slice = std::slice::from_raw_parts_mut(ptr as *mut u8, size);
    for chunk in slice.chunks_exact_mut(4) {
        chunk.copy_from_slice(&pixel);
    }
    libc::munmap(ptr, size);
    fd
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_dirty_tracker_creates_successfully() {
        match GpuFrameProcessor::new(2048) {
            Ok(tracker) => {
                assert!(tracker.max_tiles > 0);
            }
            Err(e) => {
                eprintln!("Skipping GPU diff test (no Vulkan GPU?): {e}");
            }
        }
    }

    #[test]
    fn identical_frames_produce_no_dirty_tiles() {
        let width = 64u32;
        let height = 64u32;
        let stride = width * 4;
        // Solid red pixel
        let pixel: [u8; 4] = [0, 0, 255, 255]; // BGRA: R

        let mut tracker = match GpuFrameProcessor::new(256) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Skipping identical_frames test (no Vulkan GPU?): {e}");
                return;
            }
        };

        unsafe {
            // First call: establishes prev, returns all tiles dirty.
            // memfds are not real DMA-BUFs; on a machine without DRM/GBM the
            // DMA_BUF_EXT import will fail — treat that as a graceful skip.
            let fd1 = make_memfd(width, height, pixel);
            let first = match tracker.diff(fd1, width, height, stride) {
                Ok(v) => v,
                Err(e) => {
                    libc::close(fd1);
                    eprintln!("Skipping identical_frames (memfd not a real DMA-BUF): {e}");
                    return;
                }
            };
            libc::close(fd1);

            // Expect all 4 tiles dirty (64/32 = 2 cols * 2 rows = 4 tiles)
            assert_eq!(first.len(), 4, "first frame should have all tiles dirty");

            // Second call with identical content: should have no dirty tiles
            let fd2 = make_memfd(width, height, pixel);
            let second = match tracker.diff(fd2, width, height, stride) {
                Ok(v) => v,
                Err(e) => {
                    libc::close(fd2);
                    eprintln!("Skipping identical_frames second diff: {e}");
                    return;
                }
            };
            libc::close(fd2);

            assert_eq!(
                second.len(),
                0,
                "identical frames should produce no dirty tiles, got: {second:?}"
            );
        }
    }

    #[test]
    fn changed_pixel_detected() {
        let width = 64u32;
        let height = 64u32;
        let stride = width * 4;

        let mut tracker = match GpuFrameProcessor::new(256) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Skipping changed_pixel test (no Vulkan GPU?): {e}");
                return;
            }
        };

        unsafe {
            // Frame 1: all black.
            // memfds are not real DMA-BUFs; on a machine without DRM/GBM the
            // DMA_BUF_EXT import will fail — treat that as a graceful skip.
            let black: [u8; 4] = [0, 0, 0, 255];
            let fd1 = make_memfd(width, height, black);
            let first = match tracker.diff(fd1, width, height, stride) {
                Ok(v) => v,
                Err(e) => {
                    libc::close(fd1);
                    eprintln!("Skipping changed_pixel (memfd not a real DMA-BUF): {e}");
                    return;
                }
            };
            libc::close(fd1);
            assert_eq!(first.len(), 4, "first frame: all tiles dirty");

            // Frame 2: tile (1,0) changed to white.
            // Tile (1,0) spans pixels x=[32..63], y=[0..31].
            // We change only those pixels to white.
            let size = (stride * height) as usize;
            let name = std::ffi::CString::new("ghost-test2").unwrap();
            let fd2 = libc::memfd_create(name.as_ptr(), 0);
            assert!(fd2 >= 0);
            libc::ftruncate(fd2, size as i64);

            let ptr = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd2,
                0,
            );
            assert_ne!(ptr, libc::MAP_FAILED);
            let frame = std::slice::from_raw_parts_mut(ptr as *mut u8, size);

            // Fill all black first
            for chunk in frame.chunks_exact_mut(4) {
                chunk.copy_from_slice(&black);
            }
            // Paint tile (col=1, row=0) white: x=[32..63], y=[0..31]
            for row in 0..32u32 {
                for col in 32..64u32 {
                    let offset = ((row * stride) + col * 4) as usize;
                    frame[offset..offset + 4].copy_from_slice(&[255u8, 255, 255, 255]);
                }
            }
            libc::munmap(ptr, size);

            let second = match tracker.diff(fd2, width, height, stride) {
                Ok(v) => v,
                Err(e) => {
                    libc::close(fd2);
                    eprintln!("Skipping changed_pixel second diff: {e}");
                    return;
                }
            };
            libc::close(fd2);

            // Only tile index 1 (col=1, row=0 → 0*2+1 = 1) should be dirty
            assert_eq!(
                second.len(),
                1,
                "only one tile should be dirty, got: {second:?}"
            );
            assert_eq!(second[0], 1, "dirty tile should be index 1 (col=1,row=0)");
        }
    }
}
