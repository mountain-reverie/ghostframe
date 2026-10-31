//! The seam between the transport event loop and the GPU and encoder work of a
//! frame (WS-C2, PS-2).
//!
//! A captured frame needs a GPU analysis (dirty tiles, per-tile colour data,
//! NV12), then either a hardware H.264 encode or the lossless tile path (a
//! readback, PalRle inputs, CDF 5/3 coefficients to turn into passes). None of
//! that touches transport state: it reads the GPU's buffers and the frame, and
//! produces owned results. Everything that does touch transport state (the
//! classifier, metrics, scheduler, palette table, the `now_std()` stamps) stays
//! on the loop and consumes those results.
//!
//! `FrameBackend` is that boundary. A frame is two calls, in order:
//!
//! 1. [`FrameBackend::analyse`]: GPU submit and fence wait. Returns an owned
//!    [`Analysed`]; the backend keeps the raw GPU outputs for step 2.
//! 2. one of [`FrameBackend::finish_h264`], [`FrameBackend::finish_tile`] or
//!    [`FrameBackend::finish_discard`], chosen by the loop once it has
//!    classified the frame. This releases the GPU outputs.
//!
//! The loop decides between H.264 and the tile path after step 1, from
//! classifier state it owns, so the work cannot be one call.
//!
//! Today the loop calls both steps itself, back to back, which is exactly what
//! it did before the seam existed. The point of the seam is that the backend
//! owns its GPU processor and encoder and hands out only owned data, so it can
//! later run on its own thread without the loop's state following it. The GPU
//! outputs (`FrameAnalysis`, the processor's coefficient pointers) are valid
//! only until the next `analyse`, which is why they never cross the seam.
//!
//! A GPU call that must run between a frame's fence and its tile commit
//! (`forward_cdf53_tiles`, for tiles the tile encoders cannot carry) is one
//! more method here, not a reach into the processor.
//!
//! The backend must stamp nothing protocol-visible: no `now_std()`. Time on the
//! loop is tokio's clock; a worker thread would read a different one under the
//! paused browserless harness.

use std::os::unix::io::AsRawFd;

use crate::capture::dmabuf::ReadbackFrame;
use crate::capture::gpu_pipeline::{
    FrameAnalysis, FramePaletteEntryRaw, GpuFrameProcessor, TileAnalysis,
};
use crate::encoder::h264_vaapi::FullFrameEncoder;
use crate::server::FrameSubmission;

use super::frame::{clamp_gpu_tile_count, palrle_index_slice};

const LOG_TARGET: &str = "ghostframe_lib::transport::io_bridge";

/// Something the loop asks the backend to do to its hardware state, outside a
/// frame. Applied in the order issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Drop the GPU's previous-frame baseline and report the next `n` frames
    /// as fully dirty.
    InvalidateBaseline(u32),
    /// The next H.264 frame is an IDR.
    RequestKeyframe,
    /// A frame that named only some tile rows was dropped before the GPU
    /// stage saw it: upload and compare the next frame whole.
    ForgetDamage,
}

/// The GPU stage's outputs, owned.
#[derive(Debug, Default)]
pub struct Analysed {
    /// Flat indices of tiles that changed since the previous frame.
    pub dirty_tiles: Vec<u32>,
    /// The per-tile analysis of each tile in `dirty_tiles`, same order. `None`
    /// where the GPU's analysis buffer has no entry for that index (the first
    /// frame, before it exists).
    pub dirty_analysis: Vec<Option<TileAnalysis>>,
    pub palrle_compact_count: u32,
    pub frame_palette_set_count: u32,
    /// Filled only when asked for (`analyse`'s `want_diag`).
    pub diag: Option<GpuDiag>,
}

