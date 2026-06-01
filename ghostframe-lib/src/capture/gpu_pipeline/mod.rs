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

/// Pixels-per-tile dimension (matches shader local_size_x/y).
const TILE_SIZE: u32 = 32;

/// SAD score above which a tile is considered dirty.
/// Chosen to ignore sub-pixel rounding noise while catching any visible change.
const SAD_THRESHOLD: u32 = 64;

/// The sentinel value stored in `TileAnalysis::count` when the GPU analysis
/// pass could not count unique colors (e.g., early-exit due to complexity).
/// Matches `tile::UNIQUE_COLORS_UNKNOWN` (u16::MAX = 65535) cast to u32.
const UNIQUE_COLORS_UNKNOWN_SENTINEL: u32 = u16::MAX as u32;

/// M3.3c escalation cap: maximum number of idle-escalation candidates the
/// io_bridge will dispatch in a single frame. Bounds GPU memory for the
/// dedicated escalation_coefficients_buffer (K × 3072 × 4 = 6 MB at K=512).
pub const MAX_ESCALATION_PER_FRAME: usize = 512;

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

    // Cdf53 compact pipeline (Stage 4a, added M3.3a)
    cdf53_compact_shader_module: vk::ShaderModule,
    cdf53_compact_pipeline: vk::Pipeline,
    cdf53_compact_pipeline_layout: vk::PipelineLayout,
    cdf53_compact_descriptor_set_layout: vk::DescriptorSetLayout,
    // HOST_VISIBLE | HOST_COHERENT, persistently mapped. One u32 per tile.
    cdf53_compact_list_buffer: vk::Buffer,
    cdf53_compact_list_memory: vk::DeviceMemory,
    // Phase B in io_bridge reads this to iterate over Cdf53-classified tiles.
    pub cdf53_compact_list_ptr: *const u32,
    // HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST, persistently mapped. 4 bytes.
    cdf53_compact_count_buffer: vk::Buffer,
    cdf53_compact_count_memory: vk::DeviceMemory,
    // Phase B in io_bridge reads this to know how many Cdf53 tiles to process.
    pub cdf53_compact_count_ptr: *const u32,

    // Cdf53 indirect-args pipeline (Stage 4b, added M3.3a)
    cdf53_dispatch_args_buffer: vk::Buffer,
    cdf53_dispatch_args_memory: vk::DeviceMemory,
    // No CPU pointer — HOST_VISIBLE only for simple alloc; read by GPU via
    // vkCmdDispatchIndirect. Written once per frame by cdf53_indirect_args shader.

    cdf53_indirect_args_shader_module: vk::ShaderModule,
    cdf53_indirect_args_pipeline: vk::Pipeline,
    cdf53_indirect_args_pipeline_layout: vk::PipelineLayout,
    cdf53_indirect_args_descriptor_set_layout: vk::DescriptorSetLayout,

    // Cdf53 coefficient buffer (Stage 4c, added M3.3a Task 7)
    // max_tiles * 3 channels * 1024 i32 = max_tiles * 12 KiB.
    // HOST_VISIBLE | HOST_COHERENT, persistently mapped for Phase B readback.
    cdf53_coefficients_buffer: vk::Buffer,
    cdf53_coefficients_memory: vk::DeviceMemory,
    // Persistent CPU mapping of the coefficient buffer.
    // Each compact slot `c` occupies `3 * 1024` i32 entries at
    // offset `c * 3 * 1024`. Phase B reads and casts each i32 to i16.
    pub cdf53_coefficients_ptr: *const i32,

    // M3.3c — idle-escalation second-dispatch resources. CPU writes the
    // escalation list + count via host-coherent buffer; second invocation of
    // the cdf53_forward_l{1,2,3} shaders reads them and writes coefficients
    // into the dedicated escalation_coefficients_buffer. K = MAX_ESCALATION_PER_FRAME
    // bounds the dispatch + the buffer sizes.
    pub(crate) cdf53_escalation_list_buffer: vk::Buffer,
    pub(crate) cdf53_escalation_list_memory: vk::DeviceMemory,
    /// CPU writes tile-idx values via this pointer (host-coherent).
    pub cdf53_escalation_list_ptr: *mut u32,

    pub(crate) cdf53_escalation_count_buffer: vk::Buffer,
    pub(crate) cdf53_escalation_count_memory: vk::DeviceMemory,
    /// CPU writes per-frame escalation count via this pointer (host-coherent).
    pub cdf53_escalation_count_ptr: *mut u32,

    pub(crate) cdf53_escalation_coefficients_buffer: vk::Buffer,
    pub(crate) cdf53_escalation_coefficients_memory: vk::DeviceMemory,
    /// CPU reads escalation forward coefficients via this pointer post-fence.
    pub cdf53_escalation_coefficients_ptr: *const i32,

    // Cdf53 forward L1 pipeline (Stage 4c-L1, added M3.3a Task 7)
    cdf53_forward_l1_shader_module: vk::ShaderModule,
    cdf53_forward_l1_pipeline: vk::Pipeline,
    cdf53_forward_l1_pipeline_layout: vk::PipelineLayout,
    cdf53_forward_l1_descriptor_set_layout: vk::DescriptorSetLayout,

    // Cdf53 forward L2 pipeline (Stage 4c-L2, added M3.3a Task 7)
    cdf53_forward_l2_shader_module: vk::ShaderModule,
    cdf53_forward_l2_pipeline: vk::Pipeline,
    cdf53_forward_l2_pipeline_layout: vk::PipelineLayout,
    cdf53_forward_l2_descriptor_set_layout: vk::DescriptorSetLayout,

    // Cdf53 forward L3 pipeline (Stage 4c-L3, added M3.3a Task 7)
    cdf53_forward_l3_shader_module: vk::ShaderModule,
    cdf53_forward_l3_pipeline: vk::Pipeline,
    cdf53_forward_l3_pipeline_layout: vk::PipelineLayout,
    cdf53_forward_l3_descriptor_set_layout: vk::DescriptorSetLayout,

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

    /// Counter for the post-session-reset cushion. While > 0, the first-frame
    /// branch in `process_frame_with_imported` skips the snapshot copy and
    /// leaves `prev_image = None`, so every frame is reported fully dirty
    /// without committing a new baseline. Mirrors the CPU path's
    /// `force_dirty_frames` no-commit semantics.
    force_all_dirty_remaining: u32,

    /// Staged count from the most recent `set_escalation_candidates` call.
    /// Copied from the HOST_COHERENT pointer into a plain field so the
    /// command-buffer recording paths can read it without a raw-pointer
    /// deref mid-build. Reset to 0 at the start of each `process_frame_inner`
    /// so a frame that skips `set_escalation_candidates` doesn't replay the
    /// previous frame's list.
    staged_escalation_count: u32,
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

