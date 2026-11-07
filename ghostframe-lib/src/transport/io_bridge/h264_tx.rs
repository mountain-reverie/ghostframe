//! H.264 frames as members of the transport: the per-frame tracker, the
//! stall flag the classifier reads, the bridge's IDR accounting and the
//! encoder's backpressure. What recovers from loss is the `RefManager`
//! (`h264_refs`), not an IDR.
//!
//! Every fragment is a ledger transmission
//! (`ReliableTileEmitter::submit_h264_fragment`), so it is acknowledged or
//! judged exactly once, charged to the admission bucket, and fed to the
//! estimators.
//!
//! * **IDR fragments are repaired, P frames are held.** A verdict on an IDR
//!   fragment resends it (the emitter does that); the manager plans no P frame
//!   until every fragment of the newest IDR is acknowledged, because the client
//!   evicts what is older than `latest - 2` and P frames behind an unfinished
//!   IDR would push it out.
//! * **Frame skip.** The encoder is not run while the emission queue holds more
//!   H.264 bytes than a frame period of the admission rate (or two gate
//!   depths): a skipped capture loses nothing, every frame being a whole
//!   screen, and queueing stale fragments behind it would only delay the
//!   fresh frame. Fragments of a frame an IDR supersedes are purged from the
//!   queue.

use super::*;
use crate::encoder::h264::tracker::{FragAck, TRACKER_RING};
use crate::transport::reliable_emitter::Content;
use crate::transport::transmission_ledger::Transmission;

/// Floor of the in-flight bound for `h264_stalled` (a): unresolved frames
/// beyond `max(this, ceil(2 * srtt / frame interval) + 8)` mean the path has
/// stopped carrying the stream.
pub(super) const STALL_INFLIGHT_FLOOR: u32 = 32;
/// `h264_stalled` (b): a frame was sent after the client last made progress
/// (the decode clock: a frame newly decoded, an IDR fragment acknowledged, a P
/// fragment acknowledged while the client is not refusing) and this long has
/// passed since. (A large IDR on a slow link takes seconds to drain; a
/// fragment at a time is progress, a whole frame is not the right unit.)
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

/// Why an IDR was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdrCause {
    /// A new live session (first frame, or after a takeover or reconnect).
    Session,
    /// The client asked (feedback stream 0x09).
    ClientReset,
    /// The frame size changed and the encoder was reopened.
    Resolution,
    /// The classifier returned from the tile codec to H.264 with no usable
    /// anchor.
    Reentry,
    /// The path MTU fell below the size fragments already built were cut for
    /// (quinn's black-hole detector, after discovery had raised it), so
    /// stored H.264 fragments can no longer be sent. A transport event of the
    /// same nature as a session start: the fragments are gone, and an IDR
    /// sized for the connection as it is now is the only way to a picture.
    MtuShrink,
    /// The backend made an IDR the manager did not plan (observed mode), or
    /// the stream moved to another backend after a fault limit.
    BackendFallback,
    /// The backend failed or made a stream that is not the plan.
    EncoderFault,
}

impl IdrCause {
    pub(crate) const COUNT: usize = 7;
    pub const ALL: [IdrCause; Self::COUNT] = [
        IdrCause::Session,
        IdrCause::ClientReset,
        IdrCause::Resolution,
        IdrCause::Reentry,
        IdrCause::MtuShrink,
        IdrCause::BackendFallback,
        IdrCause::EncoderFault,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn name(self) -> &'static str {
        match self {
            IdrCause::Session => "session",
            IdrCause::ClientReset => "client_reset",
            IdrCause::Resolution => "resolution",
            IdrCause::Reentry => "reentry",
            IdrCause::MtuShrink => "mtu_shrink",
            IdrCause::BackendFallback => "backend_fallback",
            IdrCause::EncoderFault => "encoder_fault",
        }
    }
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
    /// IDRs made, by cause (`IdrCause::index`).
    pub idr_by_cause: [u64; IdrCause::COUNT],
    /// Frames not encoded, by why.
    pub frames_skipped_hold: u64,
    pub frames_skipped_backpressure: u64,
    /// Datagrams quinn refused as larger than the connection's current
    /// maximum (the path MTU fell after they were built).
    pub send_too_large: u64,
    /// Queued datagrams retired because they no longer fit the connection's
    /// datagram size (it fell after they were built), by what they carried:
    /// a tile pass, an H.264 fragment, probe padding, or something the ledger
    /// does not track (tile XOR parity). None of them reached quinn.
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

