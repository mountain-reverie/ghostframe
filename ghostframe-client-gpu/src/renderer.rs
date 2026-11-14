// SPDX-License-Identifier: MIT OR Apache-2.0
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
use crate::paint_order::SeqLedger;
use crate::pipelines::cdf53_grid::Cdf53Grid;
use crate::pipelines::cdf53_state::{
    Cdf53GridState, Cdf53Stats, FLUSH_BUDGET_US, MAX_TILES_PER_FLUSH,
};
use crate::pipelines::palrle::PalRlePipeline;
use crate::pipelines::rgba::RgbaPipeline;
use crate::pipelines::rgba_slab::{RgbaSlabPipeline, SlabWork};
use crate::pipelines::solid::SolidPipeline;
use crate::ring::{ExportRing, PublishedFrame};
use crate::upload_pool::{SlotRef, UploadPool};
use crate::wgpu_ctx::WgpuContext;
use crate::GpuError;

use ghostframe_client_core::cdf53_prevalidate::PrevalidatedCdf53;
use ghostframe_client_core::cdf53_tile_state::Cdf53TileState;
use ghostframe_client_core::{DecodeErrorCode, Event, TileData};
use ghostframe_protocol::protocol::Codec;
use ghostframe_protocol::tile::TILE_SIZE;

/// Whether `width x height` is a size this renderer will allocate for.
/// The same bound `ClientCore` enforces; repeated here because the renderer
/// is a public API and must not trust that every caller went through it.
fn dimensions_valid(width: u32, height: u32) -> bool {
    let range = 1..=ghostframe_client_core::MAX_FRAME_DIMENSION;
    range.contains(&width) && range.contains(&height)
}

/// The `frame_seq` a decoded picture belongs to: the pts it was fed with (a
/// picture may come out on a later call than the access unit that made it).
/// `None` when the decoder lost it. Such a picture is drawn, but cannot be
/// ordered: it neither raises the H.264 floor nor supersedes CDF 5/3 draws.
/// (Falling back to the last fed access unit would, because that can be newer
/// than the picture, raise the floor too far and drop tiles that are newer
/// than the picture but older than that unit.)
fn h264_picture_seq(frame: &ghostframe_client_h264::decoder::HwFrame) -> Option<u32> {
    frame.pts().and_then(|p| u32::try_from(p).ok())
}

fn tile_grid(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(TILE_SIZE), height.div_ceil(TILE_SIZE))
}

/// Where one buffered `TileReady` tile's pixels are.
enum RgbaSrc {
    /// Owned bytes, to be staged and copied at flush.
    Pixels(Vec<u8>),
    /// A slot of an upload slab the decode thread already wrote.
    Slot(SlotRef),
}

/// Upper bound on [`GpuDecodeError`]s held between two drains.
const MAX_PENDING_DECODE_ERRORS: usize = 1024;

/// The smallest rectangle of dirty `TileReady` tiles worth its own
/// `write_texture`; smaller ones go through [`RgbaPipeline`]. Measured on a
/// Pinebook Pro: a call costs ~178us plus ~12us per tile, the pipeline ~18us
/// per tile, so they cross near 30.
const MIN_TILES_PER_UPLOAD: u32 = 32;

/// Row stride of a buffer-to-texture copy: wgpu requires a multiple of this
/// whenever the copy has more than one row.
const COPY_ROW_ALIGN: u64 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as u64;

/// First size of [`UploadStaging`]'s buffer. A full 1920x1080 repaint is 8 MiB,
/// so a typical session grows it at most a couple of times.
const MIN_UPLOAD_STAGING: u64 = 1 << 20;

/// Where `TileReady` pixels wait between being written by the CPU and being
/// copied into the framebuffer by a command in the frame's encoder.
///
/// This replaces `queue.write_texture` for those uploads. A queue write runs
/// ahead of *every* command of the next submit, so a rectangle uploaded that
/// way would land before the frame's Solid and PalRle draws instead of after
/// them -- changing which wins where two entries of one `frame_seq` cover the
/// same tile (see [`crate::paint_order`]). A copy recorded in the encoder keeps
/// its place in the paint order. The buffer is written through
/// `write_buffer_with`, so the pixels still go straight into wgpu's staging
/// memory with no extra pass.
///
/// Bump-allocated per flush: every upload gets its own range, so no write can
/// overwrite another's before the GPU has run its copy.
#[derive(Default)]
struct UploadStaging {
    buffer: Option<wgpu::Buffer>,
    capacity: u64,
    cursor: u64,
}

impl UploadStaging {
    /// Start a flush: the previous flush's ranges are free again (its copies
    /// were submitted, and queue writes are ordered after them).
    fn begin(&mut self) {
        self.cursor = 0;
    }

    /// A range of `size` bytes, 256-aligned. Growing replaces the buffer; the
    /// copies already recorded keep the old one alive through the encoder.
    fn reserve(&mut self, device: &wgpu::Device, size: u64) -> (&wgpu::Buffer, u64) {
        let size = size.next_multiple_of(COPY_ROW_ALIGN);
        if self.buffer.is_none() || self.cursor + size > self.capacity {
            self.capacity = size.max(self.capacity * 2).max(MIN_UPLOAD_STAGING);
            self.cursor = 0;
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ghostframe-tile-upload-staging"),
                size: self.capacity,
                usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        let offset = self.cursor;
        self.cursor += size;
        (self.buffer.as_ref().expect("just ensured"), offset)
    }
}

/// One buffer-to-texture copy of a flush: the pixel rectangle `(x, y, w, h)`
/// it covers and where its rows come from.
struct Copy<'a> {
    px: (u32, u32, u32, u32),
    src: CopySrc<'a>,
}

enum CopySrc<'a> {
    /// A `TileReady` payload with fewer than 32 rows, or a tile on a narrow
    /// framebuffer edge.
    Short(&'a [u8]),
    /// A coalesced rectangle (in tile coordinates) of full tiles.
    Tiles(crate::coalesce::Rect),
}

/// The pixel rectangle a short `TileReady` payload covers, clipped to the
/// framebuffer; `None` for a payload with no full row or one wholly outside.
/// Performs no swizzle: `TileReady::rgba` is already RGBA.
fn short_tile_rect(
    fb: &Framebuffer,
    tile_x: u8,
    tile_y: u8,
    rgba: &[u8],
) -> Option<(u32, u32, u32, u32)> {
    let row_bytes = (TILE_SIZE * 4) as usize;
    let full_rows = (rgba.len() / row_bytes).min(TILE_SIZE as usize);
    let x = tile_x as u32 * TILE_SIZE;
    let y = tile_y as u32 * TILE_SIZE;
    let w = TILE_SIZE.min(fb.width.saturating_sub(x));
    let h = (full_rows as u32).min(fb.height.saturating_sub(y));
    (w > 0 && h > 0).then_some((x, y, w, h))
}

/// A tile the GPU decode rejected; the shape `ClientCore`'s
/// `DecodeErrorBatcher::report` takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuDecodeError {
    pub codec: Codec,
    pub tile_x: u8,
    pub tile_y: u8,
    pub code: DecodeErrorCode,
}

/// Map a shader error code to the wire enum. `None` for a code the shader
/// does not define.
fn decode_error_code(code: u32) -> Option<DecodeErrorCode> {
    Some(match code {
        1 => DecodeErrorCode::PayloadTooShort,
        2 => DecodeErrorCode::CountOutOfRange,
        3 => DecodeErrorCode::ThinUncachedPalette,
        4 => DecodeErrorCode::BundledTruncated,
        5 => DecodeErrorCode::IndexOob,
        6 => DecodeErrorCode::RleOvershoot,
        7 => DecodeErrorCode::RleUndershoot,
        8 => DecodeErrorCode::Cdf53BadPass,
        9 => DecodeErrorCode::Cdf53Truncated,
        10 => DecodeErrorCode::Cdf53RleLength,
        _ => return None,
    })
}

/// Which route the most recent H.264 frame took to reach the framebuffer.
/// Tracked (not just logged once ever) so a mid-session transition --
/// e.g. a resolution change that makes the decoder reallocate its pool
/// with a different pitch -- is still visible instead of the log
/// permanently claiming whichever path frame 0 happened to take. Task 12
/// reads that log line as ground truth for which path a run took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum H264ImportPath {
    ZeroCopy,
    CpuCopy,
}

/// The GPU CDF 5/3 decoder and the host state that schedules it.
struct Cdf53Gpu {
    grid: Cdf53Grid,
    state: Cdf53GridState,
    /// Grid frames recorded since the stats were last taken.
    frames: u64,
    cpu_fallback_logged: bool,
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
    rgba: RgbaPipeline,
    /// The slab-sourced twin of `rgba`; `Some` once a pool is attached.
    rgba_slab: Option<RgbaSlabPipeline>,
    upload_pool: Option<UploadPool>,
    /// GPU reconstruction of CDF 5/3 tiles; see [`Renderer::enable_cdf53_gpu`].
    /// `None` until enabled.
    cdf53_gpu: Option<Cdf53Gpu>,
    /// The CPU decoder CDF 5/3 passes fall back to on the render thread when
    /// the GPU one is not enabled or the frame is too large for its plane
    /// store. (The native client's decode thread normally decodes on the CPU
    /// before the renderer sees a pass, in which case neither is used.)
    cdf53_cpu: Cdf53TileState,
    /// Set while a flush runs only to put older work under an H.264 frame:
    /// the grid then keeps its pending tiles instead of spending its budget on
    /// pictures that are about to be covered.
    cdf53_hold: bool,

    /// Every pending stateless entry carries the `frame_seq` it came from
    /// (first field); `batch_seq` holds the newest per tile. See
    /// [`crate::paint_order`].
    pending_solid: Vec<(u32, u8, u8, [u8; 4])>,
    pending_palrle: Vec<(u32, u8, u8, u8, u8, Vec<u8>)>,
    /// `TileReady` tiles awaiting upload, batched by [`Renderer::flush`].
    pending_rgba: Vec<(u32, u8, u8, RgbaSrc)>,
    /// Not cleared by a flush: it orders paints across flushes too.
    batch_seq: SeqLedger,
    /// Staging for `TileReady` pixel uploads; see [`UploadStaging`].
    upload_staging: UploadStaging,
    /// Submits the most recent flush made for its own GPU work. See
    /// [`Renderer::last_flush_submits`].
    last_flush_submits: u32,
    /// GPU-detected decode failures not yet drained by
    /// [`Renderer::take_decode_errors`].
    decode_errors: Vec<GpuDecodeError>,
    /// Whether `flush` reads the PalRle error slots back. Off by default: the
    /// readback is a blocking `poll(wait_indefinitely)` per flush, and until
    /// there is a path from the render thread to `ClientCore`'s feedback
    /// batcher the result cannot reach the server, so paying for it would be
    /// pure cost. See [`Renderer::set_collect_decode_errors`].
    collect_decode_errors: bool,

