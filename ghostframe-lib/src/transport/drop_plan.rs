//! Deterministic drop injection for e2e tests.
//!
//! `NetProfile::loss` is probabilistic, which cannot produce the case this
//! exists for: a tile whose *only* transmission is lost, so the receiver
//! never learns it was sent and can never NACK it. Reaching that case by
//! raising `loss` does not work — at the rates where it becomes likely the
//! scene stops establishing at all (measured: bails at 0.60 and 0.90).
//!
//! ## Why this lives here, not at the QUIC layer
//!
//! An earlier version of this plan was consulted inside the browserless
//! harness's `rule()`, which operates on raw UDP payloads — i.e. QUIC
//! packets. That is the wrong layer: application tile datagrams travel
//! *inside* QUIC DATAGRAM frames, encrypted, so `is_tile_datagram` and the
//! tile-coordinate offsets below are meaningless once QUIC has touched the
//! bytes. Measured on a scene that genuinely delivers a tile
//! (`a_single_solid_tile_arrives_on_a_perfect_link`): 20 server-to-client
//! packets, and `is_tile_datagram` returned true exactly once — on packet
//! #1, the QUIC Initial (long-header, `byte0=0xc3`), because bit 7 of byte 0
//! is the unprotected long-header form bit. Every packet that could
//! actually carry a tile is short-header, where that bit is always 0. So a
//! rule wired in at that layer could never drop a tile, and a rule whose
//! coordinates happened to match the Initial's bytes 16/17 would eat the
//! handshake instead — while still reporting that it fired, fabricating the
//! "this drop is real" signal by destroying the connection.
//!
//! `IoBridge::send_to_all_sessions` is the plaintext seam instead: it takes
//! the application datagram *before* QUIC encrypts it, and already hosts
//! the equivalent `outbound_loss` hook for probabilistic loss. This plan is
//! consulted there, immediately after that hook.
//!
//! Wire constants are imported rather than re-declared, for the reason
//! `pump.rs` (in `ghostframe-e2e`) gives: the production framing helpers are
//! `pub` specifically so nothing consulting them can drift from what
//! actually goes on the wire.

use crate::transport::protocol::{is_tile_datagram, DATAGRAM_HEADER_SIZE};

/// Tile coordinates are the first two bytes of `TileHeader`, which follows
/// the fixed-size `DatagramHeader`.
const TILE_X_OFFSET: usize = DATAGRAM_HEADER_SIZE;
const TILE_Y_OFFSET: usize = DATAGRAM_HEADER_SIZE + 1;
/// Shortest payload whose byte at `TILE_Y_OFFSET` exists.
const MIN_TILE_LEN: usize = TILE_Y_OFFSET + 1;

/// Drop the given occurrences of datagrams carrying this tile.
///
/// `occurrences` are zero-based counts of matching datagrams seen so far:
/// `vec![0]` drops the first and lets every later one through, which is the
/// "last write lost, then static" case.
///
/// Note that `(255, 255)` is not a tile: it is the frame-dimensions sentinel
/// (`FRAME_DIMENSIONS_SENTINEL_X`/`_Y`), so a rule naming it would drop
/// control traffic rather than picture content.
#[derive(Debug, Clone)]
pub struct DropRule {
    pub tile_x: u8,
    pub tile_y: u8,
    pub occurrences: Vec<u32>,
}

/// A set of deterministic drop rules, at most one per tile.
///
/// Deliberately not `Clone`: the occurrence counters are live state, and a
/// copy would silently fork the drop schedule.
#[derive(Debug, Default)]
pub struct DropPlan {
    rules: Vec<DropRule>,
    /// Matching datagrams seen so far, per rule, parallel to `rules`.
    seen: Vec<u32>,
    /// Datagrams actually dropped so far, per rule, parallel to `rules`.
    dropped: Vec<u32>,
}

impl DropPlan {
    /// Build a plan from `rules`.
    ///
    /// The `debug_assert`s reject the two ways to write a rule that can never
    /// fire. Both matter more than usual here: this type exists to make a
    /// test's premise real, and a rule that quietly does nothing turns that
    /// test into one that passes without testing anything.
    pub fn new(rules: Vec<DropRule>) -> Self {
        for (i, rule) in rules.iter().enumerate() {
            debug_assert!(
                !rule.occurrences.is_empty(),
                "DropRule for tile ({}, {}) names no occurrences: it would match \
                 every transmission and drop none, so the test it was written for \
                 would pass vacuously",
                rule.tile_x,
                rule.tile_y
            );
            debug_assert!(
                !rules[..i]
                    .iter()
                    .any(|r| r.tile_x == rule.tile_x && r.tile_y == rule.tile_y),
                "duplicate DropRule for tile ({}, {}): should_drop returns on the \
                 first match, so this rule could never fire",
                rule.tile_x,
                rule.tile_y
            );
        }
        let n = rules.len();
        Self {
            rules,
            seen: vec![0; n],
            dropped: vec![0; n],
        }
    }

    /// Datagrams actually dropped so far, per rule, parallel to the rules
    /// given to `new`.
    ///
    /// A scene asserting on the *effect* of an injected drop should first
    /// assert the matching entry is non-zero. A rule naming a tile the scene
    /// never sends is silent, and without this the assertion would pass
    /// whether or not anything was ever dropped.
    pub fn drops(&self) -> &[u32] {
        &self.dropped
    }

