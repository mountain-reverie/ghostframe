//! Injectable boundaries for unit/integration testing.

/// The emitter calls `send` for every datagram (source or parity) it
/// wants on the wire. Real code passes a wrapper around
/// `IoBridge::send_to_all_sessions`; tests pass a `Vec<Bytes>` collector
/// or a lossy mock.
pub trait DatagramSender {
    fn send(&mut self, dg: &[u8]);
}

#[cfg(test)]
pub mod testing {
    use super::*;

    #[derive(Default)]
    pub struct CollectSender {
        pub sent: Vec<Vec<u8>>,
    }
    impl DatagramSender for CollectSender {
        fn send(&mut self, dg: &[u8]) {
            self.sent.push(dg.to_vec());
        }
    }
}
