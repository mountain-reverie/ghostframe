//! GPU-accelerated dirty tile detection via Vulkan compute (SAD shader),
//! plus BGRA→NV12 conversion via a second compute shader with HOST_VISIBLE output.
//!
//! [`GpuFrameProcessor`] imports each frame as a DMA-BUF VkImage, runs:
//! 1. `tile_sad` compute: per-tile Sum of Absolute Differences for dirty detection.
//! 2. `bgra_to_nv12` compute: convert the frame to NV12 into a HOST_VISIBLE buffer.
//!
//! After [`process_frame`][GpuFrameProcessor::process_frame] returns,
//! [`FrameAnalysis::nv12_data`] points directly into GPU-managed system RAM
//! (HOST_VISIBLE | HOST_COHERENT memory, no DMA-BUF export needed).
//! The pointer is valid until the next call to `process_frame`.
//!
//! [`diff`][GpuFrameProcessor::diff] is a thin wrapper that calls `process_frame`
//! and returns only the dirty tile indices, preserving the old API.

use ash::vk;
use std::ffi::CStr;
use std::io;

// ---------------------------------------------------------------------------
// RAII guards for per-frame transient Vulkan resources
// ---------------------------------------------------------------------------
//
// `process_frame_with_imported` and `run_nv12_and_snapshot` allocate descriptor
// sets, command buffers, and fences per call. Without RAII guards, any `?`
// early-exit would leak those resources — and the descriptor pool has bounded
// capacity (`max_sets=2`), so a few leaked sets would brick subsequent frames.
//
// Each guard frees its resource on drop; for these per-frame transients the
// success path also goes through drop (no ownership transfer), so the
// previously-explicit cleanup at the end of the success path collapses to
// "let the guard drop normally at end of scope".

struct ScopedDescriptorSets<'a> {
    device: &'a ash::Device,
    pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
}

impl Drop for ScopedDescriptorSets<'_> {
    fn drop(&mut self) {
        if !self.sets.is_empty() {
            // SAFETY: sets/pool came from a successful allocate_descriptor_sets
            // on `self.device`; freeing on drop is the matching deallocation.
            // The pool was created with FREE_DESCRIPTOR_SET so this is legal.
            // Errors are ignored — there's nothing useful to do on a free
            // failure during teardown.
            unsafe {
                let _ = self.device.free_descriptor_sets(self.pool, &self.sets);
            }
        }
    }
}

struct ScopedCommandBuffers<'a> {
    device: &'a ash::Device,
    pool: vk::CommandPool,
    bufs: Vec<vk::CommandBuffer>,
}

impl Drop for ScopedCommandBuffers<'_> {
    fn drop(&mut self) {
        if !self.bufs.is_empty() {
            // SAFETY: bufs/pool came from allocate_command_buffers on
            // `self.device`. Free is the matching deallocation. Caller is
            // responsible for ensuring the buffers are no longer in use
            // (we only drop after wait_for_fences).
            unsafe {
                self.device.free_command_buffers(self.pool, &self.bufs);
            }
        }
    }
}

struct ScopedFence<'a> {
    device: &'a ash::Device,
    fence: vk::Fence,
}

impl Drop for ScopedFence<'_> {
    fn drop(&mut self) {
        if self.fence != vk::Fence::null() {
            // SAFETY: fence was created via create_fence on `self.device`.
            // Caller must wait_for_fences before dropping (we do).
            unsafe {
                self.device.destroy_fence(self.fence, None);
            }
        }
    }
}

/// Pixels-per-tile dimension (matches shader local_size_x/y).
const TILE_SIZE: u32 = 32;

/// SAD score above which a tile is considered dirty.
/// Chosen to ignore sub-pixel rounding noise while catching any visible change.
const SAD_THRESHOLD: u32 = 64;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Per-tile analysis output from the `tile_analysis.comp` shader.
///
/// Layout matches the std430 struct in `shaders/tile_analysis.comp`:
/// 80 bytes per tile, 4-byte aligned. `_pad` exists to push `colors`
/// to offset 16 in std430.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TileAnalysis {
    /// Distinct color count. 1..=16 means `colors` holds that many valid
    /// entries. `17` is the overflow sentinel — PalRLE-infeasible; the
    /// `colors` array is undefined.
    pub count: u32,
    /// Per-mille (0..=1000) fraction of pixels above the gradient threshold.
    pub edge_density_thou: u32,
    pub _pad: [u32; 2],
    /// Packed BGRA values: byte 0 = B, byte 1 = G, byte 2 = R, byte 3 = A.
    /// Order is slot-traversal order (not insertion order). Valid up to `count`.
    pub colors: [u32; 16],
}

