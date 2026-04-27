# Codec Bench Skeleton Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the Criterion + iai-callgrind bench harness §9.4 and M3 §12 require, with content-class fixtures and a `BenchEncoder` trait that future M3 codecs plug into without harness changes.

**Architecture:** Pre-M3, only one codec is real (H.264). The deliverable is the *harness* — a `BenchEncoder` trait, six procedurally generated content-class tiles, two Criterion benches (encode latency, pipeline throughput), and one iai-callgrind bench (deterministic protocol primitives). Each future M3 codec lands by implementing the trait; no harness changes needed. GPU-dependent benches gate on a `gpu-bench` cargo feature so CI without a GPU can still run the suite.

**Tech Stack:** Rust, `criterion = "0.5"`, `iai-callgrind = "0.14"`, `lz4_flex = "0.11"` (so codecs can opt into post-compression measurement), existing `ghostframe-lib` encoder + protocol modules.

---

## File Structure

```
ghostframe-lib/
├── Cargo.toml                          # criterion + iai-callgrind dev-deps,
│                                       # gpu-bench feature, [[bench]] entries
└── benches/
    ├── fixtures/
    │   ├── mod.rs                      # ContentClass enum, BenchEncoder trait
    │   ├── solid.rs                    # 32×32 single colour
    │   ├── flat_ui.rs                  # 16-colour palettised content
    │   ├── text.rs                     # glyph stroke pattern
    │   ├── gradient.rs                 # smooth linear gradient
    │   ├── photo.rs                    # noisy, high-entropy pattern
    │   └── motion.rs                   # frame-pair (delta heavy)
    ├── codec_latency.rs                # Criterion: encode time per codec × class
    ├── pipeline_throughput.rs          # Criterion: fragment / FEC / DirtyTracker
    └── codec_callgrind.rs              # iai-callgrind: deterministic primitives
```

The `benches/fixtures/` directory is shared by all three benches via `#[path = "fixtures/mod.rs"] mod fixtures;`.

---

## Task 1: Add bench dependencies and feature flags

**Files:**
- Modify: `ghostframe-lib/Cargo.toml`

- [ ] **Step 1: Add criterion, iai-callgrind, and lz4_flex as dev-dependencies**

Edit `ghostframe-lib/Cargo.toml`. Append to `[dev-dependencies]`:

```toml
criterion = { version = "0.5", features = ["html_reports"] }
iai-callgrind = "0.14"
lz4_flex = "0.11"
```

- [ ] **Step 2: Add a `[features]` section above `[dependencies]`**

Insert in `ghostframe-lib/Cargo.toml` (between `[lib]` and `[dependencies]`):

```toml
[features]
default = []
# Enables benches that require real VA-API / Vulkan (skipped on CI without GPU).
gpu-bench = []
# Enables benches for codecs not yet implemented (BC1, PalRle, CDF53, Solid).
# Currently a no-op; flip on as each codec lands in M3.
m3 = []
```

- [ ] **Step 3: Verify the crate still builds**

Run: `cargo build -p ghostframe-lib --tests`
Expected: clean build, criterion/iai-callgrind/lz4_flex resolved.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/Cargo.toml
git commit -m "chore(bench): add criterion, iai-callgrind, lz4_flex; add gpu-bench/m3 features"
```

---

## Task 2: ContentClass + BenchEncoder skeleton

**Files:**
- Create: `ghostframe-lib/benches/fixtures/mod.rs`

- [ ] **Step 1: Write the fixtures module**

Create `ghostframe-lib/benches/fixtures/mod.rs`:

```rust
//! Shared bench fixtures: content classes and the BenchEncoder trait.
//!
//! M3 codec implementations land by adding a new `BenchEncoder` impl in this
//! module (or a sibling file) — no changes needed in the bench files
//! themselves.

#![allow(dead_code)]

pub mod flat_ui;
pub mod gradient;
pub mod motion;
pub mod photo;
pub mod solid;
pub mod text;

/// Edge length of a single tile, in pixels. Mirrors `ghostframe_lib::tile::TILE_SIZE`.
pub const TILE_SIZE: u32 = 32;

/// Bytes per pixel — BGRA8.
pub const BPP: u32 = 4;

