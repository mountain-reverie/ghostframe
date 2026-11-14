// SPDX-License-Identifier: MIT OR Apache-2.0
//! [`TileRenderer`]: applies tile events, records their draws, submits on flush.
//!
//! Extracted from `ghostframe-client-gpu`'s `Renderer`, which keeps the native
//! half (export ring, upload slabs, H.264 decode) around one of these.
//!
//! **Device and queue are passed on every call**, not stored. The pipelines
//! under it already take them that way, the native client's `WgpuContext`
//! owns the device and lends it, and the web client's `WebRenderer` holds its
//! own clones. A stored clone would make the renderer's lifetime part of the
//! device's and add nothing the callers lack.
//!
//! Per-frame tile work is batched: [`TileRenderer::apply_event`] only records
//! what arrived; [`TileRenderer::flush`] runs the render and compute passes
//! once per frame, in one submit, rather than one pass per tile.

use crate::coalesce::{coalesce, Rect};
use crate::dirty::DirtyGrid;
use crate::framebuffer::Framebuffer;
use crate::paint_order::SeqLedger;
use crate::pipelines::cdf53_grid::{probe_grid, Cdf53Grid};
use crate::pipelines::cdf53_state::{
    Cdf53GridState, Cdf53Stats, Cdf53TileDebug, FLUSH_BUDGET_US, MAX_TILES_PER_FLUSH,
};
use crate::pipelines::palrle::PalRlePipeline;
use crate::pipelines::rgba::RgbaPipeline;
use crate::pipelines::solid::SolidPipeline;
use crate::GpuError;

use ghostframe_client_core::cdf53_prevalidate::PrevalidatedCdf53;
use ghostframe_client_core::cdf53_tile_state::Cdf53TileState;
use ghostframe_client_core::{DecodeErrorCode, Event, TileData};
use ghostframe_protocol::protocol::Codec;
use ghostframe_protocol::tile::TILE_SIZE;
use serde::Serialize;

/// Whether `width x height` is a size a renderer will allocate for.
/// The same bound `ClientCore` enforces; repeated here because the renderer
/// is a public API and must not trust that every caller went through it.
pub fn dimensions_valid(width: u32, height: u32) -> bool {
    let range = 1..=ghostframe_client_core::MAX_FRAME_DIMENSION;
    range.contains(&width) && range.contains(&height)
}

/// Tiles across and down for a `width x height` framebuffer.
pub fn tile_grid(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(TILE_SIZE), height.div_ceil(TILE_SIZE))
}

/// A tile whose pixels already sit in a caller-owned slab (the native
/// client's upload pool), so only its place in the paint order is recorded
/// here. `slab` and `index` are what [`TileRenderer`] needs to tell two slots
/// apart; `token` is the caller's own handle, handed back to the
/// [`SlotSink`] untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotTile {
    pub slab: usize,
    pub index: u32,
    pub token: u64,
}

/// What the caller does with [`SlotTile`]s at a flush. A `TileRenderer` used
/// without slots passes [`NoSlots`].
pub trait SlotSink {
    /// A slot given to this flush is done with: drawn, superseded or dropped.
    /// Called exactly once per slot.
    fn retire(&mut self, slot: &SlotTile);

    /// Record the draws of the slots that survived, one `(tile_x, tile_y,
    /// slot)` per tile (the later of two for a tile has already won). Return
    /// whether anything was recorded.
    fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        fb: &Framebuffer,
        tiles: &[(u32, u32, SlotTile)],
    ) -> bool;
}

/// The [`SlotSink`] for a renderer that is given no slot tiles.
pub struct NoSlots;

impl SlotSink for NoSlots {
    fn retire(&mut self, _slot: &SlotTile) {}

    fn encode(
        &mut self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _encoder: &mut wgpu::CommandEncoder,
        _fb: &Framebuffer,
        _tiles: &[(u32, u32, SlotTile)],
    ) -> bool {
        false
    }
}

/// Where one buffered `TileReady` tile's pixels are.
enum RgbaSrc {
    /// Owned bytes, to be staged and copied at flush.
    Pixels(Vec<u8>),
    /// A slot of a slab the caller already wrote.
    Slot(SlotTile),
}
/// Upper bound on [`GpuDecodeError`]s held between two drains.
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
pub struct GpuDecodeError {
    pub codec: Codec,
    pub tile_x: u8,
    pub tile_y: u8,
    pub code: DecodeErrorCode,
}

/// Map a shader error code to the wire enum. `None` for a code the shader
/// does not define.
#[cfg(not(target_arch = "wasm32"))]
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

/// The GPU CDF 5/3 decoder and the host state that schedules it.
struct Cdf53Gpu {
    grid: Cdf53Grid,
    state: Cdf53GridState,
    /// Grid frames recorded since the stats were last taken.
    frames: u64,
    cpu_fallback_logged: bool,
}

/// More tiles than this marked dirty between two drains collapse into "all of
/// it", so a caller that never drains cannot grow the list without bound.
const MAX_DIRTY_LIST: usize = 1 << 16;

/// What one flush did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushReport {
    /// Whether any GPU work was recorded (and so submitted).
    pub recorded: bool,
    /// Submits this flush made for its own work: 1 when `recorded`, else 0.
    pub submits: u32,
    /// Pixel rectangles that changed since the previous flush (or since the
    /// caller last drained them with [`TileRenderer::drain_dirty`]): the dirty
    /// tiles coalesced, clamped to the framebuffer; the whole framebuffer after
    /// a resize or [`TileRenderer::mark_all_dirty`].
    pub damage: Vec<Rect>,
    /// CDF 5/3 tiles are waiting for a later flush (the GPU budget was spent,
    /// or a partial tile is being throttled). The caller should flush again
    /// soon even if no event arrives.
    pub deferred_work: bool,
    /// The `now_us` this flush was given.
    pub at_us: u64,
}

/// One tile as the renderer sees it, for diagnostics. Serialises to JSON with
/// camelCase field names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugTile {
    pub tile_x: u32,
    pub tile_y: u32,
    /// The newest `frame_seq` painted (or about to be) on this tile.
    pub newest_frame_seq: Option<u32>,
    /// The CDF 5/3 grid's state for the tile; `None` when the GPU grid is not
    /// enabled or the tile has received nothing.
    pub cdf53: Option<Cdf53TileDebug>,
}

