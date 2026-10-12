//! Per-frame processing: from a captured `FrameSubmission` to tiles handed to
//! the scheduler or, on the H.264 path, to the emitter.
//!
//! Owns, as logic over `IoBridge` fields that stay in `io_bridge.rs`:
//! - the entry point `process_frame` and its two pipelines, the CPU tile path
//!   (`process_frame_cpu`) and the GPU path (`process_frame_gpu`, kept whole);
//! - the stranded-tile re-enqueue helpers (`stranded_reenqueue`,
//!   `detect_stranded_cdf53_tiles`);
//! - the PalRle phases (`phase_a_palette_allocation`, `phase_b_encode_payloads`,
//!   `phase_b_encode_payloads_with_caps`) and `PalRleTileWorkPrep`;
//! - the one-shot frame-dump path (`take_dump_frame_path`), the GPU metric
//!   write-back (`populate_gpu_metrics`), the compact-list count clamp
//!   (`clamp_gpu_tile_count`) and the `frame.last_send` clock
//!   (`monotonic_now_ns`).
//!
//! Not here: the emission tail (`dispatch_dirty_tiles_via_scheduler`,
//! `drain_scheduler_into_quinn`), which stays in `io_bridge.rs`;
//! `FrameSendStats` and `WT_VARINT_OVERHEAD`, which are shared with it.

use super::*;

/// Bytes a `pacer_rate_bps` rate (bits per second, from
/// `BweSnapshot::pacer_rate_bps`) affords over one tick of `tick_interval_us`
/// microseconds. Pure arithmetic, factored out of `combine_pacing_budget` so
/// it's independently checkable and so `bwe_bench.rs` can pin the unit
/// conversion directly.
/// Clamp a GPU-written compact-list count to the number of tiles on the grid,
/// the most entries the list can legitimately hold. A buggy shader or a
/// device fault must not turn the count into an out-of-bounds slice length.
pub(crate) fn clamp_gpu_tile_count(raw: u32, tiles: usize) -> usize {
    (raw as usize).min(tiles)
}

/// Read CLOCK_MONOTONIC and return nanoseconds since the epoch.
/// Used for the `frame.last_send` telemetry event (and symmetric to the
/// capture-side `capture_done_ns` populated by `xdaemon`).
///
/// # Safety
/// `libc::clock_gettime` is async-signal-safe. The timespec struct is
/// zero-initialised before the call and written atomically by the kernel.
pub(super) fn monotonic_now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime is async-signal-safe; ts is valid.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

/// Write GPU-derived per-tile metrics into the tracker for the dirty tiles.
///
/// Non-dirty tile entries in `tracker` are untouched. If a computed index
/// falls outside `tile_analysis`, that tile is silently skipped — this is
/// what makes the first-frame path safe to call with an empty slice.
/// Caller must ensure every `(tx, ty)` in `dirty` is within the tracker grid.
pub(crate) fn populate_gpu_metrics(
    tracker: &mut crate::tile::MetricsTracker,
    dirty: &[(u32, u32)],
    cols: u32,
    tile_analysis: &[crate::capture::gpu_pipeline::TileAnalysis],
) {
    for &(tx, ty) in dirty {
        let idx = (ty as usize) * (cols as usize) + (tx as usize);
        let Some(entry) = tile_analysis.get(idx) else {
            continue;
        };
        let m = tracker.get_mut(tx, ty);
        m.unique_colors = entry.count.min(u16::MAX as u32) as u16;
        m.edge_density = entry.edge_density_thou as f32 / 1000.0;
    }
}

/// Per-tile staging produced by Phase A, consumed by Phase B.
#[derive(Debug, Clone)]
pub(crate) struct PalRleTileWorkPrep {
    pub tile_xy: (u32, u32),
    /// Owned copy of 512 B index slice. Owned (not borrowed) so Phase B's
    /// rayon parallel iter is `Send`.
    pub indices: [u8; 512],
    pub palette: crate::encoder::pal_rle::PaletteEntry,
    pub palette_id: u8,
    pub bundled: bool,
}

