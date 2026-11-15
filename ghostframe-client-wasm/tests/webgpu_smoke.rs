// SPDX-License-Identifier: MIT OR Apache-2.0
//! Spike: does this browser hand wgpu a WebGPU adapter in the browser tests?
//! Run with `wasm-pack test --headless --chrome ghostframe-client-wasm --test webgpu_smoke`.
#![cfg(target_arch = "wasm32")]

use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
async fn the_browser_hands_wgpu_an_adapter_and_a_device() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions::default())
        .await
        .expect("no WebGPU adapter: see Task 1 of the PR 2 plan for the flag ladder");
    let info = adapter.get_info();
    web_sys::console::log_1(&format!("adapter: {} ({:?})", info.name, info.backend).into());
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default())
        .await
        .expect("request_device");
    assert!(device.as_webgpu().is_some(), "not the browser WebGPU backend");
    assert!(queue.as_webgpu().is_some());
}
