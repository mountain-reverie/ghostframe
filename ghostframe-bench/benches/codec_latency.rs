//! Encode latency per codec × content class.
//!
//! The tile codecs (PalRle, CDF53, Solid) plug in as `BenchEncoder` impls
//! below. H.264 has no per-tile row: it is a whole-frame stream
//! (libva-direct, openh264) with its own tests, not a tile codec.

use criterion::{criterion_group, criterion_main, Criterion};

#[cfg(feature = "m3")]
use ghostframe_bench::fixtures;

#[cfg(feature = "m3")]
use fixtures::BenchEncoder;

#[cfg(all(feature = "gpu-bench", feature = "m3"))]
#[path = "side_channel.rs"]
mod side_channel;

#[cfg(all(feature = "gpu-bench", feature = "m3"))]
use criterion::{black_box, BenchmarkId};

#[cfg(all(feature = "gpu-bench", feature = "m3"))]
use fixtures::{ContentClass, Lz4Wrapper, TILE_BYTES};

// ── M3 placeholders ─────────────────────────────────────────────────────────

#[cfg(feature = "m3")]
#[allow(dead_code)]
struct PalRleEncoder {
    palette_table: ghostframe_lib::encoder::pal_rle::PaletteTable,
}

#[cfg(feature = "m3")]
impl PalRleEncoder {
    #[allow(dead_code)]
    fn new() -> Self {
        Self {
            palette_table: ghostframe_lib::encoder::pal_rle::PaletteTable::new(),
        }
    }
}

#[cfg(feature = "m3")]
impl BenchEncoder for PalRleEncoder {
    fn name(&self) -> &'static str {
        "pal_rle"
    }
    fn encode(&mut self, tile_bgra: &[u8]) -> Vec<u8> {
        use ghostframe_lib::encoder::pal_rle::{
            encode_pal_rle_payload, PaletteEntry, MAX_PALETTE_COUNT,
        };

        // Extract unique colors from the BGRA tile.
        let mut seen: Vec<[u8; 4]> = Vec::with_capacity(16);
        for chunk in tile_bgra.chunks_exact(4) {
            let c: [u8; 4] = chunk.try_into().unwrap();
            if !seen.contains(&c) {
                if seen.len() >= MAX_PALETTE_COUNT {
                    // Tile not PalRLE-feasible — bench harness records empty payload.
                    return Vec::new();
                }
                seen.push(c);
            }
        }
        // Canonical-sort by BGRA-as-u32 ascending (matches the GPU sort
        // in tile_analysis.comp Stage 1 extension).
        seen.sort_by_key(|c| u32::from_le_bytes(*c));

        let mut palette = PaletteEntry::default();
        for (i, c) in seen.iter().enumerate() {
            palette.colors[i] = *c;
        }
        palette.count = seen.len() as u8;

        // Build the 4-bit index stream (low nibble of byte 0 = pixel 0).
        let mut indices = [0u8; 512];
        for (pixel, chunk) in tile_bgra.chunks_exact(4).enumerate() {
            let c: [u8; 4] = chunk.try_into().unwrap();
            let idx = seen.iter().position(|x| x == &c).unwrap() as u8;
            let byte = pixel / 2;
            let shift = (pixel % 2) * 4;
            indices[byte] |= (idx & 0x0F) << shift;
        }

        let id = self
            .palette_table
            .acquire_or_allocate(&palette)
            .unwrap_or(0);
        let bundled = !self.palette_table.delivered.contains(id);
        encode_pal_rle_payload(&indices, &palette, id, bundled)
    }
}

// ── Solid encoder ────────────────────────────────────────────────────────────

#[cfg(feature = "m3")]
#[allow(dead_code)]
struct SolidEncoder;

#[cfg(feature = "m3")]
impl BenchEncoder for SolidEncoder {
    fn name(&self) -> &'static str {
        "solid"
    }
    fn encode(&mut self, tile_bgra: &[u8]) -> Vec<u8> {
        use ghostframe_lib::encoder::solid::encode_solid;
        // encode_solid returns [u8; 4] directly — the tile must be
        // uniformly-colored for the result to be semantically valid, but
        // the bench calls it on all content classes regardless (the
        // Solid content class is the meaningful case).
        encode_solid(tile_bgra).to_vec()
    }
}