/// The renderer's own counters and sizes, for diagnostics. Serialises to JSON
/// with camelCase field names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugState {
    pub width: u32,
    pub height: u32,
    pub cols: u32,
    pub rows: u32,
    /// Completed flushes.
    pub flushes: u64,
    /// The `now_us` of the most recent flush.
    pub last_flush_us: u64,
    pub pending_solid: usize,
    pub pending_palrle: usize,
    pub pending_rgba: usize,
    /// The GPU CDF 5/3 grid was enabled, and the frame fits it.
    pub cdf53_gpu: bool,
    /// CDF 5/3 tiles are waiting for a flush.
    pub cdf53_pending: bool,
    /// The newest `frame_seq` of any tile entry noted.
    pub newest_tile_seq: Option<u32>,
    /// The newest H.264 picture noted with [`TileRenderer::note_h264`].
    pub newest_h264_seq: Option<u32>,
}

/// The pixel rectangles of `tiles`, as a flush reports them. Tiles outside
/// the grid are dropped (the wire carries `u8` coordinates, whatever the
/// framebuffer's size).
fn damage_rects(all: bool, tiles: &[(u8, u8)], fb_width: u32, fb_height: u32) -> Vec<Rect> {
    if all {
        return vec![Rect {
            x: 0,
            y: 0,
            w: fb_width,
            h: fb_height,
        }];
    }
    let (cols, rows) = tile_grid(fb_width, fb_height);
    let mut grid = DirtyGrid::new(cols, rows);
    let mut any = false;
    for &(x, y) in tiles {
        if (x as u32) < cols && (y as u32) < rows {
            grid.set(x as u32, y as u32);
            any = true;
        }
    }
    if !any {
        return Vec::new();
    }
    coalesce(&grid)
        .into_iter()
        .map(|r| r.to_pixels(fb_width, fb_height))
        .collect()
}

/// Owns the framebuffer and the decode pipelines for one client session.
///
/// See the module doc for the calling convention.
pub struct TileRenderer {
    fb: Framebuffer,
    solid: SolidPipeline,
    palrle: PalRlePipeline,
    rgba: RgbaPipeline,
    /// GPU reconstruction of CDF 5/3 tiles; see
    /// [`TileRenderer::enable_cdf53_gpu`]. `None` until enabled.
    cdf53_gpu: Option<Cdf53Gpu>,
    /// The CPU decoder CDF 5/3 passes fall back to when the GPU one is not
    /// enabled or the frame is too large for its plane store.
    cdf53_cpu: Cdf53TileState,
    /// Set while a flush runs only to put older work under an H.264 frame:
    /// the grid then keeps its pending tiles instead of spending its budget on
    /// pictures that are about to be covered. See [`TileRenderer::hold_cdf53`].
    cdf53_hold: bool,

    /// Every pending stateless entry carries the `frame_seq` it came from
    /// (first field); `batch_seq` holds the newest per tile. See
    /// [`crate::paint_order`].
    pending_solid: Vec<(u32, u8, u8, [u8; 4])>,
    pending_palrle: Vec<(u32, u8, u8, u8, u8, Vec<u8>)>,
    /// `TileReady` tiles awaiting upload, batched by `flush`.
    pending_rgba: Vec<(u32, u8, u8, RgbaSrc)>,
    /// Not cleared by a flush: it orders paints across flushes too.
    batch_seq: SeqLedger,
    /// Staging for `TileReady` pixel uploads; see [`UploadStaging`].
    upload_staging: UploadStaging,

    /// Tiles marked dirty since the last drain or flush.
    dirty_tiles: Vec<(u8, u8)>,
    /// Everything is dirty (a resize, [`TileRenderer::mark_all_dirty`], or an
    /// overflowing list).
    dirty_all: bool,
    flushes: u64,
    last_flush_us: u64,

