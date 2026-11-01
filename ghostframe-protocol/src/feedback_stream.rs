//! The client -> server feedback stream's message-type namespace, in one
//! place.
//!
//! Every message on the reliable bidirectional feedback stream starts with a
//! one-byte type. Those bytes used to be defined independently in
//! `ghostframe-lib` (server) and `ghostframe-client-core` (client), so a new
//! type added on one side could collide with an existing one on the other
//! and nothing would notice. Defining them here, once, makes a collision a
//! failing test in this file ([`tests::stream_message_types_are_distinct`]).
//!
//! Both crates re-export these names so existing paths keep working, and the
//! web client reads them through the wasm getters in
//! `ghostframe-client-wasm/src/constants.rs`.
//!
//! ## What is *not* in this namespace
//!
//! Datagram envelopes ([`crate::protocol::TILE_PARITY_ENVELOPE`] = 0x04,
//! [`crate::protocol::TILE_NACK_ENVELOPE`] = 0x05, and
//! [`crate::ack::ACK_BATCH_MSG_TYPE`] = 0x06) travel as QUIC datagrams, a
//! different channel, and are routed by a different dispatcher. Parity and
//! NACK already reuse byte values that mean something else here (0x04 and
//! 0x05); that is harmless only because the channels never share a
//! dispatcher. Never route a byte from one channel through the other's.
//! (The ACK batch moved 0x04 -> 0x06 for an entry-format change, skipping
//! 0x05 so it could not alias the NACK envelope.)

/// Receiver loss/suspension report (see [`crate::feedback::ReceiverFeedback`]).
pub const FEEDBACK_MSG_TYPE: u8 = 0x01;

/// One-shot capability advertisement, `[HELLO_MSG_TYPE, caps]`.
pub const HELLO_MSG_TYPE: u8 = 0x03;
/// Encoded length of a HELLO message.
pub const HELLO_SIZE: usize = 2;

/// Per-tile decode failure report.
pub const DECODE_ERROR_MSG_TYPE: u8 = 0x04;

/// Input event (pointer, wheel, key). A sub-kind byte follows.
pub const INPUT_MSG_TYPE: u8 = 0x05;

/// Client request for a fresh IDR, `[RESET_REQUEST_MSG_TYPE, reason,
/// frame_seq u32 LE]`; see [`ResetRequest`].
pub const RESET_REQUEST_MSG_TYPE: u8 = 0x09;
/// Encoded length of a reset request.
pub const RESET_REQUEST_SIZE: usize = 6;

/// Client display capability advertisement.
pub const DISPLAY_INFO_MSG_TYPE: u8 = 0x07;
/// Client display mode request.
pub const DISPLAY_MODE_MSG_TYPE: u8 = 0x08;

/// HELLO capability bit 0: the client can decode `indices_raw` PalRLE
/// payloads (PalRLE flags bit 1).
pub const HELLO_CAP_INDICES_RAW: u8 = 0x01;
/// HELLO capability bit 1: the client can decode H.264 and sample the
/// resulting `GPUExternalTexture` (`texture_external`). Firefox WebGPU
/// cannot, and sends this bit clear so the server never emits unplayable
/// H.264.
pub const HELLO_CAP_SUPPORTS_H264: u8 = 0x02;
/// HELLO capability bit 2: the client speaks the v2 H.264 feedback -- it sends
/// reset requests (stream 0x09) and, in later phases, frame acks (datagram
/// 0x0A). It does not gate whether H.264 is sent: that is
/// [`HELLO_CAP_SUPPORTS_H264`] alone. Every client reads the 22-byte v2 frame
/// header; this bit says only that the client also gives feedback.
pub const HELLO_CAP_H264_ACKED_REF: u8 = 0x04;
/// HELLO capability bits that must be zero today; the server ignores them.
pub const HELLO_CAPS_RESERVED_MASK: u8 = 0xF8;

/// Why a client asks for a new IDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ResetReason {
    /// The decoder returned an error.
    DecoderFailure = 1,
    /// The client's display resolution changed.
    Resolution = 2,
    /// The client has no usable decoder for the stream.
    NoDecoder = 3,
    /// The client's reference gate found no decodable reference.
    GateReset = 4,
}

impl ResetReason {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::DecoderFailure),
            2 => Some(Self::Resolution),
            3 => Some(Self::NoDecoder),
            4 => Some(Self::GateReset),
            _ => None,
        }
    }
}

