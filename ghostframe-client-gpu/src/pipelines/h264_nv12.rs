//! Full-frame NV12 -> RGBA blit into the framebuffer.
//!
//! Unlike the tile pipelines, this one always covers the whole surface:
//! H.264 frame mode replaces the entire image, so there is no instancing and
//! no tile rect.

use crate::framebuffer::Framebuffer;

const SHADER_SRC: &str = include_str!("../../../shaders/client/h264_nv12_blit.wgsl");

pub struct H264Nv12Pipeline {
    pipeline: wgpu::RenderPipeline,
}

impl H264Nv12Pipeline {
    /// Production entry point: a pipeline targeting `Rgba8Unorm`, the format
    /// `Framebuffer` always uses.
    pub fn new(device: &wgpu::Device) -> Self {
        Self::with_format(device, wgpu::TextureFormat::Rgba8Unorm)
    }

    /// Build against an arbitrary colour-target format.
    ///
    /// Parameterised rather than hardcoding `Rgba8Unorm` so Task 8's Tier A
    /// oracle can construct a second instance of this same pipeline against
    /// `Rgba32Float` -- the format that receives the fragment shader's
    /// `vec4<f32>` output with no fp16 packing (design doc §9.1), which is
    /// what makes a bit-exact arithmetic comparison possible in the first
    /// place. Production always calls [`Self::new`].
    pub fn with_format(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ghostframe-h264-nv12-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ghostframe-h264-nv12-pipeline"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
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
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        H264Nv12Pipeline { pipeline }
    }

    /// Upload CPU-side NV12 planes as textures. Used by the oracle, and by
    /// the CPU fallback path when a dmabuf cannot be imported.
    pub fn upload_planes(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        luma: &[u8],
        chroma: &[u8],
    ) -> (wgpu::Texture, wgpu::Texture) {
        let luma_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ghostframe-h264-luma"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &luma_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            luma,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let chroma_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ghostframe-h264-chroma"),
            size: wgpu::Extent3d {
                width: width / 2,
                height: height / 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &chroma_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            chroma,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height / 2),
            },
            wgpu::Extent3d {
                width: width / 2,
                height: height / 2,
                depth_or_array_layers: 1,
            },
        );

        (luma_tex, chroma_tex)
    }

    /// Draw the whole frame into `fb`.
    ///
    /// `LoadOp::Load`, never `Clear`: the framebuffer is shared with the
    /// tile codecs, and the draw covers every pixel anyway.
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        fb: &Framebuffer,
        luma: &wgpu::Texture,
        chroma: &wgpu::Texture,
    ) {
        self.draw_to_texture(device, queue, fb.texture(), luma, chroma);
    }

    /// Draw the whole frame into an arbitrary colour target.
    ///
    /// `draw` (the production path) is a thin wrapper around this for
    /// `fb.texture()`. Task 8's Tier A oracle calls this directly against a
    /// raw `Rgba32Float` texture that has no `Framebuffer` wrapper -- see
    /// [`Self::with_format`].
    pub fn draw_to_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target: &wgpu::Texture,
        luma: &wgpu::Texture,
        chroma: &wgpu::Texture,
    ) {
        let luma_view = luma.create_view(&wgpu::TextureViewDescriptor::default());
        let chroma_view = chroma.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ghostframe-h264-nv12-bind-group"),
            layout: &self.pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&luma_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&chroma_view),
                },
            ],
        });

        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-h264-nv12-encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ghostframe-h264-nv12-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
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
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
        queue.submit(Some(encoder.finish()));
    }
}
