//! `RefManager`: decides each H.264 frame's reference plan (design 3.2, 3.3).
//!
//! Pure: it sees frames sent and what the client reported, and answers with a
//! [`FramePlan`]. Calls are strictly sequential, plans are not pipelined:
//! [`RefManager::frame_sent`] (or [`RefManager::frame_dropped`]) for a plan
//! must come before the next [`RefManager::plan_next`]. While healthy a P predicts from the previous reference
//! frame. After a loss it predicts from the newest anchor the client has
//! confirmed decoding (a long-term reference) until a re-root frame is
//! decoded. IDR is allowed only at session start, on a resolution change, on
//! a client reset and after repeated plan/stream mismatches.
//!
//! Two long-term slots hold anchor candidates. A candidate is a P marked with
//! MMCO 6; it becomes an anchor only when its frame is reported decoded *and*
//! it is still the last frame marked into its slot (the occupant rule), and a
//! new candidate always goes into the slot that does not hold the newest
//! confirmed anchor.

use std::time::{Duration, Instant};

use ghostframe_protocol::protocol::{
    FrameRefInfo, FRAME_FLAG_IDR, FRAME_FLAG_MARK_LT, FRAME_FLAG_MARK_TARGET, FRAME_FLAG_NON_REF,
    FRAME_FLAG_TARGET_IS_LT, REF_FRAME_NONE,
};

use super::backend::BackendFeedback;
use super::syntax::{DecoderModel, Effective, Mmco, RefPic, Rplm, SliceHeader, StreamParams};
use super::tracker::{seq_lt, FragAck, FrameRec, FrameState, FrameTable, PicKind};

pub(crate) const LT_SLOTS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdrCause {
    SessionStart,
    ResolutionChange,
    ClientReset,
    /// Three plans in a row did not match what the encoder emitted.
    EncoderFault,
    /// The path MTU fell below the size the epoch's IDR was fragmented for:
    /// its queued fragments were retired and the IDR is cut again for the
    /// size the connection takes now (see `frame_oversize`).
    MtuShrink,
    /// The backend made an IDR nobody asked for (observed mode: the library's
    /// own fallback, e.g. an unknown recovery frame), or the stream moved to a
    /// different backend after a fault. A new epoch, adopted as such.
    BackendFallback,
}

/// Why the client asked for a reset (feedback stream 0x09).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResetReason {
    DecoderFailure,
    Resolution,
    NoDecoder,
    GateReset,
}

/// The picture a P slice predicts from, as the plan names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefTarget {
    Short { seq: u32, frame_num: u32 },
    Long { slot: u8, seq: u32 },
}

impl RefTarget {
    pub(crate) fn seq(&self) -> u32 {
        match *self {
            Self::Short { seq, .. } | Self::Long { seq, .. } => seq,
        }
    }
}

/// One frame's plan. `epoch` lets the caller drop results that outlived a
/// session or resolution change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FramePlan {
    pub epoch: u32,
    pub frame_seq: u32,
    pub kind: PicKind,
    pub idr_cause: Option<IdrCause>,
    pub nal_ref_idc: u8,
    pub frame_num: u32,
    /// Picture order count: 2 per H.264 frame since the IDR. The slice header
    /// carries its low bits.
    pub poc: u32,
    pub target: Option<RefTarget>,
    /// What the caller asked of the backend. `Executed`: the backend executes
    /// `target`, `mark`, `mmco` and `rplm` (explicit mode). In observed mode
    /// those fields are empty on a plan and filled in from the emitted header
    /// by [`RefManager::observe`].
    pub intent: Intent,
    /// The long-term slot this frame is marked into (candidate anchor).
    pub mark: Option<u8>,
    /// The mark in `mark` lands on the short-term picture this frame predicts
    /// from, not on this frame (an MMCO 3 marking). Reaches the client as
    /// `FRAME_FLAG_MARK_TARGET` in place of `FRAME_FLAG_MARK_LT`.
    pub mark_target: bool,
    pub mmco: Vec<Mmco>,
    pub rplm: Vec<Rplm>,
}

/// What a plan asks of the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Intent {
    /// The plan's references, marking and list modification are executed as
    /// written (explicit mode).
    Executed,
    /// Observed mode: continue the backend's own chain.
    Healthy,
    /// Observed mode: predict from the confirmed long-term picture `root`
    /// (the bridge sends the backend an `LtrRecover` for it).
    Recover { root: Anchor },
}

impl FramePlan {
    /// The slice header (up to the QP) that executes this plan.
    pub(crate) fn slice_header(&self, params: &StreamParams) -> SliceHeader {
        let lsb = self.poc & ((1u32 << params.log2_max_poc_lsb) - 1);
        if self.kind == PicKind::Idr {
            let mut h = SliceHeader::idr(self.epoch & 0xffff, true);
            h.poc_lsb = lsb;
            return h;
        }
        let mut h = SliceHeader::p(self.frame_num, lsb, self.nal_ref_idc);
        h.rplm = self.rplm.clone();
        h.mmco = (!self.mmco.is_empty()).then(|| self.mmco.clone());
        h
    }

    /// `ref_frame_seq` for the frame header: the named picture, if any.
    pub(crate) fn ref_frame_seq(&self) -> Option<u32> {
        self.target.map(|t| t.seq())
    }

    /// The wire's description of this frame (design 6.1): IDR, non-ref, mark
    /// and target-is-LT flags, the two slot indices, and the reference. The
    /// reference is `ref_seq`, what the backend really put in
    /// `RefPicList0[0]`, so the client gate checks the stream and not the
    /// manager's belief; `None` falls back to the plan's.
    pub(crate) fn wire_info(&self, ref_seq: Option<u32>) -> FrameRefInfo {
        let mut flags = 0u8;
        let mut slots = 0u8;
        if self.kind == PicKind::Idr {
            flags |= FRAME_FLAG_IDR;
        }
        if self.nal_ref_idc == 0 {
            flags |= FRAME_FLAG_NON_REF;
        }
        if let Some(k) = self.mark {
            flags |= if self.mark_target {
                FRAME_FLAG_MARK_TARGET
            } else {
                FRAME_FLAG_MARK_LT
            };
            slots |= k & 3;
        }
        if let Some(RefTarget::Long { slot, .. }) = self.target {
            flags |= FRAME_FLAG_TARGET_IS_LT;
            slots |= (slot & 3) << 2;
        }
        FrameRefInfo {
            flags,
            slots,
            ref_frame_seq: ref_seq.or(self.ref_frame_seq()).unwrap_or(REF_FRAME_NONE),
        }
    }
}

/// The answer to [`RefManager::plan_next`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    Frame(FramePlan),
    /// Nothing to send yet: the epoch's IDR is still being delivered (design
    /// 5.4: no P frame until every IDR fragment is transport-acked). The
    /// caller skips the frame, as the Phase A hold does. Call again at the
    /// next frame; the manager re-sends the IDR itself if delivery stalls.
    Hold,
}

#[cfg(test)]
impl Plan {
    pub(crate) fn frame(self) -> FramePlan {
        match self {
            Self::Frame(p) => p,
            Self::Hold => panic!("held"),
        }
    }
}

/// A confirmed anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Anchor {
    pub slot: u8,
    pub seq: u32,
}