/// Raw tile size in bytes.
pub const TILE_BYTES: usize = (TILE_SIZE * TILE_SIZE * BPP) as usize;

/// One representative tile per content class.
/// All buffers are exactly `TILE_BYTES` long, BGRA8.
#[derive(Debug, Clone, Copy)]
pub enum ContentClass {
    Solid,
    FlatUi,
    Text,
    Gradient,
    Photo,
    Motion,
}

impl ContentClass {
    pub const ALL: &'static [ContentClass] = &[
        ContentClass::Solid,
        ContentClass::FlatUi,
        ContentClass::Text,
        ContentClass::Gradient,
        ContentClass::Photo,
        ContentClass::Motion,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ContentClass::Solid => "solid",
            ContentClass::FlatUi => "flat_ui",
            ContentClass::Text => "text",
            ContentClass::Gradient => "gradient",
            ContentClass::Photo => "photo",
            ContentClass::Motion => "motion",
        }
    }

    /// Returns the BGRA tile buffer for this content class.
    pub fn tile(self) -> Vec<u8> {
        match self {
            ContentClass::Solid => solid::tile(),
            ContentClass::FlatUi => flat_ui::tile(),
            ContentClass::Text => text::tile(),
            ContentClass::Gradient => gradient::tile(),
            ContentClass::Photo => photo::tile(),
            ContentClass::Motion => motion::tile(),
        }
    }
}

/// Trait every bench-able codec implements.
///
/// One instance per codec is created up front; the bench loop calls `encode`
/// repeatedly. `name` and `lz4` together form the bench-id namespace
/// (e.g. `h264/text`, `h264+lz4/text`).
pub trait BenchEncoder {
    fn name(&self) -> &'static str;
    fn lz4(&self) -> bool {
        false
    }
    fn encode(&mut self, tile: &[u8]) -> Vec<u8>;
}

/// Wraps any `BenchEncoder` in an LZ4 post-pass. Used to measure the
/// per-codec break-even §12 calls for.
pub struct Lz4Wrapper<E: BenchEncoder> {
    inner: E,
}

impl<E: BenchEncoder> Lz4Wrapper<E> {
    pub fn new(inner: E) -> Self {
        Self { inner }
    }
}

impl<E: BenchEncoder> BenchEncoder for Lz4Wrapper<E> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn lz4(&self) -> bool {
        true
    }
    fn encode(&mut self, tile: &[u8]) -> Vec<u8> {
        let raw = self.inner.encode(tile);
        lz4_flex::compress_prepend_size(&raw)
    }
}
```

- [ ] **Step 2: Confirm the module is referenced by at least one bench (placeholder)**

Skip — Task 3 onward includes the `#[path = "fixtures/mod.rs"] mod fixtures;` reference.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/benches/fixtures/mod.rs
git commit -m "test(bench): ContentClass enum and BenchEncoder trait"
```

---

## Task 3: Procedural content-class tiles

**Files:**
- Create: `ghostframe-lib/benches/fixtures/solid.rs`
- Create: `ghostframe-lib/benches/fixtures/flat_ui.rs`
- Create: `ghostframe-lib/benches/fixtures/text.rs`
- Create: `ghostframe-lib/benches/fixtures/gradient.rs`
- Create: `ghostframe-lib/benches/fixtures/photo.rs`
- Create: `ghostframe-lib/benches/fixtures/motion.rs`

- [ ] **Step 1: Write solid.rs**

Create `ghostframe-lib/benches/fixtures/solid.rs`:

```rust
use super::TILE_BYTES;

/// 32×32 tile filled with a single BGRA colour.
pub fn tile() -> Vec<u8> {
    let mut buf = Vec::with_capacity(TILE_BYTES);
    for _ in 0..(TILE_BYTES / 4) {
        buf.extend_from_slice(&[0x33, 0x66, 0x99, 0xFF]); // muted blue
    }
    buf
}
```

- [ ] **Step 2: Write flat_ui.rs**

Create `ghostframe-lib/benches/fixtures/flat_ui.rs`:

```rust
use super::{BPP, TILE_BYTES, TILE_SIZE};

