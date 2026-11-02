//! Copies the damaged tiles of the framebuffer into an export buffer with a
//! compute kernel, touching no other pixel.
//!
//! The render-pass blit ([`crate::framebuffer::Framebuffer::encode_blit_rects`])
//! costs the bounding box of its draws on a tiled GPU: the pass loads and
//! stores every screen tile any draw touches, so damage in two corners costs
//! the whole frame. EGL's damage-region extensions would fix that but are not
//! reachable through wgpu. A dispatch over exactly the dirty tiles is. See
//! `shaders/client/export_copy_rgba_alias.wgsl`.
//!
//! Used only when every export buffer offers a storage texture
//! ([`crate::export::ExportedImage::storage_texture`]); the ring falls back to
//! the blit otherwise, or when asked to (`GHOSTFRAME_NO_COMPUTE_EXPORT`).

use crate::framebuffer::Framebuffer;

#[cfg(feature = "vulkan")]
const SHADER_SRC: &str = include_str!("../../shaders/client/export_copy_bgra.wgsl");
#[cfg(feature = "gles")]
const SHADER_SRC: &str = include_str!("../../shaders/client/export_copy_rgba_alias.wgsl");

/// Bytes per tile-list entry: tile_x, tile_y as u32.
const ENTRY_BYTES: usize = 8;

/// Whether the render-pass blit is the cheaper way to copy `tiles`.
///
/// A pass costs about its bounding box on a tiled GPU, the kernel about the
/// tiles it is given. Measured on Mali-T860 at 1080p (full frame 4.3 ms blit
/// against 5.1 ms compute, i.e. the kernel costs ~1.18x per tile, and the blit
/// also has the cheaper issue path for a handful of adjacent tiles), so the
/// blit wins once the damage fills about 85% of its bounding box or more, and
/// loses by a wide margin below that (half-filled checkerboard: 11.4 ms against
/// 3.7 ms).
pub(crate) fn prefers_blit(tiles: &[(u32, u32)]) -> bool {
    let Some(&(x0, y0)) = tiles.first() else {
        return true;
    };
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (x0, x0, y0, y0);
    for &(x, y) in tiles {
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }
    let bbox = u64::from(max_x - min_x + 1) * u64::from(max_y - min_y + 1);
    tiles.len() as u64 * 20 >= bbox * 17
}

/// A bind group and the things it holds that can be replaced under it.
struct Cached {
    /// [`Framebuffer::epoch`] of the view it holds.
    fb_epoch: u64,
    /// [`ExportCopy::list_generation`] of the tile list it holds.
    list_generation: u64,
    group: wgpu::BindGroup,
}

pub(crate) struct ExportCopy {
    pipeline: wgpu::ComputePipeline,
    list: Option<wgpu::Buffer>,
    /// Entries `list` holds room for.
    capacity: usize,
    /// Names the current `list`; bumped when it is reallocated.
    list_generation: u64,
    /// One per export buffer, by buffer id.
    groups: Vec<Option<Cached>>,
    /// Reused across frames so building the list does not allocate.
    bytes: Vec<u8>,
}