/// One resident reference picture, for the backend's `ReferenceFrames[]`
/// (every resident reference must be listed on every frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResidentRef {
    pub seq: u32,
    pub frame_num: u32,
    pub lt_idx: Option<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct RefConfig {
    pub params: StreamParams,
    pub frame_interval: Duration,
    pub initial_srtt: Duration,
    /// The backend keeps its own reference structure (openh264): the plan is
    /// an intent and the manager reads what the stream did from each emitted
    /// header. `params` are replaced by the stream's own at its first IDR
    /// ([`RefManager::adopt_stream_params`]).
    pub observed: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RefStats {
    pub idr_session: u64,
    pub idr_resolution: u64,
    pub idr_client_reset: u64,
    pub idr_encoder_fault: u64,
    pub idr_mtu_shrink: u64,
    /// An IDR sent again because the first never became an anchor.
    pub idr_reissued: u64,
    pub p_healthy: u64,
    pub p_recovery: u64,
    /// Non-reference probes at the unconfirmed IDR.
    pub idr_probes: u64,
    pub reroots: u64,
    pub candidates_marked: u64,
    pub anchors_confirmed: u64,
    /// A decoded bit for a candidate that had been overwritten.
    pub late_confirm_ignored: u64,
    pub recoveries: u64,
    pub recovered: u64,
    pub reentries: u64,
    pub plan_mismatch: u64,
    pub deadline_suspects: u64,
    pub acks_unknown: u64,
    /// A decoded bit for a frame already judged suspect: the ledger's
    /// hypothesis lost to the client's word (design 5.2).
    pub suspect_overridden: u64,
    /// Anchors confirmed that were not the IDR (the IDR is candidate 0 and
    /// every decoded IDR confirms it): the sign that P frames work.
    pub p_anchors_confirmed: u64,
    /// Observed mode: IDRs the backend made that nobody asked for, and
    /// streams moved to another backend (each a new epoch).
    pub idr_backend_fallback: u64,
    /// Observed mode: long-term marks seen in the emitted headers.
    pub marks_observed: u64,
    /// Marks that the wire cannot describe (neither the frame itself nor the
    /// picture it predicts from): counted, never an anchor candidate.
    pub mark_unrepresentable: u64,
    /// `LtrRecover` requests made.
    pub ltr_recoveries_requested: u64,
    /// `LtrMarkFailed` sent: for lost or refused carriers, and for marks the
    /// wire cannot describe (`mark_unrepresentable`).
    pub ltr_mark_failed_sent: u64,
}

/// What [`RefManager::frame_sent`] found when it replayed the emitted header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SentReport {
    /// The header's effective reference is the plan's and nothing broke a rule.
    pub matched: bool,
    pub effective: Effective,
    /// The plan as the stream executed it: in observed mode the plan with
    /// its reference, marking and kind read from the header; otherwise the
    /// plan given.
    pub plan: FramePlan,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    /// The picture last marked into this slot (explicit mode: the frame that
    /// carried an MMCO 6; observed mode: also the picture an MMCO 3 named).
    occupant: Option<u32>,
    /// The frame whose decoded bit confirms the mark: the marked picture
    /// itself for MMCO 6, the frame that carried the MMCO 3 otherwise.
    carrier: Option<u32>,
    /// `frame_num` of the marked picture, and of the carrier.
    marked_fn: u32,
    carrier_fn: u32,
    confirmed: bool,
    /// `LtrMarkFailed` was sent for this mark: it is void.
    failed_sent: bool,
    sent_idx: u64,
    sent_at: Option<Instant>,
}

impl Slot {
    const EMPTY: Self = Self {
        occupant: None,
        carrier: None,
        marked_fn: 0,
        carrier_fn: 0,
        confirmed: false,
        failed_sent: false,
        sent_idx: 0,
        sent_at: None,
    };
}

#[derive(Debug, Clone)]
enum Chain {
    Healthy,
    Recovering {
        root: Anchor,
        last_reroot_at: Option<Instant>,
        /// Re-root frames sent in this episode.
        reroots: Vec<u32>,
    },
}

/// The encoder-side picture of the client's DPB, assuming every frame the
/// encoder produced was decoded. Driven by the emitted slice headers.
#[derive(Debug, Default, Clone)]
struct EncDpb {
    /// Short-term pictures (seq, frame_num), oldest first.
    st: Vec<(u32, u32)>,
    /// Long-term pictures (seq, frame_num) by slot.
    lt: [Option<(u32, u32)>; LT_SLOTS],
}

impl EncDpb {
    fn lt_count(&self) -> usize {
        self.lt.iter().flatten().count()
    }

    fn len(&self) -> usize {
        self.st.len() + self.lt_count()
    }

    /// Index of the short-term picture furthest behind `cur_frame_num`.
    fn oldest_st(&self, cur_frame_num: u32, max_fn: u32) -> Option<usize> {
        (0..self.st.len()).max_by_key(|&i| (cur_frame_num + max_fn - self.st[i].1) % max_fn)
    }
}

/// A long-term mark a header made: the slot, and the picture marked.
#[derive(Debug, Clone, Copy)]
struct MarkSeen {
    slot: u8,
    marked_seq: u32,
    marked_fn: u32,
}

/// What an emitted header says (observed mode).
struct Observed {
    plan: FramePlan,
    mark: Option<MarkSeen>,
    /// `frame_num`s of the pictures marked that the wire cannot describe.
    unrepresentable: Vec<u32>,
    /// An IDR the plan did not ask for.
    unsolicited: bool,
}

/// Whether a frame the encoder produced went on the wire.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fate {
    Sent,
    /// Encoded, then dropped locally: the encoder's DPB has it, the client
    /// never will.
    Dropped,
}

pub(crate) struct RefManager {
    cfg: RefConfig,
    epoch: u32,
    table: FrameTable,
    srtt: Duration,
    pending_idr: Option<IdrCause>,
    /// The cause of this epoch's IDR, reused if it has to be sent again.
    idr_cause: IdrCause,
    idr_seq: Option<u32>,
    chain: Chain,
    /// The newest reference frame on the healthy path.
    prev: Option<u32>,
    dpb: EncDpb,
    /// The encoder-side decoder model, fed every emitted header.
    enc_model: DecoderModel,
    slots: [Slot; LT_SLOTS],
    /// A slot whose occupant was ever confirmed decoded in this epoch: the
    /// client certainly holds a long-term picture there.
    slot_confirmed_once: [bool; LT_SLOTS],
    prev_ref_frame_num: u32,
    poc_count: u32,
    sent_count: u64,
    mismatch_run: u32,
    unknown_hw: Option<u32>,
    /// The last time the IDR's delivery progressed (sent, or a fragment
    /// acknowledged).
    idr_progress_at: Option<Instant>,
    /// IDRs re-sent for the same request without an anchor in between; each
    /// doubles the time allowed (capped), so a slow link converges.
    idr_backoff: u32,
    /// The IDR died because the path MTU fell below its fragments, not for
    /// want of progress: its one re-send does not back the timeout off.
    idr_mtu_dead: bool,
    stats: RefStats,
    // ---- observed mode
    /// `idr_pic_id` of the epoch's IDR: feedback names the IDR period.
    idr_pic_id: u32,
    /// Feedback for the backend, in the order it arose.
    events: Vec<BackendFeedback>,
    /// When the last recovery was asked for (at most one per `srtt` while the
    /// last re-root is unusable).
    last_intent_at: Option<Instant>,
    /// The newest `frame_num` the encoder produced in this epoch.
    newest_fn: u32,
}

impl RefManager {
    pub(crate) fn new(cfg: RefConfig) -> Self {
        let srtt = cfg.initial_srtt;
        let enc_model = DecoderModel::new(cfg.params);
        let mut m = Self {
            cfg,
            epoch: 0,
            table: FrameTable::default(),
            srtt,
            pending_idr: None,
            idr_cause: IdrCause::SessionStart,
            idr_seq: None,
            chain: Chain::Healthy,
            prev: None,
            dpb: EncDpb::default(),
            enc_model,
            slots: [Slot::EMPTY; LT_SLOTS],
            slot_confirmed_once: [false; LT_SLOTS],
            prev_ref_frame_num: 0,
            poc_count: 0,
            sent_count: 0,
            mismatch_run: 0,
            unknown_hw: None,
            idr_progress_at: None,
            idr_backoff: 0,
            idr_mtu_dead: false,
            stats: RefStats::default(),
            idr_pic_id: 0,
            events: Vec::new(),
            last_intent_at: None,
            newest_fn: 0,
        };
        m.begin_epoch(IdrCause::SessionStart);
        m
    }

