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

/// Build a whole-queue-family-ignoring [`vk::BufferMemoryBarrier`] at offset 0.
fn buf_barrier(
    buffer: vk::Buffer,
    src_access: vk::AccessFlags,
    dst_access: vk::AccessFlags,
    size: vk::DeviceSize,
) -> vk::BufferMemoryBarrier<'static> {
    vk::BufferMemoryBarrier::default()
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .buffer(buffer)
        .offset(0)
        .size(size)
}

// ---------------------------------------------------------------------------
// RAII guards for per-frame transient Vulkan resources
// ---------------------------------------------------------------------------
//
// `process_frame_with_imported` and `run_first_frame_passes` allocate descriptor
// sets, command buffers, and fences per call. Without RAII guards, any `?`
// early-exit would leak those resources — and the descriptor pool has bounded
// capacity (`max_sets=20`), so leaked sets would exhaust the pool and brick
// subsequent frames.
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

/// One descriptor binding for `GpuFrameProcessor::record_stage`; the
/// position in the binding slice is the shader's binding number.
#[derive(Clone, Copy)]
enum StageBinding {
    Image(vk::ImageView),
    Buffer(vk::Buffer),
}

/// How `GpuFrameProcessor::record_stage` dispatches. `Skip` still binds
/// and pushes (used by the cdf53-diag skip logic).
#[derive(Clone, Copy)]
enum StageDispatch {
    Direct(u32, u32, u32),
    Indirect(vk::Buffer),
    Skip,
}

/// Largest descriptor-binding count of any compute stage (palette_fold,
/// pal_rle_index).
const MAX_STAGE_BINDINGS: usize = 6;

// ---------------------------------------------------------------------------
// Persistent per-processor command buffer + fence
// ---------------------------------------------------------------------------

impl GpuFrameProcessor {
    /// Hand out the processor's single command buffer, reset and ready for
    /// `begin_command_buffer`.
    ///
    /// Invariant: every frame submits `frame_cmd` once and waits on
    /// `frame_fence` before returning, so at frame start nothing is in
    /// flight. The one exception is a previous frame whose `queue_submit`
    /// succeeded but whose `wait_for_fences` failed (`frame_fence_pending`):
    /// then the GPU may still own the buffer, so we wait again first, and if
    /// that fails too (device lost) we return the error without touching
    /// either object.
    ///
    /// Both resets are unconditionally safe after that: resetting a command
    /// buffer left in the recording/executable state by an early `?` return
    /// is legal (the pool has RESET_COMMAND_BUFFER), and resetting a fence
    /// that is signalled (normal case) or unsignalled (previous frame failed
    /// before/at submit) is legal as long as it is not attached to a pending
    /// submission, which the wait above rules out.
    pub(super) unsafe fn begin_frame_cmd(
        &self,
    ) -> Result<vk::CommandBuffer, Box<dyn std::error::Error>> {
        if self.frame_fence_pending.get() {
            self.device
                .wait_for_fences(&[self.frame_fence], true, u64::MAX)?;
            self.frame_fence_pending.set(false);
        }
        self.device.reset_fences(&[self.frame_fence])?;
        self.device
            .reset_command_buffer(self.frame_cmd, vk::CommandBufferResetFlags::empty())?;
        Ok(self.frame_cmd)
    }

    /// End-of-frame: submit `cmd` (already `end_command_buffer`ed) and block
    /// until the GPU is done with it.
    unsafe fn submit_frame_cmd_and_wait(
        &self,
        cmd: vk::CommandBuffer,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cmds = [cmd];
        let submit_info = vk::SubmitInfo::default().command_buffers(&cmds);
        self.device
            .queue_submit(self.queue, &[submit_info], self.frame_fence)?;
        // Submitted: until the wait returns the GPU owns cmd and the fence.
        self.frame_fence_pending.set(true);
        self.device
            .wait_for_fences(&[self.frame_fence], true, u64::MAX)?;
        self.frame_fence_pending.set(false);
        Ok(())
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
        // Interleaved U,V: ceil(width/2) pairs per row, so the stride is
        // width rounded UP to even. For odd widths `width` bytes would put
        // the last V on the next row's first U (and one byte past the
        // buffer on the last row).
        let uv_stride = width.next_multiple_of(2);
        let uv_offset = y_stride * height; // UV follows immediately after Y
                                           // The shader writes whole u32 words (atomicAnd/Or on `data[addr / 4]`),
                                           // so the buffer must be a whole number of words or the last 1-3 UV
                                           // bytes fall in a word past the descriptor range.
        let total =
            ((uv_offset + uv_stride * height.div_ceil(2)) as vk::DeviceSize).next_multiple_of(4);

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
        // Capture the escalation count staged by set_escalation_candidates
        // (called by io_bridge before this frame), then immediately reset it to 0
        // so a caller that forgets set_escalation_candidates on the next frame
        // does not replay the current frame's list.
        // A failed frame must not leave the previous frame's tier readable.
        let prev_ingest = self.last_ingest.take();
        let escalation_count_this_frame = self.staged_escalation_count;
        self.staged_escalation_count = 0;

        // Reject bad geometry before touching any other state: a zero dimension
        // otherwise reaches ensure_nv12_buffer and creates zero-size buffers.
        dmabuf_frame_bytes(width, height, stride)?;

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
        let current = match self.import_dmabuf(fd, width, height, stride) {
            Ok(c) => {
                self.last_ingest = Some(IngestTier::DmabufBuffer);
                c
            }
            Err(e) => {
                // Not importable (see import_dmabuf). Read it through the CPU
                // with DMA_BUF_IOCTL_SYNC and run the from-pixels path rather
                // than analyse anything unvalidated.
                // Warn on the transition only: a permanently unimportable
                // source would otherwise log at frame rate.
                if prev_ingest != Some(IngestTier::CpuMmap) {
                    tracing::warn!("dma-buf GPU import refused ({e}); using CPU-mmap ingest");
                }
                let tight = read_dmabuf_tight(fd, width, height, stride)?;
                // Re-stage what was consumed above; the delegate consumes it.
                self.staged_escalation_count = escalation_count_this_frame;
                self.last_ingest = Some(IngestTier::CpuMmap);
                return self.process_frame_inner_from_pixels(&tight, width, height, width * 4);
            }
        };
        // Result must always destroy `current` at end. Use a closure-style
        // cleanup by carrying it forward and explicitly destroying.
        let result = self.process_frame_with_imported(&current, geom, escalation_count_this_frame);
        // Always clean up the imported DMA-BUF VkImage (transient).
        self.destroy_prev_frame(current);
        result
    }

