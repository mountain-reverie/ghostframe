# PR 1: extract `ghostframe-render` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax. Model tags follow the owner's split: `[haiku]` mechanical, `[sonnet]` default, `[opus]` review only.

**Goal:** Move the portable part of `ghostframe-client-gpu` into a new crate `ghostframe-render` (MIT OR Apache-2.0) that builds for the host and for `wasm32-unknown-unknown`, with a `TileRenderer` that applies tile events and flushes, and make the native `Renderer` delegate to it. Native behaviour does not change.

**Architecture:** `ghostframe-render` holds `framebuffer`, `paint_order`, `dirty`, `coalesce`, `submit`, the Solid / PalRLE / RGBA / CDF 5/3 pipelines, an async-capable readback, and the new `tile_renderer` (the tile-event half of `renderer.rs`). `ghostframe-client-gpu` depends on it, re-exports the moved modules at their old paths, and its `Renderer` becomes `TileRenderer` + `ExportRing` + upload slabs + H.264. The native-only CDF 5/3 probe (clock and blocking wait) stays in `ghostframe-client-gpu` as a facade over the portable async self-test.

**Tech Stack:** Rust 1.96.1 (pinned), wgpu 30.0.1 (no backend feature in the new crate; its tests use wgpu's `noop` backend as a dev-dependency), serde (debug structs), `cargo`/`just`, GitHub Actions.

**Spec:** `docs/specs/all-rust-web-client-design.md` section 5.1 and the PR 1 row of section 6. **Base:** master at bcc59e03 plus the spec commit (766d6539).

**How this plan was checked.** Tasks 1 to 9 were replayed command by command, in order, on a fresh checkout of the base commit (every file below written from this document's text, every script run as printed, commits made), and the numbers in their "Expected" blocks are what that replay printed: `cargo clippy -D warnings` on the host, with the `gles` feature set and for `wasm32-unknown-unknown`; the new crate's 51 unit tests and 10 moved integration tests; the native GPU tests against a baseline taken on unmodified master; every mutation turning its test red. A last round of changes (`reset_session` clearing the palette atlas, `debug_tile` returning an `Option`, camelCase debug structs, `cdf53_gpu_enabled`) was re-verified afterwards: Task 5's and Task 6's commands on the committed Task 4 state (51 unit tests, clippy on the host and for wasm32), and Task 8's GPU test file and mutation on the final tree. Task 10 (`just ci-client`, `just ci-local`, `just containers-build`) was **not** replayed: the pieces of those recipes that matter here (clippy over the client crates, the licence, env-read and local-path checks, the native test suites, the wasm gate) were run, but the recipes themselves, the web build, the cbindgen diff, the release builds and the Docker build were not. Run them; they are the gate.

---

## Contract for PR 2 (read first)

PR 2's plan is written against these names. They are the ones the code has.

```rust
// crate ghostframe-render
pub struct TileRenderer { /* private */ }
impl TileRenderer {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue,
               format: wgpu::TextureFormat, width: u32, height: u32) -> Result<Self, GpuError>;
    pub fn apply_event(&mut self, device: &wgpu::Device, queue: &wgpu::Queue,
                       ev: ghostframe_client_core::Event) -> Result<(), GpuError>;
    pub fn flush(&mut self, device: &wgpu::Device, queue: &wgpu::Queue,
                 now_us: u64) -> Result<FlushReport, GpuError>;
    pub fn framebuffer(&self) -> &wgpu::Texture;
    pub fn fb(&self) -> &Framebuffer;                    // texture + view + size + epoch
    pub fn note_h264(&mut self, seq: u32);               // ledger + cancels CDF 5/3 draws it covers
    pub fn h264_stale(&self, seq: u32) -> bool;
    pub fn mark_all_dirty(&mut self);                    // the H.264 caller does this itself
    pub fn hold_cdf53(&mut self, hold: bool);
    pub fn reset_session(&mut self, device: &wgpu::Device, queue: &wgpu::Queue)
                         -> Result<(), GpuError>;           // pending, ledger, palette atlas, CDF 5/3
    pub fn resize(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, w: u32, h: u32);
    pub fn debug_tile(&self, x: u32, y: u32) -> Option<DebugTile>;  // None off the grid; serde, camelCase
    pub fn debug_state(&self) -> DebugState;                        // serde, camelCase
    pub fn cdf53_gpu_enabled(&self) -> bool;
    pub fn enable_cdf53_gpu(&mut self, device: &wgpu::Device);  // unverified
    pub async fn try_enable_cdf53_gpu(&mut self, device: &wgpu::Device,
                                      queue: &wgpu::Queue) -> Result<(), String>; // self-test, then enable
    pub fn has_deferred_work(&self) -> bool;
    pub fn take_cdf53_stats(&mut self) -> Option<(Cdf53Stats, u64)>;
}
pub struct FlushReport { pub recorded: bool, pub submits: u32, pub damage: Vec<coalesce::Rect>,
                         pub deferred_work: bool, pub at_us: u64 }
impl Framebuffer { pub async fn read_rgba(&self, device: &wgpu::Device, queue: &wgpu::Queue)
                       -> Result<Vec<u8>, GpuError>; }          // works on wasm; native: readback::block_on_device
```

**Device and queue are passed on every call, not stored.** The pipelines under `TileRenderer` already take them that way; the native `WgpuContext` owns the device and lends it; `WebRenderer` holds its own clones and passes them. A stored clone would tie the renderer's lifetime to the device's and gain nothing.

### Deviations from the brief, and why (the code forced each)

1. **`format` must be `Rgba8Unorm`** (`GpuError::UnsupportedFormat` otherwise). The decode shaders write the framebuffer as a `texture_storage_2d<rgba8unorm, write>`; a `bgra8unorm` framebuffer cannot be bound that way. The spec's "canvas preferred format" is therefore **not** a framebuffer format: PR 2's `WebRenderer` blits the Rgba8Unorm framebuffer to the canvas with `present_blit.wgsl` (which it already plans) and passes `Rgba8Unorm` here. The parameter stays so the call shape does not change if a second format is ever supported.
2. **There is no session-reset `Event`.** `apply_event` handles tile events and `FrameDimensions` (resize); a session reset is `reset_session(device, queue)`, a separate method (native recreates its `Renderer` per session and never calls it). It also zeroes the palette atlas, as the TypeScript `onSessionReset` does.
3. **The self-test is `async`** (`try_enable_cdf53_gpu`), because reading the result back cannot block in a browser. It does **not** calibrate the GPU's cost per tile (that needs a clock and a blocking wait); the web uses the default cost model. Native keeps its calibrating `probe()` in `ghostframe-client-gpu`.
4. **`note_h264` cancels pending CDF 5/3 draws and orders the ledger, but does not mark damage.** A client that reports damage calls `mark_all_dirty()` after blitting a picture (the native export ring already does its own).
5. **`flush` returns damage as pixel rectangles accumulated since the last flush or `drain_dirty`.** The native client drains per event instead (`drain_dirty`), to keep its export ring's dirty history exactly as before.
6. **No clock is read at all.** `now_us` is only recorded (`FlushReport::at_us`, `DebugState::last_flush_us`); nothing in the portable code needs a time source, so `web-time` is not a dependency. (`ghostframe-client-core` has no `web_time` pattern; `ghostframe-client-net` does, in `src/clock.rs`, and was checked.)
7. **`wgpu` has no backend feature in `ghostframe-render`.** `cargo check --target wasm32-unknown-unknown` passes on a backend-less wgpu (verified). PR 2 adds `webgpu` in `ghostframe-client-wasm`. `GpuDecodeError` and the PalRle error readback are native-only (they block on the device).

### Reconciliation with the PR 2 plan's contract table (C1 to C12)

PR 2's plan (`2026-11-14-all-rust-web-2-web-renderer.md`) was written in parallel against the brief. Where it differs from this code, the code wins; the differences, so the PR 2 plan can be adjusted before it is executed:

| PR 2 item | This plan's code | What PR 2 must change |
|---|---|---|
| C1, C2, C3, C4, C5, C10 | as assumed (C3: `note_h264` also cancels CDF 5/3 draws; C4: `damage` is `Vec<Rect>`) | nothing |
| C6 `debug_tile -> Option<impl Serialize>`, camelCase | `Option<DebugTile>`, `DebugState`, camelCase (`Cdf53TileDebug` nested) | nothing |
| C7 `cdf53_grid::{Probed, probe}` in the render crate; `enable_cdf53_gpu_with(device, probed, max_tiles)` | **`Probed` and the blocking `probe` stay in `ghostframe-client-gpu`** (they need `Instant` and a blocking wait). The render crate has the portable pieces: `cdf53_grid::{self_test (async), probe_grid (async), ErrorScopes}` and `TileRenderer::try_enable_cdf53_gpu(device, queue).await` (self-test, then enable, default cost model), `enable_cdf53_gpu_with(device, grid: Cdf53Grid, us_per_tile_k14: Option<f64>, max_tiles)`, `cdf53_gpu_enabled()` | The web calls `try_enable_cdf53_gpu(..).await`; PR 2's Task 4 (a non-blocking probe) is already done by this PR |
| C8 `reset_session(device, queue) -> Result` incl. palette atlas | exactly that | nothing |
| C9 `set_collect_decode_errors` / `take_decode_errors` / `GpuDecodeError` on every target | **native only** (`cfg(not(target_arch = "wasm32"))`): they exist to serve the blocking PalRle error readback | PR 2's Task 3 adds the non-blocking readback and un-gates these three on wasm |
| C11 `cdf53_passes.rs` moved to the render crate | **stays in `ghostframe-client-gpu`** with its test; PR 2 retires it | nothing: PR 2 retires it (it has no production caller) |
| C12 blocking forms replaced | Done except PalRle's error readback: `Framebuffer::read_rgba` (async) and the CDF 5/3 self-test are non-blocking; `debug_read` and `PalRlePipeline::take_errors` are native-only | PR 2's Task 3 covers `take_errors` |

---

## File map

**Created**
- `ghostframe-render/Cargo.toml`, `src/lib.rs` (modules, `GpuError`), `src/readback.rs` (async texture readback, native `block_on_device`), `src/tile_renderer.rs` (`TileRenderer` and its unit tests), `src/noop_device.rs` (test-only no-op device), `src/pipelines/mod.rs`
- `ci/check-render-portable.sh` (wasm dependency tree and source guard)
- `ghostframe-client-gpu/src/pipelines/cdf53_grid.rs` (native facade: re-exports the grid, adds `probe`), `ghostframe-client-gpu/src/submit.rs` (re-export), `ghostframe-client-gpu/tests/tile_renderer.rs` (GPU test)

**Moved with `git mv`** (history kept; one commit, no edits)
- `ghostframe-client-gpu/src/{framebuffer,paint_order,dirty,coalesce,submit}.rs` to `ghostframe-render/src/`
- `ghostframe-client-gpu/src/pipelines/{solid,palrle,rgba,cdf53_grid,cdf53_state}.rs` to `ghostframe-render/src/pipelines/`
- `ghostframe-client-gpu/tests/{coalesce,dirty}.rs` to `ghostframe-render/tests/`

**Modified**
- `Cargo.toml` (member), `Cargo.lock`, `tests/containers/test-server/Dockerfile` (per-crate COPY)
- `ghostframe-client-gpu/{Cargo.toml, src/lib.rs, src/renderer.rs, src/pipelines/mod.rs, src/upload_pool.rs, README.md}`
- `Justfile`, `.github/workflows/{ci,client-core,e2e}.yml`, `DEVELOPERS.md`

**Not touched, on purpose:** `shaders/` (the `cdf53_grid` panfrost shaderdb line is unchanged by construction; Task 10 proves it with a hash), `ghostframe-client-native`, `ghostframe-client-wasm`, `ghostframe-lib`, `ghostframe-server`. `src/pipelines/cdf53_passes.rs` and its test stay in `ghostframe-client-gpu`, untouched: it has no production caller and PR 2 retires it.

## What stays native, and why

`renderer.rs` is 2075 lines. About 700 are tile-event application and flush (they move); about 1100 are H.264 decode and blit, the dump, and ring publishing (they stay). The seam is three small interfaces, all in the plan:

| Native thing | Seam in `TileRenderer` |
|---|---|
| `ExportRing::mark_dirty` at every event | `drain_dirty(f)`; the native `Renderer` drains after each call that can mark |
| One submit per flush including the export blit | `record(.., encoder, slots) -> bool` and `finish_flush`; native adds its blit to the same encoder |
| Upload-slab tiles (`RgbaSlabPipeline`, `UploadPool`) | `SlotTile` + `trait SlotSink` (`retire`, `encode`); `NoSlots` for everyone else |
| H.264 ledger and CDF 5/3 cancel | `note_h264`, `h264_stale`, `hold_cdf53`, `supersede_cdf53` |
| `ctx.device` / `ctx.queue` | passed on each call |

## Conventions for every task

- Work from the repository root. Run builds with the repo's pinned toolchain.
- GPU-touching commands go through `~/work/gf-gpu-run` (it serialises GPU use and refuses a faulted GPU). Never run two at once.
- Commit only the paths named in the step (never `git add -A`; the tree has untracked scratch files).
- Every new file starts with `// SPDX-License-Identifier: MIT OR Apache-2.0` (`#` in shell and Python, after a shebang). `ci/check-licenses.py` enforces it.
- **No home-directory paths in the repo.** Command lines in this plan use `~/work/gf-gpu-run`, which is a path on the owner's machine, not repository content.
- The parts directory `/tmp/pr1-parts` holds scripts and fragments used to assemble files. It is not part of the repo.


## Task 1: Preflight: branch and baselines [haiku]

Record what master does today, on this host, so Task 10 compares against it instead of against an assumption.


- [ ] **Step 1: Branch**

From the spec branch's tip (or from master once the spec PR has merged).

```bash
git switch -c refactor/ghostframe-render
git status --short | head
```


- [ ] **Step 2: Write the GPU test runner**

Create the directory first with `mkdir -p /tmp/pr1-parts`.

Write `/tmp/pr1-parts/gpu-tests.sh`:

```bash
#!/usr/bin/env bash
# usage: gpu-tests.sh <outfile> [extra cargo test args]
# The GPU test set of ghostframe-client-gpu, summarised to one line per target
# with the binary hash and timings stripped, so two runs can be diffed.
set -u
out="$1"; shift
~/work/gf-gpu-run cargo test -p ghostframe-client-gpu --no-fail-fast \
    --lib --test cdf53_grid --test cdf53_renderer --test gpu_paint_order \
    --test gpu_pipelines --test gpu_flush_submits --test gpu_tileready \
    --test gpu_oracle --test gpu_export --test cdf53_passes \
    --test gpu_h264_render --test gpu_import "$@" 2>&1 \
  | grep -E '^test result|^test .* FAILED|Running' \
  | sed -E 's/ \(.*\)//; s/; finished in.*//' > "$out"
cat "$out"
```


- [ ] **Step 3: Baseline on unmodified master**

**These two failures are already there on the author's host (RADV: a binding limit larger than the test assumes, and a driver without the CPU-access hooks the test expects).** The baseline file is the reference, not green. If your host differs, your baseline differs; what matters is that Task 10's run matches it line for line except the `--lib` count.

```bash
chmod +x /tmp/pr1-parts/gpu-tests.sh
/tmp/pr1-parts/gpu-tests.sh /tmp/pr1-parts/baseline-gpu.txt
```

Expected:

```
     Running unittests src/lib.rs
test result: ok. 72 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/cdf53_grid.rs
test an_oversized_grid_is_refused_and_leaves_the_grid_unusable ... FAILED
test result: FAILED. 18 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out
...
     Running tests/gpu_tileready.rs
test drivers_without_cpu_access_hooks_skip_the_sync_ioctls_and_others_keep_them ... FAILED
test result: FAILED. 17 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/gpu_h264_render.rs
...
     Running tests/gpu_import.rs
...
```

`gpu_h264_render` and `gpu_import` are in the set on purpose: Task 7 rewrites the native H.264 path (`self.fb` becomes `self.tiles.fb()`, and the ledger calls go through `note_h264`), and no other compared target drives it. `gpu_h264_render` needs VA-API, and `gpu-run` points VA-API at a missing device, so on a host without a usable device it fails (or skips) in the baseline too. Whatever it prints here is its baseline; the comparison only asks that it does not change. If it cannot run under the wrapper, say so in the PR and run it once, solo and with the owner's approval (`GF_ALLOW_VAAPI=1`, see the wrapper's header), before and after; `ghostframe-e2e --test h264` is the owner-run end-to-end check of the same path.


- [ ] **Step 4: Record the shader hashes**

No panfrost device on this host. The shaders are not edited by this PR, so the `cdf53_grid` shaderdb line (about 930 instructions, 8 registers, ~21:57 spills) cannot move; Task 10 proves it by hash. On a Mali host, also run the `shaderdb` check from AGENTS.md before and after.

```bash
sha256sum shaders/client/*.wgsl > /tmp/pr1-parts/shaders.sha256
wc -l /tmp/pr1-parts/shaders.sha256
```

Expected:

```
18 /tmp/pr1-parts/shaders.sha256
```


## Task 2: Scaffold the crate, the workspace member and the Docker COPY [haiku]

A crate that compiles and passes the licence check, so every later commit has somewhere to land.


- [ ] **Step 1: Create the manifest and a minimal lib.rs**

Write `ghostframe-render/Cargo.toml`:

```toml
[package]
name = "ghostframe-render"
version = "0.1.0"
edition = "2021"
license = "MIT OR Apache-2.0"

# The portable tile renderer: the framebuffer, the Solid / PalRLE / RGBA /
# CDF 5/3 pipelines, the paint-order ledger and the damage bookkeeping,
# extracted from `ghostframe-client-gpu` so the native client and the web
# client draw tiles with the same code.
#
# It builds for the host and for `wasm32-unknown-unknown`, and that is the
# constraint every dependency here answers to. Nothing in it may need a
# thread, a blocking wait, a dmabuf, EGL, ffmpeg or `std::time::Instant`:
# time comes in as a parameter and the device is the caller's. `pollster`,
# `libc`, `ash`, `khronos-egl` and `ghostframe-client-h264` are therefore
# not dependencies, and a new one needs a reason that survives that rule.
# `ci/check-licenses.py` also keeps this crate off `ghostframe-lib`.

[dependencies]
ghostframe-protocol = { path = "../ghostframe-protocol" }
ghostframe-client-core = { path = "../ghostframe-client-core" }
# No backend feature: the caller's crate picks one (`ghostframe-client-gpu`
# selects vulkan or gles; the web client selects webgpu). The workspace entry
# enables `wgsl` only.
wgpu = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
serde = { version = "1", features = ["derive"] }

[dev-dependencies]
proptest = "1"
serde_json = "1"
# wgpu's no-op backend: a real `Device` that validates and records but runs
# nothing, so `TileRenderer`'s bookkeeping is tested on a runner with no GPU.
# Dev-only, so it never reaches a daemon or client build.
wgpu = { workspace = true, features = ["noop"] }
```

Write `ghostframe-render/src/lib.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The portable tile renderer shared by the native and the web client.
//!
//! Scaffold: the modules arrive in the following commits.

use thiserror::Error;

/// What can go wrong in this crate. `ghostframe-client-gpu`'s own `GpuError`
/// converts from it (`From`), so its callers keep matching the variants they
/// always matched.
#[derive(Debug, Error)]
pub enum GpuError {
    /// The device failed or was lost while the renderer waited on it (a
    /// buffer map or a poll). Carried as an error so the render thread can
    /// surface it instead of panicking.
    #[error("gpu device: {0}")]
    Device(String),

    /// A frame size outside `1..=MAX_FRAME_DIMENSION` per axis. The value
    /// originates on the network, so it is refused before any texture or
    /// buffer is sized from it.
    #[error(
        "frame dimensions {width}x{height} are outside 1..={max} per axis",
        max = ghostframe_client_core::MAX_FRAME_DIMENSION
    )]
    InvalidDimensions { width: u32, height: u32 },

    /// `TileRenderer::new` was given a framebuffer format the decode shaders
    /// cannot write. They bind the framebuffer as a `rgba8unorm` storage
    /// texture, which no other format (the canvas's preferred `bgra8unorm`
    /// included) can stand in for.
    #[error(
        "framebuffer format {0:?} is not supported: the decode shaders write an rgba8unorm storage texture"
    )]
    UnsupportedFormat(wgpu::TextureFormat),
}
```


- [ ] **Step 2: Add the workspace member and the test image's COPY**

```bash
python3 - <<'EOF'
def edit(path, a, b):
    s = open(path).read()
    assert s.count(a) == 1, (path, a)
    open(path, 'w').write(s.replace(a, b))

edit('Cargo.toml', '    "ghostframe-client-core",\n', '    "ghostframe-client-core",\n    "ghostframe-render",\n')
# The test-server image copies each workspace member's manifest and source
# before resolving the workspace; a member it does not know fails the build
# before any test runs.
edit('tests/containers/test-server/Dockerfile',
     "COPY ghostframe-client-gpu/src/ ghostframe-client-gpu/src/\n",
     "COPY ghostframe-client-gpu/src/ ghostframe-client-gpu/src/\n"
     "# ghostframe-client-gpu depends on it.\n"
     "COPY ghostframe-render/Cargo.toml ghostframe-render/Cargo.toml\n"
     "COPY ghostframe-render/src/ ghostframe-render/src/\n")
EOF
git diff --stat
```


- [ ] **Step 3: Check it builds and passes the licence gate**

The crate count is one more than before the change; the third-party count does not move (serde, wgpu and the rest were already in the lock).

```bash
cargo check -p ghostframe-render
ci/check-licenses.py
```

Expected:

```
    Finished `dev` profile ...
licences OK: 17 crates, 574 third-party crates
```

```bash
git add Cargo.toml Cargo.lock ghostframe-render/Cargo.toml ghostframe-render/src/lib.rs tests/containers/test-server/Dockerfile
git status --short | head -30
git commit -q -F - <<'MSG'
feat(render): scaffold ghostframe-render (MIT OR Apache-2.0), workspace member, image COPY

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>
MSG
git log --oneline -1
```


## Task 3: Move the files (history only) [haiku]

One commit that only renames, so `git log --follow` and blame survive. **This commit does not build** (the moved files still say `crate::export` and friends); Task 4 repairs them. Say so in the message so a bisect knows.


- [ ] **Step 1: git mv**

```bash
mkdir -p ghostframe-render/src/pipelines ghostframe-render/tests
for f in coalesce dirty framebuffer paint_order submit; do
  git mv ghostframe-client-gpu/src/$f.rs ghostframe-render/src/$f.rs
done
for f in solid palrle rgba cdf53_grid cdf53_state; do
  git mv ghostframe-client-gpu/src/pipelines/$f.rs ghostframe-render/src/pipelines/$f.rs
done
for f in coalesce dirty; do
  git mv ghostframe-client-gpu/tests/$f.rs ghostframe-render/tests/$f.rs
done
git status --short | grep -c '^R'
```

Expected:

```
12
```


- [ ] **Step 2: Commit the renames alone**

```bash
git commit -q -m "refactor(render): git mv the portable renderer files into ghostframe-render

Rename only, so history follows the files. This commit does not build; the
next one makes the moved files compile in their new crate.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
git log --oneline -1
git diff --stat HEAD~1 | tail -1
```

Expected:

```
12 files changed, 0 insertions(+), 0 deletions(-)
```


## Task 4: Make the moved files compile in `ghostframe-render` [sonnet]

Fix paths, split the CDF 5/3 probe (the native half stays behind), make the readback async-capable. After this task `cargo test -p ghostframe-render` passes with the moved unit and integration tests; nothing uses `TileRenderer` yet, so the ledger is unused (dead-code warnings are expected until Task 5).


- [ ] **Step 1: Write the new modules**

Write `ghostframe-render/src/lib.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The portable tile renderer shared by the native and the web client.
//!
//! The framebuffer, the Solid / PalRLE / RGBA / CDF 5/3 pipelines, the
//! paint-order ledger and the damage bookkeeping, extracted from
//! `ghostframe-client-gpu`. The crate builds for `wasm32-unknown-unknown`: no
//! threads, no blocking waits (except in the native-only helpers marked as
//! such), no `std::time::Instant`. Time comes in as `now_us`.
//!
//! ## Catch-all match arms
//!
//! `clippy::wildcard_enum_match_arm` is on for non-test code, as in
//! `ghostframe-client-gpu`: a `_ =>` arm that silently absorbs a new variant
//! hid real defects twice. List the variants, so adding one is a compile error
//! at every site that must decide about it.
#![cfg_attr(not(test), warn(clippy::wildcard_enum_match_arm))]

pub mod coalesce;
pub mod dirty;
pub mod framebuffer;
pub(crate) mod paint_order;
pub mod pipelines;
pub mod readback;
pub mod submit;

use thiserror::Error;

/// What can go wrong in this crate. `ghostframe-client-gpu`'s own `GpuError`
/// converts from it (`From`), so its callers keep matching the variants they
/// always matched.
#[derive(Debug, Error)]
pub enum GpuError {
    /// The device failed or was lost while the renderer waited on it (a
    /// buffer map or a poll). Carried as an error so the render thread can
    /// surface it instead of panicking.
    #[error("gpu device: {0}")]
    Device(String),

    /// A frame size outside `1..=MAX_FRAME_DIMENSION` per axis. The value
    /// originates on the network, so it is refused before any texture or
    /// buffer is sized from it.
    #[error(
        "frame dimensions {width}x{height} are outside 1..={max} per axis",
        max = ghostframe_client_core::MAX_FRAME_DIMENSION
    )]
    InvalidDimensions { width: u32, height: u32 },

    /// `TileRenderer::new` was given a framebuffer format the decode shaders
    /// cannot write. They bind the framebuffer as a `rgba8unorm` storage
    /// texture, which no other format (the canvas's preferred `bgra8unorm`
    /// included) can stand in for.
    #[error(
        "framebuffer format {0:?} is not supported: the decode shaders write an rgba8unorm storage texture"
    )]
    UnsupportedFormat(wgpu::TextureFormat),
}
```

`readback.rs` turns `map_async`'s callback into a future with a one-shot channel, so the CDF 5/3 self-test reads pixels back the same way in a browser and natively. `block_on_device` is the native driver: it polls the future and, whenever it is pending, the device.

Write `ghostframe-render/src/readback.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Reading a texture back to the CPU, on every target.
//!
//! `wgpu::Buffer::map_async` takes a callback, and the callback runs when the
//! device is polled (native) or when the browser's event loop gets to it (web).
//! [`read_texture_rgba8`] turns that into a future with a one-shot channel, so
//! the same code serves the CDF 5/3 self-test on both. A caller on a thread
//! with no event loop uses [`block_on_device`], which polls the device between
//! polls of the future.

use crate::GpuError;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

struct Slot<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

/// The receiving half of [`oneshot`].
struct Receiver<T>(Arc<Mutex<Slot<T>>>);

/// A one-shot channel whose sender is a `Send` closure (what `map_async`
/// takes) and whose receiver is a future.
fn oneshot<T: Send + 'static>() -> (impl FnOnce(T) + Send + 'static, Receiver<T>) {
    let shared = Arc::new(Mutex::new(Slot {
        value: None,
        waker: None,
    }));
    let tx = Arc::clone(&shared);
    let send = move |value: T| {
        let waker = {
            let mut slot = tx.lock().expect("oneshot lock");
            slot.value = Some(value);
            slot.waker.take()
        };
        // Outside the lock: a waker may poll the receiver inline.
        if let Some(waker) = waker {
            waker.wake();
        }
    };
    (send, Receiver(shared))
}

impl<T> Future for Receiver<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut slot = self.0.lock().expect("oneshot lock");
        match slot.value.take() {
            Some(value) => Poll::Ready(value),
            None => {
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// Copy `texture` (4 bytes per texel, `width` x `height`) into a staging
/// buffer and return it as tight rows, `width * height * 4` bytes.
///
/// Submits one encoder of its own, through [`crate::submit::submit`].
pub async fn read_texture_rgba8(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, GpuError> {
    let unpadded_bytes_per_row = width * 4;
    let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ghostframe-texture-readback"),
        size: padded_bytes_per_row as u64 * height as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("ghostframe-texture-readback"),
    });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    crate::submit::submit(queue, encoder);

    let (done, mapped) = oneshot();
    staging.slice(..).map_async(wgpu::MapMode::Read, done);
    mapped
        .await
        .map_err(|e| GpuError::Device(format!("map readback staging buffer: {e}")))?;

    let padded = staging
        .slice(..)
        .get_mapped_range()
        .map_err(|e| GpuError::Device(format!("readback get_mapped_range: {e}")))?;
    // Strip row padding: the staging buffer's rows are padded to
    // `COPY_BYTES_PER_ROW_ALIGNMENT`, but callers want tight rows. Skipping
    // this looks right at the start of each row and drifts further out of
    // alignment with every row after it.
    let mut tight = Vec::with_capacity(unpadded_bytes_per_row as usize * height as usize);
    for row in 0..height as usize {
        let start = row * padded_bytes_per_row as usize;
        tight.extend_from_slice(&padded[start..start + unpadded_bytes_per_row as usize]);
    }
    drop(padded);
    staging.unmap();
    Ok(tight)
}

/// Drive `fut` to completion on this thread, polling `device` whenever it is
/// pending. Native only: a browser has no blocking wait, and its event loop
/// does this job.
///
/// "Pending" is taken to mean "a buffer-map or work-done callback is
/// outstanding", which is true of every future in this crate. A future that
/// waits on anything else would spin here.
///
/// # Panics
///
/// If the device cannot be polled (it was lost): the future could then never
/// complete.
#[cfg(not(target_arch = "wasm32"))]
pub fn block_on_device<F: Future>(device: &wgpu::Device, fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
            return value;
        }
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("polling the device for a pending GPU callback");
    }
}
```

Write `ghostframe-render/src/pipelines/mod.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tile codec pipelines that run the real, shipped WGSL against the
//! persistent [`crate::framebuffer::Framebuffer`].
//!
//! Each pipeline here loads its shader with `include_str!` from
//! `shaders/client/`, the directory both the web client and the native client
//! compile the exact same files from.

pub mod cdf53_grid;
pub mod cdf53_state;
pub mod palrle;
pub mod rgba;
pub mod solid;
```


- [ ] **Step 2: Write and run the edit script**

It asserts every anchor it edits, so it fails loudly instead of editing the wrong place. What it does: `submit` becomes `pub` (the native crate counts submits through it); doc links stop naming `crate::renderer` and `crate::export`; `Framebuffer::debug_read` becomes `read_rgba` (async) plus a native-only blocking `debug_read`; `cdf53_grid` loses `probe`/`calibrate`/`pollster` (the native half comes back in Task 7) and gains `ErrorScopes`, `probe_grid`, an async `self_test` and the `self_test_tiles` fixtures; the two moved tests import `ghostframe_render`.

Write `/tmp/pr1-parts/edit_moved.py`:

```python
#!/usr/bin/env python3
"""T3: turn the files git-mv'd into ghostframe-render into files that belong
to it. Run from the repository root."""
import re

def edit(path, pairs, count=1):
    s = open(path).read()
    for a, b in pairs:
        assert s.count(a) == count, (path, s.count(a), a)
        s = s.replace(a, b)
    open(path, 'w').write(s)

R = 'ghostframe-render/src/'

# submit.rs: the counter is used across the crate boundary now.
s = open(R + 'submit.rs').read()
assert s.count('pub(crate) fn ') == 3
s = s.replace('pub(crate) fn ', 'pub fn ')
a = "[`Renderer::flush`](crate::renderer::Renderer::flush)"
assert s.count(a) == 1
s = s.replace(a, "[`TileRenderer::flush`](crate::tile_renderer::TileRenderer::flush)")
open(R + 'submit.rs', 'w').write(s)

# paint_order.rs: only the doc link.
edit(R + 'paint_order.rs', [(
    "[`Renderer::flush`](crate::renderer::Renderer::flush)",
    "[`TileRenderer::flush`](crate::tile_renderer::TileRenderer::flush)")])

# framebuffer.rs: no reference to the native export module; the readback is
# async-first, with the blocking form kept for native tests.
s = open(R + 'framebuffer.rs').read()
assert s.count("[`crate::export::ExportedImage`]") == 2
s = s.replace("[`crate::export::ExportedImage`]", "`ExportedImage` (native)")
start = s.index("    /// Read the framebuffer back as tight RGBA. Test use only.\n    pub fn debug_read")
end = s.index("impl BlitPipeline")
s = s[:start] + '''    /// Read the framebuffer back as tight RGBA (`width * height * 4` bytes).
    ///
    /// Completes when the GPU has delivered the bytes. A browser's event loop
    /// drives that; on native something must poll the device while the future
    /// is pending, which is what [`crate::readback::block_on_device`] does.
    pub async fn read_rgba(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<Vec<u8>, crate::GpuError> {
        crate::readback::read_texture_rgba8(device, queue, &self.texture, self.width, self.height)
            .await
    }

    /// [`Framebuffer::read_rgba`], blocking. Native only; test use.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn debug_read(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> Vec<u8> {
        crate::readback::block_on_device(device, self.read_rgba(device, queue))
            .expect("framebuffer readback")
    }
}

''' + s[end:]
open(R + 'framebuffer.rs', 'w').write(s)

# cdf53_grid.rs: no probe (it needs a clock and a blocking wait: it stays in
# ghostframe-client-gpu), an async self-test, and the error scopes as a type.
s = open(R + 'pipelines/cdf53_grid.rs').read()
a = s.index("/// A grid that passed [`probe`], with what the probe measured.")
b = s.index("/// One tile as the wire carries it, decoded: the planes that exist, and the")
s = s[:a] + '''/// Out-of-memory, internal and validation error scopes opened together, so a
/// shader or pipeline that fails to build comes back as an error instead of
/// reaching wgpu's default uncaptured-error handler, which panics.
///
/// Open with [`ErrorScopes::push`], do the fallible work, then
/// [`ErrorScopes::finish`] and read the first error it reports.
pub struct ErrorScopes {
    oom: wgpu::ErrorScopeGuard,
    internal: wgpu::ErrorScopeGuard,
    validation: wgpu::ErrorScopeGuard,
}

impl ErrorScopes {
    pub fn push(device: &wgpu::Device) -> Self {
        let oom = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        ErrorScopes {
            oom,
            internal,
            validation,
        }
    }

    /// Close the scopes (last in, first out) and report the first error.
    pub async fn finish(self) -> Option<String> {
        let errors = [
            self.validation.pop().await,
            self.internal.pop().await,
            self.oom.pop().await,
        ];
        errors
            .into_iter()
            .flatten()
            .next()
            .map(|e| format!("GPU error: {e}"))
    }
}

/// Build a grid with `make` and prove it works on this device
/// ([`self_test`]), inside [`ErrorScopes`]. The grid it returns is the one
/// that was tested: hand it to `TileRenderer::enable_cdf53_gpu_with` rather
/// than building another (the reconstruct shader is slow to compile on
/// panfrost).
///
/// Does not time anything: the native client adds a calibration of its own
/// (`ghostframe_client_gpu::pipelines::cdf53_grid::probe`), which needs a
/// clock and a blocking wait. A browser uses this and the default cost model.
pub async fn probe_grid(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    make: impl FnOnce(&wgpu::Device) -> Cdf53Grid,
) -> Result<Cdf53Grid, String> {
    let scopes = ErrorScopes::push(device);
    let mut grid = make(device);
    let tested = self_test(device, queue, &mut grid).await;
    if let Some(e) = scopes.finish().await {
        return Err(e);
    }
    tested.map(|()| grid)
}

''' + s[b:]
for a, b in [
    ('''struct Fixture {
    present: u16,
    planes: Vec<Option<Vec<u8>>>,
}

/// Two known pictures: a smooth ramp with an edge, and noise.
fn fixtures() -> Vec<Fixture> {''', '''pub struct SelfTestTile {
    pub present: u16,
    pub planes: Vec<Option<Vec<u8>>>,
}

/// Two known pictures: a smooth ramp with an edge, and noise.
pub fn self_test_tiles() -> Vec<SelfTestTile> {'''),
    ("            Fixture { present, planes }", "            SelfTestTile { present, planes }"),
    ('''pub fn self_test_with(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    grid: &mut Cdf53Grid,
) -> Result<(), String> {''', '''pub async fn self_test(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    grid: &mut Cdf53Grid,
) -> Result<(), String> {'''),
    ("    let cases = fixtures();", "    let cases = self_test_tiles();"),
    ("        let got = fb.debug_read(device, queue);",
     "        let got = fb\n            .read_rgba(device, queue)\n            .await\n            .map_err(|e| e.to_string())?;"),
]:
    assert s.count(a) == 1, a
    s = s.replace(a, b)
a = s.index("/// Tiles in the calibration dispatch")
b = s.index("fn entry(binding: u32")
s = s[:a] + s[b:]
open(R + 'pipelines/cdf53_grid.rs', 'w').write(s)
for bad in ("pollster", "Instant", "fixtures()", "calibrate"):
    assert bad not in s, bad

# The two tests that moved name the new crate.
for t in ('ghostframe-render/tests/coalesce.rs', 'ghostframe-render/tests/dirty.rs'):
    s = open(t).read()
    assert 'ghostframe_client_gpu::' in s
    open(t, 'w').write(s.replace('ghostframe_client_gpu::', 'ghostframe_render::'))
print("edit_moved: ok")
```

```bash
python3 /tmp/pr1-parts/edit_moved.py
cargo fmt -p ghostframe-render
```

Expected:

```
edit_moved: ok
```


- [ ] **Step 3: Test**

36 = the 28 `cdf53_state` tests + 6 `paint_order` + 2 `cdf53_grid`, all moved. 6 and 4 are `coalesce` and `dirty`. Warnings that `SeqLedger` and friends are never used are expected here and disappear in Task 5.

```bash
cargo test -p ghostframe-render 2>&1 | grep -E "^test result|warning: .*never|error" 
```

Expected:

```
test result: ok. 36 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```


- [ ] **Step 4: Break it: the moved `dirty` test must bite in its new home**

`DirtyGrid::union_with` must OR the other grid in; overwriting instead loses every generation but the last. The file is restored from the copy in the last line.

```bash
cp ghostframe-render/src/dirty.rs /tmp/pr1-parts/dirty.rs.orig
python3 - <<'EOF'
p = 'ghostframe-render/src/dirty.rs'
s = open(p).read()
a = "            *a |= *b;"
assert s.count(a) == 1
open(p, 'w').write(s.replace(a, "            *a = *b;"))
EOF
cargo test -p ghostframe-render --test dirty 2>&1 | grep -E "^test |test result"
cp /tmp/pr1-parts/dirty.rs.orig ghostframe-render/src/dirty.rs
```

Expected:

```
test history_unions_every_generation_after_the_buffer_was_filled ... FAILED
test result: FAILED. 3 passed; 1 failed
```

```bash
git add ghostframe-render
git status --short | head -30
git commit -q -F - <<'MSG'
refactor(render): make the moved files compile in ghostframe-render

submit is public; the CDF 5/3 self-test is async and reads back through
readback::read_texture_rgba8; the native probe and its calibration stay behind
for ghostframe-client-gpu; Framebuffer::debug_read is native-only over the
async read_rgba.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>
MSG
git log --oneline -1
```


## Task 5: `TileRenderer` [sonnet]

The tile-event half of `renderer.rs`, as a portable type. The bodies of the pieces that move are extracted from `renderer.rs` by the script (so they are byte-identical to what ran in production); everything new is written out below. Tests run on wgpu's no-op backend, so they run in CI with no GPU.


- [ ] **Step 1: Add the ledger and CDF 5/3 diagnostics the new type reads, and the module list**

Write `/tmp/pr1-parts/edit_t4.py`:

```python
#!/usr/bin/env python3
"""T4: the ledger and CDF 5/3 state getters TileRenderer's diagnostics read,
and the module list. Run from the repository root."""
def edit(path, pairs):
    s = open(path).read()
    for a, b in pairs:
        assert s.count(a) == 1, (path, a)
        s = s.replace(a, b)
    open(path, 'w').write(s)

R = 'ghostframe-render/src/'
edit(R + 'paint_order.rs', [("    pub(crate) fn clear(&mut self) {", '''    /// The newest `frame_seq` noted for tile `(tx, ty)`, for diagnostics.
    pub(crate) fn newest_for(&self, tx: u8, ty: u8) -> Option<u32> {
        self.newest
            .get(ty as usize * 256 + tx as usize)
            .copied()
            .flatten()
    }

    /// The newest `frame_seq` of a tile entry noted anywhere, for diagnostics.
    pub(crate) fn max_tile(&self) -> Option<u32> {
        self.max_tile
    }

    /// The newest H.264 picture noted, for diagnostics.
    pub(crate) fn h264(&self) -> Option<u32> {
        self.h264
    }

    pub(crate) fn clear(&mut self) {''')])

edit(R + 'pipelines/cdf53_state.rs', [
("pub struct Cdf53GridState {", '''/// One tile's host-side state, for diagnostics. Serialises to JSON with
/// camelCase field names.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cdf53TileDebug {
    pub generation: u8,
    /// Bit `p` set when pass `p` of this generation has arrived.
    pub received_mask: u16,
    /// Pass 0's `present_passes`, once it has arrived.
    pub present_passes: Option<u16>,
    /// The prefix length last handed to the GPU for this generation.
    pub drawn_k: Option<u8>,
    /// Newest `frame_seq` of any pass of this generation.
    pub newest_seq: u32,
    pub pending: bool,
}

pub struct Cdf53GridState {'''),
("    fn index(&self, tile_x: u8, tile_y: u8) -> Option<usize> {", '''    /// The state of a tile that has received anything since it was last
    /// reset, for diagnostics; `None` for one that has not, or is off the grid.
    pub fn debug_tile(&self, tile_x: u8, tile_y: u8) -> Option<Cdf53TileDebug> {
        let t = &self.tiles[self.index(tile_x, tile_y)?];
        t.live.then_some(Cdf53TileDebug {
            generation: t.generation,
            received_mask: t.received,
            present_passes: t.present,
            drawn_k: t.drawn_k,
            newest_seq: t.newest_seq,
            pending: t.pending,
        })
    }

    fn index(&self, tile_x: u8, tile_y: u8) -> Option<usize> {'''),
])

edit(R + 'pipelines/palrle.rs', [
("    /// Ensure the per-batch buffers can hold at least `capacity` tiles,", '''    /// Zero every palette, as a session reset must: the next session's
    /// palettes arrive as `PaletteUpdated` events, and until they do a tile
    /// that names a stale one should draw nothing rather than the old colours.
    pub fn clear_palettes(&mut self, queue: &wgpu::Queue) {
        queue.write_buffer(&self.palette_atlas, 0, &[0u8; PALETTE_ATLAS_SIZE as usize]);
    }

    /// Ensure the per-batch buffers can hold at least `capacity` tiles,'''),
])

edit(R + 'lib.rs', [
("pub mod submit;\n", "pub mod submit;\npub mod tile_renderer;\n\n#[cfg(test)]\nmod noop_device;\n\npub use tile_renderer::{\n    dimensions_valid, tile_grid, DebugState, DebugTile, FlushReport, GpuDecodeError, NoSlots,\n    SlotSink, SlotTile, TileRenderer,\n};\n"),
])
print("edit_t4: ok")
```

```bash
python3 /tmp/pr1-parts/edit_t4.py
```

Expected:

```
edit_t4: ok
```


- [ ] **Step 2: Write the no-op device helper**

wgpu's `noop` backend validates every call and runs nothing; a texture reads back as zeros. That is exactly enough for what `TileRenderer` decides (ordering, damage, submit counts, what it refuses). What pixels it draws is Task 8's GPU test.

Write `ghostframe-render/src/noop_device.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! A `wgpu::Device` for unit tests that have no GPU: wgpu's no-op backend
//! validates everything and records nothing it can run, so a texture reads back
//! as zeros. Enough to test what [`crate::TileRenderer`] decides (ordering,
//! damage, submit counts, what it refuses); not what pixels it draws -- that is
//! `ghostframe-client-gpu`'s GPU tests, which need a real device.

use std::future::Future;
use std::task::{Context, Poll, Waker};

/// A device and queue on the no-op backend.
pub(crate) fn device() -> (wgpu::Device, wgpu::Queue) {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::NOOP,
        backend_options: wgpu::BackendOptions {
            noop: wgpu::NoopBackendOptions {
                enable: true,
                ..Default::default()
            },
            ..Default::default()
        },
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        .expect("no-op adapter");
    block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("no-op device")
}

/// Poll `fut` until it is ready. For futures that complete without a device
/// callback (the no-op backend's request calls).
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
    }
}
```


- [ ] **Step 3: Add the `noop` dev-dependency's feature to the lockfile and check it resolves**

```bash
cargo metadata --format-version 1 --no-deps >/dev/null && echo resolved
```

Expected:

```
resolved
```


- [ ] **Step 4: Write the parts of tile_renderer.rs**

Header, imports, `SlotTile`/`SlotSink`/`NoSlots` and the private `RgbaSrc`.

Write `/tmp/pr1-parts/tr_head.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! [`TileRenderer`]: applies tile events, records their draws, submits on flush.
//!
//! Extracted from `ghostframe-client-gpu`'s `Renderer`, which keeps the native
//! half (export ring, upload slabs, H.264 decode) around one of these.
//!
//! **Device and queue are passed on every call**, not stored. The pipelines
//! under it already take them that way, the native client's `WgpuContext`
//! owns the device and lends it, and the web client's `WebRenderer` holds its
//! own clones. A stored clone would make the renderer's lifetime part of the
//! device's and add nothing the callers lack.
//!
//! Per-frame tile work is batched: [`TileRenderer::apply_event`] only records
//! what arrived; [`TileRenderer::flush`] runs the render and compute passes
//! once per frame, in one submit, rather than one pass per tile.

use crate::coalesce::{coalesce, Rect};
use crate::dirty::DirtyGrid;
use crate::framebuffer::Framebuffer;
use crate::paint_order::SeqLedger;
use crate::pipelines::cdf53_grid::{probe_grid, Cdf53Grid};
use crate::pipelines::cdf53_state::{
    Cdf53GridState, Cdf53Stats, Cdf53TileDebug, FLUSH_BUDGET_US, MAX_TILES_PER_FLUSH,
};
use crate::pipelines::palrle::PalRlePipeline;
use crate::pipelines::rgba::RgbaPipeline;
use crate::pipelines::solid::SolidPipeline;
use crate::GpuError;

use ghostframe_client_core::cdf53_prevalidate::PrevalidatedCdf53;
use ghostframe_client_core::cdf53_tile_state::Cdf53TileState;
use ghostframe_client_core::{DecodeErrorCode, Event, TileData};
use ghostframe_protocol::protocol::Codec;
use ghostframe_protocol::tile::TILE_SIZE;
use serde::Serialize;

/// Whether `width x height` is a size a renderer will allocate for.
/// The same bound `ClientCore` enforces; repeated here because the renderer
/// is a public API and must not trust that every caller went through it.
pub fn dimensions_valid(width: u32, height: u32) -> bool {
    let range = 1..=ghostframe_client_core::MAX_FRAME_DIMENSION;
    range.contains(&width) && range.contains(&height)
}

/// Tiles across and down for a `width x height` framebuffer.
pub fn tile_grid(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(TILE_SIZE), height.div_ceil(TILE_SIZE))
}

/// A tile whose pixels already sit in a caller-owned slab (the native
/// client's upload pool), so only its place in the paint order is recorded
/// here. `slab` and `index` are what [`TileRenderer`] needs to tell two slots
/// apart; `token` is the caller's own handle, handed back to the
/// [`SlotSink`] untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotTile {
    pub slab: usize,
    pub index: u32,
    pub token: u64,
}

/// What the caller does with [`SlotTile`]s at a flush. A `TileRenderer` used
/// without slots passes [`NoSlots`].
pub trait SlotSink {
    /// A slot given to this flush is done with: drawn, superseded or dropped.
    /// Called exactly once per slot.
    fn retire(&mut self, slot: &SlotTile);

    /// Record the draws of the slots that survived, one `(tile_x, tile_y,
    /// slot)` per tile (the later of two for a tile has already won). Return
    /// whether anything was recorded.
    fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        fb: &Framebuffer,
        tiles: &[(u32, u32, SlotTile)],
    ) -> bool;
}