    // ------------------------------------------------------------- queries

    pub(crate) fn epoch(&self) -> u32 {
        self.epoch
    }

    pub(crate) fn stats(&self) -> RefStats {
        self.stats
    }

    pub(crate) fn is_healthy(&self) -> bool {
        matches!(self.chain, Chain::Healthy)
    }

    pub(crate) fn pending_idr(&self) -> Option<IdrCause> {
        self.pending_idr
    }

    pub(crate) fn frame_state(&self, seq: u32) -> Option<FrameState> {
        self.table.state(seq)
    }

    /// The encoder-side decoder model (every emitted header, in order).
    #[cfg(test)]
    pub(crate) fn enc_model(&self) -> &DecoderModel {
        &self.enc_model
    }

    #[cfg(test)]
    pub(crate) fn debug_slots(&self) -> String {
        format!(
            "{:?} once {:?} chain {:?} prev {:?}",
            self.slots, self.slot_confirmed_once, self.chain, self.prev
        )
    }

    /// The newest confirmed anchor: the newest slot occupant whose frame was
    /// reported decoded while it still was the occupant.
    pub(crate) fn anchor(&self) -> Option<Anchor> {
        let mut best: Option<Anchor> = None;
        for k in 0..LT_SLOTS {
            let s = &self.slots[k];
            if let (true, Some(seq)) = (s.confirmed, s.occupant) {
                if best.is_none_or(|b| seq_lt(b.seq, seq)) {
                    best = Some(Anchor { slot: k as u8, seq });
                }
            }
        }
        best
    }

    fn is_confirmed_anchor(&self, seq: u32) -> bool {
        self.slots
            .iter()
            .any(|s| s.confirmed && s.occupant == Some(seq))
    }

    /// How long ago the newest confirmed anchor was sent, if there is one.
    pub(crate) fn anchor_age(&self, now: Instant) -> Option<Duration> {
        let a = self.anchor()?;
        let f = self.table.get(a.seq)?;
        Some(now.saturating_duration_since(f.sent_at))
    }

    /// Every reference the encoder-side DPB holds (short-term oldest first,
    /// then long-term by slot): the backend lists all of them on every frame.
    pub(crate) fn resident_refs(&self) -> Vec<ResidentRef> {
        let mut v: Vec<ResidentRef> = self
            .dpb
            .st
            .iter()
            .map(|&(seq, frame_num)| ResidentRef {
                seq,
                frame_num,
                lt_idx: None,
            })
            .collect();
        for (k, s) in self.dpb.lt.iter().enumerate() {
            if let Some((seq, frame_num)) = *s {
                v.push(ResidentRef {
                    seq,
                    frame_num,
                    lt_idx: Some(k as u8),
                });
            }
        }
        v
    }

    // -------------------------------------------------------------- events

    fn begin_epoch(&mut self, cause: IdrCause) {
        self.epoch += 1;
        self.table.clear();
        self.chain = Chain::Healthy;
        self.prev = None;
        self.dpb = EncDpb::default();
        self.enc_model = DecoderModel::new(self.cfg.params);
        self.slots = [Slot::EMPTY; LT_SLOTS];
        self.slot_confirmed_once = [false; LT_SLOTS];
        self.idr_seq = None;
        self.prev_ref_frame_num = 0;
        self.poc_count = 0;
        self.mismatch_run = 0;
        self.idr_cause = cause;
        self.idr_mtu_dead = false;
        self.pending_idr = Some(cause);
        self.idr_pic_id = 0;
        self.events.clear();
        self.last_intent_at = None;
        self.newest_fn = 0;
    }

    // ------------------------------------------------------- observed mode

    pub(crate) fn is_observed(&self) -> bool {
        self.cfg.observed
    }

    /// The backend's stream parameters, read from the SPS/PPS of an IDR access
    /// unit it made. Switches the manager to observed mode and models the
    /// stream with them. Call before [`frame_sent`](Self::frame_sent) for that
    /// IDR (the model is replaced; an IDR resets it anyway).
    pub(crate) fn adopt_stream_params(&mut self, params: StreamParams) {
        self.cfg.observed = true;
        if self.cfg.params != params {
            self.cfg.params = params;
        }
        self.enc_model = DecoderModel::new(self.cfg.params);
    }

    /// The stream leaves this backend (a fault limit, a demotion): a new
    /// epoch that begins with an IDR from whichever backend serves it next,
    /// counted as a backend fallback and not as a session start. The next
    /// access unit says whether the manager runs observed.
    pub(crate) fn backend_fallback(&mut self) {
        self.idr_backoff = 0;
        self.cfg.observed = false;
        self.cfg.params = StreamParams::writer();
        self.begin_epoch(IdrCause::BackendFallback);
    }

    /// Feedback the receiver's word produced for a backend that keeps its own
    /// long-term bookkeeping, in order. Empty in explicit mode. Events of an
    /// earlier epoch are gone (a new epoch clears them; the backend would
    /// ignore their stale `idr_pic_id` anyway).
    pub(crate) fn take_backend_feedback(&mut self) -> Vec<BackendFeedback> {
        std::mem::take(&mut self.events)
    }

    pub(crate) fn session_reset(&mut self) {
        self.idr_backoff = 0;
        self.begin_epoch(IdrCause::SessionStart);
    }

    /// The backend failed (or its stream could not be followed): start over
    /// with an IDR, counted as an encoder fault and not as a session start.
    pub(crate) fn encoder_fault(&mut self) {
        self.idr_backoff = 0;
        self.begin_epoch(IdrCause::EncoderFault);
    }

    /// The stream left H.264 while the epoch's IDR was still being delivered:
    /// the fragments were purged, so the IDR is dead and the return sends it
    /// again at once instead of waiting out its timeout.
    pub(crate) fn idr_abandoned(&mut self) {
        if let Some(i) = self.idr_seq.filter(|&i| self.table.idr_incomplete(i)) {
            self.table.set_dead(i);
        }
    }

    /// A fragment of `seq` outgrew the connection (the path MTU fell after it
    /// was built) and was retired unsent. Returns whether `seq` is the IDR
    /// still being delivered.
    ///
    /// The IDR cannot complete: its fragments are all the old size. It is
    /// abandoned as [`idr_abandoned`](Self::idr_abandoned) does (dead, so the
    /// next plan sends it again, with no anchor to recover from) and the
    /// resend is counted under [`IdrCause::MtuShrink`]. Any other frame is
    /// treated as one the ledger judged lost: suspect, and the next plan
    /// recovers from the anchor, fragmented at the new size.
    pub(crate) fn frame_oversize(&mut self, seq: u32) -> bool {
        if self.idr_seq == Some(seq) && self.table.idr_incomplete(seq) {
            self.table.set_dead(seq);
            self.idr_cause = IdrCause::MtuShrink;
            self.idr_mtu_dead = true;
            return true;
        }
        self.frame_suspect(seq);
        false
    }

    pub(crate) fn resolution_change(&mut self) {
        self.idr_backoff = 0;
        self.begin_epoch(IdrCause::ResolutionChange);
    }

    /// A reset request from the client (0x09). `Resolution` is a resolution
    /// change; the others are a client reset. Rate limiting is the caller's.
    pub(crate) fn client_reset(&mut self, reason: ResetReason) {
        self.idr_backoff = 0;
        self.begin_epoch(match reason {
            ResetReason::Resolution => IdrCause::ResolutionChange,
            ResetReason::DecoderFailure | ResetReason::NoDecoder | ResetReason::GateReset => {
                IdrCause::ClientReset
            }
        });
    }

