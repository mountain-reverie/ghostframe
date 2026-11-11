// SPDX-License-Identifier: AGPL-3.0-or-later
//! Output queue of datagrams waiting to be handed to the transport.

use bytes::Bytes;
use std::collections::VecDeque;

/// Highest `pass_idx` of the critical tier (passes 0-3); everything above is
/// refinement. The single definition behind `pass_tier` and [`WireClass`].
pub(crate) const CRITICAL_PASS_MAX: u8 = 3;

/// What a datagram handed to the transport is for. Measurement only: it
/// labels bytes for the offered-load accounting and decides nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WireClass {
    /// A first transmission of a critical-tier tile pass (passes 0-3).
    Critical,
    /// A first transmission of a refinement-tier tile pass (passes 4-13).
    Refinement,
    /// A retransmission the client asked for.
    NackRepair,
    /// A retransmission the emitter's RTO timer fired.
    LossRetransmit,
    /// The scheduler re-sent a pass it had in flight and nothing else was
    /// holding (`Scheduler::drain_priority_queue`'s fallback retry). It goes
    /// through `submit_one` like a first send, so it is labelled here.
    SchedulerRetry,
    /// H.264 frame fragments.
    H264,
    /// Datagrams that are no tile pass: frame dimensions, pongs, eviction.
    Control,
}

impl WireClass {
    pub(crate) const COUNT: usize = 7;
    #[cfg(test)]
    pub(crate) const ALL: [WireClass; Self::COUNT] = [
        WireClass::Critical,
        WireClass::Refinement,
        WireClass::NackRepair,
        WireClass::LossRetransmit,
        WireClass::SchedulerRetry,
        WireClass::H264,
        WireClass::Control,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    /// The class of a pass's first transmission.
    pub fn for_first_send(pass_idx: u8) -> Self {
        if pass_idx <= CRITICAL_PASS_MAX {
            WireClass::Critical
        } else {
            WireClass::Refinement
        }
    }
}

#[derive(Debug, Clone)]
pub enum Emission {
    /// A source datagram and the reason it is on the wire.
    Source(Bytes, WireClass),
}

impl Emission {
    /// The `wire_seq` a source datagram carries (stamped at bytes 8..12 by the
    /// emitter), which names the transmission in the ledger.
    pub fn wire_seq(&self) -> Option<u32> {
        match self {
            Emission::Source(b, _) if b.len() >= 12 => {
                let ws = u32::from_be_bytes([b[8], b[9], b[10], b[11]]);
                // `NO_ACK` marks a datagram the ledger does not track (an H.264
                // fragment sent without a ledger record): it names no transmission.
                (ws != crate::transport::protocol::NO_ACK).then_some(ws)
            }
            Emission::Source(..) => None,
        }
    }

    pub fn class(&self) -> WireClass {
        match self {
            Emission::Source(_, c) => *c,
        }
    }
}

pub struct EmissionQueue {
    queue: VecDeque<Emission>,
    /// Bytes of the frame datagrams in `queue` (H.264 fragments and resends): the backlog the encoder's backpressure reads.
    frame_bytes: usize,
}

impl Default for EmissionQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `bytes` is an H.264 frame datagram rather than a tile datagram: a
/// tile datagram's first bit is set (`TILE_DATAGRAM_FLAG`), a frame's never is.
fn is_frame_datagram(bytes: &[u8]) -> bool {
    bytes.len() >= crate::transport::protocol::FRAME_HEADER_SIZE && bytes[0] & 0x80 == 0
}

fn frame_len(e: &Emission) -> usize {
    match e {
        Emission::Source(b, _) if is_frame_datagram(b) => b.len(),
        Emission::Source(..) => 0,
    }
}

impl EmissionQueue {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            frame_bytes: 0,
        }
    }

    /// Emissions waiting to be handed to the transport.
    ///
    /// A gauge, not a counter. Step A made the drain stop at the first
    /// refusal, which raised the question this measures: if the transport
    /// keeps refusing while the scheduler keeps submitting, does the backlog
    /// simply move from quinn's send buffer into this queue?
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Put a popped emission back at the head, undoing the pop.
    ///
    /// For the case where the transport refused the datagram. Safe precisely
    /// because a queued `Emission` is a byte blob that has not left yet:
    /// `submit_one` has already allocated the wire_seq, baked it into these
    /// bytes, inserted the retransmit-cache entry, so
    /// there is nothing to roll back and the transmission simply happens
    /// later under the sequence it already holds.
    ///
    /// Front, not back: the queue is in wire order, so this restores exactly
    /// the order that existed before the pop.
    pub fn push_front(&mut self, emission: Emission) {
        self.frame_bytes += frame_len(&emission);
        self.queue.push_front(emission);
    }

    pub fn push_source(&mut self, bytes: Bytes, class: WireClass) {
        let e = Emission::Source(bytes, class);
        self.frame_bytes += frame_len(&e);
        self.queue.push_back(e);
    }

    /// Pop the next emission.
    pub fn pop(&mut self) -> Option<Emission> {
        let popped = self.queue.pop_front();
        if let Some(e) = &popped {
            self.frame_bytes -= frame_len(e);
        }
        popped
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Bytes of H.264 frame datagrams waiting to be sent.
    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    /// Remove every queued frame datagram whose frame `doomed` names.
    /// Returns how many were removed and the `wire_seq`s of those that carried
    /// one.
    pub fn purge_frames(&mut self, doomed: impl Fn(u32) -> bool) -> (usize, Vec<u32>) {
        let mut wire_seqs = Vec::new();
        let mut removed = 0usize;
        let mut freed = 0usize;
        self.queue.retain(|e| {
            let Emission::Source(b, _) = e;
            if !is_frame_datagram(b) {
                return true;
            }
            let frame_seq = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
            if !doomed(frame_seq) {
                return true;
            }
            removed += 1;
            freed += b.len();
            if let Some(ws) = e.wire_seq() {
                wire_seqs.push(ws);
            }
            false
        });
        self.frame_bytes -= freed;
        (removed, wire_seqs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_only_pops_in_order() {
        let mut q = EmissionQueue::new();
        q.push_source(Bytes::from_static(&[1]), WireClass::Critical);
        q.push_source(Bytes::from_static(&[2]), WireClass::Critical);
        assert!(matches!(q.pop(), Some(Emission::Source(b, _)) if b[0] == 1));
        assert!(matches!(q.pop(), Some(Emission::Source(b, _)) if b[0] == 2));
        assert!(q.pop().is_none());
    }
}
