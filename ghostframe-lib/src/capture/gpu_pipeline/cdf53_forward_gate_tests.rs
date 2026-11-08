//! The server GPU's CDF 5/3 forward against the CPU oracle, `cdf53::forward`,
//! exactly. Two entry points: `forward_cdf53_tiles` (the escalation/fallback
//! dispatch) on adversarial tiles, and a full noise frame through
//! `process_frame_from_pixels` (the frame's own compact list, indirect
//! dispatch). Needs a Vulkan GPU; CI skips this module by name.

use super::*;
use crate::encoder::cdf53;

const NEEDS_GPU: &str =
    "GpuFrameProcessor::new failed: this test needs a Vulkan GPU and must not skip";

/// One 32x32 BGRA tile of pattern `kind` (0..=7); `seed` varies the noise, the
/// impulse's corner and the gradient's phase (the flat and striped patterns do
/// not depend on it).
/// The patterns hit the transform's edges: flat extremes, maximal
/// high-pass energy (checkerboard, stripes), a lone corner impulse
/// (mirror boundary), full-range noise and a gradient.
fn pattern(kind: u32, seed: u64) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut t = vec![0u8; 32 * 32 * 4];
    for i in 0..1024usize {
        let (x, y) = (i % 32, i / 32);
        for c in 0..3usize {
            t[i * 4 + c] = match kind {
                0 => 0,
                1 => 255,
                2 => {
                    if (x + y + c) % 2 == 0 {
                        255
                    } else {
                        0
                    }
                }
                3 => {
                    if x % 2 == 0 {
                        255
                    } else {
                        0
                    }
                }
                4 => {
                    if y % 2 == 0 {
                        255
                    } else {
                        0
                    }
                }
                5 => {
                    // A lone impulse in one corner per seed: both mirror ends.
                    let (cx, cy) = [(0, 0), (31, 0), (0, 31), (31, 31)][(seed % 4) as usize];
                    if x == cx && y == cy {
                        255
                    } else {
                        0
                    }
                }
                6 => rnd() as u8,
                _ => ((x * 8 + y * 3 + c * 40 + seed as usize * 17) % 256) as u8,
            };
        }
        t[i * 4 + 3] = 255;
    }
    t
}

#[test]
fn adversarial_tiles_match_the_cpu_forward_exactly() {
    let mut gpu = GpuFrameProcessor::new(256).expect(NEEDS_GPU);
    let tiles: Vec<Vec<u8>> = (0..8u32)
        .flat_map(|k| (0..4u64).map(move |s| pattern(k, s)))
        .collect();
    let refs: Vec<&[u8]> = tiles.iter().map(|t| t.as_slice()).collect();
    let got = gpu.forward_cdf53_tiles(&refs).expect("forward_cdf53_tiles");
    let n = cdf53::CDF53_TOTAL_COEFFS;
    assert_eq!(got.len(), tiles.len() * n);
    let bad: Vec<usize> = (0..tiles.len())
        .filter(|&i| got[i * n..(i + 1) * n] != cdf53::forward(&tiles[i])[..])
        .collect();
    assert!(
        bad.is_empty(),
        "tiles differing from the CPU forward: {bad:?}"
    );
}

#[test]
fn a_full_noise_frame_matches_the_cpu_forward_on_every_listed_tile() {
    let (w, h) = (1920u32, 1088u32);
    let (cols, rows) = (w / 32, h / 32);
    let mut gpu = GpuFrameProcessor::new(cols * rows + 8).expect(NEEDS_GPU);
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    let px: Vec<u8> = (0..w * h)
        .flat_map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            [s as u8, (s >> 8) as u8, (s >> 16) as u8, 255]
        })
        .collect();
    gpu.process_frame_from_pixels(&px, w, h, w * 4)
        .expect("process_frame_from_pixels");
    let count = unsafe { *gpu.cdf53_compact_count_ptr } as usize;
    // Noise has far more than 16 colours per tile, so nearly every tile is
    // listed. The exact count is the classifier's business; the gate is
    // that every listed tile is exact, and that the list is not tiny.
    assert!(
        count >= (cols * rows) as usize * 9 / 10 && count <= (cols * rows) as usize,
        "compact list holds {count} of {} tiles",
        cols * rows
    );
    let list = unsafe { std::slice::from_raw_parts(gpu.cdf53_compact_list_ptr, count) };
    let n = cdf53::CDF53_TOTAL_COEFFS;
    let mut bad = 0usize;
    for (slot, &t) in list.iter().enumerate() {
        let (tx, ty) = (t % cols, t / cols);
        let mut tile = vec![0u8; 4096];
        for r in 0..32u32 {
            let src = (((ty * 32 + r) * w + tx * 32) * 4) as usize;
            tile[(r * 128) as usize..(r * 128 + 128) as usize].copy_from_slice(&px[src..src + 128]);
        }
        let have =
            unsafe { std::slice::from_raw_parts(gpu.cdf53_coefficients_ptr.add(slot * n), n) };
        let want = cdf53::forward(&tile);
        if have.iter().zip(&want).any(|(&g, &c)| g as i16 != c) {
            bad += 1;
        }
    }
    assert_eq!(
        bad, 0,
        "{bad}/{count} listed tiles differ from the CPU forward"
    );
}
