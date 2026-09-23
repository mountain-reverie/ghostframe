//! Solid codec: one instanced quad per tile.
//!
//! Instance layout is 3 x u32 = 12 bytes (tile_x, tile_y, color_packed),
//! matching `shaders/client/solid.wgsl`'s @location(1..3). The colour is
//! BGRA packed LSB-first; the shader swizzles to RGBA, so the CPU side
//! passes wire bytes through unchanged.

use crate::framebuffer::Framebuffer;

const SHADER_SRC: &str = include_str!("../../../shaders/client/solid.wgsl");

/// Bytes per instance: tile_x (u32) + tile_y (u32) + color_packed (u32).
const INSTANCE_STRIDE: u64 = 12;

pub struct SolidPipeline {
    pipeline: wgpu::RenderPipeline,
    canvas_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instance_buffer: Option<wgpu::Buffer>,
    /// Instance capacity of `instance_buffer`, in instances (not bytes).
    instance_capacity: usize,
}

impl SolidPipeline {
    pub fn new(device: &wgpu::Device) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ghostframe-solid-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ghostframe-solid-pipeline"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: INSTANCE_STRIDE,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Uint32,
                            offset: 0,
                            shader_location: 1,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Uint32,
                            offset: 4,
                            shader_location: 2,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Uint32,
                            offset: 8,
                            shader_location: 3,
                        },
                    ],
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let canvas_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-solid-canvas-size"),
            size: 8,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ghostframe-solid-canvas-bind-group"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: canvas_buffer.as_entire_binding(),
            }],
        });

        SolidPipeline {
            pipeline,
            canvas_buffer,
            bind_group,
            instance_buffer: None,
            instance_capacity: 0,
        }
    }

    pub fn set_canvas_size(&mut self, _device: &wgpu::Device, queue: &wgpu::Queue, w: u32, h: u32) {
        let mut data = [0u8; 8];
        data[0..4].copy_from_slice(&w.to_le_bytes());
        data[4..8].copy_from_slice(&h.to_le_bytes());
        queue.write_buffer(&self.canvas_buffer, 0, &data);
    }

    /// Ensure the instance buffer can hold at least `capacity` instances,
    /// growing (never shrinking) as needed rather than reallocating per draw.
    fn ensure_capacity(&mut self, device: &wgpu::Device, capacity: usize) {
        if capacity <= self.instance_capacity {
            return;
        }
        self.instance_buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-solid-instances"),
            size: capacity as u64 * INSTANCE_STRIDE,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.instance_capacity = capacity;
    }

    /// `tiles` is (tile_x, tile_y, bgra).
    ///
    /// The render pass uses `LoadOp::Load`, never `Clear`: the framebuffer is
    /// persistent and shared with the other codecs within a frame, and a
    /// clear here would wipe every tile PalRle, Cdf53, or Raw already wrote.
    /// An empty `tiles` is a no-op -- an empty instance buffer is a wgpu
    /// validation error, so the draw is skipped entirely rather than issued
    /// with zero instances.
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        fb: &Framebuffer,
        tiles: &[(u8, u8, [u8; 4])],
    ) {
        if tiles.is_empty() {
            return;
        }

        self.ensure_capacity(device, tiles.len());

        let mut data = Vec::with_capacity(tiles.len() * INSTANCE_STRIDE as usize);
        for (tile_x, tile_y, bgra) in tiles {
            data.extend_from_slice(&(*tile_x as u32).to_le_bytes());
            data.extend_from_slice(&(*tile_y as u32).to_le_bytes());
            // Wire bytes pass through unchanged: the shader's
            // `unpack4x8unorm` reads them as a little-endian u32 whose
            // LSB-first byte order is exactly this BGRA order, then
            // swizzles `.zyxw` to RGBA.
            data.extend_from_slice(bgra);
        }
        let instance_buffer = self
            .instance_buffer
            .as_ref()
            .expect("ensure_capacity set it");
        queue.write_buffer(instance_buffer, 0, &data);

        let view = fb
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-solid-draw"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ghostframe-solid-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, instance_buffer.slice(..));
            pass.draw(0..6, 0..tiles.len() as u32);
        }
        queue.submit(std::iter::once(encoder.finish()));
    }
}
