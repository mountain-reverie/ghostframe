//! Input encoding.
//!
//! Thin wrappers over `ghostframe_client_core::input` so there is exactly
//! one definition of the wire format.
//!
//! Coordinates are REMOTE framebuffer pixels, not window pixels. The host
//! owns the window and therefore owns any scaling; the library publishes
//! the remote resolution via `ClientEvent::Resized`.

use ghostframe_client_core::input;

/// Encode a key event: down=true for key-down, down=false for key-up.
/// `keysym` is an X11 KeySym.
pub fn encode_key(keysym: u32, down: bool) -> Vec<u8> {
    if down {
        input::encode_key_down(keysym).to_vec()
    } else {
        input::encode_key_up(keysym).to_vec()
    }
}

/// Encode a pointer-motion event. Coordinates are remote framebuffer pixels.
pub fn encode_motion(x: i16, y: i16) -> Vec<u8> {
    input::encode_pointer_move(x, y).to_vec()
}

/// Encode a pointer-button event. Coordinates are remote framebuffer pixels.
pub fn encode_button(x: i16, y: i16, button: u8, down: bool) -> Vec<u8> {
    input::encode_pointer_button(x, y, button, down).to_vec()
}

/// Encode a scroll-wheel event.
pub fn encode_wheel(dx: i16, dy: i16) -> Vec<u8> {
    input::encode_wheel(dx, dy).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_events_are_encoded_as_x11_keysyms() {
        // XK_a is 0x61. The wire carries X11 keysyms, which is why a native
        // client needs no translation table -- xkbcommon yields them directly.
        assert_eq!(
            encode_key(0x61, true),
            vec![0x05, 0x04, 0x00, 0x00, 0x00, 0x61]
        );
        assert_eq!(
            encode_key(0x61, false),
            vec![0x05, 0x05, 0x00, 0x00, 0x00, 0x61]
        );
    }

    #[test]
    fn pointer_motion_is_big_endian_and_signed() {
        // -1 must round-trip as 0xFFFF, not clamp to zero. A negative
        // coordinate clamped to zero is invisible until a pointer crosses
        // the window edge.
        assert_eq!(
            encode_motion(-1, 2),
            vec![0x05, 0x01, 0xFF, 0xFF, 0x00, 0x02]
        );
        assert_eq!(
            encode_motion(300, -300),
            vec![0x05, 0x01, 0x01, 0x2C, 0xFE, 0xD4]
        );
    }

    #[test]
    fn button_carries_position_and_state() {
        assert_eq!(
            encode_button(1, 2, 1, true),
            vec![0x05, 0x02, 0x00, 0x01, 0x00, 0x02, 0x01, 0x01]
        );
        assert_eq!(
            encode_button(1, 2, 3, false),
            vec![0x05, 0x02, 0x00, 0x01, 0x00, 0x02, 0x03, 0x00]
        );
    }

    #[test]
    fn wheel_is_signed() {
        assert_eq!(
            encode_wheel(0, -1),
            vec![0x05, 0x03, 0x00, 0x00, 0xFF, 0xFF]
        );
    }
}