    /// The tile codec handed the screen back to H.264 (design 5.6). The first
    /// frame is a re-root P from the newest confirmed anchor, whatever the
    /// healthy chain was; with no anchor the generic path probes or re-sends
    /// the IDR.
    pub(crate) fn mode_reentry(&mut self) {
        if self.pending_idr.is_some() {
            return;
        }
        self.prev = None;
        self.stats.reentries += 1;
        if let Some(root) = self.anchor() {
            self.chain = Chain::Recovering {
                root,
                last_reroot_at: None,
                reroots: Vec::new(),
            };
        }
    }

    /// Update the smoothed RTT and suspect frames that missed their decode
    /// deadline. Call once per frame decision.
    pub(crate) fn tick(&mut self, now: Instant, srtt: Duration) {
        self.srtt = srtt;
        let n = self.table.suspect_overdue(now, self.candidate_timeout());
        self.stats.deadline_suspects += u64::from(n);
    }

    /// A transport ack for one fragment of a frame. Progress on the IDR keeps
    /// it from being given up on; when its last fragment is acked the decode
    /// deadline starts (a slow link spends longer than a deadline just
    /// delivering a large IDR).
    pub(crate) fn frag_acked(&mut self, seq: u32, frag_idx: u16, now: Instant) -> FragAck {
        let r = self.table.frag_acked(seq, frag_idx);
        if let (FragAck::Acked { complete }, true) = (r, Some(seq) == self.idr_seq) {
            self.idr_progress_at = Some(now);
            if complete {
                self.table.restart_clock(seq, now);
            }
        }
        r
    }

    /// A ledger verdict on any fragment of the frame.
    pub(crate) fn frame_suspect(&mut self, seq: u32) {
        // An IDR still being delivered is not lost yet: the loss path
        // retransmits its fragments.
        if self.table.idr_incomplete(seq) {
            return;
        }
        self.table.set_suspect(seq);
    }

    /// An unknown frame in an acknowledgement is counted once, not once per
    /// repeat of a cumulative mask.
    fn note_unknown(&mut self, seq: u32) {
        if self.unknown_hw.is_none_or(|hw| seq_lt(hw, seq)) {
            self.unknown_hw = Some(seq);
            self.stats.acks_unknown += 1;
        }
    }

    fn mark_decoded(&mut self, seq: u32) -> bool {
        if !self.table.set_decoded(seq) {
            return false;
        }
        let Some(slot) = self.table.get(seq).and_then(|f| f.mark_slot) else {
            return true;
        };
        let k = usize::from(slot);
        let s = &mut self.slots[k];
        if s.carrier == Some(seq) && s.occupant.is_some() {
            if !s.confirmed {
                s.confirmed = true;
                self.idr_backoff = 0;
                self.slot_confirmed_once[k] = true;
                self.stats.anchors_confirmed += 1;
                if self.table.get(seq).is_some_and(|f| f.kind != PicKind::Idr) {
                    self.stats.p_anchors_confirmed += 1;
                    if self.cfg.observed {
                        // The mark happened at the client: tell the library,
                        // which keeps the picture usable for recovery only
                        // from here.
                        self.events.push(BackendFeedback::LtrMarked {
                            idr_pic_id: self.idr_pic_id,
                            frame_num: s.marked_fn,
                        });
                    }
                }
            }
        } else {
            self.stats.late_confirm_ignored += 1;
        }
        true
    }

    pub(crate) fn frame_decoded(&mut self, seq: u32) {
        match self.table.state(seq) {
            None => return self.note_unknown(seq),
            Some(FrameState::Decoded | FrameState::Dead) => return,
            Some(FrameState::Suspect) => self.stats.suspect_overridden += 1,
            Some(FrameState::Sent) => {}
        }
        self.mark_decoded(seq);
        // A decoded frame proves the picture it predicted from was decoded
        // (the client's gate checks the reference before feeding): this
        // repairs a lost acknowledgement of an anchor or of the IDR.
        let mut cur = self.table.get(seq).and_then(|f| f.target);
        for _ in 0..super::tracker::TRACKER_RING {
            let Some(t) = cur else { break };
            if !matches!(
                self.table.state(t),
                Some(FrameState::Sent | FrameState::Suspect)
            ) {
                break;
            }
            self.mark_decoded(t);
            cur = self.table.get(t).and_then(|f| f.target);
        }
    }

    pub(crate) fn frame_refused(&mut self, seq: u32) {
        if self.table.get(seq).is_none() {
            return self.note_unknown(seq);
        }
        self.table.set_dead(seq);
    }

    /// The 0x0A frame ack: bit `i` of each mask is frame `newest_seq - i`.
    /// Cumulative, so repeats are expected and harmless.
    pub(crate) fn frame_ack(&mut self, newest_seq: u32, decoded_mask: u64, refused_mask: u64) {
        for i in 0..64u32 {
            let seq = newest_seq.wrapping_sub(i) & 0x7FFF_FFFF;
            if decoded_mask >> i & 1 == 1 {
                self.frame_decoded(seq);
            } else if refused_mask >> i & 1 == 1 {
                self.frame_refused(seq);
            }
        }
    }

    // ------------------------------------------------------------ planning

    fn candidate_timeout(&self) -> Duration {
        (self.srtt * 4).max(Duration::from_millis(250))
    }

    /// How long the IDR may go without progress, or without a decoded bit
    /// after it is delivered, before it is sent again: three decode
    /// deadlines, doubled per earlier re-send (up to four times).
    pub(crate) fn idr_timeout(&self) -> Duration {
        self.candidate_timeout() * 3 * (1 << self.idr_backoff.min(2))
    }

    fn reissue_idr(&mut self, frame_seq: u32) -> FramePlan {
        // The same IDR again: not a new cause.
        self.stats.idr_reissued += 1;
        let cause = self.idr_cause;
        // A retirement for size is not evidence of a slow link: only a
        // re-send for want of progress doubles the time allowed.
        if !std::mem::take(&mut self.idr_mtu_dead) {
            self.idr_backoff += 1;
        }
        self.begin_epoch(cause);
        self.idr_plan(frame_seq, cause)
    }

    fn a_min(&self) -> u64 {
        let ceil = self
            .srtt
            .as_nanos()
            .div_ceil(self.cfg.frame_interval.as_nanos().max(1));
        (ceil as u64).clamp(2, 30)
    }

    /// A candidate sent, unresolved and not yet timed out.
    fn candidate_pending(&self, now: Instant) -> bool {
        let timeout = self.candidate_timeout();
        self.slots.iter().any(|s| {
            !s.confirmed
                && s.occupant.is_some_and(|seq| {
                    self.table.state(seq) == Some(FrameState::Sent)
                        && s.sent_at
                            .is_some_and(|t| now.saturating_duration_since(t) < timeout)
                })
        })
    }

    /// The slot a new candidate goes into, if one is due.
    fn mark_slot_if_due(&self, now: Instant) -> Option<u8> {
        if self.candidate_pending(now) {
            return None;
        }
        let slot = match self.anchor() {
            Some(a) => {
                let age = self.sent_count - self.slots[usize::from(a.slot)].sent_idx;
                (age >= self.a_min()).then_some(1 - a.slot)?
            }
            None => 1,
        };
        Some(slot)
    }

    /// Whether a frame with this target can carry a mark into `slot`. A mark
    /// that must drop a short-term picture names the frame's own target
    /// (`mark_ops`), so it waits for a frame whose target is short-term.
    fn mark_fits(&self, slot: u8, target: RefTarget) -> bool {
        !self.mark_drops_st(slot) || matches!(target, RefTarget::Short { .. })
    }

    /// Whether marking into `slot` has to name a short-term picture to
    /// forget: always into a slot the client may lack (its copy of the
    /// candidate may never have arrived, leaving it a short-term picture
    /// more than we count), else only when our own DPB would overfill.
    fn mark_drops_st(&self, slot: u8) -> bool {
        let lt_after = self.dpb.lt_count() + usize::from(self.dpb.lt[usize::from(slot)].is_none());
        !self.slot_confirmed_once[usize::from(slot)]
            || self.dpb.st.len() + lt_after > self.cfg.params.max_num_ref_frames as usize
    }

