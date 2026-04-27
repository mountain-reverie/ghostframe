//! Throughput of CPU-only pipeline primitives that run on every frame:
//!  * `DirtyTracker::update` (tile diff)
//!  * `protocol::fragment_tile` and `fragment_frame` (MTU split)
//!  * `fec::xor_payloads` (FEC parity generation across a fragment group)
//!
//! These have no GPU dependency, so they run on every CI machine.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use ghostframe_lib::tile::{DirtyTracker, BPP, TILE_SIZE};
use ghostframe_lib::transport::fec;
use ghostframe_lib::transport::protocol::{
    fragment_frame, fragment_tile, max_fragment_payload, Codec,
};

const FRAME_SIZES: &[(u32, u32)] = &[
    (640, 480),    // small
    (1280, 720),   // 720p
    (1920, 1080),  // 1080p
];

fn make_frame(w: u32, h: u32) -> Vec<u8> {
    // Cheap deterministic pattern — a packed BGRA frame of size w×h.
    let mut state: u32 = 0xDECAFBAD;
    let mut buf = vec![0u8; (w * h * BPP) as usize];
    for chunk in buf.chunks_exact_mut(4) {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        chunk.copy_from_slice(&state.to_le_bytes());
        chunk[3] = 0xFF;
    }
    buf
}

fn bench_dirty_tracker(c: &mut Criterion) {
    let mut group = c.benchmark_group("dirty_tracker");
    for &(w, h) in FRAME_SIZES {
        let frame = make_frame(w, h);
        group.throughput(Throughput::Bytes(frame.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{w}x{h}")),
            &frame,
            |b, f| {
                let cols = w.div_ceil(TILE_SIZE);
                let rows = h.div_ceil(TILE_SIZE);
                let mut tracker = DirtyTracker::new(cols, rows);
                // Settle the tracker so we measure the steady-state path,
                // not the all-dirty first frame.
                let _ = tracker.update(f, w * BPP, w, h);
                b.iter(|| {
                    black_box(tracker.update(f, w * BPP, w, h));
                });
            },
        );
    }
    group.finish();
}

fn bench_fragment_tile(c: &mut Criterion) {
    let mut group = c.benchmark_group("fragment_tile");
    let payload_sizes = [256usize, 1024, 4096, 16384];
    for &n in &payload_sizes {
        let payload = vec![0u8; n];
        group.throughput(Throughput::Bytes(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &payload, |b, p| {
            let max_payload = max_fragment_payload(1200);
            b.iter(|| {
                // fragment_tile(frame_seq, tile_x, tile_y, codec, payload, timestamp_us, max_payload)
                black_box(fragment_tile(0u32, 0u8, 0u8, Codec::H264, p, 0u32, max_payload));
            });
        });
    }
    group.finish();
}

fn bench_fragment_frame(c: &mut Criterion) {
    let mut group = c.benchmark_group("fragment_frame");
    let payload_sizes = [4096usize, 32768, 131072];
    for &n in &payload_sizes {
        let payload = vec![0u8; n];
        group.throughput(Throughput::Bytes(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &payload, |b, p| {
            b.iter(|| {
                black_box(fragment_frame(0u32, 0u32, false, p, 1186));
            });
        });
    }
    group.finish();
}

fn bench_fec_parity(c: &mut Criterion) {
    let mut group = c.benchmark_group("fec_parity");

    // Simulate a frame split across 64 fragments of 1186 bytes each.
    // K values track production: fec_group_size(true) = I-frame K,
    // fec_group_size(false) = P-frame K. Benching production constants
    // means the numbers stay representative if those defaults change.
    let frags: Vec<Vec<u8>> = (0..64).map(|_| vec![0u8; 1186]).collect();
    let total_bytes: u64 = frags.iter().map(|f| f.len() as u64).sum();

    for (label, is_keyframe) in [("iframe", true), ("pframe", false)] {
        let k = fec::fec_group_size(is_keyframe);
        group.throughput(Throughput::Bytes(total_bytes));
        group.bench_with_input(BenchmarkId::from_parameter(label), &k, |b, &k| {
            b.iter(|| {
                for chunk in frags.chunks(k) {
                    let refs: Vec<&[u8]> = chunk.iter().map(|v| v.as_slice()).collect();
                    black_box(fec::xor_payloads(&refs, 1186));
                }
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_dirty_tracker,
    bench_fragment_tile,
    bench_fragment_frame,
    bench_fec_parity
);
criterion_main!(benches);
