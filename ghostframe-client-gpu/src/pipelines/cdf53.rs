//! CDF 5/3 progressive wavelet decode: five compute pipelines ported from
//! `ghostframe-web-client/src/webgpu/cdf53.ts`. Buffer layouts and dispatch
//! shapes are ported from that file, not re-derived.
//!
//! Persistent per-tile state (coefficients, signs, generation,
//! passes-processed) lives in storage buffers sized by
//! [`Cdf53Pipeline::resize`] for a `cols` x `rows` tile grid. A batch of
//! arrived passes is uploaded by [`Cdf53Pipeline::integrate`], which also
//! tracks `received_mask` / `present_passes` / `passes_processed` on the
//! CPU (mirroring `Cdf53Pipeline.uploadBatch` in the TS reference) using
//! [`crate::pipelines::cdf53_passes::passes_processed`] for the sparse-K
//! rule. [`Cdf53Pipeline::inverse`] runs the four-stage inverse lifting
//! chain (L3 -> L2 -> L1 -> L1 pass2) for every tile slot, writing
//! reconstructed pixels into the framebuffer.
//!
//! ## Generation
//!
//! The TS reference tracks a per-tile `gen` so a tile whose generation
//! bumps mid-batch gets its coefficient/sign state cleared before the new
//! pass OR-integrates into it (a fresh encode never blends with a stale
//! one). This pipeline's `integrate` signature carries no generation --
//! see the task's public API -- so that clearing is out of scope here: a
//! freshly `resize`d storage buffer is zero-initialized by wgpu, and every
//! `tile_work` entry writes a constant non-zero `gen` (1) purely so the
//! inverse shaders' `tileGen[tile_idx] == 0` early-exit sees the tile as
//! active. Generation-bump handling belongs to a caller that has a
//! generation to hand it.

use crate::framebuffer::Framebuffer;
use crate::pipelines::cdf53_passes::passes_processed;

const INTEGRATE_SRC: &str = include_str!("../../../shaders/client/cdf53_integrate.wgsl");
const INVERSE_L3_SRC: &str = include_str!("../../../shaders/client/cdf53_inverse_l3.wgsl");
const INVERSE_L2_SRC: &str = include_str!("../../../shaders/client/cdf53_inverse_l2.wgsl");
const INVERSE_L1_SRC: &str = include_str!("../../../shaders/client/cdf53_inverse_l1.wgsl");
const INVERSE_L1_PASS2_SRC: &str =
    include_str!("../../../shaders/client/cdf53_inverse_l1_pass2.wgsl");

/// All 14 Cdf53 pass bits.
const FULL_PASS_MASK: u16 = (1 << 14) - 1;

/// Per-tile coefficient buffer size: 3 channels x 1024 coeffs, packed two
/// i16 lanes per u32 => 1536 u32 => 6144 bytes.
const COEFFICIENT_STRIDE: u64 = 6144;
/// Per-tile sign buffer size: 3 x 1024 bits, packed as u32 words => 96 u32
/// => 384 bytes.
const SIGN_STRIDE: u64 = 384;
/// Per-tile bit-planes payload: 3 channels x 128 bytes = 384 bytes.
const BIT_PLANES_STRIDE: u64 = 384;
/// Per-batch-entry `TileWorkEntry`: 5 x u32 = 20 bytes.
const TILE_WORK_STRIDE: u64 = 20;
/// Per-tile inverse-transform scratch: 3 channels x 32 x 32 x 4 bytes.
const WORK_AREA_STRIDE: u64 = 12288;
/// Passes per tile, the per-tile budget of a batch's tileWork/bitPlanes
/// buffers.
const MAX_PASSES_PER_TILE: u64 = 14;

/// One arrived Cdf53 pass: (tile_x, tile_y, pass_idx, bit_planes,
/// present_passes). `bit_planes` is exactly 384 bytes; `present_passes` is
/// `Some` only on pass 0.
pub type Cdf53PassEntry = (u8, u8, u8, Vec<u8>, Option<u16>);

/// `coefficient`, `sign` and `dirty_tiles` are never read back by field
/// name after `resize` builds the cached bind groups from them -- they are
/// held here only so the buffers outlive this struct (dropping the last
/// `wgpu::Buffer` handle would be premature even though the bind groups
/// hold their own internal reference, since nothing else names them).
#[allow(dead_code)]
struct PersistentBuffers {
    coefficient: wgpu::Buffer,
    sign: wgpu::Buffer,
    tile_gen: wgpu::Buffer,
    passes_processed: wgpu::Buffer,
    dirty_tiles: wgpu::Buffer,
    dirty_tiles_count: wgpu::Buffer,
    tile_work: wgpu::Buffer,
    bit_planes: wgpu::Buffer,
    work_area: wgpu::Buffer,
}