/// Result of [`GpuFrameProcessor::process_frame`].
pub struct FrameAnalysis {
    /// Flat tile indices of tiles that changed since the previous frame.
    pub dirty_tiles: Vec<u32>,
    /// Pointer to the NV12 output buffer (Y plane at offset 0, UV at `nv12_uv_offset`).
    /// Valid until the next call to `process_frame`.
    pub nv12_data: *const u8,
    pub nv12_width: u32,
    pub nv12_height: u32,
    pub nv12_y_stride: u32,
    pub nv12_uv_stride: u32,
    pub nv12_uv_offset: u32,
    /// Pointer to the per-tile analysis buffer (one `TileAnalysis` per tile,
    /// row-major, `tile_y * cols + tile_x`). Valid until the next call to
    /// `process_frame`.
    pub tile_analysis: *const TileAnalysis,
    /// Number of entries reachable via `tile_analysis` (= cols × rows).
    pub tile_analysis_len: u32,
}

impl FrameAnalysis {
    pub fn tile_analysis_slice(&self) -> &[TileAnalysis] {
        if self.tile_analysis.is_null() || self.tile_analysis_len == 0 {
            return &[];
        }
        // SAFETY: pointer is into HOST_VISIBLE mapped GPU memory owned by
        // GpuFrameProcessor. The borrow checker prevents the returned slice
        // from outliving the &self borrow; the caller must additionally not
        // hold this FrameAnalysis across a subsequent process_frame() call,
        // which may recycle the underlying GPU buffer.
        unsafe { std::slice::from_raw_parts(self.tile_analysis, self.tile_analysis_len as usize) }
    }
}

// SAFETY: nv12_data is a pointer to GPU-managed HOST_VISIBLE memory.
// The FrameAnalysis is consumed before the next process_frame call so the
// data is stable. GpuFrameProcessor is used from a single tokio task.
unsafe impl Send for FrameAnalysis {}

/// Vulkan compute-based dirty tile tracker with integrated NV12 conversion.
///
/// Call [`process_frame`][GpuFrameProcessor::process_frame] each frame with
/// the DMA-BUF fd, resolution, and stride.  Returns a [`FrameAnalysis`] with
/// both the dirty tile list and a pointer to the NV12-converted frame data in
/// HOST_VISIBLE memory.
///
/// Call [`diff`][GpuFrameProcessor::diff] if you only need the dirty tiles.
pub struct GpuFrameProcessor {
    _entry: ash::Entry,
    instance: ash::Instance,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    #[allow(dead_code)]
    queue_family_index: u32,
    command_pool: vk::CommandPool,

    // SAD pipeline
    shader_module: vk::ShaderModule,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    descriptor_set_layout: vk::DescriptorSetLayout,

    // NV12 pipeline
    nv12_shader_module: vk::ShaderModule,
    nv12_pipeline: vk::Pipeline,
    nv12_pipeline_layout: vk::PipelineLayout,
    nv12_descriptor_set_layout: vk::DescriptorSetLayout,

    descriptor_pool: vk::DescriptorPool,
    prev_image: Option<PrevFrame>,

    // SAD output buffer (HOST_VISIBLE, persistently mapped)
    sad_buffer: vk::Buffer,
    sad_memory: vk::DeviceMemory,
    sad_ptr: *mut u32,
    max_tiles: u32,

    // NV12 output buffer (HOST_VISIBLE | HOST_COHERENT, persistently mapped)
    nv12_buffer: Option<NV12Buffer>,

    last_width: u32,
    last_height: u32,
}

