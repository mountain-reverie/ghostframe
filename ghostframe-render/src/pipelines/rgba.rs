// SPDX-License-Identifier: MIT OR Apache-2.0
//! Already-decoded RGBA tiles (`Event::TileReady`), copied into the
//! framebuffer by a compute pass: one buffer upload and one dispatch per
//! flush, however many tiles and wherever they are.
//!
//! `write_texture` costs ~178us of CPU per call on a Pinebook Pro regardless
//! of size. Coalescing adjacent tiles into rectangles made a full repaint one
//! call, but scattered tiles -- the common case for text and UI changes --
//! still cost one call each. See `shaders/client/rgba_tiles.wgsl`.

use crate::framebuffer::Framebuffer;

const SHADER_SRC: &str = include_str!("../../../shaders/client/rgba_tiles.wgsl");

/// Bytes per `TileWork` entry: tile_x, tile_y as u32.
const TILE_WORK_STRIDE: u64 = 8;

/// Bytes of pixels per tile: 32x32 RGBA8.
pub const TILE_BYTES: usize = 32 * 32 * 4;

pub struct RgbaPipeline {
    pipeline: wgpu::ComputePipeline,
    tile_work_buffer: Option<wgpu::Buffer>,
    pixels_buffer: Option<wgpu::Buffer>,
    /// Tile capacity of the buffers above (not bytes).
    tile_capacity: usize,
    /// The bind group over both buffers and the framebuffer view, with the
    /// [`Framebuffer::epoch`] it was built for. Dropped when the buffers grow;
    /// rebuilt when the framebuffer's texture is replaced.
    bind_group: Option<(u64, wgpu::BindGroup)>,
    /// Reused across flushes so a batch does not allocate it.
    tile_work: Vec<u8>,
}

impl RgbaPipeline {
    /// Drop the cached bind group that holds the framebuffer's view, so a
    /// replaced framebuffer texture is not kept alive by it.
    pub fn drop_framebuffer_cache(&mut self) {
        self.bind_group = None;
    }

    pub fn new(device: &wgpu::Device) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ghostframe-rgba-tiles-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("ghostframe-rgba-tiles-pipeline"),
            layout: None,
            module: &module,
            entry_point: Some("main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        RgbaPipeline {
            pipeline,
            tile_work_buffer: None,
            pixels_buffer: None,
            tile_capacity: 0,
            bind_group: None,
            tile_work: Vec::new(),
        }
    }

    /// Grow (never shrink) the buffers to hold `capacity` tiles.
    fn ensure_capacity(&mut self, device: &wgpu::Device, capacity: usize) {
        if capacity <= self.tile_capacity {
            return;
        }
        self.tile_work_buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-rgba-tiles-work"),
            size: capacity as u64 * TILE_WORK_STRIDE,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.pixels_buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-rgba-tiles-pixels"),
            size: (capacity * TILE_BYTES) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.tile_capacity = capacity;
        self.bind_group = None;
    }

    /// Copy `tiles` into `fb` in a submit of its own; see [`RgbaPipeline::encode`]
    /// for the form a frame uses.
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        fb: &Framebuffer,
        tiles: &[(u32, u32, &[u8])],
    ) {
        crate::submit::submit_now(device, queue, "ghostframe-rgba-tiles", |e| {
            self.encode(device, queue, e, fb, tiles)
        });
    }

    /// Record a copy of `tiles` into `fb`. Each is (tile_x, tile_y, rgba) with
    /// `rgba` at least [`TILE_BYTES`] long (a full 32x32 tile; only the first
    /// `TILE_BYTES` are used). Parts of a tile past the framebuffer edge are
    /// clipped. If a tile appears twice, the later one wins. Returns whether
    /// anything was recorded.
    ///
    /// The buffers are written through the queue now, which wgpu runs ahead of
    /// the commands of the next submit. So call this at most once per encoder:
    /// a second call would overwrite the first's tiles before either's
    /// dispatch ran.
    pub fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        fb: &Framebuffer,
        tiles: &[(u32, u32, &[u8])],
    ) -> bool {
        if tiles.is_empty() {
            return false;
        }
        // Workgroups of one dispatch run in no defined order, so a tile listed
        // twice would race; keep only the last occurrence of each.
        let mut seen = std::collections::HashSet::with_capacity(tiles.len());
        let mut keep: Vec<&(u32, u32, &[u8])> = Vec::with_capacity(tiles.len());
        for t in tiles.iter().rev() {
            if seen.insert((t.0, t.1)) {
                keep.push(t);
            }
        }

        self.ensure_capacity(device, keep.len());
        self.tile_work.clear();
        for (tx, ty, _) in keep.iter().map(|t| **t) {
            self.tile_work.extend_from_slice(&tx.to_le_bytes());
            self.tile_work.extend_from_slice(&ty.to_le_bytes());
        }
        let tile_work_buffer = self
            .tile_work_buffer
            .as_ref()
            .expect("ensure_capacity set it");
        let pixels_buffer = self.pixels_buffer.as_ref().expect("ensure_capacity set it");
        queue.write_buffer(tile_work_buffer, 0, &self.tile_work);
        // Straight into wgpu's staging memory: the pixels are the bulk of the
        // bytes, and on this hardware every extra pass over them shows.
        let size =
            wgpu::BufferSize::new((keep.len() * TILE_BYTES) as u64).expect("non-empty batch");
        let Some(mut staging) = queue.write_buffer_with(pixels_buffer, 0, size) else {
            // Only on a validation failure (the size is within the buffer by
            // construction); wgpu has already reported it.
            return false;
        };
        for (i, (_, _, rgba)) in keep.iter().map(|t| **t).enumerate() {
            debug_assert!(rgba.len() >= TILE_BYTES, "RGBA tile shorter than 32x32");
            staging
                .slice(i * TILE_BYTES..(i + 1) * TILE_BYTES)
                .copy_from_slice(&rgba[..TILE_BYTES]);
        }
        drop(staging);

        let epoch = fb.epoch();
        if self.bind_group.as_ref().map(|(e, _)| *e) != Some(epoch) {
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ghostframe-rgba-tiles-bind-group"),
                layout: &self.pipeline.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: tile_work_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: pixels_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(fb.view()),
                    },
                ],
            });
            self.bind_group = Some((epoch, bind_group));
        }
        let (_, bind_group) = self.bind_group.as_ref().expect("just set");
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ghostframe-rgba-tiles-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.dispatch_workgroups(keep.len() as u32, 2, 2);
        }
        true
    }
}
