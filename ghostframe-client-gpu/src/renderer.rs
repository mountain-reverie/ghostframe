//! The façade that ties the pipelines, the framebuffer and the export ring
//! together into the one type `ghostframe-client-native` talks to.
//!
//! [`Renderer::apply_event`] is the routing point from `ClientCore`'s
//! `Event`s to the right pipeline. It borrows `WgpuContext` rather than
//! owning it -- the context (instance/adapter/device/queue) is created once
//! per process and shared across sessions in the native client, while a
//! `Renderer` is created per session (it owns per-session framebuffer/ring
//! state sized to that session's frame dimensions).

use crate::export::ExportedImage;
use crate::framebuffer::Framebuffer;
use crate::pipelines::cdf53::{Cdf53PassEntry, Cdf53Pipeline};
use crate::pipelines::palrle::PalRlePipeline;
use crate::pipelines::raw;
use crate::pipelines::solid::SolidPipeline;
use crate::ring::{ExportRing, PublishedFrame};
use crate::wgpu_ctx::WgpuContext;
use crate::GpuError;

use ghostframe_client_core::{DecodeErrorCode, Event, TileData};
use ghostframe_protocol::protocol::Codec;
use ghostframe_protocol::tile::TILE_SIZE;

fn tile_grid(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(TILE_SIZE), height.div_ceil(TILE_SIZE))
}

/// Owns the framebuffer, the decode pipelines, and the export ring (which in
/// turn owns the dirty history) for one client session.
///
/// Per-frame tile work is batched: `apply_event` only records what arrived
/// and marks the affected tile dirty in the ring's history; `flush` is what
/// actually runs the render/compute passes, once per frame, rather than one
/// pass per tile.
pub struct Renderer {
    fb: Framebuffer,
    ring: ExportRing,
    solid: SolidPipeline,
    palrle: PalRlePipeline,
    cdf53: Cdf53Pipeline,

    pending_solid: Vec<(u8, u8, [u8; 4])>,
    pending_palrle: Vec<(u8, u8, u8, u8, Vec<u8>)>,
    pending_cdf53: Vec<Cdf53PassEntry>,

    /// Lazily created: a client that never receives H.264 never opens a
    /// decoder, and a machine without VA-API never can.
    h264_decoder: Option<ghostframe_client_h264::decoder::H264Decoder>,
    h264_pipeline: Option<crate::pipelines::h264_nv12::H264Nv12Pipeline>,
    /// Logged once, so the import path taken is visible without spamming
    /// every frame.
    h264_path_logged: bool,
}

impl Renderer {
    /// `export_buffers` must be at least 1 -- a ring with zero buffers can
    /// never hand `publish` a free one, so the client would connect and
    /// then never show a frame. Rejected here rather than clamped, since a
    /// silent clamp would hide a misconfigured caller behind a working demo
    /// and a broken embed.
    pub fn new(
        ctx: &WgpuContext,
        width: u32,
        height: u32,
        export_buffers: usize,
        preferred_modifiers: &[u64],
        // Diagnostic-only: makes `debug_map_frame` possible at the cost of
        // pinning exports to CPU-visible memory. Production passes false.
        host_visible: bool,
    ) -> Result<Self, GpuError> {
        if export_buffers == 0 {
            return Err(GpuError::NoExportBuffers);
        }
        let fb = Framebuffer::new(&ctx.device, width, height);
        let ring = ExportRing::new(
            ctx,
            width,
            height,
            export_buffers,
            preferred_modifiers,
            host_visible,
        )?;

        let mut solid = SolidPipeline::new(&ctx.device);
        solid.set_canvas_size(&ctx.device, &ctx.queue, width, height);

        let palrle = PalRlePipeline::new(&ctx.device);

        let mut cdf53 = Cdf53Pipeline::new(&ctx.device);
        let (cols, rows) = tile_grid(width, height);
        cdf53.resize(&ctx.device, &ctx.queue, cols, rows);

        Ok(Renderer {
            fb,
            ring,
            solid,
            palrle,
            cdf53,
            pending_solid: Vec::new(),
            pending_palrle: Vec::new(),
            pending_cdf53: Vec::new(),
            h264_decoder: None,
            h264_pipeline: None,
            h264_path_logged: false,
        })
    }

