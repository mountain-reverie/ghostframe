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

use std::collections::HashMap;

use ghostframe_protocol::ack::AckEntry;
use ghostframe_protocol::fec::decode_parity_payload;
use ghostframe_protocol::feedback_stream::ResetReason;
use ghostframe_protocol::protocol::{
    decode_frame_datagram, FrameHeader, FRAME_FLAG_PAD, NO_ACK, REF_FRAME_NONE,
};

/// Reference-break reset requests are at least this far apart: the server
/// answers one a second at most, and the native renderer paces its own decode
/// failures the same way.
const REF_RESET_MIN_INTERVAL_US: u64 = 2_200_000;

use crate::event::Event;
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
    /// XOR parities held for groups that still have a fragment missing,
    /// keyed by `group_start`: `(group_len, xor)`. Kept so a source that
    /// arrives after its parity can still complete the group; dropped as soon
    /// as the group is complete. NOT charged to `bytes`: parity must never be
    /// able to push a real frame over the access-unit budget. Bounded by
    /// count (see `MAX_XOR_GROUP`, and the per-frame cap at the call site).
    pub parities: HashMap<u16, (u8, Vec<u8>)>,
}

/// Largest XOR group the client will read. The server uses 2 and 5; the cap
/// bounds the per-fragment parity lookup.
const MAX_XOR_GROUP: u8 = 16;

fn group_missing(asm: &FrameAssembly, group_start: u16, group_len: u8) -> usize {
    let (start, end) = (
        group_start as usize,
        group_start as usize + group_len as usize,
    );
    if end > asm.fragments.len() {
        return 0;
    }
    asm.fragments[start..end]
        .iter()
        .filter(|f| f.is_none())
        .count()
}

