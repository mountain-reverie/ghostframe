//! Acked-reference H.264 on the event loop (Phase B of the H.264 rework, design
//! sections 3 to 5).
//!
//! A session whose client sends the v2 feedback (HELLO 0x04) and whose
//! backend executes plans (libva-direct, or the harness's PCM backend) recovers
//! from loss without an IDR. The loop owns the [`RefManager`]; per frame it
//!
//! 1. plans ([`IoBridge::h264_refs_plan`]): the manager says IDR, healthy P,
//!    recovery P or hold, and the backend gets that plan with the resident
//!    reference set;
//! 2. hands the plan to the encode worker (or, in the browserless harness,
//!    runs the backend inline), which executes it;
//! 3. commits the access unit ([`IoBridge::h264_refs_commit`]): the wire header
//!    is written from the plan and the reference the backend really used, the
//!    frame goes through the normal H.264 send path, and the manager is told
//!    what was sent.
//!
//! Calls are strictly sequential: a plan is committed (or its session reset)
//! before the next is made, which the loop's one frame in flight guarantees. A
//! result made for an older plan epoch is dropped.
//!
//! What the manager hears, on these sessions:
//!
//! * fragment acknowledgements (`apply_h264_acks`) and loss verdicts
//!   (`note_h264_verdict_in`), from the ledger;
//! * the client's frame ack, 0x0A (`dispatch_frame_ack_datagram`);
//! * the client's reset request, 0x09 (`handle_reset_request`);
//! * a session change, a resolution change and the return from the tile codec.
//!
//! Not on these sessions: the periodic GOP IDR, the loss-triggered IDR, the
//! IDR hold (the manager's own `Plan::Hold`), the rate-change reopen (the rate
//! moves in place) and the H.264 XOR parity. A client without 0x04, or a
//! backend that cannot execute plans, keeps the Phase A policy (`h264_tx`).

use std::time::{Duration, Instant};

use super::frame::{stage_trace_enabled, InFlight, InFlightStage, Lap};
use super::frame_backend::{CodedAu, H264Request};
use super::*;
use crate::encoder::h264::backend::{EncodedAu, RateTarget};
use crate::encoder::h264::refs::{
    FramePlan, IdrCause as RefIdrCause, Plan, RefConfig, RefManager, RefStats, ResetReason,
};
use crate::encoder::h264::syntax::StreamParams;
use crate::encoder::h264::tracker::{seq_lt, FrameState, PicKind};

/// Encoder-fault epochs in a row, with no anchor confirmed between them, after
/// which acked references are given up on for the process (the Phase A encoder
/// takes over).
const MAX_BACKEND_FAILURES: u32 = 3;

/// Whether this process's backend executes plans, as far as the loop knows.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefsMode {
    /// No H.264 frame has been planned yet.
    #[default]
    Unknown,
    /// The backend answered a plan with a coded access unit.
    Explicit,
    /// The backend does not execute plans: Phase A for the rest of the process.
    Legacy,
}

/// What the loop counts of the acked-reference path itself (the manager counts
/// the rest, see [`RefsReport`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct RefsCounters {
    pub(super) plans: u64,
    pub(super) holds: u64,
    pub(super) epoch_dropped: u64,
    pub(super) backend_failures: u64,
    pub(super) unplanned_fallbacks: u64,
    pub(super) client_resets: u64,
    pub(super) frame_acks_applied: u64,
    pub(super) decoded_bits: u64,
    pub(super) refused_bits: u64,
    pub(super) reentries: u64,
    pub(super) reentry_bytes: u64,
    pub(super) anchor_age_ms: u64,
    pub(super) rate_updates: u64,
    /// Of `rate_updates`, those that raised the rate, and those that lowered it.
    pub(super) rate_applies_up: u64,
    pub(super) rate_applies_down: u64,
    pub(super) verdicts: u64,
    /// Frames retired because a fragment outgrew the connection (`emit.rs`):
    /// counted at the first oversize fragment of each frame, the siblings
    /// being purged with it.
    pub(super) mtu_retired: u64,
    /// Of those, frames that were the IDR in flight (abandoned, not suspect).
    pub(super) mtu_retired_idr: u64,
}

