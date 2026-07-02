//! Oracle port of ghostframe-web-client/tests/parity_decoder.test.ts
//! (wire-seq FEC) plus direct tests derived from the fec.ts contract
//! (legacy per-tile fragment FEC, no dedicated vitest file exists).

use ghostframe_client_core::fragment_parity::FragmentParity;
use ghostframe_client_core::parity_decoder::ParityDecoder;
use ghostframe_client_core::TileKey;
use ghostframe_protocol::protocol::TileParityEnvelope;

const FEC_K: u32 = 10;

/// Mirrors `fakeSource` in parity_decoder.test.ts: 16-byte DatagramHeader +
/// 8-byte TileHeader + 1-byte payload.
fn fake_source(wire_seq: u32, payload: u8) -> Vec<u8> {
    let mut buf = vec![0u8; 25];
    buf[0..4].copy_from_slice(&(0x8000_0000u32 | wire_seq).to_be_bytes()); // frame_seq | TILE_DATAGRAM_FLAG
    buf[4..6].copy_from_slice(&0u16.to_be_bytes()); // frag_idx
    buf[6..8].copy_from_slice(&1u16.to_be_bytes()); // frag_total
    buf[8..12].copy_from_slice(&wire_seq.to_be_bytes()); // wire_seq
    buf[12..16].copy_from_slice(&0u32.to_be_bytes()); // timestamp_us
    // tile header bytes 16..23 left as 0
    buf[24] = payload;
    buf
}

/// Mirrors the vitest helper `xorBytes`: right-aligned XOR of all slices,
/// output length = max slice length.
fn xor_bytes(slices: &[Vec<u8>]) -> Vec<u8> {
    let max_len = slices.iter().map(|s| s.len()).max().unwrap_or(0);
    let mut out = vec![0u8; max_len];
    for s in slices {
        let pad = max_len - s.len();
        for (i, b) in s.iter().enumerate() {
            out[pad + i] ^= b;
        }
    }
    out
}

#[test]
fn recovers_single_missing_source() {
    let decoder_cap = (FEC_K * 4) as usize;
    let mut decoder = ParityDecoder::new(decoder_cap);
    let sources: Vec<Vec<u8>> = (0..FEC_K).map(|i| fake_source(i, i as u8)).collect();
    let parity = xor_bytes(&sources);

    for i in 0..FEC_K {
        if i != 5 {
            decoder.record_source(i, &sources[i as usize]);
        }
    }

    let envelope = TileParityEnvelope {
        group_first_wire_seq: 0,
        k: FEC_K as u8,
        parity_idx: 0,
        group_first_payload_len: sources[0].len() as u16,
        parity_payload: parity,
    };
    let recovered = decoder.receive_parity(&envelope);
    assert_eq!(recovered, Some(sources[5].clone()));
}

#[test]
fn returns_none_when_multiple_sources_missing() {
    let decoder_cap = (FEC_K * 4) as usize;
    let mut decoder = ParityDecoder::new(decoder_cap);
    let sources: Vec<Vec<u8>> = (0..FEC_K).map(|i| fake_source(i, i as u8)).collect();
    let parity = xor_bytes(&sources);

    for i in 0..(FEC_K - 2) {
        decoder.record_source(i, &sources[i as usize]);
    }

    let envelope = TileParityEnvelope {
        group_first_wire_seq: 0,
        k: FEC_K as u8,
        parity_idx: 0,
        group_first_payload_len: sources[0].len() as u16,
        parity_payload: parity,
    };
    let recovered = decoder.receive_parity(&envelope);
    assert_eq!(recovered, None);
}

#[test]
fn returns_none_when_no_sources_missing() {
    let decoder_cap = (FEC_K * 4) as usize;
    let mut decoder = ParityDecoder::new(decoder_cap);
    let sources: Vec<Vec<u8>> = (0..FEC_K).map(|i| fake_source(i, i as u8)).collect();
    let parity = xor_bytes(&sources);

    for i in 0..FEC_K {
        decoder.record_source(i, &sources[i as usize]);
    }

    let envelope = TileParityEnvelope {
        group_first_wire_seq: 0,
        k: FEC_K as u8,
        parity_idx: 0,
        group_first_payload_len: sources[0].len() as u16,
        parity_payload: parity,
    };
    let recovered = decoder.receive_parity(&envelope);
    assert_eq!(recovered, None);
}

