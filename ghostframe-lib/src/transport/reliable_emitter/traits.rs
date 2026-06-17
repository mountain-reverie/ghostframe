//! Injectable boundaries for unit/integration testing.

use std::time::Instant;

/// The emitter calls `send` for every datagram (source or parity) it
/// wants on the wire. Real code passes a wrapper around
/// `IoBridge::send_to_all_sessions`; tests pass a `Vec<Bytes>` collector
/// or a lossy mock.
pub trait DatagramSender {
    fn send(&mut self, dg: &[u8]);
}

/// Injectable monotonic clock. Real code uses `Instant::now()`; tests
/// advance a `MockClock` manually.
pub trait Clock {
    fn now(&self) -> Instant;
}

/// Wall-clock impl for production.
#[derive(Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant { Instant::now() }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    pub struct CollectSender {
        pub sent: Vec<Vec<u8>>,
    }
    impl DatagramSender for CollectSender {
        fn send(&mut self, dg: &[u8]) { self.sent.push(dg.to_vec()); }
    }

    #[derive(Clone)]
    pub struct MockClock {
        now: Rc<RefCell<Instant>>,
    }
    impl MockClock {
        pub fn new(start: Instant) -> Self { Self { now: Rc::new(RefCell::new(start)) } }
        pub fn advance(&self, dt: std::time::Duration) { *self.now.borrow_mut() += dt; }
    }
    impl Clock for MockClock {
        fn now(&self) -> Instant { *self.now.borrow() }
    }
}
