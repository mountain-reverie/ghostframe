//! PalRle compute decode.
//!
//! Dispatch is `(num_tiles, 2, 2)` with `@workgroup_size(16, 16, 1)` -- four
//! workgroups per tile, arranged 2x2, each covering a 16x16 sub-region of
//! the 32x32 tile. This is NOT arbitrary: WebGPU's portable
//! `maxComputeInvocationsPerWorkgroup` is 256, and one workgroup per 32x32
//! tile would need 1024 invocations, which fails pipeline validation on
//! common adapters (including the `downlevel_defaults` limits this crate's
//! `WgpuContext` requests) and silently writes no pixels at all.
//!
//! Mirrors `ghostframe-web-client/src/webgpu/palrle.ts`, the reference
//! implementation this was ported from.

use crate::framebuffer::Framebuffer;

const SHADER_SRC: &str = include_str!("../../../shaders/client/palrle_decode.wgsl");

/// Bytes per `TileWork` entry: 8 x u32 (tile_x, tile_y, palette_id, count,
/// payload_off, and three padding words). The padding is load-bearing: the
/// shader's `array<TileWork>` stride depends on it being exactly 32 bytes.
const TILE_WORK_STRIDE: u64 = 32;

/// Bytes of packed 4-bit indices per tile (32x32 pixels, two per byte).
const INDICES_STRIDE: u64 = 512;

/// Bytes per tile's error slot.
const ERROR_STRIDE: u64 = 4;

/// Bytes per palette slot in the atlas: 16 colours x 4 bytes (BGRA) each.
const PALETTE_SLOT_STRIDE: u64 = 64;

/// Total palette atlas size: 256 possible palette ids x 64 bytes each.
const PALETTE_ATLAS_SIZE: u64 = 256 * PALETTE_SLOT_STRIDE;

pub struct PalRlePipeline {
    pipeline: wgpu::ComputePipeline,
    palette_atlas: wgpu::Buffer,
    tile_work_buffer: Option<wgpu::Buffer>,
    indices_buffer: Option<wgpu::Buffer>,
    errors_buffer: Option<wgpu::Buffer>,
    errors_staging: Option<wgpu::Buffer>,
    /// Tile capacity of the per-batch buffers above (not bytes).
    tile_capacity: usize,
    /// Number of tiles dispatched in the most recent `decode` call, i.e.
    /// how many error slots are valid to read back.
    last_batch_size: usize,
}

impl PalRlePipeline {
    pub fn new(device: &wgpu::Device) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ghostframe-palrle-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("ghostframe-palrle-pipeline"),
            layout: None,
            module: &module,
            entry_point: Some("main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

        let palette_atlas = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-palrle-palette-atlas"),
            size: PALETTE_ATLAS_SIZE,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        PalRlePipeline {
            pipeline,
            palette_atlas,
            tile_work_buffer: None,
            indices_buffer: None,
            errors_buffer: None,
            errors_staging: None,
            tile_capacity: 0,
            last_batch_size: 0,
        }
    }

    /// `colors` is BGRA, matching the wire and `Event::PaletteUpdated`.
    /// Written at byte offset `palette_id * 64`, matching the shader's
    /// `palette_atlas[work.palette_id * 16u + color_idx]` indexing (16 u32
    /// words = 64 bytes per palette).
    pub fn upload_palette(&mut self, queue: &wgpu::Queue, palette_id: u8, colors: &[[u8; 4]; 16]) {
        let mut data = Vec::with_capacity(64);
        for c in colors {
            data.extend_from_slice(c);
        }
        queue.write_buffer(
            &self.palette_atlas,
            palette_id as u64 * PALETTE_SLOT_STRIDE,
            &data,
        );
    }

    /// Ensure the per-batch buffers can hold at least `capacity` tiles,
    /// growing (never shrinking) as needed, and rebuild the bind group.
    fn ensure_capacity(&mut self, device: &wgpu::Device, capacity: usize) {
        if capacity <= self.tile_capacity {
            return;
        }

        self.tile_work_buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-palrle-tile-work"),
            size: capacity as u64 * TILE_WORK_STRIDE,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.indices_buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-palrle-indices"),
            size: capacity as u64 * INDICES_STRIDE,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.errors_buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-palrle-errors"),
            size: capacity as u64 * ERROR_STRIDE,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.errors_staging = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-palrle-errors-staging"),
            size: capacity as u64 * ERROR_STRIDE,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.tile_capacity = capacity;
    }