/// Rebuild the one missing fragment of `group_start..group_start+group_len`
/// from the group's XOR parity, if exactly one is missing. Returns the index
/// of the fragment rebuilt.
///
/// Every source but the frame's last is full-size, so the parity (as long as
/// the group's longest source) is the missing fragment's exact length. The
/// last fragment is short and comes back zero-padded; a NAL ends in a
/// non-zero byte (RBSP stop bit, or an emulation-prevention 0x03), so its
/// trailing zeros are padding and are stripped.
fn recover_group(
    asm: &mut FrameAssembly,
    group_start: u16,
    group_len: u8,
    xor: &[u8],
) -> Option<usize> {
    let start = group_start as usize;
    let end = start + group_len as usize;
    if !(2..=MAX_XOR_GROUP).contains(&group_len) || end > asm.fragments.len() {
        return None;
    }
    let mut missing = None;
    for i in start..end {
        if asm.fragments[i].is_none() {
            if missing.is_some() {
                return None;
            }
            missing = Some(i);
        }
    }
    let missing = missing?;
    let mut out = xor.to_vec();
    for f in asm.fragments[start..end].iter().flatten() {
        // A source longer than the parity cannot belong to this group.
        if f.len() > out.len() {
            return None;
        }
        for (o, b) in out.iter_mut().zip(f) {
            *o ^= b;
        }
    }
    if missing == asm.fragments.len() - 1 {
        while out.last() == Some(&0) {
            out.pop();
        }
    }
    if out.is_empty() || asm.bytes + out.len() > MAX_H264_ACCESS_UNIT_BYTES {
        return None;
    }
    asm.bytes += out.len();
    asm.fragments[missing] = Some(out);
    asm.received += 1;
    Some(missing)
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
        if !fh.is_parity() {
            // (`frag_total == 0` is a parity by `is_parity`, not reached here.)
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
        }

        // Probe padding is acknowledged (above) and nothing else: it names a
        // frame the receiver has already seen and must not touch its assembly.
        if !fh.is_parity() && fh.flags & FRAME_FLAG_PAD != 0 {
            return;
        }
        // A P-frame fragment of a frame at or before the newest one delivered
        // is moot: the decoder cannot go back, and a copy (a retransmission
        // that crossed its acknowledgement) of a single-fragment P frame would
        // complete it a second time and be decoded again. An IDR is judged
        // when it completes (below), because its late fragments are kept on
        // purpose (`IDR_EXEMPT_US`).
        if !fh.is_parity()
            && !fh.is_keyframe()
            && self
                .last_delivered_frame_seq
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
        if fh.is_parity() {
            if fh.frag_idx != fh.frag_total {
                return; // a different parity scheme: not ours to read
            }
            let Some((group_start, group_len, xor)) = decode_parity_payload(payload) else {
                return;
            };
            let Some(asm) = self.frame_assemblies.get_mut(&fh.frame_seq) else {
                return;
            };
            if fh.frag_total as usize != asm.fragments.len() {
                return;
            }
            // Bound what a hostile peer can park here: one parity per group,
            // at most one per two fragments.
            if asm.parities.contains_key(&group_start)
                || asm.parities.len() > asm.fragments.len() / 2
            {
                return;
            }
            if recover_group(asm, group_start, group_len, xor).is_some() {
                self.h264_fragments_recovered += 1;
            } else if group_missing(asm, group_start, group_len) > 0
                && (2..=MAX_XOR_GROUP).contains(&group_len)
            {
                // Two or more missing: wait for a late source. A complete
                // group (nothing missing) has nothing to wait for.
                asm.parities.insert(group_start, (group_len, xor.to_vec()));
            }
            self.finish_frame_if_complete(fh.frame_seq, now_us, events);
            return;
        }
        if fh.frag_idx >= fh.frag_total {
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
                parities: HashMap::new(),
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
            // A parity that arrived earlier may now have exactly one missing.
            for g in fi.saturating_sub(MAX_XOR_GROUP as usize - 1)..=fi {
                let Ok(g16) = u16::try_from(g) else { break };
                let Some(&(l, _)) = asm.parities.get(&g16) else {
                    continue;
                };
                if fi >= g + l as usize {
                    continue;
                }
                let Some((_, xor)) = asm.parities.remove(&g16) else {
                    continue;
                };
                if recover_group(asm, g16, l, &xor).is_some() {
                    self.h264_fragments_recovered += 1;
                } else if group_missing(asm, g16, l) > 0 {
                    asm.parities.insert(g16, (l, xor));
                }
            }
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
        if !fh.is_keyframe() || fh.is_parity() {
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
            // A keyframe at or before a frame already delivered would make the
            // decoder jump back (or decode the same picture twice): it is
            // moot, drop it.
            if asm.header.is_keyframe()
                && self
                    .last_delivered_frame_seq
                    .is_some_and(|last| frame_seq <= last)
            {
                return;
            }
            if asm.header.is_keyframe() {
                self.awaiting_idr = false;
            } else {
                // A P frame names the frame it predicts from. If that frame
                // was never delivered (it was lost, or dropped as stale), the
                // decoder has no reference for this one and every one after
                // it: a 3 s GOP would leave the picture broken for seconds
                // with nothing to say so. Do not hand such frames over; ask
                // for an IDR (paced), and hold P frames until it arrives.
                let reference = asm.header.ref_frame_seq;
                let broken =
                    reference != REF_FRAME_NONE && self.last_delivered_frame_seq != Some(reference);
                if self.awaiting_idr || broken {
                    self.awaiting_idr = true;
                    let due = self
                        .last_ref_reset_us
                        .is_none_or(|t| now_us.saturating_sub(t) >= REF_RESET_MIN_INTERVAL_US);
                    if due {
                        self.last_ref_reset_us = Some(now_us);
                        self.request_reset(ResetReason::GateReset, frame_seq);
                    }
                    return;
                }
            }
            self.last_delivered_frame_seq = Some(
                self.last_delivered_frame_seq
                    .map_or(frame_seq, |last| last.max(frame_seq)),
            );
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
                payload: out,
            });
        }
    }
}