struct NV12Buffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    /// Total byte size of the allocation.
    #[allow(dead_code)]
    size: usize,
    width: u32,
    height: u32,
    y_stride: u32,
    uv_stride: u32,
    /// Byte offset of the UV plane inside the buffer.
    uv_offset: u32,
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

        // --- SAD Shader module ---
        let sad_spv = include_bytes!("shaders/tile_sad.spv");
        let sad_spv_words = ash::util::read_spv(&mut std::io::Cursor::new(sad_spv.as_slice()))?;
        let shader_module_ci = vk::ShaderModuleCreateInfo::default().code(&sad_spv_words);
        let shader_module = device.create_shader_module(&shader_module_ci, None)?;

        // --- NV12 Shader module ---
        let nv12_spv = include_bytes!("shaders/bgra_to_nv12.spv");
        let nv12_spv_words = ash::util::read_spv(&mut std::io::Cursor::new(nv12_spv.as_slice()))?;
        let nv12_shader_ci = vk::ShaderModuleCreateInfo::default().code(&nv12_spv_words);
        let nv12_shader_module = device.create_shader_module(&nv12_shader_ci, None)?;

        // --- SAD Descriptor set layout ---
        // binding 0: STORAGE_IMAGE (current frame)
        // binding 1: STORAGE_IMAGE (prev frame)
        // binding 2: STORAGE_BUFFER (SAD output)
        let sad_bindings = [
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
        let dsl_ci = vk::DescriptorSetLayoutCreateInfo::default().bindings(&sad_bindings);
        let descriptor_set_layout = device.create_descriptor_set_layout(&dsl_ci, None)?;

        // --- SAD Pipeline layout ---
        // Push constants: 3 x u32 = 12 bytes (frame_width, frame_height, cols)
        let sad_push_range = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(12)];
        let sad_pipeline_layout_ci = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(std::slice::from_ref(&descriptor_set_layout))
            .push_constant_ranges(&sad_push_range);
        let pipeline_layout = device.create_pipeline_layout(&sad_pipeline_layout_ci, None)?;

        // --- SAD Compute pipeline ---
        let entry_name = c"main";
        let sad_stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(shader_module)
            .name(entry_name);
        let sad_compute_ci = vk::ComputePipelineCreateInfo::default()
            .stage(sad_stage)
            .layout(pipeline_layout);
        let pipelines = device
            .create_compute_pipelines(vk::PipelineCache::null(), &[sad_compute_ci], None)
            .map_err(|(_, e)| e)?;
        let pipeline = pipelines[0];

        // --- NV12 Descriptor set layout ---
        // binding 0: STORAGE_IMAGE (BGRA input)
        // binding 1: STORAGE_BUFFER (NV12 output)
        let nv12_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let nv12_dsl_ci =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&nv12_bindings);
        let nv12_descriptor_set_layout =
            device.create_descriptor_set_layout(&nv12_dsl_ci, None)?;

        // --- NV12 Pipeline layout ---
        // Push constants: 5 x u32 = 20 bytes (width, height, y_stride, uv_offset, uv_stride)
        let nv12_push_range = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(20)];
        let nv12_pipeline_layout_ci = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(std::slice::from_ref(&nv12_descriptor_set_layout))
            .push_constant_ranges(&nv12_push_range);
        let nv12_pipeline_layout =
            device.create_pipeline_layout(&nv12_pipeline_layout_ci, None)?;

        // --- NV12 Compute pipeline ---
        let nv12_stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(nv12_shader_module)
            .name(entry_name);
        let nv12_compute_ci = vk::ComputePipelineCreateInfo::default()
            .stage(nv12_stage)
            .layout(nv12_pipeline_layout);
        let nv12_pipelines = device
            .create_compute_pipelines(vk::PipelineCache::null(), &[nv12_compute_ci], None)
            .map_err(|(_, e)| e)?;
        let nv12_pipeline = nv12_pipelines[0];

        // --- Descriptor pool ---
        // 2 sets: SAD (2 STORAGE_IMAGE + 1 STORAGE_BUFFER) + NV12 (1 STORAGE_IMAGE + 1 STORAGE_BUFFER)
        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_IMAGE,
                descriptor_count: 3,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 2,
            },
        ];
        let dp_ci = vk::DescriptorPoolCreateInfo::default()
            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
            .max_sets(2)
            .pool_sizes(&pool_sizes);
        let descriptor_pool = device.create_descriptor_pool(&dp_ci, None)?;

        // --- SAD output buffer ---
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
            nv12_shader_module,
            nv12_pipeline,
            nv12_pipeline_layout,
            nv12_descriptor_set_layout,
            descriptor_pool,
            prev_image: None,
            sad_buffer,
            sad_memory,
            sad_ptr,
            max_tiles,
            nv12_buffer: None,
            last_width: 0,
            last_height: 0,
        })
    }
}

// ---------------------------------------------------------------------------
// NV12 buffer allocation
// ---------------------------------------------------------------------------