    /// How many PPM dumps have been written, against
    /// `GHOSTFRAME_CLIENT_DUMP_COUNT`. Counts attempts, not successes -- see
    /// `maybe_dump_framebuffer`.
    dumps_written: u64,
    /// Dump sampling, read from the environment once at construction.
    dump: crate::config::DumpConfig,
    /// Completed `flush` calls, so the dump can be deferred past a server's
    /// opening burst.
    flushes: u64,

    /// `Some` from construction on when `Renderer::new`'s `open_h264_eagerly`
    /// was `true` (the decoder is opened synchronously in `Renderer::new` --
    /// see that function's doc). Otherwise lazily created on the first
    /// `NeedsH264` event in `decode_h264`: a client that was never going to
    /// receive H.264 never opens a decoder, and a machine without VA-API
    /// never can.
    h264_decoder: Option<ghostframe_client_h264::decoder::H264Decoder>,
    h264_pipeline: Option<crate::pipelines::h264_nv12::H264Nv12Pipeline>,
    /// `true` once opening the decoder has failed. Latches the failure so a
    /// machine that advertised H.264 without usable hardware pays the cost
    /// of `H264Decoder::new()` (open `/dev/dri/renderD128`, `vaInitialize`,
    /// dlopen the VA driver, tear it all down) exactly once, not on every
    /// arriving access unit -- at 60fps that retry-forever was the
    /// difference between "never can" and "never can, loudly, every frame".
    h264_unavailable: bool,
    /// Which path the most recently blitted H.264 frame took. `None` until
    /// the first frame; logged on every *change*, not just once ever -- see
    /// [`H264ImportPath`].
    h264_import_path: Option<H264ImportPath>,
    /// `frame_seq` of the last access unit handed to `decode_h264`.
    h264_last_frame_seq: Option<u32>,
    /// What the decoder did with each access unit, for the embedder to report
    /// to the client core (`ClientCore::h264_result`): the gate learns which
    /// frames became references, and the server learns which were decoded.
    /// Collected with [`Renderer::take_h264_results`].
    h264_results: Vec<(u32, ghostframe_client_core::H264Outcome)>,
    /// A decode failed and no access unit marked `reset_decoder` has arrived:
    /// the decoder's state is unknown, so access units queued behind the
    /// failure are not decoded (they would be concealed, and shown) and are
    /// reported `NotOutput`.
    h264_broken: bool,
    /// Whether the "decoder is far behind" warning has been logged this episode.
    h264_delay_warned: bool,
    /// Handed to the decoder whenever one is opened. See
    /// [`Renderer::set_h264_frame_ready_notify`].
    h264_ready_notify: Option<ghostframe_client_h264::FrameReadyNotify>,
    /// Access units fed and not yet answered; see [`crate::h264_pending`].
    h264_pending: crate::h264_pending::PendingFrames,
    /// Throttles the size-mismatch warning in [`Renderer::blit_h264_frame`]
    /// to its first occurrence per mismatch episode, so a persistent
    /// disagreement does not burst-log once per frame at 60fps. Cleared as
    /// soon as a frame's size agrees with the framebuffer again.
    h264_size_mismatch_logged: bool,
}

impl Drop for Renderer {
    /// Slots buffered but never flushed were issued by the decode thread and
    /// will never be drawn; without this their slabs would stay held for the
    /// life of the pool, which outlives a replaced renderer.
    fn drop(&mut self) {
        let Some(pool) = &self.upload_pool else {
            return;
        };
        let mut counts = vec![0u32; pool.slab_count()];
        for (_, _, _, src) in &self.pending_rgba {
            if let RgbaSrc::Slot(slot) = src {
                counts[slot.slab()] += 1;
            }
        }
        pool.retire_now(&counts);
    }
}

