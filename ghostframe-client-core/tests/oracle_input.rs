/// Oracle tests ported from ghostframe-web-client/tests/input.test.ts
/// All byte sequences and keysym mappings are exact replicas of the vitest tests.
use ghostframe_client_core::input::{
    encode_key_down, encode_key_up, encode_pointer_button, encode_pointer_move, encode_wheel,
    key_to_keysym, INPUT_MSG_TYPE,
};

#[test]
fn test_input_msg_type_is_0x05() {
    assert_eq!(INPUT_MSG_TYPE, 0x05);
}

// ============================================================================
// keyboardEventToKeysym / key_to_keysym tests
// ============================================================================

#[test]
fn test_key_to_keysym_dead_key() {
    assert_eq!(key_to_keysym("Dead"), None);
}

#[test]
fn test_key_to_keysym_unidentified() {
    assert_eq!(key_to_keysym("Unidentified"), None);
}

#[test]
fn test_key_to_keysym_enter() {
    assert_eq!(key_to_keysym("Enter"), Some(0xff0d));
}

#[test]
fn test_key_to_keysym_arrow_up() {
    assert_eq!(key_to_keysym("ArrowUp"), Some(0xff52));
}

#[test]
fn test_key_to_keysym_f1() {
    assert_eq!(key_to_keysym("F1"), Some(0xffbe));
}

#[test]
fn test_key_to_keysym_printable_a() {
    assert_eq!(key_to_keysym("a"), Some(0x61));
}

#[test]
fn test_key_to_keysym_latin1_ntilde() {
    assert_eq!(key_to_keysym("ñ"), Some(0xf1));
}

#[test]
fn test_key_to_keysym_higher_bmp_cjk() {
    // '中' codepoint is 0x4e2d
    assert_eq!(key_to_keysym("中"), Some(0x01000000 | 0x4e2d));
}

#[test]
fn test_key_to_keysym_space_literal() {
    assert_eq!(key_to_keysym(" "), Some(0x20));
}

#[test]
fn test_key_to_keysym_unknown_multichar() {
    assert_eq!(key_to_keysym("NotARealKey"), None);
}

// Full named keysyms test (named key table)
#[test]
fn test_key_to_keysym_backspace() {
    assert_eq!(key_to_keysym("Backspace"), Some(0xff08));
}

#[test]
fn test_key_to_keysym_tab() {
    assert_eq!(key_to_keysym("Tab"), Some(0xff09));
}

#[test]
fn test_key_to_keysym_escape() {
    assert_eq!(key_to_keysym("Escape"), Some(0xff1b));
}

#[test]
fn test_key_to_keysym_delete() {
    assert_eq!(key_to_keysym("Delete"), Some(0xffff));
}

#[test]
fn test_key_to_keysym_home() {
    assert_eq!(key_to_keysym("Home"), Some(0xff50));
}

#[test]
fn test_key_to_keysym_end() {
    assert_eq!(key_to_keysym("End"), Some(0xff57));
}

#[test]
fn test_key_to_keysym_pageup() {
    assert_eq!(key_to_keysym("PageUp"), Some(0xff55));
}

#[test]
fn test_key_to_keysym_pagedown() {
    assert_eq!(key_to_keysym("PageDown"), Some(0xff56));
}

#[test]
fn test_key_to_keysym_arrow_left() {
    assert_eq!(key_to_keysym("ArrowLeft"), Some(0xff51));
}

#[test]
fn test_key_to_keysym_arrow_right() {
    assert_eq!(key_to_keysym("ArrowRight"), Some(0xff53));
}

#[test]
fn test_key_to_keysym_arrow_down() {
    assert_eq!(key_to_keysym("ArrowDown"), Some(0xff54));
}

#[test]
fn test_key_to_keysym_insert() {
    assert_eq!(key_to_keysym("Insert"), Some(0xff63));
}

#[test]
fn test_key_to_keysym_f2() {
    assert_eq!(key_to_keysym("F2"), Some(0xffbf));
}

#[test]
fn test_key_to_keysym_f3() {
    assert_eq!(key_to_keysym("F3"), Some(0xffc0));
}

#[test]
fn test_key_to_keysym_f4() {
    assert_eq!(key_to_keysym("F4"), Some(0xffc1));
}