impl GpuFrameProcessor {
    /// Allocate (or re-allocate) the HOST_VISIBLE NV12 output buffer.
    unsafe fn ensure_nv12_buffer(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Check if already correctly sized.
        if let Some(ref b) = self.nv12_buffer {
            if b.width == width && b.height == height {
                return Ok(());
            }
            // Resolution changed — destroy old buffer.
            let old = self.nv12_buffer.take().unwrap();
            self.device.unmap_memory(old.memory);
            self.device.destroy_buffer(old.buffer, None);
            self.device.free_memory(old.memory, None);
        }

        // Y plane: width * height bytes, stride = width (packed).
        // UV plane: width * (height/2) bytes (interleaved U,V, half height).
        let y_stride = width;
        let uv_stride = width; // interleaved U,V, so stride = width bytes
        let uv_offset = y_stride * height; // UV follows immediately after Y
        let total = (uv_offset + uv_stride * height.div_ceil(2)) as vk::DeviceSize;

        let buf_ci = vk::BufferCreateInfo::default()
            .size(total)
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = self.device.create_buffer(&buf_ci, None)?;
        let reqs = self.device.get_buffer_memory_requirements(buffer);

        let mem_props = self
            .instance
            .get_physical_device_memory_properties(self.physical_device);

        let mem_type = find_memory_type(
            &mem_props,
            reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or("no host-visible memory type for NV12 buffer")?;

        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(mem_type);
        let memory = self.device.allocate_memory(&alloc, None)?;
        self.device.bind_buffer_memory(buffer, memory, 0)?;

        let ptr = self.device.map_memory(memory, 0, total, vk::MemoryMapFlags::empty())? as *mut u8;

        self.nv12_buffer = Some(NV12Buffer {
            buffer,
            memory,
            ptr,
            size: total as usize,
            width,
            height,
            y_stride,
            uv_stride,
            uv_offset,
        });

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// process_frame() — main public API
// ---------------------------------------------------------------------------

impl GpuFrameProcessor {
    /// Process a frame: run SAD dirty detection AND BGRA→NV12 conversion.
    ///
    /// Returns [`FrameAnalysis`] with dirty tile indices and a pointer to the
    /// NV12 data in HOST_VISIBLE GPU memory. The pointer is valid until the
    /// next call to `process_frame`.
    ///
    /// On the first call (no previous frame), all tiles are returned as dirty.
    /// The `fd` is **not** consumed; this function dups it internally.
    pub fn process_frame(
        &mut self,
        fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<FrameAnalysis, Box<dyn std::error::Error>> {
        unsafe { self.process_frame_inner(fd, width, height, stride) }
    }

    unsafe fn process_frame_inner(
        &mut self,
        fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<FrameAnalysis, Box<dyn std::error::Error>> {
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

        // Ensure the NV12 output buffer is allocated at the right size.
        self.ensure_nv12_buffer(width, height)?;

        // Import DMA-BUF as VkImage. This is transient: we destroy it at the
        // end of this function. We never reuse it as the SAD `prev_image`
        // because PRIME-exported FBs share physical memory across imports
        // (active scanout BO doesn't change between captures), so a previous
        // import would always be byte-identical to a fresh one and SAD = 0.
        let current = self.import_dmabuf(fd, width, height, stride)?;
        // Result must always destroy `current` at end. Use a closure-style
        // cleanup by carrying it forward and explicitly destroying.
        let result = self.process_frame_with_imported(
            &current,
            width,
            height,
            tile_count,
            cols,
            rows,
        );
        // Always clean up the imported DMA-BUF VkImage (transient).
        self.destroy_prev_frame(current);
        result
    }

    /// Run SAD + NV12 + snapshot-copy against an already-imported DMA-BUF
    /// VkImage. Splits out the destruction of `current` from
    /// [`process_frame_inner`] so the cleanup path is bullet-proof regardless
    /// of which step errors out.
    unsafe fn process_frame_with_imported(
        &mut self,
        current: &PrevFrame,
        width: u32,
        height: u32,
        tile_count: u32,
        cols: u32,
        rows: u32,
    ) -> Result<FrameAnalysis, Box<dyn std::error::Error>> {
        let nv12 = self.nv12_buffer.as_ref().unwrap();
        let (nv12_buffer, nv12_y_stride, nv12_uv_stride, nv12_uv_offset) =
            (nv12.buffer, nv12.y_stride, nv12.uv_stride, nv12.uv_offset);
        let nv12_ptr = nv12.ptr;

        // --- First frame: no SAD, mark all dirty, run NV12, copy current →
        // newly-allocated owned snapshot for next frame's SAD source. ---
        if self.prev_image.is_none() {
            let all_dirty: Vec<u32> = (0..tile_count).collect();

            // Allocate the owned snapshot image once. It persists for the
            // lifetime of the processor (until resolution changes).
            let snapshot = self.allocate_owned_image(width, height)?;

            // Run NV12 conversion AND snapshot copy in one cmd buffer. The
            // snapshot is what we will compare future frames against.
            self.run_nv12_and_snapshot(
                current,
                &snapshot,
                width,
                height,
                nv12_buffer,
                nv12_y_stride,
                nv12_uv_offset,
                nv12_uv_stride,
            )?;

            let mut snap = snapshot;
            snap.layout = vk::ImageLayout::GENERAL;
            self.prev_image = Some(snap);

            return Ok(FrameAnalysis {
                dirty_tiles: all_dirty,
                nv12_data: nv12_ptr,
                nv12_width: width,
                nv12_height: height,
                nv12_y_stride,
                nv12_uv_stride,
                nv12_uv_offset,
                tile_analysis: std::ptr::null(),
                tile_analysis_len: 0,
            });
        }

        // --- Subsequent frames: both SAD and NV12 in one command buffer ---
        // Pull the fields we need from prev_image up-front so we don't hold a
        // borrow on `self.prev_image` while we mutably touch other parts of
        // `self`. prev_image is owned (allocate_owned_image) and persistent —
        // its `image`/`view` handles stay valid until `prev_image` is replaced
        // (only on resolution change), so caching them by value is safe within
        // this call.
        let prev_image = self.prev_image.as_ref().unwrap().image;
        let prev_view = self.prev_image.as_ref().unwrap().view;
        let prev_layout = self.prev_image.as_ref().unwrap().layout;

        // Allocate descriptor sets for both passes. Wrap in RAII guards so any
        // subsequent `?` early-exit frees the sets back to the bounded pool.
        let sad_set_layouts = [self.descriptor_set_layout];
        let sad_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&sad_set_layouts);
        let sad_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&sad_ds_alloc)?,
        };
        let sad_ds = sad_ds_guard.sets[0];

        let nv12_set_layouts = [self.nv12_descriptor_set_layout];
        let nv12_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&nv12_set_layouts);
        let nv12_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&nv12_ds_alloc)?,
        };
        let nv12_ds = nv12_ds_guard.sets[0];