/// The [`SlotSink`] for a renderer that is given no slot tiles.
pub struct NoSlots;

impl SlotSink for NoSlots {
    fn retire(&mut self, _slot: &SlotTile) {}

    fn encode(
        &mut self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _encoder: &mut wgpu::CommandEncoder,
        _fb: &Framebuffer,
        _tiles: &[(u32, u32, SlotTile)],
    ) -> bool {
        false
    }
}

/// Where one buffered `TileReady` tile's pixels are.
enum RgbaSrc {
    /// Owned bytes, to be staged and copied at flush.
    Pixels(Vec<u8>),
    /// A slot of a slab the caller already wrote.
    Slot(SlotTile),
}
```

`Cdf53Gpu`, `FlushReport`, `DebugTile`, `DebugState`, `damage_rects` (pure, unit-tested) and the `TileRenderer` struct.

Write `/tmp/pr1-parts/tr_types.rs`:

```rust
/// The GPU CDF 5/3 decoder and the host state that schedules it.
struct Cdf53Gpu {
    grid: Cdf53Grid,
    state: Cdf53GridState,
    /// Grid frames recorded since the stats were last taken.
    frames: u64,
    cpu_fallback_logged: bool,
}

/// More tiles than this marked dirty between two drains collapse into "all of
/// it", so a caller that never drains cannot grow the list without bound.
const MAX_DIRTY_LIST: usize = 1 << 16;

