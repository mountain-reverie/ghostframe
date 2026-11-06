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

/// The window of `RefsReport::rate_sends_10s`.
const RATE_SEND_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

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
    /// Every send of the rate buffers (the initial one included; the policy's
    /// and the probe raise's, entering and leaving a window), and those in the
    /// last [`RATE_SEND_WINDOW`].
    pub(super) rate_sends: u64,
    pub(super) rate_sends_window: u64,
    /// The probe raise (see [`IoBridge::h264_probe_raise`]): windows in which
    /// P frames were coded at the window's rate, those frames, and their bytes.
    pub(super) probe_raise_windows: u64,
    pub(super) probe_raise_frames: u64,
    pub(super) probe_raise_bytes: u64,
    /// Raised frames a verdict named a fragment of, frames fully acknowledged,
    /// and the time they took from being sent to being fully acknowledged.
    pub(super) raised_frame_lost: u64,
    pub(super) raised_frames_acked: u64,
    pub(super) raised_delivery_us_sum: u64,
    pub(super) raised_delivery_us_max: u64,
    pub(super) verdicts: u64,
    /// Frames retired because a fragment outgrew the connection (`emit.rs`):
    /// counted at the first oversize fragment of each frame, the siblings
    /// being purged with it.
    pub(super) mtu_retired: u64,
    /// Of those, frames that were the IDR in flight (abandoned, not suspect).
    pub(super) mtu_retired_idr: u64,
    /// Streams moved from libva-direct to openh264 after its fault limit.
    pub(super) backend_demotions: u64,
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
    /// The frame in flight was coded at a probe window's rate, and the window.
    inflight_raised: Option<(u32, i32)>,
    /// The window the last raised frame belonged to.
    pub(super) raise_window: Option<i32>,
    /// Frames raised in `raise_window` so far.
    pub(super) raise_window_frames: u32,
    /// Raised frames sent and not yet resolved (oldest first, bounded).
    raised: std::collections::VecDeque<RaisedFrame>,
    /// Late acknowledgements seen at two checkpoints at least
    /// [`RAISE_REORDER_MEMORY`] apart: the raise stays off while any came in
    /// since the older one (the path reorders, and a probe burst on it reads
    /// as loss).
    late_ckpt: [Option<(Instant, u64)>; 2],
}

/// A frame coded at a probe window's rate.
#[derive(Debug, Clone, Copy)]
struct RaisedFrame {
    frame_seq: u32,
    sent_at: Instant,
    lost: bool,
}

/// How many raised frames are followed at once.
const RAISED_TRACKED: usize = 64;
/// A queue on the path counts as distress for a held spike once it has stood
/// this long without a break (a probe burst's own queue drains well inside it).
/// 400 ms and 800 ms cost the step-up population nothing (sus_use 51/58/55 and
/// 52/58/55 of 96, against 52/58/55); 200 ms let go a spike's hold during
/// ordinary bursts.
pub(super) const SPIKE_QUEUE_PERSIST: Duration = Duration::from_millis(400);
/// No raise for this long after a late acknowledgement (a spurious loss
/// verdict): a path that reorders makes a burst look like loss. The memory is
/// two checkpoints at least this far apart, so a late acknowledgement keeps
/// the raise off for between this and twice this (5 to 10 s).
const RAISE_REORDER_MEMORY: Duration = Duration::from_secs(5);
/// No raise for this long after a raised frame was named lost, or a probe pad
/// (the same burst, without a picture to lose) was. A window's
/// burst on a buffer shallower than the burst tail-drops the frames queued
/// behind it (review 2: a median 6 raised frames lost per step-up run at a 60
/// ms buffer, 12 at 30 ms, against none with the raise off); padding alone
/// fills the windows meanwhile, as before the raise.
pub(super) const RAISE_LOSS_PAUSE: Duration = Duration::from_secs(10);