        // Write SAD descriptor set.
        let current_image_info = [vk::DescriptorImageInfo::default()
            .image_view(current.view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let prev_image_info = [vk::DescriptorImageInfo::default()
            .image_view(prev_view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let sad_buffer_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.sad_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let sad_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(sad_ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&current_image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(sad_ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&prev_image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(sad_ds)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&sad_buffer_info),
        ];
        self.device.update_descriptor_sets(&sad_writes, &[]);

        // Write NV12 descriptor set.
        let nv12_image_info = [vk::DescriptorImageInfo::default()
            .image_view(current.view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let nv12_buffer_info = [vk::DescriptorBufferInfo::default()
            .buffer(nv12_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let nv12_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(nv12_ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&nv12_image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(nv12_ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&nv12_buffer_info),
        ];
        self.device.update_descriptor_sets(&nv12_writes, &[]);

        // --- Command buffer (RAII-guarded) ---
        let cmd_alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cmd_guard = ScopedCommandBuffers {
            device: &self.device,
            pool: self.command_pool,
            bufs: self.device.allocate_command_buffers(&cmd_alloc)?,
        };
        let cmd = cmd_guard.bufs[0];

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        self.device.begin_command_buffer(cmd, &begin_info)?;

        // 1. Image barriers: current (PREINITIALIZED→GENERAL) + prev (layout→GENERAL)
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
                .old_layout(prev_layout)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(prev_image)
                .subresource_range(subresource_range)
                .src_access_mask(if prev_layout == vk::ImageLayout::PREINITIALIZED {
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

        // 2. SAD dispatch
        self.device
            .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.pipeline_layout,
            0,
            &[sad_ds],
            &[],
        );
        let sad_push: [u32; 3] = [width, height, cols];
        let sad_push_bytes = std::slice::from_raw_parts(
            sad_push.as_ptr() as *const u8,
            std::mem::size_of_val(&sad_push),
        );
        self.device.cmd_push_constants(
            cmd,
            self.pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            sad_push_bytes,
        );
        self.device.cmd_dispatch(cmd, cols, rows, 1);

        // 3. No barrier needed between SAD and NV12 dispatches:
        //    - both read `current.image` (read-after-read is hazard-free),
        //    - SAD writes `sad_buffer` and NV12 writes `nv12_buffer` — disjoint
        //      regions, no overlap.
        //    A previous version inserted an empty execution-only
        //    COMPUTE→COMPUTE barrier here; per Vulkan 1.1 spec §6.1
        //    (Execution and Memory Dependencies), an execution barrier with no
        //    memory/buffer/image barriers and no access masks introduces no
        //    memory dependency, and same-queue same-stage sequential dispatches
        //    already have implicit execution ordering — so it was a no-op.
        //    The HOST-readback barrier at step 5 covers both buffers.

        // 4. NV12 dispatch
        self.device
            .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.nv12_pipeline);
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.nv12_pipeline_layout,
            0,
            &[nv12_ds],
            &[],
        );
        // Push constants: width, height, y_stride, uv_offset, uv_stride (5 x u32 = 20 bytes)
        let nv12_push: [u32; 5] = [width, height, nv12_y_stride, nv12_uv_offset, nv12_uv_stride];
        let nv12_push_bytes = std::slice::from_raw_parts(
            nv12_push.as_ptr() as *const u8,
            std::mem::size_of_val(&nv12_push),
        );
        self.device.cmd_push_constants(
            cmd,
            self.nv12_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            nv12_push_bytes,
        );
        // NV12 shader: workgroup = 2x2 pixels, dispatch = (width/2, height/2, 1)
        let nv12_groups_x = width.div_ceil(2);
        let nv12_groups_y = height.div_ceil(2);
        self.device.cmd_dispatch(cmd, nv12_groups_x, nv12_groups_y, 1);

        // 5a. Snapshot copy: current (DMA-BUF) → prev_image (owned). This
        // makes prev_image a true point-in-time snapshot of THIS frame for
        // the next frame's SAD comparison. Must run AFTER both SAD and NV12
        // shader reads, since they read from current.
        let copy_barriers = [
            // current: SHADER_READ → TRANSFER_READ
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(current.image)
                .subresource_range(subresource_range)
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ),
            // prev_image: SHADER_READ → TRANSFER_WRITE
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(prev_image)
                .subresource_range(subresource_range)
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE),
        ];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &copy_barriers,
        );

        let copy_region = [vk::ImageCopy::default()
            .src_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .src_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .dst_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .dst_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })];
        self.device.cmd_copy_image(
            cmd,
            current.image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            prev_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &copy_region,
        );

        // 5b. Restore prev_image to GENERAL for next frame's SAD pass.
        let post_copy_barrier = [vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(prev_image)
            .subresource_range(subresource_range)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &post_copy_barrier,
        );

        // 5. Barriers: SAD buffer → HOST_READ; NV12 buffer → HOST_READ
        let buf_barriers = [
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.sad_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(nv12_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
        ];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            &buf_barriers,
            &[],
        );

        self.device.end_command_buffer(cmd)?;

        // 6. Submit and wait. Fence is RAII-guarded so an early `?` can't leak it.
        let fence_ci = vk::FenceCreateInfo::default();
        let fence_guard = ScopedFence {
            device: &self.device,
            fence: self.device.create_fence(&fence_ci, None)?,
        };
        let submit_info = vk::SubmitInfo::default().command_buffers(&cmd_guard.bufs);
        self.device
            .queue_submit(self.queue, &[submit_info], fence_guard.fence)?;
        self.device
            .wait_for_fences(&[fence_guard.fence], true, u64::MAX)?;

        // 7. Read SAD values
        let sad_slice = std::slice::from_raw_parts(self.sad_ptr, tile_count as usize);
        let dirty: Vec<u32> = (0..tile_count)
            .filter(|&i| sad_slice[i as usize] > SAD_THRESHOLD)
            .collect();

        // Diagnostic: SAD distribution. Helps diagnose "no dirty tiles ever" cases
        // where the FB exported via PRIME isn't seeing Xorg renders (PageFlip,
        // ShadowFB-not-flushing, DMA-BUF coherency, etc.).
        if tracing::enabled!(tracing::Level::DEBUG) {
            let max_sad = sad_slice.iter().copied().max().unwrap_or(0);
            let nonzero = sad_slice.iter().filter(|&&s| s > 0).count();
            tracing::debug!(
                tile_count,
                dirty_count = dirty.len(),
                max_sad,
                nonzero_sad_tiles = nonzero,
                threshold = SAD_THRESHOLD,
                "SAD pass complete"
            );
        }

        // 8. Per-frame transients (descriptor sets, command buffer, fence) are
        //    freed when their RAII guards drop at end of scope. We touch the
        //    guards here to keep them alive past the `wait_for_fences` above.
        let _ = (&sad_ds_guard, &nv12_ds_guard, &cmd_guard, &fence_guard);

        // 9. prev_image is persistent (own-allocated, layout still GENERAL
        // after the post-copy barrier). It now holds a snapshot of THIS frame
        // for next frame's SAD comparison. The imported `current` is destroyed
        // by the caller (process_frame_inner).
        if let Some(prev_mut) = self.prev_image.as_mut() {
            prev_mut.layout = vk::ImageLayout::GENERAL;
        }

        Ok(FrameAnalysis {
            dirty_tiles: dirty,
            nv12_data: nv12_ptr,
            nv12_width: width,
            nv12_height: height,
            nv12_y_stride,
            nv12_uv_stride,
            nv12_uv_offset,
            tile_analysis: std::ptr::null(),
            tile_analysis_len: 0,
        })
    }

    /// First-frame helper: NV12 conversion + snapshot copy.
    ///
    /// On the first frame we have no SAD comparison to do (no prev), so we
    /// only run NV12 and then `cmdCopyImage(current → snapshot)` to seed
    /// `snapshot` with this frame's content for subsequent SAD passes.
    ///
    /// `snapshot` is always freshly allocated by the caller
    /// (`allocate_owned_image`), so its starting layout is
    /// `vk::ImageLayout::UNDEFINED` — we hardcode that transition.
    #[allow(clippy::too_many_arguments)]
    unsafe fn run_nv12_and_snapshot(
        &self,
        current: &PrevFrame,
        snapshot: &PrevFrame,
        width: u32,
        height: u32,
        nv12_buffer: vk::Buffer,
        nv12_y_stride: u32,
        nv12_uv_offset: u32,
        nv12_uv_stride: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Allocate NV12 descriptor set, RAII-guarded so any subsequent `?`
        // early-exit returns the set to the bounded pool.
        let nv12_set_layouts = [self.nv12_descriptor_set_layout];
        let nv12_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&nv12_set_layouts);
        let nv12_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&nv12_ds_alloc)?,
        };
        let nv12_ds = nv12_ds_guard.sets[0];

        let nv12_image_info = [vk::DescriptorImageInfo::default()
            .image_view(current.view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let nv12_buf_info = [vk::DescriptorBufferInfo::default()
            .buffer(nv12_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let nv12_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(nv12_ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&nv12_image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(nv12_ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&nv12_buf_info),
        ];
        self.device.update_descriptor_sets(&nv12_writes, &[]);

        let cmd_alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cmd_guard = ScopedCommandBuffers {
            device: &self.device,
            pool: self.command_pool,
            bufs: self.device.allocate_command_buffers(&cmd_alloc)?,
        };
        let cmd = cmd_guard.bufs[0];

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        self.device.begin_command_buffer(cmd, &begin_info)?;

        let subresource_range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        // Transition current image PREINITIALIZED → GENERAL
        let img_barrier = [vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::PREINITIALIZED)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(current.image)
            .subresource_range(subresource_range)
            .src_access_mask(vk::AccessFlags::HOST_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::HOST,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &img_barrier,
        );

        // NV12 dispatch
        self.device
            .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.nv12_pipeline);
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.nv12_pipeline_layout,
            0,
            &[nv12_ds],
            &[],
        );
        let nv12_push: [u32; 5] = [width, height, nv12_y_stride, nv12_uv_offset, nv12_uv_stride];
        let nv12_push_bytes = std::slice::from_raw_parts(
            nv12_push.as_ptr() as *const u8,
            std::mem::size_of_val(&nv12_push),
        );
        self.device.cmd_push_constants(
            cmd,
            self.nv12_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            nv12_push_bytes,
        );
        let nv12_groups_x = width.div_ceil(2);
        let nv12_groups_y = height.div_ceil(2);
        self.device.cmd_dispatch(cmd, nv12_groups_x, nv12_groups_y, 1);

        // Snapshot copy: current (GENERAL) → snapshot (TRANSFER_DST_OPTIMAL).
        // `snapshot` is always freshly allocated by `allocate_owned_image` and
        // its initial layout is UNDEFINED, so we hardcode that transition (no
        // prior shader access to wait on).
        let snap_barriers = [
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(current.image)
                .subresource_range(subresource_range)
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ),
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(snapshot.image)
                .subresource_range(subresource_range)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE),
        ];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &snap_barriers,
        );

        let copy_region = [vk::ImageCopy::default()
            .src_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .src_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .dst_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .dst_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })];
        self.device.cmd_copy_image(
            cmd,
            current.image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            snapshot.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &copy_region,
        );

        // Restore snapshot to GENERAL for next frame's SAD pass.
        let post_copy_barrier = [vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(snapshot.image)
            .subresource_range(subresource_range)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &post_copy_barrier,
        );

        // NV12 buffer → HOST_READ
        let buf_barrier = [vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(nv12_buffer)
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

        // Fence is RAII-guarded so an early `?` can't leak it.
        let fence_ci = vk::FenceCreateInfo::default();
        let fence_guard = ScopedFence {
            device: &self.device,
            fence: self.device.create_fence(&fence_ci, None)?,
        };
        let submit_info = vk::SubmitInfo::default().command_buffers(&cmd_guard.bufs);
        self.device
            .queue_submit(self.queue, &[submit_info], fence_guard.fence)?;
        self.device
            .wait_for_fences(&[fence_guard.fence], true, u64::MAX)?;

        // Per-frame transients (descriptor set, command buffer, fence) are
        // freed when their RAII guards drop at end of scope. Touch them here
        // to keep them alive past `wait_for_fences`.
        let _ = (&nv12_ds_guard, &cmd_guard, &fence_guard);

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// diff() — backward-compatible wrapper
// ---------------------------------------------------------------------------

impl GpuFrameProcessor {
    /// Compare the DMA-BUF frame at `fd` against the previous frame.
    ///
    /// Returns flat tile indices of dirty tiles.  On the first call (no
    /// previous frame), all tiles are returned as dirty.
    ///
    /// This is a thin wrapper around [`process_frame`][Self::process_frame]
    /// that discards the NV12 output.
    ///
    /// The `fd` is **not** consumed; this function dups it internally.
    pub fn diff(
        &mut self,
        fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
        let analysis = self.process_frame(fd, width, height, stride)?;
        Ok(analysis.dirty_tiles)
    }
}

// ---------------------------------------------------------------------------
// DMA-BUF import helpers
// ---------------------------------------------------------------------------

impl GpuFrameProcessor {
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

    /// Allocate an own (non-DMA-BUF, DEVICE_LOCAL) `B8G8R8A8_UNORM` image used
    /// as the persistent "previous frame" snapshot for SAD comparison.
    ///
    /// Why this exists: the live framebuffer exported via PRIME has unstable
    /// memory semantics — every `import_dmabuf()` call returns VkImages backed
    /// by the SAME physical memory (the active scanout buffer that Xorg/KMS
    /// keeps writing to). Using one of those imports as `prev_image` makes
    /// SAD compare two views of the SAME bytes at SAME instant → SAD always 0
    /// → no dirty tiles ever detected. We instead `cmdCopyImage(current →
    /// prev_snapshot)` after each frame to capture a true point-in-time
    /// snapshot independent of the live FB.
    unsafe fn allocate_owned_image(
        &self,
        width: u32,
        height: u32,
    ) -> Result<PrevFrame, Box<dyn std::error::Error>> {
        // STORAGE so SAD shader can imageLoad; TRANSFER_DST so we can
        // cmdCopyImage from the imported DMA-BUF into it; OPTIMAL tiling
        // for best read perf (we never CPU-map this image).
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
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        let image = self.device.create_image(&image_ci, None)?;
        let mem_reqs = self.device.get_image_memory_requirements(image);

        let mem_props = self
            .instance
            .get_physical_device_memory_properties(self.physical_device);
        let mem_type = find_memory_type(
            &mem_props,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .or_else(|| find_memory_type(&mem_props, mem_reqs.memory_type_bits, vk::MemoryPropertyFlags::empty()))
        .ok_or("no suitable memory type for owned snapshot image")?;

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mem_type);
        let memory = self.device.allocate_memory(&alloc_info, None)?;
        self.device.bind_image_memory(image, memory, 0)?;

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
            layout: vk::ImageLayout::UNDEFINED,
            width,
            height,
        })
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

            // NV12 buffer
            if let Some(nv12) = self.nv12_buffer.take() {
                self.device.unmap_memory(nv12.memory);
                self.device.destroy_buffer(nv12.buffer, None);
                self.device.free_memory(nv12.memory, None);
            }

            // SAD resources
            self.device.unmap_memory(self.sad_memory);
            self.device.destroy_buffer(self.sad_buffer, None);
            self.device.free_memory(self.sad_memory, None);

            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);

            // NV12 pipeline
            self.device
                .destroy_pipeline(self.nv12_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.nv12_pipeline_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.nv12_descriptor_set_layout, None);
            self.device
                .destroy_shader_module(self.nv12_shader_module, None);

            // SAD pipeline
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

    /// Test that process_frame returns NV12 data with correct pointer and dimensions.
    #[test]
    fn process_frame_returns_nv12_data() {
        let width = 64u32;
        let height = 64u32;
        let stride = width * 4;
        // Solid red in BGRA: B=0, G=0, R=255, A=255
        let pixel: [u8; 4] = [0, 0, 255, 255];

        let mut processor = match GpuFrameProcessor::new(256) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Skipping process_frame_returns_nv12_data (no Vulkan GPU?): {e}");
                return;
            }
        };

        unsafe {
            let fd = make_memfd(width, height, pixel);
            let analysis = match processor.process_frame(fd, width, height, stride) {
                Ok(a) => a,
                Err(e) => {
                    libc::close(fd);
                    eprintln!("Skipping process_frame_returns_nv12_data (memfd not a real DMA-BUF): {e}");
                    return;
                }
            };
            libc::close(fd);

            // Verify dimensions
            assert_eq!(analysis.nv12_width, width);
            assert_eq!(analysis.nv12_height, height);
            assert_eq!(analysis.nv12_y_stride, width);
            assert_eq!(analysis.nv12_uv_stride, width);
            assert_eq!(analysis.nv12_uv_offset, width * height);

            // First frame: all tiles dirty
            assert_eq!(analysis.dirty_tiles.len(), 4, "first frame: 4 tiles dirty");

            // Pointer must not be null
            assert!(!analysis.nv12_data.is_null(), "nv12_data pointer should not be null");

            // Verify Y values for solid red (R=1.0, G=0, B=0):
            // Y = 0.299*1 + 0.587*0 + 0.114*0 = 0.299 → ~76
            let y_slice = std::slice::from_raw_parts(analysis.nv12_data, (width * height) as usize);
            let y_avg: u32 = y_slice.iter().map(|&b| b as u32).sum::<u32>() / y_slice.len() as u32;
            // Allow generous tolerance for GPU rounding differences
            assert!(
                y_avg > 50 && y_avg < 100,
                "Y for solid red should be ~76, got average {y_avg}"
            );
        }
    }

    #[test]
    fn tile_analysis_struct_has_expected_layout() {
        assert_eq!(std::mem::size_of::<TileAnalysis>(), 80, "TileAnalysis must be 80 bytes");
        assert_eq!(std::mem::align_of::<TileAnalysis>(), 4, "TileAnalysis alignment");

        // Offsets match std430 layout from the shader.
        let zero = TileAnalysis { count: 0, edge_density_thou: 0, _pad: [0; 2], colors: [0; 16] };
        let base = &zero as *const _ as usize;
        assert_eq!(&zero.count             as *const _ as usize - base, 0);
        assert_eq!(&zero.edge_density_thou as *const _ as usize - base, 4);
        assert_eq!(&zero.colors            as *const _ as usize - base, 16);
    }

    #[test]
    fn frame_analysis_tile_analysis_slice_returns_correct_range() {
        // Build a fake FrameAnalysis backed by a heap Vec so we can exercise the
        // slice helper without spinning up Vulkan.
        let mut backing = vec![
            TileAnalysis { count: 1, edge_density_thou: 100, _pad: [0; 2], colors: [0xAAAAAAAAu32; 16] },
            TileAnalysis { count: 2, edge_density_thou: 200, _pad: [0; 2], colors: [0xBBBBBBBBu32; 16] },
        ];
        let analysis = FrameAnalysis {
            dirty_tiles: vec![],
            nv12_data: std::ptr::null(),
            nv12_width: 0, nv12_height: 0,
            nv12_y_stride: 0, nv12_uv_stride: 0, nv12_uv_offset: 0,
            tile_analysis: backing.as_mut_ptr() as *const TileAnalysis,
            tile_analysis_len: 2,
        };
        let slice = analysis.tile_analysis_slice();
        assert_eq!(slice.len(), 2);
        assert_eq!(slice[0].count, 1);
        assert_eq!(slice[1].edge_density_thou, 200);
        assert_eq!(slice[1].colors[0], 0xBBBBBBBBu32);
    }
}
