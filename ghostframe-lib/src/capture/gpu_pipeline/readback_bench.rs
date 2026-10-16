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

/// How many distinct palettes would a screen put on the 256-slot PalRle table?
/// Counts, per synthetic 4K screen, the tiles with 16 or fewer colours and the
/// distinct colour sets among them (the table's demand before the GPU folds
/// subsets into supersets). A screen with more than 256 distinct sets in use
/// at once is what makes `phase_a` fall back (to CDF 5/3 now). Synthetic, so
/// it bounds the question; it does not replace a trace of a real desktop.
#[test]
#[ignore]
fn palette_demand_report() {
    let (w, h) = (3840u32, 2160u32);
    let mut gpu =
        GpuFrameProcessor::new((w / 32) * h.div_ceil(32) + 8).expect("needs a Vulkan GPU");
    type Screen = (&'static str, Box<dyn Fn(u32, u32) -> [u8; 4]>);
    let screens: Vec<Screen> = vec![
        (
            "editor, bitmap font, 8 syntax colours (no antialiasing)",
            Box::new(|x, y| {
                let (cx, cy) = (x / 9, y / 18);
                let ink = ((x % 9) < 5) && ((y % 18) < 12) && ((cx * 7 + cy * 13) % 3 != 0);
                if ink {
                    let c = ((cx / 6 + cy) % 8) as u8;
                    [40 + c * 25, 200 - c * 20, 90 + c * 15, 255]
                } else {
                    [30, 30, 30, 255]
                }
            }),
        ),
        (
            "text with antialiasing (grey ramp edges)",
            Box::new(|x, y| {
                let (cx, cy) = (x / 9, y / 18);
                let e = ((x % 9) * 40 + (y % 18) * 9 + cx * 3 + cy) % 256;
                if (cx * 7 + cy * 13) % 3 != 0 {
                    [e as u8, e as u8, e as u8, 255]
                } else {
                    [255, 255, 255, 255]
                }
            }),
        ),
        (
            "flat dashboard: 64 colours, 4 per cell, 24 px cells",
            Box::new(|x, y| {
                let (cx, cy) = (x / 24, y / 24);
                let sub = (x % 24) / 12 + 2 * ((y % 24) / 12);
                let k = (cx * 31 + cy * 17 + sub * 7) % 64;
                [
                    (k * 4) as u8,
                    (255 - k * 3) as u8,
                    (k * 11 % 256) as u8,
                    255,
                ]
            }),
        ),
    ];
    for (name, f) in screens {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&f(x, y));
            }
        }
        // Two frames so the second has a baseline and everything is dirty.
        let other: Vec<u8> = px.iter().map(|b| b.wrapping_add(1)).collect();
        unsafe { gpu.process_frame_inner_from_pixels(&other, w, h, w * 4) }.unwrap();
        let a = unsafe { gpu.process_frame_inner_from_pixels(&px, w, h, w * 4) }.unwrap();
        let mut sets = std::collections::HashSet::new();
        let mut small = 0usize;
        for i in 0..a.tile_analysis_len as usize {
            let t = unsafe { &*a.tile_analysis.add(i) };
            if (1..=16).contains(&t.count) {
                small += 1;
                let mut cs: Vec<u32> = t.colors[..t.count as usize].to_vec();
                cs.sort_unstable();
                sets.insert(cs);
            }
        }
        println!(
            "PALDEMAND {name}: tiles={} le16_colours={} distinct_palettes={} (table holds 256)",
            a.tile_analysis_len,
            small,
            sets.len()
        );
    }
}
