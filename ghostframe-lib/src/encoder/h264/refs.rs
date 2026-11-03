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
    /// The long-term slot this frame is marked into (candidate anchor).
    pub mark: Option<u8>,
    pub mmco: Vec<Mmco>,
    pub rplm: Vec<Rplm>,
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
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RefStats {
    pub idr_session: u64,
    pub idr_resolution: u64,
    pub idr_client_reset: u64,
    pub idr_encoder_fault: u64,
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
}

/// What [`RefManager::frame_sent`] found when it replayed the emitted header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SentReport {
    /// The header's effective reference is the plan's and nothing broke a rule.
    pub matched: bool,
    pub effective: Effective,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    /// The last frame sent with MMCO 6 into this slot.
    occupant: Option<u32>,
    confirmed: bool,
    sent_idx: u64,
    sent_at: Option<Instant>,
}

impl Slot {
    const EMPTY: Self = Self {
        occupant: None,
        confirmed: false,
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
#[derive(Debug, Default)]
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
    stats: RefStats,
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
            stats: RefStats::default(),
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
        self.pending_idr = Some(cause);
    }

    pub(crate) fn session_reset(&mut self) {
        self.idr_backoff = 0;
        self.begin_epoch(IdrCause::SessionStart);
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
        if s.occupant == Some(seq) {
            if !s.confirmed {
                s.confirmed = true;
                self.idr_backoff = 0;
                self.slot_confirmed_once[k] = true;
                self.stats.anchors_confirmed += 1;
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
            Some(FrameState::Sent | FrameState::Suspect) => {}
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
    fn idr_timeout(&self) -> Duration {
        self.candidate_timeout() * 3 * (1 << self.idr_backoff.min(2))
    }

    fn reissue_idr(&mut self, frame_seq: u32) -> FramePlan {
        // The same IDR again: not a new cause.
        self.stats.idr_reissued += 1;
        let cause = self.idr_cause;
        self.idr_backoff += 1;
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
            mark: Some(0),
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
            mark,
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
        Plan::Frame(self.plan_p(frame_seq, now))
    }

    fn plan_p(&mut self, frame_seq: u32, now: Instant) -> FramePlan {
        // Leave recovery once the newest re-root is decoded: it is then the
        // newest reference frame sent, which is what `prev` must be. Leaving
        // for an older decoded re-root while a newer one is outstanding (or
        // lost) would put `prev` behind frames the client may have to
        // gap-fill over, evicting it, or ahead of the client's newest
        // reference, which the gate refuses.
        if let Chain::Recovering { reroots, .. } = &self.chain {
            if let Some(&last) = reroots.last() {
                if self.table.state(last) == Some(FrameState::Decoded) && self.resident(last) {
                    self.prev = Some(last);
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
                None => return self.no_anchor_plan(frame_seq, now),
            }
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
                self.p_plan(frame_seq, PicKind::Healthy, target, mark)
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
                    self.p_plan(frame_seq, PicKind::Reroot, target, mark)
                } else {
                    self.p_plan(frame_seq, PicKind::Recovery, target, None)
                }
            }
        }
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
                confirmed: false,
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
        Some(SentReport { matched, effective })
    }

    /// Apply an emitted header to the encoder-side DPB. Returns the long-term
    /// slot the picture was marked into.
    fn apply_au(&mut self, seq: u32, au: &SliceHeader) -> Option<u8> {
        let max_fn = self.cfg.params.max_frame_num();
        let max_refs = self.cfg.params.max_num_ref_frames as usize;
        self.poc_count = if au.idr { 1 } else { self.poc_count + 1 };
        if au.idr {
            self.dpb = EncDpb::default();
            self.prev_ref_frame_num = 0;
            if au.long_term_reference_flag {
                self.dpb.lt[0] = Some((seq, 0));
                return Some(0);
            }
            self.dpb.st.push((seq, 0));
            return None;
        }
        if au.nal_ref_idc == 0 {
            return None;
        }
        let cur = au.frame_num;
        let mut mark = None;
        match &au.mmco {
            None => {
                if self.dpb.len() >= max_refs {
                    if let Some(i) = self.dpb.oldest_st(cur, max_fn) {
                        self.dpb.st.remove(i);
                    }
                }
            }
            Some(ops) => {
                for op in ops {
                    match *op {
                        Mmco::ForgetShort(d) => {
                            let fnum = (cur + max_fn - ((d + 1) % max_fn)) % max_fn;
                            self.dpb.st.retain(|&(_, f)| f != fnum);
                        }
                        Mmco::ForgetLong(n) => {
                            if let Some(s) = self.dpb.lt.get_mut(n as usize) {
                                *s = None;
                            }
                        }
                        Mmco::ShortToLong(d, idx) => {
                            let fnum = (cur + max_fn - ((d + 1) % max_fn)) % max_fn;
                            if let Some(i) = self.dpb.st.iter().position(|&(_, f)| f == fnum) {
                                let p = self.dpb.st.remove(i);
                                if let Some(s) = self.dpb.lt.get_mut(idx as usize) {
                                    *s = Some(p);
                                }
                            }
                        }
                        Mmco::MaxLong(p) => {
                            for (k, s) in self.dpb.lt.iter_mut().enumerate() {
                                if k as u32 >= p {
                                    *s = None;
                                }
                            }
                        }
                        Mmco::ClearAll => self.dpb = EncDpb::default(),
                        Mmco::CurrentToLong(k) => mark = Some(k as u8),
                    }
                }
            }
        }
        match mark.and_then(|k| self.dpb.lt.get_mut(usize::from(k))) {
            Some(s) => *s = Some((seq, cur)),
            None => {
                mark = None;
                self.dpb.st.push((seq, cur));
            }
        }
        self.prev_ref_frame_num = cur;
        mark
    }
}