    /// True if this datagram should be dropped. Advances the per-rule
    /// occurrence counter for whichever rule matched.
    pub fn should_drop(&mut self, payload: &[u8]) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        // Both guards are load-bearing and independent: a non-tile datagram
        // can be longer than MIN_TILE_LEN and carry arbitrary bytes at the
        // coordinate offsets. An ACK batch is exactly that.
        if payload.len() < MIN_TILE_LEN || !is_tile_datagram(payload) {
            return false;
        }
        let tx = payload[TILE_X_OFFSET];
        let ty = payload[TILE_Y_OFFSET];
        for (i, rule) in self.rules.iter().enumerate() {
            if rule.tile_x == tx && rule.tile_y == ty {
                let n = self.seen[i];
                self.seen[i] = n.saturating_add(1);
                let drop = rule.occurrences.contains(&n);
                if drop {
                    self.dropped[i] = self.dropped[i].saturating_add(1);
                }
                return drop;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal tile datagram: flag bit in byte 0, tile_x at 16,
    /// tile_y at 17. Everything else is zero — `DropPlan` reads nothing else.
    fn tile_datagram(tile_x: u8, tile_y: u8) -> Vec<u8> {
        let mut v = vec![0u8; 20];
        v[0] = 0x80;
        v[TILE_X_OFFSET] = tile_x;
        v[TILE_Y_OFFSET] = tile_y;
        v
    }

    #[test]
    fn drops_only_the_named_occurrence() {
        let mut plan = DropPlan::new(vec![DropRule {
            tile_x: 2,
            tile_y: 3,
            occurrences: vec![0],
        }]);
        let dg = tile_datagram(2, 3);
        assert!(plan.should_drop(&dg), "first occurrence must drop");
        assert!(!plan.should_drop(&dg), "second occurrence must pass");
        assert!(!plan.should_drop(&dg), "third occurrence must pass");
        assert_eq!(plan.drops(), &[1], "exactly one datagram was dropped");
    }

    #[test]
    fn leaves_other_tiles_alone() {
        let mut plan = DropPlan::new(vec![DropRule {
            tile_x: 2,
            tile_y: 3,
            occurrences: vec![0],
        }]);
        assert!(!plan.should_drop(&tile_datagram(0, 0)));
        assert!(!plan.should_drop(&tile_datagram(2, 4)));
        // The named tile is still on its first occurrence: a non-matching
        // datagram must not have consumed it.
        assert!(plan.should_drop(&tile_datagram(2, 3)));
    }

    /// The tile-flag guard, isolated from the length guard.
    ///
    /// An ACK batch (`ACK_BATCH_MSG_TYPE` = 0x06, 7-byte entries) runs well
    /// past byte 17, and whatever entry bytes land there are arbitrary — they
    /// can equal any tile coordinate. Without the flag check a plan would
    /// silently eat acknowledgements, corrupting the very feedback path these
    /// scenes measure, while still looking like a successful tile drop.
    #[test]
    fn a_non_tile_datagram_is_never_dropped_however_long_it_is() {
        let mut plan = DropPlan::new(vec![DropRule {
            tile_x: 6,
            tile_y: 0,
            occurrences: vec![0],
        }]);
        let mut ack = vec![0u8; 32];
        ack[0] = 0x06;
        ack[TILE_X_OFFSET] = 6;
        ack[TILE_Y_OFFSET] = 0;
        assert!(
            !plan.should_drop(&ack),
            "a non-tile datagram must never drop"
        );
        assert_eq!(plan.drops(), &[0]);
        assert!(
            plan.should_drop(&tile_datagram(6, 0)),
            "the real first transmission must still be the one that drops"
        );
    }

    #[test]
    fn a_payload_too_short_to_carry_coordinates_is_ignored() {
        let mut plan = DropPlan::new(vec![DropRule {
            tile_x: 0,
            tile_y: 0,
            occurrences: vec![0],
        }]);
        assert!(!plan.should_drop(&[0x80u8, 0, 0]));
    }

    /// Pins the exact length boundary: an off-by-one here would index out of
    /// bounds on a 17-byte datagram.
    #[test]
    fn reads_coordinates_at_the_exact_length_boundary() {
        let mut plan = DropPlan::new(vec![DropRule {
            tile_x: 9,
            tile_y: 9,
            occurrences: vec![0],
        }]);
        let mut dg = vec![0u8; MIN_TILE_LEN];
        dg[0] = 0x80;
        dg[TILE_X_OFFSET] = 9;
        dg[TILE_Y_OFFSET] = 9;
        assert!(plan.should_drop(&dg), "18 bytes carries both coordinates");
        assert!(
            !plan.should_drop(&dg[..MIN_TILE_LEN - 1]),
            "one byte shorter must be ignored, not indexed"
        );
    }

    /// The whole `occurrences` list is honoured, not just its first entry.
    #[test]
    fn honours_every_listed_occurrence_not_just_the_first() {
        let mut plan = DropPlan::new(vec![DropRule {
            tile_x: 1,
            tile_y: 1,
            occurrences: vec![1, 3],
        }]);
        let dg = tile_datagram(1, 1);
        let fates: Vec<bool> = (0..5).map(|_| plan.should_drop(&dg)).collect();
        assert_eq!(fates, vec![false, true, false, true, false]);
        assert_eq!(plan.drops(), &[2]);
    }

    #[test]
    fn an_empty_plan_drops_nothing() {
        let mut plan = DropPlan::default();
        assert!(!plan.should_drop(&tile_datagram(1, 1)));
        assert!(plan.drops().is_empty());
    }
}
