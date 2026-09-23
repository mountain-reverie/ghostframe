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

/// Route one core event to the renderer, lazily constructing it on the
/// first `FrameDimensions` (the earliest point at which a size is known --
/// see the design doc's "Allocation timing" note). Returns whether a
/// render-worthy change happened, i.e. whether `flush`+`publish` are worth
/// running this batch.
fn handle_core_event(
    ctx: &WgpuContext,
    renderer: &mut Option<Renderer>,
    ev: CoreEvent,
    queue: &EventQueue,
    export_buffers: usize,
    host_visible: bool,
    preferred_modifiers: &[u64],
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
    renderer
        .as_mut()
        .expect("renderer.is_none() handled above")
        .apply_event(ctx, &ev);
    true
}

pub(crate) fn run(
    ctx: WgpuContext,
    rx: Receiver<RenderMsg>,
    queue: Arc<EventQueue>,
    published: Arc<Mutex<Option<PublishedFrame>>>,
    export_buffers: u32,
    host_visible: bool,
    preferred_modifiers: Vec<u64>,
) {
    let mut renderer: Option<Renderer> = None;
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
                if let Some(r) = renderer.as_mut() {
                    r.release(id);
                }
            }
            RenderMsg::Core(ev) => {
                got_event |= handle_core_event(
                    &ctx,
                    &mut renderer,
                    ev,
                    &queue,
                    export_buffers,
                    host_visible,
                    &preferred_modifiers,
                );
            }
            RenderMsg::DebugMapFrame(buffer_id, reply) => {
                let _ = reply.send(handle_debug_map_frame(&renderer, buffer_id));
            }
        }

        // Drain whatever else is already queued without blocking, so a
        // burst of events lands in one render pass instead of one per
        // message.
        loop {
            match rx.try_recv() {
                Ok(RenderMsg::Shutdown) => {
                    shutdown = true;
                    break;
                }
                Ok(RenderMsg::Release(id)) => {
                    if let Some(r) = renderer.as_mut() {
                        r.release(id);
                    }
                }
                Ok(RenderMsg::Core(ev)) => {
                    got_event |= handle_core_event(
                        &ctx,
                        &mut renderer,
                        ev,
                        &queue,
                        export_buffers,
                        host_visible,
                        &preferred_modifiers,
                    );
                }
                Ok(RenderMsg::DebugMapFrame(buffer_id, reply)) => {
                    let _ = reply.send(handle_debug_map_frame(&renderer, buffer_id));
                }
                Err(_) => break,
            }
        }

        if got_event {
            if let Some(r) = renderer.as_mut() {
                r.flush(&ctx);
                if let Some(pf) = r.publish(&ctx) {
                    let frame_id = pf.frame_id;
                    *published.lock().unwrap_or_else(|e| e.into_inner()) = Some(pf);
                    queue.push(ClientEvent::FrameReady { frame_id });
                }
            }
        }

        if shutdown {
            break;
        }
    }
}
