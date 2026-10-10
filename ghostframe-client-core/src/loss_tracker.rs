//! Loss/suspension tracking and Hello/ReceiverFeedback wire encoders. Ports
//! `ghostframe-web-client/src/feedback.ts`, but time is injected
//! (`now_us: u64`) instead of using `performance.now()`.

use ghostframe_protocol::feedback::ReceiverFeedback;

/// Suspension threshold: how much longer a datagram may take to arrive
/// after its predecessor than the sender spaced them, before the difference
/// is called a stall in the path. 100 ms, from the initial spec's WiFi
/// suspension detector.
const SUSPENSION_GAP_US: u64 = 100_000;

/// Client capability announcement, sent once on the feedback stream at
/// construction. Wire layout `[HELLO_MSG_TYPE, caps]`, where caps bit0 =
/// indices_raw_enabled and bit1 = supports_h264.
pub const HELLO_MSG_TYPE: u8 = 0x03;

/// Encoded length of a Hello message.
pub const HELLO_SIZE: usize = 2;

pub struct LossTracker {
    received: u32,
    lost: u32,
    recovered_fec: u32,
    /// `(arrival, sender stamp)` of the newest timed datagram seen: the
    /// baseline the next one's spacing is judged against.
    last_timed: Option<(u64, u32)>,
    suspension: bool,
}

impl LossTracker {
    pub fn new() -> Self {
        LossTracker {
            received: 0,
            lost: 0,
            recovered_fec: 0,
            last_timed: None,
            suspension: false,
        }
    }

    /// Count a datagram whose timing says nothing about the path: a
    /// frame-dimensions or eviction sentinel, which bypass the emitter and
    /// are stamped on the capture clock instead, or one rebuilt from FEC
    /// parity, which was never delivered at all.
    ///
    /// Says nothing about suspension, because it cannot: see
    /// `on_datagram_sent_at`.
    pub fn on_datagram(&mut self, _now_us: u64) {
        self.received += 1;
    }

    /// Count a received datagram and judge whether the path stalled before
    /// delivering it.
    ///
    /// `sent_us` is the sender's emit stamp from the datagram header
    /// (microseconds on the sender's clock, wrapping at 2^32). A stall is an
    /// arrival gap that exceeds the *send* gap by more than
    /// `SUSPENSION_GAP_US`: the network sat on a datagram the sender had
    /// already let go of. That is the WiFi-suspension signature the flag
    /// exists to report -- a hole followed by a burst.
    ///
    /// This used to compare arrival times alone, so any 100 ms of quiet
    /// counted, and a sender with nothing to say is quiet. On a static
    /// screen the first datagram after the user touched anything arrived
    /// "after a suspension", and the server answers that flag by forcing
    /// H.264 with hysteresis bypassed. Measured: a 9.4 s idle screen, one
    /// change, an immediate `TileCodec -> H264 reason="suspension"`.
    ///
    /// It then could not leave. Only tile datagrams are counted here, and
    /// H.264 mode sends almost none, so each stray one arrived after a long
    /// gap and re-raised the flag; when the classifier did get out, the
    /// first tile datagram of the new mode arrived a full H.264 dwell after
    /// the last and sent it straight back. That is the `suspension` /
    /// `cost_comparison` flip every ~1.2 s seen on any changing content.
    ///
    /// A stamp older than the baseline is a datagram overtaken by newer
    /// traffic. It is counted and otherwise ignored: its lateness is its
    /// own, not the path's.
    ///
    /// Only for datagrams that came off the wire. One rebuilt from FEC
    /// parity has a stamp but no arrival, and the caller must not time it:
    /// see `on_datagram`.
    pub fn on_datagram_sent_at(&mut self, now_us: u64, sent_us: u32) {
        self.received += 1;
        let Some((prev_arrival, prev_sent)) = self.last_timed else {
            self.last_timed = Some((now_us, sent_us));
            return;
        };
        // Signed, so that the wrap at 2^32 us (71.6 min) reads as a small
        // forward step rather than a huge backward one.
        let send_gap = sent_us.wrapping_sub(prev_sent) as i32;
        if send_gap < 0 {
            return;
        }
        let arrival_gap = now_us.saturating_sub(prev_arrival);
        if arrival_gap > send_gap as u64 + SUSPENSION_GAP_US {
            self.suspension = true;
        }
        self.last_timed = Some((now_us, sent_us));
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
    vec![HELLO_MSG_TYPE, caps]
}

/// DisplayInfo message type. Mirrors `ghostframe_lib::transport::display`;
/// duplicated here because `ghostframe-client-core` must not depend on
/// `ghostframe-lib` (the server crate), so both sides are pinned together
/// only by the byte-exact oracle tests in `tests/oracle_feedback.rs`.
pub const DISPLAY_INFO_MSG_TYPE: u8 = 0x07;

/// DisplayMode message type.
pub const DISPLAY_MODE_MSG_TYPE: u8 = 0x08;

/// Encode a DisplayInfo message:
/// `[0x07][max_w:u16][max_h:u16][scale_milli:u16][mm_w:u16][mm_h:u16]`, all
/// big-endian. `scale_milli` is authoritative; `mm_width`/`mm_height` are
/// advisory only -- see [`crate::ClientDisplay`].
pub fn encode_display_info(
    max_width: u16,
    max_height: u16,
    scale_milli: u16,
    mm_width: u16,
    mm_height: u16,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(11);
    buf.push(DISPLAY_INFO_MSG_TYPE);
    buf.extend_from_slice(&max_width.to_be_bytes());
    buf.extend_from_slice(&max_height.to_be_bytes());
    buf.extend_from_slice(&scale_milli.to_be_bytes());
    buf.extend_from_slice(&mm_width.to_be_bytes());
    buf.extend_from_slice(&mm_height.to_be_bytes());
    buf
}

/// Encode a DisplayMode message: `[0x08][w:u16][h:u16]`, big-endian.
pub fn encode_display_mode(width: u16, height: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5);
    buf.push(DISPLAY_MODE_MSG_TYPE);
    buf.extend_from_slice(&width.to_be_bytes());
    buf.extend_from_slice(&height.to_be_bytes());
    buf
}