    /// CPU-pixels variant of `process_frame_inner`: uploads BGRA pixels from
    /// host memory into a transient HOST_VISIBLE LINEAR STORAGE VkImage and
    /// runs the same SAD/NV12/classifier pipeline as the DMA-BUF path.
    ///
    /// Used by the `xdaemon`'s modesetting-FB capture path when DMA-BUF cross-
    /// device import is unreliable (e.g. VKMS exporter → discrete-GPU importer).
    pub(super) unsafe fn process_frame_inner_from_pixels(
        &mut self,
        pixels: &[u8],
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<FrameAnalysis, Box<dyn std::error::Error>> {
        // Reject bad input before any state changes (staged escalation count,
        // baseline, NV12 buffer): a rejected frame must cost nothing.
        if width == 0 || height == 0 {
            return Err(format!("from-pixels frame has a zero dimension: {width}x{height}").into());
        }
        if (stride as u64) < (width as u64) * 4 {
            return Err(format!(
                "from-pixels stride {stride} < width*4 = {}",
                (width as u64) * 4
            )
            .into());
        }

        let escalation_count_this_frame = self.staged_escalation_count;
        self.staged_escalation_count = 0;

        let geom = FrameGeometry::from_dims(width, height);

        if geom.tile_count() > self.max_tiles {
            return Err(format!(
                "tile_count {} exceeds max_tiles {}",
                geom.tile_count(),
                self.max_tiles
            )
            .into());
        }

        if width != self.last_width || height != self.last_height {
            if let Some(prev) = self.prev_image.take() {
                self.destroy_prev_frame(prev);
            }
            self.last_width = width;
            self.last_height = height;
        }

        self.ensure_nv12_buffer(width, height)?;

        let current = self.import_pixels(pixels, width, height, stride)?;
        let result = self.process_frame_with_imported(&current, geom, escalation_count_this_frame);
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
        escalation_count: u32,
    ) -> Result<FrameAnalysis, Box<dyn std::error::Error>> {
        let FrameGeometry {
            width,
            height,
            cols,
            rows,
        } = geom;
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
            self.run_first_frame_passes(
                current,
                snapshot.as_ref(),
                geom,
                nv12_layout,
                escalation_count,
            )?;

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
                tile_analysis: self.analysis_buf.ptr as *const TileAnalysis,
                tile_analysis_len: cols * rows,
                palrle_compact_list: self.palrle_compact_list.ptr as *const u32,
                palrle_compact_count: *self.palrle_compact_count.ptr,
                frame_palette_set: self.frame_palette_set.ptr as *const FramePaletteEntryRaw,
                frame_palette_set_count: *self.frame_palette_count.ptr,
                per_tile_frame_palette_id: self.per_tile_frame_palette_id.ptr as *const u8,
                folded_into: self.folded_into.ptr as *const u32,
                index_buffer: self.index.ptr as *const u8,
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

        // --- Command buffer (persistent; reset per frame) ---
        let cmd = self.begin_frame_cmd()?;

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
        // A dma-buf ingest acquires `current` itself (copy + GENERAL); only
        // `prev` still needs the barrier then (element 1).
        let acquired = self.record_acquire_current(cmd, current);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::HOST,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &barriers[usize::from(acquired)..],
        );

        // 2. SAD dispatch
        let sad_push: [u32; 3] = [width, height, cols];
        let sad_ds_guard =
            self.run_sad_stage(cmd, current.view, prev_view, &sad_push, (cols, rows, 1))?;

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
        let analysis_ds_guard =
            self.run_analysis_stage(cmd, current.view, &analysis_push, (cols, rows, 1))?;

        // Inter-dispatch barrier: SAD + tile_analysis writes must be visible to Stage 1.5a reads.
        let buf_barrier_inputs = [
            buf_barrier(
                self.sad_buf.buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::SHADER_READ,
                vk::WHOLE_SIZE,
            ),
            buf_barrier(
                self.analysis_buf.buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::SHADER_READ,
                vk::WHOLE_SIZE,
            ),
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

        let guards = self.record_shared_stages(cmd, current.view, cols, rows, escalation_count)?;
        let pal_id_bytes = guards.pal_id_bytes;

        // 5a. Snapshot copy: current (DMA-BUF) -> prev_image (owned). This
        // makes prev_image a true point-in-time snapshot of THIS frame for
        // the next frame's SAD comparison. Must run AFTER all three shader
        // dispatches (SAD, NV12, analysis), since they all read from current.
        self.record_snapshot_copy(
            cmd,
            current.image,
            prev_image,
            vk::ImageLayout::GENERAL,
            vk::AccessFlags::SHADER_READ,
            width,
            height,
        );

        // 5. Barriers: every output buffer -> HOST_READ.
        self.record_host_readback_barriers(
            cmd,
            nv12_buffer,
            vk::AccessFlags::SHADER_WRITE,
            pal_id_bytes,
        );

        self.device.end_command_buffer(cmd)?;

        // 6. Submit and wait on the persistent fence.
        self.submit_frame_cmd_and_wait(cmd)?;

        // 7. Read SAD values
        let sad_slice = std::slice::from_raw_parts(self.sad_buf.ptr, tile_count as usize);
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

        // 8. Per-frame descriptor sets are freed when their RAII guards drop
        //    at end of scope. We touch the
        //    guards here to keep them alive past the `wait_for_fences` above.
        //    sad_ds_guard, nv12_ds_guard, analysis_ds_guard,
        //    palrle_compact_ds_guard, palrle_indirect_args_ds_guard,
        //    cdf53_compact_ds_guard, cdf53_indirect_args_ds_guard,
        //    cdf53_forward_l1/l2/l3_ds_guard, _cdf53_escalation_ds_guards,
        //    palette_fold_ds_guard, palette_subset_fold_init_ds_guard,
        //    palette_subset_fold_ds_guard, and pal_rle_index_ds_guard are
        //    dropped explicitly before step 9 to
        //    release the immutable borrows on self (all are returned from helper
        //    methods whose lifetimes are tied to &self) before the mutable
        //    self.prev_image borrow.
        //    GPU-safety: these drops must NOT move above the `wait_for_fences` at
        //    step 6 — releasing descriptor sets while the GPU is still using them
        //    is undefined behaviour (use-after-free in the descriptor pool).
        let _ = (&nv12_ds_guard, &analysis_ds_guard, &guards);
        drop(sad_ds_guard);
        drop(nv12_ds_guard);
        drop(analysis_ds_guard);
        guards.release_forward();

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
            tile_analysis: self.analysis_buf.ptr as *const TileAnalysis,
            tile_analysis_len: cols * rows,
            palrle_compact_list: self.palrle_compact_list.ptr as *const u32,
            palrle_compact_count: *self.palrle_compact_count.ptr,
            frame_palette_set: self.frame_palette_set.ptr as *const FramePaletteEntryRaw,
            frame_palette_set_count: *self.frame_palette_count.ptr,
            per_tile_frame_palette_id: self.per_tile_frame_palette_id.ptr as *const u8,
            folded_into: self.folded_into.ptr as *const u32,
            index_buffer: self.index.ptr as *const u8,
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
        escalation_count: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let FrameGeometry {
            width,
            height,
            cols,
            rows,
        } = geom;
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

        let cmd = self.begin_frame_cmd()?;

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
        if !self.record_acquire_current(cmd, current) {
            self.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::HOST,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &img_barrier,
            );
        }

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
        // buffers (`nv12_buffer` vs `self.analysis_buf.buffer`). The
        // COMPUTE→HOST buffer barrier below covers both writes. This mirrors
        // the subsequent-frames branch comment at the SAD/NV12/Analysis
        // dispatch trio.
        let analysis_push: [u32; 3] = [width, height, cols];
        let analysis_ds_guard =
            self.run_analysis_stage(cmd, current.view, &analysis_push, (cols, rows, 1))?;

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
            .cmd_fill_buffer(cmd, self.sad_buf.buffer, 0, vk::WHOLE_SIZE, 0xFFFF_FFFFu32);

        // Inter-dispatch barrier: analysis write + sad pre-fill must be
        // visible to Stage 1.5a reads.
        // srcStageMask covers COMPUTE_SHADER (analysis write) and TRANSFER
        // (cmd_fill_buffer).
        let buf_barrier_inputs = [
            buf_barrier(
                self.sad_buf.buffer,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::SHADER_READ,
                vk::WHOLE_SIZE,
            ),
            buf_barrier(
                self.analysis_buf.buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::SHADER_READ,
                vk::WHOLE_SIZE,
            ),
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

        let guards = self.record_shared_stages(cmd, current.view, cols, rows, escalation_count)?;
        let pal_id_bytes = guards.pal_id_bytes;

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
            self.record_snapshot_copy(
                cmd,
                current.image,
                snap.image,
                vk::ImageLayout::UNDEFINED,
                vk::AccessFlags::empty(),
                width,
                height,
            );
        }

        // All output buffers -> HOST_READ. srcStageMask covers both
        // COMPUTE_SHADER (shader writes) and TRANSFER (cmd_fill_buffer on
        // sad_buffer and palrle_compact_count_buffer).
        self.record_host_readback_barriers(
            cmd,
            nv12_buffer,
            vk::AccessFlags::TRANSFER_WRITE,
            pal_id_bytes,
        );

        self.device.end_command_buffer(cmd)?;

        self.submit_frame_cmd_and_wait(cmd)?;

        // Per-frame descriptor sets are freed when their RAII guards drop at
        // end of scope. Touch them here
        // to keep them alive past `wait_for_fences`.
        let _ = (&nv12_ds_guard, &analysis_ds_guard, &guards);
        guards.release_reverse();

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DMA-BUF import helpers
// ---------------------------------------------------------------------------

impl GpuFrameProcessor {
    /// Import a DMA-BUF fd and return a pipeline image that will hold its
    /// pixels once the frame's command buffer has run `record_acquire_current`.
    ///
    /// The dma-buf's memory is imported as a `VkBuffer`, not as a `VkImage`.
    /// A LINEAR `VkImage` has a driver-chosen row pitch (RADV pads it to 256
    /// bytes) and `VK_EXT_external_memory_dma_buf` has no way to tell the
    /// driver the producer's pitch, so a zero-copy image import is only right
    /// when the two happen to coincide. A buffer has no pitch: the copy into
    /// the (driver-owned, OPTIMAL) pipeline image takes the real stride as
    /// `bufferRowLength`, so any stride that is a multiple of 4 is exact on
    /// any driver.
    ///
    /// Returns `Err` (caller takes the CPU-mmap path) when the input cannot be
    /// imported safely: bad geometry, fd smaller than the frame, a fd that is
    /// not a dma-buf, a driver without dma-buf buffer import, or a driver whose
    /// buffer requirements exceed the fd. Nothing is ever analysed unvalidated.
    pub(super) unsafe fn import_dmabuf(
        &self,
        fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<PrevFrame, Box<dyn std::error::Error>> {
        use std::os::fd::{BorrowedFd, IntoRawFd};

        let need = dmabuf_frame_bytes(width, height, stride)?;
        if !stride.is_multiple_of(4) {
            // bufferRowLength is in texels; a stride that is not a whole
            // number of texels cannot be expressed to the copy.
            return Err(format!("dma-buf import: stride {stride} is not a multiple of 4").into());
        }

        let fd_size = libc::lseek(fd, 0, libc::SEEK_END);
        if fd_size < 0 {
            return Err(format!(
                "dma-buf import: cannot size fd: {}",
                io::Error::last_os_error()
            )
            .into());
        }
        // Leave the offset where a dma-buf mmap/read expects it.
        libc::lseek(fd, 0, libc::SEEK_SET);
        if need > fd_size as u64 {
            return Err(
                format!("dma-buf import: frame needs {need} bytes, fd has {fd_size}").into(),
            );
        }

        // --- Does the driver import dma-buf memory as a buffer at all? ---
        let ext_buf_info = vk::PhysicalDeviceExternalBufferInfo::default()
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut ext_buf_props = vk::ExternalBufferProperties::default();
        self.instance
            .get_physical_device_external_buffer_properties(
                self.physical_device,
                &ext_buf_info,
                &mut ext_buf_props,
            );
        let feats = ext_buf_props
            .external_memory_properties
            .external_memory_features;
        if !feats.contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE) {
            return Err("dma-buf import: driver does not import dma-buf as a buffer".into());
        }
        let dedicated_only = feats.contains(vk::ExternalMemoryFeatureFlags::DEDICATED_ONLY);

        // --- What the fd itself says it can be imported as. ---
        let efd = ash::khr::external_memory_fd::Device::new(&self.instance, &self.device);
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        efd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            fd,
            &mut fd_props,
        )
        .map_err(|e| format!("dma-buf import: vkGetMemoryFdProperties: {e:?} (not a dma-buf?)"))?;

        // Owns the fd until Vulkan accepts it; owns each object until the
        // PrevFrame is built. Drop order is reverse of creation.
        struct Pending<'a> {
            dev: &'a ash::Device,
            fd: Option<std::os::fd::OwnedFd>,
            buffer: vk::Buffer,
            buf_mem: vk::DeviceMemory,
            image: vk::Image,
            img_mem: vk::DeviceMemory,
            view: vk::ImageView,
        }
        impl Drop for Pending<'_> {
            fn drop(&mut self) {
                // Destroying/freeing VK_NULL_HANDLE is a legal no-op.
                unsafe {
                    self.dev.destroy_image_view(self.view, None);
                    self.dev.destroy_image(self.image, None);
                    self.dev.free_memory(self.img_mem, None);
                    self.dev.destroy_buffer(self.buffer, None);
                    self.dev.free_memory(self.buf_mem, None);
                }
            }
        }
        let mut p = Pending {
            dev: &self.device,
            fd: Some(BorrowedFd::borrow_raw(fd).try_clone_to_owned()?),
            buffer: vk::Buffer::null(),
            buf_mem: vk::DeviceMemory::null(),
            image: vk::Image::null(),
            img_mem: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
        };

