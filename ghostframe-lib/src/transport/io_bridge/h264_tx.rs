//! H.264 frames as members of the transport: the per-frame tracker, the IDR
//! hold, the IDR request policy (and its rate limit), and the encoder's
//! backpressure.
//!
//! Phase A of the H.264 rework (the design's sections 4.3 and 5): the ffmpeg
//! encoder has no reference control, so recovery from a loss is an IDR. What
//! changes from a side channel is that every fragment is a ledger
//! transmission (`ReliableTileEmitter::submit_h264_fragment`), so it is
//! acknowledged or judged exactly once, charged to the admission bucket, and
//! fed to the estimators.
//!
//! Policy implemented here:
//!
//! * **Interim loss-triggered IDR.** A loss verdict on a fragment of a P frame
//!   that is not already acknowledged marks the frame suspect, purges the
//!   queued fragments of every frame from it on (they chain from a picture the
//!   client does not have), and asks the encoder for an IDR, at most one per
//!   [`LOSS_IDR_MIN_INTERVAL`]. A request the limit turned away is remembered
//!   and made when the interval has passed. Deleted when acked-reference
//!   recovery lands (Phase B).
//! * **IDR fragments are repaired, P frames are held.** A verdict on an IDR
//!   fragment resends it (the emitter does that); until every fragment of the
//!   newest IDR is acknowledged no P frame is encoded, because the client
//!   evicts what is older than `latest - 2` and P frames behind an unfinished
//!   IDR would push it out. The hold has a ceiling ([`IDR_HOLD_MAX`]): past it
//!   the hold is dropped and a fresh IDR is requested.
//! * **Frame skip.** The encoder is not run while the emission queue holds more
//!   H.264 bytes than a frame period of the admission rate (or two gate
//!   depths): a skipped capture loses nothing, every frame being a whole
//!   screen, and queueing stale fragments behind it would only delay the
//!   fresh frame. Fragments of a frame an IDR supersedes are purged from the
//!   queue.

use super::h264_rate::ScriptedCbr;
use super::*;
use crate::encoder::h264::tracker::{FragAck, TRACKER_RING};
use crate::transport::reliable_emitter::Content;
use crate::transport::transmission_ledger::Transmission;

/// Floor of the in-flight bound for `h264_stalled` (a): unresolved frames
/// beyond `max(this, ceil(2 * srtt / frame interval) + 8)` mean the path has
/// stopped carrying the stream.
pub(super) const STALL_INFLIGHT_FLOOR: u32 = 32;
/// `h264_stalled` (b): no H.264 fragment acknowledged for this long while
/// frames are in flight. (A large IDR on a slow link takes seconds to drain;
/// a fragment at a time is progress, a whole frame is not the right unit.)
pub(super) const STALL_NO_PROGRESS: std::time::Duration = std::time::Duration::from_millis(500);
/// `h264_stalled` (c): honoured client reset requests (each one an IDR the
/// server made) that raise the flag: this many within [`STALL_RESET_WINDOW`].
/// The client asking again and again means the IDRs do not help. (A client
/// that paces its requests just outside the server's own 2 s limit would
/// never trip a "rate-limited" test.)
pub(super) const STALL_RESET_COUNT: usize = 3;
pub(super) const STALL_RESET_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);
/// How long the flag stays up once (c) trips.
pub(super) const STALL_RESET_HOLD: std::time::Duration = std::time::Duration::from_millis(500);

/// The in-flight bound of the stall flag (a), from the smoothed RTT.
///
/// Assumes the design's frame interval of a 30 Hz stream
/// ([`SCHEDULER_TICK_INTERVAL_US`], 33.3 ms), not the capture rate actually in
/// force: a 60 Hz stream has twice the frames in flight per RTT, which the
/// floor of 32 and the `+ 8` absorb up to a ~400 ms round trip.
pub(super) fn stall_inflight_bound(srtt_us: f32) -> u32 {
    let frames_per_two_rtt = if srtt_us > 0.0 {
        (2.0 * f64::from(srtt_us) / SCHEDULER_TICK_INTERVAL_US).ceil() as u32
    } else {
        0
    };
    STALL_INFLIGHT_FLOOR.max(frames_per_two_rtt.saturating_add(8))
}

/// Loss-triggered IDRs are made at most this often.
pub(super) const LOSS_IDR_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// The longest P frames wait for an IDR without any of its fragments being
/// acknowledged (a no-progress timeout: each acknowledged fragment restarts
/// it). Retransmission and the probe normally settle an IDR in a few round
/// trips; a client whose acknowledgements never arrive must not freeze the
/// H.264 stream for good.
pub(super) const IDR_HOLD_MAX: std::time::Duration = std::time::Duration::from_secs(3);

/// Why an IDR was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdrCause {
    /// A new live session (first frame, or after a takeover or reconnect).
    Session,
    /// A loss verdict on a P-frame fragment (interim, Phase A).
    Loss,
    /// The client asked (feedback stream 0x09).
    ClientReset,
    /// The frame size changed and the encoder was reopened.
    Resolution,
    /// The classifier returned from the tile codec to H.264.
    Reentry,
    /// The previous IDR was never fully acknowledged and the hold gave up.
    HoldTimeout,
    /// The rate fell by more than 30 % and the encoder was reopened at once
    /// (a rate change that waits for the scheduled GOP IDR is `GopPeriodic`).
    RateDrop,
    /// The rate rose by more than 30 % and the encoder was reopened at once.
    RateRise,
    /// None of the above asked for it: the encoder's own GOP.
    GopPeriodic,
    /// The path MTU fell below the size fragments already built were cut for
    /// (quinn's black-hole detector, after discovery had raised it), so
    /// stored H.264 fragments can no longer be sent. A transport event of the
    /// same nature as a session start: the fragments are gone, and an IDR
    /// sized for the connection as it is now is the only way to a picture.
    MtuShrink,
}

impl IdrCause {
    pub(crate) const COUNT: usize = 10;
    pub const ALL: [IdrCause; Self::COUNT] = [
        IdrCause::Session,
        IdrCause::Loss,
        IdrCause::ClientReset,
        IdrCause::Resolution,
        IdrCause::Reentry,
        IdrCause::HoldTimeout,
        IdrCause::RateDrop,
        IdrCause::RateRise,
        IdrCause::GopPeriodic,
        IdrCause::MtuShrink,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn name(self) -> &'static str {
        match self {
            IdrCause::Session => "session",
            IdrCause::Loss => "loss",
            IdrCause::ClientReset => "client_reset",
            IdrCause::Resolution => "resolution",
            IdrCause::Reentry => "reentry",
            IdrCause::HoldTimeout => "hold_timeout",
            IdrCause::RateDrop => "rate_drop",
            IdrCause::RateRise => "rate_rise",
            IdrCause::GopPeriodic => "gop_periodic",
            IdrCause::MtuShrink => "mtu_shrink",
        }
    }
}

/// What the encoder gate decided for a frame the classifier put in H.264 mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum H264Gate {
    /// Encode it.
    Encode,
    /// An IDR is not fully acknowledged yet: no P frame.
    SkipHold,
    /// The emission queue holds more H.264 than a frame period carries.
    SkipBackpressure,
}

/// Cumulative H.264 transport counters. Published to the harness and logged
/// as the `h264 stats` line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct H264Stats {
    pub frames_sent: u64,
    pub frames_idr: u64,
    /// Frames whose every fragment was acknowledged.
    pub frames_acked: u64,
    /// Frames a verdict on a fragment marked suspect.
    pub frames_suspect: u64,
    /// Fragments recorded in the ledger (every transmission, resends included).
    pub tx_recorded: u64,
    /// Of those: acknowledged while outstanding, judged lost, or taken back
    /// with no verdict (superseded in the queue, or found with their content
    /// gone by the probe). A recorded transmission is exactly one of these or
    /// still outstanding; `tx_acked_late` counts the acknowledgements of ones
    /// already judged and is not a fourth outcome.
    ///
    /// The identity `tx_recorded = tx_acked_live + tx_judged_lost +
    /// tx_forgotten + outstanding` ignores two ways a record leaves the
    /// ledger that this counter set does not see: capacity evictions (the
    /// ledger's hard cap, `LedgerStats::capacity_evictions`, non-zero only if
    /// transmissions outrun acknowledgements badly) and session resets (the
    /// ledger is wiped with the session while these counters stay cumulative).
    /// It holds exactly within one session that stays under the cap.
    pub tx_acked_live: u64,
    pub tx_judged_lost: u64,
    pub tx_forgotten: u64,
    pub tx_acked_late: u64,
    /// Verdicts by what they named.
    pub verdicts_idr_frag: u64,
    pub verdicts_p_frag: u64,
    /// Of those: verdicts on a datagram that departed inside a probe burst
    /// (what padding could have caused), by its departure, not the verdict's time.
    pub p_verdicts_in_probe: u64,
    /// The fragment (or its whole frame) had been acknowledged by another
    /// transmission already: a spurious verdict, no action.
    pub verdicts_already_acked: u64,
    /// The frame had left the tracker.
    pub verdicts_unknown_frame: u64,
    /// A P-frame fragment's verdict that an IDR already sent or pending
    /// supersedes: no request, no purge (the IDR does both).
    pub verdicts_superseded: u64,
    /// IDRs made, by cause (`IdrCause::index`).
    pub idr_by_cause: [u64; IdrCause::COUNT],
    /// IDR requests made, by cause, and the loss requests the rate limit turned
    /// away (they are made later, see `deferred_loss_idr`).
    pub idr_requests_by_cause: [u64; IdrCause::COUNT],
    pub loss_idr_rate_limited: u64,
    /// Requests that found one already pending.
    pub idr_requests_coalesced: u64,
    /// Time P frames were held for an IDR, ms, and how many holds there were
    /// and how many gave up.
    pub p_hold_ms: u64,
    pub p_holds: u64,
    pub hold_timeouts: u64,
    /// Frames not encoded, by why.
    pub frames_skipped_hold: u64,
    pub frames_skipped_backpressure: u64,
    /// Datagrams quinn refused as larger than the connection's current
    /// maximum (the path MTU fell after they were built).
    pub send_too_large: u64,
    /// Queued datagrams retired because they no longer fit the connection's
    /// datagram size (it fell after they were built), by what they carried:
    /// a tile pass, an H.264 fragment, probe padding, or something the ledger
    /// does not track (XOR parity). None of them reached quinn.
    pub oversize_tile: u64,
    pub oversize_h264: u64,
    pub oversize_pad: u64,
    pub oversize_untracked: u64,
    /// Queued emissions removed because an IDR superseded their frame or a
    /// verdict made it suspect.
    pub queue_purged: u64,
    /// Time the `h264_stalled` flag was up, ms; and how many times it went up,
    /// by the condition that raised it (in-flight bound, no progress, client
    /// reset requests over the rate limit).
    pub h264_stalled_ms: u64,
    pub stalls_inflight: u64,
    pub stalls_no_progress: u64,
    pub stalls_reset: u64,
    /// Encoder reopens made to change the rate: at once for a fall of more
    /// than 30 % (`IdrCause::RateDrop`), or at the scheduled GOP IDR.
    pub rate_reopens_drop: u64,
    pub rate_reopens_gop: u64,
    pub rate_reopens_rise: u64,
    /// The rate the encoder was last told to run at, bits per second.
    pub rate_target_bps: u32,
    /// The per-frame budget of the last acked-reference plan, bytes: the cap
    /// on a P frame and on an IDR (`RateTarget::steady`). Gauges.
    pub frame_cap_bytes: u32,
    pub idr_cap_bytes: u32,
    /// Acked-reference frames bigger than 1.15 x the cap they were coded
    /// under, and the biggest frame over its cap, in percent (100: at the
    /// cap). The cap is soft on the VCE (S2): observed, never gated.
    pub frames_over_cap: u64,
    pub max_cap_ratio_pct: u32,
    /// Probe clusters the application started itself (see `h264_rate`).
    pub app_probes: u64,
    /// Probes not started because padding could not fill a cluster of the
    /// shortest useful duration at this estimate.
    pub app_probes_skipped: u64,
    /// Frames skipped for the emission backlog while padding was in flight
    /// (a subset of `frames_skipped_backpressure`).
    pub frames_skipped_pad: u64,
    /// Probe padding sent to fill a probe window without an IDR: windows
    /// filled that way, datagrams and bytes (wire bytes of the datagrams,
    /// before the transport's per-datagram overhead).
    pub probe_padding_windows: u64,
    pub probe_padding_datagrams: u64,
    pub probe_padding_bytes: u64,
    /// Probe pads the loss path judged lost: each pauses the probe raise.
    pub pads_lost: u64,
}

/// What the harness reads back after a scene: the counters plus two gauges.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct H264Report {
    pub stats: H264Stats,
    /// Ledger records outstanding at the end (all H.264 in an H.264-only scene).
    pub ledger_records: usize,
    /// Fragments still held in the probe cache.
    pub probe_cache_fragments: usize,
    /// Acknowledgement datagrams the harness's blackout dropped (0 in production).
    pub acks_blacked_out: u64,
    /// The `h264_stalled` flag as of the last evaluation.
    pub stalled: bool,
    /// Frame acks (datagram 0x0A) that parsed, and ones that did not. Nothing
    /// consumes them yet; a scene asserts the client's datagram reaches the
    /// server intact.
    pub frame_acks_received: u64,
    pub frame_acks_malformed: u64,
    /// Frame acks the harness's loss window dropped (0 in production).
    pub frame_acks_dropped: u64,
    /// Acked-reference recovery: the manager's counters and the loop's.
    pub refs: super::h264_refs::RefsReport,
}

/// One frame the tracker follows.
pub(super) struct TrackedFrame {
    frame_seq: u32,
    is_idr: bool,
    frags_total: u16,
    /// One bit per fragment: acknowledged (through any transmission of it).
    acked: Vec<u64>,
    acked_count: u16,
    pub(super) suspect: bool,
    /// The client answered for the frame (a frame ack named it decoded or
    /// refused): it is not in flight any more, whatever its fragments did.
    resolved: bool,
    sent_at: std::time::Instant,
}

impl TrackedFrame {
    fn frag_acked(&self, frag_idx: u16) -> bool {
        self.acked
            .get(frag_idx as usize / 64)
            .is_some_and(|w| w & (1 << (frag_idx % 64)) != 0)
    }

    fn complete(&self) -> bool {
        self.acked_count == self.frags_total
    }
}

/// Ring of the last [`TRACKER_RING`] frames sent: O(1) per datagram or
/// acknowledgement.
pub(super) struct FrameTracker {
    ring: Vec<Option<TrackedFrame>>,
}

impl Default for FrameTracker {
    fn default() -> Self {
        Self {
            ring: (0..TRACKER_RING).map(|_| None).collect(),
        }
    }
}

impl FrameTracker {
    fn slot(frame_seq: u32) -> usize {
        frame_seq as usize % TRACKER_RING
    }