/// `[0x09][reason u8][frame_seq u32 LE]`: the client asks for an IDR.
/// `frame_seq` is the frame the client was handling when it gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetRequest {
    pub reason: ResetReason,
    pub frame_seq: u32,
}

impl ResetRequest {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(RESET_REQUEST_MSG_TYPE);
        buf.push(self.reason as u8);
        buf.extend_from_slice(&self.frame_seq.to_le_bytes());
    }

    /// `None` for a short buffer, a wrong type byte or an unknown reason. The
    /// stream is length-framed by message type, so bytes past
    /// [`RESET_REQUEST_SIZE`] belong to the next message and are not examined.
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < RESET_REQUEST_SIZE || data[0] != RESET_REQUEST_MSG_TYPE {
            return None;
        }
        Some(Self {
            reason: ResetReason::from_u8(data[1])?,
            frame_seq: u32::from_le_bytes(data[2..6].try_into().ok()?),
        })
    }
}

/// Every feedback-stream message type, for exhaustiveness checks.
pub const FEEDBACK_STREAM_MSG_TYPES: [u8; 7] = [
    FEEDBACK_MSG_TYPE,
    HELLO_MSG_TYPE,
    DECODE_ERROR_MSG_TYPE,
    INPUT_MSG_TYPE,
    DISPLAY_INFO_MSG_TYPE,
    DISPLAY_MODE_MSG_TYPE,
    RESET_REQUEST_MSG_TYPE,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ack::ACK_BATCH_MSG_TYPE;

    #[test]
    fn stream_message_types_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for t in FEEDBACK_STREAM_MSG_TYPES {
            assert!(
                seen.insert(t),
                "feedback-stream type {t:#04x} defined twice"
            );
        }
        assert_eq!(seen.len(), FEEDBACK_STREAM_MSG_TYPES.len());
    }

    #[test]
    fn ack_batch_does_not_alias_any_stream_type() {
        // The ACK batch is a datagram, but keep its byte off the stream
        // namespace anyway so a dump of either channel is unambiguous.
        assert!(!FEEDBACK_STREAM_MSG_TYPES.contains(&ACK_BATCH_MSG_TYPE));
    }

    #[test]
    fn hello_cap_bits_are_disjoint_single_bits() {
        let all = [
            HELLO_CAP_INDICES_RAW,
            HELLO_CAP_SUPPORTS_H264,
            HELLO_CAP_H264_ACKED_REF,
        ];
        let mut union = 0u8;
        for bit in all {
            assert_eq!(bit.count_ones(), 1);
            assert_eq!(union & bit, 0, "{bit:#04x} overlaps an earlier cap");
            union |= bit;
        }
        assert_eq!(HELLO_CAPS_RESERVED_MASK & union, 0);
        assert_eq!(HELLO_CAPS_RESERVED_MASK | union, 0xFF);
    }

    #[test]
    fn reset_request_roundtrip_and_layout() {
        for reason in [
            ResetReason::DecoderFailure,
            ResetReason::Resolution,
            ResetReason::NoDecoder,
            ResetReason::GateReset,
        ] {
            let r = ResetRequest {
                reason,
                frame_seq: 0x0102_0304,
            };
            let mut buf = Vec::new();
            r.encode(&mut buf);
            assert_eq!(buf.len(), RESET_REQUEST_SIZE);
            assert_eq!(buf[0], 0x09);
            assert_eq!(&buf[2..], &[4, 3, 2, 1], "little-endian");
            assert_eq!(ResetRequest::decode(&buf), Some(r));
        }
    }

    #[test]
    fn reset_request_hostile_input() {
        let mut good = Vec::new();
        ResetRequest {
            reason: ResetReason::DecoderFailure,
            frame_seq: u32::MAX,
        }
        .encode(&mut good);
        for len in 0..RESET_REQUEST_SIZE {
            assert_eq!(ResetRequest::decode(&good[..len]), None, "len {len}");
        }
        // Reason 0 and every undefined reason are refused.
        for reason in [0u8, 5, 0x7F, 0xFF] {
            let mut bad = good.clone();
            bad[1] = reason;
            assert_eq!(ResetRequest::decode(&bad), None, "reason {reason}");
        }
        let mut wrong = good.clone();
        wrong[0] = DISPLAY_MODE_MSG_TYPE;
        assert_eq!(ResetRequest::decode(&wrong), None);
        // Bytes after the message are the next message's, not an error.
        let mut longer = good.clone();
        longer.extend_from_slice(&[0xAA; 32]);
        assert!(ResetRequest::decode(&longer).is_some());
    }
}
