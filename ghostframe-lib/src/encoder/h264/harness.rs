//! The H.264 source of the browserless harness (design T3): real, decodable
//! PCM-patch streams in place of the encoder, behind the transport's real
//! send path.
//!
//! Two sources, one truth log.
//!
//! * [`PcmMode::RefManager`] is production's acked-reference path with a
//!   [`PcmBackend`] for an encoder: the bridge's `RefManager` plans every
//!   frame, the backend writes the plan with the real slice-header writer, and
//!   the bridge feeds the manager what the transport sees (fragment
//!   acknowledgements, loss verdicts) and what the client reports (its own
//!   0x0A datagrams and 0x09 reset requests), exactly as it does for the
//!   libva-direct backend. Only the thread hop to the encode worker is
//!   missing: the stepped harness cannot run one.
//!
//! * [`PcmMode::OpenH264`] is the same manager over the real openh264 library
//!   in observed mode ([`Openh264Source`]): the library's own reference
//!   structure, its own rate control, lossy pictures.
//!
//! Whatever the source, [`PcmLog`] records, for each frame sent, the picture a
//! client that decoded the stream the encoder meant would show. The client
//! side compares what libavcodec really produced against it: a difference is
//! a wrong picture displayed.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub use super::pcm_decoder::{Decoded, FeedError, PcmDecoder, Strictness};
pub use super::pcm_rate::{PcmModelStats, PcmRateModel};
pub use super::pcm_writer::{
    canvas_cell, canvas_picture, marker_cell, read_lossy, LossyRead, Picture,
};

use super::backend::{
    AuMeta, BackendCaps, BackendError, BackendFeedback, EncodedAu, H264Backend, Nv12View,
    RateTarget, StreamConfig,
};
use super::pcm_rate::RateState;
use super::pcm_writer::{access_unit, canvas_nv12, parameter_sets, pcm_params, FRAME_BYTES};
use super::refs::{FramePlan, ResidentRef};
use super::syntax::StreamParams;
use super::tracker::PicKind;

/// Bytes of a decoded I420 frame of the PCM writer's pictures. A function, not
/// a constant: a `pub const` of this crate lands in the generated C header.
pub fn frame_bytes() -> usize {
    FRAME_BYTES
}

/// Where a scene's H.264 frames come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmMode {
    /// The `RefManager`'s plans.
    RefManager,
    /// The real openh264 library behind the observed-mode manager: each frame's
    /// canvas ([`canvas_nv12`]) is encoded inline, as the encode worker does,
    /// and the client's picture is read back lossily ([`read_lossy`]). Every
    /// `scene_change_every` frames (0: never) the whole canvas changes; with
    /// `churn_every` not 0 the texture around each cell is also repainted
    /// every `churn_every` frames, staggered by cell (a steady load).
    /// `stale_every` (0: never) is a test of the measurement: every
    /// `stale_every`-th frame (from the 7th) is *painted* from the previous
    /// frame's canvas while the truth log keeps the right one, so a source
    /// that shows a frame late is a wrong picture and a reader that cannot
    /// tell must fail the scene that expects one.
    OpenH264 {
        scene_change_every: u32,
        churn_every: u32,
        stale_every: u32,
    },
}

/// What the encoder meant a frame to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruthEntry {
    pub idr: bool,
    /// The frame the picture was predicted from; `None` for an IDR.
    pub ref_seq: Option<u32>,
    /// The picture a decoder that had every frame it needed would show.
    pub picture: Picture,
    pub non_ref: bool,
}

/// An access unit the encoder made: the frame's timestamp (scene time, from the
/// scene's cadence), its coded size and whether it was an IDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuRecord {
    pub timestamp_us: u32,
    pub bytes: usize,
    pub idr: bool,
    /// Coded at a probe window's rate (the probe raise), not at the rate in force.
    pub raised: bool,
}

/// The `RefManager`'s counters (and the bridge's), copied out for the scene's
/// assertions.
pub type PcmRefCounters = crate::transport::io_bridge::RefsReport;

/// Shared with the scene runner, which reads it after the scene.
#[derive(Debug, Default, Clone)]
pub struct PcmLogInner {
    pub truth: BTreeMap<u32, TruthEntry>,
    /// Frame sequence numbers in the order the encoder produced them.
    pub order: Vec<u32>,
    /// Published by the bridge as the manager hears things.
    pub refs: PcmRefCounters,
    /// What the rate model did to the frame sizes (zero without a model).
    pub model: PcmModelStats,
    /// The access units an acked-reference scene's encoder made, in order.
    pub au_log: Vec<AuRecord>,
}