        // --- The buffer over the dma-buf. ---
        let mut ext_buf_ci = vk::ExternalMemoryBufferCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let buf_ci = vk::BufferCreateInfo::default()
            .size(need)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut ext_buf_ci);
        p.buffer = self.device.create_buffer(&buf_ci, None)?;
        let buf_reqs = self.device.get_buffer_memory_requirements(p.buffer);
        if buf_reqs.size > fd_size as u64 {
            return Err(format!(
                "dma-buf import: driver needs {} bytes for the buffer, fd has {fd_size}",
                buf_reqs.size
            )
            .into());
        }
        // The memory type comes from the fd, intersected with what the buffer
        // can live in. No DEVICE_LOCAL preference: an import is whatever the
        // exporter's memory is.
        let usable = fd_props.memory_type_bits & buf_reqs.memory_type_bits;
        if usable == 0 {
            return Err(format!(
                "dma-buf import: no common memory type (fd {:#x}, buffer {:#x})",
                fd_props.memory_type_bits, buf_reqs.memory_type_bits
            )
            .into());
        }
        let buf_mem_type = usable.trailing_zeros();

        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(std::os::fd::AsRawFd::as_raw_fd(p.fd.as_ref().unwrap()));
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(p.buffer);
        let mut alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(buf_reqs.size)
            .memory_type_index(buf_mem_type)
            .push_next(&mut import_info);
        if dedicated_only {
            alloc_info = alloc_info.push_next(&mut dedicated);
        }
        p.buf_mem = self.device.allocate_memory(&alloc_info, None)?;
        // Vulkan owns the fd now; closing it would be a double close.
        let _ = p.fd.take().unwrap().into_raw_fd();
        self.device.bind_buffer_memory(p.buffer, p.buf_mem, 0)?;

        // --- The pipeline image the compute stages read. Owned, OPTIMAL: ---
        // STORAGE for imageLoad, TRANSFER_DST for the ingest copy, and
        // TRANSFER_SRC because record_snapshot_copy copies out of it.
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
            .usage(
                vk::ImageUsageFlags::STORAGE
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        p.image = self.device.create_image(&image_ci, None)?;
        let img_reqs = self.device.get_image_memory_requirements(p.image);
        let mem_props = self
            .instance
            .get_physical_device_memory_properties(self.physical_device);
        let img_mem_type = find_memory_type(
            &mem_props,
            img_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .or_else(|| {
            find_memory_type(
                &mem_props,
                img_reqs.memory_type_bits,
                vk::MemoryPropertyFlags::empty(),
            )
        })
        .ok_or("no memory type for the dma-buf ingest image")?;
        let img_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(img_reqs.size)
            .memory_type_index(img_mem_type);
        p.img_mem = self.device.allocate_memory(&img_alloc, None)?;
        self.device.bind_image_memory(p.image, p.img_mem, 0)?;

        let view_ci = vk::ImageViewCreateInfo::default()
            .image(p.image)
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
        p.view = self.device.create_image_view(&view_ci, None)?;

        let frame = PrevFrame {
            image: p.image,
            memory: p.img_mem,
            view: p.view,
            layout: vk::ImageLayout::UNDEFINED,
            width,
            height,
            ingest: Some(BufferIngest {
                buffer: p.buffer,
                memory: p.buf_mem,
                row_length_texels: stride / 4,
            }),
        };
        // Ownership moves to the PrevFrame; disarm the guard.
        std::mem::forget(p);
        Ok(frame)
    }

    /// Record the acquisition of `current` into `cmd`, leaving it in GENERAL
    /// with SHADER_READ visible. Returns `true` when this recorded everything
    /// (dma-buf ingest); `false` when the image is host-written and the caller
    /// still needs its own PREINITIALIZED -> GENERAL barrier.
    pub(super) unsafe fn record_acquire_current(
        &self,
        cmd: vk::CommandBuffer,
        current: &PrevFrame,
    ) -> bool {
        let Some(ing) = current.ingest.as_ref() else {
            return false;
        };
        let range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        // 1. Take the buffer from the foreign producer; image -> TRANSFER_DST.
        let buf_acquire = [vk::BufferMemoryBarrier::default()
            .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
            .dst_queue_family_index(self.queue_family_index)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .buffer(ing.buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE)];
        let to_dst = [vk::ImageMemoryBarrier::default()
            .old_layout(current.layout)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(current.image)
            .subresource_range(range)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &buf_acquire,
            &to_dst,
        );
        // 2. The copy. bufferRowLength is the real stride, in texels.
        let region = [vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(ing.row_length_texels)
            .buffer_image_height(0)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D {
                width: current.width,
                height: current.height,
                depth: 1,
            })];
        self.device.cmd_copy_buffer_to_image(
            cmd,
            ing.buffer,
            current.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &region,
        );
        // 3. Hand the image to the compute stages.
        let to_general = [vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(current.image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &to_general,
        );
        true
    }

    /// Destroy all Vulkan resources held by a `PrevFrame`.
    pub(super) unsafe fn destroy_prev_frame(&self, frame: PrevFrame) {
        self.device.destroy_image_view(frame.view, None);
        self.device.destroy_image(frame.image, None);
        self.device.free_memory(frame.memory, None);
        if let Some(ing) = frame.ingest {
            self.device.destroy_buffer(ing.buffer, None);
            self.device.free_memory(ing.memory, None);
        }
    }

    /// Upload BGRA pixels from CPU memory into a HOST_VISIBLE LINEAR STORAGE
    /// VkImage and return it as a `PrevFrame`. Mirrors `import_dmabuf` for the
    /// non-zero-copy capture path used when DMA-BUF import is unreliable
    /// (e.g. VKMS exporter → discrete-GPU importer: same-GPU + UMA works, but
    /// cross-device produces stale/scrambled content).
    ///
    /// The result is a transient image: the caller passes it to
    /// `process_frame_with_imported` and then calls `destroy_prev_frame`.
    unsafe fn import_pixels(
        &self,
        pixels: &[u8],
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<PrevFrame, Box<dyn std::error::Error>> {
        // A stride below one row of pixels would leave the tail of every GPU
        // row uninitialised and the frame would be garbage, not an error.
        if (stride as u64) < (width as u64) * 4 {
            return Err(format!(
                "import_pixels: stride {stride} < width*4 = {}",
                (width as u64) * 4
            )
            .into());
        }
        let required = (stride as usize) * (height as usize);
        if pixels.len() < required {
            return Err(format!(
                "import_pixels: pixels.len()={} < stride*height={}",
                pixels.len(),
                required
            )
            .into());
        }

        // Create LINEAR STORAGE image first so we can query its real row pitch.
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
            .initial_layout(vk::ImageLayout::PREINITIALIZED);
        let image = self.device.create_image(&image_ci, None)?;

        let mem_reqs = self.device.get_image_memory_requirements(image);
        let mem_props = self
            .instance
            .get_physical_device_memory_properties(self.physical_device);

        let mem_type = find_memory_type(
            &mem_props,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or_else(|| {
            // Image creation succeeded; release it before bubbling up.
            self.device.destroy_image(image, None);
            "no HOST_VISIBLE+HOST_COHERENT memory type for LINEAR STORAGE image"
        })?;

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mem_type);
        let memory = match self.device.allocate_memory(&alloc_info, None) {
            Ok(m) => m,
            Err(e) => {
                self.device.destroy_image(image, None);
                return Err(e.into());
            }
        };
        if let Err(e) = self.device.bind_image_memory(image, memory, 0) {
            self.device.free_memory(memory, None);
            self.device.destroy_image(image, None);
            return Err(e.into());
        }

        // Query subresource layout so we can copy row-by-row at the GPU's
        // expected row pitch (LINEAR images may pad rows for alignment).
        let subresource = vk::ImageSubresource::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .mip_level(0)
            .array_layer(0);
        let sl = self.device.get_image_subresource_layout(image, subresource);
        let gpu_row_pitch = sl.row_pitch as usize;
        let gpu_offset = sl.offset as usize;

        // Map and memcpy row-by-row.
        let map_ptr =
            self.device
                .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?
                as *mut u8;
        let src_row_pitch = stride as usize;
        // Copy the pixels, not the source padding.
        let copy_bytes_per_row = (width as usize * 4).min(gpu_row_pitch);
        for row in 0..(height as usize) {
            let src = pixels.as_ptr().add(row * src_row_pitch);
            let dst = map_ptr.add(gpu_offset + row * gpu_row_pitch);
            std::ptr::copy_nonoverlapping(src, dst, copy_bytes_per_row);
        }
        self.device.unmap_memory(memory);

        // Create VkImageView.
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
        let view = match self.device.create_image_view(&view_ci, None) {
            Ok(v) => v,
            Err(e) => {
                self.device.free_memory(memory, None);
                self.device.destroy_image(image, None);
                return Err(e.into());
            }
        };

        Ok(PrevFrame {
            image,
            memory,
            view,
            layout: vk::ImageLayout::PREINITIALIZED,
            width,
            height,
            ingest: None,
        })
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
            ingest: None,
        })
    }

    /// Generic compute-stage recorder shared by every `run_*_stage`.
    ///
    /// Allocates one descriptor set from the shared pool, writes
    /// `bindings` (index == binding number) in a single
    /// `update_descriptor_sets`, binds the pipeline and set, pushes `push`
    /// as raw `u32` words at offset 0 (skipped when empty), then dispatches.
    ///
    /// # Safety
    ///
    /// Same contract as the wrappers: `cmd` is recording, image views are
    /// in `GENERAL`, and the returned guard outlives `wait_for_fences`.
    unsafe fn record_stage<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        stage: &ComputeStage,
        bindings: &[StageBinding],
        push: &[u32],
        dispatch: StageDispatch,
    ) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
        debug_assert!(bindings.len() <= MAX_STAGE_BINDINGS);
        // Allocate one descriptor set from the pool. The returned guard frees
        // it on drop, ensuring the bounded pool doesn't leak on early-exit.
        let set_layouts = [stage.dsl];
        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&set_layouts);
        let guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&ds_alloc)?,
        };
        let ds = guard.sets[0];

        let mut image_infos = [vk::DescriptorImageInfo::default(); MAX_STAGE_BINDINGS];
        let mut buffer_infos = [vk::DescriptorBufferInfo::default(); MAX_STAGE_BINDINGS];
        for (i, b) in bindings.iter().enumerate() {
            match *b {
                StageBinding::Image(view) => {
                    image_infos[i] = vk::DescriptorImageInfo::default()
                        .image_view(view)
                        .image_layout(vk::ImageLayout::GENERAL);
                }
                StageBinding::Buffer(buffer) => {
                    buffer_infos[i] = vk::DescriptorBufferInfo::default()
                        .buffer(buffer)
                        .offset(0)
                        .range(vk::WHOLE_SIZE);
                }
            }
        }
        let mut writes = [vk::WriteDescriptorSet::default(); MAX_STAGE_BINDINGS];
        for (i, b) in bindings.iter().enumerate() {
            let w = vk::WriteDescriptorSet::default()
                .dst_set(ds)
                .dst_binding(i as u32);
            writes[i] = match *b {
                StageBinding::Image(_) => w
                    .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    .image_info(std::slice::from_ref(&image_infos[i])),
                StageBinding::Buffer(_) => w
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(std::slice::from_ref(&buffer_infos[i])),
            };
        }
        self.device
            .update_descriptor_sets(&writes[..bindings.len()], &[]);

        // Bind + push + dispatch.
        self.device
            .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, stage.pipeline);
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            stage.layout,
            0,
            &[ds],
            &[],
        );
        if !push.is_empty() {
            let push_bytes =
                std::slice::from_raw_parts(push.as_ptr() as *const u8, std::mem::size_of_val(push));
            self.device.cmd_push_constants(
                cmd,
                stage.layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                push_bytes,
            );
        }
        match dispatch {
            StageDispatch::Direct(x, y, z) => self.device.cmd_dispatch(cmd, x, y, z),
            StageDispatch::Indirect(buffer) => self.device.cmd_dispatch_indirect(cmd, buffer, 0),
            StageDispatch::Skip => {}
        }

        Ok(guard)
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
        self.record_stage(
            cmd,
            &self.sad,
            &[
                StageBinding::Image(current_view),
                StageBinding::Image(prev_view),
                StageBinding::Buffer(self.sad_buf.buffer),
            ],
            push_constants,
            StageDispatch::Direct(workgroups.0, workgroups.1, workgroups.2),
        )
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
        // NV12 shader: workgroup = 2x2 pixels, dispatch = (width/2, height/2, 1)
        self.record_stage(
            cmd,
            &self.nv12,
            &[
                StageBinding::Image(current_view),
                StageBinding::Buffer(nv12_buffer),
            ],
            push_constants,
            StageDispatch::Direct(workgroups.0, workgroups.1, workgroups.2),
        )
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
        self.record_stage(
            cmd,
            &self.analysis,
            &[
                StageBinding::Image(current_view),
                StageBinding::Buffer(self.analysis_buf.buffer),
            ],
            push_constants,
            StageDispatch::Direct(workgroups.0, workgroups.1, workgroups.2),
        )
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
        self.record_stage(
            cmd,
            &self.palrle_compact,
            &[
                StageBinding::Buffer(self.sad_buf.buffer),
                StageBinding::Buffer(self.analysis_buf.buffer),
                StageBinding::Buffer(self.palrle_compact_list.buffer),
                StageBinding::Buffer(self.palrle_compact_count.buffer),
            ],
            push_constants,
            StageDispatch::Direct(workgroups.0, workgroups.1, workgroups.2),
        )
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
        self.record_stage(
            cmd,
            &self.cdf53_compact,
            &[
                StageBinding::Buffer(self.sad_buf.buffer),
                StageBinding::Buffer(self.analysis_buf.buffer),
                StageBinding::Buffer(self.cdf53_compact_list_buffer),
                StageBinding::Buffer(self.cdf53_compact_count_buffer),
            ],
            push_constants,
            StageDispatch::Direct(workgroups.0, workgroups.1, workgroups.2),
        )
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
        self.record_stage(
            cmd,
            &self.palrle_indirect_args,
            &[
                StageBinding::Buffer(self.palrle_compact_count.buffer),
                StageBinding::Buffer(self.palrle_indirect_args_buf.buffer),
            ],
            &[],
            StageDispatch::Direct(1, 1, 1),
        )
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
        self.record_stage(
            cmd,
            &self.cdf53_indirect_args,
            &[
                StageBinding::Buffer(self.cdf53_compact_count_buffer),
                StageBinding::Buffer(self.cdf53_dispatch_args.buffer),
            ],
            &[],
            StageDispatch::Direct(1, 1, 1),
        )
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
    /// `self.palrle_indirect_args_buf.buffer` at offset 0 (the workgroup
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
        // palrle_indirect_args_buffer already barriered for INDIRECT_COMMAND_READ
        // by the Stage 1.5b barrier in the orchestrator.
        self.record_stage(
            cmd,
            &self.palette_fold,
            &[
                StageBinding::Buffer(self.analysis_buf.buffer),
                StageBinding::Buffer(self.palrle_compact_list.buffer),
                StageBinding::Buffer(self.frame_palette_set.buffer),
                StageBinding::Buffer(self.frame_palette_count.buffer),
                StageBinding::Buffer(self.hash_table.buffer),
                StageBinding::Buffer(self.per_tile_frame_palette_id.buffer),
            ],
            &[],
            StageDispatch::Indirect(self.palrle_indirect_args_buf.buffer),
        )
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
        self.record_stage(
            cmd,
            &self.palette_subset_fold_init,
            &[StageBinding::Buffer(self.folded_into.buffer)],
            &[],
            StageDispatch::Direct(workgroups.0, workgroups.1, workgroups.2),
        )
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
        // One workgroup per palette slot so each slot evaluates its own row.
        self.record_stage(
            cmd,
            &self.palette_subset_fold,
            &[
                StageBinding::Buffer(self.frame_palette_set.buffer),
                StageBinding::Buffer(self.folded_into.buffer),
            ],
            &[],
            StageDispatch::Direct(PALETTE_HASH_SLOTS as u32, 1, 1),
        )
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
    /// `self.palrle_indirect_args_buf.buffer` at offset 0 — the workgroup count
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
        // palrle_indirect_args_buffer already barriered for INDIRECT_COMMAND_READ
        // by the preceding palrle_indirect_args stage barrier in the orchestrator.
        self.record_stage(
            cmd,
            &self.pal_rle_index,
            &[
                StageBinding::Image(current_view),
                StageBinding::Buffer(self.palrle_compact_list.buffer),
                StageBinding::Buffer(self.frame_palette_set.buffer),
                StageBinding::Buffer(self.per_tile_frame_palette_id.buffer),
                StageBinding::Buffer(self.folded_into.buffer),
                StageBinding::Buffer(self.index.buffer),
            ],
            push_constants,
            StageDispatch::Indirect(self.palrle_indirect_args_buf.buffer),
        )
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
        // cdf53_dispatch_args_buffer already barriered for INDIRECT_COMMAND_READ
        // by the preceding cdf53_indirect_args stage barrier in the orchestrator.
        self.record_stage(
            cmd,
            &self.cdf53_forward_l1,
            &[
                StageBinding::Image(current_view),
                StageBinding::Buffer(self.cdf53_compact_list_buffer),
                StageBinding::Buffer(self.cdf53_coefficients_buffer),
            ],
            &[cols, rows],
            StageDispatch::Indirect(self.cdf53_dispatch_args.buffer),
        )
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
        // M3.3b diagnostic: GHOSTFRAME_CDF53_SKIP_L2_L3=1 skips this stage's
        // dispatch (descriptor set is still bound). Leaves L1's LL1 output in
        // place at coefficients[0..256] so the host-side diff can verify L1
        // in isolation. Gated behind `cdf53-diag` feature so production builds
        // skip the env-var check.
        #[cfg(feature = "cdf53-diag")]
        let skip_l2 = std::env::var("GHOSTFRAME_CDF53_SKIP_L2_L3").is_ok();
        #[cfg(not(feature = "cdf53-diag"))]
        let skip_l2 = false;
        let dispatch = if skip_l2 {
            StageDispatch::Skip
        } else {
            StageDispatch::Indirect(self.cdf53_dispatch_args.buffer)
        };
        self.record_stage(
            cmd,
            &self.cdf53_forward_l2,
            &[
                StageBinding::Buffer(self.cdf53_compact_list_buffer),
                StageBinding::Buffer(self.cdf53_coefficients_buffer),
            ],
            &[cols, rows],
            dispatch,
        )
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
        let dispatch = if skip_l3 {
            StageDispatch::Skip
        } else {
            StageDispatch::Indirect(self.cdf53_dispatch_args.buffer)
        };
        self.record_stage(
            cmd,
            &self.cdf53_forward_l3,
            &[
                StageBinding::Buffer(self.cdf53_compact_list_buffer),
                StageBinding::Buffer(self.cdf53_coefficients_buffer),
            ],
            &[cols, rows],
            dispatch,
        )
    }

    /// Run the L1→L2→L3 CDF 5/3 forward chain a SECOND time, against the
    /// escalation buffers. The image binding still points at `current_view`
    /// (escalation reads pixel data from the same source). The compact list
    /// and coefficient output are the dedicated escalation buffers.
    ///
    /// Uses `cmd_dispatch(count, 1, 1)` — the count is known on the CPU so no
    /// indirect-args buffer is needed.
    ///
    /// SAFETY:
    ///   - `count` MUST equal the value in `cdf53_escalation_count_buffer`
    ///     written by the host before this call.
    ///   - The `cdf53_escalation_list_buffer` MUST be filled with `count` valid
    ///     tile-idx values before the dispatch (host writeBuffer is ordered
    ///     before the subsequent submit, so this happens naturally).
    ///   - Holds three `ScopedDescriptorSets` until after `wait_for_fences`.
    ///   - If `count == 0`, returns an error (callers should skip this entirely).
    unsafe fn run_cdf53_forward_escalation_stages<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        current_view: vk::ImageView,
        cols: u32,
        rows: u32,
        count: u32,
    ) -> Result<
        (
            ScopedDescriptorSets<'a>,
            ScopedDescriptorSets<'a>,
            ScopedDescriptorSets<'a>,
        ),
        Box<dyn std::error::Error>,
    > {
        if count == 0 {
            return Err("escalation dispatch called with count=0".into());
        }

        // ── L1 stage ─────────────────────────────────────────────────────────
        // Binding 0: current frame STORAGE_IMAGE (same layout as dirty L1)
        // Binding 1: cdf53_escalation_list_buffer  (replaces cdf53_compact_list_buffer)
        // Binding 2: cdf53_escalation_coefficients_buffer  (replaces cdf53_coefficients_buffer)
        let l1_guard = self.record_stage(
            cmd,
            &self.cdf53_forward_l1,
            &[
                StageBinding::Image(current_view),
                StageBinding::Buffer(self.cdf53_escalation_list.buffer),
                StageBinding::Buffer(self.cdf53_escalation_coefficients_buffer),
            ],
            &[cols, rows],
            StageDispatch::Direct(count, 1, 1),
        )?;

        // Barrier L1 → L2: escalation coefficient writes from L1 must be
        // visible to L2.
        let buf_barrier_esc_l1_to_l2 = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_escalation_coefficients_buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_esc_l1_to_l2),
            &[],
        );

        // ── L2 stage ─────────────────────────────────────────────────────────
        // Binding 0: cdf53_escalation_list_buffer  (read-only)
        // Binding 1: cdf53_escalation_coefficients_buffer  (read LL1, write L2 subbands)
        let l2_guard = self.record_stage(
            cmd,
            &self.cdf53_forward_l2,
            &[
                StageBinding::Buffer(self.cdf53_escalation_list.buffer),
                StageBinding::Buffer(self.cdf53_escalation_coefficients_buffer),
            ],
            &[cols, rows],
            StageDispatch::Direct(count, 1, 1),
        )?;

        // Barrier L2 → L3: escalation coefficient writes from L2 must be
        // visible to L3.
        let buf_barrier_esc_l2_to_l3 = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_escalation_coefficients_buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_esc_l2_to_l3),
            &[],
        );

        // ── L3 stage ─────────────────────────────────────────────────────────
        // Binding 0: cdf53_escalation_list_buffer  (read-only)
        // Binding 1: cdf53_escalation_coefficients_buffer  (read LL2, write L3 subbands)
        let l3_guard = self.record_stage(
            cmd,
            &self.cdf53_forward_l3,
            &[
                StageBinding::Buffer(self.cdf53_escalation_list.buffer),
                StageBinding::Buffer(self.cdf53_escalation_coefficients_buffer),
            ],
            &[cols, rows],
            StageDispatch::Direct(count, 1, 1),
        )?;

        // Barrier L3 → HOST_READ: Phase B reads escalation coefficient buffer
        // after fence.
        let buf_barrier_esc_l3_host = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.cdf53_escalation_coefficients_buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&buf_barrier_esc_l3_host),
            &[],
        );

        Ok((l1_guard, l2_guard, l3_guard))
    }

    /// Snapshot copy: `current_image` (GENERAL) -> `dst_image`, then restore
    /// `dst_image` to GENERAL for the next frame's SAD pass. Must be recorded
    /// after every shader dispatch that reads `current_image`.
    ///
    /// The steady-state path passes its persistent `prev_image` (GENERAL,
    /// SHADER_READ); the first-frame path passes the freshly allocated
    /// snapshot (UNDEFINED, empty access: no prior access to wait on).
    ///
    /// # Safety
    ///
    /// `cmd` is recording and both images are valid for this device.
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_snapshot_copy(
        &self,
        cmd: vk::CommandBuffer,
        current_image: vk::Image,
        dst_image: vk::Image,
        dst_old_layout: vk::ImageLayout,
        dst_src_access: vk::AccessFlags,
        width: u32,
        height: u32,
    ) {
        let subresource_range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let copy_barriers = [
            // current: SHADER_READ → TRANSFER_READ
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(current_image)
                .subresource_range(subresource_range)
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ),
            // dst: dst_src_access → TRANSFER_WRITE
            vk::ImageMemoryBarrier::default()
                .old_layout(dst_old_layout)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(dst_image)
                .subresource_range(subresource_range)
                .src_access_mask(dst_src_access)
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
            current_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            dst_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &copy_region,
        );

        // Restore the destination to GENERAL for the next frame's SAD pass.
        let post_copy_barrier = [vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(dst_image)
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

    /// Final barriers making every output buffer visible to the host:
    /// SAD, NV12, analysis, palrle compact list + count, Stage 2a outputs,
    /// Stage 2b output and Stage 3 output -> HOST_READ.
    ///
    /// `sad_src_access` is how the paths differ: the steady-state path's SAD
    /// shader wrote the buffer (SHADER_WRITE); the first-frame path
    /// pre-filled it with `cmd_fill_buffer` (TRANSFER_WRITE).
    ///
    /// # Safety
    ///
    /// `cmd` is recording.
    unsafe fn record_host_readback_barriers(
        &self,
        cmd: vk::CommandBuffer,
        nv12_buffer: vk::Buffer,
        sad_src_access: vk::AccessFlags,
        pal_id_bytes: vk::DeviceSize,
    ) {
        let buf_barriers = [
            buf_barrier(
                self.sad_buf.buffer,
                sad_src_access,
                vk::AccessFlags::HOST_READ,
                vk::WHOLE_SIZE,
            ),
            buf_barrier(
                nv12_buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::HOST_READ,
                vk::WHOLE_SIZE,
            ),
            buf_barrier(
                self.analysis_buf.buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::HOST_READ,
                vk::WHOLE_SIZE,
            ),
            buf_barrier(
                self.palrle_compact_list.buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::HOST_READ,
                vk::WHOLE_SIZE,
            ),
            buf_barrier(
                self.palrle_compact_count.buffer,
                vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::HOST_READ,
                vk::WHOLE_SIZE,
            ),
            // Stage 2a outputs
            buf_barrier(
                self.frame_palette_set.buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::HOST_READ,
                vk::WHOLE_SIZE,
            ),
            buf_barrier(
                self.frame_palette_count.buffer,
                vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::HOST_READ,
                4,
            ),
            buf_barrier(
                self.per_tile_frame_palette_id.buffer,
                vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::HOST_READ,
                pal_id_bytes,
            ),
            // Stage 2b output
            buf_barrier(
                self.folded_into.buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::HOST_READ,
                PALETTE_SLOT_U32_BYTES,
            ),
            // Stage 3 output
            buf_barrier(
                self.index.buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::HOST_READ,
                vk::WHOLE_SIZE,
            ),
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
    }

    /// The stages both frame paths record identically, from the palrle_compact
    /// count fill through `pal_rle_index`: Stage 1.5a/b, 4a/b, 4c L1-L3, 4d
    /// escalation, 2a, 2b init, 2b and 3, with their internal barriers and
    /// fills. The caller records everything before (image transition, SAD /
    /// NV12 / analysis, the path-specific `inputs` buffer barrier) and after
    /// (snapshot copy, HOST_READ barrier, submit).
    ///
    /// Each descriptor-set guard is a local until the final struct is built,
    /// so an early `?` unwinds them in reverse declaration order as before.
    ///
    /// # Safety
    ///
    /// Same contract as the `run_*_stage` helpers: `cmd` is recording,
    /// `current_view` is in GENERAL layout, and the returned guards must stay
    /// alive until `wait_for_fences` returns.
    unsafe fn record_shared_stages<'a>(
        &'a self,
        cmd: vk::CommandBuffer,
        current_view: vk::ImageView,
        cols: u32,
        rows: u32,
        escalation_count: u32,
    ) -> Result<SharedStageGuards<'a>, Box<dyn std::error::Error>> {
        // Stage 1.5: zero compact_count, dispatch compact scan, dispatch indirect-args writer.
        self.device
            .cmd_fill_buffer(cmd, self.palrle_compact_count.buffer, 0, 4, 0);

        // Barrier: transfer-write → shader-read/write
        let buf_barrier_fill = buf_barrier(
            self.palrle_compact_count.buffer,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
            4,
        );
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
        let palrle_compact_ds_guard =
            self.run_palrle_compact_stage(cmd, &palrle_compact_push, (cols * rows, 1, 1))?;

        // Barrier between Stage 1.5a and 1.5b: shader write → shader read on count.
        let buf_barrier_count = buf_barrier(
            self.palrle_compact_count.buffer,
            vk::AccessFlags::SHADER_WRITE,
            vk::AccessFlags::SHADER_READ,
            4,
        );
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
        let buf_barrier_args = buf_barrier(
            self.palrle_indirect_args_buf.buffer,
            vk::AccessFlags::SHADER_WRITE,
            vk::AccessFlags::INDIRECT_COMMAND_READ | vk::AccessFlags::SHADER_READ,
            12,
        );
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
        let buf_barrier_cdf53_fill = buf_barrier(
            self.cdf53_compact_count_buffer,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
            4,
        );
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
        let cdf53_compact_ds_guard =
            self.run_cdf53_compact_stage(cmd, &cdf53_compact_push, (cols * rows, 1, 1))?;
        // Barrier between Stage 4a and 4b: shader write → shader read on count.
        let buf_barrier_cdf53_count = buf_barrier(
            self.cdf53_compact_count_buffer,
            vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::SHADER_READ,
            4,
        );
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
        let buf_barrier_cdf53_args = buf_barrier(
            self.cdf53_dispatch_args.buffer,
            vk::AccessFlags::SHADER_WRITE,
            vk::AccessFlags::INDIRECT_COMMAND_READ | vk::AccessFlags::SHADER_READ,
            12,
        );
        // Barrier: compact_list + compact_count → HOST_READ (CPU readback after fence).
        let buf_barrier_cdf53_list_host = buf_barrier(
            self.cdf53_compact_list_buffer,
            vk::AccessFlags::SHADER_WRITE,
            vk::AccessFlags::HOST_READ,
            vk::WHOLE_SIZE,
        );
        let buf_barrier_cdf53_count_host = buf_barrier(
            self.cdf53_compact_count_buffer,
            vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::SHADER_READ,
            vk::AccessFlags::HOST_READ,
            4,
        );
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
        let cdf53_forward_l1_ds_guard =
            self.run_cdf53_forward_l1_stage(cmd, current_view, cols, rows)?;

        // Barrier L1 → L2: coefficient writes from L1 must be visible to L2.
        let buf_barrier_l1_to_l2 = buf_barrier(
            self.cdf53_coefficients_buffer,
            vk::AccessFlags::SHADER_WRITE,
            vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
            vk::WHOLE_SIZE,
        );
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
        let buf_barrier_l2_to_l3 = buf_barrier(
            self.cdf53_coefficients_buffer,
            vk::AccessFlags::SHADER_WRITE,
            vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
            vk::WHOLE_SIZE,
        );
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
        let buf_barrier_l3_host = buf_barrier(
            self.cdf53_coefficients_buffer,
            vk::AccessFlags::SHADER_WRITE,
            vk::AccessFlags::HOST_READ,
            vk::WHOLE_SIZE,
        );
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
        // Stage 4d: cdf53_forward escalation (idle-escalation candidates)
        // ============================================================
        let cdf53_escalation_ds_guards = if escalation_count > 0 {
            let guards = self.run_cdf53_forward_escalation_stages(
                cmd,
                current_view,
                cols,
                rows,
                escalation_count,
            )?;
            // L3 → HOST_READ barrier so Phase B can read after fence.
            let buf_barrier_esc_host = buf_barrier(
                self.cdf53_escalation_coefficients_buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::HOST_READ,
                vk::WHOLE_SIZE,
            );
            self.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[],
                std::slice::from_ref(&buf_barrier_esc_host),
                &[],
            );
            Some(guards)
        } else {
            None
        };

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
        self.device.cmd_fill_buffer(
            cmd,
            self.frame_palette_set.buffer,
            0,
            FRAME_PALETTE_SET_BYTES,
            0,
        );
        self.device
            .cmd_fill_buffer(cmd, self.frame_palette_count.buffer, 0, 4, 0);
        self.device
            .cmd_fill_buffer(cmd, self.hash_table.buffer, 0, PALETTE_SLOT_U32_BYTES, 0);
        self.device.cmd_fill_buffer(
            cmd,
            self.per_tile_frame_palette_id.buffer,
            0,
            pal_id_bytes,
            0,
        );

        // Barrier: zero-fills + compact_list write must be visible to Stage 2a.
        // srcStageMask covers TRANSFER (zero-fills) and COMPUTE_SHADER (compact_list write).
        let stage_2a_input_barriers = [
            buf_barrier(
                self.frame_palette_set.buffer,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
                FRAME_PALETTE_SET_BYTES,
            ),
            buf_barrier(
                self.frame_palette_count.buffer,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
                4,
            ),
            buf_barrier(
                self.hash_table.buffer,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
                PALETTE_SLOT_U32_BYTES,
            ),
            buf_barrier(
                self.per_tile_frame_palette_id.buffer,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
                pal_id_bytes,
            ),
            buf_barrier(
                self.palrle_compact_list.buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::SHADER_READ,
                vk::WHOLE_SIZE,
            ),
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
            buf_barrier(
                self.frame_palette_set.buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::SHADER_READ,
                vk::WHOLE_SIZE,
            ),
            buf_barrier(
                self.frame_palette_count.buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::SHADER_READ,
                4,
            ),
            buf_barrier(
                self.per_tile_frame_palette_id.buffer,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::SHADER_READ,
                pal_id_bytes,
            ),
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
        let stage_2b_init_to_fold_barrier = buf_barrier(
            self.folded_into.buffer,
            vk::AccessFlags::SHADER_WRITE,
            vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
            PALETTE_SLOT_U32_BYTES,
        );
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
        let stage_2b_output_barrier = buf_barrier(
            self.folded_into.buffer,
            vk::AccessFlags::SHADER_WRITE,
            vk::AccessFlags::SHADER_READ,
            PALETTE_SLOT_U32_BYTES,
        );
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
        let pal_rle_index_ds_guard =
            self.run_pal_rle_index_stage(cmd, current_view, &pal_rle_index_push)?;

        Ok(SharedStageGuards {
            palrle_compact_ds_guard,
            palrle_indirect_args_ds_guard,
            cdf53_compact_ds_guard,
            cdf53_indirect_args_ds_guard,
            cdf53_forward_l1_ds_guard,
            cdf53_forward_l2_ds_guard,
            cdf53_forward_l3_ds_guard,
            cdf53_escalation_ds_guards,
            palette_fold_ds_guard,
            palette_subset_fold_init_ds_guard,
            palette_subset_fold_ds_guard,
            pal_rle_index_ds_guard,
            pal_id_bytes,
        })
    }
}