/// What one flush did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushReport {
    /// Whether any GPU work was recorded (and so submitted).
    pub recorded: bool,
    /// Submits this flush made for its own work: 1 when `recorded`, else 0.
    pub submits: u32,
    /// Pixel rectangles that changed since the previous flush (or since the
    /// caller last drained them with [`TileRenderer::drain_dirty`]): the dirty
    /// tiles coalesced, clamped to the framebuffer; the whole framebuffer after
    /// a resize or [`TileRenderer::mark_all_dirty`].
    pub damage: Vec<Rect>,
    /// CDF 5/3 tiles are waiting for a later flush (the GPU budget was spent,
    /// or a partial tile is being throttled). The caller should flush again
    /// soon even if no event arrives.
    pub deferred_work: bool,
    /// The `now_us` this flush was given.
    pub at_us: u64,
}

/// One tile as the renderer sees it, for diagnostics. Serialises to JSON with
/// camelCase field names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugTile {
    pub tile_x: u32,
    pub tile_y: u32,
    /// The newest `frame_seq` painted (or about to be) on this tile.
    pub newest_frame_seq: Option<u32>,
    /// The CDF 5/3 grid's state for the tile; `None` when the GPU grid is not
    /// enabled or the tile has received nothing.
    pub cdf53: Option<Cdf53TileDebug>,
}

/// The renderer's own counters and sizes, for diagnostics. Serialises to JSON
/// with camelCase field names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugState {
    pub width: u32,
    pub height: u32,
    pub cols: u32,
    pub rows: u32,
    /// Completed flushes.
    pub flushes: u64,
    /// The `now_us` of the most recent flush.
    pub last_flush_us: u64,
    pub pending_solid: usize,
    pub pending_palrle: usize,
    pub pending_rgba: usize,
    /// The GPU CDF 5/3 grid was enabled, and the frame fits it.
    pub cdf53_gpu: bool,
    /// CDF 5/3 tiles are waiting for a flush.
    pub cdf53_pending: bool,
    /// The newest `frame_seq` of any tile entry noted.
    pub newest_tile_seq: Option<u32>,
    /// The newest H.264 picture noted with [`TileRenderer::note_h264`].
    pub newest_h264_seq: Option<u32>,
}

/// The pixel rectangles of `tiles`, as a flush reports them. Tiles outside
/// the grid are dropped (the wire carries `u8` coordinates, whatever the
/// framebuffer's size).
fn damage_rects(all: bool, tiles: &[(u8, u8)], fb_width: u32, fb_height: u32) -> Vec<Rect> {
    if all {
        return vec![Rect {
            x: 0,
            y: 0,
            w: fb_width,
            h: fb_height,
        }];
    }
    let (cols, rows) = tile_grid(fb_width, fb_height);
    let mut grid = DirtyGrid::new(cols, rows);
    let mut any = false;
    for &(x, y) in tiles {
        if (x as u32) < cols && (y as u32) < rows {
            grid.set(x as u32, y as u32);
            any = true;
        }
    }
    if !any {
        return Vec::new();
    }
    coalesce(&grid)
        .into_iter()
        .map(|r| r.to_pixels(fb_width, fb_height))
        .collect()
}

/// Owns the framebuffer and the decode pipelines for one client session.
///
/// See the module doc for the calling convention.
pub struct TileRenderer {
    fb: Framebuffer,
    solid: SolidPipeline,
    palrle: PalRlePipeline,
    rgba: RgbaPipeline,
    /// GPU reconstruction of CDF 5/3 tiles; see
    /// [`TileRenderer::enable_cdf53_gpu`]. `None` until enabled.
    cdf53_gpu: Option<Cdf53Gpu>,
    /// The CPU decoder CDF 5/3 passes fall back to when the GPU one is not
    /// enabled or the frame is too large for its plane store.
    cdf53_cpu: Cdf53TileState,
    /// Set while a flush runs only to put older work under an H.264 frame:
    /// the grid then keeps its pending tiles instead of spending its budget on
    /// pictures that are about to be covered. See [`TileRenderer::hold_cdf53`].
    cdf53_hold: bool,

    /// Every pending stateless entry carries the `frame_seq` it came from
    /// (first field); `batch_seq` holds the newest per tile. See
    /// [`crate::paint_order`].
    pending_solid: Vec<(u32, u8, u8, [u8; 4])>,
    pending_palrle: Vec<(u32, u8, u8, u8, u8, Vec<u8>)>,
    /// `TileReady` tiles awaiting upload, batched by `flush`.
    pending_rgba: Vec<(u32, u8, u8, RgbaSrc)>,
    /// Not cleared by a flush: it orders paints across flushes too.
    batch_seq: SeqLedger,
    /// Staging for `TileReady` pixel uploads; see [`UploadStaging`].
    upload_staging: UploadStaging,

    /// Tiles marked dirty since the last drain or flush.
    dirty_tiles: Vec<(u8, u8)>,
    /// Everything is dirty (a resize, [`TileRenderer::mark_all_dirty`], or an
    /// overflowing list).
    dirty_all: bool,
    flushes: u64,
    last_flush_us: u64,

    /// GPU-detected decode failures not yet drained by
    /// [`TileRenderer::take_decode_errors`].
    #[cfg(not(target_arch = "wasm32"))]
    decode_errors: Vec<GpuDecodeError>,
    /// Whether the flush reads the PalRle error slots back. Off by default:
    /// the readback is a blocking `poll(wait_indefinitely)` per flush.
    #[cfg(not(target_arch = "wasm32"))]
    collect_decode_errors: bool,
    /// The tiles of the last flush's PalRle batch, to attribute error slots.
    #[cfg(not(target_arch = "wasm32"))]
    palrle_tiles: Vec<(u8, u8)>,
}
```

`push_dirty`, the capped dirty list.

Write `/tmp/pr1-parts/tr_free.rs`:

```rust
/// Add a tile to the dirty list, or fold the list into "all" when it would
/// overflow [`MAX_DIRTY_LIST`].
fn push_dirty(list: &mut Vec<(u8, u8)>, all: &mut bool, tile_x: u8, tile_y: u8) {
    if *all {
        return;
    }
    if list.len() >= MAX_DIRTY_LIST {
        list.clear();
        *all = true;
    } else {
        list.push((tile_x, tile_y));
    }
}
```

`new`, `apply_event`, `resize`, `reset_session`, the H.264 ledger methods, dirty draining, slot tiles, diagnostics, the CDF 5/3 methods (enable, self-test, resize, pass, record, stats) and the native-only decode-error accessors.

Write `/tmp/pr1-parts/tr_impl1.rs`:

```rust
impl TileRenderer {
    /// A renderer with a `width x height` framebuffer.
    ///
    /// `format` is the framebuffer's format and must be
    /// [`wgpu::TextureFormat::Rgba8Unorm`]: the decode shaders write it as an
    /// `rgba8unorm` storage texture. It is not the canvas format; a caller
    /// that presents to a `bgra8unorm` surface blits the framebuffer there.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Result<Self, GpuError> {
        if format != wgpu::TextureFormat::Rgba8Unorm {
            return Err(GpuError::UnsupportedFormat(format));
        }
        if !dimensions_valid(width, height) {
            return Err(GpuError::InvalidDimensions { width, height });
        }
        let fb = Framebuffer::new(device, width, height);
        let mut solid = SolidPipeline::new(device);
        solid.set_canvas_size(device, queue, width, height);
        Ok(TileRenderer {
            fb,
            solid,
            palrle: PalRlePipeline::new(device),
            rgba: RgbaPipeline::new(device),
            cdf53_gpu: None,
            cdf53_cpu: Cdf53TileState::new(),
            cdf53_hold: false,
            pending_solid: Vec::new(),
            pending_palrle: Vec::new(),
            pending_rgba: Vec::new(),
            batch_seq: SeqLedger::default(),
            upload_staging: UploadStaging::default(),
            dirty_tiles: Vec::new(),
            dirty_all: false,
            flushes: 0,
            last_flush_us: 0,
            #[cfg(not(target_arch = "wasm32"))]
            decode_errors: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            collect_decode_errors: false,
            #[cfg(not(target_arch = "wasm32"))]
            palrle_tiles: Vec::new(),
        })
    }

    /// The framebuffer texture. `Rgba8Unorm`, with `TEXTURE_BINDING`,
    /// `COPY_SRC` and `RENDER_ATTACHMENT`; replaced by a resize, so do not hold
    /// it across [`TileRenderer::apply_event`].
    pub fn framebuffer(&self) -> &wgpu::Texture {
        self.fb.texture()
    }

    /// The framebuffer with its view, size and epoch, for a caller's own
    /// passes over it (the native client's export blit and H.264 blit).
    pub fn fb(&self) -> &Framebuffer {
        &self.fb
    }

    /// Apply one `ClientCore` event: batch the tile for the next flush, or
    /// resize.
    ///
    /// Handles `TileReady`, `TilePayload` (Solid, PalRle and CDF 5/3),
    /// `PaletteUpdated` and `FrameDimensions` (which flushes what is pending
    /// against the old size first, with no slot sink: a caller that feeds
    /// [`SlotTile`]s routes `FrameDimensions` itself -- `flush_with` its sink,
    /// then [`TileRenderer::resize`] -- as the native `Renderer` does).
    /// `NeedsH264` is **not** handled: each client
    /// has its own H.264 path and routes that event itself, using
    /// [`TileRenderer::h264_stale`] and [`TileRenderer::note_h264`] so both
    /// apply the same ordering rules. `DecodeError` is logged (`ClientCore`
    /// already reports it) and `Evicted` is terminal, so neither draws.
    ///
    /// Every `Event` variant is matched explicitly -- see the crate doc on
    /// `clippy::wildcard_enum_match_arm` for why a `_ =>` arm is refused here.
    pub fn apply_event(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        ev: Event,
    ) -> Result<(), GpuError> {
        match ev {
            Event::TileReady {
                tile_x,
                tile_y,
                rgba,
                frame_seq,
            } => {
                // Not the path the shaders exist for (that is `TilePayload`),
                // but a valid event this renderer must still handle correctly:
                // the bytes are already final RGBA pixels.
                //
                // Buffered rather than uploaded here, and batched in `flush`.
                // One `write_texture` per tile costs ~178us on a Pinebook Pro
                // regardless of size -- almost entirely CPU-side call overhead
                // -- so a full 1920x1080 repaint spent 363ms just issuing 2040
                // of them. See `encode_pending_rgba`.
                self.batch_seq.note(tile_x, tile_y, frame_seq);
                self.note_stateless(tile_x, tile_y, frame_seq);
                self.pending_rgba
                    .push((frame_seq, tile_x, tile_y, RgbaSrc::Pixels(rgba)));
                self.mark_dirty(tile_x, tile_y);
            }
            Event::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                generation,
                data,
            } => match data {
                TileData::Solid(bgra) => {
                    self.note_stateless(tile_x, tile_y, frame_seq);
                    self.batch_seq.note(tile_x, tile_y, frame_seq);
                    self.pending_solid.push((frame_seq, tile_x, tile_y, bgra));
                    self.mark_dirty(tile_x, tile_y);
                }
                TileData::PalRle {
                    palette_id,
                    count,
                    indices,
                    // Diagnostics-only; the decoder derives what it needs
                    // from `count` and the indices themselves.
                    wire_flags: _,
                } => {
                    self.note_stateless(tile_x, tile_y, frame_seq);
                    self.batch_seq.note(tile_x, tile_y, frame_seq);
                    self.pending_palrle
                        .push((frame_seq, tile_x, tile_y, palette_id, count, indices));
                    self.mark_dirty(tile_x, tile_y);
                }
                TileData::Cdf53 {
                    pass_idx,
                    bit_planes,
                    present_passes,
                    new_generation,
                } => {
                    self.apply_cdf53_pass(
                        tile_x,
                        tile_y,
                        frame_seq,
                        generation,
                        new_generation,
                        pass_idx,
                        present_passes,
                        bit_planes,
                    );
                }
                // The decode thread expands these (`TileDecoder`); one
                // reaching the renderer bypassed it. Expand here rather than
                // draw nothing.
                TileData::Cdf53Wire {
                    pass_idx,
                    streams,
                    present_passes,
                    new_generation,
                } => {
                    debug_assert!(false, "TileData::Cdf53Wire reached the renderer");
                    let mut planes = vec![0u8; 384];
                    let expanded = <&mut [u8; 384]>::try_from(planes.as_mut_slice())
                        .map(|p| {
                            ghostframe_client_core::cdf53_prevalidate::decode_cdf53_wire(
                                &streams, p,
                            )
                            .is_ok()
                        })
                        .unwrap_or(false);
                    if expanded {
                        self.apply_cdf53_pass(
                            tile_x,
                            tile_y,
                            frame_seq,
                            generation,
                            new_generation,
                            pass_idx,
                            present_passes,
                            planes,
                        );
                    }
                }
            },
            Event::FrameDimensions { width, height } => {
                // Ignore a size the renderer would not have been built with:
                // keep the current framebuffer rather than allocate from a
                // network-supplied number.
                if !dimensions_valid(width, height) {
                    tracing::warn!(width, height, "ignoring out-of-range FrameDimensions");
                    return Ok(());
                }
                // Run any batched work against the OLD framebuffer size
                // before resizing -- the pending tiles were addressed
                // against it. `NoSlots` cannot draw a slot tile, so a pending
                // one means the caller should have routed this event itself.
                debug_assert!(
                    !self
                        .pending_rgba
                        .iter()
                        .any(|(_, _, _, src)| matches!(src, RgbaSrc::Slot(_))),
                    "FrameDimensions with slot tiles pending: flush_with the slot sink, then resize"
                );
                self.flush_with(device, queue, self.last_flush_us, &mut NoSlots)?;
                self.resize(device, queue, width, height);
            }
            Event::NeedsH264 { .. } => {
                // Not this renderer's: see the method doc.
            }
            Event::DecodeError {
                codec,
                tile_x,
                tile_y,
                code,
            } => {
                // `ClientCore` already reports this over the feedback
                // stream (`decode_error_batcher`); the renderer has no
                // separate telemetry sink, so this is a deliberate no-op
                // rather than a silent drop -- logged so a real decode
                // failure is still visible locally.
                log_decode_error(codec, tile_x, tile_y, code);
            }
            Event::PaletteUpdated { palette_id, colors } => {
                // `colors` reports only `count` entries (see the type doc
                // on `Event::PaletteUpdated`); pad the remainder of the
                // 16-slot atlas row with zeros. Safe: `palrle_decode.wgsl`
                // rejects any index >= count as ERR_INDEX_OOB before it
                // would ever read a padding slot.
                let mut full = [[0u8; 4]; 16];
                for (slot, c) in full.iter_mut().zip(colors.iter()) {
                    *slot = *c;
                }
                self.palrle.upload_palette(queue, palette_id, &full);
            }
            Event::Evicted { .. } => {
                // Terminal: the session is going away and the net thread
                // separately surfaces this to the host. Nothing left to paint.
            }
        }
        Ok(())
    }

    /// Resize the framebuffer to `width x height` (a size that passed
    /// [`dimensions_valid`]), keeping what fits at the origin.
    ///
    /// **Flush first.** Pending tiles were addressed against the old size;
    /// [`TileRenderer::apply_event`] does that for `FrameDimensions`, and a
    /// caller that resizes directly (the native client, whose flush also
    /// publishes) must too. Marks everything dirty.
    pub fn resize(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
    ) {
        let epoch = self.fb.epoch();
        self.fb.resize(device, queue, width, height);
        if self.fb.epoch() != epoch {
            // A new picture: what was painted on the old one says nothing
            // about this one.
            self.batch_seq.clear();
            // The cached bind groups hold the old texture; drop them now
            // rather than when each codec next draws, or the old framebuffer
            // outlives the resize.
            self.rgba.drop_framebuffer_cache();
            self.palrle.drop_framebuffer_cache();
            if let Some(g) = self.cdf53_gpu.as_mut() {
                g.grid.drop_framebuffer_cache();
            }
            self.dirty_all = true;
        }
        self.solid.set_canvas_size(device, queue, width, height);
        self.resize_cdf53(device, width, height);
        // The CPU accumulator keys its tiles by position, so a smaller grid
        // drops the tiles outside it; planes of a tile that stays belong to
        // the same picture.
        let (cols, rows) = tile_grid(width, height);
        self.cdf53_cpu
            .retain_tiles(|x, y| (x as u32) < cols && (y as u32) < rows);
    }

    /// Forget everything about the session's picture: pending draws, the
    /// paint-order ledger (the H.264 floor included), the PalRLE palette atlas
    /// (zeroed, as the TypeScript renderer's `onSessionReset` did) and the
    /// CDF 5/3 state on both sides. The framebuffer's pixels stay; the next
    /// full refresh from the server overwrites them. Marks everything dirty.
    ///
    /// Slot tiles still pending are dropped without being retired: a caller
    /// that uses slots rebuilds the renderer instead (the native client does,
    /// per session). `device` is not used today; it is in the signature so a
    /// reset that has to allocate does not change the call.
    pub fn reset_session(
        &mut self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<(), GpuError> {
        self.palrle.clear_palettes(queue);
        self.batch_seq.clear();
        self.pending_solid.clear();
        self.pending_palrle.clear();
        self.pending_rgba.clear();
        self.cdf53_cpu.reset();
        if let Some(g) = self.cdf53_gpu.as_mut() {
            // A new shape forgets every tile; the same shape would keep them.
            let (cols, rows) = tile_grid(self.fb.width, self.fb.height);
            g.state.resize(0, 0);
            if g.grid.is_usable() {
                g.state.resize(cols, rows);
            }
        }
        self.dirty_tiles.clear();
        self.dirty_all = true;
        Ok(())
    }

    /// Whether an H.264 picture from `seq` is older than something already
    /// painted: a newer picture, or a tile from a later frame. Such a picture
    /// covers the whole screen, so drawing it would undo the newer paint; the
    /// caller drops it.
    pub fn h264_stale(&self, seq: u32) -> bool {
        self.batch_seq.h264_stale(seq)
    }

    /// An H.264 picture from `seq` is now over the whole framebuffer: record
    /// it in the paint-order ledger, cancel the CDF 5/3 draws it is newer than
    /// and refuse older passes. It does not touch the damage: a caller that
    /// reports damage marks it itself with [`TileRenderer::mark_all_dirty`]
    /// (the native client's export ring already does).
    pub fn note_h264(&mut self, seq: u32) {
        self.batch_seq.note_h264(seq);
        self.supersede_cdf53(seq);
    }

    /// An H.264 frame from `frame_seq` was blitted over the whole framebuffer:
    /// cancel CDF 5/3 draws still waiting, and refuse older passes.
    pub fn supersede_cdf53(&mut self, frame_seq: u32) {
        if let Some(g) = self.cdf53_gpu.as_mut() {
            g.state.supersede_all(frame_seq);
        }
    }

    /// While `hold` is set, a flush keeps the CDF 5/3 tiles pending instead of
    /// drawing them. For the flush that puts older Solid / PalRle / RGBA work
    /// under an H.264 frame: the frame covers the screen, so drawing the CDF 5/3
    /// tiles first would only spend a flush's GPU budget, and if the frame is
    /// then blitted they are cancelled ([`TileRenderer::note_h264`]) where
    /// drawing them later would put them on top of it.
    pub fn hold_cdf53(&mut self, hold: bool) {
        self.cdf53_hold = hold;
    }

    /// Mark the whole surface dirty, so the next report carries all of it.
    pub fn mark_all_dirty(&mut self) {
        self.dirty_all = true;
    }

    fn mark_dirty(&mut self, tile_x: u8, tile_y: u8) {
        push_dirty(&mut self.dirty_tiles, &mut self.dirty_all, tile_x, tile_y);
    }

    /// Hand every tile marked dirty since the last drain (or flush) to `f`,
    /// and forget them. Returns whether *everything* is dirty, in which case
    /// `f` was not called. A caller that tracks damage itself (the native
    /// client's export ring) drains after each call that can mark; one that
    /// does not reads [`FlushReport::damage`].
    pub fn drain_dirty(&mut self, mut f: impl FnMut(u8, u8)) -> bool {
        for (x, y) in self.dirty_tiles.drain(..) {
            f(x, y);
        }
        std::mem::take(&mut self.dirty_all)
    }

    /// Run `f` over every slot tile still waiting for a flush. For a caller
    /// that is dropped with slots outstanding and must hand them back.
    pub fn for_each_pending_slot(&self, mut f: impl FnMut(&SlotTile)) {
        for (_, _, _, src) in &self.pending_rgba {
            if let RgbaSrc::Slot(slot) = src {
                f(slot);
            }
        }
    }

    /// The slot counterpart of an `Event::TileReady`: the tile's pixels are
    /// already in a slab, so only its place in the paint order is recorded.
    /// Behaves exactly as `TileReady` does for ordering, superseding and
    /// duplicates (the later of two for one tile wins, whichever kind). The
    /// flush hands the slot to its [`SlotSink`].
    pub fn apply_slot_tile(&mut self, frame_seq: u32, tile_x: u8, tile_y: u8, slot: SlotTile) {
        self.batch_seq.note(tile_x, tile_y, frame_seq);
        self.note_stateless(tile_x, tile_y, frame_seq);
        self.pending_rgba
            .push((frame_seq, tile_x, tile_y, RgbaSrc::Slot(slot)));
        self.mark_dirty(tile_x, tile_y);
    }

    /// The newest `frame_seq` noted for a tile and the CDF 5/3 grid's state
    /// for it, for diagnostics; `None` for a tile outside the framebuffer's
    /// grid.
    pub fn debug_tile(&self, tile_x: u32, tile_y: u32) -> Option<DebugTile> {
        let (cols, rows) = tile_grid(self.fb.width, self.fb.height);
        if tile_x >= cols || tile_y >= rows {
            return None;
        }
        let (x, y) = (u8::try_from(tile_x).ok()?, u8::try_from(tile_y).ok()?);
        Some(DebugTile {
            tile_x,
            tile_y,
            newest_frame_seq: self.batch_seq.newest_for(x, y),
            cdf53: self
                .cdf53_gpu
                .as_ref()
                .and_then(|g| g.state.debug_tile(x, y)),
        })
    }

    /// Whether the GPU CDF 5/3 grid is enabled and the current frame fits it
    /// (otherwise passes are decoded on the CPU).
    pub fn cdf53_gpu_enabled(&self) -> bool {
        self.cdf53_gpu.as_ref().is_some_and(|g| g.grid.is_usable())
    }

    /// Counters and sizes, for diagnostics.
    pub fn debug_state(&self) -> DebugState {
        let (cols, rows) = tile_grid(self.fb.width, self.fb.height);
        DebugState {
            width: self.fb.width,
            height: self.fb.height,
            cols,
            rows,
            flushes: self.flushes,
            last_flush_us: self.last_flush_us,
            pending_solid: self.pending_solid.len(),
            pending_palrle: self.pending_palrle.len(),
            pending_rgba: self.pending_rgba.len(),
            cdf53_gpu: self.cdf53_gpu_enabled(),
            cdf53_pending: self.has_deferred_work(),
            newest_tile_seq: self.batch_seq.max_tile(),
            newest_h264_seq: self.batch_seq.h264(),
        }
    }

    /// Turn on GPU reconstruction of CDF 5/3 tiles. Allocates the grid's plane
    /// store for the current frame size. The caller is expected to have proven
    /// the shaders on this device first
    /// ([`TileRenderer::try_enable_cdf53_gpu`] does both): this does not.
    ///
    /// A frame whose plane store the device cannot bind (about 8K) is decoded
    /// on the CPU by this renderer instead, and the GPU is used again as soon
    /// as a frame fits.
    ///
    /// Builds its own grid with the cost model of the Mali-T860 it was fitted
    /// on (and the 1000-tile submit cap); the native client instead hands over
    /// the grid it probed, with [`TileRenderer::enable_cdf53_gpu_with`].
    pub fn enable_cdf53_gpu(&mut self, device: &wgpu::Device) {
        if self.cdf53_gpu.is_none() {
            self.install_cdf53(device, Cdf53Grid::new(device), None, MAX_TILES_PER_FLUSH);
        }
    }

    /// [`TileRenderer::enable_cdf53_gpu`] with a grid that already passed its
    /// self-test on this device, so the shaders are compiled once, and with the
    /// per-flush budget calibrated from what the probe measured
    /// (`us_per_tile_k14`; `None` keeps the default cost model). `max_tiles`
    /// caps one submit (the ~500 ms job watchdog of panfrost/GL is the reason;
    /// Vulkan can take far more).
    pub fn enable_cdf53_gpu_with(
        &mut self,
        device: &wgpu::Device,
        grid: Cdf53Grid,
        us_per_tile_k14: Option<f64>,
        max_tiles: usize,
    ) {
        if self.cdf53_gpu.is_none() {
            self.install_cdf53(device, grid, us_per_tile_k14, max_tiles);
        }
    }

    /// Run the CDF 5/3 grid's start-up self-test on this device and, only if
    /// it passes, enable the GPU grid. The reconstruct shader is sensitive to
    /// register allocation on panfrost: a build that tips over it silently
    /// writes nothing, so the grid is not trusted until two known tiles come
    /// back exact. On `Err` (a shader that does not build, or a wrong picture)
    /// nothing is enabled and CDF 5/3 passes keep being decoded on the CPU
    /// (`Cdf53TileState`); the caller logs the message.
    ///
    /// Async because the check reads pixels back; a native caller drives it
    /// with [`crate::readback::block_on_device`].
    pub async fn try_enable_cdf53_gpu(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<(), String> {
        if self.cdf53_gpu.is_some() {
            return Ok(());
        }
        let grid = probe_grid(device, queue, Cdf53Grid::new).await?;
        self.install_cdf53(device, grid, None, MAX_TILES_PER_FLUSH);
        Ok(())
    }

    fn install_cdf53(
        &mut self,
        device: &wgpu::Device,
        grid: Cdf53Grid,
        us_per_tile_k14: Option<f64>,
        max_tiles: usize,
    ) {
        let mut state = Cdf53GridState::with_budget(FLUSH_BUDGET_US, max_tiles);
        if let Some(us) = us_per_tile_k14 {
            state.set_cost_per_tile_k14(us);
        }
        self.cdf53_gpu = Some(Cdf53Gpu {
            grid,
            state,
            frames: 0,
            cpu_fallback_logged: false,
        });
        let (w, h) = (self.fb.width, self.fb.height);
        self.resize_cdf53(device, w, h);
    }

    /// Pretend the device's storage binding limit is `bytes`, from the next
    /// resize on. For tests of the too-large-frame fallback.
    #[doc(hidden)]
    pub fn debug_set_cdf53_limit(&mut self, bytes: Option<u64>) {
        if let Some(g) = self.cdf53_gpu.as_mut() {
            g.grid.set_limit_for_test(bytes);
        }
    }

    /// Size the CDF 5/3 grid and its host state for a `width` x `height` frame.
    fn resize_cdf53(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        let Some(g) = self.cdf53_gpu.as_mut() else {
            return;
        };
        let (cols, rows) = tile_grid(width, height);
        let was_gpu = g.grid.is_usable();
        let result = g.grid.resize(device, cols, rows);
        if result.is_ok() != was_gpu {
            // Switching sides. Whatever the side we leave had accumulated --
            // planes, or a generation started there -- is of the old frame
            // size and must not be completed by the other side: the server's
            // full refresh after a resize re-sends every tile as a new
            // generation, so starting both sides clean loses nothing.
            self.cdf53_cpu.reset();
        }
        let g = self.cdf53_gpu.as_mut().expect("just used");
        match result {
            Ok(()) => {
                g.state.resize(cols, rows);
                g.cpu_fallback_logged = false;
            }
            Err(e) => {
                g.state.resize(0, 0);
                if !g.cpu_fallback_logged {
                    g.cpu_fallback_logged = true;
                    tracing::warn!(
                        error = %e, width, height,
                        "CDF 5/3 GPU decode unavailable for this frame size; \
                         decoding on the render thread's CPU"
                    );
                }
            }
        }
    }

    /// The GPU CDF 5/3 state, when enabled and the frame fits.
    fn cdf53_gpu_mut(&mut self) -> Option<&mut Cdf53Gpu> {
        self.cdf53_gpu.as_mut().filter(|g| g.grid.is_usable())
    }

    /// A Solid / PalRle / RGBA tile for the tile was applied: it may
    /// supersede a CDF 5/3 draw still waiting.
    fn note_stateless(&mut self, tile_x: u8, tile_y: u8, frame_seq: u32) {
        if let Some(g) = self.cdf53_gpu_mut() {
            g.state.stateless(tile_x, tile_y, frame_seq);
        }
    }

    /// One CDF 5/3 pass. With the GPU grid it is only recorded: nothing is
    /// drawn, noted in the paint-order ledger or marked dirty until a flush
    /// lists the tile. Without it the pass is reconstructed here, on the CPU,
    /// into a `TileReady`-style upload.
    #[allow(clippy::too_many_arguments)]
    fn apply_cdf53_pass(
        &mut self,
        tile_x: u8,
        tile_y: u8,
        frame_seq: u32,
        generation: u8,
        new_generation: bool,
        pass_idx: u8,
        present_passes: Option<u16>,
        bit_planes: Vec<u8>,
    ) {
        if self.cdf53_gpu_mut().is_some() {
            // Waiting passes count towards "something later is on its way"
            // for H.264 pictures, though not towards any tile's entry (see
            // `crate::paint_order`).
            self.batch_seq.note_pending(frame_seq);
        }
        if let Some(g) = self.cdf53_gpu_mut() {
            g.state.pass(
                tile_x,
                tile_y,
                frame_seq,
                generation,
                new_generation,
                pass_idx,
                present_passes,
                bit_planes,
            );
            return;
        }
        if new_generation {
            self.cdf53_cpu.forget_tile(tile_x, tile_y);
        }
        let entry = PrevalidatedCdf53 {
            generation,
            pass_idx,
            bit_planes,
            present_passes,
        };
        self.batch_seq.note(tile_x, tile_y, frame_seq);
        if let Some(rgba) = self.cdf53_cpu.integrate(tile_x, tile_y, &entry) {
            self.pending_rgba
                .push((frame_seq, tile_x, tile_y, RgbaSrc::Pixels(rgba)));
            self.mark_dirty(tile_x, tile_y);
        }
    }

    /// Plan this flush's CDF 5/3 tiles and record the grid. See
    /// [`Cdf53GridState::plan`] for what gets listed.
    fn record_cdf53(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
    ) -> bool {
        if self.cdf53_hold {
            return false;
        }
        let Some(g) = self.cdf53_gpu.as_mut().filter(|g| g.grid.is_usable()) else {
            return false;
        };
        let listed = g.state.plan();
        if listed.is_empty() {
            return false;
        }
        g.grid.begin_frame();
        for t in &listed {
            for (pass, plane) in &t.planes {
                g.grid.push_plane(t.index, *pass, plane);
            }
            g.grid.push_tile(t.index, t.mask, t.k);
            self.batch_seq.note(t.tile_x, t.tile_y, t.frame_seq);
            // Not `self.mark_dirty`: `g` still borrows `self.cdf53_gpu`.
            push_dirty(
                &mut self.dirty_tiles,
                &mut self.dirty_all,
                t.tile_x,
                t.tile_y,
            );
        }
        g.frames += 1;
        g.grid.record(device, queue, encoder, &self.fb)
    }

    /// Whether CDF 5/3 tiles are waiting for a later flush (the GPU budget was
    /// spent, or a partial tile is being throttled). The caller should flush
    /// again soon even if no event arrives.
    pub fn has_deferred_work(&self) -> bool {
        self.cdf53_gpu
            .as_ref()
            .is_some_and(|g| g.grid.is_usable() && g.state.has_pending())
    }

    /// CDF 5/3 scheduling counters since the last call, and the number of
    /// grid frames recorded; `None` when the GPU path is not enabled.
    pub fn take_cdf53_stats(&mut self) -> Option<(Cdf53Stats, u64)> {
        let g = self.cdf53_gpu.as_mut()?;
        let frames = std::mem::take(&mut g.frames);
        Some((g.state.take_stats(), frames))
    }

    /// Enable (or disable) reading the PalRle error slots back after each
    /// flush. Native only: the readback blocks on the device.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_collect_decode_errors(&mut self, on: bool) {
        self.collect_decode_errors = on;
    }

    /// Take the GPU-detected tile decode failures accumulated by flushes.
    ///
    /// The shader validates PalRle payloads on the device (an index past the
    /// palette count, say), so only the readback after the decode can tell.
    /// The caller is expected to relay these as DECODE_ERROR feedback.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn take_decode_errors(&mut self) -> Vec<GpuDecodeError> {
        std::mem::take(&mut self.decode_errors)
    }