// ── CDF 5/3 encoder ──────────────────────────────────────────────────────────

#[cfg(feature = "m3")]
#[allow(dead_code)]
struct Cdf53Encoder;

#[cfg(feature = "m3")]
impl BenchEncoder for Cdf53Encoder {
    fn name(&self) -> &'static str {
        "cdf53"
    }
    fn encode(&mut self, tile_bgra: &[u8]) -> Vec<u8> {
        use ghostframe_lib::encoder::cdf53;
        let coeffs = cdf53::forward(tile_bgra);
        let passes: Vec<Vec<u8>> = cdf53::encode_passes(&coeffs);
        // Concatenate with a 2-byte LE length prefix per pass so the
        // round-trip proptest in Task 15 can split them back.
        let mut out: Vec<u8> = Vec::with_capacity(passes.iter().map(|p| 2 + p.len()).sum());
        for p in &passes {
            assert!(
                p.len() <= u16::MAX as usize,
                "pass too large for u16 prefix"
            );
            out.extend_from_slice(&(p.len() as u16).to_le_bytes());
            out.extend_from_slice(p);
        }
        out
    }
}

// ── CDF 5/3 per-pass encoder ─────────────────────────────────────────────────

#[cfg(feature = "m3")]
#[allow(dead_code)]
struct Cdf53PerPassEncoder {
    passes_kept: u8,
}

#[cfg(feature = "m3")]
impl BenchEncoder for Cdf53PerPassEncoder {
    fn name(&self) -> &'static str {
        "cdf53_per_pass"
    }
    fn encode(&mut self, tile_bgra: &[u8]) -> Vec<u8> {
        use ghostframe_lib::encoder::cdf53;
        let coeffs = cdf53::forward(tile_bgra);
        let all_passes: Vec<Vec<u8>> = cdf53::encode_passes(&coeffs);
        let kept = (self.passes_kept as usize).min(all_passes.len());
        let mut out: Vec<u8> =
            Vec::with_capacity(all_passes[..kept].iter().map(|p| 2 + p.len()).sum());
        for p in &all_passes[..kept] {
            out.extend_from_slice(&(p.len() as u16).to_le_bytes());
            out.extend_from_slice(p);
        }
        out
    }
}

// ── Bench driver ────────────────────────────────────────────────────────────
//
// Note: Criterion samples each BenchmarkId across many iterations. Because
// the H.264 encoder runs with GOP=30 and accumulating PTS, the measured
// latency distribution is a mixed I-frame + P-frame average, not the
// per-frame-type latency of either. For relative codec comparison this
// is fine; for absolute single-frame numbers, see the codec_callgrind
// bench (Task 6) or future per-frame-type harnesses.

#[cfg(all(feature = "gpu-bench", feature = "m3"))]
fn bench_codec(c: &mut Criterion, encoder: &mut dyn BenchEncoder) {
    let group_name = if encoder.lz4() {
        format!("{}+lz4", encoder.name())
    } else {
        encoder.name().to_string()
    };
    let mut group = c.benchmark_group(&group_name);

    for class in ContentClass::ALL {
        let tile = class.tile();
        assert_eq!(tile.len(), TILE_BYTES);

        // Record compressed size OUTSIDE the criterion timing loop.
        #[cfg(all(feature = "gpu-bench", feature = "m3"))]
        {
            let encoded = encoder.encode(&tile);
            side_channel::record_compressed_bytes(
                encoder.name(),
                class.name(),
                encoder.lz4(),
                encoded.len(),
            );
        }

        group.bench_with_input(BenchmarkId::from_parameter(class.name()), &tile, |b, t| {
            b.iter(|| black_box(encoder.encode(t)));
        });
    }
    group.finish();
}

