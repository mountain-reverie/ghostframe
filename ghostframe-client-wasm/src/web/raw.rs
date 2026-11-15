// SPDX-License-Identifier: MIT OR Apache-2.0
//! Hand-declared bindings for the raw WebGPU and WebCodecs calls the H.264
//! escape makes. The objects they are called on are the very ones wgpu uses
//! (`Device::as_webgpu`, `Queue::as_webgpu`, `TextureView::as_webgpu`).
//!
//! web-sys has these behind `--cfg=web_sys_unstable_apis`; declaring the dozen
//! methods here keeps that flag out of every build of this repository.
//! Methods that can throw on bad input are `catch`; the rest report problems
//! through the device's error scopes, as WebGPU does.

use js_sys::{Array, Object, Promise, Reflect};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    /// A `GPUDevice`.
    #[wasm_bindgen(extends = Object)]
    pub type RawDevice;
    #[wasm_bindgen(method, catch, js_name = createSampler)]
    pub fn create_sampler(this: &RawDevice, descriptor: &JsValue) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(method, catch, js_name = createShaderModule)]
    pub fn create_shader_module(this: &RawDevice, descriptor: &JsValue)
        -> Result<JsValue, JsValue>;
    #[wasm_bindgen(method, catch, js_name = createRenderPipeline)]
    pub fn create_render_pipeline(
        this: &RawDevice,
        descriptor: &JsValue,
    ) -> Result<RawPipeline, JsValue>;
    /// Throws for a closed or foreign `VideoFrame`.
    #[wasm_bindgen(method, catch, js_name = importExternalTexture)]
    pub fn import_external_texture(
        this: &RawDevice,
        descriptor: &JsValue,
    ) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(method, js_name = createBindGroup)]
    pub fn create_bind_group(this: &RawDevice, descriptor: &JsValue) -> JsValue;
    #[wasm_bindgen(method, js_name = createCommandEncoder)]
    pub fn create_command_encoder(this: &RawDevice) -> RawEncoder;
    #[wasm_bindgen(method, js_name = pushErrorScope)]
    pub fn push_error_scope(this: &RawDevice, filter: &str);
    #[wasm_bindgen(method, js_name = popErrorScope)]
    pub fn pop_error_scope(this: &RawDevice) -> Promise;

    /// A `GPUQueue`.
    #[wasm_bindgen(extends = Object)]
    pub type RawQueue;
    #[wasm_bindgen(method)]
    pub fn submit(this: &RawQueue, command_buffers: &Array);

    /// A `GPURenderPipeline`.
    #[wasm_bindgen(extends = Object)]
    pub type RawPipeline;
    #[wasm_bindgen(method, js_name = getBindGroupLayout)]
    pub fn get_bind_group_layout(this: &RawPipeline, index: u32) -> JsValue;

    /// A `GPUCommandEncoder`.
    #[wasm_bindgen(extends = Object)]
    pub type RawEncoder;
    #[wasm_bindgen(method, js_name = beginRenderPass)]
    pub fn begin_render_pass(this: &RawEncoder, descriptor: &JsValue) -> RawPass;
    #[wasm_bindgen(method)]
    pub fn finish(this: &RawEncoder) -> JsValue;

    /// A `GPURenderPassEncoder`.
    #[wasm_bindgen(extends = Object)]
    pub type RawPass;
    #[wasm_bindgen(method, js_name = setPipeline)]
    pub fn set_pipeline(this: &RawPass, pipeline: &RawPipeline);
    #[wasm_bindgen(method, js_name = setBindGroup)]
    pub fn set_bind_group(this: &RawPass, index: u32, bind_group: &JsValue);
    #[wasm_bindgen(method, js_name = setViewport)]
    pub fn set_viewport(
        this: &RawPass,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        min_depth: f32,
        max_depth: f32,
    );
    #[wasm_bindgen(method)]
    pub fn draw(this: &RawPass, vertex_count: u32);
    #[wasm_bindgen(method)]
    pub fn end(this: &RawPass);
}

/// The `GPUDevice` behind a wgpu device, or why there is none.
pub fn raw_device(device: &wgpu::Device) -> Result<RawDevice, String> {
    let handle = device
        .as_webgpu()
        .ok_or("the wgpu device is not the browser WebGPU backend")?;
    let js: &JsValue = handle.as_ref();
    Ok(js.clone().unchecked_into())
}

pub fn raw_queue(queue: &wgpu::Queue) -> Result<RawQueue, String> {
    let handle = queue
        .as_webgpu()
        .ok_or("the wgpu queue is not the browser WebGPU backend")?;
    let js: &JsValue = handle.as_ref();
    Ok(js.clone().unchecked_into())
}

/// The `GPUTextureView` behind a wgpu view.
pub fn raw_view(view: &wgpu::TextureView) -> Result<JsValue, String> {
    let handle = view
        .as_webgpu()
        .ok_or("the wgpu texture view is not the browser WebGPU backend")?;
    let js: &JsValue = handle.as_ref();
    Ok(js.clone())
}

/// A plain JS object from `(key, value)` pairs.
pub fn obj(entries: &[(&str, JsValue)]) -> JsValue {
    let o = Object::new();
    for (k, v) in entries {
        // Setting a property on a fresh plain object cannot fail.
        let _ = Reflect::set(&o, &JsValue::from_str(k), v);
    }
    o.into()
}

pub fn array(items: &[JsValue]) -> JsValue {
    items.iter().cloned().collect::<Array>().into()
}

pub fn num(n: u32) -> JsValue {
    JsValue::from_f64(f64::from(n))
}

/// `VideoFrame.close()`; closing twice is harmless, and a value that is not a
/// frame is ignored (a stale close must never trap the module).
pub fn close_frame(frame: &JsValue) {
    let close = Reflect::get(frame, &JsValue::from_str("close"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
    if let Some(close) = close {
        let _ = close.call0(frame);
    }
}

/// The `message` of a JS error (or the value's string form).
pub fn js_message(e: &JsValue) -> String {
    Reflect::get(e, &JsValue::from_str("message"))
        .ok()
        .and_then(|m| m.as_string())
        .or_else(|| e.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}
