//! Per-frame cost of the CPU reading the pipeline's host-visible outputs back,
//! plus the whole `process_frame` call. A measurement, not a gate: `#[ignore]`d.
//! `gf-gpu-run cargo test -p ghostframe-lib --lib readback_report -- --ignored --nocapture`
#![allow(
    clippy::disallowed_methods,
    reason = "wall-clock measurement of real GPU/CPU time; no tokio clock in these tests"
)]

use super::*;
use std::time::Instant;

fn frame(w: u32, h: u32, seed: u32, noisy: bool) -> Vec<u8> {
    let mut s = 0x2545_F491u32 ^ seed.wrapping_mul(0x9E37_79B9) | 1;
    (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            if noisy {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                [(s >> 3) as u8, (s >> 11) as u8, (s >> 19) as u8, 255]
            } else {
                // Text-like: two colours with a seed-dependent glyph pattern.
                let on = ((x / 3 + y / 5 + seed) % 7) < 2 || (x % 11 == seed % 11);
                if on {
                    [20, 20, 20, 255]
                } else {
                    [240, 240, 240, 255]
                }
            }
        })
        .collect()
}

fn med(v: &mut [f64]) -> (f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (v[v.len() / 2], v[0], v[v.len() - 1])
}

#[test]
#[ignore]
fn readback_report() {
    for (label, w, h) in [("1080p", 1920u32, 1088u32), ("4K", 3840, 2160)] {
        for (content, noisy) in [("noise (all Cdf53)", true), ("text (PalRle)", false)] {
            let tiles = (w / 32) as usize * h.div_ceil(32) as usize;
            let mut gpu = GpuFrameProcessor::new(tiles as u32 + 8).expect("needs a Vulkan GPU");
            let stride = w * 4;
            let mut t_proc = Vec::new();
            let mut t_an = Vec::new();
            let mut t_pal = Vec::new();
            let mut t_cdf = Vec::new();
            let mut t_nv12 = Vec::new();
            let mut ncdf = 0;
            let mut npal = 0;
            for it in 0..8u32 {
                let px = frame(w, h, it, noisy);
                let t0 = Instant::now();
                let a = unsafe { gpu.process_frame_inner_from_pixels(&px, w, h, stride) }
                    .expect("process_frame");
                let tp = t0.elapsed().as_secs_f64() * 1e3;
                if it == 0 {
                    continue; // first frame has no baseline; not steady state
                }
                t_proc.push(tp);
                // analysis, as populate_gpu_metrics walks it
                let t0 = Instant::now();
                let mut acc = 0u64;
                for i in 0..a.tile_analysis_len as usize {
                    let ta = unsafe { &*a.tile_analysis.add(i) };
                    acc += (ta.count + ta.edge_density_thou) as u64;
                }
                t_an.push(t0.elapsed().as_secs_f64() * 1e3);
                // palrle: palette set, per-tile ids, index buffer slice
                let t0 = Instant::now();
                npal = a.palrle_compact_count as usize;
                for c in 0..npal {
                    let _id = unsafe { *a.per_tile_frame_palette_id.add(c) };
                    let _t = unsafe { *a.palrle_compact_list.add(c) };
                    let ix =
                        unsafe { std::slice::from_raw_parts(a.index_buffer.add(c * 512), 512) };
                    acc += ix[0] as u64 + ix[511] as u64;
                }
                for s in 0..256usize {
                    acc += unsafe { (*a.frame_palette_set.add(s)).count } as u64;
                }
                t_pal.push(t0.elapsed().as_secs_f64() * 1e3);
                // cdf53: per-tile 3072 i32 -> i16, as the production Phase B does
                let t0 = Instant::now();
                ncdf = unsafe { *gpu.cdf53_compact_count_ptr } as usize;
                ncdf = ncdf.min(tiles);
                let mut keep = 0usize;
                for slot in 0..ncdf {
                    let c = unsafe {
                        std::slice::from_raw_parts(
                            gpu.cdf53_coefficients_ptr
                                .add(slot * crate::encoder::cdf53::CDF53_TOTAL_COEFFS),
                            crate::encoder::cdf53::CDF53_TOTAL_COEFFS,
                        )
                    };
                    let v: Vec<i16> = c.iter().map(|&x| x as i16).collect();
                    keep += v.len();
                }
                t_cdf.push(t0.elapsed().as_secs_f64() * 1e3);
                // NV12: a straight copy, as handing it to an encoder would
                let t0 = Instant::now();
                let n = (a.nv12_uv_offset + a.nv12_uv_stride * a.nv12_height.div_ceil(2)) as usize;
                let copy = unsafe { std::slice::from_raw_parts(a.nv12_data, n) }.to_vec();
                t_nv12.push(t0.elapsed().as_secs_f64() * 1e3);
                std::hint::black_box((acc, keep, copy.len()));
            }
            let (p, an, pal, cdf, nv) = (
                med(&mut t_proc),
                med(&mut t_an),
                med(&mut t_pal),
                med(&mut t_cdf),
                med(&mut t_nv12),
            );
            println!(
                "READBACK {label} {content}: tiles={tiles} cdf53_tiles={ncdf} palrle_tiles={npal} | \
                 process_frame {:.2} ms (min {:.2} max {:.2}) | analysis {:.2} | palrle {:.2} | cdf53 coeffs {:.2} | nv12 copy {:.2}",
                p.0, p.1, p.2, an.0, pal.0, cdf.0, nv.0
            );
        }
    }
}
