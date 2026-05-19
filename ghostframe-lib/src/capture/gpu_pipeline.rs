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

use super::pipeline_builder::{self, BindingSpec};

// ---------------------------------------------------------------------------
// Pipeline-wide constants
// ---------------------------------------------------------------------------
//
// These values are coupled to the SPIR-V/WGSL shaders under `shaders/`. Any
// change here requires the matching shader update and a re-build of the
// `.spv` artefacts.

/// Slots in the per-frame palette set (and the same-sized `folded_into`
/// array). The shaders address palettes by `slot ∈ [0, PALETTE_HASH_SLOTS)`.
const PALETTE_HASH_SLOTS: usize = 256;

/// Bytes per tile in the nibble-packed index buffer:
/// 32×32 pixels × 4 bits/pixel = 512 bytes per tile.
const PER_TILE_INDEX_BYTES: vk::DeviceSize = 512;

/// Byte size of a buffer that holds one `u32` per palette slot.
/// Used for `hash_table_buffer` (open-addressed slot pointers) and
/// `folded_into_buffer` (resolved fold-into IDs).
const PALETTE_SLOT_U32_BYTES: vk::DeviceSize = (PALETTE_HASH_SLOTS as vk::DeviceSize) * 4;

// ---------------------------------------------------------------------------
// RAII guards for per-frame transient Vulkan resources
// ---------------------------------------------------------------------------
//
// `process_frame_with_imported` and `run_first_frame_passes` allocate descriptor
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

/// std430 mirror of `palette_fold.comp` `PaletteEntry`: 80 bytes,
/// 16-byte aligned (count + 3 pad + 16 packed BGRA colors).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramePaletteEntryRaw {
    pub count: u32,
    pub _pad: [u32; 3],
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
    /// Pointer to the compact list of (dirty AND PalRLE-feasible) tile indices.
    /// Valid until next process_frame call. Read `palrle_compact_count` entries.
    pub palrle_compact_list: *const u32,
    /// Number of valid entries in `palrle_compact_list`.
    pub palrle_compact_count: u32,
    /// Pointer to the per-frame palette set (Stage 2a output).
    /// 256 slots of `FramePaletteEntryRaw`; only entries 0..frame_palette_set_count
    /// whose hash slot was claimed contain valid data.
    /// Valid until next process_frame call.
    pub frame_palette_set: *const FramePaletteEntryRaw,
    /// Number of unique palettes in this frame.
    pub frame_palette_set_count: u32,
    /// Pointer to per-tile frame palette ID array (Stage 2a output).
    /// Length = `palrle_compact_count`. Each byte is the frame palette slot
    /// index for the corresponding compact tile.
    /// Valid until next process_frame call.
    pub per_tile_frame_palette_id: *const u8,
    /// Pointer to the Stage 2b folded_into array (256 entries of u32).
    /// Each entry encodes the best strict superset found for that palette slot:
    /// `((255 - count_B) << 8) | id_B`, or `(255 << 8) | self_id` (default-self)
    /// when no superset was found. Extract the resolved palette id via `entry & 0xFF`.
    /// Null on the first frame. Valid until next process_frame call.
    pub folded_into: *const u32,
    /// Pointer to the Stage 3 index buffer (packed 4-bit palette indices).
    /// Each compact slot `c` occupies 512 bytes at offset `c * 512`.
    /// Only slots 0..palrle_compact_count have valid data; other slots retain
    /// stale bytes from prior frames. Null on the first frame.
    /// Valid until next process_frame call.
    pub index_buffer: *const u8,
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

    pub fn palrle_compact_list_slice(&self) -> &[u32] {
        if self.palrle_compact_list.is_null() || self.palrle_compact_count == 0 {
            return &[];
        }
        // SAFETY: pointer is into HOST_VISIBLE mapped GPU memory owned by
        // GpuFrameProcessor. The borrow checker prevents the returned slice
        // from outliving the &self borrow; the caller must not hold this
        // FrameAnalysis across a subsequent process_frame() call.
        unsafe {
            std::slice::from_raw_parts(self.palrle_compact_list, self.palrle_compact_count as usize)
        }
    }

    /// Returns the full 256-entry frame palette set buffer (the entire hash
    /// table). Entries are HASH-INDEXED, not dense — the shader writes a
    /// palette into `slot = (hash + probe) & 0xFFu`, so slot positions
    /// reachable by `per_tile_frame_palette_id` are valid; other positions
    /// contain stale bytes from previous frames.
    ///
    /// Callers must NEVER iterate this slice linearly. Index into it using
    /// values obtained from `per_tile_frame_palette_id_slice()` (Task 12+
    /// will also consult `folded_into`). The `frame_palette_set_count`
    /// field is the high-water mark of claimed slot IDs and is informational
    /// only (useful for stats / observability).
    pub fn frame_palette_set_slice(&self) -> &[FramePaletteEntryRaw] {
        if self.frame_palette_set.is_null() {
            return &[];
        }
        // SAFETY: same lifetime contract as tile_analysis_slice. The
        // PALETTE_HASH_SLOTS entries are the entire allocated hash-table
        // buffer, owned by GpuFrameProcessor and stable until the next
        // process_frame call.
        unsafe {
            std::slice::from_raw_parts(self.frame_palette_set, PALETTE_HASH_SLOTS)
        }
    }

    pub fn per_tile_frame_palette_id_slice(&self) -> &[u8] {
        if self.per_tile_frame_palette_id.is_null() || self.palrle_compact_count == 0 {
            return &[];
        }
        // SAFETY: pointer is into HOST_VISIBLE mapped GPU memory owned by
        // GpuFrameProcessor. The borrow checker prevents the returned slice
        // from outliving the &self borrow; the caller must not hold this
        // FrameAnalysis across a subsequent process_frame() call.
        // Length = palrle_compact_count (one byte per compact slot).
        unsafe {
            std::slice::from_raw_parts(
                self.per_tile_frame_palette_id,
                self.palrle_compact_count as usize,
            )
        }
    }

    /// Returns the full `PALETTE_HASH_SLOTS`-entry folded_into array. Each entry is
    /// `((255 - count_B) << 8) | id_B` for a slot that found a strict
    /// superset, or `(255 << 8) | self_id` (default-self) otherwise.
    /// Callers extract the resolved palette id via `entry & 0xFF`.
    pub fn folded_into_slice(&self) -> &[u32] {
        if self.folded_into.is_null() {
            return &[];
        }
        // SAFETY: same lifetime as other GPU-pointer slice helpers.
        unsafe { std::slice::from_raw_parts(self.folded_into, PALETTE_HASH_SLOTS) }
    }

    /// Returns the `PER_TILE_INDEX_BYTES` (512) nibble-packed index bytes for compact slot `c`.
    /// Only c values in `0..palrle_compact_count` have meaningful data;
    /// other slots may contain stale bytes from prior frames.
    ///
    /// Returns an empty slice (rather than panicking) if `c` is at or past
    /// `palrle_compact_count`. Callers should iterate `0..palrle_compact_count`
    /// to get meaningful data.
    pub fn index_buffer_slice_for(&self, c: usize) -> &[u8] {
        if self.index_buffer.is_null() {
            return &[];
        }
        if (c as u32) >= self.palrle_compact_count {
            return &[];
        }
        // SAFETY: same lifetime contract as other slice helpers; c is
        // bounded by palrle_compact_count which is bounded by max_tiles
        // by GPU shader contract.
        let stride = PER_TILE_INDEX_BYTES as usize;
        unsafe { std::slice::from_raw_parts(self.index_buffer.add(c * stride), stride) }
    }
}

