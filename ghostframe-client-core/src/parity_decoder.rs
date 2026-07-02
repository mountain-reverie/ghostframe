//! Wire-sequence FEC parity decoder.
//!
//! Port of `ghostframe-web-client/src/parity_decoder.ts`. Maintains a
//! bounded, insertion-order window of received source datagrams keyed by
//! `wire_seq`, plus a set of buffered parity envelopes that could not be
//! resolved (more than one source missing) at the time they arrived. When a
//! later source arrives it may unlock a buffered parity, recovering the
//! still-missing source datagram.

use std::collections::{HashMap, VecDeque};

use ghostframe_protocol::protocol::TileParityEnvelope;

use crate::ordered_map::OrderedMap;

/// XOR `src` into `out`, right-aligned (i.e. matching trailing bytes).
///
/// Mirrors `xorInto` in parity_decoder.ts: `out` is assumed to be at least
/// as long as `src`; the shorter buffer's bytes line up with the tail of
/// the longer one.
fn xor_into(out: &mut [u8], src: &[u8]) {
    let pad = out.len() - src.len();
    for (i, b) in src.iter().enumerate() {
        out[pad + i] ^= b;
    }
}

pub struct ParityDecoder {
    window: HashMap<u32, Vec<u8>>,
    order: VecDeque<u32>,
    // Insertion-ordered (not `HashMap`) to match the TS reference's
    // `Map<number, ParityHeader>`: recovery probing below iterates in
    // insertion order and returns the *first* recoverable entry, so the
    // iteration order must be deterministic and match the TS `Map`.
    pending_parities: OrderedMap<u32, TileParityEnvelope>,
    window_capacity: usize,
}

impl ParityDecoder {
    pub fn new(window_capacity: usize) -> Self {
        ParityDecoder {
            window: HashMap::new(),
            order: VecDeque::new(),
            pending_parities: OrderedMap::new(),
            window_capacity,
        }
    }

    pub fn has_source(&self, wire_seq: u32) -> bool {
        self.window.contains_key(&wire_seq)
    }

    /// Insert a received source datagram; may unlock a buffered parity.
    /// Returns the recovered source datagram if one buffered parity group
    /// became recoverable.
    pub fn record_source(&mut self, wire_seq: u32, bytes: &[u8]) -> Option<Vec<u8>> {
        if !self.window.contains_key(&wire_seq) {
            self.window.insert(wire_seq, bytes.to_vec());
            self.order.push_back(wire_seq);
            while self.window.len() > self.window_capacity {
                match self.order.pop_front() {
                    Some(oldest) => {
                        self.window.remove(&oldest);
                    }
                    None => break,
                }
            }
        }

        // Probe pending parities that *might* now be recoverable, in
        // insertion order (matches JS Map iteration order). Returns the
        // FIRST recoverable entry, so order matters when multiple entries
        // are recoverable.
        let keys: Vec<u32> = self.pending_parities.iter().map(|(k, _)| *k).collect();
        for gfws in keys {
            if let Some(parity) = self.pending_parities.get(&gfws).cloned() {
                if let Some(result) = self.try_recover(&parity) {
                    self.pending_parities.remove(&gfws);
                    return Some(result);
                }
            }
        }
        None
    }

    pub fn receive_parity(&mut self, env: &TileParityEnvelope) -> Option<Vec<u8>> {
        let result = self.try_recover(env);
        if result.is_none() {
            // Matches JS `Map.set` on an existing key: updates the value in
            // place, keeping its original insertion position.
            self.pending_parities
                .set(env.group_first_wire_seq, env.clone());
        }
        result
    }

    fn try_recover(&self, parity: &TileParityEnvelope) -> Option<Vec<u8>> {
        let mut missing: Option<u32> = None;
        let mut missing_count = 0usize;
        let mut received: Vec<&Vec<u8>> = Vec::new();
        for i in 0..parity.k as u32 {
            let ws = parity.group_first_wire_seq + i;
            match self.window.get(&ws) {
                None => {
                    missing_count += 1;
                    missing = Some(ws);
                }
                Some(src) => received.push(src),
            }
        }
        if missing_count != 1 {
            return None;
        }
        let _ = missing;

        let target_len = received
            .iter()
            .map(|s| s.len())
            .fold(parity.parity_payload.len(), std::cmp::max);
        let mut out = vec![0u8; target_len];
        xor_into(&mut out, &parity.parity_payload);
        for src in received {
            xor_into(&mut out, src);
        }
        Some(out)
    }
}
