// Wire encoders for INPUT_MSG_TYPE (0x05).
//
// Layout (big-endian, matching FEEDBACK/HELLO/DECODE_ERROR):
//   pointer-move    [0x05][0x01][x:i16][y:i16]                    6 bytes
//   pointer-button  [0x05][0x02][x:i16][y:i16][button:u8][down:u8] 8 bytes
//   wheel           [0x05][0x03][dx:i16][dy:i16]                  6 bytes
//   key-down        [0x05][0x04][keysym:u32]                      6 bytes
//   key-up          [0x05][0x05][keysym:u32]                      6 bytes
//
// Spec: docs/superpowers/specs/2026-06-13-input-forwarding-design.md
// Ported from ghostframe-web-client/src/input/encode.ts and keymap.ts


pub const INPUT_MSG_TYPE: u8 = 0x05;

/// Encode a pointer-move event: [0x05, 0x01, x:i16 BE, y:i16 BE]
pub fn encode_pointer_move(x: i16, y: i16) -> [u8; 6] {
    [
        INPUT_MSG_TYPE,
        0x01,
        (x >> 8) as u8,
        x as u8,
        (y >> 8) as u8,
        y as u8,
    ]
}

/// Encode a pointer-button event: [0x05, 0x02, x:i16 BE, y:i16 BE, button:u8, down:u8]
pub fn encode_pointer_button(x: i16, y: i16, button: u8, down: bool) -> [u8; 8] {
    [
        INPUT_MSG_TYPE,
        0x02,
        (x >> 8) as u8,
        x as u8,
        (y >> 8) as u8,
        y as u8,
        button,
        if down { 1 } else { 0 },
    ]
}

/// Encode a wheel event: [0x05, 0x03, dx:i16 BE, dy:i16 BE]
pub fn encode_wheel(dx: i16, dy: i16) -> [u8; 6] {
    [
        INPUT_MSG_TYPE,
        0x03,
        (dx >> 8) as u8,
        dx as u8,
        (dy >> 8) as u8,
        dy as u8,
    ]
}

/// Encode a key-down event: [0x05, 0x04, keysym:u32 BE]
pub fn encode_key_down(keysym: u32) -> [u8; 6] {
    encode_key(0x04, keysym)
}

/// Encode a key-up event: [0x05, 0x05, keysym:u32 BE]
pub fn encode_key_up(keysym: u32) -> [u8; 6] {
    encode_key(0x05, keysym)
}

/// Helper: encode key event with sub-type and keysym: [0x05, sub, keysym:u32 BE]
fn encode_key(sub: u8, keysym: u32) -> [u8; 6] {
    [
        INPUT_MSG_TYPE,
        sub,
        (keysym >> 24) as u8,
        (keysym >> 16) as u8,
        (keysym >> 8) as u8,
        keysym as u8,
    ]
}

// X11 KeySym translation for browser KeyboardEvent.key values.
//
// Two paths:
//   1. Named keys (Enter, ArrowUp, F-keys, modifiers, ...) - lookup table.
//   2. Printable single-codepoint text - X11 keysym-from-Unicode rule:
//      U+0020..U+00FF maps directly; higher BMP ORs with 0x01000000.

/// Get the X11 keysym for a named key, or None if not found.
fn named_key_to_keysym(key: &str) -> Option<u32> {
    match key {
        // Whitespace / control
        "Backspace" => Some(0xff08),
        "Tab" => Some(0xff09),
        "Enter" => Some(0xff0d),
        "Escape" => Some(0xff1b),
        "Delete" => Some(0xffff),

        // Navigation
        "Home" => Some(0xff50),
        "End" => Some(0xff57),
        "PageUp" => Some(0xff55),
        "PageDown" => Some(0xff56),
        "ArrowLeft" => Some(0xff51),
        "ArrowUp" => Some(0xff52),
        "ArrowRight" => Some(0xff53),
        "ArrowDown" => Some(0xff54),

        // Editing
        "Insert" => Some(0xff63),

        // Function keys
        "F1" => Some(0xffbe),
        "F2" => Some(0xffbf),
        "F3" => Some(0xffc0),
        "F4" => Some(0xffc1),
        "F5" => Some(0xffc2),
        "F6" => Some(0xffc3),
        "F7" => Some(0xffc4),
        "F8" => Some(0xffc5),
        "F9" => Some(0xffc6),
        "F10" => Some(0xffc7),
        "F11" => Some(0xffc8),
        "F12" => Some(0xffc9),

        // Modifiers - map to the *_L variants
        "Shift" => Some(0xffe1),
        "Control" => Some(0xffe3),
        "Alt" => Some(0xffe9),
        "Meta" => Some(0xffeb),
        "AltGraph" => Some(0xfe03),
        "CapsLock" => Some(0xffe5),
        "NumLock" => Some(0xff7f),
        "ScrollLock" => Some(0xff14),

        // Misc
        "PrintScreen" => Some(0xff61),
        "Pause" => Some(0xff13),
        "ContextMenu" => Some(0xff67),
        "Space" => Some(0x0020),

        _ => None,
    }
}