    fn resident(&self, seq: u32) -> bool {
        self.dpb.st.iter().any(|&(s, _)| s == seq)
            || self.dpb.lt.iter().flatten().any(|&(s, _)| s == seq)
    }

    fn locate(&self, seq: u32) -> Option<RefTarget> {
        if let Some(k) = self
            .dpb
            .lt
            .iter()
            .position(|s| s.is_some_and(|(q, _)| q == seq))
        {
            return Some(RefTarget::Long { slot: k as u8, seq });
        }
        self.dpb
            .st
            .iter()
            .find(|&&(s, _)| s == seq)
            .map(|&(seq, frame_num)| RefTarget::Short { seq, frame_num })
    }

    fn prev_usable(&self) -> bool {
        self.prev
            .is_some_and(|p| self.table.usable(p) && self.resident(p))
    }

    fn idr_plan(&self, frame_seq: u32, cause: IdrCause) -> FramePlan {
        FramePlan {
            epoch: self.epoch,
            frame_seq,
            kind: PicKind::Idr,
            idr_cause: Some(cause),
            nal_ref_idc: 3,
            frame_num: 0,
            poc: 0,
            target: None,
            intent: Intent::Executed,
            mark: Some(0),
            mark_target: false,
            mmco: Vec::new(),
            rplm: Vec::new(),
        }
    }

    /// MMCO 4 (max long-term idx 2), then MMCO 1 on the frame's own
    /// short-term target when the mark could overfill a DPB, then MMCO 6.
    ///
    /// The client's slot may be empty where ours is not: a candidate it never
    /// received left it with a non-existing short-term picture instead, and
    /// marking then costs it one more reference than we count. Until a
    /// slot's occupant has been confirmed decoded, the mark assumes the
    /// client's slot is empty and always drops a short-term picture.
    ///
    /// The picture it names must be a real one at every decoder. Pictures the
    /// client never received exist there, if at all, as "non-existing" frames
    /// from the `frame_num` gap process, and decoders disagree about which:
    /// libavcodec measures a gap against the previous picture, non-reference
    /// ones included, so it never infers a lost reference frame that shares
    /// its `frame_num` with a non-reference picture that arrived, and naming
    /// it fails with "unref short failure". The target of a healthy frame is
    /// the one picture the client gate guarantees (it admits the frame only
    /// if that picture is its newest reference), and nothing older is ever a
    /// target, while the marked frame becomes the new `prev`.
    fn mark_ops(&self, slot: u8, cur_frame_num: u32, target: RefTarget) -> Vec<Mmco> {
        let max_fn = self.cfg.params.max_frame_num();
        let mut ops = vec![Mmco::MaxLong(LT_SLOTS as u32)];
        if self.mark_drops_st(slot) {
            let RefTarget::Short { frame_num, .. } = target else {
                unreachable!("`mark_fits` keeps a dropping mark off long-term targets");
            };
            let d = (cur_frame_num + max_fn - frame_num) % max_fn;
            ops.push(Mmco::ForgetShort(d - 1));
        }
        ops.push(Mmco::CurrentToLong(u32::from(slot)));
        ops
    }

    fn p_plan(
        &self,
        frame_seq: u32,
        kind: PicKind,
        target: RefTarget,
        mark: Option<u8>,
    ) -> FramePlan {
        let max_fn = self.cfg.params.max_frame_num();
        let frame_num = (self.prev_ref_frame_num + 1) % max_fn;
        let rplm = match target {
            RefTarget::Short { frame_num: t, .. } => {
                let d = (frame_num + max_fn - t) % max_fn;
                debug_assert!(d != 0, "a short-term target cannot share frame_num");
                vec![Rplm::Sub(d.saturating_sub(1))]
            }
            RefTarget::Long { slot, .. } => vec![Rplm::Long(u32::from(slot))],
        };
        FramePlan {
            epoch: self.epoch,
            frame_seq,
            kind,
            idr_cause: None,
            nal_ref_idc: if kind == PicKind::Recovery { 0 } else { 2 },
            frame_num,
            poc: 2 * self.poc_count,
            target: Some(target),
            intent: Intent::Executed,
            mark,
            mark_target: false,
            mmco: mark.map_or_else(Vec::new, |k| self.mark_ops(k, frame_num, target)),
            rplm,
        }
    }

    /// No chain to continue and no confirmed anchor: the epoch's IDR is the
    /// only candidate. Probe it with a non-reference P (the client's gate
    /// refuses it if the IDR is missing; if it is there, handling it returns
    /// the IDR's decoded bit), and send the IDR again when it is dead or has
    /// been silent for three decode deadlines.
    fn no_anchor_plan(&mut self, frame_seq: u32, now: Instant) -> FramePlan {
        let idr = self.idr_seq.and_then(|s| self.table.get(s));
        let give_up = match idr {
            None => true,
            Some(f) => match f.state {
                FrameState::Dead => true,
                // `sent_at` restarted when the last fragment was acked.
                FrameState::Suspect => {
                    now.saturating_duration_since(f.sent_at) > self.idr_timeout()
                }
                FrameState::Sent | FrameState::Decoded => false,
            },
        };
        let held = self.dpb.lt[0].filter(|&(s, _)| Some(s) == self.idr_seq);
        match (give_up, held) {
            (false, Some((seq, _))) => {
                self.stats.idr_probes += 1;
                self.p_plan(
                    frame_seq,
                    PicKind::Recovery,
                    RefTarget::Long { slot: 0, seq },
                    None,
                )
            }
            _ => self.reissue_idr(frame_seq),
        }
    }

    /// Decide the next frame. Pure with respect to the DPB: the plan takes
    /// effect only when [`frame_sent`](Self::frame_sent) or
    /// [`frame_dropped`](Self::frame_dropped) is called with it. (The chain
    /// state, derived from events, is refreshed here.)
    pub(crate) fn plan_next(&mut self, frame_seq: u32, now: Instant) -> Plan {
        if let Some(cause) = self.pending_idr {
            return Plan::Frame(self.idr_plan(frame_seq, cause));
        }
        // No P frame until every IDR fragment is acknowledged (design 5.4):
        // the client window would evict an unfinished IDR. Delivery that makes
        // no progress for a timeout is sent again.
        if self.idr_seq.is_some_and(|i| self.table.idr_incomplete(i)) {
            let since = self.idr_progress_at.unwrap_or(now);
            if now.saturating_duration_since(since) <= self.idr_timeout() {
                return Plan::Hold;
            }
            return Plan::Frame(self.reissue_idr(frame_seq));
        }
        self.plan_p(frame_seq, now)
    }

