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

/// Byte size of the frame palette set buffer: one 80-byte
/// [`FramePaletteEntryRaw`] per palette hash slot.
pub(crate) const FRAME_PALETTE_SET_BYTES: vk::DeviceSize =
    (PALETTE_HASH_SLOTS * std::mem::size_of::<FramePaletteEntryRaw>()) as vk::DeviceSize;
const _: () = assert!(FRAME_PALETTE_SET_BYTES == 20480);

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
pub(crate) const MAX_ESCALATION_PER_FRAME: usize = 512;

/// The most tiles one frame may have, which sizes the processor's buffers
/// (about 13 KB of host memory per tile, 200 MB at this value). It must cover
/// the largest screen the server is asked to serve: one 4K screen is 8160
/// tiles and two side by side 16320. A frame above it fails every time, and
/// with no software path a failing frame is a frozen screen, so this is a
/// ceiling to raise deliberately, not a default to forget.
pub(crate) const MAX_FRAME_TILES: u32 = 16_384;

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
    /// 256 slots of `FramePaletteEntryRaw`; only the hash slots a tile claimed
    /// (below `frame_palette_set_count`) contain valid data.
    /// Valid until next process_frame call.
    pub frame_palette_set: *const FramePaletteEntryRaw,
    /// High-water mark of claimed hash slots (highest claimed slot id + 1),
    /// NOT a count of unique palettes: slots are hash-indexed, so it is
    /// usually larger than the number of distinct palettes. Observability only;
    /// resolve palettes through `per_tile_frame_palette_id`.
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

/// A GPU-written compact count is untrusted: the compact lists hold at most
/// one entry per tile, so anything above the tile count is a corrupt value.
pub(crate) fn clamp_gpu_count(raw: u32, tile_count: u32) -> u32 {
    raw.min(tile_count)
}