fn run_codecs(c: &mut Criterion) {
    #[cfg(all(feature = "gpu-bench", feature = "m3"))]
    {
        // PalRle bench (CPU-side encode).
        let mut palrle = PalRleEncoder::new();
        bench_codec(c, &mut palrle);

        // LZ4-wrapped variant.
        let palrle_lz4 = PalRleEncoder::new();
        let mut wrapped = Lz4Wrapper::new(palrle_lz4);
        bench_codec(c, &mut wrapped);

        // Solid bench
        let mut solid = SolidEncoder;
        bench_codec(c, &mut solid);
        let mut solid_lz4 = Lz4Wrapper::new(SolidEncoder);
        bench_codec(c, &mut solid_lz4);

        // Cdf53 (full lossless encode) bench
        let mut cdf53 = Cdf53Encoder;
        bench_codec(c, &mut cdf53);
        let mut cdf53_lz4 = Lz4Wrapper::new(Cdf53Encoder);
        bench_codec(c, &mut cdf53_lz4);

        // SSIM + bytes-to-lossless for Cdf53 (full encode, all 9 passes).
        for class in ContentClass::ALL {
            let tile = class.tile();
            let coeffs = ghostframe_lib::encoder::cdf53::forward(&tile);
            let passes = ghostframe_lib::encoder::cdf53::encode_passes(&coeffs);
            let pass_refs: Vec<&[u8]> = passes.iter().map(|p| &p[..]).collect();
            let recovered_coeffs = ghostframe_lib::encoder::cdf53::decode_passes(&pass_refs);
            let recovered_bgr = ghostframe_lib::encoder::cdf53::inverse(&recovered_coeffs);
            // inverse() returns BGR (3 bytes/pixel); convert to BGRA for SSIM.
            let mut recovered_bgra = Vec::with_capacity(32 * 32 * 4);
            for chunk in recovered_bgr.chunks_exact(3) {
                recovered_bgra.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 0xFF]);
            }
            let ssim = side_channel::compute_ssim(&tile, &recovered_bgra, 32, 32);
            side_channel::record_ssim("cdf53", class.name(), None, ssim);
            let bytes_per_pass: Vec<usize> = passes.iter().map(|p| p.len()).collect();
            side_channel::record_bytes_to_lossless(class.name(), bytes_per_pass);
        }

        // Cdf53PerPass: one bench group per K showing the K progression.
        for k in 1..=9u8 {
            let mut per_pass = Cdf53PerPassEncoder { passes_kept: k };
            let mut group = c.benchmark_group(format!("cdf53_per_pass_k{}", k));
            for class in ContentClass::ALL {
                let tile = class.tile();
                assert_eq!(tile.len(), TILE_BYTES);
                group.bench_with_input(BenchmarkId::from_parameter(class.name()), &tile, |b, t| {
                    b.iter(|| black_box(per_pass.encode(t)))
                });
            }
            group.finish();

            // SSIM for Cdf53PerPass at this K value, per class.
            for class in ContentClass::ALL {
                let tile = class.tile();
                let coeffs = ghostframe_lib::encoder::cdf53::forward(&tile);
                let all_passes = ghostframe_lib::encoder::cdf53::encode_passes(&coeffs);
                let kept = (k as usize).min(all_passes.len());
                let kept_refs: Vec<&[u8]> = all_passes[..kept].iter().map(|p| &p[..]).collect();
                let recovered_coeffs = ghostframe_lib::encoder::cdf53::decode_passes(&kept_refs);
                let recovered_bgr = ghostframe_lib::encoder::cdf53::inverse(&recovered_coeffs);
                // inverse() returns BGR (3 bytes/pixel); convert to BGRA for SSIM.
                let mut recovered_bgra = Vec::with_capacity(32 * 32 * 4);
                for chunk in recovered_bgr.chunks_exact(3) {
                    recovered_bgra.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 0xFF]);
                }
                let ssim = side_channel::compute_ssim(&tile, &recovered_bgra, 32, 32);
                side_channel::record_ssim("cdf53_per_pass", class.name(), Some(k), ssim);
            }
        }
    }
    #[cfg(not(all(feature = "gpu-bench", feature = "m3")))]
    {
        let _ = c;
        eprintln!("codec_latency: built without --features gpu-bench, no codecs to bench");
    }
    #[cfg(all(feature = "gpu-bench", feature = "m3"))]
    {
        side_channel::flush().expect("side_channel flush");
    }
}

criterion_group!(benches, run_codecs);
criterion_main!(benches);
