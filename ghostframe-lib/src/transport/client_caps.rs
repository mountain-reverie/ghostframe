//! HELLO message (msg_type = 0x03): client → server one-shot
//! capability advertisement on the reliable bidi FEEDBACK stream.
//!
//! Wire format (2 bytes):
//!   [0]  msg_type = 0x03
//!   [1]  capabilities : u8   bit 0 = supports indices_raw (PalRle flags bit 1)
//!                            bit 1 = supports cdf53 (high-color tile codec)
//!                            bits 2..7 reserved (must be 0; deserializer ignores)

pub const HELLO_MSG_TYPE: u8 = 0x03;
pub const HELLO_SIZE: usize = 2;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientCapabilities {
    pub indices_raw_enabled: bool,
    pub supports_cdf53: bool,
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
                supports_cdf53:      (caps_byte & 0x02) != 0,
            },
        })
    }

    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(HELLO_MSG_TYPE);
        let mut caps_byte = 0u8;
        if self.caps.indices_raw_enabled { caps_byte |= 0x01; }
        if self.caps.supports_cdf53      { caps_byte |= 0x02; }
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
    }

    #[test]
    fn decode_no_caps() {
        let msg = HelloMsg::decode(&[HELLO_MSG_TYPE, 0x00]).unwrap();
        assert!(!msg.caps.indices_raw_enabled);
    }

    #[test]
    fn decode_ignores_reserved_bits() {
        // bits 2..7 set but bits 0..1 clear → both capabilities stay disabled.
        let msg = HelloMsg::decode(&[HELLO_MSG_TYPE, 0xFC]).unwrap();
        assert!(!msg.caps.indices_raw_enabled);
        assert!(!msg.caps.supports_cdf53);
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
    fn roundtrip() {
        let msg = HelloMsg { caps: ClientCapabilities { indices_raw_enabled: true, ..Default::default() } };
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        assert_eq!(buf, vec![HELLO_MSG_TYPE, 0x01]);
        assert_eq!(HelloMsg::decode(&buf), Some(msg));
    }

    #[test]
    fn decode_enables_supports_cdf53() {
        // Bit 1 set, bit 0 set: both capabilities enabled.
        let msg = HelloMsg::decode(&[HELLO_MSG_TYPE, 0b0000_0011]).unwrap();
        assert!(msg.caps.indices_raw_enabled);
        assert!(msg.caps.supports_cdf53);
    }

    #[test]
    fn decode_supports_cdf53_only() {
        // Bit 1 set, bit 0 clear: indices_raw off, cdf53 on.
        let msg = HelloMsg::decode(&[HELLO_MSG_TYPE, 0b0000_0010]).unwrap();
        assert!(!msg.caps.indices_raw_enabled);
        assert!(msg.caps.supports_cdf53);
    }

    #[test]
    fn decode_legacy_client_has_no_cdf53_support() {
        // M3.2-era client: only bit 0 set.
        let msg = HelloMsg::decode(&[HELLO_MSG_TYPE, 0b0000_0001]).unwrap();
        assert!(msg.caps.indices_raw_enabled);
        assert!(!msg.caps.supports_cdf53);
    }

    #[test]
    fn encode_roundtrip_both_caps() {
        let msg = HelloMsg { caps: ClientCapabilities {
            indices_raw_enabled: true,
            supports_cdf53: true,
        }};
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        assert_eq!(buf, vec![HELLO_MSG_TYPE, 0b0000_0011]);
        assert_eq!(HelloMsg::decode(&buf), Some(msg));
    }
}
