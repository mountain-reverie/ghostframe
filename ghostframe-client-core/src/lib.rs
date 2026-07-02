mod event;
pub mod ack_batcher;
pub use event::{DecodeErrorCode, Event, PollOutput, TileKey};

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy)]
pub struct ClientConfig {
    pub indices_raw_enabled: bool,
    pub supports_h264: bool,
}

pub struct ClientCore {
    outbox: VecDeque<PollOutput>,
}

impl ClientCore {
    pub fn new(config: ClientConfig, _now_us: u64) -> Self {
        let mut core = ClientCore {
            outbox: VecDeque::new(),
        };

        // Construct the Hello message [0x03, caps]
        // bit0 = indices_raw_enabled, bit1 = supports_h264
        let mut caps = 0u8;
        if config.indices_raw_enabled {
            caps |= 0x01;
        }
        if config.supports_h264 {
            caps |= 0x02;
        }

        core.outbox.push_back(PollOutput::Stream(vec![0x03, caps]));

        core
    }

    /// Feed one received datagram. Returns decode/render events.
    pub fn handle_datagram(&mut self, _bytes: &[u8], _now_us: u64) -> Vec<Event> {
        Vec::new()
    }

    /// Drain one pending outbound message; call until None.
    pub fn poll_transmit(&mut self, _now_us: u64) -> Option<PollOutput> {
        self.outbox.pop_front()
    }

    /// Earliest deadline (µs) at which on_timeout must be called, if any.
    pub fn poll_timeout(&self) -> Option<u64> {
        None
    }

    pub fn on_timeout(&mut self, _now_us: u64) -> Vec<Event> {
        Vec::new()
    }
}