impl FrameAnalysis {
    /// `palrle_compact_count` limited to what the compact list can hold.
    fn compact_len(&self) -> usize {
        clamp_gpu_count(self.palrle_compact_count, self.tile_analysis_len) as usize
    }

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
        if self.palrle_compact_list.is_null() || self.compact_len() == 0 {
            return &[];
        }
        // SAFETY: pointer is into HOST_VISIBLE mapped GPU memory owned by
        // GpuFrameProcessor. The borrow checker prevents the returned slice
        // from outliving the &self borrow; the caller must not hold this
        // FrameAnalysis across a subsequent process_frame() call.
        unsafe { std::slice::from_raw_parts(self.palrle_compact_list, self.compact_len()) }
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
        unsafe { std::slice::from_raw_parts(self.frame_palette_set, PALETTE_HASH_SLOTS) }
    }

    pub fn per_tile_frame_palette_id_slice(&self) -> &[u8] {
        if self.per_tile_frame_palette_id.is_null() || self.compact_len() == 0 {
            return &[];
        }
        // SAFETY: pointer is into HOST_VISIBLE mapped GPU memory owned by
        // GpuFrameProcessor. The borrow checker prevents the returned slice
        // from outliving the &self borrow; the caller must not hold this
        // FrameAnalysis across a subsequent process_frame() call.
        // Length = palrle_compact_count (one byte per compact slot).
        unsafe { std::slice::from_raw_parts(self.per_tile_frame_palette_id, self.compact_len()) }
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
        if c >= self.compact_len() {
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
    /// Persistent per-frame command buffer (allocated once from `command_pool`,
    /// which has RESET_COMMAND_BUFFER) and its fence; see `begin_frame_cmd`.
    frame_cmd: vk::CommandBuffer,
    frame_fence: vk::Fence,
    /// True between a successful `queue_submit` and the matching successful
    /// `wait_for_fences`.
    frame_fence_pending: std::cell::Cell<bool>,

    // SAD pipeline
    sad: ComputeStage,

    // NV12 pipeline
    nv12: ComputeStage,

    // Tile-analysis pipeline (new)
    analysis: ComputeStage,
    // HOST_VISIBLE | HOST_COHERENT, persistently mapped. Layout matches
    // `TileAnalysis` (80 bytes per tile).
    analysis_buf: HostBuffer<TileAnalysis>,

    // PalRLE compact pipeline (Stage 1.5)
    palrle_compact: ComputeStage,
    // HOST_VISIBLE | HOST_COHERENT, persistently mapped. One u32 per tile.
    // Task 10b will read this pointer to retrieve the compact tile index list.
    palrle_compact_list: HostBuffer<u32>,
    // HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST, persistently mapped. 4 bytes.
    // Task 10b will read this pointer to retrieve the compact tile count.
    palrle_compact_count: HostBuffer<u32>,

    // Cdf53 compact pipeline (Stage 4a, added M3.3a)
    cdf53_compact: ComputeStage,
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
    cdf53_dispatch_args: HostBuffer<u32>,
    // No CPU pointer — HOST_VISIBLE only for simple alloc; read by GPU via
    // vkCmdDispatchIndirect. Written once per frame by cdf53_indirect_args shader.
    cdf53_indirect_args: ComputeStage,

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
    /// CPU writes tile-idx values via this pointer (host-coherent).
    cdf53_escalation_list: HostBuffer<u32>,

    pub(crate) cdf53_escalation_coefficients_buffer: vk::Buffer,
    pub(crate) cdf53_escalation_coefficients_memory: vk::DeviceMemory,
    /// CPU reads escalation forward coefficients via this pointer post-fence.
    pub cdf53_escalation_coefficients_ptr: *const i32,

    // CDF 5/3 fallback forward (`forward_cdf53_tiles`): tiles the CPU rejected
    // for PalRle after Phase A. Separate from the escalation buffers so the
    // call never aliases a frame's escalation output, which matters as soon as
    // a backend pipelines frames (today `finish_tile` has already copied the
    // escalation coefficients out before the fallback runs).
    cdf53_fallback_list: HostBuffer<u32>,
    cdf53_fallback_coefficients: HostBuffer<i32>,

    // Cdf53 forward L1 pipeline (Stage 4c-L1, added M3.3a Task 7)
    cdf53_forward_l1: ComputeStage,

    // Cdf53 forward L2 pipeline (Stage 4c-L2, added M3.3a Task 7)
    cdf53_forward_l2: ComputeStage,

    // Cdf53 forward L3 pipeline (Stage 4c-L3, added M3.3a Task 7)
    cdf53_forward_l3: ComputeStage,

    // PalRLE indirect-args pipeline (Stage 1.5b)
    palrle_indirect_args: ComputeStage,
    // HOST_VISIBLE | HOST_COHERENT, size = 12 bytes. Written by shader; used as
    // INDIRECT_BUFFER for vkCmdDispatchIndirect in Task 10b.
    palrle_indirect_args_buf: HostBuffer<u32>,

    // palette_fold pipeline (Stage 2a)
    palette_fold: ComputeStage,
    // HOST_VISIBLE | HOST_COHERENT, FRAME_PALETTE_SET_BYTES. Persistently mapped.
    // Persistent CPU mapping of the frame palette set buffer; pointed to by
    // FrameAnalysis::frame_palette_set.
    frame_palette_set: HostBuffer<FramePaletteEntryRaw>,
    // HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST, 4 bytes. Persistently mapped.
    // Persistent CPU mapping of the frame palette count buffer; pointed to by
    // FrameAnalysis::frame_palette_set_count (high-water mark of claimed slot IDs).
    frame_palette_count: HostBuffer<u32>,
    // HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST, 256 * 4 = 1024 bytes.
    // No persistent CPU mapping needed (Stage 2a is the only consumer).
    hash_table: HostBuffer<u32>,
    // HOST_VISIBLE | HOST_COHERENT | TRANSFER_DST, (max_tiles+3)/4*4 bytes. Persistently mapped.
    // Persistent CPU mapping of the per-tile frame palette ID buffer; pointed
    // to by FrameAnalysis::per_tile_frame_palette_id.
    per_tile_frame_palette_id: HostBuffer<u8>,

    // palette_subset_fold_init pipeline (Stage 2b init)
    palette_subset_fold_init: ComputeStage,

    // palette_subset_fold pipeline (Stage 2b)
    palette_subset_fold: ComputeStage,

    // Stage 2b output: folded_into[i] encodes the best superset for slot i.
    // 256 × 4 = 1024 bytes. HOST_VISIBLE | HOST_COHERENT | STORAGE_BUFFER | TRANSFER_DST.
    // Persistently mapped; Task 12b will read this after the Stage 2b dispatch.
    // Persistent CPU mapping of the folded_into buffer; pointed to by
    // FrameAnalysis::folded_into after Stage 2b dispatch.
    folded_into: HostBuffer<u32>,

    // pal_rle_index pipeline (Stage 3)
    pal_rle_index: ComputeStage,
    // HOST_VISIBLE | HOST_COHERENT, size = max_tiles * 512 bytes.
    // Each tile occupies 128 u32 entries (512 bytes) of packed 4-bit indices.
    // Persistently mapped for CPU readback after Stage 3 dispatch.
    // Persistent CPU mapping of the index buffer; pointed to by
    // FrameAnalysis::index_buffer after Stage 3 dispatch.
    index: HostBuffer<u8>,

    descriptor_pool: vk::DescriptorPool,
    prev_image: Option<PrevFrame>,

    // SAD output buffer (HOST_VISIBLE, persistently mapped)
    sad_buf: HostBuffer<u32>,
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

    /// Which ingest path the last `process_frame` took (`None` before the first).
    last_ingest: Option<IngestTier>,

    /// Staged count from the most recent `set_escalation_candidates` call.
    /// Copied from the HOST_COHERENT pointer into a plain field so the
    /// command-buffer recording paths can read it without a raw-pointer
    /// deref mid-build. Reset to 0 at the start of each `process_frame_inner`
    /// so a frame that skips `set_escalation_candidates` doesn't replay the
    /// previous frame's list.
    staged_escalation_count: u32,
}

/// The four Vulkan objects that make up one compute stage. Plain data: it has
/// no `Drop`, because `GpuFrameProcessor::drop` interleaves stage and buffer
/// teardown in a specific order and calls [`ComputeStage::destroy`] in place.
pub(super) struct ComputeStage {
    pub(super) module: vk::ShaderModule,
    pub(super) dsl: vk::DescriptorSetLayout,
    pub(super) layout: vk::PipelineLayout,
    pub(super) pipeline: vk::Pipeline,
}

impl ComputeStage {
    /// Destroy pipeline, layout, descriptor-set layout, then shader module.
    ///
    /// # Safety
    /// `d` must be the device that created these objects and none may still
    /// be in use by the GPU.
    pub(super) unsafe fn destroy(&self, d: &ash::Device) {
        d.destroy_pipeline(self.pipeline, None);
        d.destroy_pipeline_layout(self.layout, None);
        d.destroy_descriptor_set_layout(self.dsl, None);
        d.destroy_shader_module(self.module, None);
    }
}

/// A HOST_VISIBLE buffer, its memory and its persistent CPU mapping. Plain
/// data with no `Drop`: `GpuFrameProcessor::drop` interleaves buffer and
/// stage teardown in a fixed order and calls [`HostBuffer::destroy`] in place.
///
/// `ptr` is null for buffers that are not mapped at drop time (mapped only
/// transiently in setup, or never); `destroy` then skips the unmap.
pub(super) struct HostBuffer<T> {
    pub(super) buffer: vk::Buffer,
    pub(super) memory: vk::DeviceMemory,
    pub(super) ptr: *mut T,
}

impl<T> HostBuffer<T> {
    /// Unmap (only if `ptr` is non-null), destroy the buffer, free the memory.
    ///
    /// # Safety
    /// `d` must be the device that created these objects, the memory must be
    /// mapped iff `ptr` is non-null, and the GPU must be done with the buffer.
    pub(super) unsafe fn destroy(&self, d: &ash::Device) {
        if !self.ptr.is_null() {
            d.unmap_memory(self.memory);
        }
        d.destroy_buffer(self.buffer, None);
        d.free_memory(self.memory, None);
    }
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
    /// Set when `image` was filled from an imported dma-buf `VkBuffer`
    /// (see `import_dmabuf`). The copy into `image` is recorded in the frame's
    /// command buffer by `record_acquire_current`; `None` for images the host
    /// wrote directly (`import_pixels`) and for owned snapshots.
    ingest: Option<BufferIngest>,
}

/// The imported dma-buf, as a `VkBuffer` (buffers have no row pitch), plus the
/// row length the copy into the pipeline image must use.
struct BufferIngest {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// `bufferRowLength` for `vkCmdCopyBufferToImage`, in texels (`stride / 4`).
    row_length_texels: u32,
}

/// How the most recent `process_frame` obtained its pixels. Exposed so tests
/// can prove which path ran instead of trusting that "it worked".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestTier {
    /// dma-buf imported as a `VkBuffer`, copied to the pipeline image on the GPU.
    DmabufBuffer,
    /// dma-buf could not be imported; pixels were read with mmap +
    /// `DMA_BUF_IOCTL_SYNC` and uploaded like `process_frame_from_pixels`.
    CpuMmap,
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
        // Test builds: GPU use is mutually exclusive with the VA-API encoder
        // tests, per call (never for the processor's lifetime: tests move
        // processors between threads and tasks).
        #[cfg(test)]
        let _gpu = crate::gpu_test_lock::lock();
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

    /// Which ingest path the most recent `process_frame` took.
    pub fn last_ingest_tier(&self) -> Option<IngestTier> {
        self.last_ingest
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
            std::ptr::copy_nonoverlapping(candidates.as_ptr(), self.cdf53_escalation_list.ptr, n);
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
    /// The cushion is the longer of what is already running and `force_frames`.
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
        // Never shorten a cushion already running: a session reset's 20 frames
        // must survive the 1- or 2-frame repaints the mode handoff and the
        // stuck-entry retire ask for right after it.
        self.force_all_dirty_remaining = self.force_all_dirty_remaining.max(force_frames);
    }
}

