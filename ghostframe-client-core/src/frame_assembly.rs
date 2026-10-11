//! H.264 full-frame fragment reassembly.
//!
//! Port of the frame-level datagram branch in `main.ts` (~1326-1399): a
//! separate reassembly track from the per-tile pipeline, keyed by
//! `frame_seq` alone (there is exactly one in-flight H.264 access unit
//! per `frame_seq`). Uses `FrameHeader` (14 bytes) instead of `TileHeader`.
//!
//! Safety: nothing here may panic on malformed/truncated/hostile input;
//! all arithmetic on sequence numbers saturates; the assembly map is
//! bounded by the same `latest - 2` eviction window used by the tile
//! pipeline, so it cannot grow unboundedly under a flood of distinct
//! `frame_seq` values.

use ghostframe_protocol::protocol::{decode_frame_datagram, FrameHeader};

use crate::event::Event;
use crate::{ClientCore, MAX_FRAME_FRAGMENTS, MAX_H264_ACCESS_UNIT_BYTES};

/// One in-progress full-frame (H.264 access unit) assembly.
pub(crate) struct FrameAssembly {
    pub header: FrameHeader,
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
        _now_us: u64,
        events: &mut Vec<Event>,
    ) {
        let (fh, payload) = match decode_frame_datagram(bytes) {
            Ok(x) => x,
            Err(_) => return,
        };

        // Drop datagrams for frames older than the eviction window.
        if fh.frame_seq < self.latest_full_frame_seq.saturating_sub(2) {
            return;
        }
        if fh.frame_seq > self.latest_full_frame_seq {
            self.latest_full_frame_seq = fh.frame_seq;
        }

        // Evict in-progress assemblies older than latest - 2.
        let threshold = self.latest_full_frame_seq.saturating_sub(2);
        let stale: Vec<u32> = self
            .frame_assemblies
            .keys()
            .filter(|&&seq| seq < threshold)
            .copied()
            .collect();
        for seq in stale {
            self.frame_assemblies.remove(&seq);
        }

        // Skip parity fragments: this crate does not do FEC recovery for
        // H.264 frames, just drops parity (frag_idx >= frag_total).
        if fh.is_parity() {
            return;
        }
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

        let asm = self
            .frame_assemblies
            .entry(fh.frame_seq)
            .or_insert_with(|| FrameAssembly {
                header: fh,
                fragments: vec![None; fh.frag_total as usize],
                received: 0,
                bytes: 0,
            });

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

        if asm.received == asm.fragments.len() {
            let asm = match self.frame_assemblies.remove(&fh.frame_seq) {
                Some(a) => a,
                None => return,
            };
            let mut out = Vec::new();
            for b in asm.fragments.iter().flatten() {
                out.extend_from_slice(b);
            }
            events.push(Event::NeedsH264 {
                frame_seq: asm.header.frame_seq,
                timestamp_us: asm.header.timestamp_us,
                is_keyframe: asm.header.is_keyframe(),
                payload: out,
            });
        }
    }
}
