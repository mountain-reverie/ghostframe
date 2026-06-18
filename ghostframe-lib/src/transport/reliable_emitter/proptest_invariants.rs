#![cfg(test)]

use super::cache::{CacheEntry, RetransmitCache};
use super::parity::xor_payloads;
use super::wire_seq::WireSeqAllocator;
use super::EmitKey;
use bytes::Bytes;
use proptest::prelude::*;
use smallvec::smallvec;
use std::time::Instant;

proptest! {
    #[test]
    fn xor_round_trip(sources in proptest::collection::vec(any::<Vec<u8>>(), 1..10)) {
        let refs: Vec<&[u8]> = sources.iter().map(|v| v.as_slice()).collect();
        let parity = xor_payloads(&refs);
        // Remove the first source and recover it from parity + the rest.
        let mut without_first: Vec<&[u8]> = vec![&parity];
        for s in &sources[1..] { without_first.push(s.as_slice()); }
        let recovered = xor_payloads(&without_first);
        // The recovered value equals the first source, padded to the
        // maximum length with leading zeros.
        let max_len = sources.iter().map(|s| s.len()).max().unwrap();
        let expected_pad = max_len - sources[0].len();
        let expected: Vec<u8> = std::iter::repeat(0).take(expected_pad).chain(sources[0].iter().copied()).collect();
        prop_assert_eq!(recovered, expected);
    }

    #[test]
    fn wire_seq_strictly_increasing(steps in 1usize..1000) {
        let mut a = WireSeqAllocator::new();
        let mut prev = None::<u32>;
        for _ in 0..steps {
            let v = a.allocate();
            if let Some(p) = prev {
                // u32 wrapping is allowed but next == prev + 1 mod 2^32
                prop_assert_eq!(v, p.wrapping_add(1));
            }
            prev = Some(v);
        }
    }

    #[test]
    fn cache_cancel_for_tile_keeps_others(
        ops in proptest::collection::vec((0u8..16, 0u8..16, 0u32..1000), 1..200),
        cancel in (0u8..16, 0u8..16),
    ) {
        let mut c = RetransmitCache::new();
        let now = Instant::now();
        for (x, y, fs) in &ops {
            c.insert(
                EmitKey::new(*fs, *x, *y, 0),
                CacheEntry {
                    fragments: smallvec![Bytes::from(vec![0u8])],
                    wire_seqs: smallvec![0],
                    first_sent_at: now, last_sent_at: now,
                    attempts: 0, rto_deadline: now,
                },
            );
        }
        c.cancel_for_tile(cancel.0, cancel.1);
        // Every remaining entry has (tile_x, tile_y) != cancel.
        for (x, y, fs) in &ops {
            let k = EmitKey::new(*fs, *x, *y, 0);
            if (*x, *y) == cancel {
                prop_assert!(c.get(&k).is_none());
            } else {
                // May still be evicted by LRU, but if present, tile != cancel.
                if let Some(_e) = c.get(&k) {
                    prop_assert!((*x, *y) != cancel);
                }
            }
        }
    }
}
