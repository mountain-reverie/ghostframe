// SPDX-License-Identifier: MIT OR Apache-2.0
//! The native half of the client's renderer: a [`TileRenderer`] (the portable
//! tile pipelines and framebuffer, in `ghostframe-render`) plus the export
//! ring, the upload slabs and the H.264 decode and blit.
//!
//! [`Renderer::apply_event`] is the routing point from `ClientCore`'s
//! `Event`s: tile events go to the `TileRenderer`, `NeedsH264` to the decoder.
//! It borrows `WgpuContext` rather than owning it -- the context
//! (instance/adapter/device/queue) is created once per process and shared
//! across sessions in the native client, while a `Renderer` is created per
//! session (it owns per-session framebuffer/ring state sized to that session's
//! frame dimensions).

use crate::export::ExportedImage;
use crate::pipelines::cdf53_grid::Probed;
use crate::pipelines::cdf53_state::Cdf53Stats;
use crate::pipelines::rgba_slab::{RgbaSlabPipeline, SlabWork};
use crate::ring::{ExportRing, PublishedFrame};
use crate::upload_pool::{SlotRef, UploadPool};
use crate::wgpu_ctx::WgpuContext;
use crate::GpuError;

use ghostframe_client_core::Event;
pub use ghostframe_render::GpuDecodeError;
use ghostframe_render::{dimensions_valid, SlotSink, SlotTile, TileRenderer};
/// The `frame_seq` a decoded picture belongs to: the pts it was fed with (a
/// picture may come out on a later call than the access unit that made it).
/// `None` when the decoder lost it. Such a picture is drawn, but cannot be
/// ordered: it neither raises the H.264 floor nor supersedes CDF 5/3 draws.
/// (Falling back to the last fed access unit would, because that can be newer
/// than the picture, raise the floor too far and drop tiles that are newer
/// than the picture but older than that unit.)
fn h264_picture_seq(frame: &ghostframe_client_h264::decoder::HwFrame) -> Option<u32> {
    frame.pts().and_then(|p| u32::try_from(p).ok())
}

/// Which route the most recent H.264 frame took to reach the framebuffer.
/// Tracked (not just logged once ever) so a mid-session transition --
/// e.g. a resolution change that makes the decoder reallocate its pool
/// with a different pitch -- is still visible instead of the log
/// permanently claiming whichever path frame 0 happened to take. Task 12
/// reads that log line as ground truth for which path a run took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum H264ImportPath {
    ZeroCopy,
    CpuCopy,
}

/// What the native client adds to a [`TileRenderer`]'s tile path: tiles whose
/// pixels the decode thread already wrote into an upload slab, drawn by a
/// compute pipeline that reads the slab.
///
/// It is the renderer's [`SlotSink`]: the flush hands it every slot it was
/// given, once, and the slab pool is told after the submit which slabs the
/// GPU has finished with.
#[derive(Default)]
struct SlotState {
    /// The slab-sourced twin of the RGBA pipeline; `Some` once a pool is attached.
    pipeline: Option<RgbaSlabPipeline>,
    pool: Option<UploadPool>,
    /// Slots of the flush in progress, counted per slab.
    retired: Vec<u32>,
}

impl SlotState {
    fn begin_flush(&mut self) {
        self.retired = vec![0; self.pool.as_ref().map_or(0, |p| p.slab_count())];
    }
}

impl SlotSink for SlotState {
    fn retire(&mut self, slot: &SlotTile) {
        // Every slot handed to the flush comes through here (drawn or
        // superseded), so this covers all of them, as the old inline check did.
        if let Some(pool) = &self.pool {
            debug_assert!(
                pool.is_current(SlotRef::from_bits(slot.token)),
                "slot {slot:?} was recycled before it was drawn"
            );
        }
        if let Some(n) = self.retired.get_mut(slot.slab) {
            *n += 1;
        }
    }

    fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        fb: &crate::framebuffer::Framebuffer,
        tiles: &[(u32, u32, SlotTile)],
    ) -> bool {
        let (Some(pipeline), Some(pool)) = (self.pipeline.as_mut(), &self.pool) else {
            return false;
        };
        // One dispatch per slab used.
        let mut batches: Vec<(usize, Vec<SlabWork>)> = Vec::new();
        for &(tile_x, tile_y, slot) in tiles {
            let work = SlabWork {
                tile_x,
                tile_y,
                slot: slot.index,
            };
            match batches.iter_mut().find(|(slab, _)| *slab == slot.slab) {
                Some((_, tiles)) => tiles.push(work),
                None => batches.push((slot.slab, vec![work])),
            }
        }
        pipeline.encode(device, queue, encoder, fb, pool, &batches)
    }
}

/// Owns the tile renderer and the export ring (which in turn owns the dirty
/// history) for one client session.
///
/// Per-frame tile work is batched: `apply_event` only records what arrived
/// and marks the affected tile dirty in the ring's history; `flush` is what
/// actually runs the render/compute passes, once per frame, rather than one
/// pass per tile.
pub struct Renderer {
    tiles: TileRenderer,
    ring: ExportRing,
    slots: SlotState,
    /// Submits the most recent flush made for its own GPU work. See
    /// [`Renderer::last_flush_submits`].
    last_flush_submits: u32,

    /// How many PPM dumps have been written, against
    /// `GHOSTFRAME_CLIENT_DUMP_COUNT`. Counts attempts, not successes -- see
    /// `maybe_dump_framebuffer`.
    dumps_written: u64,
    /// Dump sampling, read from the environment once at construction.
    dump: crate::config::DumpConfig,
    /// Completed `flush` calls, so the dump can be deferred past a server's
    /// opening burst.
    flushes: u64,
    /// `Some` from construction on when `Renderer::new`'s `open_h264_eagerly`
    /// was `true` (the decoder is opened synchronously in `Renderer::new` --
    /// see that function's doc). Otherwise lazily created on the first
    /// `NeedsH264` event in `decode_h264`: a client that was never going to
    /// receive H.264 never opens a decoder, and a machine without VA-API
    /// never can.
    h264_decoder: Option<ghostframe_client_h264::decoder::H264Decoder>,
    h264_pipeline: Option<crate::pipelines::h264_nv12::H264Nv12Pipeline>,
    /// `true` once opening the decoder has failed. Latches the failure so a
    /// machine that advertised H.264 without usable hardware pays the cost
    /// of `H264Decoder::new()` (open `/dev/dri/renderD128`, `vaInitialize`,
    /// dlopen the VA driver, tear it all down) exactly once, not on every
    /// arriving access unit -- at 60fps that retry-forever was the
    /// difference between "never can" and "never can, loudly, every frame".
    h264_unavailable: bool,
    /// Which path the most recently blitted H.264 frame took. `None` until
    /// the first frame; logged on every *change*, not just once ever -- see
    /// [`H264ImportPath`].
    h264_import_path: Option<H264ImportPath>,
    /// `frame_seq` of the last access unit handed to `decode_h264`.
    h264_last_frame_seq: Option<u32>,
    /// What the decoder did with each access unit, for the embedder to report
    /// to the client core (`ClientCore::h264_result`): the gate learns which
    /// frames became references, and the server learns which were decoded.
    /// Collected with [`Renderer::take_h264_results`].
    h264_results: Vec<(u32, ghostframe_client_core::H264Outcome)>,
    /// A decode failed and no access unit marked `reset_decoder` has arrived:
    /// the decoder's state is unknown, so access units queued behind the
    /// failure are not decoded (they would be concealed, and shown) and are
    /// reported `NotOutput`.
    h264_broken: bool,
    /// Whether the "decoder is far behind" warning has been logged this episode.
    h264_delay_warned: bool,
    /// Handed to the decoder whenever one is opened. See
    /// [`Renderer::set_h264_frame_ready_notify`].
    h264_ready_notify: Option<ghostframe_client_h264::FrameReadyNotify>,
    /// Access units fed and not yet answered; see [`crate::h264_pending`].
    h264_pending: crate::h264_pending::PendingFrames,
    /// Throttles the size-mismatch warning in [`Renderer::blit_h264_frame`]
    /// to its first occurrence per mismatch episode, so a persistent
    /// disagreement does not burst-log once per frame at 60fps. Cleared as
    /// soon as a frame's size agrees with the framebuffer again.
    h264_size_mismatch_logged: bool,
}

