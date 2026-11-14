// SPDX-License-Identifier: MIT OR Apache-2.0
//! Reading a texture back to the CPU, on every target.
//!
//! `wgpu::Buffer::map_async` takes a callback, and the callback runs when the
//! device is polled (native) or when the browser's event loop gets to it (web).
//! [`read_texture_rgba8`] turns that into a future with a one-shot channel, so
//! the same code serves the CDF 5/3 self-test on both. A caller on a thread
//! with no event loop uses [`block_on_device`], which polls the device between
//! polls of the future.

use crate::GpuError;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

struct Slot<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

/// The receiving half of [`oneshot`].
struct Receiver<T>(Arc<Mutex<Slot<T>>>);

/// A one-shot channel whose sender is a `Send` closure (what `map_async`
/// takes) and whose receiver is a future.
fn oneshot<T: Send + 'static>() -> (impl FnOnce(T) + Send + 'static, Receiver<T>) {
    let shared = Arc::new(Mutex::new(Slot {
        value: None,
        waker: None,
    }));
    let tx = Arc::clone(&shared);
    let send = move |value: T| {
        let waker = {
            let mut slot = tx.lock().expect("oneshot lock");
            slot.value = Some(value);
            slot.waker.take()
        };
        // Outside the lock: a waker may poll the receiver inline.
        if let Some(waker) = waker {
            waker.wake();
        }
    };
    (send, Receiver(shared))
}

impl<T> Future for Receiver<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut slot = self.0.lock().expect("oneshot lock");
        match slot.value.take() {
            Some(value) => Poll::Ready(value),
            None => {
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// Copy `texture` (4 bytes per texel, `width` x `height`) into a staging
/// buffer and return it as tight rows, `width * height * 4` bytes.
///
/// Submits one encoder of its own, through [`crate::submit::submit`].
pub async fn read_texture_rgba8(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, GpuError> {
    let unpadded_bytes_per_row = width * 4;
    let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ghostframe-texture-readback"),
        size: padded_bytes_per_row as u64 * height as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("ghostframe-texture-readback"),
    });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    crate::submit::submit(queue, encoder);

    let (done, mapped) = oneshot();
    staging.slice(..).map_async(wgpu::MapMode::Read, done);
    mapped
        .await
        .map_err(|e| GpuError::Device(format!("map readback staging buffer: {e}")))?;

    let padded = staging
        .slice(..)
        .get_mapped_range()
        .map_err(|e| GpuError::Device(format!("readback get_mapped_range: {e}")))?;
    // Strip row padding: the staging buffer's rows are padded to
    // `COPY_BYTES_PER_ROW_ALIGNMENT`, but callers want tight rows. Skipping
    // this looks right at the start of each row and drifts further out of
    // alignment with every row after it.
    let mut tight = Vec::with_capacity(unpadded_bytes_per_row as usize * height as usize);
    for row in 0..height as usize {
        let start = row * padded_bytes_per_row as usize;
        tight.extend_from_slice(&padded[start..start + unpadded_bytes_per_row as usize]);
    }
    drop(padded);
    staging.unmap();
    Ok(tight)
}

/// Drive `fut` to completion on this thread, polling `device` whenever it is
/// pending. Native only: a browser has no blocking wait, and its event loop
/// does this job.
///
/// "Pending" is taken to mean "a buffer-map or work-done callback is
/// outstanding", which is true of every future in this crate. A future that
/// waits on anything else would spin here.
///
/// # Panics
///
/// If the device cannot be polled (it was lost): the future could then never
/// complete.
#[cfg(not(target_arch = "wasm32"))]
pub fn block_on_device<F: Future>(device: &wgpu::Device, fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
            return value;
        }
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("polling the device for a pending GPU callback");
    }
}
