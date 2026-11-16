// SPDX-License-Identifier: MIT OR Apache-2.0
//! What the user is told when the session ends.

use ghostframe_protocol::eviction::EvictionReason;

/// QUIC application close code the server uses for a session whose peer no
/// longer passes the tailnet access rule (`CLOSE_CODE_ACCESS_REVOKED` in
/// ghostframe-lib's io_bridge session; that crate is AGPL and is never a
/// dependency here, so the value is repeated, as `close_message.ts` did).
pub const CLOSE_CODE_ACCESS_REVOKED: u32 = 0x6704;

/// The overlay text for a transport that closed with `close_code`, or `None`
/// when the close needs nothing beyond the page log (takeover has its own
/// eviction overlay; other closes are not explained to the user).
pub fn close_overlay_message(close_code: Option<u32>) -> Option<&'static str> {
    (close_code == Some(CLOSE_CODE_ACCESS_REVOKED)).then_some("Access revoked")
}

/// The overlay text for the server's eviction notice; `reason` is
/// `EvictionReason::to_byte()`.
pub fn evicted_message(reason: u8) -> &'static str {
    if reason == EvictionReason::DisplacedByNewSession.to_byte() {
        "Another session took over this desktop."
    } else {
        "The server ended this session."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_revoked_is_the_only_explained_close() {
        assert_eq!(CLOSE_CODE_ACCESS_REVOKED, 26372);
        assert_eq!(close_overlay_message(Some(0x6704)), Some("Access revoked"));
        assert_eq!(close_overlay_message(Some(0x6701)), None);
        assert_eq!(close_overlay_message(Some(0)), None);
        assert_eq!(close_overlay_message(None), None);
    }

    #[test]
    fn eviction_names_a_takeover_and_nothing_else() {
        assert_eq!(
            evicted_message(EvictionReason::DisplacedByNewSession.to_byte()),
            "Another session took over this desktop."
        );
        assert_eq!(evicted_message(0xEE), "The server ended this session.");
    }
}