impl IoBridge {
    /// Dispatch to GPU or CPU pipeline depending on whether a DMA-BUF fd is
    /// available and the GPU dirty tracker has been initialized.
    ///
    /// NOTE: `GHOSTFRAME_DUMP_FRAME` only fires on the CPU path. The DRM/DMA-BUF
    /// capture backend sets `frame.pixels = Vec::new()` and puts pixel data
    /// exclusively in the DMA-BUF fd, so there is nothing to dump here without a
    /// DMA-BUF readback (out of scope for M3.2c). If you need a raw-BGRA dump on
    /// the GPU path, implement DMA-BUF readback in `process_frame_gpu` first.
    pub(super) fn process_frame(&mut self, frame: FrameSubmission) {
        // Route to the GPU classifier whenever a processor is available and
        // we have *either* a DMA-BUF fd (zero-copy path) *or* CPU pixels
        // (modesetting-FB fallback uploads them into a HOST_VISIBLE VkImage).
        // Without this, CPU-pixel frames hit `process_frame_cpu` which emits
        // every tile as `Codec::Raw` and never exercises the wavelet path.
        let has_gpu_input = frame.dmabuf_fd.is_some() || !frame.pixels.is_empty();
        let path = if has_gpu_input && self.gpu_frame_processor.is_some() {
            "gpu"
        } else {
            "cpu"
        };
        // Deliberately the real wall clock, not `now_std()`: this measures
        // how long the CPU actually spent inside `process_frame` (encode
        // work), not protocol/deadline time. A virtual-clock timestamp would
        // not advance while the CPU is busy, so it would report ~0us under
        // the paused-clock browserless harness just like the bug this
        // replaces did.
        let t0 = std::time::Instant::now();
        tracing::trace!(
            target: "ghostframe::io_bridge::diag",
            w = frame.width,
            h = frame.height,
            has_dmabuf = frame.dmabuf_fd.is_some(),
            pixels_len = frame.pixels.len(),
            path,
            "[BRIDGE] process_frame ENTER"
        );
        if has_gpu_input && self.gpu_frame_processor.is_some() {
            self.process_frame_gpu(frame);
        } else {
            self.process_frame_cpu(frame);
        }
        #[allow(
            clippy::disallowed_methods,
            reason = "t0 above is deliberately std::time::Instant::now(), not now_std() -- this probe measures real CPU time spent inside process_frame(), which must keep advancing even while the harness's virtual clock is paused"
        )]
        let dt_us = t0.elapsed().as_micros() as u64;
        tracing::trace!(
            target: "ghostframe::io_bridge::diag",
            dt_us,
            path,
            "[BRIDGE] process_frame EXIT"
        );
    }

    /// Takes the one-shot BGRA frame-dump path, leaving `None` behind so a
    /// second call (and hence a second dump) never happens. Replaces the
    /// former read-then-`std::env::remove_var("GHOSTFRAME_DUMP_FRAME")`
    /// pattern, which used the process environment as mutable one-shot
    /// state — exactly what `LibConfig` exists to remove.
    pub fn take_dump_frame_path(&mut self) -> Option<String> {
        self.diagnostics.dump_frame_path.take()
    }

    /// CPU-side tile-based pipeline (original implementation).
    fn process_frame_cpu(&mut self, frame: FrameSubmission) {
        let grid = TileGrid::new(frame.width, frame.height);
        self.frame_seq = self.frame_seq.wrapping_add(1);
        let seq = self.frame_seq;

        // M3.5 bench telemetry — CPU path is always tile codec.
        tracing::info!(
            target: "ghostframe::bench",
            frame_seq = seq,
            capture_done_ns = frame.capture_done_ns,
            mode = "tile",
            "frame.captured"
        );

        // One-shot BGRA frame dump: write raw pixels to the path configured
        // via GHOSTFRAME_DUMP_FRAME, then take it so subsequent frames are
        // not re-dumped.
        //
        // CPU-path only: the DRM/DMA-BUF capture backend (GPU path) always
        // constructs FrameSubmission with `pixels = Vec::new()`, placing pixel
        // data exclusively in the DMA-BUF fd. Hoisting this block to
        // `process_frame()` would therefore write zero bytes on the GPU path.
        // DMA-BUF readback support (to make this work on the GPU path) is
        // deferred beyond M3.2c.
        if let Some(path) = self.take_dump_frame_path() {
            let _ = std::fs::write(&path, &frame.pixels);
            tracing::info!(target: "ghostframe::diagnose", "dumped frame to {}", path);
        }

        // Determine the maximum fragment payload from the smallest connected
        // session's QUIC max datagram size.  Check for connected sessions
        // BEFORE updating the dirty tracker — otherwise the tracker consumes
        // frame state even when no client is listening, and once a client
        // connects the content appears "unchanged" so nothing is ever sent.
        let max_frag = {
            let mut min_size: Option<usize> = None;
            if let Some(handle) = self.live_send_handle() {
                if let Some(conn) = self.server.connections.get_mut(&handle) {
                    if let Some(sz) = conn.datagrams().max_size() {
                        let usable =
                            max_fragment_payload(sz.saturating_sub(Self::WT_VARINT_OVERHEAD));
                        min_size = Some(match min_size {
                            Some(prev) => prev.min(usable),
                            None => usable,
                        });
                    }
                }
            }
            match min_size {
                Some(0) | None => {
                    // No connected sessions or MTU too small — skip frame
                    // without updating dirty tracker state.
                    return;
                }
                Some(sz) => sz,
            }
        };

        // Emit frame-dimensions datagram on first frame, on dimension change,
        // and N more times after any change to absorb datagram loss.
        self.emit_frame_dimensions(seq, frame.timestamp_us, frame.width, frame.height);

        // During QUIC slow-start after a new session connects, datagrams may
        // be silently dropped by congestion control. Use no-commit mode so
        // dropped tiles remain dirty on subsequent frames until the congestion
        // window opens up enough to deliver them all.
        let no_commit = self.force_dirty_frames > 0;
        if no_commit {
            self.force_dirty_frames -= 1;
        }

        // Determine dirty tiles (only after confirming we have a connected session).
        // During slow-start (no_commit), ignore damage hints and do full-frame
        // comparison without committing, so unsent tiles stay dirty.
        //
        // On the first commit frame after slow-start ends, do a full-frame scan
        // regardless of XDamage hints. During no-commit mode prev_tiles is never
        // updated (it stays all-zeros), so switching immediately to hinted mode
        // would only check the animated region (e.g. spinner) and silently skip
        // static regions — leaving solid/text/gradient tiles committed as zeros and
        // never retransmitted. A full scan at the transition commits every tile.
        let dirty_tiles = if no_commit {
            self.dirty_tracker.update_no_commit(
                &frame.pixels,
                frame.stride,
                frame.width,
                frame.height,
            )
        } else if !self.dirty_tracker.has_been_committed() {
            // First commit frame: full scan so every tile is flushed from baseline.
            self.dirty_tracker
                .update(&frame.pixels, frame.stride, frame.width, frame.height)
        } else {
            match &frame.damage_tiles {
                Some(hints) => self.dirty_tracker.update_with_hints(
                    &frame.pixels,
                    frame.stride,
                    frame.width,
                    frame.height,
                    hints,
                ),
                None => self.dirty_tracker.update(
                    &frame.pixels,
                    frame.stride,
                    frame.width,
                    frame.height,
                ),
            }
        };

        if dirty_tiles.is_empty() {
            return;
        }

        // Also sync metrics_tracker grid so it stays aligned with dirty_tracker.
        if self.metrics_tracker.cols() != grid.cols || self.metrics_tracker.rows() != grid.rows {
            self.metrics_tracker.resize(grid.cols, grid.rows);
        }

        // Route through the shared scheduler-dispatch helper. CPU path emits
        // Codec::Raw per M3.1 D1 (classifier sentinel-gated).
        let stats = self.dispatch_dirty_tiles_via_scheduler(
            &dirty_tiles,
            &grid,
            TileDispatchFrame {
                pixels: &frame.pixels,
                stride: frame.stride,
                seq,
                timestamp_us: frame.timestamp_us,
            },
            max_frag,
            SchedulerEmissionPolicy::CpuRawOnly,
            None,
        );

        // M3.5 bench telemetry: timestamp of the last datagram handed to QUIC.
        tracing::info!(
            target: "ghostframe::bench",
            frame_seq = seq,
            last_send_ns = monotonic_now_ns(),
            total_wire_bytes = stats.total_wire_bytes,
            tile_count = stats.tile_count,
            codec_solid = stats.codec_histogram.solid,
            codec_palrle = stats.codec_histogram.palrle,
            codec_cdf53 = stats.codec_histogram.cdf53,
            codec_raw = stats.codec_histogram.raw,
            codec_h264 = stats.codec_histogram.h264,
            "frame.last_send"
        );
    }

    /// Phase 1.5-B stranded-tile re-enqueue: given the freshly re-encoded
    /// sparse `passes` (and their `present_passes` bitmap) for one
    /// escalation slot already known to be Cdf53 (`codec_state`),
    /// re-enqueues ONLY the still-unacked passes at the existing
    /// generation.
    ///
    /// No `cancel_for_tile` (the retransmit cache is empty for a stranded
    /// tile — that's the detection gate), no `bump_generation` (a bump
    /// would discard the client's partial bitmap for already-acked
    /// passes, which this path exists to preserve).
    ///
    /// Guards, in order:
    ///  - `codec_state` must be Cdf53 — anything else means the detection
    ///    gate raced with a codec-state change since candidate collection;
    ///    skip rather than panic.
    ///  - `unacked == 0` — the client ACKed everything between detection
    ///    and dispatch this frame; nothing to resend.
    ///  - `refinement_queue_holds_tile` — the tile's passes are still
    ///    `Pending`/`InFlight` in the refinement queue, i.e. merely
    ///    unsent, not actually stranded. Re-enqueueing here would emit
    ///    the same pass twice on a path whose entire purpose is
    ///    recovering from scarcity, not multiplying unsent work.
    ///    `refinement_queue_holds_tile` was written for exactly this
    ///    check and, until now, was never called.
    pub(super) fn stranded_reenqueue(
        &mut self,
        tile_x: u8,
        tile_y: u8,
        codec_state: crate::tile::CodecState,
        present_passes: u16,
        passes: Vec<(u8, Vec<u8>)>,
    ) {
        if !matches!(codec_state, crate::tile::CodecState::Cdf53 { .. }) {
            return; // Should not happen — detection enforced.
        }
        let gen = self.scheduler.generation_for(tile_x, tile_y);
        let unacked = self
            .scheduler
            .cdf53_unacked_pass_mask(tile_x, tile_y, gen, present_passes);
        if unacked == 0 {
            return; // Race: client ACKed everything between detect and dispatch.
        }
        // Still queued means unsent, not stranded: re-enqueueing here
        // would emit the same pass twice. `refinement_queue_holds_tile`
        // was written for this check and was never called.
        if self.scheduler.refinement_queue_holds_tile(tile_x, tile_y) {
            return;
        }
        let only_unacked: Vec<(u8, Vec<u8>)> = passes
            .into_iter()
            .filter(|(pass_idx, _)| (unacked & (1u16 << pass_idx)) != 0)
            .collect();
        let subset_mask: u16 = only_unacked
            .iter()
            .fold(0u16, |acc, (pass_idx, _)| acc | (1u16 << pass_idx));
        for (pass_idx, payload) in &only_unacked {
            // DEBUG, not INFO: this fires once per pass per tile. A full-screen first
            // frame emits ~9200 of them in ~300 ms, which overruns journald's default
            // RateLimitBurst (10000 per 30 s) and makes it silently discard everything
            // the daemon logs for the rest of the interval -- measured in production as
            // "Suppressed 23232 messages", a 20-second hole that reads exactly like a
            // stalled capture loop and cost a debugging session before the suppression
            // notice was found. The per-frame aggregate stays at INFO on the "dispatch
            // returned" line (codec_cdf53=N), and e2e runs RUST_LOG=ghostframe=trace,debug
            // so it still sees every line.
            tracing::debug!(
                target: "ghostframe::cdf53",
                tile_x = tile_x,
                tile_y = tile_y,
                gen = gen,
                pass_idx = *pass_idx,
                unacked_mask = unacked,
                payload_size = payload.len(),
                source = "stranded",
                "cdf53.emit"
            );
        }
        let payloads: Vec<Vec<u8>> = only_unacked.into_iter().map(|(_, p)| p).collect();
        // Use the index-preserving variant so the scheduler enqueues the
        // correct pass_idx for each retained payload (not 0..N).
        self.scheduler.enqueue_refinement_subset_at(
            tile_x,
            tile_y,
            gen,
            subset_mask,
            payloads,
            now_std(),
        );
    }

    /// Phase 1.5-B detection: Cdf53 tiles, idle past `IDLE_THRESHOLD`, that
    /// the client has not fully acknowledged and that nothing is still
    /// working on. Returns up to `limit` flat tile indices.
    ///
    /// "Nothing is still working on" has two halves, and for a long time
    /// only one was checked here:
    ///
    ///  - the retransmit cache holds no entry for the tile, so no RTO or
    ///    NACK will resend anything; and
    ///  - the refinement queue holds none of its passes, so the scheduler
    ///    is not about to send anything either.
    ///
    /// The second half lived only in `stranded_reenqueue`, i.e. *after* the
    /// candidate had been handed to the GPU for a forward transform and its
    /// coefficients re-encoded on the CPU. A tile whose passes are merely
    /// waiting their turn in the queue -- the normal state of every tile on
    /// a link slower than the screen -- therefore cost a full re-encode per
    /// frame to be told it was not stranded. Measured on a 2040-tile screen
    /// refining over 8 Mbit: capture-to-dispatch went from 0.6 ms to 106 ms
    /// the moment the tiles crossed the idle threshold, the frame rate fell
    /// from 30 to 8, and since the scheduler is ticked per frame, delivery
    /// fell with it.
    ///
    /// Inlined in this file (not in escalation.rs) to avoid a tile/ ->
    /// transport/ layering dependency: the check needs the scheduler's
    /// per-(tile, gen) ACK bitmap and the reliable emitter's cache
    /// occupancy, both transport-layer state.
    pub(super) fn detect_stranded_cdf53_tiles(&self, limit: usize) -> Vec<u32> {
        let mut out = Vec::new();
        let cols = self.metrics_tracker.cols();
        for (idx, m) in self.metrics_tracker.metrics().iter().enumerate() {
            if out.len() >= limit {
                break;
            }
            if m.idle_frames <= crate::tile::escalation::IDLE_THRESHOLD {
                continue;
            }
            let present_passes = match m.codec_state {
                crate::tile::CodecState::Cdf53 { present_passes, .. } => present_passes,
                // Cdf53-only sweep: a tile in any other state has no
                // pass set to refine. Listed so a future progressive
                // codec is not silently skipped here.
                crate::tile::CodecState::PalRle { .. }
                | crate::tile::CodecState::Solid
                | crate::tile::CodecState::H264 { .. }
                | crate::tile::CodecState::PixelPerfect
                | crate::tile::CodecState::Skip => continue,
            };
            // NOTE: TileMetrics.passes_sent is dead state in the current
            // codebase — initialized to 0 at every Cdf53 transition and
            // never incremented — so a `passes_sent == max_passes` gate
            // never fires. The live `reliable_emitter` cache, the scheduler
            // ACK bitmap and the refinement queue are the source of truth.
            let tile_x = (idx as u32 % cols) as u8;
            let tile_y = (idx as u32 / cols) as u8;
            // RTO still has cached entries → it's pumping retransmits, not
            // stranded.
            if self
                .reliable_emitter
                .has_cache_entries_for_tile(tile_x, tile_y)
            {
                continue;
            }
            // Still queued → unsent, not stranded.
            if self.scheduler.refinement_queue_holds_tile(tile_x, tile_y) {
                continue;
            }
            let gen = self.scheduler.generation_for(tile_x, tile_y);
            let unacked =
                self.scheduler
                    .cdf53_unacked_pass_mask(tile_x, tile_y, gen, present_passes);
            if unacked == 0 {
                continue;
            }
            out.push(idx as u32);
        }
        out
    }

    /// GPU-accelerated full-frame pipeline: Vulkan compute dirty detection +
    /// VA-API VPP BGRA→NV12 conversion + H.264 encoding (true zero-copy).
    fn process_frame_gpu(&mut self, frame: FrameSubmission) {
        // Either a DMA-BUF fd (zero-copy import) or CPU pixels (upload via
        // HOST_VISIBLE staging image) must be present — the public dispatcher
        // (`process_frame`) enforces that invariant.
        let raw_fd = frame.dmabuf_fd.as_ref().map(|fd| fd.as_raw_fd());

        self.frame_seq = self.frame_seq.wrapping_add(1);
        let seq = self.frame_seq;

        let max_dg_size = match self.compute_max_datagram_size() {
            Some(sz) => sz,
            None => {
                tracing::trace!(target: LOG_TARGET, seq, "process_frame_gpu: no connected sessions, dropping");
                return;
            }
        };

        // Emit frame-dimensions datagram on first frame, on dimension change,
        // and N more times after any change to absorb datagram loss.
        self.emit_frame_dimensions(seq, frame.timestamp_us, frame.width, frame.height);

        // Per-frame idle-escalation sweep. Computes the candidate list against
        // the previous frame's tile metrics (the current frame's metrics are
        // updated after the GPU work returns), then hands the list to the
        // GpuFrameProcessor which records the forward dispatch in the same
        // command buffer as the dirty-Cdf53 forward stages.
        //
        // Note: metrics_tracker.resize() happens after process_frame returns
        // (below, around `self.metrics_tracker.resize(cols, rows)`). On the
        // very first frame the tracker has cols=0, rows=0 (constructed via
        // MetricsTracker::new(0, 0)), so detect_escalation_candidates iterates
        // 0 tiles and returns empty — safe.
        self.collect_escalation_candidates();

        // GPU pipeline: Vulkan SAD dirty detection + NV12 conversion.
        // Dispatch to the DMA-BUF path if a fd was provided; otherwise upload
        // the CPU pixel buffer via the HOST_VISIBLE staging-image variant.
        let processor = self.gpu_frame_processor.as_mut().unwrap();
        let processor_result = match raw_fd {
            Some(fd) => processor.process_frame(fd, frame.width, frame.height, frame.stride),
            None => processor.process_frame_from_pixels(
                &frame.pixels,
                frame.width,
                frame.height,
                frame.stride,
            ),
        };
        let analysis = match processor_result {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(target: LOG_TARGET, "GPU process_frame failed: {e}, falling back to CPU path");
                self.process_frame_cpu(frame);
                return;
            }
        };

        tracing::debug!(
            target: LOG_TARGET,
            seq,
            dirty_count = analysis.dirty_tiles.len(),
            "process_frame_gpu: dirty tile detection complete"
        );

        // GPU pipeline diagnostic: one line per frame showing the state of
        // every output buffer FrameAnalysis exposes. Drives W1 root-cause
        // investigation when unique_colors stays at UNIQUE_COLORS_UNKNOWN.
        if self.diagnostics.diagnose_gpu_pipeline {
            let ta_slice = analysis.tile_analysis_slice();
            let first_nonzero = ta_slice
                .iter()
                .take(2048)
                .enumerate()
                .find(|(_, t)| t.count != 0)
                .map(|(i, t)| (i, t.count, t.edge_density_thou));
            tracing::info!(
                target: "ghostframe::diagnose",
                seq,
                dirty_count = analysis.dirty_tiles.len(),
                tile_analysis_is_null = analysis.tile_analysis.is_null(),
                tile_analysis_len = analysis.tile_analysis_len,
                tile_analysis_slice_len = ta_slice.len(),
                first_nonzero_idx = first_nonzero.map(|(i, _, _)| i),
                first_nonzero_count = first_nonzero.map(|(_, c, _)| c),
                first_nonzero_edge_thou = first_nonzero.map(|(_, _, e)| e),
                palrle_compact_count = analysis.palrle_compact_count,
                frame_palette_set_count = analysis.frame_palette_set_count,
                "gpu-pipeline"
            );
        }

        // Convert flat tile indices (Vec<u32>) into (tile_x, tile_y) pairs
        // matching the row-major layout used by MetricsTracker / TileGrid.
        let cols = frame.width.div_ceil(crate::tile::TILE_SIZE);
        let rows = frame.height.div_ceil(crate::tile::TILE_SIZE);
        let (dirty_xy, prev_mode, new_mode) =
            self.classify_dirty_tiles(&analysis, &frame, cols, rows, seq);
        use crate::tile::FrameMode;

        // ---- M3.3c: PixelPerfect transition sweep ----
        // Runs every frame, unconditionally, before the dirty-tile early return.
        // This ensures that ACKs arriving during static phases (where dirty_xy is
        // empty and the function would otherwise return immediately) still drive
        // the Cdf53 → PixelPerfect transition.
        //
        // Phase B (enqueue_refinement) runs inside the FrameMode::TileCodec arm
        // BELOW, so on dirty frames the sweep runs before new Cdf53 work is
        // enqueued. This is correct: after Phase B bumps the generation and clears
        // the ACK counter, the new generation needs subsequent frames' ACKs to
        // accumulate before tile_fully_acked returns true — so running the sweep
        // here (before Phase B) vs. after Phase B yields no difference in practice.
        self.pixelperfect_sweep(cols, rows, seq);

        // Early-return on empty dirty is mode-specific:
        //  - H264: no new frame to encode, nothing to emit. Bail.
        //  - TileCodec: the scheduler's refinement queue may still hold
        //    un-drained pass-N work from earlier dirty frames (cdf53's
        //    first-frame paint emits 14 progressive passes per tile that
        //    pace out over many frames under the A+B-light budget). We
        //    must keep calling `dispatch_dirty_tiles_via_scheduler` to
        //    drain it — even when there's no new dirty work — or the
        //    queue stagnates and the wizard never finishes refining.
        match new_mode {
            FrameMode::H264 => {
                if dirty_xy.is_empty() {
                    return;
                }
                // Lazily initialize full-frame encoder
                let needs_init = match &self.full_frame_encoder {
                    Some(enc) => enc.width() != frame.width || enc.height() != frame.height,
                    None => true,
                };
                if needs_init {
                    match FullFrameEncoder::new(frame.width, frame.height) {
                        Ok(enc) => {
                            self.full_frame_encoder = Some(enc);
                        }
                        Err(e) => {
                            tracing::warn!(target: LOG_TARGET, "Full-frame encoder init failed: {e}");
                            return;
                        }
                    }
                }

                // Re-entry into H264: force IDR for a fresh client anchor.
                if prev_mode == FrameMode::TileCodec {
                    if let Some(enc) = self.full_frame_encoder.as_mut() {
                        enc.request_keyframe();
                    }
                }

                // Encode from the GPU-computed NV12 HOST_VISIBLE buffer
                let encoder = self.full_frame_encoder.as_mut().unwrap();
                let encoded = match encoder.encode_nv12_buffer(
                    analysis.nv12_data,
                    analysis.nv12_width,
                    analysis.nv12_height,
                    analysis.nv12_y_stride,
                    analysis.nv12_uv_stride,
                    analysis.nv12_uv_offset,
                ) {
                    Ok(Some(enc)) => enc,
                    Ok(None) => {
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(target: LOG_TARGET, "NV12 encode failed: {e}");
                        return;
                    }
                };

                // Fragment and send. `fragment_frame` chunks the payload at the
                // per-fragment payload limit (datagram size minus the frame
                // header), not the raw datagram size — otherwise the on-wire
                // datagram (header + chunk) overshoots the MTU and WebTransport
                // returns TooLarge, dropping every H.264 datagram silently.
                // And the limit leaves room for the parity's own header, or
                // it is the parity that overshoots instead.
                let datagrams = fragment_frame(
                    seq,
                    frame.timestamp_us,
                    encoded.is_keyframe,
                    &encoded.payload,
                    max_fec_frame_fragment_payload(max_dg_size),
                );

                self.cumulative_datagrams_emitted.h264 = self
                    .cumulative_datagrams_emitted
                    .h264
                    .saturating_add(datagrams.len() as u64);

                for dg in &datagrams {
                    self.send_to_all_sessions(dg);
                }

                // FEC parity
                let fec_k = fec_group_size(encoded.is_keyframe);
                if datagrams.len() > 1 {
                    let source_payloads: Vec<&[u8]> = datagrams
                        .iter()
                        .map(|dg| &dg[FRAME_HEADER_SIZE..])
                        .collect();
                    let parities = fec::generate_parity(&source_payloads, fec_k);
                    for (_group_start, parity_payload) in &parities {
                        let parity_dg = build_frame_parity_datagram(
                            seq,
                            frame.timestamp_us,
                            encoded.is_keyframe,
                            datagrams.len() as u16,
                            parity_payload,
                        );
                        self.send_to_all_sessions(&parity_dg);
                    }
                }

                // Store fragments for NACK
                let oldest_kept = seq.wrapping_sub(3);
                self.recent_frame_fragments
                    .retain(|(s, _), _| s.wrapping_sub(oldest_kept) <= 3);
                for dg in &datagrams {
                    if let Ok(hdr) = FrameHeader::decode(dg) {
                        self.recent_frame_fragments
                            .insert((hdr.frame_seq, hdr.frag_idx), dg.clone());
                    }
                }

                // M3.5 bench telemetry: H264 frame completed — all fragments sent.
                let h264_wire_bytes: u32 = datagrams.iter().map(|dg| dg.len() as u32).sum();
                tracing::info!(
                    target: "ghostframe::bench",
                    frame_seq = seq,
                    last_send_ns = monotonic_now_ns(),
                    total_wire_bytes = h264_wire_bytes,
                    tile_count = 0u16,
                    codec_solid = 0u16,
                    codec_palrle = 0u16,
                    codec_cdf53 = 0u16,
                    codec_raw = 0u16,
                    codec_h264 = 1u8,
                    "frame.last_send"
                );
            }
            FrameMode::TileCodec => {
                let max_frag = max_fragment_payload(max_dg_size);
                if max_frag == 0 {
                    return;
                }

                // Source for tile pixel extraction. CPU path populates
                // `frame.pixels`; GPU/DMA-BUF leaves it empty.
                let pixels_owned;
                let pixels: &[u8] = if frame.pixels.is_empty() {
                    match frame.dmabuf_fd.as_ref() {
                        Some(fd) => match crate::capture::dmabuf::readback_dmabuf(
                            fd.as_raw_fd(),
                            frame.width,
                            frame.height,
                            frame.stride,
                        ) {
                            Ok(p) => {
                                pixels_owned = p;
                                &pixels_owned
                            }
                            Err(e) => {
                                tracing::warn!(
                                    target: LOG_TARGET,
                                    "DMA-BUF readback failed in TileCodec mode: {e}; \
                                     skipping frame to avoid emitting zero-filled tiles",
                                );
                                return;
                            }
                        },
                        None => {
                            tracing::warn!(
                                target: LOG_TARGET,
                                "TileCodec mode but no pixels and no DMA-BUF fd; skipping frame",
                            );
                            return;
                        }
                    }
                } else {
                    &frame.pixels
                };

                let grid = TileGrid::new(frame.width, frame.height);

                // Phase A — serial palette-table allocation per dirty PalRle-feasible tile.
                let cols = grid.cols;
                let palrle_compact_count = analysis.palrle_compact_count;
                let preps = self.phase_a_palette_allocation(
                    cols,
                    analysis.palrle_compact_list_slice(),
                    analysis.per_tile_frame_palette_id_slice(),
                    analysis.folded_into_slice(),
                    analysis.frame_palette_set_slice(),
                    // No bulk slice helper exists for index_buffer (only per-tile); keep inline.
                    if palrle_compact_count > 0 && !analysis.index_buffer.is_null() {
                        unsafe {
                            std::slice::from_raw_parts(
                                analysis.index_buffer,
                                (palrle_compact_count * 512) as usize,
                            )
                        }
                    } else {
                        &[]
                    },
                );

                // Phase B — rayon parallel encode.
                let caps = self.client_caps;
                #[cfg(any(test, feature = "test-loss-injection"))]
                let inject = self.oob_inject_at;
                #[cfg(not(any(test, feature = "test-loss-injection")))]
                let inject: Option<(u32, u32)> = None;
                // Only consume the one-shot injection when the target tile
                // is actually in this frame's preps — otherwise the hook
                // would be silently spent on the first PalRle frame whose
                // preps happen to exclude the injection target.
                #[cfg(any(test, feature = "test-loss-injection"))]
                let inject_will_fire = inject
                    .map(|xy| preps.iter().any(|p| p.tile_xy == xy))
                    .unwrap_or(false);
                let mut palrle_payloads =
                    IoBridge::phase_b_encode_payloads_with_caps(&preps, &caps, inject);
                #[cfg(any(test, feature = "test-loss-injection"))]
                if inject_will_fire {
                    self.oob_inject_at = None;
                }

                // Phase B — Cdf53 encode bit-plane passes for high-color tiles,
                // enqueue into the scheduler's refinement queue.
                {
                    // Extract raw pointers from the GPU processor up-front so that
                    // the immutable borrow of self.gpu_frame_processor ends before
                    // the mutable borrows of self.scheduler below.
                    // SAFETY: cdf53_compact_count_ptr, cdf53_compact_list_ptr, and
                    // cdf53_coefficients_ptr are HOST_VISIBLE | HOST_COHERENT mapped
                    // GPU memory, valid until the next process_frame call. This block
                    // runs inside the current process_frame_gpu invocation, before
                    // any subsequent call.
                    let (cdf53_compact_count_ptr, cdf53_compact_list_ptr, cdf53_coefficients_ptr) = {
                        let gpu = self.gpu_frame_processor.as_ref().unwrap();
                        (
                            gpu.cdf53_compact_count_ptr,
                            gpu.cdf53_compact_list_ptr,
                            gpu.cdf53_coefficients_ptr,
                        )
                    };
                    // The count is written by the GPU; clamp it to the list's
                    // capacity (one slot per tile) before it sizes a slice.
                    let cdf53_count = clamp_gpu_tile_count(
                        unsafe { *cdf53_compact_count_ptr },
                        (grid.cols * grid.rows) as usize,
                    );
                    if cdf53_count > 0 {
                        let cdf53_list = unsafe {
                            std::slice::from_raw_parts(cdf53_compact_list_ptr, cdf53_count)
                        };
                        for (slot, &flat_tile_idx) in cdf53_list.iter().enumerate() {
                            // Also GPU-written: an index off the grid would
                            // wrap in the u8 casts below and address the
                            // wrong tile.
                            if flat_tile_idx >= grid.cols * grid.rows {
                                continue;
                            }
                            let tile_x = (flat_tile_idx % cols) as u8;
                            let tile_y = (flat_tile_idx / cols) as u8;
                            // Read 3 * 1024 = 3072 i32 values from the compact-slot
                            // indexed coefficient buffer. The GPU writes i32-wide
                            // values; CPU casts to i16 (actual coefficient bit-width;
                            // high bits are sign-extension).
                            let coeffs_i32 = unsafe {
                                std::slice::from_raw_parts(
                                    cdf53_coefficients_ptr
                                        .add(slot * crate::encoder::cdf53::CDF53_TOTAL_COEFFS),
                                    crate::encoder::cdf53::CDF53_TOTAL_COEFFS,
                                )
                            };
                            let coeffs_i16: Vec<i16> =
                                coeffs_i32.iter().map(|&v| v as i16).collect();
                            // M3.3b diagnostic: when GHOSTFRAME_CDF53_DIFF_TILE=X,Y is
                            // set, log a one-shot side-by-side comparison of GPU vs
                            // CPU `cdf53::forward(tile_bgra)` for that tile. Gated
                            // behind the `cdf53-diag` cargo feature so production
                            // builds skip the config check entirely.
                            #[cfg(feature = "cdf53-diag")]
                            if let Some((target_x, target_y)) = self.diagnostics.cdf53_diff_tile {
                                if tile_x as u32 == target_x && tile_y as u32 == target_y {
                                    let tile_bgra = grid.extract_tile(
                                        pixels,
                                        frame.stride,
                                        tile_x as u32,
                                        tile_y as u32,
                                    );
                                    // Pick CPU reference based on which dispatches were skipped:
                                    // SKIP_L2_L3 → L1-only; SKIP_L3 → L2-only; otherwise full.
                                    let skip_l2_l3 = self.diagnostics.cdf53_skip_l2_l3;
                                    let skip_l3 = self.diagnostics.cdf53_skip_l3;
                                    let cpu_coeffs = if skip_l2_l3 {
                                        crate::encoder::cdf53::forward_level1_only(&tile_bgra)
                                    } else if skip_l3 {
                                        crate::encoder::cdf53::forward_level2_only(&tile_bgra)
                                    } else {
                                        crate::encoder::cdf53::forward(&tile_bgra)
                                    };
                                    // B↔R swap hypothesis: build a fake tile where B and R are
                                    // swapped in the BGRA buffer, then CPU-forward it. If the GPU
                                    // result matches THIS, the shader's rgba8/bgra8 mismatch is
                                    // swapping channel 0 (B) with channel 2 (R).
                                    let mut tile_swapped = tile_bgra.clone();
                                    for px in tile_swapped.chunks_exact_mut(4) {
                                        px.swap(0, 2); // swap B and R bytes
                                    }
                                    let cpu_coeffs_swapped =
                                        crate::encoder::cdf53::forward(&tile_swapped);
                                    // Sample tile pixels (first 8 BGRA px).
                                    let bgra_head: Vec<u8> =
                                        tile_bgra.iter().take(32).copied().collect();
                                    tracing::info!(
                                        target: "ghostframe::cdf53::diff",
                                        tile_x = tile_x,
                                        tile_y = tile_y,
                                        "cdf53.diff.tile_bgra_head_32 = {:?}",
                                        bgra_head
                                    );
                                    let mut n_diff_per_ch = [0usize; 3];
                                    let mut n_match_swapped_per_ch = [0usize; 3];
                                    let mut shown = 0usize;
                                    for i in 0..crate::encoder::cdf53::CDF53_TOTAL_COEFFS {
                                        let ch = i / 1024;
                                        if coeffs_i16[i] != cpu_coeffs[i] {
                                            n_diff_per_ch[ch] += 1;
                                            if shown < 100 {
                                                tracing::info!(
                                                    target: "ghostframe::cdf53::diff",
                                                    tile_x = tile_x,
                                                    tile_y = tile_y,
                                                    "cdf53.diff coef[{i}] ch={} idx={} gpu={} cpu={} diff={} cpu_swap={}",
                                                    ch,
                                                    i % 1024,
                                                    coeffs_i16[i],
                                                    cpu_coeffs[i],
                                                    coeffs_i16[i] as i32 - cpu_coeffs[i] as i32,
                                                    cpu_coeffs_swapped[i],
                                                );
                                                shown += 1;
                                            }
                                        }
                                        if coeffs_i16[i] == cpu_coeffs_swapped[i] {
                                            n_match_swapped_per_ch[ch] += 1;
                                        }
                                    }
                                    let n_diff = n_diff_per_ch.iter().sum::<usize>();
                                    // Per-subband mismatches (channel 0 / B only — quickest signal).
                                    // Subband ranges in per-channel layout:
                                    //   [0..16] LL3, [16..32] HL3, [32..48] LH3, [48..64] HH3,
                                    //   [64..128] HL2, [128..192] LH2, [192..256] HH2,
                                    //   [256..512] HL1, [512..768] LH1, [768..1024] HH1.
                                    // Per-channel per-subband breakdown. Subband boundaries
                                    // are within each channel's 1024-coefficient block.
                                    let mut sb_diff = [[0usize; 10]; 3];
                                    let band_ranges = [
                                        (0, 16),
                                        (16, 32),
                                        (32, 48),
                                        (48, 64),
                                        (64, 128),
                                        (128, 192),
                                        (192, 256),
                                        (256, 512),
                                        (512, 768),
                                        (768, 1024),
                                    ];
                                    for ch in 0..3 {
                                        let base = ch * 1024;
                                        for (b, (lo, hi)) in band_ranges.iter().enumerate() {
                                            for i in *lo..*hi {
                                                if coeffs_i16[base + i] != cpu_coeffs[base + i] {
                                                    sb_diff[ch][b] += 1;
                                                }
                                            }
                                        }
                                    }
                                    tracing::info!(
                                        target: "ghostframe::cdf53::diff",
                                        tile_x = tile_x,
                                        tile_y = tile_y,
                                        "cdf53.diff.summary total_diffs={n_diff}/3072 per_ch_diff={:?} per_ch_match_swapped={:?}",
                                        n_diff_per_ch,
                                        n_match_swapped_per_ch,
                                    );
                                    tracing::info!(
                                        target: "ghostframe::cdf53::diff",
                                        tile_x = tile_x,
                                        tile_y = tile_y,
                                        "cdf53.diff.subbands (LL3 HL3 LH3 HH3 HL2 LH2 HH2 HL1 LH1 HH1): ch0={:?} ch1={:?} ch2={:?}",
                                        sb_diff[0], sb_diff[1], sb_diff[2],
                                    );
                                }
                            }
                            let (present_passes, passes) =
                                crate::encoder::cdf53::encode_passes_sparse(&coeffs_i16);
                            // Drop emitter retransmit state in lock-step (see
                            // dispatch_dirty_tiles_via_scheduler for the
                            // dangling-pointer rationale).
                            self.reliable_emitter.cancel_for_tile(tile_x, tile_y);
                            let gen = self.scheduler.bump_generation(tile_x, tile_y);
                            self.bump_count_accumulator.cdf53_dirty_path = self
                                .bump_count_accumulator
                                .cdf53_dirty_path
                                .saturating_add(1);
                            // (Old generation Cdf53 in-flight coverage is now
                            // dropped via the `reliable_emitter.cancel_for_tile`
                            // call above; the fragment_coverage drop here
                            // became redundant when Task 25 paired emitter
                            // cancellation with every bump_generation site.)
                            // New generation: tile is eligible for re-escalation.
                            self.metrics_tracker
                                .get_mut(tile_x as u32, tile_y as u32)
                                .already_escalated_this_gen = false;
                            // Diagnostic: one line per pass actually sent (sparse
                            // encoding skips empty bit-planes) for the e2e log scan.
                            for (pass_idx, payload) in &passes {
                                // DEBUG, not INFO: this fires once per pass per tile. A full-screen first
                                // frame emits ~9200 of them in ~300 ms, which overruns journald's default
                                // RateLimitBurst (10000 per 30 s) and makes it silently discard everything
                                // the daemon logs for the rest of the interval -- measured in production as
                                // "Suppressed 23232 messages", a 20-second hole that reads exactly like a
                                // stalled capture loop and cost a debugging session before the suppression
                                // notice was found. The per-frame aggregate stays at INFO on the "dispatch
                                // returned" line (codec_cdf53=N), and e2e runs RUST_LOG=ghostframe=trace,debug
                                // so it still sees every line.
                                tracing::debug!(
                                    target: "ghostframe::cdf53",
                                    tile_x = tile_x,
                                    tile_y = tile_y,
                                    gen = gen,
                                    pass_idx = *pass_idx,
                                    payload_size = payload.len(),
                                    source = "dirty",
                                    "cdf53.emit"
                                );
                            }
                            let payloads: Vec<Vec<u8>> =
                                passes.into_iter().map(|(_, p)| p).collect();
                            self.scheduler.enqueue_refinement_subset_at(
                                tile_x,
                                tile_y,
                                gen,
                                present_passes,
                                payloads,
                                now_std(),
                            );
                            // Override the CPU classifier's codec_state so that
                            // dispatch_dirty_tiles_via_scheduler's pre-check can
                            // correctly identify this tile as Cdf53 and skip the
                            // priority-queue enqueue (which would otherwise supersede
                            // the refinement passes just enqueued above).
                            // The CPU classifier produces Cdf53 (via Rules 3/5/8); the GPU compact
                            // list and coefficient buffer are authoritative for Cdf53 emission.
                            self.metrics_tracker
                                .get_mut(tile_x as u32, tile_y as u32)
                                .codec_state = crate::tile::CodecState::Cdf53 {
                                passes_sent: 0,
                                present_passes,
                            };
                        }
                    }
                }

                // ---- M3.3c + Phase 1.5-B: post-fence Phase B for escalation candidates ----
                // Runs after the GPU fence (the existing fence covers all
                // compute work in this command buffer, including the
                // escalation forward dispatches recorded above). Reads
                // escalation coefficients per-slot, encodes 14 passes,
                // enqueues into the scheduler.
                //
                // Two flavors live in the same merged list (stranded first,
                // then H264):
                //  - Stranded (Phase 1.5-B): tiles already in Cdf53 state
                //    whose retransmit cache drained but the client never
                //    ACKed every pass. Re-emit ONLY the unacked passes at
                //    the EXISTING generation (no `cancel_for_tile`, no
                //    `bump_generation`) so the client's already-delivered
                //    passes don't get discarded by a fresh-gen reset.
                //  - H264 (M3.3c): tiles in H264 state being promoted to
                //    Cdf53 for the first time on this generation.
                let escalation_candidates =
                    std::mem::take(&mut self.cdf53_escalation_candidates_this_frame);
                let stranded_candidates =
                    std::mem::take(&mut self.cdf53_stranded_candidates_this_frame);
                let stranded_count = stranded_candidates.len();
                if !escalation_candidates.is_empty() {
                    let gpu_coeffs_ptr = {
                        let gpu = self.gpu_frame_processor.as_ref().unwrap();
                        gpu.cdf53_escalation_coefficients_ptr
                    };
                    for (slot, &flat_tile_idx) in escalation_candidates.iter().enumerate() {
                        let tile_x = (flat_tile_idx % cols) as u8;
                        let tile_y = (flat_tile_idx / cols) as u8;
                        let is_stranded = slot < stranded_count;
                        let codec_state = self
                            .metrics_tracker
                            .get(tile_x as u32, tile_y as u32)
                            .codec_state;
                        if !is_stranded {
                            // H264-escalation slot: skip if the dirty path
                            // already promoted this tile to Cdf53 in this
                            // SAME frame. Re-firing would bump again,
                            // supersede the just-enqueued passes, emit 14
                            // duplicate cdf53.emit lines, and waste a GPU
                            // forward dispatch with the same coefficients.
                            if matches!(codec_state, crate::tile::CodecState::Cdf53 { .. }) {
                                continue;
                            }
                        }
                        let coeffs_i32 = unsafe {
                            std::slice::from_raw_parts(
                                gpu_coeffs_ptr
                                    .add(slot * crate::encoder::cdf53::CDF53_TOTAL_COEFFS),
                                crate::encoder::cdf53::CDF53_TOTAL_COEFFS,
                            )
                        };
                        let coeffs_i16: Vec<i16> = coeffs_i32.iter().map(|&v| v as i16).collect();
                        let (present_passes, passes) =
                            crate::encoder::cdf53::encode_passes_sparse(&coeffs_i16);
                        if is_stranded {
                            self.stranded_reenqueue(
                                tile_x,
                                tile_y,
                                codec_state,
                                present_passes,
                                passes,
                            );
                            continue;
                        }
                        // H264 escalation path (existing).
                        // Drop emitter retransmit state in lock-step (see
                        // dispatch_dirty_tiles_via_scheduler for the
                        // dangling-pointer rationale).
                        self.reliable_emitter.cancel_for_tile(tile_x, tile_y);
                        let gen = self.scheduler.bump_generation(tile_x, tile_y);
                        self.bump_count_accumulator.cdf53_escalation_path = self
                            .bump_count_accumulator
                            .cdf53_escalation_path
                            .saturating_add(1);
                        // (Old generation Cdf53 in-flight coverage is now
                        // dropped via the `reliable_emitter.cancel_for_tile`
                        // call above; the fragment_coverage drop here
                        // became redundant when Task 25 paired emitter
                        // cancellation with every bump_generation site.)
                        for (pass_idx, payload) in &passes {
                            // DEBUG, not INFO: this fires once per pass per tile. A full-screen first
                            // frame emits ~9200 of them in ~300 ms, which overruns journald's default
                            // RateLimitBurst (10000 per 30 s) and makes it silently discard everything
                            // the daemon logs for the rest of the interval -- measured in production as
                            // "Suppressed 23232 messages", a 20-second hole that reads exactly like a
                            // stalled capture loop and cost a debugging session before the suppression
                            // notice was found. The per-frame aggregate stays at INFO on the "dispatch
                            // returned" line (codec_cdf53=N), and e2e runs RUST_LOG=ghostframe=trace,debug
                            // so it still sees every line.
                            tracing::debug!(
                                target: "ghostframe::cdf53",
                                tile_x = tile_x,
                                tile_y = tile_y,
                                gen = gen,
                                pass_idx = *pass_idx,
                                payload_size = payload.len(),
                                source = "escalation",
                                "cdf53.emit"
                            );
                        }
                        let payloads: Vec<Vec<u8>> = passes.into_iter().map(|(_, p)| p).collect();
                        self.scheduler.enqueue_refinement_subset_at(
                            tile_x,
                            tile_y,
                            gen,
                            present_passes,
                            payloads,
                            now_std(),
                        );

                        let tm = self.metrics_tracker.get_mut(tile_x as u32, tile_y as u32);
                        tm.codec_state = crate::tile::CodecState::Cdf53 {
                            passes_sent: 0,
                            present_passes,
                        };
                        tm.already_escalated_this_gen = true;
                    }
                }

                #[cfg(feature = "cdf53-diag")]
                if self.diagnostics.cdf53_dump_pending {
                    // Queued passes *and* emitted-but-unacknowledged ones.
                    // This used to pass an empty slice for the second, which
                    // left the snapshot showing only what had not been sent
                    // yet -- a set that empties within a frame or two of
                    // being filled, whether or not a bump cancelled
                    // anything. `e2e_refinement_cancel` kept passing only
                    // because slow tile frames pushed the server into H.264,
                    // where this dump stops and its last line stays put.
                    let in_flight = self.fragment_coverage.snapshot();
                    let snap = self.scheduler.pending_refinement_snapshot(&in_flight);
                    tracing::info!(
                        target: "ghostframe::cdf53::diff",
                        "cdf53.pending_snapshot {:?}",
                        snap
                    );
                }

                // Diagnostic: pre-dispatch announce.
                //
                // DEBUG, not INFO. This fires once per captured frame -- ~29
                // times a second, forever, including on a screen with nothing
                // dirty and nothing to send. Paired with "dispatch returned"
                // below it was two of the six log lines a live idle session
                // wrote per frame (~207 lines/second measured), each costing
                // formatting plus a journald write, and contributing to the
                // RateLimitBurst exhaustion that silently swallowed 23232
                // messages during a first-frame burst.
                //
                // Nothing consumes these two lines: the bench harness parses
                // `frame.captured` / `frame.last_send` (still INFO), and the
                // only other reference is a code comment. The per-frame
                // aggregate a reader actually wants -- codec counts and wire
                // bytes -- is on the `cumulative emit` line, which stays INFO
                // and fires once every 30 frames.
                tracing::debug!(
                    target: LOG_TARGET,
                    frame_seq = seq,
                    dirty_tiles = dirty_xy.len(),
                    priority_queue_len = self.scheduler.queue_len(),
                    refinement_deficit_tiles = self.scheduler.refinement_deficit_tiles(),
                    max_frag = max_frag,
                    "process_frame_gpu: about to dispatch"
                );

                // Real CPU time, deliberately not `now_std()`: this is what
                // the frame cost the encode thread, which a paused tokio
                // clock would report as zero.
                let dispatch_started = std::time::Instant::now();
                let stats = self.dispatch_dirty_tiles_via_scheduler(
                    &dirty_xy,
                    &grid,
                    TileDispatchFrame {
                        pixels,
                        stride: frame.stride,
                        seq,
                        timestamp_us: frame.timestamp_us,
                    },
                    max_frag,
                    SchedulerEmissionPolicy::GpuClassifierDriven,
                    Some(&mut palrle_payloads),
                );

                // Per-frame RTO sweep: fire any cached retransmits whose
                // RTO deadline has elapsed, then drain the resulting
                // queue (source + parity) to the wire. Cheap when nothing
                // is due (no cache hits, no parity ready).
                self.sweep_rto_retransmits();

                // Diagnostic: post-dispatch summary. If THIS fires but
                // frame.last_send below does not, something in the palrle
                // stats log block is silencing execution. If neither fires,
                // dispatch_dirty_tiles_via_scheduler is panicking/returning
                // through an unwind.
                // `dispatch_us` is here because its absence hid a quadratic:
                // tile mode was spending 83 ms a frame on bookkeeping for
                // tiles the cost model prices at 0.05 us each, and nothing
                // in the log said where a frame's time went.
                let dispatch_us = std::time::Instant::now()
                    .duration_since(dispatch_started)
                    .as_micros() as u64;
                tracing::debug!(
                    target: LOG_TARGET,
                    frame_seq = seq,
                    dispatch_us = dispatch_us,
                    dispatched_tiles = stats.tile_count,
                    wire_bytes = stats.total_wire_bytes,
                    codec_solid = stats.codec_histogram.solid,
                    codec_palrle = stats.codec_histogram.palrle,
                    codec_cdf53 = stats.codec_histogram.cdf53,
                    codec_raw = stats.codec_histogram.raw,
                    codec_h264 = stats.codec_histogram.h264,
                    refinement_deficit_after = self.scheduler.refinement_deficit_tiles(),
                    send_datagram_errs_total = self.datagram_send_errs,
                    "process_frame_gpu: dispatch returned"
                );

                // ---- Cumulative emit + color-histogram diagnostics ----
                //
                // Throttled to once per `CUMULATIVE_EMIT_LOG_INTERVAL_FRAMES`
                // (~2 s at 30 fps) so the line lines up with the client's
                // periodic `stats:` log. The numbers are directly comparable
                // to the client's `rx{r:.. s:.. p:.. c:.. h:..}` totals:
                //   - if (emitted - send_errs) == client.rx for every codec,
                //     the wire is clean — any missing tile is server-side
                //     (never emitted because the scheduler queue was empty
                //     or the classifier picked a different codec for it).
                //   - if (emitted - send_errs) >  client.rx, the gap is
                //     UDP-level packet loss between the server's
                //     `wt.send_datagram(...)` and the browser's
                //     WebTransport reader.
                //   - if send_errs > 0, we have QUIC backpressure — the new
                //     buffer-space cap (commit 66486b1) should keep this at
                //     0 in steady state, so any non-zero value is a regression.
                if seq.wrapping_sub(self.last_cumulative_emit_log_frame)
                    >= CUMULATIVE_EMIT_LOG_INTERVAL_FRAMES
                {
                    self.last_cumulative_emit_log_frame = seq;
                    let c = self.cumulative_datagrams_emitted;
                    let es = self.reliable_emitter.stats;
                    tracing::info!(
                        target: LOG_TARGET,
                        frame_seq = seq,
                        emitted_solid = c.solid,
                        emitted_palrle = c.palrle,
                        emitted_cdf53 = c.cdf53,
                        emitted_raw = c.raw,
                        emitted_h264 = c.h264,
                        send_datagram_errs_total = self.datagram_send_errs,
                        fec_parity_emitted = es.parity_emitted,
                        rto_fired = es.rto_fired,
                        rto_max_retransmits_reached = es.rto_max_retransmits_reached,
                        // Split, not summed. This was logged as a single
                        // `nack_received = nack_hit + nack_miss`, which cannot
                        // distinguish "the client asked and we resent" from
                        // "the client asked and we had nothing to send" --
                        // and the second is the one that leaves a tile stuck
                        // at a partial pass set for the rest of the session.
                        //
                        // `on_nack` treats a cache miss as `nack_miss += 1;
                        // continue`, i.e. the request is dropped silently. A
                        // client that keeps sweeping for the same pass and a
                        // server whose cache no longer holds it will sit in
                        // that loop until the client gives up, with nothing
                        // in the log to say so.
                        nack_received = es.nack_hit + es.nack_miss,
                        nack_hit = es.nack_hit,
                        nack_miss = es.nack_miss,
                        // How many of those hits only landed because the
                        // cache was searched by tile-pass instead of by the
                        // `frame_seq` the client sent. A coverage NACK names
                        // a pass the client never received, so it has no
                        // frame to echo; on a refinement-heavy session this
                        // should be most of `nack_hit`. Reading zero while
                        // `nack_miss` climbs means the content index is not
                        // being maintained, which is the shape that left
                        // tiles stranded without their finest bit-planes.
                        nack_resolved_by_content = es.nack_resolved_by_content,
                        nack_deduped = es.nack_deduped,
                        retransmit_budget_deferrals = es.retransmit_budget_deferrals,
                        // Step A's counter: datagrams quinn refused. Never
                        // surfaced before, and it is the difference between
                        // "we never sent it" and "we sent it and it was lost".
                        send_rejected = es.send_rejected,
                        // `emitter_` prefix so `e2e_ack_telemetry_no_waste`'s
                        // `ack_miss` log-line filter doesn't false-match this
                        // structured field — the test counts per-ACK debug
                        // events, not the aggregate counter.
                        emitter_ack_hits = es.ack_hit,
                        emitter_ack_misses = es.ack_miss,
                        continuation_resumes = self.continuation_resumes,
                        continuation_drained_bytes = self.continuation_drained_bytes,
                        cache_lru_eviction = self.reliable_emitter.cache.stats.lru_eviction,
                        retransmit_attempts_total = es.retransmit_attempts_total,
                        // Per-window snapshot of un-ACKed tile-passes still in
                        // the emitter cache awaiting RTO retry. Steady-state
                        // ~0 when the link is healthy; persistent >0 means
                        // the long-tail un-deliverable passes that the
                        // client should be NACKing.
                        cache_pending_entries = self.reliable_emitter.pending_cache_entries(),
                        // Sustained growth here means acknowledgements are not
                        // retiring content the normal way -- the sweep is a
                        // backstop, not the intended mechanism.
                        stuck_entries_retired = es.stuck_entries_retired,
                        // Ledger stats. These existed but were never logged,
                        // which left the field unable to tell the two halves
                        // of an ACK failure apart: `emitter_ack_hits` frozen
                        // could mean no ACK datagrams are arriving at all, or
                        // that they arrive and resolve to nothing. An ACK that
                        // fails to resolve is filtered out before `on_ack`, so
                        // it lands in `ledger_unknown_acks` and in NEITHER
                        // emitter counter -- invisible without this line.
                        //
                        // ledger_capacity_evictions > 0 means transmissions
                        // outran acknowledgements badly enough that the cap,
                        // not the horizon, bound the ledger; every evicted
                        // record is an ACK that can no longer land, which
                        // strands its cache entry permanently.
                        ledger_records = self.transmission_ledger.len(),
                        ledger_unknown_acks = self.transmission_ledger.stats().unknown_acks,
                        ledger_capacity_evictions =
                            self.transmission_ledger.stats().capacity_evictions,
                        ledger_acked_after_declared_lost =
                            self.transmission_ledger.stats().acked_after_declared_lost,
                        ledger_tombstones = self.transmission_ledger.tombstone_len(),
                        // BWE Stage 2.0: per-tier ACK round-trip latency
                        // (emit -> ACK receipt, server clock only — see
                        // `TierLatencyStats`'s doc comment). Cumulative
                        // since startup, same semantics as the byte
                        // counters above. This is the baseline the pacer
                        // (Stage 2) is measured against: pass 0-3 latency
                        // should drop without pass 4-13 throughput
                        // regressing.
                        critical_latency_count = self.critical_latency_stats.count,
                        critical_latency_mean_us = self.critical_latency_stats.mean_us(),
                        critical_latency_max_us = self.critical_latency_stats.max_us,
                        // Bucket counts for `[0-5, 5-10, 10-20, 20-50,
                        // 50-100, 100-200, 200-500, 500+]` ms (see
                        // `LATENCY_BUCKET_BOUNDS_MS`), populated since
                        // startup but never logged until now -- a mean
                        // alone hid exactly the distribution shape (29 ms
                        // mean, 103 ms max) that made a fixed 40 ms RTO
                        // deadline fire constantly.
                        critical_latency_buckets = ?self.critical_latency_stats.buckets,
                        refinement_latency_count = self.refinement_latency_stats.count,
                        refinement_latency_mean_us = self.refinement_latency_stats.mean_us(),
                        refinement_latency_max_us = self.refinement_latency_stats.max_us,
                        refinement_latency_buckets = ?self.refinement_latency_stats.buckets,
                        // Current RTO policy inputs/output (see
                        // `rto::rto_for_attempt`): the rolling p95 this
                        // window's ack_deadline was derived from, and the
                        // first-attempt deadline that derivation currently
                        // yields.
                        // -1 distinguishes "no measurement yet" from a
                        // genuinely tiny p95 in the log, the same distinction
                        // the Option carries in the code.
                        rto_ack_latency_p95_us = self
                            .ack_latency_tracker
                            .p95()
                            .map(|d| d.as_micros() as i64)
                            .unwrap_or(-1),
                        rto_first_attempt_deadline_us =
                            crate::transport::reliable_emitter::rto::rto_for_attempt(
                                self.ack_latency_tracker.p95(),
                                0,
                            )
                            .as_micros() as u64,
                        // BWE Stage 2.1: same population, measured from
                        // `queued_at` (when the work became available to
                        // the scheduler) instead of `last_sent_at`. This is
                        // the interval scheduler prioritisation actually
                        // shortens; always >= the corresponding
                        // `*_latency_*_us` field above for the same sample.
                        queued_critical_latency_count = self.queued_critical_latency_stats.count,
                        queued_critical_latency_mean_us =
                            self.queued_critical_latency_stats.mean_us(),
                        queued_critical_latency_max_us = self.queued_critical_latency_stats.max_us,
                        queued_refinement_latency_count =
                            self.queued_refinement_latency_stats.count,
                        queued_refinement_latency_mean_us =
                            self.queued_refinement_latency_stats.mean_us(),
                        queued_refinement_latency_max_us =
                            self.queued_refinement_latency_stats.max_us,
                        // The per-tick emission budget and the floor it is
                        // clamped against. These were only ever logged at
                        // `debug!`, on a per-frame site, which the xdaemon's
                        // `ghostframe=info` default filter drops -- so a
                        // deployed build could not show whether the budget
                        // was tracking the path or pinned at its floor.
                        //
                        // That distinction is the whole point of the floor
                        // change: a flat 256 KiB floor is 7.86 MB/s and beat
                        // the bandwidth term on every link below ~70 Mbit/s,
                        // making the estimate dead code. If
                        // `base_budget_bytes` here equals
                        // `tick_budget_floor_bytes`, the estimate is still
                        // not binding and the same failure has returned.
                        //
                        // Logged on the existing 60-frame cadence rather
                        // than per frame: journald's RateLimitBurst drops a
                        // burst wholesale and leaves a hole that reads as a
                        // stalled loop, which has already cost one
                        // misdiagnosis here.
                        base_budget_bytes = self.base_budget_bytes(),
                        tick_budget_floor_bytes = self.tick_budget_floor(),
                        bytes_per_us = self.adaptation_context.bytes_per_us,
                        smoothed_rtt_us = self.adaptation_context.smoothed_rtt_us,
                        "cumulative emit (datagrams handed to quinn since startup, per codec)"
                    );

                    // Per-window bump counts. Each bump_generation supersedes
                    // the prior gen's still-Pending refinement passes. If
                    // these stay non-zero after the initial burst, repeated
                    // bumping explains the client-side "no tile reaches 14
                    // passes" plateau — the refinement queue empties to
                    // ALL Superseded entries before any single gen's
                    // 14-pass burst finishes draining.
                    let b = self.bump_count_accumulator;
                    let refinement_queue_len = self.scheduler.refinement_queue_len();
                    tracing::info!(
                        target: LOG_TARGET,
                        frame_seq = seq,
                        bumps_priority_path = b.priority_path,
                        bumps_cdf53_dirty_path = b.cdf53_dirty_path,
                        bumps_cdf53_escalation_path = b.cdf53_escalation_path,
                        refinement_queue_len = refinement_queue_len,
                        "bump counts (since last cumulative log) + scheduler refinement queue depth"
                    );
                    self.bump_count_accumulator = BumpCountAccumulator::default();

                    if self.diagnostics.diagnose_color_hist {
                        let h = self.color_histogram_accumulator;
                        tracing::info!(
                            target: LOG_TARGET,
                            frame_seq = seq,
                            colors_1 = h.bucket_1,
                            colors_2_to_4 = h.bucket_2_4,
                            colors_5_to_16 = h.bucket_5_16,
                            colors_overflow_17plus = h.bucket_overflow,
                            colors_unknown_sentinel = h.bucket_unknown,
                            "unique-colors histogram over dirty tiles \
                             since last cumulative log (1=Solid eligible, \
                             2..16=PalRle eligible, 17=Cdf53 forced)"
                        );
                        self.color_histogram_accumulator = UniqueColorHistogram::default();
                    }
                }

                // ---- Phase 1 Task 9: periodic BWE snapshot log ----
                // Fires at most once per ~1 s wall-clock. Outputs the
                // estimator's current bandwidth estimate alongside the
                // per-tier emit rates (derived from snapshot deltas of
                // the cumulative byte counters added in Task 6). All
                // values are observability — Phase 1 doesn't drive
                // emission from any of this.
                let now = now_std();
                let bwe_log_due = self
                    .bwe_log_last_at
                    .is_none_or(|last| now.duration_since(last).as_millis() >= 1000);
                if bwe_log_due {
                    let prev_at = self.bwe_log_last_at;
                    self.bwe_log_last_at = Some(now);
                    let bwe_snap = self.bwe.snapshot();
                    let (prev_crit, prev_refn) = self.bytes_emitted_snapshot;
                    // Compute dt against the previous log's instant — on
                    // the very first log, fall back to a sensible window
                    // (1 s) so the first rate isn't divided by zero.
                    let dt_ms = match prev_at {
                        Some(p) => now.duration_since(p).as_millis().max(1) as u64,
                        None => 1000,
                    };
                    let bps_crit = (self.bytes_emitted_critical.saturating_sub(prev_crit))
                        .saturating_mul(8)
                        .saturating_mul(1000)
                        / dt_ms;
                    let bps_refn = (self.bytes_emitted_refinement.saturating_sub(prev_refn))
                        .saturating_mul(8)
                        .saturating_mul(1000)
                        / dt_ms;
                    self.bytes_emitted_snapshot =
                        (self.bytes_emitted_critical, self.bytes_emitted_refinement);
                    tracing::info!(
                        target: "ghostframe::bwe",
                        bwe_estimate_bps = bwe_snap.bitrate_bps,
                        bwe_samples_seen = bwe_snap.samples_seen,
                        // BWE Stage 2.2 observability: which budget source
                        // is actually governing emission right now, and
                        // what goog_cc's pacer is asking for (0 = no
                        // pacer_config absorbed yet).
                        pacing_mode = ?select_pacing_mode(bwe_snap.samples_seen),
                        pacer_rate_bps = bwe_snap.pacer_rate_bps.unwrap_or(0),
                        bps_critical = bps_crit,
                        bps_refinement = bps_refn,
                        cumulative_bytes_critical = self.bytes_emitted_critical,
                        cumulative_bytes_refinement = self.bytes_emitted_refinement,
                        "bwe periodic snapshot"
                    );
                }

                // Frame stats — emit per design Section 4.
                tracing::debug!(
                    target: "palrle.frame",
                    reused_or_allocated = self.palette_table.stats_frame.reused_or_allocated,
                    fell_back_to_raw = self.palette_table.stats_frame.fell_back_to_raw,
                    unique_frame_palettes = analysis.frame_palette_set_count,
                    "palrle frame stats"
                );
                self.palette_table.stats_frame = Default::default();

                // M3.5 bench telemetry: timestamp of the last datagram handed to QUIC.
                tracing::info!(
                    target: "ghostframe::bench",
                    frame_seq = seq,
                    last_send_ns = monotonic_now_ns(),
                    total_wire_bytes = stats.total_wire_bytes,
                    tile_count = stats.tile_count,
                    codec_solid = stats.codec_histogram.solid,
                    codec_palrle = stats.codec_histogram.palrle,
                    codec_cdf53 = stats.codec_histogram.cdf53,
                    codec_raw = stats.codec_histogram.raw,
                    codec_h264 = stats.codec_histogram.h264,
                    "frame.last_send"
                );
            }
        }
    }

    /// Per-frame idle-escalation sweep and stranded-tile detection; stores the
    /// merged candidate list on the bridge and hands it to the GPU processor.
    fn collect_escalation_candidates(&mut self) {
        let _caps = self.client_caps;
        // Gate on self.frame_mode (the PREVIOUS frame's mode). Skipping
        // escalation while the previous frame was H264 avoids ~5 ms of
        // wasted GPU forward + 6 MB of HOST_COHERENT writes per frame:
        // the post-fence Phase B that consumes the coefficients lives
        // inside the FrameMode::TileCodec arm below, so under H264 the
        // forward dispatch's output is silently dropped.
        //
        // One-frame-late on the H264->TileCodec transition: prev_mode is
        // still H264 on the transition frame, so escalation fires on the
        // SECOND TileCodec frame. Acceptable — refinement queries run
        // every frame and there are plenty of cycles to escalate.
        let frame_mode_eligible = matches!(self.frame_mode, crate::tile::FrameMode::TileCodec);
        let h264_candidates = if frame_mode_eligible {
            crate::tile::detect_escalation_candidates(
                &self.metrics_tracker,
                crate::capture::gpu_pipeline::MAX_ESCALATION_PER_FRAME,
            )
        } else {
            Vec::new()
        };
        // Phase 1.5-B: detect stranded CDF53 tiles whose retransmit
        // cache has drained but the client hasn't ACKed every pass.
        // These need re-emission at the EXISTING generation (no
        // bump). Dispatched by Phase B based on `codec_state ==
        // Cdf53`. Inlined here (not in escalation.rs) to avoid a
        // tile/ → transport/ layering dependency: the eligibility
        // check needs the scheduler's per-(tile, gen) ACK bitmap
        // and the reliable_emitter's cache occupancy, both
        // transport-layer state.
        let stranded_candidates = if frame_mode_eligible {
            self.detect_stranded_cdf53_tiles(
                crate::capture::gpu_pipeline::MAX_ESCALATION_PER_FRAME
                    .saturating_sub(h264_candidates.len()),
            )
        } else {
            Vec::new()
        };
        // Merge: stranded first (more time-sensitive — client has
        // been waiting > 30 frames), then H264 escalation.
        let mut candidates = Vec::with_capacity(stranded_candidates.len() + h264_candidates.len());
        candidates.extend(&stranded_candidates);
        candidates.extend(&h264_candidates);
        candidates.truncate(crate::capture::gpu_pipeline::MAX_ESCALATION_PER_FRAME);
        let processor = self.gpu_frame_processor.as_mut().unwrap();
        processor.set_escalation_candidates(&candidates);
        self.cdf53_escalation_candidates_this_frame = candidates;
        self.cdf53_stranded_candidates_this_frame = stranded_candidates;
    }

    /// Dirty-tile bookkeeping, per-tile classification and frame-mode decision
    /// for one GPU frame. Returns `(dirty_xy, prev_mode, new_mode)`.
    fn classify_dirty_tiles(
        &mut self,
        analysis: &crate::capture::gpu_pipeline::FrameAnalysis,
        frame: &FrameSubmission,
        cols: u32,
        rows: u32,
        seq: u32,
    ) -> (
        Vec<(u32, u32)>,
        crate::tile::FrameMode,
        crate::tile::FrameMode,
    ) {
        let dirty_xy: Vec<(u32, u32)> = analysis
            .dirty_tiles
            .iter()
            .map(|&idx| (idx % cols, idx / cols))
            .collect();

        // Keep metrics_tracker AND scheduler grids in sync with dirty-detection
        // (before the early return on empty dirty_xy, so the scheduler stays
        // consistent even when no tiles are dispatched this frame).
        if self.metrics_tracker.cols() != cols || self.metrics_tracker.rows() != rows {
            self.metrics_tracker.resize(cols, rows);
            self.scheduler.resize(cols, rows);
        }
        // Always update per-tile metrics — idle_frames advances on every frame,
        // EMA decays toward 0 when tiles aren't dirty.
        self.metrics_tracker.record_frame(&dirty_xy);

        // Populate GPU-derived metrics (unique_colors, edge_density) for the dirty
        // tiles from the tile_analysis buffer. The classifier rule for PalRLE
        // (and Solid, post-M3.1) reads these.
        populate_gpu_metrics(
            &mut self.metrics_tracker,
            &dirty_xy,
            cols,
            analysis.tile_analysis_slice(),
        );

        // Classify each dirty tile. Empty dirty_xy → empty tentative — that's
        // the no-motion signal the classifier needs to exit H264 after exit_sustain.
        use crate::tile::{classifier::classify_tile, FrameMode};
        let tentative: Vec<crate::tile::CodecState> = dirty_xy
            .iter()
            .map(|&(tx, ty)| {
                let m = self.metrics_tracker.get(tx, ty);
                let prev = m.codec_state;
                classify_tile(m, &prev)
            })
            .collect();

        // Capture previous mode before evaluating the classifier, so the
        // keyframe-request guard below can compare prev vs. new.
        let prev_mode = self.frame_mode;

        // Always evaluate mode — empty tentative drives the exit-sustain counter.
        let deficit = self.scheduler.refinement_deficit_tiles();
        self.classifier.set_refinement_deficit_tiles(deficit);
        // `decide_frame_mode` (no `_at`) reads `Classifier`'s own
        // `Instant::now()`-seeded epoch via `.elapsed()` — a 14th time
        // source invisible to this file, and one that saturates to 0 (or
        // drifts independently of frame counts) under the browserless
        // harness's paused clock. Route the dwell timers through the
        // bridge's own `now_std()` epoch instead, so `enter_sustain_micros`
        // / `exit_sustain_micros` advance with virtual time exactly like
        // every other deadline in this file.
        let now_us = now_std().duration_since(self.epoch).as_micros() as u64;
        let new_mode = self
            .classifier
            .decide_frame_mode_at(now_us, &tentative, prev_mode);

        // Persist tentative state back into per-tile metrics for dirty tiles only.
        for (i, &(tx, ty)) in dirty_xy.iter().enumerate() {
            self.metrics_tracker.get_mut(tx, ty).codec_state = tentative[i];
        }

        // Accumulate the per-dirty-tile unique-color histogram. Cheap
        // (one u32 increment per tile, no allocation), so always-on; the
        // log emission is gated by the env var, and the accumulator is
        // reset there. See `DiagnosticsConfig::from_lookup`'s doc comment for
        // the motivation (verify the "is the textured background really
        // > 16 colors per 32×32 tile?" hypothesis).
        for &(tx, ty) in &dirty_xy {
            let uc = self.metrics_tracker.get(tx, ty).unique_colors;
            if uc == crate::tile::UNIQUE_COLORS_UNKNOWN {
                self.color_histogram_accumulator.bucket_unknown = self
                    .color_histogram_accumulator
                    .bucket_unknown
                    .saturating_add(1);
            } else if uc == 1 {
                self.color_histogram_accumulator.bucket_1 =
                    self.color_histogram_accumulator.bucket_1.saturating_add(1);
            } else if uc <= 4 {
                self.color_histogram_accumulator.bucket_2_4 = self
                    .color_histogram_accumulator
                    .bucket_2_4
                    .saturating_add(1);
            } else if uc <= 16 {
                self.color_histogram_accumulator.bucket_5_16 = self
                    .color_histogram_accumulator
                    .bucket_5_16
                    .saturating_add(1);
            } else {
                self.color_histogram_accumulator.bucket_overflow = self
                    .color_histogram_accumulator
                    .bucket_overflow
                    .saturating_add(1);
            }
        }

        // Per-tile diagnostic tracing: emit one log line per dirty tile when
        // GHOSTFRAME_DIAGNOSE_TILES=1 (or =true).  Parseable by downstream awk.
        if self.diagnostics.diagnose_tiles {
            for &(tx, ty) in &dirty_xy {
                let m = self.metrics_tracker.get(tx, ty);
                tracing::info!(
                    target: "ghostframe::diagnose",
                    tile_x = tx,
                    tile_y = ty,
                    unique_colors = m.unique_colors,
                    codec_state = ?m.codec_state,
                    "per-tile"
                );
            }
        }

        if new_mode != self.frame_mode {
            tracing::info!(
                target: LOG_TARGET,
                prev = ?self.frame_mode,
                new = ?new_mode,
                seq,
                "classifier flipped frame mode"
            );

            // H264 → TileCodec architectural invariant: invalidate the GPU SAD
            // baseline so the next TileCodec frame re-emits every tile,
            // overwriting whatever the H.264 phase rendered. Without this,
            // static content produces 0 dirty tiles on mode entry (prev_image
            // is current relative to the static screen) and the lossless tile
            // codecs never get a chance to upgrade the canvas from the H.264
            // lossy render. Symmetric to the existing `request_keyframe()` on
            // TileCodec → H264 in the `FrameMode::H264` match arm below: each
            // direction of the mode flip resets the state that the entering mode
            // relies on (H.264 GOP for the H264 direction, GPU SAD baseline for
            // the TileCodec direction).
            //
            // force_frames = 1 (not 20) because we're past QUIC slow-start by
            // the time exit_sustain elapses; ACK-based retries handle
            // individual datagram drops on the lossless tiles.
            if prev_mode == FrameMode::H264 && new_mode == FrameMode::TileCodec {
                if let Some(p) = self.gpu_frame_processor.as_mut() {
                    p.invalidate_baseline(1);
                    tracing::info!(
                        target: LOG_TARGET,
                        seq,
                        "H264→TileCodec handoff: invalidate_baseline(1) — lossless repaint"
                    );
                }
            }
        }

        // Update frame mode for next frame's hysteresis reference.
        self.frame_mode = new_mode;

        // M3.5 bench telemetry — emit after classifier picks the mode for this frame.
        // target: "ghostframe::bench" lets the bench filter independently of the
        // default ghostframe:: target.
        {
            let mode_str = match new_mode {
                FrameMode::H264 => "h264",
                FrameMode::TileCodec => "tile",
            };
            tracing::info!(
                target: "ghostframe::bench",
                frame_seq = seq,
                capture_done_ns = frame.capture_done_ns,
                mode = mode_str,
                "frame.captured"
            );
        }
        (dirty_xy, prev_mode, new_mode)
    }

    /// Cdf53 -> PixelPerfect transition sweep over every tile of the grid.
    fn pixelperfect_sweep(&mut self, cols: u32, rows: u32, seq: u32) {
        let tile_count = (cols * rows) as usize;
        let diag_frame = seq.is_multiple_of(10);
        let mut sweep_acked_total: u32 = 0;
        let mut sweep_present_total: u32 = 0;
        let mut sweep_tile_count: u32 = 0;
        for idx in 0..tile_count {
            let tile_x = (idx as u32 % cols) as u8;
            let tile_y = (idx as u32 / cols) as u8;
            let present_passes = {
                let tm = self.metrics_tracker.get(tile_x as u32, tile_y as u32);
                match tm.codec_state {
                    crate::tile::CodecState::Cdf53 { present_passes, .. } => present_passes,
                    // Cdf53-only sweep: a tile in any other state has no
                    // pass set to refine. Listed so a future progressive
                    // codec is not silently skipped here.
                    crate::tile::CodecState::PalRle { .. }
                    | crate::tile::CodecState::Solid
                    | crate::tile::CodecState::H264 { .. }
                    | crate::tile::CodecState::PixelPerfect
                    | crate::tile::CodecState::Skip => continue,
                }
            };
            let gen = self.scheduler.generation_for(tile_x, tile_y);
            let acked = self.scheduler.cdf53_passes_acked_count(tile_x, tile_y, gen);
            sweep_acked_total = sweep_acked_total.saturating_add(acked as u32);
            sweep_present_total = sweep_present_total.saturating_add(present_passes.count_ones());
            sweep_tile_count = sweep_tile_count.saturating_add(1);
            if self
                .scheduler
                .tile_fully_acked(tile_x, tile_y, gen, present_passes)
            {
                self.metrics_tracker
                    .get_mut(tile_x as u32, tile_y as u32)
                    .codec_state = crate::tile::CodecState::PixelPerfect;
                // Diagnostic: emit one info log line per transition so
                // e2e tests can verify the lossless terminal state was
                // reached without requiring the cdf53-diag feature gate.
                tracing::info!(
                    target: "ghostframe::bench",
                    tile_x = tile_x,
                    tile_y = tile_y,
                    gen = gen,
                    "cdf53.pixelperfect"
                );
            }
        }
        // One per-sweep summary in place of the previous ~1.7 k per-tile
        // DEBUG trace lines. Keeps the per-sweep ACK-progress signal
        // that e2e / journal inspection uses, at <0.1% of the log
        // volume.
        if diag_frame && sweep_tile_count > 0 {
            tracing::debug!(
                target: "ghostframe::cdf53",
                cdf53_tiles = sweep_tile_count,
                acked_total = sweep_acked_total,
                present_total = sweep_present_total,
                "cdf53.sweep_summary"
            );
        }
    }

    /// Phase A: serially walk the GPU's compact list, map each tile's
    /// frame-local palette id to a persistent slot via PaletteTable, and
    /// produce per-tile work preps for Phase B.
    pub(crate) fn phase_a_palette_allocation(
        &mut self,
        cols: u32,
        compact_list: &[u32],
        per_tile_frame_palette_id: &[u8],
        folded_into: &[u32], // 256 entries
        frame_palette_set: &[crate::capture::gpu_pipeline::FramePaletteEntryRaw],
        index_buffer: &[u8], // compact_list.len() * 512 bytes
    ) -> Vec<PalRleTileWorkPrep> {
        use crate::encoder::pal_rle::PaletteEntry;
        use crate::tile::CodecState;

        let mut preps: Vec<PalRleTileWorkPrep> = Vec::with_capacity(compact_list.len());
        for (c, &tile_idx) in compact_list.iter().enumerate() {
            let tx = tile_idx % cols;
            let ty = tile_idx / cols;

            // Honour the classifier's actual decision.
            if !matches!(
                self.metrics_tracker.get(tx, ty).codec_state,
                CodecState::PalRle { .. }
            ) {
                continue;
            }

            let pal_id_local = per_tile_frame_palette_id[c];
            if pal_id_local == 0xFF {
                // Stage 2a sentinel — frame-palette-set overflow.
                self.metrics_tracker.get_mut(tx, ty).codec_state = CodecState::Skip;
                self.palette_table.stats_frame.fell_back_to_raw += 1;
                continue;
            }
            let effective = (folded_into[pal_id_local as usize] & 0xFF) as usize;
            let raw = &frame_palette_set[effective];
            let mut palette = PaletteEntry {
                count: raw.count as u8,
                ..Default::default()
            };
            for i in 0..palette.count as usize {
                let v = raw.colors[i];
                palette.colors[i] = [
                    (v & 0xFF) as u8,
                    ((v >> 8) & 0xFF) as u8,
                    ((v >> 16) & 0xFF) as u8,
                    ((v >> 24) & 0xFF) as u8,
                ];
            }

            match self.palette_table.acquire_or_allocate(&palette) {
                Some(id) => {
                    let bundled = !self.palette_table.delivered.contains(id);
                    if bundled {
                        self.palette_table.in_flight_carrying[id as usize] += 1;
                    }
                    let mut indices = [0u8; 512];
                    indices.copy_from_slice(&index_buffer[c * 512..(c + 1) * 512]);
                    self.metrics_tracker.get_mut(tx, ty).codec_state =
                        CodecState::PalRle { palette_id: id };
                    self.palette_table.stats_frame.reused_or_allocated += 1;
                    preps.push(PalRleTileWorkPrep {
                        tile_xy: (tx, ty),
                        indices,
                        palette,
                        palette_id: id,
                        bundled,
                    });
                }
                None => {
                    self.metrics_tracker.get_mut(tx, ty).codec_state = CodecState::Skip;
                    self.palette_table.stats_frame.fell_back_to_raw += 1;
                }
            }
        }
        preps
    }

    /// Phase B: rayon-parallel per-tile encode of PalRle payloads.
    /// Returns a HashMap keyed by (tile_x, tile_y) for Phase C lookup.
    ///
    /// Legacy entry point — equivalent to `phase_b_encode_payloads_with_caps`
    /// with default (no-capabilities) client. Kept for callers that haven't
    /// threaded capabilities through yet.
    // Kept as a thin wrapper for tests that exercise the no-caps path; production calls phase_b_encode_payloads_with_caps directly.
    #[allow(dead_code)]
    pub(crate) fn phase_b_encode_payloads(
        preps: &[PalRleTileWorkPrep],
    ) -> std::collections::HashMap<(u32, u32), Vec<u8>> {
        Self::phase_b_encode_payloads_with_caps(
            preps,
            &crate::transport::client_caps::ClientCapabilities::default(),
            None,
        )
    }

    /// Phase B with explicit client capabilities. When
    /// `caps.indices_raw_enabled == true`, thin payloads are emitted as
    /// `indices_raw` (flags bit 1) instead of nibble-RLE. Bundled payloads
    /// always use the bundled format regardless of caps.
    ///
    /// `inject_at` is an optional cfg-gated test hook: when the tile at the
    /// matching `(tile_x, tile_y)` is encoded, its payload is replaced with a
    /// hand-built bundled payload whose single RLE byte references palette
    /// index 2 against a count=1 palette. The client GPU shader detects the
    /// OOB and writes `error_code = 5 (ERR_INDEX_OOB)` to its per-tile error
    /// slot. The wire format is documented in `palrle-codec-design.md`.
    pub(crate) fn phase_b_encode_payloads_with_caps(
        preps: &[PalRleTileWorkPrep],
        caps: &crate::transport::client_caps::ClientCapabilities,
        inject_at: Option<(u32, u32)>,
    ) -> std::collections::HashMap<(u32, u32), Vec<u8>> {
        use rayon::prelude::*;
        preps
            .par_iter()
            .map(|p| {
                let mut payload = if !p.bundled && caps.indices_raw_enabled {
                    tracing::info!(
                        target: "palrle.wire",
                        palette_id = p.palette_id,
                        tile_x = p.tile_xy.0,
                        tile_y = p.tile_xy.1,
                        "indices_raw emitted"
                    );
                    crate::encoder::pal_rle::encode_pal_rle_payload_indices_raw(
                        &p.indices,
                        p.palette_id,
                    )
                } else {
                    crate::encoder::pal_rle::encode_pal_rle_payload(
                        &p.indices,
                        &p.palette,
                        p.palette_id,
                        p.bundled,
                    )
                };
                if let Some(inject) = inject_at {
                    if p.tile_xy == inject {
                        // Hand-built bundled payload: flags=0x01 (bundled),
                        // palette_id=0, count=1, one BGRA red entry, then 64
                        // RLE bytes of 0x2F (index=2, run_len=16) covering
                        // all 1024 tile pixels. Passes client prevalidation
                        // (which insists on exactly 1024 pixels) so the GPU
                        // compute shader actually runs and detects the OOB
                        // (color_idx=2 >= count=1), writing error_code=5.
                        let mut p_inj = Vec::with_capacity(7 + 64);
                        p_inj.extend_from_slice(&[0x01u8, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xFF]);
                        p_inj.extend(std::iter::repeat_n(0x2Fu8, 64));
                        payload = p_inj;
                        tracing::info!(
                            target: "palrle.wire",
                            tile_x = p.tile_xy.0,
                            tile_y = p.tile_xy.1,
                            "test-hook: injected OOB PalRle payload"
                        );
                    }
                }
                (p.tile_xy, payload)
            })
            .collect()
    }
}