// ---------------------------------------------------------------------------
// Drop
// ---------------------------------------------------------------------------

impl Drop for GpuFrameProcessor {
    fn drop(&mut self) {
        #[cfg(test)]
        let _gpu = crate::gpu_test_lock::lock();
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
            self.sad_buf.destroy(&self.device);

            // Analysis buffer
            self.analysis_buf.destroy(&self.device);

            // PalRLE compact list buffer
            self.palrle_compact_list.destroy(&self.device);

            // PalRLE compact count buffer
            self.palrle_compact_count.destroy(&self.device);

            // PalRLE indirect args buffer (no CPU mapping to unmap)
            self.palrle_indirect_args_buf.destroy(&self.device);

            // palette_fold buffers
            self.per_tile_frame_palette_id.destroy(&self.device);

            // hash_table buffer (no persistent CPU mapping)
            self.hash_table.destroy(&self.device);

            self.frame_palette_count.destroy(&self.device);

            self.frame_palette_set.destroy(&self.device);

            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);

            // NV12 pipeline
            self.nv12.destroy(&self.device);

            // SAD pipeline
            self.sad.destroy(&self.device);

            // Analysis pipeline
            self.analysis.destroy(&self.device);

            // PalRLE compact pipeline
            self.palrle_compact.destroy(&self.device);

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
            self.cdf53_compact.destroy(&self.device);

