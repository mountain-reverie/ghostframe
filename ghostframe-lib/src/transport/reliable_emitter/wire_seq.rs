//! Per-session monotonic wire_seq counter.

use crate::transport::protocol::NO_ACK;

#[derive(Debug, Default, Clone, Copy)]
pub struct WireSeqAllocator {
    next: u32,
}

impl WireSeqAllocator {
    pub fn new() -> Self {
        Self { next: 0 }
    }
    pub fn peek(&self) -> u32 {
        if self.next == NO_ACK {
            0
        } else {
            self.next
        }
    }
    /// The next wire_seq. Never `NO_ACK` (0xFFFF_FFFF): that value on the
    /// wire means "not tracked", so a tracked transmission must not use it.
    pub fn allocate(&mut self) -> u32 {
        if self.next == NO_ACK {
            self.next = 0;
        }
        let v = self.next;
        self.next = self.next.wrapping_add(1);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_monotonically_from_zero() {
        let mut a = WireSeqAllocator::new();
        assert_eq!(a.allocate(), 0);
        assert_eq!(a.allocate(), 1);
        assert_eq!(a.allocate(), 2);
    }

    #[test]
    fn peek_does_not_advance() {
        let mut a = WireSeqAllocator::new();
        let _ = a.allocate();
        assert_eq!(a.peek(), 1);
        assert_eq!(a.peek(), 1);
        assert_eq!(a.allocate(), 1);
    }

    #[test]
    fn wraps_at_u32_max() {
        let mut a = WireSeqAllocator { next: u32::MAX - 1 };
        assert_eq!(a.allocate(), u32::MAX - 1);
        // u32::MAX is NO_ACK: skipped, the sequence wraps straight to 0.
        assert_eq!(a.allocate(), 0);
        assert_eq!(a.allocate(), 1);
    }

    #[test]
    fn never_allocates_no_ack() {
        let mut a = WireSeqAllocator { next: u32::MAX };
        assert_eq!(a.allocate(), 0, "a counter sitting on NO_ACK skips it");
        let mut a = WireSeqAllocator { next: u32::MAX - 2 };
        for _ in 0..8 {
            assert_ne!(a.allocate(), NO_ACK);
        }
    }
}