    /// Route one `ClientCore` event to the right pipeline and mark the tile
    /// dirty. Batches within a frame; [`Renderer::flush`] submits.
    ///
    /// Every `Event` variant is matched explicitly -- see this crate's
    /// module doc on `clippy::wildcard_enum_match_arm` for why a `_ =>` arm
    /// is refused here.
    pub fn apply_event(&mut self, ctx: &WgpuContext, ev: &Event) {
        match ev {
            Event::TileReady {
                tile_x,
                tile_y,
                rgba,
                ..
            } => {
                // Not the path this crate exists to test (that's
                // `TilePayload`, decoded by the real WGSL below), but a
                // valid event this renderer must still handle correctly:
                // the bytes are already final RGBA pixels, so upload them
                // straight into the framebuffer, same as `pipelines::raw`
                // does for `Codec::Raw` (short of the BGRA swizzle, since
                // there is none to do here).
                upload_rgba_tile(&ctx.queue, &self.fb, *tile_x, *tile_y, rgba);
                self.ring.mark_dirty(*tile_x as u32, *tile_y as u32);
            }
            Event::TilePayload {
                tile_x,
                tile_y,
                generation,
                data,
                ..
            } => {
                match data {
                    TileData::Raw(bgra) => {
                        raw::upload_raw_tile(&ctx.queue, &self.fb, *tile_x, *tile_y, bgra);
                    }
                    TileData::Solid(bgra) => {
                        self.pending_solid.push((*tile_x, *tile_y, *bgra));
                    }
                    TileData::PalRle {
                        palette_id,
                        count,
                        indices,
                    } => {
                        self.pending_palrle.push((
                            *tile_x,
                            *tile_y,
                            *palette_id,
                            *count,
                            indices.clone(),
                        ));
                    }
                    TileData::Cdf53 {
                        pass_idx,
                        bit_planes,
                        present_passes,
                    } => {
                        self.pending_cdf53.push((
                            *tile_x,
                            *tile_y,
                            *generation,
                            *pass_idx,
                            bit_planes.clone(),
                            *present_passes,
                        ));
                    }
                }
                self.ring.mark_dirty(*tile_x as u32, *tile_y as u32);
            }
            Event::FrameDimensions { width, height } => {
                // Run any batched work against the OLD framebuffer size
                // before resizing -- the pending tiles were addressed
                // against it.
                self.flush(ctx);

                self.fb.resize(&ctx.device, &ctx.queue, *width, *height);
                if let Err(e) = self.ring.resize(ctx, *width, *height) {
                    tracing::error!(error = %e, width, height, "export ring resize failed");
                }
                self.solid
                    .set_canvas_size(&ctx.device, &ctx.queue, *width, *height);
                let (cols, rows) = tile_grid(*width, *height);
                self.cdf53.resize(&ctx.device, &ctx.queue, cols, rows);
            }
            Event::NeedsH264 {
                frame_seq,
                payload,
                is_keyframe,
                ..
            } => {
                self.decode_h264(ctx, *frame_seq, *is_keyframe, payload);
            }
            Event::DecodeError {
                codec,
                tile_x,
                tile_y,
                code,
            } => {
                // `ClientCore` already reports this over the feedback
                // stream (`decode_error_batcher`); the renderer has no
                // separate telemetry sink of its own for M1, so this is a
                // deliberate no-op rather than a silent drop -- logged so a
                // real decode failure is still visible locally.
                log_decode_error(*codec, *tile_x, *tile_y, *code);
            }
            Event::PaletteUpdated { palette_id, colors } => {
                // `colors` reports only `count` entries (see the type doc
                // on `Event::PaletteUpdated`); pad the remainder of the
                // 16-slot atlas row with zeros. Safe: `palrle_decode.wgsl`
                // rejects any index >= count as ERR_INDEX_OOB before it
                // would ever read a padding slot.
                let mut full = [[0u8; 4]; 16];
                for (slot, c) in full.iter_mut().zip(colors.iter()) {
                    *slot = *c;
                }
                self.palrle.upload_palette(&ctx.queue, *palette_id, &full);
            }
        }
    }

