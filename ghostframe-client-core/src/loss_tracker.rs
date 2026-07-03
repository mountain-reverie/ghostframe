//! Loss/suspension tracking and Hello/ReceiverFeedback wire encoders. Ports
//! `ghostframe-web-client/src/feedback.ts`, but time is injected
//! (`now_us: u64`) instead of using `performance.now()`.

use ghostframe_protocol::feedback::ReceiverFeedback;

/// Suspension threshold: 100 ms in microseconds (mirrors the `100` in
/// feedback.ts `onDatagram`).
const SUSPENSION_GAP_US: u64 = 100_000;

pub struct LossTracker {
    received: u32,
    lost: u32,
    recovered_fec: u32,
    last_datagram_us: u64,
    suspension: bool,
}

impl LossTracker {
    pub fn new() -> Self {
        LossTracker {
            received: 0,
            lost: 0,
            recovered_fec: 0,
            last_datagram_us: 0,
            suspension: false,
        }
    }

    /// Call for every received datagram (source or parity). Sets
    /// `suspension_detected` if the gap since the previous datagram exceeds
    /// 100 ms.
    pub fn on_datagram(&mut self, now_us: u64) {
        if self.last_datagram_us > 0
            && now_us.saturating_sub(self.last_datagram_us) > SUSPENSION_GAP_US
        {
            self.suspension = true;
        }
        self.last_datagram_us = now_us;
        self.received += 1;
    }

    /// Call when a stale assembly is evicted with missing fragments.
    pub fn on_stale_tile(&mut self, expected: usize, received: usize) {
        if expected > received {
            self.lost += (expected - received) as u32;
        }
    }

    /// Call when a fragment is recovered via FEC.
    pub fn on_fec_recovery(&mut self) {
        self.recovered_fec += 1;
    }

    /// Encode a 22-byte, big-endian `ReceiverFeedback` message and reset
    /// counters. `timestamp_ns = now_us * 1000`.
    pub fn encode_feedback(&mut self, now_us: u64) -> Vec<u8> {
        let fb = ReceiverFeedback {
            timestamp_ns: now_us * 1000,
            datagrams_received: self.received,
            datagrams_lost: self.lost,
            datagrams_recovered_fec: self.recovered_fec,
            suspension_detected: self.suspension,
        };
        let mut buf = Vec::new();
        fb.encode(&mut buf);

        self.received = 0;
        self.lost = 0;
        self.recovered_fec = 0;
        self.suspension = false;

        buf
    }
}

impl Default for LossTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Encode a Hello capability-advertisement message: `[0x03, caps]`.
/// bit0 = indices_raw, bit1 = h264.
pub fn encode_hello(indices_raw: bool, supports_h264: bool) -> Vec<u8> {
    let mut caps = 0u8;
    if indices_raw {
        caps |= 0x01;
    }
    if supports_h264 {
        caps |= 0x02;
    }
    vec![0x03, caps]
}
