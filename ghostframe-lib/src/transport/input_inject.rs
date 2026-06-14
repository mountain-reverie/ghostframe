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