```

`flush`, `flush_with`, `record` (everything between creating the encoder and submitting it) and `finish_flush` (the opt-in PalRle readback, native only).

Write `/tmp/pr1-parts/tr_impl2.rs`:

```rust
    /// Submit all batched work, in one submit, and report what changed.
    ///
    /// `now_us` is the caller's clock in microseconds (any epoch): the
    /// renderer reads no clock of its own. It is recorded in the report and
    /// in [`DebugState::last_flush_us`].
    ///
    /// Every codec records into one command encoder, in the paint order
    /// below, and the encoder is submitted once. Each submit costs a
    /// `glFlush` and a fence on GLES, and on panfrost a flush ends the job
    /// batch -- a separate kernel submission -- so one per codec per frame
    /// was most of this function's cost on a Pinebook Pro.
    ///
    /// **Why the order survives.** Buffer and texture contents reach the GPU
    /// two ways, and they do not interleave. `queue.write_buffer` /
    /// `write_buffer_with` / `write_texture` are queue operations that wgpu
    /// runs ahead of *all* the commands of the next submit; commands in an
    /// encoder run in the order they were recorded. So (1) every codec's
    /// inputs are in place before any codec's pass runs, which is harmless
    /// because the codecs use disjoint buffers, and (2) nothing that must be
    /// ordered against a pass may go through the queue: the `TileReady`
    /// rectangles are copied by commands (see [`UploadStaging`]) for exactly
    /// that reason. (3) A buffer written twice before one submit keeps only
    /// the second write, so each pipeline's `encode` is called once per
    /// encoder, and the staging ranges never overlap.
    ///
    /// Fails only if the opt-in PalRle error readback cannot wait on the
    /// device (e.g. it was lost). That runs after the submit, so the frame's
    /// work is already on the GPU; the errors it would have reported are lost.
    pub fn flush(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        now_us: u64,
    ) -> Result<FlushReport, GpuError> {
        self.flush_with(device, queue, now_us, &mut NoSlots)
    }

    /// [`TileRenderer::flush`] for a caller that feeds slot tiles.
    pub fn flush_with(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        now_us: u64,
        slots: &mut dyn SlotSink,
    ) -> Result<FlushReport, GpuError> {
        self.last_flush_us = now_us;
        let submits_before = crate::submit::count();
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-flush"),
        });
        let recorded = self.record(device, queue, &mut encoder, slots);
        if recorded {
            crate::submit::submit(queue, encoder);
        }
        let submits = crate::submit::count().wrapping_sub(submits_before) as u32;
        self.finish_flush(device, queue)?;
        let all = std::mem::take(&mut self.dirty_all);
        let tiles = std::mem::take(&mut self.dirty_tiles);
        let damage = damage_rects(all, &tiles, self.fb.width, self.fb.height);
        Ok(FlushReport {
            recorded,
            submits,
            damage,
            deferred_work: self.has_deferred_work(),
            at_us: now_us,
        })
    }

    /// Record every pending draw into `encoder`, in paint order, without
    /// submitting it. Returns whether anything was recorded.
    ///
    /// For a caller that adds its own commands to the same encoder (the native
    /// client's export blit) so the flush stays one submit. Pair it with
    /// submitting the encoder and then [`TileRenderer::finish_flush`].
    pub fn record(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        slots: &mut dyn SlotSink,
    ) -> bool {
        // Order within a batch is by `frame_seq`, not by codec (see
        // `crate::paint_order`). The CDF 5/3 grid goes first so that a newer
        // stateless entry lands on top of an older pass; a stateless entry
        // older than anything else for its tile is dropped, so a newer pass
        // is not covered either. Only tiles the grid is *drawing* this flush
        // count towards a tile's newest `frame_seq`: a pass that is held back
        // must not make an older Solid disappear.
        let mut recorded = self.record_cdf53(device, queue, encoder);

        let ledger = &self.batch_seq;
        let solid: Vec<(u8, u8, [u8; 4])> = std::mem::take(&mut self.pending_solid)
            .into_iter()
            .filter(|&(seq, tx, ty, _)| !ledger.superseded(tx, ty, seq))
            .map(|(_, tx, ty, c)| (tx, ty, c))
            .collect();
        let palrle_batch: Vec<(u8, u8, u8, u8, Vec<u8>)> = std::mem::take(&mut self.pending_palrle)
            .into_iter()
            .filter(|&(seq, tx, ty, ..)| !ledger.superseded(tx, ty, seq))
            .map(|(_, tx, ty, id, n, idx)| (tx, ty, id, n, idx))
            .collect();
        // Slots of tiles dropped here never reach the GPU; they are still
        // issued, so the sink retires them with the rest of the flush's.
        self.pending_rgba.retain(|(seq, tx, ty, src)| {
            let keep = !ledger.superseded(*tx, *ty, *seq);
            if let (false, RgbaSrc::Slot(slot)) = (keep, src) {
                slots.retire(slot);
            }
            keep
        });

        recorded |= self.solid.encode(device, queue, encoder, &self.fb, &solid);
        recorded |= self
            .palrle
            .encode(device, queue, encoder, &self.fb, &palrle_batch);
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.palrle_tiles = palrle_batch.iter().map(|&(tx, ty, ..)| (tx, ty)).collect();
        }
        recorded |= self.encode_pending_rgba(device, queue, encoder, slots);
        recorded
    }

    /// What follows a submit of [`TileRenderer::record`]'s encoder: the opt-in
    /// PalRle error readback (native only), and the flush count.
    /// [`TileRenderer::flush`] does it for you.
    pub fn finish_flush(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<(), GpuError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let tiles = std::mem::take(&mut self.palrle_tiles);
            if !tiles.is_empty() && self.collect_decode_errors {
                let errors = self.palrle.take_errors(device, queue)?;
                for (i, code) in errors {
                    let Some(&(tile_x, tile_y)) = tiles.get(i as usize) else {
                        continue;
                    };
                    let Some(code) = decode_error_code(code) else {
                        tracing::warn!(code, tile_x, tile_y, "unknown GPU palrle error code");
                        continue;
                    };
                    // Bounded: a hostile stream of bad tiles must not grow
                    // this without limit between two drains.
                    if self.decode_errors.len() < MAX_PENDING_DECODE_ERRORS {
                        self.decode_errors.push(GpuDecodeError {
                            codec: Codec::PalRle,
                            tile_x,
                            tile_y,
                            code,
                        });
                    }
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        let _ = (device, queue);
        self.flushes = self.flushes.saturating_add(1);
        Ok(())
    }
```

The unit tests (15). Each says what it proves; Step 7 proves they can fail.

Write `/tmp/pr1-parts/tr_tests.rs`:

```rust

#[cfg(test)]
mod tests {
    use super::*;
    use crate::noop_device::device;
    use crate::pipelines::cdf53_grid::self_test_tiles;

    const RGBA: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

    fn renderer(w: u32, h: u32) -> (TileRenderer, wgpu::Device, wgpu::Queue) {
        let (d, q) = device();
        let r = TileRenderer::new(&d, &q, RGBA, w, h).expect("renderer");
        (r, d, q)
    }

    fn solid(seq: u32, x: u8, y: u8) -> Event {
        Event::TilePayload {
            frame_seq: seq,
            tile_x: x,
            tile_y: y,
            generation: 0,
            data: TileData::Solid([1, 2, 3, 255]),
        }
    }

    fn pass(seq: u32, x: u8, y: u8, idx: u8, present: Option<u16>, tag: u8) -> Event {
        Event::TilePayload {
            frame_seq: seq,
            tile_x: x,
            tile_y: y,
            generation: 0,
            data: TileData::Cdf53 {
                pass_idx: idx,
                bit_planes: vec![tag; 384],
                present_passes: present,
                new_generation: false,
            },
        }
    }

    #[test]
    fn new_refuses_a_format_the_decode_shaders_cannot_write() {
        let (d, q) = device();
        for format in [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ] {
            match TileRenderer::new(&d, &q, format, 64, 64) {
                Err(GpuError::UnsupportedFormat(f)) => assert_eq!(f, format),
                other => panic!("{format:?}: expected UnsupportedFormat, got {:?}", other.err()),
            }
        }
    }

    #[test]
    fn new_refuses_sizes_outside_the_wire_bound() {
        let (d, q) = device();
        let too_big = ghostframe_client_core::MAX_FRAME_DIMENSION + 1;
        for (w, h) in [(0, 64), (64, 0), (too_big, 64), (64, too_big)] {
            assert!(
                matches!(
                    TileRenderer::new(&d, &q, RGBA, w, h),
                    Err(GpuError::InvalidDimensions { .. })
                ),
                "{w}x{h}"
            );
        }
    }

    #[test]
    fn a_tile_flushes_in_one_submit_and_its_damage_is_reported_once() {
        let (mut r, d, q) = renderer(100, 70);
        r.apply_event(&d, &q, solid(1, 1, 2)).unwrap();
        let report = r.flush(&d, &q, 1_000).unwrap();
        assert!(report.recorded);
        assert_eq!(report.submits, 1, "a flush is one submit");
        assert_eq!(report.at_us, 1_000);
        // Tile (1, 2) of a 100x70 frame: x = 32, y = 64, clamped to 6 rows.
        assert_eq!(
            report.damage,
            vec![Rect {
                x: 32,
                y: 64,
                w: 32,
                h: 6
            }]
        );
        assert!(!report.deferred_work);

        let again = r.flush(&d, &q, 2_000).unwrap();
        assert!(!again.recorded);
        assert_eq!(again.submits, 0, "nothing to draw, nothing to submit");
        assert!(again.damage.is_empty(), "damage is not reported twice");
        let state = r.debug_state();
        assert_eq!((state.flushes, state.last_flush_us), (2, 2_000));
    }

    #[test]
    fn the_h264_ledger_orders_pictures_against_pictures_and_tiles() {
        let (mut r, d, q) = renderer(64, 64);
        assert!(!r.h264_stale(1), "nothing painted yet");
        r.note_h264(100);
        assert!(r.h264_stale(99), "older than the picture on screen");
        assert!(!r.h264_stale(100), "equal never supersedes");
        assert!(!r.h264_stale(101));
        r.apply_event(&d, &q, solid(200, 0, 0)).unwrap();
        assert!(r.h264_stale(150), "a tile from a later frame is on screen");
        assert!(!r.h264_stale(200));
        assert_eq!(r.debug_state().newest_h264_seq, Some(100));
    }

    #[test]
    fn a_resize_forgets_the_picture_and_damages_all_of_it() {
        let (mut r, d, q) = renderer(64, 64);
        r.apply_event(&d, &q, solid(5, 0, 0)).unwrap();
        r.flush(&d, &q, 1).unwrap();
        assert_eq!(r.debug_tile(0, 0).unwrap().newest_frame_seq, Some(5));

        // The same size again (the server repeats FrameDimensions): nothing
        // about the picture changes.
        r.apply_event(
            &d,
            &q,
            Event::FrameDimensions {
                width: 64,
                height: 64,
            },
        )
        .unwrap();
        assert_eq!(r.debug_tile(0, 0).unwrap().newest_frame_seq, Some(5));

        r.apply_event(
            &d,
            &q,
            Event::FrameDimensions {
                width: 128,
                height: 96,
            },
        )
        .unwrap();
        assert_eq!(r.debug_tile(0, 0).unwrap().newest_frame_seq, None);
        assert_eq!(
            (r.framebuffer().width(), r.framebuffer().height()),
            (128, 96)
        );
        let report = r.flush(&d, &q, 2).unwrap();
        assert_eq!(
            report.damage,
            vec![Rect {
                x: 0,
                y: 0,
                w: 128,
                h: 96
            }]
        );
    }

    #[test]
    fn an_out_of_range_resize_is_ignored() {
        let (mut r, d, q) = renderer(64, 64);
        r.apply_event(
            &d,
            &q,
            Event::FrameDimensions {
                width: 0,
                height: 64,
            },
        )
        .unwrap();
        assert_eq!(r.framebuffer().width(), 64);
    }

    #[test]
    fn the_cdf53_grid_holds_passes_until_a_flush_lists_them() {
        let (mut r, d, q) = renderer(64, 64);
        r.enable_cdf53_gpu(&d);
        // Pass 0 carries the bitmap: passes 0 and 2 exist, so two make the tile.
        r.apply_event(&d, &q, pass(7, 0, 0, 0, Some(0b101), 1))
            .unwrap();
        r.apply_event(&d, &q, pass(7, 0, 0, 2, None, 3)).unwrap();
        assert!(r.has_deferred_work(), "the grid is waiting for a flush");
        let tile = r
            .debug_tile(0, 0)
            .unwrap()
            .cdf53
            .expect("grid state for the tile");
        assert_eq!(tile.received_mask, 0b101);
        assert_eq!(tile.drawn_k, None);
        assert_eq!(
            r.debug_tile(0, 0).unwrap().newest_frame_seq,
            None,
            "a waiting pass is not a paint"
        );

        let report = r.flush(&d, &q, 1).unwrap();
        assert!(report.recorded, "the grid drew the tile");
        assert!(!report.deferred_work && !r.has_deferred_work());
        assert_eq!(
            report.damage,
            vec![Rect {
                x: 0,
                y: 0,
                w: 32,
                h: 32
            }]
        );
        assert_eq!(r.debug_tile(0, 0).unwrap().newest_frame_seq, Some(7));
        let (stats, frames) = r.take_cdf53_stats().expect("grid enabled");
        assert_eq!((stats.listed, frames), (1, 1));
    }

    #[test]
    fn without_the_grid_cdf53_passes_decode_on_the_cpu() {
        let (mut r, d, q) = renderer(64, 64);
        let tile = &self_test_tiles()[0];
        for (p, plane) in tile.planes.iter().enumerate() {
            let Some(plane) = plane else { continue };
            let ev = Event::TilePayload {
                frame_seq: 3,
                tile_x: 1,
                tile_y: 0,
                generation: 0,
                data: TileData::Cdf53 {
                    pass_idx: p as u8,
                    bit_planes: plane.clone(),
                    present_passes: (p == 0).then_some(tile.present),
                    new_generation: false,
                },
            };
            r.apply_event(&d, &q, ev).unwrap();
        }
        let state = r.debug_state();
        assert!(!state.cdf53_gpu);
        assert!(state.pending_rgba >= 1, "the CPU decode produced an upload");
        let report = r.flush(&d, &q, 1).unwrap();
        assert!(report.recorded);
        assert_eq!(
            report.damage,
            vec![Rect {
                x: 32,
                y: 0,
                w: 32,
                h: 32
            }]
        );
    }

    #[test]
    fn the_self_test_keeps_a_grid_that_draws_nothing_off() {
        // The no-op backend runs no shader, so the reconstruct draws nothing
        // and the two known tiles do not come back: the gate must refuse.
        let (mut r, d, q) = renderer(64, 64);
        let err = crate::readback::block_on_device(&d, r.try_enable_cdf53_gpu(&d, &q))
            .expect_err("a device that draws nothing must fail the self-test");
        assert!(err.contains("frame 1"), "{err}");
        assert!(!r.debug_state().cdf53_gpu, "the grid stays off");
        // And the CPU path is still there.
        r.apply_event(&d, &q, pass(1, 0, 0, 0, Some(1), 1)).unwrap();
        assert!(!r.has_deferred_work());
    }

    #[test]
    fn reset_session_forgets_pending_work_the_ledger_and_the_h264_floor() {
        let (mut r, d, q) = renderer(64, 64);
        r.apply_event(&d, &q, solid(9, 0, 0)).unwrap();
        r.note_h264(50);
        r.reset_session(&d, &q).unwrap();
        assert_eq!(r.debug_state().pending_solid, 0);
        assert_eq!(r.debug_tile(0, 0).unwrap().newest_frame_seq, None);
        assert!(!r.h264_stale(1), "the H.264 floor is gone");
        let report = r.flush(&d, &q, 1).unwrap();
        assert!(!report.recorded, "the dropped tile is not drawn");
        assert_eq!(report.damage.len(), 1, "but the picture is all dirty");
    }

    #[test]
    fn a_caller_that_drains_dirty_tiles_sees_each_once_and_the_flush_none() {
        let (mut r, d, q) = renderer(100, 70);
        r.apply_event(&d, &q, solid(1, 1, 2)).unwrap();
        r.apply_event(&d, &q, solid(2, 0, 0)).unwrap();
        let mut seen = Vec::new();
        assert!(!r.drain_dirty(|x, y| seen.push((x, y))));
        assert_eq!(seen, vec![(1, 2), (0, 0)]);
        assert!(!r.drain_dirty(|_, _| panic!("drained twice")));
        let report = r.flush(&d, &q, 1).unwrap();
        assert!(report.recorded);
        assert!(report.damage.is_empty(), "the caller already took them");
    }

    #[test]
    fn a_dirty_list_that_is_never_drained_folds_into_all() {
        let mut list = Vec::new();
        let mut all = false;
        for i in 0..MAX_DIRTY_LIST {
            push_dirty(&mut list, &mut all, (i % 256) as u8, 0);
        }
        assert!(!all);
        assert_eq!(list.len(), MAX_DIRTY_LIST);
        push_dirty(&mut list, &mut all, 0, 0);
        assert!(all && list.is_empty());
        push_dirty(&mut list, &mut all, 1, 1);
        assert!(list.is_empty(), "once all is set the list stays empty");
    }

    #[test]
    fn damage_rects_clamp_coalesce_and_drop_tiles_off_the_grid() {
        // 100x70 is 4x3 tiles; the right column is 4 px wide, the bottom row 6 tall.
        let full = damage_rects(true, &[], 100, 70);
        assert_eq!(
            full,
            vec![Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 70
            }]
        );
        let edge = damage_rects(false, &[(3, 2)], 100, 70);
        assert_eq!(
            edge,
            vec![Rect {
                x: 96,
                y: 64,
                w: 4,
                h: 6
            }]
        );
        // Two neighbours are one rectangle; (200, 0) is not on the grid.
        let row = damage_rects(false, &[(0, 0), (1, 0), (200, 0)], 100, 70);
        assert_eq!(
            row,
            vec![Rect {
                x: 0,
                y: 0,
                w: 64,
                h: 32
            }]
        );
        assert!(damage_rects(false, &[(200, 0)], 100, 70).is_empty());
    }

    #[test]
    fn the_debug_structs_serialise_for_the_e2e_hooks() {
        let (mut r, d, q) = renderer(64, 64);
        r.apply_event(&d, &q, solid(4, 1, 1)).unwrap();
        let state = serde_json::to_value(r.debug_state()).unwrap();
        assert_eq!(state["width"], 64);
        assert_eq!(state["pendingSolid"], 1);
        assert_eq!(state["newestTileSeq"], 4);
        assert!(state["newestH264Seq"].is_null());
        assert_eq!(state["cdf53Gpu"], false);
        let tile = serde_json::to_value(r.debug_tile(1, 1)).unwrap();
        assert_eq!((tile["tileX"].as_u64(), tile["tileY"].as_u64()), (Some(1), Some(1)));
        assert_eq!(tile["newestFrameSeq"], 4);
        assert!(r.debug_tile(5, 5).is_none(), "outside the 2x2 grid");
    }

    /// A [`SlotSink`] that records what it was asked.
    #[derive(Default)]
    struct Recording {
        retired: Vec<u64>,
        encoded: Vec<(u32, u32, u64)>,
    }

    impl SlotSink for Recording {
        fn retire(&mut self, slot: &SlotTile) {
            self.retired.push(slot.token);
        }

        fn encode(
            &mut self,
            _device: &wgpu::Device,
            _queue: &wgpu::Queue,
            _encoder: &mut wgpu::CommandEncoder,
            _fb: &Framebuffer,
            tiles: &[(u32, u32, SlotTile)],
        ) -> bool {
            self.encoded
                .extend(tiles.iter().map(|&(x, y, s)| (x, y, s.token)));
            !tiles.is_empty()
        }
    }

    fn slot(token: u64) -> SlotTile {
        SlotTile {
            slab: 0,
            index: token as u32,
            token,
        }
    }

    #[test]
    fn slot_tiles_are_retired_exactly_once_and_the_later_one_for_a_tile_wins() {
        let (mut r, d, q) = renderer(64, 64);
        r.apply_slot_tile(3, 0, 0, slot(30));
        r.apply_slot_tile(4, 0, 0, slot(40));
        r.apply_slot_tile(5, 1, 1, slot(50));
        let mut pending = Vec::new();
        r.for_each_pending_slot(|s| pending.push(s.token));
        assert_eq!(pending, vec![30, 40, 50]);

        let mut sink = Recording::default();
        let report = r.flush_with(&d, &q, 1, &mut sink).unwrap();
        assert!(report.recorded);
        sink.retired.sort_unstable();
        assert_eq!(sink.retired, vec![30, 40, 50], "every slot, once");
        sink.encoded.sort_unstable();
        assert_eq!(
            sink.encoded,
            vec![(0, 0, 40), (1, 1, 50)],
            "seq 3 lost to seq 4 for the tile"
        );
    }
}
```

The assembler. It pulls four blocks out of `renderer.rs` by anchor text (it asserts each anchor is unique): the upload-staging constants and type, `GpuDecodeError`, `encode_pending_rgba` (rewritten around `SlotSink` by exact substring replacement) and `encode_staged_copies` (only its device/queue arguments change).

Write `/tmp/pr1-parts/build_tr.py`:

```python
#!/usr/bin/env python3
"""Assemble ghostframe-render/src/tile_renderer.rs from the new parts and the
bodies that move verbatim out of ghostframe-client-gpu/src/renderer.rs.
Run from the repository root: python3 /tmp/pr1-parts/build_tr.py /tmp/pr1-parts"""
import sys, pathlib
parts = pathlib.Path(sys.argv[1])
old = open('ghostframe-client-gpu/src/renderer.rs').read()

def grab(a, b):
    assert old.count(a) == 1, a
    i = old.index(a)
    j = old.index(b, i)
    return old[i:j]

def sub(text, a, b, n=1):
    assert text.count(a) == n, (text.count(a), a)
    return text.replace(a, b)

# 1. consts, UploadStaging, Copy, CopySrc, short_tile_rect
blk1 = grab("/// Upper bound on [`GpuDecodeError`]s held between two drains.",
            "/// A tile the GPU decode rejected")
blk1 = sub(blk1, "const MAX_PENDING_DECODE_ERRORS",
           '#[cfg(not(target_arch = "wasm32"))]\nconst MAX_PENDING_DECODE_ERRORS')

# 2. GpuDecodeError and decode_error_code (native only: only the blocking
#    readback produces them)
blk2 = grab("/// A tile the GPU decode rejected", "/// Which route the most recent H.264")
blk2 = sub(blk2, "pub struct GpuDecodeError",
           '#[cfg(not(target_arch = "wasm32"))]\npub struct GpuDecodeError')
blk2 = sub(blk2, "fn decode_error_code",
           '#[cfg(not(target_arch = "wasm32"))]\nfn decode_error_code')

# 3. encode_pending_rgba, with the slot handling turned into the SlotSink calls
epr = grab("    /// Record the upload of every buffered `TileReady` tile with as few",
           "    /// Record every copy in `copies` into the framebuffer")
epr = sub(epr, """        &mut self,
        ctx: &WgpuContext,
        encoder: &mut wgpu::CommandEncoder,
        retired: &mut [u32],
    ) -> bool {""", """        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        slots: &mut dyn SlotSink,
    ) -> bool {""")
epr = sub(epr, """        for (_, _, _, src) in &pending {
            if let (RgbaSrc::Slot(slot), Some(pool)) = (src, &self.upload_pool) {
                debug_assert!(
                    pool.is_current(*slot),
                    "slot {slot:?} was recycled before it was drawn"
                );
                retired[slot.slab()] += 1;
            }
        }
""", """        for (_, _, _, src) in &pending {
            if let RgbaSrc::Slot(slot) = src {
                slots.retire(slot);
            }
        }
""")
epr = sub(epr, "HashMap<(u32, u32), SlotRef>", "HashMap<(u32, u32), SlotTile>")
epr = sub(epr, """        let mut recorded = self.encode_staged_copies(ctx, encoder, &copies, &by_tile);
        recorded |= self
            .rgba
            .encode(&ctx.device, &ctx.queue, encoder, &self.fb, &scattered);
""", """        let mut recorded = self.encode_staged_copies(device, queue, encoder, &copies, &by_tile);
        recorded |= self
            .rgba
            .encode(device, queue, encoder, &self.fb, &scattered);
""")
i = epr.index("        // The slab tiles: one dispatch per slab used.")
epr = epr[:i] + """        // The slot tiles, drawn by the caller's sink.
        let slot_tiles: Vec<(u32, u32, SlotTile)> =
            by_slot.iter().map(|(&(x, y), &slot)| (x, y, slot)).collect();
        recorded |= slots.encode(device, queue, encoder, &self.fb, &slot_tiles);
        recorded
    }

"""

# 4. encode_staged_copies, moved as is but for the device/queue arguments
esc = grab("    /// Record every copy in `copies` into the framebuffer",
           "    /// Seal a generation and hand out an export buffer")
esc = sub(esc, """        &mut self,
        ctx: &WgpuContext,
        encoder: &mut wgpu::CommandEncoder,
        copies: &[Copy],""", """        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        copies: &[Copy],""")
esc = sub(esc, "self.upload_staging.reserve(&ctx.device, total)",
          "self.upload_staging.reserve(device, total)")
esc = sub(esc, "ctx.queue.write_buffer_with(", "queue.write_buffer_with(")
assert "ctx" not in esc.replace("context", ""), "ctx left in encode_staged_copies"
assert "ctx" not in epr.replace("context", ""), "ctx left in encode_pending_rgba"

tail = '''}

fn log_decode_error(codec: Codec, tile_x: u8, tile_y: u8, code: DecodeErrorCode) {
    tracing::warn!(?codec, tile_x, tile_y, ?code, "tile decode error");
}
'''
text = (parts / 'tr_head.rs').read_text() + blk1 + blk2 \
    + (parts / 'tr_types.rs').read_text() + (parts / 'tr_free.rs').read_text() \
    + (parts / 'tr_impl1.rs').read_text() + (parts / 'tr_impl2.rs').read_text() \
    + epr + esc.rstrip() + "\n" + tail
if (parts / 'tr_tests.rs').exists():
    text += (parts / 'tr_tests.rs').read_text()
open('ghostframe-render/src/tile_renderer.rs', 'w').write(text)
print("wrote", len(text.splitlines()), "lines")
```


- [ ] **Step 5: Assemble, format, test**

51 = 36 + the 15 new ones.

```bash
python3 /tmp/pr1-parts/build_tr.py /tmp/pr1-parts
cargo fmt -p ghostframe-render
cargo test -p ghostframe-render 2>&1 | grep -E "^test result|FAILED|^error" 
```

Expected:

```
wrote 1918 lines
test result: ok. 51 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```


- [ ] **Step 6: Lint (host)**

```bash
cargo clippy -p ghostframe-render --all-targets -- -D warnings
```

Expected:

```
    Finished `dev` profile ...
```


- [ ] **Step 7: Break it: three guards, each must turn its test red**

Each file is restored from a copy before the next mutation, and the full suite is run once more at the end. The format and dimension guards, the damage maths and the dirty-list cap are covered by tests that fail on their own obvious mutation (change `Rgba8Unorm` in `new`, drop the `h`-clamp in `to_pixels`, raise `MAX_DIRTY_LIST`); do one of those by hand if you doubt them.

```bash
cp ghostframe-render/src/tile_renderer.rs /tmp/pr1-parts/tile_renderer.rs.orig
cp ghostframe-render/src/paint_order.rs /tmp/pr1-parts/paint_order.rs.orig

echo "== 1. the CDF 5/3 grid never records"
python3 - <<'EOF'
p = 'ghostframe-render/src/tile_renderer.rs'
s = open(p).read()
a = "        if self.cdf53_hold {\n            return false;"
assert s.count(a) == 1
open(p, 'w').write(s.replace(a, "        if true {\n            return false;"))
EOF
cargo test -p ghostframe-render --lib the_cdf53_grid_holds 2>&1 | grep -E "^test |test result"
cp /tmp/pr1-parts/tile_renderer.rs.orig ghostframe-render/src/tile_renderer.rs

echo "== 2. the H.264 stale test compares the wrong way round"
python3 - <<'EOF'
p = 'ghostframe-render/src/paint_order.rs'
s = open(p).read()
a = "|| self.max_tile.is_some_and(|t| seq_newer(t, seq))"
assert s.count(a) == 1
open(p, 'w').write(s.replace(a, "|| self.max_tile.is_some_and(|t| seq_newer(seq, t))"))
EOF
cargo test -p ghostframe-render --lib the_h264_ledger 2>&1 | grep -E "^test |test result"
cp /tmp/pr1-parts/paint_order.rs.orig ghostframe-render/src/paint_order.rs

echo "== 3. a superseded slot tile is not retired"
python3 - <<'EOF'
p = 'ghostframe-render/src/tile_renderer.rs'
s = open(p).read()
a = "                slots.retire(slot);\n            }\n            keep"
assert s.count(a) == 1
open(p, 'w').write(s.replace(a, "            }\n            keep"))
EOF
cargo test -p ghostframe-render --lib slot_tiles_are 2>&1 | grep -E "^test |test result"
cp /tmp/pr1-parts/tile_renderer.rs.orig ghostframe-render/src/tile_renderer.rs
cargo test -p ghostframe-render 2>&1 | grep -E "^test result" | head -1
```

Expected:

```
== 1. the CDF 5/3 grid never records
test tile_renderer::tests::the_cdf53_grid_holds_passes_until_a_flush_lists_them ... FAILED
test result: FAILED. 0 passed; 1 failed
== 2. the H.264 stale test compares the wrong way round
test tile_renderer::tests::the_h264_ledger_orders_pictures_against_pictures_and_tiles ... FAILED
test result: FAILED. 0 passed; 1 failed
== 3. a superseded slot tile is not retired
test tile_renderer::tests::slot_tiles_are_retired_exactly_once_and_the_later_one_for_a_tile_wins ... FAILED
test result: FAILED. 0 passed; 1 failed
test result: ok. 51 passed
```

```bash
git add ghostframe-render
git status --short | head -30
git commit -q -F - <<'MSG'
feat(render): TileRenderer, the tile-event half of the native Renderer, portable

apply_event / flush / record / finish_flush, the paint-order ledger, damage
reports, serialisable diagnostics, the CDF 5/3 self-test gate with its CPU
fallback, and a SlotSink seam for the native upload slabs. Tested on wgpu's
no-op backend.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>
MSG
git log --oneline -1
```


## Task 6: Build for the browser, and keep it that way [sonnet]

`cargo check --target wasm32-unknown-unknown` is the gate that makes the crate portable. Run it first, see what it says, fix exactly that, then add the guard for what the compiler cannot see.


- [ ] **Step 1: See it fail**

(The unused-import warning may or may not print before the error, depending on how far the compiler got.) `GpuDecodeError` is native-only on purpose (only the blocking PalRle readback produces it), and so is `take_errors`, the only user of `GpuError` in `palrle.rs`. Everything else in the crate already compiles for wasm32; that is the proof that the moved code needed no change.

```bash
cargo check -p ghostframe-render --target wasm32-unknown-unknown 2>&1 | grep -E "^(error|warning)" -A3 | head -20
```

Expected:

```
error[E0432]: unresolved import `tile_renderer::GpuDecodeError`
note: found an item that was configured out
warning: unused import: `crate::GpuError`
```


- [ ] **Step 2: Fix exactly that**

Write `/tmp/pr1-parts/edit_t5.py`:

```python
#!/usr/bin/env python3
"""T5: what `cargo check --target wasm32-unknown-unknown` asked for."""
def edit(path, pairs):
    s = open(path).read()
    for a, b in pairs:
        assert s.count(a) == 1, (path, a)
        s = s.replace(a, b)
    open(path, 'w').write(s)

R = 'ghostframe-render/src/'
# GpuDecodeError exists only where the blocking readback that produces it does.
edit(R + 'lib.rs', [("""pub use tile_renderer::{
    dimensions_valid, tile_grid, DebugState, DebugTile, FlushReport, GpuDecodeError, NoSlots,
    SlotSink, SlotTile, TileRenderer,
};""", """#[cfg(not(target_arch = "wasm32"))]
pub use tile_renderer::GpuDecodeError;
pub use tile_renderer::{
    dimensions_valid, tile_grid, DebugState, DebugTile, FlushReport, NoSlots, SlotSink, SlotTile,
    TileRenderer,
};""")])
# take_errors blocks on the device, so only native has it, and only it needs GpuError.
s = open(R + 'pipelines/palrle.rs').read()
a = "use crate::GpuError;\n"
assert s.count(a) == 1
s = s.replace(a, '#[cfg(not(target_arch = "wasm32"))]\nuse crate::GpuError;\n')
a = "    pub fn take_errors("
assert s.count(a) == 1
s = s.replace(a, '    #[cfg(not(target_arch = "wasm32"))]\n' + a)
a = "    /// staging buffer cannot be mapped, e.g. after device loss.\n"
assert s.count(a) == 1
s = s.replace(a, a + "    ///\n    /// Native only: it blocks on the device, which a browser cannot do.\n")
open(R + 'pipelines/palrle.rs', 'w').write(s)
print("edit_t5: ok")
```

```bash
python3 /tmp/pr1-parts/edit_t5.py
cargo fmt -p ghostframe-render
cargo check -p ghostframe-render --target wasm32-unknown-unknown
cargo clippy -p ghostframe-render --target wasm32-unknown-unknown --lib -- -D warnings
cargo clippy -p ghostframe-render --all-targets -- -D warnings
cargo test -p ghostframe-render 2>&1 | grep -E "^test result" | head -1
```

The wasm32 clippy is `--lib` only, deliberately: the unit tests call the native-only `readback::block_on_device` (and run on a native no-op device), so a wasm32 test target would not compile. The host `--all-targets` clippy lints the tests. Do not "fix" the wasm line to `--all-targets`.

Expected:

```
edit_t5: ok
    Finished `dev` profile ...
    Finished `dev` profile ...
    Finished `dev` profile ...
test result: ok. 51 passed
```


- [ ] **Step 3: Add the guard the compiler cannot give**

`std::thread` and `Instant::now` compile for `wasm32-unknown-unknown` and panic when called, so a green wasm build proves less than it seems. This script checks the wasm dependency tree for crates a native-only feature would pull in (`pollster`, `libc`, `ash`, `khronos-egl`, ffmpeg, the server) and the source for calls that block, spawn, read a clock or touch the environment.

Write `ci/check-render-portable.sh`:

```bash
#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# ghostframe-render must stay portable: the web client links it into a wasm
# module that has no threads, no blocking waits, no C libraries and no
# std::time::Instant. The compiler only catches some of that (a std::thread use
# compiles for wasm32-unknown-unknown and panics at run time), so this checks
# the two things that can regress without a build error:
#
#   1. the crate's dependency tree for the wasm target names none of the crates
#      a native-only feature would pull in;
#   2. its source calls nothing that blocks or reads a clock.
#
# `just ci-local`, `just ci-client` and CI's clippy job run it.
set -euo pipefail
cd "$(dirname "$0")/.."

fail() { echo "check-render-portable: $*" >&2; exit 1; }

banned='^(pollster|libc|ash|khronos-egl|ffmpeg-next|ffmpeg-sys-next|tokio|ghostframe-lib|ghostframe-client-gpu|ghostframe-client-h264|ghostframe-client-net) '
deps=$(cargo tree -p ghostframe-render --target wasm32-unknown-unknown -e normal --prefix none)
hits=$(printf '%s\n' "$deps" | grep -E "$banned" | sort -u || true)
if [[ -n "$hits" ]]; then
    printf '%s\n' "$hits" >&2
    fail "the wasm dependency tree of ghostframe-render names a crate it must not link"
fi

# Calls, not mentions: the crate docs say why Instant is absent.
hits=$(grep -rnE 'Instant::now|thread::spawn|std::thread::|pollster::|std::fs::|std::env::' ghostframe-render/src || true)
if [[ -n "$hits" ]]; then
    printf '%s\n' "$hits" >&2
    fail "ghostframe-render reads a clock, spawns or blocks on a thread, or touches the environment"
fi
echo "check-render-portable: ok"
```

```bash
chmod +x ci/check-render-portable.sh
ci/check-render-portable.sh
ci/check-licenses.py
```

Expected:

```
check-render-portable: ok
licences OK: 17 crates, 574 third-party crates
```


- [ ] **Step 4: Break it: the guard must reject a clock and a banned dependency**

```bash
cp ghostframe-render/src/submit.rs /tmp/pr1-parts/submit.rs.orig
cp ghostframe-render/Cargo.toml /tmp/pr1-parts/render.Cargo.toml.orig
cp Cargo.lock /tmp/pr1-parts/Cargo.lock.orig

echo 'fn _clock() { let _ = std::time::Instant::now(); }' >> ghostframe-render/src/submit.rs
ci/check-render-portable.sh; echo "exit=$?"
cp /tmp/pr1-parts/submit.rs.orig ghostframe-render/src/submit.rs

sed -i 's/^thiserror = { workspace = true }/thiserror = { workspace = true }\npollster = { workspace = true }/' ghostframe-render/Cargo.toml
ci/check-render-portable.sh; echo "exit=$?"
cp /tmp/pr1-parts/render.Cargo.toml.orig ghostframe-render/Cargo.toml
cp /tmp/pr1-parts/Cargo.lock.orig Cargo.lock
ci/check-render-portable.sh
```

Expected:

```
ghostframe-render/src/submit.rs:46:fn _clock() { let _ = std::time::Instant::now(); }
check-render-portable: ghostframe-render reads a clock, spawns or blocks on a thread, or touches the environment
exit=1
pollster v1.0.1
check-render-portable: the wasm dependency tree of ghostframe-render names a crate it must not link
exit=1
check-render-portable: ok
```

```bash
git add ghostframe-render ci/check-render-portable.sh
git status --short | head -30
git commit -q -F - <<'MSG'
feat(render): build ghostframe-render for wasm32 and guard its portability

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>
MSG
git log --oneline -1
```


## Task 7: The native `Renderer` delegates to `TileRenderer` [sonnet]

`ghostframe-client-gpu` depends on the new crate, keeps every public path, and its `Renderer` becomes a `TileRenderer` plus the export ring, the upload slabs and H.264. The H.264 decode/blit, the PPM dump and the ring helpers are carried over from the old file by the script, with `self.fb` turned into `self.tiles.fb()`. Do not commit until the crate builds (the first three steps leave it broken).


- [ ] **Step 1: Dependency, module paths, error conversion, `SlotRef` token**

`lib.rs` stops declaring the moved modules and re-exports them (`pub use ghostframe_render::{coalesce, dirty, framebuffer}`), so `ghostframe_client_gpu::framebuffer::Framebuffer`, `crate::coalesce::Rect` and the tests' imports all still resolve. `From<ghostframe_render::GpuError>` maps onto the variants this crate always had. `SlotRef` gains `to_bits`/`from_bits` (with a round-trip test) so a slot can travel through `SlotTile::token`.

Write `/tmp/pr1-parts/edit_gpu.py`:

```python
#!/usr/bin/env python3
"""T6: ghostframe-client-gpu depends on ghostframe-render and keeps the old
module paths. Run from the repository root."""
def edit(path, pairs):
    s = open(path).read()
    for a, b in pairs:
        assert s.count(a) == 1, (path, a)
        s = s.replace(a, b)
    open(path, 'w').write(s)

G = 'ghostframe-client-gpu/'
edit(G + 'Cargo.toml', [(
    'ghostframe-client-core = { path = "../ghostframe-client-core" }\n',
    'ghostframe-client-core = { path = "../ghostframe-client-core" }\n'
    '# The portable half: framebuffer, tile pipelines, paint order, damage. `Renderer`\n'
    '# owns a `TileRenderer` and adds the export ring, upload slabs and H.264.\n'
    'ghostframe-render = { path = "../ghostframe-render" }\n')])

edit(G + 'src/lib.rs', [
    ("pub mod coalesce;\npub mod config;\npub mod dirty;\n", "pub mod config;\n"),
    ("pub mod framebuffer;\n#[cfg(feature = \"vulkan\")]\n#[path = \"import.rs\"]",
     "#[cfg(feature = \"vulkan\")]\n#[path = \"import.rs\"]"),
    ("pub(crate) mod paint_order;\npub mod pipelines;", "pub mod pipelines;"),
    ("pub mod wgpu_ctx;\n\nuse thiserror::Error;", """pub mod wgpu_ctx;

// The modules that moved to `ghostframe-render` keep their paths here, so
// `ghostframe_client_gpu::framebuffer::Framebuffer` and friends still resolve
// for the native client, the tests and this crate's own `crate::` paths.
pub use ghostframe_render::{coalesce, dirty, framebuffer};

use thiserror::Error;"""),
])
s = open(G + 'src/lib.rs').read()
s += '''
/// `ghostframe-render`'s errors map onto the variants this crate always had, so
/// a caller that matches `GpuError::InvalidDimensions` or `GpuError::Device`
/// keeps working.
impl From<ghostframe_render::GpuError> for GpuError {
    fn from(e: ghostframe_render::GpuError) -> Self {
        match e {
            ghostframe_render::GpuError::Device(s) => GpuError::Device(s),
            ghostframe_render::GpuError::InvalidDimensions { width, height } => {
                GpuError::InvalidDimensions { width, height }
            }
            ghostframe_render::GpuError::UnsupportedFormat(_) => GpuError::Device(e.to_string()),
        }
    }
}
'''
open(G + 'src/lib.rs', 'w').write(s)

# SlotRef <-> the u64 token ghostframe-render hands back to the sink.
edit(G + 'src/upload_pool.rs', [
("    pub fn generation(&self) -> u32 {\n        self.generation\n    }\n}\n", '''    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Pack into the `u64` that `ghostframe_render::SlotTile::token` carries:
    /// `slab` in bits 48.., `index` in bits 32..48, `generation` in bits 0..32.
    pub(crate) fn to_bits(self) -> u64 {
        (u64::from(self.slab) << 48) | (u64::from(self.index) << 32) | u64::from(self.generation)
    }

    /// The inverse of [`SlotRef::to_bits`].
    pub(crate) fn from_bits(bits: u64) -> Self {
        SlotRef {
            slab: (bits >> 48) as u8,
            index: (bits >> 32) as u16,
            generation: bits as u32,
        }
    }
}
'''),
("#[cfg(test)]\nmod tests {\n    use super::*;\n", '''#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_ref_survives_the_trip_through_the_renderer_s_token() {
        for slot in [
            SlotRef {
                slab: 0,
                index: 0,
                generation: 0,
            },
            SlotRef {
                slab: u8::MAX,
                index: u16::MAX,
                generation: u32::MAX,
            },
            SlotRef {
                slab: 3,
                index: 700,
                generation: 0x8000_0001,
            },
        ] {
            assert_eq!(SlotRef::from_bits(slot.to_bits()), slot);
        }
    }
'''),
])
print("edit_gpu: ok")
```

```bash
python3 /tmp/pr1-parts/edit_gpu.py
```

Expected:

```
edit_gpu: ok
```


- [ ] **Step 2: The three small files**

Write `ghostframe-client-gpu/src/submit.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The submit counter lives in `ghostframe-render`, which makes the flush's
//! submits; this crate's export ring and `Renderer` count theirs through the
//! same per-thread counter, so `Renderer::last_flush_submits` sees all of them.

pub(crate) use ghostframe_render::submit::{count, submit};
```

Write `ghostframe-client-gpu/src/pipelines/mod.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tile codec pipelines that run the real, shipped WGSL against the
//! client's persistent [`crate::framebuffer::Framebuffer`].
//!
//! Each pipeline here loads its shader with `include_str!` from
//! `shaders/client/`, the directory both the web client and this native client
//! compile the exact same file from. The portable ones live in
//! `ghostframe-render` and are re-exported at their old paths.

pub mod cdf53_grid;
pub mod cdf53_passes;
pub use ghostframe_render::pipelines::{cdf53_state, palrle, rgba, solid};
pub mod h264_nv12;
pub mod rgba_slab;
```

The native facade: `pub use` of the portable grid, plus `Probed`, `probe`, `probe_with`, a blocking `self_test_with` and `calibrate`, which are the parts that need `std::time::Instant` and a blocking wait. The signatures are the ones `ghostframe-client-native` and `tests/cdf53_grid.rs` already call.

Write `ghostframe-client-gpu/src/pipelines/cdf53_grid.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The CDF 5/3 grid, as the native client uses it: `ghostframe-render`'s grid
//! plus the start-up probe, which needs a clock and a blocking wait.
//!
//! The probe is [`ghostframe_render::pipelines::cdf53_grid::self_test`] (is the
//! picture right?) followed by a calibration (what does a tile cost on this
//! GPU?). A browser runs the first and not the second.

pub use ghostframe_render::pipelines::cdf53_grid::*;

use crate::framebuffer::Framebuffer;
use ghostframe_render::readback::block_on_device;

/// A grid that passed [`probe`], with what the probe measured.
pub struct Probed {
    pub grid: Cdf53Grid,
    /// GPU microseconds per tile at a complete prefix (k = 14) on this device,
    /// from a 256-tile dispatch with the empty-submit overhead taken off.
    pub us_per_tile_k14: f64,
}

/// Build a grid, prove it works on this device, and measure what it costs.
///
/// Everything runs inside wgpu error scopes (validation, internal and
/// out-of-memory), so a pipeline that fails to build -- a shader the GL
/// backend cannot compile, say -- comes back as `Err` instead of reaching
/// wgpu's default uncaptured-error handler, which panics the render thread.
/// The caller then decodes on the CPU.
///
/// Blocks on the device; call it once, at start-up, on the render thread. The
/// grid it returns is the one that was tested: hand it to the renderer rather
/// than building another (the reconstruct shader is slow to compile on
/// panfrost).
pub fn probe(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Probed, String> {
    probe_with(device, queue, Cdf53Grid::new)
}

/// [`probe`] with the grid made by `make` (a test that must see it fail).
pub fn probe_with(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    make: impl FnOnce(&wgpu::Device) -> Cdf53Grid,
) -> Result<Probed, String> {
    let scopes = ErrorScopes::push(device);
    let result = (|| {
        let mut grid = make(device);
        self_test_with(device, queue, &mut grid)?;
        let us_per_tile_k14 = calibrate(device, queue, &mut grid)?;
        Ok(Probed {
            grid,
            us_per_tile_k14,
        })
    })();
    if let Some(e) = block_on_device(device, scopes.finish()) {
        return Err(e);
    }
    result
}

/// [`self_test`], blocking.
pub fn self_test_with(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    grid: &mut Cdf53Grid,
) -> Result<(), String> {
    block_on_device(device, self_test(device, queue, grid))
}

/// Tiles in the calibration dispatch: enough that the fixed cost of a submit
/// is a small part of it (at 64 tiles it inflated the per-tile figure by 40%
/// on a Mali-T860), few enough to take ~15 ms there.
const CALIBRATION_TILES: u32 = 256;

/// GPU microseconds per complete tile on this device: the wall time of
/// submit + wait for a [`CALIBRATION_TILES`]-tile frame of known content,
/// minus that of an empty submit, median of five after a warm-up. Wall time
/// includes the driver's own overhead, which is what a flush pays too.
fn calibrate(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    grid: &mut Cdf53Grid,
) -> Result<f64, String> {
    use std::time::Instant;
    let tiles = self_test_tiles();
    let fixture = &tiles[0];
    grid.resize(device, 16, 16).map_err(|e| e.to_string())?;
    let fb = Framebuffer::new(device, 16 * 32, 16 * 32);
    let wait = || device.poll(wgpu::PollType::wait_indefinitely());
    let timed = |f: &mut dyn FnMut() -> Result<(), String>| -> Result<f64, String> {
        let t = Instant::now();
        f()?;
        wait().map_err(|e| e.to_string())?;
        Ok(Instant::now().duration_since(t).as_secs_f64() * 1e6)
    };
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() / 2]
    };
    let mut empty = || {
        let enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        queue.submit(std::iter::once(enc.finish()));
        Ok(())
    };
    let mut full = || {
        grid.begin_frame();
        for i in 0..CALIBRATION_TILES {
            for (p, plane) in fixture.planes.iter().enumerate() {
                if let Some(plane) = plane {
                    grid.push_plane(i, p as u8, plane);
                }
            }
            grid.push_tile(i, fixture.present as u32, 14);
        }
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-cdf53-calibration"),
        });
        if !grid.record(device, queue, &mut enc, &fb) {
            return Err("calibration recorded nothing".to_string());
        }
        queue.submit(std::iter::once(enc.finish()));
        Ok(())
    };
    timed(&mut full)?; // warm-up: first use compiles and binds
    let mut base = Vec::new();
    let mut work = Vec::new();
    for _ in 0..5 {
        base.push(timed(&mut empty)?);
        work.push(timed(&mut full)?);
    }
    let per_tile = (median(work) - median(base)) / CALIBRATION_TILES as f64;
    Ok(per_tile.clamp(0.5, 500.0))
}
```


- [ ] **Step 3: Write the new `renderer.rs` parts and assemble**

Header and imports.

Write `/tmp/pr1-parts/rn_head.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The native half of the client's renderer: a [`TileRenderer`] (the portable
//! tile pipelines and framebuffer, in `ghostframe-render`) plus the export
//! ring, the upload slabs and the H.264 decode and blit.
//!
//! [`Renderer::apply_event`] is the routing point from `ClientCore`'s
//! `Event`s: tile events go to the `TileRenderer`, `NeedsH264` to the decoder.
//! It borrows `WgpuContext` rather than owning it -- the context
//! (instance/adapter/device/queue) is created once per process and shared
//! across sessions in the native client, while a `Renderer` is created per
//! session (it owns per-session framebuffer/ring state sized to that session's
//! frame dimensions).

