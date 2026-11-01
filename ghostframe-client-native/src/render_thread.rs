//! The render thread: owns `WgpuContext` and the per-session `Renderer`.
//!
//! Kept off the net thread deliberately. `ExportRing::publish` calls
//! `device.poll(PollType::wait_indefinitely())` after its blit (see the
//! design doc's section 5.5) -- a real GPU stall, not a fast path. If that lived
//! on the same thread that owns `ClientNet`, a stall there would delay
//! ACK/NACK batching and timer-driven feedback, and much of the M3
//! transport stack is timing-sensitive. Splitting the threads means a GPU
//! hiccup only ever delays a frame, never the transport's own clock.
//!
//! `ghostframe_client_core::Event`s arrive over an `mpsc` channel from the
//! net thread; `RenderMsg::Release` arrives from whichever thread the
//! embedder calls `Client::release_frame` from. A batch of whatever is
//! immediately available is applied, then `flush` + `publish` run once --
//! this is the "frame boundary" the crate-level docs describe, since
//! `ClientCore` has no explicit end-of-frame event of its own to key off.
//!
//! `flush` + `publish` are further paced by a [`FrameClock`]: events are applied
//! as they arrive, but the expensive half runs at most once per display
//! interval, and the loop wakes at the deadline on its own.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use ghostframe_client_core::Event as CoreEvent;
use ghostframe_client_gpu::renderer::Renderer;
use ghostframe_client_gpu::ring::PublishedFrame;
use ghostframe_client_gpu::upload_pool::{SlotAllocator, SlotRef, UploadPool};
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;

use crate::event::{ClientEvent, EventQueue};
use crate::frame_clock::{FrameClock, Pace};
use crate::DebugFrameBytes;

/// Default export-ring buffer count used when [`crate::Config::n_export_buffers`]
/// is `0`. Kept here, in the caller, rather than inside `Renderer` -- the
/// renderer treats 0 as a rejected configuration (see
/// [`ghostframe_client_gpu::GpuError::NoExportBuffers`]), and it is this
/// thread's job to decide what a client that didn't specify a count gets.
const DEFAULT_EXPORT_BUFFER_COUNT: u32 = 3;

pub(crate) enum RenderMsg {
    Core(CoreEvent),
    /// A `TileReady` whose pixels the decode thread already wrote into an
    /// upload slab. Rides the same channel as [`RenderMsg::Core`], so it keeps
    /// its place among tiles, dimensions and H.264 frames.
    SlabTile {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        slot: SlotRef,
    },
    Release(u32),
    /// Map a published frame's exported dmabuf and reply with its bytes.
    /// Test and diagnostic use only -- see [`crate::Client::debug_map_frame`].
    /// The render thread owns the `Renderer` (and therefore the
    /// `ExportedImage`), so this round-trips through here rather than
    /// letting the caller reach the buffer directly.
    DebugMapFrame(u32, Sender<Result<DebugFrameBytes, String>>),
    /// The H.264 decoder finished a frame after the access unit that fed it
    /// was applied. Sent from a decoder thread; see
    /// [`Renderer::set_h264_frame_ready_notify`].
    H264FramesReady,
    /// Mark the whole surface dirty and publish: the host lost what it was
    /// showing and a quiet remote screen would never resend it.
    Redraw,
    Shutdown,
}

/// Handle a [`RenderMsg::DebugMapFrame`]: look up the exported buffer, map
/// it, and reply. Never panics on a bad `buffer_id` or an unconstructed
/// renderer -- errors are reported to the caller instead, since this is a
/// debug path a test may call before the first frame exists.
fn handle_debug_map_frame(
    renderer: &Option<Renderer>,
    buffer_id: u32,
) -> Result<DebugFrameBytes, String> {
    let renderer = renderer
        .as_ref()
        .ok_or_else(|| "renderer not yet constructed (no frame published yet)".to_string())?;
    let exported = renderer.export_buffer(buffer_id);
    let plane = exported
        .planes
        .first()
        .copied()
        .ok_or_else(|| "exported image has no plane layout".to_string())?;
    let bytes = exported.map_read().map_err(|e| e.to_string())?;
    Ok(DebugFrameBytes {
        bytes,
        stride: plane.stride,
        offset: plane.offset,
        width: exported.width,
        height: exported.height,
    })
}

/// Withdraw a published frame the host has not acquired, releasing its buffer
/// back to the ring.
///
/// The `published` slot holds the LATEST frame only. Publishing over a frame
/// the host never acquired drops its `PublishedFrame` on the floor, but the
/// ring still counts that buffer `in_flight` and only `release` clears it --
/// and the host will never release a frame it was never handed. Each such
/// overwrite therefore leaked one export buffer for good, until a host that
/// presents slower than the server sends starved the ring (`publish` returning
/// `None` forever).
///
/// Also run before a `FrameDimensions` resize: the unacquired frame names a
/// buffer from the old ring, and handing out its fd afterwards is a
/// use-after-close.
fn withdraw_unacquired(published: &Mutex<Option<PublishedFrame>>, renderer: &mut Option<Renderer>) {
    let stale = published.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let (Some(pf), Some(r)) = (stale, renderer.as_mut()) {
        r.withdraw_frame(pf.frame_id);
    }
}

