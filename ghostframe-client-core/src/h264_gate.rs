//! The H.264 reference gate (design 7.1) and the frame-ack window (6.2).
//!
//! Pure: no clock of its own (every call that needs time takes `now_us`), no
//! I/O. `ClientCore` runs every completed access unit through
//! [`H264Gate::on_frame`] before handing it to a decoder, and tells the gate
//! what the decoder did with it (`ClientCore::h264_result`, which resets the
//! gate on a decode error and otherwise leaves it alone).
//!
//! # What the gate decides
//!
//! A frame is **fed** iff
//! - its `frame_seq` is newer than the newest frame fed so far, and
//! - it is an IDR, or the picture it predicts from is one this client really
//!   holds: for a long-term target (`FRAME_FLAG_TARGET_IS_LT`) the named slot
//!   must contain `ref_frame_seq`; for a short-term target `ref_frame_seq`
//!   must be a good frame and still the newest reference.
//!
//! The header's `ref_frame_seq` is the sender's belief (filled from the
//! encoder's DPB). Checking it against the client's own slot contents turns a
//! server belief error into a refusal instead of silent drift.
//!
//! # Legacy streams
//!
//! A server that does not plan references (Phase A) sets no flag on a P frame
//! and names the previous H.264 frame it sent in `ref_frame_seq`. That is a
//! short-term target on the newest reference, so the same rule gives today's
//! behaviour: a P whose reference never arrived is held back, and so is every
//! frame chained from it, until an IDR; the server is asked for that IDR
//! (`GateReset`, paced). A P frame with `REF_FRAME_NONE` names nothing and is
//! fed unless the gate is already waiting for an IDR.
//!
//! The one place the two stream kinds differ is *when* a refusal turns into a
//! `GateReset`. A legacy server can only recover with an IDR, so the request
//! goes out at once. A server that has shown acked-reference features
//! (non-ref, mark or long-term-target flags) repairs from an anchor on the
//! refused bit in the frame ack, so the gate only asks for an IDR when it has
//! been refusing, with nothing fed, for [`ACKED_REF_STUCK_US`].

use std::collections::VecDeque;

use ghostframe_protocol::feedback_stream::ResetReason;
use ghostframe_protocol::protocol::{
    FrameAck, FrameHeader, FRAME_FLAG_MARK_LT, FRAME_FLAG_NON_REF, FRAME_FLAG_TARGET_IS_LT,
    REF_FRAME_NONE,
};

/// Reset requests of one reason are at least this far apart: the server honours
/// one per 2 s, so a retry sent exactly one window later could land inside it.
pub const RESET_REQUEST_MIN_INTERVAL_US: u64 = 2_200_000;

/// How long an acked-reference stream may be refused with nothing fed before
/// the gate falls back to asking for an IDR.
pub const ACKED_REF_STUCK_US: u64 = 2_200_000;

/// Frame acks are sent at most this often.
pub const FRAME_ACK_MIN_INTERVAL_US: u64 = 4_000;

/// A frame ack that has waited this long for an ack batch to ride with goes
/// out by itself (an asynchronous decoder on a quiet screen: no tile or frame
/// datagrams arrive, so no batch is ever flushed). Longer than a frame
/// interval plus the batch window, so a decoder that answers after its own
/// frame's batch still catches the next frame's.
pub const FRAME_ACK_FALLBACK_US: u64 = 100_000;

/// Frames remembered as fed, to tell a duplicate of one from a late frame.
const FED_CAP: usize = 128;

/// Decoded frames remembered as usable references. Bounded: the server's
/// frame tracker spans 256 frames and nothing older is ever named.
const GOOD_CAP: usize = 128;

/// Long-term slots.
const LT_SLOTS: usize = 2;

/// Wrap-safe "is `a` newer than `b`" on the 31-bit `frame_seq`.
pub fn seq_after(a: u32, b: u32) -> bool {
    let forward = a.wrapping_sub(b) & 0x7FFF_FFFF;
    forward != 0 && forward < (1 << 30)
}

/// What the gate decided about one completed access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Hand it to the decoder. `reset_decoder` is set when the decoder must
    /// be reset first: the frame is an IDR, or the gate was reset since the
    /// last frame it fed.
    Feed { reset_decoder: bool },
    /// Do not decode it; it is reported in the ack's refused mask.
    Refuse(RefuseCause),
}

/// Why a frame was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseCause {
    /// Not newer than the newest frame already fed (reordering, or a copy).
    Stale,
    /// The reference it names is not one the client holds.
    NoReference,
    /// A long-term slot index outside the two the stream has.
    BadSlot,
}

