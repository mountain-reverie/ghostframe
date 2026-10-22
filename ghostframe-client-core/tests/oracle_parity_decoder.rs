//! Oracle port of ghostframe-web-client/tests/parity_decoder.test.ts
//! (wire-seq FEC).

use ghostframe_client_core::parity_decoder::ParityDecoder;
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
        source_lens: vec![sources[0].len() as u16; FEC_K as usize],
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
        source_lens: vec![sources[0].len() as u16; FEC_K as usize],
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
        source_lens: vec![sources[0].len() as u16; FEC_K as usize],
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
        source_lens: vec![sources[0].len() as u16; FEC_K as usize],
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

    // Payloads are 4 bytes (not 2): `try_recover` now validates a
    // reconstruction with `is_tile_datagram`, which requires at least 4
    // bytes, and 0xAA's top bit doubles as the tile-datagram flag.
    //
    // Group A (inserted FIRST): k=2, covers wire_seq {100, 101}. Neither is
    // present yet, so this is buffered (missing_count == 2).
    let group_a = TileParityEnvelope {
        group_first_wire_seq: 100,
        k: 2,
        parity_idx: 0,
        source_lens: vec![4, 4],
        parity_payload: vec![0xAA, 0xAA, 0xAA, 0xAA],
    };
    assert_eq!(decoder.receive_parity(&group_a), None);

    // Group B (inserted SECOND): k=2, covers wire_seq {99, 100}. Neither is
    // present yet either, so this is also buffered.
    let group_b = TileParityEnvelope {
        group_first_wire_seq: 99,
        k: 2,
        parity_idx: 0,
        source_lens: vec![4, 4],
        parity_payload: vec![0x00, 0x00, 0x00, 0x00],
    };
    assert_eq!(decoder.receive_parity(&group_b), None);

    // Recording wire_seq 100 leaves group A missing only 101 (count=1) and
    // group B missing only 99 (count=1) — BOTH become recoverable.
    // Insertion order (group A first) must win.
    let recovered = decoder.record_source(100, &[0x00, 0x00, 0x00, 0x00]);
    assert_eq!(recovered, Some(vec![0xAA, 0xAA, 0xAA, 0xAA]));
}