/// The loop-side state of one live session's acked references.
/// A frame is over its cap past this many percent of it (S2: a P frame goes
/// 8 % over a reachable cap).
const OVER_CAP_PCT: u32 = 115;

pub(super) struct RefsState {
    pub(super) mgr: RefManager,
    /// Frame size of the last plan: a change is a resolution change.
    dims: Option<(u32, u32)>,
    /// Encoder-fault epochs since an anchor was last confirmed, and the
    /// `anchors_confirmed` count at the last fault (a confirmation since then
    /// clears the run).
    faults_without_anchor: u32,
    p_anchors_at_fault: u64,
    /// Set when the stream returns from the tile codec: the age (ms) of the
    /// anchor the first frame predicts from, until that frame is sent.
    reentry_anchor_age_ms: Option<u64>,
    /// The frame in flight and the cap (bytes) its plan was coded under: the
    /// budget travels with the request, and the commit holds the result to it.
    inflight_cap: Option<(u32, u32)>,
}

impl RefsState {
    fn new(frame_interval: Duration, srtt: Duration) -> Self {
        Self {
            mgr: RefManager::new(RefConfig {
                params: StreamParams::writer(),
                frame_interval,
                initial_srtt: srtt,
            }),
            dims: None,
            faults_without_anchor: 0,
            p_anchors_at_fault: 0,
            reentry_anchor_age_ms: None,
            inflight_cap: None,
        }
    }
}

/// The `RefManager`'s counters and the loop's, for the harness and the
/// `h264 refs` log line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RefsReport {
    /// Acked references were in use at the time of the report.
    pub active: bool,
    /// The backend that executes the plans: `""` before the first frame.
    pub backend: &'static str,
    pub plans: u64,
    /// Frames the manager held for an IDR still being delivered.
    pub holds: u64,
    /// Results of an older plan epoch, dropped.
    pub epoch_dropped: u64,
    pub backend_failures: u64,
    /// Plans a backend could not execute: the process went back to Phase A.
    pub unplanned_fallbacks: u64,
    /// Client reset requests honoured (0x09).
    pub client_resets: u64,
    pub frame_acks_applied: u64,
    /// Frames the client reported decoded, and refused (each counted once).
    pub frames_decoded_acked: u64,
    pub frames_refused: u64,
    pub verdicts: u64,
    /// Frames retired unsent because the path MTU fell below a fragment's
    /// size (counted once per frame, at its first oversize fragment); of
    /// which `mtu_retired_idr` were the IDR in flight.
    pub mtu_retired: u64,
    pub mtu_retired_idr: u64,
    pub reentries: u64,
    /// Wire bytes of the first frame after each return from the tile codec,
    /// summed; and the age of the anchor it predicted from, last.
    pub reentry_bytes: u64,
    pub anchor_age_ms: u64,
    pub rate_updates: u64,
    pub rate_applies_up: u64,
    pub rate_applies_down: u64,
    // The manager's own counters.
    pub idr_session: u64,
    pub idr_resolution: u64,
    pub idr_client_reset: u64,
    pub idr_encoder_fault: u64,
    pub idr_mtu_shrink: u64,
    pub idr_reissued: u64,
    pub idr_probes: u64,
    pub p_healthy: u64,
    pub p_recovery: u64,
    pub reroots: u64,
    pub candidates_marked: u64,
    pub anchors_confirmed: u64,
    pub late_confirm_ignored: u64,
    pub recoveries: u64,
    pub recovered: u64,
    pub plan_mismatch: u64,
    pub deadline_suspects: u64,
    pub acks_unknown: u64,
    pub suspect_overridden: u64,
    pub p_anchors_confirmed: u64,
}

/// Which `IdrCause` an IDR the manager planned is counted under.
fn bridge_idr_cause(c: Option<RefIdrCause>) -> IdrCause {
    match c {
        Some(RefIdrCause::ResolutionChange) => IdrCause::Resolution,
        Some(RefIdrCause::ClientReset) => IdrCause::ClientReset,
        // Counted as a loss IDR in the bridge's table; the manager's own
        // `idr_encoder_fault` says what it was.
        Some(RefIdrCause::EncoderFault) => IdrCause::Loss,
        Some(RefIdrCause::MtuShrink) => IdrCause::MtuShrink,
        Some(RefIdrCause::SessionStart) | None => IdrCause::Session,
    }
}