use crate::export::ExportedImage;
use crate::pipelines::cdf53_grid::Probed;
use crate::pipelines::cdf53_state::Cdf53Stats;
use crate::pipelines::rgba_slab::{RgbaSlabPipeline, SlabWork};
use crate::ring::{ExportRing, PublishedFrame};
use crate::upload_pool::{SlotRef, UploadPool};
use crate::wgpu_ctx::WgpuContext;
use crate::GpuError;

use ghostframe_client_core::Event;
pub use ghostframe_render::GpuDecodeError;
use ghostframe_render::{dimensions_valid, SlotSink, SlotTile, TileRenderer};
```

`SlotState` (the native `SlotSink`) and the `Renderer` struct's new fields.

Write `/tmp/pr1-parts/rn_mid1.rs`:

```rust
/// What the native client adds to a [`TileRenderer`]'s tile path: tiles whose
/// pixels the decode thread already wrote into an upload slab, drawn by a
/// compute pipeline that reads the slab.
///
/// It is the renderer's [`SlotSink`]: the flush hands it every slot it was
/// given, once, and the slab pool is told after the submit which slabs the
/// GPU has finished with.
#[derive(Default)]
struct SlotState {
    /// The slab-sourced twin of the RGBA pipeline; `Some` once a pool is attached.
    pipeline: Option<RgbaSlabPipeline>,
    pool: Option<UploadPool>,
    /// Slots of the flush in progress, counted per slab.
    retired: Vec<u32>,
}

