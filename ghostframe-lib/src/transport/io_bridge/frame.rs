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

/// Which Phase B path started a Cdf53 generation; selects the bump
/// counter and the `source` field of the `cdf53.emit` log line.
#[derive(Clone, Copy)]
pub(super) enum Cdf53Source {
    Dirty,
    Escalation,
}

impl Cdf53Source {
    fn label(self) -> &'static str {
        match self {
            Cdf53Source::Dirty => "dirty",
            Cdf53Source::Escalation => "escalation",
        }
    }
}

/// Clamp a GPU-written compact-list count to the number of tiles on the grid,
/// the most entries the list can legitimately hold. A buggy shader or a
/// device fault must not turn the count into an out-of-bounds slice length.
pub(crate) fn clamp_gpu_tile_count(raw: u32, tiles: usize) -> usize {
    (raw as usize).min(tiles)
}

/// The PalRle index buffer's valid bytes: 512 per compact slot, at most one
/// slot per tile. The compact count is written by the GPU, so it is clamped to
/// `tiles` before it sizes a slice over GPU memory. No bulk helper exists on
/// `FrameAnalysis` for this buffer (only the per-slot `index_buffer_slice_for`).
pub(crate) fn palrle_index_slice(
    analysis: &crate::capture::gpu_pipeline::FrameAnalysis,
    tiles: usize,
) -> &[u8] {
    let count = clamp_gpu_tile_count(analysis.palrle_compact_count, tiles);
    if count == 0 || analysis.index_buffer.is_null() {
        return &[];
    }
    // SAFETY: the buffer holds one 512-byte slot per tile and `count` is at
    // most the tile count. Valid until the next process_frame call, which the
    // returned borrow of `analysis` cannot outlive.
    unsafe { std::slice::from_raw_parts(analysis.index_buffer, count * 512) }
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
#[cfg(test)]
pub(crate) fn populate_gpu_metrics(
    tracker: &mut crate::tile::MetricsTracker,
    dirty: &[(u32, u32)],
    cols: u32,
    tile_analysis: &[crate::capture::gpu_pipeline::TileAnalysis],
) {
    for &(tx, ty) in dirty {
        let idx = (ty as usize) * (cols as usize) + (tx as usize);
        apply_gpu_metrics(tracker, tx, ty, tile_analysis.get(idx));
    }
}

/// Write one tile's GPU-derived metrics; `None` (no analysis entry) leaves the
/// tile untouched.
pub(crate) fn apply_gpu_metrics(
    tracker: &mut crate::tile::MetricsTracker,
    tx: u32,
    ty: u32,
    entry: Option<&crate::capture::gpu_pipeline::TileAnalysis>,
) {
    let Some(entry) = entry else {
        return;
    };
    let m = tracker.get_mut(tx, ty);
    m.unique_colors = entry.count.min(u16::MAX as u32) as u16;
    m.edge_density = entry.edge_density_thou as f32 / 1000.0;
}

/// Real-clock lap timer for the per-stage cost trace (`frame.stages`). It
/// measures what the frame cost the thread, so it must not follow tokio's
/// possibly paused clock; hence `std::time::Instant`, not `now_std()`.
pub(super) struct Lap(Option<std::time::Instant>);

impl Lap {
    /// `enabled == false` makes every lap a no-op returning 0, so a frame with
    /// the trace off pays no clock reads.
    pub(super) fn new(enabled: bool) -> Self {
        Self(enabled.then(std::time::Instant::now))
    }

    /// Nanoseconds since the previous lap (or construction); restarts the lap.
    pub(super) fn lap_ns(&mut self) -> u64 {
        let Some(prev) = self.0 else { return 0 };
        let now = std::time::Instant::now();
        self.0 = Some(now);
        now.duration_since(prev).as_nanos() as u64
    }

    /// Microseconds since the previous lap.
    fn lap(&mut self) -> u64 {
        self.lap_ns() / 1000
    }
}

/// Whether the per-stage trace is wanted at all.
pub(super) fn stage_trace_enabled() -> bool {
    tracing::enabled!(target: "ghostframe::io_bridge::diag", tracing::Level::TRACE)
}

/// The parts of a [`FrameSubmission`] the loop still needs after the frame
/// itself has gone to the worker.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameMeta {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub timestamp_us: u32,
    pub capture_done_ns: u64,
}

impl FrameMeta {
    pub(crate) fn of(f: &FrameSubmission) -> Self {
        Self {
            width: f.width,
            height: f.height,
            stride: f.stride,
            timestamp_us: f.timestamp_us,
            capture_done_ns: f.capture_done_ns,
        }
    }
}

/// Loop-side durations of the stages before the worker's finish, carried to
/// the `frame.stages` trace line.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct StageTrace {
    pre_us: u64,
    gpu_us: u64,
    classify_us: u64,
}

/// What to tell the worker for a frame the classifier put in H.264 mode: nothing
/// when no tile changed, otherwise encode, forcing an IDR when the previous
/// frame was on the tile path (a fresh anchor for the client).
pub(super) fn h264_finish_action(
    dirty_is_empty: bool,
    prev_mode: crate::tile::FrameMode,
) -> FinishAction {
    if dirty_is_empty {
        FinishAction::Discard
    } else {
        FinishAction::H264 {
            force_keyframe: prev_mode == crate::tile::FrameMode::TileCodec,
        }
    }
}

