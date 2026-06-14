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
//!                            bits 2..7 reserved (must be 0; deserializer ignores)

pub const HELLO_MSG_TYPE: u8 = 0x03;
pub const HELLO_SIZE: usize = 2;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientCapabilities {
    pub indices_raw_enabled: bool,
    /// `true` if the client positively asserts it can decode and render
    /// H.264 frames. Default `false` is conservative: until HELLO arrives,
    /// the server stays in TileCodec mode so a Firefox client that connects
    /// without ever sending a HELLO never receives unplayable H.264.
    pub supports_h264: bool,
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
                indices_raw_enabled: (caps_byte & 0x01) != 0,
                supports_h264: (caps_byte & 0x02) != 0,
            },
        })
    }

    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(HELLO_MSG_TYPE);
        let mut caps_byte = 0u8;
        if self.caps.indices_raw_enabled {
            caps_byte |= 0x01;
        }
        if self.caps.supports_h264 {
            caps_byte |= 0x02;
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
        // Bits 2..7 set, bits 0+1 clear → both caps stay disabled.
        let msg = HelloMsg::decode(&[HELLO_MSG_TYPE, 0xFC]).unwrap();
        assert!(!msg.caps.indices_raw_enabled);
        assert!(!msg.caps.supports_h264);
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
            },
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        assert_eq!(buf, vec![HELLO_MSG_TYPE, 0x03]);
        assert_eq!(HelloMsg::decode(&buf), Some(msg));
    }
}