impl SlotState {
    fn begin_flush(&mut self) {
        self.retired = vec![0; self.pool.as_ref().map_or(0, |p| p.slab_count())];
    }
}

impl SlotSink for SlotState {
    fn retire(&mut self, slot: &SlotTile) {
        // Every slot handed to the flush comes through here (drawn or
        // superseded), so this covers all of them, as the old inline check did.
        if let Some(pool) = &self.pool {
            debug_assert!(
                pool.is_current(SlotRef::from_bits(slot.token)),
                "slot {slot:?} was recycled before it was drawn"
            );
        }
        if let Some(n) = self.retired.get_mut(slot.slab) {
            *n += 1;
        }
    }

    fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        fb: &crate::framebuffer::Framebuffer,
        tiles: &[(u32, u32, SlotTile)],
    ) -> bool {
        let (Some(pipeline), Some(pool)) = (self.pipeline.as_mut(), &self.pool) else {
            return false;
        };
        // One dispatch per slab used.
        let mut batches: Vec<(usize, Vec<SlabWork>)> = Vec::new();
        for &(tile_x, tile_y, slot) in tiles {
            let work = SlabWork {
                tile_x,
                tile_y,
                slot: slot.index,
            };
            match batches.iter_mut().find(|(slab, _)| *slab == slot.slab) {
                Some((_, tiles)) => tiles.push(work),
                None => batches.push((slot.slab, vec![work])),
            }
        }
        pipeline.encode(device, queue, encoder, fb, pool, &batches)
    }
}

/// Owns the tile renderer and the export ring (which in turn owns the dirty
/// history) for one client session.
///
/// Per-frame tile work is batched: `apply_event` only records what arrived
/// and marks the affected tile dirty in the ring's history; `flush` is what
/// actually runs the render/compute passes, once per frame, rather than one
/// pass per tile.
pub struct Renderer {
    tiles: TileRenderer,
    ring: ExportRing,
    slots: SlotState,
    /// Submits the most recent flush made for its own GPU work. See
    /// [`Renderer::last_flush_submits`].
    last_flush_submits: u32,