    fn plan_p(&mut self, frame_seq: u32, now: Instant) -> Plan {
        // Leave recovery once the newest re-root is decoded: it is then the
        // newest reference frame sent, which is what `prev` must be. Leaving
        // for an older decoded re-root while a newer one is outstanding (or
        // lost) would put `prev` behind frames the client may have to
        // gap-fill over, evicting it, or ahead of the client's newest
        // reference, which the gate refuses.
        if let Chain::Recovering { reroots, .. } = &self.chain {
            if let Some(&last) = reroots.last() {
                // Observed: the re-root need not stay resident (the library
                // keeps one short-term picture), only be decoded.
                if self.table.state(last) == Some(FrameState::Decoded)
                    && (self.cfg.observed || self.resident(last))
                {
                    // Observed: the backend's chain runs on from its newest
                    // frame, which `prev` already is.
                    if !self.cfg.observed {
                        self.prev = Some(last);
                    }
                    self.chain = Chain::Healthy;
                    self.stats.recovered += 1;
                    self.table.supersede_suspects(last);
                }
            }
        }
        if matches!(self.chain, Chain::Healthy) && !self.prev_usable() {
            match self.anchor() {
                Some(root) => {
                    self.chain = Chain::Recovering {
                        root,
                        last_reroot_at: None,
                        reroots: Vec::new(),
                    };
                    self.stats.recoveries += 1;
                }
                None if self.cfg.observed => return self.no_anchor_observed(frame_seq, now),
                None => return Plan::Frame(self.no_anchor_plan(frame_seq, now)),
            }
        }
        if self.cfg.observed {
            return Plan::Frame(self.plan_observed(frame_seq, now));
        }

        let mark_due = self.mark_slot_if_due(now);
        let interval = self.srtt.max(self.cfg.frame_interval);
        let fresh_root = self.anchor();
        if let (Chain::Recovering { root, .. }, Some(a)) = (&mut self.chain, fresh_root) {
            *root = a;
        }
        match &self.chain {
            Chain::Healthy => {
                let prev = self.prev.expect("healthy chain has a usable prev");
                let target = self.locate(prev).expect("usable prev is resident");
                // `prev` may be a marked candidate that was lost, so the
                // client's slot can hold an older picture. The client gate's
                // check of `ref_frame_seq` against its slot content is what
                // makes that safe (design 7.1): it must not be weakened.
                let mark = mark_due.filter(|&k| self.mark_fits(k, target));
                Plan::Frame(self.p_plan(frame_seq, PicKind::Healthy, target, mark))
            }
            Chain::Recovering {
                root,
                last_reroot_at,
                reroots,
            } => {
                let target = RefTarget::Long {
                    slot: root.slot,
                    seq: root.seq,
                };
                // One re-root at a time: the next only once the previous is
                // unusable (lost, refused or past its deadline), never on a
                // timer. Two outstanding re-roots would leave the client's
                // newest reference ahead of the one the chain continues from.
                let due = match reroots.last() {
                    None => true,
                    Some(&r) => {
                        !self.table.usable(r)
                            && last_reroot_at
                                .is_none_or(|t| now.saturating_duration_since(t) >= interval)
                    }
                };
                if due {
                    let mark = mark_due.filter(|&k| self.mark_fits(k, target));
                    Plan::Frame(self.p_plan(frame_seq, PicKind::Reroot, target, mark))
                } else {
                    Plan::Frame(self.p_plan(frame_seq, PicKind::Recovery, target, None))
                }
            }
        }
    }

    // ------------------------------------------------- observed-mode planning

    /// A plan with only an intent: the backend picks the reference, the
    /// marking and the list modification, the header says which.
    fn observed_plan(&self, frame_seq: u32, kind: PicKind, intent: Intent) -> FramePlan {
        let max_fn = self.cfg.params.max_frame_num();
        FramePlan {
            epoch: self.epoch,
            frame_seq,
            kind,
            idr_cause: None,
            nal_ref_idc: 3,
            frame_num: (self.prev_ref_frame_num + 1) % max_fn,
            poc: 2 * self.poc_count,
            target: None,
            intent,
            mark: None,
            mark_target: false,
            mmco: Vec::new(),
            rplm: Vec::new(),
        }
    }

    /// No confirmed anchor and the chain is broken. There are no non-reference
    /// probes to make here, so the frame waits for the IDR to be decoded; an
    /// IDR that is dead or silent for its timeout is sent again.
    fn no_anchor_observed(&mut self, frame_seq: u32, now: Instant) -> Plan {
        let give_up = match self.idr_seq.and_then(|s| self.table.get(s)) {
            None => true,
            Some(f) => match f.state {
                FrameState::Dead => true,
                FrameState::Suspect => {
                    now.saturating_duration_since(f.sent_at) > self.idr_timeout()
                }
                FrameState::Sent | FrameState::Decoded => false,
            },
        };
        if give_up {
            Plan::Frame(self.reissue_idr(frame_seq))
        } else {
            Plan::Hold
        }
    }

    /// `LtrMarkFailed` for every mark whose carrier is refused, dropped, gone
    /// from the table, or has been suspect for `candidate_timeout`: the
    /// library marks nothing again until it is told. The slot's content is no
    /// longer an anchor candidate.
    ///
    /// A carrier that is merely suspect is not reported: the ledger's verdict
    /// is a hypothesis, most of them under loss are datagrams still queued,
    /// and a decoded acknowledgement that follows confirms the mark (as in
    /// explicit mode). The report is irreversible, so it waits.
    fn judge_marks(&mut self, now: Instant) {
        let patience = self.candidate_timeout();
        for k in 0..LT_SLOTS {
            let s = self.slots[k];
            let (Some(_), Some(c), false, false) =
                (s.occupant, s.carrier, s.confirmed, s.failed_sent)
            else {
                continue;
            };
            let rec = self.table.get(c);
            if rec.is_some_and(|f| f.kind == PicKind::Idr) {
                continue;
            }
            let lost = rec.is_none_or(|f| match f.state {
                FrameState::Dead => true,
                FrameState::Suspect => now.saturating_duration_since(f.sent_at) > patience,
                FrameState::Sent | FrameState::Decoded => false,
            });
            if lost {
                self.events.push(BackendFeedback::LtrMarkFailed {
                    idr_pic_id: self.idr_pic_id,
                    frame_num: s.marked_fn,
                });
                self.stats.ltr_mark_failed_sent += 1;
                self.slots[k] = Slot {
                    failed_sent: true,
                    ..Slot::EMPTY
                };
            }
        }
    }

    fn plan_observed(&mut self, frame_seq: u32, now: Instant) -> FramePlan {
        self.judge_marks(now);
        let interval = self.srtt.max(self.cfg.frame_interval);
        let fresh_root = self.anchor();
        if let (Chain::Recovering { root, .. }, Some(a)) = (&mut self.chain, fresh_root) {
            *root = a;
        }
        let Chain::Recovering { root, reroots, .. } = &self.chain else {
            return self.observed_plan(frame_seq, PicKind::Healthy, Intent::Healthy);
        };
        let root = *root;
        // One recovery at a time: the next only once the previous re-root is
        // unusable, and at most one per round trip.
        let due = reroots.last().is_none_or(|&r| !self.table.usable(r))
            && self
                .last_intent_at
                .is_none_or(|t| now.saturating_duration_since(t) >= interval);
        if !due {
            // Frames after a re-root depend on it; they are sent, and they
            // die with it if it dies.
            return self.observed_plan(frame_seq, PicKind::Healthy, Intent::Healthy);
        }
        let slot = &self.slots[usize::from(root.slot)];
        // The newest frame number encoded, as it stands: the library compares
        // frame numbers modulo `MaxFrameNum` and ignores a request whose
        // current is behind its last recovery's, so a number remembered
        // across the wrap (a `max`) would be behind it for good.
        let current = self.newest_fn;
        self.events.push(BackendFeedback::LtrRecover {
            idr_pic_id: self.idr_pic_id,
            // The carrier's number: the marked picture's own is an unknown
            // frame to the library.
            last_correct_frame_num: slot.carrier_fn,
            current_frame_num: current,
        });
        self.last_intent_at = Some(now);
        self.stats.ltr_recoveries_requested += 1;
        self.observed_plan(frame_seq, PicKind::Reroot, Intent::Recover { root })
    }

    /// The plan as the stream executed it, read from the emitted header
    /// (observed mode). Pure.
    pub(crate) fn observe(&self, plan: &FramePlan, au: &SliceHeader) -> FramePlan {
        self.observe_inner(plan, au).plan
    }