#[test]
fn test_key_to_keysym_f5() {
    assert_eq!(key_to_keysym("F5"), Some(0xffc2));
}

#[test]
fn test_key_to_keysym_f6() {
    assert_eq!(key_to_keysym("F6"), Some(0xffc3));
}

#[test]
fn test_key_to_keysym_f7() {
    assert_eq!(key_to_keysym("F7"), Some(0xffc4));
}

#[test]
fn test_key_to_keysym_f8() {
    assert_eq!(key_to_keysym("F8"), Some(0xffc5));
}

#[test]
fn test_key_to_keysym_f9() {
    assert_eq!(key_to_keysym("F9"), Some(0xffc6));
}

#[test]
fn test_key_to_keysym_f10() {
    assert_eq!(key_to_keysym("F10"), Some(0xffc7));
}

#[test]
fn test_key_to_keysym_f11() {
    assert_eq!(key_to_keysym("F11"), Some(0xffc8));
}

#[test]
fn test_key_to_keysym_f12() {
    assert_eq!(key_to_keysym("F12"), Some(0xffc9));
}

#[test]
fn test_key_to_keysym_shift() {
    assert_eq!(key_to_keysym("Shift"), Some(0xffe1));
}

#[test]
fn test_key_to_keysym_control() {
    assert_eq!(key_to_keysym("Control"), Some(0xffe3));
}

#[test]
fn test_key_to_keysym_alt() {
    assert_eq!(key_to_keysym("Alt"), Some(0xffe9));
}

#[test]
fn test_key_to_keysym_meta() {
    assert_eq!(key_to_keysym("Meta"), Some(0xffeb));
}

#[test]
fn test_key_to_keysym_altgraph() {
    assert_eq!(key_to_keysym("AltGraph"), Some(0xfe03));
}

#[test]
fn test_key_to_keysym_capslock() {
    assert_eq!(key_to_keysym("CapsLock"), Some(0xffe5));
}

#[test]
fn test_key_to_keysym_numlock() {
    assert_eq!(key_to_keysym("NumLock"), Some(0xff7f));
}

#[test]
fn test_key_to_keysym_scrolllock() {
    assert_eq!(key_to_keysym("ScrollLock"), Some(0xff14));
}

#[test]
fn test_key_to_keysym_printscreen() {
    assert_eq!(key_to_keysym("PrintScreen"), Some(0xff61));
}

#[test]
fn test_key_to_keysym_pause() {
    assert_eq!(key_to_keysym("Pause"), Some(0xff13));
}

#[test]
fn test_key_to_keysym_contextmenu() {
    assert_eq!(key_to_keysym("ContextMenu"), Some(0xff67));
}

#[test]
fn test_key_to_keysym_space_named() {
    assert_eq!(key_to_keysym("Space"), Some(0x0020));
}

// ============================================================================
// encodeInput* tests
// ============================================================================

#[test]
fn test_encode_pointer_move_basic() {
    let buf = encode_pointer_move(400, -2);
    assert_eq!(buf, [0x05, 0x01, 0x01, 0x90, 0xff, 0xfe]);
}

#[test]
fn test_encode_pointer_button_down_true() {
    let buf = encode_pointer_button(5, 10, 1, true);
    assert_eq!(buf, [0x05, 0x02, 0, 5, 0, 10, 1, 1]);
}

#[test]
fn test_encode_pointer_button_down_false() {
    let buf = encode_pointer_button(5, 10, 3, false);
    assert_eq!(buf, [0x05, 0x02, 0, 5, 0, 10, 3, 0]);
}

#[test]
fn test_encode_wheel_basic() {
    let buf = encode_wheel(0, 1);
    assert_eq!(buf, [0x05, 0x03, 0, 0, 0, 1]);
}

#[test]
fn test_encode_key_down_basic() {
    let buf = encode_key_down(0xff0d);
    assert_eq!(buf, [0x05, 0x04, 0, 0, 0xff, 0x0d]);
}

#[test]
fn test_encode_key_up_basic() {
    let buf = encode_key_up(0x61);
    assert_eq!(buf, [0x05, 0x05, 0, 0, 0, 0x61]);
}

#[test]
fn test_encode_pointer_move_negative_both() {
    let buf = encode_pointer_move(-1, -1);
    assert_eq!(buf, [0x05, 0x01, 0xff, 0xff, 0xff, 0xff]);
}