    pub(super) fn on_frame_sent(
        &mut self,
        frame_seq: u32,
        is_idr: bool,
        frags_total: u16,
        now: std::time::Instant,
    ) {
        self.ring[Self::slot(frame_seq)] = Some(TrackedFrame {
            frame_seq,
            is_idr,
            frags_total,
            acked: vec![0; (frags_total as usize).div_ceil(64)],
            acked_count: 0,
            suspect: false,
            resolved: false,
            sent_at: now,
        });
    }

    /// The frames the stall flag counts as in flight: sent, not fully
    /// acknowledged, not judged lost, at or after `floor` (older ones are
    /// superseded by an IDR or abandoned with H.264 mode) and before
    /// `doomed_from` (frames after a suspect one chain from it and will never
    /// be resolved). Returns their number and the send time of the oldest.
    /// A scan of the ring, once per frame decision, not per datagram.
    pub(super) fn unresolved(
        &self,
        floor: u32,
        doomed_from: Option<u32>,
    ) -> (u32, Option<std::time::Instant>) {
        let mut count = 0u32;
        let mut oldest: Option<std::time::Instant> = None;
        for f in self.ring.iter().flatten() {
            if f.frame_seq < floor
                || f.suspect
                || f.resolved
                || f.complete()
                || doomed_from.is_some_and(|d| f.frame_seq >= d)
            {
                continue;
            }
            count += 1;
            oldest = Some(oldest.map_or(f.sent_at, |o| o.min(f.sent_at)));
        }
        (count, oldest)
    }

    pub(super) fn get(&self, frame_seq: u32) -> Option<&TrackedFrame> {
        self.ring[Self::slot(frame_seq)]
            .as_ref()
            .filter(|f| f.frame_seq == frame_seq)
    }

    pub(super) fn get_mut(&mut self, frame_seq: u32) -> Option<&mut TrackedFrame> {
        self.ring[Self::slot(frame_seq)]
            .as_mut()
            .filter(|f| f.frame_seq == frame_seq)
    }

    pub(super) fn on_frag_acked(&mut self, frame_seq: u32, frag_idx: u16) -> FragAck {
        let Some(f) = self.get_mut(frame_seq) else {
            return FragAck::Unknown;
        };
        if frag_idx >= f.frags_total || f.frag_acked(frag_idx) {
            return FragAck::Duplicate;
        }
        f.acked[frag_idx as usize / 64] |= 1 << (frag_idx % 64);
        f.acked_count += 1;
        FragAck::Acked {
            complete: f.complete(),
        }
    }

    /// The client answered for the frame (0x0A).
    pub(super) fn resolve(&mut self, frame_seq: u32) {
        if let Some(f) = self.get_mut(frame_seq) {
            f.resolved = true;
        }
    }

    #[cfg(test)]
    pub(super) fn is_complete(&self, frame_seq: u32) -> Option<bool> {
        self.get(frame_seq).map(|f| f.complete())
    }

    pub(super) fn clear(&mut self) {
        self.ring.iter_mut().for_each(|s| *s = None);
    }
}

/// The IDR whose fragments P frames wait for.
#[derive(Debug, Clone, Copy)]
pub(super) struct IdrHold {
    pub(super) frame_seq: u32,
    /// When the hold began (for the time P frames waited).
    pub(super) since: std::time::Instant,
    /// The last time a fragment of the held IDR was acknowledged, or the
    /// start. The ceiling is a no-progress timeout measured from here, so a
    /// large IDR on a slow link that takes longer than [`IDR_HOLD_MAX`] to
    /// drain is not given up on while it is still being delivered.
    pub(super) last_progress: std::time::Instant,
}

/// The bridge's H.264 transport state. Everything here belongs to one live
/// session and is cleared by [`H264Tx::reset_session`]; the counters survive.
#[derive(Default)]
pub(super) struct H264Tx {
    pub(super) tracker: FrameTracker,
    pub(super) hold: Option<IdrHold>,
    /// An IDR asked for and not yet made, and who asked first. While set,
    /// further requests are coalesced into it.
    pub(super) pending_idr: Option<IdrCause>,
    /// When the last loss-triggered IDR was requested.
    pub(super) last_loss_idr_at: Option<std::time::Instant>,
    /// A loss-triggered IDR the rate limit turned away; made when it can be.
    pub(super) deferred_loss_idr: bool,
    /// Frame size of the last frame encoded, to tell a reopen from a GOP.
    pub(super) last_dims: Option<(u32, u32)>,
    /// `frame_seq` of the newest IDR sent: every older frame is superseded.
    pub(super) last_idr_seq: Option<u32>,
    /// Whether any H.264 fragment has been acknowledged in this session, and
    /// whether the "client does not acknowledge" warning was given.
    pub(super) acked_any: bool,
    pub(super) warned_no_acks: bool,
    pub(super) stats: H264Stats,
    /// Frames older than this are not in flight as far as the stall flag is
    /// concerned (set when the session leaves H.264 mode).
    pub(super) ignore_before: u32,
    /// The oldest frame a loss verdict made suspect since the last IDR.
    pub(super) doomed_from: Option<u32>,
    /// When an H.264 fragment was last acknowledged.
    pub(super) last_ack_progress: Option<std::time::Instant>,
    /// When the last [`STALL_RESET_COUNT`] client reset requests were
    /// honoured (oldest first).
    pub(super) honoured_resets: Vec<std::time::Instant>,
    /// Until when a burst of honoured reset requests holds the stall flag.
    pub(super) reset_storm_until: Option<std::time::Instant>,
    /// The stall flag as of the last evaluation, and when that was.
    pub(super) stalled: bool,
    pub(super) stall_eval_at: Option<std::time::Instant>,
    /// The rate policy (see `h264_rate`).
    pub(super) rate: super::h264_rate::RatePolicy,
    /// The GOP: an IDR is made when this long has passed since the last one,
    /// whatever caused that one.
    pub(super) gop: std::time::Duration,
    /// The frame rate the encoder is opened for.
    pub(super) fps: u32,
    /// When the last IDR was made; `None` until one. The bridge's clock for
    /// "the next frame is the scheduled GOP IDR".
    pub(super) last_idr_at: Option<std::time::Instant>,
    /// A recently acknowledged P-frame fragment's datagram: the padding of a
    /// probe window (see `h264_rate`).
    pub(super) pad_template: Option<bytes::Bytes>,
    /// The application's own probing: cadence and chain state.
    pub(super) app_probe: super::h264_rate::AppProbe,
    /// When the encoder's rate was last cut by a reopen (padding keeps quiet).
    pub(super) last_cut_at: Option<std::time::Instant>,
    /// The harness's scripted CBR encoder, if the scene asked for one.
    pub(super) scripted: Option<super::h264_rate::ScriptedCbr>,
    /// Acked-reference recovery (Phase B, `h264_refs`): whether it is allowed
    /// at all, what the backend turned out to be, the live session's manager
    /// and the loop's counters.
    pub(super) refs_enabled: bool,
    /// Which backends the planned path may open, and the openh264 library
    /// to load (`GHOSTFRAME_H264_BACKEND`, `GHOSTFRAME_H264_OPENH264_LIB`).
    pub(super) backend: crate::config::H264BackendChoice,
    pub(super) openh264_lib: Option<std::sync::Arc<std::path::Path>>,
    pub(super) refs_mode: super::h264_refs::RefsMode,
    pub(super) refs: Option<super::h264_refs::RefsState>,
    pub(super) refs_counters: super::h264_refs::RefsCounters,
    /// The probe raise stays off until then (see `RAISE_LOSS_PAUSE`).
    pub(super) raise_paused_until: Option<std::time::Instant>,
    /// The rate target the backend was last planned with.
    pub(super) last_planned_rate: Option<crate::encoder::h264::backend::RateTarget>,
    /// When the rate buffers were last sent (the last 10 s).
    pub(super) rate_send_times: std::collections::VecDeque<std::time::Instant>,
    /// Acked-reference sessions: when a frame was last newly decoded (or the
    /// IDR last progressed), and when a frame was last sent. Frames sent since
    /// the last decode, for 500 ms, are a stall (a client that only refuses
    /// never decodes).
    pub(super) decode_clock: Option<std::time::Instant>,
    pub(super) last_sent_at: Option<std::time::Instant>,
    /// The client refused a frame after the last one it decoded: P-fragment
    /// acknowledgements then say nothing about it being served.
    pub(super) refused_since_decode: bool,
    /// The newest frame the client has been counted as decoding. A refusal
    /// only raises `refused_since_decode` when it is newer than this, so a
    /// cumulative 0x0A that reports old refusals together with a newer decode
    /// (or a late one with only the old refusals) leaves the flag alone.
    pub(super) newest_decoded: Option<u32>,
    /// Harness mutation: the probe raise is off (`InjectedH264::no_probe_raise`).
    /// Production has no switch: the raise always runs on acked-reference
    /// streams (see `h264_probe_raise`).
    #[cfg(any(test, feature = "browserless-harness"))]
    pub(super) raise_mutated_off: bool,
    /// Since when a queue has stood on the path, without a break.
    pub(super) queue_since: Option<std::time::Instant>,
    /// The encoder behind the stream, for the `h264 refs` line.
    pub(super) backend_kind: &'static str,
    /// The harness's PCM-patch source and the log the scene reads (design T3).
    #[cfg(feature = "browserless-harness")]
    pub(super) pcm: Option<crate::encoder::h264::harness::PcmSource>,
    /// The PCM encoder the acked-reference path runs inline in a scene, in
    /// place of the encode worker.
    #[cfg(feature = "browserless-harness")]
    pub(super) pcm_backend: Option<crate::encoder::h264::harness::PcmBackend>,
    #[cfg(feature = "browserless-harness")]
    pub(super) pcm_log: crate::encoder::h264_harness::PcmLog,
    /// See `InjectedH264::pcm_phase_a_rate`.
    #[cfg(feature = "browserless-harness")]
    pub(super) phase_a_rate_step: bool,
}

impl H264Tx {
    /// Forget the session's state (tracker, hold, pending request, rate limit).
    /// Quinn reuses connection handles and the next session starts with an
    /// IDR, so none of this may carry over.
    pub(super) fn reset_session(&mut self) {
        self.tracker.clear();
        self.hold = None;
        self.pending_idr = None;
        self.last_loss_idr_at = None;
        self.deferred_loss_idr = false;
        self.last_dims = None;
        self.last_idr_seq = None;
        self.acked_any = false;
        self.warned_no_acks = false;
        self.ignore_before = 0;
        self.doomed_from = None;
        self.last_ack_progress = None;
        self.honoured_resets.clear();
        self.reset_storm_until = None;
        self.stalled = false;
        self.stall_eval_at = None;
        self.rate.reset_session();
        self.last_idr_at = None;
        self.pad_template = None;
        // The next session builds its own manager (and may be a client that
        // does not send the v2 feedback at all).
        self.refs = None;
        self.decode_clock = None;
        self.last_sent_at = None;
        self.refused_since_decode = false;
        self.newest_decoded = None;
        #[cfg(feature = "browserless-harness")]
        if let Some(p) = self.pcm.as_mut() {
            p.session_reset();
        }
    }
}

impl IoBridge {
    /// The newest-IDR hold is over (the IDR is acknowledged, or the hold gave
    /// up, or a newer IDR replaced it): account for how long P frames waited.
    pub(super) fn end_h264_hold(&mut self, now: std::time::Instant) {
        if let Some(h) = self.h264.hold.take() {
            self.h264.stats.p_hold_ms += now.saturating_duration_since(h.since).as_millis() as u64;
        }
    }

    /// Ask the encoder for an IDR. Returns whether this started a request (as
    /// opposed to joining one already pending).
    ///
    /// The encoder's keyframe flag is idempotent, so the control is always
    /// sent; what coalesces is the attribution: while a request is pending
    /// (made, and no IDR out yet) the cause stays the first one's. Loss
    /// requests are limited to one per [`LOSS_IDR_MIN_INTERVAL`]; one turned
    /// away is made later by [`Self::h264_frame_gate`].
    pub(super) fn request_h264_idr(&mut self, cause: IdrCause, now: std::time::Instant) -> bool {
        if cause == IdrCause::Loss {
            let limited = self
                .h264
                .last_loss_idr_at
                .is_some_and(|t| now.saturating_duration_since(t) < LOSS_IDR_MIN_INTERVAL);
            if limited {
                self.h264.stats.loss_idr_rate_limited += 1;
                self.h264.deferred_loss_idr = true;
                // The IDR is still owed (see `h264_frame_gate`): the next
                // capture must be whole when it is made.
                self.ensure_baseline_invalidated();
                return false;
            }
            self.h264.last_loss_idr_at = Some(now);
            self.h264.deferred_loss_idr = false;
        }
        if cause == IdrCause::Loss {
            // On a screen that has stopped changing there would be no capture
            // with dirty tiles, so no H.264 frame, so no IDR: make the next
            // capture whole.
            self.ensure_baseline_invalidated();
        }
        self.backend_apply(Control::RequestKeyframe);
        if self.h264.pending_idr.is_some() {
            self.h264.stats.idr_requests_coalesced += 1;
            return false;
        }
        self.h264.pending_idr = Some(cause);
        self.h264.stats.idr_requests_by_cause[cause.index()] += 1;
        true
    }

    /// Record that the next frame is an IDR for `cause` without sending the
    /// encoder a request: it will make one on its own (a fresh encoder, a
    /// forced keyframe on re-entry).
    pub(super) fn expect_h264_idr(&mut self, cause: IdrCause) {
        if self.h264.pending_idr.is_none() {
            self.h264.pending_idr = Some(cause);
            self.h264.stats.idr_requests_by_cause[cause.index()] += 1;
        }
    }

    /// The next frame is the re-entry IDR from the tile codec. A loss request
    /// still pending was made before the tile stretch (and the tile codec
    /// repainted the screen since): the IDR is credited to the re-entry, which
    /// is what made it necessary now. Any other pending cause stays.
    pub(super) fn name_reentry_idr(&mut self) {
        match self.h264.pending_idr {
            None => self.expect_h264_idr(IdrCause::Reentry),
            Some(IdrCause::Loss) => {
                self.h264.pending_idr = Some(IdrCause::Reentry);
                self.h264.stats.idr_requests_by_cause[IdrCause::Reentry.index()] += 1;
            }
            Some(_) => {}
        }
    }

    /// Whether the encoder may run for this frame. See the module docs.
    pub(crate) fn h264_frame_gate(&mut self, now: std::time::Instant) -> H264Gate {
        if self.h264.deferred_loss_idr {
            // A loss IDR the rate limit turned away: due now?
            let due = self
                .h264
                .last_loss_idr_at
                .is_none_or(|t| now.saturating_duration_since(t) >= LOSS_IDR_MIN_INTERVAL);
            if due {
                self.request_h264_idr(IdrCause::Loss, now);
            }
        }
        if let Some(h) = self.h264.hold {
            if now.saturating_duration_since(h.last_progress) >= IDR_HOLD_MAX {
                self.h264.stats.hold_timeouts += 1;
                if !self.h264.acked_any && !self.h264.warned_no_acks {
                    self.h264.warned_no_acks = true;
                    tracing::warn!(
                        target: LOG_TARGET,
                        frame_seq = h.frame_seq,
                        "H.264 IDR hold timed out and this session has never acknowledged \
                         an H.264 fragment: the client does not acknowledge H.264"
                    );
                }
                self.end_h264_hold(now);
                self.request_h264_idr(IdrCause::HoldTimeout, now);
            }
        }
        // An IDR on its way supersedes what is held or queued.
        if self.h264.pending_idr.is_some() {
            return H264Gate::Encode;
        }
        // A loss IDR the rate limit is still holding back: the P frames that
        // would be encoded now chain from the suspect frame and are doomed.
        if self.h264.deferred_loss_idr {
            self.h264.stats.frames_skipped_hold += 1;
            return H264Gate::SkipHold;
        }
        if self.h264.hold.is_some() {
            self.h264.stats.frames_skipped_hold += 1;
            return H264Gate::SkipHold;
        }
        if self.h264_backlog_exceeded() {
            self.h264.stats.frames_skipped_backpressure += 1;
            if !self.reliable_emitter.pads.is_empty() {
                self.h264.stats.frames_skipped_pad += 1;
            }
            return H264Gate::SkipBackpressure;
        }
        H264Gate::Encode
    }