/// Descriptor-set guards of the stages recorded by `record_shared_stages`,
/// fields declared in REVERSE creation order, so that the implicit drop (an
/// error after `record_shared_stages` returned) frees them in the reverse
/// order the pre-refactor locals unwound in. Deliberately has no `Drop`
/// logic of its own on the success path: callers release it explicitly with
/// `release_forward` or `release_reverse` after `wait_for_fences`, matching
/// the free order each path had before the stages were shared.
struct SharedStageGuards<'a> {
    pal_rle_index_ds_guard: ScopedDescriptorSets<'a>,
    palette_subset_fold_ds_guard: ScopedDescriptorSets<'a>,
    palette_subset_fold_init_ds_guard: ScopedDescriptorSets<'a>,
    palette_fold_ds_guard: ScopedDescriptorSets<'a>,
    cdf53_escalation_ds_guards: Option<(
        ScopedDescriptorSets<'a>,
        ScopedDescriptorSets<'a>,
        ScopedDescriptorSets<'a>,
    )>,
    cdf53_forward_l3_ds_guard: ScopedDescriptorSets<'a>,
    cdf53_forward_l2_ds_guard: ScopedDescriptorSets<'a>,
    cdf53_forward_l1_ds_guard: ScopedDescriptorSets<'a>,
    cdf53_indirect_args_ds_guard: ScopedDescriptorSets<'a>,
    cdf53_compact_ds_guard: ScopedDescriptorSets<'a>,
    palrle_indirect_args_ds_guard: ScopedDescriptorSets<'a>,
    palrle_compact_ds_guard: ScopedDescriptorSets<'a>,
    /// Not a guard: the Stage 2a per-tile palette-id buffer size, which the
    /// callers' final HOST_READ barrier reuses.
    pal_id_bytes: vk::DeviceSize,
}