/// What the `diagnose_gpu_pipeline` log line prints about the GPU buffers.
#[derive(Debug, Clone)]
pub struct GpuDiag {
    pub tile_analysis_is_null: bool,
    pub tile_analysis_len: u32,
    pub tile_analysis_slice_len: usize,
    /// `(index, count, edge_density_thou)` of the first non-empty entry among
    /// the first 2048.
    pub first_nonzero: Option<(usize, u32, u32)>,
}

/// One CDF 5/3 tile's passes, encoded.
#[derive(Debug)]
pub struct Cdf53Encoded {
    pub present_passes: u16,
    pub passes: Vec<(u8, Vec<u8>)>,
}

/// A dirty tile from the GPU's CDF 5/3 compact list, encoded.
#[derive(Debug)]
pub struct Cdf53Tile {
    pub tile_x: u8,
    pub tile_y: u8,
    pub encoded: Cdf53Encoded,
    /// The tile's coefficients, for the `cdf53-diag` GPU-versus-CPU diff.
    #[cfg(feature = "cdf53-diag")]
    pub coeffs_i16: Vec<i16>,
}

/// One escalation candidate, as the loop saw it when it asked for the frame.
#[derive(Debug, Clone, Copy)]
pub struct EscalationSlot {
    /// The tile's flat index, as given to `analyse`.
    pub tile_idx: u32,
    /// A stranded-recovery slot: always encoded. The others are H.264
    /// escalations, skipped when the tile is already CDF 5/3 this frame.
    pub stranded: bool,
    /// The tile's codec state was already CDF 5/3 when the loop asked.
    pub tile_is_cdf53: bool,
}

/// What the tile path needs from the backend for one frame.
#[derive(Debug)]
pub struct TileRequest<'a> {
    pub cols: u32,
    pub rows: u32,
    /// Same order and length as the candidates handed to `analyse`.
    pub escalation: &'a [EscalationSlot],
    /// Dirty tiles that no tile encoder will carry (their codec state is
    /// Skip, H264 or PixelPerfect), which the loop will send as CDF 5/3. The
    /// backend forward-transforms and encodes them here, in the same call,
    /// so the loop does not have to wait for the GPU when it commits.
    pub fallback: &'a [(u32, u32)],
}

/// Where the tile path reads pixels from.
pub enum PixelSource {
    /// `frame.pixels` (the CPU-pixel capture path).
    Frame,
    /// A read-back of the frame's DMA-BUF.
    Readback(ReadbackFrame),
}

/// Real-clock nanoseconds the tile stage spent, for the `frame.tile_stages`
/// trace. Zero unless the trace is on.
#[derive(Debug, Default, Clone, Copy)]
pub struct TileTimings {
    pub coef_read_ns: u64,
    pub coef_encode_ns: u64,
}

/// The PalRle inputs the GPU wrote, copied out, with the same clamps the
/// loop applied when it read them in place.
#[derive(Debug, Default)]
pub struct OwnedPalRle {
    pub compact_list: Vec<u32>,
    pub palette_ids: Vec<u8>,
    pub folded_into: Vec<u32>,
    pub palette_set: Vec<FramePaletteEntryRaw>,
    pub index: Vec<u8>,
}

/// Everything the tile path takes from the GPU and the frame.
pub struct TileOut {
    /// `Err` carries the warning text: the frame is skipped.
    pub pixels: Result<PixelSource, String>,
    pub palrle: OwnedPalRle,
    /// In compact-list order, tiles off the grid dropped.
    pub cdf53_dirty: Vec<Cdf53Tile>,
    /// Same length as `TileRequest::escalation`; `None` where a slot was skipped.
    pub cdf53_escalation: Vec<Option<Cdf53Encoded>>,
    /// `TileRequest::fallback` tiles, encoded. A tile whose forward failed is
    /// simply absent; the loop then asks for it again when it commits.
    pub cdf53_fallback: Vec<((u32, u32), Cdf53Encoded)>,
    pub timings: TileTimings,
}

