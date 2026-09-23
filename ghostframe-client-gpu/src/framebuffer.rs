//! The private persistent framebuffer.
//!
//! Never exported. Tiles patch it in place and it is blitted into an export
//! buffer when a frame is published. Mirrors the web client's `Framebuffer`
//! in `ghostframe-web-client/src/webgpu/framebuffer.ts`, including the
//! preserve-on-resize copy: without that copy, tiles written before a late
//! sentinel-driven resize are lost (the new texture is zero-initialized per
//! spec, and `wgpu::Texture::destroy` on the old one would otherwise happen
//! before anything read it).

/// Format every framebuffer uses. Matches `export::FORMAT`/`FORMAT_WGPU` so
/// `blit_full`'s `copy_texture_to_texture` needs no format conversion.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Usage every framebuffer texture is created with. `STORAGE_BINDING` and
/// `TEXTURE_BINDING` are for later tasks (compute decode writes it as a
/// storage texture; the solid pipeline binds it as a sampled texture);
/// `RENDER_ATTACHMENT` is for the solid pipeline's render pass;
/// `COPY_DST`/`COPY_SRC` are for tile patches in and the export blit out.
const USAGE: wgpu::TextureUsages = wgpu::TextureUsages::STORAGE_BINDING
    .union(wgpu::TextureUsages::TEXTURE_BINDING)
    .union(wgpu::TextureUsages::RENDER_ATTACHMENT)
    .union(wgpu::TextureUsages::COPY_DST)
    .union(wgpu::TextureUsages::COPY_SRC);

/// The client's private, persistent framebuffer.
///
/// Holds the one texture tiles are decoded into and patched against across
/// the lifetime of a session. Distinct from any [`crate::export::ExportedImage`]:
/// this texture is never itself exported, only blitted into one.
pub struct Framebuffer {
    texture: wgpu::Texture,
    /// Public rather than an accessor: the M1 plan's own test asserts
    /// `fb.width`/`fb.height` directly after a resize.
    pub width: u32,
    pub height: u32,
}