    /// Whether the emission queue holds more H.264 than
    /// `max(one frame period of the admission rate, two gate depths)`.
    pub(super) fn h264_backlog_exceeded(&self) -> bool {
        let rate = self.admission_rate().bytes_per_sec;
        let frame_period_s = SCHEDULER_TICK_INTERVAL_US / 1_000_000.0;
        let per_frame = (rate * frame_period_s) as usize;
        let two_depths = self.gate.map_or(0, |g| 2 * g.d_target);
        let limit = per_frame
            .max(two_depths)
            .max(self.last_max_datagram_size.unwrap_or(1200));
        self.reliable_emitter.queued_h264_bytes() > limit
    }

    /// An H.264 frame is out of the encoder: attribute it. Returns the cause
    /// when it is an IDR.
    #[cfg(test)]
    pub(super) fn note_h264_encoded(
        &mut self,
        is_keyframe: bool,
        dims: (u32, u32),
    ) -> Option<IdrCause> {
        self.note_h264_encoded_as(is_keyframe, dims, None)
    }

    /// [`Self::note_h264_encoded`] for a frame whose plan named its IDR cause.
    pub(super) fn note_h264_encoded_as(
        &mut self,
        is_keyframe: bool,
        dims: (u32, u32),
        planned: Option<IdrCause>,
    ) -> Option<IdrCause> {
        let dims_changed = self.h264.last_dims.is_some_and(|d| d != dims);
        self.h264.last_dims = Some(dims);
        if !is_keyframe {
            return None;
        }
        // A plan names its own cause; the Phase A request (made by the session
        // code, whatever the backend) is spent either way.
        let pending = self.h264.pending_idr.take();
        let cause = planned.or(pending).unwrap_or(if dims_changed {
            IdrCause::Resolution
        } else {
            IdrCause::GopPeriodic
        });
        self.h264.stats.idr_by_cause[cause.index()] += 1;
        self.h264.stats.frames_idr += 1;
        self.h264.last_idr_at = Some(now_std());
        Some(cause)
    }

    /// An IDR at `frame_seq` is about to be sent: everything older is
    /// superseded. Forget its cached fragments, take its queued ones out of
    /// the emission queue, and take their ledger records back (a datagram that
    /// never reached quinn is neither delivered nor lost).
    pub(super) fn supersede_h264_before(&mut self, frame_seq: u32, now: std::time::Instant) {
        // The ledger must hold what the emitter has reported before it can
        // forget any of it.
        self.record_emitted_transmissions(now);
        let before = self.reliable_emitter.stats.h264_queue_purged;
        let wire_seqs = self.reliable_emitter.supersede_h264_before(frame_seq);
        self.h264.stats.queue_purged += self.reliable_emitter.stats.h264_queue_purged - before;
        self.forget_purged(&wire_seqs);
    }

    pub(super) fn forget_purged(&mut self, wire_seqs: &[u32]) {
        for ws in wire_seqs {
            // Only an H.264 fragment is in the identity
            // `tx_recorded >= acked + lost + forgotten`.
            if let Some(tx) = self.transmission_ledger.take_undeparted(*ws) {
                if matches!(tx.content, Content::H264Frag { .. }) {
                    self.h264.stats.tx_forgotten += 1;
                }
            }
            // A purged pad never left: its ring entry has no acknowledgement
            // to wait for.
            self.reliable_emitter.pads.remove(*ws);
        }
    }

    /// Open the hold for the IDR at `frame_seq`, replacing an older one.
    pub(super) fn begin_h264_hold(&mut self, frame_seq: u32, now: std::time::Instant) {
        self.end_h264_hold(now);
        self.h264.hold = Some(IdrHold {
            frame_seq,
            since: now,
            last_progress: now,
        });
        self.h264.stats.p_holds += 1;
    }

    /// The frame is fully acknowledged.
    fn on_h264_frame_acked(&mut self, frame_seq: u32, now: std::time::Instant) {
        self.h264.stats.frames_acked += 1;
        self.note_raised_acked(frame_seq, now);
        if self.h264.hold.is_some_and(|h| h.frame_seq == frame_seq) {
            self.end_h264_hold(now);
        }
    }

    /// Resolved acknowledgements that named H.264 fragments: release their
    /// cache entries and tell the tracker.
    pub(super) fn apply_h264_acks(
        &mut self,
        resolved: &[(Content, Transmission, u16, bool)],
        now: std::time::Instant,
    ) {
        for (content, _, _, late) in resolved {
            if let Content::Pad(ws) = content {
                // Padding: its probe tag has been read (before this); the
                // entry has nothing more to do.
                self.reliable_emitter.pads.remove(*ws);
                continue;
            }
            let Content::H264Frag {
                frame_seq,
                frag_idx,
            } = *content
            else {
                continue;
            };
            if *late {
                self.h264.stats.tx_acked_late += 1;
            } else {
                self.h264.stats.tx_acked_live += 1;
            }
            self.h264.acked_any = true;
            self.h264.last_ack_progress = Some(now);
            if let Some(h) = self.h264.hold.as_mut().filter(|h| h.frame_seq == frame_seq) {
                h.last_progress = now;
            }
            // Keep a small P-frame fragment's bytes for probe padding before
            // the acknowledgement releases it.
            if self.reliable_emitter.h264.is_idr(frame_seq) == Some(false) {
                if let Some(e) = self.reliable_emitter.h264.get(frame_seq, frag_idx) {
                    if e.datagram.len() >= super::h264_rate::PAD_MIN_TEMPLATE_BYTES {
                        self.h264.pad_template = Some(e.datagram.clone());
                    }
                }
            }
            self.reliable_emitter.on_h264_ack(frame_seq, frag_idx);
            // An IDR draining on a slow link is progress, as for Phase A.
            // A P frame's fragment acknowledged is progress too, until the client
            // has refused something since it last decoded: a 0x0A blackout with
            // the transport acks flowing is not a stall.
            if self.h264_refs_live()
                && (self.reliable_emitter.h264.is_idr(frame_seq) == Some(true)
                    || !self.h264.refused_since_decode)
            {
                self.h264.decode_clock = Some(now);
            }
            if let Some(st) = self.h264.refs.as_mut() {
                st.mgr.frag_acked(frame_seq, frag_idx, now);
            }
            if let FragAck::Acked { complete: true } =
                self.h264.tracker.on_frag_acked(frame_seq, frag_idx)
            {
                self.on_h264_frame_acked(frame_seq, now);
            }
        }
    }

    /// A transmission of an H.264 fragment was judged lost (by a threshold or
    /// by the probe's content-gone rule). An IDR fragment was sent again by the
    /// loss path already. A P-frame fragment makes its frame suspect: nothing
    /// that chains from it can be decoded, so the queued fragments of it and
    /// of every later frame are purged, and an IDR is requested.
    #[cfg(test)]
    pub(super) fn note_h264_verdict(
        &mut self,
        frame_seq: u32,
        frag_idx: u16,
        now: std::time::Instant,
    ) {
        self.note_h264_verdict_in(frame_seq, frag_idx, now, false);
    }

    /// [`Self::note_h264_verdict`] for a datagram that did (`in_burst`) or did
    /// not depart inside a probe burst (read from the ledger, by departure,
    /// not by when the verdict came).
    pub(super) fn note_h264_verdict_in(
        &mut self,
        frame_seq: u32,
        frag_idx: u16,
        now: std::time::Instant,
        in_burst: bool,
    ) {
        self.h264.stats.tx_judged_lost += 1;
        let Some(f) = self.h264.tracker.get(frame_seq) else {
            self.h264.stats.verdicts_unknown_frame += 1;
            return;
        };
        if f.complete() || f.frag_acked(frag_idx) {
            self.h264.stats.verdicts_already_acked += 1;
            return;
        }
        if f.is_idr {
            self.h264.stats.verdicts_idr_frag += 1;
            return;
        }
        self.h264.stats.verdicts_p_frag += 1;
        if in_burst {
            self.h264.stats.p_verdicts_in_probe += 1;
        }
        // An acked-reference session recovers from its anchor: the verdict
        // makes the frame suspect and the interim loss IDR below is not made.
        let already_suspect = f.suspect;
        self.note_raised_lost(frame_seq, now);
        if self.h264_refs_live() {
            self.h264_refs_verdict(frame_seq, now);
            return;
        }
        if already_suspect {
            return;
        }
        // A frame an IDR has superseded, or will (one is on its way), is
        // doomed already: asking again would make a second IDR for the same
        // loss once the interval passed.
        if self.h264.pending_idr.is_some() || self.h264.last_idr_seq.is_some_and(|i| frame_seq < i)
        {
            self.h264.stats.verdicts_superseded += 1;
            return;
        }
        if let Some(f) = self.h264.tracker.get_mut(frame_seq) {
            f.suspect = true;
        }
        self.h264.doomed_from = Some(
            self.h264
                .doomed_from
                .map_or(frame_seq, |d| d.min(frame_seq)),
        );
        self.h264.stats.frames_suspect += 1;
        self.record_emitted_transmissions(now);
        let before = self.reliable_emitter.stats.h264_queue_purged;
        let wire_seqs = self.reliable_emitter.purge_queued_h264(|f| f >= frame_seq);
        self.h264.stats.queue_purged += self.reliable_emitter.stats.h264_queue_purged - before;
        self.forget_purged(&wire_seqs);
        // And the probe cache: entries for bytes that never left would keep
        // the probe timer and bytes-in-flight counting them.
        self.reliable_emitter.drop_h264_from(frame_seq);
        self.request_h264_idr(IdrCause::Loss, now);
    }

    /// Evaluate the `h264_stalled` flag at `now`, publish it to the
    /// classifier's context, and return it. Called once per frame decision,
    /// just before the classifier runs.
    ///
    /// Raised by any of: (a) unresolved frames beyond
    /// [`stall_inflight_bound`]; (b) frames in flight and none fully
    /// acknowledged for [`STALL_NO_PROGRESS`] (Phase A: progress is any H.264
    /// fragment the 0x06 acknowledgements name; the frame-ack datagram does
    /// not exist yet); (c) [`STALL_RESET_COUNT`] honoured client reset
    /// requests within [`STALL_RESET_WINDOW`] (see
    /// [`Self::note_h264_reset_honoured`]).
    pub(super) fn update_h264_stall(&mut self, now: std::time::Instant) -> bool {
        let floor = self
            .h264
            .ignore_before
            .max(self.h264.last_idr_seq.unwrap_or(0));
        let (in_flight, oldest) = self.h264.tracker.unresolved(floor, self.h264.doomed_from);
        let bound = stall_inflight_bound(self.adaptation_context.smoothed_rtt_us);
        let over_bound = in_flight > bound;
        let no_progress = if self.h264_refs_live() {
            // Frames were sent since the client last decoded one, and it has
            // not for 500 ms: a client that refuses everything is stalled
            // however well the transport delivers.
            match (self.h264.decode_clock, self.h264.last_sent_at) {
                (Some(d), Some(sent)) => {
                    sent > d && now.saturating_duration_since(d) >= STALL_NO_PROGRESS
                }
                _ => false,
            }
        } else {
            oldest.is_some_and(|oldest| {
                let since = self
                    .h264
                    .last_ack_progress
                    .map_or(oldest, |p| p.max(oldest));
                now.saturating_duration_since(since) >= STALL_NO_PROGRESS
            })
        };
        let reset_storm = self.h264.reset_storm_until.is_some_and(|t| now < t);
        let stalled = over_bound || no_progress || reset_storm;

        let was = self.h264.stalled;
        if was {
            if let Some(t) = self.h264.stall_eval_at {
                self.h264.stats.h264_stalled_ms +=
                    now.saturating_duration_since(t).as_millis() as u64;
            }
        }
        if stalled && !was {
            let st = &mut self.h264.stats;
            if over_bound {
                st.stalls_inflight += 1;
            } else if no_progress {
                st.stalls_no_progress += 1;
            } else {
                st.stalls_reset += 1;
            }
            tracing::info!(
                target: LOG_TARGET,
                in_flight,
                bound,
                over_bound,
                no_progress,
                reset_storm,
                "h264 stalled: the classifier is told to leave H.264"
            );
        }
        self.h264.stalled = stalled;
        self.h264.stall_eval_at = Some(now);
        if self.adaptation_context.h264_stalled != stalled {
            self.adaptation_context.h264_stalled = stalled;
            self.adaptation_context.last_update_seq =
                self.adaptation_context.last_update_seq.wrapping_add(1);
            self.classifier
                .set_adaptation_context(self.adaptation_context);
        }
        stalled
    }

    /// A client reset request was honoured at `now`. The third within
    /// [`STALL_RESET_WINDOW`] raises the stall flag for [`STALL_RESET_HOLD`]:
    /// a client that keeps asking is not helped by the IDRs it gets.
    pub(super) fn note_h264_reset_honoured(&mut self, now: std::time::Instant) {
        let r = &mut self.h264.honoured_resets;
        r.push(now);
        if r.len() > STALL_RESET_COUNT {
            r.remove(0);
        }
        if r.len() == STALL_RESET_COUNT && now.saturating_duration_since(r[0]) <= STALL_RESET_WINDOW
        {
            self.h264.reset_storm_until = Some(now + STALL_RESET_HOLD);
        }
    }

    /// The session left H.264 mode at `frame_seq`: frames sent so far are no
    /// longer in flight as far as the stall flag is concerned (the tile codec
    /// is carrying the screen now; their fate is the ledger's business).
    ///
    /// What was still queued for them is purged and their probe-cache entries
    /// dropped (they would compete with the tile repaint for the same pacer
    /// and bytes-in-flight, and the client is no longer decoding them), and
    /// any P hold ends. Retransmissions already with quinn are left alone.
    pub(super) fn abandon_h264_inflight(&mut self, frame_seq: u32, now: std::time::Instant) {
        self.h264.ignore_before = self.h264.ignore_before.max(frame_seq);
        self.supersede_h264_before(frame_seq, now);
        self.end_h264_hold(now);
        // An IDR that was still being delivered is gone with the purge: the
        // manager sends it again at the return, not after its timeout.
        if self.h264_refs_live() {
            if let Some(st) = self.h264.refs.as_mut() {
                st.mgr.idr_abandoned();
            }
        }
        // Only frames sent after this count for the stall flag (the Phase A
        // `ignore_before`): the last ones were purged or will never be
        // answered, and nothing is sent while the tile codec has the screen.
        self.h264.last_sent_at = None;
        self.h264.decode_clock = None;
        self.h264.refused_since_decode = false;
        self.h264.newest_decoded = None;
        // The rate side of H.264 lets go here too, in one place: the padding in
        // flight is forgotten and the application's probe cadence starts over
        // (`h264_reset_probing`), the rate policy's estimate restarts (the
        // encoder keeps its rate), and the GOP clock is cleared (the return is
        // an IDR, which starts it again).
        self.h264_reset_probing();
        self.h264.rate.reset_session();
        self.h264.last_idr_at = None;
    }

