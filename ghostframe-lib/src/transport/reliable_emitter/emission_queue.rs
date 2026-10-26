//! Output queue that interleaves source datagrams with FEC parity
//! datagrams offset by PARITY_INTERLEAVE_OFFSET wire_seqs behind the
//! group's first source — so the parity is well-separated from its
//! sources in the kernel UDP write window.

use crate::transport::reliable_emitter::END_OF_STREAM_PARITY_FLUSH_MS;
use crate::transport::reliable_emitter::PARITY_INTERLEAVE_OFFSET;
use bytes::Bytes;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::time::{Duration, Instant};

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
    /// FEC parity (tile groups and H.264 frames).
    Parity,
    /// H.264 frame fragments.
    H264,
    /// Datagrams that are no tile pass: frame dimensions, pongs, eviction.
    Control,
}

impl WireClass {
    pub(crate) const COUNT: usize = 8;
    #[cfg(test)]
    pub(crate) const ALL: [WireClass; Self::COUNT] = [
        WireClass::Critical,
        WireClass::Refinement,
        WireClass::NackRepair,
        WireClass::LossRetransmit,
        WireClass::SchedulerRetry,
        WireClass::Parity,
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
    /// Parity datagram of the tile group starting at the given `wire_seq`.
    Parity(Bytes, u32),
}

impl Emission {
    /// For a tile parity datagram, the first `wire_seq` of the group it covers.
    pub fn parity_group(&self) -> Option<u32> {
        match self {
            Emission::Parity(_, g) => Some(*g),
            Emission::Source(..) => None,
        }
    }

    /// The `wire_seq` a source datagram carries (stamped at bytes 8..12 by the
    /// emitter), which names the transmission in the ledger. Parity is not a
    /// tracked transmission.
    pub fn wire_seq(&self) -> Option<u32> {
        match self {
            Emission::Source(b, _) if b.len() >= 12 => {
                Some(u32::from_be_bytes([b[8], b[9], b[10], b[11]]))
            }
            Emission::Source(..) | Emission::Parity(..) => None,
        }
    }

    pub fn class(&self) -> WireClass {
        match self {
            Emission::Source(_, c) => *c,
            Emission::Parity(..) => WireClass::Parity,
        }
    }
}

pub struct EmissionQueue {
    queue: VecDeque<Emission>,
    /// (Reverse(emit_after_wire_seq), parity_bytes). BinaryHeap is max-heap
    /// by default; Reverse turns it into a min-heap by emit_after.
    pending_parity: BinaryHeap<Reverse<(u32, Vec<u8>)>>,
    /// Set when the queue runs dry with parity still pending. After
    /// END_OF_STREAM_PARITY_FLUSH_MS the pending parity is force-flushed.
    end_of_stream_idle_since: Option<Instant>,
}

impl Default for EmissionQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl EmissionQueue {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            pending_parity: BinaryHeap::new(),
            end_of_stream_idle_since: None,
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
    /// bytes, inserted the retransmit-cache entry and fed the FEC group, so
    /// there is nothing to roll back and the transmission simply happens
    /// later under the sequence it already holds.
    ///
    /// Front, not back: the queue is in wire order, so this restores exactly
    /// the order that existed before the pop.
    pub fn push_front(&mut self, emission: Emission) {
        self.queue.push_front(emission);
        self.end_of_stream_idle_since = None;
    }

    pub fn push_source(&mut self, bytes: Bytes, class: WireClass) {
        self.queue.push_back(Emission::Source(bytes, class));
        self.end_of_stream_idle_since = None;
    }

    /// Schedule a parity datagram to be emitted when the upstream allocator
    /// has advanced to `emit_after_wire_seq`.
    pub fn schedule_parity(&mut self, emit_after_wire_seq: u32, bytes: Vec<u8>) {
        self.pending_parity
            .push(Reverse((emit_after_wire_seq, bytes)));
    }