    /// How many PPM dumps have been written, against
    /// `GHOSTFRAME_CLIENT_DUMP_COUNT`. Counts attempts, not successes -- see
    /// `maybe_dump_framebuffer`.
    dumps_written: u64,
    /// Dump sampling, read from the environment once at construction.
    dump: crate::config::DumpConfig,
    /// Completed `flush` calls, so the dump can be deferred past a server's
    /// opening burst.
    flushes: u64,
```

`Drop` (hands back unflushed slots) and the start of the impl.

Write `/tmp/pr1-parts/rn_mid2.rs`:

```rust

impl Drop for Renderer {
    /// Slots buffered but never flushed were issued by the decode thread and
    /// will never be drawn; without this their slabs would stay held for the
    /// life of the pool, which outlives a replaced renderer.
    fn drop(&mut self) {
        let Some(pool) = &self.slots.pool else {
            return;
        };
        let mut counts = vec![0u32; pool.slab_count()];
        self.tiles
            .for_each_pending_slot(|slot| counts[slot.slab] += 1);
        pool.retire_now(&counts);
    }
}

impl Renderer {
```

`Renderer::new`: builds the `TileRenderer`, then the ring.

Write `/tmp/pr1-parts/rn_new_sig.rs`:

```rust
    pub fn new(
        ctx: &WgpuContext,
        width: u32,
        height: u32,
        export_buffers: usize,
        preferred_modifiers: &[u64],
        // Diagnostic-only: makes `debug_map_frame` possible at the cost of
        // pinning exports to CPU-visible memory. Production passes false.
        host_visible: bool,
        open_h264_eagerly: bool,
    ) -> Result<Self, GpuError> {
        if export_buffers == 0 {
            return Err(GpuError::NoExportBuffers);
        }
        if !dimensions_valid(width, height) {
            return Err(GpuError::InvalidDimensions { width, height });
        }
        let tiles = TileRenderer::new(
            &ctx.device,
            &ctx.queue,
            wgpu::TextureFormat::Rgba8Unorm,
            width,
            height,
        )?;
        let ring = ExportRing::new(
            ctx,
            width,
            height,
            export_buffers,
            preferred_modifiers,
            host_visible,
        )?;
```

The struct literal's new fields.

Write `/tmp/pr1-parts/rn_new_tail.rs`:

```rust
        Ok(Renderer {
            tiles,
            ring,
            slots: SlotState::default(),
            last_flush_submits: 0,
            dumps_written: 0,
            dump: crate::config::DumpConfig::from_env(),
            flushes: 0,
```

`apply_event` (tile events go to the `TileRenderer`, `NeedsH264` and `FrameDimensions` stay here), `apply_frame_dimensions`, the CDF 5/3 and slot methods, `flush_inner` (record, add the export blit to the same encoder, one submit, then the readback), `publish`.

Write `/tmp/pr1-parts/rn_events.rs`:

```rust

    /// Mark in the export ring every tile the tile renderer dirtied since the
    /// last call, so the next publish carries it.
    fn sync_dirty(&mut self) {
        let ring = &mut self.ring;
        if self
            .tiles
            .drain_dirty(|x, y| ring.mark_dirty(x as u32, y as u32))
        {
            ring.mark_dirty_all();
        }
    }

    /// Route one `ClientCore` event to the right pipeline and mark the tile
    /// dirty. Batches within a frame; [`Renderer::flush`] submits.
    ///
    /// Every `Event` variant is matched explicitly -- see this crate's
    /// module doc on `clippy::wildcard_enum_match_arm` for why a `_ =>` arm
    /// is refused here.
    pub fn apply_event(&mut self, ctx: &WgpuContext, ev: Event) -> Result<(), GpuError> {
        match ev {
            Event::NeedsH264 {
                frame_seq,
                payload,
                is_keyframe,
                reset_decoder,
                ..
            } => {
                self.decode_h264(ctx, frame_seq, is_keyframe, reset_decoder, &payload)?;
            }
            Event::FrameDimensions { width, height } => {
                self.apply_frame_dimensions(ctx, width, height)?;
            }
            ev @ (Event::TileReady { .. }
            | Event::TilePayload { .. }
            | Event::DecodeError { .. }
            | Event::PaletteUpdated { .. }
            | Event::Evicted { .. }) => {
                self.tiles.apply_event(&ctx.device, &ctx.queue, ev)?;
                self.sync_dirty();
            }
        }
        Ok(())
    }

    fn apply_frame_dimensions(
        &mut self,
        ctx: &WgpuContext,
        width: u32,
        height: u32,
    ) -> Result<(), GpuError> {
        // Ignore a size the renderer would not have been built with: keep the
        // current framebuffer rather than allocate from a network-supplied
        // number.
        if !dimensions_valid(width, height) {
            tracing::warn!(width, height, "ignoring out-of-range FrameDimensions");
            return Ok(());
        }
        // Run any batched work against the OLD framebuffer size before
        // resizing -- the pending tiles were addressed against it.
        self.flush(ctx)?;

        let epoch = self.tiles.fb().epoch();
        self.tiles.resize(&ctx.device, &ctx.queue, width, height);
        if self.tiles.fb().epoch() != epoch {
            // The cached bind groups hold the old texture; drop them now
            // rather than when the codec next draws, or the old framebuffer
            // outlives the resize.
            if let Some(p) = self.slots.pipeline.as_mut() {
                p.drop_framebuffer_cache();
            }
        }
        if let Err(e) = self.ring.resize(ctx, width, height) {
            tracing::error!(error = %e, width, height, "export ring resize failed");
        }
        // `tiles.resize` marked everything dirty; marking the ring all-dirty
        // after `ring.resize` is redundant but harmless: `ring.resize` clears
        // `last_publish`, so the first publish after a resize is a full blit
        // and reports the full surface either way. Native behaviour is as before.
        self.sync_dirty();
        Ok(())
    }

    /// Turn on GPU reconstruction of CDF 5/3 tiles. Allocates the grid's plane
    /// store for the current frame size. The caller is expected to have run
    /// [`crate::pipelines::cdf53_grid::probe`] on this device first: this does
    /// not.
    ///
    /// A frame whose plane store the device cannot bind (about 8K) is decoded
    /// on the CPU by this renderer instead, and the GPU is used again as soon
    /// as a frame fits.
    ///
    /// Builds its own grid with the cost model of the Mali-T860 it was fitted
    /// on (and the 1000-tile submit cap); the native client instead hands over
    /// the grid it probed, with [`Renderer::enable_cdf53_gpu_with`].
    pub fn enable_cdf53_gpu(&mut self, ctx: &WgpuContext) {
        self.tiles.enable_cdf53_gpu(&ctx.device);
    }

    /// [`Renderer::enable_cdf53_gpu`] with a grid that already passed
    /// [`crate::pipelines::cdf53_grid::probe`] on this device, so the shaders
    /// are compiled once, and with the per-flush budget calibrated from what
    /// the probe measured. `max_tiles` caps one submit (the ~500 ms job
    /// watchdog of panfrost/GL is the reason; Vulkan can take far more).
    pub fn enable_cdf53_gpu_with(
        &mut self,
        ctx: &WgpuContext,
        probed: Probed,
        max_tiles: usize,
    ) {
        self.tiles.enable_cdf53_gpu_with(
            &ctx.device,
            probed.grid,
            Some(probed.us_per_tile_k14),
            max_tiles,
        );
    }

    /// Pretend the device's storage binding limit is `bytes`, from the next
    /// resize on. For tests of the too-large-frame fallback.
    #[doc(hidden)]
    pub fn debug_set_cdf53_limit(&mut self, bytes: Option<u64>) {
        self.tiles.debug_set_cdf53_limit(bytes);
    }

    /// Whether CDF 5/3 tiles are waiting for a later flush (the GPU budget was
    /// spent, or a partial tile is being throttled). The caller should flush
    /// again soon even if no event arrives.
    pub fn has_deferred_work(&self) -> bool {
        self.tiles.has_deferred_work()
    }

    /// CDF 5/3 scheduling counters since the last call, and the number of
    /// grid frames recorded; `None` when the GPU path is not enabled.
    pub fn take_cdf53_stats(&mut self) -> Option<(Cdf53Stats, u64)> {
        self.tiles.take_cdf53_stats()
    }

    /// Let this renderer draw tiles from `pool`'s slabs
    /// ([`Renderer::apply_slot_tile`]). Without it such tiles are dropped.
    ///
    /// The pool outlives any one renderer (it belongs to the GPU context); a
    /// renderer that is replaced retires the slots it still held when it drops.
    pub fn attach_upload_pool(&mut self, ctx: &WgpuContext, pool: &UploadPool) {
        self.slots.pipeline = Some(RgbaSlabPipeline::new(&ctx.device, pool));
        self.slots.pool = Some(pool.clone());
    }

    /// The slab counterpart of an `Event::TileReady`: the tile's pixels are
    /// already in `slot`, so only its place in the paint order is recorded.
    /// Behaves exactly as `TileReady` does for ordering, superseding and
    /// duplicates (the later of two for one tile wins, whichever kind).
    ///
    /// Returns whether the renderer took the slot. When it did not (no pool
    /// attached) the slot is the caller's to hand back with
    /// [`UploadPool::retire_now`]; when it did, the renderer retires it.
    #[must_use]
    pub fn apply_slot_tile(
        &mut self,
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        slot: SlotRef,
    ) -> bool {
        let Some(pool) = &self.slots.pool else {
            return false;
        };
        debug_assert!(
            pool.is_current(slot),
            "slot {slot:?} arrived after its slab was recycled"
        );
        self.tiles.apply_slot_tile(
            frame_seq,
            tile_x,
            tile_y,
            SlotTile {
                slab: slot.slab(),
                index: slot.index(),
                token: slot.to_bits(),
            },
        );
        self.sync_dirty();
        true
    }

    /// Whether [`Renderer::flush`] reads the PalRle error slots back. Off by
    /// default: the readback is a blocking `poll(wait_indefinitely)` per
    /// flush, and until there is a path from the render thread to
    /// `ClientCore`'s feedback batcher the result cannot reach the server, so
    /// paying for it would be pure cost.
    pub fn set_collect_decode_errors(&mut self, on: bool) {
        self.tiles.set_collect_decode_errors(on);
    }

    /// Take the GPU-detected tile decode failures accumulated by
    /// [`Renderer::flush`].
    ///
    /// The shader validates PalRle payloads on the device (an index past the
    /// palette count, say), so only the readback after the decode can tell.
    /// The caller is expected to relay these as DECODE_ERROR feedback.
    pub fn take_decode_errors(&mut self) -> Vec<GpuDecodeError> {
        self.tiles.take_decode_errors()
    }

    /// Submit all batched work, in one submit. The ordering rules are
    /// [`TileRenderer::flush`]'s.
    ///
    /// Fails only if the opt-in PalRle error readback cannot wait on the
    /// device (e.g. it was lost). That runs after the submit, so the frame's
    /// work is already on the GPU; the errors it would have reported are lost.
    pub fn flush(&mut self, ctx: &WgpuContext) -> Result<(), GpuError> {
        self.flush_inner(ctx, false).map(|_| ())
    }

    /// [`Renderer::flush`] and [`Renderer::publish`] in one submit: the
    /// export blit is recorded after the codecs, into the same encoder.
    ///
    /// `Ok(None)` when every export buffer is held by the host (the flush
    /// still ran). On error nothing is published; the damage stays in the
    /// ring's history for the next publish.
    pub fn flush_and_publish(
        &mut self,
        ctx: &WgpuContext,
    ) -> Result<Option<PublishedFrame>, GpuError> {
        self.flush_inner(ctx, true)
    }

    /// Submits the most recent flush made for its own GPU work: 1 when it had
    /// anything to draw (with or without a publish), 0 when it had nothing.
    /// Not counted: the opt-in PalRle error readback and framebuffer dump,
    /// which are submits of their own by design. For tests and diagnostics.
    pub fn last_flush_submits(&self) -> u32 {
        self.last_flush_submits
    }

    fn flush_inner(
        &mut self,
        ctx: &WgpuContext,
        publish: bool,
    ) -> Result<Option<PublishedFrame>, GpuError> {
        let submits_before = crate::submit::count();
        if self.slots.pool.is_some() {
            // Fires the completion callbacks of earlier submits, so slots they
            // read are retired even when nothing else polls the device (a
            // flush that did not publish waits for nothing).
            let _ = ctx.device.poll(wgpu::PollType::Poll);
        }
        self.slots.begin_flush();
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ghostframe-flush"),
            });
        let mut recorded =
            self.tiles
                .record(&ctx.device, &ctx.queue, &mut encoder, &mut self.slots);
        // The CDF 5/3 tiles the grid listed are dirty now, before the plan.
        self.sync_dirty();

        let plan = if publish {
            self.ring.plan_publish(self.tiles.fb())
        } else {
            None
        };
        if let Some(plan) = &plan {
            recorded |= self.ring.encode_blit(
                &ctx.device,
                &ctx.queue,
                &mut encoder,
                self.tiles.fb(),
                plan,
            );
        }

        if recorded {
            if plan.is_some() {
                self.ring.arm_fence(ctx);
            }
            crate::submit::submit(&ctx.queue, encoder);
        }
        if let Some(pool) = &self.slots.pool {
            // After the submit that may read them, or at once if there was none.
            // Not tied to the publish below, which may not happen.
            let retired = std::mem::take(&mut self.slots.retired);
            if recorded {
                pool.retire_when_submitted_work_done(&ctx.queue, retired);
            } else {
                pool.retire_now(&retired);
            }
        }
        let submits = crate::submit::count().wrapping_sub(submits_before) as u32;
        self.last_flush_submits = submits;
        tracing::debug!(submits, publish, "flush: submitted");

        self.tiles.finish_flush(&ctx.device, &ctx.queue)?;
        self.flushes = self.flushes.saturating_add(1);
        self.maybe_dump_framebuffer(ctx);
        Ok(plan.map(|plan| self.ring.finish_publish(ctx, plan, recorded)))
    }

    /// Seal a generation and hand out an export buffer, if one is free.
    pub fn publish(&mut self, ctx: &WgpuContext) -> Option<PublishedFrame> {
        self.ring.publish(ctx, self.tiles.fb())
    }
```

`debug_read_framebuffer`.

Write `/tmp/pr1-parts/rn_dbg.rs`:

```rust
    pub fn debug_read_framebuffer(&self, ctx: &WgpuContext) -> Vec<u8> {
        self.tiles.fb().debug_read(&ctx.device, &ctx.queue)
    }
```

The four H.264 ordering helpers, now one-liners over the ledger.

Write `/tmp/pr1-parts/rn_h264.rs`:

```rust
    /// The flush that precedes an H.264 blit: Solid / PalRle / RGBA work lands
    /// under the frame, as it always did, but CDF 5/3 tiles are held back. The
    /// frame covers the screen, so drawing them first would only spend a
    /// flush's GPU budget; and if the frame is then blitted they are
    /// cancelled ([`Renderer::supersede_cdf53`]), where drawing them later
    /// would put them on top of it.
    fn flush_under_h264(&mut self, ctx: &WgpuContext) -> Result<(), GpuError> {
        self.tiles.hold_cdf53(true);
        let r = self.flush(ctx);
        self.tiles.hold_cdf53(false);
        r
    }

    /// An H.264 frame from `frame_seq` was blitted over the whole framebuffer:
    /// cancel CDF 5/3 draws still waiting, and refuse older passes.
    pub fn supersede_cdf53(&mut self, frame_seq: u32) {
        self.tiles.supersede_cdf53(frame_seq);
    }

    /// An H.264 picture from `seq` is now over the whole framebuffer: record
    /// it in the paint-order ledger and cancel the CDF 5/3 draws it is newer
    /// than.
    fn h264_picture_drawn(&mut self, seq: u32) {
        self.tiles.note_h264(seq);
    }

    /// The ordering decision of the H.264 blit without a picture, for tests:
    /// `false` when a picture from `seq` is older than what is on screen (and
    /// would not be drawn), otherwise records it as drawn, as the blit does.
    #[doc(hidden)]
    pub fn debug_accept_h264_picture(&mut self, seq: u32) -> bool {
        if self.tiles.h264_stale(seq) {
            return false;
        }
        self.h264_picture_drawn(seq);
        true
    }
```

Anchor-based, asserting. It keeps verbatim: `h264_picture_seq`, `H264ImportPath`, the H.264 struct fields and their initialisers, the eager-open block, the dump/ring helpers, `decode_h264` and everything from `blit_h264_frames` to the end of the file. Its last assertion fails if any reference to a field that moved (`self.fb`, `self.batch_seq`, `self.cdf53*`, `self.pending_*`, `self.solid`, `self.palrle`, `self.rgba`, `self.upload_*`) survived.

Write `/tmp/pr1-parts/build_renderer.py`:

```python
#!/usr/bin/env python3
"""Rewrite ghostframe-client-gpu/src/renderer.rs: the new head and the
delegating methods from the parts directory, the H.264 decode/blit and the
dump/ring helpers kept verbatim from the old file (with `self.fb` turned into
`self.tiles.fb()` and the paint ledger into the tile renderer's).
Run from the repository root: python3 /tmp/pr1-parts/build_renderer.py /tmp/pr1-parts"""
import re, sys, pathlib
parts = pathlib.Path(sys.argv[1])
path = 'ghostframe-client-gpu/src/renderer.rs'
old = open(path).read()
P = lambda n: (parts / n).read_text()

def grab(a, b, include_b=False):
    assert old.count(a) == 1, ("anchor not unique", a)
    i = old.index(a)
    j = old.index(b, i)
    return old[i:j + (len(b) if include_b else 0)]

def retarget(text):
    text = text.replace("self.batch_seq.h264_stale(", "self.tiles.h264_stale(")
    text = text.replace("self.fb.", "self.tiles.fb().")
    text = re.sub(r"&self\.fb\b", "self.tiles.fb()", text)
    return text

seq_fn = grab("/// The `frame_seq` a decoded picture belongs to", "fn tile_grid(")
import_path = grab("/// Which route the most recent H.264 frame took",
                   "/// The GPU CDF 5/3 decoder and the host state")
new_doc = grab("    /// `export_buffers` must be at least 1", "    pub fn new(")
h264_eager = grab("        // `open_h264_eagerly == false` behaves exactly as before this",
                  "        Ok(Renderer {")
h264_fields = grab("    /// `Some` from construction on when `Renderer::new`'s `open_h264_eagerly`",
                   "    h264_size_mismatch_logged: bool,\n}\n", include_b=True)
h264_init = grab("            h264_decoder,\n", "        })\n    }\n", include_b=True)
t_dump = retarget(grab("    /// Write the framebuffer to binary PPM files when",
                       "    pub fn debug_read_framebuffer("))
t_decode = retarget(grab("    /// Decode one access unit and blit every frame the decoder has ready.",
                         "    /// The flush that precedes an H.264 blit"))
t_blit = retarget(old[old.index("    /// Blit `frames` and mark the ring dirty if any landed."):])
# `log_decode_error` moved to ghostframe-render with the DecodeError arm.
cut = t_blit.index("fn log_decode_error(")
assert t_blit[cut:].count("}") == 1, "log_decode_error is not the last item as expected"
t_blit = t_blit[:cut].rstrip() + "\n"

text = (P('rn_head.rs') + seq_fn + import_path + P('rn_mid1.rs') + h264_fields
        + P('rn_mid2.rs') + new_doc + P('rn_new_sig.rs') + h264_eager
        + P('rn_new_tail.rs') + h264_init + P('rn_events.rs') + t_dump
        + P('rn_dbg.rs') + t_decode + P('rn_h264.rs') + t_blit)
open(path, 'w').write(text)
left = re.findall(r"self\.(?:fb|batch_seq|cdf53\w*|pending_\w+|solid|palrle|rgba|upload_\w+)\b", text)
assert not left, left
print("wrote", len(text.splitlines()), "lines")
```

No warnings: an unused import or a leftover reference would show here.

```bash
python3 /tmp/pr1-parts/build_renderer.py /tmp/pr1-parts
cargo fmt -p ghostframe-client-gpu -p ghostframe-render
cargo check -p ghostframe-client-gpu --all-targets 2>&1 | tail -3
```

Expected:

```
wrote 1317 lines
    Finished `dev` profile ...
```


- [ ] **Step 4: Lint: host, the GLES feature set, and the crates above**

```bash
cargo clippy -p ghostframe-client-gpu -p ghostframe-client-native -p ghostframe-client-capi -p ghostframe-render --all-targets -- -D warnings
cargo clippy -p ghostframe-client-gpu --no-default-features --features gles,decode-vaapi,test-support --all-targets -- -D warnings
```

Expected:

```
    Finished `dev` profile ... (twice, no diagnostics)
```


- [ ] **Step 5: Unit tests of the native crates (no GPU needed for these, but run through the wrapper)**

`--lib` is 37 where master had 72: 36 unit tests moved to the new crate and the `SlotRef` round-trip test was added.

```bash
~/work/gf-gpu-run cargo test -p ghostframe-client-gpu --lib 2>&1 | grep -E "^test result"
~/work/gf-gpu-run cargo test -p ghostframe-client-native -p ghostframe-client-capi -p ghostframe-cli --no-fail-fast 2>&1 | grep -E "^test result|FAILED"
```

Expected:

```
test result: ok. 37 passed; 0 failed
test result: ok. 34 passed ... (cli)
... all ok, none FAILED
```

```bash
git add Cargo.lock ghostframe-client-gpu/Cargo.toml ghostframe-client-gpu/src
git status --short | head -30
git commit -q -F - <<'MSG'
refactor(client-gpu): Renderer delegates tile work to ghostframe-render's TileRenderer

Public API and module paths are unchanged (the moved modules are re-exported).
The CDF 5/3 probe keeps its blocking, timed form here as a facade over the
portable async self-test. One submit per flush still includes the export blit.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>
MSG
git log --oneline -1
```


## Task 8: GPU tests: the same pixels, the portable pieces, the baseline [sonnet]

The unit tests cannot say what pixels are drawn. These do, on a real device, and compare the wrapper against the type it wraps.


- [ ] **Step 1: Write the test**

Four tests. (1) A `TileRenderer` alone and the native `Renderer`, fed the same events (a Solid, a `TileReady`, a newer Solid over it, an older Solid arriving late, an edge tile), leave byte-identical framebuffers, and the pixels are what the events say. (2) The async readback equals the blocking one and strips row padding (100 pixels is 400 bytes a row, not a multiple of 256). (3) The CDF 5/3 self-test passes on the real device through `try_enable_cdf53_gpu`. (4) `reset_session` zeroes the palette atlas and clears the paint order: a PalRLE tile from an older frame draws, and draws nothing visible. Like the other `gpu_*` targets it is not named in any workflow: no GPU is a failure, never a skip.

Write `ghostframe-client-gpu/tests/tile_renderer.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! `ghostframe-render`'s `TileRenderer` on its own, on a real device, against
//! the native `Renderer` that wraps one: the same events must leave the same
//! pixels, and the portable pieces (the async readback, the CDF 5/3 self-test)
//! must work the way the web client will use them. Requires a GPU (no GPU is a
//! failure, not a skip); not named in any CI workflow.

use ghostframe_client_core::{Event, TileData};
use ghostframe_client_gpu::renderer::Renderer;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_render::readback::block_on_device;
use ghostframe_render::TileRenderer;

// 100 pixels is 400 bytes a row: not a multiple of the 256-byte copy alignment,
// so a readback that mishandles row padding shears.
const W: u32 = 100;
const H: u32 = 70;

fn events() -> Vec<Event> {
    let mut rgba = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32u32 {
        for x in 0..32u32 {
            rgba.extend_from_slice(&[(x * 8) as u8, (y * 8) as u8, (x + y) as u8, 255]);
        }
    }
    vec![
        Event::TilePayload {
            frame_seq: 1,
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            data: TileData::Solid([10, 20, 30, 255]),
        },
        Event::TileReady {
            frame_seq: 2,
            tile_x: 1,
            tile_y: 0,
            rgba,
        },
        // Newer Solid over the TileReady's tile: must win.
        Event::TilePayload {
            frame_seq: 3,
            tile_x: 1,
            tile_y: 0,
            generation: 0,
            data: TileData::Solid([200, 100, 50, 255]),
        },
        // An older one for the first tile arriving late: must lose.
        Event::TilePayload {
            frame_seq: 0,
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            data: TileData::Solid([99, 99, 99, 255]),
        },
        Event::TileReady {
            frame_seq: 4,
            tile_x: 3,
            tile_y: 2,
            rgba: vec![77; 32 * 32 * 4],
        },
    ]
}

fn px(fb: &[u8], x: u32, y: u32) -> [u8; 4] {
    let o = ((y * W + x) * 4) as usize;
    [fb[o], fb[o + 1], fb[o + 2], fb[o + 3]]
}

#[test]
fn a_tile_renderer_alone_draws_what_the_native_renderer_draws() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut tiles = TileRenderer::new(
        &ctx.device,
        &ctx.queue,
        wgpu::TextureFormat::Rgba8Unorm,
        W,
        H,
    )
    .expect("tile renderer");
    let mut native = Renderer::new(&ctx, W, H, 3, &[], false, false).expect("renderer");
    for ev in events() {
        tiles
            .apply_event(&ctx.device, &ctx.queue, ev.clone())
            .unwrap();
        native.apply_event(&ctx, ev).unwrap();
    }
    let report = tiles.flush(&ctx.device, &ctx.queue, 1_000).unwrap();
    native.flush(&ctx).unwrap();

    let got = tiles.fb().debug_read(&ctx.device, &ctx.queue);
    assert_eq!(got, native.debug_read_framebuffer(&ctx));

    // Non-vacuous: the pictures are what the events say, not two black frames.
    assert_eq!(
        px(&got, 3, 3),
        [30, 20, 10, 255],
        "Solid is BGRA on the wire"
    );
    assert_eq!(px(&got, 40, 3), [50, 100, 200, 255], "the newer Solid won");
    assert_eq!(
        px(&got, 99, 69),
        [77, 77, 77, 77],
        "TileReady in the clamped corner"
    );
    assert!(report.recorded);
    assert_eq!(report.submits, 1);
    assert_eq!(
        report.damage.len(),
        2,
        "tiles (0,0)+(1,0) merge; (3,2) is apart"
    );
}

#[test]
fn the_async_readback_matches_the_blocking_one_and_strips_row_padding() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut tiles = TileRenderer::new(
        &ctx.device,
        &ctx.queue,
        wgpu::TextureFormat::Rgba8Unorm,
        W,
        H,
    )
    .unwrap();
    for ev in events() {
        tiles.apply_event(&ctx.device, &ctx.queue, ev).unwrap();
    }
    tiles.flush(&ctx.device, &ctx.queue, 0).unwrap();
    let blocking = tiles.fb().debug_read(&ctx.device, &ctx.queue);
    let asynchronous = block_on_device(&ctx.device, tiles.fb().read_rgba(&ctx.device, &ctx.queue))
        .expect("async readback");
    assert_eq!(asynchronous.len(), (W * H * 4) as usize, "tight rows");
    assert_eq!(asynchronous, blocking);
    // The right-hand column of tiles is 4 pixels wide: a row that drifted by
    // the padding would put the TileReady's pixels at the wrong x.
    assert_eq!(px(&asynchronous, 96, 64), [77, 77, 77, 77]);
}

#[test]
fn the_cdf53_self_test_passes_on_the_real_device_and_the_grid_then_decodes() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut tiles = TileRenderer::new(
        &ctx.device,
        &ctx.queue,
        wgpu::TextureFormat::Rgba8Unorm,
        W,
        H,
    )
    .unwrap();
    block_on_device(
        &ctx.device,
        tiles.try_enable_cdf53_gpu(&ctx.device, &ctx.queue),
    )
    .expect("the shipped shaders pass their own self-test");
    assert!(tiles.debug_state().cdf53_gpu);
}