/// Return the X11 KeySym for a DOM KeyboardEvent.key string, or None if it should
/// not be sent (e.g. dead-key composing state).
///
/// Ported from ghostframe-web-client/src/input/keymap.ts::keyboardEventToKeysym
pub fn key_to_keysym(key: &str) -> Option<u32> {
    // Dead keys: the browser will fire a follow-up event with the resolved
    // character. Don't send anything yet.
    if key == "Dead" || key == "Unidentified" {
        return None;
    }

    // Named keys take priority.
    if let Some(keysym) = named_key_to_keysym(key) {
        return Some(keysym);
    }

    // Single character: Unicode → X11 keysym.
    if key.chars().count() == 1 {
        let cp = key.chars().next().unwrap() as u32;
        if cp >= 0x20 && cp <= 0xff {
            return Some(cp);
        }
        return Some(cp | 0x01000000);
    }

    // Unknown multi-char name we don't have in the table.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_pointer_move_positive() {
        let result = encode_pointer_move(400, -2);
        assert_eq!(result, [0x05, 0x01, 0x01, 0x90, 0xff, 0xfe]);
    }

    #[test]
    fn test_encode_pointer_move_negative() {
        let result = encode_pointer_move(-1, -1);
        assert_eq!(result, [0x05, 0x01, 0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn test_encode_pointer_button() {
        let result = encode_pointer_button(5, 10, 1, true);
        assert_eq!(result, [0x05, 0x02, 0, 5, 0, 10, 1, 1]);
    }

    #[test]
    fn test_encode_wheel() {
        let result = encode_wheel(0, 1);
        assert_eq!(result, [0x05, 0x03, 0, 0, 0, 1]);
    }

    #[test]
    fn test_encode_key_down() {
        let result = encode_key_down(0xff0d);
        assert_eq!(result, [0x05, 0x04, 0, 0, 0xff, 0x0d]);
    }

    #[test]
    fn test_encode_key_up() {
        let result = encode_key_up(0x61);
        assert_eq!(result, [0x05, 0x05, 0, 0, 0, 0x61]);
    }

    #[test]
    fn test_key_to_keysym_named() {
        assert_eq!(key_to_keysym("Enter"), Some(0xff0d));
        assert_eq!(key_to_keysym("ArrowUp"), Some(0xff52));
        assert_eq!(key_to_keysym("F1"), Some(0xffbe));
    }

    #[test]
    fn test_key_to_keysym_dead_keys() {
        assert_eq!(key_to_keysym("Dead"), None);
        assert_eq!(key_to_keysym("Unidentified"), None);
    }

    #[test]
    fn test_key_to_keysym_printable() {
        assert_eq!(key_to_keysym("a"), Some(0x61));
        assert_eq!(key_to_keysym(" "), Some(0x20));
    }

    #[test]
    fn test_key_to_keysym_latin1() {
        assert_eq!(key_to_keysym("ñ"), Some(0xf1));
    }

    #[test]
    fn test_key_to_keysym_bmp() {
        assert_eq!(key_to_keysym("中"), Some(0x01000000 | 0x4e2d));
    }

    #[test]
    fn test_key_to_keysym_unknown() {
        assert_eq!(key_to_keysym("NotARealKey"), None);
    }
}