/// What the decoder did with a frame the gate fed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264Outcome {
    /// A picture with this frame's pts came out of the call that consumed it.
    Decoded,
    /// The frame was consumed but no picture for it came out. It is reported
    /// refused; the gate keeps it as a reference, since the decoder holds it
    /// (a decoder with one frame of output delay answers every frame so).
    NotOutput,
    /// The decoder failed. The frame is refused, and the decoder's state is
    /// unknown: the gate resets and asks for an IDR.
    DecodeError,
    /// There is no decoder for the stream.
    NoDecoder,
}

/// Why the gate is being reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetCause {
    /// An IDR arrived (done by `on_frame`). No request: the IDR is the answer.
    IdrReceived,
    /// The display resolution changed. No request: the server follows it with
    /// an IDR of its own, and an IDR resets the gate (`IdrReceived`). Nothing
    /// calls this today; it exists so a consumer that learns of the change
    /// before the IDR arrives can drop its references early.
    Resolution,
    /// The decoder failed.
    DecoderFailure,
    /// There is no decoder for the stream.
    NoDecoder,
}

/// The gate. See the module docs.
#[derive(Debug, Default)]
pub struct H264Gate {
    /// Frames fed since the last reset (decoded, or still in the decoder).
    /// Optimistic: a frame is usable as a reference as soon as it is fed,
    /// because an asynchronous decoder (WebCodecs) answers later and the next
    /// frame must not wait for it. A frame reported not output stays: the
    /// decoder holds it.
    good: VecDeque<u32>,
    /// The frame in each long-term slot.
    lt: [Option<u32>; LT_SLOTS],
    /// The newest frame that is a reference (nal_ref_idc != 0).
    newest_ref: Option<u32>,
    /// The newest frame fed.
    last_fed: Option<u32>,
    /// Waiting for an IDR: a reference was found missing, or the gate was
    /// reset. Only a `REF_FRAME_NONE` P frame consults it (it names no
    /// reference to check).
    waiting_for_idr: bool,
    /// The stream has shown acked-reference features.
    acked_ref_stream: bool,
    /// When the current run of refusals began (cleared by any feed).
    refusing_since_us: Option<u64>,
    /// Frames fed, newest last, never cleared by a reset.
    fed: VecDeque<u32>,
    /// When a `GateReset` last went out. A `GateReset` is also held back for
    /// 2.2 s after a `DecoderFailure`: a decode error and the refusals that
    /// follow it are one event to the server.
    last_request_us: Option<u64>,
    /// When a `DecoderFailure` request last went out. Kept apart from
    /// `last_request_us`: the server acts only on `DecoderFailure`, so an
    /// earlier `GateReset` must never delay one.
    last_failure_request_us: Option<u64>,
    /// No decoder exists. Sticky: the request went out once, and nothing the
    /// stream does changes that, so neither it nor `GateReset` is repeated.
    no_decoder: bool,
    /// A reset request for the caller to send.
    request: Option<(ResetReason, u32)>,
    /// The next frame fed must reset the decoder.
    decoder_reset_pending: bool,
}

impl H264Gate {
    pub fn new() -> Self {
        Self::default()
    }

    /// The newest frame fed, if any.
    pub fn last_fed(&self) -> Option<u32> {
        self.last_fed
    }

    /// The reset request the gate wants sent, once.
    pub fn take_request(&mut self) -> Option<(ResetReason, u32)> {
        self.request.take()
    }

    /// Whether the stream has used any acked-reference feature.
    pub fn is_acked_ref_stream(&self) -> bool {
        self.acked_ref_stream
    }

    /// The frame currently held in long-term slot `slot`.
    pub fn long_term(&self, slot: usize) -> Option<u32> {
        self.lt.get(slot).copied().flatten()
    }

    /// Whether `seq` is currently a usable reference.
    pub fn is_good(&self, seq: u32) -> bool {
        self.good.contains(&seq)
    }