// SAFETY: `nv12_data`, `tile_analysis`, `palrle_compact_list`,
// `frame_palette_set`, `per_tile_frame_palette_id`, `folded_into`, and
// `index_buffer` are all pointers to GPU-managed HOST_VISIBLE memory owned
// by `GpuFrameProcessor`. The FrameAnalysis is consumed before the next
// `process_frame` call so the data is stable. GpuFrameProcessor is used
// from a single tokio task.
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

    // Tile-analysis pipeline (new)
    analysis_shader_module: vk::ShaderModule,
    analysis_pipeline: vk::Pipeline,
    analysis_pipeline_layout: vk::PipelineLayout,
    analysis_descriptor_set_layout: vk::DescriptorSetLayout,
    // HOST_VISIBLE | HOST_COHERENT, persistently mapped. Layout matches
    // `TileAnalysis` (80 bytes per tile).
    analysis_buffer: vk::Buffer,
    analysis_memory: vk::DeviceMemory,
    analysis_ptr: *mut TileAnalysis,

    // PalRLE compact pipeline (Stage 1.5)
    palrle_compact_shader_module: vk::ShaderModule,
    palrle_compact_pipeline: vk::Pipeline,
    palrle_compact_pipeline_layout: vk::PipelineLayout,
    palrle_compact_descriptor_set_layout: vk::DescriptorSetLayout,
    // HOST_VISIBLE | HOST_COHERENT, persistently mapped. One u32 per tile.
    palrle_compact_list_buffer: vk::Buffer,
    palrle_compact_list_memory: vk::DeviceMemory,
    // Task 10b will read this pointer to retrieve the compact tile index list.
    #[allow(dead_code)]
    palrle_compact_list_ptr: *mut u32,
    // HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST, persistently mapped. 4 bytes.
    palrle_compact_count_buffer: vk::Buffer,
    palrle_compact_count_memory: vk::DeviceMemory,
    // Task 10b will read this pointer to retrieve the compact tile count.
    #[allow(dead_code)]
    palrle_compact_count_ptr: *mut u32,

    // PalRLE indirect-args pipeline (Stage 1.5b)
    palrle_indirect_args_shader_module: vk::ShaderModule,
    palrle_indirect_args_pipeline: vk::Pipeline,
    palrle_indirect_args_pipeline_layout: vk::PipelineLayout,
    palrle_indirect_args_descriptor_set_layout: vk::DescriptorSetLayout,
    // HOST_VISIBLE | HOST_COHERENT, size = 12 bytes. Written by shader; used as
    // INDIRECT_BUFFER for vkCmdDispatchIndirect in Task 10b.
    palrle_indirect_args_buffer: vk::Buffer,
    palrle_indirect_args_memory: vk::DeviceMemory,

    // palette_fold pipeline (Stage 2a)
    palette_fold_shader_module: vk::ShaderModule,
    palette_fold_pipeline: vk::Pipeline,
    palette_fold_pipeline_layout: vk::PipelineLayout,
    palette_fold_descriptor_set_layout: vk::DescriptorSetLayout,
    // HOST_VISIBLE | HOST_COHERENT, 256 * 80 = 20480 bytes. Persistently mapped.
    frame_palette_set_buffer: vk::Buffer,
    frame_palette_set_memory: vk::DeviceMemory,
    // Persistent CPU mapping of the frame palette set buffer; pointed to by
    // FrameAnalysis::frame_palette_set.
    frame_palette_set_ptr: *mut FramePaletteEntryRaw,
    // HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST, 4 bytes. Persistently mapped.
    frame_palette_count_buffer: vk::Buffer,
    frame_palette_count_memory: vk::DeviceMemory,
    // Persistent CPU mapping of the frame palette count buffer; pointed to by
    // FrameAnalysis::frame_palette_set_count (high-water mark of claimed slot IDs).
    frame_palette_count_ptr: *mut u32,
    // HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST, 256 * 4 = 1024 bytes.
    // No persistent CPU mapping needed (Stage 2a is the only consumer).
    hash_table_buffer: vk::Buffer,
    hash_table_memory: vk::DeviceMemory,
    // HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST, (max_tiles+3)/4*4 bytes. Persistently mapped.
    per_tile_frame_palette_id_buffer: vk::Buffer,
    per_tile_frame_palette_id_memory: vk::DeviceMemory,
    // Persistent CPU mapping of the per-tile frame palette ID buffer; pointed
    // to by FrameAnalysis::per_tile_frame_palette_id.
    per_tile_frame_palette_id_ptr: *mut u8,

    // palette_subset_fold_init pipeline (Stage 2b init)
    palette_subset_fold_init_shader_module: vk::ShaderModule,
    palette_subset_fold_init_pipeline: vk::Pipeline,
    palette_subset_fold_init_pipeline_layout: vk::PipelineLayout,
    palette_subset_fold_init_descriptor_set_layout: vk::DescriptorSetLayout,

    // palette_subset_fold pipeline (Stage 2b)
    palette_subset_fold_shader_module: vk::ShaderModule,
    palette_subset_fold_pipeline: vk::Pipeline,
    palette_subset_fold_pipeline_layout: vk::PipelineLayout,
    palette_subset_fold_descriptor_set_layout: vk::DescriptorSetLayout,

    // Stage 2b output: folded_into[i] encodes the best superset for slot i.
    // 256 × 4 = 1024 bytes. HOST_VISIBLE | HOST_COHERENT | STORAGE_BUFFER | TRANSFER_DST.
    // Persistently mapped; Task 12b will read this after the Stage 2b dispatch.
    folded_into_buffer: vk::Buffer,
    folded_into_memory: vk::DeviceMemory,
    // Persistent CPU mapping of the folded_into buffer; pointed to by
    // FrameAnalysis::folded_into after Stage 2b dispatch.
    folded_into_ptr: *mut u32,

    // pal_rle_index pipeline (Stage 3)
    pal_rle_index_shader_module: vk::ShaderModule,
    pal_rle_index_pipeline: vk::Pipeline,
    pal_rle_index_pipeline_layout: vk::PipelineLayout,
    pal_rle_index_descriptor_set_layout: vk::DescriptorSetLayout,
    // HOST_VISIBLE | HOST_COHERENT, size = max_tiles * 512 bytes.
    // Each tile occupies 128 u32 entries (512 bytes) of packed 4-bit indices.
    // Persistently mapped for CPU readback after Stage 3 dispatch.
    index_buffer: vk::Buffer,
    index_buffer_memory: vk::DeviceMemory,
    // Persistent CPU mapping of the index buffer; pointed to by
    // FrameAnalysis::index_buffer after Stage 3 dispatch.
    index_buffer_ptr: *mut u8,

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

/// The four NV12 destination-buffer fields needed by the per-frame compute
/// passes. Trivially `Copy` so the value can be threaded by value into helper
/// functions without holding a borrow on the parent `NV12Buffer`.
#[derive(Clone, Copy)]
struct Nv12OutputLayout {
    buffer: vk::Buffer,
    y_stride: u32,
    uv_offset: u32,
    uv_stride: u32,
}

impl Nv12OutputLayout {
    fn from_buffer(b: &NV12Buffer) -> Self {
        Self {
            buffer: b.buffer,
            y_stride: b.y_stride,
            uv_offset: b.uv_offset,
            uv_stride: b.uv_stride,
        }
    }
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
        let queue_families = instance.get_physical_device_queue_family_properties(physical_device);

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

        // --- Analysis Shader module ---
        let analysis_spv = include_bytes!("shaders/tile_analysis.spv");
        let analysis_spv_words =
            ash::util::read_spv(&mut std::io::Cursor::new(analysis_spv.as_slice()))?;
        let analysis_shader_ci = vk::ShaderModuleCreateInfo::default().code(&analysis_spv_words);
        let analysis_shader_module = device.create_shader_module(&analysis_shader_ci, None)?;

        // --- SAD compute pipeline ---
        // Bindings:
        //   0: STORAGE_IMAGE (current frame)
        //   1: STORAGE_IMAGE (prev frame)
        //   2: STORAGE_BUFFER (SAD output)
        // Push constants: 3 × u32 = 12 bytes (frame_width, frame_height, cols).
        let (descriptor_set_layout, pipeline_layout, pipeline) =
            pipeline_builder::build_compute_pipeline(
                &device,
                &[
                    BindingSpec::storage_image(0),
                    BindingSpec::storage_image(1),
                    BindingSpec::storage_buffer(2),
                ],
                12,
                shader_module,
            )?;

        // --- NV12 compute pipeline ---
        // Bindings:
        //   0: STORAGE_IMAGE (BGRA input)
        //   1: STORAGE_BUFFER (NV12 output)
        // Push constants: 5 × u32 = 20 bytes (width, height, y_stride, uv_offset, uv_stride).
        let (nv12_descriptor_set_layout, nv12_pipeline_layout, nv12_pipeline) =
            pipeline_builder::build_compute_pipeline(
                &device,
                &[
                    BindingSpec::storage_image(0),
                    BindingSpec::storage_buffer(1),
                ],
                20,
                nv12_shader_module,
            )?;

        // --- Analysis compute pipeline ---
        // Bindings:
        //   0: STORAGE_IMAGE (current frame, read-only in shader)
        //   1: STORAGE_BUFFER (analysis output)
        // Push constants: 3 × u32 = 12 bytes (frame_width, frame_height, cols) — same
        // shape as SAD's, but a distinct pipeline-layout object is required because
        // descriptor sets differ.
        let (analysis_descriptor_set_layout, analysis_pipeline_layout, analysis_pipeline) =
            pipeline_builder::build_compute_pipeline(
                &device,
                &[
                    BindingSpec::storage_image(0),
                    BindingSpec::storage_buffer(1),
                ],
                12,
                analysis_shader_module,
            )?;