/// One tile's CPU-side pass-tracking state, mirroring `Cdf53Pipeline`'s
/// `receivedMaskCpu` / `presentPassesCpu` / `passesProcessedCpu` in the TS
/// reference (there is no `lastSeenGen` here -- see the module doc).
#[derive(Clone, Copy, Default)]
struct TileTrack {
    received_mask: u16,
    present_passes: u16,
    passes_processed: u32,
}

pub struct Cdf53Pipeline {
    integrate_pipeline: wgpu::ComputePipeline,
    inverse_l3_pipeline: wgpu::ComputePipeline,
    inverse_l2_pipeline: wgpu::ComputePipeline,
    inverse_l1_pipeline: wgpu::ComputePipeline,
    inverse_l1_pass2_pipeline: wgpu::ComputePipeline,

    uniforms: wgpu::Buffer,
    buffers: Option<PersistentBuffers>,

    integrate_bind_group: Option<wgpu::BindGroup>,
    inverse_l3_bind_group: Option<wgpu::BindGroup>,
    inverse_l2_bind_group: Option<wgpu::BindGroup>,
    inverse_l1_bind_group: Option<wgpu::BindGroup>,

    cols: u32,
    max_tiles: u32,

    tracks: Vec<TileTrack>,
}

impl Cdf53Pipeline {
    pub fn new(device: &wgpu::Device) -> Self {
        let make_module = |label: &str, src: &str| {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(src.into()),
            })
        };
        let integrate_module = make_module("ghostframe-cdf53-integrate-shader", INTEGRATE_SRC);
        let inverse_l3_module = make_module("ghostframe-cdf53-inverse-l3-shader", INVERSE_L3_SRC);
        let inverse_l2_module = make_module("ghostframe-cdf53-inverse-l2-shader", INVERSE_L2_SRC);
        let inverse_l1_module = make_module("ghostframe-cdf53-inverse-l1-shader", INVERSE_L1_SRC);
        let inverse_l1_pass2_module = make_module(
            "ghostframe-cdf53-inverse-l1-pass2-shader",
            INVERSE_L1_PASS2_SRC,
        );

        let make_pipeline = |label: &str, module: &wgpu::ShaderModule| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: None,
                module,
                entry_point: Some("main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: None,
            })
        };
        let integrate_pipeline =
            make_pipeline("ghostframe-cdf53-integrate-pipeline", &integrate_module);
        let inverse_l3_pipeline =
            make_pipeline("ghostframe-cdf53-inverse-l3-pipeline", &inverse_l3_module);
        let inverse_l2_pipeline =
            make_pipeline("ghostframe-cdf53-inverse-l2-pipeline", &inverse_l2_module);
        let inverse_l1_pipeline =
            make_pipeline("ghostframe-cdf53-inverse-l1-pipeline", &inverse_l1_module);
        let inverse_l1_pass2_pipeline = make_pipeline(
            "ghostframe-cdf53-inverse-l1-pass2-pipeline",
            &inverse_l1_pass2_module,
        );

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-cdf53-uniforms"),
            // Uniforms { cols: u32, _pad: vec3<u32> } -- vec3 alignment
            // forces stride 32.
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Cdf53Pipeline {
            integrate_pipeline,
            inverse_l3_pipeline,
            inverse_l2_pipeline,
            inverse_l1_pipeline,
            inverse_l1_pass2_pipeline,
            uniforms,
            buffers: None,
            integrate_bind_group: None,
            inverse_l3_bind_group: None,
            inverse_l2_bind_group: None,
            inverse_l1_bind_group: None,
            cols: 0,
            max_tiles: 0,
            tracks: Vec::new(),
        }
    }

    /// Resize per-tile storage for a framebuffer of `cols` x `rows` tiles.
    ///
    /// Idempotent for the same shape; reallocates (never shrinks) on
    /// growth, mirroring the TS reference's `resize`.
    pub fn resize(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, cols: u32, rows: u32) {
        let mut cols_buf = Vec::with_capacity(16);
        cols_buf.extend_from_slice(&cols.to_le_bytes());
        cols_buf.extend_from_slice(&[0u8; 12]);
        queue.write_buffer(&self.uniforms, 0, &cols_buf);
        self.cols = cols;

        let max_tiles = (cols as u64 * rows as u64).max(1) as u32;
        if max_tiles == self.max_tiles && self.buffers.is_some() {
            return;
        }

        self.tracks = vec![TileTrack::default(); max_tiles as usize];

        let coefficient = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-cdf53-coefficient"),
            size: max_tiles as u64 * COEFFICIENT_STRIDE,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let sign = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-cdf53-sign"),
            size: max_tiles as u64 * SIGN_STRIDE,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let tile_gen = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-cdf53-tile-gen"),
            size: max_tiles as u64 * 4,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let passes_processed_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-cdf53-passes-processed"),
            size: max_tiles as u64 * 4,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let dirty_tiles = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-cdf53-dirty-tiles"),
            size: max_tiles as u64 * 4,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let dirty_tiles_count = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-cdf53-dirty-tiles-count"),
            // 4 u32: count + 3 padding.
            size: 16,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::INDIRECT,
            mapped_at_creation: false,
        });
        // Per-batch budget: a batch holds at most max_tiles x 14 passes.
        let max_batch_entries = max_tiles as u64 * MAX_PASSES_PER_TILE;
        let tile_work = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-cdf53-tile-work"),
            size: max_batch_entries * TILE_WORK_STRIDE,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bit_planes = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-cdf53-bit-planes"),
            size: max_batch_entries * BIT_PLANES_STRIDE,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let work_area = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-cdf53-work-area"),
            size: max_tiles as u64 * WORK_AREA_STRIDE,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });

        let integrate_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ghostframe-cdf53-integrate-bind-group"),
            layout: &self.integrate_pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: tile_work.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: bit_planes.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: coefficient.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: sign.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: tile_gen.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: dirty_tiles.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: dirty_tiles_count.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: self.uniforms.as_entire_binding(),
                },
            ],
        });

        let make_inverse_bind_group = |pipeline: &wgpu::ComputePipeline, label: &str| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &pipeline.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: coefficient.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: sign.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: tile_gen.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: work_area.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: passes_processed_buf.as_entire_binding(),
                    },
                ],
            })
        };
        let inverse_l3_bind_group = make_inverse_bind_group(
            &self.inverse_l3_pipeline,
            "ghostframe-cdf53-inverse-l3-bind-group",
        );
        let inverse_l2_bind_group = make_inverse_bind_group(
            &self.inverse_l2_pipeline,
            "ghostframe-cdf53-inverse-l2-bind-group",
        );
        let inverse_l1_bind_group = make_inverse_bind_group(
            &self.inverse_l1_pipeline,
            "ghostframe-cdf53-inverse-l1-bind-group",
        );

        self.buffers = Some(PersistentBuffers {
            coefficient,
            sign,
            tile_gen,
            passes_processed: passes_processed_buf,
            dirty_tiles,
            dirty_tiles_count,
            tile_work,
            bit_planes,
            work_area,
        });
        self.integrate_bind_group = Some(integrate_bind_group);
        self.inverse_l3_bind_group = Some(inverse_l3_bind_group);
        self.inverse_l2_bind_group = Some(inverse_l2_bind_group);
        self.inverse_l1_bind_group = Some(inverse_l1_bind_group);
        self.max_tiles = max_tiles;
    }

    /// `tiles` is (tile_x, tile_y, pass_idx, bit_planes, present_passes).
    /// `bit_planes` is exactly 384 bytes. `present_passes` is Some only on
    /// pass 0.
    pub fn integrate(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        tiles: &[Cdf53PassEntry],
    ) {
        if tiles.is_empty() {
            return;
        }
        let buffers = self
            .buffers
            .as_ref()
            .expect("resize must be called before integrate");
        assert!(
            tiles.len() as u64 <= self.max_tiles as u64 * MAX_PASSES_PER_TILE,
            "Cdf53 batch {} exceeds capacity {}",
            tiles.len(),
            self.max_tiles as u64 * MAX_PASSES_PER_TILE
        );

        let mut tile_work = Vec::with_capacity(tiles.len() * TILE_WORK_STRIDE as usize);
        let mut bit_planes_data = Vec::with_capacity(tiles.len() * BIT_PLANES_STRIDE as usize);

        for (i, (tile_x, tile_y, pass_idx, bit_planes, present_passes)) in tiles.iter().enumerate()
        {
            debug_assert_eq!(
                bit_planes.len(),
                BIT_PLANES_STRIDE as usize,
                "Cdf53 bit_planes must be exactly 384 bytes"
            );
            let tile_idx = *tile_y as u32 * self.cols + *tile_x as u32;
            let track = &mut self.tracks[tile_idx as usize];

            track.received_mask |= 1u16 << pass_idx;
            if let Some(present) = present_passes {
                track.present_passes = present & FULL_PASS_MASK;
            }
            track.passes_processed = passes_processed(
                track.received_mask,
                track.present_passes,
                track.passes_processed,
                *pass_idx,
            );
            queue.write_buffer(
                &buffers.passes_processed,
                tile_idx as u64 * 4,
                &track.passes_processed.to_le_bytes(),
            );

            tile_work.extend_from_slice(&(*tile_x as u32).to_le_bytes());
            tile_work.extend_from_slice(&(*tile_y as u32).to_le_bytes());
            // gen: constant non-zero marker -- see module doc for why this
            // pipeline carries no real generation.
            tile_work.extend_from_slice(&1u32.to_le_bytes());
            tile_work.extend_from_slice(&(*pass_idx as u32).to_le_bytes());
            // u32 index into bitPlanes: 96 u32 (384 bytes) per entry.
            tile_work.extend_from_slice(&(i as u32 * 96).to_le_bytes());
            bit_planes_data.extend_from_slice(bit_planes);
        }

        queue.write_buffer(&buffers.tile_work, 0, &tile_work);
        queue.write_buffer(&buffers.bit_planes, 0, &bit_planes_data);
        queue.write_buffer(&buffers.dirty_tiles_count, 0, &[0u8; 16]);

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-cdf53-integrate"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ghostframe-cdf53-integrate-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.integrate_pipeline);
            pass.set_bind_group(0, self.integrate_bind_group.as_ref().unwrap(), &[]);
            pass.dispatch_workgroups(tiles.len() as u32, 1, 1);
        }
        queue.submit(std::iter::once(encoder.finish()));
    }

    /// Run the inverse levels for all dirty tiles into `fb`.
    ///
    /// Always dispatches one workgroup per possible tile slot; each inverse
    /// shader early-exits when `tileGen[tile_idx] == 0`. This keeps the
    /// framebuffer fresh across repeated calls even when no new passes
    /// arrived: the persistent coefficient/sign state for a previously
    /// integrated tile is still there, so re-running the inverse reproduces
    /// the same pixels.
    pub fn inverse(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, fb: &Framebuffer) {
        let buffers = self
            .buffers
            .as_ref()
            .expect("resize must be called before inverse");
        let wg_cap = self.max_tiles.max(1);

        let fb_view = fb
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let inverse_l1_pass2_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ghostframe-cdf53-inverse-l1-pass2-bind-group"),
            layout: &self.inverse_l1_pass2_pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffers.tile_gen.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: buffers.work_area.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&fb_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.uniforms.as_entire_binding(),
                },
            ],
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-cdf53-inverse"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ghostframe-cdf53-inverse-l3-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.inverse_l3_pipeline);
            pass.set_bind_group(0, self.inverse_l3_bind_group.as_ref().unwrap(), &[]);
            pass.dispatch_workgroups(wg_cap, 1, 1);
        }
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ghostframe-cdf53-inverse-l2-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.inverse_l2_pipeline);
            pass.set_bind_group(0, self.inverse_l2_bind_group.as_ref().unwrap(), &[]);
            pass.dispatch_workgroups(wg_cap, 1, 1);
        }
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ghostframe-cdf53-inverse-l1-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.inverse_l1_pipeline);
            pass.set_bind_group(0, self.inverse_l1_bind_group.as_ref().unwrap(), &[]);
            // Four workgroups per tile -- one per 16x16 quadrant of the
            // 32x32 tile. See cdf53_inverse_l1.wgsl's module doc.
            pass.dispatch_workgroups(wg_cap * 4, 1, 1);
        }
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ghostframe-cdf53-inverse-l1-pass2-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.inverse_l1_pass2_pipeline);
            pass.set_bind_group(0, &inverse_l1_pass2_bind_group, &[]);
            pass.dispatch_workgroups(wg_cap, 1, 1);
        }
        queue.submit(std::iter::once(encoder.finish()));
    }
}
