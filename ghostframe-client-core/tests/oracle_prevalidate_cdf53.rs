//! Port of `ghostframe-web-client/tests/prevalidate_cdf53.test.ts`
//! (5 rleDecode cases + 4 prevalidateCdf53 cases), plus a fixture walk test.

use ghostframe_client_core::cdf53_prevalidate::prevalidate_cdf53;
use ghostframe_client_core::DecodeErrorCode;
use ghostframe_protocol::codec::cdf53::rle_decode;

// --- rleDecode ---

#[test]
fn all_zero_token_decodes_to_128_zero_bytes() {
    let out = rle_decode(&[0xFF]);
    assert_eq!(out.len(), 128);
    assert!(out.iter().all(|&b| b == 0));
}

#[test]
fn two_zero_runs_concatenate() {
    let out = rle_decode(&[0x80, 0x81]);
    assert_eq!(out, vec![0, 0, 0]);
}

#[test]
fn literal_bytes_pass_through() {
    let out = rle_decode(&[0x05, 0x42, 0x10]);
    assert_eq!(out, vec![0x05, 0x42, 0x10]);
}

#[test]
fn escape_emits_next_byte_literally() {
    let out = rle_decode(&[0x7F, 0x80, 0x05]);
    assert_eq!(out, vec![0x80, 0x05]);
}

#[derive(serde::Deserialize)]
struct Fixture {
    pass_count: usize,
    channels: usize,
    encoded_passes: Vec<Vec<u8>>,
    bit_planes_per_pass: Vec<Vec<Vec<u8>>>,
}

fn load_fixture() -> Fixture {
    let s = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../ghostframe-e2e/src/harness/fixtures/cdf53_fixture.json"
    ))
    .expect("fixture file readable");
    serde_json::from_str(&s).expect("fixture parses")
}

#[test]
fn rle_decode_matches_fixture_for_all_passes_and_channels() {
    let fixture = load_fixture();
    for pass in 0..fixture.pass_count {
        let payload = &fixture.encoded_passes[pass];
        let mut offset = 0usize;
        for ch in 0..fixture.channels {
            let len = ((payload[offset] as usize) << 8) | (payload[offset + 1] as usize);
            offset += 2;
            let rle = &payload[offset..offset + len];
            offset += len;
            let decoded = rle_decode(rle);
            assert_eq!(decoded.len(), 128);
            assert_eq!(decoded, fixture.bit_planes_per_pass[pass][ch]);
        }
    }
}

// --- prevalidateCdf53 ---

#[test]
fn happy_path_roundtrip_vs_fixture() {
    let fixture = load_fixture();
    for pass_idx in 0..fixture.pass_count {
        let payload = &fixture.encoded_passes[pass_idx];
        let r = prevalidate_cdf53(payload, 1, pass_idx as u8).expect("ok");
        assert_eq!(r.generation, 1);
        assert_eq!(r.pass_idx, pass_idx as u8);
        assert_eq!(r.bit_planes.len(), 384);
        for ch in 0..3 {
            let expected = &fixture.bit_planes_per_pass[pass_idx][ch];
            assert_eq!(&r.bit_planes[ch * 128..ch * 128 + 128], expected.as_slice());
        }
    }
}

#[test]
fn rejects_pass_idx_ge_14_with_cdf53_bad_pass() {
    let r = prevalidate_cdf53(&[0, 1, 0xFF], 0, 14);
    assert_eq!(r, Err(DecodeErrorCode::Cdf53BadPass));
}

#[test]
fn rejects_truncated_section_header_with_cdf53_truncated() {
    let r = prevalidate_cdf53(&[0], 0, 0);
    assert_eq!(r, Err(DecodeErrorCode::Cdf53Truncated));
}

#[test]
fn rejects_rle_wrong_byte_count_with_cdf53_rle_length() {
    let r = prevalidate_cdf53(&[0, 1, 0x05], 0, 0);
    assert_eq!(r, Err(DecodeErrorCode::Cdf53RleLength));
}
