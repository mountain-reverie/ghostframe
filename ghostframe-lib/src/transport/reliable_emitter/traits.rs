//! Injectable boundaries for unit/integration testing.

/// The emitter calls `send` for every datagram (source or parity) it
/// wants on the wire. Real code passes a wrapper around
/// `IoBridge::send_to_all_sessions`; tests pass a `Vec<Bytes>` collector
/// or a lossy mock.
/// Whether a datagram handed to the transport actually left.
///
/// `Rejected` means quinn refused it -- in practice
/// `SendDatagramError::Blocked`, its datagram send buffer being full. That is
/// normal flow control, not an error: quinn's own high-level API provokes it
/// deliberately (`Connection::send_datagram_wait` sends, takes `Blocked`,
/// then waits on the `DatagramsUnblocked` event it produces).
///
/// It has to be reported because this trait is the single funnel every
/// datagram passes through -- source, parity and retransmit alike. While
/// `send` returned unit, a rejection was counted and the datagram discarded,
/// and by then the scheduler had already popped the work
/// (`drain_refinement_pass_major` removes rather than retaining), so the only
/// path back was the emitter's bounded RTO. Measured at production scale:
/// 4,726 rejections cost a third of delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    Rejected,
}

pub trait DatagramSender {
    /// Hand one datagram to the transport.
    ///
    /// Returning `Rejected` must be side-effect free from the caller's point
    /// of view: the emitter will re-queue the same bytes and try again, so an
    /// implementation must not have partially consumed them.
    fn send(&mut self, dg: &[u8]) -> SendOutcome;
}

#[cfg(test)]
pub mod testing {
    use super::*;

    #[derive(Default)]
    pub struct CollectSender {
        pub sent: Vec<Vec<u8>>,
    }
    impl DatagramSender for CollectSender {
        fn send(&mut self, dg: &[u8]) -> SendOutcome {
            self.sent.push(dg.to_vec());
            SendOutcome::Sent
        }
    }

    /// Accepts `accept_first` datagrams and rejects everything after, so a
    /// test can exercise the re-queue path without a real full send buffer.
    pub struct RejectAfterSender {
        pub sent: Vec<Vec<u8>>,
        pub rejected: usize,
        pub accept_first: usize,
    }

    impl RejectAfterSender {
        pub fn new(accept_first: usize) -> Self {
            Self {
                sent: Vec::new(),
                rejected: 0,
                accept_first,
            }
        }
    }

    impl DatagramSender for RejectAfterSender {
        fn send(&mut self, dg: &[u8]) -> SendOutcome {
            if self.sent.len() < self.accept_first {
                self.sent.push(dg.to_vec());
                SendOutcome::Sent
            } else {
                self.rejected += 1;
                SendOutcome::Rejected
            }
        }
    }
}
