//! Encode latency per codec × content class.
//!
//! Pre-M3, only H.264 is real. Future codecs (PalRle, BC1, CDF53, Solid)
//! plug in by adding a new `BenchEncoder` impl below.

use criterion::{criterion_group, criterion_main, Criterion};

#[cfg(feature = "gpu-bench")]
#[path = "fixtures/mod.rs"]
mod fixtures;

// ── H.264 (per-tile) ───────────────────────────────────────────────────────
//
// `H264VaapiEncoder` encodes a single 32×32 BGRA tile per call. The encoder
// internally pads to the VA-API minimum, so per-tile latency is dominated
// by VA-API submission overhead, not pixel count. That's still informative.

#[cfg(feature = "gpu-bench")]
use criterion::{black_box, BenchmarkId};

#[cfg(feature = "gpu-bench")]
use fixtures::{BenchEncoder, ContentClass, Lz4Wrapper, TILE_BYTES};

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
    fn name(&self) -> &'static str { "h264" }
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
struct PalRleEncoder;

#[cfg(feature = "m3")]
impl BenchEncoder for PalRleEncoder {
    fn name(&self) -> &'static str { "pal_rle" }
    fn encode(&mut self, _tile: &[u8]) -> Vec<u8> {
        unimplemented!("PalRle encoder lands in M3");
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
    #[cfg(not(feature = "gpu-bench"))]
    {
        let _ = c;
        eprintln!("codec_latency: built without --features gpu-bench, no codecs to bench");
    }
}

criterion_group!(benches, run_codecs);
criterion_main!(benches);
