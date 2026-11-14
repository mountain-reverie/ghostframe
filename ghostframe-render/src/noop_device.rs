// SPDX-License-Identifier: MIT OR Apache-2.0
//! A `wgpu::Device` for unit tests that have no GPU: wgpu's no-op backend
//! validates everything and records nothing it can run, so a texture reads back
//! as zeros. Enough to test what [`crate::TileRenderer`] decides (ordering,
//! damage, submit counts, what it refuses); not what pixels it draws -- that is
//! `ghostframe-client-gpu`'s GPU tests, which need a real device.

use std::future::Future;
use std::task::{Context, Poll, Waker};

/// A device and queue on the no-op backend.
pub(crate) fn device() -> (wgpu::Device, wgpu::Queue) {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::NOOP,
        backend_options: wgpu::BackendOptions {
            noop: wgpu::NoopBackendOptions {
                enable: true,
                ..Default::default()
            },
            ..Default::default()
        },
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        .expect("no-op adapter");
    block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("no-op device")
}

/// Poll `fut` until it is ready. For futures that complete without a device
/// callback (the no-op backend's request calls).
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
    }
}
