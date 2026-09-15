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
    //
    // Bounded by `prune_pending`. Every operation on an `OrderedMap` is a
    // linear scan and `record_source` probes the whole set on *every*
    // source datagram, so an unbounded set makes per-datagram cost grow
    // with session length.
    pending_parities: OrderedMap<u32, TileParityEnvelope>,
    /// Highest `wire_seq` ever recorded, which is what `window`'s retention
    /// horizon is measured back from.
    newest_wire_seq: Option<u32>,
    window_capacity: usize,
}

impl ParityDecoder {
    pub fn new(window_capacity: usize) -> Self {
        ParityDecoder {
            window: HashMap::new(),
            order: VecDeque::new(),
            pending_parities: OrderedMap::new(),
            newest_wire_seq: None,
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
        self.newest_wire_seq = Some(match self.newest_wire_seq {
            Some(prev) => prev.max(wire_seq),
            None => wire_seq,
        });
        if let std::collections::hash_map::Entry::Vacant(e) = self.window.entry(wire_seq) {
            e.insert(bytes.to_vec());
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
        //
        // The recoverable entry is found under a shared borrow and removed
        // afterwards, rather than cloning each envelope to release the
        // borrow first: this runs per source datagram, and the envelopes
        // carry a full parity payload.
        let mut recovered: Option<(u32, Vec<u8>)> = None;
        for (gfws, parity) in self.pending_parities.iter() {
            if let Some(result) = self.try_recover(parity) {
                recovered = Some((*gfws, result));
                break;
            }
        }
        let (gfws, result) = recovered?;
        self.pending_parities.remove(&gfws);
        Some(result)
    }

    pub fn receive_parity(&mut self, env: &TileParityEnvelope) -> Option<Vec<u8>> {
        let result = self.try_recover(env);
        if result.is_none() {
            // Matches JS `Map.set` on an existing key: updates the value in
            // place, keeping its original insertion position.
            self.pending_parities
                .set(env.group_first_wire_seq, env.clone());
            // This is the only place the pending set grows, so bounding it
            // here bounds it everywhere.
            self.prune_pending();
        }
        result
    }

    /// Drop buffered parities that can no longer recover anything.
    ///
    /// `try_recover` needs exactly one of a group's `k` sources to be
    /// missing from `window`. `window` retains only the newest
    /// `window_capacity` sources, so once a group's last `wire_seq` has
    /// fallen behind that horizon every source it covers has been evicted,
    /// leaving `k` missing rather than one. Nothing can bring such a group
    /// back: the sources are gone from the window for good.
    fn prune_pending(&mut self) {
        if let Some(newest) = self.newest_wire_seq {
            let horizon = newest.saturating_sub(self.window_capacity as u32);
            self.pending_parities.retain(|gfws, env| {
                // `k` is wire-supplied; a malformed 0 must not underflow.
                let group_last = gfws.saturating_add(u32::from(env.k).saturating_sub(1));
                group_last >= horizon
            });
        }

        // Backstop. The horizon above is computed from `wire_seq`, which is
        // chosen by the peer and wraps at `u32::MAX`; neither a wrap nor a
        // peer that simply never lets a group complete may grow this set
        // without limit. `window_capacity` groups is already generous — a
        // group spans several sources, so this covers far more traffic than
        // the source window itself retains.
        while self.pending_parities.len() > self.window_capacity {
            self.pending_parities.remove_oldest();
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A source datagram is opaque to the decoder — it only ever XORs them
    /// — so a fixed-length filler keyed on `wire_seq` is enough.
    fn src(wire_seq: u32) -> Vec<u8> {
        vec![(wire_seq & 0xFF) as u8; 8]
    }

    /// Parity over the whole group, so the group becomes recoverable the
    /// moment exactly one of its `k` sources is still missing.
    fn parity_for(group_first: u32, k: u8) -> TileParityEnvelope {
        let mut payload = vec![0u8; 8];
        for i in 0..k as u32 {
            xor_into(&mut payload, &src(group_first + i));
        }
        TileParityEnvelope {
            group_first_wire_seq: group_first,
            k,
            parity_idx: 0,
            group_first_payload_len: 8,
            parity_payload: payload,
        }
    }

    #[test]
    fn a_pending_parity_whose_group_has_aged_out_is_pruned() {
        let cap = 8;
        let mut d = ParityDecoder::new(cap);

        // Two of the group's four sources are missing, so the parity cannot
        // resolve yet and gets buffered.
        d.record_source(0, &src(0));
        d.record_source(1, &src(1));
        assert!(d.receive_parity(&parity_for(0, 4)).is_none());
        assert_eq!(d.pending_parities.len(), 1);

        // Slide the source window far past that group. Every source it
        // covers has now been evicted from `window`, so no later arrival can
        // ever bring it back to exactly-one-missing.
        for ws in 100..100 + (cap as u32 * 3) {
            d.record_source(ws, &src(ws));
        }
        // Pruning runs when the pending set grows, i.e. on parity arrival.
        assert!(d.receive_parity(&parity_for(500, 4)).is_none());

        assert_eq!(
            d.pending_parities.len(),
            1,
            "the aged-out group must be dropped, leaving only the new parity"
        );
        assert!(
            d.pending_parities.get(&0).is_none(),
            "the aged-out group's own entry must be the one dropped"
        );
    }

    #[test]
    fn the_pending_set_stays_bounded_under_sustained_unrecoverable_parities() {
        let cap = 8;
        let mut d = ParityDecoder::new(cap);

        // Every one of these parities is unrecoverable on arrival (none of
        // their sources were ever recorded), which is exactly the case that
        // used to accumulate without limit.
        for g in (0..4_000u32).step_by(4) {
            d.record_source(g, &src(g));
            assert!(d.receive_parity(&parity_for(g, 4)).is_none());
        }

        assert!(
            d.pending_parities.len() <= cap,
            "pending parities must stay bounded, found {}",
            d.pending_parities.len()
        );
    }

    #[test]
    fn a_parity_still_inside_the_window_survives_pruning_and_recovers() {
        // The guard against over-eager eviction: pruning must not touch a
        // group whose sources are still in the window, or FEC silently
        // stops recovering anything.
        let cap = 32;
        let mut d = ParityDecoder::new(cap);

        // Group [0,4): sources 0 and 1 present, 2 and 3 missing -> buffered.
        d.record_source(0, &src(0));
        d.record_source(1, &src(1));
        assert!(d.receive_parity(&parity_for(0, 4)).is_none());

        // Unrelated traffic arrives, but not enough to age the group out.
        for ws in 10..20 {
            d.record_source(ws, &src(ws));
        }
        assert!(d.receive_parity(&parity_for(10, 4)).is_none());

        // Source 2 lands, leaving exactly one missing: the buffered parity
        // must still be there to recover source 3.
        let recovered = d.record_source(2, &src(2));
        assert_eq!(
            recovered,
            Some(src(3)),
            "a group still inside the window must still recover"
        );
    }
}