/// An encoded H.264 frame.
#[derive(Clone)]
pub struct H264Out {
    pub payload: Vec<u8>,
    pub is_keyframe: bool,
}

/// The `fallback` tiles the GPU's own CDF 5/3 compact list does not already
/// cover. `in_compact_list` holds the flat indices of that list.
pub(super) fn fallback_not_in_compact_list(
    fallback: &[(u32, u32)],
    cols: u32,
    in_compact_list: &std::collections::HashSet<u32>,
) -> Vec<(u32, u32)> {
    fallback
        .iter()
        .copied()
        .filter(|&(x, y)| !in_compact_list.contains(&(y * cols + x)))
        .collect()
}

/// See the module docs.
pub trait FrameBackend: Send {
    /// Apply a control. Controls are applied in the order the loop issued them.
    fn apply(&mut self, control: Control);

    /// GPU stage. `escalation` is the flat tile indices to forward for
    /// escalation in the same command buffer. Errors carry the message the
    /// loop logs before falling back to the CPU path.
    fn analyse(
        &mut self,
        frame: &FrameSubmission,
        escalation: &[u32],
        want_diag: bool,
    ) -> Result<Analysed, String>;

    /// H.264 arm. Creates or resizes the encoder as needed. `None` when there
    /// is nothing to send (encoder unavailable, failed, or buffering).
    fn finish_h264(&mut self, frame: &FrameSubmission, force_keyframe: bool) -> Option<H264Out>;

    /// Tile arm.
    fn finish_tile(&mut self, frame: &FrameSubmission, req: &TileRequest<'_>) -> TileOut;

    /// The loop will not use this frame's outputs.
    fn finish_discard(&mut self);

    /// CDF 5/3 forward transform of the given 32x32 BGRA tiles (4096 bytes
    /// each), `CDF53_TOTAL_COEFFS` coefficients per tile, in order. For tiles
    /// the frame's own compact pass did not list and no tile encoder can
    /// carry (see `dispatch_dirty_tiles_via_scheduler`). Independent of any
    /// frame in flight: it reads the tiles it is given, not the GPU's
    /// outputs. One GPU round trip per 512 tiles.
    fn forward_cdf53_tiles(&mut self, tiles: &[&[u8]]) -> Result<Vec<i16>, String>;
}

/// The production backend: a Vulkan `GpuFrameProcessor` and, lazily, a
/// VA-API (or software) `FullFrameEncoder`.
pub struct GpuBackend {
    processor: GpuFrameProcessor,
    encoder: Option<FullFrameEncoder>,
    /// The GPU outputs of the frame in flight, between `analyse` and `finish_*`.
    held: Option<FrameAnalysis>,
}

impl GpuBackend {
    pub fn new(processor: GpuFrameProcessor) -> Self {
        Self {
            processor,
            encoder: None,
            held: None,
        }
    }

    /// Test seam: has the encoder been asked for a keyframe and not yet made one?
    #[cfg(test)]
    pub(crate) fn encoder_keyframe_pending(&self) -> Option<bool> {
        self.encoder.as_ref().map(|e| e.keyframe_pending())
    }

    #[cfg(test)]
    pub(crate) fn set_encoder_for_test(&mut self, enc: FullFrameEncoder) {
        self.encoder = Some(enc);
    }
}

impl FrameBackend for GpuBackend {
    fn forward_cdf53_tiles(&mut self, tiles: &[&[u8]]) -> Result<Vec<i16>, String> {
        debug_assert!(
            self.held.is_none(),
            "forward_cdf53_tiles during a frame in flight"
        );
        self.processor
            .forward_cdf53_tiles(tiles)
            .map_err(|e| e.to_string())
    }

    fn apply(&mut self, control: Control) {
        match control {
            Control::InvalidateBaseline(n) => self.processor.invalidate_baseline(n),
            Control::ForgetDamage => self.processor.forget_damage(),
            Control::RequestKeyframe => {
                if let Some(enc) = self.encoder.as_mut() {
                    enc.request_keyframe();
                }
            }
        }
    }

