// SPDX-License-Identifier: MIT OR Apache-2.0
//! The one place this crate hands a command encoder to the queue.
//!
//! Each `queue.submit` ends, in wgpu-hal's GLES backend, in a `glFlush` and a
//! fence, and on panfrost a flush ends the job batch: a separate kernel
//! submission. So the number of submits per frame is a cost in its own right,
//! and the renderer's contract is one per flush (see
//! [`Renderer::flush`](crate::renderer::Renderer::flush)).
//!
//! Routing every submit through here is what makes that contract testable: the
//! counter below is per thread, so parallel tests (and a render thread next to
//! a net thread) never see each other's submits.

use std::cell::Cell;

thread_local! {
    static SUBMITS: Cell<u64> = const { Cell::new(0) };
}

/// Finish `encoder` and submit it, counting the submit.
pub(crate) fn submit(queue: &wgpu::Queue, encoder: wgpu::CommandEncoder) -> wgpu::SubmissionIndex {
    SUBMITS.with(|c| c.set(c.get().wrapping_add(1)));
    queue.submit(std::iter::once(encoder.finish()))
}

/// Submits made through this module on the calling thread so far.
pub(crate) fn count() -> u64 {
    SUBMITS.with(Cell::get)
}

/// Open an encoder, let `record` fill it, and submit it if `record` says it
/// recorded anything. For callers that are not batching into a frame's
/// encoder: tests, benches, one-off passes.
pub(crate) fn submit_now(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    record: impl FnOnce(&mut wgpu::CommandEncoder) -> bool,
) {
    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some(label) });
    if record(&mut encoder) {
        submit(queue, encoder);
    }
}