pub type PcmLog = Arc<Mutex<PcmLogInner>>;

fn tag_of(seq: u32) -> u16 {
    u16::try_from(seq).expect("a harness scene has fewer than 65536 frames")
}

fn record(log: &PcmLog, seq: u32, entry: TruthEntry) {
    let mut l = log.lock().expect("pcm log poisoned");
    l.truth.insert(seq, entry);
    l.order.push(seq);
}

/// A PCM-patch encoder behind the backend trait: it executes the
/// `RefManager`'s plan with the real slice-header writer and records the
/// picture the frame is meant to show. Frame sizes are set by the scene
/// (`set_sizes`): the natural size of a PCM P frame is one datagram.
pub(crate) struct PcmBackend {
    params: StreamParams,
    log: PcmLog,
    /// Sizes the access units are padded to; 0 leaves the natural size.
    idr_bytes: usize,
    p_bytes: usize,
    rate: Option<RateTarget>,
    /// Sizes frames from the rate the backend was told, instead of
    /// `idr_bytes` and `p_bytes`.
    model: Option<PcmRateModel>,
    rate_state: RateState,
}

impl PcmBackend {
    pub(crate) fn new(log: PcmLog) -> Self {
        Self {
            params: pcm_params(),
            log,
            idr_bytes: 0,
            p_bytes: 0,
            rate: None,
            model: None,
            rate_state: RateState::default(),
        }
    }

    pub(crate) fn set_model(&mut self, model: Option<PcmRateModel>) {
        self.model = model;
    }

    pub(crate) fn set_sizes(&mut self, idr_bytes: usize, p_bytes: usize) {
        self.idr_bytes = idr_bytes;
        self.p_bytes = p_bytes;
    }
}

impl H264Backend for PcmBackend {
    fn name(&self) -> &'static str {
        "pcm"
    }

    fn caps(&self) -> BackendCaps {
        BackendCaps {
            explicit_refs: true,
            max_lt: 2,
            max_ref_frames: self.params.max_num_ref_frames as u8,
            vbr: true,
        }
    }

    fn open(&mut self, cfg: &StreamConfig) -> Result<(), BackendError> {
        self.rate = Some(cfg.rate);
        Ok(())
    }

    fn encode(
        &mut self,
        _nv12: &Nv12View<'_>,
        plan: &FramePlan,
        _refs: &[ResidentRef],
    ) -> Result<EncodedAu, BackendError> {
        let seq = plan.frame_seq;
        let tag = tag_of(seq);
        let header = plan.slice_header(&self.params);
        let is_idr = plan.kind == PicKind::Idr;
        let (picture, ref_seq) = if is_idr {
            (Picture::idr(tag), None)
        } else {
            let t = plan
                .ref_frame_seq()
                .ok_or_else(|| BackendError::Plan("a P plan names its target".into()))?;
            let base = self
                .log
                .lock()
                .expect("pcm log poisoned")
                .truth
                .get(&t)
                .map(|e| e.picture.clone())
                .ok_or_else(|| BackendError::Plan(format!("frame {seq}: {t} was never made")))?;
            (Picture::predicted(&base, tag), Some(t))
        };
        let mut payload = Vec::new();
        if is_idr {
            payload.extend(parameter_sets(&self.params));
        }
        payload.extend(access_unit(&self.params, &header, tag));
        let target = match (&self.model, &self.rate) {
            (Some(m), Some(rate)) => {
                let (bytes, outcome) = self.rate_state.frame_bytes(m, is_idr, rate);
                self.log
                    .lock()
                    .expect("pcm log poisoned")
                    .model
                    .note(is_idr, outcome);
                bytes
            }
            _ if is_idr => self.idr_bytes,
            _ => self.p_bytes,
        };
        pad_to(&mut payload, target);
        record(
            &self.log,
            seq,
            TruthEntry {
                idr: is_idr,
                ref_seq,
                picture,
                non_ref: plan.nal_ref_idc == 0,
            },
        );
        Ok(EncodedAu {
            payload,
            meta: AuMeta {
                idr: is_idr,
                header: Some(header),
                ref_seq,
                params: None,
                encode_us: 0,
            },
        })
    }

    fn set_rate(&mut self, rate: RateTarget) {
        self.rate = Some(rate);
    }

    fn feedback(&mut self, _fb: BackendFeedback) {}
}