fn reset_reason(r: ghostframe_protocol::feedback_stream::ResetReason) -> ResetReason {
    use ghostframe_protocol::feedback_stream::ResetReason as W;
    match r {
        W::DecoderFailure => ResetReason::DecoderFailure,
        W::Resolution => ResetReason::Resolution,
        W::NoDecoder => ResetReason::NoDecoder,
        W::GateReset => ResetReason::GateReset,
    }
}

impl IoBridge {
    /// Whether H.264 frames are planned by the `RefManager`: the client sends
    /// the v2 feedback, acked references are not switched off, and the backend
    /// has not turned out unable to execute plans.
    pub(super) fn h264_refs_active(&self) -> bool {
        self.h264.refs_enabled
            && self.client_caps.h264_acked_ref
            && self.h264.refs_mode != RefsMode::Legacy
    }

    /// Whether this session's frames are planned by a live manager: the
    /// routing test of every feedback path. A manager left from an earlier
    /// session never counts (`reset_session` drops it, and a client without
    /// 0x04 is not served by it).
    pub(super) fn h264_refs_live(&self) -> bool {
        self.h264_refs_active() && self.h264.refs.is_some()
    }

    fn refs_state(&mut self) -> &mut RefsState {
        let interval = Duration::from_secs_f64(1.0 / f64::from(self.h264.fps.max(1)));
        let srtt = self.reliable_emitter.rtt.srtt();
        self.h264
            .refs
            .get_or_insert_with(|| RefsState::new(interval, srtt))
    }

    /// Plan the next H.264 frame. `None`: the frame is skipped (the emission
    /// queue is backed up, or the manager holds for the IDR still being
    /// delivered); the caller drops the capture and makes the next one whole.
    pub(super) fn h264_refs_plan(
        &mut self,
        seq: u32,
        dims: (u32, u32),
        now: Instant,
        rate_policy: bool,
    ) -> Option<Box<H264Request>> {
        if rate_policy {
            self.h264_rate_step_refs(now);
        }
        let srtt = self.reliable_emitter.rtt.srtt();
        let st = self.refs_state();
        if st.dims.is_some_and(|d| d != dims) {
            st.mgr.resolution_change();
        }
        st.dims = Some(dims);
        // A new IDR supersedes what is queued, so a backed-up queue does not
        // stop one; everything else waits for the queue to drain.
        if st.mgr.pending_idr().is_none() && self.h264_backlog_exceeded() {
            self.h264.stats.frames_skipped_backpressure += 1;
            if !self.reliable_emitter.pads.is_empty() {
                self.h264.stats.frames_skipped_pad += 1;
            }
            return None;
        }
        let bps = self
            .h264
            .rate
            .applied()
            .unwrap_or(super::h264_rate::RATE_INITIAL_BPS);
        let fps = self.h264.fps;
        let st = self.refs_state();
        st.mgr.tick(now, srtt);
        let plan = match st.mgr.plan_next(seq, now) {
            Plan::Frame(p) => p,
            Plan::Hold => {
                self.h264.refs_counters.holds += 1;
                self.h264.stats.frames_skipped_hold += 1;
                return None;
            }
        };
        let refs = st.mgr.resident_refs();
        // The frame's budget: an IDR gets its own (a quarter second of the
        // rate), a P frame the cap of a few average frames.
        let rate = RateTarget::steady(bps, fps);
        let cap = if plan.kind == PicKind::Idr {
            rate.idr_max_bytes
        } else {
            rate.max_frame_bytes
        };
        st.inflight_cap = Some((plan.frame_seq, cap));
        self.h264.stats.frame_cap_bytes = rate.max_frame_bytes;
        self.h264.stats.idr_cap_bytes = rate.idr_max_bytes;
        self.h264.refs_counters.plans += 1;
        Some(Box::new(H264Request {
            plan,
            refs,
            fps,
            rate,
        }))
    }