/// The frame the worker holds, and what the loop needs to finish it.
pub(super) struct InFlight {
    pub(super) seq: u32,
    /// `session_epoch` when the frame started; a result for an older epoch is
    /// discarded.
    pub(super) epoch: u64,
    pub(super) meta: FrameMeta,
    pub(super) max_dg_size: usize,
    pub(super) stage: InFlightStage,
}

pub(super) enum InFlightStage {
    Analysing {
        pre_us: u64,
    },
    FinishingH264 {
        trace: StageTrace,
    },
    FinishingTile {
        dirty_xy: Vec<(u32, u32)>,
        max_frag: usize,
        escalation_candidates: Vec<u32>,
        stranded_count: usize,
        frame_palette_set_count: u32,
        trace: StageTrace,
    },
    FinishingDiscard,
}

/// What the frame backend hands the loop for a tile-path frame, minus the
/// pixels: owned, so nothing here points into GPU memory.
pub(super) struct TileResults {
    pub(super) palrle: OwnedPalRle,
    pub(super) cdf53_dirty: Vec<Cdf53Tile>,
    pub(super) cdf53_escalation: Vec<Option<Cdf53Encoded>>,
    pub(super) escalation_candidates: Vec<u32>,
    pub(super) stranded_count: usize,
    pub(super) timings: TileTimings,
    /// High-water mark of the GPU's frame palette set, for the emit log.
    pub(super) frame_palette_set_count: u32,
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
        // Without this, CPU-pixel frames hit `process_frame_cpu`, which
        // has no classifier and sends every tile as CDF 5/3.
        let has_gpu_input = frame.dmabuf_fd.is_some() || !frame.pixels.is_empty();
        let path = if has_gpu_input && self.frame_pipeline.is_some() {
            "gpu"
        } else {
            "cpu"
        };
        // Deliberately the real wall clock, not `now_std()`: this measures
        // how long the loop actually spent inside `process_frame` (for a GPU
        // frame that is only `begin_gpu_frame`: the GPU and encode work runs on
        // the frame worker), not protocol/deadline time. A virtual-clock timestamp would
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
        if has_gpu_input && self.frame_pipeline.is_some() {
            self.begin_gpu_frame(frame);
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
        self.frame_seq = self.frame_seq.wrapping_add(1);
        let seq = self.frame_seq;
        self.process_frame_cpu_at(frame, seq, true);
    }