            // Cdf53 indirect-args buffer (Stage 4b)
            self.cdf53_dispatch_args.destroy(&self.device);

            // Cdf53 indirect-args pipeline (Stage 4b)
            self.cdf53_indirect_args.destroy(&self.device);

            // Cdf53 coefficient buffer (Stage 4c)
            self.device.unmap_memory(self.cdf53_coefficients_memory);
            self.device
                .destroy_buffer(self.cdf53_coefficients_buffer, None);
            self.device
                .free_memory(self.cdf53_coefficients_memory, None);

            // M3.3c escalation buffers
            self.cdf53_escalation_list.destroy(&self.device);
            self.cdf53_fallback_list.destroy(&self.device);
            self.cdf53_fallback_coefficients.destroy(&self.device);

            self.device
                .unmap_memory(self.cdf53_escalation_coefficients_memory);
            self.device
                .destroy_buffer(self.cdf53_escalation_coefficients_buffer, None);
            self.device
                .free_memory(self.cdf53_escalation_coefficients_memory, None);

            // Cdf53 forward L1 pipeline (Stage 4c-L1)
            self.cdf53_forward_l1.destroy(&self.device);

            // Cdf53 forward L2 pipeline (Stage 4c-L2)
            self.cdf53_forward_l2.destroy(&self.device);

            // Cdf53 forward L3 pipeline (Stage 4c-L3)
            self.cdf53_forward_l3.destroy(&self.device);

            // PalRLE indirect-args pipeline
            self.palrle_indirect_args.destroy(&self.device);

            // palette_fold pipeline
            self.palette_fold.destroy(&self.device);

            // palette_subset_fold_init pipeline
            self.palette_subset_fold_init.destroy(&self.device);

            // palette_subset_fold pipeline
            self.palette_subset_fold.destroy(&self.device);

            // folded_into buffer
            self.folded_into.destroy(&self.device);

            // pal_rle_index pipeline (Stage 3)
            self.pal_rle_index.destroy(&self.device);

            // index_buffer (Stage 3 output)
            self.index.destroy(&self.device);

            // Persistent per-frame command buffer + fence: device_wait_idle
            // above means nothing is in flight. Fence before the device is
            // destroyed; command buffer before its pool.
            self.device.destroy_fence(self.frame_fence, None);
            self.device
                .free_command_buffers(self.command_pool, &[self.frame_cmd]);
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

mod frame;
mod setup;

#[cfg(test)]
#[path = "../../../tests/support/dmabuf_fixture.rs"]
pub(crate) mod dmabuf_fixture;

#[cfg(test)]
mod ingest_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod readback_bench;

#[cfg(test)]
mod fallback_tests;