/// 32×32 tile drawn with 16 fixed BGRA palette entries — representative of
/// flat UI elements (toolbars, list rows). Uses a low-entropy block layout
/// so palettised RLE / BC1 both have something realistic to compress.
pub fn tile() -> Vec<u8> {
    let palette: [[u8; 4]; 16] = [
        [0x1E, 0x1E, 0x1E, 0xFF], [0x2D, 0x2D, 0x30, 0xFF],
        [0x3F, 0x3F, 0x46, 0xFF], [0x55, 0x55, 0x5A, 0xFF],
        [0x68, 0x68, 0x70, 0xFF], [0x80, 0x80, 0x88, 0xFF],
        [0x9A, 0x9A, 0xA0, 0xFF], [0xB0, 0xB0, 0xB8, 0xFF],
        [0xC8, 0xC8, 0xD0, 0xFF], [0xE0, 0xE0, 0xE8, 0xFF],
        [0xF0, 0xF0, 0xF5, 0xFF], [0xFF, 0xFF, 0xFF, 0xFF],
        [0x00, 0x7A, 0xCC, 0xFF], [0xCC, 0x66, 0x00, 0xFF],
        [0x33, 0x99, 0x33, 0xFF], [0x99, 0x33, 0x99, 0xFF],
    ];
    let mut buf = vec![0u8; TILE_BYTES];
    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            // Block-y palette index — sub-tile bands for run-length friendliness.
            let idx = (((x / 4) + (y / 8) * 4) as usize) % palette.len();
            let off = (y * TILE_SIZE * BPP + x * BPP) as usize;
            buf[off..off + 4].copy_from_slice(&palette[idx]);
        }
    }
    buf
}
```

- [ ] **Step 3: Write text.rs**

Create `ghostframe-lib/benches/fixtures/text.rs`:

```rust
use super::{BPP, TILE_BYTES, TILE_SIZE};

/// 32×32 tile representing monospace text — vertical strokes on a dark
/// background. Two distinct colours, high-contrast: the canonical input
/// for palettised RLE.
pub fn tile() -> Vec<u8> {
    let bg = [0x14, 0x14, 0x14, 0xFF]; // near-black
    let fg = [0xF5, 0xF5, 0xF5, 0xFF]; // near-white
    let mut buf = vec![0u8; TILE_BYTES];
    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            // Vertical stroke pattern: column x is "ink" if x % 6 in {1, 2}
            // and y is within a glyph row band.
            let col_ink = matches!(x % 6, 1 | 2);
            let row_band = (y % 12) >= 2 && (y % 12) <= 9;
            let on = col_ink && row_band;
            let c = if on { fg } else { bg };
            let off = (y * TILE_SIZE * BPP + x * BPP) as usize;
            buf[off..off + 4].copy_from_slice(&c);
        }
    }
    buf
}
```

- [ ] **Step 4: Write gradient.rs**

Create `ghostframe-lib/benches/fixtures/gradient.rs`:

```rust
use super::{BPP, TILE_BYTES, TILE_SIZE};

/// 32×32 tile carrying a smooth diagonal gradient — the canonical input
/// for BC1 / CDF 5/3 wavelet.
pub fn tile() -> Vec<u8> {
    let mut buf = vec![0u8; TILE_BYTES];
    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            let t = ((x + y) as f32) / ((TILE_SIZE * 2 - 2) as f32);
            let v = (t * 255.0) as u8;
            let off = (y * TILE_SIZE * BPP + x * BPP) as usize;
            buf[off..off + 4].copy_from_slice(&[v, 255 - v, v / 2, 0xFF]);
        }
    }
    buf
}
```

- [ ] **Step 5: Write photo.rs**

Create `ghostframe-lib/benches/fixtures/photo.rs`:

```rust
use super::{BPP, TILE_BYTES, TILE_SIZE};

/// 32×32 tile with high-entropy pseudo-random pixels — stresses any codec
/// that relies on spatial coherence.
pub fn tile() -> Vec<u8> {
    let mut buf = vec![0u8; TILE_BYTES];
    // Linear-congruential PRNG so the fixture is deterministic without rand.
    let mut state: u32 = 0xC0FFEE_u32;
    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let off = (y * TILE_SIZE * BPP + x * BPP) as usize;
            buf[off + 0] = (state & 0xFF) as u8;
            buf[off + 1] = ((state >> 8) & 0xFF) as u8;
            buf[off + 2] = ((state >> 16) & 0xFF) as u8;
            buf[off + 3] = 0xFF;
        }
    }
    buf
}
```

- [ ] **Step 6: Write motion.rs**

Create `ghostframe-lib/benches/fixtures/motion.rs`:

```rust
use super::{BPP, TILE_BYTES, TILE_SIZE};