    /// Pop the next emission given the next wire_seq the allocator will hand
    /// out for a new source and the current time.
    pub fn pop(&mut self, next_wire_seq: u32, now: Instant) -> Option<Emission> {
        // Promote any parity whose emit_after_wire_seq is reached.
        while let Some(Reverse((after, _))) = self.pending_parity.peek() {
            if *after > next_wire_seq {
                break;
            }
            let Reverse((after, bytes)) = self.pending_parity.pop().unwrap();
            self.queue.push_back(Emission::Parity(
                Bytes::from(bytes),
                after.wrapping_sub(PARITY_INTERLEAVE_OFFSET),
            ));
        }
        // Check end-of-stream flush.
        self.maybe_flush_pending(now);
        let popped = self.queue.pop_front();
        if popped.is_some() {
            self.end_of_stream_idle_since = None;
        } else if !self.pending_parity.is_empty() && self.end_of_stream_idle_since.is_none() {
            self.end_of_stream_idle_since = Some(now);
        }
        popped
    }

    fn maybe_flush_pending(&mut self, now: Instant) {
        let Some(idle_since) = self.end_of_stream_idle_since else {
            return;
        };
        if now.duration_since(idle_since) < Duration::from_millis(END_OF_STREAM_PARITY_FLUSH_MS) {
            return;
        }
        // Flush every pending parity unconditionally.
        while let Some(Reverse((after, bytes))) = self.pending_parity.pop() {
            self.queue.push_back(Emission::Parity(
                Bytes::from(bytes),
                after.wrapping_sub(PARITY_INTERLEAVE_OFFSET),
            ));
        }
        self.end_of_stream_idle_since = None;
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty() && self.pending_parity.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::reliable_emitter::PARITY_INTERLEAVE_OFFSET;

    fn epoch() -> Instant {
        // A fixed Instant we can deterministically compare against.
        Instant::now()
    }

    #[test]
    fn source_only_pops_in_order() {
        let mut q = EmissionQueue::new();
        let t = epoch();
        q.push_source(Bytes::from_static(&[1]), WireClass::Critical);
        q.push_source(Bytes::from_static(&[2]), WireClass::Critical);
        assert!(matches!(q.pop(2, t), Some(Emission::Source(..))));
        assert!(matches!(q.pop(2, t), Some(Emission::Source(..))));
        assert!(q.pop(2, t).is_none());
    }

    #[test]
    fn parity_emits_after_offset_wire_seq() {
        let mut q = EmissionQueue::new();
        let t = epoch();
        q.push_source(Bytes::from_static(&[0]), WireClass::Critical); // wire_seq 0
        q.schedule_parity(PARITY_INTERLEAVE_OFFSET, vec![0xAA]); // emit after wire_seq 20
                                                                 // next_wire_seq = 1 (allocator about to hand out 1): parity not yet
        let e = q.pop(1, t).unwrap();
        assert!(matches!(e, Emission::Source(..)));
        // Still no parity at next=5 — queue is now empty (source already popped above)
        assert!(q.pop(5, t).is_none());
        // At next=20 the parity becomes ready
        let e = q.pop(20, t).unwrap();
        assert!(matches!(e, Emission::Parity(b, 0) if b == vec![0xAA]));
    }

    #[test]
    fn end_of_stream_flush_releases_parity_after_idle() {
        let mut q = EmissionQueue::new();
        let t0 = Instant::now();
        q.schedule_parity(PARITY_INTERLEAVE_OFFSET, vec![0xBB]);
        // No sources to advance wire_seq → parity blocked normally.
        assert!(q.pop(0, t0).is_none());
        // Advance time past flush window
        let t1 = t0 + Duration::from_millis(END_OF_STREAM_PARITY_FLUSH_MS + 1);
        let e = q.pop(0, t1).expect("flushed");
        assert!(matches!(e, Emission::Parity(..)));
        assert!(q.is_empty());
    }
}