    /// Hold the frame to the cap its plan carried: counted, never acted on.
    fn note_frame_cap(&mut self, seq: u32, len: usize) {
        let cap = self
            .h264
            .refs
            .as_mut()
            .and_then(|st| st.inflight_cap.take())
            .filter(|(s, c)| *s == seq && *c > 0);
        let Some((_, cap)) = cap else {
            return;
        };
        let pct = (len as u64 * 100 / u64::from(cap)).min(u64::from(u32::MAX)) as u32;
        self.h264.stats.max_cap_ratio_pct = self.h264.stats.max_cap_ratio_pct.max(pct);
        self.h264.stats.frames_over_cap += u64::from(pct > OVER_CAP_PCT);
    }

    /// The backend made `au` for `plan`: send it and tell the manager. Returns
    /// whether it went on the wire. A result of an older epoch is dropped.
    pub(super) fn h264_refs_commit(
        &mut self,
        plan: &FramePlan,
        au: EncodedAu,
        meta: &frame::FrameMeta,
        max_dg_size: usize,
        now: Instant,
    ) -> bool {
        let seq = plan.frame_seq;
        let Some(st) = self.h264.refs.as_mut() else {
            return false;
        };
        // The decode clock starts (strictly after the last progress) with the first frame sent that nothing
        // decoded yet: after idle, after a stretch of tile frames, at the start.
        if self
            .h264
            .last_sent_at
            .zip(self.h264.decode_clock)
            .is_none_or(|(sent, decoded)| sent < decoded)
        {
            self.h264.decode_clock = Some(now);
        }
        self.h264.last_sent_at = Some(now);
        if plan.epoch != st.mgr.epoch() {
            self.h264.refs_counters.epoch_dropped += 1;
            return false;
        }
        // A stream that does not parse, or whose IDR flag disagrees with the
        // plan, cannot be followed: the backend failed.
        let header = au
            .meta
            .header
            .clone()
            .filter(|_| au.meta.idr == (plan.kind == PicKind::Idr));
        let Some(header) = header else {
            self.h264_refs_encoder_failed(now);
            return false;
        };
        let described = frame::Described {
            info: plan.wire_info(au.meta.ref_seq),
            idr_cause: plan
                .idr_cause
                .map(|c| bridge_idr_cause(Some(c)))
                .filter(|_| au.meta.idr),
        };
        let len = au.payload.len();
        self.note_frame_cap(seq, len);
        let frags = self.send_h264_frame_described(
            H264Out {
                payload: au.payload,
                is_keyframe: au.meta.idr,
            },
            meta,
            seq,
            max_dg_size,
            Some(described),
        );
        let mut faulted = false;
        if let Some(st) = self.h264.refs.as_mut() {
            let before = st.mgr.pending_idr();
            let _ = st.mgr.frame_sent(plan, &header, frags, now);
            // Three plan mismatches in a row: the manager started an
            // encoder-fault epoch itself.
            faulted = st.mgr.pending_idr() == Some(RefIdrCause::EncoderFault)
                && before != Some(RefIdrCause::EncoderFault);
            if let Some(age) = st.reentry_anchor_age_ms.take() {
                let c = &mut self.h264.refs_counters;
                c.reentries += 1;
                c.reentry_bytes += len as u64;
                c.anchor_age_ms = age;
                tracing::info!(
                    target: "ghostframe::h264",
                    frame_seq = seq,
                    reentry_bytes = len,
                    anchor_age_ms = age,
                    idr = au.meta.idr,
                    "h264 reentry from the tile codec"
                );
            }
        }
        if faulted {
            self.h264_refs_note_fault(now, false);
        }
        self.publish_refs_counters();
        true
    }

