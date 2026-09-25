//! DisplayInfo (0x07) and DisplayMode (0x08): client → server display
//! negotiation on the reliable bidi feedback stream.
//!
//! Wire formats, all big-endian:
//!   DisplayInfo  [0x07][max_w:u16][max_h:u16][scale_milli:u16][mm_w:u16][mm_h:u16]  11 bytes
//!   DisplayMode  [0x08][w:u16][h:u16]                                                5 bytes
//!
//! Separate from HELLO: HELLO is a one-shot capability advertisement and
//! eviction keys on it (M4a), while display state changes throughout a
//! session.
//!
//! **`scale_milli` is authoritative; `mm_*` is advisory.** A future Wayland
//! backend can set mode and scale but NOT physical size -- the
//! wlr-output-management protocol says physical_size "cannot be changed by
//! clients". Millimetres are carried for logs and for a backend that might
//! one day use them; nothing reads them to make a decision.
//!
//! Scale is thousandths: 1000 = 1.0, 1500 = 1.5.

pub const DISPLAY_INFO_MSG_TYPE: u8 = 0x07;
pub const DISPLAY_MODE_MSG_TYPE: u8 = 0x08;
pub const DISPLAY_INFO_SIZE: usize = 11;
pub const DISPLAY_MODE_SIZE: usize = 5;

/// Client → server display-capability advertisement.
///
/// `mm_width`/`mm_height` are advisory only -- see the module docs for why
/// `scale_milli` is the authoritative signal and millimetres must never be
/// read to make a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayInfoMsg {
    pub max_width: u16,
    pub max_height: u16,
    /// Thousandths: 1000 = 1.0.
    pub scale_milli: u16,
    pub mm_width: u16,
    pub mm_height: u16,
}

impl DisplayInfoMsg {
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < DISPLAY_INFO_SIZE {
            return None;
        }
        if data[0] != DISPLAY_INFO_MSG_TYPE {
            return None;
        }
        Some(Self {
            max_width: u16::from_be_bytes([data[1], data[2]]),
            max_height: u16::from_be_bytes([data[3], data[4]]),
            scale_milli: u16::from_be_bytes([data[5], data[6]]),
            mm_width: u16::from_be_bytes([data[7], data[8]]),
            mm_height: u16::from_be_bytes([data[9], data[10]]),
        })
    }

    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(DISPLAY_INFO_MSG_TYPE);
        buf.extend_from_slice(&self.max_width.to_be_bytes());
        buf.extend_from_slice(&self.max_height.to_be_bytes());
        buf.extend_from_slice(&self.scale_milli.to_be_bytes());
        buf.extend_from_slice(&self.mm_width.to_be_bytes());
        buf.extend_from_slice(&self.mm_height.to_be_bytes());
    }
}

/// Client → server display-mode request. The server clamps and reports what
/// it actually set through the existing frame-dimensions message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayModeMsg {
    pub width: u16,
    pub height: u16,
}

impl DisplayModeMsg {
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < DISPLAY_MODE_SIZE {
            return None;
        }
        if data[0] != DISPLAY_MODE_MSG_TYPE {
            return None;
        }
        Some(Self {
            width: u16::from_be_bytes([data[1], data[2]]),
            height: u16::from_be_bytes([data[3], data[4]]),
        })
    }

    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(DISPLAY_MODE_MSG_TYPE);
        buf.extend_from_slice(&self.width.to_be_bytes());
        buf.extend_from_slice(&self.height.to_be_bytes());
    }
}

/// Error from a `DisplayController::set_output` call. The X backend can fail
/// for several reasons (RandR mode add/set failure, output not found); a
/// future headless-Wayland backend would have its own. Kept as a plain
/// reason string rather than a structured enum because every caller
/// (`IoBridge::on_timeout`) only ever logs it -- there is no recovery branch
/// that depends on which failure this was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayError(pub String);

impl std::fmt::Display for DisplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "display controller error: {}", self.0)
    }
}

impl std::error::Error for DisplayError {}

/// Server-side display control. Implemented against X/RandR in
/// `ghostframe-xdaemon`; mocked in tests. Mirrors `InputInjector`
/// (`transport/input_inject.rs`), which exists for the same reason: keep
/// protocol logic testable without a running display server.
///
/// **Nothing above this trait may mention X, RandR, or millimetres.** A
/// future headless-Wayland backend implements the same two methods with
/// `set_custom_mode` and `set_scale`; that port should need no change above
/// this line. See the M4b design doc §5 and §7.
pub trait DisplayController: Send + Sync {
    /// The framebuffer ceiling. Requests above it are clamped, never rejected.
    fn ceiling(&self) -> (u16, u16);

    /// Apply a resolution and scale together -- they are one user-visible
    /// change, and applying them separately would show an intermediate
    /// state at the wrong size or the wrong font scale.
    fn set_output(&self, width: u16, height: u16, scale_milli: u16) -> Result<(), DisplayError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_info_round_trips() {
        let msg = DisplayInfoMsg {
            max_width: 2560,
            max_height: 1440,
            scale_milli: 1500,
            mm_width: 597,
            mm_height: 336,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        assert_eq!(buf[0], DISPLAY_INFO_MSG_TYPE);
        assert_eq!(DisplayInfoMsg::decode(&buf), Some(msg));
    }

    #[test]
    fn display_mode_round_trips() {
        let msg = DisplayModeMsg {
            width: 1280,
            height: 800,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        assert_eq!(buf[0], DISPLAY_MODE_MSG_TYPE);
        assert_eq!(DisplayModeMsg::decode(&buf), Some(msg));
    }

    #[test]
    fn a_truncated_message_decodes_to_none_rather_than_panicking() {
        // The feedback dispatcher buffers partial stream reads, so a short
        // slice is normal traffic, not corruption.
        let msg = DisplayInfoMsg {
            max_width: 2560,
            max_height: 1440,
            scale_milli: 1000,
            mm_width: 597,
            mm_height: 336,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        for n in 0..buf.len() {
            assert_eq!(
                DisplayInfoMsg::decode(&buf[..n]),
                None,
                "prefix of length {n}"
            );
        }
    }

    #[test]
    fn the_two_types_do_not_collide_with_existing_messages() {
        // 0x01 feedback, 0x03 hello, 0x04 decode-error, 0x05 input, 0x06 ack.
        for taken in [0x01u8, 0x03, 0x04, 0x05, 0x06] {
            assert_ne!(DISPLAY_INFO_MSG_TYPE, taken);
            assert_ne!(DISPLAY_MODE_MSG_TYPE, taken);
        }
        assert_ne!(DISPLAY_INFO_MSG_TYPE, DISPLAY_MODE_MSG_TYPE);
    }
}
