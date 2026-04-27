//! Encode latency per codec × content class.
//!
//! Pre-M3, only H.264 is real. Future codecs (PalRle, BC1, CDF53, Solid)
//! plug in by adding a new `BenchEncoder` impl below.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

#[path = "fixtures/mod.rs"]
mod fixtures;

use fixtures::{BenchEncoder, ContentClass, Lz4Wrapper, TILE_BYTES};

// ── H.264 (per-tile) ────────────────────────────────────────────────────────
//
// `H264VaapiEncoder` encodes a single 32×32 BGRA tile per call. The encoder
// internally pads to the VA-API minimum, so per-tile latency is dominated
// by VA-API submission overhead, not pixel count. That's still informative.

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
        match self.inner.encode(tile) {
            Ok(Some(enc)) => enc.payload,
            _ => Vec::new(),
        }
    }
}

// ── M3 placeholders ─────────────────────────────────────────────────────────
//
// Each placeholder compiles only with `--features m3`. Implement them by
// replacing the `unimplemented!` with the real codec call when it lands.

#[cfg(feature = "m3")]
struct PalRleEncoder;

#[cfg(feature = "m3")]
impl BenchEncoder for PalRleEncoder {
    fn name(&self) -> &'static str { "pal_rle" }
    fn encode(&mut self, _tile: &[u8]) -> Vec<u8> {
        unimplemented!("PalRle encoder lands in M3");
    }
}

// (Add Bc1Encoder, Cdf53Encoder, SolidEncoder, RawEncoder here in M3.)

// ── Bench driver ────────────────────────────────────────────────────────────

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
            b.iter(|| encoder.encode(t));
        });
    }
    group.finish();
}

fn run_codecs(c: &mut Criterion) {
    #[cfg(feature = "gpu-bench")]
    {
        if let Some(mut h264) = H264TileEncoder::new() {
            bench_codec(c, &mut h264);
            // Re-create for the LZ4 wrapper because the inner encoder has state.
            if let Some(h264_again) = H264TileEncoder::new() {
                let mut wrapped = Lz4Wrapper::new(h264_again);
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