    /// Step 3 for a frame the worker planned and executed.
    pub(super) fn on_h264_planned_done(
        &mut self,
        inflight: InFlight,
        reply: super::frame_backend::H264Reply,
        worker_us: u64,
    ) {
        use super::frame_backend::H264Reply;
        let max_dg_size = self.commit_max_datagram_size(&inflight);
        let InFlightStage::FinishingH264Planned { trace, plan } = inflight.stage else {
            return;
        };
        let m = inflight.meta;
        let now = now_std();
        match reply {
            H264Reply::Coded(CodedAu(au)) => {
                self.h264.refs_mode = RefsMode::Explicit;
                if self.h264.backend_kind.is_empty() {
                    self.h264.backend_kind = "libva-direct";
                }
                let Some(max_dg_size) = max_dg_size else {
                    // The session changed (the manager started a new epoch
                    // with it, so the result is stale) or has no path: the
                    // encoder's chain advanced past a frame nobody will get.
                    self.h264_refs_frame_lost(&plan, &au, now);
                    return;
                };
                self.emit_frame_dimensions(inflight.seq, m.timestamp_us, m.width, m.height);
                let mut lap = Lap::new(stage_trace_enabled());
                self.h264_refs_commit(&plan, au, &m, max_dg_size, now);
                let emit_us = lap.lap();
                tracing::trace!(
                    target: "ghostframe::io_bridge::diag",
                    seq = inflight.seq, mode = "h264",
                    pre_us = trace.pre_us, gpu_us = trace.gpu_us, classify_us = trace.classify_us,
                    worker_finish_us = worker_us, emit_us,
                    "[BRIDGE] frame.stages"
                );
            }
            H264Reply::Unplanned => {
                // No plan-executing backend here: the Phase A encoder serves
                // the stream from the next frame on. This frame was never
                // encoded; the next is made whole and is an IDR (a fresh
                // encoder's first frame).
                tracing::info!(
                    target: "ghostframe::h264",
                    "h264 acked references unavailable on this machine: using the Phase A policy"
                );
                self.h264.refs_counters.unplanned_fallbacks += 1;
                self.h264_refs_give_up(now);
                self.emit_frame_dimensions(inflight.seq, m.timestamp_us, m.width, m.height);
            }
            H264Reply::Nothing => {
                self.ensure_baseline_invalidated();
                self.emit_frame_dimensions(inflight.seq, m.timestamp_us, m.width, m.height);
            }
            H264Reply::Failed => {
                self.h264_refs_encoder_failed(now);
                self.emit_frame_dimensions(inflight.seq, m.timestamp_us, m.width, m.height);
            }
        }
    }

    /// The backend failed, or made a stream that is not the plan: the next
    /// frame reopens it with an IDR, counted as an encoder fault. A run of
    /// them with no anchor confirmed between gives acked references up.
    pub(super) fn h264_refs_encoder_failed(&mut self, now: Instant) {
        self.h264.refs_counters.backend_failures += 1;
        self.ensure_baseline_invalidated();
        self.h264_refs_note_fault(now, true);
    }

    /// Count an encoder-fault epoch. `start` begins it (a failed encode); a
    /// plan-mismatch fault was begun by the manager already. The run clears
    /// only when a P frame was confirmed as an anchor since the last fault: the
    /// IDR is candidate 0, so a backend whose IDRs work and whose P frames fail
    /// confirms an anchor every time and would otherwise never be given up on.
    fn h264_refs_note_fault(&mut self, now: Instant, start: bool) {
        let Some(st) = self.h264.refs.as_mut() else {
            return;
        };
        let anchors = st.mgr.stats().p_anchors_confirmed;
        if anchors > st.p_anchors_at_fault {
            st.faults_without_anchor = 0;
        }
        st.p_anchors_at_fault = anchors;
        st.faults_without_anchor += 1;
        let given_up = st.faults_without_anchor >= MAX_BACKEND_FAILURES;
        if start {
            st.mgr.encoder_fault();
        }
        if given_up {
            tracing::warn!(
                target: "ghostframe::h264",
                "h264 acked-reference encoder faulted {MAX_BACKEND_FAILURES} times with no \
                 anchor confirmed between: using the Phase A policy for this process"
            );
            self.h264_refs_give_up(now);
        }
    }

    /// Acked references are over for this process: the Phase A encoder serves
    /// the stream from the next frame, and asks for its IDR the Phase A way.
    fn h264_refs_give_up(&mut self, now: Instant) {
        self.h264.refs_mode = RefsMode::Legacy;
        self.h264.refs = None;
        self.request_h264_idr(IdrCause::Session, now);
        self.ensure_baseline_invalidated();
    }

