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
/// HELLO capability bits that must be zero today; the server ignores them.
pub const HELLO_CAPS_RESERVED_MASK: u8 = 0xFC;

/// Every feedback-stream message type, for exhaustiveness checks.
pub const FEEDBACK_STREAM_MSG_TYPES: [u8; 6] = [
    FEEDBACK_MSG_TYPE,
    HELLO_MSG_TYPE,
    DECODE_ERROR_MSG_TYPE,
    INPUT_MSG_TYPE,
    DISPLAY_INFO_MSG_TYPE,
    DISPLAY_MODE_MSG_TYPE,
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
        assert_eq!(HELLO_CAP_INDICES_RAW.count_ones(), 1);
        assert_eq!(HELLO_CAP_SUPPORTS_H264.count_ones(), 1);
        assert_eq!(HELLO_CAP_INDICES_RAW & HELLO_CAP_SUPPORTS_H264, 0);
        assert_eq!(
            HELLO_CAPS_RESERVED_MASK & (HELLO_CAP_INDICES_RAW | HELLO_CAP_SUPPORTS_H264),
            0
        );
        assert_eq!(
            HELLO_CAPS_RESERVED_MASK | HELLO_CAP_INDICES_RAW | HELLO_CAP_SUPPORTS_H264,
            0xFF
        );
    }
}