    fn analyse(
        &mut self,
        frame: &FrameSubmission,
        escalation: &[u32],
        want_diag: bool,
    ) -> Result<Analysed, String> {
        self.held = None;
        self.processor.set_escalation_candidates(escalation);
        let raw_fd = frame.dmabuf_fd.as_ref().map(|fd| fd.as_raw_fd());
        let result = match raw_fd {
            Some(fd) => self
                .processor
                .process_frame(fd, frame.width, frame.height, frame.stride),
            None => self.processor.process_frame_from_pixels_damaged(
                &frame.pixels,
                frame.width,
                frame.height,
                frame.stride,
                &frame.damage,
            ),
        };
        let analysis = result.map_err(|e| e.to_string())?;

        let ta = analysis.tile_analysis_slice();
        let dirty_analysis = analysis
            .dirty_tiles
            .iter()
            .map(|&idx| ta.get(idx as usize).copied())
            .collect();
        let diag = want_diag.then(|| GpuDiag {
            tile_analysis_is_null: analysis.tile_analysis.is_null(),
            tile_analysis_len: analysis.tile_analysis_len,
            tile_analysis_slice_len: ta.len(),
            first_nonzero: ta
                .iter()
                .take(2048)
                .enumerate()
                .find(|(_, t)| t.count != 0)
                .map(|(i, t)| (i, t.count, t.edge_density_thou)),
        });
        let out = Analysed {
            dirty_tiles: analysis.dirty_tiles.clone(),
            dirty_analysis,
            palrle_compact_count: analysis.palrle_compact_count,
            frame_palette_set_count: analysis.frame_palette_set_count,
            diag,
        };
        self.held = Some(analysis);
        Ok(out)
    }

    fn finish_h264(&mut self, frame: &FrameSubmission, force_keyframe: bool) -> Option<H264Out> {
        let analysis = self.held.take()?;
        let needs_init = match &self.encoder {
            Some(enc) => enc.width() != frame.width || enc.height() != frame.height,
            None => true,
        };
        if needs_init {
            match FullFrameEncoder::new(frame.width, frame.height) {
                Ok(enc) => self.encoder = Some(enc),
                Err(e) => {
                    tracing::warn!(target: LOG_TARGET, "Full-frame encoder init failed: {e}");
                    return None;
                }
            }
        }

        // Re-entry into H264: force IDR for a fresh client anchor.
        if force_keyframe {
            if let Some(enc) = self.encoder.as_mut() {
                enc.request_keyframe();
            }
        }

        // Encode from the GPU-computed NV12 HOST_VISIBLE buffer.
        let encoder = self.encoder.as_mut()?;
        match encoder.encode_nv12_buffer(
            analysis.nv12_data,
            analysis.nv12_width,
            analysis.nv12_height,
            analysis.nv12_y_stride,
            analysis.nv12_uv_stride,
            analysis.nv12_uv_offset,
        ) {
            Ok(Some(enc)) => Some(H264Out {
                payload: enc.payload,
                is_keyframe: enc.is_keyframe,
            }),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(target: LOG_TARGET, "NV12 encode failed: {e}");
                None
            }
        }
    }

