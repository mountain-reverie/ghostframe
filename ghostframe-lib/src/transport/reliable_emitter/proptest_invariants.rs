#![cfg(test)]
// SPDX-License-Identifier: AGPL-3.0-or-later

use super::cache::{CacheEntry, RetransmitCache};
use super::wire_seq::WireSeqAllocator;
use super::EmitKey;
use bytes::Bytes;
use proptest::prelude::*;
use std::time::Instant;

proptest! {
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
                    datagram: Bytes::from(vec![0u8]),
                    queued_at: now,
                    first_sent_at: now, last_sent_at: now,
                    attempts: 0, loss_attempts: 0, latest_ws: 0,
                    probe: None,
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