impl Renderer {
    /// `export_buffers` must be at least 1 -- a ring with zero buffers can
    /// never hand `publish` a free one, so the client would connect and
    /// then never show a frame. Rejected here rather than clamped, since a
    /// silent clamp would hide a misconfigured caller behind a working demo
    /// and a broken embed.
    /// `open_h264_eagerly` opens the H.264 decoder (device + codec) right
    /// here, synchronously, instead of lazily inside
    /// [`Renderer::apply_event`]'s `NeedsH264` handling -- moving
    /// `H264Decoder::new()`'s ~12.7 ms session-start device-open cost off
    /// the first H.264 access unit. Pass `Client::effective_h264` (the AND
    /// of the host's request and the local VA-API probe): a session that
    /// begins in H.264 mode (the server's default -- `io_bridge.rs`) then
    /// never pays this cost on its first frame, and a client that never
    /// advertised the capability never opens a decoder it will not use.
    ///
    /// A pre-built *hardware frames pool*, sized ahead of the stream's
    /// resolution, was tried and measured here too (design doc §10.7/§10.8)
    /// -- it bought nothing: the ~30 ms cold-decode cost lives in
    /// `vaCreateConfig`/`vaCreateContext` (`ff_vaapi_decode_init`), which
    /// runs unconditionally at first decode regardless of whether
    /// `hw_frames_ctx` was pre-supplied. Only the device/codec open moved
    /// the needle, which is all `open_h264_eagerly` does. The
    /// oversized/mismatched-resolution case remains ffmpeg's own default
    /// behaviour: `get_format` leaves `hw_frames_ctx` null and
    /// `ff_decode_get_hw_frames_ctx` builds a correctly-sized one on
    /// demand, unconditionally, whether the decoder was opened eagerly or
    /// lazily.
    pub fn new(
        ctx: &WgpuContext,
        width: u32,
        height: u32,
        export_buffers: usize,
        preferred_modifiers: &[u64],
        // Diagnostic-only: makes `debug_map_frame` possible at the cost of
        // pinning exports to CPU-visible memory. Production passes false.
        host_visible: bool,
        open_h264_eagerly: bool,
    ) -> Result<Self, GpuError> {
        if export_buffers == 0 {
            return Err(GpuError::NoExportBuffers);
        }
        if !dimensions_valid(width, height) {
            return Err(GpuError::InvalidDimensions { width, height });
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
        let rgba = RgbaPipeline::new(&ctx.device);

        // `open_h264_eagerly == false` behaves exactly as before this
        // task: lazy open on the first `NeedsH264` event, see `decode_h264`.
        let (h264_decoder, h264_unavailable) = if open_h264_eagerly {
            #[allow(
                clippy::disallowed_methods,
                reason = "measuring a real hardware VA-API device-open stall for M3 §10's decode-thread decision, not a virtual-clock path"
            )]
            let start = std::time::Instant::now();
            let opened = ghostframe_client_h264::decoder::H264Decoder::new();
            #[allow(
                clippy::disallowed_methods,
                reason = "measuring a real hardware VA-API device-open stall for M3 §10's decode-thread decision, not a virtual-clock path"
            )]
            let new_us = start.elapsed().as_micros() as u64;
            tracing::debug!(new_us, "Renderer::new: H264Decoder::new stall");
            match opened {
                Ok(d) => (Some(d), false),
                // Same reasoning as `decode_h264`'s lazy open: a genuine
                // device/codec-open failure (no VA-API at all) means every
                // later retry would fail identically, so latch it rather
                // than retry per access unit.
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "H.264 was probed available but eager decoder open failed -- the \
                         capability was advertised without hardware to back it"
                    );
                    (None, true)
                }
            }
        } else {
            (None, false)
        };

        Ok(Renderer {
            fb,
            ring,
            solid,
            palrle,
            rgba,
            rgba_slab: None,
            upload_pool: None,
            cdf53_gpu: None,
            cdf53_cpu: Cdf53TileState::new(),
            cdf53_hold: false,
            pending_solid: Vec::new(),
            pending_palrle: Vec::new(),
            pending_rgba: Vec::new(),
            batch_seq: SeqLedger::default(),
            upload_staging: UploadStaging::default(),
            last_flush_submits: 0,
            decode_errors: Vec::new(),
            collect_decode_errors: false,
            dumps_written: 0,
            dump: crate::config::DumpConfig::from_env(),
            flushes: 0,
            h264_decoder,
            h264_pipeline: None,
            h264_unavailable,
            h264_import_path: None,
            h264_last_frame_seq: None,
            h264_results: Vec::new(),
            h264_broken: false,
            h264_delay_warned: false,
            h264_ready_notify: None,
            h264_pending: Default::default(),
            h264_size_mismatch_logged: false,
        })
    }

    /// Route one `ClientCore` event to the right pipeline and mark the tile
    /// dirty. Batches within a frame; [`Renderer::flush`] submits.
    ///
    /// Every `Event` variant is matched explicitly -- see this crate's
    /// module doc on `clippy::wildcard_enum_match_arm` for why a `_ =>` arm
    /// is refused here.
    pub fn apply_event(&mut self, ctx: &WgpuContext, ev: Event) -> Result<(), GpuError> {
        match ev {
            Event::TileReady {
                tile_x,
                tile_y,
                rgba,
                frame_seq,
            } => {
                // Not the path this crate exists to test (that's
                // `TilePayload`, decoded by the real WGSL below), but a
                // valid event this renderer must still handle correctly:
                // the bytes are already final RGBA pixels.
                //
                // Buffered rather than uploaded here, and batched in `flush`.
                // One `write_texture` per tile costs ~178us on a Pinebook Pro
                // regardless of size -- almost entirely CPU-side call overhead
                // -- so a full 1920x1080 repaint spent 363ms just issuing 2040
                // of them. See `upload_pending_rgba`.
                self.batch_seq.note(tile_x, tile_y, frame_seq);
                self.note_stateless(tile_x, tile_y, frame_seq);
                self.pending_rgba
                    .push((frame_seq, tile_x, tile_y, RgbaSrc::Pixels(rgba)));
                self.ring.mark_dirty(tile_x as u32, tile_y as u32);
            }
            Event::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                generation,
                data,
            } => {
                match data {
                    TileData::Solid(bgra) => {
                        self.note_stateless(tile_x, tile_y, frame_seq);
                        self.batch_seq.note(tile_x, tile_y, frame_seq);
                        self.pending_solid.push((frame_seq, tile_x, tile_y, bgra));
                        self.ring.mark_dirty(tile_x as u32, tile_y as u32);
                    }
                    TileData::PalRle {
                        palette_id,
                        count,
                        indices,
                        // Diagnostics-only; the decoder derives what it needs
                        // from `count` and the indices themselves.
                        wire_flags: _,
                    } => {
                        self.note_stateless(tile_x, tile_y, frame_seq);
                        self.batch_seq.note(tile_x, tile_y, frame_seq);
                        self.pending_palrle
                            .push((frame_seq, tile_x, tile_y, palette_id, count, indices));
                        self.ring.mark_dirty(tile_x as u32, tile_y as u32);
                    }
                    TileData::Cdf53 {
                        pass_idx,
                        bit_planes,
                        present_passes,
                        new_generation,
                    } => {
                        self.apply_cdf53_pass(
                            tile_x,
                            tile_y,
                            frame_seq,
                            generation,
                            new_generation,
                            pass_idx,
                            present_passes,
                            bit_planes,
                        );
                    }
                    // The decode thread expands these (`TileDecoder`); one
                    // reaching the renderer bypassed it. Expand here rather
                    // than draw nothing.
                    TileData::Cdf53Wire {
                        pass_idx,
                        streams,
                        present_passes,
                        new_generation,
                    } => {
                        debug_assert!(false, "TileData::Cdf53Wire reached the renderer");
                        let mut planes = vec![0u8; 384];
                        let expanded = <&mut [u8; 384]>::try_from(planes.as_mut_slice())
                            .map(|p| {
                                ghostframe_client_core::cdf53_prevalidate::decode_cdf53_wire(
                                    &streams, p,
                                )
                                .is_ok()
                            })
                            .unwrap_or(false);
                        if expanded {
                            self.apply_cdf53_pass(
                                tile_x,
                                tile_y,
                                frame_seq,
                                generation,
                                new_generation,
                                pass_idx,
                                present_passes,
                                planes,
                            );
                        }
                    }
                }
            }
            Event::FrameDimensions { width, height } => {
                // Ignore a size the renderer would not have been built with:
                // keep the current framebuffer rather than allocate from a
                // network-supplied number.
                if !dimensions_valid(width, height) {
                    tracing::warn!(width, height, "ignoring out-of-range FrameDimensions");
                    return Ok(());
                }
                // Run any batched work against the OLD framebuffer size
                // before resizing -- the pending tiles were addressed
                // against it.
                self.flush(ctx)?;

                let epoch = self.fb.epoch();
                self.fb.resize(&ctx.device, &ctx.queue, width, height);
                if self.fb.epoch() != epoch {
                    // A new picture: what was painted on the old one says
                    // nothing about this one.
                    self.batch_seq.clear();
                    // The cached bind groups hold the old texture; drop them
                    // now rather than when each codec next draws, or the old
                    // framebuffer outlives the resize.
                    self.rgba.drop_framebuffer_cache();
                    if let Some(p) = self.rgba_slab.as_mut() {
                        p.drop_framebuffer_cache();
                    }
                    self.palrle.drop_framebuffer_cache();
                    if let Some(g) = self.cdf53_gpu.as_mut() {
                        g.grid.drop_framebuffer_cache();
                    }
                }
                if let Err(e) = self.ring.resize(ctx, width, height) {
                    tracing::error!(error = %e, width, height, "export ring resize failed");
                }
                self.solid
                    .set_canvas_size(&ctx.device, &ctx.queue, width, height);
                self.resize_cdf53(ctx, width, height);
                // The CPU accumulator keys its tiles by position, so a smaller
                // grid drops the tiles outside it; planes of a tile that stays
                // belong to the same picture.
                let (cols, rows) = tile_grid(width, height);
                self.cdf53_cpu
                    .retain_tiles(|x, y| (x as u32) < cols && (y as u32) < rows);
            }
            Event::NeedsH264 {
                frame_seq,
                payload,
                is_keyframe,
                reset_decoder,
                ..
            } => {
                self.decode_h264(ctx, frame_seq, is_keyframe, reset_decoder, &payload)?;
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
                log_decode_error(codec, tile_x, tile_y, code);
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
                self.palrle.upload_palette(&ctx.queue, palette_id, &full);
            }
            Event::Evicted { .. } => {
                // Terminal: the session is going away and the net thread
                // separately surfaces this to the host as
                // `ClientEvent::Disconnected`. Nothing left to paint.
            }
        }
        Ok(())
    }

    /// Turn on GPU reconstruction of CDF 5/3 tiles. Allocates the grid's plane
    /// store for the current frame size. The caller is expected to have run
    /// [`crate::pipelines::cdf53_grid::self_test`] on this device first: this
    /// does not.
    ///
    /// A frame whose plane store the device cannot bind (about 8K) is decoded
    /// on the CPU by this renderer instead, and the GPU is used again as soon
    /// as a frame fits.
    ///
    /// Builds its own grid with the cost model of the Mali-T860 it was fitted
    /// on (and the 1000-tile submit cap); the native client instead hands over
    /// the grid it probed, with [`Renderer::enable_cdf53_gpu_with`].
    pub fn enable_cdf53_gpu(&mut self, ctx: &WgpuContext) {
        if self.cdf53_gpu.is_none() {
            self.install_cdf53(ctx, Cdf53Grid::new(&ctx.device), None, MAX_TILES_PER_FLUSH);
        }
    }

    /// [`Renderer::enable_cdf53_gpu`] with a grid that already passed
    /// [`crate::pipelines::cdf53_grid::probe`] on this device, so the shaders
    /// are compiled once, and with the per-flush budget calibrated from what
    /// the probe measured. `max_tiles` caps one submit (the ~500 ms job
    /// watchdog of panfrost/GL is the reason; Vulkan can take far more).
    pub fn enable_cdf53_gpu_with(
        &mut self,
        ctx: &WgpuContext,
        probed: crate::pipelines::cdf53_grid::Probed,
        max_tiles: usize,
    ) {
        if self.cdf53_gpu.is_none() {
            self.install_cdf53(ctx, probed.grid, Some(probed.us_per_tile_k14), max_tiles);
        }
    }

    fn install_cdf53(
        &mut self,
        ctx: &WgpuContext,
        grid: Cdf53Grid,
        us_per_tile_k14: Option<f64>,
        max_tiles: usize,
    ) {
        let mut state = Cdf53GridState::with_budget(FLUSH_BUDGET_US, max_tiles);
        if let Some(us) = us_per_tile_k14 {
            state.set_cost_per_tile_k14(us);
        }
        self.cdf53_gpu = Some(Cdf53Gpu {
            grid,
            state,
            frames: 0,
            cpu_fallback_logged: false,
        });
        let (w, h) = (self.fb.width, self.fb.height);
        self.resize_cdf53(ctx, w, h);
    }

    /// Pretend the device's storage binding limit is `bytes`, from the next
    /// resize on. For tests of the too-large-frame fallback.
    #[doc(hidden)]
    pub fn debug_set_cdf53_limit(&mut self, bytes: Option<u64>) {
        if let Some(g) = self.cdf53_gpu.as_mut() {
            g.grid.set_limit_for_test(bytes);
        }
    }

    /// Size the CDF 5/3 grid and its host state for a `width` x `height` frame.
    fn resize_cdf53(&mut self, ctx: &WgpuContext, width: u32, height: u32) {
        let Some(g) = self.cdf53_gpu.as_mut() else {
            return;
        };
        let (cols, rows) = tile_grid(width, height);
        let was_gpu = g.grid.is_usable();
        let result = g.grid.resize(&ctx.device, cols, rows);
        if result.is_ok() != was_gpu {
            // Switching sides. Whatever the side we leave had accumulated --
            // planes, or a generation started there -- is of the old frame
            // size and must not be completed by the other side: the server's
            // full refresh after a resize re-sends every tile as a new
            // generation, so starting both sides clean loses nothing.
            self.cdf53_cpu.reset();
        }
        let g = self.cdf53_gpu.as_mut().expect("just used");
        match result {
            Ok(()) => {
                g.state.resize(cols, rows);
                g.cpu_fallback_logged = false;
            }
            Err(e) => {
                g.state.resize(0, 0);
                if !g.cpu_fallback_logged {
                    g.cpu_fallback_logged = true;
                    tracing::warn!(
                        error = %e, width, height,
                        "CDF 5/3 GPU decode unavailable for this frame size; \
                         decoding on the render thread's CPU"
                    );
                }
            }
        }
    }

    /// The GPU CDF 5/3 state, when enabled and the frame fits.
    fn cdf53_gpu_mut(&mut self) -> Option<&mut Cdf53Gpu> {
        self.cdf53_gpu.as_mut().filter(|g| g.grid.is_usable())
    }

    /// A Solid / PalRle / RGBA tile for the tile was applied: it may
    /// supersede a CDF 5/3 draw still waiting.
    fn note_stateless(&mut self, tile_x: u8, tile_y: u8, frame_seq: u32) {
        if let Some(g) = self.cdf53_gpu_mut() {
            g.state.stateless(tile_x, tile_y, frame_seq);
        }
    }

    /// One CDF 5/3 pass. With the GPU grid it is only recorded: nothing is
    /// drawn, noted in the paint-order ledger or marked dirty until
    /// [`Renderer::flush`] lists the tile. Without it the pass is
    /// reconstructed here, on the CPU, into a `TileReady`-style upload.
    #[allow(clippy::too_many_arguments)]
    fn apply_cdf53_pass(
        &mut self,
        tile_x: u8,
        tile_y: u8,
        frame_seq: u32,
        generation: u8,
        new_generation: bool,
        pass_idx: u8,
        present_passes: Option<u16>,
        bit_planes: Vec<u8>,
    ) {
        if self.cdf53_gpu_mut().is_some() {
            // Waiting passes count towards "something later is on its way"
            // for H.264 pictures, though not towards any tile's entry (see
            // `crate::paint_order`).
            self.batch_seq.note_pending(frame_seq);
        }
        if let Some(g) = self.cdf53_gpu_mut() {
            g.state.pass(
                tile_x,
                tile_y,
                frame_seq,
                generation,
                new_generation,
                pass_idx,
                present_passes,
                bit_planes,
            );
            return;
        }
        if new_generation {
            self.cdf53_cpu.forget_tile(tile_x, tile_y);
        }
        let entry = PrevalidatedCdf53 {
            generation,
            pass_idx,
            bit_planes,
            present_passes,
        };
        self.batch_seq.note(tile_x, tile_y, frame_seq);
        if let Some(rgba) = self.cdf53_cpu.integrate(tile_x, tile_y, &entry) {
            self.pending_rgba
                .push((frame_seq, tile_x, tile_y, RgbaSrc::Pixels(rgba)));
            self.ring.mark_dirty(tile_x as u32, tile_y as u32);
        }
    }

    /// Plan this flush's CDF 5/3 tiles and record the grid. See
    /// [`Cdf53GridState::plan`] for what gets listed.
    fn record_cdf53(&mut self, ctx: &WgpuContext, encoder: &mut wgpu::CommandEncoder) -> bool {
        if self.cdf53_hold {
            return false;
        }
        let Some(g) = self.cdf53_gpu.as_mut().filter(|g| g.grid.is_usable()) else {
            return false;
        };
        let listed = g.state.plan();
        if listed.is_empty() {
            return false;
        }
        g.grid.begin_frame();
        for t in &listed {
            for (pass, plane) in &t.planes {
                g.grid.push_plane(t.index, *pass, plane);
            }
            g.grid.push_tile(t.index, t.mask, t.k);
            self.batch_seq.note(t.tile_x, t.tile_y, t.frame_seq);
            self.ring.mark_dirty(t.tile_x as u32, t.tile_y as u32);
        }
        g.frames += 1;
        g.grid.record(&ctx.device, &ctx.queue, encoder, &self.fb)
    }

    /// Whether CDF 5/3 tiles are waiting for a later flush (the GPU budget was
    /// spent, or a partial tile is being throttled). The caller should flush
    /// again soon even if no event arrives.
    pub fn has_deferred_work(&self) -> bool {
        self.cdf53_gpu
            .as_ref()
            .is_some_and(|g| g.grid.is_usable() && g.state.has_pending())
    }

    /// CDF 5/3 scheduling counters since the last call, and the number of
    /// grid frames recorded; `None` when the GPU path is not enabled.
    pub fn take_cdf53_stats(&mut self) -> Option<(Cdf53Stats, u64)> {
        let g = self.cdf53_gpu.as_mut()?;
        let frames = std::mem::take(&mut g.frames);
        Some((g.state.take_stats(), frames))
    }

    /// Let this renderer draw tiles from `pool`'s slabs
    /// ([`Renderer::apply_slot_tile`]). Without it such tiles are dropped.
    ///
    /// The pool outlives any one renderer (it belongs to the GPU context); a
    /// renderer that is replaced retires the slots it still held when it drops.
    pub fn attach_upload_pool(&mut self, ctx: &WgpuContext, pool: &UploadPool) {
        self.rgba_slab = Some(RgbaSlabPipeline::new(&ctx.device, pool));
        self.upload_pool = Some(pool.clone());
    }

    /// The slab counterpart of an `Event::TileReady`: the tile's pixels are
    /// already in `slot`, so only its place in the paint order is recorded.
    /// Behaves exactly as `TileReady` does for ordering, superseding and
    /// duplicates (the later of two for one tile wins, whichever kind).
    ///
    /// Returns whether the renderer took the slot. When it did not (no pool
    /// attached) the slot is the caller's to hand back with
    /// [`UploadPool::retire_now`]; when it did, the renderer retires it.
    #[must_use]
    pub fn apply_slot_tile(
        &mut self,
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        slot: SlotRef,
    ) -> bool {
        let Some(pool) = &self.upload_pool else {
            return false;
        };
        debug_assert!(
            pool.is_current(slot),
            "slot {slot:?} arrived after its slab was recycled"
        );
        self.batch_seq.note(tile_x, tile_y, frame_seq);
        self.note_stateless(tile_x, tile_y, frame_seq);
        self.pending_rgba
            .push((frame_seq, tile_x, tile_y, RgbaSrc::Slot(slot)));
        self.ring.mark_dirty(tile_x as u32, tile_y as u32);
        true
    }

    /// Take the GPU-detected tile decode failures accumulated by [`flush`].
    ///
    /// The shader validates PalRle payloads on the device (an index past the
    /// palette count, say), so only the readback after the decode can tell.
    /// The caller is expected to relay these as DECODE_ERROR feedback.
    ///
    /// [`flush`]: Renderer::flush
    pub fn set_collect_decode_errors(&mut self, on: bool) {
        self.collect_decode_errors = on;
    }

    pub fn take_decode_errors(&mut self) -> Vec<GpuDecodeError> {
        std::mem::take(&mut self.decode_errors)
    }

    /// Submit all batched work, in one submit.
    ///
    /// Every codec records into one command encoder, in the paint order
    /// below, and the encoder is submitted once. Each submit costs a
    /// `glFlush` and a fence on GLES, and on panfrost a flush ends the job
    /// batch -- a separate kernel submission -- so one per codec per frame
    /// was most of this function's cost on a Pinebook Pro.
    ///
    /// **Why the order survives.** Buffer and texture contents reach the GPU
    /// two ways, and they do not interleave. `queue.write_buffer` /
    /// `write_buffer_with` / `write_texture` are queue operations that wgpu
    /// runs ahead of *all* the commands of the next submit; commands in an
    /// encoder run in the order they were recorded. So (1) every codec's
    /// inputs are in place before any codec's pass runs, which is harmless
    /// because the codecs use disjoint buffers, and (2) nothing that must be
    /// ordered against a pass may go through the queue: the `TileReady`
    /// rectangles are copied by commands (see [`UploadStaging`]) for exactly
    /// that reason. (3) A buffer written twice before one submit keeps only
    /// the second write, so each pipeline's `encode` is called once per
    /// encoder, and the staging ranges never overlap.
    ///
    /// Fails only if the opt-in PalRle error readback cannot wait on the
    /// device (e.g. it was lost). That runs after the submit, so the frame's
    /// work is already on the GPU; the errors it would have reported are lost.
    pub fn flush(&mut self, ctx: &WgpuContext) -> Result<(), GpuError> {
        self.flush_inner(ctx, false).map(|_| ())
    }

    /// [`Renderer::flush`] and [`Renderer::publish`] in one submit: the
    /// export blit is recorded after the codecs, into the same encoder.
    ///
    /// `Ok(None)` when every export buffer is held by the host (the flush
    /// still ran). On error nothing is published; the damage stays in the
    /// ring's history for the next publish.
    pub fn flush_and_publish(
        &mut self,
        ctx: &WgpuContext,
    ) -> Result<Option<PublishedFrame>, GpuError> {
        self.flush_inner(ctx, true)
    }

    /// Submits the most recent flush made for its own GPU work: 1 when it had
    /// anything to draw (with or without a publish), 0 when it had nothing.
    /// Not counted: the opt-in PalRle error readback and framebuffer dump,
    /// which are submits of their own by design. For tests and diagnostics.
    pub fn last_flush_submits(&self) -> u32 {
        self.last_flush_submits
    }

    fn flush_inner(
        &mut self,
        ctx: &WgpuContext,
        publish: bool,
    ) -> Result<Option<PublishedFrame>, GpuError> {
        let submits_before = crate::submit::count();
        if self.upload_pool.is_some() {
            // Fires the completion callbacks of earlier submits, so slots they
            // read are retired even when nothing else polls the device (a
            // flush that did not publish waits for nothing).
            let _ = ctx.device.poll(wgpu::PollType::Poll);
        }
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ghostframe-flush"),
            });
        let mut recorded = false;

        // Order within a batch is by `frame_seq`, not by codec (see
        // `crate::paint_order`). The CDF 5/3 grid goes first so that a newer
        // stateless entry lands on top of an older pass; a stateless entry
        // older than anything else for its tile is dropped, so a newer pass
        // is not covered either. Only tiles the grid is *drawing* this flush
        // count towards a tile's newest `frame_seq`: a pass that is held back
        // must not make an older Solid disappear.
        recorded |= self.record_cdf53(ctx, &mut encoder);

        let ledger = &self.batch_seq;
        let solid: Vec<(u8, u8, [u8; 4])> = std::mem::take(&mut self.pending_solid)
            .into_iter()
            .filter(|&(seq, tx, ty, _)| !ledger.superseded(tx, ty, seq))
            .map(|(_, tx, ty, c)| (tx, ty, c))
            .collect();
        let palrle_batch: Vec<(u8, u8, u8, u8, Vec<u8>)> = std::mem::take(&mut self.pending_palrle)
            .into_iter()
            .filter(|&(seq, tx, ty, ..)| !ledger.superseded(tx, ty, seq))
            .map(|(_, tx, ty, id, n, idx)| (tx, ty, id, n, idx))
            .collect();
        // Slots of tiles dropped here never reach the GPU; they are still
        // issued, so they are retired with the rest of the flush's.
        let mut retired = vec![0u32; self.upload_pool.as_ref().map_or(0, |p| p.slab_count())];
        self.pending_rgba.retain(|(seq, tx, ty, src)| {
            let keep = !ledger.superseded(*tx, *ty, *seq);
            if let (false, RgbaSrc::Slot(slot)) = (keep, src) {
                retired[slot.slab()] += 1;
            }
            keep
        });

        recorded |= self
            .solid
            .encode(&ctx.device, &ctx.queue, &mut encoder, &self.fb, &solid);
        recorded |= self.palrle.encode(
            &ctx.device,
            &ctx.queue,
            &mut encoder,
            &self.fb,
            &palrle_batch,
        );
        recorded |= self.encode_pending_rgba(ctx, &mut encoder, &mut retired);

        let plan = if publish {
            self.ring.plan_publish(&self.fb)
        } else {
            None
        };
        if let Some(plan) = &plan {
            recorded |=
                self.ring
                    .encode_blit(&ctx.device, &ctx.queue, &mut encoder, &self.fb, plan);
        }

        if recorded {
            if plan.is_some() {
                self.ring.arm_fence(ctx);
            }
            crate::submit::submit(&ctx.queue, encoder);
        }
        if let Some(pool) = &self.upload_pool {
            // After the submit that may read them, or at once if there was none.
            // Not tied to the publish below, which may not happen.
            if recorded {
                pool.retire_when_submitted_work_done(&ctx.queue, retired);
            } else {
                pool.retire_now(&retired);
            }
        }
        let submits = crate::submit::count().wrapping_sub(submits_before) as u32;
        self.last_flush_submits = submits;
        tracing::debug!(submits, publish, "flush: submitted");

        if !palrle_batch.is_empty() && self.collect_decode_errors {
            let errors = self.palrle.take_errors(&ctx.device, &ctx.queue)?;
            for (i, code) in errors {
                let Some((tile_x, tile_y, ..)) = palrle_batch.get(i as usize) else {
                    continue;
                };
                let Some(code) = decode_error_code(code) else {
                    tracing::warn!(code, tile_x, tile_y, "unknown GPU palrle error code");
                    continue;
                };
                // Bounded: a hostile stream of bad tiles must not grow this
                // without limit between two drains.
                if self.decode_errors.len() < MAX_PENDING_DECODE_ERRORS {
                    self.decode_errors.push(GpuDecodeError {
                        codec: Codec::PalRle,
                        tile_x: *tile_x,
                        tile_y: *tile_y,
                        code,
                    });
                }
            }
        }

        self.flushes = self.flushes.saturating_add(1);
        self.maybe_dump_framebuffer(ctx);
        Ok(plan.map(|plan| self.ring.finish_publish(ctx, plan, recorded)))
    }

    /// Record the upload of every buffered `TileReady` tile with as few
    /// commands as possible: one buffer-to-texture copy per large rectangle of
    /// dirty tiles, and every other tile in one [`RgbaPipeline`] dispatch.
    /// Returns whether anything was recorded.
    ///
    /// **Why this exists.** `write_texture` costs about the same whatever it
    /// carries: measured on a Pinebook Pro (Mali-T860), 2040 calls of 32x32
    /// took 363ms while a single 1920x1080 call carrying more bytes took
    /// 18.6ms -- and `issue` time equalled total time in both, so it is CPU-side
    /// per-call overhead, not pixel throughput. A full-screen repaint was
    /// therefore ~3fps, bounded entirely by call count.
    ///
    /// So the dirty tiles are merged into rectangles with the same
    /// [`crate::coalesce`] pass the export ring already uses for damage. A
    /// rectangle of at least [`MIN_TILES_PER_UPLOAD`] tiles is assembled into
    /// one contiguous buffer and uploaded with one call; a full repaint is a
    /// single call. Smaller rectangles -- scattered tiles, the usual shape of
    /// text and UI changes -- would still cost one call each, so they all go to
    /// the GPU in one buffer and one dispatch instead (see
    /// `upload_bench.rs::rgba_pipeline_breakdown` for the crossover).
    ///
    /// The rectangles go through [`UploadStaging`] and a recorded copy rather
    /// than `write_texture`, so that they stay in the frame's paint order
    /// within its one submit; the cost model above is unchanged, since
    /// `write_texture` stages through a buffer the same way.
    ///
    /// Tiles whose payload is shorter than a full tile take neither route and
    /// are uploaded individually. Both routes assume every tile supplies all
    /// its rows; a short one would leave the remainder as buffer fill and
    /// write black over live pixels, which is a silent corruption rather than
    /// a dropped update.
    fn encode_pending_rgba(
        &mut self,
        ctx: &WgpuContext,
        encoder: &mut wgpu::CommandEncoder,
        retired: &mut [u32],
    ) -> bool {
        if self.pending_rgba.is_empty() {
            return false;
        }
        let full_tile_bytes = (TILE_SIZE * TILE_SIZE * 4) as usize;
        let pending = std::mem::take(&mut self.pending_rgba);

        // Every slot given to this flush is retired after it, whichever of the
        // paths below (or none) ends up drawing it.
        for (_, _, _, src) in &pending {
            if let (RgbaSrc::Slot(slot), Some(pool)) = (src, &self.upload_pool) {
                debug_assert!(
                    pool.is_current(*slot),
                    "slot {slot:?} was recycled before it was drawn"
                );
                retired[slot.slab()] += 1;
            }
        }

        // Short payloads keep the one-copy-each path; see the doc above.
        let short: Vec<(u8, u8, &[u8])> = pending
            .iter()
            .filter_map(|(_, tx, ty, src)| match src {
                RgbaSrc::Pixels(rgba) if rgba.len() < full_tile_bytes => {
                    Some((*tx, *ty, rgba.as_slice()))
                }
                RgbaSrc::Pixels(_) | RgbaSrc::Slot(_) => None,
            })
            .collect();

        // Last write wins for a tile that arrived twice in one batch, matching
        // the per-tile path's ordering -- across both kinds: a slab tile that
        // follows a `Vec` tile for the same place replaces it and vice versa,
        // so the two routes never both draw one tile and their relative
        // order cannot matter.
        let (cols, rows) = tile_grid(self.fb.width, self.fb.height);
        let mut by_tile: std::collections::HashMap<(u32, u32), &[u8]> =
            std::collections::HashMap::with_capacity(pending.len());
        let mut by_slot: std::collections::HashMap<(u32, u32), SlotRef> =
            std::collections::HashMap::new();
        for (_, tx, ty, src) in &pending {
            let key = (*tx as u32, *ty as u32);
            if key.0 >= cols || key.1 >= rows {
                continue;
            }
            match src {
                RgbaSrc::Pixels(rgba) if rgba.len() >= full_tile_bytes => {
                    by_slot.remove(&key);
                    by_tile.insert(key, rgba.as_slice());
                }
                RgbaSrc::Slot(slot) => {
                    by_tile.remove(&key);
                    by_slot.insert(key, *slot);
                }
                RgbaSrc::Pixels(_) => {}
            }
        }
        let mut grid = crate::dirty::DirtyGrid::new(cols, rows);
        for &(tx, ty) in by_tile.keys() {
            grid.set(tx, ty);
        }

        // Every copy of the flush, in paint order (short tiles first, as
        // before), each with the pixel rectangle it covers.
        let mut copies: Vec<Copy> = Vec::new();
        for &(tx, ty, rgba) in &short {
            if let Some(px) = short_tile_rect(&self.fb, tx, ty, rgba) {
                copies.push(Copy {
                    px,
                    src: CopySrc::Short(rgba),
                });
            }
        }
        let mut scattered: Vec<(u32, u32, &[u8])> = Vec::new();
        for rect in crate::coalesce::coalesce(&grid) {
            if rect.w * rect.h < MIN_TILES_PER_UPLOAD {
                for ty in rect.y..rect.y + rect.h {
                    for tx in rect.x..rect.x + rect.w {
                        if let Some(src) = by_tile.get(&(tx, ty)) {
                            scattered.push((tx, ty, src));
                        }
                    }
                }
                continue;
            }
            let px = rect.to_pixels(self.fb.width, self.fb.height);
            if px.w == 0 || px.h == 0 {
                continue;
            }
            copies.push(Copy {
                px: (px.x, px.y, px.w, px.h),
                src: CopySrc::Tiles(rect),
            });
        }

        let mut recorded = self.encode_staged_copies(ctx, encoder, &copies, &by_tile);
        recorded |= self
            .rgba
            .encode(&ctx.device, &ctx.queue, encoder, &self.fb, &scattered);

        // The slab tiles: one dispatch per slab used.
        if let (Some(pipeline), Some(pool)) = (self.rgba_slab.as_mut(), &self.upload_pool) {
            let mut batches: Vec<(usize, Vec<SlabWork>)> = Vec::new();
            for (&(tile_x, tile_y), slot) in &by_slot {
                let work = SlabWork {
                    tile_x,
                    tile_y,
                    slot: slot.index(),
                };
                match batches.iter_mut().find(|(slab, _)| *slab == slot.slab()) {
                    Some((_, tiles)) => tiles.push(work),
                    None => batches.push((slot.slab(), vec![work])),
                }
            }
            recorded |= pipeline.encode(&ctx.device, &ctx.queue, encoder, &self.fb, pool, &batches);
        }
        recorded
    }

    /// Record every copy in `copies` into the framebuffer, all staged through
    /// one range of [`UploadStaging`] and so one `write_buffer_with`: each
    /// queue write is a staging buffer and, on panfrost, several flushes.
    /// Each copy has its own sub-range (a multiple of the row alignment), so
    /// none overwrites another. Returns whether anything was recorded.
    fn encode_staged_copies(
        &mut self,
        ctx: &WgpuContext,
        encoder: &mut wgpu::CommandEncoder,
        copies: &[Copy],
        by_tile: &std::collections::HashMap<(u32, u32), &[u8]>,
    ) -> bool {
        if copies.is_empty() {
            return false;
        }
        self.upload_staging.begin();
        // (row stride, offset within the range) per copy.
        let mut layout = Vec::with_capacity(copies.len());
        let mut total = 0u64;
        for c in copies {
            let stride = (c.px.2 as u64 * 4).next_multiple_of(COPY_ROW_ALIGN);
            layout.push((stride, total));
            total += stride * c.px.3 as u64;
        }
        let (buffer, base) = self.upload_staging.reserve(&ctx.device, total);
        let Some(mut view) = ctx.queue.write_buffer_with(
            buffer,
            base,
            wgpu::BufferSize::new(total).expect("non-empty copies"),
        ) else {
            // Only on a validation failure (the range is within the buffer by
            // construction); wgpu has already reported it.
            return false;
        };
        let row_bytes = (TILE_SIZE * 4) as usize;
        for (c, &(stride, at)) in copies.iter().zip(&layout) {
            let (stride, at) = (stride as usize, at as usize);
            let (_, _, w, h) = c.px;
            match &c.src {
                CopySrc::Short(rgba) => {
                    for row in 0..h as usize {
                        let d = at + row * stride;
                        view.slice(d..d + w as usize * 4).copy_from_slice(
                            &rgba[row * row_bytes..row * row_bytes + w as usize * 4],
                        );
                    }
                }
                CopySrc::Tiles(rect) => {
                    for ty in rect.y..rect.y + rect.h {
                        for tx in rect.x..rect.x + rect.w {
                            // `coalesce` emits rectangles covering exactly the
                            // set tiles, so every tile here was inserted.
                            let Some(src) = by_tile.get(&(tx, ty)) else {
                                continue;
                            };
                            let dx = (tx - rect.x) as usize * TILE_SIZE as usize;
                            let dy = (ty - rect.y) as usize * TILE_SIZE as usize;
                            // Clip against the rectangle, which is itself
                            // already clamped to the framebuffer edge.
                            let tw = (TILE_SIZE as usize).min(w as usize - dx);
                            let th = (TILE_SIZE as usize).min(h as usize - dy);
                            for row in 0..th {
                                let d = at + (dy + row) * stride + dx * 4;
                                view.slice(d..d + tw * 4).copy_from_slice(
                                    &src[row * row_bytes..row * row_bytes + tw * 4],
                                );
                            }
                        }
                    }
                }
            }
        }
        drop(view);
        for (c, &(stride, at)) in copies.iter().zip(&layout) {
            let (x, y, w, h) = c.px;
            encoder.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo {
                    buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: base + at,
                        bytes_per_row: Some(stride as u32),
                        rows_per_image: Some(h),
                    },
                },
                wgpu::TexelCopyTextureInfo {
                    texture: self.fb.texture(),
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        }
        true
    }

    /// Seal a generation and hand out an export buffer, if one is free.
    pub fn publish(&mut self, ctx: &WgpuContext) -> Option<PublishedFrame> {
        self.ring.publish(ctx, &self.fb)
    }

    /// Write the framebuffer to binary PPM files when
    /// `GHOSTFRAME_CLIENT_DUMP_FRAME` names a path prefix.
    ///
    /// Writes `<prefix>.<n>.ppm` on each of the first
    /// `GHOSTFRAME_CLIENT_DUMP_COUNT` flushes (default 1), logging dimensions
    /// and a non-black pixel count for each.
    ///
    /// **A series, not one frame, and that is deliberate.** A server's opening
    /// burst is thousands of tiles, and the render thread batches aggressively
    /// -- 12k events collapsed into a handful of flushes on the machine this
    /// was written for. So a single dump lands at an arbitrary point in the
    /// paint, and an almost-empty framebuffer is indistinguishable from one
    /// that never fills. A short series shows whether content is accumulating.
    ///
    /// **Reads the framebuffer, deliberately -- not the exported dmabuf.** The
    /// framebuffer is an ordinary wgpu texture, so `debug_read` returns tight
    /// row-major RGBA whatever the export's DRM modifier is. Where the export
    /// is tiled or compressed (see `export_gles.rs`) that distinction separates
    /// two bugs that look identical from outside:
    ///
    /// - dumps fill in -> decode and render are fine; the export layout or the
    ///   consumer's import is at fault.
    /// - dumps stay black -> the fault is upstream, in tile decode or the
    ///   shaders, and the export is a red herring.
    ///
    /// PPM because it needs no dependency and every viewer opens it;
    /// `ghostframe-server` pulls in a PNG encoder for its own capture dump,
    /// but that is a server-side cost and this is a library.
    fn maybe_dump_framebuffer(&mut self, ctx: &WgpuContext) {
        let Some(prefix) = self.dump.frame_prefix.clone() else {
            return;
        };
        // `GHOSTFRAME_CLIENT_DUMP_EVERY=n` samples every nth flush instead,
        // overwriting `<prefix>.latest.ppm`, so a long session can be compared
        // against the export dump at the same moment rather than only during
        // the opening paint.
        let n = self.dumps_written;
        if !self.dump.should_dump(n) {
            self.dumps_written += 1;
            return;
        }
        // Incremented regardless of outcome: a path that cannot be written will
        // not start working on the next flush, and retrying every flush would
        // turn a typo into a log flood.
        self.dumps_written += 1;

        let (w, h) = (self.fb.width, self.fb.height);
        let rgba = self.fb.debug_read(&ctx.device, &ctx.queue);

        // "I dumped a frame" is not the useful fact; "the frame was entirely
        // black" is, and it saves opening the file to find out.
        let non_black = rgba
            .chunks_exact(4)
            .filter(|p| p[0] != 0 || p[1] != 0 || p[2] != 0)
            .count();

        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        ppm.reserve(w as usize * h as usize * 3);
        for px in rgba.chunks_exact(4) {
            ppm.extend_from_slice(&px[..3]);
        }

        let path = if self.dump.every > 0 {
            format!("{prefix}.latest.ppm")
        } else {
            format!("{prefix}.{n}.ppm")
        };
        match std::fs::write(&path, &ppm) {
            Ok(()) => tracing::info!(
                path = %path,
                width = w,
                height = h,
                non_black_px = non_black,
                total_px = w as usize * h as usize,
                flush = self.flushes,
                "dumped framebuffer (PPM via wgpu readback -- unaffected by the \
                 exported dmabuf's modifier)"
            ),
            Err(e) => tracing::warn!(path = %path, error = %e, "framebuffer dump failed"),
        }
    }

    pub fn release(&mut self, frame_id: u32) {
        self.ring.release(frame_id);
    }

    /// Mark the whole surface dirty, so the next `publish` carries all of it.
    pub fn mark_all_dirty(&mut self) {
        self.ring.mark_dirty_all();
    }

    /// Take back a published frame the host never acquired.
    ///
    /// Releases its buffer, and marks the whole surface dirty. Releasing alone
    /// under-reports damage: the next `publish` may reuse a different buffer
    /// whose watermark predates the withdrawn frame, so it would report only
    /// the changes since THAT buffer was filled, while the host is still
    /// showing an even older frame. Re-damaging everything is the one answer
    /// that is right whichever buffer comes next; it costs a full blit, and
    /// only on frames the host was too slow to take.
    pub fn withdraw_frame(&mut self, frame_id: u32) {
        self.ring.release(frame_id);
        self.ring.mark_dirty_all();
    }

    /// The exported dmabuf image for a buffer id `publish` handed out.
    pub fn export_buffer(&self, buffer_id: u32) -> &ExportedImage {
        self.ring.buffer(buffer_id)
    }

    pub fn debug_read_framebuffer(&self, ctx: &WgpuContext) -> Vec<u8> {
        self.fb.debug_read(&ctx.device, &ctx.queue)
    }

    /// Decode one access unit and blit every frame the decoder has ready.
    /// Frames that finish later are collected by [`Renderer::poll_h264`].
    ///
    /// Failures are logged, never fatal: a corrupt access unit must not take
    /// down the session. Whether to decode at all is the client core's gate;
    /// `reset_decoder` is its instruction to reset first (an IDR, or after
    /// the gate was reset): ffmpeg's H.264 decoder does not error on missing
    /// references, it conceals, so an IDR alone would not clear stale state.
    ///
    /// Outcomes are queued for [`Renderer::take_h264_results`]: a frame is
    /// decoded when a picture carrying its pts comes out, on this call or a
    /// later one; units older than an output picture are refused.
    fn decode_h264(
        &mut self,
        ctx: &WgpuContext,
        frame_seq: u32,
        is_keyframe: bool,
        reset_decoder: bool,
        au: &[u8],
    ) -> Result<(), GpuError> {
        use ghostframe_client_core::H264Outcome;
        if self.h264_unavailable {
            self.h264_results.push((frame_seq, H264Outcome::NoDecoder));
            return Ok(());
        }
        if self.h264_decoder.is_none() {
            // Timed separately from `decoder.decode()` below: this is the
            // one-time session-startup cost (`av_hwdevice_ctx_create`, open
            // `/dev/dri/renderD128`, `vaInitialize`, dlopen the VA driver --
            // see `h264_unavailable`'s doc), paid once per session, not
            // once per frame. M3 §10's first review round found it excluded
            // entirely, which understated the real first-frame stall by
            // roughly half.
            #[allow(
                clippy::disallowed_methods,
                reason = "measuring a real hardware VA-API device-open stall for M3 §10's decode-thread decision, not a virtual-clock path"
            )]
            let new_start = std::time::Instant::now();
            let opened = ghostframe_client_h264::decoder::H264Decoder::new();
            #[allow(
                clippy::disallowed_methods,
                reason = "measuring a real hardware VA-API device-open stall for M3 §10's decode-thread decision, not a virtual-clock path"
            )]
            let new_us = new_start.elapsed().as_micros() as u64;
            tracing::debug!(new_us, "decode_h264: H264Decoder::new stall");
            match opened {
                Ok(mut d) => {
                    if let Some(notify) = &self.h264_ready_notify {
                        d.set_frame_ready_notify(notify.clone());
                    }
                    self.h264_decoder = Some(d);
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "H.264 arrived but no decoder could be opened -- the capability \
                         was advertised without hardware to back it"
                    );
                    self.h264_unavailable = true;
                    self.h264_results.push((frame_seq, H264Outcome::NoDecoder));
                    return Ok(());
                }
            }
        }

        // Flush batched tile work for the OLD framebuffer state before this
        // new full-frame H.264 draw, same reasoning as the
        // `Event::FrameDimensions` arm above: `apply_event` only queues
        // solid/palrle/cdf53 work and batches it until `flush`, but
        // `blit_h264_frame` below draws immediately. Without this, a burst
        // containing tile payloads for frame N followed by an H.264 access
        // unit for frame N+1 -- exactly what a mode-switch transition
        // produces -- would draw the H.264 frame first and then paint the
        // older, now-stale tiles over it when the batch's `flush` finally
        // runs.
        self.flush_under_h264(ctx)?;

        if self.h264_pipeline.is_none() {
            self.h264_pipeline = Some(crate::pipelines::h264_nv12::H264Nv12Pipeline::new(
                &ctx.device,
            ));
        }

        if self.h264_broken && !reset_decoder {
            self.h264_results.push((frame_seq, H264Outcome::NotOutput));
            return Ok(());
        }
        let decoder = self.h264_decoder.as_mut().expect("just created");

        self.h264_last_frame_seq = Some(frame_seq);
        if reset_decoder {
            // Whatever the decoder still held is flushed with it: those
            // access units will never produce a picture.
            self.h264_results.extend(self.h264_pending.refuse_all());
            decoder.reset();
            self.h264_broken = false;
        }

        // Timed at `debug` level so M3's decode-cost measurement (design doc
        // §10) can isolate this span from everything else `decode_h264`
        // does. Two unconditional `Instant::now()` calls (~40ns/frame,
        // matching `ring.rs`'s `publish` timing precedent exactly) bracket
        // it; only the `tracing::debug!` line below is level-gated, off by
        // default, on with `RUST_LOG=ghostframe_client_gpu::renderer=debug`.
        //
        // What this span is NOT: "submit to surface available". ffmpeg's
        // VA-API hwaccel does not synchronise the decoded surface inside
        // `avcodec_receive_frame` -- the `vaSyncSurface` wait for the
        // hardware to actually finish decoding lives in
        // `frame.map_dmabuf()` (its own explicit sync) in `blit_h264_frame`
        // below, which is timed separately. This span is submit-through-
        // `avcodec_receive_frame`-return: CPU-side packet handling plus
        // whatever of the decode VA-API's async submission model makes
        // synchronous, not the hardware decode itself.
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real hardware H.264 decode stall for M3 §10's decode-thread decision, not a virtual-clock path"
        )]
        let decode_start = std::time::Instant::now();
        let frames = match decoder.decode_pts(au, i64::from(frame_seq)) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(frame_seq, is_keyframe, error = %e, "H.264 decode failed");
                // A hard decode error leaves the decoder's internal state for
                // this access unit unknown. The gate resets, asks the server
                // for an IDR (paced) and resets the decoder at it.
                self.h264_broken = true;
                // Older units still inside the decoder are lost with its
                // state; refuse them first, then report the failure itself
                // (the core refuses everything until the reset frame).
                self.h264_results.extend(self.h264_pending.refuse_all());
                self.h264_results
                    .push((frame_seq, H264Outcome::DecodeError));
                return Ok(());
            }
        };
        // Answer by picture, not by call: a decoder with output delay (the
        // GStreamer backend returns a picture on the call after its access
        // unit) still decoded every frame, just later.
        self.h264_results.extend(self.h264_pending.fed(frame_seq));
        for f in &frames {
            let answers = self.h264_pending.output(f.pts());
            self.h264_results.extend(answers);
        }
        let pending = self.h264_pending.len();
        if pending > crate::h264_pending::WARN_PENDING {
            if !self.h264_delay_warned {
                self.h264_delay_warned = true;
                tracing::warn!(
                    frame_seq,
                    pending,
                    "H.264 decoder is more than a few access units behind: \
                     frames are acknowledged late, and refused once the backlog \
                     reaches its bound"
                );
            }
        } else if pending <= 1 {
            self.h264_delay_warned = false;
        }
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

        self.blit_h264_frames(ctx, frame_seq, frames);
        Ok(())
    }

    /// The flush that precedes an H.264 blit: Solid / PalRle / RGBA work lands
    /// under the frame, as it always did, but CDF 5/3 tiles are held back. The
    /// frame covers the screen, so drawing them first would only spend a
    /// flush's GPU budget; and if the frame is then blitted they are
    /// cancelled ([`Renderer::supersede_cdf53`]), where drawing them later
    /// would put them on top of it.
    fn flush_under_h264(&mut self, ctx: &WgpuContext) -> Result<(), GpuError> {
        self.cdf53_hold = true;
        let r = self.flush(ctx);
        self.cdf53_hold = false;
        r
    }

    /// An H.264 frame from `frame_seq` was blitted over the whole framebuffer:
    /// cancel CDF 5/3 draws still waiting, and refuse older passes.
    pub fn supersede_cdf53(&mut self, frame_seq: u32) {
        if let Some(g) = self.cdf53_gpu.as_mut() {
            g.state.supersede_all(frame_seq);
        }
    }

    /// An H.264 picture from `seq` is now over the whole framebuffer: record
    /// it in the paint-order ledger and cancel the CDF 5/3 draws it is newer
    /// than.
    fn h264_picture_drawn(&mut self, seq: u32) {
        self.batch_seq.note_h264(seq);
        self.supersede_cdf53(seq);
    }

    /// The ordering decision of the H.264 blit without a picture, for tests:
    /// `false` when a picture from `seq` is older than what is on screen (and
    /// would not be drawn), otherwise records it as drawn, as the blit does.
    #[doc(hidden)]
    pub fn debug_accept_h264_picture(&mut self, seq: u32) -> bool {
        if self.batch_seq.h264_stale(seq) {
            return false;
        }
        self.h264_picture_drawn(seq);
        true
    }

    /// Blit `frames` and mark the ring dirty if any landed.
    fn blit_h264_frames(
        &mut self,
        ctx: &WgpuContext,
        frame_seq: u32,
        frames: Vec<ghostframe_client_h264::decoder::HwFrame>,
    ) -> bool {
        let mut any_blitted = false;
        for frame in frames {
            let seq = h264_picture_seq(&frame);
            if seq.is_some_and(|s| self.batch_seq.h264_stale(s)) {
                // Older than a paint already on screen (a tile of a later
                // frame, or a newer picture): it covers every tile, so
                // drawing it would undo that paint. The access unit was
                // decoded and answered; only its picture is dropped.
                tracing::debug!(
                    ?seq,
                    "H.264 picture older than what is on screen: not drawn"
                );
                continue;
            }
            if self.blit_h264_frame(ctx, seq.unwrap_or(frame_seq), &frame) {
                any_blitted = true;
                if let Some(s) = seq {
                    self.h264_picture_drawn(s);
                }
            }
        }
        // Once after the loop, not once per frame inside it: an access unit
        // that completes more than one frame (reordering drain) would
        // otherwise mark the ring dirty redundantly, and an access unit
        // that completed none (every frame dropped on a size mismatch)
        // would otherwise mark it dirty for nothing.
        if any_blitted {
            self.ring.mark_dirty_all();
        }
        any_blitted
    }

    /// Install the callback the H.264 decoder runs when a frame finishes
    /// decoding after [`Renderer::apply_event`] returned -- see
    /// [`ghostframe_client_h264::FrameReadyNotify`]. The owner answers it by
    /// calling [`Renderer::poll_h264`]. Applies to the current decoder and to
    /// any opened later.
    pub fn set_h264_frame_ready_notify(
        &mut self,
        notify: ghostframe_client_h264::FrameReadyNotify,
    ) {
        if let Some(d) = self.h264_decoder.as_mut() {
            d.set_frame_ready_notify(notify.clone());
        }
        self.h264_ready_notify = Some(notify);
    }

    /// Blit every H.264 frame the decoder finished since the last access unit.
    /// Returns whether anything was drawn, i.e. whether a publish is due.
    ///
    /// Without this a frame waits for the *next* access unit to be collected,
    /// and on a remote desktop the next one comes when the screen next
    /// changes: a keystroke's echo stayed invisible until the one after it.
    pub fn poll_h264(&mut self, ctx: &WgpuContext) -> Result<bool, GpuError> {
        let Some(decoder) = self.h264_decoder.as_mut() else {
            return Ok(false);
        };
        let frames = match decoder.take_ready() {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "H.264 decode failed (late frame)");
                // The decoder's state is unknown; same as a hard error in
                // `decode_h264`: access units are refused until the gate
                // resets it.
                self.h264_broken = true;
                self.h264_results.extend(self.h264_pending.refuse_all());
                return Ok(false);
            }
        };
        if frames.is_empty() {
            return Ok(false);
        }
        // A picture that came out after the call that fed it: this is where
        // its frame ack is owed.
        for f in &frames {
            let answers = self.h264_pending.output(f.pts());
            self.h264_results.extend(answers);
        }
        // Same ordering rule as `decode_h264`: batched tile work for the
        // older framebuffer state lands before the full-frame draw.
        self.flush_under_h264(ctx)?;
        if self.h264_pipeline.is_none() {
            self.h264_pipeline = Some(crate::pipelines::h264_nv12::H264Nv12Pipeline::new(
                &ctx.device,
            ));
        }
        let frame_seq = self.h264_last_frame_seq.unwrap_or(0);
        tracing::debug!(
            frame_seq,
            frame_count = frames.len(),
            "poll_h264: late frames ready"
        );
        Ok(self.blit_h264_frames(ctx, frame_seq, frames))
    }

    /// What the decoder did with each access unit since the last call, in
    /// order. The embedder reports each to `ClientCore::h264_result`.
    pub fn take_h264_results(&mut self) -> Vec<(u32, ghostframe_client_core::H264Outcome)> {
        std::mem::take(&mut self.h264_results)
    }

    /// Drain and blit any frames still buffered by B-frame reordering.
    ///
    /// Terminal for the underlying decoder -- see [`ghostframe_client_h264::
    /// decoder::H264Decoder::finish`]'s own doc -- so this is for session
    /// teardown, not mid-stream use. A no-op if no H.264 has arrived yet.
    pub fn finish_h264(&mut self, ctx: &WgpuContext) {
        let Some(decoder) = self.h264_decoder.as_mut() else {
            return;
        };
        let frame_seq = self.h264_last_frame_seq.unwrap_or(0);
        match decoder.finish() {
            Ok(frames) => {
                // Teardown: nothing is flushed under the pictures and no
                // frame ack is owed (the session is ending), so only the
                // ordering against what is on screen is kept.
                let mut any_blitted = false;
                for frame in &frames {
                    let seq = h264_picture_seq(frame);
                    if seq.is_some_and(|s| self.batch_seq.h264_stale(s)) {
                        continue;
                    }
                    if self.blit_h264_frame(ctx, seq.unwrap_or(frame_seq), frame) {
                        any_blitted = true;
                        if let Some(s) = seq {
                            self.h264_picture_drawn(s);
                        }
                    }
                }
                if any_blitted {
                    self.ring.mark_dirty_all();
                }
            }
            Err(e) => tracing::warn!(error = %e, "H.264 finish (drain) failed"),
        }
    }

    /// Blit one decoded H.264 frame into the framebuffer. Returns whether it
    /// actually drew anything, so callers can decide whether the ring needs
    /// marking dirty.
    ///
    /// Each `HwFrame` holds a VA-API surface out of the decoder's pool for
    /// as long as it lives. Every caller of this function (`decode_h264`'s
    /// `for frame in frames` loop, `finish_h264`'s equivalent) drops each
    /// frame at the end of its own loop iteration, so nothing is held
    /// across presents and the decoder's default pool size is fine -- but
    /// that is a property of the caller, not a guarantee. A later change
    /// that queues frames (e.g. to hand them to a compositor across a frame
    /// boundary) makes the next `receive_frame` stall on pool exhaustion
    /// rather than fail, which reads as a hang, not a decode error.
    ///
    /// ## What "held until the submission retires" does and does not cover
    ///
    /// `mapped` (the dmabuf mapping) is kept alive until after `pipeline.draw`
    /// has been submitted below, not dropped the moment `import_nv12`
    /// returns: `map_dmabuf` takes its own reference on the VA-API surface
    /// (see its doc comment), so dropping `mapped` early would release that
    /// reference before the GPU work sampling it had even been submitted.
    ///
    /// That said, "submitted" is not "complete". `pipeline.draw` returns
    /// once the command buffer is queued, not once the GPU has executed it.
    /// `frame` (the `HwFrame`) is dropped by the caller shortly after this
    /// function returns -- returning its VA-API surface to the decoder's
    /// pool, which is LIFO, so the very next `decode()` call can hand that
    /// same surface back to the decoder to write into while the GPU may
    /// still be sampling it for this draw. Nothing in this function fences
    /// against that. It is believed safe on this project's hardware
    /// (amdgpu/RADV) because the kernel fences a shared dma-buf's BO
    /// through its reservation object implicitly -- any driver access that
    /// goes through the same dma-buf (VA-API's write, Vulkan's read here)
    /// is ordered by the kernel independent of userspace synchronization --
    /// but that is measured/assumed driver behaviour, exactly the class
    /// `import.rs`'s module doc warns must be written down rather than
    /// relied on silently: a driver that does not fence a shared dma-buf's
    /// BO this way would read the decoder's next write mid-sample instead
    /// of the frame this function intended, without any error.
    ///
    /// **That check has now been done, for the GLES/GStreamer target, and the
    /// answer was no.** V4L2 hands a buffer back to its pool on release with
    /// no fence for a GPU reader to wait on, and EGL's dmabuf import takes no
    /// fence either -- so nothing orders the decoder's next write against this
    /// draw. That path therefore waits for its own submission explicitly
    /// before the frame is released; see `wait_for_submission`, which also
    /// carries the measured cost. The Vulkan path keeps the implicit-fencing
    /// assumption, because it is a different driver stack and has not been
    /// re-measured -- not because it has been shown safe.
    fn blit_h264_frame(
        &mut self,
        ctx: &WgpuContext,
        frame_seq: u32,
        frame: &ghostframe_client_h264::decoder::HwFrame,
    ) -> bool {
        // A decoded size that disagrees with the framebuffer means the tile
        // stream's FrameDimensions and the H.264 SPS disagree. Blitting
        // anyway corrupts the framebuffer; dropping recovers at the next
        // keyframe. Logged at `warn` only on the first occurrence of a
        // mismatch episode -- a persistent disagreement would otherwise
        // burst-log once per frame at 60fps, the exact shape that has
        // blinded diagnosis here before (a journald rate-limit hole reading
        // as a stalled loop).
        if frame.width() != self.fb.width || frame.height() != self.fb.height {
            if !self.h264_size_mismatch_logged {
                tracing::warn!(
                    frame_seq,
                    decoded_w = frame.width(),
                    decoded_h = frame.height(),
                    fb_w = self.fb.width,
                    fb_h = self.fb.height,
                    "dropping H.264 frame(s) whose size disagrees with the framebuffer"
                );
                self.h264_size_mismatch_logged = true;
            }
            return false;
        }
        self.h264_size_mismatch_logged = false;

        let pipeline = self.h264_pipeline.as_mut().expect("created by caller");

        // Preferred path: import the decoder's dmabuf directly. Falls back
        // to a CPU download when the surface is tiled, misaligned, or the
        // driver's linear pitch disagrees with the producer's -- all of
        // which `import_nv12` reports rather than rendering wrong pixels.
        // `mapped` is kept alive alongside `imported`/the textures through
        // the draw below -- see this function's doc for exactly what that
        // does and does not guarantee.
        //
        // Timed at `debug` for the same M3 §10 measurement as
        // `decode_h264`'s `decode_us` above, and load-bearing to that
        // measurement's honesty: this call, not `decoder.decode()`, is
        // where `vaSyncSurface` actually waits for the hardware to finish
        // decoding (`HwFrame::map_dmabuf` syncs explicitly: FFmpeg 4.4's
        // map-to-DRM path does not, and skipping it drew a green frame).
        // A first review round timed only `decode()` and reported the
        // submit cost as if it were the decode cost; this span is what was
        // missing.
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real hardware vaSyncSurface wait for M3 §10's decode-thread decision, not a virtual-clock path"
        )]
        let map_start = std::time::Instant::now();
        let mapped = frame.map_dmabuf();
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real hardware vaSyncSurface wait for M3 §10's decode-thread decision, not a virtual-clock path"
        )]
        let map_us = map_start.elapsed().as_micros() as u64;
        tracing::debug!(frame_seq, map_us, "blit_h264_frame: map_dmabuf stall");
        let imported = match &mapped {
            Ok(m) => crate::import::import_nv12(ctx, m.planes()).map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };

        let path = match imported {
            Ok(imported) => {
                let submission = pipeline.draw(
                    &ctx.device,
                    &ctx.queue,
                    &self.fb,
                    imported.luma(),
                    imported.chroma(),
                );
                // Wait for exactly this draw before returning, because the
                // caller drops `frame` right after and that hands the decoder's
                // buffer straight back to its pool. See this function's doc:
                // the Vulkan path left this to the kernel's implicit dma-buf
                // fencing and asked a future hardware target to check rather
                // than inherit the assumption. This is that target, the
                // assumption does not hold here, and the fence is explicit.
                //
                // `submission` and not "the most recent submission": the tile
                // codecs share this queue, so an unqualified wait would block
                // on their work too and make this cost depend on what else the
                // frame happened to contain.
                wait_for_submission(ctx, submission, frame_seq);
                H264ImportPath::ZeroCopy
            }
            Err(reason) => {
                if self.h264_import_path != Some(H264ImportPath::CpuCopy) {
                    tracing::info!(
                        reason = %reason,
                        "H.264 import path: CPU copy (dmabuf import unavailable)"
                    );
                }
                let Some((luma, chroma)) = frame.download_nv12() else {
                    tracing::warn!(frame_seq, "H.264 CPU download failed");
                    return false;
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
                // No wait: these textures are wgpu's own, uploaded from a
                // `Vec` that has already been copied out of the decoder's
                // buffer. Nothing reclaims them underneath the GPU.
                let _ = pipeline.draw(&ctx.device, &ctx.queue, &self.fb, &luma_tex, &chroma_tex);
                H264ImportPath::CpuCopy
            }
        };
        if path == H264ImportPath::ZeroCopy && self.h264_import_path != Some(path) {
            // The layout is logged with it because a wrong offset or pitch is
            // the fault that still renders: right shape, wrong content.
            match &mapped {
                Ok(m) => tracing::info!(
                    planes = ?m.planes(),
                    "H.264 import path: zero-copy dmabuf"
                ),
                Err(_) => tracing::info!("H.264 import path: zero-copy dmabuf"),
            }
        }
        self.h264_import_path = Some(path);
        // `mapped` drops here, at the end of this function -- after both
        // draw calls above, not before either of them. See this function's
        // doc for why that matters and what it does not guarantee.
        true
    }
}