    /// `tiles` is (tile_x, tile_y, palette_id, count, indices) where
    /// `indices` is exactly 512 bytes. `count` is the palette entry count;
    /// an index >= count is an out-of-range error the shader records.
    pub fn decode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        fb: &Framebuffer,
        tiles: &[(u8, u8, u8, u8, Vec<u8>)],
    ) {
        if tiles.is_empty() {
            self.last_batch_size = 0;
            return;
        }

        self.ensure_capacity(device, tiles.len());

        let mut tile_work = Vec::with_capacity(tiles.len() * TILE_WORK_STRIDE as usize);
        let mut indices = Vec::with_capacity(tiles.len() * INDICES_STRIDE as usize);
        for (i, (tile_x, tile_y, palette_id, count, tile_indices)) in tiles.iter().enumerate() {
            debug_assert_eq!(
                tile_indices.len(),
                INDICES_STRIDE as usize,
                "PalRle tile indices must be exactly 512 bytes"
            );
            tile_work.extend_from_slice(&(*tile_x as u32).to_le_bytes());
            tile_work.extend_from_slice(&(*tile_y as u32).to_le_bytes());
            tile_work.extend_from_slice(&(*palette_id as u32).to_le_bytes());
            tile_work.extend_from_slice(&(*count as u32).to_le_bytes());
            tile_work.extend_from_slice(&(i as u32 * INDICES_STRIDE as u32).to_le_bytes());
            // Padding: three zero u32 words.
            tile_work.extend_from_slice(&0u32.to_le_bytes());
            tile_work.extend_from_slice(&0u32.to_le_bytes());
            tile_work.extend_from_slice(&0u32.to_le_bytes());

            indices.extend_from_slice(tile_indices);
        }

        let tile_work_buffer = self
            .tile_work_buffer
            .as_ref()
            .expect("ensure_capacity set it");
        let indices_buffer = self
            .indices_buffer
            .as_ref()
            .expect("ensure_capacity set it");
        let errors_buffer = self.errors_buffer.as_ref().expect("ensure_capacity set it");
        queue.write_buffer(tile_work_buffer, 0, &tile_work);
        queue.write_buffer(indices_buffer, 0, &indices);

        // Zero the error slots for this batch before dispatch, or
        // `take_errors` would report stale failures from a previous frame.
        let zeros = vec![0u8; tiles.len() * ERROR_STRIDE as usize];
        queue.write_buffer(errors_buffer, 0, &zeros);

        let fb_view = fb
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ghostframe-palrle-bind-group"),
            layout: &self.pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.palette_atlas.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: tile_work_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: indices_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&fb_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: errors_buffer.as_entire_binding(),
                },
            ],
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-palrle-decode"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ghostframe-palrle-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            // Four workgroups per tile in a 2x2 arrangement -- see module
            // doc comment for why this can't be a single (num_tiles, 1, 1)
            // dispatch of 32x32 workgroups.
            pass.dispatch_workgroups(tiles.len() as u32, 2, 2);
        }
        queue.submit(std::iter::once(encoder.finish()));

        self.last_batch_size = tiles.len();
    }

    /// Read back and clear the per-tile error slots from the last `decode`.
    /// Returns (tile_index_within_batch, code) for each non-zero slot.
    pub fn take_errors(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> Vec<(u32, u32)> {
        if self.last_batch_size == 0 {
            return Vec::new();
        }

        let errors_buffer = self
            .errors_buffer
            .as_ref()
            .expect("decode ran, buffer exists");
        let errors_staging = self
            .errors_staging
            .as_ref()
            .expect("decode ran, buffer exists");
        let read_bytes = self.last_batch_size as u64 * ERROR_STRIDE;

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-palrle-errors-readback"),
        });
        encoder.copy_buffer_to_buffer(errors_buffer, 0, errors_staging, 0, read_bytes);
        queue.submit(std::iter::once(encoder.finish()));

        let slice = errors_staging.slice(0..read_bytes);
        slice.map_async(wgpu::MapMode::Read, |r| {
            r.expect("map palrle errors staging buffer")
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll take_errors");

        let mut result = Vec::new();
        {
            let mapped = slice.get_mapped_range().expect("get_mapped_range");
            for (i, chunk) in mapped.chunks_exact(ERROR_STRIDE as usize).enumerate() {
                let code = u32::from_le_bytes(chunk.try_into().expect("4-byte chunk"));
                if code != 0 {
                    result.push((i as u32, code));
                }
            }
        }
        errors_staging.unmap();

        result
    }
}