    /// Decide one completed access unit. Updates the gate on `Feed`.
    pub fn on_frame(&mut self, h: &FrameHeader, now_us: u64) -> Verdict {
        let seq = h.frame_seq;
        if h.flags & (FRAME_FLAG_NON_REF | FRAME_FLAG_MARK_LT | FRAME_FLAG_TARGET_IS_LT) != 0 {
            self.acked_ref_stream = true;
        }
        if self.last_fed.is_some_and(|l| !seq_after(seq, l)) {
            return Verdict::Refuse(RefuseCause::Stale);
        }
        if h.is_keyframe() {
            self.reset(ResetCause::IdrReceived, seq, now_us);
            self.last_fed = Some(seq);
            self.note_fed(seq);
            self.good.push_back(seq);
            self.lt = [Some(seq), None];
            self.newest_ref = Some(seq);
            self.waiting_for_idr = false;
            self.refusing_since_us = None;
            self.decoder_reset_pending = false;
            return Verdict::Feed {
                reset_decoder: true,
            };
        }

        let marks = h.flags & FRAME_FLAG_MARK_LT != 0 && h.flags & FRAME_FLAG_NON_REF == 0;
        let mark_slot = usize::from(h.slots & 0b11);
        if marks && mark_slot >= LT_SLOTS {
            return self.refuse(RefuseCause::BadSlot, seq, now_us);
        }
        let held = if h.flags & FRAME_FLAG_TARGET_IS_LT != 0 {
            let slot = usize::from((h.slots >> 2) & 0b11);
            if slot >= LT_SLOTS {
                return self.refuse(RefuseCause::BadSlot, seq, now_us);
            }
            h.ref_frame_seq != REF_FRAME_NONE && self.lt[slot] == Some(h.ref_frame_seq)
        } else if h.ref_frame_seq == REF_FRAME_NONE {
            !self.waiting_for_idr
        } else {
            self.good.contains(&h.ref_frame_seq) && self.newest_ref == Some(h.ref_frame_seq)
        };
        if !held {
            return self.refuse(RefuseCause::NoReference, seq, now_us);
        }

        self.last_fed = Some(seq);
        self.note_fed(seq);
        self.good.push_back(seq);
        if self.good.len() > GOOD_CAP {
            self.good.pop_front();
        }
        if h.flags & FRAME_FLAG_NON_REF == 0 {
            self.newest_ref = Some(seq);
            if marks {
                self.lt[mark_slot] = Some(seq);
            }
        }
        self.refusing_since_us = None;
        Verdict::Feed {
            reset_decoder: std::mem::take(&mut self.decoder_reset_pending),
        }
    }

    fn note_fed(&mut self, seq: u32) {
        self.fed.push_back(seq);
        if self.fed.len() > FED_CAP {
            self.fed.pop_front();
        }
    }

    /// Whether `seq` was fed to the decoder (and is still remembered). A copy
    /// of such a frame that completes again is refused as stale, but it is not
    /// a frame the client refused: the original is on its way to an answer.
    pub fn was_fed(&self, seq: u32) -> bool {
        self.fed.contains(&seq)
    }

    fn refuse(&mut self, cause: RefuseCause, seq: u32, now_us: u64) -> Verdict {
        if cause == RefuseCause::NoReference {
            self.waiting_for_idr = true;
            if self.no_decoder {
                return Verdict::Refuse(cause);
            }
            let since = *self.refusing_since_us.get_or_insert(now_us);
            let due = !self.acked_ref_stream || now_us.saturating_sub(since) >= ACKED_REF_STUCK_US;
            if due {
                self.paced_request(ResetReason::GateReset, seq, now_us);
            }
        }
        Verdict::Refuse(cause)
    }

    fn paced_request(&mut self, reason: ResetReason, seq: u32, now_us: u64) {
        let due = |last: Option<u64>| {
            last.is_none_or(|t| now_us.saturating_sub(t) >= RESET_REQUEST_MIN_INTERVAL_US)
        };
        // One direction only: a GateReset waits out a DecoderFailure, never the
        // other way round, because the server ignores GateReset.
        let send = match reason {
            ResetReason::DecoderFailure => due(self.last_failure_request_us),
            ResetReason::GateReset => {
                due(self.last_request_us) && due(self.last_failure_request_us)
            }
            ResetReason::Resolution | ResetReason::NoDecoder => due(self.last_request_us),
        };
        if send {
            match reason {
                ResetReason::DecoderFailure => self.last_failure_request_us = Some(now_us),
                ResetReason::GateReset | ResetReason::Resolution | ResetReason::NoDecoder => {
                    self.last_request_us = Some(now_us)
                }
            }
            self.request = Some((reason, seq));
        }
    }

    // A frame the decoder consumed without a same-call picture (`NotOutput`)
    // changes nothing here: a decoder with one frame of output delay answers
    // every frame that way and still holds every reference. Only a decode
    // error or a reset (`reset`) evicts.

