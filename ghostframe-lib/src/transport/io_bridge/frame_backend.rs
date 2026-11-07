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
//! 2. one of [`FrameBackend::finish_h264_planned`], [`FrameBackend::finish_tile`] or
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
    FrameAnalysis, FramePaletteEntryRaw, GpuFrameProcessor, TileAnalysis, UnchangedRefused,
};
use crate::config::H264BackendChoice;
use crate::encoder::h264::backend::{
    BackendError, BackendFeedback, EncodedAu, H264Backend, Nv12View, RateTarget, StreamConfig,
};
use crate::encoder::h264::openh264::OpenH264Backend;
use crate::encoder::h264::refs::{FramePlan, ResidentRef};
use crate::encoder::h264::tracker::PicKind;
use crate::encoder::h264::vaapi_direct::VaapiDirect;
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
    /// Pixels the backend holds itself: none, for an unchanged tick.
    Owned(Vec<u8>),
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

/// A frame planned by the `RefManager` for a backend that executes plans
/// (design 4.1). Opaque outside the crate.
pub struct H264Request {
    pub(crate) plan: FramePlan,
    /// The encoder-side DPB before this frame: the backend lists all of it.
    pub(crate) refs: Vec<ResidentRef>,
    /// Frame rate the stream is opened with, and the rate target this frame
    /// is coded under: the rate with its frame caps and VBV, made by the
    /// planner so the worker applies exactly what the budget was measured
    /// against (`RateTarget::steady`).
    pub(crate) fps: u32,
    pub(crate) rate: RateTarget,
    /// What the stream's receiver told the reference manager since the last
    /// plan, for a backend that keeps its own long-term bookkeeping: applied
    /// before the frame is encoded, in order.
    pub(crate) feedback: Vec<BackendFeedback>,
    /// Which backends may open, and the openh264 library to load.
    pub(crate) backend: H264BackendChoice,
    pub(crate) openh264_lib: Option<std::sync::Arc<std::path::Path>>,
}

/// An access unit a plan-executing backend made, and the name of the backend
/// that made it (`H264Backend::name`).
pub struct CodedAu(pub(crate) EncodedAu, pub(crate) &'static str);

/// What [`FrameBackend::finish_h264_planned`] came back with.
pub enum H264Reply {
    /// The plan was executed.
    Coded(CodedAu),
    /// No backend can code this stream: libva-direct is not here (or refused
    /// it) and openh264 is not here either, or is outside its limits.
    /// Nothing was encoded for this frame; the caller stops offering H.264.
    /// `process_wide` says whether that is a fact about the machine (every
    /// backend tried is absent: no device, no library; or a backend gave up)
    /// or only about this picture size (a backend refused the configuration or
    /// failed at open: `Incapable`, `Driver`), in which case a later,
    /// different size may still open (see [`unavailable_scope`]).
    Unavailable { process_wide: bool, why: String },
    /// Nothing was encoded: the frame's GPU outputs were gone.
    Nothing,
    /// The backend failed (or could not follow the plan). It is dropped and
    /// reopens on the next IDR plan.
    Failed,
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

/// Why `analyse` produced nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnalyseError {
    /// An unchanged tick the backend cannot honour: the producer's claim, not
    /// a GPU fault. The loop drops the frame quietly and tells the producer.
    Refused(String),
    /// The GPU stage failed; the loop logs it and counts it.
    Failed(String),
}

impl From<String> for AnalyseError {
    fn from(e: String) -> Self {
        Self::Failed(e)
    }
}

impl std::fmt::Display for AnalyseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::Refused(e) | Self::Failed(e)) = self;
        f.write_str(e)
    }
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
    ) -> Result<Analysed, AnalyseError>;

    /// H.264 arm: execute the `RefManager`'s plan. A backend without plan support answers
    /// [`H264Reply::Unavailable`].
    fn finish_h264_planned(&mut self, frame: &FrameSubmission, req: &H264Request) -> H264Reply {
        let _ = (frame, req);
        H264Reply::Unavailable {
            process_wide: true,
            why: "this frame backend executes no plans".into(),
        }
    }

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

/// The first rung of the ladder: libva-direct, when the machine has it.
fn open_direct(cfg: &StreamConfig) -> Result<Box<dyn H264Backend>, BackendError> {
    let mut b = VaapiDirect::new();
    b.open(cfg)?;
    Ok(Box::new(b))
}

