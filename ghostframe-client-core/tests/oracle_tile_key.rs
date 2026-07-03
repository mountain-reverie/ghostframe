//! Port of `ghostframe-web-client/tests/tile_key.test.ts`.
//!
//! In the Rust port, tile assemblies are keyed by the `TileKey` struct
//! (frame_seq, tile_x, tile_y, pass_idx) used directly as a `HashMap` key
//! rather than a `"frameSeq:x:y:pass"` string. These tests pin the
//! equality / hashing semantics that the datagram reassembler relies on:
//! two passes of the same tile must occupy distinct buckets, and distinct
//! frame_seqs must too.

use std::collections::HashMap;

use ghostframe_client_core::TileKey;

fn k(frame_seq: u32, x: u8, y: u8, pass: u8) -> TileKey {
    TileKey {
        frame_seq,
        tile_x: x,
        tile_y: y,
        pass_idx: pass,
    }
}

// ---- describe('tileKey') ----------------------------------------------

#[test]
fn keys_for_two_passes_of_same_tile_are_distinct() {
    // tileKey(100, 5, 6, 0) != tileKey(100, 5, 6, 1)
    assert_ne!(k(100, 5, 6, 0), k(100, 5, 6, 1));
}

#[test]
fn keys_for_same_tuple_are_equal() {
    // tileKey(7, 3, 4, 9) == tileKey(7, 3, 4, 9)
    assert_eq!(k(7, 3, 4, 9), k(7, 3, 4, 9));
}

#[test]
fn keys_with_same_coords_but_different_frame_seq_stay_distinct() {
    assert_ne!(k(1, 2, 3, 4), k(2, 2, 3, 4));
}

// ---- describe('tile assembly bucket isolation') -----------------------

#[test]
fn two_passes_for_same_tile_in_same_frame_seq_own_separate_buckets() {
    // Models the cross-pass collision regression: pass 0 and pass 1 of
    // the same tile both have multi-fragment payloads and interleave.
    type Bucket = (Vec<Option<Vec<u8>>>, usize);
    let mut map: HashMap<TileKey, Bucket> = HashMap::new();
    let frame_seq = 0x1234;
    let (tx, ty) = (10u8, 30u8);

    let k0 = k(frame_seq, tx, ty, 0);
    map.insert(k0, (vec![Some(vec![0xAA]), None], 1));

    // Pass 1 frag 0 arrives BEFORE pass 0 frag 1 lands.
    let k1 = k(frame_seq, tx, ty, 1);
    map.insert(k1, (vec![Some(vec![0xBB]), None], 1));

    assert_eq!(map.len(), 2);
    assert_eq!(map.get(&k0).unwrap().0[0], Some(vec![0xAA]));
    assert_eq!(map.get(&k1).unwrap().0[0], Some(vec![0xBB]));
}

#[test]
fn distinct_frame_seq_occupy_distinct_entries() {
    let mut map: HashMap<TileKey, u8> = HashMap::new();
    map.insert(k(1, 2, 3, 0), 1);
    map.insert(k(2, 2, 3, 0), 2);
    map.insert(k(3, 2, 3, 0), 3);
    assert_eq!(map.len(), 3);
}
