// SPDX-License-Identifier: MIT OR Apache-2.0
//! The H.264 escape (spec 5.3): a decoded `VideoFrame` onto the framebuffer
//! with no copy through wasm memory.
//!
//! Ordering is the caller's job (`WebRenderer::paint_tick`): the paint ledger
//! decides whether a picture is stale (`TileRenderer::h264_stale`), this only
//! draws the ones it is handed. wgpu's WebGPU backend hands each
//! `queue.submit` to the same `GPUQueue` immediately, so a raw submit made
//! after `TileRenderer::flush` lands after the flush's work.

use js_sys::Array;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use super::raw::{self, RawDevice, RawPipeline, RawQueue};

const H264_WGSL: &str = include_str!("../../../shaders/client/h264_blit.wgsl");

pub struct H264Blit {
    device: RawDevice,
    queue: RawQueue,
    pipeline: RawPipeline,
    sampler: JsValue,
}

impl H264Blit {
    /// Build the pipeline inside a `validation` error scope, as `probeH264`
    /// did: a browser without `texture_external` (Firefox) fails here, softly,
    /// and `Err` means "H.264 is unsupported in this browser", not a bug.
    pub async fn probe(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<H264Blit, String> {
        let raw_device = raw::raw_device(device)?;
        let raw_queue = raw::raw_queue(queue)?;
        raw_device.push_error_scope("validation");
        let built = Self::build(&raw_device);
        let scope = JsFuture::from(raw_device.pop_error_scope())
            .await
            .map_err(|e| raw::js_message(&e))?;
        let (pipeline, sampler) =
            built.map_err(|e| format!("pipeline construction threw: {}", raw::js_message(&e)))?;
        if !scope.is_null() && !scope.is_undefined() {
            return Err(format!("validation error: {}", raw::js_message(&scope)));
        }
        Ok(H264Blit {
            device: raw_device,
            queue: raw_queue,
            pipeline,
            sampler,
        })
    }

    fn build(device: &RawDevice) -> Result<(RawPipeline, JsValue), JsValue> {
        let sampler = device.create_sampler(&raw::obj(&[
            ("magFilter", "linear".into()),
            ("minFilter", "linear".into()),
            ("addressModeU", "clamp-to-edge".into()),
            ("addressModeV", "clamp-to-edge".into()),
        ]))?;
        let module = device.create_shader_module(&raw::obj(&[
            ("label", "h264_blit".into()),
            ("code", H264_WGSL.into()),
        ]))?;
        let pipeline = device.create_render_pipeline(&raw::obj(&[
            ("layout", "auto".into()),
            (
                "vertex",
                raw::obj(&[("module", module.clone()), ("entryPoint", "vs_main".into())]),
            ),
            (
                "fragment",
                raw::obj(&[
                    ("module", module),
                    ("entryPoint", "fs_main".into()),
                    (
                        "targets",
                        raw::array(&[raw::obj(&[("format", "rgba8unorm".into())])]),
                    ),
                ]),
            ),
            (
                "primitive",
                raw::obj(&[("topology", "triangle-list".into())]),
            ),
        ]))?;
        Ok((pipeline, sampler))
    }

    /// Draw every frame of `frames` full-screen into `framebuffer_view` (the
    /// raw `GPUTextureView` of the tile renderer's framebuffer; `loadOp: load`,
    /// so tiles stay where the picture does not cover them) in one pass and
    /// one submit. The frames must be open; closing them is the caller's.
    /// A frame that cannot be imported is skipped and named in the error;
    /// the others are still drawn.
    pub fn draw(
        &self,
        framebuffer_view: &JsValue,
        width: u32,
        height: u32,
        frames: &[JsValue],
    ) -> Result<(), String> {
        let encoder = self.device.create_command_encoder();
        let attachment = raw::obj(&[
            ("view", framebuffer_view.clone()),
            ("loadOp", "load".into()),
            ("storeOp", "store".into()),
        ]);
        let pass = encoder.begin_render_pass(&raw::obj(&[(
            "colorAttachments",
            raw::array(&[attachment]),
        )]));
        let mut failure = None;
        for frame in frames {
            let external = match self
                .device
                .import_external_texture(&raw::obj(&[("source", frame.clone())]))
            {
                Ok(e) => e,
                Err(e) => {
                    failure = Some(format!("importExternalTexture: {}", raw::js_message(&e)));
                    continue;
                }
            };
            let bind_group = self.device.create_bind_group(&raw::obj(&[
                ("layout", self.pipeline.get_bind_group_layout(0)),
                (
                    "entries",
                    raw::array(&[
                        raw::obj(&[("binding", raw::num(0)), ("resource", external)]),
                        raw::obj(&[("binding", raw::num(1)), ("resource", self.sampler.clone())]),
                    ]),
                ),
            ]));
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group);
            pass.set_viewport(0.0, 0.0, width as f32, height as f32, 0.0, 1.0);
            pass.draw(6);
        }
        pass.end();
        self.queue.submit(&Array::of1(&encoder.finish()));
        failure.map_or(Ok(()), Err)
    }
}