    /// An access unit the backend made could not be sent. The encoder's chain
    /// has it; the client never will: the manager starts over with an IDR.
    fn h264_refs_frame_lost(&mut self, plan: &FramePlan, au: &EncodedAu, now: Instant) {
        let Some(st) = self.h264.refs.as_mut() else {
            return;
        };
        if plan.epoch != st.mgr.epoch() {
            self.h264.refs_counters.epoch_dropped += 1;
            return;
        }
        if let Some(h) = au.meta.header.as_ref() {
            st.mgr.frame_dropped(plan, h, now);
        }
        st.mgr.session_reset();
        self.ensure_baseline_invalidated();
    }

    /// The client's frame ack (0x0A) on an acked-reference session.
    pub(super) fn h264_refs_frame_ack(&mut self, newest: u32, decoded: u64, refused: u64) {
        let now = now_std();
        if !self.h264_refs_live() {
            return;
        }
        let Some(st) = self.h264.refs.as_mut() else {
            return;
        };
        // Count each frame once: the masks are cumulative.
        let c = &mut self.h264.refs_counters;
        // Newest first: the first hit of each kind is the newest.
        let mut newest_new_decoded: Option<u32> = None;
        let mut newest_new_refused: Option<u32> = None;
        for i in 0..64u32 {
            let seq = newest.wrapping_sub(i) & 0x7FFF_FFFF;
            match st.mgr.frame_state(seq) {
                Some(FrameState::Decoded | FrameState::Dead) | None => {}
                Some(FrameState::Sent | FrameState::Suspect) => {
                    if decoded >> i & 1 == 1 {
                        c.decoded_bits += 1;
                        newest_new_decoded.get_or_insert(seq);
                    } else if refused >> i & 1 == 1 {
                        c.refused_bits += 1;
                        newest_new_refused.get_or_insert(seq);
                    }
                }
            }
        }
        st.mgr.frame_ack(newest, decoded, refused);
        self.h264.refs_counters.frame_acks_applied += 1;
        // Frames the client answered for are not in flight, whether or not all
        // their fragments were acknowledged, and an answer is progress.
        for i in 0..64u32 {
            if (decoded | refused) >> i & 1 == 1 {
                self.h264
                    .tracker
                    .resolve(newest.wrapping_sub(i) & 0x7FFF_FFFF);
            }
        }
        // Progress is a frame newly decoded, not a decoded bit: the mask is
        // cumulative and keeps a bit for 64 frames, so a client that refuses
        // everything from here on would otherwise look alive for two seconds.
        if let Some(seq) = newest_new_decoded {
            self.h264.decode_clock = Some(now);
            self.h264.last_ack_progress = Some(now);
            self.h264.refused_since_decode = false;
            let nd = &mut self.h264.newest_decoded;
            if nd.is_none_or(|old| seq_lt(old, seq)) {
                *nd = Some(seq);
            }
        }
        // A refusal counts against the client only when it is newer than the
        // newest decode: old refusals reported late, or in the same cumulative
        // ack as a newer decode, say nothing about it being served now.
        if let Some(seq) = newest_new_refused {
            if self.h264.newest_decoded.is_none_or(|d| seq_lt(d, seq)) {
                self.h264.refused_since_decode = true;
            }
        }
        self.publish_refs_counters();
    }

    /// A loss verdict on a P-frame fragment of an acked-reference session: the
    /// frame is suspect, not dead (the client's decoded bit overrides it), the
    /// rest of its queued fragments are not worth sending, and the next frame
    /// is made whole so a screen that has stopped changing is repaired too.
    pub(super) fn h264_refs_verdict(&mut self, frame_seq: u32, now: Instant) {
        if !self.h264_refs_live() {
            return;
        }
        // Another fragment of a frame already judged: nothing more to do.
        if self.h264.tracker.get(frame_seq).is_some_and(|f| f.suspect) {
            return;
        }
        let Some(st) = self.h264.refs.as_mut() else {
            return;
        };
        st.mgr.frame_suspect(frame_seq);
        self.h264.refs_counters.verdicts += 1;
        if let Some(f) = self.h264.tracker.get_mut(frame_seq) {
            if !f.suspect {
                f.suspect = true;
                self.h264.stats.frames_suspect += 1;
            }
        }
        self.record_emitted_transmissions(now);
        let before = self.reliable_emitter.stats.h264_queue_purged;
        let wire_seqs = self.reliable_emitter.purge_queued_h264(|f| f == frame_seq);
        self.h264.stats.queue_purged += self.reliable_emitter.stats.h264_queue_purged - before;
        self.forget_purged(&wire_seqs);
        self.reliable_emitter.drop_h264_frame(frame_seq);
        self.ensure_baseline_invalidated();
        self.publish_refs_counters();
    }