impl Framebuffer {
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let texture = create_texture(device, width, height);
        Framebuffer {
            texture,
            width,
            height,
        }
    }

    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// Replace the framebuffer with a new one of `width` x `height`,
    /// preserving as much of the old content as fits at the origin.
    ///
    /// Mirrors `framebuffer.ts`'s `resize`: a fresh `wgpu::Texture` is
    /// zero-initialized, so without the copy any tiles already patched into
    /// the old texture (which can happen before a late, sentinel-driven
    /// resize on a slow QUIC startup) would simply vanish.
    pub fn resize(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, width: u32, height: u32) {
        if self.width == width && self.height == height {
            return;
        }

        let new_texture = create_texture(device, width, height);

        let copy_width = self.width.min(width);
        let copy_height = self.height.min(height);
        if copy_width > 0 && copy_height > 0 {
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ghostframe-framebuffer-resize-preserve"),
            });
            encoder.copy_texture_to_texture(
                self.texture.as_image_copy(),
                new_texture.as_image_copy(),
                wgpu::Extent3d {
                    width: copy_width,
                    height: copy_height,
                    depth_or_array_layers: 1,
                },
            );
            queue.submit(std::iter::once(encoder.finish()));
        }

        self.texture = new_texture;
        self.width = width;
        self.height = height;
    }

    /// Blit the whole framebuffer into `dst` (e.g. an
    /// [`crate::export::ExportedImage`] wrapped via `as_wgpu_texture`).
    pub fn blit_full(&self, device: &wgpu::Device, queue: &wgpu::Queue, dst: &wgpu::Texture) {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-framebuffer-blit-full"),
        });
        encoder.copy_texture_to_texture(
            self.texture.as_image_copy(),
            dst.as_image_copy(),
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(std::iter::once(encoder.finish()));
    }

    /// Copy only `rects` (PIXEL coordinates) into `dst`.
    ///
    /// One `copy_texture_to_texture` per rect in a single encoder, so a
    /// frame's damage costs one submission regardless of rect count.
    /// Zero-area rects are skipped rather than handed to wgpu: a
    /// `copy_texture_to_texture` with a zero-sized `Extent3d` is a
    /// validation error, and [`crate::coalesce::Rect::to_pixels`] can
    /// legitimately produce one when clamping a tile rect against a
    /// non-tile-aligned framebuffer edge.
    pub fn blit_rects(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        dst: &wgpu::Texture,
        rects: &[crate::coalesce::Rect],
    ) {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-framebuffer-blit-rects"),
        });
        for rect in rects {
            if rect.w == 0 || rect.h == 0 {
                continue;
            }
            let origin = wgpu::Origin3d {
                x: rect.x,
                y: rect.y,
                z: 0,
            };
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: dst,
                    mip_level: 0,
                    origin,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: rect.w,
                    height: rect.h,
                    depth_or_array_layers: 1,
                },
            );
        }
        queue.submit(std::iter::once(encoder.finish()));
    }

    /// Fill the whole framebuffer with one RGBA colour. Test use only.
    pub fn debug_fill(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, rgba: [u8; 4]) {
        let pixel_count = self.width as usize * self.height as usize;
        let mut data = Vec::with_capacity(pixel_count * 4);
        for _ in 0..pixel_count {
            data.extend_from_slice(&rgba);
        }

        queue.write_texture(
            self.texture.as_image_copy(),
            &data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.width * 4),
                rows_per_image: Some(self.height),
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        // Make the write visible to a subsequent debug_read/blit in the
        // same test without requiring the caller to poll first.
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll after debug_fill");
    }

    /// Fill one 32x32 tile at tile coordinates `(tile_x, tile_y)` with one
    /// RGBA colour. Test use only. Clamped to the framebuffer edge, mirroring
    /// [`crate::coalesce::Rect::to_pixels`]'s clamp for a non-tile-aligned
    /// bottom/right edge.
    pub fn debug_fill_tile(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        tile_x: u32,
        tile_y: u32,
        rgba: [u8; 4],
    ) {
        const T: u32 = 32;
        let x = tile_x * T;
        let y = tile_y * T;
        let w = T.min(self.width.saturating_sub(x));
        let h = T.min(self.height.saturating_sub(y));
        if w == 0 || h == 0 {
            return;
        }

        let mut data = Vec::with_capacity(w as usize * h as usize * 4);
        for _ in 0..(w * h) {
            data.extend_from_slice(&rgba);
        }

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll after debug_fill_tile");
    }

    /// Read the framebuffer back as tight RGBA. Test use only.
    pub fn debug_read(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> Vec<u8> {
        let unpadded_bytes_per_row = self.width * 4;
        let padded_bytes_per_row = unpadded_bytes_per_row
            .div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

        let buffer_size = padded_bytes_per_row as u64 * self.height as u64;
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-framebuffer-debug-read"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-framebuffer-debug-read"),
        });
        encoder.copy_texture_to_buffer(
            self.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(std::iter::once(encoder.finish()));

        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| {
            r.expect("map debug_read staging buffer")
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll debug_read");

        let padded = slice.get_mapped_range().expect("get_mapped_range");

        // Strip row padding: the staging buffer's rows are padded to
        // `COPY_BYTES_PER_ROW_ALIGNMENT`, but callers want a tight
        // `width * height * 4` buffer. Skipping this looks correct at the
        // start of each row and drifts further out of alignment with every
        // row after that.
        let mut tight = Vec::with_capacity(unpadded_bytes_per_row as usize * self.height as usize);
        for row in 0..self.height as usize {
            let start = row * padded_bytes_per_row as usize;
            let end = start + unpadded_bytes_per_row as usize;
            tight.extend_from_slice(&padded[start..end]);
        }

        tight
    }
}

fn create_texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ghostframe-framebuffer"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: USAGE,
        view_formats: &[],
    })
}
