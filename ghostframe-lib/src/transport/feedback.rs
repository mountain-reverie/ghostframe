pub const FEEDBACK_MSG_TYPE: u8 = 0x01;
pub const FEEDBACK_SIZE: usize = 22;

/// Loss / suspension report sent from the receiver back to the sender.
///
/// Wire format (22 bytes, all big-endian):
///   [0]      message_type  = 0x01
///   [1..9]   timestamp_ns  u64
///   [9..13]  datagrams_received  u32
///   [13..17] datagrams_lost      u32
///   [17..21] datagrams_recovered_fec  u32
///   [21]     flags  u8  (bit 0 = suspension_detected)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverFeedback {
    pub timestamp_ns: u64,
    pub datagrams_received: u32,
    pub datagrams_lost: u32,
    pub datagrams_recovered_fec: u32,
    pub suspension_detected: bool,
}

impl ReceiverFeedback {
    /// Encode into `buf` (appends exactly [`FEEDBACK_SIZE`] bytes).
    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(FEEDBACK_MSG_TYPE);
        buf.extend_from_slice(&self.timestamp_ns.to_be_bytes());
        buf.extend_from_slice(&self.datagrams_received.to_be_bytes());
        buf.extend_from_slice(&self.datagrams_lost.to_be_bytes());
        buf.extend_from_slice(&self.datagrams_recovered_fec.to_be_bytes());
        let flags: u8 = if self.suspension_detected { 0x01 } else { 0x00 };
        buf.push(flags);
    }

    /// Decode from a byte slice. Returns `None` if the slice is too short or
    /// the message-type byte does not match [`FEEDBACK_MSG_TYPE`].
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < FEEDBACK_SIZE {
            return None;
        }
        if data[0] != FEEDBACK_MSG_TYPE {
            return None;
        }
        let timestamp_ns = u64::from_be_bytes(data[1..9].try_into().ok()?);
        let datagrams_received = u32::from_be_bytes(data[9..13].try_into().ok()?);
        let datagrams_lost = u32::from_be_bytes(data[13..17].try_into().ok()?);
        let datagrams_recovered_fec = u32::from_be_bytes(data[17..21].try_into().ok()?);
        let flags = data[21];
        let suspension_detected = (flags & 0x01) != 0;
        Some(Self {
            timestamp_ns,
            datagrams_received,
            datagrams_lost,
            datagrams_recovered_fec,
            suspension_detected,
        })
    }

    /// Fraction of datagrams lost: `lost / (received + lost)`.
    /// Returns `0.0` when no datagrams have been observed.
    pub fn loss_rate(&self) -> f64 {
        let total = self.datagrams_received as u64 + self.datagrams_lost as u64;
        if total == 0 {
            return 0.0;
        }
        self.datagrams_lost as f64 / total as f64
    }

    /// Average of per-window `loss_rate()` across `history`. Returns `0.0`
    /// for empty history. Caller is responsible for capping `history` at
    /// the smoothing window length (M3.6 uses 5).
    ///
    /// **Weighting**: this is an UNWEIGHTED mean. A window with 100
    /// datagrams contributes the same weight as a window with 10,000.
    /// Acceptable for fixed-cadence 100ms feedback (M3.6 default) where
    /// window sizes are roughly uniform. If feedback batching ever varies
    /// window sizes significantly, switch to weighting by total datagrams
    /// (`sum(lost) / sum(received + lost)`).
    pub fn smoothed_loss_rate(history: &std::collections::VecDeque<Self>) -> f32 {
        if history.is_empty() {
            return 0.0;
        }
        let sum: f64 = history.iter().map(|fb| fb.loss_rate()).sum();
        (sum / history.len() as f64) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let fb = ReceiverFeedback {
            timestamp_ns: 1_234_567_890_000,
            datagrams_received: 1000,
            datagrams_lost: 10,
            datagrams_recovered_fec: 3,
            suspension_detected: false,
        };
        let mut buf = Vec::new();
        fb.encode(&mut buf);
        assert_eq!(buf.len(), FEEDBACK_SIZE);
        let decoded = ReceiverFeedback::decode(&buf).expect("decode failed");
        assert_eq!(decoded, fb);
    }

    #[test]
    fn roundtrip_with_suspension() {
        let fb = ReceiverFeedback {
            timestamp_ns: 9_999_999_999_999,
            datagrams_received: 500,
            datagrams_lost: 5,
            datagrams_recovered_fec: 1,
            suspension_detected: true,
        };
        let mut buf = Vec::new();
        fb.encode(&mut buf);
        assert_eq!(buf.len(), FEEDBACK_SIZE);
        let decoded = ReceiverFeedback::decode(&buf).expect("decode failed");
        assert_eq!(decoded, fb);
        assert!(decoded.suspension_detected);
    }

    #[test]
    fn decode_too_short() {
        let data = vec![FEEDBACK_MSG_TYPE, 0x00, 0x01];
        assert!(ReceiverFeedback::decode(&data).is_none());
    }

    #[test]
    fn decode_wrong_type() {
        let fb = ReceiverFeedback {
            timestamp_ns: 0,
            datagrams_received: 0,
            datagrams_lost: 0,
            datagrams_recovered_fec: 0,
            suspension_detected: false,
        };
        let mut buf = Vec::new();
        fb.encode(&mut buf);
        buf[0] = 0x02; // wrong message type
        assert!(ReceiverFeedback::decode(&buf).is_none());
    }

    #[test]
    fn loss_rate_calculation() {
        let fb = ReceiverFeedback {
            timestamp_ns: 0,
            datagrams_received: 97,
            datagrams_lost: 3,
            datagrams_recovered_fec: 0,
            suspension_detected: false,
        };
        let rate = fb.loss_rate();
        // 3 / 100 = 0.03
        assert!((rate - 0.03).abs() < 1e-10, "expected ~0.03, got {rate}");
    }

    #[test]
    fn loss_rate_zero_datagrams() {
        let fb = ReceiverFeedback {
            timestamp_ns: 0,
            datagrams_received: 0,
            datagrams_lost: 0,
            datagrams_recovered_fec: 0,
            suspension_detected: false,
        };
        assert_eq!(fb.loss_rate(), 0.0);
    }

    #[test]
    fn smoothed_loss_rate_5_window_average() {
        use std::collections::VecDeque;
        let mut history: VecDeque<ReceiverFeedback> = VecDeque::new();
        for lost in [0u32, 0, 200, 0, 0] {
            history.push_back(ReceiverFeedback {
                timestamp_ns: 0,
                datagrams_received: 800,
                datagrams_lost: lost,
                datagrams_recovered_fec: 0,
                suspension_detected: false,
            });
        }
        let avg = ReceiverFeedback::smoothed_loss_rate(&history);
        // Window 3: loss = 200 / (800 + 200) = 0.20. Others: 0.0.
        // Mean: 0.20 / 5 = 0.04.
        assert!((avg - 0.04).abs() < 1e-6, "got {avg}");
    }

    #[test]
    fn smoothed_loss_rate_empty_history_is_zero() {
        use std::collections::VecDeque;
        let history: VecDeque<ReceiverFeedback> = VecDeque::new();
        assert_eq!(ReceiverFeedback::smoothed_loss_rate(&history), 0.0);
    }
}