    fn observe_inner(&self, plan: &FramePlan, au: &SliceHeader) -> Observed {
        let seq = plan.frame_seq;
        if au.idr {
            let (cause, unsolicited) = match (plan.kind, plan.idr_cause) {
                (PicKind::Idr, c) => (c, false),
                _ => (Some(IdrCause::BackendFallback), true),
            };
            let mut p = self.idr_plan(seq, cause.unwrap_or(self.idr_cause));
            if unsolicited {
                // `frame_sent` starts this epoch before it adopts the frame.
                p.epoch = self.epoch + 1;
            }
            p.intent = Intent::Healthy;
            p.nal_ref_idc = au.nal_ref_idc;
            p.mark = au.long_term_reference_flag.then_some(0);
            return Observed {
                plan: p,
                mark: au.long_term_reference_flag.then_some(MarkSeen {
                    slot: 0,
                    marked_seq: seq,
                    marked_fn: 0,
                }),
                unrepresentable: Vec::new(),
                unsolicited,
            };
        }
        let eff = self.enc_model.peek(seq, au);
        let target = match eff.reference {
            Some(RefPic::Real(s)) => self.locate(s),
            _ => None,
        };
        let kind = match (au.nal_ref_idc, &self.chain, target) {
            (0, ..) => PicKind::Recovery,
            // A P from any confirmed anchor is a re-root: when the carrier's
            // decoded ack and the loss reach the library in one batch it
            // recovers from the older anchor (it latches mark feedback for
            // the next frame), and that frame is as good.
            (_, Chain::Recovering { .. }, Some(t)) if self.is_confirmed_anchor(t.seq()) => {
                PicKind::Reroot
            }
            _ => PicKind::Healthy,
        };
        // The marking, from the change of long-term occupancy.
        let mut dpb = self.dpb.clone();
        Self::apply_au_to(&mut dpb, &self.cfg.params, seq, au);
        let mut mark = None;
        let mut unrepresentable = Vec::new();
        for k in 0..LT_SLOTS {
            let (before, after) = (self.dpb.lt[k], dpb.lt[k]);
            let Some((marked_seq, marked_fn)) = after.filter(|&a| Some(a) != before) else {
                continue;
            };
            let on_wire = marked_seq == seq || target.is_some_and(|t| t.seq() == marked_seq);
            if on_wire && mark.is_none() {
                mark = Some(MarkSeen {
                    slot: k as u8,
                    marked_seq,
                    marked_fn,
                });
            } else {
                unrepresentable.push(marked_fn);
            }
        }
        let mut p = self.observed_plan(seq, kind, Intent::Healthy);
        p.epoch = plan.epoch;
        p.nal_ref_idc = au.nal_ref_idc;
        p.frame_num = au.frame_num;
        p.target = target;
        p.mark = mark.map(|m| m.slot);
        p.mark_target = mark.is_some_and(|m| m.marked_seq != seq);
        p.mmco = au.mmco.clone().unwrap_or_default();
        p.rplm = au.rplm.clone();
        p.intent = plan.intent;
        Observed {
            plan: p,
            mark,
            unrepresentable,
            unsolicited: false,
        }
    }

    fn commit_observed(
        &mut self,
        plan: &FramePlan,
        au: &SliceHeader,
        frags_total: u16,
        now: Instant,
        fate: Fate,
    ) -> Option<SentReport> {
        let seq = plan.frame_seq;
        let mut obs = self.observe_inner(plan, au);
        if obs.unsolicited {
            // The library made an IDR the manager did not ask for: a new
            // epoch, with this frame as its IDR.
            self.begin_epoch(IdrCause::BackendFallback);
            obs = self.observe_inner(plan, au);
            obs.plan.epoch = self.epoch;
        }
        let eff = obs.plan.clone();
        let effective = self.enc_model.push(seq, au);
        let matched = !effective.violations.any()
            && (au.idr || matches!(effective.reference, Some(RefPic::Real(_))));
        if fate == Fate::Sent && !matched {
            self.stats.plan_mismatch += 1;
        }
        let usable = matched && fate == Fate::Sent;
        self.apply_au(seq, au);
        // A mark the client cannot be told about never gets a confirmation;
        // the library marks nothing again until it hears the mark failed.
        for &marked_fn in &obs.unrepresentable {
            self.stats.mark_unrepresentable += 1;
            self.stats.ltr_mark_failed_sent += 1;
            self.events.push(BackendFeedback::LtrMarkFailed {
                idr_pic_id: self.idr_pic_id,
                frame_num: marked_fn,
            });
        }
        // A slot the stream no longer holds a picture in is not an anchor.
        for k in 0..LT_SLOTS {
            if self.slots[k].occupant.is_some()
                && self.slots[k].occupant != self.dpb.lt[k].map(|(s, _)| s)
            {
                self.slots[k] = Slot::EMPTY;
            }
        }
        let mut rec = FrameRec::new(seq, eff.kind, frags_total, now, self.sent_count);
        rec.target = eff.ref_frame_seq();
        rec.reference = au.nal_ref_idc != 0;
        rec.mark_slot = obs.mark.map(|m| m.slot);
        rec.dep = match eff.kind {
            PicKind::Healthy | PicKind::Recovery => rec.target,
            PicKind::Idr | PicKind::Reroot => None,
        };
        if !usable {
            rec.state = FrameState::Dead;
        }
        self.table.insert(rec);
        if let Some(m) = obs.mark {
            self.slots[usize::from(m.slot)] = Slot {
                occupant: Some(m.marked_seq),
                carrier: Some(seq),
                marked_fn: m.marked_fn,
                carrier_fn: au.frame_num,
                confirmed: false,
                failed_sent: false,
                sent_idx: self.sent_count,
                sent_at: Some(now),
            };
            if !au.idr {
                self.stats.candidates_marked += 1;
                self.stats.marks_observed += 1;
            }
        }
        self.newest_fn = au.frame_num;
        if au.idr {
            self.slots[1] = if obs.mark.is_some_and(|m| m.slot == 1) {
                self.slots[1]
            } else {
                Slot::EMPTY
            };
            self.slot_confirmed_once = [false; LT_SLOTS];
            self.idr_seq = Some(seq);
            self.idr_pic_id = au.idr_pic_id;
            self.idr_progress_at = Some(now);
            self.prev = usable.then_some(seq);
            self.chain = Chain::Healthy;
            self.pending_idr = None;
            match eff.idr_cause {
                Some(IdrCause::SessionStart) => self.stats.idr_session += 1,
                Some(IdrCause::ResolutionChange) => self.stats.idr_resolution += 1,
                Some(IdrCause::ClientReset) => self.stats.idr_client_reset += 1,
                Some(IdrCause::EncoderFault) => self.stats.idr_encoder_fault += 1,
                Some(IdrCause::MtuShrink) => self.stats.idr_mtu_shrink += 1,
                // Only an IDR that reached the wire: the bridge counts the
                // IDRs it sent, and a dropped one is not among them.
                Some(IdrCause::BackendFallback) if fate == Fate::Sent => {
                    self.stats.idr_backend_fallback += 1
                }
                Some(IdrCause::BackendFallback) | None => {}
            }
        } else {
            if au.nal_ref_idc != 0 {
                // The backend's chain continues from its newest reference
                // frame, whether or not the client will get it.
                self.prev = usable.then_some(seq);
            }
            match eff.kind {
                PicKind::Healthy => self.stats.p_healthy += 1,
                PicKind::Recovery => self.stats.p_recovery += 1,
                PicKind::Reroot => {
                    self.stats.reroots += 1;
                    if let Chain::Recovering {
                        last_reroot_at,
                        reroots,
                        ..
                    } = &mut self.chain
                    {
                        *last_reroot_at = Some(now);
                        reroots.push(seq);
                    }
                }
                PicKind::Idr => {}
            }
        }
        self.sent_count += 1;
        Some(SentReport {
            matched,
            effective,
            plan: eff,
        })
    }

    // --------------------------------------------------------- frame events