/// Block until `submission` has executed on the GPU.
///
/// The price of a zero-copy import whose buffer is borrowed from a decoder
/// pool. Without it, the sequence is: submit the draw, return, caller drops the
/// frame, the pool hands that same buffer to the decoder, and the decoder
/// writes the next picture into memory the GPU may still be sampling. The
/// result is not an error -- it is a frame with a band of the *next* frame in
/// it, which reads as tearing and has no diagnostic attached.
///
/// It would be nicer to hand the decoder a real fence and let the two overlap.
/// Neither EGL's dmabuf import nor V4L2's buffer handover offers one here, and
/// relying on the kernel to fence the dma-buf implicitly is what the Vulkan
/// path does and explicitly flagged as unverified on new hardware.
///
/// **Measured on RK3399/Mali-T860 at 1920x1080**, paced 60Hz, 120 frames:
/// p50 3.8ms, p90 5.7ms, p99 13.7ms, with a one-off 20.8ms on the third frame
/// (pipeline warmup). About 11% of a 33ms frame budget at the median.
///
/// Most of that is the blit itself -- 2M pixels of NV12->RGBA on a 500MHz
/// 4-core GPU -- not fence overhead, so the real cost of this call is the
/// pipelining it gives up rather than the time it adds. Affordable at 30fps,
/// tight at 60.
///
/// The identified optimisation, not taken here, is to defer the frame's release
/// by one instead of waiting: a frame-time of slack is longer than any measured
/// blit, so the buffer would be safe to recycle with no stall at all. It needs
/// `Renderer` to hold the previous `HwFrame` (cheap -- the pool has ~11
/// buffers), and it trades an explicit guarantee for a temporal one, which is
/// worth doing deliberately with its own measurement rather than bolted on
/// here.
fn wait_for_submission(ctx: &WgpuContext, submission: wgpu::SubmissionIndex, frame_seq: u32) {
    #[allow(
        clippy::disallowed_methods,
        reason = "measuring a real GPU wait, not a virtual-clock path"
    )]
    let start = std::time::Instant::now();
    match ctx.device.poll(wgpu::PollType::Wait {
        submission_index: Some(submission),
        // No timeout: a draw that never completes means the GPU is wedged, and
        // continuing would corrupt the next frame rather than recover. An
        // indefinite wait at least stalls visibly instead of rendering
        // plausible garbage.
        timeout: None,
    }) {
        Ok(_) => {}
        Err(e) => tracing::warn!(frame_seq, error = ?e, "h264 blit: waiting for the GPU failed"),
    }
    #[allow(
        clippy::disallowed_methods,
        reason = "measuring a real GPU wait, not a virtual-clock path"
    )]
    let wait_us = start.elapsed().as_micros() as u64;
    tracing::debug!(frame_seq, wait_us, "h264 blit: GPU fence wait");
}

fn log_decode_error(codec: Codec, tile_x: u8, tile_y: u8, code: DecodeErrorCode) {
    tracing::warn!(?codec, tile_x, tile_y, ?code, "tile decode error");
}