impl SharedStageGuards<'_> {
    /// Free in creation order (the steady-state path's explicit drops).
    fn release_forward(self) {
        let Self {
            palrle_compact_ds_guard,
            palrle_indirect_args_ds_guard,
            cdf53_compact_ds_guard,
            cdf53_indirect_args_ds_guard,
            cdf53_forward_l1_ds_guard,
            cdf53_forward_l2_ds_guard,
            cdf53_forward_l3_ds_guard,
            cdf53_escalation_ds_guards,
            palette_fold_ds_guard,
            palette_subset_fold_init_ds_guard,
            palette_subset_fold_ds_guard,
            pal_rle_index_ds_guard,
            pal_id_bytes: _,
        } = self;
        drop(palrle_compact_ds_guard);
        drop(palrle_indirect_args_ds_guard);
        drop(cdf53_compact_ds_guard);
        drop(cdf53_indirect_args_ds_guard);
        drop(cdf53_forward_l1_ds_guard);
        drop(cdf53_forward_l2_ds_guard);
        drop(cdf53_forward_l3_ds_guard);
        drop(cdf53_escalation_ds_guards);
        drop(palette_fold_ds_guard);
        drop(palette_subset_fold_init_ds_guard);
        drop(palette_subset_fold_ds_guard);
        drop(pal_rle_index_ds_guard);
    }

    /// Free in reverse creation order (the first-frame path's scope-end
    /// order). The escalation triple still frees l1, l2, l3 as a tuple.
    fn release_reverse(self) {
        let Self {
            palrle_compact_ds_guard,
            palrle_indirect_args_ds_guard,
            cdf53_compact_ds_guard,
            cdf53_indirect_args_ds_guard,
            cdf53_forward_l1_ds_guard,
            cdf53_forward_l2_ds_guard,
            cdf53_forward_l3_ds_guard,
            cdf53_escalation_ds_guards,
            palette_fold_ds_guard,
            palette_subset_fold_init_ds_guard,
            palette_subset_fold_ds_guard,
            pal_rle_index_ds_guard,
            pal_id_bytes: _,
        } = self;
        drop(pal_rle_index_ds_guard);
        drop(palette_subset_fold_ds_guard);
        drop(palette_subset_fold_init_ds_guard);
        drop(palette_fold_ds_guard);
        drop(cdf53_escalation_ds_guards);
        drop(cdf53_forward_l3_ds_guard);
        drop(cdf53_forward_l2_ds_guard);
        drop(cdf53_forward_l1_ds_guard);
        drop(cdf53_indirect_args_ds_guard);
        drop(cdf53_compact_ds_guard);
        drop(palrle_indirect_args_ds_guard);
        drop(palrle_compact_ds_guard);
    }
}