    /// Forget every reference. The only way the decoder gets reset: the next
    /// frame fed carries `reset_decoder`. Asks the server for an IDR unless
    /// the cause is a received IDR or a resolution change.
    pub fn reset(&mut self, cause: ResetCause, frame_seq: u32, now_us: u64) {
        self.good.clear();
        self.lt = [None; LT_SLOTS];
        self.newest_ref = None;
        self.decoder_reset_pending = true;
        match cause {
            ResetCause::IdrReceived => {}
            ResetCause::Resolution => self.waiting_for_idr = true,
            ResetCause::DecoderFailure => {
                self.waiting_for_idr = true;
                self.paced_request(ResetReason::DecoderFailure, frame_seq, now_us);
            }
            ResetCause::NoDecoder => {
                self.waiting_for_idr = true;
                if !self.no_decoder {
                    self.no_decoder = true;
                    self.request = Some((ResetReason::NoDecoder, frame_seq));
                    self.last_request_us = Some(now_us);
                }
            }
        }
    }
}

/// The cumulative frame-ack window (design 6.2): the outcome of the 64 newest
/// handled frames, and when the next ack may go out.
#[derive(Debug, Default)]
pub struct FrameAckWindow {
    newest: Option<u32>,
    decoded: u64,
    refused: u64,
    /// When the oldest unreported handled frame was recorded.
    dirty_since_us: Option<u64>,
    last_sent_us: Option<u64>,
}

const SEQ_MASK: u32 = 0x7FFF_FFFF;

impl FrameAckWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `seq` was handled at `now_us`: `decoded`, or refused.
    /// Ordering is 31-bit serial arithmetic, as the server reads it.
    pub fn record(&mut self, seq: u32, decoded: bool, now_us: u64) {
        match self.newest {
            None => {
                self.newest = Some(seq);
                self.decoded = 0;
                self.refused = 0;
                self.set(0, decoded);
            }
            Some(newest) => {
                let forward = seq.wrapping_sub(newest) & SEQ_MASK;
                if forward == 0 {
                    self.set(0, decoded);
                } else if forward < (1 << 30) {
                    // Newer: the window slides.
                    if forward >= 64 {
                        self.decoded = 0;
                        self.refused = 0;
                    } else {
                        self.decoded <<= forward;
                        self.refused <<= forward;
                    }
                    self.newest = Some(seq);
                    self.set(0, decoded);
                } else {
                    let back = newest.wrapping_sub(seq) & SEQ_MASK;
                    if back >= 64 {
                        return; // fell off the window; nothing to report
                    }
                    self.set(back, decoded);
                }
            }
        }
        self.dirty_since_us.get_or_insert(now_us);
    }

    fn set(&mut self, bit: u32, decoded: bool) {
        let m = 1u64 << bit;
        // A decode outranks a later refusal of the same frame.
        if !decoded && self.decoded & m != 0 {
            return;
        }
        if decoded {
            self.decoded |= m;
            self.refused &= !m;
        } else {
            self.refused |= m;
            self.decoded &= !m;
        }
    }

    /// The ack to send now, if one is due: something is unreported and
    /// [`FRAME_ACK_MIN_INTERVAL_US`] has passed since the last one. The
    /// caller asks right after an ack batch went out, so the two share a QUIC
    /// packet and the frame ack adds no packet of its own.
    pub fn take_due(&mut self, now_us: u64) -> Option<FrameAck> {
        self.dirty_since_us?;
        if self
            .last_sent_us
            .is_some_and(|t| now_us.saturating_sub(t) < FRAME_ACK_MIN_INTERVAL_US)
        {
            return None;
        }
        self.take(now_us)
    }

    /// The ack to send on its own because no ack batch came to carry it for
    /// [`FRAME_ACK_FALLBACK_US`].
    pub fn take_overdue(&mut self, now_us: u64) -> Option<FrameAck> {
        if self.fallback_at()? > now_us {
            return None;
        }
        self.take(now_us)
    }

    fn take(&mut self, now_us: u64) -> Option<FrameAck> {
        let newest_seq = self.newest?;
        self.dirty_since_us = None;
        self.last_sent_us = Some(now_us);
        Some(FrameAck {
            newest_seq,
            decoded_mask: self.decoded,
            refused_mask: self.refused,
        })
    }

    /// When [`Self::take_overdue`] next has something to send. Only the
    /// fallback has a deadline: the normal path rides the ack batch's.
    pub fn poll_timeout(&self) -> Option<u64> {
        self.fallback_at()
    }

    fn fallback_at(&self) -> Option<u64> {
        self.dirty_since_us.map(|t| t + FRAME_ACK_FALLBACK_US)
    }
}