    pub(super) fn on_frame_sent(&mut self, frame_seq: u32, is_idr: bool, frags_total: u16) {
        self.ring[Self::slot(frame_seq)] = Some(TrackedFrame {
            frame_seq,
            is_idr,
            frags_total,
            acked: vec![0; (frags_total as usize).div_ceil(64)],
            acked_count: 0,
            suspect: false,
            resolved: false,
        });
    }

    /// The frames the stall flag counts as in flight: sent, not fully
    /// acknowledged, not judged lost, at or after `floor` (older ones are
    /// superseded by an IDR or abandoned with H.264 mode). A scan of the ring,
    /// once per frame decision, not per datagram.
    pub(super) fn unresolved(&self, floor: u32) -> u32 {
        self.ring
            .iter()
            .flatten()
            .filter(|f| f.frame_seq >= floor && !f.suspect && !f.resolved && !f.complete())
            .count() as u32
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

/// The bridge's H.264 transport state. Everything here belongs to one live
/// session and is cleared by [`H264Tx::reset_session`]; the counters survive.
#[derive(Default)]
pub(super) struct H264Tx {
    pub(super) tracker: FrameTracker,
    /// `frame_seq` of the newest IDR sent: every older frame is superseded.
    pub(super) last_idr_seq: Option<u32>,
    pub(super) stats: H264Stats,
    /// Frames older than this are not in flight as far as the stall flag is
    /// concerned (set when the session leaves H.264 mode).
    pub(super) ignore_before: u32,
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
    /// The frame rate the encoder is opened for.
    pub(super) fps: u32,
    /// A recently acknowledged P-frame fragment's datagram: the padding of a
    /// probe window (see `h264_rate`).
    pub(super) pad_template: Option<bytes::Bytes>,
    /// The application's own probing: cadence and chain state.
    pub(super) app_probe: super::h264_rate::AppProbe,
    /// When the encoder's rate was last cut by a reopen (padding keeps quiet).
    pub(super) last_cut_at: Option<std::time::Instant>,
    /// Acked-reference recovery (Phase B, `h264_refs`): the live session's
    /// manager and the loop's counters.
    /// Which backends the planned path may open, and the openh264 library
    /// to load (`GHOSTFRAME_H264_BACKEND`, `GHOSTFRAME_H264_OPENH264_LIB`).
    pub(super) backend: crate::config::H264BackendChoice,
    pub(super) openh264_lib: Option<std::sync::Arc<std::path::Path>>,
    /// Whether any encoder can code H.264 (see `H264Availability`). Not part
    /// of a session: [`H264Tx::reset_session`] leaves it alone.
    pub(super) availability: super::h264_refs::H264Availability,
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
    /// The PCM encoder the acked-reference path runs inline in a scene, in
    /// place of the encode worker.
    #[cfg(feature = "browserless-harness")]
    pub(super) pcm_backend: Option<crate::encoder::h264::harness::PcmBackend>,
    /// Harness: the openh264 source of a `PcmMode::OpenH264` scene.
    #[cfg(feature = "browserless-harness")]
    pub(super) oh_source: Option<crate::encoder::h264::harness::Openh264Source>,
    /// The log the scene reads (design T3).
    #[cfg(feature = "browserless-harness")]
    pub(super) pcm_log: crate::encoder::h264_harness::PcmLog,
}

impl H264Tx {
    /// Forget the session's state (tracker, stall bookkeeping, rate policy).
    /// Quinn reuses connection handles and the next session starts with an
    /// IDR, so none of this may carry over.
    pub(super) fn reset_session(&mut self) {
        self.tracker.clear();
        self.last_idr_seq = None;
        self.ignore_before = 0;
        self.last_ack_progress = None;
        self.honoured_resets.clear();
        self.reset_storm_until = None;
        self.stalled = false;
        self.stall_eval_at = None;
        self.rate.reset_session();
        self.pad_template = None;
        // The next session builds its own manager (and may be a client that
        // does not send the v2 feedback at all).
        self.refs = None;
        self.decode_clock = None;
        self.last_sent_at = None;
        self.refused_since_decode = false;
        self.newest_decoded = None;
    }
}

impl IoBridge {
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

    /// An H.264 frame is out of the encoder: count it. The plan names the
    /// cause of an IDR; one nobody planned is a session's.
    pub(super) fn note_h264_encoded_as(&mut self, is_keyframe: bool, planned: Option<IdrCause>) {
        if !is_keyframe {
            return;
        }
        let cause = planned.unwrap_or(IdrCause::Session);
        self.h264.stats.idr_by_cause[cause.index()] += 1;
        self.h264.stats.frames_idr += 1;
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

    /// The frame is fully acknowledged.
    fn on_h264_frame_acked(&mut self, frame_seq: u32, now: std::time::Instant) {
        self.h264.stats.frames_acked += 1;
        self.note_raised_acked(frame_seq, now);
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
            self.h264.last_ack_progress = Some(now);
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
            // An IDR draining on a slow link is progress.
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
    /// loss path already. A P-frame fragment makes its frame suspect and the
    /// manager plans the recovery.
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
        // The session recovers from its anchor: the verdict makes the frame
        // suspect (the client's decoded bit overrides it).
        self.note_raised_lost(frame_seq, now);
        if self.h264_refs_live() {
            self.h264_refs_verdict(frame_seq, now);
        }
    }

    /// Evaluate the `h264_stalled` flag at `now`, publish it to the
    /// classifier's context, and return it. Called once per frame decision,
    /// just before the classifier runs.
    ///
    /// Raised by any of: (a) unresolved frames beyond
    /// [`stall_inflight_bound`]; (b) a frame sent after the last progress, and
    /// [`STALL_NO_PROGRESS`] since (progress is the decode clock:
    /// a frame newly decoded per the 0x0A frame ack, an IDR fragment
    /// acknowledged, or a P fragment acknowledged unless the client has refused
    /// a frame since its last decode); (c) [`STALL_RESET_COUNT`] honoured client reset
    /// requests within [`STALL_RESET_WINDOW`] (see
    /// [`Self::note_h264_reset_honoured`]).
    pub(super) fn update_h264_stall(&mut self, now: std::time::Instant) -> bool {
        let floor = self
            .h264
            .ignore_before
            .max(self.h264.last_idr_seq.unwrap_or(0));
        let in_flight = self.h264.tracker.unresolved(floor);
        let bound = stall_inflight_bound(self.adaptation_context.smoothed_rtt_us);
        let over_bound = in_flight > bound;
        // Frames were sent since the client last decoded one, and it has not
        // for 500 ms: a client that refuses everything is stalled however well
        // the transport delivers.
        let no_progress = match (self.h264.decode_clock, self.h264.last_sent_at) {
            (Some(d), Some(sent)) => {
                sent > d && now.saturating_duration_since(d) >= STALL_NO_PROGRESS
            }
            _ => false,
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
        // An IDR that was still being delivered is gone with the purge: the
        // manager sends it again at the return, not after its timeout.
        if self.h264_refs_live() {
            if let Some(st) = self.h264.refs.as_mut() {
                st.mgr.idr_abandoned();
            }
        }
        // Only frames sent after this count for the stall flag (and see
        // `ignore_before`): the last ones were purged or will never be
        // answered, and nothing is sent while the tile codec has the screen.
        self.h264.last_sent_at = None;
        self.h264.decode_clock = None;
        self.h264.refused_since_decode = false;
        self.h264.newest_decoded = None;
        // The rate side of H.264 lets go here too, in one place: the padding in
        // flight is forgotten and the application's probe cadence starts over
        // (`h264_reset_probing`), and the rate policy's estimate restarts (the
        // encoder keeps its rate).
        self.h264_reset_probing();
        self.h264.rate.reset_session();
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

    /// The browserless harness's scripted H.264 frame (see [`InjectedH264`]).
    /// Nothing is sent without a live session, as the tile injection path.
    #[cfg(feature = "browserless-harness")]
    pub(super) fn apply_injected_h264(&mut self, seq: u32, timestamp_us: u32, h: InjectedH264) {
        let Some(max_dg_size) = self.compute_max_datagram_size() else {
            return;
        };
        let now = now_std();
        self.note_injected_h264_mode();
        // The classifier is not in this scripted path; the flag still is.
        self.update_h264_stall(now);
        self.emit_frame_dimensions(seq, timestamp_us, h.width, h.height);
        self.apply_injected_pcm_refs(seq, timestamp_us, h, max_dg_size, now);
    }

    /// Without the harness there is nothing to inject: the encoder behind a
    /// scripted frame is the harness's.
    #[cfg(not(feature = "browserless-harness"))]
    pub(super) fn apply_injected_h264(&mut self, seq: u32, timestamp_us: u32, h: InjectedH264) {
        let _ = (seq, timestamp_us, h);
    }

    /// The injected stream is in H.264 mode now. Coming back from a stretch of
    /// tile frames the next frame is a recovery P from the anchor
    /// (`h264_refs_reentry`); the first H.264 frame of the scene is not a
    /// return.
    #[cfg(feature = "browserless-harness")]
    fn note_injected_h264_mode(&mut self) {
        let prev = std::mem::replace(&mut self.frame_mode, crate::tile::FrameMode::H264);
        if prev == crate::tile::FrameMode::TileCodec && self.h264.stats.frames_sent > 0 {
            self.h264_refs_reentry(now_std());
        }
    }

    /// The log of the PCM source: what each frame was meant to show, and the
    /// `RefManager`'s counters. Read it after the scene.
    #[cfg(feature = "browserless-harness")]
    pub fn h264_pcm_log_handle(&self) -> crate::encoder::h264_harness::PcmLog {
        self.h264.pcm_log.clone()
    }

    /// A scripted H.264 frame: production's plan and commit (`h264_refs`)
    /// around a PCM encoder or the real openh264 library run inline, in place
    /// of the encode worker. The stepped harness cannot run a worker thread;
    /// everything else, the manager, the wire header, the send path, the
    /// ledger feedback and the client's own 0x0A and 0x09, is production's.
    #[cfg(feature = "browserless-harness")]
    /// A scripted H.264 frame of an acked-reference session: production's
    /// plan and commit (`h264_refs`) around a PCM encoder run inline, in place
    /// of the encode worker. The stepped harness cannot run a worker thread;
    /// everything else, the manager, the wire header, the send path, the
    /// ledger feedback and the client's own 0x0A and 0x09, is production's.
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
        self.h264.backend_kind = if matches!(
            h.pcm,
            crate::encoder::h264_harness::PcmMode::OpenH264 { .. }
        ) {
            "openh264"
        } else {
            "pcm"
        };
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
        if let crate::encoder::h264_harness::PcmMode::OpenH264 {
            scene_change_every,
            churn_every,
            stale_every,
        } = h.pcm
        {
            self.apply_injected_openh264(
                seq,
                (scene_change_every, churn_every, stale_every),
                &req,
                &meta,
                max_dg_size,
                now,
            );
            return;
        }
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

    /// The `PcmMode::OpenH264` arm of [`apply_injected_pcm_refs`]: the real
    /// library encodes the frame's canvas inline, where the encode worker
    /// would, and the bridge commits the access unit it made. The truth log
    /// gets the reference the stream really names.
    #[cfg(feature = "browserless-harness")]
    fn apply_injected_openh264(
        &mut self,
        seq: u32,
        canvas: (u32, u32, u32),
        req: &super::frame_backend::H264Request,
        meta: &frame::FrameMeta,
        max_dg_size: usize,
        now: std::time::Instant,
    ) {
        use crate::encoder::h264::harness::Openh264Source;
        let log = self.h264.pcm_log.clone();
        let src = self
            .h264
            .oh_source
            .get_or_insert_with(|| Openh264Source::new(log, canvas));
        let is_idr = req.plan.kind == crate::encoder::h264::tracker::PicKind::Idr;
        src.ensure_open(
            req.openh264_lib
                .as_deref()
                .map(std::path::Path::to_path_buf),
            req.rate,
            req.fps,
            is_idr,
        )
        .expect(
            "the openh264 library opens (install libopenh264 or set GHOSTFRAME_H264_OPENH264_LIB)",
        );
        let picture = src.paint(seq);
        let (view, backend) = src.parts().expect("opened just above");
        let au = super::frame_backend::encode_planned(backend, &view, req)
            .expect("openh264 encodes every plan");
        let ref_seq = au.meta.header.as_ref().and_then(|hdr| {
            let st = self.h264.refs.as_ref()?;
            st.mgr.observe(&req.plan, hdr).ref_frame_seq()
        });
        let src = self.h264.oh_source.as_ref().expect("created above");
        src.record(seq, picture, &au, ref_seq);
        self.h264
            .pcm_log
            .lock()
            .expect("pcm log poisoned")
            .au_log
            .push(crate::encoder::h264::harness::AuRecord {
                timestamp_us: meta.timestamp_us,
                bytes: au.payload.len(),
                idr: au.meta.idr,
                raised: self.h264.rate.applied().is_some_and(|a| a != req.rate.bps),
            });
        self.h264_refs_commit(&req.plan, au, meta, max_dg_size, now);
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
        t.on_frame_sent(7, true, 70);
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
        t.on_frame_sent(5, false, 2);
        t.on_frame_sent(5 + TRACKER_RING as u32, false, 2);
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
        t.on_frame_sent(1, true, 1);
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

    /// A bridge whose live session is served by a `RefManager` (the client
    /// sends the v2 feedback and one plan was made), which is what reset
    /// requests act on.
    async fn refs_live_bridge() -> IoBridge {
        let mut b = bridge().await;
        b.client_caps.supports_h264 = true;
        b.client_caps.h264_acked_ref = true;
        let _ = b.h264_refs_plan(0, (64, 64), now_std(), false);
        assert!(b.h264_refs_live(), "premise: a manager serves the session");
        b
    }

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

    /// Send a frame; an IDR is the session's.
    fn send(b: &mut IoBridge, seq: u32, idr: bool) {
        let described = frame::Described {
            info: if idr {
                FrameRefInfo::IDR
            } else {
                FrameRefInfo::predicted(seq.wrapping_sub(1))
            },
            idr_cause: idr.then_some(IdrCause::Session),
        };
        b.send_h264_frame(
            H264Out {
                payload: vec![seq as u8; PAYLOAD],
                is_keyframe: idr,
            },
            &meta(),
            seq,
            DG,
            described,
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

    /// An IDR fragment is repaired by the loss path, not by a new IDR. A
    /// verdict on a fragment another transmission already delivered is
    /// spurious and does nothing.
    #[tokio::test(start_paused = true)]
    async fn idr_and_spurious_verdicts_make_no_frame_suspect() {
        let mut b = bridge().await;
        send(&mut b, 1, true);
        let idr = tracked_sent(&b, 0);
        depart(&mut b, &idr);
        b.note_h264_verdict(1, 0, now_std());
        assert_eq!(b.h264.stats.verdicts_idr_frag, 1);
        assert_eq!(
            b.h264.stats.frames_suspect, 0,
            "an IDR fragment is repaired"
        );

        // A P frame whose fragment was acknowledged through a probe copy.
        send(&mut b, 2, false);
        let sent = tracked_sent(&b, 0);
        depart(&mut b, &sent);
        let (ws, f, i) = *sent.iter().rfind(|x| x.1 == 2).expect("a frame-2 fragment");
        ack(&mut b, &[ws]);
        b.note_h264_verdict(f, i, now_std());
        assert_eq!(b.h264.stats.verdicts_already_acked, 1);
        assert_eq!(b.h264.stats.frames_suspect, 0);
        // And a frame the tracker no longer follows.
        b.note_h264_verdict(9_999, 0, now_std());
        assert_eq!(b.h264.stats.verdicts_unknown_frame, 1);
    }

    /// The backlog test of the frame skip: the encoder is not run while the
    /// queue holds more H.264 than a frame period carries (the plan side of it,
    /// and the IDR that runs past it, are in `tests.rs`).
    #[tokio::test(start_paused = true)]
    async fn the_backlog_predicate_closes_while_the_queue_is_backed_up_and_reopens() {
        let mut b = bridge().await;
        send(&mut b, 1, true);
        let idr = tracked_sent(&b, 0);
        depart(&mut b, &idr);
        let ws: Vec<u32> = idr.iter().map(|x| x.0).collect();
        ack(&mut b, &ws);
        assert!(!b.h264_backlog_exceeded(), "empty queue");

        // The FIFO is at the gate: frames pile up in the queue.
        b.forced_fifo_depth_for_test = Some(usize::MAX / 2);
        let mut seq = 2;
        while !b.h264_backlog_exceeded() {
            send(&mut b, seq, false);
            seq += 1;
            assert!(seq < 200, "the backlog never closed the gate");
        }
        assert!(seq > 2, "the first frame after an empty queue is encoded");
        // The queue drains: frames are encoded again.
        b.forced_fifo_depth_for_test = None;
        b.pump_admission();
        assert_eq!(
            b.reliable_emitter.queued_h264_bytes(),
            0,
            "premise: drained"
        );
        assert!(!b.h264_backlog_exceeded());
    }

    /// With no acknowledgement ever arriving, the probe cache and the tracker
    /// stay bounded: the cache to its newest frames, the tracker by its ring.
    #[tokio::test(start_paused = true)]
    async fn the_probe_cache_stays_bounded_when_nothing_is_acknowledged() {
        use crate::transport::reliable_emitter::h264::H264_CACHE_FRAMES;
        let mut b = bridge().await;
        for seq in 1..=(H264_CACHE_FRAMES as u32 + 40) {
            send(&mut b, seq, seq == 1);
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
        assert!(b.reliable_emitter.h264.get(1, 0).is_none());

        // The IDR for the new size supersedes what is left.
        send(&mut b, 3, true);
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

        // One byte less: the biggest no longer fits.
        b.forced_raw_datagram_size_for_test = Some(biggest + framing - 1);
        b.drain_reliable_emitter(now_std());
        assert!(b.h264.stats.oversize_h264 > 0, "one byte over");
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

        // Every fragment is recognised by its header, not counted as untracked.
        assert_eq!(b.h264.stats.oversize_h264, wss.len() as u64);
        assert_eq!(b.h264.stats.tx_forgotten, 0);
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
        // Premises: all of it is populated.
        assert!(b.reliable_emitter.queued_h264_bytes() > 0);
        assert!(!b.reliable_emitter.h264.is_empty());
        assert_eq!(b.h264.tracker.is_complete(1), Some(false));
        let counted = b.h264.stats.frames_sent;

        b.fire_session_reset(ConnectionHandle(0));

        assert_eq!(b.h264.tracker.is_complete(1), None, "frame tracker");
        assert!(b.reliable_emitter.h264.is_empty(), "probe cache");
        assert_eq!(
            b.reliable_emitter.queued_h264_bytes(),
            0,
            "queued fragments"
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
        assert!(!b.reliable_emitter.h264.is_empty());
        b.forget_session(h);
        assert!(b.reliable_emitter.h264.is_empty());
        assert_eq!(b.reliable_emitter.queued_h264_bytes(), 0);
        assert_eq!(b.h264.tracker.is_complete(1), None);
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

    /// The fragments of an IDR burst and of a P frame coded at an open window's
    /// rate (the probe raise) are tagged for the window; a P frame sent after
    /// the window ended is not.
    #[tokio::test(start_paused = true)]
    async fn an_idr_burst_and_a_p_frame_inside_the_window_are_probe_material() {
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
        send(&mut b, 2, false);
        assert!(
            b.active_probe.as_ref().unwrap().packets_sent > after_idr,
            "a P frame inside the window is part of its burst"
        );
        assert!(b.reliable_emitter.h264.get(2, 0).unwrap().probe.is_some());
        tokio::time::advance(Duration::from_secs(11)).await;
        let before = b.active_probe.as_ref().unwrap().packets_sent;
        send(&mut b, 3, false);
        assert_eq!(
            b.active_probe.as_ref().unwrap().packets_sent,
            before,
            "after the window's end nothing is tagged"
        );
        assert!(b.reliable_emitter.h264.get(3, 0).unwrap().probe.is_none());
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

    /// (c): three honoured client reset requests within 10 s.
    #[tokio::test(start_paused = true)]
    async fn three_honoured_reset_requests_in_ten_seconds_raise_the_stall_flag() {
        use ghostframe_protocol::feedback_stream::{ResetReason, ResetRequest};
        let mut b = refs_live_bridge().await;
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
            let mut b = refs_live_bridge().await;
            let t0 = now_std();
            b.handle_reset_request(req, t0);
            b.handle_reset_request(req, t0 + ms(gap));
            assert!(!b.update_h264_stall(t0 + ms(gap)), "gap {gap}");
        }
        // One every 5.1 s: the third is 10.2 s after the first.
        let mut b = refs_live_bridge().await;
        let t0 = now_std();
        for i in 0..6u64 {
            let t = t0 + ms(i * 5_100);
            b.handle_reset_request(req, t);
            assert!(!b.update_h264_stall(t), "request {i}");
        }
        assert_eq!(b.reset_requests_honoured, 6, "premise: all honoured");
        // A burst inside the 2 s limit makes one IDR, not three.
        let mut b = refs_live_bridge().await;
        let t0 = now_std();
        for i in 0..5u64 {
            b.handle_reset_request(req, t0 + ms(i * 100));
        }
        assert_eq!(b.reset_requests_honoured, 1, "premise: one honoured");
        assert!(!b.update_h264_stall(t0 + ms(400)));
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
        let analysed = crate::transport::io_bridge::frame_backend::Analysed::default();
        let (_, prev, new) = b.classify_dirty_tiles(&analysed, &meta(), 4, 4, 100);
        assert_eq!((prev, new), (FrameMode::H264, FrameMode::TileCodec));
        assert_eq!(b.frame_mode, FrameMode::TileCodec);
        // The abandoned H.264 does not compete with the tile repaint.
        assert_eq!(b.reliable_emitter.queued_h264_bytes(), 0, "queue purged");
        assert!(b.reliable_emitter.h264.is_empty(), "probe cache dropped");
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
}