    /// Submit all batched work.
    pub fn flush(&mut self, ctx: &WgpuContext) {
        if !self.pending_solid.is_empty() {
            self.solid
                .draw(&ctx.device, &ctx.queue, &self.fb, &self.pending_solid);
            self.pending_solid.clear();
        }
        if !self.pending_palrle.is_empty() {
            self.palrle
                .decode(&ctx.device, &ctx.queue, &self.fb, &self.pending_palrle);
            self.pending_palrle.clear();
        }
        if !self.pending_cdf53.is_empty() {
            self.cdf53
                .integrate(&ctx.device, &ctx.queue, &self.pending_cdf53);
            self.pending_cdf53.clear();
            self.cdf53.inverse(&ctx.device, &ctx.queue, &self.fb);
        }
    }

    /// Seal a generation and hand out an export buffer, if one is free.
    pub fn publish(&mut self, ctx: &WgpuContext) -> Option<PublishedFrame> {
        self.ring.publish(&ctx.device, &ctx.queue, &self.fb)
    }

    pub fn release(&mut self, frame_id: u32) {
        self.ring.release(frame_id);
    }

    /// The exported dmabuf image for a buffer id `publish` handed out.
    pub fn export_buffer(&self, buffer_id: u32) -> &ExportedImage {
        self.ring.buffer(buffer_id)
    }

    pub fn debug_read_framebuffer(&self, ctx: &WgpuContext) -> Vec<u8> {
        self.fb.debug_read(&ctx.device, &ctx.queue)
    }

    /// Decode one access unit and blit every frame it produced.
    ///
    /// Failures are logged, never fatal: a corrupt access unit (FEC failed
    /// to recover one) must not take down the session, and the next
    /// keyframe recovers.
    fn decode_h264(&mut self, ctx: &WgpuContext, frame_seq: u32, is_keyframe: bool, au: &[u8]) {
        if self.h264_decoder.is_none() {
            match ghostframe_client_h264::decoder::H264Decoder::new() {
                Ok(d) => self.h264_decoder = Some(d),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "H.264 arrived but no decoder could be opened -- the capability \
                         was advertised without hardware to back it"
                    );
                    return;
                }
            }
        }
        if self.h264_pipeline.is_none() {
            self.h264_pipeline = Some(crate::pipelines::h264_nv12::H264Nv12Pipeline::new(
                &ctx.device,
            ));
        }

        let decoder = self.h264_decoder.as_mut().expect("just created");

        // Timed at `debug` level so M3's decode-cost measurement (design doc
        // §10) can isolate the hardware decode stall -- submit to surface
        // available -- from everything else `decode_h264` does, without
        // adding an unconditional cost to the hot path -- off by default, on
        // with `RUST_LOG=ghostframe_client_gpu::renderer=debug`. Mirrors the
        // precedent in `ring.rs`'s `publish` timing exactly.
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real hardware H.264 decode stall for M3 §10's decode-thread decision, not a virtual-clock path"
        )]
        let decode_start = std::time::Instant::now();
        let frames = match decoder.decode(au) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(frame_seq, is_keyframe, error = %e, "H.264 decode failed");
                return;
            }
        };
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real hardware H.264 decode stall for M3 §10's decode-thread decision, not a virtual-clock path"
        )]
        let decode_us = decode_start.elapsed().as_micros() as u64;
        tracing::debug!(
            frame_seq,
            is_keyframe,
            decode_us,
            frame_count = frames.len(),
            "decode_h264: decoder.decode stall"
        );

        for frame in frames {
            self.blit_h264_frame(ctx, frame_seq, &frame);
        }
    }

    /// Blit one decoded H.264 frame into the framebuffer.
    ///
    /// Each `HwFrame` holds a VA-API surface out of the decoder's pool for
    /// as long as it lives. This function drops every frame within the loop
    /// iteration it was decoded in (the `for frame in frames` loop in
    /// [`Renderer::decode_h264`]), so nothing is held across presents and
    /// the decoder's default pool size is fine -- but that is a property of
    /// this code, not a guarantee. A later change that queues frames (e.g.
    /// to hand them to a compositor across a frame boundary) makes the next
    /// `receive_frame` stall on pool exhaustion rather than fail, which
    /// reads as a hang, not a decode error.
    fn blit_h264_frame(
        &mut self,
        ctx: &WgpuContext,
        frame_seq: u32,
        frame: &ghostframe_client_h264::decoder::HwFrame,
    ) {
        // A decoded size that disagrees with the framebuffer means the tile
        // stream's FrameDimensions and the H.264 SPS disagree. Blitting
        // anyway corrupts the framebuffer; dropping recovers at the next
        // keyframe.
        if frame.width() != self.fb.width || frame.height() != self.fb.height {
            tracing::warn!(
                frame_seq,
                decoded_w = frame.width(),
                decoded_h = frame.height(),
                fb_w = self.fb.width,
                fb_h = self.fb.height,
                "dropping H.264 frame whose size disagrees with the framebuffer"
            );
            return;
        }

        let pipeline = self.h264_pipeline.as_mut().expect("created by caller");

        // Preferred path: import the decoder's dmabuf directly. Falls back
        // to a CPU download when the surface is tiled, misaligned, or the
        // driver's linear pitch disagrees with the producer's -- all of
        // which `import_nv12` reports rather than rendering wrong pixels.
        // The import (and the `MappedFrame` it borrows planes from) is not
        // kept alive past this call: `ImportedNv12` has no `Drop` of its
        // own (Task 6) -- each texture carries a wgpu drop callback that
        // destroys its `VkImage`, and they share an `Arc` whose drop frees
        // the backing memory, so wgpu-core keeps everything alive until the
        // submission below retires. No `poll(Wait)` needed here.
        let imported = frame.map_dmabuf().and_then(|mapped| {
            crate::import::import_nv12(ctx, mapped.planes())
                .map_err(|e| ghostframe_client_h264::H264Error::Ffmpeg(e.to_string()))
        });

        match imported {
            Ok(imported) => {
                if !self.h264_path_logged {
                    tracing::info!("H.264 import path: zero-copy dmabuf");
                    self.h264_path_logged = true;
                }
                pipeline.draw(
                    &ctx.device,
                    &ctx.queue,
                    &self.fb,
                    imported.luma(),
                    imported.chroma(),
                );
            }
            Err(e) => {
                if !self.h264_path_logged {
                    tracing::info!(
                        reason = %e,
                        "H.264 import path: CPU copy (dmabuf import unavailable)"
                    );
                    self.h264_path_logged = true;
                }
                let Some((luma, chroma)) = frame.download_nv12() else {
                    tracing::warn!(frame_seq, "H.264 CPU download failed");
                    return;
                };
                let (luma_tex, chroma_tex) =
                    crate::pipelines::h264_nv12::H264Nv12Pipeline::upload_planes(
                        &ctx.device,
                        &ctx.queue,
                        self.fb.width,
                        self.fb.height,
                        &luma,
                        &chroma,
                    );
                pipeline.draw(&ctx.device, &ctx.queue, &self.fb, &luma_tex, &chroma_tex);
            }
        }

        // H.264 replaces the whole frame, so every tile is dirty.
        self.ring.mark_dirty_all();
    }
}

