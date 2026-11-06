//! The H.264 source of the browserless harness (design T3): real, decodable
//! PCM-patch streams in place of the encoder, behind the transport's real
//! send path.
//!
//! Two sources, one truth log.
//!
//! * [`PcmMode::Legacy`] is the Phase A policy's stream: the bridge decides
//!   when an IDR is made (session start, loss, hold timeout, scripted GOP) and
//!   every P frame predicts from the frame sent before it, with the default
//!   reference list. It is the baseline the acked-reference policy is
//!   measured against ([`PcmSource`]).
//! * [`PcmMode::RefManager`] is production's acked-reference path with a
//!   [`PcmBackend`] for an encoder: the bridge's `RefManager` plans every
//!   frame, the backend writes the plan with the real slice-header writer, and
//!   the bridge feeds the manager what the transport sees (fragment
//!   acknowledgements, loss verdicts) and what the client reports (its own
//!   0x0A datagrams and 0x09 reset requests), exactly as it does for the
//!   libva-direct backend. Only the thread hop to the encode worker is
//!   missing: the stepped harness cannot run one.
//!
//! Whatever the source, [`PcmLog`] records, for each frame sent, the picture a
//! client that decoded the stream the encoder meant would show. The client
//! side compares what libavcodec really produced against it: a difference is
//! a wrong picture displayed.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub use super::pcm_decoder::{Decoded, FeedError, PcmDecoder, Strictness};
pub use super::pcm_writer::{marker_cell, Picture};

use super::backend::{
    AuMeta, BackendCaps, BackendError, BackendFeedback, EncodedAu, H264Backend, Nv12View,
    RateTarget, StreamConfig,
};
use super::pcm_writer::{access_unit, parameter_sets, pcm_params, FRAME_BYTES};
use super::refs::{FramePlan, ResidentRef};
use super::syntax::{SliceHeader, StreamParams};
use super::tracker::PicKind;

/// Bytes of a decoded I420 frame of the PCM writer's pictures. A function, not
/// a constant: a `pub const` of this crate lands in the generated C header.
pub fn frame_bytes() -> usize {
    FRAME_BYTES
}

/// Where a scene's H.264 frames come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmMode {
    /// The Phase A policy: the bridge's IDR decisions, each P from the frame
    /// before it.
    Legacy,
    /// The `RefManager`'s plans.
    RefManager,
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

#[derive(Debug)]
struct Prev {
    seq: u32,
    frame_num: u32,
    poc: u32,
    picture: Picture,
}

/// The Phase A stream: an IDR when the bridge asks for one, otherwise a P from
/// the frame sent before it.
pub(crate) struct PcmSource {
    params: StreamParams,
    log: PcmLog,
    prev: Option<Prev>,
    idr_pic_id: u32,
    /// See `InjectedH264::pcm_wrong_ref_every`.
    wrong_ref_every: u32,
}

impl PcmSource {
    pub(crate) fn new(log: PcmLog) -> Self {
        Self {
            params: pcm_params(),
            log,
            prev: None,
            idr_pic_id: 0,
            wrong_ref_every: 0,
        }
    }

    pub(crate) fn set_wrong_ref_every(&mut self, n: u32) {
        self.wrong_ref_every = n;
    }

    /// The Phase A stream: an IDR when `is_idr`, otherwise a P from the frame
    /// sent before this one. Its wire description is the bridge's own
    /// (`h264_ref_info`). Returns the payload and whether it is an IDR: the
    /// first frame of a stream is one whatever was asked.
    pub(crate) fn legacy_frame(&mut self, seq: u32, is_idr: bool) -> (Vec<u8>, bool) {
        let tag = tag_of(seq);
        let mut au = Vec::new();
        let (header, picture, ref_seq, frame_num, poc) = match (&self.prev, is_idr) {
            (Some(prev), false) => {
                let frame_num = (prev.frame_num + 1) % self.params.max_frame_num();
                let poc = prev.poc + 2;
                let lsb = poc & ((1 << self.params.log2_max_poc_lsb) - 1);
                // The defect under test: written as a non-reference picture,
                // recorded as the reference of the next frame.
                let defect = self.wrong_ref_every > 0 && seq.is_multiple_of(self.wrong_ref_every);
                (
                    SliceHeader::p(frame_num, lsb, if defect { 0 } else { 2 }),
                    Picture::predicted(&prev.picture, tag),
                    Some(prev.seq),
                    frame_num,
                    poc,
                )
            }
            _ => {
                // The first frame of a stream is an IDR whatever was asked.
                self.idr_pic_id = (self.idr_pic_id + 1) & 0xffff;
                au.extend(parameter_sets(&self.params));
                (
                    SliceHeader::idr(self.idr_pic_id, false),
                    Picture::idr(tag),
                    None,
                    0,
                    0,
                )
            }
        };
        au.extend(access_unit(&self.params, &header, tag));
        record(
            &self.log,
            seq,
            TruthEntry {
                idr: ref_seq.is_none(),
                ref_seq,
                picture: picture.clone(),
                non_ref: false,
            },
        );
        self.prev = Some(Prev {
            seq,
            frame_num,
            poc,
            picture,
        });
        (au, ref_seq.is_none())
    }

