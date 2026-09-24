//! The server telling a client it has been displaced.
//!
//! Carried as a control datagram using sentinel tile coordinates, the same
//! idiom `protocol::build_frame_dimensions_datagram` established: tile
//! coordinates are `u8`, and a sentinel value is structurally impossible at
//! any sensible resolution, so the receiver can route on it without a new
//! datagram type or a version negotiation.
//!
//! **This is best-effort.** Datagrams are lossy and the session closes
//! shortly after, so a client may never see it and will fall back to a
//! generic disconnect. That is degraded, not broken.

use crate::protocol::{
    decode_tile_datagram, fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG,
};

/// Sentinel tile coordinates marking an eviction notice. Distinct from
/// `FRAME_DIMENSIONS_SENTINEL_*` (0xFF) so the two control messages cannot
/// be confused for one another.
pub const EVICTION_SENTINEL_X: u8 = 0xFE;
pub const EVICTION_SENTINEL_Y: u8 = 0xFE;

/// Why the server closed this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EvictionReason {
    /// Another client connected. This server serves one client at a time.
    DisplacedByNewSession = 1,
    /// A reason this build does not know. Still an eviction: the session is
    /// going away regardless, and a client that ignored it would sit
    /// connected to a server that has already dropped it.
    Unknown = 0xFF,
}

impl EvictionReason {
    fn from_byte(b: u8) -> Self {
        match b {
            1 => EvictionReason::DisplacedByNewSession,
            _ => EvictionReason::Unknown,
        }
    }
}

/// Build a single eviction datagram. Always fits one datagram: the payload
/// is one byte.
pub fn build_eviction_datagram(reason: EvictionReason) -> Vec<u8> {
    let payload = [reason as u8];
    let inputs = TileFragmentInputs {
        frame_seq: TILE_DATAGRAM_FLAG,
        tile_x: EVICTION_SENTINEL_X,
        tile_y: EVICTION_SENTINEL_Y,
        codec: Codec::Skip,
        generation: 0,
        pass: 0,
        timestamp_us: 0,
    };
    let datagrams = fragment_tile(&inputs, &payload, /* max_fragment_payload */ 1);
    debug_assert_eq!(
        datagrams.len(),
        1,
        "an eviction notice must fit one datagram"
    );
    datagrams.into_iter().next().unwrap()
}

/// Returns the reason if `datagram` is an eviction notice, `None` if it is
/// any other datagram (including one too short to be any tile datagram at
/// all).
pub fn parse_eviction(datagram: &[u8]) -> Option<EvictionReason> {
    let (_dh, th, payload) = decode_tile_datagram(datagram).ok()?;
    if th.tile_x != EVICTION_SENTINEL_X || th.tile_y != EVICTION_SENTINEL_Y {
        return None;
    }
    Some(EvictionReason::from_byte(*payload.first()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_the_reason() {
        let dg = build_eviction_datagram(EvictionReason::DisplacedByNewSession);
        assert_eq!(
            parse_eviction(&dg),
            Some(EvictionReason::DisplacedByNewSession)
        );
    }

    #[test]
    fn a_normal_tile_datagram_is_not_an_eviction() {
        // Sentinel coordinates are what route this message. An ordinary tile
        // must not be mistaken for one, or a busy screen would disconnect
        // the client.
        let inputs = crate::protocol::TileFragmentInputs {
            frame_seq: 7 | crate::protocol::TILE_DATAGRAM_FLAG,
            tile_x: 3,
            tile_y: 4,
            codec: crate::protocol::Codec::Solid,
            generation: 0,
            pass: 0,
            timestamp_us: 0,
        };
        let dg = crate::protocol::fragment_tile(&inputs, &[0u8; 4], 4)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(parse_eviction(&dg), None);
    }

    #[test]
    fn an_unknown_reason_code_parses_as_unknown_rather_than_none() {
        // A future server may add reasons this client predates. Treating an
        // unknown code as "not an eviction" would leave the client connected
        // to a server that has already dropped it; treating it as Unknown
        // still disconnects, just without a specific message.
        let mut dg = build_eviction_datagram(EvictionReason::DisplacedByNewSession);
        let last = dg.len() - 1;
        dg[last] = 0xEE;
        assert_eq!(parse_eviction(&dg), Some(EvictionReason::Unknown));
    }
}