        // --- PalRLE compact shader module ---
        let palrle_compact_spv = include_bytes!("shaders/palrle_compact.spv");
        let palrle_compact_spv_words =
            ash::util::read_spv(&mut std::io::Cursor::new(palrle_compact_spv.as_slice()))?;
        let palrle_compact_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&palrle_compact_spv_words);
        let palrle_compact_shader_module =
            device.create_shader_module(&palrle_compact_shader_ci, None)?;

        // --- PalRLE compact compute pipeline ---
        // Bindings:
        //   0: STORAGE_BUFFER (SAD output, read-only)
        //   1: STORAGE_BUFFER (tile analysis, read-only)
        //   2: STORAGE_BUFFER (compact list output)
        //   3: STORAGE_BUFFER (compact count output)
        // Push constants: 3 × u32 = 12 bytes (cols, rows, dirty_threshold).
        let (
            palrle_compact_descriptor_set_layout,
            palrle_compact_pipeline_layout,
            palrle_compact_pipeline,
        ) = pipeline_builder::build_compute_pipeline(
            &device,
            &[
                BindingSpec::storage_buffer(0),
                BindingSpec::storage_buffer(1),
                BindingSpec::storage_buffer(2),
                BindingSpec::storage_buffer(3),
            ],
            12,
            palrle_compact_shader_module,
        )?;

        // --- PalRLE indirect-args shader module ---
        let palrle_indirect_args_spv = include_bytes!("shaders/palrle_indirect_args.spv");
        let palrle_indirect_args_spv_words =
            ash::util::read_spv(&mut std::io::Cursor::new(palrle_indirect_args_spv.as_slice()))?;
        let palrle_indirect_args_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&palrle_indirect_args_spv_words);
        let palrle_indirect_args_shader_module =
            device.create_shader_module(&palrle_indirect_args_shader_ci, None)?;

        // --- PalRLE indirect-args compute pipeline (no push constants) ---
        // Bindings:
        //   0: STORAGE_BUFFER (compact count, read-only)
        //   1: STORAGE_BUFFER (indirect args output)
        let (
            palrle_indirect_args_descriptor_set_layout,
            palrle_indirect_args_pipeline_layout,
            palrle_indirect_args_pipeline,
        ) = pipeline_builder::build_compute_pipeline(
            &device,
            &[
                BindingSpec::storage_buffer(0),
                BindingSpec::storage_buffer(1),
            ],
            0,
            palrle_indirect_args_shader_module,
        )?;

        // --- pal_rle_index shader module (Stage 3) ---
        let pal_rle_index_spv = include_bytes!("shaders/pal_rle_index.spv");
        let pal_rle_index_spv_words =
            ash::util::read_spv(&mut std::io::Cursor::new(pal_rle_index_spv.as_slice()))?;
        let pal_rle_index_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&pal_rle_index_spv_words);
        let pal_rle_index_shader_module =
            device.create_shader_module(&pal_rle_index_shader_ci, None)?;

        // --- pal_rle_index compute pipeline ---
        // Bindings:
        //   0: STORAGE_IMAGE (current_frame, read-only)
        //   1: STORAGE_BUFFER (compact_list, read-only)
        //   2: STORAGE_BUFFER (frame_palette_set, read-only)
        //   3: STORAGE_BUFFER (per_tile_frame_palette_id, read-only)
        //   4: STORAGE_BUFFER (folded_into, read-only)
        //   5: STORAGE_BUFFER (index_buffer output)
        // Push constants: 1 × u32 = 4 bytes (cols).
        let (
            pal_rle_index_descriptor_set_layout,
            pal_rle_index_pipeline_layout,
            pal_rle_index_pipeline,
        ) = pipeline_builder::build_compute_pipeline(
            &device,
            &[
                BindingSpec::storage_image(0),
                BindingSpec::storage_buffer(1),
                BindingSpec::storage_buffer(2),
                BindingSpec::storage_buffer(3),
                BindingSpec::storage_buffer(4),
                BindingSpec::storage_buffer(5),
            ],
            4,
            pal_rle_index_shader_module,
        )?;

        // --- Descriptor pool ---
        // 9 sets: SAD (2 STORAGE_IMAGE + 1 STORAGE_BUFFER)
        //       + NV12 (1 STORAGE_IMAGE + 1 STORAGE_BUFFER)
        //       + Analysis (1 STORAGE_IMAGE + 1 STORAGE_BUFFER)
        //       + palrle_compact (4 STORAGE_BUFFER)
        //       + palrle_indirect_args (2 STORAGE_BUFFER)
        //       + palette_fold (6 STORAGE_BUFFER)
        //       + palette_subset_fold_init (1 STORAGE_BUFFER)
        //       + palette_subset_fold (2 STORAGE_BUFFER)
        //       + pal_rle_index (1 STORAGE_IMAGE + 5 STORAGE_BUFFER)
        // Total: 5 STORAGE_IMAGE, 23 STORAGE_BUFFER, 9 max_sets.
        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_IMAGE,
                descriptor_count: 5,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 23,
            },
        ];
        let dp_ci = vk::DescriptorPoolCreateInfo::default()
            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
            .max_sets(9)
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

        let sad_ptr = device.map_memory(sad_memory, 0, sad_buf_size, vk::MemoryMapFlags::empty())?
            as *mut u32;

        // --- Analysis output buffer ---
        let analysis_entry_bytes = std::mem::size_of::<TileAnalysis>() as vk::DeviceSize;
        let analysis_buf_size = (max_tiles as vk::DeviceSize) * analysis_entry_bytes;
        let analysis_buf_ci = vk::BufferCreateInfo::default()
            .size(analysis_buf_size)
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let analysis_buffer = device.create_buffer(&analysis_buf_ci, None)?;
        let analysis_buf_reqs = device.get_buffer_memory_requirements(analysis_buffer);
        let analysis_mem_type = find_memory_type(
            &mem_props,
            analysis_buf_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or("no host-visible memory type for analysis buffer")?;

        let analysis_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(analysis_buf_reqs.size)
            .memory_type_index(analysis_mem_type);
        let analysis_memory = device.allocate_memory(&analysis_alloc, None)?;
        device.bind_buffer_memory(analysis_buffer, analysis_memory, 0)?;

        let analysis_ptr = device.map_memory(
            analysis_memory,
            0,
            analysis_buf_size,
            vk::MemoryMapFlags::empty(),
        )? as *mut TileAnalysis;

        // --- PalRLE compact list buffer ---
        // One u32 per tile slot. HOST_VISIBLE | HOST_COHERENT, STORAGE_BUFFER.
        let palrle_compact_list_size = (max_tiles * 4) as vk::DeviceSize;
        let palrle_compact_list_buf_ci = vk::BufferCreateInfo::default()
            .size(palrle_compact_list_size)
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let palrle_compact_list_buffer = device.create_buffer(&palrle_compact_list_buf_ci, None)?;
        let palrle_compact_list_reqs =
            device.get_buffer_memory_requirements(palrle_compact_list_buffer);
        let palrle_compact_list_mem_type = find_memory_type(
            &mem_props,
            palrle_compact_list_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or("no host-visible memory type for palrle compact list buffer")?;
        let palrle_compact_list_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(palrle_compact_list_reqs.size)
            .memory_type_index(palrle_compact_list_mem_type);
        let palrle_compact_list_memory = device.allocate_memory(&palrle_compact_list_alloc, None)?;
        device.bind_buffer_memory(
            palrle_compact_list_buffer,
            palrle_compact_list_memory,
            0,
        )?;
        let palrle_compact_list_ptr = device.map_memory(
            palrle_compact_list_memory,
            0,
            palrle_compact_list_size,
            vk::MemoryMapFlags::empty(),
        )? as *mut u32;

        // --- PalRLE compact count buffer ---
        // 4 bytes (one u32). HOST_VISIBLE | HOST_COHERENT, STORAGE_BUFFER | TRANSFER_DST.
        let palrle_compact_count_size = 4_u64;
        let palrle_compact_count_buf_ci = vk::BufferCreateInfo::default()
            .size(palrle_compact_count_size)
            .usage(
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let palrle_compact_count_buffer =
            device.create_buffer(&palrle_compact_count_buf_ci, None)?;
        let palrle_compact_count_reqs =
            device.get_buffer_memory_requirements(palrle_compact_count_buffer);
        let palrle_compact_count_mem_type = find_memory_type(
            &mem_props,
            palrle_compact_count_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or("no host-visible memory type for palrle compact count buffer")?;
        let palrle_compact_count_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(palrle_compact_count_reqs.size)
            .memory_type_index(palrle_compact_count_mem_type);
        let palrle_compact_count_memory =
            device.allocate_memory(&palrle_compact_count_alloc, None)?;
        device.bind_buffer_memory(
            palrle_compact_count_buffer,
            palrle_compact_count_memory,
            0,
        )?;
        let palrle_compact_count_ptr = device.map_memory(
            palrle_compact_count_memory,
            0,
            palrle_compact_count_size,
            vk::MemoryMapFlags::empty(),
        )? as *mut u32;

        // --- PalRLE indirect args buffer ---
        // 12 bytes (3 u32s: group_count_x/y/z). Written by shader, read as
        // INDIRECT_BUFFER. HOST_VISIBLE | HOST_COHERENT for simple initialization
        // (no staging buffer needed, and avoids DEVICE_LOCAL complication in Drop).
        // HOST_VISIBLE chosen (deviation from spec D14: DEVICE_LOCAL) for simpler
        // init — vkCmdDispatchIndirect reads correctly from HOST_VISIBLE memory.
        // PCIe-bar read cost is acceptable for 12 B per frame.
        let palrle_indirect_args_size = 12_u64;
        let palrle_indirect_args_buf_ci = vk::BufferCreateInfo::default()
            .size(palrle_indirect_args_size)
            .usage(
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::INDIRECT_BUFFER,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let palrle_indirect_args_buffer =
            device.create_buffer(&palrle_indirect_args_buf_ci, None)?;
        let palrle_indirect_args_reqs =
            device.get_buffer_memory_requirements(palrle_indirect_args_buffer);
        let palrle_indirect_args_mem_type = find_memory_type(
            &mem_props,
            palrle_indirect_args_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or("no host-visible memory type for palrle indirect args buffer")?;
        let palrle_indirect_args_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(palrle_indirect_args_reqs.size)
            .memory_type_index(palrle_indirect_args_mem_type);
        let palrle_indirect_args_memory =
            device.allocate_memory(&palrle_indirect_args_alloc, None)?;
        device.bind_buffer_memory(
            palrle_indirect_args_buffer,
            palrle_indirect_args_memory,
            0,
        )?;
        // Initialize to (0, 1, 1) so a stale dispatch is a safe no-op.
        let init_ptr = device.map_memory(
            palrle_indirect_args_memory,
            0,
            palrle_indirect_args_size,
            vk::MemoryMapFlags::empty(),
        )? as *mut u32;
        init_ptr.write(0);
        init_ptr.add(1).write(1);
        init_ptr.add(2).write(1);
        device.unmap_memory(palrle_indirect_args_memory);

        // --- palette_fold shader module ---
        let palette_fold_spv = include_bytes!("shaders/palette_fold.spv");
        let palette_fold_spv_words =
            ash::util::read_spv(&mut std::io::Cursor::new(palette_fold_spv.as_slice()))?;
        let palette_fold_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&palette_fold_spv_words);
        let palette_fold_shader_module =
            device.create_shader_module(&palette_fold_shader_ci, None)?;

        // --- palette_fold compute pipeline (no push constants) ---
        // Bindings:
        //   0: STORAGE_BUFFER (tile analysis input, read-only)
        //   1: STORAGE_BUFFER (compact list input, read-only)
        //   2: STORAGE_BUFFER (frame_palette_set output)
        //   3: STORAGE_BUFFER (frame_palette_count output)
        //   4: STORAGE_BUFFER (hash_table, per-frame scratch)
        //   5: STORAGE_BUFFER (per_tile_frame_palette_id output)
        let (
            palette_fold_descriptor_set_layout,
            palette_fold_pipeline_layout,
            palette_fold_pipeline,
        ) = pipeline_builder::build_compute_pipeline(
            &device,
            &[
                BindingSpec::storage_buffer(0),
                BindingSpec::storage_buffer(1),
                BindingSpec::storage_buffer(2),
                BindingSpec::storage_buffer(3),
                BindingSpec::storage_buffer(4),
                BindingSpec::storage_buffer(5),
            ],
            0,
            palette_fold_shader_module,
        )?;

        // --- frame_palette_set buffer ---
        // 256 PaletteEntry slots × 80 bytes = 20480 bytes.
        // HOST_VISIBLE | HOST_COHERENT, STORAGE_BUFFER | TRANSFER_DST.
        // TRANSFER_DST allows Task 12b to zero-fill between frames via
        // cmd_fill_buffer so Stage 2b can safely check count == 0 per slot.
        let frame_palette_set_size =
            (PALETTE_HASH_SLOTS * std::mem::size_of::<FramePaletteEntryRaw>()) as vk::DeviceSize;
        let frame_palette_set_buf_ci = vk::BufferCreateInfo::default()
            .size(frame_palette_set_size)
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let frame_palette_set_buffer = device.create_buffer(&frame_palette_set_buf_ci, None)?;
        let frame_palette_set_reqs =
            device.get_buffer_memory_requirements(frame_palette_set_buffer);
        let frame_palette_set_mem_type = find_memory_type(
            &mem_props,
            frame_palette_set_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or("no host-visible memory type for frame_palette_set buffer")?;
        let frame_palette_set_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(frame_palette_set_reqs.size)
            .memory_type_index(frame_palette_set_mem_type);
        let frame_palette_set_memory = device.allocate_memory(&frame_palette_set_alloc, None)?;
        device.bind_buffer_memory(frame_palette_set_buffer, frame_palette_set_memory, 0)?;
        let frame_palette_set_ptr = device.map_memory(
            frame_palette_set_memory,
            0,
            frame_palette_set_size,
            vk::MemoryMapFlags::empty(),
        )? as *mut FramePaletteEntryRaw;

        // --- frame_palette_count buffer ---
        // 4 bytes. HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST (for cmd_fill_buffer zero between frames).
        let frame_palette_count_size = 4_u64;
        let frame_palette_count_buf_ci = vk::BufferCreateInfo::default()
            .size(frame_palette_count_size)
            .usage(
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let frame_palette_count_buffer =
            device.create_buffer(&frame_palette_count_buf_ci, None)?;
        let frame_palette_count_reqs =
            device.get_buffer_memory_requirements(frame_palette_count_buffer);
        let frame_palette_count_mem_type = find_memory_type(
            &mem_props,
            frame_palette_count_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or("no host-visible memory type for frame_palette_count buffer")?;
        let frame_palette_count_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(frame_palette_count_reqs.size)
            .memory_type_index(frame_palette_count_mem_type);
        let frame_palette_count_memory =
            device.allocate_memory(&frame_palette_count_alloc, None)?;
        device.bind_buffer_memory(
            frame_palette_count_buffer,
            frame_palette_count_memory,
            0,
        )?;
        let frame_palette_count_ptr = device.map_memory(
            frame_palette_count_memory,
            0,
            frame_palette_count_size,
            vk::MemoryMapFlags::empty(),
        )? as *mut u32;

        // --- hash_table buffer ---
        // 256 slots × 4 bytes = 1024 bytes. HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST.
        // No persistent CPU mapping needed.
        let hash_table_size = 256_u64 * 4;
        let hash_table_buf_ci = vk::BufferCreateInfo::default()
            .size(hash_table_size)
            .usage(
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let hash_table_buffer = device.create_buffer(&hash_table_buf_ci, None)?;
        let hash_table_reqs = device.get_buffer_memory_requirements(hash_table_buffer);
        let hash_table_mem_type = find_memory_type(
            &mem_props,
            hash_table_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or("no host-visible memory type for hash_table buffer")?;
        let hash_table_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(hash_table_reqs.size)
            .memory_type_index(hash_table_mem_type);
        let hash_table_memory = device.allocate_memory(&hash_table_alloc, None)?;
        device.bind_buffer_memory(hash_table_buffer, hash_table_memory, 0)?;

        // --- per_tile_frame_palette_id buffer ---
        // (max_tiles + 3) / 4 * 4 bytes (round up to u32 alignment).
        // HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST, STORAGE_BUFFER.
        let per_tile_id_size = (((max_tiles + 3) / 4 * 4) as vk::DeviceSize).max(4);
        let per_tile_id_buf_ci = vk::BufferCreateInfo::default()
            .size(per_tile_id_size)
            .usage(
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let per_tile_frame_palette_id_buffer =
            device.create_buffer(&per_tile_id_buf_ci, None)?;
        let per_tile_id_reqs =
            device.get_buffer_memory_requirements(per_tile_frame_palette_id_buffer);
        let per_tile_id_mem_type = find_memory_type(
            &mem_props,
            per_tile_id_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or("no host-visible memory type for per_tile_frame_palette_id buffer")?;
        let per_tile_id_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(per_tile_id_reqs.size)
            .memory_type_index(per_tile_id_mem_type);
        let per_tile_frame_palette_id_memory =
            device.allocate_memory(&per_tile_id_alloc, None)?;
        device.bind_buffer_memory(
            per_tile_frame_palette_id_buffer,
            per_tile_frame_palette_id_memory,
            0,
        )?;
        let per_tile_frame_palette_id_ptr = device.map_memory(
            per_tile_frame_palette_id_memory,
            0,
            per_tile_id_size,
            vk::MemoryMapFlags::empty(),
        )? as *mut u8;

        // --- palette_subset_fold_init shader module ---
        let palette_subset_fold_init_spv =
            include_bytes!("shaders/palette_subset_fold_init.spv");
        let palette_subset_fold_init_spv_words = ash::util::read_spv(&mut std::io::Cursor::new(
            palette_subset_fold_init_spv.as_slice(),
        ))?;
        let palette_subset_fold_init_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&palette_subset_fold_init_spv_words);
        let palette_subset_fold_init_shader_module =
            device.create_shader_module(&palette_subset_fold_init_shader_ci, None)?;

        // --- palette_subset_fold_init compute pipeline (no push constants) ---
        // Bindings:
        //   0: STORAGE_BUFFER (FoldedInto)
        let (
            palette_subset_fold_init_descriptor_set_layout,
            palette_subset_fold_init_pipeline_layout,
            palette_subset_fold_init_pipeline,
        ) = pipeline_builder::build_compute_pipeline(
            &device,
            &[BindingSpec::storage_buffer(0)],
            0,
            palette_subset_fold_init_shader_module,
        )?;

        // --- palette_subset_fold shader module ---
        let palette_subset_fold_spv = include_bytes!("shaders/palette_subset_fold.spv");
        let palette_subset_fold_spv_words = ash::util::read_spv(&mut std::io::Cursor::new(
            palette_subset_fold_spv.as_slice(),
        ))?;
        let palette_subset_fold_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&palette_subset_fold_spv_words);
        let palette_subset_fold_shader_module =
            device.create_shader_module(&palette_subset_fold_shader_ci, None)?;

        // --- palette_subset_fold compute pipeline (no push constants) ---
        // Bindings:
        //   0: STORAGE_BUFFER (FramePaletteSet, read-only)
        //   1: STORAGE_BUFFER (FoldedInto, read-write)
        let (
            palette_subset_fold_descriptor_set_layout,
            palette_subset_fold_pipeline_layout,
            palette_subset_fold_pipeline,
        ) = pipeline_builder::build_compute_pipeline(
            &device,
            &[
                BindingSpec::storage_buffer(0),
                BindingSpec::storage_buffer(1),
            ],
            0,
            palette_subset_fold_shader_module,
        )?;

        // --- folded_into buffer ---
        // folded_into_buffer: each frame is reset by palette_subset_fold_init.comp
        // (the init shader writes the default-self sentinel). TRANSFER_DST is kept
        // for symmetry with other Stage 2 buffers and to permit a future fallback
        // to vkCmdFillBuffer if the init dispatch is ever replaced.
        // 256 slots × 4 bytes = 1024 bytes. HOST_VISIBLE | HOST_COHERENT,
        // persistently mapped for CPU readback (Task 12b+).
        let folded_into_size = 256_u64 * 4;
        let folded_into_buf_ci = vk::BufferCreateInfo::default()
            .size(folded_into_size)
            .usage(
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let folded_into_buffer = device.create_buffer(&folded_into_buf_ci, None)?;
        let folded_into_reqs = device.get_buffer_memory_requirements(folded_into_buffer);
        let folded_into_mem_type = find_memory_type(
            &mem_props,
            folded_into_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or("no host-visible memory type for folded_into buffer")?;
        let folded_into_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(folded_into_reqs.size)
            .memory_type_index(folded_into_mem_type);
        let folded_into_memory = device.allocate_memory(&folded_into_alloc, None)?;
        device.bind_buffer_memory(folded_into_buffer, folded_into_memory, 0)?;
        let folded_into_ptr = device.map_memory(
            folded_into_memory,
            0,
            folded_into_size,
            vk::MemoryMapFlags::empty(),
        )? as *mut u32;

        // --- index_buffer (Stage 3 output) ---
        // max_tiles * 512 bytes: each tile has 128 u32 entries of packed 4-bit indices.
        // HOST_VISIBLE | HOST_COHERENT, STORAGE_BUFFER, persistently mapped.
        let index_buffer_size =
            ((max_tiles as vk::DeviceSize) * PER_TILE_INDEX_BYTES).max(PER_TILE_INDEX_BYTES);
        let index_buf_ci = vk::BufferCreateInfo::default()
            .size(index_buffer_size)
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let index_buffer = device.create_buffer(&index_buf_ci, None)?;
        let index_buf_reqs = device.get_buffer_memory_requirements(index_buffer);
        let index_mem_type = find_memory_type(
            &mem_props,
            index_buf_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or("no host-visible memory type for index_buffer")?;
        let index_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(index_buf_reqs.size)
            .memory_type_index(index_mem_type);
        let index_buffer_memory = device.allocate_memory(&index_alloc, None)?;
        device.bind_buffer_memory(index_buffer, index_buffer_memory, 0)?;
        let index_buffer_ptr = device.map_memory(
            index_buffer_memory,
            0,
            index_buffer_size,
            vk::MemoryMapFlags::empty(),
        )? as *mut u8;

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
            analysis_shader_module,
            analysis_pipeline,
            analysis_pipeline_layout,
            analysis_descriptor_set_layout,
            analysis_buffer,
            analysis_memory,
            analysis_ptr,
            palrle_compact_shader_module,
            palrle_compact_pipeline,
            palrle_compact_pipeline_layout,
            palrle_compact_descriptor_set_layout,
            palrle_compact_list_buffer,
            palrle_compact_list_memory,
            palrle_compact_list_ptr,
            palrle_compact_count_buffer,
            palrle_compact_count_memory,
            palrle_compact_count_ptr,
            palrle_indirect_args_shader_module,
            palrle_indirect_args_pipeline,
            palrle_indirect_args_pipeline_layout,
            palrle_indirect_args_descriptor_set_layout,
            palrle_indirect_args_buffer,
            palrle_indirect_args_memory,
            palette_fold_shader_module,
            palette_fold_pipeline,
            palette_fold_pipeline_layout,
            palette_fold_descriptor_set_layout,
            frame_palette_set_buffer,
            frame_palette_set_memory,
            frame_palette_set_ptr,
            frame_palette_count_buffer,
            frame_palette_count_memory,
            frame_palette_count_ptr,
            hash_table_buffer,
            hash_table_memory,
            per_tile_frame_palette_id_buffer,
            per_tile_frame_palette_id_memory,
            per_tile_frame_palette_id_ptr,
            palette_subset_fold_init_shader_module,
            palette_subset_fold_init_pipeline,
            palette_subset_fold_init_pipeline_layout,
            palette_subset_fold_init_descriptor_set_layout,
            palette_subset_fold_shader_module,
            palette_subset_fold_pipeline,
            palette_subset_fold_pipeline_layout,
            palette_subset_fold_descriptor_set_layout,
            folded_into_buffer,
            folded_into_memory,
            folded_into_ptr,
            pal_rle_index_shader_module,
            pal_rle_index_pipeline,
            pal_rle_index_pipeline_layout,
            pal_rle_index_descriptor_set_layout,
            index_buffer,
            index_buffer_memory,
            index_buffer_ptr,
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

        let ptr =
            self.device
                .map_memory(memory, 0, total, vk::MemoryMapFlags::empty())? as *mut u8;

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
        let result =
            self.process_frame_with_imported(&current, width, height, tile_count, cols, rows);
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
        let nv12_layout = Nv12OutputLayout::from_buffer(nv12);
        let Nv12OutputLayout {
            buffer: nv12_buffer,
            y_stride: nv12_y_stride,
            uv_offset: nv12_uv_offset,
            uv_stride: nv12_uv_stride,
        } = nv12_layout;
        let nv12_ptr = nv12.ptr;

        // --- First frame: no SAD (no prev to compare), mark all dirty, run
        // NV12 + tile_analysis, copy current → newly-allocated owned snapshot
        // for next frame's SAD source.
        //
        // tile_analysis runs on the first frame because it only reads the
        // current frame's pixels (no dependency on prev_image). Without it,
        // every tile's `unique_colors` stays at UNIQUE_COLORS_UNKNOWN, which
        // forces the classifier into Codec::Raw for static content where the
        // first frame is the only frame with dirty tiles. See
        // docs/superpowers/specs/2026-05-15-m3.2c-verification-design.md.
        //
        // The downstream Stage 1.5/2a/2b/3 passes (palrle_compact /
        // palette_fold / palette_subset_fold / pal_rle_index) intentionally
        // remain skipped on the first frame: they feed Phase B PalRle
        // encoding, which is keyed by classifier output, not by historical
        // state — so tile_analysis populated is sufficient for the classifier
        // to pick Solid/PalRle. ---
        if self.prev_image.is_none() {
            let all_dirty: Vec<u32> = (0..tile_count).collect();

            // Allocate the owned snapshot image once. It persists for the
            // lifetime of the processor (until resolution changes).
            let snapshot = self.allocate_owned_image(width, height)?;

            // Run NV12 conversion + tile_analysis + snapshot copy in one cmd
            // buffer. The snapshot is what we will compare future frames
            // against.
            self.run_first_frame_passes(
                current,
                &snapshot,
                width,
                height,
                cols,
                rows,
                nv12_layout,
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
                tile_analysis: self.analysis_ptr as *const TileAnalysis,
                tile_analysis_len: cols * rows,
                palrle_compact_list: self.palrle_compact_list_ptr as *const u32,
                palrle_compact_count: *self.palrle_compact_count_ptr,
                frame_palette_set: self.frame_palette_set_ptr as *const FramePaletteEntryRaw,
                frame_palette_set_count: *self.frame_palette_count_ptr,
                per_tile_frame_palette_id: self.per_tile_frame_palette_id_ptr as *const u8,
                folded_into: self.folded_into_ptr as *const u32,
                index_buffer: self.index_buffer_ptr as *const u8,
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

        let analysis_set_layouts = [self.analysis_descriptor_set_layout];
        let analysis_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&analysis_set_layouts);
        let analysis_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&analysis_ds_alloc)?,
        };
        let analysis_ds = analysis_ds_guard.sets[0];

        let palrle_compact_set_layouts = [self.palrle_compact_descriptor_set_layout];
        let palrle_compact_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&palrle_compact_set_layouts);
        let palrle_compact_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&palrle_compact_ds_alloc)?,
        };
        let palrle_compact_ds = palrle_compact_ds_guard.sets[0];

        let palrle_indirect_args_set_layouts = [self.palrle_indirect_args_descriptor_set_layout];
        let palrle_indirect_args_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&palrle_indirect_args_set_layouts);
        let palrle_indirect_args_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&palrle_indirect_args_ds_alloc)?,
        };
        let palrle_indirect_args_ds = palrle_indirect_args_ds_guard.sets[0];

        // palette_fold descriptor set (Stage 2a)
        let palette_fold_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(std::slice::from_ref(&self.palette_fold_descriptor_set_layout));
        let palette_fold_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&palette_fold_ds_alloc)?,
        };
        let palette_fold_descriptor_set = palette_fold_ds_guard.sets[0];

        // palette_subset_fold_init descriptor set (Stage 2b init)
        let palette_subset_fold_init_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(std::slice::from_ref(
                &self.palette_subset_fold_init_descriptor_set_layout,
            ));
        let palette_subset_fold_init_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self
                .device
                .allocate_descriptor_sets(&palette_subset_fold_init_ds_alloc)?,
        };
        let palette_subset_fold_init_descriptor_set =
            palette_subset_fold_init_ds_guard.sets[0];

        // palette_subset_fold descriptor set (Stage 2b)
        let palette_subset_fold_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(std::slice::from_ref(
                &self.palette_subset_fold_descriptor_set_layout,
            ));
        let palette_subset_fold_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self
                .device
                .allocate_descriptor_sets(&palette_subset_fold_ds_alloc)?,
        };
        let palette_subset_fold_descriptor_set = palette_subset_fold_ds_guard.sets[0];

        // pal_rle_index descriptor set (Stage 3)
        let pal_rle_index_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(std::slice::from_ref(&self.pal_rle_index_descriptor_set_layout));
        let pal_rle_index_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&pal_rle_index_ds_alloc)?,
        };
        let pal_rle_index_descriptor_set = pal_rle_index_ds_guard.sets[0];

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

        // Write Analysis descriptor set.
        let analysis_image_info = [vk::DescriptorImageInfo::default()
            .image_view(current.view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let analysis_buffer_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.analysis_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let analysis_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(analysis_ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&analysis_image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(analysis_ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&analysis_buffer_info),
        ];
        self.device.update_descriptor_sets(&analysis_writes, &[]);

        // Write palrle_compact descriptor set.
        // binding 0: sad_buffer (read-only in shader)
        // binding 1: analysis_buffer (read-only in shader)
        // binding 2: palrle_compact_list_buffer (write)
        // binding 3: palrle_compact_count_buffer (atomic write)
        let palrle_compact_sad_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.sad_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palrle_compact_analysis_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.analysis_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palrle_compact_list_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.palrle_compact_list_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palrle_compact_count_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.palrle_compact_count_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palrle_compact_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(palrle_compact_ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palrle_compact_sad_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palrle_compact_ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palrle_compact_analysis_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palrle_compact_ds)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palrle_compact_list_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palrle_compact_ds)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palrle_compact_count_info),
        ];
        self.device.update_descriptor_sets(&palrle_compact_writes, &[]);

        // Write palrle_indirect_args descriptor set.
        // binding 0: palrle_compact_count_buffer (read)
        // binding 1: palrle_indirect_args_buffer (write)
        let palrle_indirect_count_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.palrle_compact_count_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palrle_indirect_args_buf_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.palrle_indirect_args_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palrle_indirect_args_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(palrle_indirect_args_ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palrle_indirect_count_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palrle_indirect_args_ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palrle_indirect_args_buf_info),
        ];
        self.device
            .update_descriptor_sets(&palrle_indirect_args_writes, &[]);

        // Write palette_fold descriptor set.
        // binding 0: analysis_buffer (read-only)
        // binding 1: palrle_compact_list_buffer (read-only)
        // binding 2: frame_palette_set_buffer (write)
        // binding 3: frame_palette_count_buffer (atomic write)
        // binding 4: hash_table_buffer (atomic CAS state)
        // binding 5: per_tile_frame_palette_id_buffer (write)
        let palette_fold_analysis_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.analysis_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palette_fold_compact_list_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.palrle_compact_list_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palette_fold_set_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.frame_palette_set_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palette_fold_count_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.frame_palette_count_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palette_fold_hash_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.hash_table_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palette_fold_per_tile_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.per_tile_frame_palette_id_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palette_fold_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_descriptor_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_analysis_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_descriptor_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_compact_list_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_descriptor_set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_set_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_descriptor_set)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_count_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_descriptor_set)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_hash_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_descriptor_set)
                .dst_binding(5)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_per_tile_info),
        ];
        self.device.update_descriptor_sets(&palette_fold_writes, &[]);

        // Write palette_subset_fold_init descriptor set.
        // binding 0: folded_into_buffer (write)
        let subset_fold_init_folded_into_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.folded_into_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let subset_fold_init_writes = [vk::WriteDescriptorSet::default()
            .dst_set(palette_subset_fold_init_descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .buffer_info(&subset_fold_init_folded_into_info)];
        self.device
            .update_descriptor_sets(&subset_fold_init_writes, &[]);

        // Write palette_subset_fold descriptor set.
        // binding 0: frame_palette_set_buffer (read-only)
        // binding 1: folded_into_buffer (read-write)
        let subset_fold_set_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.frame_palette_set_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let subset_fold_folded_into_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.folded_into_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let subset_fold_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(palette_subset_fold_descriptor_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&subset_fold_set_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_subset_fold_descriptor_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&subset_fold_folded_into_info),
        ];
        self.device.update_descriptor_sets(&subset_fold_writes, &[]);

        // Write pal_rle_index descriptor set (Stage 3).
        // binding 0: STORAGE_IMAGE (current_frame, read-only)
        // binding 1: STORAGE_BUFFER (palrle_compact_list_buffer, read-only)
        // binding 2: STORAGE_BUFFER (frame_palette_set_buffer, read-only)
        // binding 3: STORAGE_BUFFER (per_tile_frame_palette_id_buffer, read-only)
        // binding 4: STORAGE_BUFFER (folded_into_buffer, read-only)
        // binding 5: STORAGE_BUFFER (index_buffer, write)
        let pal_rle_index_image_info = [vk::DescriptorImageInfo::default()
            .image_view(current.view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let pal_rle_index_compact_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.palrle_compact_list_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let pal_rle_index_fps_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.frame_palette_set_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let pal_rle_index_ptfpi_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.per_tile_frame_palette_id_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let pal_rle_index_folded_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.folded_into_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let pal_rle_index_out_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.index_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let pal_rle_index_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_descriptor_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&pal_rle_index_image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_descriptor_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_compact_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_descriptor_set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_fps_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_descriptor_set)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_ptfpi_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_descriptor_set)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_folded_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_descriptor_set)
                .dst_binding(5)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_out_info),
        ];
        self.device
            .update_descriptor_sets(&pal_rle_index_writes, &[]);

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
        self.device
            .cmd_dispatch(cmd, nv12_groups_x, nv12_groups_y, 1);

        // 4b. Analysis dispatch. No barrier needed vs SAD/NV12 — all read
        // current_frame read-only and write to disjoint buffers; the final
        // HOST-readback barrier covers all three output buffers.
        self.device
            .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.analysis_pipeline);
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.analysis_pipeline_layout,
            0,
            &[analysis_ds],
            &[],
        );
        let analysis_push: [u32; 3] = [width, height, cols];
        let analysis_push_bytes = std::slice::from_raw_parts(
            analysis_push.as_ptr() as *const u8,
            std::mem::size_of_val(&analysis_push),
        );
        self.device.cmd_push_constants(
            cmd,
            self.analysis_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            analysis_push_bytes,
        );
        self.device.cmd_dispatch(cmd, cols, rows, 1);

        // Inter-dispatch barrier: SAD + tile_analysis writes must be visible to Stage 1.5a reads.
        let buf_barrier_inputs = [
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.sad_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.analysis_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
        ];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &buf_barrier_inputs,
            &[],
        );

        // Stage 1.5: zero compact_count, dispatch compact scan, dispatch indirect-args writer.
        self.device
            .cmd_fill_buffer(cmd, self.palrle_compact_count_buffer, 0, 4, 0);

        // Barrier: transfer-write → shader-read/write
        let buf_barrier_fill = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.palrle_compact_count_buffer)
            .offset(0)
            .size(4);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_fill),
            &[],
        );

        // Stage 1.5a: bind palrle_compact pipeline.
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palrle_compact_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palrle_compact_pipeline_layout,
            0,
            &[palrle_compact_ds],
            &[],
        );
        let compact_push: [u32; 3] = [cols, rows, SAD_THRESHOLD];
        let compact_push_bytes = std::slice::from_raw_parts(
            compact_push.as_ptr() as *const u8,
            std::mem::size_of_val(&compact_push),
        );
        self.device.cmd_push_constants(
            cmd,
            self.palrle_compact_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            compact_push_bytes,
        );
        // tile_idx = gl_WorkGroupID.x; bound check `tile_idx < cols * rows` is in the shader.
        self.device.cmd_dispatch(cmd, cols * rows, 1, 1);

        // Barrier between Stage 1.5a and 1.5b: shader write → shader read on count.
        let buf_barrier_count = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.palrle_compact_count_buffer)
            .offset(0)
            .size(4);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_count),
            &[],
        );

        // Stage 1.5b: indirect-args writer.
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palrle_indirect_args_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palrle_indirect_args_pipeline_layout,
            0,
            &[palrle_indirect_args_ds],
            &[],
        );
        self.device.cmd_dispatch(cmd, 1, 1, 1);

        // Barrier for future stages (Task 11+) to consume indirect args.
        let buf_barrier_args = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(
                vk::AccessFlags::INDIRECT_COMMAND_READ | vk::AccessFlags::SHADER_READ,
            )
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.palrle_indirect_args_buffer)
            .offset(0)
            .size(12);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::DRAW_INDIRECT | vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_args),
            &[],
        );

        // ============================================================
        // Stage 2a: palette_fold (intra-frame palette dedup)
        // ============================================================

        // Zero the four buffers that Stage 2a uses as state/output, plus
        // frame_palette_set_buffer which Stage 2b reads (count==0 per slot check):
        //   frame_palette_set (Stage 2b checks count==0 to detect empty slots)
        //   frame_palette_count (atomic write target)
        //   hash_table (atomic CAS state, must start empty)
        //   per_tile_frame_palette_id (output, must start zero for atomicAnd/Or pattern)
        let pal_id_bytes = ((self.max_tiles + 3) & !3u32) as vk::DeviceSize;
        self.device
            .cmd_fill_buffer(cmd, self.frame_palette_set_buffer, 0, 20480, 0);
        self.device
            .cmd_fill_buffer(cmd, self.frame_palette_count_buffer, 0, 4, 0);
        self.device
            .cmd_fill_buffer(cmd, self.hash_table_buffer, 0, PALETTE_SLOT_U32_BYTES, 0);
        self.device
            .cmd_fill_buffer(cmd, self.per_tile_frame_palette_id_buffer, 0, pal_id_bytes, 0);

        // Barrier: zero-fills + compact_list write must be visible to Stage 2a.
        // srcStageMask covers TRANSFER (zero-fills) and COMPUTE_SHADER (compact_list write).
        let stage_2a_input_barriers = [
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.frame_palette_set_buffer)
                .offset(0)
                .size(20480),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.frame_palette_count_buffer)
                .offset(0)
                .size(4),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.hash_table_buffer)
                .offset(0)
                .size(PALETTE_SLOT_U32_BYTES),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.per_tile_frame_palette_id_buffer)
                .offset(0)
                .size(pal_id_bytes),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.palrle_compact_list_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
        ];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &stage_2a_input_barriers,
            &[],
        );

        // Stage 2a: indirect dispatch.
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palette_fold_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palette_fold_pipeline_layout,
            0,
            &[palette_fold_descriptor_set],
            &[],
        );
        // palrle_indirect_args_buffer already barriered for INDIRECT_COMMAND_READ
        // by the Stage 1.5b barrier above.
        self.device
            .cmd_dispatch_indirect(cmd, self.palrle_indirect_args_buffer, 0);

        // Barrier: Stage 2a outputs available to downstream stages and HOST readback.
        let stage_2a_output_barriers = [
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.frame_palette_set_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.frame_palette_count_buffer)
                .offset(0)
                .size(4),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.per_tile_frame_palette_id_buffer)
                .offset(0)
                .size(pal_id_bytes),
        ];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &stage_2a_output_barriers,
            &[],
        );

        // ============================================================
        // Stage 2b init: write default-self sentinel to folded_into
        // ============================================================
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palette_subset_fold_init_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palette_subset_fold_init_pipeline_layout,
            0,
            &[palette_subset_fold_init_descriptor_set],
            &[],
        );
        self.device.cmd_dispatch(cmd, 1, 1, 1);

        // Barrier between init and fold: folded_into writes must be visible.
        let stage_2b_init_to_fold_barrier = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.folded_into_buffer)
            .offset(0)
            .size(PALETTE_SLOT_U32_BYTES);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&stage_2b_init_to_fold_barrier),
            &[],
        );

        // ============================================================
        // Stage 2b fold: atomicMin-based subset detection
        // ============================================================
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palette_subset_fold_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palette_subset_fold_pipeline_layout,
            0,
            &[palette_subset_fold_descriptor_set],
            &[],
        );
        self.device.cmd_dispatch(cmd, PALETTE_HASH_SLOTS as u32, 1, 1);

        // Barrier for Stage 3 consumers and HOST readback.
        let stage_2b_output_barrier = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.folded_into_buffer)
            .offset(0)
            .size(PALETTE_SLOT_U32_BYTES);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&stage_2b_output_barrier),
            &[],
        );

        // ============================================================
        // Stage 3: pal_rle_index — per-pixel binary search → 4-bit index stream
        // ============================================================
        //
        // Barrier chain visibility for Stage 3 inputs:
        //   - current_frame image: layout GENERAL (set at top of frame, valid
        //     through end of compute section).
        //   - palrle_compact_list: written by Stage 1.5a, barriered for
        //     SHADER_READ in stage_2a_input_barriers; visibility propagates
        //     transitively because no intervening writer touches it.
        //   - frame_palette_set & per_tile_frame_palette_id: written by Stage 2a,
        //     barriered for SHADER_READ in stage_2a_output_barriers. Visibility
        //     remains valid for Stage 3 because Stage 2b (which executes
        //     between Stage 2a's barrier and this dispatch) does NOT write
        //     these buffers — only folded_into.
        //   - folded_into: written by Stage 2b fold, barriered for SHADER_READ
        //     in stage_2b_output_barrier directly above.
        //
        // All five inputs are correctly visible at COMPUTE_SHADER stage by
        // Vulkan execution-dependency rules.
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.pal_rle_index_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.pal_rle_index_pipeline_layout,
            0,
            &[pal_rle_index_descriptor_set],
            &[],
        );
        let stage3_push: [u32; 1] = [cols];
        let stage3_push_bytes = std::slice::from_raw_parts(
            stage3_push.as_ptr() as *const u8,
            std::mem::size_of_val(&stage3_push),
        );
        self.device.cmd_push_constants(
            cmd,
            self.pal_rle_index_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            stage3_push_bytes,
        );
        // Indirect dispatch: uses palrle_indirect_args_buffer (barriered at Stage 1.5b).
        self.device
            .cmd_dispatch_indirect(cmd, self.palrle_indirect_args_buffer, 0);

        // 5a. Snapshot copy: current (DMA-BUF) → prev_image (owned). This
        // makes prev_image a true point-in-time snapshot of THIS frame for
        // the next frame's SAD comparison. Must run AFTER all three shader
        // dispatches (SAD, NV12, analysis), since they all read from current.
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

        // 5. Barriers: SAD buffer → HOST_READ; NV12 buffer → HOST_READ;
        //    analysis buffer → HOST_READ; palrle compact list + count → HOST_READ;
        //    Stage 2a outputs (frame_palette_set, frame_palette_count,
        //    per_tile_frame_palette_id) → HOST_READ.
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
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.analysis_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.palrle_compact_list_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.palrle_compact_count_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            // Stage 2a outputs
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.frame_palette_set_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.frame_palette_count_buffer)
                .offset(0)
                .size(4),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.per_tile_frame_palette_id_buffer)
                .offset(0)
                .size(pal_id_bytes),
            // Stage 2b output
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.folded_into_buffer)
                .offset(0)
                .size(PALETTE_SLOT_U32_BYTES),
            // Stage 3 output
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.index_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
        ];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
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
        let _ = (
            &sad_ds_guard,
            &nv12_ds_guard,
            &analysis_ds_guard,
            &palrle_compact_ds_guard,
            &palrle_indirect_args_ds_guard,
            &palette_fold_ds_guard,
            &palette_subset_fold_init_ds_guard,
            &palette_subset_fold_ds_guard,
            &pal_rle_index_ds_guard,
            &cmd_guard,
            &fence_guard,
        );

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
            tile_analysis: self.analysis_ptr as *const TileAnalysis,
            tile_analysis_len: cols * rows,
            palrle_compact_list: self.palrle_compact_list_ptr as *const u32,
            palrle_compact_count: *self.palrle_compact_count_ptr,
            frame_palette_set: self.frame_palette_set_ptr as *const FramePaletteEntryRaw,
            frame_palette_set_count: *self.frame_palette_count_ptr,
            per_tile_frame_palette_id: self.per_tile_frame_palette_id_ptr as *const u8,
            folded_into: self.folded_into_ptr as *const u32,
            index_buffer: self.index_buffer_ptr as *const u8,
        })
    }

    /// First-frame helper: NV12 conversion + tile_analysis + snapshot copy.
    ///
    /// On the first frame we have no SAD comparison to do (no prev), so we
    /// run NV12 (chroma subsampling for the encoder) and tile_analysis (so
    /// the classifier sees a populated `unique_colors` on the only frame
    /// with dirty tiles for static content), then
    /// `cmdCopyImage(current → snapshot)` to seed `snapshot` with this
    /// frame's content for subsequent SAD passes.
    ///
    /// All three steps share a single command buffer + fence to keep
    /// first-frame submission cost identical in shape to the steady-state
    /// subsequent-frames branch.
    ///
    /// `snapshot` is always freshly allocated by the caller
    /// (`allocate_owned_image`), so its starting layout is
    /// `vk::ImageLayout::UNDEFINED` — we hardcode that transition.
    unsafe fn run_first_frame_passes(
        &self,
        current: &PrevFrame,
        snapshot: &PrevFrame,
        width: u32,
        height: u32,
        cols: u32,
        rows: u32,
        nv12: Nv12OutputLayout,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Nv12OutputLayout {
            buffer: nv12_buffer,
            y_stride: nv12_y_stride,
            uv_offset: nv12_uv_offset,
            uv_stride: nv12_uv_stride,
        } = nv12;
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

        // Allocate Analysis descriptor set, also RAII-guarded.
        let analysis_set_layouts = [self.analysis_descriptor_set_layout];
        let analysis_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&analysis_set_layouts);
        let analysis_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&analysis_ds_alloc)?,
        };
        let analysis_ds = analysis_ds_guard.sets[0];

        let analysis_image_info = [vk::DescriptorImageInfo::default()
            .image_view(current.view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let analysis_buffer_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.analysis_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let analysis_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(analysis_ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&analysis_image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(analysis_ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&analysis_buffer_info),
        ];
        self.device.update_descriptor_sets(&analysis_writes, &[]);

        // Allocate descriptor sets for Stages 1.5-3 (all RAII-guarded).
        let palrle_compact_set_layouts = [self.palrle_compact_descriptor_set_layout];
        let palrle_compact_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&palrle_compact_set_layouts);
        let palrle_compact_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&palrle_compact_ds_alloc)?,
        };
        let palrle_compact_ds = palrle_compact_ds_guard.sets[0];

        let palrle_indirect_args_set_layouts = [self.palrle_indirect_args_descriptor_set_layout];
        let palrle_indirect_args_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&palrle_indirect_args_set_layouts);
        let palrle_indirect_args_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&palrle_indirect_args_ds_alloc)?,
        };
        let palrle_indirect_args_ds = palrle_indirect_args_ds_guard.sets[0];

        let palette_fold_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(std::slice::from_ref(&self.palette_fold_descriptor_set_layout));
        let palette_fold_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&palette_fold_ds_alloc)?,
        };
        let palette_fold_descriptor_set = palette_fold_ds_guard.sets[0];

        let palette_subset_fold_init_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(std::slice::from_ref(
                &self.palette_subset_fold_init_descriptor_set_layout,
            ));
        let palette_subset_fold_init_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self
                .device
                .allocate_descriptor_sets(&palette_subset_fold_init_ds_alloc)?,
        };
        let palette_subset_fold_init_descriptor_set =
            palette_subset_fold_init_ds_guard.sets[0];

        let palette_subset_fold_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(std::slice::from_ref(
                &self.palette_subset_fold_descriptor_set_layout,
            ));
        let palette_subset_fold_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self
                .device
                .allocate_descriptor_sets(&palette_subset_fold_ds_alloc)?,
        };
        let palette_subset_fold_descriptor_set = palette_subset_fold_ds_guard.sets[0];

        let pal_rle_index_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(std::slice::from_ref(&self.pal_rle_index_descriptor_set_layout));
        let pal_rle_index_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&pal_rle_index_ds_alloc)?,
        };
        let pal_rle_index_descriptor_set = pal_rle_index_ds_guard.sets[0];

        // Write palrle_compact descriptor set.
        let palrle_compact_sad_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.sad_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palrle_compact_analysis_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.analysis_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palrle_compact_list_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.palrle_compact_list_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palrle_compact_count_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.palrle_compact_count_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palrle_compact_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(palrle_compact_ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palrle_compact_sad_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palrle_compact_ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palrle_compact_analysis_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palrle_compact_ds)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palrle_compact_list_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palrle_compact_ds)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palrle_compact_count_info),
        ];
        self.device.update_descriptor_sets(&palrle_compact_writes, &[]);

        // Write palrle_indirect_args descriptor set.
        let palrle_indirect_count_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.palrle_compact_count_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palrle_indirect_args_buf_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.palrle_indirect_args_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palrle_indirect_args_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(palrle_indirect_args_ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palrle_indirect_count_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palrle_indirect_args_ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palrle_indirect_args_buf_info),
        ];
        self.device
            .update_descriptor_sets(&palrle_indirect_args_writes, &[]);

        // Write palette_fold descriptor set.
        let palette_fold_analysis_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.analysis_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palette_fold_compact_list_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.palrle_compact_list_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palette_fold_set_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.frame_palette_set_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palette_fold_count_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.frame_palette_count_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palette_fold_hash_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.hash_table_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palette_fold_per_tile_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.per_tile_frame_palette_id_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palette_fold_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_descriptor_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_analysis_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_descriptor_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_compact_list_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_descriptor_set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_set_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_descriptor_set)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_count_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_descriptor_set)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_hash_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_descriptor_set)
                .dst_binding(5)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_per_tile_info),
        ];
        self.device.update_descriptor_sets(&palette_fold_writes, &[]);

        // Write palette_subset_fold_init descriptor set.
        let subset_fold_init_folded_into_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.folded_into_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let subset_fold_init_writes = [vk::WriteDescriptorSet::default()
            .dst_set(palette_subset_fold_init_descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .buffer_info(&subset_fold_init_folded_into_info)];
        self.device
            .update_descriptor_sets(&subset_fold_init_writes, &[]);

        // Write palette_subset_fold descriptor set.
        let subset_fold_set_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.frame_palette_set_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let subset_fold_folded_into_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.folded_into_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let subset_fold_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(palette_subset_fold_descriptor_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&subset_fold_set_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_subset_fold_descriptor_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&subset_fold_folded_into_info),
        ];
        self.device.update_descriptor_sets(&subset_fold_writes, &[]);

        // Write pal_rle_index descriptor set (Stage 3).
        let pal_rle_index_image_info = [vk::DescriptorImageInfo::default()
            .image_view(current.view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let pal_rle_index_compact_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.palrle_compact_list_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let pal_rle_index_fps_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.frame_palette_set_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let pal_rle_index_ptfpi_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.per_tile_frame_palette_id_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let pal_rle_index_folded_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.folded_into_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let pal_rle_index_out_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.index_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let pal_rle_index_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_descriptor_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&pal_rle_index_image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_descriptor_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_compact_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_descriptor_set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_fps_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_descriptor_set)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_ptfpi_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_descriptor_set)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_folded_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_descriptor_set)
                .dst_binding(5)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_out_info),
        ];
        self.device
            .update_descriptor_sets(&pal_rle_index_writes, &[]);

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
        self.device
            .cmd_dispatch(cmd, nv12_groups_x, nv12_groups_y, 1);

        // Analysis dispatch. No barrier needed vs NV12 — both read `current`
        // read-only (read-after-read is hazard-free) and write to disjoint
        // buffers (`nv12_buffer` vs `self.analysis_buffer`). The
        // COMPUTE→HOST buffer barrier below covers both writes. This mirrors
        // the subsequent-frames branch comment at the SAD/NV12/Analysis
        // dispatch trio.
        self.device
            .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.analysis_pipeline);
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.analysis_pipeline_layout,
            0,
            &[analysis_ds],
            &[],
        );
        let analysis_push: [u32; 3] = [width, height, cols];
        let analysis_push_bytes = std::slice::from_raw_parts(
            analysis_push.as_ptr() as *const u8,
            std::mem::size_of_val(&analysis_push),
        );
        self.device.cmd_push_constants(
            cmd,
            self.analysis_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            analysis_push_bytes,
        );
        self.device.cmd_dispatch(cmd, cols, rows, 1);

        // ============================================================
        // Stages 1.5–3: PalRle compact, indirect args, palette fold,
        // subset fold, and index generation.
        //
        // No prev_image on the first frame means SAD didn't run.
        // Pre-fill sad_buffer with 0xFFFFFFFF so palrle_compact sees
        // every tile as dirty (SAD_THRESHOLD = 64, and
        // 0xFFFFFFFF > 64 is always TRUE).
        // ============================================================
        self.device
            .cmd_fill_buffer(cmd, self.sad_buffer, 0, vk::WHOLE_SIZE, 0xFFFF_FFFFu32);

        // Inter-dispatch barrier: analysis write + sad pre-fill must be
        // visible to Stage 1.5a reads.
        // srcStageMask covers COMPUTE_SHADER (analysis write) and TRANSFER
        // (cmd_fill_buffer).
        let buf_barrier_inputs = [
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.sad_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.analysis_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
        ];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &buf_barrier_inputs,
            &[],
        );

        // Stage 1.5: zero compact_count, dispatch compact scan, dispatch indirect-args writer.
        self.device
            .cmd_fill_buffer(cmd, self.palrle_compact_count_buffer, 0, 4, 0);

        // Barrier: transfer-write → shader-read/write
        let buf_barrier_fill = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.palrle_compact_count_buffer)
            .offset(0)
            .size(4);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_fill),
            &[],
        );

        // Stage 1.5a: bind palrle_compact pipeline.
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palrle_compact_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palrle_compact_pipeline_layout,
            0,
            &[palrle_compact_ds],
            &[],
        );
        let compact_push: [u32; 3] = [cols, rows, SAD_THRESHOLD];
        let compact_push_bytes = std::slice::from_raw_parts(
            compact_push.as_ptr() as *const u8,
            std::mem::size_of_val(&compact_push),
        );
        self.device.cmd_push_constants(
            cmd,
            self.palrle_compact_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            compact_push_bytes,
        );
        self.device.cmd_dispatch(cmd, cols * rows, 1, 1);

        // Barrier between Stage 1.5a and 1.5b: shader write → shader read on count.
        let buf_barrier_count = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.palrle_compact_count_buffer)
            .offset(0)
            .size(4);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_count),
            &[],
        );

        // Stage 1.5b: indirect-args writer.
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palrle_indirect_args_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palrle_indirect_args_pipeline_layout,
            0,
            &[palrle_indirect_args_ds],
            &[],
        );
        self.device.cmd_dispatch(cmd, 1, 1, 1);

        // Barrier for future stages to consume indirect args.
        let buf_barrier_args = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(
                vk::AccessFlags::INDIRECT_COMMAND_READ | vk::AccessFlags::SHADER_READ,
            )
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.palrle_indirect_args_buffer)
            .offset(0)
            .size(12);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::DRAW_INDIRECT | vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_args),
            &[],
        );

        // ============================================================
        // Stage 2a: palette_fold (intra-frame palette dedup)
        // ============================================================
        let pal_id_bytes = ((self.max_tiles + 3) & !3u32) as vk::DeviceSize;
        self.device
            .cmd_fill_buffer(cmd, self.frame_palette_set_buffer, 0, 20480, 0);
        self.device
            .cmd_fill_buffer(cmd, self.frame_palette_count_buffer, 0, 4, 0);
        self.device
            .cmd_fill_buffer(cmd, self.hash_table_buffer, 0, PALETTE_SLOT_U32_BYTES, 0);
        self.device
            .cmd_fill_buffer(cmd, self.per_tile_frame_palette_id_buffer, 0, pal_id_bytes, 0);

        // Barrier: zero-fills + compact_list write must be visible to Stage 2a.
        let stage_2a_input_barriers = [
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.frame_palette_set_buffer)
                .offset(0)
                .size(20480),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.frame_palette_count_buffer)
                .offset(0)
                .size(4),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.hash_table_buffer)
                .offset(0)
                .size(PALETTE_SLOT_U32_BYTES),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.per_tile_frame_palette_id_buffer)
                .offset(0)
                .size(pal_id_bytes),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.palrle_compact_list_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
        ];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &stage_2a_input_barriers,
            &[],
        );

        // Stage 2a: indirect dispatch.
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palette_fold_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palette_fold_pipeline_layout,
            0,
            &[palette_fold_descriptor_set],
            &[],
        );
        self.device
            .cmd_dispatch_indirect(cmd, self.palrle_indirect_args_buffer, 0);

        // Barrier: Stage 2a outputs available to downstream stages.
        let stage_2a_output_barriers = [
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.frame_palette_set_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.frame_palette_count_buffer)
                .offset(0)
                .size(4),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.per_tile_frame_palette_id_buffer)
                .offset(0)
                .size(pal_id_bytes),
        ];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &stage_2a_output_barriers,
            &[],
        );

        // ============================================================
        // Stage 2b init: write default-self sentinel to folded_into
        // ============================================================
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palette_subset_fold_init_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palette_subset_fold_init_pipeline_layout,
            0,
            &[palette_subset_fold_init_descriptor_set],
            &[],
        );
        self.device.cmd_dispatch(cmd, 1, 1, 1);

        // Barrier between init and fold: folded_into writes must be visible.
        let stage_2b_init_to_fold_barrier = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.folded_into_buffer)
            .offset(0)
            .size(PALETTE_SLOT_U32_BYTES);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&stage_2b_init_to_fold_barrier),
            &[],
        );

        // ============================================================
        // Stage 2b fold: atomicMin-based subset detection
        // ============================================================
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palette_subset_fold_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.palette_subset_fold_pipeline_layout,
            0,
            &[palette_subset_fold_descriptor_set],
            &[],
        );
        self.device.cmd_dispatch(cmd, PALETTE_HASH_SLOTS as u32, 1, 1);

        // Barrier for Stage 3 consumers and HOST readback.
        let stage_2b_output_barrier = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.folded_into_buffer)
            .offset(0)
            .size(PALETTE_SLOT_U32_BYTES);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&stage_2b_output_barrier),
            &[],
        );

        // ============================================================
        // Stage 3: pal_rle_index
        // ============================================================
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.pal_rle_index_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.pal_rle_index_pipeline_layout,
            0,
            &[pal_rle_index_descriptor_set],
            &[],
        );
        let stage3_push: [u32; 1] = [cols];
        let stage3_push_bytes = std::slice::from_raw_parts(
            stage3_push.as_ptr() as *const u8,
            std::mem::size_of_val(&stage3_push),
        );
        self.device.cmd_push_constants(
            cmd,
            self.pal_rle_index_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            stage3_push_bytes,
        );
        self.device
            .cmd_dispatch_indirect(cmd, self.palrle_indirect_args_buffer, 0);

        // Snapshot copy: current (GENERAL) → snapshot (TRANSFER_DST_OPTIMAL).
        // `snapshot` is always freshly allocated by `allocate_owned_image` and
        // its initial layout is UNDEFINED, so we hardcode that transition (no
        // prior shader access to wait on).
        //
        // Stage 3 reads current.image (SHADER_READ). The srcAccessMask for
        // current covers all compute reads up to this point.
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

        // All output buffers → HOST_READ. These are HOST_VISIBLE | HOST_COHERENT
        // and consumed via raw pointer on return.
        // srcStageMask covers both COMPUTE_SHADER (shader writes) and TRANSFER
        // (cmd_fill_buffer on sad_buffer and palrle_compact_count_buffer).
        let buf_barrier = [
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
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
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.analysis_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.palrle_compact_list_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.palrle_compact_count_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            // Stage 2a outputs
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.frame_palette_set_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.frame_palette_count_buffer)
                .offset(0)
                .size(4),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.per_tile_frame_palette_id_buffer)
                .offset(0)
                .size(pal_id_bytes),
            // Stage 2b output
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.folded_into_buffer)
                .offset(0)
                .size(PALETTE_SLOT_U32_BYTES),
            // Stage 3 output
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.index_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
        ];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
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

        // Per-frame transients (descriptor sets, command buffer, fence) are
        // freed when their RAII guards drop at end of scope. Touch them here
        // to keep them alive past `wait_for_fences`.
        let _ = (
            &nv12_ds_guard,
            &analysis_ds_guard,
            &palrle_compact_ds_guard,
            &palrle_indirect_args_ds_guard,
            &palette_fold_ds_guard,
            &palette_subset_fold_init_ds_guard,
            &palette_subset_fold_ds_guard,
            &pal_rle_index_ds_guard,
            &cmd_guard,
            &fence_guard,
        );

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

        let image_mem_type =
            find_memory_type(&mem_props, u32::MAX, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .or_else(|| {
                    find_memory_type(&mem_props, u32::MAX, vk::MemoryPropertyFlags::empty())
                })
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
        .or_else(|| {
            find_memory_type(
                &mem_props,
                mem_reqs.memory_type_bits,
                vk::MemoryPropertyFlags::empty(),
            )
        })
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

            // Analysis buffer
            self.device.unmap_memory(self.analysis_memory);
            self.device.destroy_buffer(self.analysis_buffer, None);
            self.device.free_memory(self.analysis_memory, None);

            // PalRLE compact list buffer
            self.device.unmap_memory(self.palrle_compact_list_memory);
            self.device
                .destroy_buffer(self.palrle_compact_list_buffer, None);
            self.device.free_memory(self.palrle_compact_list_memory, None);

            // PalRLE compact count buffer
            self.device.unmap_memory(self.palrle_compact_count_memory);
            self.device
                .destroy_buffer(self.palrle_compact_count_buffer, None);
            self.device
                .free_memory(self.palrle_compact_count_memory, None);

            // PalRLE indirect args buffer (no CPU mapping to unmap)
            self.device
                .destroy_buffer(self.palrle_indirect_args_buffer, None);
            self.device
                .free_memory(self.palrle_indirect_args_memory, None);

            // palette_fold buffers
            self.device.unmap_memory(self.per_tile_frame_palette_id_memory);
            self.device
                .destroy_buffer(self.per_tile_frame_palette_id_buffer, None);
            self.device
                .free_memory(self.per_tile_frame_palette_id_memory, None);

            // hash_table buffer (no persistent CPU mapping)
            self.device.destroy_buffer(self.hash_table_buffer, None);
            self.device.free_memory(self.hash_table_memory, None);

            self.device.unmap_memory(self.frame_palette_count_memory);
            self.device
                .destroy_buffer(self.frame_palette_count_buffer, None);
            self.device
                .free_memory(self.frame_palette_count_memory, None);

            self.device.unmap_memory(self.frame_palette_set_memory);
            self.device
                .destroy_buffer(self.frame_palette_set_buffer, None);
            self.device
                .free_memory(self.frame_palette_set_memory, None);

            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);

            // NV12 pipeline
            self.device.destroy_pipeline(self.nv12_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.nv12_pipeline_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.nv12_descriptor_set_layout, None);
            self.device
                .destroy_shader_module(self.nv12_shader_module, None);

            // SAD pipeline
            self.device.destroy_pipeline(self.pipeline, None);
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            self.device.destroy_shader_module(self.shader_module, None);

            // Analysis pipeline
            self.device.destroy_pipeline(self.analysis_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.analysis_pipeline_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.analysis_descriptor_set_layout, None);
            self.device
                .destroy_shader_module(self.analysis_shader_module, None);

            // PalRLE compact pipeline
            self.device
                .destroy_pipeline(self.palrle_compact_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.palrle_compact_pipeline_layout, None);
            self.device.destroy_descriptor_set_layout(
                self.palrle_compact_descriptor_set_layout,
                None,
            );
            self.device
                .destroy_shader_module(self.palrle_compact_shader_module, None);

            // PalRLE indirect-args pipeline
            self.device
                .destroy_pipeline(self.palrle_indirect_args_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.palrle_indirect_args_pipeline_layout, None);
            self.device.destroy_descriptor_set_layout(
                self.palrle_indirect_args_descriptor_set_layout,
                None,
            );
            self.device
                .destroy_shader_module(self.palrle_indirect_args_shader_module, None);

            // palette_fold pipeline
            self.device
                .destroy_pipeline(self.palette_fold_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.palette_fold_pipeline_layout, None);
            self.device.destroy_descriptor_set_layout(
                self.palette_fold_descriptor_set_layout,
                None,
            );
            self.device
                .destroy_shader_module(self.palette_fold_shader_module, None);

            // palette_subset_fold_init pipeline
            self.device
                .destroy_pipeline(self.palette_subset_fold_init_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.palette_subset_fold_init_pipeline_layout, None);
            self.device.destroy_descriptor_set_layout(
                self.palette_subset_fold_init_descriptor_set_layout,
                None,
            );
            self.device
                .destroy_shader_module(self.palette_subset_fold_init_shader_module, None);

            // palette_subset_fold pipeline
            self.device
                .destroy_pipeline(self.palette_subset_fold_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.palette_subset_fold_pipeline_layout, None);
            self.device.destroy_descriptor_set_layout(
                self.palette_subset_fold_descriptor_set_layout,
                None,
            );
            self.device
                .destroy_shader_module(self.palette_subset_fold_shader_module, None);

            // folded_into buffer
            self.device.unmap_memory(self.folded_into_memory);
            self.device.destroy_buffer(self.folded_into_buffer, None);
            self.device.free_memory(self.folded_into_memory, None);

            // pal_rle_index pipeline (Stage 3)
            self.device
                .destroy_pipeline(self.pal_rle_index_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.pal_rle_index_pipeline_layout, None);
            self.device.destroy_descriptor_set_layout(
                self.pal_rle_index_descriptor_set_layout,
                None,
            );
            self.device
                .destroy_shader_module(self.pal_rle_index_shader_module, None);

            // index_buffer (Stage 3 output)
            self.device.unmap_memory(self.index_buffer_memory);
            self.device.destroy_buffer(self.index_buffer, None);
            self.device.free_memory(self.index_buffer_memory, None);

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


#[cfg(test)]
#[path = "gpu_pipeline_tests.rs"]
mod tests;