    pub(crate) fn session_reset(&mut self) {
        self.prev = None;
    }
}

/// What the PCM encoder's rate model is asked to produce, and how it limits
/// it. Without a model a scene fixes the frame sizes (`set_sizes`), so the
/// rate the backend is told changes nothing: a stream under a rate policy then
/// proves nothing about rate control.
///
/// This is the bps-only baseline: a VBR bucket of `bps / 10` (0.1 s of the
/// rate), refilled by `bps / 8 / fps` for each frame and starting 75 % full. A
/// frame is `min(demand, bucket)`, so a scene whose demand is above the rate is
/// held to the rate it was told, and one below it is not stuffed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PcmRateModel {
    /// What the content would cost with no limit, bytes.
    pub demand_p_bytes: usize,
    pub demand_idr_bytes: usize,
    /// The scene's frame rate (the bucket refills once per frame).
    pub fps: u32,
}

/// The model's state: the bucket level, bytes.
#[derive(Debug, Clone, Copy, Default)]
struct RateBucket {
    level: Option<f64>,
}

impl RateBucket {
    /// The size of the next access unit under `rate`.
    fn frame_bytes(&mut self, m: &PcmRateModel, is_idr: bool, rate: &RateTarget) -> usize {
        let bps = f64::from(rate.bps);
        let capacity = bps / 10.0 / 8.0;
        let refill = bps / 8.0 / f64::from(m.fps.max(1));
        let level = self.level.unwrap_or(capacity * 0.75);
        let level = (level + refill).min(capacity);
        let demand = if is_idr {
            m.demand_idr_bytes
        } else {
            m.demand_p_bytes
        };
        let size = (demand as f64).min(level);
        self.level = Some(level - size);
        size as usize
    }
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
    bucket: RateBucket,
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
            bucket: RateBucket::default(),
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
            (Some(m), Some(rate)) => self.bucket.frame_bytes(m, is_idr, rate),
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

#[cfg(test)]
mod rate_model_tests {
    use super::*;

    fn model() -> PcmRateModel {
        PcmRateModel {
            demand_p_bytes: 40_000,
            demand_idr_bytes: 120_000,
            fps: 30,
        }
    }

    /// Sizes of `n` P frames at `bps`, after an IDR.
    fn run(bucket: &mut RateBucket, bps: u32, n: usize, demand_p: usize) -> Vec<usize> {
        let m = PcmRateModel {
            demand_p_bytes: demand_p,
            ..model()
        };
        let rate = RateTarget::steady(bps, 30);
        (0..n)
            .map(|_| bucket.frame_bytes(&m, false, &rate))
            .collect()
    }

    #[test]
    fn a_demanding_stream_is_held_to_the_rate_it_was_told() {
        let mut b = RateBucket::default();
        let sizes = run(&mut b, 3_000_000, 90, 40_000);
        // Past the initial fullness, a frame is the rate's share of a frame.
        let tail: usize = sizes[30..].iter().sum();
        let want = 3_000_000 / 8 / 30 * 60;
        assert!(tail.abs_diff(want) <= want / 50, "{tail} vs {want}");
        assert!(sizes[0] > sizes[89], "the first frames spend the bucket");
    }

    #[test]
    fn a_rate_change_moves_the_next_frame() {
        let mut b = RateBucket::default();
        run(&mut b, 6_000_000, 60, 40_000);
        let low = run(&mut b, 1_500_000, 40, 40_000);
        let steady_low: usize = low[20..].iter().sum::<usize>() / 20;
        assert_eq!(steady_low, 1_500_000 / 8 / 30);
        assert!(
            low[0] < 6_000_000 / 8 / 30 / 2 + 1_500_000 / 8 / 10,
            "the very next frame is already bounded by the smaller bucket: {}",
            low[0]
        );
    }

    #[test]
    fn content_cheaper_than_the_rate_is_not_stuffed() {
        let mut b = RateBucket::default();
        let sizes = run(&mut b, 6_000_000, 60, 3_000);
        assert!(sizes.iter().all(|&s| s == 3_000), "{sizes:?}");
    }
}