    /// A fragment of an acked-reference session's frame outgrew the
    /// connection and was retired unsent (`retire_oversize`). Not an IDR
    /// request: the frame is treated as the loss path would treat a lost one.
    /// A P frame is suspect and the manager recovers from its anchor with
    /// frames fragmented at the new size; the IDR in flight is abandoned and
    /// sent again at that size (counted `idr_mtu_shrink`). Either way the
    /// frame's other queued and cached fragments (the same old size) go, and
    /// nothing is left waiting on content that can no longer be sent: a
    /// suspect frame is superseded by the recovery or timed out by the
    /// manager, a dead IDR is re-planned at once.
    pub(super) fn h264_refs_oversize(&mut self, frame_seq: u32, now: Instant) {
        let Some(st) = self.h264.refs.as_mut() else {
            return;
        };
        let was_idr = st.mgr.frame_oversize(frame_seq);
        self.h264.refs_counters.mtu_retired += 1;
        if was_idr {
            self.h264.refs_counters.mtu_retired_idr += 1;
        }
        if let Some(f) = self.h264.tracker.get_mut(frame_seq) {
            if !f.suspect {
                f.suspect = true;
                self.h264.stats.frames_suspect += 1;
            }
        }
        self.record_emitted_transmissions(now);
        let before = self.reliable_emitter.stats.h264_queue_purged;
        let wire_seqs = self.reliable_emitter.purge_queued_h264(|f| f == frame_seq);
        self.h264.stats.queue_purged += self.reliable_emitter.stats.h264_queue_purged - before;
        self.forget_purged(&wire_seqs);
        self.reliable_emitter.drop_h264_frame(frame_seq);
        self.ensure_baseline_invalidated();
        self.publish_refs_counters();
    }

    /// A client reset request (0x09) on an acked-reference session: a new plan
    /// epoch, which begins with an IDR. Rate limiting is the caller's.
    pub(super) fn h264_refs_client_reset(
        &mut self,
        reason: ghostframe_protocol::feedback_stream::ResetReason,
    ) {
        if !self.h264_refs_live() {
            return;
        }
        let Some(st) = self.h264.refs.as_mut() else {
            return;
        };
        st.mgr.client_reset(reset_reason(reason));
        self.h264.refs_counters.client_resets += 1;
        // The IDR is owed even if the screen has stopped changing.
        self.ensure_baseline_invalidated();
        self.publish_refs_counters();
    }

    /// The tile codec handed the screen back to H.264 (design 5.6): the first
    /// frame is a recovery P from the newest confirmed anchor, or an IDR when
    /// there is none.
    pub(super) fn h264_refs_reentry(&mut self, now: Instant) {
        if !self.h264_refs_live() {
            return;
        }
        let Some(st) = self.h264.refs.as_mut() else {
            return;
        };
        let age = st
            .mgr
            .anchor_age(now)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let had_idr_pending = st.mgr.pending_idr().is_some();
        st.mgr.mode_reentry();
        if !had_idr_pending {
            st.reentry_anchor_age_ms = Some(age);
        }
        self.publish_refs_counters();
    }

    /// The rate policy for a frame on an acked-reference session. The encoder
    /// changes rate in place, so a change is a plain rate update: no IDR, no
    /// reopen, no GOP, and none of Phase A's reopen economics
    /// ([`RatePolicy::step_in_place`]).
    fn h264_rate_step_refs(&mut self, now: Instant) {
        let estimate = self.bwe.snapshot().bitrate_bps;
        let before = self.h264.rate.applied();
        #[cfg(feature = "browserless-harness")]
        let apply = if self.h264.phase_a_rate_step {
            self.phase_a_apply(now, estimate)
        } else {
            self.h264.rate.step_in_place(now, estimate)
        };
        #[cfg(not(feature = "browserless-harness"))]
        let apply = self.h264.rate.step_in_place(now, estimate);
        let Some(apply) = apply else {
            return;
        };
        let bps = apply.bps;
        if !apply.initial {
            self.h264.refs_counters.rate_updates += 1;
            if before.is_some_and(|b| bps > b) {
                self.h264.refs_counters.rate_applies_up += 1;
            } else {
                self.h264.refs_counters.rate_applies_down += 1;
            }
            tracing::info!(
                target: LOG_TARGET,
                bps,
                estimate_bps = estimate,
                cut = apply.cut,
                "H.264 rate: moved in place"
            );
        }
        if apply.cut {
            self.h264.last_cut_at = Some(now);
        }
        self.h264.stats.rate_target_bps = bps;
        let fps = self.h264.fps;
        self.h264_control(Control::H264Rate {
            bps,
            fps,
            reopen: false,
        });
    }

