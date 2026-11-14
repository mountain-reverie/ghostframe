// SPDX-License-Identifier: MIT OR Apache-2.0
//! H.264 full-frame fragment reassembly.
//!
//! Port of the frame-level datagram branch in `main.ts` (~1326-1399): a
//! separate reassembly track from the per-tile pipeline, keyed by
//! `frame_seq` alone (there is exactly one in-flight H.264 access unit
//! per `frame_seq`). Uses `FrameHeader` (22 bytes, v2) instead of `TileHeader`.
//!
//! Safety: nothing here may panic on malformed/truncated/hostile input;
//! all arithmetic on sequence numbers saturates; the assembly map is
//! bounded by the same `latest - 2` eviction window used by the tile
//! pipeline, so it cannot grow unboundedly under a flood of distinct
//! `frame_seq` values.

use ghostframe_protocol::ack::AckEntry;
use ghostframe_protocol::protocol::{decode_frame_datagram, FrameHeader, FRAME_FLAG_PAD, NO_ACK};

use crate::event::Event;
use crate::h264_gate::{RefuseCause, Verdict};
use crate::{ClientCore, PollOutput, MAX_FRAME_FRAGMENTS, MAX_H264_ACCESS_UNIT_BYTES};

/// How long an IDR assembly is exempt from the `latest - 2` window. The server
/// sends an IDR's fragments again on a loss verdict and holds its P frames back
/// until every fragment is acknowledged, so an IDR can legitimately complete
/// after newer frame sequence numbers have been seen (tile frames share the
/// sequence). Bounded in time and in count: a hostile peer cannot park more
/// than one of them (the newest; an older IDR is superseded by it), for this
/// long.
pub(crate) const IDR_EXEMPT_US: u64 = 2_000_000;

/// One in-progress full-frame (H.264 access unit) assembly.
pub(crate) struct FrameAssembly {
    pub header: FrameHeader,
    /// When the first fragment arrived (`now_us`), for the IDR window exemption.
    pub created_us: u64,
    pub fragments: Vec<Option<Vec<u8>>>,
    pub received: usize,
    /// Payload bytes buffered so far (bounded by `MAX_H264_ACCESS_UNIT_BYTES`).
    pub bytes: usize,
}

impl ClientCore {
    /// Feed one non-tile datagram through the H.264 frame-reassembly path
    /// (`main.ts` ~1326-1399). Malformed datagrams are dropped silently.
    pub(crate) fn handle_frame_datagram(
        &mut self,
        bytes: &[u8],
        now_us: u64,
        events: &mut Vec<Event>,
    ) {
        let (fh, payload) = match decode_frame_datagram(bytes) {
            Ok(x) => x,
            Err(_) => return,
        };

        // A source fragment is acknowledged on receipt, whatever becomes of it:
        // the acknowledgement says the datagram arrived, which is all the
        // server's ledger and loss path ask of it. What the client then does
        // with the frame (decode, refuse, drop it as stale) is no part of it.
        // Anything the wire format cannot express as a fragment is dropped
        // first, so it is not acknowledged, like an invalid tile.
        if fh.frag_idx >= fh.frag_total || fh.frag_total > MAX_FRAME_FRAGMENTS {
            return;
        }
        if fh.wire_seq != NO_ACK {
            self.wire_loss.on_received(fh.wire_seq);
            let entry = AckEntry {
                wire_seq: fh.wire_seq,
                arrival_time_ms_lo16: ((now_us / 1000) & 0xFFFF) as u16,
            };
            if let Some(dg) = self.ack_batcher.add(entry, now_us) {
                self.outbox.push_back(PollOutput::Datagram(dg));
            }
        }

        // Probe padding is acknowledged (above) and nothing else: it names a
        // frame the receiver has already seen and must not touch its assembly.
        if fh.flags & FRAME_FLAG_PAD != 0 {
            return;
        }
        // A P-frame fragment of a frame at or before the newest one delivered
        // is moot: the decoder cannot go back, and a copy (a retransmission
        // that crossed its acknowledgement) of a single-fragment P frame would
        // complete it a second time and be decoded again. An IDR is judged
        // when it completes (below), because its late fragments are kept on
        // purpose (`IDR_EXEMPT_US`).
        if !fh.is_keyframe()
            && self
                .h264_gate
                .last_fed()
                .is_some_and(|last| fh.frame_seq <= last)
        {
            return;
        }
        // Drop datagrams for frames older than the eviction window, except the
        // fragments of an IDR that may still complete (see `IDR_EXEMPT_US`).
        if fh.frame_seq < self.latest_full_frame_seq.saturating_sub(2)
            && !self.idr_fragment_is_exempt(&fh, now_us)
        {
            return;
        }
        if fh.frame_seq > self.latest_full_frame_seq {
            self.latest_full_frame_seq = fh.frame_seq;
        }

        self.evict_stale_frame_assemblies(now_us);

        // A frag_total of 0 is malformed (no valid fragment index can ever
        // satisfy frag_idx < frag_total); drop defensively.
        if fh.frag_total == 0 {
            return;
        }
        // `frag_total` is an attacker-chosen u16 and sizes a Vec on first
        // sight; refuse claims no real access unit makes.
        if fh.frag_total > MAX_FRAME_FRAGMENTS {
            return;
        }

        self.frame_assemblies
            .entry(fh.frame_seq)
            .or_insert_with(|| FrameAssembly {
                header: fh,
                created_us: now_us,
                fragments: vec![None; fh.frag_total as usize],
                received: 0,
                bytes: 0,
            });
        // A new IDR assembly supersedes the older ones: they go now, not at
        // the next datagram, so the map is never more than the window plus one.
        if fh.is_keyframe() {
            self.evict_stale_frame_assemblies(now_us);
        }
        let Some(asm) = self.frame_assemblies.get_mut(&fh.frame_seq) else {
            return;
        };

        let fi = fh.frag_idx as usize;
        if fi < asm.fragments.len() && asm.fragments[fi].is_none() {
            // Bound the access unit itself, not only its fragment count: each
            // fragment can carry a full datagram. An oversize unit is
            // abandoned whole -- a truncated H.264 stream is undecodable.
            if asm.bytes + payload.len() > MAX_H264_ACCESS_UNIT_BYTES {
                self.frame_assemblies.remove(&fh.frame_seq);
                return;
            }
            asm.bytes += payload.len();
            asm.fragments[fi] = Some(payload.to_vec());
            asm.received += 1;
        }
        self.finish_frame_if_complete(fh.frame_seq, now_us, events);
    }

