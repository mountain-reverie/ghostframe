//! `forward_cdf53_tiles`: the GPU forward transform for tiles the frame's own
//! compact pass did not list. The oracle is the CPU `cdf53::forward`, which
//! the lossless contract is defined against: exact, never toleranced.

use super::*;
use crate::encoder::cdf53;

const NEEDS_GPU: &str =
    "GpuFrameProcessor::new failed: this test needs a Vulkan GPU and must not skip";

/// Deterministic tile content with every channel exercised: a gradient
/// perturbed by an xorshift stream, so adjacent tiles differ.
fn tile(seed: u64) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    (0..32u32 * 32)
        .flat_map(|i| {
            let (x, y) = (i % 32, i / 32);
            let n = (next() % 41) as u32;
            [
                ((x * 8 + n) % 256) as u8,
                ((y * 8 + n * 3) % 256) as u8,
                (((x + y) * 4 + n * 5) % 256) as u8,
                255,
            ]
        })
        .collect()
}

fn check(n: usize) {
    let mut gpu = GpuFrameProcessor::new(256).expect(NEEDS_GPU);
    let tiles: Vec<Vec<u8>> = (0..n as u64).map(tile).collect();
    let refs: Vec<&[u8]> = tiles.iter().map(|t| t.as_slice()).collect();
    let got = gpu.forward_cdf53_tiles(&refs).expect("forward failed");
    assert_eq!(got.len(), n * cdf53::CDF53_TOTAL_COEFFS);
    let mut bad = 0;
    for (i, t) in tiles.iter().enumerate() {
        let want = cdf53::forward(t);
        let have = &got[i * cdf53::CDF53_TOTAL_COEFFS..(i + 1) * cdf53::CDF53_TOTAL_COEFFS];
        if have != want.as_slice() {
            bad += 1;
        }
    }
    assert_eq!(bad, 0, "{bad}/{n} tiles differ from the CPU forward");
}

#[test]
fn one_tile_matches_the_cpu_forward_exactly() {
    check(1);
}

/// More than one row of the staging layout (32 per row) and a partial last row.
#[test]
fn a_multi_row_batch_matches_the_cpu_forward_exactly() {
    check(70);
}

/// Crosses the chunk boundary, so the second submit reuses the buffers.
#[test]
fn a_batch_over_one_chunk_matches_the_cpu_forward_exactly() {
    check(MAX_ESCALATION_PER_FRAME + 40);
}

#[test]
fn a_wrongly_sized_tile_is_rejected() {
    let mut gpu = GpuFrameProcessor::new(256).expect(NEEDS_GPU);
    let short = vec![0u8; 100];
    assert!(gpu.forward_cdf53_tiles(&[&short]).is_err());
}

/// Round-trip cost of `forward_cdf53_tiles`, for the PR description. Prints
/// median and spread over 15 runs per size. `#[ignore]`d: a measurement, not
/// a gate. Run:
/// `gf-gpu-run cargo test -p ghostframe-lib --lib fallback_round_trip -- --ignored --nocapture`
#[test]
#[ignore]
fn fallback_round_trip_cost() {
    let mut gpu = GpuFrameProcessor::new(256).expect(NEEDS_GPU);
    let tiles: Vec<Vec<u8>> = (0..8160u64).map(tile).collect();
    for (label, n) in [
        ("few (8 tiles)", 8usize),
        ("64 tiles", 64),
        ("512 tiles (one chunk)", 512),
        ("1080p full screen (2040 tiles)", 2040),
        ("4K full screen (8160 tiles)", 8160),
    ] {
        let refs: Vec<&[u8]> = tiles[..n].iter().map(|t| t.as_slice()).collect();
        let mut ms: Vec<f64> = (0..15)
            .map(|_| {
                let t0 = std::time::Instant::now();
                let out = gpu.forward_cdf53_tiles(&refs).unwrap();
                assert_eq!(out.len(), n * cdf53::CDF53_TOTAL_COEFFS);
                #[allow(
                    clippy::disallowed_methods,
                    reason = "deliberate wall-clock measurement of real GPU round-trip time; no tokio clock in this test"
                )]
                let e = t0.elapsed();
                e.as_secs_f64() * 1e3
            })
            .collect();
        ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "FALLBACK {label}: median {:.2} ms  min {:.2}  max {:.2}  ({:.1} us/tile)",
            ms[7],
            ms[0],
            ms[14],
            ms[7] * 1e3 / n as f64
        );
    }
}

/// The production tile ceiling must actually allocate on the host GPU: it sets
/// ~200 MB of host-visible buffers, and a failure here is a startup failure.
#[test]
fn the_production_tile_ceiling_allocates() {
    let p = GpuFrameProcessor::new(MAX_FRAME_TILES).expect(NEEDS_GPU);
    assert!(p.max_tiles >= 8160 * 2, "must hold two 4K screens");
}
