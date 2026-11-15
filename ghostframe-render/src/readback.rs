// SPDX-License-Identifier: MIT OR Apache-2.0
//! Reading a texture back to the CPU, on every target.
//!
//! `wgpu::Buffer::map_async` takes a callback, and the callback runs when the
//! device is polled (native) or when the browser's event loop gets to it (web).
//! [`begin_texture_read`] and [`read_texture_rgba8`] turn that into a future
//! with a one-shot channel, so the same code serves the CDF 5/3 self-test on
//! both. The rectangle must be non-empty and inside the texture. A caller on a thread
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

/// Rows of `padded` (pitch `padded_row` bytes) cut to `width_px` RGBA pixels
/// each, in one tight buffer. A readback's rows are padded to
/// `COPY_BYTES_PER_ROW_ALIGNMENT`; skipping this looks right at the start of
/// each row and drifts further out of alignment with every row after it.
pub(crate) fn tight_rows(padded: &[u8], width_px: u32, height_px: u32, padded_row: u32) -> Vec<u8> {
    let row = width_px as usize * 4;
    let mut tight = Vec::with_capacity(row * height_px as usize);
    for y in 0..height_px as usize {
        let start = y * padded_row as usize;
        tight.extend_from_slice(&padded[start..start + row]);
    }
    tight
}

/// A texture-to-buffer copy that has been **submitted**; [`PendingRead::finish`]
/// waits for the bytes. Splitting the two lets a caller order the copy after
/// everything already submitted and await it later.
pub struct PendingRead {
    staging: wgpu::Buffer,
    mapped: Receiver<Result<(), wgpu::BufferAsyncError>>,
    width: u32,
    height: u32,
    padded_row: u32,
}

/// Copy the `w` x `h` rectangle at (`x`, `y`) of a 4-byte-per-texel `texture`
/// (it needs `COPY_SRC`) into a staging buffer, submit the copy through
/// [`crate::submit::submit`] and start mapping the buffer.
pub fn begin_texture_read(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> PendingRead {
    let padded_row =
        (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ghostframe-texture-readback"),
        size: u64::from(padded_row) * u64::from(h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("ghostframe-texture-readback"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d { x, y, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    crate::submit::submit(queue, encoder);
    let (done, mapped) = oneshot();
    staging.slice(..).map_async(wgpu::MapMode::Read, done);
    PendingRead {
        staging,
        mapped,
        width: w,
        height: h,
        padded_row,
    }
}

impl PendingRead {
    /// The rectangle as `w * h * 4` tight RGBA bytes. Needs no device: the
    /// map callback is delivered by the browser's event loop, or on native by
    /// whoever polls the device ([`block_on_device`]).
    pub async fn finish(self) -> Result<Vec<u8>, GpuError> {
        self.mapped
            .await
            .map_err(|e| GpuError::Device(format!("map readback staging buffer: {e}")))?;
        let padded = self
            .staging
            .slice(..)
            .get_mapped_range()
            .map_err(|e| GpuError::Device(format!("readback get_mapped_range: {e}")))?;
        let tight = tight_rows(&padded, self.width, self.height, self.padded_row);
        drop(padded);
        self.staging.unmap();
        Ok(tight)
    }
}

/// Copy the whole `width` x `height` `texture` into tight rows,
/// `width * height * 4` bytes. Submits one encoder of its own.
pub async fn read_texture_rgba8(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, GpuError> {
    begin_texture_read(device, queue, texture, 0, 0, width, height)
        .finish()
        .await
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

#[cfg(test)]
mod tests {
    use super::tight_rows;

    #[test]
    fn tight_rows_drops_exactly_the_padding() {
        // 3 px wide, 2 rows, pitch 16: row 0 = 1.., row 1 = 101.., padding 0xEE.
        let mut raw = vec![0xEE; 32];
        raw[..12].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        raw[16..28].copy_from_slice(&[101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112]);
        let out = tight_rows(&raw, 3, 2, 16);
        assert_eq!(out.len(), 24);
        assert!(!out.contains(&0xEE), "padding leaked into the result");
        assert_eq!(&out[12..16], &[101, 102, 103, 104]);
    }
}
