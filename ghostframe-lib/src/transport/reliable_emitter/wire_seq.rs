//! Per-session monotonic wire_seq counter.

#[derive(Debug, Default, Clone, Copy)]
pub struct WireSeqAllocator {
    next: u32,
}

impl WireSeqAllocator {
    pub fn new() -> Self { Self { next: 0 } }
    pub fn peek(&self) -> u32 { self.next }
    pub fn allocate(&mut self) -> u32 {
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
        let mut a = WireSeqAllocator { next: u32::MAX };
        assert_eq!(a.allocate(), u32::MAX);
        assert_eq!(a.allocate(), 0);
        assert_eq!(a.allocate(), 1);
    }
}