/// Grow an access unit to `target` bytes with a filler-data NAL unit (type 12),
/// which every decoder skips. A scene sets frame sizes this way: the natural
/// PCM size of a P frame is a single datagram.
pub(crate) fn pad_to(au: &mut Vec<u8>, target: usize) {
    const OVERHEAD: usize = 4 + 1 + 1;
    if au.len() + OVERHEAD >= target {
        return;
    }
    let fill = target - au.len() - OVERHEAD;
    au.extend_from_slice(&[0, 0, 0, 1, 0x0C]);
    au.extend(std::iter::repeat_n(0xFF, fill));
    au.push(0x80);
}

/// The source of [`PcmMode::OpenH264`] scenes: the canvas of each frame, the
/// openh264 session that encodes it, and the truth log entry for it. The
/// bridge runs it inline where the encode worker would, with the same
/// feedback-then-picture order (`encode_planned`).
pub(crate) struct Openh264Source {
    log: PcmLog,
    scene_change_every: u32,
    churn_every: u32,
    stale_every: u32,
    backend: Option<Box<dyn H264Backend>>,
    nv12: Vec<u8>,
}

impl Openh264Source {
    pub(crate) fn new(log: PcmLog, canvas: (u32, u32, u32)) -> Self {
        let (scene_change_every, churn_every, stale_every) = canvas;
        Self {
            log,
            scene_change_every,
            churn_every,
            stale_every,
            backend: None,
            nv12: Vec::new(),
        }
    }

    /// Open the session the first time (a stream starts on an IDR, as in
    /// production). `Err` carries why.
    pub(crate) fn ensure_open(
        &mut self,
        lib: Option<std::path::PathBuf>,
        rate: RateTarget,
        fps: u32,
        idr: bool,
    ) -> Result<(), BackendError> {
        if self.backend.is_some() {
            return Ok(());
        }
        if !idr {
            return Err(BackendError::Plan("a stream starts on an IDR".into()));
        }
        let cfg = StreamConfig {
            rate,
            ..StreamConfig::new(
                super::pcm_writer::WIDTH as u32,
                super::pcm_writer::HEIGHT as u32,
                fps,
                rate.bps,
            )
        };
        let mut b = super::openh264::OpenH264Backend::new(lib);
        b.open(&cfg)?;
        self.backend = Some(Box::new(b));
        Ok(())
    }

    /// Paint frame `seq`'s canvas; the picture a decoder with everything it
    /// needs shows.
    pub(crate) fn paint(&mut self, seq: u32) -> Picture {
        let stale =
            seq > 0 && self.stale_every != 0 && seq % self.stale_every == 7 % self.stale_every;
        let painted = if stale { seq - 1 } else { seq };
        canvas_nv12(
            painted,
            self.scene_change_every,
            self.churn_every,
            &mut self.nv12,
        );
        super::pcm_writer::canvas_picture(seq, self.scene_change_every)
    }

    /// The NV12 view of the canvas [`paint`](Self::paint) made, and the backend.
    pub(crate) fn parts(&mut self) -> Option<(Nv12View<'_>, &mut dyn H264Backend)> {
        let w = super::pcm_writer::WIDTH;
        let view = Nv12View {
            data: &self.nv12,
            width: w as u32,
            height: super::pcm_writer::HEIGHT as u32,
            y_stride: w,
            uv_stride: w,
            uv_offset: w * super::pcm_writer::HEIGHT,
        };
        let b = self.backend.as_mut()?;
        Some((view, b.as_mut()))
    }

    /// Log what frame `seq` is meant to show and what it predicts from.
    pub(crate) fn record(&self, seq: u32, picture: Picture, au: &EncodedAu, ref_seq: Option<u32>) {
        record(
            &self.log,
            seq,
            TruthEntry {
                idr: au.meta.idr,
                ref_seq,
                picture,
                non_ref: au.meta.header.as_ref().is_some_and(|h| h.nal_ref_idc == 0),
            },
        );
    }
}

/// Rate changes the openh264 backend has had refused so far in this process
/// (the `openh264_rate_refused` field of `h264 stats`); a scene reads it
/// before and after its run.
pub fn openh264_rate_refusals() -> u64 {
    super::openh264::rate_refusals()
}

/// The openh264 library the scenes will run on, `origin version`; `Err` says
/// what was found instead. It is the library the bridge will open: the one
/// `GHOSTFRAME_H264_OPENH264_LIB` names (through `config`, as the daemon reads
/// it) or else the system's. A scene calls this first, so a missing library is
/// one clear failure and not a stalled harness.
pub fn openh264_library() -> Result<String, String> {
    super::openh264_sys::Lib::get(super::openh264_sys::Lib::configured_path().as_deref())
        .map(|l| format!("{} {}", l.origin, l.version))
        .map_err(str::to_string)
}