#[test]
fn evicts_oldest_sources_when_window_full() {
    let mut decoder = ParityDecoder::new(4);
    decoder.record_source(0, &[1]);
    decoder.record_source(1, &[2]);
    decoder.record_source(2, &[3]);
    decoder.record_source(3, &[4]);
    decoder.record_source(4, &[5]); // evicts wire_seq 0
    assert!(!decoder.has_source(0));
    assert!(decoder.has_source(4));
}

#[test]
fn replays_buffered_parity_when_missing_source_finally_arrives() {
    let decoder_cap = (FEC_K * 4) as usize;
    let mut decoder = ParityDecoder::new(decoder_cap);
    let sources: Vec<Vec<u8>> = (0..FEC_K).map(|i| fake_source(i, i as u8)).collect();
    let parity = xor_bytes(&sources);

    // Feed K-2 sources, leaving indices K-2 and K-1 BOTH missing.
    for i in 0..(FEC_K - 2) {
        decoder.record_source(i, &sources[i as usize]);
    }

    let envelope = TileParityEnvelope {
        group_first_wire_seq: 0,
        k: FEC_K as u8,
        parity_idx: 0,
        group_first_payload_len: sources[0].len() as u16,
        parity_payload: parity,
    };

    // Parity arrives with 2 missing — buffer, return None.
    assert_eq!(decoder.receive_parity(&envelope), None);

    // Add source K-2 — now only K-1 is missing. The buffered parity
    // replays and recovers sources[K-1].
    let recovered = decoder.record_source(FEC_K - 2, &sources[(FEC_K - 2) as usize]);
    assert_eq!(recovered, Some(sources[(FEC_K - 1) as usize].clone()));
}

#[test]
fn recovers_first_inserted_pending_parity_when_multiple_become_recoverable() {
    // Pins order-dependence: `pending_parities` must be insertion-ordered
    // (matching the TS `Map`), and `record_source` must probe it in
    // insertion order, returning the FIRST recoverable entry — even when a
    // later-inserted entry is also recoverable.
    let mut decoder = ParityDecoder::new(64);

    // Group A (inserted FIRST): k=2, covers wire_seq {100, 101}. Neither is
    // present yet, so this is buffered (missing_count == 2).
    let group_a = TileParityEnvelope {
        group_first_wire_seq: 100,
        k: 2,
        parity_idx: 0,
        group_first_payload_len: 2,
        parity_payload: vec![0xAA, 0xAA],
    };
    assert_eq!(decoder.receive_parity(&group_a), None);

    // Group B (inserted SECOND): k=2, covers wire_seq {99, 100}. Neither is
    // present yet either, so this is also buffered.
    let group_b = TileParityEnvelope {
        group_first_wire_seq: 99,
        k: 2,
        parity_idx: 0,
        group_first_payload_len: 2,
        parity_payload: vec![0x00, 0x00],
    };
    assert_eq!(decoder.receive_parity(&group_b), None);

    // Recording wire_seq 100 leaves group A missing only 101 (count=1) and
    // group B missing only 99 (count=1) — BOTH become recoverable.
    // Insertion order (group A first) must win.
    let recovered = decoder.record_source(100, &[0x00, 0x00]);
    assert_eq!(recovered, Some(vec![0xAA, 0xAA]));
}

// ---------------------------------------------------------------------------
// Legacy fragment FEC (fec.ts) — no dedicated vitest file exists upstream,
// so these tests are derived directly from the fec.ts contract:
//   parity payload = [group_start u16 BE][group_len u8][xor_data]
//   groups of k=4 fragments within a tile's fragment list
//   recovery only when exactly one fragment in the group is missing
//   zero-pad from index 0 (left-aligned XOR, unlike the wire-seq decoder)
// ---------------------------------------------------------------------------

fn key() -> TileKey {
    TileKey {
        frame_seq: 42,
        tile_x: 1,
        tile_y: 2,
        pass_idx: 0,
    }
}

fn parity_payload(group_start: u16, group_len: u8, xor_data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&group_start.to_be_bytes());
    buf.push(group_len);
    buf.extend_from_slice(xor_data);
    buf
}