/// Read a dma-buf through the CPU, bracketed by `DMA_BUF_IOCTL_SYNC`, and
/// return tightly packed rows (`width * 4` bytes each). Validates geometry and
/// the fd's size first: mapping past the end of a dma-buf is a SIGBUS, not an
/// error.
fn read_dmabuf_tight(
    fd: std::os::unix::io::RawFd,
    width: u32,
    height: u32,
    stride: u32,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let row = u64::from(width) * 4;
    if width == 0 || height == 0 || u64::from(stride) < row {
        return Err(format!("dma-buf mmap: bad geometry {width}x{height} stride {stride}").into());
    }
    let need = u64::from(height - 1) * u64::from(stride) + row;
    if row * u64::from(height) > u64::from(u32::MAX) {
        return Err(format!("dma-buf mmap: {width}x{height} is too large").into());
    }
    // SAFETY: plain libc calls on a caller-owned fd; the mapping is read only
    // within `need` bytes, which is checked against the fd size first.
    unsafe {
        let fd_size = libc::lseek(fd, 0, libc::SEEK_END);
        libc::lseek(fd, 0, libc::SEEK_SET);
        if fd_size < 0 || need > fd_size as u64 {
            return Err(format!("dma-buf mmap: frame needs {need} bytes, fd has {fd_size}").into());
        }
        let len = need as usize;
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if ptr == libc::MAP_FAILED {
            return Err(format!("dma-buf mmap: {}", io::Error::last_os_error()).into());
        }
        // _IOW('b', 0, struct dma_buf_sync{u64 flags}); READ = 1, END = 4.
        // Ignored on failure: memfds (ENOTTY) and some exporters have no sync.
        let sync = |flags: u64| {
            let s = flags;
            libc::ioctl(fd, 0x4008_6200, &s);
        };
        sync(1);
        let mut out = vec![0u8; (row as usize) * height as usize];
        for y in 0..height as usize {
            let src = (ptr as *const u8).add(y * stride as usize);
            std::ptr::copy_nonoverlapping(
                src,
                out.as_mut_ptr().add(y * row as usize),
                row as usize,
            );
        }
        sync(1 | 4);
        libc::munmap(ptr, len);
        Ok(out)
    }
}

/// Bytes a `width` x `height` BGRA frame occupies at `stride`: the last row
/// need not be padded. Computed in u64 (`stride * height` wraps u32 at 4 GiB).
fn dmabuf_frame_bytes(
    width: u32,
    height: u32,
    stride: u32,
) -> Result<u64, Box<dyn std::error::Error>> {
    if width == 0 || height == 0 {
        return Err(format!("dma-buf: empty frame {width}x{height}").into());
    }
    let row = u64::from(width) * 4;
    if u64::from(stride) < row {
        return Err(format!("dma-buf: stride {stride} < width*4 {row}").into());
    }
    Ok(u64::from(height - 1) * u64::from(stride) + row)
}