    fn finish_tile(&mut self, frame: &FrameSubmission, req: &TileRequest<'_>) -> TileOut {
        let analysis = self.held.take();
        let empty = |pixels| TileOut {
            pixels,
            palrle: OwnedPalRle::default(),
            cdf53_dirty: Vec::new(),
            cdf53_escalation: Vec::new(),
            cdf53_fallback: Vec::new(),
            timings: TileTimings::default(),
        };

        // Source for tile pixel extraction. The CPU path populates
        // `frame.pixels`; the GPU/DMA-BUF path leaves it empty.
        let pixels = if !frame.pixels.is_empty() {
            Ok(PixelSource::Frame)
        } else {
            match frame.dmabuf_fd.as_ref() {
                Some(fd) => match crate::capture::dmabuf::readback_dmabuf(
                    fd.as_raw_fd(),
                    frame.width,
                    frame.height,
                    frame.stride,
                ) {
                    Ok(p) => Ok(PixelSource::Readback(p)),
                    Err(e) => Err(format!(
                        "DMA-BUF readback failed in TileCodec mode: {e}; \
                         skipping frame to avoid emitting zero-filled tiles"
                    )),
                },
                None => Err(
                    "TileCodec mode but no pixels and no DMA-BUF fd; skipping frame".to_string(),
                ),
            }
        };
        if pixels.is_err() {
            return empty(pixels);
        }
        let Some(analysis) = analysis else {
            return empty(pixels);
        };

        let tiles = (req.cols * req.rows) as usize;
        let cols = req.cols;
        let trace_on = super::frame::stage_trace_enabled();
        let mut timings = TileTimings::default();
        let mut lap = super::frame::Lap::new(trace_on);

        // PalRle inputs, clamped as the in-place reader clamped them.
        let count = clamp_gpu_tile_count(analysis.palrle_compact_count, tiles);
        let compact = analysis.palrle_compact_list_slice();
        let ids = analysis.per_tile_frame_palette_id_slice();
        let palrle = OwnedPalRle {
            compact_list: compact[..count.min(compact.len())].to_vec(),
            palette_ids: ids[..count.min(ids.len())].to_vec(),
            folded_into: analysis.folded_into_slice().to_vec(),
            palette_set: analysis.frame_palette_set_slice().to_vec(),
            index: palrle_index_slice(&analysis, tiles).to_vec(),
        };

        // Dirty CDF 5/3 tiles: the GPU compact list, read from the
        // coefficient buffer by compact slot.
        let mut cdf53_dirty = Vec::new();
        let mut in_dirty_list = std::collections::HashSet::new();
        // The count is written by the GPU; clamp it to the list's capacity
        // (one slot per tile) before it sizes a slice.
        // SAFETY: the pointers are HOST_VISIBLE | HOST_COHERENT mapped GPU
        // memory, valid until the next `analyse`, which cannot run before this
        // call returns.
        let cdf53_count =
            clamp_gpu_tile_count(unsafe { *self.processor.cdf53_compact_count_ptr }, tiles);
        if cdf53_count > 0 {
            let list = unsafe {
                std::slice::from_raw_parts(self.processor.cdf53_compact_list_ptr, cdf53_count)
            };
            for (slot, &flat_tile_idx) in list.iter().enumerate() {
                // Also GPU-written: an index off the grid would wrap in the u8
                // casts below and address the wrong tile.
                if flat_tile_idx as usize >= tiles {
                    continue;
                }
                let coeffs_i32 = unsafe {
                    std::slice::from_raw_parts(
                        self.processor
                            .cdf53_coefficients_ptr
                            .add(slot * crate::encoder::cdf53::CDF53_TOTAL_COEFFS),
                        crate::encoder::cdf53::CDF53_TOTAL_COEFFS,
                    )
                };
                lap.lap_ns();
                // The GPU writes i32-wide values; the coefficients are i16
                // (high bits are sign extension).
                let coeffs_i16: Vec<i16> = coeffs_i32.iter().map(|&v| v as i16).collect();
                timings.coef_read_ns += lap.lap_ns();
                let (present_passes, passes) =
                    crate::encoder::cdf53::encode_passes_sparse(&coeffs_i16);
                timings.coef_encode_ns += lap.lap_ns();
                in_dirty_list.insert(flat_tile_idx);
                cdf53_dirty.push(Cdf53Tile {
                    tile_x: (flat_tile_idx % cols) as u8,
                    tile_y: (flat_tile_idx / cols) as u8,
                    encoded: Cdf53Encoded {
                        present_passes,
                        passes,
                    },
                    #[cfg(feature = "cdf53-diag")]
                    coeffs_i16,
                });
            }
        }

        // Escalation slots: stranded ones always; H.264 escalations unless the
        // tile is (or just became) CDF 5/3 this frame, since re-firing would
        // bump again, supersede the passes just enqueued and encode the same
        // coefficients twice. A tile in the dirty list becomes CDF 5/3 when the
        // loop commits it, which is why membership counts as "already".
        let mut cdf53_escalation = Vec::with_capacity(req.escalation.len());
        if !req.escalation.is_empty() {
            let gpu_coeffs_ptr = self.processor.cdf53_escalation_coefficients_ptr;
            for (slot, s) in req.escalation.iter().enumerate() {
                if !s.stranded && (s.tile_is_cdf53 || in_dirty_list.contains(&s.tile_idx)) {
                    cdf53_escalation.push(None);
                    continue;
                }
                let coeffs_i32 = unsafe {
                    std::slice::from_raw_parts(
                        gpu_coeffs_ptr.add(slot * crate::encoder::cdf53::CDF53_TOTAL_COEFFS),
                        crate::encoder::cdf53::CDF53_TOTAL_COEFFS,
                    )
                };
                lap.lap_ns();
                let coeffs_i16: Vec<i16> = coeffs_i32.iter().map(|&v| v as i16).collect();
                timings.coef_read_ns += lap.lap_ns();
                let (present_passes, passes) =
                    crate::encoder::cdf53::encode_passes_sparse(&coeffs_i16);
                timings.coef_encode_ns += lap.lap_ns();
                cdf53_escalation.push(Some(Cdf53Encoded {
                    present_passes,
                    passes,
                }));
            }
        }

        // Tiles no tile encoder will carry: forward them on the GPU in one
        // round trip and encode their passes, here, off the loop.
        let mut cdf53_fallback = Vec::new();
        // Tiles already in the GPU's compact list are encoded above and become
        // CDF 5/3 when the loop commits them; transforming them again would be
        // a wasted full-screen GPU round trip after an H.264 exit.
        let fallback = fallback_not_in_compact_list(req.fallback, req.cols, &in_dirty_list);
        if !fallback.is_empty() {
            let source_pixels: &[u8] = match &pixels {
                Ok(PixelSource::Frame) => &frame.pixels,
                Ok(PixelSource::Readback(rb)) => rb,
                Err(_) => &[],
            };
            let grid = crate::tile::TileGrid::new(frame.width, frame.height);
            let tiles: Vec<Vec<u8>> = fallback
                .iter()
                .map(|&(x, y)| grid.extract_tile(source_pixels, frame.stride, x, y))
                .collect();
            let refs: Vec<&[u8]> = tiles.iter().map(|t| t.as_slice()).collect();
            lap.lap_ns();
            match self.processor.forward_cdf53_tiles(&refs) {
                Ok(coeffs) => {
                    let n = crate::encoder::cdf53::CDF53_TOTAL_COEFFS;
                    for (i, &xy) in fallback.iter().enumerate() {
                        let (present_passes, passes) = crate::encoder::cdf53::encode_passes_sparse(
                            &coeffs[i * n..(i + 1) * n],
                        );
                        cdf53_fallback.push((
                            xy,
                            Cdf53Encoded {
                                present_passes,
                                passes,
                            },
                        ));
                    }
                }
                Err(e) => {
                    tracing::warn!(target: LOG_TARGET, error = %e, tiles = fallback.len(),
                        "cdf53 fallback forward failed in the frame's finish; the commit retries");
                }
            }
            timings.coef_encode_ns += lap.lap_ns();
        }

        TileOut {
            pixels,
            palrle,
            cdf53_dirty,
            cdf53_escalation,
            cdf53_fallback,
            timings,
        }
    }

    fn finish_discard(&mut self) {
        self.held = None;
    }
}