/// 32×32 tile representing a frame from a video stream — strong horizontal
/// gradients with one moving high-contrast object. This is the canonical
/// input for H.264.
pub fn tile() -> Vec<u8> {
    let mut buf = vec![0u8; TILE_BYTES];
    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            // Horizontal gradient backdrop.
            let r = (x * 8) as u8;
            let g = (y * 8) as u8;
            let b = ((x ^ y) * 4) as u8;
            // High-contrast object: a 6×6 white block off-centre.
            let in_obj = (10..16).contains(&x) && (12..18).contains(&y);
            let c = if in_obj { [0xFF, 0xFF, 0xFF, 0xFF] } else { [b, g, r, 0xFF] };
            let off = (y * TILE_SIZE * BPP + x * BPP) as usize;
            buf[off..off + 4].copy_from_slice(&c);
        }
    }
    buf
}
```

- [ ] **Step 7: Verify all fixtures compile**

Run: `cargo build -p ghostframe-lib --benches`
Expected: clean build (each fixture file compiles, mod.rs imports them, but no bench file consumes them yet — Task 4 wires them in).

The build won't yet *do* anything because there are no `[[bench]]` entries; but Cargo still validates the bench module tree.

- [ ] **Step 8: Commit**

```bash
git add ghostframe-lib/benches/fixtures/
git commit -m "test(bench): six procedural content-class fixtures (solid, flat_ui, text, gradient, photo, motion)"
```

---

## Task 4: codec_latency Criterion bench

**Files:**
- Create: `ghostframe-lib/benches/codec_latency.rs`
- Modify: `ghostframe-lib/Cargo.toml` (add `[[bench]]` entry)

- [ ] **Step 1: Write the bench file**

Create `ghostframe-lib/benches/codec_latency.rs`:

```rust
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
```

- [ ] **Step 2: Register the bench in Cargo.toml**

Edit `ghostframe-lib/Cargo.toml`. Append:

```toml
[[bench]]
name = "codec_latency"
harness = false
```

- [ ] **Step 3: Smoke-test the bench compiles without `gpu-bench`**

Run: `cargo bench -p ghostframe-lib --bench codec_latency -- --test`
Expected: bench compiles, runs the harness in test mode, prints "codec_latency: built without --features gpu-bench, no codecs to bench". Exit 0.

- [ ] **Step 4: If a GPU is available, run with `gpu-bench`**

Run: `cargo bench -p ghostframe-lib --bench codec_latency --features gpu-bench -- --quick`
Expected: Criterion produces a `target/criterion/h264/` report tree. If VA-API isn't available, the bench prints "h264: VA-API encoder unavailable, skipping" and exits cleanly.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/benches/codec_latency.rs ghostframe-lib/Cargo.toml
git commit -m "test(bench): codec_latency Criterion bench skeleton (H.264 + M3 placeholders)"
```

---

## Task 5: pipeline_throughput Criterion bench

**Files:**
- Create: `ghostframe-lib/benches/pipeline_throughput.rs`
- Modify: `ghostframe-lib/Cargo.toml`

- [ ] **Step 1: Write the bench**

Create `ghostframe-lib/benches/pipeline_throughput.rs`:

```rust
//! Throughput of CPU-only pipeline primitives that run on every frame:
//!  * `DirtyTracker::update` (tile diff)
//!  * `protocol::fragment_tile` and `fragment_frame` (MTU split)
//!  * `fec::xor_parity_in_place` (FEC parity generation)
//!
//! These have no GPU dependency, so they run on every CI machine.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use ghostframe_lib::tile::{DirtyTracker, BPP, TILE_SIZE};
use ghostframe_lib::transport::fec;
use ghostframe_lib::transport::protocol::{fragment_frame, fragment_tile, max_fragment_payload};

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
                black_box(fragment_tile(0, 0, 0, 1, 0, 0, p, max_payload));
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
                black_box(fragment_frame(0, 0, false, p, 1186));
            });
        });
    }
    group.finish();
}

fn bench_fec_parity(c: &mut Criterion) {
    let mut group = c.benchmark_group("fec_parity");
    // Simulate a frame split across 64 fragments of 1186 bytes each, K=4.
    let frags: Vec<Vec<u8>> = (0..64).map(|_| vec![0u8; 1186]).collect();
    group.throughput(Throughput::Bytes(frags.iter().map(|f| f.len() as u64).sum()));
    group.bench_function("k4_64frags", |b| {
        b.iter(|| {
            for chunk in frags.chunks(4) {
                let refs: Vec<&[u8]> = chunk.iter().map(|v| v.as_slice()).collect();
                black_box(fec::xor_parity(&refs));
            }
        });
    });
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
```

- [ ] **Step 2: Verify the FEC + protocol API names match the bench**

Run: `grep -nE "pub fn (xor_parity|fragment_tile|fragment_frame|max_fragment_payload)" ghostframe-lib/src/transport/fec.rs ghostframe-lib/src/transport/protocol.rs`
Expected: all four functions exist. If `xor_parity` is named differently in `fec.rs` (e.g. `compute_parity`, `xor_in_place`), update the bench to use the actual name. Update the `fragment_tile` argument list to match its real signature.

- [ ] **Step 3: Register the bench**

Edit `ghostframe-lib/Cargo.toml`. Append:

```toml
[[bench]]
name = "pipeline_throughput"
harness = false
```

- [ ] **Step 4: Run the bench in test mode**

Run: `cargo bench -p ghostframe-lib --bench pipeline_throughput -- --test`
Expected: every group runs once, exits 0. Report tree at `target/criterion/dirty_tracker/`, `target/criterion/fragment_tile/`, etc.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/benches/pipeline_throughput.rs ghostframe-lib/Cargo.toml
git commit -m "test(bench): pipeline_throughput Criterion bench (DirtyTracker, fragment, FEC)"
```

---

## Task 6: codec_callgrind iai-callgrind bench

**Files:**
- Create: `ghostframe-lib/benches/codec_callgrind.rs`
- Modify: `ghostframe-lib/Cargo.toml`

- [ ] **Step 1: Write the bench**

Create `ghostframe-lib/benches/codec_callgrind.rs`:

```rust
//! Deterministic instruction-count tracking for protocol primitives.
//!
//! iai-callgrind runs each function under valgrind --tool=callgrind and
//! reports instruction counts that don't vary between runs — perfect for
//! catching regressions in CI without flakiness.
//!
//! Codecs with non-deterministic codepaths (VA-API, GPU compute) are NOT
//! benchmarked here — they go in `codec_latency.rs` under Criterion.

use iai_callgrind::{library_benchmark, library_benchmark_group, main};

use ghostframe_lib::transport::fec;
use ghostframe_lib::transport::protocol::{
    fragment_frame, fragment_tile, max_fragment_payload, DatagramHeader, FrameHeader,
    TileHeader,
};

#[library_benchmark]
fn datagram_header_encode_decode() {
    let h = DatagramHeader { frame_seq: 1024, frag_idx: 3, frag_total: 8, timestamp_us: 16_666 };
    let mut buf = Vec::with_capacity(12);
    h.encode(&mut buf);
    std::hint::black_box(DatagramHeader::decode(&buf).unwrap());
}

#[library_benchmark]
fn frame_header_encode_decode() {
    let h = FrameHeader { frame_seq: 1024, frag_idx: 3, frag_total: 8, timestamp_us: 16_666, flags: 1, reserved: 0 };
    let mut buf = Vec::with_capacity(14);
    h.encode(&mut buf);
    std::hint::black_box(FrameHeader::decode(&buf).unwrap());
}

#[library_benchmark]
fn tile_header_encode_decode() {
    let h = TileHeader::default();
    let mut buf = Vec::with_capacity(8);
    h.encode(&mut buf);
    std::hint::black_box(TileHeader::decode(&buf).unwrap());
}

#[library_benchmark]
fn fragment_tile_4kb() {
    let payload = vec![0u8; 4096];
    let max = max_fragment_payload(1200);
    std::hint::black_box(fragment_tile(0, 0, 0, 1, 0, 0, &payload, max));
}