/// Route one core event to the renderer, lazily constructing it on the
/// first `FrameDimensions` (the earliest point at which a size is known --
/// see the design doc's "Allocation timing" note). Returns whether a
/// render-worthy change happened, i.e. whether `flush`+`publish` are worth
/// running this batch.
#[allow(clippy::too_many_arguments)]
fn handle_core_event(
    ctx: &WgpuContext,
    renderer: &mut Option<Renderer>,
    ev: CoreEvent,
    queue: &EventQueue,
    published: &Mutex<Option<PublishedFrame>>,
    export_buffers: usize,
    host_visible: bool,
    preferred_modifiers: &[u64],
    open_h264_eagerly: bool,
    self_tx: &Sender<RenderMsg>,
    pool: Option<&UploadPool>,
) -> bool {
    // A bigger frame needs a bigger pool. Before the renderer sees the event:
    // the decode thread has already been decoding the new size's tiles.
    if let (CoreEvent::FrameDimensions { width, height }, Some(pool)) = (&ev, pool) {
        if let Err(e) = pool.ensure_for(ctx, *width, *height) {
            tracing::warn!(error = %e, "could not grow the upload pool; extra tiles use Vecs");
        }
    }
    if renderer.is_none() {
        let CoreEvent::FrameDimensions { width, height } = &ev else {
            tracing::warn!(
                ?ev,
                "core event before the first FrameDimensions; renderer not \
                 yet constructed, dropping"
            );
            return false;
        };
        match Renderer::new(
            ctx,
            *width,
            *height,
            export_buffers,
            preferred_modifiers,
            host_visible,
            open_h264_eagerly,
        ) {
            Ok(mut r) => {
                // A Sender into this thread's own channel, held by the decoder,
                // so `rx.recv()` cannot end by every Sender dropping while the
                // renderer lives. It does not need to: the loop ends on the
                // explicit `RenderMsg::Shutdown` the client always sends.
                let tx = self_tx.clone();
                r.set_h264_frame_ready_notify(Arc::new(move || {
                    // A send error means the loop has already exited.
                    let _ = tx.send(RenderMsg::H264FramesReady);
                }));
                if let Some(pool) = pool {
                    r.attach_upload_pool(ctx, pool);
                }
                *renderer = Some(r);
            }
            Err(e) => {
                queue.push(ClientEvent::Error {
                    message: format!("renderer init failed: {e}"),
                });
                return false;
            }
        }
        // `Renderer::new` already sized the framebuffer/ring to this
        // event's dimensions; there is nothing further to apply from it.
        return true;
    }
    if matches!(ev, CoreEvent::FrameDimensions { .. }) {
        withdraw_unacquired(published, renderer);
    }
    if let Err(e) = renderer
        .as_mut()
        .expect("renderer.is_none() handled above")
        .apply_event(ctx, ev)
    {
        queue.push(ClientEvent::Error {
            message: format!("renderer failed applying event: {e}"),
        });
        return false;
    }
    true
}

/// Route one slab tile to the renderer. Returns whether it was taken.
///
/// A tile that cannot be drawn (no renderer yet, no pool attached) still holds
/// a slot the decode thread issued, so it is handed back at once: dropping it
/// silently would pin its slab until the pool is gone.
fn handle_slab_tile(
    renderer: &mut Option<Renderer>,
    pool: Option<&UploadPool>,
    frame_seq: u32,
    tile_x: u8,
    tile_y: u8,
    slot: SlotRef,
) -> bool {
    let taken = renderer
        .as_mut()
        .is_some_and(|r| r.apply_slot_tile(frame_seq, tile_x, tile_y, slot));
    if !taken {
        if let Some(pool) = pool {
            let mut counts = vec![0u32; pool.slab_count()];
            counts[slot.slab()] = 1;
            pool.retire_now(&counts);
        }
    }
    taken
}

/// Answer [`RenderMsg::Redraw`]: re-damage everything. `true` when a publish
/// is due, i.e. there is a renderer to publish from.
fn redraw(renderer: &mut Option<Renderer>) -> bool {
    match renderer.as_mut() {
        Some(r) => {
            r.mark_all_dirty();
            true
        }
        None => false,
    }
}

/// Answer [`RenderMsg::H264FramesReady`]: blit what the decoder finished.
/// `true` when a publish is due -- including on an error, so the batch's
/// `flush` meets and reports it the usual way rather than this reporting it
/// a second time.
fn poll_h264(ctx: &WgpuContext, renderer: &mut Option<Renderer>) -> bool {
    let Some(r) = renderer.as_mut() else {
        return false;
    };
    match r.poll_h264(ctx) {
        Ok(drew) => drew,
        Err(e) => {
            tracing::warn!(error = %e, "H.264 late-frame blit failed");
            true
        }
    }
}

