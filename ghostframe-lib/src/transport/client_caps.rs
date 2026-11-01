//! HELLO message (msg_type = 0x03): client → server one-shot
//! capability advertisement on the reliable bidi FEEDBACK stream.
//!
//! Wire format (2 bytes):
//!   [0]  msg_type = 0x03
//!   [1]  capabilities : u8   bit 0 = supports indices_raw (PalRle flags bit 1)
//!                            bit 1 = supports H.264 render path (set means the
//!                                    client can decode the H.264 codec and
//!                                    sample the resulting GPUExternalTexture
//!                                    via the texture_external WGSL capability;
//!                                    Firefox WebGPU as of mid-2026 cannot,
//!                                    so it sends this bit clear)
//!                            bit 2 = sends the v2 H.264 feedback (reset requests
//!                                    0x09; frame acks 0x0A in later phases)
//!                            bits 3..7 reserved (must be 0; deserializer ignores)
//!
//! H.264 is enabled by bit 1 alone. Bit 2 is recorded for later phases.

use ghostframe_protocol::feedback_stream::{
    HELLO_CAP_H264_ACKED_REF, HELLO_CAP_INDICES_RAW, HELLO_CAP_SUPPORTS_H264,
};
pub use ghostframe_protocol::feedback_stream::{HELLO_MSG_TYPE, HELLO_SIZE};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientCapabilities {
    pub indices_raw_enabled: bool,
    /// `true` if the client positively asserts it can decode and render
    /// H.264 frames. Default `false` is conservative: until HELLO arrives,
    /// the server stays in TileCodec mode so a Firefox client that connects
    /// without ever sending a HELLO never receives unplayable H.264.
    pub supports_h264: bool,
    /// `true` if the client sends the v2 H.264 feedback (HELLO bit 2): reset
    /// requests now, frame acks later. Recorded for later phases; it does not
    /// gate H.264 -- `supports_h264` does.
    pub h264_acked_ref: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelloMsg {
    pub caps: ClientCapabilities,
}

impl HelloMsg {
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < HELLO_SIZE {
            return None;
        }
        if data[0] != HELLO_MSG_TYPE {
            return None;
        }
        let caps_byte = data[1];
        Some(Self {
            caps: ClientCapabilities {
                indices_raw_enabled: (caps_byte & HELLO_CAP_INDICES_RAW) != 0,
                supports_h264: (caps_byte & HELLO_CAP_SUPPORTS_H264) != 0,
                h264_acked_ref: (caps_byte & HELLO_CAP_H264_ACKED_REF) != 0,
            },
        })
    }

    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(HELLO_MSG_TYPE);
        let mut caps_byte = 0u8;
        if self.caps.indices_raw_enabled {
            caps_byte |= HELLO_CAP_INDICES_RAW;
        }
        if self.caps.supports_h264 {
            caps_byte |= HELLO_CAP_SUPPORTS_H264;
        }
        if self.caps.h264_acked_ref {
            caps_byte |= HELLO_CAP_H264_ACKED_REF;
        }
        buf.push(caps_byte);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_enables_indices_raw() {
        let msg = HelloMsg::decode(&[HELLO_MSG_TYPE, 0x01]).unwrap();
        assert!(msg.caps.indices_raw_enabled);
        assert!(!msg.caps.supports_h264);
    }

    #[test]
    fn decode_enables_h264() {
        let msg = HelloMsg::decode(&[HELLO_MSG_TYPE, 0x02]).unwrap();
        assert!(!msg.caps.indices_raw_enabled);
        assert!(msg.caps.supports_h264);
    }

    #[test]
    fn decode_enables_both() {
        let msg = HelloMsg::decode(&[HELLO_MSG_TYPE, 0x03]).unwrap();
        assert!(msg.caps.indices_raw_enabled);
        assert!(msg.caps.supports_h264);
    }

    #[test]
    fn decode_no_caps() {
        let msg = HelloMsg::decode(&[HELLO_MSG_TYPE, 0x00]).unwrap();
        assert!(!msg.caps.indices_raw_enabled);
        assert!(!msg.caps.supports_h264);
    }

    #[test]
    fn decode_ignores_reserved_bits() {
        // Bits 3..7 set, bits 0..2 clear → every cap stays disabled.
        let msg = HelloMsg::decode(&[HELLO_MSG_TYPE, 0xF8]).unwrap();
        assert!(!msg.caps.indices_raw_enabled);
        assert!(!msg.caps.supports_h264);
        assert!(!msg.caps.h264_acked_ref);
    }

    #[test]
    fn decode_too_short() {
        assert!(HelloMsg::decode(&[]).is_none());
        assert!(HelloMsg::decode(&[HELLO_MSG_TYPE]).is_none());
    }

    #[test]
    fn decode_wrong_msg_type() {
        assert!(HelloMsg::decode(&[0x01, 0x01]).is_none());
    }

    #[test]
    fn roundtrip_indices_raw_only() {
        let msg = HelloMsg {
            caps: ClientCapabilities {
                indices_raw_enabled: true,
                supports_h264: false,
                h264_acked_ref: false,
            },
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        assert_eq!(buf, vec![HELLO_MSG_TYPE, 0x01]);
        assert_eq!(HelloMsg::decode(&buf), Some(msg));
    }

    #[test]
    fn roundtrip_h264_only() {
        let msg = HelloMsg {
            caps: ClientCapabilities {
                indices_raw_enabled: false,
                supports_h264: true,
                h264_acked_ref: false,
            },
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        assert_eq!(buf, vec![HELLO_MSG_TYPE, 0x02]);
        assert_eq!(HelloMsg::decode(&buf), Some(msg));
    }

    #[test]
    fn roundtrip_both() {
        let msg = HelloMsg {
            caps: ClientCapabilities {
                indices_raw_enabled: true,
                supports_h264: true,
                h264_acked_ref: false,
            },
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        assert_eq!(buf, vec![HELLO_MSG_TYPE, 0x03]);
        assert_eq!(HelloMsg::decode(&buf), Some(msg));
    }

    #[test]
    fn h264_is_gated_by_bit_1_alone() {
        for (byte, expect_h264, expect_feedback) in [
            (0x00u8, false, false),
            (0x01, false, false),
            (0x02, true, false), // as on master: a decoder is all it takes
            (0x04, false, true), // feedback without a decoder: no H.264
            (0x06, true, true),
            (0x07, true, true),
        ] {
            let msg = HelloMsg::decode(&[HELLO_MSG_TYPE, byte]).unwrap();
            assert_eq!(msg.caps.supports_h264, expect_h264, "caps byte {byte:#04x}");
            assert_eq!(
                msg.caps.h264_acked_ref, expect_feedback,
                "caps byte {byte:#04x}"
            );
        }
    }

    #[test]
    fn roundtrip_h264_with_acked_ref() {
        let msg = HelloMsg {
            caps: ClientCapabilities {
                indices_raw_enabled: true,
                supports_h264: true,
                h264_acked_ref: true,
            },
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        assert_eq!(buf, vec![HELLO_MSG_TYPE, 0x07]);
        assert_eq!(HelloMsg::decode(&buf), Some(msg));
    }
}