fn open_openh264(
    cfg: &StreamConfig,
    lib: Option<&std::path::Path>,
) -> Result<Box<dyn H264Backend>, BackendError> {
    let mut b = OpenH264Backend::new(lib.map(std::path::Path::to_path_buf));
    b.open(cfg)?;
    Ok(Box::new(b))
}

/// Why libva-direct cannot serve this process, once a probe found out: the
/// next stream does not probe again (each IDR would otherwise open and fail
/// the same way). Per process, like the machine it describes.
pub(crate) static DIRECT_PROBE_FAILURE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Open the backend a session runs on: libva-direct, then openh264 for
/// `auto`; one of them alone when the operator chose it. `Err` means neither
/// is available and the caller stops offering H.264 (see
/// [`H264Reply::Unavailable`]): `Unavailable` only if every backend tried
/// said it is not here (the machine); `Incapable` or `Driver` if any of them
/// refused or failed on this stream's configuration (see
/// [`unavailable_scope`]).
///
/// A libva-direct open that fails as *unavailable* (no device, driver or
/// entrypoint) is a probe result about the machine and is remembered in
/// `probe`; later calls go straight to the next backend. *Incapable* is about
/// the stream asked for (a picture under 128x128, a rate-control mode the
/// driver lacks for this configuration), so a later, larger stream can still
/// open: it is not remembered, and neither is a fault at run time.
pub(crate) fn open_backend(
    choice: H264BackendChoice,
    cfg: &StreamConfig,
    direct: impl FnOnce(&StreamConfig) -> Result<Box<dyn H264Backend>, BackendError>,
    openh264: impl FnOnce(&StreamConfig) -> Result<Box<dyn H264Backend>, BackendError>,
    probe: &std::sync::OnceLock<String>,
) -> Result<Box<dyn H264Backend>, BackendError> {
    let try_direct = |cfg: &StreamConfig| -> Result<Box<dyn H264Backend>, BackendError> {
        if let Some(why) = probe.get() {
            return Err(BackendError::Unavailable(why.clone()));
        }
        direct(cfg).inspect_err(|e| {
            if matches!(e, BackendError::Unavailable(_)) {
                let _ = probe.set(e.to_string());
            }
        })
    };
    match choice {
        H264BackendChoice::Vaapi => try_direct(cfg),
        H264BackendChoice::Openh264 => openh264(cfg),
        H264BackendChoice::Auto => match try_direct(cfg) {
            Ok(b) => Ok(b),
            Err(first) => {
                // Said once: afterwards the cached probe makes this the same
                // line on every IDR.
                tracing::info!(
                    target: LOG_TARGET,
                    "libva-direct H.264 unavailable ({first}); trying openh264"
                );
                openh264(cfg).map_err(|second| {
                    let both = format!("{first}; {second}");
                    // "Unavailable" is the machine, and only when every
                    // backend tried said so (no device, no library). If any
                    // of them refused this configuration or failed at open,
                    // that is about this stream: a later, smaller one may
                    // still open.
                    if matches!(first, BackendError::Unavailable(_))
                        && matches!(second, BackendError::Unavailable(_))
                    {
                        BackendError::Unavailable(both)
                    } else {
                        BackendError::Incapable(both)
                    }
                })
            }
        },
    }
}

/// Whether an open failure is a fact about the machine (a latch for the
/// process) rather than about one stream's configuration (picture size, level,
/// frame rate) or a transient driver failure, which a later, different stream
/// may not share. Only `Unavailable` (no device, no library) is the machine.
pub(crate) fn unavailable_scope(e: &BackendError) -> bool {
    matches!(e, BackendError::Unavailable(_))
}

/// What a failed open becomes: "H.264 unavailable", with the scope
/// [`unavailable_scope`] gives it.
pub(crate) fn open_failure_reply(e: &BackendError) -> H264Reply {
    H264Reply::Unavailable {
        process_wide: unavailable_scope(e),
        why: e.to_string(),
    }
}

/// One planned frame through `backend`: the rate, the receiver's feedback in
/// order, then the picture.
pub(crate) fn encode_planned(
    backend: &mut dyn H264Backend,
    view: &Nv12View<'_>,
    req: &H264Request,
) -> Result<EncodedAu, BackendError> {
    backend.set_rate(req.rate);
    for fb in &req.feedback {
        backend.feedback(*fb);
    }
    backend.encode(view, &req.plan, &req.refs)
}

