// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tiles that already sit in upload slabs, copied into the framebuffer by a
//! compute pass: one dispatch per slab, however many tiles and wherever they
//! are. The slab-sourced sibling of [`super::rgba::RgbaPipeline`], and what
//! `shaders/client/rgba_slab_tiles.wgsl` runs.
//!
//! The only data that crosses the queue is the work table (tile position and
//! slot, 12 bytes a tile). All the slabs of a flush share one buffer and one
//! `write_buffer`; each slab's table sits at its own 256-aligned offset and its
//! bind group reads it through a dynamic offset, so a flush that uses three
//! slabs makes one queue write and three dispatches.

use crate::framebuffer::Framebuffer;
use crate::upload_pool::UploadPool;

const SHADER_SRC: &str = include_str!("../../../shaders/client/rgba_slab_tiles.wgsl");

/// Bytes per work entry: tile_x, tile_y, slot as u32.
const WORK_STRIDE: u64 = 12;

/// One tile to draw from a slab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlabWork {
    pub tile_x: u32,
    pub tile_y: u32,
    pub slot: u32,
}

pub struct RgbaSlabPipeline {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    /// One table per pool slab, `table_stride` apart, for `tables` slabs. Grown
    /// (replaced) when the pool grows.
    work_buffer: wgpu::Buffer,
    tables: usize,
    table_stride: u64,
    /// Bytes one slab's table can need: every slot drawn once.
    table_bytes: u64,
    /// Per slab: the bind group and the [`Framebuffer::epoch`] it was built for.
    bind_groups: Vec<Option<(u64, wgpu::BindGroup)>>,
    /// Reused across flushes so a batch does not allocate it.
    scratch: Vec<u8>,
}

impl RgbaSlabPipeline {
    pub fn new(device: &wgpu::Device, pool: &UploadPool) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ghostframe-rgba-slab-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });
        // Explicit, not `layout: None`: the work table's binding needs a
        // dynamic offset, which auto layout cannot express.
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ghostframe-rgba-slab-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(WORK_STRIDE),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ghostframe-rgba-slab-pipeline-layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("ghostframe-rgba-slab-pipeline"),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        // The dynamic offset must be a multiple of the device's storage offset
        // alignment (256 on every backend we run); a table is 12 KiB, which is.
        let align = device.limits().min_storage_buffer_offset_alignment as u64;
        let table_bytes = pool.slots_per_slab() as u64 * WORK_STRIDE;
        let table_stride = table_bytes.next_multiple_of(align);
        let tables = pool.slab_count();
        RgbaSlabPipeline {
            pipeline,
            layout,
            work_buffer: Self::work_buffer(device, table_stride, tables),
            tables,
            table_stride,
            table_bytes,
            bind_groups: (0..tables).map(|_| None).collect(),
            scratch: Vec::new(),
        }
    }

    fn work_buffer(device: &wgpu::Device, table_stride: u64, tables: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-rgba-slab-work"),
            size: table_stride * tables as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// The pool grew: make room for the new slabs' tables. The bind groups
    /// hold the old buffer, so they go too.
    fn grow_to(&mut self, device: &wgpu::Device, tables: usize) {
        if tables <= self.tables {
            return;
        }
        self.work_buffer = Self::work_buffer(device, self.table_stride, tables);
        self.tables = tables;
        self.bind_groups = (0..tables).map(|_| None).collect();
    }

    /// Drop the cached bind groups, which hold the framebuffer's view, so a
    /// replaced framebuffer texture is not kept alive by them.
    pub fn drop_framebuffer_cache(&mut self) {
        self.bind_groups.iter_mut().for_each(|b| *b = None);
    }

    /// Record the copy of every tile in `batches` (`(slab, tiles)`, a slab at
    /// most once, no tile twice) into `fb`. Returns whether anything was
    /// recorded.
    ///
    /// The work tables go through the queue now, which wgpu runs ahead of the
    /// next submit's commands: call this at most once per encoder.
    pub fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        fb: &Framebuffer,
        pool: &UploadPool,
        batches: &[(usize, Vec<SlabWork>)],
    ) -> bool {
        if batches.iter().all(|(_, w)| w.is_empty()) {
            return false;
        }
        self.grow_to(device, pool.slab_count());
        debug_assert!(
            batches
                .iter()
                .all(|(_, w)| w.len() <= pool.slots_per_slab() as usize),
            "a slab cannot have more tiles than slots"
        );
        // One buffer write covering the first table to the end of the last.
        let used = batches
            .iter()
            .filter(|(_, w)| !w.is_empty())
            .map(|(slab, w)| {
                (
                    *slab as u64 * self.table_stride,
                    w.len() as u64 * WORK_STRIDE,
                )
            });
        let start = used.clone().map(|(at, _)| at).min().expect("non-empty");
        let end = used.map(|(at, len)| at + len).max().expect("non-empty");
        self.scratch.clear();
        self.scratch.resize((end - start) as usize, 0);
        for (slab, work) in batches {
            let at = (*slab as u64 * self.table_stride - start) as usize;
            for (i, w) in work.iter().enumerate() {
                let e = at + i * WORK_STRIDE as usize;
                self.scratch[e..e + 4].copy_from_slice(&w.tile_x.to_le_bytes());
                self.scratch[e + 4..e + 8].copy_from_slice(&w.tile_y.to_le_bytes());
                self.scratch[e + 8..e + 12].copy_from_slice(&w.slot.to_le_bytes());
            }
        }
        queue.write_buffer(&self.work_buffer, start, &self.scratch);

        let epoch = fb.epoch();
        for (slab, work) in batches {
            if work.is_empty() {
                continue;
            }
            if self.bind_groups[*slab].as_ref().map(|(e, _)| *e) != Some(epoch) {
                let view = pool
                    .texture(*slab)
                    .create_view(&wgpu::TextureViewDescriptor::default());
                let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("ghostframe-rgba-slab-bind-group"),
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: &self.work_buffer,
                                offset: 0,
                                size: wgpu::BufferSize::new(self.table_bytes),
                            }),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(fb.view()),
                        },
                    ],
                });
                self.bind_groups[*slab] = Some((epoch, bind_group));
            }
        }
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("ghostframe-rgba-slab-pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.pipeline);
        for (slab, work) in batches {
            if work.is_empty() {
                continue;
            }
            let (_, bind_group) = self.bind_groups[*slab].as_ref().expect("built above");
            pass.set_bind_group(0, bind_group, &[(*slab as u64 * self.table_stride) as u32]);
            pass.dispatch_workgroups(work.len() as u32, 2, 2);
        }
        true
    }
}
