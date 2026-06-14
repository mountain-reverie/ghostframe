//! Browser → server input forwarding.
//!
//! New `INPUT_MSG_TYPE = 0x05` on the bidi feedback stream. Sub-kind byte
//! after the message-type selects pointer-move / pointer-button / wheel /
//! key-down / key-up. KeySyms (not scan codes) on the wire so layout
//! mismatches between browser host and guest desktop don't silently
//! corrupt characters.
//!
//! See `docs/superpowers/specs/2026-06-13-input-forwarding-design.md`.

/// Top-level feedback message type for input events. Routed by the first
/// byte in `IoBridge::dispatch_feedback_bytes`. Sub-kind byte at offset 1
/// selects the specific event (see `decode_input_msg`).
pub const INPUT_MSG_TYPE: u8 = 0x05;

const SUB_POINTER_MOVE: u8 = 0x01;
const SUB_POINTER_BUTTON: u8 = 0x02;
const SUB_WHEEL: u8 = 0x03;
const SUB_KEY_DOWN: u8 = 0x04;
const SUB_KEY_UP: u8 = 0x05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMsg {
    PointerMove { x: i16, y: i16 },
    PointerButton { x: i16, y: i16, button: u8, down: bool },
    Wheel { dx: i16, dy: i16 },
    KeyDown { keysym: u32 },
    KeyUp { keysym: u32 },
}

/// Abstract handle the I/O layer uses to inject events into a windowing
/// system. Production impl lives in `ghostframe-xdaemon` (X11 via
/// `x11rb`'s `xtest`); test bridges pass `None` and skip injection.
pub trait InputInjector: Send + Sync {
    fn pointer_move(&self, x: i16, y: i16);
    fn pointer_button(&self, x: i16, y: i16, button: u8, down: bool);
    fn wheel(&self, dx: i16, dy: i16);
    fn key(&self, keysym: u32, down: bool);
}

/// Returns `Some((msg, consumed_bytes))` on success, `None` on:
///   - empty input
///   - byte[0] != INPUT_MSG_TYPE
///   - unknown sub-kind (byte[1] not in {0x01..0x05})
///   - short buffer (insufficient bytes for the sub-kind)
///
/// All multi-byte fields are big-endian, matching FEEDBACK/HELLO/DECODE_ERROR.
pub fn decode_input_msg(data: &[u8]) -> Option<(InputMsg, usize)> {
    if data.len() < 2 || data[0] != INPUT_MSG_TYPE {
        return None;
    }
    match data[1] {
        SUB_POINTER_MOVE => {
            if data.len() < 6 {
                return None;
            }
            let x = i16::from_be_bytes([data[2], data[3]]);
            let y = i16::from_be_bytes([data[4], data[5]]);
            Some((InputMsg::PointerMove { x, y }, 6))
        }
        SUB_POINTER_BUTTON => {
            if data.len() < 8 {
                return None;
            }
            let x = i16::from_be_bytes([data[2], data[3]]);
            let y = i16::from_be_bytes([data[4], data[5]]);
            let button = data[6];
            let down = data[7] != 0;
            Some((
                InputMsg::PointerButton { x, y, button, down },
                8,
            ))
        }
        SUB_WHEEL => {
            if data.len() < 6 {
                return None;
            }
            let dx = i16::from_be_bytes([data[2], data[3]]);
            let dy = i16::from_be_bytes([data[4], data[5]]);
            Some((InputMsg::Wheel { dx, dy }, 6))
        }
        SUB_KEY_DOWN => {
            if data.len() < 6 {
                return None;
            }
            let keysym =
                u32::from_be_bytes([data[2], data[3], data[4], data[5]]);
            Some((InputMsg::KeyDown { keysym }, 6))
        }
        SUB_KEY_UP => {
            if data.len() < 6 {
                return None;
            }
            let keysym =
                u32::from_be_bytes([data[2], data[3], data[4], data[5]]);
            Some((InputMsg::KeyUp { keysym }, 6))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_pointer_move() {
        let bytes = [0x05, 0x01, 0x01, 0x90, 0xff, 0xfe]; // x=400, y=-2
        let (msg, consumed) = decode_input_msg(&bytes).unwrap();
        assert_eq!(consumed, 6);
        assert_eq!(msg, InputMsg::PointerMove { x: 400, y: -2 });
    }

    #[test]
    fn decode_pointer_button() {
        let bytes = [0x05, 0x02, 0x00, 0x05, 0x00, 0x0a, 0x01, 0x01];
        let (msg, consumed) = decode_input_msg(&bytes).unwrap();
        assert_eq!(consumed, 8);
        assert_eq!(
            msg,
            InputMsg::PointerButton { x: 5, y: 10, button: 1, down: true }
        );
    }

    #[test]
    fn decode_pointer_button_up() {
        let bytes = [0x05, 0x02, 0x00, 0x05, 0x00, 0x0a, 0x03, 0x00];
        let (msg, _) = decode_input_msg(&bytes).unwrap();
        assert_eq!(
            msg,
            InputMsg::PointerButton { x: 5, y: 10, button: 3, down: false }
        );
    }

    #[test]
    fn decode_wheel() {
        let bytes = [0x05, 0x03, 0x00, 0x00, 0x00, 0x01];
        let (msg, consumed) = decode_input_msg(&bytes).unwrap();
        assert_eq!(consumed, 6);
        assert_eq!(msg, InputMsg::Wheel { dx: 0, dy: 1 });
    }

    #[test]
    fn decode_key_down() {
        // 'a' = 0x61
        let bytes = [0x05, 0x04, 0x00, 0x00, 0x00, 0x61];
        let (msg, consumed) = decode_input_msg(&bytes).unwrap();
        assert_eq!(consumed, 6);
        assert_eq!(msg, InputMsg::KeyDown { keysym: 0x61 });
    }

    #[test]
    fn decode_key_up() {
        // XK_Return = 0xff0d
        let bytes = [0x05, 0x05, 0x00, 0x00, 0xff, 0x0d];
        let (msg, consumed) = decode_input_msg(&bytes).unwrap();
        assert_eq!(consumed, 6);
        assert_eq!(msg, InputMsg::KeyUp { keysym: 0xff0d });
    }

    #[test]
    fn decode_rejects_wrong_msg_type() {
        let bytes = [0x04, 0x01, 0, 0, 0, 0];
        assert_eq!(decode_input_msg(&bytes), None);
    }

    #[test]
    fn decode_rejects_unknown_subkind() {
        let bytes = [0x05, 0xfe, 0, 0, 0, 0];
        assert_eq!(decode_input_msg(&bytes), None);
    }

    #[test]
    fn decode_rejects_short_buffer() {
        assert_eq!(decode_input_msg(&[0x05, 0x01, 0, 0]), None);
        assert_eq!(decode_input_msg(&[]), None);
        assert_eq!(decode_input_msg(&[0x05]), None);
    }
}
