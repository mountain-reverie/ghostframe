//! Per-frame compute dispatch path for [`GpuFrameProcessor`].
//!
//! Owns the orchestrator methods (`process_frame_inner`,
//! `process_frame_with_imported`, `run_first_frame_passes`) that record the
//! 9-stage compute pipeline into a command buffer, the per-frame Vulkan
//! resource lifecycle (`ensure_nv12_buffer`, `import_dmabuf`,
//! `destroy_prev_frame`, `allocate_owned_image`), and the RAII guards used
//! to keep per-frame descriptor-set / command-buffer / fence allocations
//! safely scoped.

use ash::vk;
use std::io;

use super::super::pipeline_builder::{alloc_host_buffer_mapped, find_memory_type};
use super::*;

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

        let mem_props = self
            .instance
            .get_physical_device_memory_properties(self.physical_device);
        let (buffer, memory, ptr) = alloc_host_buffer_mapped::<u8>(
            &self.device,
            &mem_props,
            total,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            "NV12 buffer",
        )?;

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
    pub(super) unsafe fn process_frame_inner(
        &mut self,
        fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<FrameAnalysis, Box<dyn std::error::Error>> {
        let geom = FrameGeometry::from_dims(width, height);

        if geom.tile_count() > self.max_tiles {
            return Err(format!(
                "tile_count {} exceeds max_tiles {}",
                geom.tile_count(),
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
        let result = self.process_frame_with_imported(&current, geom);
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
        geom: FrameGeometry,
    ) -> Result<FrameAnalysis, Box<dyn std::error::Error>> {
        let FrameGeometry { width, height, cols, rows } = geom;
        let tile_count = geom.tile_count();
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

            // Session-reset cushion: while the counter is > 0, run all stages
            // but DO NOT snapshot the current frame as the new baseline.
            // prev_image stays None so the next frame hits this branch again
            // and re-emits all tiles dirty. Mirrors the CPU path's
            // `force_dirty_frames` no-commit semantics — datagrams dropped
            // during QUIC slow-start re-surface as dirty until a real
            // commit-frame snapshot lands.
            let no_commit = self.force_all_dirty_remaining > 0;
            let snapshot = if no_commit {
                None
            } else {
                // Allocate the owned snapshot image once. It persists for the
                // lifetime of the processor (until resolution changes).
                Some(self.allocate_owned_image(width, height)?)
            };

            // Run NV12 conversion + tile_analysis + snapshot copy in one cmd
            // buffer. The snapshot is what we will compare future frames
            // against.
            self.run_first_frame_passes(current, snapshot.as_ref(), geom, nv12_layout)?;

            if let Some(mut snap) = snapshot {
                snap.layout = vk::ImageLayout::GENERAL;
                self.prev_image = Some(snap);
            }
            if no_commit {
                self.force_all_dirty_remaining -= 1;
            }

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
        // Note: SAD descriptor set is allocated inside run_sad_stage (called below).
        // Note: NV12 descriptor set is allocated inside run_nv12_stage (called below).
        // Note: analysis descriptor set is allocated inside run_analysis_stage (called below).
        // Note: palrle_compact descriptor set is allocated inside run_palrle_compact_stage (called below).
        // Note: palrle_indirect_args descriptor set is allocated inside run_palrle_indirect_args_stage (called below).

        // Note: palette_fold descriptor set is allocated inside run_palette_fold_stage (called below).
        // Note: palette_subset_fold_init descriptor set is allocated inside run_palette_subset_fold_init_stage (called below).
        // Note: palette_subset_fold descriptor set is allocated inside run_palette_subset_fold_stage (called below).

        // Note: pal_rle_index descriptor set is allocated inside run_pal_rle_index_stage (called below).

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
        let sad_push: [u32; 3] = [width, height, cols];
        let sad_ds_guard = self.run_sad_stage(
            cmd,
            current.view,
            prev_view,
            &sad_push,
            (cols, rows, 1),
        )?;

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
        let nv12_push: [u32; 5] = [width, height, nv12_y_stride, nv12_uv_offset, nv12_uv_stride];
        let nv12_ds_guard = self.run_nv12_stage(
            cmd,
            current.view,
            nv12_buffer,
            &nv12_push,
            (width.div_ceil(2), height.div_ceil(2), 1),
        )?;

        // 4b. Analysis dispatch. No barrier needed vs SAD/NV12 — all read
        // current_frame read-only and write to disjoint buffers; the final
        // HOST-readback barrier covers all three output buffers.
        let analysis_push: [u32; 3] = [width, height, cols];
        let analysis_ds_guard = self.run_analysis_stage(
            cmd,
            current.view,
            &analysis_push,
            (cols, rows, 1),
        )?;

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

        // Stage 1.5a: palrle_compact dispatch.
        // tile_idx = gl_WorkGroupID.x; bound check `tile_idx < cols * rows` is in the shader.
        let palrle_compact_push: [u32; 3] = [cols, rows, SAD_THRESHOLD];
        let palrle_compact_ds_guard = self.run_palrle_compact_stage(
            cmd,
            &palrle_compact_push,
            (cols * rows, 1, 1),
        )?;

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
        let palrle_indirect_args_ds_guard = self.run_palrle_indirect_args_stage(cmd)?;

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

        // Stage 4a: cdf53_compact dispatch.
        // Zero the atomic count, then dispatch one workgroup per tile.
        // Reads sad_buffer + analysis_buffer (both already visible via the
        // buf_barrier_inputs above); writes cdf53_compact_list + count.
        self.device
            .cmd_fill_buffer(cmd, self.cdf53_compact_count_buffer, 0, 4, 0);
        let buf_barrier_cdf53_fill = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_compact_count_buffer)
            .offset(0)
            .size(4);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_cdf53_fill),
            &[],
        );
        let cdf53_compact_push: [u32; 4] =
            [cols, rows, SAD_THRESHOLD, UNIQUE_COLORS_UNKNOWN_SENTINEL];
        let cdf53_compact_ds_guard = self.run_cdf53_compact_stage(
            cmd,
            &cdf53_compact_push,
            (cols * rows, 1, 1),
        )?;
        // Barrier between Stage 4a and 4b: shader write → shader read on count.
        let buf_barrier_cdf53_count = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_compact_count_buffer)
            .offset(0)
            .size(4);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_cdf53_count),
            &[],
        );

        // Stage 4b: cdf53_indirect_args writer.
        let cdf53_indirect_args_ds_guard = self.run_cdf53_indirect_args_stage(cmd)?;

        // Barrier: dispatch_args SHADER_WRITE → INDIRECT_COMMAND_READ | SHADER_READ
        // for Stage 4c (cdf53_forward, Task 7) via vkCmdDispatchIndirect.
        let buf_barrier_cdf53_args = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(
                vk::AccessFlags::INDIRECT_COMMAND_READ | vk::AccessFlags::SHADER_READ,
            )
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_dispatch_args_buffer)
            .offset(0)
            .size(12);
        // Barrier: compact_list + compact_count → HOST_READ (CPU readback after fence).
        let buf_barrier_cdf53_list_host = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_compact_list_buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        let buf_barrier_cdf53_count_host = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::SHADER_READ)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_compact_count_buffer)
            .offset(0)
            .size(4);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::DRAW_INDIRECT
                | vk::PipelineStageFlags::COMPUTE_SHADER
                | vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            &[
                buf_barrier_cdf53_args,
                buf_barrier_cdf53_list_host,
                buf_barrier_cdf53_count_host,
            ],
            &[],
        );

        // ============================================================
        // Stage 4c: cdf53_forward L1 → L2 → L3 (per-tile wavelet transform)
        // ============================================================

        // Stage 4c-L1: 32×32 BGRA tile → 4 × 16×16 subbands.
        let cdf53_forward_l1_ds_guard = self.run_cdf53_forward_l1_stage(
            cmd,
            current.view,
            cols,
            rows,
        )?;

        // Barrier L1 → L2: coefficient writes from L1 must be visible to L2.
        let buf_barrier_l1_to_l2 = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_coefficients_buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_l1_to_l2),
            &[],
        );

        // Stage 4c-L2: 16×16 LL1 → 4 × 8×8 subbands.
        let cdf53_forward_l2_ds_guard = self.run_cdf53_forward_l2_stage(cmd, cols, rows)?;

        // Barrier L2 → L3: coefficient writes from L2 must be visible to L3.
        let buf_barrier_l2_to_l3 = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_coefficients_buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_l2_to_l3),
            &[],
        );

        // Stage 4c-L3: 8×8 LL2 → 4 × 4×4 subbands. Final wavelet coefficients.
        let cdf53_forward_l3_ds_guard = self.run_cdf53_forward_l3_stage(cmd, cols, rows)?;

        // Barrier L3 → HOST_READ: Phase B reads coefficient buffer after fence.
        let buf_barrier_l3_host = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_coefficients_buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_l3_host),
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

        // Stage 2a: palette fold (indirect dispatch via helper).
        let palette_fold_ds_guard = self.run_palette_fold_stage(cmd)?;

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
        let palette_subset_fold_init_ds_guard =
            self.run_palette_subset_fold_init_stage(cmd, (1, 1, 1))?;

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
        let palette_subset_fold_ds_guard = self.run_palette_subset_fold_stage(cmd)?;

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
        let pal_rle_index_push: [u32; 1] = [cols];
        let pal_rle_index_ds_guard = self.run_pal_rle_index_stage(
            cmd,
            current.view,
            &pal_rle_index_push,
        )?;

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
        //    sad_ds_guard, nv12_ds_guard, analysis_ds_guard,
        //    palrle_compact_ds_guard, palrle_indirect_args_ds_guard,
        //    cdf53_compact_ds_guard, cdf53_indirect_args_ds_guard,
        //    cdf53_forward_l1/l2/l3_ds_guard,
        //    palette_fold_ds_guard, palette_subset_fold_init_ds_guard,
        //    palette_subset_fold_ds_guard, and pal_rle_index_ds_guard are
        //    dropped explicitly before step 9 to
        //    release the immutable borrows on self (all are returned from helper
        //    methods whose lifetimes are tied to &self) before the mutable
        //    self.prev_image borrow.
        //    GPU-safety: these drops must NOT move above the `wait_for_fences` at
        //    step 6 — releasing descriptor sets while the GPU is still using them
        //    is undefined behaviour (use-after-free in the descriptor pool).
        let _ = (
            &nv12_ds_guard,
            &analysis_ds_guard,
            &palrle_compact_ds_guard,
            &palrle_indirect_args_ds_guard,
            &cdf53_compact_ds_guard,
            &cdf53_indirect_args_ds_guard,
            &cdf53_forward_l1_ds_guard,
            &cdf53_forward_l2_ds_guard,
            &cdf53_forward_l3_ds_guard,
            &palette_fold_ds_guard,
            &palette_subset_fold_init_ds_guard,
            &palette_subset_fold_ds_guard,
            &pal_rle_index_ds_guard,
            &cmd_guard,
            &fence_guard,
        );
        drop(sad_ds_guard);
        drop(nv12_ds_guard);
        drop(analysis_ds_guard);
        drop(palrle_compact_ds_guard);
        drop(palrle_indirect_args_ds_guard);
        drop(cdf53_compact_ds_guard);
        drop(cdf53_indirect_args_ds_guard);
        drop(cdf53_forward_l1_ds_guard);
        drop(cdf53_forward_l2_ds_guard);
        drop(cdf53_forward_l3_ds_guard);
        drop(palette_fold_ds_guard);
        drop(palette_subset_fold_init_ds_guard);
        drop(palette_subset_fold_ds_guard);
        drop(pal_rle_index_ds_guard);

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
    /// When `snapshot` is `Some`, the `PrevFrame` is always freshly allocated
    /// by the caller (`allocate_owned_image`), so its starting layout is
    /// `vk::ImageLayout::UNDEFINED` — we hardcode that transition.
    /// When `snapshot` is `None` the snapshot-copy block is skipped entirely.
    unsafe fn run_first_frame_passes(
        &self,
        current: &PrevFrame,
        snapshot: Option<&PrevFrame>,
        geom: FrameGeometry,
        nv12: Nv12OutputLayout,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let FrameGeometry { width, height, cols, rows } = geom;
        let Nv12OutputLayout {
            buffer: nv12_buffer,
            y_stride: nv12_y_stride,
            uv_offset: nv12_uv_offset,
            uv_stride: nv12_uv_stride,
        } = nv12;
        // Note: NV12 descriptor set is allocated inside run_nv12_stage (called below).
        // Note: analysis descriptor set is allocated inside run_analysis_stage (called below).
        // Note: palrle_compact descriptor set is allocated inside run_palrle_compact_stage (called below).
        // Note: palrle_indirect_args descriptor set is allocated inside run_palrle_indirect_args_stage (called below).

        // Allocate descriptor sets for Stages 2b-3 (all RAII-guarded).
        // Note: palette_fold descriptor set is allocated inside run_palette_fold_stage (called below).
        // Note: palette_subset_fold_init descriptor set is allocated inside run_palette_subset_fold_init_stage (called below).
        // Note: palette_subset_fold descriptor set is allocated inside run_palette_subset_fold_stage (called below).
        // Note: pal_rle_index descriptor set is allocated inside run_pal_rle_index_stage (called below).

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
        let nv12_push: [u32; 5] = [width, height, nv12_y_stride, nv12_uv_offset, nv12_uv_stride];
        let nv12_ds_guard = self.run_nv12_stage(
            cmd,
            current.view,
            nv12_buffer,
            &nv12_push,
            (width.div_ceil(2), height.div_ceil(2), 1),
        )?;

        // Analysis dispatch. No barrier needed vs NV12 — both read `current`
        // read-only (read-after-read is hazard-free) and write to disjoint
        // buffers (`nv12_buffer` vs `self.analysis_buffer`). The
        // COMPUTE→HOST buffer barrier below covers both writes. This mirrors
        // the subsequent-frames branch comment at the SAD/NV12/Analysis
        // dispatch trio.
        let analysis_push: [u32; 3] = [width, height, cols];
        let analysis_ds_guard = self.run_analysis_stage(
            cmd,
            current.view,
            &analysis_push,
            (cols, rows, 1),
        )?;

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

        // Stage 1.5a: palrle_compact dispatch.
        // tile_idx = gl_WorkGroupID.x; bound check `tile_idx < cols * rows` is in the shader.
        let palrle_compact_push: [u32; 3] = [cols, rows, SAD_THRESHOLD];
        let palrle_compact_ds_guard = self.run_palrle_compact_stage(
            cmd,
            &palrle_compact_push,
            (cols * rows, 1, 1),
        )?;

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
        let palrle_indirect_args_ds_guard = self.run_palrle_indirect_args_stage(cmd)?;

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

        // Stage 4a: cdf53_compact dispatch (first-frame path).
        self.device
            .cmd_fill_buffer(cmd, self.cdf53_compact_count_buffer, 0, 4, 0);
        let buf_barrier_cdf53_fill = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_compact_count_buffer)
            .offset(0)
            .size(4);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_cdf53_fill),
            &[],
        );
        let cdf53_compact_push: [u32; 4] =
            [cols, rows, SAD_THRESHOLD, UNIQUE_COLORS_UNKNOWN_SENTINEL];
        let cdf53_compact_ds_guard = self.run_cdf53_compact_stage(
            cmd,
            &cdf53_compact_push,
            (cols * rows, 1, 1),
        )?;
        // Barrier between Stage 4a and 4b: shader write → shader read on count
        // (first-frame path).
        let buf_barrier_cdf53_count = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_compact_count_buffer)
            .offset(0)
            .size(4);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_cdf53_count),
            &[],
        );

        // Stage 4b: cdf53_indirect_args writer (first-frame path).
        let cdf53_indirect_args_ds_guard = self.run_cdf53_indirect_args_stage(cmd)?;

        // Barrier: dispatch_args SHADER_WRITE → INDIRECT_COMMAND_READ | SHADER_READ
        // for Stage 4c (cdf53_forward, Task 7) via vkCmdDispatchIndirect.
        let buf_barrier_cdf53_args = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(
                vk::AccessFlags::INDIRECT_COMMAND_READ | vk::AccessFlags::SHADER_READ,
            )
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_dispatch_args_buffer)
            .offset(0)
            .size(12);
        // Barrier: compact_list + compact_count → HOST_READ (CPU readback after fence).
        let buf_barrier_cdf53_list_host = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_compact_list_buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        let buf_barrier_cdf53_count_host = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::SHADER_READ)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_compact_count_buffer)
            .offset(0)
            .size(4);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::DRAW_INDIRECT
                | vk::PipelineStageFlags::COMPUTE_SHADER
                | vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            &[
                buf_barrier_cdf53_args,
                buf_barrier_cdf53_list_host,
                buf_barrier_cdf53_count_host,
            ],
            &[],
        );

        // ============================================================
        // Stage 4c: cdf53_forward L1 → L2 → L3 (first-frame path)
        // ============================================================

        // Stage 4c-L1: 32×32 BGRA tile → 4 × 16×16 subbands.
        let cdf53_forward_l1_ds_guard = self.run_cdf53_forward_l1_stage(
            cmd,
            current.view,
            cols,
            rows,
        )?;

        // Barrier L1 → L2
        let buf_barrier_l1_to_l2 = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_coefficients_buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_l1_to_l2),
            &[],
        );

        // Stage 4c-L2: 16×16 LL1 → 4 × 8×8 subbands.
        let cdf53_forward_l2_ds_guard = self.run_cdf53_forward_l2_stage(cmd, cols, rows)?;

        // Barrier L2 → L3
        let buf_barrier_l2_to_l3 = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_coefficients_buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_l2_to_l3),
            &[],
        );

        // Stage 4c-L3: 8×8 LL2 → final LL3/HL3/LH3/HH3 subbands.
        let cdf53_forward_l3_ds_guard = self.run_cdf53_forward_l3_stage(cmd, cols, rows)?;

        // Barrier L3 → HOST_READ: Phase B reads coefficient buffer after fence.
        let buf_barrier_l3_host = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_coefficients_buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_l3_host),
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

        // Stage 2a: palette fold (indirect dispatch via helper).
        let palette_fold_ds_guard = self.run_palette_fold_stage(cmd)?;

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
        let palette_subset_fold_init_ds_guard =
            self.run_palette_subset_fold_init_stage(cmd, (1, 1, 1))?;

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
        let palette_subset_fold_ds_guard = self.run_palette_subset_fold_stage(cmd)?;

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
        let pal_rle_index_push: [u32; 1] = [cols];
        let pal_rle_index_ds_guard = self.run_pal_rle_index_stage(
            cmd,
            current.view,
            &pal_rle_index_push,
        )?;

        // Snapshot copy: current (GENERAL) → snapshot (TRANSFER_DST_OPTIMAL).
        // Inside this `Some` arm the `PrevFrame` is always freshly allocated
        // by `allocate_owned_image`, so its initial layout is UNDEFINED and we
        // hardcode that transition (no prior shader access to wait on).
        //
        // Stage 3 reads current.image (SHADER_READ). The srcAccessMask for
        // current covers all compute reads up to this point.
        //
        // When `snapshot` is `None` (session-reset cushion), the entire block
        // is skipped: `current.image` stays in `GENERAL` (no transition to
        // `TRANSFER_SRC_OPTIMAL` happens), and the subsequent `buf_barrier`
        // targets buffers only — it doesn't depend on `current`'s layout.
        if let Some(snap) = snapshot {
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
                    .image(snap.image)
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
                snap.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &copy_region,
            );

            // Restore snapshot to GENERAL for next frame's SAD pass.
            let post_copy_barrier = [vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(snap.image)
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
        }

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
            &cdf53_compact_ds_guard,
            &cdf53_indirect_args_ds_guard,
            &cdf53_forward_l1_ds_guard,
            &cdf53_forward_l2_ds_guard,
            &cdf53_forward_l3_ds_guard,
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
    pub(super) unsafe fn destroy_prev_frame(&self, frame: PrevFrame) {
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

    /// SAD (Sum of Absolute Differences) compute dispatch.
    ///
    /// Reads the current frame image (binding 0) and the previous-frame
    /// owned-image snapshot (binding 1), writes per-tile SAD scores to
    /// the SAD output buffer (binding 2). Push constants are
    /// `[width, height, cols]` (12 bytes).
    ///
    /// Caller must invoke `cmd_pipeline_barrier` AFTER this call to make
    /// the SAD output buffer visible to downstream stages.
    ///
    /// # Safety
    ///
    /// Caller must ensure: `cmd` is currently recording; the image views
    /// passed are in `vk::ImageLayout::GENERAL` and remain valid for the
    /// duration of the recording; the workgroup count `(cols, rows, 1)`
    /// matches the SAD shader's expected dispatch shape; and the returned
    /// guard is held alive until `wait_for_fences` returns for the command
    /// buffer containing this dispatch — dropping it earlier returns the
    /// descriptor set to the pool while the GPU is still using it.
    unsafe fn run_sad_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        current_view: vk::ImageView,
        prev_view: vk::ImageView,
        push_constants: &[u32],
        workgroups: (u32, u32, u32),
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        // Allocate one descriptor set from the pool. The returned guard frees
        // it on drop, ensuring the bounded pool doesn't leak on early-exit.
        let sad_set_layouts = [self.descriptor_set_layout];
        let sad_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&sad_set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&sad_ds_alloc)?,
        };
        let sad_ds = guard.sets[0];

        // Write the three SAD descriptors:
        //   binding 0 — current frame image (STORAGE_IMAGE, read)
        //   binding 1 — previous frame image (STORAGE_IMAGE, read)
        //   binding 2 — SAD output buffer    (STORAGE_BUFFER, write)
        let current_image_info = [vk::DescriptorImageInfo::default()
            .image_view(current_view)
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

        // Bind + push + dispatch.
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
        let push_bytes = std::slice::from_raw_parts(
            push_constants.as_ptr() as *const u8,
            std::mem::size_of_val(push_constants),
        );
        self.device.cmd_push_constants(
            cmd,
            self.pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            push_bytes,
        );
        self.device
            .cmd_dispatch(cmd, workgroups.0, workgroups.1, workgroups.2);

        Ok(guard)
    }

    /// NV12 (BGRA → NV12) compute dispatch.
    ///
    /// Reads the current frame image (binding 0, STORAGE_IMAGE), writes
    /// to the NV12 HOST_VISIBLE output buffer (binding 1, STORAGE_BUFFER).
    /// Push constants are `[width, height, y_stride, uv_offset, uv_stride]`
    /// (20 bytes).
    ///
    /// Caller must invoke `cmd_pipeline_barrier` AFTER this call to make
    /// the NV12 buffer's HOST_VISIBLE memory writes visible to the host
    /// (HOST_READ access for the readback).
    ///
    /// # Safety
    ///
    /// Caller must ensure: `cmd` is currently recording; `current_view` is
    /// in `vk::ImageLayout::GENERAL`; `nv12_buffer` is bound to memory and
    /// sized for `width * height + uv_offset + (width * height / 2)` bytes;
    /// the workgroup count matches the shader's half-resolution NV12
    /// dispatch shape; and the returned guard is held alive until
    /// `wait_for_fences` returns for the command buffer (dropping it
    /// earlier returns the descriptor set to the pool while the GPU is
    /// still using it).
    unsafe fn run_nv12_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        current_view: vk::ImageView,
        nv12_buffer: vk::Buffer,
        push_constants: &[u32],
        workgroups: (u32, u32, u32),
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        // Allocate one descriptor set from the pool. The returned guard frees
        // it on drop, ensuring the bounded pool doesn't leak on early-exit.
        let nv12_set_layouts = [self.nv12_descriptor_set_layout];
        let nv12_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&nv12_set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&nv12_ds_alloc)?,
        };
        let nv12_ds = guard.sets[0];

        // Write the two NV12 descriptors:
        //   binding 0 — current frame image  (STORAGE_IMAGE,  read)
        //   binding 1 — NV12 output buffer   (STORAGE_BUFFER, write)
        let nv12_image_info = [vk::DescriptorImageInfo::default()
            .image_view(current_view)
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

        // Bind + push + dispatch.
        // NV12 shader: workgroup = 2x2 pixels, dispatch = (width/2, height/2, 1)
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
        let push_bytes = std::slice::from_raw_parts(
            push_constants.as_ptr() as *const u8,
            std::mem::size_of_val(push_constants),
        );
        self.device.cmd_push_constants(
            cmd,
            self.nv12_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            push_bytes,
        );
        self.device
            .cmd_dispatch(cmd, workgroups.0, workgroups.1, workgroups.2);

        Ok(guard)
    }

    /// Per-tile color analysis compute dispatch.
    ///
    /// Reads the current frame image (binding 0, STORAGE_IMAGE), writes
    /// per-tile `TileAnalysis` entries to the analysis buffer (binding 1,
    /// STORAGE_BUFFER). Push constants are `[width, height, cols]` (12 bytes).
    ///
    /// Caller must invoke `cmd_pipeline_barrier` AFTER this call to make
    /// the analysis buffer visible to downstream stages (palrle_compact /
    /// palette_fold) or to the host (HOST_READ on the persistent mapped
    /// pointer).
    ///
    /// # Safety
    ///
    /// Caller must ensure: `cmd` is recording; `current_view` is in GENERAL
    /// layout; workgroup count is `(cols, rows, 1)` (one workgroup per
    /// tile); and the returned guard is held alive until `wait_for_fences`
    /// returns for the command buffer (dropping it earlier returns the
    /// descriptor set to the pool while the GPU is still using it).
    unsafe fn run_analysis_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        current_view: vk::ImageView,
        push_constants: &[u32],
        workgroups: (u32, u32, u32),
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        // Allocate one descriptor set from the pool. The returned guard frees
        // it on drop, ensuring the bounded pool doesn't leak on early-exit.
        let analysis_set_layouts = [self.analysis_descriptor_set_layout];
        let analysis_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&analysis_set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&analysis_ds_alloc)?,
        };
        let analysis_ds = guard.sets[0];

        // Write the two analysis descriptors:
        //   binding 0 — current frame image   (STORAGE_IMAGE,  read)
        //   binding 1 — analysis output buffer (STORAGE_BUFFER, write)
        let analysis_image_info = [vk::DescriptorImageInfo::default()
            .image_view(current_view)
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

        // Bind + push + dispatch.
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
        let push_bytes = std::slice::from_raw_parts(
            push_constants.as_ptr() as *const u8,
            std::mem::size_of_val(push_constants),
        );
        self.device.cmd_push_constants(
            cmd,
            self.analysis_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            push_bytes,
        );
        self.device
            .cmd_dispatch(cmd, workgroups.0, workgroups.1, workgroups.2);

        Ok(guard)
    }

    /// PalRLE compact compute dispatch.
    ///
    /// Reads SAD output (binding 0, STORAGE_BUFFER) and tile analysis
    /// (binding 1, STORAGE_BUFFER); writes the compact list of
    /// PalRLE-feasible tile indices (binding 2) and the compact count
    /// (binding 3). Push constants are `[cols, rows, dirty_threshold]`.
    ///
    /// Caller must invoke `cmd_pipeline_barrier` AFTER this call so that
    /// the compact list and count buffers are visible to the downstream
    /// `palrle_indirect_args` and `palette_fold` stages.
    ///
    /// # Safety
    ///
    /// Caller must ensure: `cmd` is recording; the SAD and analysis buffers
    /// are populated and made visible via a preceding barrier (or were
    /// reset/zeroed appropriately); workgroup count `(cols * rows, 1, 1)`
    /// matches the shader's per-tile dispatch implementation; and the returned
    /// guard is held alive until `wait_for_fences` returns for the command
    /// buffer (dropping it earlier returns the descriptor set to the pool
    /// while the GPU is still using it).
    unsafe fn run_palrle_compact_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        push_constants: &[u32],
        workgroups: (u32, u32, u32),
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        // Allocate one descriptor set from the pool. The returned guard frees
        // it on drop, ensuring the bounded pool doesn't leak on early-exit.
        let palrle_compact_set_layouts = [self.palrle_compact_descriptor_set_layout];
        let palrle_compact_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&palrle_compact_set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&palrle_compact_ds_alloc)?,
        };
        let palrle_compact_ds = guard.sets[0];

        // Write the four palrle_compact descriptors (all STORAGE_BUFFER):
        //   binding 0 — sad_buffer               (read)
        //   binding 1 — analysis_buffer           (read)
        //   binding 2 — palrle_compact_list_buffer (write)
        //   binding 3 — palrle_compact_count_buffer (atomic write)
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

        // Bind + push + dispatch.
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
        let push_bytes = std::slice::from_raw_parts(
            push_constants.as_ptr() as *const u8,
            std::mem::size_of_val(push_constants),
        );
        self.device.cmd_push_constants(
            cmd,
            self.palrle_compact_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            push_bytes,
        );
        self.device
            .cmd_dispatch(cmd, workgroups.0, workgroups.1, workgroups.2);

        Ok(guard)
    }

    /// Cdf53 compact compute dispatch (Stage 4a).
    ///
    /// Reads sad_buffer + analysis_buffer; writes cdf53_compact_list_buffer +
    /// cdf53_compact_count_buffer. Predicate: dirty (SAD > threshold) AND
    /// unique_colors > 16 AND unique_colors != unknown_sentinel.
    ///
    /// Mirrors run_palrle_compact_stage but uses the cdf53_compact pipeline.
    unsafe fn run_cdf53_compact_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        push_constants: &[u32],
        workgroups: (u32, u32, u32),
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        let cdf53_compact_set_layouts = [self.cdf53_compact_descriptor_set_layout];
        let cdf53_compact_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&cdf53_compact_set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&cdf53_compact_ds_alloc)?,
        };
        let cdf53_compact_ds = guard.sets[0];

        // Write the four cdf53_compact descriptors (all STORAGE_BUFFER):
        //   binding 0 — sad_buffer                    (read)
        //   binding 1 — analysis_buffer               (read)
        //   binding 2 — cdf53_compact_list_buffer     (write)
        //   binding 3 — cdf53_compact_count_buffer    (atomic write)
        let cdf53_sad_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.sad_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let cdf53_analysis_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.analysis_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let cdf53_list_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.cdf53_compact_list_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let cdf53_count_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.cdf53_compact_count_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let cdf53_compact_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(cdf53_compact_ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&cdf53_sad_info),
            vk::WriteDescriptorSet::default()
                .dst_set(cdf53_compact_ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&cdf53_analysis_info),
            vk::WriteDescriptorSet::default()
                .dst_set(cdf53_compact_ds)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&cdf53_list_info),
            vk::WriteDescriptorSet::default()
                .dst_set(cdf53_compact_ds)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&cdf53_count_info),
        ];
        self.device.update_descriptor_sets(&cdf53_compact_writes, &[]);

        // Bind + push + dispatch.
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.cdf53_compact_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.cdf53_compact_pipeline_layout,
            0,
            &[cdf53_compact_ds],
            &[],
        );
        let push_bytes = std::slice::from_raw_parts(
            push_constants.as_ptr() as *const u8,
            std::mem::size_of_val(push_constants),
        );
        self.device.cmd_push_constants(
            cmd,
            self.cdf53_compact_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            push_bytes,
        );
        self.device
            .cmd_dispatch(cmd, workgroups.0, workgroups.1, workgroups.2);

        Ok(guard)
    }

    /// PalRLE indirect-args compute dispatch (builds the dispatch params
    /// for the pal_rle_index stage from the compact count).
    ///
    /// Reads the compact count (binding 0, STORAGE_BUFFER); writes
    /// indirect dispatch args (binding 1, STORAGE_BUFFER). No push
    /// constants. Workgroups `(1, 1, 1)` (single-workgroup transform).
    ///
    /// Caller must invoke `cmd_pipeline_barrier` AFTER this call so the
    /// indirect-args buffer is visible to the subsequent
    /// `cmd_dispatch_indirect` consumer (`pal_rle_index`).
    ///
    /// # Safety
    ///
    /// Caller must ensure: `cmd` is recording; the compact count buffer
    /// has been populated by a preceding palrle_compact dispatch and a
    /// barrier; the indirect args buffer is bound to memory; and the
    /// returned guard is held alive until `wait_for_fences` returns for
    /// the command buffer (dropping it earlier returns the descriptor set
    /// to the pool while the GPU is still using it).
    unsafe fn run_palrle_indirect_args_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        // Allocate one descriptor set from the pool. The returned guard frees
        // it on drop, ensuring the bounded pool doesn't leak on early-exit.
        let palrle_indirect_args_set_layouts = [self.palrle_indirect_args_descriptor_set_layout];
        let palrle_indirect_args_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&palrle_indirect_args_set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&palrle_indirect_args_ds_alloc)?,
        };
        let palrle_indirect_args_ds = guard.sets[0];

        // Write the two palrle_indirect_args descriptors (both STORAGE_BUFFER):
        //   binding 0 — palrle_compact_count_buffer  (read)
        //   binding 1 — palrle_indirect_args_buffer  (write)
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

        // Bind + dispatch. No push constants for this stage.
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

        Ok(guard)
    }

    /// Stage 4b — cdf53 indirect-args writer.
    ///
    /// Reads `cdf53_compact_count_buffer` (binding 0) and writes the
    /// vkCmdDispatchIndirect args `(count, 1, 1)` to `cdf53_dispatch_args_buffer`
    /// (binding 1). Dispatch is always (1, 1, 1): one thread sets the three u32s.
    ///
    /// Caller must have issued a SHADER_WRITE → SHADER_READ barrier on
    /// `cdf53_compact_count_buffer` before calling this (the post-cdf53_compact
    /// barrier covers this). Caller must issue a SHADER_WRITE →
    /// INDIRECT_COMMAND_READ barrier on `cdf53_dispatch_args_buffer` after
    /// returning.
    ///
    /// # Safety
    ///
    /// Caller must ensure: `cmd` is recording; `cdf53_compact_count_buffer`
    /// has been written and barriered; the returned guard is held alive until
    /// `wait_for_fences` returns.
    unsafe fn run_cdf53_indirect_args_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        let set_layouts = [self.cdf53_indirect_args_descriptor_set_layout];
        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&ds_alloc)?,
        };
        let ds = guard.sets[0];

        // Write the two cdf53_indirect_args descriptors (both STORAGE_BUFFER):
        //   binding 0 — cdf53_compact_count_buffer  (read)
        //   binding 1 — cdf53_dispatch_args_buffer  (write)
        let count_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.cdf53_compact_count_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let args_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.cdf53_dispatch_args_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&count_info),
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&args_info),
        ];
        self.device.update_descriptor_sets(&writes, &[]);

        // Bind + dispatch. No push constants for this stage.
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.cdf53_indirect_args_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.cdf53_indirect_args_pipeline_layout,
            0,
            &[ds],
            &[],
        );
        self.device.cmd_dispatch(cmd, 1, 1, 1);

        Ok(guard)
    }

    /// Stage 2a — palette fold compute dispatch (builds the per-frame
    /// palette set by merging duplicate tile palettes).
    ///
    /// Six STORAGE_BUFFER bindings: tile analysis (0 r/o), compact list
    /// (1 r/o), frame_palette_set (2 r/w), frame_palette_count (3 r/w),
    /// hash_table scratch (4 r/w), per_tile_frame_palette_id (5 r/w).
    /// No push constants.
    ///
    /// Uses `cmd_dispatch_indirect` reading from
    /// `self.palrle_indirect_args_buffer` at offset 0 (the workgroup
    /// count is determined at runtime by the palrle_indirect_args stage,
    /// not at record time — there is no fixed `(x, y, z)` tuple).
    ///
    /// Caller must invoke `cmd_pipeline_barrier` AFTER this call so the
    /// subsequent palette_subset_fold stages and the host-readback can
    /// see the writes to frame_palette_set / frame_palette_count /
    /// per_tile_frame_palette_id.
    ///
    /// # Safety
    ///
    /// Caller must ensure: `cmd` is recording; preceding stages have
    /// populated `analysis_buffer` and `palrle_compact_list_buffer` (with
    /// barriers); `hash_table_buffer` has been cleared earlier in the
    /// command buffer (`vkCmdFillBuffer`); `palrle_indirect_args_buffer`
    /// has been populated by `run_palrle_indirect_args_stage` and barriered
    /// for `INDIRECT_COMMAND_READ`; and the returned guard is held
    /// alive until `wait_for_fences` returns for the command buffer
    /// (dropping it earlier returns the descriptor set to the pool while
    /// the GPU is still using it).
    unsafe fn run_palette_fold_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        // Allocate one descriptor set from the pool. The returned guard frees
        // it on drop, ensuring the bounded pool doesn't leak on early-exit.
        let palette_fold_set_layouts = [self.palette_fold_descriptor_set_layout];
        let palette_fold_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&palette_fold_set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&palette_fold_ds_alloc)?,
        };
        let palette_fold_ds = guard.sets[0];

        // Write the six palette_fold descriptors (all STORAGE_BUFFER):
        //   binding 0 — analysis_buffer              (read)
        //   binding 1 — palrle_compact_list_buffer   (read)
        //   binding 2 — frame_palette_set_buffer     (read/write)
        //   binding 3 — frame_palette_count_buffer   (read/write)
        //   binding 4 — hash_table_buffer            (read/write scratch)
        //   binding 5 — per_tile_frame_palette_id_buffer (write)
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
                .dst_set(palette_fold_ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_analysis_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_compact_list_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_ds)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_set_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_ds)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_count_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_ds)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_hash_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_fold_ds)
                .dst_binding(5)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&palette_fold_per_tile_info),
        ];
        self.device.update_descriptor_sets(&palette_fold_writes, &[]);

        // Bind + indirect dispatch. No push constants for this stage.
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
            &[palette_fold_ds],
            &[],
        );
        // palrle_indirect_args_buffer already barriered for INDIRECT_COMMAND_READ
        // by the Stage 1.5b barrier in the orchestrator.
        self.device
            .cmd_dispatch_indirect(cmd, self.palrle_indirect_args_buffer, 0);

        Ok(guard)
    }

    /// Stage 2b init — zero out the `folded_into` array to the default-self
    /// sentinel before the subset-fold scan.
    ///
    /// Single STORAGE_BUFFER binding: `folded_into_buffer` at binding 0.
    /// No push constants.
    ///
    /// Caller must invoke `cmd_pipeline_barrier` AFTER this call so the
    /// folded_into writes are visible to the subsequent
    /// `palette_subset_fold` stage.
    ///
    /// # Safety
    ///
    /// Caller must ensure: `cmd` is recording; `folded_into_buffer` is
    /// bound to memory; the workgroup count provided initializes every
    /// `PALETTE_HASH_SLOTS` entry (consult the shader's `@workgroup_size`
    /// attribute); and the returned guard is held alive until
    /// `wait_for_fences` returns for the command buffer (dropping it
    /// earlier returns the descriptor set to the pool while the GPU is
    /// still using it).
    unsafe fn run_palette_subset_fold_init_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        workgroups: (u32, u32, u32),
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        // Allocate one descriptor set from the pool. The returned guard frees
        // it on drop, ensuring the bounded pool doesn't leak on early-exit.
        let palette_subset_fold_init_set_layouts = [self.palette_subset_fold_init_descriptor_set_layout];
        let palette_subset_fold_init_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&palette_subset_fold_init_set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&palette_subset_fold_init_ds_alloc)?,
        };
        let palette_subset_fold_init_ds = guard.sets[0];

        // Write the single palette_subset_fold_init descriptor (STORAGE_BUFFER):
        //   binding 0 — folded_into_buffer (write)
        let palette_subset_fold_init_folded_into_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.folded_into_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let palette_subset_fold_init_writes = [vk::WriteDescriptorSet::default()
            .dst_set(palette_subset_fold_init_ds)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .buffer_info(&palette_subset_fold_init_folded_into_info)];
        self.device
            .update_descriptor_sets(&palette_subset_fold_init_writes, &[]);

        // Bind + direct dispatch. No push constants for this stage.
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
            &[palette_subset_fold_init_ds],
            &[],
        );
        self.device.cmd_dispatch(cmd, workgroups.0, workgroups.1, workgroups.2);

        Ok(guard)
    }

    /// Stage 2b — palette subset-fold compute dispatch (resolves subset
    /// palettes into their containing supersets).
    ///
    /// Two STORAGE_BUFFER bindings: frame_palette_set (0, r/o),
    /// folded_into (1, r/w). No push constants.
    ///
    /// Caller must invoke `cmd_pipeline_barrier` AFTER this call so the
    /// folded_into writes are visible to the subsequent pal_rle_index
    /// stage and to the host-readback.
    ///
    /// # Safety
    ///
    /// Caller must ensure: `cmd` is recording; `frame_palette_set_buffer`
    /// is populated by preceding palette_fold + barrier;
    /// palette_subset_fold_init has run earlier and a barrier makes its
    /// writes visible; and the returned guard is held alive until
    /// `wait_for_fences` returns for the command buffer.
    unsafe fn run_palette_subset_fold_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        // Allocate one descriptor set from the pool. The returned guard frees
        // it on drop, ensuring the bounded pool doesn't leak on early-exit.
        let palette_subset_fold_set_layouts = [self.palette_subset_fold_descriptor_set_layout];
        let palette_subset_fold_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&palette_subset_fold_set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&palette_subset_fold_ds_alloc)?,
        };
        let palette_subset_fold_ds = guard.sets[0];

        // Write the two palette_subset_fold descriptors (STORAGE_BUFFER):
        //   binding 0 — frame_palette_set_buffer (read-only)
        //   binding 1 — folded_into_buffer (read-write)
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
                .dst_set(palette_subset_fold_ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&subset_fold_set_info),
            vk::WriteDescriptorSet::default()
                .dst_set(palette_subset_fold_ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&subset_fold_folded_into_info),
        ];
        self.device.update_descriptor_sets(&subset_fold_writes, &[]);

        // Bind + direct dispatch. No push constants for this stage.
        // One workgroup per palette slot so each slot evaluates its own row.
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
            &[palette_subset_fold_ds],
            &[],
        );
        self.device.cmd_dispatch(cmd, PALETTE_HASH_SLOTS as u32, 1, 1);

        Ok(guard)
    }

    /// Stage 3 — pal_rle_index compute dispatch (emits the nibble-packed
    /// index buffer for each PalRLE-feasible tile).
    ///
    /// Bindings: current frame (0, STORAGE_IMAGE r/o), compact_list (1,
    /// STORAGE_BUFFER r/o), frame_palette_set (2, r/o),
    /// per_tile_frame_palette_id (3, r/o), folded_into (4, r/o),
    /// index_buffer (5, r/w). Push constants `[cols]` (4 bytes).
    ///
    /// Uses `cmd_dispatch_indirect` against
    /// `self.palrle_indirect_args_buffer` at offset 0 — the workgroup count
    /// was computed at runtime by the preceding palrle_indirect_args stage.
    ///
    /// Caller must invoke `cmd_pipeline_barrier` AFTER this call so the
    /// index_buffer writes are visible to the host-readback.
    ///
    /// # Safety
    ///
    /// Caller must ensure: `cmd` is recording; `current_view` is in
    /// `vk::ImageLayout::GENERAL`; preceding stages (compact, fold,
    /// subset_fold) have populated their outputs with barriers; the
    /// indirect-args buffer holds a valid `(group_x, group_y, group_z)`
    /// triple at offset 0; and the returned guard is held alive until
    /// `wait_for_fences` returns for the command buffer.
    unsafe fn run_pal_rle_index_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        current_view: vk::ImageView,
        push_constants: &[u32],
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        // Allocate one descriptor set from the pool. The returned guard frees
        // it on drop, ensuring the bounded pool doesn't leak on early-exit.
        let pal_rle_index_set_layouts = [self.pal_rle_index_descriptor_set_layout];
        let pal_rle_index_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&pal_rle_index_set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&pal_rle_index_ds_alloc)?,
        };
        let pal_rle_index_ds = guard.sets[0];

        // Write the six pal_rle_index descriptors:
        //   binding 0 — current frame image (STORAGE_IMAGE, read-only)
        //   binding 1 — palrle_compact_list_buffer (STORAGE_BUFFER, read-only)
        //   binding 2 — frame_palette_set_buffer (STORAGE_BUFFER, read-only)
        //   binding 3 — per_tile_frame_palette_id_buffer (STORAGE_BUFFER, read-only)
        //   binding 4 — folded_into_buffer (STORAGE_BUFFER, read-only)
        //   binding 5 — index_buffer (STORAGE_BUFFER, read-write)
        let pal_rle_index_image_info = [vk::DescriptorImageInfo::default()
            .image_view(current_view)
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
                .dst_set(pal_rle_index_ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&pal_rle_index_image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_compact_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_ds)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_fps_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_ds)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_ptfpi_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_ds)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_folded_info),
            vk::WriteDescriptorSet::default()
                .dst_set(pal_rle_index_ds)
                .dst_binding(5)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&pal_rle_index_out_info),
        ];
        self.device.update_descriptor_sets(&pal_rle_index_writes, &[]);

        // Bind + push + indirect dispatch.
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
            &[pal_rle_index_ds],
            &[],
        );
        let push_bytes = std::slice::from_raw_parts(
            push_constants.as_ptr() as *const u8,
            std::mem::size_of_val(push_constants),
        );
        self.device.cmd_push_constants(
            cmd,
            self.pal_rle_index_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            push_bytes,
        );
        // palrle_indirect_args_buffer already barriered for INDIRECT_COMMAND_READ
        // by the preceding palrle_indirect_args stage barrier in the orchestrator.
        self.device
            .cmd_dispatch_indirect(cmd, self.palrle_indirect_args_buffer, 0);

        Ok(guard)
    }

    /// Stage 4c-L1 — CDF 5/3 forward wavelet, level 1.
    ///
    /// Reads the current frame image (STORAGE_IMAGE) and compact list
    /// (STORAGE_BUFFER), writes coefficient buffer (STORAGE_BUFFER).
    /// Dispatches via `cmd_dispatch_indirect` from `cdf53_dispatch_args_buffer`.
    ///
    /// Caller must:
    ///   - Ensure `current_view` is in `GENERAL` layout (same state as for SAD/NV12).
    ///   - Ensure `cdf53_dispatch_args_buffer` is barriered for `INDIRECT_COMMAND_READ`.
    ///   - Add a SHADER_WRITE → SHADER_READ barrier on `cdf53_coefficients_buffer`
    ///     after this call before dispatching L2.
    ///   - Hold the returned guard alive until after `wait_for_fences`.
    unsafe fn run_cdf53_forward_l1_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        current_view: vk::ImageView,
        cols: u32,
        rows: u32,
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        let set_layouts = [self.cdf53_forward_l1_descriptor_set_layout];
        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&ds_alloc)?,
        };
        let ds = guard.sets[0];

        // Binding 0: current frame STORAGE_IMAGE (read-only in shader, rgba8)
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(current_view)
            .image_layout(vk::ImageLayout::GENERAL)];
        // Binding 1: cdf53_compact_list (read-only)
        let compact_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.cdf53_compact_list_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        // Binding 2: cdf53_coefficients (write)
        let coeff_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.cdf53_coefficients_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&compact_info),
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&coeff_info),
        ];
        self.device.update_descriptor_sets(&writes, &[]);

        let push_vals: [u32; 2] = [cols, rows];
        let push_bytes = std::slice::from_raw_parts(
            push_vals.as_ptr() as *const u8,
            std::mem::size_of_val(&push_vals),
        );

        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.cdf53_forward_l1_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.cdf53_forward_l1_pipeline_layout,
            0,
            &[ds],
            &[],
        );
        self.device.cmd_push_constants(
            cmd,
            self.cdf53_forward_l1_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            push_bytes,
        );
        // cdf53_dispatch_args_buffer already barriered for INDIRECT_COMMAND_READ
        // by the preceding cdf53_indirect_args stage barrier in the orchestrator.
        self.device
            .cmd_dispatch_indirect(cmd, self.cdf53_dispatch_args_buffer, 0);

        Ok(guard)
    }

    /// Stage 4c-L2 — CDF 5/3 forward wavelet, level 2.
    ///
    /// Reads LL1 from `cdf53_coefficients_buffer[0..256]` (per channel),
    /// overwrites those offsets with LL2/HL2/LH2/HH2.
    /// Dispatches via `cmd_dispatch_indirect` from `cdf53_dispatch_args_buffer`.
    ///
    /// Caller must:
    ///   - Ensure a SHADER_WRITE → SHADER_READ barrier on `cdf53_coefficients_buffer`
    ///     was issued after L1.
    ///   - Add a SHADER_WRITE → SHADER_READ barrier on `cdf53_coefficients_buffer`
    ///     after this call before dispatching L3.
    ///   - Hold the returned guard alive until after `wait_for_fences`.
    unsafe fn run_cdf53_forward_l2_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        cols: u32,
        rows: u32,
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        let set_layouts = [self.cdf53_forward_l2_descriptor_set_layout];
        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&ds_alloc)?,
        };
        let ds = guard.sets[0];

        // Binding 0: cdf53_compact_list (read-only)
        let compact_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.cdf53_compact_list_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        // Binding 1: cdf53_coefficients (read LL1, write L2 subbands)
        let coeff_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.cdf53_coefficients_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&compact_info),
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&coeff_info),
        ];
        self.device.update_descriptor_sets(&writes, &[]);

        let push_vals: [u32; 2] = [cols, rows];
        let push_bytes = std::slice::from_raw_parts(
            push_vals.as_ptr() as *const u8,
            std::mem::size_of_val(&push_vals),
        );

        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.cdf53_forward_l2_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.cdf53_forward_l2_pipeline_layout,
            0,
            &[ds],
            &[],
        );
        self.device.cmd_push_constants(
            cmd,
            self.cdf53_forward_l2_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            push_bytes,
        );
        // M3.3b diagnostic: GHOSTFRAME_CDF53_SKIP_L2_L3=1 skips this stage's
        // dispatch (descriptor set is still bound). Leaves L1's LL1 output in
        // place at coefficients[0..256] so the host-side diff can verify L1
        // in isolation. Gated behind `cdf53-diag` feature so production builds
        // skip the env-var check.
        #[cfg(feature = "cdf53-diag")]
        let skip_l2 = std::env::var("GHOSTFRAME_CDF53_SKIP_L2_L3").is_ok();
        #[cfg(not(feature = "cdf53-diag"))]
        let skip_l2 = false;
        if !skip_l2 {
            self.device
                .cmd_dispatch_indirect(cmd, self.cdf53_dispatch_args_buffer, 0);
        }

        Ok(guard)
    }

    /// Stage 4c-L3 — CDF 5/3 forward wavelet, level 3.
    ///
    /// Reads LL2 from `cdf53_coefficients_buffer[0..64]` (per channel),
    /// overwrites those offsets with LL3/HL3/LH3/HH3.
    /// Dispatches via `cmd_dispatch_indirect` from `cdf53_dispatch_args_buffer`.
    ///
    /// Caller must:
    ///   - Ensure a SHADER_WRITE → SHADER_READ barrier on `cdf53_coefficients_buffer`
    ///     was issued after L2.
    ///   - Add a SHADER_WRITE → HOST_READ barrier on `cdf53_coefficients_buffer`
    ///     after this call for Phase B readback.
    ///   - Hold the returned guard alive until after `wait_for_fences`.
    unsafe fn run_cdf53_forward_l3_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        cols: u32,
        rows: u32,
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        let set_layouts = [self.cdf53_forward_l3_descriptor_set_layout];
        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&ds_alloc)?,
        };
        let ds = guard.sets[0];

        // Binding 0: cdf53_compact_list (read-only)
        let compact_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.cdf53_compact_list_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        // Binding 1: cdf53_coefficients (read LL2, write L3 subbands)
        let coeff_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.cdf53_coefficients_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&compact_info),
            vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&coeff_info),
        ];
        self.device.update_descriptor_sets(&writes, &[]);

        let push_vals: [u32; 2] = [cols, rows];
        let push_bytes = std::slice::from_raw_parts(
            push_vals.as_ptr() as *const u8,
            std::mem::size_of_val(&push_vals),
        );

        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.cdf53_forward_l3_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.cdf53_forward_l3_pipeline_layout,
            0,
            &[ds],
            &[],
        );
        self.device.cmd_push_constants(
            cmd,
            self.cdf53_forward_l3_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            push_bytes,
        );
        // M3.3b diagnostic: GHOSTFRAME_CDF53_SKIP_L2_L3=1 or
        // GHOSTFRAME_CDF53_SKIP_L3=1 skips this stage's dispatch (descriptor
        // set is still bound). With SKIP_L3 only, L2's output is preserved at
        // coefficients[0..256] (LL2 region) and the host can verify L2.
        // Gated behind `cdf53-diag` feature so production skips env-var checks.
        #[cfg(feature = "cdf53-diag")]
        let skip_l3 = std::env::var("GHOSTFRAME_CDF53_SKIP_L2_L3").is_ok()
            || std::env::var("GHOSTFRAME_CDF53_SKIP_L3").is_ok();
        #[cfg(not(feature = "cdf53-diag"))]
        let skip_l3 = false;
        if !skip_l3 {
            self.device
                .cmd_dispatch_indirect(cmd, self.cdf53_dispatch_args_buffer, 0);
        }

        Ok(guard)
    }
}