#[library_benchmark]
fn fragment_frame_64kb() {
    let payload = vec![0u8; 65536];
    std::hint::black_box(fragment_frame(0, 0, false, &payload, 1186));
}

#[library_benchmark]
fn fec_parity_k4() {
    let frags: Vec<Vec<u8>> = (0..4).map(|i| vec![i as u8; 1186]).collect();
    let refs: Vec<&[u8]> = frags.iter().map(|f| f.as_slice()).collect();
    std::hint::black_box(fec::xor_parity(&refs));
}

library_benchmark_group!(
    name = protocol;
    benchmarks =
        datagram_header_encode_decode,
        frame_header_encode_decode,
        tile_header_encode_decode,
        fragment_tile_4kb,
        fragment_frame_64kb,
        fec_parity_k4
);

main!(library_benchmark_groups = protocol);
```

- [ ] **Step 2: Reconcile struct fields with actual code**

Run: `grep -nE "pub struct (DatagramHeader|TileHeader|FrameHeader)" ghostframe-lib/src/transport/protocol.rs`
Expected: confirms field names. If `TileHeader::default()` is missing, build it with explicit fields drawn from `protocol.rs`. The bench file must match the real struct shape.

- [ ] **Step 3: Register the bench**

Edit `ghostframe-lib/Cargo.toml`. Append:

```toml
[[bench]]
name = "codec_callgrind"
harness = false
```

- [ ] **Step 4: Run the bench (requires valgrind)**

Run: `cargo bench -p ghostframe-lib --bench codec_callgrind`
Expected: each function reports a stable instruction count. If valgrind is not installed, the bench errors with a clear message — install valgrind (`pacman -S valgrind` on Arch) and retry.

If valgrind is unavailable in the local environment, mark this task complete after `cargo build --benches` succeeds; CI will run the bench on a valgrind-capable runner.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/benches/codec_callgrind.rs ghostframe-lib/Cargo.toml
git commit -m "test(bench): iai-callgrind bench for deterministic protocol primitives"
```

---

## Task 7: README pointer for the bench harness

**Files:**
- Create: `ghostframe-lib/benches/README.md`

- [ ] **Step 1: Write the bench README**

Create `ghostframe-lib/benches/README.md`:

```markdown
# ghostframe-lib benches

Three Criterion / iai-callgrind suites covering the M3 §12 codec-comparison
requirements and the §9.4 latency/throughput tracking obligations.

| Bench | Tool | Requires |
|---|---|---|
| `codec_latency` | Criterion | `--features gpu-bench` for H.264; otherwise no-op |
| `pipeline_throughput` | Criterion | nothing (CPU only) |
| `codec_callgrind` | iai-callgrind | `valgrind` installed |

## Running

```
# Quick smoke (CI-friendly)
cargo bench -p ghostframe-lib --bench pipeline_throughput -- --test
cargo bench -p ghostframe-lib --bench codec_latency        -- --test

# Full numbers (local, with GPU + valgrind)
cargo bench -p ghostframe-lib --features gpu-bench
```

## Adding an M3 codec

1. Implement `BenchEncoder` for the new codec in `benches/codec_latency.rs`.
2. Add it to `run_codecs()`. Wrap with `Lz4Wrapper` to measure the LZ4
   break-even §12 calls for.
3. The same `ContentClass` fixtures and BenchmarkId namespace are reused;
   no harness changes needed.
```

- [ ] **Step 2: Commit**

```bash
git add ghostframe-lib/benches/README.md
git commit -m "docs(bench): how to run the bench harness and add M3 codecs"
```

---

## Final verification

- [ ] **Step 1: Build all benches**

Run: `cargo build -p ghostframe-lib --benches`
Expected: clean build for all three bench files plus fixtures.

- [ ] **Step 2: Run the CPU-only bench in test mode**

Run: `cargo bench -p ghostframe-lib --bench pipeline_throughput -- --test`
Expected: every group exits 0; HTML reports written under `target/criterion/`.

- [ ] **Step 3: Confirm the workspace test suite still passes**

Run: `cargo test --workspace`
Expected: every existing test passes — adding benches must not regress tests.
