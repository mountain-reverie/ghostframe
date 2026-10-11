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

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use ghostframe_client_core::Event as CoreEvent;
use ghostframe_client_gpu::renderer::Renderer;
use ghostframe_client_gpu::ring::PublishedFrame;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;

use crate::event::{ClientEvent, EventQueue};
use crate::DebugFrameBytes;

/// Default export-ring buffer count used when [`crate::Config::n_export_buffers`]
/// is `0`. Kept here, in the caller, rather than inside `Renderer` -- the
/// renderer treats 0 as a rejected configuration (see
/// [`ghostframe_client_gpu::GpuError::NoExportBuffers`]), and it is this
/// thread's job to decide what a client that didn't specify a count gets.
const DEFAULT_EXPORT_BUFFER_COUNT: u32 = 3;

pub(crate) enum RenderMsg {
    Core(CoreEvent),
    Release(u32),
    /// Map a published frame's exported dmabuf and reply with its bytes.
    /// Test and diagnostic use only -- see [`crate::Client::debug_map_frame`].
    /// The render thread owns the `Renderer` (and therefore the
    /// `ExportedImage`), so this round-trips through here rather than
    /// letting the caller reach the buffer directly.
    DebugMapFrame(u32, Sender<Result<DebugFrameBytes, String>>),
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
fn withdraw_unacquired(
    published: &Mutex<Option<PublishedFrame>>,
    renderer: &mut Option<Renderer>,
) {
    let stale = published.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let (Some(pf), Some(r)) = (stale, renderer.as_mut()) {
        r.release(pf.frame_id);
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
) -> bool {
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
            Ok(r) => *renderer = Some(r),
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
    /// `TileReady` tiles carrying at least one non-black pixel.
    ///
    /// Distinguishes the two explanations for a black framebuffer that no
    /// amount of GPU-side logging can separate: the upload path is broken, or
    /// the server is faithfully sending a black desktop (a headless Xorg whose
    /// WM never started is genuinely black, and the encoder has no opinion
    /// about that). If this is 0 while `tile_ready` is in the thousands, the
    /// client is working and the remote screen is blank.
    tile_ready_nonblack: u64,
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
    fn record(&mut self, ev: &ghostframe_client_core::Event) {
        use ghostframe_client_core::Event as E;
        match ev {
            E::TileReady { rgba, .. } => {
                self.tile_ready += 1;
                if rgba
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
        *self = Self::default();
    }
}

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
    ready: std::sync::mpsc::Sender<Result<(), crate::ClientError>>,
    rx: Receiver<RenderMsg>,
    queue: Arc<EventQueue>,
    published: Arc<Mutex<Option<PublishedFrame>>>,
    export_buffers: u32,
    host_visible: bool,
    preferred_modifiers: Vec<u64>,
    open_h264_eagerly: bool,
) {
    let ctx = match WgpuContext::new() {
        Ok(ctx) => {
            // Ignore a send failure: it only means `connect` gave up first,
            // and there is nothing useful to do about it here.
            let _ = ready.send(Ok(()));
            ctx
        }
        Err(e) => {
            let _ = ready.send(Err(crate::ClientError::Gpu(e)));
            return;
        }
    };
    let mut renderer: Option<Renderer> = None;
    let mut tally = EventTally::default();
    let mut export_dumps: u64 = 0;
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

    while let Ok(first) = rx.recv() {
        // `Err` here means every `Sender` was dropped; the loop condition
        // above already exits for us.
        let mut got_event = false;
        let mut shutdown = false;

        match first {
            RenderMsg::Shutdown => break,
            RenderMsg::Release(id) => {
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
                );
            }
            RenderMsg::DebugMapFrame(buffer_id, reply) => {
                let _ = reply.send(handle_debug_map_frame(&renderer, buffer_id));
            }
        }

        // Drain what is already queued without blocking, so a burst lands in
        // one render pass instead of one per message -- but BOUNDED.
        //
        // An unbounded drain is a starvation hazard: `try_recv` only stops when
        // the channel is momentarily empty, and every event here does GPU work
        // (`upload_rgba_tile` -> `write_texture`), so against a producer faster
        // than the consumer there is no guaranteed bound on how long `flush`
        // and `publish` below are deferred. The cap is one screen's worth of
        // tiles at 1080p (60x34 = 2040, rounded up), which makes the invariant
        // easy to state: never batch more than a full frame before presenting
        // one. On a machine that keeps up, the channel empties first and this
        // never binds.
        //
        // Do NOT read this as the fix for a low frame rate -- it was tried as
        // exactly that and measured not to be. On a Pinebook Pro against a live
        // server the rate was ~3 publishes/sec before and after; capping the
        // drain only changed the batch size. `publish_starved=0` throughout, so
        // the export ring was never the constraint either. The actual cost is
        // per-tile upload: ~2049 events take ~1s, i.e. roughly 0.5 ms for a
        // 32x32 `write_texture`. Fixing the frame rate means attacking that --
        // most likely batching tiles into one upload per flush, or the
        // framebuffer texture being AFBC-compressed on panfrost so every
        // partial write costs a decompress/recompress of the touched blocks
        // (the same defect `export_gles.rs` documents for the export, which is
        // fixed there and NOT here).
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
                    );
                }
                Ok(RenderMsg::DebugMapFrame(buffer_id, reply)) => {
                    let _ = reply.send(handle_debug_map_frame(&renderer, buffer_id));
                }
                Err(_) => break,
            }
        }

        if got_event {
            // The slot is latest-wins: whatever the host has not picked up is
            // superseded by what we publish next. Release it first, so its
            // buffer is free for this very publish.
            withdraw_unacquired(&published, &mut renderer);
            if let Some(r) = renderer.as_mut() {
                let flushed = r.flush(&ctx);
                if let Err(e) = &flushed {
                    // E.g. device loss while waiting on the GPU. Report it
                    // and skip this publish rather than panic the thread.
                    queue.push(ClientEvent::Error {
                        message: format!("renderer flush failed: {e}"),
                    });
                }
                // GPU-detected decode failures. They belong on the feedback
                // stream as DECODE_ERROR, but `ClientCore` owns the
                // rate-limited batcher and the net thread owns `ClientCore`;
                // there is no render-to-net command for this yet, so they
                // are logged and counted.
                for err in r.take_decode_errors() {
                    tally.gpu_decode_errors += 1;
                    tracing::warn!(?err, "GPU tile decode error (not yet sent to server)");
                }
                let published_frame = if flushed.is_ok() {
                    r.publish(&ctx)
                } else {
                    None
                };
                if flushed.is_ok() && published_frame.is_none() {
                    tally.publish_starved += 1;
                }
                if let Some(pf) = published_frame {
                    tally.publishes += 1;
                    maybe_dump_export(r, &pf, &mut export_dumps, &dump_cfg);
                    let frame_id = pf.frame_id;
                    *published.lock().unwrap_or_else(|e| e.into_inner()) = Some(pf);
                    queue.push(ClientEvent::FrameReady { frame_id });
                }
            }
        }

        // Once a second, whatever happened -- including nothing. See
        // `EventTally`'s doc for why a quiet second is the interesting case.
        //
        // `saturating_duration_since`, not `elapsed()`: the latter is banned in
        // this workspace (see the clippy config) because it reads the real wall
        // clock even on an Instant derived from a virtual one, and silently
        // returns 0ns forever instead of erroring.
        let now = std::time::Instant::now();
        if now.saturating_duration_since(last_summary) >= std::time::Duration::from_secs(1) {
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
            let (queue, published) = (Arc::clone(&queue), Arc::clone(&published));
            std::thread::spawn(move || run(ready_tx, rx, queue, published, 2, false, vec![], false))
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
        assert_eq!(
            seen, publishes,
            "ring starved after {seen} publishes with 2 buffers: an overwritten, \
             never-acquired frame kept its buffer in flight"
        );
    }
}