    /// GPU-detected decode failures not yet drained by
    /// [`TileRenderer::take_decode_errors`].
    #[cfg(not(target_arch = "wasm32"))]
    decode_errors: Vec<GpuDecodeError>,
    /// Whether the flush reads the PalRle error slots back. Off by default:
    /// the readback is a blocking `poll(wait_indefinitely)` per flush.
    #[cfg(not(target_arch = "wasm32"))]
    collect_decode_errors: bool,
    /// The tiles of the last flush's PalRle batch, to attribute error slots.
    #[cfg(not(target_arch = "wasm32"))]
    palrle_tiles: Vec<(u8, u8)>,
}
/// Add a tile to the dirty list, or fold the list into "all" when it would
/// overflow [`MAX_DIRTY_LIST`].
fn push_dirty(list: &mut Vec<(u8, u8)>, all: &mut bool, tile_x: u8, tile_y: u8) {
    if *all {
        return;
    }
    if list.len() >= MAX_DIRTY_LIST {
        list.clear();
        *all = true;
    } else {
        list.push((tile_x, tile_y));
    }
}
impl TileRenderer {
    /// A renderer with a `width x height` framebuffer.
    ///
    /// `format` is the framebuffer's format and must be
    /// [`wgpu::TextureFormat::Rgba8Unorm`]: the decode shaders write it as an
    /// `rgba8unorm` storage texture. It is not the canvas format; a caller
    /// that presents to a `bgra8unorm` surface blits the framebuffer there.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Result<Self, GpuError> {
        if format != wgpu::TextureFormat::Rgba8Unorm {
            return Err(GpuError::UnsupportedFormat(format));
        }
        if !dimensions_valid(width, height) {
            return Err(GpuError::InvalidDimensions { width, height });
        }
        let fb = Framebuffer::new(device, width, height);
        let mut solid = SolidPipeline::new(device);
        solid.set_canvas_size(device, queue, width, height);
        Ok(TileRenderer {
            fb,
            solid,
            palrle: PalRlePipeline::new(device),
            rgba: RgbaPipeline::new(device),
            cdf53_gpu: None,
            cdf53_cpu: Cdf53TileState::new(),
            cdf53_hold: false,
            pending_solid: Vec::new(),
            pending_palrle: Vec::new(),
            pending_rgba: Vec::new(),
            batch_seq: SeqLedger::default(),
            upload_staging: UploadStaging::default(),
            dirty_tiles: Vec::new(),
            dirty_all: false,
            flushes: 0,
            last_flush_us: 0,
            #[cfg(not(target_arch = "wasm32"))]
            decode_errors: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            collect_decode_errors: false,
            #[cfg(not(target_arch = "wasm32"))]
            palrle_tiles: Vec::new(),
        })
    }

    /// The framebuffer texture. `Rgba8Unorm`, with `TEXTURE_BINDING`,
    /// `COPY_SRC` and `RENDER_ATTACHMENT`; replaced by a resize, so do not hold
    /// it across [`TileRenderer::apply_event`].
    pub fn framebuffer(&self) -> &wgpu::Texture {
        self.fb.texture()
    }

    /// The framebuffer with its view, size and epoch, for a caller's own
    /// passes over it (the native client's export blit and H.264 blit).
    pub fn fb(&self) -> &Framebuffer {
        &self.fb
    }

    /// Apply one `ClientCore` event: batch the tile for the next flush, or
    /// resize.
    ///
    /// Handles `TileReady`, `TilePayload` (Solid, PalRle and CDF 5/3),
    /// `PaletteUpdated` and `FrameDimensions` (which flushes what is pending
    /// against the old size first, with no slot sink: a caller that feeds
    /// [`SlotTile`]s routes `FrameDimensions` itself -- `flush_with` its sink,
    /// then [`TileRenderer::resize`] -- as the native `Renderer` does).
    /// `NeedsH264` is **not** handled: each client
    /// has its own H.264 path and routes that event itself, using
    /// [`TileRenderer::h264_stale`] and [`TileRenderer::note_h264`] so both
    /// apply the same ordering rules. `DecodeError` is logged (`ClientCore`
    /// already reports it) and `Evicted` is terminal, so neither draws.
    ///
    /// Every `Event` variant is matched explicitly -- see the crate doc on
    /// `clippy::wildcard_enum_match_arm` for why a `_ =>` arm is refused here.
    pub fn apply_event(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        ev: Event,
    ) -> Result<(), GpuError> {
        match ev {
            Event::TileReady {
                tile_x,
                tile_y,
                rgba,
                frame_seq,
            } => {
                // Not the path the shaders exist for (that is `TilePayload`),
                // but a valid event this renderer must still handle correctly:
                // the bytes are already final RGBA pixels.
                //
                // Buffered rather than uploaded here, and batched in `flush`.
                // One `write_texture` per tile costs ~178us on a Pinebook Pro
                // regardless of size -- almost entirely CPU-side call overhead
                // -- so a full 1920x1080 repaint spent 363ms just issuing 2040
                // of them. See `encode_pending_rgba`.
                self.batch_seq.note(tile_x, tile_y, frame_seq);
                self.note_stateless(tile_x, tile_y, frame_seq);
                self.pending_rgba
                    .push((frame_seq, tile_x, tile_y, RgbaSrc::Pixels(rgba)));
                self.mark_dirty(tile_x, tile_y);
            }
            Event::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                generation,
                data,
            } => match data {
                TileData::Solid(bgra) => {
                    self.note_stateless(tile_x, tile_y, frame_seq);
                    self.batch_seq.note(tile_x, tile_y, frame_seq);
                    self.pending_solid.push((frame_seq, tile_x, tile_y, bgra));
                    self.mark_dirty(tile_x, tile_y);
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
                    self.mark_dirty(tile_x, tile_y);
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
                // reaching the renderer bypassed it. Expand here rather than
                // draw nothing.
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
            },
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
                // against it. `NoSlots` cannot draw a slot tile, so a pending
                // one means the caller should have routed this event itself.
                debug_assert!(
                    !self
                        .pending_rgba
                        .iter()
                        .any(|(_, _, _, src)| matches!(src, RgbaSrc::Slot(_))),
                    "FrameDimensions with slot tiles pending: flush_with the slot sink, then resize"
                );
                self.flush_with(device, queue, self.last_flush_us, &mut NoSlots)?;
                self.resize(device, queue, width, height);
            }
            Event::NeedsH264 { .. } => {
                // Not this renderer's: see the method doc.
            }
            Event::DecodeError {
                codec,
                tile_x,
                tile_y,
                code,
            } => {
                // `ClientCore` already reports this over the feedback
                // stream (`decode_error_batcher`); the renderer has no
                // separate telemetry sink, so this is a deliberate no-op
                // rather than a silent drop -- logged so a real decode
                // failure is still visible locally.
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
                self.palrle.upload_palette(queue, palette_id, &full);
            }
            Event::Evicted { .. } => {
                // Terminal: the session is going away and the net thread
                // separately surfaces this to the host. Nothing left to paint.
            }
        }
        Ok(())
    }

    /// Resize the framebuffer to `width x height` (a size that passed
    /// [`dimensions_valid`]), keeping what fits at the origin.
    ///
    /// **Flush first.** Pending tiles were addressed against the old size;
    /// [`TileRenderer::apply_event`] does that for `FrameDimensions`, and a
    /// caller that resizes directly (the native client, whose flush also
    /// publishes) must too. Marks everything dirty.
    pub fn resize(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, width: u32, height: u32) {
        let epoch = self.fb.epoch();
        self.fb.resize(device, queue, width, height);
        if self.fb.epoch() != epoch {
            // A new picture: what was painted on the old one says nothing
            // about this one.
            self.batch_seq.clear();
            // The cached bind groups hold the old texture; drop them now
            // rather than when each codec next draws, or the old framebuffer
            // outlives the resize.
            self.rgba.drop_framebuffer_cache();
            self.palrle.drop_framebuffer_cache();
            if let Some(g) = self.cdf53_gpu.as_mut() {
                g.grid.drop_framebuffer_cache();
            }
            self.dirty_all = true;
        }
        self.solid.set_canvas_size(device, queue, width, height);
        self.resize_cdf53(device, width, height);
        // The CPU accumulator keys its tiles by position, so a smaller grid
        // drops the tiles outside it; planes of a tile that stays belong to
        // the same picture.
        let (cols, rows) = tile_grid(width, height);
        self.cdf53_cpu
            .retain_tiles(|x, y| (x as u32) < cols && (y as u32) < rows);
    }

    /// Forget everything about the session's picture: pending draws, the
    /// paint-order ledger (the H.264 floor included), the PalRLE palette atlas
    /// (zeroed, as the TypeScript renderer's `onSessionReset` did) and the
    /// CDF 5/3 state on both sides. The framebuffer's pixels stay; the next
    /// full refresh from the server overwrites them. Marks everything dirty.
    ///
    /// Slot tiles still pending are dropped without being retired: a caller
    /// that uses slots rebuilds the renderer instead (the native client does,
    /// per session). `device` is not used today; it is in the signature so a
    /// reset that has to allocate does not change the call.
    pub fn reset_session(
        &mut self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<(), GpuError> {
        self.palrle.clear_palettes(queue);
        self.batch_seq.clear();
        self.pending_solid.clear();
        self.pending_palrle.clear();
        self.pending_rgba.clear();
        self.cdf53_cpu.reset();
        if let Some(g) = self.cdf53_gpu.as_mut() {
            // A new shape forgets every tile; the same shape would keep them.
            let (cols, rows) = tile_grid(self.fb.width, self.fb.height);
            g.state.resize(0, 0);
            if g.grid.is_usable() {
                g.state.resize(cols, rows);
            }
        }
        self.dirty_tiles.clear();
        self.dirty_all = true;
        Ok(())
    }

    /// Whether an H.264 picture from `seq` is older than something already
    /// painted: a newer picture, or a tile from a later frame. Such a picture
    /// covers the whole screen, so drawing it would undo the newer paint; the
    /// caller drops it.
    pub fn h264_stale(&self, seq: u32) -> bool {
        self.batch_seq.h264_stale(seq)
    }

    /// An H.264 picture from `seq` is now over the whole framebuffer: record
    /// it in the paint-order ledger, cancel the CDF 5/3 draws it is newer than
    /// and refuse older passes. It does not touch the damage: a caller that
    /// reports damage marks it itself with [`TileRenderer::mark_all_dirty`]
    /// (the native client's export ring already does).
    pub fn note_h264(&mut self, seq: u32) {
        self.batch_seq.note_h264(seq);
        self.supersede_cdf53(seq);
    }

    /// An H.264 frame from `frame_seq` was blitted over the whole framebuffer:
    /// cancel CDF 5/3 draws still waiting, and refuse older passes.
    pub fn supersede_cdf53(&mut self, frame_seq: u32) {
        if let Some(g) = self.cdf53_gpu.as_mut() {
            g.state.supersede_all(frame_seq);
        }
    }

    /// While `hold` is set, a flush keeps the CDF 5/3 tiles pending instead of
    /// drawing them. For the flush that puts older Solid / PalRle / RGBA work
    /// under an H.264 frame: the frame covers the screen, so drawing the CDF 5/3
    /// tiles first would only spend a flush's GPU budget, and if the frame is
    /// then blitted they are cancelled ([`TileRenderer::note_h264`]) where
    /// drawing them later would put them on top of it.
    pub fn hold_cdf53(&mut self, hold: bool) {
        self.cdf53_hold = hold;
    }

    /// Mark the whole surface dirty, so the next report carries all of it.
    pub fn mark_all_dirty(&mut self) {
        self.dirty_all = true;
    }

    fn mark_dirty(&mut self, tile_x: u8, tile_y: u8) {
        push_dirty(&mut self.dirty_tiles, &mut self.dirty_all, tile_x, tile_y);
    }

    /// Hand every tile marked dirty since the last drain (or flush) to `f`,
    /// and forget them. Returns whether *everything* is dirty, in which case
    /// `f` was not called. A caller that tracks damage itself (the native
    /// client's export ring) drains after each call that can mark; one that
    /// does not reads [`FlushReport::damage`].
    pub fn drain_dirty(&mut self, mut f: impl FnMut(u8, u8)) -> bool {
        for (x, y) in self.dirty_tiles.drain(..) {
            f(x, y);
        }
        std::mem::take(&mut self.dirty_all)
    }

    /// Run `f` over every slot tile still waiting for a flush. For a caller
    /// that is dropped with slots outstanding and must hand them back.
    pub fn for_each_pending_slot(&self, mut f: impl FnMut(&SlotTile)) {
        for (_, _, _, src) in &self.pending_rgba {
            if let RgbaSrc::Slot(slot) = src {
                f(slot);
            }
        }
    }

    /// The slot counterpart of an `Event::TileReady`: the tile's pixels are
    /// already in a slab, so only its place in the paint order is recorded.
    /// Behaves exactly as `TileReady` does for ordering, superseding and
    /// duplicates (the later of two for one tile wins, whichever kind). The
    /// flush hands the slot to its [`SlotSink`].
    pub fn apply_slot_tile(&mut self, frame_seq: u32, tile_x: u8, tile_y: u8, slot: SlotTile) {
        self.batch_seq.note(tile_x, tile_y, frame_seq);
        self.note_stateless(tile_x, tile_y, frame_seq);
        self.pending_rgba
            .push((frame_seq, tile_x, tile_y, RgbaSrc::Slot(slot)));
        self.mark_dirty(tile_x, tile_y);
    }

    /// The newest `frame_seq` noted for a tile and the CDF 5/3 grid's state
    /// for it, for diagnostics; `None` for a tile outside the framebuffer's
    /// grid.
    pub fn debug_tile(&self, tile_x: u32, tile_y: u32) -> Option<DebugTile> {
        let (cols, rows) = tile_grid(self.fb.width, self.fb.height);
        if tile_x >= cols || tile_y >= rows {
            return None;
        }
        let (x, y) = (u8::try_from(tile_x).ok()?, u8::try_from(tile_y).ok()?);
        Some(DebugTile {
            tile_x,
            tile_y,
            newest_frame_seq: self.batch_seq.newest_for(x, y),
            cdf53: self
                .cdf53_gpu
                .as_ref()
                .and_then(|g| g.state.debug_tile(x, y)),
        })
    }

    /// Whether the GPU CDF 5/3 grid is enabled and the current frame fits it
    /// (otherwise passes are decoded on the CPU).
    pub fn cdf53_gpu_enabled(&self) -> bool {
        self.cdf53_gpu.as_ref().is_some_and(|g| g.grid.is_usable())
    }

    /// Counters and sizes, for diagnostics.
    pub fn debug_state(&self) -> DebugState {
        let (cols, rows) = tile_grid(self.fb.width, self.fb.height);
        DebugState {
            width: self.fb.width,
            height: self.fb.height,
            cols,
            rows,
            flushes: self.flushes,
            last_flush_us: self.last_flush_us,
            pending_solid: self.pending_solid.len(),
            pending_palrle: self.pending_palrle.len(),
            pending_rgba: self.pending_rgba.len(),
            cdf53_gpu: self.cdf53_gpu_enabled(),
            cdf53_pending: self.has_deferred_work(),
            newest_tile_seq: self.batch_seq.max_tile(),
            newest_h264_seq: self.batch_seq.h264(),
        }
    }

    /// Turn on GPU reconstruction of CDF 5/3 tiles. Allocates the grid's plane
    /// store for the current frame size. The caller is expected to have proven
    /// the shaders on this device first
    /// ([`TileRenderer::try_enable_cdf53_gpu`] does both): this does not.
    ///
    /// A frame whose plane store the device cannot bind (about 8K) is decoded
    /// on the CPU by this renderer instead, and the GPU is used again as soon
    /// as a frame fits.
    ///
    /// Builds its own grid with the cost model of the Mali-T860 it was fitted
    /// on (and the 1000-tile submit cap); the native client instead hands over
    /// the grid it probed, with [`TileRenderer::enable_cdf53_gpu_with`].
    pub fn enable_cdf53_gpu(&mut self, device: &wgpu::Device) {
        if self.cdf53_gpu.is_none() {
            self.install_cdf53(device, Cdf53Grid::new(device), None, MAX_TILES_PER_FLUSH);
        }
    }

    /// [`TileRenderer::enable_cdf53_gpu`] with a grid that already passed its
    /// self-test on this device, so the shaders are compiled once, and with the
    /// per-flush budget calibrated from what the probe measured
    /// (`us_per_tile_k14`; `None` keeps the default cost model). `max_tiles`
    /// caps one submit (the ~500 ms job watchdog of panfrost/GL is the reason;
    /// Vulkan can take far more).
    pub fn enable_cdf53_gpu_with(
        &mut self,
        device: &wgpu::Device,
        grid: Cdf53Grid,
        us_per_tile_k14: Option<f64>,
        max_tiles: usize,
    ) {
        if self.cdf53_gpu.is_none() {
            self.install_cdf53(device, grid, us_per_tile_k14, max_tiles);
        }
    }

    /// Run the CDF 5/3 grid's start-up self-test on this device and, only if
    /// it passes, enable the GPU grid. The reconstruct shader is sensitive to
    /// register allocation on panfrost: a build that tips over it silently
    /// writes nothing, so the grid is not trusted until two known tiles come
    /// back exact. On `Err` (a shader that does not build, or a wrong picture)
    /// nothing is enabled and CDF 5/3 passes keep being decoded on the CPU
    /// (`Cdf53TileState`); the caller logs the message.
    ///
    /// Async because the check reads pixels back; a native caller drives it
    /// with [`crate::readback::block_on_device`].
    pub async fn try_enable_cdf53_gpu(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<(), String> {
        if self.cdf53_gpu.is_some() {
            return Ok(());
        }
        let grid = probe_grid(device, queue, Cdf53Grid::new).await?;
        self.install_cdf53(device, grid, None, MAX_TILES_PER_FLUSH);
        Ok(())
    }

    fn install_cdf53(
        &mut self,
        device: &wgpu::Device,
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
        self.resize_cdf53(device, w, h);
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
    fn resize_cdf53(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        let Some(g) = self.cdf53_gpu.as_mut() else {
            return;
        };
        let (cols, rows) = tile_grid(width, height);
        let was_gpu = g.grid.is_usable();
        let result = g.grid.resize(device, cols, rows);
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
    /// drawn, noted in the paint-order ledger or marked dirty until a flush
    /// lists the tile. Without it the pass is reconstructed here, on the CPU,
    /// into a `TileReady`-style upload.
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
            self.mark_dirty(tile_x, tile_y);
        }
    }

    /// Plan this flush's CDF 5/3 tiles and record the grid. See
    /// [`Cdf53GridState::plan`] for what gets listed.
    fn record_cdf53(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
    ) -> bool {
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
            // Not `self.mark_dirty`: `g` still borrows `self.cdf53_gpu`.
            push_dirty(
                &mut self.dirty_tiles,
                &mut self.dirty_all,
                t.tile_x,
                t.tile_y,
            );
        }
        g.frames += 1;
        g.grid.record(device, queue, encoder, &self.fb)
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

    /// Enable (or disable) reading the PalRle error slots back after each
    /// flush. Native only: the readback blocks on the device.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_collect_decode_errors(&mut self, on: bool) {
        self.collect_decode_errors = on;
    }

    /// Take the GPU-detected tile decode failures accumulated by flushes.
    ///
    /// The shader validates PalRle payloads on the device (an index past the
    /// palette count, say), so only the readback after the decode can tell.
    /// The caller is expected to relay these as DECODE_ERROR feedback.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn take_decode_errors(&mut self) -> Vec<GpuDecodeError> {
        std::mem::take(&mut self.decode_errors)
    }
    /// Submit all batched work, in one submit, and report what changed.
    ///
    /// `now_us` is the caller's clock in microseconds (any epoch): the
    /// renderer reads no clock of its own. It is recorded in the report and
    /// in [`DebugState::last_flush_us`].
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
    pub fn flush(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        now_us: u64,
    ) -> Result<FlushReport, GpuError> {
        self.flush_with(device, queue, now_us, &mut NoSlots)
    }

    /// [`TileRenderer::flush`] for a caller that feeds slot tiles.
    pub fn flush_with(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        now_us: u64,
        slots: &mut dyn SlotSink,
    ) -> Result<FlushReport, GpuError> {
        self.last_flush_us = now_us;
        let submits_before = crate::submit::count();
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-flush"),
        });
        let recorded = self.record(device, queue, &mut encoder, slots);
        if recorded {
            crate::submit::submit(queue, encoder);
        }
        let submits = crate::submit::count().wrapping_sub(submits_before) as u32;
        self.finish_flush(device, queue)?;
        let all = std::mem::take(&mut self.dirty_all);
        let tiles = std::mem::take(&mut self.dirty_tiles);
        let damage = damage_rects(all, &tiles, self.fb.width, self.fb.height);
        Ok(FlushReport {
            recorded,
            submits,
            damage,
            deferred_work: self.has_deferred_work(),
            at_us: now_us,
        })
    }

    /// Record every pending draw into `encoder`, in paint order, without
    /// submitting it. Returns whether anything was recorded.
    ///
    /// For a caller that adds its own commands to the same encoder (the native
    /// client's export blit) so the flush stays one submit. Pair it with
    /// submitting the encoder and then [`TileRenderer::finish_flush`].
    pub fn record(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        slots: &mut dyn SlotSink,
    ) -> bool {
        // Order within a batch is by `frame_seq`, not by codec (see
        // `crate::paint_order`). The CDF 5/3 grid goes first so that a newer
        // stateless entry lands on top of an older pass; a stateless entry
        // older than anything else for its tile is dropped, so a newer pass
        // is not covered either. Only tiles the grid is *drawing* this flush
        // count towards a tile's newest `frame_seq`: a pass that is held back
        // must not make an older Solid disappear.
        let mut recorded = self.record_cdf53(device, queue, encoder);

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
        // issued, so the sink retires them with the rest of the flush's.
        self.pending_rgba.retain(|(seq, tx, ty, src)| {
            let keep = !ledger.superseded(*tx, *ty, *seq);
            if let (false, RgbaSrc::Slot(slot)) = (keep, src) {
                slots.retire(slot);
            }
            keep
        });

        recorded |= self.solid.encode(device, queue, encoder, &self.fb, &solid);
        recorded |= self
            .palrle
            .encode(device, queue, encoder, &self.fb, &palrle_batch);
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.palrle_tiles = palrle_batch.iter().map(|&(tx, ty, ..)| (tx, ty)).collect();
        }
        recorded |= self.encode_pending_rgba(device, queue, encoder, slots);
        recorded
    }

    /// What follows a submit of [`TileRenderer::record`]'s encoder: the opt-in
    /// PalRle error readback (native only), and the flush count.
    /// [`TileRenderer::flush`] does it for you.
    pub fn finish_flush(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<(), GpuError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let tiles = std::mem::take(&mut self.palrle_tiles);
            if !tiles.is_empty() && self.collect_decode_errors {
                let errors = self.palrle.take_errors(device, queue)?;
                for (i, code) in errors {
                    let Some(&(tile_x, tile_y)) = tiles.get(i as usize) else {
                        continue;
                    };
                    let Some(code) = decode_error_code(code) else {
                        tracing::warn!(code, tile_x, tile_y, "unknown GPU palrle error code");
                        continue;
                    };
                    // Bounded: a hostile stream of bad tiles must not grow
                    // this without limit between two drains.
                    if self.decode_errors.len() < MAX_PENDING_DECODE_ERRORS {
                        self.decode_errors.push(GpuDecodeError {
                            codec: Codec::PalRle,
                            tile_x,
                            tile_y,
                            code,
                        });
                    }
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        let _ = (device, queue);
        self.flushes = self.flushes.saturating_add(1);
        Ok(())
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
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        slots: &mut dyn SlotSink,
    ) -> bool {
        if self.pending_rgba.is_empty() {
            return false;
        }
        let full_tile_bytes = (TILE_SIZE * TILE_SIZE * 4) as usize;
        let pending = std::mem::take(&mut self.pending_rgba);

        // Every slot given to this flush is retired after it, whichever of the
        // paths below (or none) ends up drawing it.
        for (_, _, _, src) in &pending {
            if let RgbaSrc::Slot(slot) = src {
                slots.retire(slot);
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
        let mut by_slot: std::collections::HashMap<(u32, u32), SlotTile> =
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

        let mut recorded = self.encode_staged_copies(device, queue, encoder, &copies, &by_tile);
        recorded |= self
            .rgba
            .encode(device, queue, encoder, &self.fb, &scattered);

        // The slot tiles, drawn by the caller's sink.
        let slot_tiles: Vec<(u32, u32, SlotTile)> = by_slot
            .iter()
            .map(|(&(x, y), &slot)| (x, y, slot))
            .collect();
        recorded |= slots.encode(device, queue, encoder, &self.fb, &slot_tiles);
        recorded
    }

    /// Record every copy in `copies` into the framebuffer, all staged through
    /// one range of [`UploadStaging`] and so one `write_buffer_with`: each
    /// queue write is a staging buffer and, on panfrost, several flushes.
    /// Each copy has its own sub-range (a multiple of the row alignment), so
    /// none overwrites another. Returns whether anything was recorded.
    fn encode_staged_copies(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
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
        let (buffer, base) = self.upload_staging.reserve(device, total);
        let Some(mut view) = queue.write_buffer_with(
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
}

fn log_decode_error(codec: Codec, tile_x: u8, tile_y: u8, code: DecodeErrorCode) {
    tracing::warn!(?codec, tile_x, tile_y, ?code, "tile decode error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::noop_device::device;
    use crate::pipelines::cdf53_grid::self_test_tiles;

    const RGBA: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

    fn renderer(w: u32, h: u32) -> (TileRenderer, wgpu::Device, wgpu::Queue) {
        let (d, q) = device();
        let r = TileRenderer::new(&d, &q, RGBA, w, h).expect("renderer");
        (r, d, q)
    }

    fn solid(seq: u32, x: u8, y: u8) -> Event {
        Event::TilePayload {
            frame_seq: seq,
            tile_x: x,
            tile_y: y,
            generation: 0,
            data: TileData::Solid([1, 2, 3, 255]),
        }
    }

    fn pass(seq: u32, x: u8, y: u8, idx: u8, present: Option<u16>, tag: u8) -> Event {
        Event::TilePayload {
            frame_seq: seq,
            tile_x: x,
            tile_y: y,
            generation: 0,
            data: TileData::Cdf53 {
                pass_idx: idx,
                bit_planes: vec![tag; 384],
                present_passes: present,
                new_generation: false,
            },
        }
    }

    #[test]
    fn new_refuses_a_format_the_decode_shaders_cannot_write() {
        let (d, q) = device();
        for format in [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ] {
            match TileRenderer::new(&d, &q, format, 64, 64) {
                Err(GpuError::UnsupportedFormat(f)) => assert_eq!(f, format),
                other => panic!(
                    "{format:?}: expected UnsupportedFormat, got {:?}",
                    other.err()
                ),
            }
        }
    }

    #[test]
    fn new_refuses_sizes_outside_the_wire_bound() {
        let (d, q) = device();
        let too_big = ghostframe_client_core::MAX_FRAME_DIMENSION + 1;
        for (w, h) in [(0, 64), (64, 0), (too_big, 64), (64, too_big)] {
            assert!(
                matches!(
                    TileRenderer::new(&d, &q, RGBA, w, h),
                    Err(GpuError::InvalidDimensions { .. })
                ),
                "{w}x{h}"
            );
        }
    }

    #[test]
    fn a_tile_flushes_in_one_submit_and_its_damage_is_reported_once() {
        let (mut r, d, q) = renderer(100, 70);
        r.apply_event(&d, &q, solid(1, 1, 2)).unwrap();
        let report = r.flush(&d, &q, 1_000).unwrap();
        assert!(report.recorded);
        assert_eq!(report.submits, 1, "a flush is one submit");
        assert_eq!(report.at_us, 1_000);
        // Tile (1, 2) of a 100x70 frame: x = 32, y = 64, clamped to 6 rows.
        assert_eq!(
            report.damage,
            vec![Rect {
                x: 32,
                y: 64,
                w: 32,
                h: 6
            }]
        );
        assert!(!report.deferred_work);

        let again = r.flush(&d, &q, 2_000).unwrap();
        assert!(!again.recorded);
        assert_eq!(again.submits, 0, "nothing to draw, nothing to submit");
        assert!(again.damage.is_empty(), "damage is not reported twice");
        let state = r.debug_state();
        assert_eq!((state.flushes, state.last_flush_us), (2, 2_000));
    }

    #[test]
    fn the_h264_ledger_orders_pictures_against_pictures_and_tiles() {
        let (mut r, d, q) = renderer(64, 64);
        assert!(!r.h264_stale(1), "nothing painted yet");
        r.note_h264(100);
        assert!(r.h264_stale(99), "older than the picture on screen");
        assert!(!r.h264_stale(100), "equal never supersedes");
        assert!(!r.h264_stale(101));
        r.apply_event(&d, &q, solid(200, 0, 0)).unwrap();
        assert!(r.h264_stale(150), "a tile from a later frame is on screen");
        assert!(!r.h264_stale(200));
        assert_eq!(r.debug_state().newest_h264_seq, Some(100));
    }

    #[test]
    fn a_resize_forgets_the_picture_and_damages_all_of_it() {
        let (mut r, d, q) = renderer(64, 64);
        r.apply_event(&d, &q, solid(5, 0, 0)).unwrap();
        r.flush(&d, &q, 1).unwrap();
        assert_eq!(r.debug_tile(0, 0).unwrap().newest_frame_seq, Some(5));

        // The same size again (the server repeats FrameDimensions): nothing
        // about the picture changes.
        r.apply_event(
            &d,
            &q,
            Event::FrameDimensions {
                width: 64,
                height: 64,
            },
        )
        .unwrap();
        assert_eq!(r.debug_tile(0, 0).unwrap().newest_frame_seq, Some(5));

        r.apply_event(
            &d,
            &q,
            Event::FrameDimensions {
                width: 128,
                height: 96,
            },
        )
        .unwrap();
        assert_eq!(r.debug_tile(0, 0).unwrap().newest_frame_seq, None);
        assert_eq!(
            (r.framebuffer().width(), r.framebuffer().height()),
            (128, 96)
        );
        let report = r.flush(&d, &q, 2).unwrap();
        assert_eq!(
            report.damage,
            vec![Rect {
                x: 0,
                y: 0,
                w: 128,
                h: 96
            }]
        );
    }

    #[test]
    fn an_out_of_range_resize_is_ignored() {
        let (mut r, d, q) = renderer(64, 64);
        r.apply_event(
            &d,
            &q,
            Event::FrameDimensions {
                width: 0,
                height: 64,
            },
        )
        .unwrap();
        assert_eq!(r.framebuffer().width(), 64);
    }

    #[test]
    fn the_cdf53_grid_holds_passes_until_a_flush_lists_them() {
        let (mut r, d, q) = renderer(64, 64);
        r.enable_cdf53_gpu(&d);
        // Pass 0 carries the bitmap: passes 0 and 2 exist, so two make the tile.
        r.apply_event(&d, &q, pass(7, 0, 0, 0, Some(0b101), 1))
            .unwrap();
        r.apply_event(&d, &q, pass(7, 0, 0, 2, None, 3)).unwrap();
        assert!(r.has_deferred_work(), "the grid is waiting for a flush");
        let tile = r
            .debug_tile(0, 0)
            .unwrap()
            .cdf53
            .expect("grid state for the tile");
        assert_eq!(tile.received_mask, 0b101);
        assert_eq!(tile.drawn_k, None);
        assert_eq!(
            r.debug_tile(0, 0).unwrap().newest_frame_seq,
            None,
            "a waiting pass is not a paint"
        );

        let report = r.flush(&d, &q, 1).unwrap();
        assert!(report.recorded, "the grid drew the tile");
        assert!(!report.deferred_work && !r.has_deferred_work());
        assert_eq!(
            report.damage,
            vec![Rect {
                x: 0,
                y: 0,
                w: 32,
                h: 32
            }]
        );
        assert_eq!(r.debug_tile(0, 0).unwrap().newest_frame_seq, Some(7));
        let (stats, frames) = r.take_cdf53_stats().expect("grid enabled");
        assert_eq!((stats.listed, frames), (1, 1));
    }

    #[test]
    fn without_the_grid_cdf53_passes_decode_on_the_cpu() {
        let (mut r, d, q) = renderer(64, 64);
        let tile = &self_test_tiles()[0];
        for (p, plane) in tile.planes.iter().enumerate() {
            let Some(plane) = plane else { continue };
            let ev = Event::TilePayload {
                frame_seq: 3,
                tile_x: 1,
                tile_y: 0,
                generation: 0,
                data: TileData::Cdf53 {
                    pass_idx: p as u8,
                    bit_planes: plane.clone(),
                    present_passes: (p == 0).then_some(tile.present),
                    new_generation: false,
                },
            };
            r.apply_event(&d, &q, ev).unwrap();
        }
        let state = r.debug_state();
        assert!(!state.cdf53_gpu);
        assert!(state.pending_rgba >= 1, "the CPU decode produced an upload");
        let report = r.flush(&d, &q, 1).unwrap();
        assert!(report.recorded);
        assert_eq!(
            report.damage,
            vec![Rect {
                x: 32,
                y: 0,
                w: 32,
                h: 32
            }]
        );
    }

    #[test]
    fn the_self_test_keeps_a_grid_that_draws_nothing_off() {
        // The no-op backend runs no shader, so the reconstruct draws nothing
        // and the two known tiles do not come back: the gate must refuse.
        let (mut r, d, q) = renderer(64, 64);
        let err = crate::readback::block_on_device(&d, r.try_enable_cdf53_gpu(&d, &q))
            .expect_err("a device that draws nothing must fail the self-test");
        assert!(err.contains("frame 1"), "{err}");
        assert!(!r.debug_state().cdf53_gpu, "the grid stays off");
        // And the CPU path is still there.
        r.apply_event(&d, &q, pass(1, 0, 0, 0, Some(1), 1)).unwrap();
        assert!(!r.has_deferred_work());
    }

    fn palrle(seq: u32, x: u8, y: u8) -> Event {
        Event::TilePayload {
            frame_seq: seq,
            tile_x: x,
            tile_y: y,
            generation: 0,
            data: TileData::PalRle {
                palette_id: 0,
                count: 1,
                indices: vec![0; 512],
                wire_flags: 0,
            },
        }
    }

    fn ready(seq: u32, x: u8, y: u8) -> Event {
        Event::TileReady {
            tile_x: x,
            tile_y: y,
            rgba: vec![7; 32 * 32 * 4],
            frame_seq: seq,
        }
    }

    /// An older stateless tile that arrives after a newer paint of the same
    /// tile in an earlier flush is not drawn, and the ledger keeps the newer.
    fn older_after_newer_is_not_drawn(newer: Event, older: Event) {
        let (mut r, d, q) = renderer(64, 64);
        r.apply_event(&d, &q, newer).unwrap();
        assert!(r.flush(&d, &q, 1).unwrap().recorded, "the newer is drawn");
        r.apply_event(&d, &q, older).unwrap();
        let report = r.flush(&d, &q, 2).unwrap();
        assert!(!report.recorded, "the older tile must not be drawn");
        let state = r.debug_state();
        assert_eq!(
            (
                state.pending_solid,
                state.pending_palrle,
                state.pending_rgba
            ),
            (0, 0, 0)
        );
        assert_eq!(r.debug_tile(0, 0).unwrap().newest_frame_seq, Some(10));
    }

    #[test]
    fn an_older_solid_does_not_cover_a_newer_paint_from_an_earlier_flush() {
        older_after_newer_is_not_drawn(solid(10, 0, 0), solid(5, 0, 0));
    }

    #[test]
    fn an_older_palrle_does_not_cover_a_newer_paint_from_an_earlier_flush() {
        older_after_newer_is_not_drawn(palrle(10, 0, 0), palrle(5, 0, 0));
        older_after_newer_is_not_drawn(solid(10, 0, 0), palrle(5, 0, 0));
    }

    #[test]
    fn an_older_tile_ready_does_not_cover_a_newer_paint_from_an_earlier_flush() {
        older_after_newer_is_not_drawn(ready(10, 0, 0), ready(5, 0, 0));
        older_after_newer_is_not_drawn(solid(10, 0, 0), ready(5, 0, 0));
    }

    #[test]
    fn reset_session_forgets_pending_work_the_ledger_and_the_h264_floor() {
        let (mut r, d, q) = renderer(64, 64);
        r.apply_event(&d, &q, solid(9, 0, 0)).unwrap();
        r.note_h264(50);
        r.reset_session(&d, &q).unwrap();
        assert_eq!(r.debug_state().pending_solid, 0);
        assert_eq!(r.debug_tile(0, 0).unwrap().newest_frame_seq, None);
        assert!(!r.h264_stale(1), "the H.264 floor is gone");
        let report = r.flush(&d, &q, 1).unwrap();
        assert!(!report.recorded, "the dropped tile is not drawn");
        assert_eq!(report.damage.len(), 1, "but the picture is all dirty");
    }

    #[test]
    fn a_caller_that_drains_dirty_tiles_sees_each_once_and_the_flush_none() {
        let (mut r, d, q) = renderer(100, 70);
        r.apply_event(&d, &q, solid(1, 1, 2)).unwrap();
        r.apply_event(&d, &q, solid(2, 0, 0)).unwrap();
        let mut seen = Vec::new();
        assert!(!r.drain_dirty(|x, y| seen.push((x, y))));
        assert_eq!(seen, vec![(1, 2), (0, 0)]);
        assert!(!r.drain_dirty(|_, _| panic!("drained twice")));
        let report = r.flush(&d, &q, 1).unwrap();
        assert!(report.recorded);
        assert!(report.damage.is_empty(), "the caller already took them");
    }

    #[test]
    fn a_dirty_list_that_is_never_drained_folds_into_all() {
        let mut list = Vec::new();
        let mut all = false;
        for i in 0..MAX_DIRTY_LIST {
            push_dirty(&mut list, &mut all, (i % 256) as u8, 0);
        }
        assert!(!all);
        assert_eq!(list.len(), MAX_DIRTY_LIST);
        push_dirty(&mut list, &mut all, 0, 0);
        assert!(all && list.is_empty());
        push_dirty(&mut list, &mut all, 1, 1);
        assert!(list.is_empty(), "once all is set the list stays empty");
    }

    #[test]
    fn damage_rects_clamp_coalesce_and_drop_tiles_off_the_grid() {
        // 100x70 is 4x3 tiles; the right column is 4 px wide, the bottom row 6 tall.
        let full = damage_rects(true, &[], 100, 70);
        assert_eq!(
            full,
            vec![Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 70
            }]
        );
        let edge = damage_rects(false, &[(3, 2)], 100, 70);
        assert_eq!(
            edge,
            vec![Rect {
                x: 96,
                y: 64,
                w: 4,
                h: 6
            }]
        );
        // Two neighbours are one rectangle; (200, 0) is not on the grid.
        let row = damage_rects(false, &[(0, 0), (1, 0), (200, 0)], 100, 70);
        assert_eq!(
            row,
            vec![Rect {
                x: 0,
                y: 0,
                w: 64,
                h: 32
            }]
        );
        assert!(damage_rects(false, &[(200, 0)], 100, 70).is_empty());
    }

    #[test]
    fn the_debug_structs_serialise_for_the_e2e_hooks() {
        let (mut r, d, q) = renderer(64, 64);
        r.apply_event(&d, &q, solid(4, 1, 1)).unwrap();
        let state = serde_json::to_value(r.debug_state()).unwrap();
        assert_eq!(state["width"], 64);
        assert_eq!(state["pendingSolid"], 1);
        assert_eq!(state["newestTileSeq"], 4);
        assert!(state["newestH264Seq"].is_null());
        assert_eq!(state["cdf53Gpu"], false);
        let tile = serde_json::to_value(r.debug_tile(1, 1)).unwrap();
        assert_eq!(
            (tile["tileX"].as_u64(), tile["tileY"].as_u64()),
            (Some(1), Some(1))
        );
        assert_eq!(tile["newestFrameSeq"], 4);
        assert!(r.debug_tile(5, 5).is_none(), "outside the 2x2 grid");
    }

    /// A [`SlotSink`] that records what it was asked.
    #[derive(Default)]
    struct Recording {
        retired: Vec<u64>,
        encoded: Vec<(u32, u32, u64)>,
    }

    impl SlotSink for Recording {
        fn retire(&mut self, slot: &SlotTile) {
            self.retired.push(slot.token);
        }

        fn encode(
            &mut self,
            _device: &wgpu::Device,
            _queue: &wgpu::Queue,
            _encoder: &mut wgpu::CommandEncoder,
            _fb: &Framebuffer,
            tiles: &[(u32, u32, SlotTile)],
        ) -> bool {
            self.encoded
                .extend(tiles.iter().map(|&(x, y, s)| (x, y, s.token)));
            !tiles.is_empty()
        }
    }

    fn slot(token: u64) -> SlotTile {
        SlotTile {
            slab: 0,
            index: token as u32,
            token,
        }
    }

    #[test]
    fn slot_tiles_are_retired_exactly_once_and_the_later_one_for_a_tile_wins() {
        let (mut r, d, q) = renderer(64, 64);
        r.apply_slot_tile(3, 0, 0, slot(30));
        r.apply_slot_tile(4, 0, 0, slot(40));
        r.apply_slot_tile(5, 1, 1, slot(50));
        let mut pending = Vec::new();
        r.for_each_pending_slot(|s| pending.push(s.token));
        assert_eq!(pending, vec![30, 40, 50]);

        let mut sink = Recording::default();
        let report = r.flush_with(&d, &q, 1, &mut sink).unwrap();
        assert!(report.recorded);
        sink.retired.sort_unstable();
        assert_eq!(sink.retired, vec![30, 40, 50], "every slot, once");
        sink.encoded.sort_unstable();
        assert_eq!(
            sink.encoded,
            vec![(0, 0, 40), (1, 1, 50)],
            "seq 3 lost to seq 4 for the tile"
        );
    }
}
