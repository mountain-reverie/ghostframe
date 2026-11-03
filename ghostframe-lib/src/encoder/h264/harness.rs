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
//!   measured against.
//! * [`PcmMode::RefManager`] runs the real `RefManager`: it decides every
//!   frame's plan, the plan is written with the real slice-header writer, and
//!   the harness feeds it what the transport sees (fragment acknowledgements,
//!   loss verdicts) and what the client reports. Production does not use it
//!   yet (B5).
//!
//! # The modelled frame ack (0x0A)
//!
//! No client sends 0x0A yet. A frame the transport has fully acknowledged is
//! *not* a stand-in for "decoded": the real `ClientCore` withholds every P
//! frame behind a lost reference, and `RefManager::frame_decoded` marks the
//! whole target chain decoded, so a complete P6 would erase the loss of P5.
//! The scene runner therefore models the ack from what the client really did:
//! each access unit libavcodec decoded to the picture the encoder meant is a
//! decoded bit, each one it failed on or showed wrongly a refused bit, and
//! the resulting `frame_ack` reaches [`PcmSource::frame_ack`] one client ->
//! server one-way delay after the client acted (applied directly to the
//! bridge, not carried by a datagram, so it is not itself subject to the
//! scene's ack loss). B5 replaces this with the real datagram.
//!
//! Whatever the source, [`PcmLog`] records, for each frame sent, the picture a
//! client that decoded the stream the encoder meant would show. The client
//! side compares what libavcodec really produced against it: a difference is
//! a wrong picture displayed.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ghostframe_protocol::protocol::{
    FrameRefInfo, FRAME_FLAG_IDR, FRAME_FLAG_MARK_LT, FRAME_FLAG_NON_REF, FRAME_FLAG_TARGET_IS_LT,
    REF_FRAME_NONE,
};

pub use super::pcm_decoder::{Decoded, FeedError, PcmDecoder, Strictness};
pub use super::pcm_writer::{marker_cell, Picture};

use super::pcm_writer::{access_unit, parameter_sets, pcm_params, FRAME_BYTES};
use super::refs::{FramePlan, Plan, RefConfig, RefManager, RefTarget};
use super::syntax::{SliceHeader, StreamParams};
use super::tracker::{FrameState, PicKind};

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

/// The `RefManager`'s counters, copied out for the scene's assertions.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PcmRefCounters {
    pub idr_session: u64,
    pub idr_resolution: u64,
    pub idr_client_reset: u64,
    pub idr_encoder_fault: u64,
    pub idr_reissued: u64,
    pub p_healthy: u64,
    pub p_recovery: u64,
    pub idr_probes: u64,
    pub reroots: u64,
    pub candidates_marked: u64,
    pub anchors_confirmed: u64,
    pub recoveries: u64,
    pub recovered: u64,
    pub plan_mismatch: u64,
    pub deadline_suspects: u64,
}

/// Shared with the scene runner, which reads it after the scene.
#[derive(Debug, Default, Clone)]
pub struct PcmLogInner {
    pub truth: BTreeMap<u32, TruthEntry>,
    /// Frame sequence numbers in the order the encoder produced them.
    pub order: Vec<u32>,
    pub refs: PcmRefCounters,
    /// Frames the `RefManager` was asked to plan and answered `Hold`.
    pub plan_holds: u64,
    /// Verdicts and fully acknowledged frames the source was told about.
    pub verdicts_seen: u64,
    pub frames_completed: u64,
    /// Modelled frame acks delivered, and client resets honoured.
    pub frame_acks: u64,
    pub client_resets: u64,
}

pub type PcmLog = Arc<Mutex<PcmLogInner>>;

/// An encoded frame, ready for the send path.
pub(crate) struct Encoded {
    pub payload: Vec<u8>,
    pub is_idr: bool,
    pub info: FrameRefInfo,
}

/// A frame the `RefManager` planned: written, not yet reported sent.
pub(crate) struct Planned {
    pub encoded: Encoded,
    plan: FramePlan,
    header: SliceHeader,
}

