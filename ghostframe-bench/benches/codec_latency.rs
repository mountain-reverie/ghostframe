//! Encode latency per codec × content class.
//!
//! Pre-M3, only H.264 is real. Future codecs (PalRle, BC1, CDF53, Solid)
//! plug in by adding a new `BenchEncoder` impl below.

use criterion::{criterion_group, criterion_main, Criterion};

#[cfg(any(feature = "gpu-bench", feature = "m3"))]
#[path = "fixtures/mod.rs"]
mod fixtures;

#[cfg(any(feature = "gpu-bench", feature = "m3"))]
use fixtures::BenchEncoder;

// ── H.264 (per-tile) ───────────────────────────────────────────────────────
//
// `H264VaapiEncoder` encodes a single 32×32 BGRA tile per call. The encoder
// internally pads to the VA-API minimum, so per-tile latency is dominated
// by VA-API submission overhead, not pixel count. That's still informative.

#[cfg(feature = "gpu-bench")]
use criterion::{black_box, BenchmarkId};

#[cfg(feature = "gpu-bench")]
use fixtures::{ContentClass, Lz4Wrapper, TILE_BYTES};

#[cfg(feature = "gpu-bench")]
struct H264TileEncoder {
    inner: ghostframe_lib::encoder::h264_vaapi::H264VaapiEncoder,
}

#[cfg(feature = "gpu-bench")]
impl H264TileEncoder {
    fn new() -> Option<Self> {
        ghostframe_lib::encoder::h264_vaapi::H264VaapiEncoder::new()
            .ok()
            .map(|inner| Self { inner })
    }
}

#[cfg(feature = "gpu-bench")]
impl BenchEncoder for H264TileEncoder {
    fn name(&self) -> &'static str {
        "h264"
    }
    fn encode(&mut self, tile: &[u8]) -> Vec<u8> {
        // Loop on Ok(None) — VA-API may buffer the first frame; without
        // looping, the buffered iteration would record a near-zero time
        // and drag the mean down. Looping records the cost honestly.
        loop {
            match self.inner.encode(tile) {
                Ok(Some(enc)) => return enc.payload,
                Ok(None) => continue,
                Err(_) => return Vec::new(),
            }
        }
    }
}

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

        let id = self.palette_table.acquire_or_allocate(&palette).unwrap_or(0);
        let bundled = !self.palette_table.delivered.contains(id);
        encode_pal_rle_payload(&indices, &palette, id, bundled)
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

#[cfg(feature = "gpu-bench")]
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
        group.bench_with_input(BenchmarkId::from_parameter(class.name()), &tile, |b, t| {
            b.iter(|| black_box(encoder.encode(t)));
        });
    }
    group.finish();
}

fn run_codecs(c: &mut Criterion) {
    #[cfg(feature = "gpu-bench")]
    {
        if let Some(mut h264) = H264TileEncoder::new() {
            // Warm up — discard the first encode result to flush any
            // VA-API initial-buffer state before measurement begins.
            let warmup = ContentClass::Motion.tile();
            let _ = h264.encode(&warmup);
            bench_codec(c, &mut h264);

            // Re-create for the LZ4 wrapper to isolate state from the prior run.
            if let Some(h264_again) = H264TileEncoder::new() {
                let mut wrapped = Lz4Wrapper::new(h264_again);
                let _ = wrapped.encode(&warmup);
                bench_codec(c, &mut wrapped);
            }
        } else {
            eprintln!("h264: VA-API encoder unavailable, skipping");
        }
    }
    #[cfg(all(feature = "gpu-bench", feature = "m3"))]
    {
        // PalRle bench (CPU-side encode).
        let mut palrle = PalRleEncoder::new();
        bench_codec(c, &mut palrle);

        // LZ4-wrapped variant.
        let palrle_lz4 = PalRleEncoder::new();
        let mut wrapped = Lz4Wrapper::new(palrle_lz4);
        bench_codec(c, &mut wrapped);
    }
    #[cfg(not(feature = "gpu-bench"))]
    {
        let _ = c;
        eprintln!("codec_latency: built without --features gpu-bench, no codecs to bench");
    }
}

criterion_group!(benches, run_codecs);
criterion_main!(benches);