impl Drop for Renderer {
    /// Slots buffered but never flushed were issued by the decode thread and
    /// will never be drawn; without this their slabs would stay held for the
    /// life of the pool, which outlives a replaced renderer.
    fn drop(&mut self) {
        let Some(pool) = &self.slots.pool else {
            return;
        };
        let mut counts = vec![0u32; pool.slab_count()];
        self.tiles
            .for_each_pending_slot(|slot| counts[slot.slab] += 1);
        pool.retire_now(&counts);
    }
}

impl Renderer {
    /// `export_buffers` must be at least 1 -- a ring with zero buffers can
    /// never hand `publish` a free one, so the client would connect and
    /// then never show a frame. Rejected here rather than clamped, since a
    /// silent clamp would hide a misconfigured caller behind a working demo
    /// and a broken embed.
    /// `open_h264_eagerly` opens the H.264 decoder (device + codec) right
    /// here, synchronously, instead of lazily inside
    /// [`Renderer::apply_event`]'s `NeedsH264` handling -- moving
    /// `H264Decoder::new()`'s ~12.7 ms session-start device-open cost off
    /// the first H.264 access unit. Pass `Client::effective_h264` (the AND
    /// of the host's request and the local VA-API probe): a session that
    /// begins in H.264 mode (the server's default -- `io_bridge.rs`) then
    /// never pays this cost on its first frame, and a client that never
    /// advertised the capability never opens a decoder it will not use.
    ///
    /// A pre-built *hardware frames pool*, sized ahead of the stream's
    /// resolution, was tried and measured here too (design doc §10.7/§10.8)
    /// -- it bought nothing: the ~30 ms cold-decode cost lives in
    /// `vaCreateConfig`/`vaCreateContext` (`ff_vaapi_decode_init`), which
    /// runs unconditionally at first decode regardless of whether
    /// `hw_frames_ctx` was pre-supplied. Only the device/codec open moved
    /// the needle, which is all `open_h264_eagerly` does. The
    /// oversized/mismatched-resolution case remains ffmpeg's own default
    /// behaviour: `get_format` leaves `hw_frames_ctx` null and
    /// `ff_decode_get_hw_frames_ctx` builds a correctly-sized one on
    /// demand, unconditionally, whether the decoder was opened eagerly or
    /// lazily.
    pub fn new(
        ctx: &WgpuContext,
        width: u32,
        height: u32,
        export_buffers: usize,
        preferred_modifiers: &[u64],
        // Diagnostic-only: makes `debug_map_frame` possible at the cost of
        // pinning exports to CPU-visible memory. Production passes false.
        host_visible: bool,
        open_h264_eagerly: bool,
    ) -> Result<Self, GpuError> {
        if export_buffers == 0 {
            return Err(GpuError::NoExportBuffers);
        }
        if !dimensions_valid(width, height) {
            return Err(GpuError::InvalidDimensions { width, height });
        }
        let tiles = TileRenderer::new(
            &ctx.device,
            &ctx.queue,
            wgpu::TextureFormat::Rgba8Unorm,
            width,
            height,
        )?;
        let ring = ExportRing::new(
            ctx,
            width,
            height,
            export_buffers,
            preferred_modifiers,
            host_visible,
        )?;
        // `open_h264_eagerly == false` behaves exactly as before this
        // task: lazy open on the first `NeedsH264` event, see `decode_h264`.
        let (h264_decoder, h264_unavailable) = if open_h264_eagerly {
            #[allow(
                clippy::disallowed_methods,
                reason = "measuring a real hardware VA-API device-open stall for M3 §10's decode-thread decision, not a virtual-clock path"
            )]
            let start = std::time::Instant::now();
            let opened = ghostframe_client_h264::decoder::H264Decoder::new();
            #[allow(
                clippy::disallowed_methods,
                reason = "measuring a real hardware VA-API device-open stall for M3 §10's decode-thread decision, not a virtual-clock path"
            )]
            let new_us = start.elapsed().as_micros() as u64;
            tracing::debug!(new_us, "Renderer::new: H264Decoder::new stall");
            match opened {
                Ok(d) => (Some(d), false),
                // Same reasoning as `decode_h264`'s lazy open: a genuine
                // device/codec-open failure (no VA-API at all) means every
                // later retry would fail identically, so latch it rather
                // than retry per access unit.
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "H.264 was probed available but eager decoder open failed -- the \
                         capability was advertised without hardware to back it"
                    );
                    (None, true)
                }
            }
        } else {
            (None, false)
        };

        Ok(Renderer {
            tiles,
            ring,
            slots: SlotState::default(),
            last_flush_submits: 0,
            dumps_written: 0,
            dump: crate::config::DumpConfig::from_env(),
            flushes: 0,
            h264_decoder,
            h264_pipeline: None,
            h264_unavailable,
            h264_import_path: None,
            h264_last_frame_seq: None,
            h264_results: Vec::new(),
            h264_broken: false,
            h264_delay_warned: false,
            h264_ready_notify: None,
            h264_pending: Default::default(),
            h264_size_mismatch_logged: false,
        })
    }

    /// Mark in the export ring every tile the tile renderer dirtied since the
    /// last call, so the next publish carries it.
    fn sync_dirty(&mut self) {
        let ring = &mut self.ring;
        if self
            .tiles
            .drain_dirty(|x, y| ring.mark_dirty(x as u32, y as u32))
        {
            ring.mark_dirty_all();
        }
    }

    /// Route one `ClientCore` event to the right pipeline and mark the tile
    /// dirty. Batches within a frame; [`Renderer::flush`] submits.
    ///
    /// Every `Event` variant is matched explicitly -- see this crate's
    /// module doc on `clippy::wildcard_enum_match_arm` for why a `_ =>` arm
    /// is refused here.
    pub fn apply_event(&mut self, ctx: &WgpuContext, ev: Event) -> Result<(), GpuError> {
        match ev {
            Event::NeedsH264 {
                frame_seq,
                payload,
                is_keyframe,
                reset_decoder,
                ..
            } => {
                self.decode_h264(ctx, frame_seq, is_keyframe, reset_decoder, &payload)?;
            }
            Event::FrameDimensions { width, height } => {
                self.apply_frame_dimensions(ctx, width, height)?;
            }
            ev @ (Event::TileReady { .. }
            | Event::TilePayload { .. }
            | Event::DecodeError { .. }
            | Event::PaletteUpdated { .. }
            | Event::Evicted { .. }) => {
                self.tiles.apply_event(&ctx.device, &ctx.queue, ev)?;
                self.sync_dirty();
            }
        }
        Ok(())
    }

    fn apply_frame_dimensions(
        &mut self,
        ctx: &WgpuContext,
        width: u32,
        height: u32,
    ) -> Result<(), GpuError> {
        // Ignore a size the renderer would not have been built with: keep the
        // current framebuffer rather than allocate from a network-supplied
        // number.
        if !dimensions_valid(width, height) {
            tracing::warn!(width, height, "ignoring out-of-range FrameDimensions");
            return Ok(());
        }
        // Run any batched work against the OLD framebuffer size before
        // resizing -- the pending tiles were addressed against it.
        self.flush(ctx)?;

        let epoch = self.tiles.fb().epoch();
        self.tiles.resize(&ctx.device, &ctx.queue, width, height);
        if self.tiles.fb().epoch() != epoch {
            // The cached bind groups hold the old texture; drop them now
            // rather than when the codec next draws, or the old framebuffer
            // outlives the resize.
            if let Some(p) = self.slots.pipeline.as_mut() {
                p.drop_framebuffer_cache();
            }
        }
        if let Err(e) = self.ring.resize(ctx, width, height) {
            tracing::error!(error = %e, width, height, "export ring resize failed");
        }
        // `tiles.resize` marked everything dirty; marking the ring all-dirty
        // after `ring.resize` is redundant but harmless: `ring.resize` clears
        // `last_publish`, so the first publish after a resize is a full blit
        // and reports the full surface either way. Native behaviour is as before.
        self.sync_dirty();
        Ok(())
    }

    /// Turn on GPU reconstruction of CDF 5/3 tiles. Allocates the grid's plane
    /// store for the current frame size. The caller is expected to have run
    /// [`crate::pipelines::cdf53_grid::probe`] on this device first: this does
    /// not.
    ///
    /// A frame whose plane store the device cannot bind (about 8K) is decoded
    /// on the CPU by this renderer instead, and the GPU is used again as soon
    /// as a frame fits.
    ///
    /// Builds its own grid with the cost model of the Mali-T860 it was fitted
    /// on (and the 1000-tile submit cap); the native client instead hands over
    /// the grid it probed, with [`Renderer::enable_cdf53_gpu_with`].
    pub fn enable_cdf53_gpu(&mut self, ctx: &WgpuContext) {
        self.tiles.enable_cdf53_gpu(&ctx.device);
    }

    /// [`Renderer::enable_cdf53_gpu`] with a grid that already passed
    /// [`crate::pipelines::cdf53_grid::probe`] on this device, so the shaders
    /// are compiled once, and with the per-flush budget calibrated from what
    /// the probe measured. `max_tiles` caps one submit (the ~500 ms job
    /// watchdog of panfrost/GL is the reason; Vulkan can take far more).
    pub fn enable_cdf53_gpu_with(&mut self, ctx: &WgpuContext, probed: Probed, max_tiles: usize) {
        self.tiles.enable_cdf53_gpu_with(
            &ctx.device,
            probed.grid,
            Some(probed.us_per_tile_k14),
            max_tiles,
        );
    }

    /// Pretend the device's storage binding limit is `bytes`, from the next
    /// resize on. For tests of the too-large-frame fallback.
    #[doc(hidden)]
    pub fn debug_set_cdf53_limit(&mut self, bytes: Option<u64>) {
        self.tiles.debug_set_cdf53_limit(bytes);
    }

    /// Whether CDF 5/3 tiles are waiting for a later flush (the GPU budget was
    /// spent, or a partial tile is being throttled). The caller should flush
    /// again soon even if no event arrives.
    pub fn has_deferred_work(&self) -> bool {
        self.tiles.has_deferred_work()
    }

    /// CDF 5/3 scheduling counters since the last call, and the number of
    /// grid frames recorded; `None` when the GPU path is not enabled.
    pub fn take_cdf53_stats(&mut self) -> Option<(Cdf53Stats, u64)> {
        self.tiles.take_cdf53_stats()
    }

    /// Let this renderer draw tiles from `pool`'s slabs
    /// ([`Renderer::apply_slot_tile`]). Without it such tiles are dropped.
    ///
    /// The pool outlives any one renderer (it belongs to the GPU context); a
    /// renderer that is replaced retires the slots it still held when it drops.
    pub fn attach_upload_pool(&mut self, ctx: &WgpuContext, pool: &UploadPool) {
        self.slots.pipeline = Some(RgbaSlabPipeline::new(&ctx.device, pool));
        self.slots.pool = Some(pool.clone());
    }

    /// The slab counterpart of an `Event::TileReady`: the tile's pixels are
    /// already in `slot`, so only its place in the paint order is recorded.
    /// Behaves exactly as `TileReady` does for ordering, superseding and
    /// duplicates (the later of two for one tile wins, whichever kind).
    ///
    /// Returns whether the renderer took the slot. When it did not (no pool
    /// attached) the slot is the caller's to hand back with
    /// [`UploadPool::retire_now`]; when it did, the renderer retires it.
    #[must_use]
    pub fn apply_slot_tile(
        &mut self,
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        slot: SlotRef,
    ) -> bool {
        let Some(pool) = &self.slots.pool else {
            return false;
        };
        debug_assert!(
            pool.is_current(slot),
            "slot {slot:?} arrived after its slab was recycled"
        );
        self.tiles.apply_slot_tile(
            frame_seq,
            tile_x,
            tile_y,
            SlotTile {
                slab: slot.slab(),
                index: slot.index(),
                token: slot.to_bits(),
            },
        );
        self.sync_dirty();
        true
    }

    /// Whether [`Renderer::flush`] reads the PalRle error slots back. Off by
    /// default: the readback is a blocking `poll(wait_indefinitely)` per
    /// flush, and until there is a path from the render thread to
    /// `ClientCore`'s feedback batcher the result cannot reach the server, so
    /// paying for it would be pure cost.
    pub fn set_collect_decode_errors(&mut self, on: bool) {
        self.tiles.set_collect_decode_errors(on);
    }

    /// Take the GPU-detected tile decode failures accumulated by
    /// [`Renderer::flush`].
    ///
    /// The shader validates PalRle payloads on the device (an index past the
    /// palette count, say), so only the readback after the decode can tell.
    /// The caller is expected to relay these as DECODE_ERROR feedback.
    pub fn take_decode_errors(&mut self) -> Vec<GpuDecodeError> {
        self.tiles.take_decode_errors()
    }

    /// Submit all batched work, in one submit. The ordering rules are
    /// [`TileRenderer::flush`]'s.
    ///
    /// Fails only if the opt-in PalRle error readback cannot wait on the
    /// device (e.g. it was lost). That runs after the submit, so the frame's
    /// work is already on the GPU; the errors it would have reported are lost.
    pub fn flush(&mut self, ctx: &WgpuContext) -> Result<(), GpuError> {
        self.flush_inner(ctx, false).map(|_| ())
    }

    /// [`Renderer::flush`] and [`Renderer::publish`] in one submit: the
    /// export blit is recorded after the codecs, into the same encoder.
    ///
    /// `Ok(None)` when every export buffer is held by the host (the flush
    /// still ran). On error nothing is published; the damage stays in the
    /// ring's history for the next publish.
    pub fn flush_and_publish(
        &mut self,
        ctx: &WgpuContext,
    ) -> Result<Option<PublishedFrame>, GpuError> {
        self.flush_inner(ctx, true)
    }

    /// Submits the most recent flush made for its own GPU work: 1 when it had
    /// anything to draw (with or without a publish), 0 when it had nothing.
    /// Not counted: the opt-in PalRle error readback and framebuffer dump,
    /// which are submits of their own by design. For tests and diagnostics.
    pub fn last_flush_submits(&self) -> u32 {
        self.last_flush_submits
    }

    fn flush_inner(
        &mut self,
        ctx: &WgpuContext,
        publish: bool,
    ) -> Result<Option<PublishedFrame>, GpuError> {
        let submits_before = crate::submit::count();
        if self.slots.pool.is_some() {
            // Fires the completion callbacks of earlier submits, so slots they
            // read are retired even when nothing else polls the device (a
            // flush that did not publish waits for nothing).
            let _ = ctx.device.poll(wgpu::PollType::Poll);
        }
        self.slots.begin_flush();
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ghostframe-flush"),
            });
        let mut recorded =
            self.tiles
                .record(&ctx.device, &ctx.queue, &mut encoder, &mut self.slots);
        // The CDF 5/3 tiles the grid listed are dirty now, before the plan.
        self.sync_dirty();

        let plan = if publish {
            self.ring.plan_publish(self.tiles.fb())
        } else {
            None
        };
        if let Some(plan) = &plan {
            recorded |=
                self.ring
                    .encode_blit(&ctx.device, &ctx.queue, &mut encoder, self.tiles.fb(), plan);
        }

        if recorded {
            if plan.is_some() {
                self.ring.arm_fence(ctx);
            }
            crate::submit::submit(&ctx.queue, encoder);
        }
        if let Some(pool) = &self.slots.pool {
            // After the submit that may read them, or at once if there was none.
            // Not tied to the publish below, which may not happen.
            let retired = std::mem::take(&mut self.slots.retired);
            if recorded {
                pool.retire_when_submitted_work_done(&ctx.queue, retired);
            } else {
                pool.retire_now(&retired);
            }
        }
        let submits = crate::submit::count().wrapping_sub(submits_before) as u32;
        self.last_flush_submits = submits;
        tracing::debug!(submits, publish, "flush: submitted");

        self.tiles.finish_flush(&ctx.device, &ctx.queue)?;
        self.flushes = self.flushes.saturating_add(1);
        self.maybe_dump_framebuffer(ctx);
        Ok(plan.map(|plan| self.ring.finish_publish(ctx, plan, recorded)))
    }

    /// Seal a generation and hand out an export buffer, if one is free.
    pub fn publish(&mut self, ctx: &WgpuContext) -> Option<PublishedFrame> {
        self.ring.publish(ctx, self.tiles.fb())
    }
    /// Write the framebuffer to binary PPM files when
    /// `GHOSTFRAME_CLIENT_DUMP_FRAME` names a path prefix.
    ///
    /// Writes `<prefix>.<n>.ppm` on each of the first
    /// `GHOSTFRAME_CLIENT_DUMP_COUNT` flushes (default 1), logging dimensions
    /// and a non-black pixel count for each.
    ///
    /// **A series, not one frame, and that is deliberate.** A server's opening
    /// burst is thousands of tiles, and the render thread batches aggressively
    /// -- 12k events collapsed into a handful of flushes on the machine this
    /// was written for. So a single dump lands at an arbitrary point in the
    /// paint, and an almost-empty framebuffer is indistinguishable from one
    /// that never fills. A short series shows whether content is accumulating.
    ///
    /// **Reads the framebuffer, deliberately -- not the exported dmabuf.** The
    /// framebuffer is an ordinary wgpu texture, so `debug_read` returns tight
    /// row-major RGBA whatever the export's DRM modifier is. Where the export
    /// is tiled or compressed (see `export_gles.rs`) that distinction separates
    /// two bugs that look identical from outside:
    ///
    /// - dumps fill in -> decode and render are fine; the export layout or the
    ///   consumer's import is at fault.
    /// - dumps stay black -> the fault is upstream, in tile decode or the
    ///   shaders, and the export is a red herring.
    ///
    /// PPM because it needs no dependency and every viewer opens it;
    /// `ghostframe-server` pulls in a PNG encoder for its own capture dump,
    /// but that is a server-side cost and this is a library.
    fn maybe_dump_framebuffer(&mut self, ctx: &WgpuContext) {
        let Some(prefix) = self.dump.frame_prefix.clone() else {
            return;
        };
        // `GHOSTFRAME_CLIENT_DUMP_EVERY=n` samples every nth flush instead,
        // overwriting `<prefix>.latest.ppm`, so a long session can be compared
        // against the export dump at the same moment rather than only during
        // the opening paint.
        let n = self.dumps_written;
        if !self.dump.should_dump(n) {
            self.dumps_written += 1;
            return;
        }
        // Incremented regardless of outcome: a path that cannot be written will
        // not start working on the next flush, and retrying every flush would
        // turn a typo into a log flood.
        self.dumps_written += 1;

        let (w, h) = (self.tiles.fb().width, self.tiles.fb().height);
        let rgba = self.tiles.fb().debug_read(&ctx.device, &ctx.queue);

        // "I dumped a frame" is not the useful fact; "the frame was entirely
        // black" is, and it saves opening the file to find out.
        let non_black = rgba
            .chunks_exact(4)
            .filter(|p| p[0] != 0 || p[1] != 0 || p[2] != 0)
            .count();

        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        ppm.reserve(w as usize * h as usize * 3);
        for px in rgba.chunks_exact(4) {
            ppm.extend_from_slice(&px[..3]);
        }

        let path = if self.dump.every > 0 {
            format!("{prefix}.latest.ppm")
        } else {
            format!("{prefix}.{n}.ppm")
        };
        match std::fs::write(&path, &ppm) {
            Ok(()) => tracing::info!(
                path = %path,
                width = w,
                height = h,
                non_black_px = non_black,
                total_px = w as usize * h as usize,
                flush = self.flushes,
                "dumped framebuffer (PPM via wgpu readback -- unaffected by the \
                 exported dmabuf's modifier)"
            ),
            Err(e) => tracing::warn!(path = %path, error = %e, "framebuffer dump failed"),
        }
    }

    pub fn release(&mut self, frame_id: u32) {
        self.ring.release(frame_id);
    }

    /// Mark the whole surface dirty, so the next `publish` carries all of it.
    pub fn mark_all_dirty(&mut self) {
        self.ring.mark_dirty_all();
    }

    /// Take back a published frame the host never acquired.
    ///
    /// Releases its buffer, and marks the whole surface dirty. Releasing alone
    /// under-reports damage: the next `publish` may reuse a different buffer
    /// whose watermark predates the withdrawn frame, so it would report only
    /// the changes since THAT buffer was filled, while the host is still
    /// showing an even older frame. Re-damaging everything is the one answer
    /// that is right whichever buffer comes next; it costs a full blit, and
    /// only on frames the host was too slow to take.
    pub fn withdraw_frame(&mut self, frame_id: u32) {
        self.ring.release(frame_id);
        self.ring.mark_dirty_all();
    }

    /// The exported dmabuf image for a buffer id `publish` handed out.
    pub fn export_buffer(&self, buffer_id: u32) -> &ExportedImage {
        self.ring.buffer(buffer_id)
    }

    pub fn debug_read_framebuffer(&self, ctx: &WgpuContext) -> Vec<u8> {
        self.tiles.fb().debug_read(&ctx.device, &ctx.queue)
    }
    /// Decode one access unit and blit every frame the decoder has ready.
    /// Frames that finish later are collected by [`Renderer::poll_h264`].
    ///
    /// Failures are logged, never fatal: a corrupt access unit must not take
    /// down the session. Whether to decode at all is the client core's gate;
    /// `reset_decoder` is its instruction to reset first (an IDR, or after
    /// the gate was reset): ffmpeg's H.264 decoder does not error on missing
    /// references, it conceals, so an IDR alone would not clear stale state.
    ///
    /// Outcomes are queued for [`Renderer::take_h264_results`]: a frame is
    /// decoded when a picture carrying its pts comes out, on this call or a
    /// later one; units older than an output picture are refused.
    fn decode_h264(
        &mut self,
        ctx: &WgpuContext,
        frame_seq: u32,
        is_keyframe: bool,
        reset_decoder: bool,
        au: &[u8],
    ) -> Result<(), GpuError> {
        use ghostframe_client_core::H264Outcome;
        if self.h264_unavailable {
            self.h264_results.push((frame_seq, H264Outcome::NoDecoder));
            return Ok(());
        }
        if self.h264_decoder.is_none() {
            // Timed separately from `decoder.decode()` below: this is the
            // one-time session-startup cost (`av_hwdevice_ctx_create`, open
            // `/dev/dri/renderD128`, `vaInitialize`, dlopen the VA driver --
            // see `h264_unavailable`'s doc), paid once per session, not
            // once per frame. M3 §10's first review round found it excluded
            // entirely, which understated the real first-frame stall by
            // roughly half.
            #[allow(
                clippy::disallowed_methods,
                reason = "measuring a real hardware VA-API device-open stall for M3 §10's decode-thread decision, not a virtual-clock path"
            )]
            let new_start = std::time::Instant::now();
            let opened = ghostframe_client_h264::decoder::H264Decoder::new();
            #[allow(
                clippy::disallowed_methods,
                reason = "measuring a real hardware VA-API device-open stall for M3 §10's decode-thread decision, not a virtual-clock path"
            )]
            let new_us = new_start.elapsed().as_micros() as u64;
            tracing::debug!(new_us, "decode_h264: H264Decoder::new stall");
            match opened {
                Ok(mut d) => {
                    if let Some(notify) = &self.h264_ready_notify {
                        d.set_frame_ready_notify(notify.clone());
                    }
                    self.h264_decoder = Some(d);
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "H.264 arrived but no decoder could be opened -- the capability \
                         was advertised without hardware to back it"
                    );
                    self.h264_unavailable = true;
                    self.h264_results.push((frame_seq, H264Outcome::NoDecoder));
                    return Ok(());
                }
            }
        }

        // Flush batched tile work for the OLD framebuffer state before this
        // new full-frame H.264 draw, same reasoning as the
        // `Event::FrameDimensions` arm above: `apply_event` only queues
        // solid/palrle/cdf53 work and batches it until `flush`, but
        // `blit_h264_frame` below draws immediately. Without this, a burst
        // containing tile payloads for frame N followed by an H.264 access
        // unit for frame N+1 -- exactly what a mode-switch transition
        // produces -- would draw the H.264 frame first and then paint the
        // older, now-stale tiles over it when the batch's `flush` finally
        // runs.
        self.flush_under_h264(ctx)?;

        if self.h264_pipeline.is_none() {
            self.h264_pipeline = Some(crate::pipelines::h264_nv12::H264Nv12Pipeline::new(
                &ctx.device,
            ));
        }

        if self.h264_broken && !reset_decoder {
            self.h264_results.push((frame_seq, H264Outcome::NotOutput));
            return Ok(());
        }
        let decoder = self.h264_decoder.as_mut().expect("just created");

        self.h264_last_frame_seq = Some(frame_seq);
        if reset_decoder {
            // Whatever the decoder still held is flushed with it: those
            // access units will never produce a picture.
            self.h264_results.extend(self.h264_pending.refuse_all());
            decoder.reset();
            self.h264_broken = false;
        }

        // Timed at `debug` level so M3's decode-cost measurement (design doc
        // §10) can isolate this span from everything else `decode_h264`
        // does. Two unconditional `Instant::now()` calls (~40ns/frame,
        // matching `ring.rs`'s `publish` timing precedent exactly) bracket
        // it; only the `tracing::debug!` line below is level-gated, off by
        // default, on with `RUST_LOG=ghostframe_client_gpu::renderer=debug`.
        //
        // What this span is NOT: "submit to surface available". ffmpeg's
        // VA-API hwaccel does not synchronise the decoded surface inside
        // `avcodec_receive_frame` -- the `vaSyncSurface` wait for the
        // hardware to actually finish decoding lives in
        // `frame.map_dmabuf()` (its own explicit sync) in `blit_h264_frame`
        // below, which is timed separately. This span is submit-through-
        // `avcodec_receive_frame`-return: CPU-side packet handling plus
        // whatever of the decode VA-API's async submission model makes
        // synchronous, not the hardware decode itself.
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real hardware H.264 decode stall for M3 §10's decode-thread decision, not a virtual-clock path"
        )]
        let decode_start = std::time::Instant::now();
        let frames = match decoder.decode_pts(au, i64::from(frame_seq)) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(frame_seq, is_keyframe, error = %e, "H.264 decode failed");
                // A hard decode error leaves the decoder's internal state for
                // this access unit unknown. The gate resets, asks the server
                // for an IDR (paced) and resets the decoder at it.
                self.h264_broken = true;
                // Older units still inside the decoder are lost with its
                // state; refuse them first, then report the failure itself
                // (the core refuses everything until the reset frame).
                self.h264_results.extend(self.h264_pending.refuse_all());
                self.h264_results
                    .push((frame_seq, H264Outcome::DecodeError));
                return Ok(());
            }
        };
        // Answer by picture, not by call: a decoder with output delay (the
        // GStreamer backend returns a picture on the call after its access
        // unit) still decoded every frame, just later.
        self.h264_results.extend(self.h264_pending.fed(frame_seq));
        for f in &frames {
            let answers = self.h264_pending.output(f.pts());
            self.h264_results.extend(answers);
        }
        let pending = self.h264_pending.len();
        if pending > crate::h264_pending::WARN_PENDING {
            if !self.h264_delay_warned {
                self.h264_delay_warned = true;
                tracing::warn!(
                    frame_seq,
                    pending,
                    "H.264 decoder is more than a few access units behind: \
                     frames are acknowledged late, and refused once the backlog \
                     reaches its bound"
                );
            }
        } else if pending <= 1 {
            self.h264_delay_warned = false;
        }
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real hardware H.264 decode stall for M3 §10's decode-thread decision, not a virtual-clock path"
        )]
        let decode_us = decode_start.elapsed().as_micros() as u64;
        tracing::debug!(
            frame_seq,
            is_keyframe,
            decode_us,
            frame_count = frames.len(),
            "decode_h264: decoder.decode stall"
        );

        self.blit_h264_frames(ctx, frame_seq, frames);
        Ok(())
    }

    /// The flush that precedes an H.264 blit: Solid / PalRle / RGBA work lands
    /// under the frame, as it always did, but CDF 5/3 tiles are held back. The
    /// frame covers the screen, so drawing them first would only spend a
    /// flush's GPU budget; and if the frame is then blitted they are
    /// cancelled ([`Renderer::supersede_cdf53`]), where drawing them later
    /// would put them on top of it.
    fn flush_under_h264(&mut self, ctx: &WgpuContext) -> Result<(), GpuError> {
        self.tiles.hold_cdf53(true);
        let r = self.flush(ctx);
        self.tiles.hold_cdf53(false);
        r
    }

    /// An H.264 frame from `frame_seq` was blitted over the whole framebuffer:
    /// cancel CDF 5/3 draws still waiting, and refuse older passes.
    pub fn supersede_cdf53(&mut self, frame_seq: u32) {
        self.tiles.supersede_cdf53(frame_seq);
    }

    /// An H.264 picture from `seq` is now over the whole framebuffer: record
    /// it in the paint-order ledger and cancel the CDF 5/3 draws it is newer
    /// than.
    fn h264_picture_drawn(&mut self, seq: u32) {
        self.tiles.note_h264(seq);
    }

    /// The ordering decision of the H.264 blit without a picture, for tests:
    /// `false` when a picture from `seq` is older than what is on screen (and
    /// would not be drawn), otherwise records it as drawn, as the blit does.
    #[doc(hidden)]
    pub fn debug_accept_h264_picture(&mut self, seq: u32) -> bool {
        if self.tiles.h264_stale(seq) {
            return false;
        }
        self.h264_picture_drawn(seq);
        true
    }
    /// Blit `frames` and mark the ring dirty if any landed.
    fn blit_h264_frames(
        &mut self,
        ctx: &WgpuContext,
        frame_seq: u32,
        frames: Vec<ghostframe_client_h264::decoder::HwFrame>,
    ) -> bool {
        let mut any_blitted = false;
        for frame in frames {
            let seq = h264_picture_seq(&frame);
            if seq.is_some_and(|s| self.tiles.h264_stale(s)) {
                // Older than a paint already on screen (a tile of a later
                // frame, or a newer picture): it covers every tile, so
                // drawing it would undo that paint. The access unit was
                // decoded and answered; only its picture is dropped.
                tracing::debug!(
                    ?seq,
                    "H.264 picture older than what is on screen: not drawn"
                );
                continue;
            }
            if self.blit_h264_frame(ctx, seq.unwrap_or(frame_seq), &frame) {
                any_blitted = true;
                if let Some(s) = seq {
                    self.h264_picture_drawn(s);
                }
            }
        }
        // Once after the loop, not once per frame inside it: an access unit
        // that completes more than one frame (reordering drain) would
        // otherwise mark the ring dirty redundantly, and an access unit
        // that completed none (every frame dropped on a size mismatch)
        // would otherwise mark it dirty for nothing.
        if any_blitted {
            self.ring.mark_dirty_all();
        }
        any_blitted
    }

    /// Install the callback the H.264 decoder runs when a frame finishes
    /// decoding after [`Renderer::apply_event`] returned -- see
    /// [`ghostframe_client_h264::FrameReadyNotify`]. The owner answers it by
    /// calling [`Renderer::poll_h264`]. Applies to the current decoder and to
    /// any opened later.
    pub fn set_h264_frame_ready_notify(
        &mut self,
        notify: ghostframe_client_h264::FrameReadyNotify,
    ) {
        if let Some(d) = self.h264_decoder.as_mut() {
            d.set_frame_ready_notify(notify.clone());
        }
        self.h264_ready_notify = Some(notify);
    }

    /// Blit every H.264 frame the decoder finished since the last access unit.
    /// Returns whether anything was drawn, i.e. whether a publish is due.
    ///
    /// Without this a frame waits for the *next* access unit to be collected,
    /// and on a remote desktop the next one comes when the screen next
    /// changes: a keystroke's echo stayed invisible until the one after it.
    pub fn poll_h264(&mut self, ctx: &WgpuContext) -> Result<bool, GpuError> {
        let Some(decoder) = self.h264_decoder.as_mut() else {
            return Ok(false);
        };
        let frames = match decoder.take_ready() {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "H.264 decode failed (late frame)");
                // The decoder's state is unknown; same as a hard error in
                // `decode_h264`: access units are refused until the gate
                // resets it.
                self.h264_broken = true;
                self.h264_results.extend(self.h264_pending.refuse_all());
                return Ok(false);
            }
        };
        if frames.is_empty() {
            return Ok(false);
        }
        // A picture that came out after the call that fed it: this is where
        // its frame ack is owed.
        for f in &frames {
            let answers = self.h264_pending.output(f.pts());
            self.h264_results.extend(answers);
        }
        // Same ordering rule as `decode_h264`: batched tile work for the
        // older framebuffer state lands before the full-frame draw.
        self.flush_under_h264(ctx)?;
        if self.h264_pipeline.is_none() {
            self.h264_pipeline = Some(crate::pipelines::h264_nv12::H264Nv12Pipeline::new(
                &ctx.device,
            ));
        }
        let frame_seq = self.h264_last_frame_seq.unwrap_or(0);
        tracing::debug!(
            frame_seq,
            frame_count = frames.len(),
            "poll_h264: late frames ready"
        );
        Ok(self.blit_h264_frames(ctx, frame_seq, frames))
    }

    /// What the decoder did with each access unit since the last call, in
    /// order. The embedder reports each to `ClientCore::h264_result`.
    pub fn take_h264_results(&mut self) -> Vec<(u32, ghostframe_client_core::H264Outcome)> {
        std::mem::take(&mut self.h264_results)
    }

    /// Drain and blit any frames still buffered by B-frame reordering.
    ///
    /// Terminal for the underlying decoder -- see [`ghostframe_client_h264::
    /// decoder::H264Decoder::finish`]'s own doc -- so this is for session
    /// teardown, not mid-stream use. A no-op if no H.264 has arrived yet.
    pub fn finish_h264(&mut self, ctx: &WgpuContext) {
        let Some(decoder) = self.h264_decoder.as_mut() else {
            return;
        };
        let frame_seq = self.h264_last_frame_seq.unwrap_or(0);
        match decoder.finish() {
            Ok(frames) => {
                // Teardown: nothing is flushed under the pictures and no
                // frame ack is owed (the session is ending), so only the
                // ordering against what is on screen is kept.
                let mut any_blitted = false;
                for frame in &frames {
                    let seq = h264_picture_seq(frame);
                    if seq.is_some_and(|s| self.tiles.h264_stale(s)) {
                        continue;
                    }
                    if self.blit_h264_frame(ctx, seq.unwrap_or(frame_seq), frame) {
                        any_blitted = true;
                        if let Some(s) = seq {
                            self.h264_picture_drawn(s);
                        }
                    }
                }
                if any_blitted {
                    self.ring.mark_dirty_all();
                }
            }
            Err(e) => tracing::warn!(error = %e, "H.264 finish (drain) failed"),
        }
    }

    /// Blit one decoded H.264 frame into the framebuffer. Returns whether it
    /// actually drew anything, so callers can decide whether the ring needs
    /// marking dirty.
    ///
    /// Each `HwFrame` holds a VA-API surface out of the decoder's pool for
    /// as long as it lives. Every caller of this function (`decode_h264`'s
    /// `for frame in frames` loop, `finish_h264`'s equivalent) drops each
    /// frame at the end of its own loop iteration, so nothing is held
    /// across presents and the decoder's default pool size is fine -- but
    /// that is a property of the caller, not a guarantee. A later change
    /// that queues frames (e.g. to hand them to a compositor across a frame
    /// boundary) makes the next `receive_frame` stall on pool exhaustion
    /// rather than fail, which reads as a hang, not a decode error.
    ///
    /// ## What "held until the submission retires" does and does not cover
    ///
    /// `mapped` (the dmabuf mapping) is kept alive until after `pipeline.draw`
    /// has been submitted below, not dropped the moment `import_nv12`
    /// returns: `map_dmabuf` takes its own reference on the VA-API surface
    /// (see its doc comment), so dropping `mapped` early would release that
    /// reference before the GPU work sampling it had even been submitted.
    ///
    /// That said, "submitted" is not "complete". `pipeline.draw` returns
    /// once the command buffer is queued, not once the GPU has executed it.
    /// `frame` (the `HwFrame`) is dropped by the caller shortly after this
    /// function returns -- returning its VA-API surface to the decoder's
    /// pool, which is LIFO, so the very next `decode()` call can hand that
    /// same surface back to the decoder to write into while the GPU may
    /// still be sampling it for this draw. Nothing in this function fences
    /// against that. It is believed safe on this project's hardware
    /// (amdgpu/RADV) because the kernel fences a shared dma-buf's BO
    /// through its reservation object implicitly -- any driver access that
    /// goes through the same dma-buf (VA-API's write, Vulkan's read here)
    /// is ordered by the kernel independent of userspace synchronization --
    /// but that is measured/assumed driver behaviour, exactly the class
    /// `import.rs`'s module doc warns must be written down rather than
    /// relied on silently: a driver that does not fence a shared dma-buf's
    /// BO this way would read the decoder's next write mid-sample instead
    /// of the frame this function intended, without any error.
    ///
    /// **That check has now been done, for the GLES/GStreamer target, and the
    /// answer was no.** V4L2 hands a buffer back to its pool on release with
    /// no fence for a GPU reader to wait on, and EGL's dmabuf import takes no
    /// fence either -- so nothing orders the decoder's next write against this
    /// draw. That path therefore waits for its own submission explicitly
    /// before the frame is released; see `wait_for_submission`, which also
    /// carries the measured cost. The Vulkan path keeps the implicit-fencing
    /// assumption, because it is a different driver stack and has not been
    /// re-measured -- not because it has been shown safe.
    fn blit_h264_frame(
        &mut self,
        ctx: &WgpuContext,
        frame_seq: u32,
        frame: &ghostframe_client_h264::decoder::HwFrame,
    ) -> bool {
        // A decoded size that disagrees with the framebuffer means the tile
        // stream's FrameDimensions and the H.264 SPS disagree. Blitting
        // anyway corrupts the framebuffer; dropping recovers at the next
        // keyframe. Logged at `warn` only on the first occurrence of a
        // mismatch episode -- a persistent disagreement would otherwise
        // burst-log once per frame at 60fps, the exact shape that has
        // blinded diagnosis here before (a journald rate-limit hole reading
        // as a stalled loop).
        if frame.width() != self.tiles.fb().width || frame.height() != self.tiles.fb().height {
            if !self.h264_size_mismatch_logged {
                tracing::warn!(
                    frame_seq,
                    decoded_w = frame.width(),
                    decoded_h = frame.height(),
                    fb_w = self.tiles.fb().width,
                    fb_h = self.tiles.fb().height,
                    "dropping H.264 frame(s) whose size disagrees with the framebuffer"
                );
                self.h264_size_mismatch_logged = true;
            }
            return false;
        }
        self.h264_size_mismatch_logged = false;

        let pipeline = self.h264_pipeline.as_mut().expect("created by caller");

        // Preferred path: import the decoder's dmabuf directly. Falls back
        // to a CPU download when the surface is tiled, misaligned, or the
        // driver's linear pitch disagrees with the producer's -- all of
        // which `import_nv12` reports rather than rendering wrong pixels.
        // `mapped` is kept alive alongside `imported`/the textures through
        // the draw below -- see this function's doc for exactly what that
        // does and does not guarantee.
        //
        // Timed at `debug` for the same M3 §10 measurement as
        // `decode_h264`'s `decode_us` above, and load-bearing to that
        // measurement's honesty: this call, not `decoder.decode()`, is
        // where `vaSyncSurface` actually waits for the hardware to finish
        // decoding (`HwFrame::map_dmabuf` syncs explicitly: FFmpeg 4.4's
        // map-to-DRM path does not, and skipping it drew a green frame).
        // A first review round timed only `decode()` and reported the
        // submit cost as if it were the decode cost; this span is what was
        // missing.
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real hardware vaSyncSurface wait for M3 §10's decode-thread decision, not a virtual-clock path"
        )]
        let map_start = std::time::Instant::now();
        let mapped = frame.map_dmabuf();
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real hardware vaSyncSurface wait for M3 §10's decode-thread decision, not a virtual-clock path"
        )]
        let map_us = map_start.elapsed().as_micros() as u64;
        tracing::debug!(frame_seq, map_us, "blit_h264_frame: map_dmabuf stall");
        let imported = match &mapped {
            Ok(m) => crate::import::import_nv12(ctx, m.planes()).map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };

        let path = match imported {
            Ok(imported) => {
                let submission = pipeline.draw(
                    &ctx.device,
                    &ctx.queue,
                    self.tiles.fb(),
                    imported.luma(),
                    imported.chroma(),
                );
                // Wait for exactly this draw before returning, because the
                // caller drops `frame` right after and that hands the decoder's
                // buffer straight back to its pool. See this function's doc:
                // the Vulkan path left this to the kernel's implicit dma-buf
                // fencing and asked a future hardware target to check rather
                // than inherit the assumption. This is that target, the
                // assumption does not hold here, and the fence is explicit.
                //
                // `submission` and not "the most recent submission": the tile
                // codecs share this queue, so an unqualified wait would block
                // on their work too and make this cost depend on what else the
                // frame happened to contain.
                wait_for_submission(ctx, submission, frame_seq);
                H264ImportPath::ZeroCopy
            }
            Err(reason) => {
                if self.h264_import_path != Some(H264ImportPath::CpuCopy) {
                    tracing::info!(
                        reason = %reason,
                        "H.264 import path: CPU copy (dmabuf import unavailable)"
                    );
                }
                let Some((luma, chroma)) = frame.download_nv12() else {
                    tracing::warn!(frame_seq, "H.264 CPU download failed");
                    return false;
                };
                let (luma_tex, chroma_tex) =
                    crate::pipelines::h264_nv12::H264Nv12Pipeline::upload_planes(
                        &ctx.device,
                        &ctx.queue,
                        self.tiles.fb().width,
                        self.tiles.fb().height,
                        &luma,
                        &chroma,
                    );
                // No wait: these textures are wgpu's own, uploaded from a
                // `Vec` that has already been copied out of the decoder's
                // buffer. Nothing reclaims them underneath the GPU.
                let _ = pipeline.draw(
                    &ctx.device,
                    &ctx.queue,
                    self.tiles.fb(),
                    &luma_tex,
                    &chroma_tex,
                );
                H264ImportPath::CpuCopy
            }
        };
        if path == H264ImportPath::ZeroCopy && self.h264_import_path != Some(path) {
            // The layout is logged with it because a wrong offset or pitch is
            // the fault that still renders: right shape, wrong content.
            match &mapped {
                Ok(m) => tracing::info!(
                    planes = ?m.planes(),
                    "H.264 import path: zero-copy dmabuf"
                ),
                Err(_) => tracing::info!("H.264 import path: zero-copy dmabuf"),
            }
        }
        self.h264_import_path = Some(path);
        // `mapped` drops here, at the end of this function -- after both
        // draw calls above, not before either of them. See this function's
        // doc for why that matters and what it does not guarantee.
        true
    }
}