/// Dump the EXPORTED dmabuf's own pixels when `GHOSTFRAME_CLIENT_DUMP_EXPORT`
/// names a path prefix, up to `GHOSTFRAME_CLIENT_DUMP_COUNT` times.
///
/// The counterpart to the framebuffer dump, and the pair is the point. The
/// framebuffer dump reads through wgpu and so says whether *rendering* is
/// right; this reads the buffer the window actually imports, and so says
/// whether what leaves the client matches what it drew. Between them:
///
/// - framebuffer full, export full  -> the client is correct end to end and a
///   wrong window is the consumer's import or present path.
/// - framebuffer full, export partial -> the export ring's damage blit is
///   losing content, and the window is faithfully showing a bad buffer.
///
/// Only possible because the GLES export is LINEAR (see `export_gles.rs`);
/// `map_read` refuses a tiled or compressed buffer rather than returning
/// plausible garbage.
fn maybe_dump_export(
    renderer: &ghostframe_client_gpu::renderer::Renderer,
    pf: &PublishedFrame,
    written: &mut u64,
    dump: &ghostframe_client_gpu::config::DumpConfig,
) {
    let Some(prefix) = dump.export_prefix.as_deref() else {
        return;
    };
    // `GHOSTFRAME_CLIENT_DUMP_EVERY=n` samples every nth publish instead,
    // overwriting `<prefix>.latest.ppm`. A fixed count always lands in the
    // opening paint -- a server's first second is thousands of tiles -- so it
    // cannot answer "what does a buffer hold once things have settled", which
    // is a different question and the one that matters for a steady-state
    // artefact.
    let n = *written;
    *written += 1;
    if !dump.should_dump(n) {
        return;
    }

    let exported = renderer.export_buffer(pf.buffer_id);
    let bytes = match exported.map_read() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "export dump failed");
            return;
        }
    };
    let (w, h) = (exported.width, exported.height);
    let stride = exported.planes.first().map(|p| p.stride).unwrap_or(0) as usize;
    if stride == 0 {
        tracing::warn!("export dump: plane 0 has no stride");
        return;
    }

    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    let mut non_black = 0usize;
    for y in 0..h as usize {
        let row = &bytes[y * stride..y * stride + w as usize * 4];
        for px in row.chunks_exact(4) {
            if px[0] != 0 || px[1] != 0 || px[2] != 0 {
                non_black += 1;
            }
            ppm.extend_from_slice(&px[..3]);
        }
    }

    let path = if dump.every > 0 {
        format!("{prefix}.latest.ppm")
    } else {
        format!("{prefix}.{n}.ppm")
    };
    match std::fs::write(&path, &ppm) {
        Ok(()) => tracing::info!(
            path = %path,
            publish_n = n,
            buffer_id = pf.buffer_id,
            frame_id = pf.frame_id,
            damage_rects = pf.damage.len(),
            non_black_px = non_black,
            total_px = w as usize * h as usize,
            "dumped the EXPORTED dmabuf (what the window imports)"
        ),
        Err(e) => tracing::warn!(path = %path, error = %e, "export dump write failed"),
    }
}

/// Rolling tally of what the render thread is actually being handed.
///
/// `ghostframe-client-net` and `ghostframe-client-core` carry no tracing at
/// all, so before this there was no way to tell "the server is sending nothing"
/// apart from "tiles arrive and decode to nothing" -- and those need completely
/// different fixes. A black window looks identical either way.
///
/// Logged as one line per second rather than per event: at 60fps with a full
/// tile grid, per-event logging is both a flood and a measurable cost on a
/// weak CPU, which is the machine this instrumentation exists for.
#[derive(Default)]
struct EventTally {
    tile_ready: u64,
    /// Of `tile_ready`, the tiles that arrived in an upload slab. Their pixels
    /// are not scanned for `tile_ready_nonblack`: reading them back from
    /// write-combined memory on the render thread would cost more than the
    /// path saves, and only to feed a debug line.
    tile_slab: u64,
    /// `TileReady` tiles carrying at least one non-black pixel.
    ///
    /// Distinguishes the two explanations for a black framebuffer that no
    /// amount of GPU-side logging can separate: the upload path is broken, or
    /// the server is faithfully sending a black desktop (a headless Xorg whose
    /// WM never started is genuinely black, and the encoder has no opinion
    /// about that). If this is 0 while `tile_ready` is in the thousands, the
    /// client is working and the remote screen is blank.
    tile_ready_nonblack: u64,
    /// Whether `record` scans each `TileReady` for non-black pixels. That is
    /// a pass over every tile's bytes on the render thread, and the result
    /// only reaches the debug-level summary, so it runs only when that level
    /// was enabled when the tally was built.
    scan_nonblack: bool,
    tile_payload: u64,
    frame_dimensions: u64,
    needs_h264: u64,
    decode_error: u64,
    /// Tiles the GPU rejected after decode (see `Renderer::take_decode_errors`).
    gpu_decode_errors: u64,
    palette_updated: u64,
    other: u64,
    publishes: u64,
    /// `publish` calls that returned `None` because every export buffer was
    /// still in flight. Paired with `releases`, this says whether the ring is
    /// starved and, if so, which side stopped: no releases means the consumer
    /// is not presenting; releases without publishes means something else.
    publish_starved: u64,
    releases: u64,
}

impl EventTally {
    fn new() -> Self {
        Self::with_scan(tracing::enabled!(tracing::Level::DEBUG))
    }

    fn with_scan(scan_nonblack: bool) -> Self {
        EventTally {
            scan_nonblack,
            ..Self::default()
        }
    }

    fn record(&mut self, ev: &ghostframe_client_core::Event) {
        use ghostframe_client_core::Event as E;
        match ev {
            E::TileReady { rgba, .. } => {
                self.tile_ready += 1;
                if self.scan_nonblack
                    && rgba
                        .chunks_exact(4)
                        .any(|p| p[0] != 0 || p[1] != 0 || p[2] != 0)
                {
                    self.tile_ready_nonblack += 1;
                }
            }
            E::TilePayload { .. } => self.tile_payload += 1,
            E::FrameDimensions { .. } => self.frame_dimensions += 1,
            E::NeedsH264 { .. } => self.needs_h264 += 1,
            E::DecodeError { .. } => self.decode_error += 1,
            E::PaletteUpdated { .. } => self.palette_updated += 1,
            _ => self.other += 1,
        }
    }

    fn total(&self) -> u64 {
        self.tile_ready
            + self.tile_payload
            + self.frame_dimensions
            + self.needs_h264
            + self.decode_error
            + self.palette_updated
            + self.other
    }