    /// The caller encoded the plan's frame and queued it. `au` is the slice
    /// header parsed from the emitted stream (what the backend really wrote).
    /// The manager replays it through its own encoder-side model: a header
    /// whose effective reference is not the plan's, or that breaks a DPB rule,
    /// is a mismatch. Such a frame is dead on arrival and never becomes
    /// `prev`; three in a row restart the stream with an `EncoderFault` IDR.
    /// A plan from an older epoch is ignored (`None`).
    pub(crate) fn frame_sent(
        &mut self,
        plan: &FramePlan,
        au: &SliceHeader,
        frags_total: u16,
        now: Instant,
    ) -> Option<SentReport> {
        self.commit(plan, au, frags_total, now, Fate::Sent)
    }

    /// The caller encoded the plan's frame and then dropped it locally. The
    /// encoder's DPB has the picture but the client never will, so it is dead
    /// on arrival and never becomes `prev`.
    pub(crate) fn frame_dropped(&mut self, plan: &FramePlan, au: &SliceHeader, now: Instant) {
        self.commit(plan, au, 0, now, Fate::Dropped);
    }

    fn commit(
        &mut self,
        plan: &FramePlan,
        au: &SliceHeader,
        frags_total: u16,
        now: Instant,
        fate: Fate,
    ) -> Option<SentReport> {
        if plan.epoch != self.epoch {
            return None;
        }
        if self.cfg.observed {
            return self.commit_observed(plan, au, frags_total, now, fate);
        }
        let seq = plan.frame_seq;
        let effective = self.enc_model.push(seq, au);
        let same = match (plan.ref_frame_seq(), effective.reference) {
            (None, None) => true,
            (Some(s), Some(RefPic::Real(e))) => s == e,
            _ => false,
        };
        let matched = same && !effective.violations.any();
        if fate == Fate::Sent {
            if matched {
                self.mismatch_run = 0;
            } else {
                self.stats.plan_mismatch += 1;
                self.mismatch_run += 1;
            }
        }
        let usable = matched && fate == Fate::Sent;
        let mark = self.apply_au(seq, au);

        let mut rec = FrameRec::new(seq, plan.kind, frags_total, now, self.sent_count);
        rec.target = plan.ref_frame_seq();
        rec.reference = au.nal_ref_idc != 0;
        rec.mark_slot = mark;
        rec.dep = match plan.kind {
            PicKind::Healthy => rec.target,
            PicKind::Idr | PicKind::Recovery | PicKind::Reroot => None,
        };
        if !usable {
            rec.state = FrameState::Dead;
        }
        self.table.insert(rec);

        if let Some(k) = mark {
            self.slots[usize::from(k)] = Slot {
                occupant: Some(seq),
                carrier: Some(seq),
                marked_fn: au.frame_num,
                carrier_fn: au.frame_num,
                confirmed: false,
                failed_sent: false,
                sent_idx: self.sent_count,
                sent_at: Some(now),
            };
            if !au.idr {
                self.stats.candidates_marked += 1;
            }
        }
        if au.idr {
            self.slots[1] = Slot::EMPTY;
            self.slot_confirmed_once = [false; LT_SLOTS];
            self.idr_seq = Some(seq);
            self.idr_progress_at = Some(now);
            self.prev = usable.then_some(seq);
            self.chain = Chain::Healthy;
            self.pending_idr = None;
            match plan.idr_cause {
                Some(IdrCause::SessionStart) => self.stats.idr_session += 1,
                Some(IdrCause::ResolutionChange) => self.stats.idr_resolution += 1,
                Some(IdrCause::ClientReset) => self.stats.idr_client_reset += 1,
                Some(IdrCause::EncoderFault) => self.stats.idr_encoder_fault += 1,
                Some(IdrCause::MtuShrink) => self.stats.idr_mtu_shrink += 1,
                Some(IdrCause::BackendFallback) => self.stats.idr_backend_fallback += 1,
                None => {}
            }
        } else {
            match plan.kind {
                PicKind::Healthy => {
                    if usable {
                        self.prev = Some(seq);
                    } else if au.nal_ref_idc != 0 {
                        // A reference frame the client will not get is now
                        // the newest in the encoder's DPB: the client would
                        // gap-fill over it, so no older picture is a safe
                        // target. Re-root from the anchor.
                        self.prev = None;
                    }
                    self.stats.p_healthy += 1;
                }
                PicKind::Recovery => self.stats.p_recovery += 1,
                PicKind::Reroot => {
                    self.stats.reroots += 1;
                    if let Chain::Recovering {
                        last_reroot_at,
                        reroots,
                        ..
                    } = &mut self.chain
                    {
                        *last_reroot_at = Some(now);
                        reroots.push(seq);
                    }
                }
                PicKind::Idr => {}
            }
        }
        self.sent_count += 1;
        if self.mismatch_run >= 3 {
            self.begin_epoch(IdrCause::EncoderFault);
        }
        Some(SentReport {
            matched,
            effective,
            plan: plan.clone(),
        })
    }

    /// Apply an emitted header to the encoder-side DPB. Returns the long-term
    /// slot the picture was marked into.
    fn apply_au(&mut self, seq: u32, au: &SliceHeader) -> Option<u8> {
        self.poc_count = if au.idr { 1 } else { self.poc_count + 1 };
        let mut dpb = std::mem::take(&mut self.dpb);
        let mark = Self::apply_au_to(&mut dpb, &self.cfg.params, seq, au);
        self.dpb = dpb;
        if au.idr {
            self.prev_ref_frame_num = 0;
        } else if au.nal_ref_idc != 0 {
            self.prev_ref_frame_num = au.frame_num;
        }
        mark
    }

    /// The DPB after an emitted header (marking and sliding window), and the
    /// long-term slot the picture itself was marked into.
    fn apply_au_to(
        dpb: &mut EncDpb,
        params: &StreamParams,
        seq: u32,
        au: &SliceHeader,
    ) -> Option<u8> {
        let max_fn = params.max_frame_num();
        let max_refs = params.max_num_ref_frames as usize;
        if au.idr {
            *dpb = EncDpb::default();
            if au.long_term_reference_flag {
                dpb.lt[0] = Some((seq, 0));
                return Some(0);
            }
            dpb.st.push((seq, 0));
            return None;
        }
        if au.nal_ref_idc == 0 {
            return None;
        }
        let cur = au.frame_num;
        let mut mark = None;
        match &au.mmco {
            None => {
                if dpb.len() >= max_refs {
                    if let Some(i) = dpb.oldest_st(cur, max_fn) {
                        dpb.st.remove(i);
                    }
                }
            }
            Some(ops) => {
                for op in ops {
                    match *op {
                        Mmco::ForgetShort(d) => {
                            let fnum = (cur + max_fn - ((d + 1) % max_fn)) % max_fn;
                            dpb.st.retain(|&(_, f)| f != fnum);
                        }
                        Mmco::ForgetLong(n) => {
                            if let Some(s) = dpb.lt.get_mut(n as usize) {
                                *s = None;
                            }
                        }
                        Mmco::ShortToLong(d, idx) => {
                            let fnum = (cur + max_fn - ((d + 1) % max_fn)) % max_fn;
                            if let Some(i) = dpb.st.iter().position(|&(_, f)| f == fnum) {
                                let p = dpb.st.remove(i);
                                if let Some(s) = dpb.lt.get_mut(idx as usize) {
                                    *s = Some(p);
                                }
                            }
                        }
                        Mmco::MaxLong(p) => {
                            for (k, s) in dpb.lt.iter_mut().enumerate() {
                                if k as u32 >= p {
                                    *s = None;
                                }
                            }
                        }
                        Mmco::ClearAll => *dpb = EncDpb::default(),
                        Mmco::CurrentToLong(k) => mark = Some(k as u8),
                    }
                }
            }
        }
        match mark.and_then(|k| dpb.lt.get_mut(usize::from(k))) {
            Some(s) => *s = Some((seq, cur)),
            None => {
                mark = None;
                dpb.st.push((seq, cur));
            }
        }
        mark
    }
}