    /// Phase A's policy on an acked-reference session, as it ran before
    /// `step_in_place` (the 2 s reopen lock, the rise confirmations), for the
    /// harness to measure the baseline and to break the in-place policy.
    #[cfg(feature = "browserless-harness")]
    fn phase_a_apply(
        &mut self,
        now: Instant,
        estimate: u64,
    ) -> Option<super::h264_rate::RateApply> {
        use super::h264_rate::{RateApply, RateStep};
        let (bps, cut, initial) = match self.h264.rate.step(now, estimate, true) {
            RateStep::Keep => return None,
            RateStep::Initial(bps) => (bps, false, true),
            RateStep::Reopen { bps, urgent } => (bps, urgent, false),
            RateStep::ReopenRise { bps } => (bps, false, false),
        };
        Some(RateApply { bps, cut, initial })
    }

    /// The `RefManager`'s counters and the loop's, as one report.
    pub(super) fn h264_refs_report(&self) -> RefsReport {
        let c = self.h264.refs_counters;
        let s: RefStats = self
            .h264
            .refs
            .as_ref()
            .map(|r| r.mgr.stats())
            .unwrap_or_default();
        RefsReport {
            active: self.h264_refs_live(),
            backend: self.h264.backend_kind,
            plans: c.plans,
            holds: c.holds,
            epoch_dropped: c.epoch_dropped,
            backend_failures: c.backend_failures,
            unplanned_fallbacks: c.unplanned_fallbacks,
            client_resets: c.client_resets,
            frame_acks_applied: c.frame_acks_applied,
            frames_decoded_acked: c.decoded_bits,
            frames_refused: c.refused_bits,
            verdicts: c.verdicts,
            mtu_retired: c.mtu_retired,
            mtu_retired_idr: c.mtu_retired_idr,
            reentries: c.reentries,
            reentry_bytes: c.reentry_bytes,
            anchor_age_ms: c.anchor_age_ms,
            rate_updates: c.rate_updates,
            rate_applies_up: c.rate_applies_up,
            rate_applies_down: c.rate_applies_down,
            idr_session: s.idr_session,
            idr_resolution: s.idr_resolution,
            idr_client_reset: s.idr_client_reset,
            idr_encoder_fault: s.idr_encoder_fault,
            idr_mtu_shrink: s.idr_mtu_shrink,
            idr_reissued: s.idr_reissued,
            idr_probes: s.idr_probes,
            p_healthy: s.p_healthy,
            p_recovery: s.p_recovery,
            reroots: s.reroots,
            candidates_marked: s.candidates_marked,
            anchors_confirmed: s.anchors_confirmed,
            late_confirm_ignored: s.late_confirm_ignored,
            recoveries: s.recoveries,
            recovered: s.recovered,
            plan_mismatch: s.plan_mismatch,
            deadline_suspects: s.deadline_suspects,
            acks_unknown: s.acks_unknown,
            suspect_overridden: s.suspect_overridden,
            p_anchors_confirmed: s.p_anchors_confirmed,
        }
    }

    /// Copy the manager's counters to the harness's log (browserless scenes
    /// read them after the bridge task is aborted).
    #[cfg(feature = "browserless-harness")]
    pub(super) fn publish_refs_counters(&self) {
        let r = self.h264_refs_report();
        self.h264.pcm_log.lock().expect("pcm log poisoned").refs = r;
    }

    #[cfg(not(feature = "browserless-harness"))]
    pub(super) fn publish_refs_counters(&self) {}
}
