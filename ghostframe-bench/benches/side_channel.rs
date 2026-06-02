//! M3.5 Layer A side-channel JSON writer.
//!
//! Criterion's macros measure latency only. We also need per-(codec,
//! class, lz4_on_off) compressed-byte size, SSIM-vs-original quality,
//! and (for Cdf53) bytes-to-lossless per class. These are recorded
//! outside the criterion timing loop and written to
//! `target/criterion/m3-codec-side-channel.json` after all benches run.
//!
//! The M3.5 codec_report binary ingests this file via the
//! `--reuse-criterion-json` CLI flag.

use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct CompressedBytesEntry {
    pub codec: String,
    pub class: String,
    pub lz4: bool,
    pub bytes: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct SsimEntry {
    pub codec: String,
    pub class: String,
    pub passes_kept: Option<u8>, // Some(K) for Cdf53PerPass; None for full-encode
    pub ssim: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BytesToLosslessEntry {
    pub codec: String, // always "cdf53" for now
    pub class: String,
    pub bytes_per_pass: Vec<usize>, // length 9
    pub bytes_total: usize,
}

#[derive(Default, Serialize)]
pub struct SideChannel {
    pub compressed_bytes: Vec<CompressedBytesEntry>,
    pub ssim: Vec<SsimEntry>,
    pub bytes_to_lossless: Vec<BytesToLosslessEntry>,
}

static SIDE_CHANNEL: OnceLock<Mutex<SideChannel>> = OnceLock::new();

fn slot() -> &'static Mutex<SideChannel> {
    SIDE_CHANNEL.get_or_init(|| Mutex::new(SideChannel::default()))
}

pub fn record_compressed_bytes(codec: &str, class: &str, lz4: bool, bytes: usize) {
    slot().lock().unwrap().compressed_bytes.push(CompressedBytesEntry {
        codec: codec.into(),
        class: class.into(),
        lz4,
        bytes,
    });
}

pub fn record_ssim(codec: &str, class: &str, passes_kept: Option<u8>, ssim: f64) {
    slot().lock().unwrap().ssim.push(SsimEntry {
        codec: codec.into(),
        class: class.into(),
        passes_kept,
        ssim,
    });
}

pub fn record_bytes_to_lossless(class: &str, bytes_per_pass: Vec<usize>) {
    let total: usize = bytes_per_pass.iter().sum();
    slot().lock().unwrap().bytes_to_lossless.push(BytesToLosslessEntry {
        codec: "cdf53".into(),
        class: class.into(),
        bytes_per_pass,
        bytes_total: total,
    });
}

/// Flush the side channel to JSON. Called from `criterion_main!`-adjacent
/// code at the end of `run_codecs`. Idempotent — second call writes the
/// same data (which is fine if it doesn't change between calls).
pub fn flush() -> std::io::Result<()> {
    let data = slot().lock().unwrap();
    let path = std::path::Path::new("target/criterion/m3-codec-side-channel.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = File::create(path)?;
    let json = serde_json::to_string_pretty(&*data).expect("serialize");
    f.write_all(json.as_bytes())?;
    Ok(())
}

/// Compute SSIM between an original BGRA tile and a reconstructed BGRA tile
/// using dssim-core. Returns a value where 1.0 = identical, < 1.0 = different.
pub fn compute_ssim(original_bgra: &[u8], reconstructed_bgra: &[u8], w: u32, h: u32) -> f64 {
    use dssim_core::Dssim;

    // Convert BGRA bytes to rgb::RGBA<u8> (R, G, B, A order).
    let to_rgba = |bgra: &[u8]| -> Vec<rgb::RGBA<u8>> {
        bgra.chunks_exact(4)
            .map(|c| rgb::RGBA { r: c[2], g: c[1], b: c[0], a: c[3] })
            .collect()
    };

    let d = Dssim::new();
    let a = d
        .create_image_rgba(&to_rgba(original_bgra), w as usize, h as usize)
        .expect("create_image_rgba a");
    let b = d
        .create_image_rgba(&to_rgba(reconstructed_bgra), w as usize, h as usize)
        .expect("create_image_rgba b");
    let (val, _maps) = d.compare(&a, &b);
    // dssim returns 0 = identical, larger = more different. Convert to
    // SSIM-like 0..1 range (1.0 = identical).
    1.0 / (1.0 + f64::from(val))
}

// Bench harness entry point (no-op). This file is used as a support module
// via `#[path = "side_channel.rs"] mod side_channel;` in other benches;
// the `[[bench]]` entry exists solely so `cargo check --benches` compiles it.
fn main() {}