impl Planned {
    pub(crate) fn cause(&self) -> Option<super::refs::IdrCause> {
        self.plan.idr_cause
    }
}

#[derive(Debug)]
struct Prev {
    seq: u32,
    frame_num: u32,
    poc: u32,
    picture: Picture,
}

pub(crate) struct PcmSource {
    mode: PcmMode,
    params: StreamParams,
    log: PcmLog,
    prev: Option<Prev>,
    idr_pic_id: u32,
    refm: Option<RefManager>,
    /// See `InjectedH264::pcm_wrong_ref_every`.
    wrong_ref_every: u32,
}

/// The wire's description of a plan (design 6.1).
fn plan_info(plan: &FramePlan) -> FrameRefInfo {
    let mut flags = 0u8;
    let mut slots = 0u8;
    if plan.kind == PicKind::Idr {
        flags |= FRAME_FLAG_IDR;
    }
    if plan.nal_ref_idc == 0 {
        flags |= FRAME_FLAG_NON_REF;
    }
    if let Some(k) = plan.mark {
        flags |= FRAME_FLAG_MARK_LT;
        slots |= k & 3;
    }
    if let Some(RefTarget::Long { slot, .. }) = plan.target {
        flags |= FRAME_FLAG_TARGET_IS_LT;
        slots |= (slot & 3) << 2;
    }
    FrameRefInfo {
        flags,
        slots,
        ref_frame_seq: plan.ref_frame_seq().unwrap_or(REF_FRAME_NONE),
    }
}

fn tag_of(seq: u32) -> u16 {
    u16::try_from(seq).expect("a harness scene has fewer than 65536 frames")
}

impl PcmSource {
    pub(crate) fn new(mode: PcmMode, log: PcmLog, frame_interval: Duration) -> Self {
        let params = pcm_params();
        let refm = (mode == PcmMode::RefManager).then(|| {
            RefManager::new(RefConfig {
                params,
                frame_interval,
                initial_srtt: frame_interval * 4,
            })
        });
        Self {
            mode,
            params,
            log,
            prev: None,
            idr_pic_id: 0,
            refm,
            wrong_ref_every: 0,
        }
    }

    pub(crate) fn set_wrong_ref_every(&mut self, n: u32) {
        self.wrong_ref_every = n;
    }

    /// The tile codec handed the screen back (design 5.6).
    pub(crate) fn mode_reentry(&mut self) {
        if let Some(m) = self.refm.as_mut() {
            m.mode_reentry();
            self.publish_counters();
        }
    }

    pub(crate) fn mode(&self) -> PcmMode {
        self.mode
    }

    fn record(&self, seq: u32, entry: TruthEntry) {
        let mut l = self.log.lock().expect("pcm log poisoned");
        l.truth.insert(seq, entry);
        l.order.push(seq);
    }