#[test]
fn a_session_reset_zeroes_the_palettes_and_forgets_the_paint_order() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut tiles = TileRenderer::new(
        &ctx.device,
        &ctx.queue,
        wgpu::TextureFormat::Rgba8Unorm,
        64,
        64,
    )
    .unwrap();
    let palrle = |seq: u32| Event::TilePayload {
        frame_seq: seq,
        tile_x: 0,
        tile_y: 0,
        generation: 0,
        data: TileData::PalRle {
            palette_id: 0,
            count: 1,
            indices: vec![0; 512],
            wire_flags: 0,
        },
    };
    let read = |tiles: &TileRenderer| {
        let rgba = tiles.fb().debug_read(&ctx.device, &ctx.queue);
        [rgba[0], rgba[1], rgba[2], rgba[3]]
    };
    let palette = Event::PaletteUpdated {
        palette_id: 0,
        colors: vec![[10, 20, 30, 255]],
    };
    for ev in [palette, palrle(5)] {
        tiles.apply_event(&ctx.device, &ctx.queue, ev).unwrap();
    }
    tiles.flush(&ctx.device, &ctx.queue, 0).unwrap();
    assert_eq!(read(&tiles), [30, 20, 10, 255], "BGRA palette entry, drawn");

    tiles.reset_session(&ctx.device, &ctx.queue).unwrap();
    // The same tile again, from an OLDER frame. Only a cleared paint order lets
    // it draw at all, and only a zeroed palette makes it draw nothing visible:
    // both halves of the reset show in this one pixel.
    tiles
        .apply_event(&ctx.device, &ctx.queue, palrle(2))
        .unwrap();
    tiles.flush(&ctx.device, &ctx.queue, 1).unwrap();
    assert_eq!(read(&tiles), [0, 0, 0, 0]);
}
```


- [ ] **Step 2: Run it**

```bash
cargo fmt -p ghostframe-client-gpu
~/work/gf-gpu-run cargo test -p ghostframe-client-gpu --test tile_renderer 2>&1 | grep -E "^test |test result"
```

Expected:

```
test a_tile_renderer_alone_draws_what_the_native_renderer_draws ... ok
test the_async_readback_matches_the_blocking_one_and_strips_row_padding ... ok
test the_cdf53_self_test_passes_on_the_real_device_and_the_grid_then_decodes ... ok
test a_session_reset_zeroes_the_palettes_and_forgets_the_paint_order ... ok
test result: ok. 4 passed
```


- [ ] **Step 3: Break it: padding, the self-test's comparison, the paint order and the session reset**

Mutation 3's anchor is the last line of `SeqLedger::superseded`; if `cargo fmt` laid it out differently the assert stops the script; mutate the `seq_newer(newest, seq)` comparison by hand. After each mutation the file is restored from its copy; the last command confirms nothing is left modified.

```bash
cp ghostframe-render/src/readback.rs /tmp/pr1-parts/readback.rs.orig
cp ghostframe-render/src/pipelines/cdf53_grid.rs /tmp/pr1-parts/cdf53_grid.rs.orig
cp ghostframe-render/src/paint_order.rs /tmp/pr1-parts/paint_order.rs.orig

echo "== 1. rows copied without skipping the padding"
python3 - <<'EOF'
p = 'ghostframe-render/src/readback.rs'
s = open(p).read()
a = "tight.extend_from_slice(&padded[start..start + unpadded_bytes_per_row as usize]);"
assert s.count(a) == 1
open(p, 'w').write(s.replace(a, "tight.extend_from_slice(&padded[row * unpadded_bytes_per_row as usize..(row + 1) * unpadded_bytes_per_row as usize]);"))
EOF
~/work/gf-gpu-run cargo test -p ghostframe-client-gpu --test tile_renderer 2>&1 | grep -E "^test .*FAILED|test result"
cp /tmp/pr1-parts/readback.rs.orig ghostframe-render/src/readback.rs

echo "== 2. the self-test never compares"
sed -i 's/if got\[o..o + 4\] != \[w\[2\], w\[1\], w\[0\], 0xFF\] {/if false \&\& got[o..o + 4] != [w[2], w[1], w[0], 0xFF] {/' ghostframe-render/src/pipelines/cdf53_grid.rs
git diff --stat ghostframe-render/src/pipelines/cdf53_grid.rs | tail -1
~/work/gf-gpu-run cargo test -p ghostframe-client-gpu --test cdf53_grid the_probe 2>&1 | grep -E "^test |test result"
cp /tmp/pr1-parts/cdf53_grid.rs.orig ghostframe-render/src/pipelines/cdf53_grid.rs

echo "== 3. paint order: an older stateless entry is never superseded"
python3 - <<'EOF'
p = 'ghostframe-render/src/paint_order.rs'
s = open(p).read()
a = "                .is_some_and(|newest| seq_newer(newest, seq))"
assert s.count(a) == 1, 'the ledger line moved; mutate `superseded` by hand'
open(p, 'w').write(s.replace(a, "                .is_some_and(|newest| seq_newer(newest, seq) && false)"))
EOF
~/work/gf-gpu-run cargo test -p ghostframe-client-gpu --test gpu_paint_order --test tile_renderer 2>&1 | grep -E "^test .*FAILED|test result"
cp /tmp/pr1-parts/paint_order.rs.orig ghostframe-render/src/paint_order.rs

echo "== 4. a session reset leaves the palettes alone"
sed -i 's/        self.palrle.clear_palettes(queue);/        let _ = queue;/' ghostframe-render/src/tile_renderer.rs
git diff --stat ghostframe-render/src/tile_renderer.rs | tail -1
~/work/gf-gpu-run cargo test -p ghostframe-client-gpu --test tile_renderer a_session_reset 2>&1 | grep -E "^test |test result"
git checkout -- ghostframe-render/src/tile_renderer.rs
git status --short ghostframe-render | head
```

Expected:

```
== 1. ...
test a_tile_renderer_alone_draws_what_the_native_renderer_draws ... FAILED
test the_async_readback_matches_the_blocking_one_and_strips_row_padding ... FAILED
== 2. ...
test the_probe_rejects_a_shader_that_writes_nothing ... FAILED
test the_probe_rejects_a_shader_that_is_wrong_only_on_the_second_frame ... FAILED
test the_probe_rejects_a_shader_that_draws_nothing_on_the_second_frame ... FAILED
== 3. ...
(gpu_paint_order tests FAILED)
== 4. ...
test a_session_reset_zeroes_the_palettes_and_forgets_the_paint_order ... FAILED
(no modified files under ghostframe-render: everything restored)
```


- [ ] **Step 4: Compare the whole GPU suite with the baseline**

**Only the `--lib` count differs** (this includes `gpu_h264_render` and `gpu_import`, the two targets that drive the H.264 path Task 7 touched). The two failures from Task 1's baseline are still the only failures, with the same names. A different diff is a regression: stop and find it.

```bash
/tmp/pr1-parts/gpu-tests.sh /tmp/pr1-parts/after-gpu.txt > /dev/null
diff /tmp/pr1-parts/baseline-gpu.txt /tmp/pr1-parts/after-gpu.txt
```

Expected:

```
2c2
< test result: ok. 72 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
---
> test result: ok. 37 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

```bash
git add ghostframe-client-gpu/tests/tile_renderer.rs
git status --short | head -30
git commit -q -F - <<'MSG'
test(client-gpu): TileRenderer alone against the native Renderer on a real device

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>
MSG
git log --oneline -1
```


## Task 9: CI, Justfile and docs wiring [haiku]

Run the wasm gate everywhere the other gates run, list the moved test targets where CI names them (CI runs integration tests by explicit name, so a moved `tests/*.rs` is invisible until a workflow lists its target), and document the crate.


- [ ] **Step 1: Apply the edits**

Every edit asserts its anchor is unique. What changes: `client-build-crates` and `client-test-crates` gain `-p ghostframe-render`; a `check-render-wasm` recipe that `ci-local` and `ci-client` both call; `ci.yml`'s clippy job installs the wasm target and runs the same three commands; `client-core.yml` tests and wasm-builds the new crate (its tests need no GPU and no system libraries); `e2e.yml` stops naming `--test coalesce --test dirty` for client-gpu (they live in the new crate now, run by `client-core.yml`); the client-gpu README and DEVELOPERS.md describe the split.

Write `/tmp/pr1-parts/edit_ci.py`:

```python
#!/usr/bin/env python3
"""T8: CI, Justfile, docs and Docker wiring for ghostframe-render. Run from the
repository root."""
def edit(path, pairs):
    s = open(path).read()
    for a, b in pairs:
        assert s.count(a) == 1, (path, s.count(a), a)
        s = s.replace(a, b)
    open(path, 'w').write(s)

# Justfile: the client recipes cover the new crate, and one recipe holds the
# wasm gate so ci-local, ci-client and CI run the same commands.
edit('Justfile', [
    ('client-build-crates := "-p ghostframe-cli -p ghostframe-client-native -p ghostframe-client-gpu ',
     'client-build-crates := "-p ghostframe-cli -p ghostframe-client-native -p ghostframe-client-gpu -p ghostframe-render '),
    ('client-test-crates := "-p ghostframe-cli -p ghostframe-client-native -p ghostframe-client-gpu ',
     'client-test-crates := "-p ghostframe-cli -p ghostframe-client-native -p ghostframe-client-gpu -p ghostframe-render '),
    ('''# Build, lint and test the native client -- the client-only `ci-local`.''',
     '''# The portable tile renderer must keep building for the browser. It shares
# its source with the native client, so a native-only call added to it (a
# blocking wait, a clock, a thread) compiles here and panics in the browser:
# `ci/check-render-portable.sh` catches what the compiler cannot.
# Clippy is `--lib` only, on purpose: the crate's unit tests call the
# native-only `block_on_device`, so a wasm32 test target does not compile.
# Do not "fix" it to `--all-targets`; the host clippy covers the tests.
check-render-wasm:
    cargo check -p ghostframe-render --target wasm32-unknown-unknown
    cargo clippy -p ghostframe-render --target wasm32-unknown-unknown --lib -- -D warnings
    ci/check-render-portable.sh

# Build, lint and test the native client -- the client-only `ci-local`.'''),
    ('''    @echo "=== GStreamer decode backend still compiles ==="
    just check-gstreamer
''', '''    @echo "=== GStreamer decode backend still compiles ==="
    just check-gstreamer
    @echo "=== ghostframe-render still builds for the browser ==="
    just check-render-wasm
'''),
    ('''    cargo clippy -p ghostframe-lib --all-targets --features cdf53-diag -- -D warnings
    @echo "=== unit tests ==="''', '''    cargo clippy -p ghostframe-lib --all-targets --features cdf53-diag -- -D warnings
    @echo "=== ghostframe-render still builds for the browser ==="
    just check-render-wasm
    @echo "=== unit tests ==="'''),
])

# ci.yml: the clippy job installs the wasm target and runs the same gate.
edit('.github/workflows/ci.yml', [(
'''      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
      - uses: Swatinem/rust-cache@v2
        with:
          shared-key: ci-fast
      - run: cargo clippy --workspace --all-targets -- -D warnings
''', '''      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
          targets: wasm32-unknown-unknown
      - uses: Swatinem/rust-cache@v2
        with:
          shared-key: ci-fast
      - run: cargo clippy --workspace --all-targets -- -D warnings
      # ghostframe-render is shared with the browser. --workspace above builds it
      # for the host only; this builds it for wasm32 and checks that nothing
      # native-only (a thread, a blocking wait, a clock) crept into it. Clippy is
      # `--lib` only on purpose: the unit tests call the native-only
      # `block_on_device`, so a wasm32 test target does not compile.
      - run: cargo check -p ghostframe-render --target wasm32-unknown-unknown
      - run: cargo clippy -p ghostframe-render --target wasm32-unknown-unknown --lib -- -D warnings
      - name: ghostframe-render stays portable
        run: ci/check-render-portable.sh
''')])

# client-core.yml: ghostframe-render's tests need no GPU and no system
# libraries (they run on wgpu's no-op backend), and it must build for wasm.
edit('.github/workflows/client-core.yml', [
    ('      - run: cargo test -p ghostframe-protocol -p ghostframe-client-core -p ghostframe-web-ui\n',
     '''      # ghostframe-render: lib tests on wgpu's no-op backend, plus its coalesce
      # and dirty targets (moved from ghostframe-client-gpu). No GPU needed.
      - run: cargo test -p ghostframe-protocol -p ghostframe-client-core -p ghostframe-render -p ghostframe-web-ui
'''),
    ('      - run: cargo build -p ghostframe-protocol -p ghostframe-client-core -p ghostframe-client-net',
     '      - run: cargo build -p ghostframe-protocol -p ghostframe-client-core -p ghostframe-render -p ghostframe-client-net'),
])

# e2e.yml: coalesce and dirty moved out of client-gpu.
edit('.github/workflows/e2e.yml', [
    ("      # --lib matters here beyond coalesce/dirty/cdf53_passes: it is what\n",
     "      # --lib matters here beyond cdf53_passes: it is what\n"),
    ("      # narrow this to `--test coalesce --test dirty --test cdf53_passes`\n",
     "      # narrow this to `--test cdf53_passes`\n"),
    ("      - run: cargo test -p ghostframe-client-gpu --lib --test coalesce --test dirty --test cdf53_passes\n",
     "      - run: cargo test -p ghostframe-client-gpu --lib --test cdf53_passes\n"),
])

edit('ghostframe-client-gpu/README.md', [
    ("| `--test coalesce` | no | yes |\n| `--test dirty` | no | yes |\n", ""),
    ("| `--test gpu_oracle` | yes | no |\n",
     "| `--test gpu_oracle` | yes | no |\n| `--test tile_renderer` | yes | no |\n"),
    ("Run everything locally with `cargo test -p ghostframe-client-gpu`.\n",
     "`coalesce` and `dirty` moved to `ghostframe-render` with the code they test; its\n"
     "`--lib` tests run on wgpu's no-op backend and need no GPU.\n\n"
     "Run everything locally with `cargo test -p ghostframe-client-gpu`.\n"),
])
edit('DEVELOPERS.md', [(
    "| `ghostframe-client-gpu` | Vulkan/wgpu decode, composite and dmabuf export for the native client. |\n",
    "| `ghostframe-render` | The portable tile renderer: framebuffer, Solid / PalRLE / RGBA / CDF 5/3 pipelines, paint order, damage. Builds for the host and for `wasm32-unknown-unknown`; `just check-render-wasm` guards the second. |\n"
    "| `ghostframe-client-gpu` | The native client's renderer: a `ghostframe-render` `TileRenderer` plus the dmabuf export ring, upload slabs and H.264 decode. |\n")])
print("edit_ci: ok")
```

```bash
python3 /tmp/pr1-parts/edit_ci.py
git diff --stat
```

Expected:

```
edit_ci: ok   (then a diffstat of Justfile, three workflows, DEVELOPERS.md, README.md)
```


- [ ] **Step 2: Run the new recipe and the checks that read these files**

That last line is exactly the command `client-core.yml` now runs, so a CI job that cannot find the new crate's tests fails here first. The `ci.yml` unit job's `cargo test --workspace --lib` reaches the new crate's 51 unit tests too.

```bash
just check-render-wasm
ci/check-licenses.py
ci/check-local-paths.sh && echo local-paths-ok
ci/check-env-reads.sh && echo env-reads-ok
cargo test -p ghostframe-protocol -p ghostframe-client-core -p ghostframe-render -p ghostframe-web-ui 2>&1 | grep -E "^test result|FAILED" | head -20
```

Expected:

```
check-render-portable: ok
licences OK: 17 crates, 574 third-party crates
local-paths-ok
env-reads-ok
(every test result ok, the new crate's 51 + 6 + 4 among them)
```


- [ ] **Step 3: Check the YAML still parses**

```bash
python3 -c "
import yaml, sys
for f in ['.github/workflows/ci.yml', '.github/workflows/client-core.yml', '.github/workflows/e2e.yml']:
    yaml.safe_load(open(f)); print('ok', f)
"
```

Expected:

```
ok .github/workflows/ci.yml
ok .github/workflows/client-core.yml
ok .github/workflows/e2e.yml
```

```bash
git add Justfile .github/workflows DEVELOPERS.md ghostframe-client-gpu/README.md
git status --short | head -30
git commit -q -F - <<'MSG'
ci(render): wasm32 check, clippy and portability guard; moved test targets; docs

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>
MSG
git log --oneline -1
```


## Task 10: The gate [sonnet]

Nothing new is written. Run what the spec's gate names, in the order a reviewer would, and read the output instead of trusting the exit code (AGENTS.md: a non-compiling test target vanishes from a test summary; only clippy sees it).


- [ ] **Step 1: The shaders are byte-identical**

The `cdf53_grid` panfrost shaderdb line (about 930 instructions, 8 registers, ~21:57 spills) is a function of `shaders/client/cdf53_reconstruct.wgsl` and the driver. **This host has no panfrost device, so the shaderdb line itself was not measured**; the guarantee is that the shader source is unchanged and `Cdf53Grid` is the same code in a different crate. If a Mali machine is available, run the AGENTS.md check there (`cargo test -p ghostframe-client-gpu --no-default-features --features gles,decode-gstreamer,test-support --release --test cdf53_grid` with `MESA_SHADER_CACHE_DISABLE=true MIDGARD_MESA_DEBUG=shaderdb`) before merging.

```bash
sha256sum -c /tmp/pr1-parts/shaders.sha256 | grep -vc ': OK$' || true
git diff --stat bcc59e03.. -- shaders | tail -1
```

Expected:

```
0
(no output from git diff: nothing under shaders/ changed)
```


- [ ] **Step 2: Native client gate**

Includes fmt, the env-read guard, licences, clippy `--all-targets` over `client-test-crates` (now with the new crate), the client tests, the cbindgen header diff, the GLES and GStreamer compile checks and the new wasm gate.

```bash
just ci-client
```

Expected:

```
=== ci-client passed ===
```


- [ ] **Step 3: Full local gate, through the GPU wrapper (VA-API is live on this host: AGENTS.md)**

Runs `clippy --workspace --all-targets`, `cargo test --workspace --lib` and the release build. (`ci/check-release-features.sh` inside it only checks `ghostframe-lib`'s features and the daemon does not link wgpu, so it says nothing about the new dev-dependency's `noop` feature; the next command does.)

```bash
~/work/gf-gpu-run just ci-local
```

Expected:

```
=== ci-local passed ===
```

Then check that wgpu's `noop` feature, enabled only by the new crate's dev-dependency, is absent from the feature graph of what ships:

```bash
cargo tree -p ghostframe-cli -e features -i wgpu | grep -c noop || true
cargo tree -p ghostframe-server -e features 2>/dev/null | grep -c 'wgpu' || true
```

Expected: `0` for the first (the client binary's wgpu has no `noop`), and `0` for the second (the daemon does not link wgpu at all).

- [ ] **Step 4: Native GPU tests, once more, now including the new target**

`GHOSTFRAME_CDF53_DECODE=cpu` is read in `ghostframe-client-native` (`config::Cdf53Decode`), which this PR does not touch: with it the render thread never calls `enable_cdf53_gpu*`, so every pass takes the CPU branch in `TileRenderer::apply_cdf53_pass`. That branch is exercised by `without_the_grid_cdf53_passes_decode_on_the_cpu` (unit) and by the `cdf53_renderer` tests that run without enabling the grid.

```bash
/tmp/pr1-parts/gpu-tests.sh /tmp/pr1-parts/final-gpu.txt > /dev/null
diff /tmp/pr1-parts/baseline-gpu.txt /tmp/pr1-parts/final-gpu.txt
~/work/gf-gpu-run cargo test -p ghostframe-client-gpu --test tile_renderer 2>&1 | grep "test result"
```

Expected:

```
the same one-line `--lib` difference as in Task 8; then `tile_renderer: 4 passed`
```


- [ ] **Step 5: The test image still builds (the Dockerfile's per-crate COPY)**

The server does not link the new crate, so e2e behaviour cannot change, but the manifest must resolve inside the image or every e2e run fails before it starts. If Docker is unavailable, state so in the PR description rather than skipping silently. An owner-run `ghostframe-e2e --test native_client` on a VKMS host is the end-to-end proof for the native path.

```bash
just containers-build
```

Expected:

```
(builds the ghostframe/test-server image; the cooked-dependency layer is cached, the workspace layer rebuilds)
```


- [ ] **Step 6: The PR is only what it says**

```bash
git log --oneline master..HEAD 2>/dev/null || git log --oneline bcc59e03..HEAD
git diff --stat bcc59e03..HEAD | tail -1
git status --short | grep -v '^??' || echo "tree clean (untracked scratch files are not ours)"
```


## Task 11: Final review [opus]

A reviewer who has not seen the plan reads the diff. Give them the code, not the conclusion; ask them to attack it.


- [ ] **Step 1: Dispatch the review**

Run `superpowers:code-reviewer` (model: opus) over `git diff bcc59e03..HEAD`, with this brief and nothing else:

> This PR extracts `ghostframe-render` from `ghostframe-client-gpu` for a web client that will link it into wasm. Native behaviour must not change. Find regressions. Specifically try to break: (1) the order and timing of dirty marking relative to `ExportRing::plan_publish` (the native client used to mark at `apply_event`; it now drains after each call, and after `record`); (2) the `SlotSink` contract: every slot handed to a flush is retired exactly once, including superseded ones and ones dropped when a resize happens between apply and flush; (3) one submit per flush including the export blit (`gpu_flush_submits`), and where the thread-local submit counter now lives; (4) `FrameDimensions` ordering in `Renderer::apply_frame_dimensions` against the old inline code; (5) anything in `ghostframe-render` that compiles for `wasm32-unknown-unknown` but would panic or hang there (blocking waits, `thread_local`, `Mutex`, `Instant`); (6) `cfg(not(target_arch = "wasm32"))` gaps: items that are unused on wasm, or public API that differs by target; (7) tests that pass while proving nothing: for each new test, say what single-line mutation it catches, and verify by running it; (8) the CI edits: does every moved test target still run somewhere, by name, in a job that runs on every PR?

- [ ] **Step 2: Act on the review**

Each finding is either fixed (with a test that failed first) or answered in the PR description with the reason it is wrong. Re-run `just ci-client` after any fix. Do not merge with an unanswered finding.

---

## Self-review against the spec

| Spec section 5.1 item | Where |
|---|---|
| Moved with history: `framebuffer`, `paint_order`, `dirty`, `coalesce`, five pipelines | Task 3 (`git mv`, one commit) |
| The part of `renderer.rs` that applies tile events and flushes | Task 5 (`tile_renderer.rs`, bodies extracted by script) |
| Tests that cover them move along | Task 3 (`coalesce`, `dirty`, inline unit tests); GPU-bound ones stay in `ghostframe-client-gpu` and run against the re-exported paths, which is what makes them the equivalence gate |
| Depends on wgpu, protocol, client-core, thiserror, tracing; no pollster/libc/ash/egl/ffmpeg/thread | Task 2 manifest; `ci/check-render-portable.sh` (Task 6) |
| No `std::time::Instant`; time as a parameter | `now_us`; calibration moved to client-gpu; guard in Task 6 |
| `TileRenderer::new(device, queue, format, size)` | Task 5, with deviation 1 |
| `apply_event`, `flush`, `framebuffer`, `note_h264`/`h264_stale`, `debug_tile`/`debug_state` (serde) | Task 5 |
| CDF 5/3 self-test and CPU fallback live in the crate | `try_enable_cdf53_gpu`, `probe_grid`, `Cdf53TileState` fallback in `apply_cdf53_pass` |
| `Renderer` keeps its public API and delegates | Task 7 |
| `GHOSTFRAME_CDF53_DECODE=cpu` keeps working | untouched path in `ghostframe-client-native`; Task 10 |
| Shaders stay in `shaders/client/`, `include_str!` from the new crate | same relative depth, so the moved files' paths did not change; Task 10 hash check |
| CI: `cargo check -p ghostframe-render --target wasm32-unknown-unknown` | Task 9 (`ci.yml`, `Justfile`) |
| Gate: native GPU tests, self-test and oracle tests, shaderdb unchanged, clippy | Task 10 |