    /// The CPU pipeline for a frame whose sequence number is already
    /// assigned. `emit_dimensions` is false when the caller (the GPU path
    /// falling back) has already announced this frame's dimensions.
    fn process_frame_cpu_at(&mut self, frame: FrameSubmission, seq: u32, emit_dimensions: bool) {
        let grid = TileGrid::new(frame.width, frame.height);

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
            let min_size = self
                .live_max_datagram_size_raw()
                .map(|sz| max_fragment_payload(sz.saturating_sub(Self::WT_VARINT_OVERHEAD)));
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
        if emit_dimensions {
            self.emit_frame_dimensions(seq, frame.timestamp_us, frame.width, frame.height);
        }

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

        // Route through the shared scheduler-dispatch helper. CPU path sends
        // every tile as CDF 5/3 (no classifier).
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
            codec_h264 = stats.codec_histogram.h264,
            "frame.last_send"
        );
    }

    /// Commit a tile to Cdf53: start a new generation
    /// ([`Self::enqueue_cdf53_generation`]), then write the tile's
    /// `codec_state` and `already_escalated_this_gen`.
    ///
    /// `codec_state` is overridden so that `dispatch_dirty_tiles_via_scheduler`'s
    /// pre-check identifies the tile as Cdf53 and skips the priority-queue
    /// enqueue, which would otherwise supersede the refinement passes just
    /// enqueued. The GPU compact list and coefficient buffer are authoritative
    /// for Cdf53 emission, whatever the CPU classifier said.
    ///
    /// `already_escalated_this_gen` is `true` only for an escalation commit: a
    /// dirty commit is a new generation, so the tile is eligible for
    /// re-escalation once it goes idle again.
    pub(super) fn commit_tile_to_cdf53(
        &mut self,
        tile_x: u8,
        tile_y: u8,
        present_passes: u16,
        passes: Vec<(u8, Vec<u8>)>,
        source: Cdf53Source,
    ) {
        self.enqueue_cdf53_generation(tile_x, tile_y, present_passes, passes, source);
        let tm = self.metrics_tracker.get_mut(tile_x as u32, tile_y as u32);
        tm.codec_state = crate::tile::CodecState::Cdf53 {
            passes_sent: 0,
            present_passes,
        };
        tm.already_escalated_this_gen = matches!(source, Cdf53Source::Escalation);
    }

    /// Start a new Cdf53 generation for one tile and queue its passes:
    /// cancel emitter retransmit state, bump the scheduler generation,
    /// count the bump against `source`, log one `cdf53.emit` line per
    /// pass, and enqueue the payloads as refinement work.
    ///
    /// Callers own the `codec_state` and `already_escalated_this_gen`
    /// writes, which must follow this call.
    pub(super) fn enqueue_cdf53_generation(
        &mut self,
        tile_x: u8,
        tile_y: u8,
        present_passes: u16,
        passes: Vec<(u8, Vec<u8>)>,
        source: Cdf53Source,
    ) {
        // Drop emitter retransmit state in lock-step (see
        // dispatch_dirty_tiles_via_scheduler for the
        // dangling-pointer rationale).
        self.reliable_emitter.cancel_for_tile(tile_x, tile_y);
        let gen = self.scheduler.bump_generation(tile_x, tile_y);
        let counter = match source {
            Cdf53Source::Dirty => &mut self.bump_count_accumulator.cdf53_dirty_path,
            Cdf53Source::Escalation => &mut self.bump_count_accumulator.cdf53_escalation_path,
        };
        *counter = counter.saturating_add(1);
        // (Old generation Cdf53 in-flight coverage is now
        // dropped via the `reliable_emitter.cancel_for_tile`
        // call above; the fragment_coverage drop here
        // became redundant when Task 25 paired emitter
        // cancellation with every bump_generation site.)
        let source_label = source.label();
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
                source = source_label,
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

    /// Hand a frame to the CPU pipeline after the GPU processor failed on it.
    ///
    /// `seq` is the number `begin_gpu_frame` already assigned. The frame's
    /// dimensions are announced only once its analysis has been accepted, so a
    /// failed analysis has not announced them: pass `emit_dimensions`. A frame that
    /// arrived as a DMA-BUF alone has no CPU pixels, and the CPU pipeline would
    /// read an empty buffer as black and send black tiles for the whole
    /// screen. Such a frame is read back from the DMA-BUF first (the same
    /// readback the tile-codec path uses), so a failure that repeats every
    /// frame degrades to the CPU pipeline instead of freezing the stream. If
    /// the readback fails too, the frame is dropped rather than sent black.
    pub(super) fn fall_back_to_cpu_after_gpu_error(
        &mut self,
        mut frame: FrameSubmission,
        seq: u32,
        emit_dimensions: bool,
    ) {
        if frame.pixels.is_empty() {
            let readback = frame.dmabuf_fd.as_ref().map(|fd| {
                crate::capture::dmabuf::readback_dmabuf(
                    fd.as_raw_fd(),
                    frame.width,
                    frame.height,
                    frame.stride,
                )
            });
            match readback {
                Some(Ok(pixels)) => frame.pixels = pixels.to_vec(),
                _ => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        seq,
                        "GPU process_frame failed and the DMA-BUF could not be read back; \
                         skipping the frame to avoid emitting zero-filled tiles",
                    );
                    return;
                }
            }
        }
        self.process_frame_cpu_at(frame, seq, emit_dimensions);
    }

    /// GPU-accelerated full-frame pipeline: Vulkan compute dirty detection +
    /// VA-API VPP BGRA→NV12 conversion + H.264 encoding (true zero-copy).
    ///
    /// The frame is processed in loop-sized steps around a worker thread that
    /// owns the GPU and the encoder (see `frame_worker`), one frame in flight:
    ///
    /// 1. here: sequence number, session gate, escalation candidates, then the
    ///    frame goes to the worker for its GPU stage;
    /// 2. [`Self::on_analysed`] when the worker answers: dimensions, the
    ///    classifier, the sweep and the mode decision (all loop state), then the
    ///    worker is told what to finish with;
    /// 3. [`Self::on_h264_done`] / [`Self::on_tile_done`]: commit the results to
    ///    the scheduler, palette table and wire.
    ///
    /// Between the steps the loop serves the network, timers and everything
    /// else; the frame arm of `IoBridge::run` stays off until step 3 is done.
    fn begin_gpu_frame(&mut self, frame: FrameSubmission) {
        debug_assert!(self.in_flight.is_none(), "one frame in flight at a time");
        let mut lap = Lap::new(stage_trace_enabled());
        self.frame_seq = self.frame_seq.wrapping_add(1);
        let seq = self.frame_seq;

        let max_dg_size = match self.compute_max_datagram_size() {
            Some(sz) => sz,
            None => {
                tracing::trace!(target: LOG_TARGET, seq, "process_frame_gpu: no connected sessions, dropping");
                return;
            }
        };

        // Per-frame idle-escalation sweep. Computes the candidate list against
        // the previous frame's tile metrics (the current frame's metrics are
        // updated after the GPU work returns), then hands the list to the
        // GPU processor, which records the forward dispatch in the same
        // command buffer as the dirty-Cdf53 forward stages.
        //
        // Note: metrics_tracker.resize() happens when the analysis is
        // accepted (`on_analysed`). On the very first frame the tracker has
        // cols=0, rows=0 (constructed via MetricsTracker::new(0, 0)), so
        // detect_escalation_candidates iterates 0 tiles and returns empty.
        let candidates = self.collect_escalation_candidates();
        let pre_us = lap.lap();
        self.start_analysis(frame, seq, max_dg_size, candidates, pre_us);
    }

    /// Send a frame to the worker for its GPU stage and mark it in flight.
    pub(super) fn start_analysis(
        &mut self,
        frame: FrameSubmission,
        seq: u32,
        max_dg_size: usize,
        candidates: Vec<u32>,
        pre_us: u64,
    ) {
        let meta = FrameMeta::of(&frame);
        let controls = std::mem::take(&mut self.pending_controls);
        let want_diag = self.diagnostics.diagnose_gpu_pipeline;
        let sent = self.frame_pipeline.as_ref().is_some_and(|p| {
            p.send(ToWorker::Analyse {
                seq,
                frame,
                escalation: candidates,
                want_diag,
                controls,
            })
        });
        if !sent {
            self.fatal = Some("the frame worker has stopped".into());
            return;
        }
        self.in_flight = Some(InFlight {
            seq,
            epoch: self.session_epoch,
            meta,
            max_dg_size,
            stage: InFlightStage::Analysing { pre_us },
        });
    }

    /// Queue a hardware control for the worker; it is applied, in order, before
    /// the next message runs. No-op without a frame worker.
    pub(super) fn backend_apply(&mut self, control: Control) {
        if self.frame_pipeline.is_some() {
            self.pending_controls.push(control);
        }
    }

    /// Queue an `InvalidateBaseline` unless one is already waiting: a reset's
    /// 20-frame cushion must not be shortened to 1 by a discard queued after it
    /// (the processor keeps the last value it is given).
    fn ensure_baseline_invalidated(&mut self) {
        if !self
            .pending_controls
            .iter()
            .any(|c| matches!(c, Control::InvalidateBaseline(_)))
        {
            self.backend_apply(Control::InvalidateBaseline(1));
        }
    }

    pub(super) fn send_finish(
        &mut self,
        mut inflight: InFlight,
        action: FinishAction,
        stage: InFlightStage,
    ) {
        let controls = std::mem::take(&mut self.pending_controls);
        let sent = self.frame_pipeline.as_ref().is_some_and(|p| {
            p.send(ToWorker::Finish {
                seq: inflight.seq,
                action,
                controls,
            })
        });
        if !sent {
            self.fatal = Some("the frame worker has stopped".into());
            return;
        }
        inflight.stage = stage;
        self.in_flight = Some(inflight);
    }

    /// A message from the frame worker, for the frame in flight.
    pub(super) fn on_frame_worker_message(&mut self, msg: FromWorker) {
        self.handle_frame_worker_message(msg);
        // Decode errors that named a tile of the frame that was being committed
        // wait for the commit, so they see the tile's real codec state.
        if self.in_flight.is_none() && !self.deferred_decode_errors.is_empty() {
            for err in std::mem::take(&mut self.deferred_decode_errors) {
                self.recover_from_decode_error(err);
            }
        }
    }

    /// Whether `(x, y)` is a tile of the tile frame currently in flight, whose
    /// codec states are tentative until it is committed.
    pub(super) fn tile_is_in_flight(&self, x: u8, y: u8) -> bool {
        matches!(
            self.in_flight.as_ref().map(|f| &f.stage),
            Some(InFlightStage::FinishingTile { dirty_xy, .. })
                if dirty_xy.contains(&(x as u32, y as u32))
        )
    }

    fn handle_frame_worker_message(&mut self, msg: FromWorker) {
        let Some(inflight) = self.in_flight.take() else {
            self.fatal = Some("the frame worker answered with no frame in flight".into());
            return;
        };
        let seq = match &msg {
            FromWorker::Analysed { seq, .. }
            | FromWorker::AnalyseFailed { seq, .. }
            | FromWorker::H264Done { seq, .. }
            | FromWorker::TileDone { seq, .. }
            | FromWorker::Discarded { seq } => *seq,
        };
        if seq != inflight.seq {
            self.fatal = Some(format!(
                "the frame worker answered for frame {seq}, expected {}",
                inflight.seq
            ));
            return;
        }
        match msg {
            FromWorker::Analysed {
                analysed,
                worker_us,
                ..
            } => self.on_analysed(inflight, *analysed, worker_us),
            FromWorker::AnalyseFailed { error, frame, .. } => {
                if inflight.epoch != self.session_epoch {
                    return;
                }
                tracing::warn!(target: LOG_TARGET, "GPU process_frame failed: {error}, falling back to CPU path");
                self.fall_back_to_cpu_after_gpu_error(*frame, inflight.seq, true);
            }
            FromWorker::H264Done { out, worker_us, .. } => {
                self.on_h264_done(inflight, out, worker_us)
            }
            FromWorker::TileDone {
                out,
                frame_pixels,
                worker_us,
                ..
            } => self.on_tile_done(inflight, *out, frame_pixels, worker_us),
            FromWorker::Discarded { .. } => {}
        }
    }

    /// Step 2: the worker's GPU stage is done. Everything here reads and
    /// writes loop state, so it runs on the loop.
    fn on_analysed(&mut self, inflight: InFlight, analysis: Analysed, gpu_us: u64) {
        use crate::tile::FrameMode;
        let InFlight {
            seq,
            epoch,
            meta,
            max_dg_size,
            ..
        } = inflight;
        let pre_us = match inflight.stage {
            InFlightStage::Analysing { pre_us } => pre_us,
            InFlightStage::FinishingH264 { .. }
            | InFlightStage::FinishingTile { .. }
            | InFlightStage::FinishingDiscard => 0,
        };
        let inflight = InFlight {
            seq,
            epoch,
            meta,
            max_dg_size,
            stage: InFlightStage::FinishingDiscard,
        };
        // The session ended or was reset while the GPU worked: the frame's
        // dirty tiles belong to a client that is gone. Its baseline advanced
        // already, so make the next frame repaint everything.
        if epoch != self.session_epoch {
            self.ensure_baseline_invalidated();
            self.send_finish(
                inflight,
                FinishAction::Discard,
                InFlightStage::FinishingDiscard,
            );
            return;
        }
        let mut lap = Lap::new(stage_trace_enabled());

        tracing::debug!(
            target: LOG_TARGET,
            seq,
            dirty_count = analysis.dirty_tiles.len(),
            "process_frame_gpu: dirty tile detection complete"
        );

        // GPU pipeline diagnostic: one line per frame showing the state of
        // every output buffer FrameAnalysis exposes. Drives W1 root-cause
        // investigation when unique_colors stays at UNIQUE_COLORS_UNKNOWN.
        if let Some(d) = analysis.diag.as_ref() {
            tracing::info!(
                target: "ghostframe::diagnose",
                seq,
                dirty_count = analysis.dirty_tiles.len(),
                tile_analysis_is_null = d.tile_analysis_is_null,
                tile_analysis_len = d.tile_analysis_len,
                tile_analysis_slice_len = d.tile_analysis_slice_len,
                first_nonzero_idx = d.first_nonzero.map(|(i, _, _)| i),
                first_nonzero_count = d.first_nonzero.map(|(_, c, _)| c),
                first_nonzero_edge_thou = d.first_nonzero.map(|(_, _, e)| e),
                palrle_compact_count = analysis.palrle_compact_count,
                frame_palette_set_count = analysis.frame_palette_set_count,
                "gpu-pipeline"
            );
        }

        // Convert flat tile indices (Vec<u32>) into (tile_x, tile_y) pairs
        // matching the row-major layout used by MetricsTracker / TileGrid.
        let cols = meta.width.div_ceil(crate::tile::TILE_SIZE);
        let rows = meta.height.div_ceil(crate::tile::TILE_SIZE);
        let (dirty_xy, prev_mode, new_mode) =
            self.classify_dirty_tiles(&analysis, &meta, cols, rows, seq);

        // ---- M3.3c: PixelPerfect transition sweep ----
        // Runs every frame, unconditionally, before the dirty-tile early return.
        // This ensures that ACKs arriving during static phases (where dirty_xy is
        // empty and the function would otherwise return immediately) still drive
        // the Cdf53 → PixelPerfect transition.
        //
        // Phase B (enqueue_refinement) runs when the tile results are committed
        // (`on_tile_done`), so on dirty frames the sweep runs before new Cdf53
        // work is enqueued. This is correct: after Phase B bumps the generation
        // and clears the ACK counter, the new generation needs subsequent
        // frames' ACKs to accumulate before tile_fully_acked returns true.
        self.pixelperfect_sweep(cols, rows, seq);
        let classify_us = lap.lap();
        let trace = StageTrace {
            pre_us,
            gpu_us,
            classify_us,
        };

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
                let action = h264_finish_action(dirty_xy.is_empty(), prev_mode);
                if matches!(action, FinishAction::Discard) {
                    // Nothing will be committed for this frame, so its
                    // dimensions go out now.
                    self.emit_frame_dimensions(seq, meta.timestamp_us, meta.width, meta.height);
                    self.send_finish(inflight, action, InFlightStage::FinishingDiscard);
                } else {
                    self.send_finish(inflight, action, InFlightStage::FinishingH264 { trace });
                }
            }
            FrameMode::TileCodec => {
                let max_frag = max_fragment_payload(max_dg_size);
                if max_frag == 0 {
                    self.emit_frame_dimensions(seq, meta.timestamp_us, meta.width, meta.height);
                    self.send_finish(
                        inflight,
                        FinishAction::Discard,
                        InFlightStage::FinishingDiscard,
                    );
                    return;
                }

                let (action, escalation_candidates, stranded_count) =
                    self.tile_finish_action(cols, rows, &dirty_xy);
                self.send_finish(
                    inflight,
                    action,
                    InFlightStage::FinishingTile {
                        dirty_xy,
                        max_frag,
                        escalation_candidates,
                        stranded_count,
                        frame_palette_set_count: analysis.frame_palette_set_count,
                        trace,
                    },
                );
            }
        }
    }

    /// The datagram size to commit a finished frame with, or `None` when the
    /// frame is stale: its session ended or was reset while the worker ran, or
    /// no live session remains. Re-read here because the path MTU can shrink
    /// while the worker runs; the smaller value wins.
    fn commit_max_datagram_size(&mut self, inflight: &InFlight) -> Option<usize> {
        if inflight.epoch != self.session_epoch {
            return None;
        }
        // A test bridge has no live quinn connection to size datagrams from.
        #[cfg(test)]
        if let Some(n) = self.test_commit_dg_size {
            return Some(n.min(inflight.max_dg_size));
        }
        self.compute_max_datagram_size()
            .map(|now| now.min(inflight.max_dg_size))
    }

    /// What to ask the worker for a tile frame: the escalation candidates this
    /// frame with what the loop knows about each tile (the worker encodes their
    /// passes), and the dirty tiles no tile encoder will carry. Also returns
    /// the candidates and how many of them are stranded-recovery slots.
    pub(super) fn tile_finish_action(
        &mut self,
        cols: u32,
        rows: u32,
        dirty_xy: &[(u32, u32)],
    ) -> (FinishAction, Vec<u32>, usize) {
        let escalation_candidates =
            std::mem::take(&mut self.cdf53_escalation_candidates_this_frame);
        let stranded_count = std::mem::take(&mut self.cdf53_stranded_candidates_this_frame).len();
        let slots: Vec<EscalationSlot> = escalation_candidates
            .iter()
            .enumerate()
            .map(|(slot, &tile_idx)| EscalationSlot {
                tile_idx,
                stranded: slot < stranded_count,
                tile_is_cdf53: matches!(
                    self.metrics_tracker
                        .get(tile_idx % cols, tile_idx / cols)
                        .codec_state,
                    crate::tile::CodecState::Cdf53 { .. }
                ),
            })
            .collect();
        // Dirty tiles no tile encoder will carry; the worker encodes them as
        // CDF 5/3 in the same call (see `TileRequest`).
        let fallback = self.worker_fallback_tiles(dirty_xy);
        (
            FinishAction::Tile {
                cols,
                rows,
                slots,
                fallback,
            },
            escalation_candidates,
            stranded_count,
        )
    }

    /// The dirty tiles no tile encoder will carry, whose CDF 5/3 the worker
    /// encodes with the frame's own finish: their codec state is Skip, H264 or
    /// PixelPerfect. A PalRle tile that loses its palette at commit joins them
    /// later and is encoded on request.
    pub(super) fn worker_fallback_tiles(&self, dirty_xy: &[(u32, u32)]) -> Vec<(u32, u32)> {
        dirty_xy
            .iter()
            .copied()
            .filter(|&(x, y)| {
                matches!(
                    self.metrics_tracker.get(x, y).codec_state,
                    crate::tile::CodecState::Skip
                        | crate::tile::CodecState::H264 { .. }
                        | crate::tile::CodecState::PixelPerfect
                )
            })
            .collect()
    }

    /// Step 3 for an H.264 frame: fragment and send what the worker encoded.
    pub(super) fn on_h264_done(
        &mut self,
        inflight: InFlight,
        out: Option<H264Out>,
        worker_us: u64,
    ) {
        // The session changed while the encode ran. The encoder's reference
        // chain advanced past this frame, so the next one must be an IDR or the
        // client decodes garbage until the next GOP.
        let Some(max_dg_size) = self.commit_max_datagram_size(&inflight) else {
            self.backend_apply(Control::RequestKeyframe);
            return;
        };
        let InFlightStage::FinishingH264 { trace } = inflight.stage else {
            return;
        };
        // The frame's dimensions go out with the frame, not before the worker
        // has finished it: on a resolution change the client resizes its canvas
        // on them, and the finish can take a second.
        let m = inflight.meta;
        self.emit_frame_dimensions(inflight.seq, m.timestamp_us, m.width, m.height);
        let Some(encoded) = out else {
            return;
        };
        let mut lap = Lap::new(stage_trace_enabled());
        self.send_h264_frame(encoded, &inflight.meta, inflight.seq, max_dg_size);
        let emit_us = lap.lap();
        tracing::trace!(
            target: "ghostframe::io_bridge::diag",
            seq = inflight.seq, mode = "h264",
            pre_us = trace.pre_us, gpu_us = trace.gpu_us, classify_us = trace.classify_us,
            worker_finish_us = worker_us, emit_us,
            "[BRIDGE] frame.stages"
        );
    }

    /// Step 3 for a tile frame: commit what the worker produced.
    pub(super) fn on_tile_done(
        &mut self,
        inflight: InFlight,
        out: TileOut,
        frame_pixels: Vec<u8>,
        worker_us: u64,
    ) {
        // The session changed while the tiles were encoded. The GPU baseline
        // advanced past this frame, so its dirty tiles would never be sent
        // again: repaint everything.
        let Some(max_dg_size) = self.commit_max_datagram_size(&inflight) else {
            self.ensure_baseline_invalidated();
            return;
        };
        let InFlightStage::FinishingTile {
            dirty_xy,
            max_frag,
            escalation_candidates,
            stranded_count,
            frame_palette_set_count,
            trace,
        } = inflight.stage
        else {
            return;
        };
        let TileOut {
            pixels,
            palrle,
            cdf53_dirty,
            cdf53_escalation,
            cdf53_fallback,
            timings,
        } = out;
        // The frame's dimensions go out with the frame (see `on_h264_done`).
        let m = inflight.meta;
        self.emit_frame_dimensions(inflight.seq, m.timestamp_us, m.width, m.height);
        // The path MTU can shrink while the worker runs; fragment for the
        // smaller of the two.
        let max_frag = max_frag.min(max_fragment_payload(max_dg_size));
        if max_frag == 0 {
            self.ensure_baseline_invalidated();
            return;
        }
        let source = match pixels {
            Ok(src) => src,
            Err(msg) => {
                tracing::warn!(target: LOG_TARGET, "{msg}");
                return;
            }
        };
        let pixels: &[u8] = match &source {
            PixelSource::Frame => &frame_pixels,
            PixelSource::Readback(rb) => rb,
        };
        let mut lap = Lap::new(stage_trace_enabled());
        // Hand the worker's pre-encoded fallback tiles to the dispatch below,
        // which takes the ones it needs and asks the worker for the rest.
        self.precomputed_fallback.clear();
        self.precomputed_fallback.extend(cdf53_fallback);
        self.emit_tile_codec_frame(
            &dirty_xy,
            &inflight.meta,
            pixels,
            TileResults {
                palrle,
                cdf53_dirty,
                cdf53_escalation,
                escalation_candidates,
                stranded_count,
                timings,
                frame_palette_set_count,
            },
            inflight.seq,
            max_frag,
        );
        self.precomputed_fallback.clear();
        let emit_us = lap.lap();
        tracing::trace!(
            target: "ghostframe::io_bridge::diag",
            seq = inflight.seq, mode = "tile",
            pre_us = trace.pre_us, gpu_us = trace.gpu_us, classify_us = trace.classify_us,
            worker_finish_us = worker_us, emit_us,
            blocking_forwards_total = self
                .frame_pipeline
                .as_ref()
                .map_or(0, |p| p.forward_requests()),
            "[BRIDGE] frame.stages"
        );
    }

    /// Per-frame idle-escalation sweep and stranded-tile detection; stores the
    /// merged candidate list on the bridge and hands it to the GPU processor.
    fn collect_escalation_candidates(&mut self) -> Vec<u32> {
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
        self.cdf53_escalation_candidates_this_frame = candidates.clone();
        self.cdf53_stranded_candidates_this_frame = stranded_candidates;
        candidates
    }

    /// Dirty-tile bookkeeping, per-tile classification and frame-mode decision
    /// for one GPU frame. Returns `(dirty_xy, prev_mode, new_mode)`.
    fn classify_dirty_tiles(
        &mut self,
        analysis: &Analysed,
        frame: &FrameMeta,
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
        for (&(tx, ty), entry) in dirty_xy.iter().zip(analysis.dirty_analysis.iter()) {
            apply_gpu_metrics(&mut self.metrics_tracker, tx, ty, entry.as_ref());
        }

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
            if prev_mode == FrameMode::H264
                && new_mode == FrameMode::TileCodec
                && self.frame_pipeline.is_some()
            {
                self.backend_apply(Control::InvalidateBaseline(1));
                tracing::info!(
                    target: LOG_TARGET,
                    seq,
                    "H264→TileCodec handoff: invalidate_baseline(1) — lossless repaint"
                );
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

    /// H.264 arm: fragment the frame the worker encoded from the GPU's NV12
    /// output, send it with FEC parity and keep fragments for NACK.
    pub(super) fn send_h264_frame(
        &mut self,
        encoded: H264Out,
        frame: &FrameMeta,
        seq: u32,
        max_dg_size: usize,
    ) {
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
            codec_h264 = 1u8,
            "frame.last_send"
        );
    }

    /// TileCodec arm of `process_frame_gpu`, after the pixel source is chosen:
    /// PalRle phases A/B, Cdf53 passes for dirty and escalated tiles, scheduler
    /// dispatch, RTO sweep and the per-frame logs. The early returns (no
    /// fragment room, no pixel source) stay in the caller.
    pub(super) fn emit_tile_codec_frame(
        &mut self,
        dirty_xy: &[(u32, u32)],
        frame: &FrameMeta,
        pixels: &[u8],
        tile: TileResults,
        seq: u32,
        max_frag: usize,
    ) {
        let TileResults {
            palrle,
            cdf53_dirty,
            cdf53_escalation,
            escalation_candidates,
            stranded_count,
            timings,
            frame_palette_set_count,
        } = tile;
        let trace_on = stage_trace_enabled();
        let mut lap = Lap::new(trace_on);
        let grid = TileGrid::new(frame.width, frame.height);

        // Phase A — serial palette-table allocation per dirty PalRle-feasible tile.
        let cols = grid.cols;
        let preps = self.phase_a_palette_allocation(
            cols,
            &palrle.compact_list,
            &palrle.palette_ids,
            &palrle.folded_into,
            &palrle.palette_set,
            &palrle.index,
        );
        let phase_a_us = lap.lap();

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
        let palrle_phase_b_us = lap.lap();
        let (coef_read_ns, coef_encode_ns) = (timings.coef_read_ns, timings.coef_encode_ns);
        #[cfg(any(test, feature = "test-loss-injection"))]
        if inject_will_fire {
            self.oob_inject_at = None;
        }

        // Phase B — Cdf53 bit-plane passes for high-color tiles, enqueued into
        // the scheduler's refinement queue. The passes were encoded by the
        // frame backend from the GPU's compact list; this commits them.
        for tile in cdf53_dirty {
            let (tile_x, tile_y) = (tile.tile_x, tile.tile_y);
            #[cfg(feature = "cdf53-diag")]
            let coeffs_i16 = &tile.coeffs_i16;
            // M3.3b diagnostic: when GHOSTFRAME_CDF53_DIFF_TILE=X,Y is
            // set, log a one-shot side-by-side comparison of GPU vs
            // CPU `cdf53::forward(tile_bgra)` for that tile. Gated
            // behind the `cdf53-diag` cargo feature so production
            // builds skip the config check entirely.
            #[cfg(feature = "cdf53-diag")]
            if let Some((target_x, target_y)) = self.diagnostics.cdf53_diff_tile {
                if tile_x as u32 == target_x && tile_y as u32 == target_y {
                    let tile_bgra =
                        grid.extract_tile(pixels, frame.stride, tile_x as u32, tile_y as u32);
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
                    let cpu_coeffs_swapped = crate::encoder::cdf53::forward(&tile_swapped);
                    // Sample tile pixels (first 8 BGRA px).
                    let bgra_head: Vec<u8> = tile_bgra.iter().take(32).copied().collect();
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
                    for (ch, ch_diff) in sb_diff.iter_mut().enumerate() {
                        let base = ch * 1024;
                        for (b, (lo, hi)) in band_ranges.iter().enumerate() {
                            for i in *lo..*hi {
                                if coeffs_i16[base + i] != cpu_coeffs[base + i] {
                                    ch_diff[b] += 1;
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
            self.commit_tile_to_cdf53(
                tile_x,
                tile_y,
                tile.encoded.present_passes,
                tile.encoded.passes,
                Cdf53Source::Dirty,
            );
        }

        // ---- M3.3c + Phase 1.5-B: post-fence Phase B for escalation candidates ----
        // The backend encoded the escalation slots' passes after the GPU
        // fence (the existing fence covers all compute work in this command
        // buffer, including the escalation forward dispatches). Here they are
        // enqueued into the scheduler.
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
        debug_assert_eq!(
            escalation_candidates.len(),
            cdf53_escalation.len(),
            "a backend returns one escalation result per candidate"
        );
        for (slot, (&flat_tile_idx, encoded)) in escalation_candidates
            .iter()
            .zip(cdf53_escalation)
            .enumerate()
        {
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
                // forward dispatch with the same coefficients. The
                // backend skips encoding these; this is the same
                // check against the state after the dirty commits.
                if matches!(codec_state, crate::tile::CodecState::Cdf53 { .. }) {
                    continue;
                }
            }
            let Some(encoded) = encoded else {
                continue;
            };
            if is_stranded {
                self.stranded_reenqueue(
                    tile_x,
                    tile_y,
                    codec_state,
                    encoded.present_passes,
                    encoded.passes,
                );
                continue;
            }
            // H264 escalation path (existing).
            self.commit_tile_to_cdf53(
                tile_x,
                tile_y,
                encoded.present_passes,
                encoded.passes,
                Cdf53Source::Escalation,
            );
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
        let cdf53_total_us = lap.lap();
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
            dirty_xy,
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
        let sched_dispatch_us = lap.lap();
        self.sweep_rto_retransmits();
        let rto_us = lap.lap();
        tracing::trace!(
            target: "ghostframe::io_bridge::diag",
            seq, phase_a_us, palrle_phase_b_us, cdf53_total_us,
            coef_read_us = coef_read_ns / 1000, coef_encode_us = coef_encode_ns / 1000,
            sched_dispatch_us, rto_us,
            "[BRIDGE] frame.tile_stages"
        );

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
        self.log_cumulative_emit(seq);

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
            unique_frame_palettes = frame_palette_set_count,
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
            codec_h264 = stats.codec_histogram.h264,
            "frame.last_send"
        );
    }

    /// Throttled cumulative per-codec emission line and bump-count/histogram
    /// diagnostics; the comment above its call site explains the numbers.
    fn log_cumulative_emit(&mut self, seq: u32) {
        if seq.wrapping_sub(self.last_cumulative_emit_log_frame)
            >= CUMULATIVE_EMIT_LOG_INTERVAL_FRAMES
        {
            self.last_cumulative_emit_log_frame = seq;
            let c = self.cumulative_datagrams_emitted;
            let es = self.reliable_emitter.stats;
            let tp = self.transport_probe;
            tracing::info!(
                target: LOG_TARGET,
                frame_seq = seq,
                emitted_solid = c.solid,
                emitted_palrle = c.palrle,
                emitted_cdf53 = c.cdf53,
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
                // Retransmission cause and the RTO's hit rate. Of
                // `retransmit_attempts_total`, `retx_nack` were asked for by
                // the client and `rto_fired` were the timer's. A fire whose
                // pass was then acknowledged by an earlier transmission is
                // `rto_spurious`; one repaired by its retransmission is
                // `rto_useful`.
                retx_nack = es.retransmit_by_nack,
                rto_spurious = es.rto_spurious,
                rto_useful = es.rto_useful,
                // quinn's view of the path and the depth of its datagram
                // send queue. A queue that only grows while `quinn_cwnd`
                // stays cut is the standing queue behind the RTO cascade.
                quinn_cwnd = tp.cwnd,
                quinn_lost_packets = tp.lost_packets,
                quinn_sent_packets = tp.sent_packets,
                quinn_congestion_events = tp.congestion_events,
                quinn_queue_bytes = tp.queue_bytes,
                quinn_queue_bytes_max = tp.max_queue_bytes,
                // The client's own counts (summed `ReceiverFeedback`).
                client_received = tp.client_received,
                client_lost = tp.client_lost,
                client_recovered_fec = tp.client_recovered,
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
                pending_losses_retracted = self.pending_losses_retracted,
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
        if cols == 0 {
            return preps;
        }
        for (c, &tile_idx) in compact_list.iter().enumerate() {
            let tx = tile_idx % cols;
            let ty = tile_idx / cols;

            // The list is written by the GPU. An index off the grid would
            // panic in `metrics_tracker`; skip it like the Cdf53 path does.
            if tx >= self.metrics_tracker.cols() || ty >= self.metrics_tracker.rows() {
                continue;
            }

            // Honour the classifier's actual decision.
            if !matches!(
                self.metrics_tracker.get(tx, ty).codec_state,
                CodecState::PalRle { .. }
            ) {
                continue;
            }

            // Also GPU-written, and sized separately from the list: a short
            // slice must skip the tile, not panic.
            let Some(&pal_id_local) = per_tile_frame_palette_id.get(c) else {
                continue;
            };
            if pal_id_local == 0xFF {
                // Stage 2a sentinel — frame-palette-set overflow.
                self.metrics_tracker.get_mut(tx, ty).codec_state = CodecState::Skip;
                self.palette_table.stats_frame.fell_back_to_raw += 1;
                continue;
            }
            let Some(&folded) = folded_into.get(pal_id_local as usize) else {
                continue;
            };
            let Some(raw) = frame_palette_set.get((folded & 0xFF) as usize) else {
                continue;
            };
            // Before the palette is acquired, so a short buffer leaves no
            // reference or carry count behind.
            let Some(tile_indices) = index_buffer.get(c * 512..(c + 1) * 512) else {
                continue;
            };
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
                    indices.copy_from_slice(tile_indices);
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