fn log_decode_error(codec: Codec, tile_x: u8, tile_y: u8, code: DecodeErrorCode) {
    tracing::warn!(?codec, tile_x, tile_y, ?code, "tile decode error");
}

/// Upload an already-RGBA tile (`Event::TileReady`'s payload) directly into
/// `fb` at tile coordinates `(tile_x, tile_y)`.
///
/// Mirrors [`raw::upload_raw_tile`]'s partial-row and edge-clamping
/// handling, but performs no BGRA -> RGBA swizzle: `TileReady::rgba` is
/// already RGBA.
fn upload_rgba_tile(queue: &wgpu::Queue, fb: &Framebuffer, tile_x: u8, tile_y: u8, rgba: &[u8]) {
    let row_bytes = (TILE_SIZE * 4) as usize;
    let full_rows = (rgba.len() / row_bytes).min(TILE_SIZE as usize);
    if full_rows == 0 {
        return;
    }

    let x = tile_x as u32 * TILE_SIZE;
    let y = tile_y as u32 * TILE_SIZE;
    let w = TILE_SIZE.min(fb.width.saturating_sub(x));
    let h = (full_rows as u32).min(fb.height.saturating_sub(y));
    if w == 0 || h == 0 {
        return;
    }

    // Clip to `w` columns per row, matching `upload_raw_tile`'s handling of
    // a framebuffer edge narrower than a full tile.
    let mut tight = Vec::with_capacity(w as usize * h as usize * 4);
    for row in 0..h as usize {
        let row_start = row * row_bytes;
        let row_end = row_start + w as usize * 4;
        tight.extend_from_slice(&rgba[row_start..row_end]);
    }

    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: fb.texture(),
            mip_level: 0,
            origin: wgpu::Origin3d { x, y, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        &tight,
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
}
