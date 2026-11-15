// SPDX-License-Identifier: MIT OR Apache-2.0
//! Port of `window.__readGradientGolden` (the old `diagnostics.ts`): write a
//! known gradient into the framebuffer with a compute shader, read 16 points
//! back, report which differ. It exists so `e2e_readpixel_correctness` can
//! trust the pixel readbacks before any pixel assertion is trusted. It
//! overwrites the framebuffer; test use only.

use ghostframe_render::readback::begin_texture_read;
use js_sys::Promise;
use serde::Serialize;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::future_to_promise;

const GRADIENT_WGSL: &str = include_str!("../../../shaders/client/debug_gradient.wgsl");

#[derive(Serialize)]
struct Mismatch {
    x: u32,
    y: u32,
    got: Vec<u8>,
    want: Vec<u8>,
}

#[derive(Serialize)]
struct Golden {
    ok: bool,
    mismatches: Vec<Mismatch>,
}

fn sample_points(w: u32, h: u32) -> Vec<(u32, u32)> {
    [
        (0, 0),
        (1, 0),
        (255, 0),
        (256, 0),
        (0, 1),
        (0, 255),
        (0, 256),
        (100, 200),
        (16, 16),
        (31, 31),
        (32, 32),
        (63, 63),
        (w / 2, h / 2),
        (w - 1, h - 1),
        (255.min(w - 1), 255.min(h - 1)),
        (256.min(w - 1), 256.min(h - 1)),
    ]
    .into_iter()
    .filter(|&(x, y)| x < w && y < h)
    .collect()
}

/// Dispatch the gradient into `framebuffer` now; the returned promise
/// resolves to `{ ok, mismatches: [{x, y, got, want}] }`.
pub fn read_gradient_golden(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    framebuffer: &wgpu::Texture,
) -> Promise {
    let (w, h) = (framebuffer.width(), framebuffer.height());
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("debug_gradient"),
        source: wgpu::ShaderSource::Wgsl(GRADIENT_WGSL.into()),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("debug_gradient"),
        layout: None,
        module: &module,
        entry_point: Some("debug_gradient"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });
    let view = framebuffer.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("debug_gradient"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(&view),
        }],
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("debug_gradient"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("debug_gradient"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(w.div_ceil(8), h.div_ceil(8), 1);
    }
    queue.submit(std::iter::once(encoder.finish()));

    // Every readback copy is submitted now (ordered after the dispatch); the
    // awaits only collect the bytes, so nothing here borrows the renderer.
    let reads: Vec<_> = sample_points(w, h)
        .into_iter()
        .map(|(x, y)| {
            (
                x,
                y,
                begin_texture_read(device, queue, framebuffer, x, y, 1, 1),
            )
        })
        .collect();
    future_to_promise(async move {
        let mut mismatches = Vec::new();
        for (x, y, read) in reads {
            let got = read
                .finish()
                .await
                .map_err(|e| JsValue::from_str(&e.to_string()))?;
            let want = vec![(x & 0xFF) as u8, (y & 0xFF) as u8, 0, 255];
            if got != want {
                mismatches.push(Mismatch { x, y, got, want });
            }
        }
        let report = Golden {
            ok: mismatches.is_empty(),
            mismatches,
        };
        serde_wasm_bindgen::to_value(&report).map_err(|e| JsValue::from_str(&e.to_string()))
    })
}