impl ExportCopy {
    pub(crate) fn new(device: &wgpu::Device, buffers: usize) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ghostframe-export-copy-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("ghostframe-export-copy-pipeline"),
            layout: None,
            module: &module,
            entry_point: Some("main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        ExportCopy {
            pipeline,
            list: None,
            capacity: 0,
            list_generation: 0,
            groups: (0..buffers).map(|_| None).collect(),
            bytes: Vec::new(),
        }
    }

    pub(crate) fn buffer_count(&self) -> usize {
        self.groups.len()
    }

    /// Forget the bind groups, which hold views of export textures. For when
    /// the ring's buffers are replaced.
    pub(crate) fn drop_export_cache(&mut self, buffers: usize) {
        self.groups = (0..buffers).map(|_| None).collect();
    }

    /// Record a copy of `tiles` (tile coordinates, each once) from `fb` into
    /// `dst`, the storage view of export buffer `buffer_id`; the two travel as
    /// one argument pair. Returns whether
    /// anything was recorded.
    ///
    /// The tile list goes through the queue now, which wgpu runs ahead of the
    /// next submit's commands: call this at most once per encoder.
    pub(crate) fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        fb: &Framebuffer,
        (buffer_id, dst): (usize, &wgpu::TextureView),
        tiles: &[(u32, u32)],
    ) -> bool {
        if tiles.is_empty() {
            return false;
        }
        if tiles.len() > self.capacity {
            let capacity = tiles.len().next_power_of_two();
            self.list = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ghostframe-export-copy-tiles"),
                size: (capacity * ENTRY_BYTES) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.capacity = capacity;
            self.list_generation += 1;
        }
        let list = self.list.as_ref().expect("sized above");

        self.bytes.clear();
        for (tx, ty) in tiles {
            self.bytes.extend_from_slice(&tx.to_le_bytes());
            self.bytes.extend_from_slice(&ty.to_le_bytes());
        }
        queue.write_buffer(list, 0, &self.bytes);

        let slot = &mut self.groups[buffer_id];
        let fresh = slot
            .as_ref()
            .is_some_and(|c| c.fb_epoch == fb.epoch() && c.list_generation == self.list_generation);
        if !fresh {
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ghostframe-export-copy-bind-group"),
                layout: &self.pipeline.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: list.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(fb.view()),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(dst),
                    },
                ],
            });
            *slot = Some(Cached {
                fb_epoch: fb.epoch(),
                list_generation: self.list_generation,
                group,
            });
        }
        let cached = slot.as_ref().expect("just set");
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ghostframe-export-copy-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &cached.group, &[]);
            pass.dispatch_workgroups(tiles.len() as u32, 2, 2);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::prefers_blit;

    #[test]
    fn dense_damage_takes_the_blit_and_sparse_damage_the_kernel() {
        let rect = |w: u32, h: u32| -> Vec<(u32, u32)> {
            (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).collect()
        };
        assert!(prefers_blit(&[(5, 5)]), "one tile fills its own box");
        assert!(prefers_blit(&rect(8, 8)));
        assert!(prefers_blit(&rect(60, 34)), "a full frame");
        // Two corners: 2 tiles in a 60x34 box.
        assert!(!prefers_blit(&[(0, 0), (59, 33)]));
        // A checkerboard fills half its box.
        let checker: Vec<_> = rect(60, 34)
            .into_iter()
            .filter(|(x, y)| (x + y) % 2 == 0)
            .collect();
        assert!(!prefers_blit(&checker));
        // 90% dense: a full rectangle minus every tenth tile.
        let dense: Vec<_> = rect(10, 10)
            .into_iter()
            .filter(|(x, y)| (x + y * 10) % 10 != 3)
            .collect();
        assert!(prefers_blit(&dense));
        assert!(prefers_blit(&[]));

        // The threshold itself, pinned from both sides.
        let sparse = |w: u32, h: u32, holes: &[(u32, u32)]| -> Vec<(u32, u32)> {
            rect(w, h)
                .into_iter()
                .filter(|t| !holes.contains(t))
                .collect()
        };
        // 4x5 box (20 tiles), corners kept.
        let box20 = |n_holes: usize| sparse(4, 5, &[(1, 1), (2, 2), (1, 3), (2, 1)][..n_holes]);
        assert_eq!(box20(3).len(), 17);
        assert!(prefers_blit(&box20(3)), "exactly 85% takes the blit");
        assert!(!prefers_blit(&box20(4)), "80% takes the kernel");
        // 21 of 25 is 84%.
        let holes = [(1, 1), (2, 2), (3, 1), (1, 3)];
        assert_eq!(sparse(5, 5, &holes).len(), 21);
        assert!(!prefers_blit(&sparse(5, 5, &holes)), "84% takes the kernel");
    }
}
