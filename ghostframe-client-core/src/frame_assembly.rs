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

use ghostframe_protocol::fec::decode_parity_payload;
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
        // XOR parity (frag_idx == frag_total) is handled before anything that
        // moves `latest_full_frame_seq` or evicts: it carries no frame data of
        // its own and must never be able to advance the window and so drop
        // real frames. It only helps a frame that already has an assembly
        // (parity follows its sources); anything else is ignored.
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
            self.finish_frame_if_complete(fh.frame_seq, events);
            return;
        }
        if fh.frag_idx >= fh.frag_total {
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
                parities: HashMap::new(),
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
        self.finish_frame_if_complete(fh.frame_seq, events);
    }

    fn finish_frame_if_complete(&mut self, frame_seq: u32, events: &mut Vec<Event>) {
        let complete = self
            .frame_assemblies
            .get(&frame_seq)
            .is_some_and(|a| a.received == a.fragments.len());
        if complete {
            let asm = match self.frame_assemblies.remove(&frame_seq) {
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
                ref_frame_seq: asm.header.ref_frame_seq,
                flags: asm.header.flags,
                slots: asm.header.slots,
                payload: out,
            });
        }
    }
}