    /// The newest IDR assembly held, if any.
    fn newest_idr_assembly(&self) -> Option<u32> {
        self.frame_assemblies
            .iter()
            .filter(|(_, a)| a.header.is_keyframe())
            .map(|(&s, _)| s)
            .max()
    }

    /// Evict in-progress assemblies older than `latest - 2`, except the newest
    /// IDR assembly while it is inside its exemption: an older IDR is
    /// superseded by it, and exempting every one would let a stream of IDR
    /// fragments with fresh frame numbers (each claiming 16 K fragments) hold
    /// memory for the exemption's length.
    fn evict_stale_frame_assemblies(&mut self, now_us: u64) {
        let threshold = self.latest_full_frame_seq.saturating_sub(2);
        let keep = self.newest_idr_assembly().filter(|seq| {
            self.frame_assemblies
                .get(seq)
                .is_some_and(|a| now_us.saturating_sub(a.created_us) < IDR_EXEMPT_US)
        });
        self.frame_assemblies
            .retain(|&seq, _| seq >= threshold || Some(seq) == keep);
    }

    /// Whether a fragment older than the window may still be taken: it belongs
    /// to the newest IDR, which either has an assembly inside its exemption
    /// already, or is newer than any IDR assembly held and may start one.
    fn idr_fragment_is_exempt(&self, fh: &FrameHeader, now_us: u64) -> bool {
        if !fh.is_keyframe() {
            return false;
        }
        match self.frame_assemblies.get(&fh.frame_seq) {
            Some(asm) => {
                now_us.saturating_sub(asm.created_us) < IDR_EXEMPT_US
                    && self.newest_idr_assembly() == Some(fh.frame_seq)
            }
            None => self
                .newest_idr_assembly()
                .is_none_or(|newest| fh.frame_seq > newest),
        }
    }

    fn finish_frame_if_complete(&mut self, frame_seq: u32, now_us: u64, events: &mut Vec<Event>) {
        let complete = self
            .frame_assemblies
            .get(&frame_seq)
            .is_some_and(|a| a.received == a.fragments.len());
        if complete {
            let asm = match self.frame_assemblies.remove(&frame_seq) {
                Some(a) => a,
                None => return,
            };
            // The gate decides (design 7.1). A frame it refuses is never
            // decoded, and is reported as refused in the next frame ack.
            let reset_decoder = match self.h264_gate.on_frame(&asm.header, now_us) {
                Verdict::Feed { reset_decoder } => reset_decoder,
                Verdict::Refuse(cause) => {
                    // A copy of a frame already fed is stale, not refused: the
                    // original has an answer coming, and a refusal now would
                    // race it.
                    let copy = cause == RefuseCause::Stale && self.h264_gate.was_fed(frame_seq);
                    if !copy {
                        self.frame_acks.record(frame_seq, false, now_us);
                    }
                    self.after_gate();
                    return;
                }
            };
            if reset_decoder {
                // Results from before the reset are untrustworthy only if the
                // decoder had failed. A reset on a healthy decoder (a GOP IDR)
                // leaves frames still in flight valid: their answers may
                // arrive after it.
                if self.decoder_broken {
                    self.decoder_epoch_start = Some(frame_seq);
                }
                self.decoder_broken = false;
            }
            self.after_gate();
            let mut out = Vec::new();
            for b in asm.fragments.iter().flatten() {
                out.extend_from_slice(b);
            }
            events.push(Event::NeedsH264 {
                frame_seq: asm.header.frame_seq,
                timestamp_us: asm.header.timestamp_us,
                is_keyframe: asm.header.is_keyframe(),
                ref_frame_seq: asm.header.ref_frame_seq,
                flags: asm.header.flags,
                slots: asm.header.slots,
                reset_decoder,
                payload: out,
            });
        }
    }
}
