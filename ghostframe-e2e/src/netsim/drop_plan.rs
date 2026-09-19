//! Deterministic drop injection for the browserless harness.
//!
//! `NetProfile::loss` is probabilistic, which cannot produce the case this
//! exists for: a tile whose *only* transmission is lost, so the receiver
//! never learns it was sent and can never NACK it. Reaching that case by
//! raising `loss` does not work — at the rates where it becomes likely the
//! scene stops establishing at all (measured: bails at 0.60 and 0.90).
//!
//! Applied *after* `NetSim::decide` so the rng stream is untouched; see
//! that function's doc comment on draw ordering.

/// Byte offsets of the tile coordinates inside a tile datagram.
const TILE_X_OFFSET: usize = 16;
const TILE_Y_OFFSET: usize = 17;
/// Shortest payload that can carry both coordinates.
const MIN_TILE_LEN: usize = TILE_Y_OFFSET + 1;
/// Byte 0 has the tile flag bit set (0x80 in big-endian, which is bit 31 of frame_seq).
const TILE_DATAGRAM_FLAG_BYTE: u8 = 0x80;

/// Drop the given occurrences of datagrams carrying this tile.
///
/// `occurrences` are zero-based counts of matching datagrams seen so far:
/// `vec![0]` drops the first and lets every later one through, which is the
/// "last write lost, then static" case.
#[derive(Debug, Clone)]
pub struct DropRule {
    pub tile_x: u8,
    pub tile_y: u8,
    pub occurrences: Vec<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct DropPlan {
    rules: Vec<DropRule>,
    /// Matches seen so far per rule, parallel to `rules`.
    seen: Vec<u32>,
}

impl DropPlan {
    pub fn new(rules: Vec<DropRule>) -> Self {
        let seen = vec![0; rules.len()];
        Self { rules, seen }
    }

    /// True if this datagram should be dropped. Advances the per-rule
    /// occurrence counter for whichever rule matched.
    pub fn should_drop(&mut self, payload: &[u8]) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        if payload.len() < MIN_TILE_LEN || (payload[0] & TILE_DATAGRAM_FLAG_BYTE) == 0 {
            return false;
        }
        let tx = payload[TILE_X_OFFSET];
        let ty = payload[TILE_Y_OFFSET];
        for (i, rule) in self.rules.iter().enumerate() {
            if rule.tile_x == tx && rule.tile_y == ty {
                let n = self.seen[i];
                self.seen[i] = n.saturating_add(1);
                return rule.occurrences.contains(&n);
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
        v[16] = tile_x;
        v[17] = tile_y;
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
        // The named tile is still on its first occurrence.
        assert!(plan.should_drop(&tile_datagram(2, 3)));
    }

    #[test]
    fn ignores_non_tile_datagrams() {
        let mut plan = DropPlan::new(vec![DropRule {
            tile_x: 0,
            tile_y: 0,
            occurrences: vec![0],
        }]);
        // ACK/NACK envelopes do not set the tile flag; byte 0 is a message
        // type. A plan must never swallow one.
        let ack = vec![0x06u8, 0, 0, 0, 0, 0];
        assert!(!plan.should_drop(&ack));
        // Too short to carry tile coordinates.
        assert!(!plan.should_drop(&[0x80u8, 0, 0]));
    }

    #[test]
    fn an_empty_plan_drops_nothing() {
        let mut plan = DropPlan::default();
        assert!(!plan.should_drop(&tile_datagram(1, 1)));
    }
}