/// Frame pixel dimensions together with the derived tile-grid dimensions.
/// `cols` / `rows` are `width.div_ceil(TILE_SIZE)` / `height.div_ceil(TILE_SIZE)`.
#[derive(Clone, Copy)]
struct FrameGeometry {
    width: u32,
    height: u32,
    cols: u32,
    rows: u32,
}

impl FrameGeometry {
    fn from_dims(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            cols: width.div_ceil(TILE_SIZE),
            rows: height.div_ceil(TILE_SIZE),
        }
    }

    fn tile_count(&self) -> u32 {
        self.cols * self.rows
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

    /// CPU-pixels variant of [`process_frame`]: same SAD + NV12 + classifier
    /// pipeline, but the current frame's BGRA pixels are uploaded from host
    /// memory instead of imported via DMA-BUF.
    ///
    /// `stride` is the source row stride in bytes (must be >= width*4).
    /// `pixels.len()` must be at least `stride * height`.
    pub fn process_frame_from_pixels(
        &mut self,
        pixels: &[u8],
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<FrameAnalysis, Box<dyn std::error::Error>> {
        unsafe { self.process_frame_inner_from_pixels(pixels, width, height, stride) }
    }

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

    /// Stage the per-frame idle-escalation candidate list. Must be called
    /// before `process_frame` to take effect on the *current* frame
    /// (`process_frame_inner` consumes `staged_escalation_count` at entry and
    /// resets it to 0; the count fires the Stage 4d forward dispatch recorded
    /// into that frame's command buffer).
    ///
    /// `candidates` is a slice of flat tile indices (row-major). The slice is
    /// truncated to `MAX_ESCALATION_PER_FRAME` if longer. Writes are to
    /// HOST_COHERENT buffers — visible to the GPU on the next submit without
    /// an explicit flush.
    pub fn set_escalation_candidates(&mut self, candidates: &[u32]) {
        let n = candidates.len().min(MAX_ESCALATION_PER_FRAME);
        // SAFETY: cdf53_escalation_list_ptr is a HOST_VISIBLE | HOST_COHERENT
        // persistently-mapped pointer into a buffer of MAX_ESCALATION_PER_FRAME
        // u32 entries, allocated in setup.rs. Writing `n` entries is within bounds.
        unsafe {
            std::ptr::copy_nonoverlapping(candidates.as_ptr(), self.cdf53_escalation_list_ptr, n);
            *self.cdf53_escalation_count_ptr = n as u32;
        }
        self.staged_escalation_count = n as u32;
    }

    /// Invalidate the GPU SAD baseline by dropping `prev_image` and arming the
    /// no-snapshot cushion for the next `force_frames` frames.
    ///
    /// While the cushion is active, every frame is reported as fully dirty
    /// without snapshotting a new baseline — datagrams dropped during the
    /// cushion period (e.g. QUIC slow-start, or a lossy→lossless repaint
    /// burst) naturally re-surface as dirty until the cushion exhausts and
    /// the next frame becomes the first real snapshot.
    ///
    /// Call sites:
    /// - `fire_session_reset` calls with `force_frames = 20` to cover QUIC
    ///   slow-start datagram loss after a new session connects.
    /// - The H264 → TileCodec mode-flip handoff in `process_frame_gpu` calls
    ///   with `force_frames = 1` to trigger a one-shot lossless full-repaint
    ///   that overwrites the H.264 lossy render.
    pub fn invalidate_baseline(&mut self, force_frames: u32) {
        unsafe {
            // device_wait_idle ensures no in-flight GPU work still references
            // the prev_image we're about to destroy. The Drop impl uses the
            // same pattern.
            self.device.device_wait_idle().ok();
            if let Some(prev) = self.prev_image.take() {
                self.destroy_prev_frame(prev);
            }
        }
        self.force_all_dirty_remaining = force_frames;
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

            // Cdf53 compact buffers (Stage 4a)
            self.device.unmap_memory(self.cdf53_compact_list_memory);
            self.device
                .destroy_buffer(self.cdf53_compact_list_buffer, None);
            self.device
                .free_memory(self.cdf53_compact_list_memory, None);

            self.device.unmap_memory(self.cdf53_compact_count_memory);
            self.device
                .destroy_buffer(self.cdf53_compact_count_buffer, None);
            self.device
                .free_memory(self.cdf53_compact_count_memory, None);

            // Cdf53 compact pipeline (Stage 4a)
            self.device
                .destroy_pipeline(self.cdf53_compact_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.cdf53_compact_pipeline_layout, None);
            self.device.destroy_descriptor_set_layout(
                self.cdf53_compact_descriptor_set_layout,
                None,
            );
            self.device
                .destroy_shader_module(self.cdf53_compact_shader_module, None);

            // Cdf53 indirect-args buffer (Stage 4b)
            self.device.destroy_buffer(self.cdf53_dispatch_args_buffer, None);
            self.device.free_memory(self.cdf53_dispatch_args_memory, None);

            // Cdf53 indirect-args pipeline (Stage 4b)
            self.device
                .destroy_pipeline(self.cdf53_indirect_args_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.cdf53_indirect_args_pipeline_layout, None);
            self.device.destroy_descriptor_set_layout(
                self.cdf53_indirect_args_descriptor_set_layout,
                None,
            );
            self.device
                .destroy_shader_module(self.cdf53_indirect_args_shader_module, None);

            // Cdf53 coefficient buffer (Stage 4c)
            self.device.unmap_memory(self.cdf53_coefficients_memory);
            self.device.destroy_buffer(self.cdf53_coefficients_buffer, None);
            self.device.free_memory(self.cdf53_coefficients_memory, None);

            // M3.3c escalation buffers
            self.device.unmap_memory(self.cdf53_escalation_list_memory);
            self.device.destroy_buffer(self.cdf53_escalation_list_buffer, None);
            self.device.free_memory(self.cdf53_escalation_list_memory, None);

            self.device.unmap_memory(self.cdf53_escalation_count_memory);
            self.device.destroy_buffer(self.cdf53_escalation_count_buffer, None);
            self.device.free_memory(self.cdf53_escalation_count_memory, None);

            self.device.unmap_memory(self.cdf53_escalation_coefficients_memory);
            self.device.destroy_buffer(self.cdf53_escalation_coefficients_buffer, None);
            self.device.free_memory(self.cdf53_escalation_coefficients_memory, None);

            // Cdf53 forward L1 pipeline (Stage 4c-L1)
            self.device
                .destroy_pipeline(self.cdf53_forward_l1_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.cdf53_forward_l1_pipeline_layout, None);
            self.device.destroy_descriptor_set_layout(
                self.cdf53_forward_l1_descriptor_set_layout,
                None,
            );
            self.device
                .destroy_shader_module(self.cdf53_forward_l1_shader_module, None);

            // Cdf53 forward L2 pipeline (Stage 4c-L2)
            self.device
                .destroy_pipeline(self.cdf53_forward_l2_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.cdf53_forward_l2_pipeline_layout, None);
            self.device.destroy_descriptor_set_layout(
                self.cdf53_forward_l2_descriptor_set_layout,
                None,
            );
            self.device
                .destroy_shader_module(self.cdf53_forward_l2_shader_module, None);

            // Cdf53 forward L3 pipeline (Stage 4c-L3)
            self.device
                .destroy_pipeline(self.cdf53_forward_l3_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.cdf53_forward_l3_pipeline_layout, None);
            self.device.destroy_descriptor_set_layout(
                self.cdf53_forward_l3_descriptor_set_layout,
                None,
            );
            self.device
                .destroy_shader_module(self.cdf53_forward_l3_shader_module, None);

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

mod setup;
mod frame;

#[cfg(test)]
mod tests;
