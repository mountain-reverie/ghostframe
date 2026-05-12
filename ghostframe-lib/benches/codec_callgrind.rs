//! Deterministic instruction-count tracking for protocol primitives.
//!
//! iai-callgrind runs each function under valgrind --tool=callgrind and
//! reports instruction counts that don't vary between runs — perfect for
//! catching regressions in CI without flakiness.
//!
//! Codecs with non-deterministic codepaths (VA-API, GPU compute) are NOT
//! benchmarked here — they go in `codec_latency.rs` under Criterion.
//!
//! Note: the fragment/FEC benches allocate output Vecs inside the measured
//! body. This is intentional — the goal is to detect regressions in the
//! full call cost including allocator pressure. If a future change wants
//! to isolate the serialization kernel from allocation cost, switch to
//! `#[bench::name(setup_fn())]` per the iai-callgrind setup-fn pattern.

use iai_callgrind::{library_benchmark, library_benchmark_group, main};

use ghostframe_lib::transport::fec;
use ghostframe_lib::transport::protocol::{
    fragment_frame, fragment_tile, max_fragment_payload, Codec,
    DatagramHeader, FrameHeader, TileHeader,
    DATAGRAM_HEADER_SIZE, FRAME_HEADER_SIZE, TILE_HEADER_SIZE,
};

#[library_benchmark]
fn datagram_header_encode_decode() {
    let h = DatagramHeader {
        frame_seq: 1024,
        frag_idx: 3,
        frag_total: 8,
        timestamp_us: 16_666,
    };
    let mut buf = Vec::with_capacity(DATAGRAM_HEADER_SIZE);
    h.encode(&mut buf);
    std::hint::black_box(DatagramHeader::decode(&buf).unwrap());
}

#[library_benchmark]
fn frame_header_encode_decode() {
    let h = FrameHeader {
        frame_seq: 1024,
        frag_idx: 3,
        frag_total: 8,
        timestamp_us: 16_666,
        flags: 1,
        reserved: 0,
    };
    let mut buf = Vec::with_capacity(FRAME_HEADER_SIZE);
    h.encode(&mut buf);
    std::hint::black_box(FrameHeader::decode(&buf).unwrap());
}

#[library_benchmark]
fn tile_header_encode_decode() {
    let h = TileHeader {
        tile_x: 7,
        tile_y: 3,
        codec: Codec::H264,
        lz4: false,
        generation: 1,
        pass: 0,
        payload_len: 1024,
    };
    let mut buf = Vec::with_capacity(TILE_HEADER_SIZE);
    h.encode(&mut buf);
    std::hint::black_box(TileHeader::decode(&buf).unwrap());
}

#[library_benchmark]
fn fragment_tile_4kb() {
    let payload = vec![0u8; 4096];
    let max = max_fragment_payload(1200);
    std::hint::black_box(fragment_tile(0u32, 0u8, 0u8, Codec::H264, 0u8, 0u8, &payload, 0u32, max));
}

#[library_benchmark]
fn fragment_frame_64kb() {
    let payload = vec![0u8; 65536];
    std::hint::black_box(fragment_frame(0u32, 0u32, false, &payload, 1186));
}

#[library_benchmark]
fn fec_parity_k4() {
    let frags: Vec<Vec<u8>> = (0..4).map(|i| vec![i as u8; 1186]).collect();
    let refs: Vec<&[u8]> = frags.iter().map(|f| f.as_slice()).collect();
    std::hint::black_box(fec::xor_payloads(&refs, 1186));
}

library_benchmark_group!(
    name = protocol;
    benchmarks =
        datagram_header_encode_decode,
        frame_header_encode_decode,
        tile_header_encode_decode,
        fragment_tile_4kb,
        fragment_frame_64kb,
        fec_parity_k4
);

main!(library_benchmark_groups = protocol);