    /// Emit and reset. `total() == 0` is the interesting case, not the boring
    /// one, so it is logged too -- "the client received nothing for a second"
    /// is precisely the fact a silent log would hide.
    fn flush_log(&mut self) {
        tracing::debug!(
            events = self.total(),
            tile_ready = self.tile_ready,
            tile_slab = self.tile_slab,
            tile_ready_nonblack = self.tile_ready_nonblack,
            tile_payload = self.tile_payload,
            frame_dimensions = self.frame_dimensions,
            needs_h264 = self.needs_h264,
            decode_error = self.decode_error,
            gpu_decode_errors = self.gpu_decode_errors,
            palette_updated = self.palette_updated,
            other = self.other,
            publishes = self.publishes,
            publish_starved = self.publish_starved,
            releases = self.releases,
            "render thread: 1s summary"
        );
        *self = Self::with_scan(self.scan_nonblack);
    }
}

/// How the render thread asks the net thread to send a reset request.
pub(crate) type ResetSink = Box<dyn Fn(ghostframe_client_core::ResetReason, u32) + Send>;

/// Build the GPU context *on this thread*, then run the render loop.
///
/// The context is constructed here rather than by the caller because a GL
/// context is current per-thread: `WgpuContext` records the thread that
/// created it and asserts on it (see `wgpu_ctx.rs`'s `assert_render_thread`),
/// and on the GLES backend using it from another thread is undefined
/// behaviour that merely happens to work sometimes. Constructing it on the
/// caller's thread and moving it here panicked this thread at the first
/// export -- a black window, because nothing else is wrong enough to fail
/// loudly.
///
/// `ready` carries the construction result back so `Client::connect` can
/// still report "no adapter" / "cannot export" synchronously, which is why
/// the caller built it in the first place. The caller blocks on it, so the
/// error surfaces from `connect` exactly as before; only the thread it is
/// built on changed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    ready: std::sync::mpsc::Sender<Result<Option<SlotAllocator>, crate::ClientError>>,
    rx: Receiver<RenderMsg>,
    queue: Arc<EventQueue>,
    published: Arc<Mutex<Option<PublishedFrame>>>,
    export_buffers: u32,
    host_visible: bool,
    preferred_modifiers: Vec<u64>,
    open_h264_eagerly: bool,
    reset_sink: ResetSink,
    self_tx: Sender<RenderMsg>,
    frame_interval: std::time::Duration,
) {
    let ctx = match WgpuContext::new() {
        Ok(ctx) => ctx,
        Err(e) => {
            let _ = ready.send(Err(crate::ClientError::Gpu(e)));
            return;
        }
    };
    // The upload slabs. They belong to the GPU context, not to a session's
    // renderer, so a renderer replaced mid-run keeps drawing from the same
    // pool; the decode thread gets the write side with the readiness message.
    // A backend that cannot make them is not an error -- tiles then travel as
    // `Vec`s, exactly as before.
    let (upload_pool, slot_allocator) =
        if ghostframe_client_gpu::config::UploadConfig::from_env().slabs_disabled {
            tracing::info!("upload slabs disabled by GHOSTFRAME_NO_UPLOAD_SLABS");
            (None, None)
        } else {
            match UploadPool::new(&ctx) {
                Ok((pool, alloc)) => (Some(pool), Some(alloc)),
                Err(e) => {
                    tracing::warn!(error = %e, "upload slabs unavailable; tiles travel as Vecs");
                    (None, None)
                }
            }
        };
    // Ignore a send failure: it only means `connect` gave up first, and there
    // is nothing useful to do about it here.
    let _ = ready.send(Ok(slot_allocator));
    let mut renderer: Option<Renderer> = None;
    let mut tally = EventTally::new();
    let mut export_dumps: u64 = 0;
    let mut flush_error_reported = false;
    // A publish was due but every export buffer was held. Nothing will make
    // the host's picture current on a quiet screen until a buffer comes back,
    // so the next `Release` must trigger the publish that was skipped.
    let mut publish_pending = false;
    let mut clock = FrameClock::new(frame_interval);
    // Events were applied (or a publish was retried) and `flush` + `publish`
    // have not run for them yet because the clock said wait.
    let mut render_due = false;
    let mut wake_at: Option<std::time::Instant> = None;
    let dump_cfg = ghostframe_client_gpu::config::DumpConfig::from_env();
    let mut last_summary = std::time::Instant::now();
    // `0` means "the embedder didn't specify a count"; a `Renderer` with
    // zero export buffers is a rejected config (it could never publish a
    // frame), so this thread -- not the renderer -- decides what "didn't
    // specify" defaults to.
    let export_buffers = if export_buffers == 0 {
        DEFAULT_EXPORT_BUFFER_COUNT as usize
    } else {
        export_buffers as usize
    };

    loop {
        // While a render is deferred, wake at its deadline even if nothing
        // arrives; otherwise block as before.
        let first = match wake_at {
            Some(t) => {
                match rx.recv_timeout(t.saturating_duration_since(std::time::Instant::now())) {
                    Ok(m) => Some(m),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match rx.recv() {
                Ok(m) => Some(m),
                // Every `Sender` was dropped.
                Err(_) => break,
            },
        };
        let mut got_event = false;
        let mut released = false;
        let mut shutdown = false;

        if let Some(first) = first {
            match first {
                RenderMsg::Shutdown => break,
                RenderMsg::Release(id) => {
                    released = true;
                    tally.releases += 1;
                    if let Some(r) = renderer.as_mut() {
                        r.release(id);
                    }
                }
                RenderMsg::Core(ev) => {
                    tally.record(&ev);
                    got_event |= handle_core_event(
                        &ctx,
                        &mut renderer,
                        ev,
                        &queue,
                        &published,
                        export_buffers,
                        host_visible,
                        &preferred_modifiers,
                        open_h264_eagerly,
                        &self_tx,
                        upload_pool.as_ref(),
                    );
                }
                RenderMsg::SlabTile {
                    frame_seq,
                    tile_x,
                    tile_y,
                    slot,
                } => {
                    tally.tile_ready += 1;
                    tally.tile_slab += 1;
                    got_event |= handle_slab_tile(
                        &mut renderer,
                        upload_pool.as_ref(),
                        frame_seq,
                        tile_x,
                        tile_y,
                        slot,
                    );
                }
                RenderMsg::DebugMapFrame(buffer_id, reply) => {
                    let _ = reply.send(handle_debug_map_frame(&renderer, buffer_id));
                }
                RenderMsg::H264FramesReady => {
                    got_event |= poll_h264(&ctx, &mut renderer);
                }
                RenderMsg::Redraw => {
                    got_event |= redraw(&mut renderer);
                }
            }
        }

        // Drain what is already queued without blocking, so a burst lands in
        // one render pass instead of one per message -- but BOUNDED.
        //
        // An unbounded drain is a starvation hazard: `try_recv` only stops when
        // the channel is momentarily empty, and every event here costs render
        // thread time (and the flush GPU work), so against a producer faster
        // than the consumer there is no guaranteed bound on how long `flush`
        // and `publish` below are deferred. The cap is one screen's worth of
        // tiles at 1080p (60x34 = 2040, rounded up), which makes the invariant
        // easy to state: never apply more than a full frame's tiles before the
        // clock is consulted again. The bound on how long `flush` and `publish`
        // are deferred is therefore time -- one frame interval (two while the
        // host holds the last frame) plus at most one drain -- not a batch
        // size. On a machine that keeps up, the channel empties first and this
        // never binds.
        //
        // Do NOT read this as the fix for a low frame rate -- it was tried as
        // exactly that and measured not to be: on a Pinebook Pro the rate was
        // ~3 publishes/sec before and after; capping the drain only changed
        // the batch size. The cost was per-tile `write_texture` calls, since
        // replaced by `Renderer::upload_pending_rgba`'s batching (one call per
        // large rectangle, one compute dispatch for all other tiles).
        const MAX_DRAIN: usize = 2048;
        let mut drained = 0usize;
        while drained < MAX_DRAIN {
            drained += 1;
            match rx.try_recv() {
                Ok(RenderMsg::Shutdown) => {
                    shutdown = true;
                    break;
                }
                Ok(RenderMsg::Release(id)) => {
                    released = true;
                    tally.releases += 1;
                    if let Some(r) = renderer.as_mut() {
                        r.release(id);
                    }
                }
                Ok(RenderMsg::Core(ev)) => {
                    tally.record(&ev);
                    got_event |= handle_core_event(
                        &ctx,
                        &mut renderer,
                        ev,
                        &queue,
                        &published,
                        export_buffers,
                        host_visible,
                        &preferred_modifiers,
                        open_h264_eagerly,
                        &self_tx,
                        upload_pool.as_ref(),
                    );
                }
                Ok(RenderMsg::SlabTile {
                    frame_seq,
                    tile_x,
                    tile_y,
                    slot,
                }) => {
                    tally.tile_ready += 1;
                    tally.tile_slab += 1;
                    got_event |= handle_slab_tile(
                        &mut renderer,
                        upload_pool.as_ref(),
                        frame_seq,
                        tile_x,
                        tile_y,
                        slot,
                    );
                }
                Ok(RenderMsg::DebugMapFrame(buffer_id, reply)) => {
                    let _ = reply.send(handle_debug_map_frame(&renderer, buffer_id));
                }
                Ok(RenderMsg::H264FramesReady) => {
                    got_event |= poll_h264(&ctx, &mut renderer);
                }
                Ok(RenderMsg::Redraw) => {
                    got_event |= redraw(&mut renderer);
                }
                Err(_) => break,
            }
        }

        // A decode error in this batch asks the server for an IDR.
        if let Some((reason, frame_seq)) = renderer.as_mut().and_then(|r| r.take_reset_request()) {
            reset_sink(reason, frame_seq);
        }

        render_due |= got_event || (released && publish_pending);
        let woke_for = wake_at.take();
        let mut run_render = false;
        if render_due {
            let pace_now = std::time::Instant::now();
            let unacquired = published
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some();
            match clock.decide(pace_now, unacquired) {
                Pace::Now => {
                    clock.attempted(pace_now, woke_for);
                    render_due = false;
                    run_render = true;
                }
                Pace::WaitUntil(t) => wake_at = Some(t),
            }
        }
        if run_render {
            // The slot is latest-wins: whatever the host has not picked up is
            // superseded by what we publish next. Release it first, so its
            // buffer is free for this very publish.
            withdraw_unacquired(&published, &mut renderer);
            if let Some(r) = renderer.as_mut() {
                // Flush and publish in one GPU submit: the export blit is
                // recorded into the flush's own command encoder.
                let flushed = r.flush_and_publish(&ctx);
                if let Err(e) = &flushed {
                    // E.g. device loss while waiting on the GPU. Report it
                    // (once: the queue is unbounded and a lost device fails
                    // every later flush) and skip this publish rather than
                    // panic the thread.
                    if !flush_error_reported {
                        flush_error_reported = true;
                        queue.push(ClientEvent::Error {
                            message: format!("renderer flush failed: {e}"),
                        });
                    }
                } else {
                    flush_error_reported = false;
                }
                // GPU-detected decode failures. They belong on the feedback
                // stream as DECODE_ERROR, but `ClientCore` owns the
                // rate-limited batcher and the net thread owns `ClientCore`;
                // there is no render-to-net command for this yet, so they
                // are logged and counted.
                let gpu_errors = r.take_decode_errors();
                if !gpu_errors.is_empty() {
                    tally.gpu_decode_errors += gpu_errors.len() as u64;
                    tracing::warn!(
                        count = gpu_errors.len(),
                        first = ?gpu_errors[0],
                        "GPU tile decode errors (not yet sent to server)"
                    );
                }
                let flush_ok = flushed.is_ok();
                let published_frame = flushed.unwrap_or(None);
                if flush_ok && published_frame.is_none() {
                    tally.publish_starved += 1;
                }
                publish_pending = flush_ok && published_frame.is_none();
                if let Some(pf) = published_frame {
                    tally.publishes += 1;
                    maybe_dump_export(r, &pf, &mut export_dumps, &dump_cfg);
                    let frame_id = pf.frame_id;
                    *published.lock().unwrap_or_else(|e| e.into_inner()) = Some(pf);
                    queue.push(ClientEvent::FrameReady { frame_id });
                }
            }
        }

        // Once a second while the loop is running -- including a second in
        // which nothing was applied. See `EventTally`'s doc for why a quiet
        // second is the interesting case. The loop blocks in `recv` when
        // nothing is deferred, so a fully idle client logs nothing until the
        // next message; the first line after an idle spell covers all of it.
        //
        // `saturating_duration_since`, not `elapsed()`: the latter is banned in
        // this workspace (see the clippy config) because it reads the real wall
        // clock even on an Instant derived from a virtual one, and silently
        // returns 0ns forever instead of erroring.
        let now = std::time::Instant::now();
        if now.saturating_duration_since(last_summary) >= std::time::Duration::from_secs(1) {
            if let Some(pool) = &upload_pool {
                let st = pool.stats();
                tracing::debug!(
                    exhausted = st.exhausted,
                    sync_ms = st.sync_ns as f64 / 1e6,
                    sync_end_ms = st.sync_end_ns as f64 / 1e6,
                    sync_calls = st.sync_calls,
                    slabs = ?st.slabs,
                    "upload pool (issued, consumed) per slab; totals since start"
                );
            }
            tally.flush_log();
            last_summary = now;
        }

        if shutdown {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn nonblack_scan_runs_only_when_enabled() {
        let ev = CoreEvent::TileReady {
            frame_seq: 0,
            tile_x: 0,
            tile_y: 0,
            rgba: vec![9u8; 32 * 32 * 4],
        };
        let mut on = EventTally::with_scan(true);
        let mut off = EventTally::with_scan(false);
        on.record(&ev);
        off.record(&ev);
        assert_eq!((on.tile_ready, on.tile_ready_nonblack), (1, 1));
        assert_eq!((off.tile_ready, off.tile_ready_nonblack), (1, 0));
        // The setting survives the once-a-second reset.
        on.flush_log();
        assert!(on.scan_nonblack && on.tile_ready == 0);
    }

    /// A slab tile that arrives before any renderer exists cannot be drawn, but
    /// its slot was issued: it must be handed back or its slab is pinned.
    #[test]
    fn a_slab_tile_with_no_renderer_is_retired_at_once() {
        use ghostframe_client_core::TileSink;
        let Ok(ctx) = WgpuContext::new() else {
            eprintln!("SKIP: no GPU");
            return;
        };
        let (pool, mut alloc) = UploadPool::with_slabs(&ctx, 1).expect("pool");
        let mut out = alloc.acquire().expect("slot");
        out.row_mut(0).fill(1);
        let slot = alloc.commit(0, 0, 0);
        alloc.end_cpu_writes();
        let mut renderer = None;
        assert!(!handle_slab_tile(&mut renderer, Some(&pool), 0, 0, 0, slot));
        assert_eq!(pool.consumed(0), 1);
    }

    fn tile(x: u8) -> RenderMsg {
        RenderMsg::Core(CoreEvent::TileReady {
            frame_seq: 0,
            tile_x: x,
            tile_y: 0,
            rgba: vec![0x80; 32 * 32 * 4],
        })
    }

    /// Wait for the next `FrameReady`, or `None` after `timeout`.
    fn next_frame_ready(queue: &EventQueue, timeout: Duration) -> Option<u32> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match queue.pop() {
                Some(ClientEvent::FrameReady { frame_id }) => return Some(frame_id),
                Some(ClientEvent::Error { message }) => panic!("render thread error: {message}"),
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        None
    }

    /// A host that never acquires a frame must not be able to starve the export
    /// ring. Two buffers, a host that acquires nothing, and far more publishes
    /// than buffers: every publish overwrites the unacquired frame in the
    /// `published` slot, and unless that overwrite releases the superseded
    /// frame's buffer, the ring runs dry after two and `FrameReady` stops.
    #[test]
    fn overwriting_an_unacquired_frame_releases_its_buffer() {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (tx, rx) = std::sync::mpsc::channel();
        let queue = Arc::new(EventQueue::new().expect("event queue"));
        let published = Arc::new(Mutex::new(None));
        let handle = {
            let (queue, published, self_tx) =
                (Arc::clone(&queue), Arc::clone(&published), tx.clone());
            std::thread::spawn(move || {
                run(
                    ready_tx,
                    rx,
                    queue,
                    published,
                    2,
                    false,
                    vec![],
                    false,
                    Box::new(|_, _| {}),
                    self_tx,
                    Duration::ZERO,
                )
            })
        };
        if ready_rx.recv().expect("ready").is_err() {
            eprintln!("SKIP: no GPU adapter that can export dmabufs");
            let _ = handle.join();
            return;
        }

        tx.send(RenderMsg::Core(CoreEvent::FrameDimensions {
            width: 128,
            height: 128,
        }))
        .unwrap();
        // Strictly more publishes than export buffers (2). Each batch is
        // separated by waiting for its FrameReady so they cannot coalesce.
        let publishes = 6;
        let mut seen = 0;
        for i in 0..publishes {
            tx.send(tile((i % 4) as u8)).unwrap();
            if next_frame_ready(&queue, Duration::from_secs(10)).is_none() {
                break;
            }
            seen += 1;
        }
        tx.send(RenderMsg::Shutdown).unwrap();
        handle.join().unwrap();
        // Every publish after the first superseded a frame the host never saw,
        // so the host's picture is older than any one buffer's watermark. Its
        // damage must therefore cover the whole surface, not just the single
        // tile this frame changed.
        let last = published.lock().unwrap().take().expect("a published frame");
        let damaged: u64 = last.damage.iter().map(|r| r.w as u64 * r.h as u64).sum();
        assert_eq!(
            damaged,
            128 * 128,
            "withdrawn frames lost their damage: {:?}",
            last.damage
        );
        assert_eq!(
            seen, publishes,
            "ring starved after {seen} publishes with 2 buffers: an overwritten, \
             never-acquired frame kept its buffer in flight"
        );
    }

    /// Spawn a render thread with `buffers` export buffers; `None` when this
    /// machine cannot export dmabufs.
    #[allow(clippy::type_complexity)]
    fn spawn_render_thread(
        buffers: u32,
    ) -> Option<(
        Sender<RenderMsg>,
        Arc<EventQueue>,
        Arc<Mutex<Option<PublishedFrame>>>,
        std::thread::JoinHandle<()>,
    )> {
        spawn_paced_render_thread(buffers, Duration::ZERO)
    }

    /// As [`spawn_render_thread`], with the frame clock set to `interval`.
    /// The older tests use zero (no pacing) so each event publishes on its own.
    #[allow(clippy::type_complexity)]
    fn spawn_paced_render_thread(
        buffers: u32,
        interval: Duration,
    ) -> Option<(
        Sender<RenderMsg>,
        Arc<EventQueue>,
        Arc<Mutex<Option<PublishedFrame>>>,
        std::thread::JoinHandle<()>,
    )> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (tx, rx) = std::sync::mpsc::channel();
        let queue = Arc::new(EventQueue::new().expect("event queue"));
        let published = Arc::new(Mutex::new(None));
        let handle = {
            let (queue, published, self_tx) =
                (Arc::clone(&queue), Arc::clone(&published), tx.clone());
            std::thread::spawn(move || {
                run(
                    ready_tx,
                    rx,
                    queue,
                    published,
                    buffers,
                    false,
                    vec![],
                    false,
                    Box::new(|_, _| {}),
                    self_tx,
                    interval,
                )
            })
        };
        if ready_rx.recv().expect("ready").is_err() {
            eprintln!("SKIP: no GPU adapter that can export dmabufs");
            let _ = handle.join();
            return None;
        }
        Some((tx, queue, published, handle))
    }

    fn pixels(rects: &[ghostframe_client_gpu::coalesce::Rect]) -> u64 {
        rects.iter().map(|r| r.w as u64 * r.h as u64).sum()
    }

    /// After a quiet period a `Redraw` publishes a frame whose damage, both
    /// against its own buffer and against the previous frame, is the whole
    /// surface -- the host lost its contents and nothing else is changing.
    #[test]
    fn redraw_after_quiet_publishes_the_whole_surface() {
        let Some((tx, queue, published, handle)) = spawn_render_thread(3) else {
            return;
        };
        tx.send(RenderMsg::Core(CoreEvent::FrameDimensions {
            width: 128,
            height: 128,
        }))
        .unwrap();
        let first = next_frame_ready(&queue, Duration::from_secs(10)).expect("first frame");
        published.lock().unwrap().take();
        tx.send(RenderMsg::Release(first)).unwrap();
        tx.send(tile(0)).unwrap();
        let second = next_frame_ready(&queue, Duration::from_secs(10)).expect("tile frame");
        published.lock().unwrap().take();
        tx.send(RenderMsg::Release(second)).unwrap();
        assert!(
            next_frame_ready(&queue, Duration::from_millis(300)).is_none(),
            "a quiet screen must not publish"
        );

        tx.send(RenderMsg::Redraw).unwrap();
        next_frame_ready(&queue, Duration::from_secs(10)).expect("redraw publishes");
        let pf = published.lock().unwrap().take().expect("redraw frame");
        tx.send(RenderMsg::Shutdown).unwrap();
        handle.join().unwrap();
        assert_eq!(pixels(&pf.damage), 128 * 128);
        let since = pf.since_previous.expect("follows an earlier frame");
        assert_eq!(pixels(&since.damage), 128 * 128, "{:?}", since.damage);
    }

    /// A publish skipped because every buffer is held must happen once a
    /// buffer returns, with no new event to trigger it.
    #[test]
    fn a_starved_publish_is_retried_when_a_buffer_is_released() {
        let Some((tx, queue, published, handle)) = spawn_render_thread(2) else {
            return;
        };
        tx.send(RenderMsg::Core(CoreEvent::FrameDimensions {
            width: 128,
            height: 128,
        }))
        .unwrap();
        // The host acquires both buffers and keeps them.
        let held = next_frame_ready(&queue, Duration::from_secs(10)).expect("frame 1");
        published.lock().unwrap().take();
        tx.send(tile(0)).unwrap();
        next_frame_ready(&queue, Duration::from_secs(10)).expect("frame 2");
        published.lock().unwrap().take();

        tx.send(tile(1)).unwrap();
        assert!(
            next_frame_ready(&queue, Duration::from_millis(300)).is_none(),
            "no free buffer: nothing can publish yet"
        );
        tx.send(RenderMsg::Release(held)).unwrap();
        let retried = next_frame_ready(&queue, Duration::from_secs(10));
        tx.send(RenderMsg::Shutdown).unwrap();
        handle.join().unwrap();
        assert!(retried.is_some(), "the starved publish was never retried");
    }
    /// Count `FrameReady`s arriving within `window`, releasing each so the ring
    /// never starves, and acquiring it so the host is never "behind".
    fn count_frames(
        tx: &Sender<RenderMsg>,
        queue: &EventQueue,
        published: &Mutex<Option<PublishedFrame>>,
        window: Duration,
    ) -> u32 {
        let deadline = Instant::now() + window;
        let mut n = 0;
        while let Some(id) =
            next_frame_ready(queue, deadline.saturating_duration_since(Instant::now()))
        {
            published.lock().unwrap().take();
            tx.send(RenderMsg::Release(id)).unwrap();
            n += 1;
        }
        n
    }

    const TEST_INTERVAL: Duration = Duration::from_millis(400);

    /// A burst of events inside one interval costs one flush + publish, not
    /// one per event -- and that one still happens without any further event.
    #[test]
    fn a_burst_within_one_interval_publishes_once_at_the_deadline() {
        let Some((tx, queue, published, handle)) = spawn_paced_render_thread(3, TEST_INTERVAL)
        else {
            return;
        };
        tx.send(RenderMsg::Core(CoreEvent::FrameDimensions {
            width: 256,
            height: 128,
        }))
        .unwrap();
        let first = next_frame_ready(&queue, Duration::from_secs(10)).expect("first frame");
        published.lock().unwrap().take();
        tx.send(RenderMsg::Release(first)).unwrap();

        let burst = 8;
        // Spread out (they would otherwise drain as one batch whether or not
        // anything paces them), yet well inside one interval: unpaced this is
        // one publish per tile.
        for i in 0..burst {
            tx.send(tile(i)).unwrap();
            std::thread::sleep(Duration::from_millis(15));
        }
        let frames = count_frames(&tx, &queue, &published, TEST_INTERVAL * 3);
        tx.send(RenderMsg::Shutdown).unwrap();
        handle.join().unwrap();
        assert_eq!(
            frames, 1,
            "{burst} events in one interval must publish once"
        );
    }

    /// An event after a quiet period is not delayed, and a deferred one lands
    /// by the end of the interval with no further event to wake the loop.
    #[test]
    fn isolated_event_is_immediate_and_a_deferred_one_lands_by_the_deadline() {
        let Some((tx, queue, published, handle)) = spawn_paced_render_thread(3, TEST_INTERVAL)
        else {
            return;
        };
        tx.send(RenderMsg::Core(CoreEvent::FrameDimensions {
            width: 256,
            height: 128,
        }))
        .unwrap();
        let first = next_frame_ready(&queue, Duration::from_secs(10)).expect("first frame");
        published.lock().unwrap().take();
        tx.send(RenderMsg::Release(first)).unwrap();

        // Quiet for longer than an interval, then one event.
        std::thread::sleep(TEST_INTERVAL * 2);
        let t0 = Instant::now();
        tx.send(tile(0)).unwrap();
        let second = next_frame_ready(&queue, Duration::from_secs(10)).expect("isolated frame");
        let isolated = Instant::now().saturating_duration_since(t0);
        published.lock().unwrap().take();
        tx.send(RenderMsg::Release(second)).unwrap();
        assert!(
            isolated < TEST_INTERVAL / 2,
            "an isolated event waited {isolated:?}"
        );

        // Right behind it: deferred to the deadline, with nothing else sent.
        let t1 = Instant::now();
        tx.send(tile(1)).unwrap();
        let third = next_frame_ready(&queue, Duration::from_secs(10)).expect("deferred frame");
        let deferred = Instant::now().saturating_duration_since(t1);
        tx.send(RenderMsg::Release(third)).unwrap();
        tx.send(RenderMsg::Shutdown).unwrap();
        handle.join().unwrap();
        // The test thread polls every 5 ms, so allow slack on the high side
        // only; the low side proves the frame really was deferred.
        assert!(
            deferred >= TEST_INTERVAL / 2,
            "the second event published after {deferred:?}: not deferred"
        );
        assert!(
            deferred < TEST_INTERVAL * 3,
            "deferred publish took {deferred:?}: the loop did not wake at the deadline"
        );
    }

    /// Shutdown while a render is deferred must not wait for the deadline.
    #[test]
    fn shutdown_does_not_wait_for_a_deferred_render() {
        let Some((tx, queue, published, handle)) =
            spawn_paced_render_thread(3, Duration::from_secs(30))
        else {
            return;
        };
        tx.send(RenderMsg::Core(CoreEvent::FrameDimensions {
            width: 128,
            height: 128,
        }))
        .unwrap();
        next_frame_ready(&queue, Duration::from_secs(10)).expect("first frame");
        published.lock().unwrap().take();
        tx.send(tile(0)).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let t0 = Instant::now();
        tx.send(RenderMsg::Shutdown).unwrap();
        handle.join().unwrap();
        assert!(Instant::now().saturating_duration_since(t0) < Duration::from_secs(5));
    }
}