/// Block until `submission` has executed on the GPU.
///
/// The price of a zero-copy import whose buffer is borrowed from a decoder
/// pool. Without it, the sequence is: submit the draw, return, caller drops the
/// frame, the pool hands that same buffer to the decoder, and the decoder
/// writes the next picture into memory the GPU may still be sampling. The
/// result is not an error -- it is a frame with a band of the *next* frame in
/// it, which reads as tearing and has no diagnostic attached.
///
/// It would be nicer to hand the decoder a real fence and let the two overlap.
/// Neither EGL's dmabuf import nor V4L2's buffer handover offers one here, and
/// relying on the kernel to fence the dma-buf implicitly is what the Vulkan
/// path does and explicitly flagged as unverified on new hardware.
///
/// **Measured on RK3399/Mali-T860 at 1920x1080**, paced 60Hz, 120 frames:
/// p50 3.8ms, p90 5.7ms, p99 13.7ms, with a one-off 20.8ms on the third frame
/// (pipeline warmup). About 11% of a 33ms frame budget at the median.
///
/// Most of that is the blit itself -- 2M pixels of NV12->RGBA on a 500MHz
/// 4-core GPU -- not fence overhead, so the real cost of this call is the
/// pipelining it gives up rather than the time it adds. Affordable at 30fps,
/// tight at 60.
///
/// The identified optimisation, not taken here, is to defer the frame's release
/// by one instead of waiting: a frame-time of slack is longer than any measured
/// blit, so the buffer would be safe to recycle with no stall at all. It needs
/// `Renderer` to hold the previous `HwFrame` (cheap -- the pool has ~11
/// buffers), and it trades an explicit guarantee for a temporal one, which is
/// worth doing deliberately with its own measurement rather than bolted on
/// here.
fn wait_for_submission(ctx: &WgpuContext, submission: wgpu::SubmissionIndex, frame_seq: u32) {
    #[allow(
        clippy::disallowed_methods,
        reason = "measuring a real GPU wait, not a virtual-clock path"
    )]
    let start = std::time::Instant::now();
    match ctx.device.poll(wgpu::PollType::Wait {
        submission_index: Some(submission),
        // No timeout: a draw that never completes means the GPU is wedged, and
        // continuing would corrupt the next frame rather than recover. An
        // indefinite wait at least stalls visibly instead of rendering
        // plausible garbage.
        timeout: None,
    }) {
        Ok(_) => {}
        Err(e) => tracing::warn!(frame_seq, error = ?e, "h264 blit: waiting for the GPU failed"),
    }
    #[allow(
        clippy::disallowed_methods,
        reason = "measuring a real GPU wait, not a virtual-clock path"
    )]
    let wait_us = start.elapsed().as_micros() as u64;
    tracing::debug!(frame_seq, wait_us, "h264 blit: GPU fence wait");
}