#[test]
fn fragment_parity_recovers_single_missing_frag_in_group() {
    let frags: Vec<Vec<u8>> = vec![vec![1, 1], vec![2, 2], vec![3, 3], vec![4, 4]];
    let mut xor = vec![0u8; 2];
    for f in &frags {
        for (i, b) in f.iter().enumerate() {
            xor[i] ^= b;
        }
    }

    let mut fp = FragmentParity::new();
    fp.store(key(), &parity_payload(0, 4, &xor));

    let fragments: Vec<Option<Vec<u8>>> = vec![
        Some(frags[0].clone()),
        None, // missing index 1
        Some(frags[2].clone()),
        Some(frags[3].clone()),
    ];
    let (idx, recovered) = fp.try_recover(key(), &fragments).expect("should recover");
    assert_eq!(idx, 1);
    assert_eq!(recovered, frags[1]);
}

#[test]
fn fragment_parity_no_recovery_when_two_missing() {
    let frags: Vec<Vec<u8>> = vec![vec![1, 1], vec![2, 2], vec![3, 3], vec![4, 4]];
    let mut xor = vec![0u8; 2];
    for f in &frags {
        for (i, b) in f.iter().enumerate() {
            xor[i] ^= b;
        }
    }

    let mut fp = FragmentParity::new();
    fp.store(key(), &parity_payload(0, 4, &xor));

    let fragments: Vec<Option<Vec<u8>>> =
        vec![Some(frags[0].clone()), None, None, Some(frags[3].clone())];
    assert!(fp.try_recover(key(), &fragments).is_none());
}

#[test]
fn fragment_parity_group_start_and_group_len_windowing() {
    // Second group (indices 4..8) with its own parity, group_len=3 (only 3
    // fragments present in this partial tile).
    let frags: Vec<Vec<u8>> = vec![vec![10], vec![20], vec![30]];
    let mut xor = vec![0u8; 1];
    for f in &frags {
        xor[0] ^= f[0];
    }

    let mut fp = FragmentParity::new();
    fp.store(key(), &parity_payload(4, 3, &xor));

    let mut fragments: Vec<Option<Vec<u8>>> = vec![None; 8];
    fragments[4] = Some(frags[0].clone());
    fragments[5] = None; // missing
    fragments[6] = Some(frags[2].clone());

    let (idx, recovered) = fp.try_recover(key(), &fragments).expect("should recover");
    assert_eq!(idx, 5);
    assert_eq!(recovered, frags[1]);
}

#[test]
fn fragment_parity_remove_clears_stored_groups() {
    let mut fp = FragmentParity::new();
    fp.store(key(), &parity_payload(0, 4, &[0xFF]));
    fp.remove(&key());

    let fragments: Vec<Option<Vec<u8>>> =
        vec![Some(vec![1]), None, Some(vec![3]), Some(vec![4])];
    assert!(fp.try_recover(key(), &fragments).is_none());
}

#[test]
fn fragment_parity_recovers_first_inserted_group_when_multiple_ambiguous() {
    // Pins order-dependence: the per-tile group map must be
    // insertion-ordered (matching the TS `Map<number, ParityInfo>` in
    // `ParityRecovery`), and `try_recover` must scan groups in insertion
    // order, returning the FIRST group with exactly one missing fragment —
    // even when a later-inserted group is *also* recoverable.
    let frags: Vec<Vec<u8>> = vec![vec![1], vec![2], vec![3], vec![4], vec![5], vec![6], vec![7], vec![8]];

    // Group at group_start=0 (inserted FIRST), missing index 1. The parity
    // xor_data covers ALL fragments in the group (it is generated before
    // any loss, from the sender's full fragment set).
    let mut xor0 = vec![0u8];
    for f in &frags[0..4] {
        xor0[0] ^= f[0];
    }

    // Group at group_start=4 (inserted SECOND), missing index 5 — also
    // recoverable given the fragments below.
    let mut xor4 = vec![0u8];
    for f in &frags[4..8] {
        xor4[0] ^= f[0];
    }

    let mut fp = FragmentParity::new();
    fp.store(key(), &parity_payload(0, 4, &xor0));
    fp.store(key(), &parity_payload(4, 4, &xor4));

    let fragments: Vec<Option<Vec<u8>>> = vec![
        Some(frags[0].clone()),
        None, // missing index 1 (group 0)
        Some(frags[2].clone()),
        Some(frags[3].clone()),
        Some(frags[4].clone()),
        None, // missing index 5 (group 4) — also recoverable
        Some(frags[6].clone()),
        Some(frags[7].clone()),
    ];

    // Both group 0 and group 4 are independently recoverable; the
    // first-inserted group (group_start=0, missing idx=1) must win.
    let (idx, recovered) = fp.try_recover(key(), &fragments).expect("should recover");
    assert_eq!(idx, 1);
    assert_eq!(recovered, frags[1]);
}