    fn publish_counters(&self) {
        let Some(m) = &self.refm else { return };
        let s = m.stats();
        self.log.lock().expect("pcm log poisoned").refs = PcmRefCounters {
            idr_session: s.idr_session,
            idr_resolution: s.idr_resolution,
            idr_client_reset: s.idr_client_reset,
            idr_encoder_fault: s.idr_encoder_fault,
            idr_reissued: s.idr_reissued,
            p_healthy: s.p_healthy,
            p_recovery: s.p_recovery,
            idr_probes: s.idr_probes,
            reroots: s.reroots,
            candidates_marked: s.candidates_marked,
            anchors_confirmed: s.anchors_confirmed,
            recoveries: s.recoveries,
            recovered: s.recovered,
            plan_mismatch: s.plan_mismatch,
            deadline_suspects: s.deadline_suspects,
        };
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
        self.record(
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

    /// Ask the `RefManager` for the next frame. `None`: it holds the frame
    /// (the epoch's IDR is still being delivered).
    pub(crate) fn plan_frame(&mut self, seq: u32, now: Instant, srtt: Duration) -> Option<Planned> {
        let m = self.refm.as_mut().expect("plan_frame needs the RefManager");
        m.tick(now, srtt);
        let plan = match m.plan_next(seq, now) {
            Plan::Hold => {
                self.log.lock().expect("pcm log poisoned").plan_holds += 1;
                self.publish_counters();
                return None;
            }
            Plan::Frame(p) => p,
        };
        let tag = tag_of(seq);
        let header = plan.slice_header(&self.params);
        let is_idr = plan.kind == PicKind::Idr;
        let (picture, ref_seq) = if is_idr {
            (Picture::idr(tag), None)
        } else {
            let t = plan.ref_frame_seq().expect("a P plan names its target");
            let base = self
                .log
                .lock()
                .expect("pcm log poisoned")
                .truth
                .get(&t)
                .map(|e| e.picture.clone())
                .unwrap_or_else(|| panic!("frame {seq}: plan names {t}, which was never made"));
            (Picture::predicted(&base, tag), Some(t))
        };
        let mut payload = Vec::new();
        if is_idr {
            payload.extend(parameter_sets(&self.params));
        }
        payload.extend(access_unit(&self.params, &header, tag));
        self.record(
            seq,
            TruthEntry {
                idr: is_idr,
                ref_seq,
                picture,
                non_ref: plan.nal_ref_idc == 0,
            },
        );
        Some(Planned {
            encoded: Encoded {
                payload,
                is_idr,
                info: plan_info(&plan),
            },
            plan,
            header,
        })
    }

    /// The planned frame went on the wire as `frags_total` fragments.
    pub(crate) fn plan_sent(&mut self, p: &Planned, frags_total: u16, now: Instant) {
        let m = self.refm.as_mut().expect("plan_sent needs the RefManager");
        m.frame_sent(&p.plan, &p.header, frags_total, now);
        self.publish_counters();
    }

    pub(crate) fn frag_acked(&mut self, seq: u32, frag_idx: u16, now: Instant) {
        if let Some(m) = self.refm.as_mut() {
            m.frag_acked(seq, frag_idx, now);
        }
    }

    /// Every fragment of the frame is acknowledged. Counted only: transport
    /// completeness says nothing about decoding (see the module docs).
    pub(crate) fn frame_complete(&mut self, _seq: u32) {
        self.log.lock().expect("pcm log poisoned").frames_completed += 1;
    }

    /// The modelled 0x0A: bit `i` of each mask is frame `newest - i`.
    pub(crate) fn frame_ack(&mut self, newest: u32, decoded: u64, refused: u64) {
        self.log.lock().expect("pcm log poisoned").frame_acks += 1;
        if let Some(m) = self.refm.as_mut() {
            m.frame_ack(newest, decoded, refused);
            self.publish_counters();
        }
    }

    /// The server honoured a client reset request (0x09).
    pub(crate) fn client_reset(&mut self) {
        self.log.lock().expect("pcm log poisoned").client_resets += 1;
        if let Some(m) = self.refm.as_mut() {
            m.client_reset(super::refs::ResetReason::DecoderFailure);
            self.publish_counters();
        }
    }

    pub(crate) fn frame_suspect(&mut self, seq: u32) {
        self.log.lock().expect("pcm log poisoned").verdicts_seen += 1;
        if let Some(m) = self.refm.as_mut() {
            m.frame_suspect(seq);
            self.publish_counters();
        }
    }

    pub(crate) fn session_reset(&mut self) {
        self.prev = None;
        if let Some(m) = self.refm.as_mut() {
            m.session_reset();
            self.publish_counters();
        }
    }

    /// Whether the `RefManager` has lost faith in the frame.
    pub(crate) fn frame_is_suspect(&self, seq: u32) -> bool {
        self.refm
            .as_ref()
            .and_then(|m| m.frame_state(seq))
            .is_some_and(|s| matches!(s, FrameState::Suspect | FrameState::Dead))
    }
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