    /// Route the loss verdicts of a pass of the loss path to the H.264 policy.
    pub(super) fn note_h264_verdicts(
        &mut self,
        lost: &[(u32, Transmission)],
        now: std::time::Instant,
    ) {
        for (ws, tx) in lost {
            if let Content::Pad(pad_ws) = tx.content {
                // A pad judged lost: its tag will not be read. The burst it was
                // part of overran the path: a raise in its place would have
                // lost real frames.
                self.reliable_emitter.pads.remove(pad_ws);
                self.h264.raise_paused_until = Some(now + super::h264_refs::RAISE_LOSS_PAUSE);
                self.h264.stats.pads_lost += 1;
                continue;
            }
            if let Content::H264Frag {
                frame_seq,
                frag_idx,
            } = tx.content
            {
                let in_burst = self.transmission_ledger.declared_lost_in_burst(*ws);
                self.note_h264_verdict_in(frame_seq, frag_idx, now, in_burst);
            }
        }
    }

    /// The browserless harness's scripted H.264 frame (see [`InjectedH264`]):
    /// the production gate and send path with a scripted encoder in place of
    /// the GPU and ffmpeg. Nothing is sent without a live session, as the tile
    /// injection path.
    pub(super) fn apply_injected_h264(&mut self, seq: u32, timestamp_us: u32, h: InjectedH264) {
        let Some(max_dg_size) = self.compute_max_datagram_size() else {
            return;
        };
        let now = now_std();
        #[cfg(feature = "browserless-harness")]
        let refs_scene = h.pcm == Some(crate::encoder::h264_harness::PcmMode::RefManager);
        #[cfg(feature = "browserless-harness")]
        self.note_injected_h264_mode(refs_scene);
        // The classifier is not in this scripted path; the flag still is.
        self.update_h264_stall(now);
        self.emit_frame_dimensions(seq, timestamp_us, h.width, h.height);
        #[cfg(feature = "browserless-harness")]
        if refs_scene {
            self.apply_injected_pcm_refs(seq, timestamp_us, h, max_dg_size, now);
            return;
        }
        if self.h264_frame_gate(now) != H264Gate::Encode {
            // Skipped: the admission pass still runs, as it does for a frame
            // the production gate drops (the loop turns on regardless).
            self.pump_admission();
            return;
        }
        #[cfg(feature = "browserless-harness")]
        if h.pcm.is_some() {
            self.apply_injected_pcm(seq, timestamp_us, h, max_dg_size);
            return;
        }
        let (is_idr, len) = if let Some(cbr) = h.cbr {
            // A CBR encoder: the real rate policy runs, and the scripted
            // encoder takes its controls (see `h264_rate`).
            let gop = cbr.gop.unwrap_or(self.h264.gop);
            self.h264
                .scripted
                .get_or_insert_with(|| ScriptedCbr::new(cbr));
            self.h264_rate_step(now, gop, cbr.fps);
            let requested = self.h264.pending_idr.is_some();
            self.h264
                .scripted
                .as_mut()
                .expect("created just above")
                .next_frame(requested)
        } else {
            let periodic = h.gop_frames > 0
                && self
                    .h264
                    .stats
                    .frames_sent
                    .is_multiple_of(u64::from(h.gop_frames));
            // The scripted encoder makes an IDR exactly when the bridge asked
            // for one, on its GOP, and for the first frame of a stream.
            let is_idr =
                self.h264.pending_idr.is_some() || periodic || self.h264.stats.frames_sent == 0;
            (is_idr, if is_idr { h.idr_bytes } else { h.p_bytes })
        };
        // Deterministic, incompressible-looking filler; the content is not read.
        let mut x = seq.wrapping_mul(0x9E37_79B1) | 1;
        let payload: Vec<u8> = (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        let meta = frame::FrameMeta {
            width: h.width,
            height: h.height,
            stride: h.width * 4,
            timestamp_us,
            capture_done_ns: 0,
            unchanged: false,
        };
        self.send_h264_frame(
            H264Out {
                payload,
                is_keyframe: is_idr,
            },
            &meta,
            seq,
            max_dg_size,
        );
    }

    /// The injected stream is in H.264 mode now. Coming back from a stretch of
    /// tile frames the next frame is a re-entry IDR, as on the capture path
    /// (`finish_h264_arm`), or a recovery P from the anchor on an
    /// acked-reference stream (`refs_scene`); the first H.264 frame of the
    /// scene is not one.
    #[cfg(feature = "browserless-harness")]
    fn note_injected_h264_mode(&mut self, refs_scene: bool) {
        let prev = std::mem::replace(&mut self.frame_mode, crate::tile::FrameMode::H264);
        if prev == crate::tile::FrameMode::TileCodec && self.h264.stats.frames_sent > 0 {
            if refs_scene {
                self.h264_refs_reentry(now_std());
            } else {
                self.name_reentry_idr();
            }
        }
    }

    /// The log of the PCM source: what each frame was meant to show, and the
    /// `RefManager`'s counters. Read it after the scene.
    #[cfg(feature = "browserless-harness")]
    pub fn h264_pcm_log_handle(&self) -> crate::encoder::h264_harness::PcmLog {
        self.h264.pcm_log.clone()
    }

    /// A scripted H.264 frame whose payload is a real PCM-patch stream (see
    /// [`crate::encoder::h264_harness`]), Phase A policy. Past the production
    /// gate, the frame goes through the same `send_h264_frame` as every other.
    #[cfg(feature = "browserless-harness")]
    fn apply_injected_pcm(
        &mut self,
        seq: u32,
        timestamp_us: u32,
        h: InjectedH264,
        max_dg_size: usize,
    ) {
        use crate::encoder::h264::harness::{pad_to, PcmSource};
        let meta = frame::FrameMeta {
            width: h.width,
            height: h.height,
            stride: h.width * 4,
            timestamp_us,
            capture_done_ns: 0,
            unchanged: false,
        };
        let log = self.h264.pcm_log.clone();
        let source = self.h264.pcm.get_or_insert_with(|| PcmSource::new(log));
        source.set_wrong_ref_every(h.pcm_wrong_ref_every);
        let periodic = h.gop_frames > 0
            && self
                .h264
                .stats
                .frames_sent
                .is_multiple_of(u64::from(h.gop_frames));
        let asked = self.h264.pending_idr.is_some() || periodic || self.h264.stats.frames_sent == 0;
        let (mut payload, is_idr) = source.legacy_frame(seq, asked);
        pad_to(&mut payload, if is_idr { h.idr_bytes } else { h.p_bytes });
        self.send_h264_frame(
            H264Out {
                payload,
                is_keyframe: is_idr,
            },
            &meta,
            seq,
            max_dg_size,
        );
    }

    /// A scripted H.264 frame of an acked-reference session: production's
    /// plan and commit (`h264_refs`) around a PCM encoder run inline, in place
    /// of the encode worker. The stepped harness cannot run a worker thread;
    /// everything else, the manager, the wire header, the send path, the
    /// ledger feedback and the client's own 0x0A and 0x09, is production's.
    #[cfg(feature = "browserless-harness")]
    fn apply_injected_pcm_refs(
        &mut self,
        seq: u32,
        timestamp_us: u32,
        h: InjectedH264,
        max_dg_size: usize,
        now: std::time::Instant,
    ) {
        use crate::encoder::h264::backend::{H264Backend, Nv12View};
        use crate::encoder::h264::harness::PcmBackend;
        assert!(
            self.h264_refs_active(),
            "an acked-reference scene needs a client that sends HELLO 0x04"
        );
        let meta = frame::FrameMeta {
            width: h.width,
            height: h.height,
            stride: h.width * 4,
            timestamp_us,
            capture_done_ns: 0,
            unchanged: false,
        };
        self.h264.refs_mode = super::h264_refs::RefsMode::Explicit;
        self.h264.backend_kind = "pcm";
        self.h264.phase_a_rate_step = h.pcm_phase_a_rate;
        self.h264.raise_mutated_off = h.no_probe_raise;
        let (hold_ms, releasable) = h.spike_tune.map_or((None, true), |(ms, r)| (Some(ms), r));
        self.h264
            .rate
            .tune_spike_for_harness(hold_ms.map(std::time::Duration::from_millis), releasable);
        let Some(req) = self.h264_refs_plan(seq, (h.width, h.height), now, h.pcm_rate_policy)
        else {
            // Skipped, as the capture path drops a frame it holds back; the
            // admission pass still runs.
            self.pump_admission();
            return;
        };
        let log = self.h264.pcm_log.clone();
        let backend = self
            .h264
            .pcm_backend
            .get_or_insert_with(|| PcmBackend::new(log));
        backend.set_sizes(h.idr_bytes, h.p_bytes);
        backend.set_model(h.pcm_rate_model);
        // The rate travels with the plan, as the encode worker applies it.
        backend.set_rate(req.rate);
        let nv12 = Nv12View {
            data: &[],
            width: 0,
            height: 0,
            y_stride: 0,
            uv_stride: 0,
            uv_offset: 0,
        };
        let au = backend
            .encode(&nv12, &req.plan, &req.refs)
            .expect("the PCM backend executes every plan");
        self.h264
            .pcm_log
            .lock()
            .expect("pcm log poisoned")
            .au_log
            .push(crate::encoder::h264::harness::AuRecord {
                timestamp_us,
                bytes: au.payload.len(),
                idr: au.meta.idr,
                raised: self.h264.rate.applied().is_some_and(|a| a != req.rate.bps),
            });
        self.h264_refs_commit(&req.plan, au, &meta, max_dg_size, now);
    }

    /// Forget the H.264 state of the session that ended: tracker, hold,
    /// pending request, rate limit, the probe cache and what was queued.
    /// Queued fragments belong to a client that is gone; sending them to the
    /// next one would put a stale frame in its window.
    pub(super) fn reset_h264_session(&mut self) {
        self.h264.reset_session();
        self.h264_reset_probing();
        self.reliable_emitter.h264.clear();
        let wire_seqs = self.reliable_emitter.purge_queued_h264(|_| true);
        // The ledger is reset with the session by its callers; there is
        // nothing to take back from it.
        let _ = wire_seqs;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tracker_completes_a_frame_exactly_once_and_ignores_duplicates() {
        let mut t = FrameTracker::default();
        t.on_frame_sent(7, true, 70, now_std());
        for i in 0..69 {
            assert_eq!(t.on_frag_acked(7, i), FragAck::Acked { complete: false });
        }
        assert_eq!(t.on_frag_acked(7, 3), FragAck::Duplicate);
        assert_eq!(t.is_complete(7), Some(false));
        assert_eq!(t.on_frag_acked(7, 69), FragAck::Acked { complete: true });
        assert_eq!(t.on_frag_acked(7, 69), FragAck::Duplicate);
        assert_eq!(t.is_complete(7), Some(true));
        assert_eq!(t.on_frag_acked(7, 70), FragAck::Duplicate, "out of range");
    }

    #[test]
    fn a_slot_reused_by_a_newer_frame_does_not_answer_for_the_old_one() {
        let mut t = FrameTracker::default();
        t.on_frame_sent(5, false, 2, now_std());
        t.on_frame_sent(5 + TRACKER_RING as u32, false, 2, now_std());
        assert_eq!(t.on_frag_acked(5, 0), FragAck::Unknown);
        assert!(t.get(5).is_none());
        assert_eq!(
            t.on_frag_acked(5 + TRACKER_RING as u32, 0),
            FragAck::Acked { complete: false }
        );
    }

    #[test]
    fn clearing_forgets_every_frame() {
        let mut t = FrameTracker::default();
        t.on_frame_sent(1, true, 1, now_std());
        t.clear();
        assert_eq!(t.on_frag_acked(1, 0), FragAck::Unknown);
    }

    #[test]
    fn every_cause_has_a_distinct_index_and_name() {
        let mut names: Vec<_> = IdrCause::ALL.iter().map(|c| c.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), IdrCause::COUNT);
        for (i, c) in IdrCause::ALL.iter().enumerate() {
            assert_eq!(c.index(), i);
        }
    }

    // ---- the bridge-level policy -------------------------------------------

    use crate::transport::protocol::NO_ACK;
    use crate::transport::reliable_emitter::Content;
    use ghostframe_protocol::ack::{AckBatch, AckEntry};
    use std::time::Duration;

    const PAYLOAD: usize = 4000;
    const DG: usize = 1200;

    async fn bridge() -> IoBridge {
        let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("QuicServer::new");
        IoBridge::new_with_stream_for_test(ours, server)
    }

    fn meta() -> frame::FrameMeta {
        frame::FrameMeta {
            width: 64,
            height: 64,
            stride: 256,
            timestamp_us: 0,
            capture_done_ns: 0,
            unchanged: false,
        }
    }

    /// Send a frame. An IDR is a requested one (the session's) unless the test
    /// has named another cause: the encoder's own periodic IDR holds nothing.
    fn send(b: &mut IoBridge, seq: u32, idr: bool) {
        if idr && b.h264.pending_idr.is_none() {
            b.h264.pending_idr = Some(IdrCause::Session);
        }
        b.send_h264_frame(
            H264Out {
                payload: vec![seq as u8; PAYLOAD],
                is_keyframe: idr,
            },
            &meta(),
            seq,
            DG,
        );
    }

    /// `(wire_seq, frame_seq, frag_idx)` of every tracked frame datagram sent
    /// since `from` in the bridge's send log.
    fn tracked_sent(b: &IoBridge, from: usize) -> Vec<(u32, u32, u16)> {
        b.sent_log_for_test[from..]
            .iter()
            .filter(|d| d.len() >= 22 && d[0] & 0x80 == 0)
            .filter_map(|d| {
                let ws = u32::from_be_bytes(d[8..12].try_into().unwrap());
                (ws != NO_ACK).then(|| {
                    (
                        ws,
                        u32::from_be_bytes(d[0..4].try_into().unwrap()),
                        u16::from_be_bytes(d[4..6].try_into().unwrap()),
                    )
                })
            })
            .collect()
    }

    /// What quinn would do with what was sent: the datagrams leave its FIFO.
    fn depart(b: &mut IoBridge, sent: &[(u32, u32, u16)]) {
        let now = now_std();
        b.record_emitted_transmissions(now);
        for (ws, _, _) in sent {
            b.transmission_ledger.mark_departed(*ws, now, 0);
            b.reliable_emitter.on_departed(*ws, now);
        }
    }

    fn ack(b: &mut IoBridge, wire_seqs: &[u32]) {
        for chunk in wire_seqs.chunks(64) {
            let batch = AckBatch {
                entries: chunk
                    .iter()
                    .map(|ws| AckEntry {
                        wire_seq: *ws,
                        arrival_time_ms_lo16: 0,
                    })
                    .collect(),
            };
            b.dispatch_ack_datagram(&batch.encode());
        }
    }

    fn h264_outstanding(b: &IoBridge) -> usize {
        b.transmission_ledger
            .count_outstanding(|c| matches!(c, Content::H264Frag { .. }))
    }

    /// IDR requests made so far (a request that joined a pending one counts).
    /// The control that reaches the encoder is covered with a recording
    /// backend in `tests.rs`; this bridge has no frame worker.
    fn idr_requests(b: &IoBridge) -> u64 {
        b.h264.stats.idr_requests_by_cause.iter().sum::<u64>() + b.h264.stats.idr_requests_coalesced
    }

    /// Every H.264 transmission with a real wire_seq has a ledger record, and
    /// every record is exactly one of: acknowledged, judged lost, taken back
    /// before it left, still outstanding. Checked over a sequence that uses all
    /// four outcomes.
    #[tokio::test(start_paused = true)]
    async fn every_recorded_h264_transmission_has_exactly_one_outcome() {
        let mut b = bridge().await;
        // Frames 1..=2 go out and depart.
        send(&mut b, 1, true);
        send(&mut b, 2, false);
        let first = tracked_sent(&b, 0);
        assert!(
            first.len() >= 8,
            "premise: several fragments: {}",
            first.len()
        );
        // Nothing drains from here: the FIFO stands at the gate. Frame 3 and 4
        // wait in the queue, recorded but never departed.
        depart(&mut b, &first);
        b.forced_fifo_depth_for_test = Some(usize::MAX / 2);
        let mark = b.sent_log_for_test.len();
        send(&mut b, 3, false);
        send(&mut b, 4, false);
        assert_eq!(tracked_sent(&b, mark).len(), 0, "premise: all held back");
        b.record_emitted_transmissions(now_std());
        let queued = b.reliable_emitter.queued_h264_bytes();
        assert!(queued > 0, "premise: fragments are queued");
        // An IDR at 5 supersedes 3 and 4 (and the cached 1 and 2).
        send(&mut b, 5, true);
        assert!(
            b.h264.stats.tx_forgotten > 0,
            "premise: some were taken back"
        );
        // Acknowledge all of frame 1 and half of frame 2; the rest is judged
        // lost by the threshold once later departures are acknowledged.
        b.forced_fifo_depth_for_test = None;
        let mark = b.sent_log_for_test.len();
        b.pump_admission();
        let idr5 = tracked_sent(&b, mark);
        depart(&mut b, &idr5);
        let f1: Vec<u32> = first.iter().filter(|x| x.1 == 1).map(|x| x.0).collect();
        let f2: Vec<u32> = first.iter().filter(|x| x.1 == 2).map(|x| x.0).collect();
        ack(&mut b, &f1);
        ack(&mut b, &f2[..f2.len() / 2]);
        let newest: Vec<u32> = idr5.iter().map(|x| x.0).collect();
        ack(&mut b, &newest);
        let later = now_std() + Duration::from_secs(5);
        b.expire_transmissions(later);

        let s = b.h264.stats;
        assert!(s.tx_judged_lost > 0, "premise: a verdict was issued: {s:?}");
        assert!(s.tx_acked_live > 0 && s.tx_forgotten > 0);
        assert_eq!(
            s.tx_recorded,
            s.tx_acked_live + s.tx_judged_lost + s.tx_forgotten + h264_outstanding(&b) as u64,
            "{s:?}"
        );
        // Each tracked wire_seq the bridge sent was recorded: the log and the
        // ledger agree on the count of first and later transmissions.
        let sent_total = tracked_sent(&b, 0).len() as u64;
        assert!(
            s.tx_recorded >= sent_total,
            "{} < {}",
            s.tx_recorded,
            sent_total
        );
    }

    /// P frames are held from the moment an IDR is sent until every one of its
    /// fragments is acknowledged.
    #[tokio::test(start_paused = true)]
    async fn p_frames_are_held_until_every_idr_fragment_is_acknowledged() {
        let mut b = bridge().await;
        send(&mut b, 1, true);
        let sent = tracked_sent(&b, 0);
        assert!(sent.len() >= 4);
        depart(&mut b, &sent);
        assert!(b.h264.hold.is_some());
        assert_eq!(b.h264_frame_gate(now_std()), H264Gate::SkipHold);
        // All but one acknowledged: still held.
        let ws: Vec<u32> = sent.iter().map(|x| x.0).collect();
        ack(&mut b, &ws[..ws.len() - 1]);
        assert_eq!(b.h264_frame_gate(now_std()), H264Gate::SkipHold);
        tokio::time::advance(Duration::from_millis(40)).await;
        ack(&mut b, &ws[ws.len() - 1..]);
        assert!(
            b.h264.hold.is_none(),
            "released by the last acknowledgement"
        );
        assert_eq!(b.h264_frame_gate(now_std()), H264Gate::Encode);
        assert!(
            b.h264.stats.p_hold_ms >= 40,
            "the time P frames waited is accounted: {}",
            b.h264.stats.p_hold_ms
        );
        assert_eq!(b.h264.stats.frames_skipped_hold, 2);
    }

    /// An IDR the client never finishes acknowledging does not freeze the
    /// stream for good: the hold gives up and a fresh IDR is asked for.
    #[tokio::test(start_paused = true)]
    async fn the_idr_hold_gives_up_and_asks_for_a_new_idr() {
        let mut b = bridge().await;
        send(&mut b, 1, true);
        tokio::time::advance(IDR_HOLD_MAX - Duration::from_millis(1)).await;
        assert_eq!(b.h264_frame_gate(now_std()), H264Gate::SkipHold);
        tokio::time::advance(Duration::from_millis(2)).await;
        assert_eq!(b.h264_frame_gate(now_std()), H264Gate::Encode);
        assert_eq!(b.h264.stats.hold_timeouts, 1);
        assert_eq!(b.h264.pending_idr, Some(IdrCause::HoldTimeout));
        assert!(
            b.h264.warned_no_acks,
            "a hold that timed out on a session that never acknowledged anything is called out"
        );
        assert_eq!(idr_requests(&b), 1);
        assert!(b.h264.hold.is_none());
    }

    /// A loss verdict on a P fragment makes one IDR request, purges the
    /// frames that chain from it, and a second inside the interval is turned
    /// away and made when the interval has passed.
    #[tokio::test(start_paused = true)]
    async fn a_p_fragment_verdict_requests_one_idr_per_interval_and_purges_the_queue() {
        let mut b = bridge().await;
        send(&mut b, 1, true);
        let idr = tracked_sent(&b, 0);
        depart(&mut b, &idr);
        let ws: Vec<u32> = idr.iter().map(|x| x.0).collect();
        ack(&mut b, &ws);
        assert_eq!(b.h264_frame_gate(now_std()), H264Gate::Encode);
        // P frames 2 and 3 are queued (the FIFO is at the gate), 4 is not.
        b.forced_fifo_depth_for_test = Some(usize::MAX / 2);
        send(&mut b, 2, false);
        send(&mut b, 3, false);
        let queued_before = b.reliable_emitter.queued_h264_bytes();
        assert!(queued_before > 0);
        b.h264.pending_idr = None; // the IDR above was made; ask afresh
        let t0 = now_std();
        b.note_h264_verdict(2, 0, t0);
        assert_eq!(b.h264.pending_idr, Some(IdrCause::Loss));
        assert_eq!(idr_requests(&b), 1);
        assert_eq!(b.h264.stats.frames_suspect, 1);
        assert_eq!(
            b.reliable_emitter.queued_h264_bytes(),
            0,
            "frames 2 and 3 chain from the lost one and are purged"
        );
        assert!(b.h264.stats.queue_purged > 0);
        // The probe cache goes too: nothing of frames 2 and 3 will be probed,
        // and the bytes-in-flight figure must not count bytes that never left.
        assert!(b.reliable_emitter.h264.is_empty());
        assert_eq!(b.reliable_emitter.bytes_in_flight(), 0);
        assert!(b.reliable_emitter.pto_deadline().is_none());
        // A second fragment of the same frame is the same suspicion.
        b.note_h264_verdict(2, 1, t0);
        assert_eq!(b.h264.stats.frames_suspect, 1);
        assert_eq!(b.h264.stats.verdicts_p_frag, 2);

        // The IDR is made; a new loss 500 ms later is turned away, remembered,
        // and made once the second is up.
        assert_eq!(b.note_h264_encoded(true, (64, 64)), Some(IdrCause::Loss));
        let base = idr_requests(&b);
        tokio::time::advance(Duration::from_millis(500)).await;
        send(&mut b, 4, false);
        b.note_h264_verdict(4, 0, now_std());
        assert_eq!(b.h264.pending_idr, None, "inside the interval: no request");
        assert_eq!(idr_requests(&b), base);
        assert_eq!(b.h264.stats.loss_idr_rate_limited, 1);
        assert!(b.h264.deferred_loss_idr);
        b.h264.hold = None;
        // While the IDR is owed, the P frames that would be encoded chain from
        // the suspect frame: they are not encoded.
        assert_eq!(b.h264_frame_gate(now_std()), H264Gate::SkipHold);
        assert_eq!(b.h264.pending_idr, None, "still inside the interval");
        tokio::time::advance(Duration::from_millis(500)).await;
        b.h264_frame_gate(now_std());
        assert_eq!(b.h264.pending_idr, Some(IdrCause::Loss), "due: made now");
        assert_eq!(idr_requests(&b), base + 1);
        assert!(!b.h264.deferred_loss_idr);
    }

    /// A verdict on a frame an IDR has superseded (or is about to) asks for
    /// nothing, and does not leave a request waiting for the rate limit.
    #[tokio::test(start_paused = true)]
    async fn a_verdict_on_a_frame_an_idr_already_covers_asks_for_nothing() {
        let mut b = bridge().await;
        send(&mut b, 1, true);
        let idr = tracked_sent(&b, 0);
        depart(&mut b, &idr);
        let ws: Vec<u32> = idr.iter().map(|x| x.0).collect();
        ack(&mut b, &ws);
        b.h264.pending_idr = None;
        send(&mut b, 2, false);
        send(&mut b, 3, false);
        let sent = tracked_sent(&b, 0);
        depart(&mut b, &sent);
        let t = now_std();
        b.note_h264_verdict(2, 0, t);
        assert_eq!(b.h264.pending_idr, Some(IdrCause::Loss));
        let base = idr_requests(&b);
        // Frame 3 chains from the lost one; the IDR on its way covers it.
        b.note_h264_verdict(3, 0, t + Duration::from_millis(10));
        assert_eq!(b.h264.stats.verdicts_superseded, 1);
        assert_eq!(idr_requests(&b), base, "no second request");
        assert!(!b.h264.deferred_loss_idr, "and none waiting for the limit");
        assert_eq!(b.h264.stats.loss_idr_rate_limited, 0);
        // The IDR goes out as frame 4: a late verdict on frame 3 is moot.
        send(&mut b, 4, true);
        assert_eq!(b.h264.pending_idr, None, "the IDR consumed the request");
        b.note_h264_verdict(3, 1, t + Duration::from_millis(20));
        assert_eq!(b.h264.stats.verdicts_superseded, 2);
        assert!(!b.h264.deferred_loss_idr);
        tokio::time::advance(LOSS_IDR_MIN_INTERVAL * 2).await;
        b.h264.hold = None;
        b.h264_frame_gate(now_std());
        assert_eq!(
            b.h264.pending_idr, None,
            "nothing was left to be made later"
        );
    }

    /// An IDR fragment is repaired by the loss path, not by a new IDR. A
    /// verdict on a fragment another transmission already delivered is
    /// spurious and does nothing.
    #[tokio::test(start_paused = true)]
    async fn idr_and_spurious_verdicts_request_no_idr() {
        let mut b = bridge().await;
        send(&mut b, 1, true);
        let idr = tracked_sent(&b, 0);
        depart(&mut b, &idr);
        b.h264.pending_idr = None;
        b.note_h264_verdict(1, 0, now_std());
        assert_eq!(b.h264.stats.verdicts_idr_frag, 1);
        assert_eq!(b.h264.pending_idr, None);
        assert_eq!(idr_requests(&b), 0);

        // A P frame whose fragment was acknowledged through a probe copy.
        b.h264.hold = None;
        send(&mut b, 2, false);
        let sent = tracked_sent(&b, 0);
        depart(&mut b, &sent);
        let (ws, f, i) = *sent.iter().rfind(|x| x.1 == 2).expect("a frame-2 fragment");
        ack(&mut b, &[ws]);
        b.note_h264_verdict(f, i, now_std());
        assert_eq!(b.h264.stats.verdicts_already_acked, 1);
        assert_eq!(b.h264.pending_idr, None);
        assert_eq!(idr_requests(&b), 0);
        // And a frame the tracker no longer follows.
        b.note_h264_verdict(9_999, 0, now_std());
        assert_eq!(b.h264.stats.verdicts_unknown_frame, 1);
    }

    /// The encoder is not run while the queue holds more H.264 than a frame
    /// period carries; a pending IDR runs it anyway.
    #[tokio::test(start_paused = true)]
    async fn the_encoder_skips_frames_while_the_queue_is_backed_up() {
        let mut b = bridge().await;
        // Start from a settled stream: the IDR is out and acknowledged.
        send(&mut b, 1, true);
        let idr = tracked_sent(&b, 0);
        depart(&mut b, &idr);
        let ws: Vec<u32> = idr.iter().map(|x| x.0).collect();
        ack(&mut b, &ws);
        b.h264.pending_idr = None;
        assert_eq!(
            b.h264_frame_gate(now_std()),
            H264Gate::Encode,
            "empty queue"
        );

        // The FIFO is at the gate: frames pile up in the queue.
        b.forced_fifo_depth_for_test = Some(usize::MAX / 2);
        let mut seq = 2;
        while b.h264_frame_gate(now_std()) == H264Gate::Encode {
            send(&mut b, seq, false);
            seq += 1;
            assert!(seq < 200, "the gate never closed");
        }
        assert!(
            seq > 2,
            "the first frame after an empty queue is always encoded"
        );
        assert!(b.h264.stats.frames_skipped_backpressure >= 1);
        let skipped = b.h264.stats.frames_skipped_backpressure;
        // A pending IDR supersedes the backlog.
        b.h264.pending_idr = Some(IdrCause::ClientReset);
        assert_eq!(b.h264_frame_gate(now_std()), H264Gate::Encode);
        b.h264.pending_idr = None;
        // The queue drains: frames are encoded again.
        b.forced_fifo_depth_for_test = None;
        b.pump_admission();
        assert_eq!(
            b.reliable_emitter.queued_h264_bytes(),
            0,
            "premise: drained"
        );
        assert_eq!(b.h264_frame_gate(now_std()), H264Gate::Encode);
        assert_eq!(b.h264.stats.frames_skipped_backpressure, skipped);
    }

    /// With no acknowledgement ever arriving, the probe cache and the tracker
    /// stay bounded: the cache to its newest frames, the tracker by its ring.
    #[tokio::test(start_paused = true)]
    async fn the_probe_cache_stays_bounded_when_nothing_is_acknowledged() {
        use crate::transport::reliable_emitter::h264::H264_CACHE_FRAMES;
        let mut b = bridge().await;
        for seq in 1..=(H264_CACHE_FRAMES as u32 + 40) {
            // A pending IDR runs the encoder past the hold and the backlog.
            b.h264.pending_idr = Some(IdrCause::ClientReset);
            send(&mut b, seq, seq == 1);
            b.h264.hold = None;
        }
        assert_eq!(b.reliable_emitter.h264.frames(), H264_CACHE_FRAMES);
        assert!(b.reliable_emitter.h264.get(1, 0).is_none());
        assert!(b
            .reliable_emitter
            .h264
            .get(H264_CACHE_FRAMES as u32 + 40, 0)
            .is_some());
        assert!(b.reliable_emitter.stats.h264_cache_dropped > 0);
    }

    /// The path MTU falls below the size queued fragments were cut for. They
    /// are retired, not offered to quinn (which would refuse each as
    /// `TooLarge` and, since a refusal re-queues, stop the drain at the first
    /// one for good): the drain empties, the ledger forgets them without a
    /// verdict, and an IDR for the new size is asked for under its own cause.
    #[tokio::test(start_paused = true)]
    async fn fragments_larger_than_the_connection_now_takes_are_retired_not_offered() {
        let mut b = bridge().await;
        // The gate holds everything back, as a standing FIFO does.
        b.forced_fifo_depth_for_test = Some(usize::MAX / 2);
        send(&mut b, 1, true);
        send(&mut b, 2, false);
        b.record_emitted_transmissions(now_std());
        let queued = b.reliable_emitter.queued_h264_bytes();
        let biggest = b
            .reliable_emitter
            .h264
            .get(1, 0)
            .map(|e| e.datagram.len())
            .expect("fragment 0 of the IDR is cached");
        // Premises: there is something to retire, and the ledger holds it.
        assert!(queued > 0 && biggest > 100);
        assert!(!b.transmission_ledger.is_empty());
        let offered = b.sent_log_for_test.len();

        b.forced_datagram_limit_for_test = Some(1);
        let stop = b.drain_reliable_emitter(now_std());

        assert!(
            matches!(stop, pacing::Stop::Empty),
            "an unsendable datagram must not stop the drain"
        );
        assert_eq!(b.sent_log_for_test.len(), offered, "nothing is offered");
        assert_eq!(b.reliable_emitter.queued_h264_bytes(), 0);
        assert!(b.h264.stats.oversize_h264 > 0);
        assert_eq!(b.h264.stats.send_too_large, 0);
        assert!(
            b.transmission_ledger.is_empty(),
            "forgotten, not left to be judged lost"
        );
        assert_eq!(b.h264.stats.tx_judged_lost, 0);
        assert_eq!(b.h264.pending_idr, Some(IdrCause::MtuShrink));
        assert!(b.reliable_emitter.h264.get(1, 0).is_none());

        // The IDR for the new size supersedes what is left and holds in its
        // own right; the retired hold is replaced, not waited on.
        send(&mut b, 3, true);
        assert_eq!(b.h264.stats.idr_by_cause[IdrCause::MtuShrink.index()], 1);
        assert_eq!(b.h264.hold.map(|h| h.frame_seq), Some(3));
        assert!(b.reliable_emitter.h264.get(2, 0).is_none());
    }

    /// The limit is derived from quinn's size as the drain really does it:
    /// `raw - framing`. A datagram exactly at the limit goes; one byte over is
    /// retired. (The forced limit of the tests above skips that subtraction.)
    #[tokio::test(start_paused = true)]
    async fn the_limit_is_quinns_size_less_the_framing() {
        let mut b = bridge().await;
        b.forced_fifo_depth_for_test = Some(usize::MAX / 2);
        send(&mut b, 1, true);
        b.record_emitted_transmissions(now_std());
        let biggest = (0u16..)
            .map_while(|i| b.reliable_emitter.h264.get(1, i).map(|e| e.datagram.len()))
            .max()
            .expect("the IDR is cached");
        assert!(biggest > 100 && b.reliable_emitter.queued_h264_bytes() > 0);
        let framing = b.live_framing_len();
        assert!(framing >= 1);

        // Exactly fits: nothing is retired.
        b.forced_raw_datagram_size_for_test = Some(biggest + framing);
        b.drain_reliable_emitter(now_std());
        assert_eq!(b.h264.stats.oversize_h264, 0, "a datagram at the limit");
        assert_eq!(b.h264.pending_idr, None);

        // One byte less: the biggest no longer fits.
        b.forced_raw_datagram_size_for_test = Some(biggest + framing - 1);
        b.drain_reliable_emitter(now_std());
        assert!(b.h264.stats.oversize_h264 > 0, "one byte over");
        assert_eq!(b.h264.pending_idr, Some(IdrCause::MtuShrink));
        assert_eq!(b.h264.stats.send_too_large, 0);
    }

    /// A fragment whose ledger record is gone (evicted by the cap) is still an
    /// H.264 fragment: it is recovered the same way, not counted as untracked,
    /// and no forgotten-count is made for a record that did not exist.
    #[tokio::test(start_paused = true)]
    async fn an_oversize_fragment_with_no_ledger_record_is_still_recovered() {
        let mut b = bridge().await;
        b.forced_fifo_depth_for_test = Some(usize::MAX / 2);
        send(&mut b, 1, true);
        b.record_emitted_transmissions(now_std());
        let wss: Vec<u32> = (0u16..)
            .map_while(|i| b.reliable_emitter.h264.get(1, i).map(|e| e.latest_ws))
            .collect();
        assert!(!wss.is_empty());
        for ws in &wss {
            assert!(b.transmission_ledger.forget_outstanding(*ws).is_some());
        }
        assert!(b.transmission_ledger.is_empty());

        b.forced_datagram_limit_for_test = Some(1);
        b.drain_reliable_emitter(now_std());

        // Every fragment is recognised by its header (frame XOR parity has no
        // content to recover and stays "untracked").
        assert_eq!(b.h264.stats.oversize_h264, wss.len() as u64);
        assert_eq!(b.h264.stats.tx_forgotten, 0);
        assert_eq!(b.h264.pending_idr, Some(IdrCause::MtuShrink));
        assert!(b.reliable_emitter.h264.get(1, 0).is_none());
    }

    /// A retired probe pad is not an H.264 fragment: it must not enter the
    /// forgotten count of the fragment identity.
    #[tokio::test(start_paused = true)]
    async fn a_retired_pad_is_not_counted_as_a_forgotten_fragment() {
        let mut b = bridge().await;
        b.forced_fifo_depth_for_test = Some(usize::MAX / 2);
        send(&mut b, 1, true);
        let template = b
            .reliable_emitter
            .h264
            .get(1, 0)
            .map(|e| e.datagram.clone())
            .expect("cached");
        let tag = crate::transport::reliable_emitter::cache::ProbeTag {
            id: 1,
            min_probes: 1,
            min_bytes: 1,
            bytes_sent_before: 0,
        };
        b.reliable_emitter
            .submit_h264_pad(&template, tag, now_std());
        assert!(!b.reliable_emitter.pads.is_empty(), "the pad is held");
        // Purge the fragments the usual way so only the pad remains queued
        // beyond them, then retire everything.
        b.forced_datagram_limit_for_test = Some(1);
        b.drain_reliable_emitter(now_std());
        assert_eq!(b.h264.stats.oversize_pad, 1);
        let frags = b.h264.stats.oversize_h264;
        assert!(frags > 0);
        assert_eq!(
            b.h264.stats.tx_forgotten, frags,
            "only fragments are counted"
        );
        assert!(b.reliable_emitter.pads.is_empty(), "the pad is released");
    }

    /// Datagrams that still fit are sent as before, whatever else is retired.
    #[tokio::test(start_paused = true)]
    async fn a_limit_that_every_datagram_meets_retires_nothing() {
        let mut b = bridge().await;
        b.forced_fifo_depth_for_test = Some(usize::MAX / 2);
        send(&mut b, 1, true);
        b.record_emitted_transmissions(now_std());
        let offered = b.sent_log_for_test.len();
        b.forced_datagram_limit_for_test = Some(usize::MAX);
        b.forced_fifo_depth_for_test = None;
        b.pump_admission();
        assert!(b.sent_log_for_test.len() > offered, "the fragments went");
        assert_eq!(b.h264.stats.oversize_h264, 0);
        assert_eq!(b.h264.pending_idr, None);
        assert_eq!(
            b.h264.stats.idr_requests_by_cause[IdrCause::MtuShrink.index()],
            0
        );
    }

    /// An IDR purges what the stale frames still have queued.
    #[tokio::test(start_paused = true)]
    async fn an_idr_purges_the_fragments_of_the_frames_it_supersedes() {
        let mut b = bridge().await;
        b.forced_fifo_depth_for_test = Some(usize::MAX / 2);
        send(&mut b, 1, false);
        send(&mut b, 2, false);
        let stale = b.reliable_emitter.queued_h264_bytes();
        assert!(stale > 0);
        send(&mut b, 3, true);
        let after = b.reliable_emitter.queued_h264_bytes();
        let idr_only = {
            let mut c = bridge().await;
            c.forced_fifo_depth_for_test = Some(usize::MAX / 2);
            send(&mut c, 3, true);
            c.reliable_emitter.queued_h264_bytes()
        };
        assert_eq!(after, idr_only, "only the IDR's own datagrams remain");
        assert!(b.h264.stats.queue_purged > 0);
        assert!(b.h264.stats.tx_forgotten > 0);
        // Nothing of the old frames is cached for the probe either.
        assert!(b.reliable_emitter.h264.get(1, 0).is_none());
        assert!(b.reliable_emitter.h264.get(3, 0).is_some());
    }

    /// Every new piece of state is per session: a new session starts clean.
    #[tokio::test(start_paused = true)]
    async fn a_session_reset_clears_every_piece_of_h264_state() {
        let mut b = bridge().await;
        b.forced_fifo_depth_for_test = Some(usize::MAX / 2);
        send(&mut b, 1, true);
        send(&mut b, 2, false);
        b.h264.last_loss_idr_at = Some(now_std());
        b.h264.deferred_loss_idr = true;
        b.h264.pending_idr = Some(IdrCause::Loss);
        // Premises: all of it is populated.
        assert!(b.h264.hold.is_some() && b.reliable_emitter.queued_h264_bytes() > 0);
        assert!(!b.reliable_emitter.h264.is_empty());
        assert_eq!(b.h264.tracker.is_complete(1), Some(false));
        let counted = b.h264.stats.frames_sent;

        b.fire_session_reset(ConnectionHandle(0));

        assert!(b.h264.hold.is_none(), "hold");
        assert!(b.h264.last_loss_idr_at.is_none(), "rate limiter");
        assert!(!b.h264.deferred_loss_idr, "deferred request");
        assert_eq!(b.h264.tracker.is_complete(1), None, "frame tracker");
        assert!(b.reliable_emitter.h264.is_empty(), "probe cache");
        assert_eq!(
            b.reliable_emitter.queued_h264_bytes(),
            0,
            "queued fragments"
        );
        assert_eq!(
            b.h264.pending_idr,
            Some(IdrCause::Session),
            "the new session asks for its own IDR, not the old one's cause"
        );
        assert_eq!(b.h264.stats.frames_sent, counted, "counters are cumulative");
    }

    /// The same state goes when the session's connection is forgotten (quinn
    /// reuses the handle for a later connection).
    #[tokio::test(start_paused = true)]
    async fn forgetting_the_live_session_clears_the_h264_state_too() {
        let mut b = bridge().await;
        let h = ConnectionHandle(4);
        b.last_hello_from = Some(h);
        b.forced_fifo_depth_for_test = Some(usize::MAX / 2);
        send(&mut b, 1, true);
        b.h264.last_loss_idr_at = Some(now_std());
        assert!(b.h264.hold.is_some() && !b.reliable_emitter.h264.is_empty());
        b.forget_session(h);
        assert!(b.h264.hold.is_none());
        assert!(b.h264.last_loss_idr_at.is_none());
        assert!(b.reliable_emitter.h264.is_empty());
        assert_eq!(b.reliable_emitter.queued_h264_bytes(), 0);
        assert_eq!(b.h264.tracker.is_complete(1), None);
    }

    /// IDR causes are attributed from what asked for them.
    #[tokio::test(start_paused = true)]
    async fn idr_causes_follow_what_asked_for_them() {
        let mut b = bridge().await;
        let now = now_std();
        // The first session: a fresh encoder starts with an IDR on its own.
        b.expect_h264_idr(IdrCause::Session);
        assert_eq!(b.note_h264_encoded(true, (64, 64)), Some(IdrCause::Session));
        // A P frame is no IDR.
        assert_eq!(b.note_h264_encoded(false, (64, 64)), None);
        // Nothing asked: the encoder's own GOP.
        assert_eq!(
            b.note_h264_encoded(true, (64, 64)),
            Some(IdrCause::GopPeriodic)
        );
        // The frame size changed: the encoder was reopened.
        assert_eq!(
            b.note_h264_encoded(true, (128, 64)),
            Some(IdrCause::Resolution)
        );
        // Re-entry from the tile codec, a client reset, and the first cause of
        // several coalesced requests.
        b.expect_h264_idr(IdrCause::Reentry);
        assert_eq!(
            b.note_h264_encoded(true, (128, 64)),
            Some(IdrCause::Reentry)
        );
        b.request_h264_idr(IdrCause::ClientReset, now);
        b.request_h264_idr(IdrCause::Loss, now);
        assert_eq!(b.h264.stats.idr_requests_coalesced, 1);
        assert_eq!(
            b.note_h264_encoded(true, (128, 64)),
            Some(IdrCause::ClientReset)
        );
        let by = b.h264.stats.idr_by_cause;
        assert_eq!(by[IdrCause::Session.index()], 1);
        assert_eq!(by[IdrCause::GopPeriodic.index()], 1);
        assert_eq!(by[IdrCause::Resolution.index()], 1);
        assert_eq!(by[IdrCause::Reentry.index()], 1);
        assert_eq!(by[IdrCause::ClientReset.index()], 1);
        assert_eq!(b.h264.stats.frames_idr, 5);
    }

    /// An acknowledged H.264 fragment is a BWE sample, in the critical tier,
    /// which is what lets the estimator learn in H.264 mode.
    #[tokio::test(start_paused = true)]
    async fn acknowledged_h264_fragments_are_bwe_samples() {
        let mut b = bridge().await;
        send(&mut b, 1, true);
        let sent = tracked_sent(&b, 0);
        depart(&mut b, &sent);
        assert!(b.bwe_samples_buffer.is_empty());
        let ws: Vec<u32> = sent.iter().map(|x| x.0).collect();
        ack(&mut b, &ws);
        assert_eq!(b.bwe_samples_buffer.len(), sent.len());
        assert!(b
            .bwe_samples_buffer
            .iter()
            .all(|s| s.tier == PassTier::Critical && s.size_bytes > 0));
        assert!(
            b.reliable_emitter.h264.is_empty(),
            "and the entries are released"
        );
        assert_eq!(b.h264.tracker.is_complete(1), Some(true));
    }

    /// A new session's first H.264 frame is an IDR named for the session, on
    /// the first HELLO (where a fresh encoder makes it unasked) and on a
    /// takeover (where the bridge asks for it), and the previous session's
    /// rate limit and deferred request are not carried into it.
    #[tokio::test(start_paused = true)]
    async fn a_hello_names_the_sessions_first_idr_and_a_takeover_asks_for_it() {
        use crate::transport::client_caps::{ClientCapabilities, HelloMsg};
        use crate::transport::webtransport::WebTransportServer;
        let (a, b) = (ConnectionHandle(0), ConnectionHandle(1));
        let mut bridge = bridge().await;
        for h in [a, b] {
            let mut wt = WebTransportServer::default();
            wt.test_set_connected(true);
            bridge.wt_sessions.insert(h, wt);
        }
        let hello = HelloMsg {
            caps: ClientCapabilities {
                indices_raw_enabled: false,
                supports_h264: true,
                h264_acked_ref: false,
            },
        };
        assert_eq!(bridge.h264.pending_idr, None);
        bridge.apply_hello(a, hello);
        assert_eq!(
            bridge.h264.pending_idr,
            Some(IdrCause::Session),
            "the first session's IDR is named"
        );
        // Its IDR is made; later the limiter and a deferred request are live.
        assert_eq!(
            bridge.note_h264_encoded(true, (64, 64)),
            Some(IdrCause::Session)
        );
        bridge.h264.last_loss_idr_at = Some(now_std());
        bridge.h264.deferred_loss_idr = true;
        // A takeover starts clean and asks for its own IDR.
        bridge.apply_hello(b, hello);
        assert_eq!(bridge.h264.pending_idr, Some(IdrCause::Session));
        assert!(bridge.h264.last_loss_idr_at.is_none());
        assert!(!bridge.h264.deferred_loss_idr);
    }

    /// The hold is a no-progress timeout: an IDR that takes longer than the
    /// ceiling to drain, but whose fragments keep being acknowledged, is not
    /// given up on; one that goes the ceiling with no acknowledgement is.
    #[tokio::test(start_paused = true)]
    async fn the_idr_hold_is_a_no_progress_timeout() {
        let big = |b: &mut IoBridge| {
            b.h264.pending_idr = Some(IdrCause::Session);
            b.send_h264_frame(
                H264Out {
                    payload: vec![1; 40_000],
                    is_keyframe: true,
                },
                &meta(),
                1,
                DG,
            );
        };
        // Steady progress for more than twice the ceiling.
        let mut b = bridge().await;
        big(&mut b);
        let sent = tracked_sent(&b, 0);
        depart(&mut b, &sent);
        assert!(sent.len() >= 31, "premise: a big IDR ({})", sent.len());
        let start = now_std();
        for (ws, _, _) in &sent {
            tokio::time::advance(Duration::from_millis(200)).await;
            assert_eq!(b.h264_frame_gate(now_std()), H264Gate::SkipHold);
            ack(&mut b, &[*ws]);
        }
        assert!(
            now_std().duration_since(start) > IDR_HOLD_MAX * 2,
            "premise: it took longer than twice the ceiling"
        );
        assert_eq!(b.h264.stats.hold_timeouts, 0, "progress never timed out");
        assert!(
            b.h264.hold.is_none(),
            "and the last acknowledgement ended it"
        );
        assert!(!b.h264.warned_no_acks, "acknowledgements were seen");

        // A few acknowledgements, then silence: the ceiling counts from the last.
        let mut b = bridge().await;
        big(&mut b);
        let sent = tracked_sent(&b, 0);
        depart(&mut b, &sent);
        for (ws, _, _) in &sent[..3] {
            tokio::time::advance(Duration::from_millis(200)).await;
            ack(&mut b, &[*ws]);
        }
        tokio::time::advance(IDR_HOLD_MAX - Duration::from_millis(1)).await;
        assert_eq!(b.h264_frame_gate(now_std()), H264Gate::SkipHold);
        tokio::time::advance(Duration::from_millis(2)).await;
        assert_eq!(b.h264_frame_gate(now_std()), H264Gate::Encode);
        assert_eq!(b.h264.stats.hold_timeouts, 1);
        assert!(!b.h264.warned_no_acks, "it did acknowledge some");
    }

    /// The encoder's own periodic IDR holds nothing, supersedes older frames,
    /// and ends a hold whose IDR it superseded (that IDR's acknowledgements
    /// will never complete).
    #[tokio::test(start_paused = true)]
    async fn a_periodic_idr_holds_no_p_frames() {
        let mut b = bridge().await;
        send(&mut b, 1, true); // the session's IDR: held
        assert!(b.h264.hold.is_some());
        b.forced_fifo_depth_for_test = Some(usize::MAX / 2);
        send(&mut b, 2, false);
        // Nothing asked for the next IDR: it is the encoder's own GOP.
        assert_eq!(b.h264.pending_idr, None);
        b.send_h264_frame(
            H264Out {
                payload: vec![9; PAYLOAD],
                is_keyframe: true,
            },
            &meta(),
            3,
            DG,
        );
        assert_eq!(
            b.h264.stats.idr_by_cause[IdrCause::GopPeriodic.index()],
            1,
            "premise: attributed to the GOP"
        );
        assert!(b.h264.hold.is_none(), "no hold, and the old one is over");
        assert_eq!(
            b.h264.last_idr_seq,
            Some(3),
            "but older frames are superseded"
        );
        assert!(b.reliable_emitter.h264.get(1, 0).is_none());
        assert_eq!(b.h264_frame_gate(now_std()), H264Gate::Encode);
        assert_eq!(b.h264.stats.p_holds, 1, "only the session's IDR held");
    }

    /// A rate-change reopen at the scheduled GOP IDR is a periodic IDR in
    /// every respect (no hold); one forced by a fall of more than 30 % is
    /// named for the rate and holds P frames like any IDR the client must get.
    /// The scripted encoder takes the same controls as the real backend, so
    /// the IDR it makes is the reopen's.
    #[tokio::test(start_paused = true)]
    async fn a_rate_drop_idr_holds_p_frames_and_a_gop_rate_change_does_not() {
        use super::super::h264_rate::{RateStep, ScriptedCbr};
        let cbr = InjectedCbr {
            fps: 30,
            idr_over_p: 5,
            gop: None,
        };
        let enc_frame = |b: &mut IoBridge| {
            let requested = b.h264.pending_idr.is_some();
            b.h264.scripted.as_mut().unwrap().next_frame(requested)
        };
        for urgent in [true, false] {
            let mut b = bridge().await;
            b.h264.scripted = Some(ScriptedCbr::new(cbr));
            b.h264_apply_rate_step(RateStep::Initial(2_400_000), 5_000_000, 30);
            // The session's IDR and a P frame.
            let (idr, _) = enc_frame(&mut b);
            assert!(idr);
            send(&mut b, 1, true);
            b.end_h264_hold(now_std());
            let (idr, p_len) = enc_frame(&mut b);
            assert!(!idr && p_len == 10_000, "premise: a P frame at 2.4 Mbit/s");
            send(&mut b, 2, false);
            let holds = b.h264.stats.p_holds;

            b.h264_apply_rate_step(
                RateStep::Reopen {
                    bps: 1_200_000,
                    urgent,
                },
                1_500_000,
                30,
            );
            assert_eq!(
                b.h264.pending_idr,
                urgent.then_some(IdrCause::RateDrop),
                "only the urgent reopen is named"
            );
            let (idr, len) = enc_frame(&mut b);
            assert!(idr, "a reopen is an IDR");
            assert_eq!(len, 25_000, "at the new rate: 5 x 5 000");
            let seq_sent = b.h264.stats.frames_sent as u32 + 1;
            b.send_h264_frame(
                H264Out {
                    payload: vec![0; PAYLOAD],
                    is_keyframe: true,
                },
                &meta(),
                seq_sent,
                DG,
            );
            let cause = if urgent {
                IdrCause::RateDrop
            } else {
                IdrCause::GopPeriodic
            };
            assert_eq!(
                b.h264.stats.idr_by_cause[cause.index()],
                1,
                "urgent={urgent}"
            );
            if urgent {
                assert_eq!(b.h264.stats.p_holds, holds + 1, "the rate drop IDR holds");
                assert_eq!(b.h264_frame_gate(now_std()), H264Gate::SkipHold);
                assert_eq!(b.h264.stats.rate_reopens_drop, 1);
            } else {
                assert_eq!(b.h264.stats.p_holds, holds, "a GOP IDR holds nothing");
                assert_eq!(b.h264_frame_gate(now_std()), H264Gate::Encode);
                assert_eq!(b.h264.stats.rate_reopens_gop, 1);
            }
            assert_eq!(b.h264.stats.rate_target_bps, 1_200_000);
        }
    }

    /// The GOP is a time since the last IDR of any cause, kept by the bridge:
    /// the encoder has no periodic IDR of its own to fire first.
    #[tokio::test(start_paused = true)]
    async fn the_gop_is_a_time_since_the_last_idr_of_any_cause() {
        let gop = Duration::from_secs(3);
        let mut b = bridge().await;
        // The first rate step: no IDR yet, so no GOP due, and nothing asked.
        b.h264_rate_step(now_std(), gop, 30);
        assert_eq!(b.h264.pending_idr, None);
        b.note_h264_encoded(true, (64, 64));
        assert!(b.h264.last_idr_at.is_some());
        tokio::time::advance(Duration::from_millis(2_900)).await;
        b.h264_rate_step(now_std(), gop, 30);
        assert_eq!(b.h264.pending_idr, None, "not yet: 2.9 s since the IDR");
        // A loss IDR in between restarts the clock.
        b.note_h264_encoded(true, (64, 64));
        tokio::time::advance(Duration::from_millis(2_900)).await;
        b.h264_rate_step(now_std(), gop, 30);
        assert_eq!(b.h264.pending_idr, None, "the loss IDR restarted the GOP");
        tokio::time::advance(Duration::from_millis(200)).await;
        b.h264_rate_step(now_std(), gop, 30);
        assert_eq!(
            b.h264.pending_idr,
            Some(IdrCause::GopPeriodic),
            "3.1 s since the last IDR: the bridge asks for the scheduled one"
        );
        b.reset_h264_session();
        assert!(b.h264.last_idr_at.is_none());
    }

    /// A probe window is claimed, and fragments tagged, for an IDR burst and
    /// not for a P frame.
    #[tokio::test(start_paused = true)]
    async fn only_an_idr_burst_is_probe_material() {
        let mut b = bridge().await;
        let now = now_std();
        b.active_probe = Some(ActiveProbe {
            id: 7,
            target_rate_bps: 8_000_000,
            ends_at: now + Duration::from_secs(10),
            min_probes: 5,
            min_bytes: 1_000,
            bytes_sent: 0,
            packets_sent: 0,
            opened_at: now,
            starved: false,
        });
        send(&mut b, 1, true);
        let after_idr = b.active_probe.as_ref().unwrap().packets_sent;
        assert!(after_idr > 0, "premise: the IDR's fragments were tagged");
        assert!(b.reliable_emitter.h264.get(1, 0).unwrap().probe.is_some());
        b.h264.hold = None;
        send(&mut b, 2, false);
        assert_eq!(b.active_probe.as_ref().unwrap().packets_sent, after_idr);
        assert!(b.reliable_emitter.h264.get(2, 0).unwrap().probe.is_none());
    }

    /// While the rate limit holds a loss IDR back, the P frames that would be
    /// encoded chain from the suspect frame and are not encoded.
    #[tokio::test(start_paused = true)]
    async fn p_frames_are_not_encoded_while_a_loss_idr_is_deferred() {
        let mut b = bridge().await;
        b.h264.deferred_loss_idr = true;
        b.h264.last_loss_idr_at = Some(now_std());
        assert_eq!(b.h264_frame_gate(now_std()), H264Gate::SkipHold);
        assert_eq!(b.h264.stats.frames_skipped_hold, 1);
        tokio::time::advance(LOSS_IDR_MIN_INTERVAL).await;
        assert_eq!(
            b.h264_frame_gate(now_std()),
            H264Gate::Encode,
            "due: the IDR is requested and the frame is it"
        );
        assert_eq!(b.h264.pending_idr, Some(IdrCause::Loss));
    }

    /// A loss IDR left pending across a tile stretch is credited to the
    /// re-entry that finally makes it; another pending cause is kept.
    #[tokio::test(start_paused = true)]
    async fn a_stale_loss_request_is_credited_to_the_reentry_idr() {
        let mut b = bridge().await;
        b.h264.pending_idr = Some(IdrCause::Loss);
        b.name_reentry_idr();
        assert_eq!(b.h264.pending_idr, Some(IdrCause::Reentry));
        b.h264.pending_idr = Some(IdrCause::ClientReset);
        b.name_reentry_idr();
        assert_eq!(b.h264.pending_idr, Some(IdrCause::ClientReset));
        b.h264.pending_idr = None;
        b.name_reentry_idr();
        assert_eq!(b.h264.pending_idr, Some(IdrCause::Reentry));
    }

    // ---- h264_stalled (A5) ----

    /// Fully acknowledge frame `seq` through the real acknowledgement path.
    fn complete(b: &mut IoBridge, seq: u32) {
        b.pump_admission();
        let sent = tracked_sent(b, 0);
        depart(b, &sent);
        let ws: Vec<u32> = sent.iter().filter(|x| x.1 == seq).map(|x| x.0).collect();
        assert!(!ws.is_empty(), "premise: frame {seq} has sent fragments");
        ack(b, &ws);
        assert_eq!(b.h264.tracker.is_complete(seq), Some(true));
    }

    fn ctx_flag(b: &IoBridge) -> bool {
        b.classifier.adaptation_context().h264_stalled
    }

    /// (a): unresolved frames beyond `max(32, ceil(2 srtt / interval) + 8)`.
    #[tokio::test(start_paused = true)]
    async fn too_many_unresolved_frames_raise_the_stall_flag() {
        let mut b = bridge().await;
        let t0 = now_std();
        // The bound is 32 at a short round trip (literal: see the classifier
        // tests): 32 frames in flight are fine, a 33rd is not.
        send(&mut b, 1, true);
        for seq in 2..=32 {
            send(&mut b, seq, false);
        }
        assert!(
            !b.update_h264_stall(t0),
            "the bound itself is not over the bound"
        );
        assert!(!ctx_flag(&b));
        send(&mut b, 33, false);
        assert!(b.update_h264_stall(t0));
        assert!(ctx_flag(&b), "the flag reaches the classifier's context");
        assert_eq!(b.h264.stats.stalls_inflight, 1);
        assert_eq!(b.h264.stats.stalls_no_progress, 0);
    }

    /// The bound grows with the round trip: a long path has more in flight.
    #[tokio::test(start_paused = true)]
    async fn the_in_flight_bound_follows_the_round_trip() {
        assert_eq!(stall_inflight_bound(0.0), 32);
        assert_eq!(stall_inflight_bound(20_000.0), 32);
        // 1 s: 2 s of frames at 30 Hz = ceil(60.0006) = 61, plus 8.
        assert_eq!(stall_inflight_bound(1_000_000.0), 69);
        let mut b = bridge().await;
        b.adaptation_context.smoothed_rtt_us = 1_000_000.0;
        let t0 = now_std();
        send(&mut b, 1, true);
        for seq in 2..=37 {
            send(&mut b, seq, false);
        }
        assert!(
            !b.update_h264_stall(t0),
            "37 frames in flight on a 1 s path is normal"
        );
    }

    /// (b): frames in flight and none fully acknowledged for 500 ms.
    #[tokio::test(start_paused = true)]
    async fn no_frame_progress_for_500ms_raises_the_stall_flag() {
        let mut b = bridge().await;
        let t0 = now_std();
        // Nothing in flight: no stall, however long.
        assert!(!b.update_h264_stall(t0 + Duration::from_secs(60)));
        send(&mut b, 1, true);
        assert!(!b.update_h264_stall(t0 + Duration::from_millis(499)));
        assert!(b.update_h264_stall(t0 + Duration::from_millis(500)));
        assert_eq!(b.h264.stats.stalls_no_progress, 1);
        assert_eq!(b.h264.stats.stalls_inflight, 0);
        assert!(ctx_flag(&b));
    }

    /// A stream that keeps completing frames never stalls, however long it
    /// runs, and the flag clears when progress resumes.
    #[tokio::test(start_paused = true)]
    async fn completing_frames_is_progress() {
        let mut b = bridge().await;
        send(&mut b, 1, true);
        complete(&mut b, 1);
        let mut seq = 2;
        for _ in 0..20 {
            tokio::time::advance(Duration::from_millis(300)).await;
            send(&mut b, seq, false);
            assert!(!b.update_h264_stall(now_std()), "stalled at frame {seq}");
            complete(&mut b, seq);
            seq += 1;
        }
        // Now the acknowledgements stop ...
        send(&mut b, seq, false);
        tokio::time::advance(Duration::from_millis(500)).await;
        assert!(b.update_h264_stall(now_std()));
        assert!(ctx_flag(&b));
        // ... and resume: the stalled frame completes, the flag drops.
        complete(&mut b, seq);
        assert!(!b.update_h264_stall(now_std()));
        assert!(!ctx_flag(&b));
    }

    /// One frame stuck (its verdict not in yet) while later frames complete is
    /// a path that is alive: any frame completing is progress.
    #[tokio::test(start_paused = true)]
    async fn a_later_frame_completing_is_progress_while_an_earlier_one_waits() {
        let mut b = bridge().await;
        send(&mut b, 1, true);
        complete(&mut b, 1);
        send(&mut b, 2, false); // never acknowledged
        for seq in 3..23 {
            tokio::time::advance(Duration::from_millis(100)).await;
            send(&mut b, seq, false);
            complete(&mut b, seq);
            assert!(!b.update_h264_stall(now_std()), "stalled at frame {seq}");
        }
        // The stuck frame is old by now; only the lack of progress would
        // raise the flag, and there is progress.
        assert_eq!(b.h264.stats.stalls_no_progress, 0);
    }

    /// Frames a loss verdict doomed (the suspect one and every later one) are
    /// not "in flight": an IDR is on its way and nothing will resolve them.
    /// A fragment verdict must not read as a stall.
    #[tokio::test(start_paused = true)]
    async fn frames_a_verdict_doomed_are_not_in_flight() {
        let mut b = bridge().await;
        send(&mut b, 1, true);
        complete(&mut b, 1);
        send(&mut b, 2, false);
        send(&mut b, 3, false);
        b.note_h264_verdict(2, 0, now_std());
        assert_eq!(
            b.h264.stats.frames_suspect, 1,
            "premise: frame 2 is suspect"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(
            !b.update_h264_stall(now_std()),
            "frames 2 and 3 chain from a lost fragment and are not awaited"
        );
        // A frame sent before the verdict's IDR is superseded by it too.
        send(&mut b, 4, false);
        send(&mut b, 5, true);
        complete(&mut b, 5);
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(!b.update_h264_stall(now_std()));
    }

    /// (c): three honoured client reset requests within 10 s.
    #[tokio::test(start_paused = true)]
    async fn three_honoured_reset_requests_in_ten_seconds_raise_the_stall_flag() {
        use ghostframe_protocol::feedback_stream::{ResetReason, ResetRequest};
        let mut b = bridge().await;
        let t0 = now_std();
        let req = ResetRequest {
            reason: ResetReason::DecoderFailure,
            frame_seq: 7,
        };
        let ms = Duration::from_millis;
        // The native client paces its requests 2.2 s apart, just outside the
        // server's own 2 s limit: every one is honoured, none is limited.
        b.handle_reset_request(req, t0);
        b.handle_reset_request(req, t0 + ms(2_200));
        assert_eq!(b.reset_requests_honoured, 2, "premise: both honoured");
        assert!(!b.update_h264_stall(t0 + ms(2_200)), "two do not stall");
        b.handle_reset_request(req, t0 + ms(4_400));
        assert_eq!(b.reset_requests_honoured, 3, "premise: the third honoured");
        let t = t0 + ms(4_400);
        assert!(b.update_h264_stall(t));
        assert_eq!(b.h264.stats.stalls_reset, 1);
        assert!(b.update_h264_stall(t + ms(499)));
        assert!(!b.update_h264_stall(t + ms(500)));
        assert!(!ctx_flag(&b));
    }

    /// Two requests with jitter, requests spread over more than 10 s, and
    /// requests the rate limit turned away (which made no IDR) do not stall.
    #[tokio::test(start_paused = true)]
    async fn sparse_or_rate_limited_reset_requests_do_not_stall() {
        use ghostframe_protocol::feedback_stream::{ResetReason, ResetRequest};
        let req = ResetRequest {
            reason: ResetReason::DecoderFailure,
            frame_seq: 7,
        };
        let ms = Duration::from_millis;
        // Two, 2.2 s apart, and again 2.35 s apart.
        for gap in [2_200, 2_350] {
            let mut b = bridge().await;
            let t0 = now_std();
            b.handle_reset_request(req, t0);
            b.handle_reset_request(req, t0 + ms(gap));
            assert!(!b.update_h264_stall(t0 + ms(gap)), "gap {gap}");
        }
        // One every 5.1 s: the third is 10.2 s after the first.
        let mut b = bridge().await;
        let t0 = now_std();
        for i in 0..6u64 {
            let t = t0 + ms(i * 5_100);
            b.handle_reset_request(req, t);
            assert!(!b.update_h264_stall(t), "request {i}");
        }
        assert_eq!(b.reset_requests_honoured, 6, "premise: all honoured");
        // A burst inside the 2 s limit makes one IDR, not three.
        let mut b = bridge().await;
        let t0 = now_std();
        for i in 0..5u64 {
            b.handle_reset_request(req, t0 + ms(i * 100));
        }
        assert_eq!(b.reset_requests_honoured, 1, "premise: one honoured");
        assert!(!b.update_h264_stall(t0 + ms(400)));
    }

    /// One large IDR draining over a slow link, a fragment at a time, is
    /// progress: it must not read as a stall, however long the drain takes.
    /// (A 40 KB IDR of 35 fragments, one acknowledged every 25 ms: the whole
    /// frame is not complete for 875 ms.)
    #[tokio::test(start_paused = true)]
    async fn a_large_idr_draining_a_fragment_at_a_time_is_not_a_stall() {
        let mut b = bridge().await;
        b.h264.pending_idr = Some(IdrCause::Session);
        b.send_h264_frame(
            H264Out {
                payload: vec![0xA5; 40_000],
                is_keyframe: true,
            },
            &meta(),
            1,
            DG,
        );
        b.pump_admission();
        let sent = tracked_sent(&b, 0);
        assert!(
            sent.len() >= 33,
            "premise: a many-fragment IDR: {}",
            sent.len()
        );
        depart(&mut b, &sent);
        let mut stalled_at = None;
        for (i, (ws, _, _)) in sent.iter().enumerate() {
            tokio::time::advance(Duration::from_millis(25)).await;
            ack(&mut b, &[*ws]);
            if b.update_h264_stall(now_std()) && stalled_at.is_none() {
                stalled_at = Some(i);
            }
        }
        assert!(
            b.h264.tracker.is_complete(1) == Some(true),
            "premise: the frame completed"
        );
        assert!(
            sent.len() as u64 * 25 > 500,
            "premise: the drain outlasts the 500 ms bound"
        );
        assert_eq!(stalled_at, None, "stalled while the IDR was draining");
        assert_eq!(b.h264.stats.stalls_no_progress, 0);
        // And a real stall is still found: the next frame's acks stop.
        send(&mut b, 2, false);
        tokio::time::advance(Duration::from_millis(500)).await;
        assert!(b.update_h264_stall(now_std()));
    }

    /// Leaving H.264 mode abandons what was in flight, so the flag clears and
    /// the classifier's re-entry time can start.
    #[tokio::test(start_paused = true)]
    async fn leaving_h264_mode_clears_the_stall_flag() {
        use crate::tile::FrameMode;
        let mut b = bridge().await;
        let t0 = now_std();
        send(&mut b, 1, true);
        for seq in 2..=34 {
            send(&mut b, seq, false);
        }
        assert!(b.update_h264_stall(t0));
        // The classifier, handed that context, takes the session to tile ...
        let now_us = 1_000_000;
        let mode = b
            .classifier
            .decide_frame_mode_at(now_us, &[], FrameMode::H264);
        assert_eq!(mode, FrameMode::TileCodec);
        // ... where the old frames no longer count.
        b.abandon_h264_inflight(50, now_std());
        assert!(!b.update_h264_stall(t0));
    }

    /// The wiring, through the real frame decision: a stalled stream in H.264
    /// mode leaves it on the next decision, the frames in flight stop counting
    /// as the mode changes, and H.264 stays out for the classifier's
    /// re-entry time even though the content would put it back.
    #[tokio::test(start_paused = true)]
    async fn the_frame_decision_leaves_h264_when_stalled_and_stays_out() {
        use crate::tile::FrameMode;
        let mut b = bridge().await;
        b.frame_mode = FrameMode::H264;
        // Nothing drains: the fragments sit in the emission queue and the
        // probe cache, and the IDR's hold is open.
        b.forced_fifo_depth_for_test = Some(usize::MAX / 2);
        send(&mut b, 1, true);
        for seq in 2..=40 {
            send(&mut b, seq, false);
        }
        b.record_emitted_transmissions(now_std());
        assert!(
            b.reliable_emitter.queued_h264_bytes() > 0,
            "premise: queued"
        );
        assert!(!b.reliable_emitter.h264.is_empty(), "premise: cached");
        assert!(b.h264.hold.is_some(), "premise: P hold open");
        let analysed = crate::transport::io_bridge::frame_backend::Analysed::default();
        let (_, prev, new) = b.classify_dirty_tiles(&analysed, &meta(), 4, 4, 100);
        assert_eq!((prev, new), (FrameMode::H264, FrameMode::TileCodec));
        assert_eq!(b.frame_mode, FrameMode::TileCodec);
        // The abandoned H.264 does not compete with the tile repaint.
        assert_eq!(b.reliable_emitter.queued_h264_bytes(), 0, "queue purged");
        assert!(b.reliable_emitter.h264.is_empty(), "probe cache dropped");
        assert!(b.h264.hold.is_none(), "P hold ended");
        assert_eq!(b.h264.stats.stalls_inflight, 1);
        // The next decision: the abandoned frames no longer count, so the
        // flag is down; the classifier still holds H264 off.
        tokio::time::advance(Duration::from_millis(40)).await;
        let (_, _, new) = b.classify_dirty_tiles(&analysed, &meta(), 4, 4, 101);
        assert!(!b.h264.stalled, "the flag must clear once H.264 is left");
        assert_eq!(new, FrameMode::TileCodec);
        // The link is starved (the guard would put H264 back after its
        // 500 ms hold): evaluated every 100 ms, the session stays in tile
        // until 2 s after the stall, and only then comes back.
        b.adaptation_context.bwe_estimate_bps = 100_000;
        b.classifier.set_adaptation_context(b.adaptation_context);
        let mut seq = 102;
        for step in 1..=18 {
            tokio::time::advance(Duration::from_millis(100)).await;
            let (_, _, new) = b.classify_dirty_tiles(&analysed, &meta(), 4, 4, seq);
            seq += 1;
            assert_eq!(
                new,
                FrameMode::TileCodec,
                "re-entered {} ms after the stall",
                40 + step * 100
            );
        }
        for _ in 0..20 {
            tokio::time::advance(Duration::from_millis(100)).await;
            b.classify_dirty_tiles(&analysed, &meta(), 4, 4, seq);
            seq += 1;
        }
        assert_eq!(
            b.frame_mode,
            FrameMode::H264,
            "the guard never brought H264 back after the holdoff"
        );
    }

    /// The headroom guard's input is the estimator's own estimate, published
    /// with the path stats (not the probe-inclusive model rate), and the test
    /// override of the headroom replaces both views.
    #[tokio::test(start_paused = true)]
    async fn the_classifier_is_given_the_bandwidth_estimate() {
        let mut b = bridge().await;
        b.apply_path_stats_snapshot(20_000.0);
        let est = b.bwe.snapshot().bitrate_bps;
        assert!(est > 0, "premise: the estimator has an estimate");
        assert_eq!(b.adaptation_context.bwe_estimate_bps, est);
        assert_eq!(b.classifier.adaptation_context().bwe_estimate_bps, est);

        b.test_force_bytes_per_us = Some(0.1);
        b.apply_path_stats_snapshot(20_000.0);
        assert_eq!(
            b.classifier.adaptation_context().bwe_estimate_bps,
            800_000,
            "0.1 B/us is 800 kbit/s"
        );
    }

    /// The time the flag was up is accounted, in whole milliseconds between
    /// evaluations that saw it up.
    #[tokio::test(start_paused = true)]
    async fn stalled_time_is_accounted() {
        let mut b = bridge().await;
        let t0 = now_std();
        send(&mut b, 1, true);
        assert!(b.update_h264_stall(t0 + Duration::from_millis(600)));
        assert!(b.update_h264_stall(t0 + Duration::from_millis(900)));
        assert!(b.update_h264_stall(t0 + Duration::from_millis(1_400)));
        assert_eq!(b.h264.stats.h264_stalled_ms, 800);
        b.abandon_h264_inflight(2, now_std());
        assert!(!b.update_h264_stall(t0 + Duration::from_millis(1_500)));
        assert_eq!(
            b.h264.stats.h264_stalled_ms, 900,
            "the last interval counts"
        );
        assert!(!b.update_h264_stall(t0 + Duration::from_millis(9_000)));
        assert_eq!(b.h264.stats.h264_stalled_ms, 900);
        assert_eq!(b.h264.stats.stalls_no_progress, 1, "one rising edge");
    }
}