impl RefsState {
    fn new(frame_interval: Duration, srtt: Duration) -> Self {
        Self {
            mgr: RefManager::new(RefConfig {
                params: StreamParams::writer(),
                frame_interval,
                initial_srtt: srtt,
                observed: false,
            }),
            dims: None,
            faults_without_anchor: 0,
            p_anchors_at_fault: 0,
            reentry_anchor_age_ms: None,
            inflight_cap: None,
            inflight_raised: None,
            raise_window: None,
            raise_window_frames: 0,
            raised: std::collections::VecDeque::new(),
            late_ckpt: [None, None],
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
    /// libva-direct streams moved to openh264 after its fault limit.
    pub backend_demotions: u64,
    pub reentries: u64,
    /// Wire bytes of the first frame after each return from the tile codec,
    /// summed; and the age of the anchor it predicted from, last.
    pub reentry_bytes: u64,
    pub anchor_age_ms: u64,
    pub rate_updates: u64,
    pub rate_applies_up: u64,
    pub rate_applies_down: u64,
    /// Rate-buffer re-sends (the policy's and the probe raise's, entering and
    /// leaving a window), in total and in the last 10 s. Each costs the
    /// encoder a dip over its next frames (T4: 1.00 / 0.56 / 0.87 of the
    /// average after a re-send at 40 % fullness; a window's end dips to about
    /// 0.5 for two frames): divide the window by 10 for the per-second rate.
    pub rate_sends: u64,
    pub rate_sends_10s: u64,
    /// The probe raise: windows in which P frames were coded at the window's
    /// rate, those frames and their bytes; of them, frames a verdict named a
    /// fragment of, frames fully acknowledged, and the sum and the maximum of
    /// the time from sending to the full acknowledgement (microseconds).
    pub probe_raise_windows: u64,
    pub probe_raise_frames: u64,
    pub probe_raise_bytes: u64,
    pub raised_frame_lost: u64,
    pub raised_frames_acked: u64,
    pub raised_delivery_us_sum: u64,
    pub raised_delivery_us_max: u64,
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
    // Observed mode (openh264).
    pub idr_backend_fallback: u64,
    pub marks_observed: u64,
    pub mark_unrepresentable: u64,
    pub ltr_recoveries_requested: u64,
    pub ltr_mark_failed_sent: u64,
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
        Some(RefIdrCause::BackendFallback) => IdrCause::BackendFallback,
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
        // What the receiver's word produced for a backend that keeps its own
        // long-term bookkeeping (observed mode); empty otherwise. Taken with
        // the plan, so it belongs to the plan's epoch.
        let feedback = st.mgr.take_backend_feedback();
        let is_idr = plan.kind == PicKind::Idr;
        // The frame's budget: an IDR gets its own (a quarter second of the
        // rate), a P frame the cap of a few average frames. A P frame planned
        // while a probe window is open is coded at the window's rate instead.
        let mut rate = RateTarget::steady(bps, fps);
        let mut raised = None;
        if rate_policy && !is_idr {
            if let Some((raised_bps, window)) = self.h264_probe_raise(now, bps) {
                rate = RateTarget::steady(raised_bps, fps);
                raised = Some((plan.frame_seq, window));
            }
        }
        if rate_policy {
            self.note_rate_send(now, rate);
        }
        let cap = if is_idr {
            rate.idr_max_bytes
        } else {
            rate.max_frame_bytes
        };
        let st = self.refs_state();
        st.inflight_cap = Some((plan.frame_seq, cap));
        st.inflight_raised = raised;
        self.h264.stats.frame_cap_bytes = rate.max_frame_bytes;
        self.h264.stats.idr_cap_bytes = rate.idr_max_bytes;
        self.h264.refs_counters.plans += 1;
        Some(Box::new(H264Request {
            plan,
            refs,
            fps,
            rate,
            feedback,
            backend: self.h264.backend,
            openh264_lib: self.h264.openh264_lib.clone(),
        }))
    }

    /// Count a re-send of the rate buffers: the backend is told a target other
    /// than the last it was told. That is a rate the policy moved, and also
    /// the frame that enters a probe window (the window's rate) and the first
    /// one after it (back to the normal rate), each of which costs the VCE a
    /// first-frame dip.
    fn note_rate_send(&mut self, now: Instant, rate: RateTarget) {
        if self.h264.last_planned_rate == Some(rate) {
            return;
        }
        self.h264.last_planned_rate = Some(rate);
        self.h264.refs_counters.rate_sends += 1;
        self.h264.rate_send_times.push_back(now);
        self.h264.refs_counters.rate_sends_window = self.h264.rate_send_times.len() as u64;
    }

    /// A frame coded at a probe window's rate goes out: count it and follow it
    /// to its acknowledgement or its first verdict.
    fn note_raised_sent(&mut self, seq: u32, len: usize, now: Instant) {
        let Some(st) = self.h264.refs.as_mut() else {
            return;
        };
        let Some((s, window)) = st.inflight_raised.take() else {
            return;
        };
        if s != seq {
            return;
        }
        let c = &mut self.h264.refs_counters;
        if st.raise_window != Some(window) {
            st.raise_window = Some(window);
            st.raise_window_frames = 0;
            c.probe_raise_windows += 1;
        }
        st.raise_window_frames += 1;
        c.probe_raise_frames += 1;
        c.probe_raise_bytes += len as u64;
        if st.raised.len() >= RAISED_TRACKED {
            st.raised.pop_front();
        }
        st.raised.push_back(RaisedFrame {
            frame_seq: seq,
            sent_at: now,
            lost: false,
        });
    }

    /// A raised frame was fully acknowledged.
    pub(super) fn note_raised_acked(&mut self, frame_seq: u32, now: Instant) {
        let Some(st) = self.h264.refs.as_mut() else {
            return;
        };
        let Some(i) = st.raised.iter().position(|r| r.frame_seq == frame_seq) else {
            return;
        };
        let r = st.raised.remove(i).expect("position just found");
        let us = now.saturating_duration_since(r.sent_at).as_micros() as u64;
        let c = &mut self.h264.refs_counters;
        c.raised_frames_acked += 1;
        c.raised_delivery_us_sum += us;
        c.raised_delivery_us_max = c.raised_delivery_us_max.max(us);
    }

    /// A verdict named a fragment of a P frame: if it was a raised one, it is
    /// counted (once per frame).
    pub(super) fn note_raised_lost(&mut self, frame_seq: u32, now: Instant) {
        let Some(st) = self.h264.refs.as_mut() else {
            return;
        };
        if let Some(r) = st
            .raised
            .iter_mut()
            .find(|r| r.frame_seq == frame_seq && !r.lost)
        {
            r.lost = true;
            self.h264.raise_paused_until = Some(now + RAISE_LOSS_PAUSE);
            self.h264.refs_counters.raised_frame_lost += 1;
        }
    }

    /// Whether the path has shown reordering lately (a late acknowledgement
    /// within [`RAISE_REORDER_MEMORY`] or twice that): the raise keeps off.
    pub(super) fn h264_reordering_lately(&mut self, now: Instant) -> bool {
        let late = self.h264.stats.tx_acked_late;
        let Some(st) = self.h264.refs.as_mut() else {
            return false;
        };
        match st.late_ckpt[1] {
            Some((t, _)) if now.saturating_duration_since(t) < RAISE_REORDER_MEMORY => {}
            None => st.late_ckpt = [Some((now, late)); 2],
            _ => {
                st.late_ckpt[0] = st.late_ckpt[1];
                st.late_ckpt[1] = Some((now, late));
            }
        }
        st.late_ckpt[0].is_some_and(|(_, n)| late > n)
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
        // A backend with its own stream (openh264) says so with the parameters
        // of its IDR: from here the manager reads the emitted headers.
        if let Some(params) = au.meta.params {
            st.mgr.adopt_stream_params(params);
        }
        let observed = st.mgr.is_observed();
        // A stream that does not parse, or whose IDR flag disagrees with the
        // plan, cannot be followed: the backend failed. In observed mode an
        // IDR nobody asked for is the backend's own fallback and is adopted
        // (a new epoch); a plan for an IDR the backend did not make is not.
        let planned_idr = plan.kind == PicKind::Idr;
        let header = au
            .meta
            .header
            .clone()
            .filter(|_| au.meta.idr == planned_idr || (observed && au.meta.idr));
        let Some(header) = header else {
            self.h264_refs_encoder_failed(now);
            return false;
        };
        // The plan as the stream executed it: in observed mode read from the
        // header (reference, marking, an unrequested IDR), else the plan.
        let executed = if observed {
            st.mgr.observe(plan, &header)
        } else {
            plan.clone()
        };
        let described = frame::Described {
            info: executed.wire_info(au.meta.ref_seq),
            idr_cause: executed
                .idr_cause
                .map(|c| bridge_idr_cause(Some(c)))
                .filter(|_| au.meta.idr),
        };
        let len = au.payload.len();
        self.note_frame_cap(seq, len);
        self.note_raised_sent(seq, len, now);
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
            H264Reply::Coded(CodedAu(au, backend)) => {
                self.h264.refs_mode = RefsMode::Explicit;
                self.h264.backend_kind = backend;
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
        if given_up && self.h264_demote_libva_direct() {
            return;
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

    /// libva-direct hit its fault limit: with `auto` the stream moves one
    /// step down to openh264, with a `backend_fallback` IDR, and libva-direct
    /// is not tried again this process. This is not the "H.264 unavailable"
    /// latch (that is for openh264 giving up). Returns whether it demoted.
    fn h264_demote_libva_direct(&mut self) -> bool {
        use crate::config::H264BackendChoice;
        if self.h264.backend != H264BackendChoice::Auto || self.h264.backend_kind != "libva-direct"
        {
            return false;
        }
        let Some(st) = self.h264.refs.as_mut() else {
            return false;
        };
        tracing::warn!(
            target: "ghostframe::h264",
            "libva-direct faulted {MAX_BACKEND_FAILURES} times with no anchor confirmed \
             between: moving this process to openh264"
        );
        st.faults_without_anchor = 0;
        st.mgr.backend_fallback();
        self.h264.backend = H264BackendChoice::Openh264;
        self.h264.backend_kind = "";
        self.h264.refs_counters.backend_demotions += 1;
        self.ensure_baseline_invalidated();
        self.publish_refs_counters();
        true
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
        if st.mgr.is_observed() {
            // The backend's chain has the frame and the client never will:
            // that is a loss like any other. The next plan asks for a
            // recovery from the confirmed long-term picture, not an IDR.
            self.publish_refs_counters();
            return;
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
        while self
            .h264
            .rate_send_times
            .front()
            .is_some_and(|&t| now.saturating_duration_since(t) > RATE_SEND_WINDOW)
        {
            self.h264.rate_send_times.pop_front();
        }
        self.h264.refs_counters.rate_sends_window = self.h264.rate_send_times.len() as u64;
        let snap = self.bwe.snapshot();
        let estimate = snap.bitrate_bps;
        // Distress: the congestion controller is limiting the estimate for
        // loss, or a queue has stood on the path for `SPIKE_QUEUE_PERSIST`
        // without a break. A held spike is let go early when the estimate
        // falls with it. A queue alone is not enough: a probe burst builds one
        // by design (releasing on the queue the moment it stood let the spike
        // go on most step-ups and cut sustained use of the headroom from 69 to
        // 12 of 96); one that outlasts the burst is the stream sending past the
        // path.
        let queue = self.bwe.standing_queue();
        let since = &mut self.h264.queue_since;
        if queue {
            since.get_or_insert(now);
        } else {
            *since = None;
        }
        let queue_persists =
            since.is_some_and(|t| now.saturating_duration_since(t) >= SPIKE_QUEUE_PERSIST);
        let distress = snap.stats.loss_state.is_limited() || queue_persists;
        let before = self.h264.rate.applied();
        #[cfg(feature = "browserless-harness")]
        let apply = if self.h264.phase_a_rate_step {
            self.phase_a_apply(now, estimate)
        } else {
            self.h264.rate.step_in_place(now, estimate, distress)
        };
        #[cfg(not(feature = "browserless-harness"))]
        let apply = self.h264.rate.step_in_place(now, estimate, distress);
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
            backend_demotions: c.backend_demotions,
            reentries: c.reentries,
            reentry_bytes: c.reentry_bytes,
            anchor_age_ms: c.anchor_age_ms,
            rate_updates: c.rate_updates,
            rate_applies_up: c.rate_applies_up,
            rate_applies_down: c.rate_applies_down,
            rate_sends: c.rate_sends,
            rate_sends_10s: c.rate_sends_window,
            probe_raise_windows: c.probe_raise_windows,
            probe_raise_frames: c.probe_raise_frames,
            probe_raise_bytes: c.probe_raise_bytes,
            raised_frame_lost: c.raised_frame_lost,
            raised_frames_acked: c.raised_frames_acked,
            raised_delivery_us_sum: c.raised_delivery_us_sum,
            raised_delivery_us_max: c.raised_delivery_us_max,
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
            idr_backend_fallback: s.idr_backend_fallback,
            marks_observed: s.marks_observed,
            mark_unrepresentable: s.mark_unrepresentable,
            ltr_recoveries_requested: s.ltr_recoveries_requested,
            ltr_mark_failed_sent: s.ltr_mark_failed_sent,
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