/// A plan-executing backend, the frame size and the choice it was opened for.
type OpenedBackend = (Box<dyn H264Backend>, (u32, u32), H264BackendChoice);

/// The production backend: a Vulkan `GpuFrameProcessor` and, lazily, the
/// H.264 backend the ladder picked.
pub struct GpuBackend {
    processor: GpuFrameProcessor,
    /// The plan-executing backend, the frame size it was opened for and the
    /// choice that opened it (a demotion changes the choice, which reopens at
    /// the next IDR). `None` until the first planned frame.
    direct: Option<OpenedBackend>,
    /// The GPU outputs of the frame in flight, between `analyse` and `finish_*`.
    held: Option<FrameAnalysis>,
}

impl GpuBackend {
    pub fn new(processor: GpuFrameProcessor) -> Self {
        Self {
            processor,
            direct: None,
            held: None,
        }
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
        }
    }

    fn analyse(
        &mut self,
        frame: &FrameSubmission,
        escalation: &[u32],
        want_diag: bool,
    ) -> Result<Analysed, AnalyseError> {
        self.held = None;
        self.processor.set_escalation_candidates(escalation);
        let raw_fd = frame.dmabuf_fd.as_ref().map(|fd| fd.as_raw_fd());
        let result = match raw_fd {
            _ if frame.content == crate::frame_damage::Content::Unchanged => self
                .processor
                .process_frame_unchanged(frame.width, frame.height),
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
        let analysis = result.map_err(|e| match e.downcast_ref::<UnchangedRefused>() {
            Some(r) => AnalyseError::Refused(r.to_string()),
            None => AnalyseError::Failed(e.to_string()),
        })?;
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

    fn finish_h264_planned(&mut self, frame: &FrameSubmission, req: &H264Request) -> H264Reply {
        // The tests of this binary share one GPU: the lock is reentrant on this
        // (the worker) thread, and no test thread may hold it while waiting for
        // the worker.
        #[cfg(test)]
        let _gpu = crate::gpu_test_lock::lock();
        let Some(analysis) = self.held.take() else {
            return H264Reply::Nothing;
        };
        let dims = (analysis.nv12_width, analysis.nv12_height);
        if self
            .direct
            .as_ref()
            .is_none_or(|(_, d, c)| *d != dims || *c != req.backend)
        {
            // A stream starts, or changes size, on an IDR; anything else
            // means the loop's plan outlived the backend it was made for.
            if req.plan.kind != PicKind::Idr {
                self.direct = None;
                return H264Reply::Failed;
            }
            self.direct = None;
            let cfg = StreamConfig {
                rate: req.rate,
                ..StreamConfig::new(dims.0, dims.1, req.fps, req.rate.bps)
            };
            let lib = req.openh264_lib.as_deref();
            match open_backend(
                req.backend,
                &cfg,
                open_direct,
                |c| open_openh264(c, lib),
                &DIRECT_PROBE_FAILURE,
            ) {
                Ok(b) => self.direct = Some((b, dims, req.backend)),
                Err(e) => return open_failure_reply(&e),
            }
        }
        let _ = frame;
        // SAFETY: the pointer and strides are the GPU stage's, valid until the
        // next `analyse`, which cannot run before this call returns; the span
        // is the Y plane's rows plus the UV plane's.
        let data = unsafe {
            std::slice::from_raw_parts(
                analysis.nv12_data,
                analysis.nv12_uv_offset as usize
                    + analysis.nv12_uv_stride as usize * (dims.1 as usize).div_ceil(2),
            )
        };
        let view = Nv12View {
            data,
            width: dims.0,
            height: dims.1,
            y_stride: analysis.nv12_y_stride as usize,
            uv_stride: analysis.nv12_uv_stride as usize,
            uv_offset: analysis.nv12_uv_offset as usize,
        };
        let Some((backend, _, _)) = self.direct.as_mut() else {
            return H264Reply::Failed;
        };
        match encode_planned(backend.as_mut(), &view, req) {
            Ok(au) => H264Reply::Coded(CodedAu(au, backend.name())),
            Err(e) => {
                tracing::warn!(target: LOG_TARGET, "acked-reference H.264 encode failed: {e}");
                self.direct = None;
                H264Reply::Failed
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
        let pixels = if frame.content == crate::frame_damage::Content::Unchanged {
            // Nothing is dirty on an unchanged tick, so nothing reads pixels.
            Ok(PixelSource::Owned(Vec::new()))
        } else if !frame.pixels.is_empty() {
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
                Ok(PixelSource::Owned(v)) => v,
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

#[cfg(test)]
mod backend_choice_tests {
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::encoder::h264::backend::{AuMeta, BackendCaps};
    use crate::encoder::h264::refs::{FramePlan, IdrCause, Intent};

    /// Records the calls a backend gets, in order.
    struct Recorder {
        log: Arc<Mutex<Vec<String>>>,
    }

    impl H264Backend for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }
        fn caps(&self) -> BackendCaps {
            BackendCaps {
                explicit_refs: false,
                max_lt: 2,
                max_ref_frames: 3,
                vbr: true,
            }
        }
        fn open(&mut self, _cfg: &StreamConfig) -> Result<(), BackendError> {
            Ok(())
        }
        fn encode(
            &mut self,
            _nv12: &Nv12View<'_>,
            plan: &FramePlan,
            _refs: &[ResidentRef],
        ) -> Result<EncodedAu, BackendError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("encode {}", plan.frame_seq));
            Ok(EncodedAu {
                payload: vec![0, 0, 0, 1],
                meta: AuMeta {
                    idr: false,
                    header: None,
                    ref_seq: None,
                    params: None,
                },
            })
        }
        fn set_rate(&mut self, rate: RateTarget) {
            self.log
                .lock()
                .unwrap()
                .push(format!("set_rate {}", rate.bps));
        }
        fn feedback(&mut self, fb: BackendFeedback) {
            self.log.lock().unwrap().push(format!("feedback {fb:?}"));
        }
    }

    fn cfg() -> StreamConfig {
        StreamConfig::new(128, 128, 30, 1_000_000)
    }

    fn rec(tag: &'static str, calls: &RefCell<Vec<&'static str>>) -> Box<dyn H264Backend> {
        calls.borrow_mut().push(tag);
        Box::new(Recorder {
            log: Arc::default(),
        })
    }

    fn unavailable(tag: &'static str, calls: &RefCell<Vec<&'static str>>) -> BackendError {
        calls.borrow_mut().push(tag);
        BackendError::Unavailable(format!("{tag} is not here"))
    }

    /// Which openers each choice reaches, and in what order.
    #[test]
    fn the_choice_decides_which_backends_are_tried() {
        use H264BackendChoice::*;
        // (choice, direct opens, openh264 opens) -> openers called, success
        let cases: [(H264BackendChoice, bool, bool, &[&str], bool); 7] = [
            (Auto, true, true, &["direct"], true),
            (Auto, false, true, &["direct", "openh264"], true),
            (Auto, false, false, &["direct", "openh264"], false),
            (Vaapi, true, true, &["direct"], true),
            (Vaapi, false, true, &["direct"], false),
            (Openh264, true, true, &["openh264"], true),
            (Openh264, true, false, &["openh264"], false),
        ];
        for (choice, direct_ok, oh_ok, want_calls, want_ok) in cases {
            let calls = RefCell::new(Vec::new());
            let got = open_backend(
                choice,
                &cfg(),
                |_| {
                    if direct_ok {
                        Ok(rec("direct", &calls))
                    } else {
                        Err(unavailable("direct", &calls))
                    }
                },
                |_| {
                    if oh_ok {
                        Ok(rec("openh264", &calls))
                    } else {
                        Err(unavailable("openh264", &calls))
                    }
                },
                &std::sync::OnceLock::new(),
            );
            assert_eq!(
                *calls.borrow(),
                want_calls,
                "{choice:?} direct={direct_ok} openh264={oh_ok}"
            );
            assert_eq!(got.is_ok(), want_ok, "{choice:?} {direct_ok} {oh_ok}");
        }
    }

    /// `auto` is libva-direct, then openh264, and only then "H.264
    /// unavailable".
    #[test]
    fn openh264_is_routed_into_the_planned_path_when_libva_direct_is_not_there() {
        let calls = RefCell::new(Vec::new());
        let r = open_backend(
            H264BackendChoice::Auto,
            &cfg(),
            |_| Err(unavailable("direct", &calls)),
            |_| Ok(rec("openh264", &calls)),
            &std::sync::OnceLock::new(),
        );
        assert!(r.is_ok(), "{:?}", r.err());
        assert_eq!(*calls.borrow(), ["direct", "openh264"]);
        // With neither, the caller gets an Err naming both.
        let calls = RefCell::new(Vec::new());
        let r = open_backend(
            H264BackendChoice::Auto,
            &cfg(),
            |_| Err(unavailable("direct", &calls)),
            |_| Err(unavailable("openh264", &calls)),
            &std::sync::OnceLock::new(),
        );
        let Err(BackendError::Unavailable(why)) = r else {
            panic!("neither opens: {r:?}", r = r.is_ok());
        };
        assert!(why.contains("direct") && why.contains("openh264"), "{why}");
    }

    /// The scope of "unavailable": process-wide only when every backend tried
    /// said it is not here (no library, no device). A backend that refused
    /// this configuration or failed at open makes it a per-size answer, so a
    /// later, smaller stream can still open.
    #[test]
    fn the_scope_of_unavailable_is_the_machine_only_when_every_backend_is_absent() {
        let scope = |choice, direct: BackendError, oh: BackendError| {
            let r = open_backend(
                choice,
                &cfg(),
                |_| Err(direct),
                |_| Err(oh),
                &std::sync::OnceLock::new(),
            );
            unavailable_scope(&r.err().expect("neither opens"))
        };
        let gone = || BackendError::Unavailable("not here".into());
        let big = || BackendError::Incapable("above 1920x1080".into());
        let driver = || BackendError::Driver {
            call: "vaCreateContext",
            detail: "refused".into(),
        };
        use H264BackendChoice::*;
        assert!(scope(Auto, gone(), gone()), "no library: the machine");
        assert!(!scope(Auto, gone(), big()), "the ceiling: this size only");
        assert!(!scope(Auto, big(), big()));
        assert!(
            !scope(Auto, big(), gone()),
            "VA-API refused this size, no libopenh264: this size only"
        );
        assert!(
            !scope(Auto, driver(), gone()),
            "a driver failure at open is not the machine"
        );
        assert!(!scope(Auto, gone(), driver()));
        assert!(scope(Openh264, gone(), gone()));
        assert!(!scope(Openh264, gone(), big()));
        assert!(
            scope(Vaapi, gone(), big()),
            "libva-direct alone is not there"
        );
        assert!(!scope(Vaapi, big(), gone()));
        assert!(!scope(Vaapi, driver(), gone()));
    }

    /// VA-API present, no libopenh264: libva-direct refuses a dual-4K stream,
    /// which is that stream's answer, and the 1080p stream after it opens.
    #[test]
    fn a_refused_4k_stream_without_openh264_does_not_shut_1080p_out() {
        let probe = std::sync::OnceLock::new();
        let uhd = StreamConfig::new(7680, 2160, 30, 8_000_000);
        let r = open_backend(
            H264BackendChoice::Auto,
            &uhd,
            |_| Err(BackendError::Incapable("above level 5.2".into())),
            |_| Err(BackendError::Unavailable("no libopenh264".into())),
            &probe,
        );
        let e = r.err().expect("4K is refused");
        assert!(!unavailable_scope(&e), "{e}");
        assert!(probe.get().is_none(), "Incapable is not remembered");
        let calls = RefCell::new(Vec::new());
        let hd = StreamConfig::new(1920, 1080, 30, 4_000_000);
        let r = open_backend(
            H264BackendChoice::Auto,
            &hd,
            |_| Ok(rec("direct", &calls)),
            |_| Err(BackendError::Unavailable("no libopenh264".into())),
            &probe,
        );
        assert!(r.is_ok());
        assert_eq!(*calls.borrow(), ["direct"]);
    }

    /// `GpuBackend` turns an open failure into its reply through
    /// `open_failure_reply`: nothing but "not here" is a latch.
    #[test]
    fn only_an_absent_backend_is_a_process_wide_open_failure() {
        let process_wide = |e: BackendError| match open_failure_reply(&e) {
            H264Reply::Unavailable { process_wide, .. } => process_wide,
            _ => panic!("an open failure is an Unavailable reply"),
        };
        assert!(process_wide(BackendError::Unavailable("x".into())));
        assert!(!process_wide(BackendError::Incapable("x".into())));
        assert!(!process_wide(BackendError::Driver {
            call: "x",
            detail: "y".into()
        }));
    }

    /// A libva-direct probe that failed is not run again on the next IDR (the
    /// cached result is per process); a fault at run time is not a probe
    /// result; openh264 alone never probes it.
    #[test]
    fn a_failed_libva_direct_probe_is_not_repeated() {
        let probe = std::sync::OnceLock::new();
        let calls = RefCell::new(Vec::new());
        for round in 0..3 {
            let r = open_backend(
                H264BackendChoice::Auto,
                &cfg(),
                |_| Err(unavailable("direct", &calls)),
                |_| Ok(rec("openh264", &calls)),
                &probe,
            );
            assert!(r.is_ok(), "round {round}");
        }
        assert_eq!(
            *calls.borrow(),
            ["direct", "openh264", "openh264", "openh264"],
            "the probe ran once; openh264 served every IDR"
        );
        assert!(probe
            .get()
            .is_some_and(|w| w.contains("direct is not here")));
        // `vaapi` alone also reads the cache.
        let calls2 = RefCell::new(Vec::new());
        let r = open_backend(
            H264BackendChoice::Vaapi,
            &cfg(),
            |_| Ok(rec("direct", &calls2)),
            |_| Ok(rec("openh264", &calls2)),
            &probe,
        );
        assert!(r.is_err() && calls2.borrow().is_empty());
        // A driver fault is not the machine's answer.
        let fresh = std::sync::OnceLock::new();
        let _ = open_backend(
            H264BackendChoice::Vaapi,
            &cfg(),
            |_| {
                Err(BackendError::Driver {
                    call: "x",
                    detail: "y".into(),
                })
            },
            |_| unreachable!(),
            &fresh,
        );
        assert!(fresh.get().is_none());
    }

    /// `Incapable` is a statement about one configuration: a small first
    /// stream must not turn libva-direct off for the process.
    #[test]
    fn an_incapable_libva_direct_open_does_not_disable_it_for_later_streams() {
        let probe = std::sync::OnceLock::new();
        let calls = RefCell::new(Vec::new());
        let small = StreamConfig::new(64, 64, 30, 1_000_000);
        let r = open_backend(
            H264BackendChoice::Auto,
            &small,
            |c| {
                assert_eq!((c.width, c.height), (64, 64));
                calls.borrow_mut().push("direct");
                Err(BackendError::Incapable("frame smaller than 128x128".into()))
            },
            |_| Ok(rec("openh264", &calls)),
            &probe,
        );
        assert!(r.is_ok());
        assert!(probe.get().is_none(), "{:?}", probe.get());
        // The next stream is 720p and libva-direct takes it.
        let big = StreamConfig::new(1280, 720, 30, 1_000_000);
        let r = open_backend(
            H264BackendChoice::Auto,
            &big,
            |_| Ok(rec("direct", &calls)),
            |_| unreachable!("libva-direct opens"),
            &probe,
        );
        assert!(r.is_ok());
        assert_eq!(*calls.borrow(), ["direct", "openh264", "direct"]);
    }

    /// The rate, then the receiver's feedback in order, then the picture.
    #[test]
    fn feedback_reaches_the_backend_before_the_frame_is_encoded() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut b = Recorder { log: log.clone() };
        let plan = FramePlan {
            epoch: 0,
            frame_seq: 7,
            kind: PicKind::Healthy,
            idr_cause: None::<IdrCause>,
            nal_ref_idc: 3,
            frame_num: 1,
            poc: 2,
            target: None,
            intent: Intent::Executed,
            mark: None,
            mark_target: false,
            mmco: Vec::new(),
            rplm: Vec::new(),
        };
        let marked = BackendFeedback::LtrMarked {
            idr_pic_id: 1,
            frame_num: 4,
        };
        let recover = BackendFeedback::LtrRecover {
            idr_pic_id: 1,
            last_correct_frame_num: 5,
            current_frame_num: 9,
        };
        let req = H264Request {
            plan,
            refs: Vec::new(),
            fps: 30,
            rate: RateTarget::steady(1_000_000, 30),
            feedback: vec![marked, recover],
            backend: H264BackendChoice::Auto,
            openh264_lib: None,
        };
        let data = vec![0u8; 128 * 128 * 3 / 2];
        let view = Nv12View {
            data: &data,
            width: 128,
            height: 128,
            y_stride: 128,
            uv_stride: 128,
            uv_offset: 128 * 128,
        };
        encode_planned(&mut b, &view, &req).unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "set_rate 1000000".to_string(),
                format!("feedback {marked:?}"),
                format!("feedback {recover:?}"),
                "encode 7".to_string(),
            ]
        );
    }
}
