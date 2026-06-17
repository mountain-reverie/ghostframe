//! Output queue that interleaves source datagrams with FEC parity
//! datagrams offset by PARITY_INTERLEAVE_OFFSET wire_seqs behind the
//! group's first source — so the parity is well-separated from its
//! sources in the kernel UDP write window.

use crate::transport::reliable_emitter::{
    PARITY_INTERLEAVE_OFFSET, END_OF_STREAM_PARITY_FLUSH_MS,
};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum Emission {
    Source(Vec<u8>),
    Parity(Vec<u8>),
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

impl EmissionQueue {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            pending_parity: BinaryHeap::new(),
            end_of_stream_idle_since: None,
        }
    }

    pub fn push_source(&mut self, bytes: Vec<u8>) {
        self.queue.push_back(Emission::Source(bytes));
        self.end_of_stream_idle_since = None;
    }

    /// Schedule a parity datagram to be emitted when the upstream allocator
    /// has advanced to `emit_after_wire_seq`.
    pub fn schedule_parity(&mut self, emit_after_wire_seq: u32, bytes: Vec<u8>) {
        self.pending_parity.push(Reverse((emit_after_wire_seq, bytes)));
    }

    /// Pop the next emission given the next wire_seq the allocator will hand
    /// out for a new source and the current time.
    pub fn pop(&mut self, next_wire_seq: u32, now: Instant) -> Option<Emission> {
        // Promote any parity whose emit_after_wire_seq is reached.
        while let Some(Reverse((after, _))) = self.pending_parity.peek() {
            if *after > next_wire_seq { break; }
            let Reverse((_, bytes)) = self.pending_parity.pop().unwrap();
            self.queue.push_back(Emission::Parity(bytes));
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
        let Some(idle_since) = self.end_of_stream_idle_since else { return };
        if now.duration_since(idle_since) < Duration::from_millis(END_OF_STREAM_PARITY_FLUSH_MS) {
            return;
        }
        // Flush every pending parity unconditionally.
        while let Some(Reverse((_, bytes))) = self.pending_parity.pop() {
            self.queue.push_back(Emission::Parity(bytes));
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

    fn epoch() -> Instant {
        // A fixed Instant we can deterministically compare against.
        Instant::now()
    }

    #[test]
    fn source_only_pops_in_order() {
        let mut q = EmissionQueue::new();
        let t = epoch();
        q.push_source(vec![1]);
        q.push_source(vec![2]);
        matches!(q.pop(2, t), Some(Emission::Source(_)));
        matches!(q.pop(2, t), Some(Emission::Source(_)));
        assert!(q.pop(2, t).is_none());
    }

    #[test]
    fn parity_emits_after_offset_wire_seq() {
        let mut q = EmissionQueue::new();
        let t = epoch();
        q.push_source(vec![0]);                    // wire_seq 0
        q.schedule_parity(PARITY_INTERLEAVE_OFFSET, vec![0xAA]);  // emit after wire_seq 20
        // next_wire_seq = 1 (allocator about to hand out 1): parity not yet
        let e = q.pop(1, t).unwrap();
        matches!(e, Emission::Source(_));
        // Still no parity at next=5
        assert!(matches!(q.pop(5, t), None | Some(Emission::Source(_))));
        // At next=20 the parity becomes ready
        let e = q.pop(20, t).unwrap();
        matches!(e, Emission::Parity(b) if b == vec![0xAA]);
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
        matches!(e, Emission::Parity(_));
        assert!(q.is_empty());
    }
}
