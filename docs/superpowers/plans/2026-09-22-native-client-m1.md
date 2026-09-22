# Native Client M1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust library that decodes a ghostframe session on the GPU and publishes it as an exported dmabuf, plus a C ABI over it, with in-process pixel assertions proving the real WGSL renders correctly.

**Architecture:** Four new crates. `ghostframe-client-gpu` builds a Vulkan device by hand (so it can enable the dmabuf export extensions wgpu will not request), hands it up to wgpu via `from_hal`, runs the WGSL decode pipelines into a private framebuffer, and blits damaged regions into a ring of dmabuf-backed export images. `ghostframe-client-native` owns the tsnet transport, a net thread and a render thread, and an eventfd-backed event queue. `ghostframe-client-capi` is a logic-free `extern "C"` shim. `ghostframe-cli` is scaffolded but not implemented until M2.

**Tech Stack:** Rust 1.96.1 (pinned), wgpu + wgpu-hal + naga, ash (Vulkan), `ghostframe-client-core` / `-net` / `-tsnet`, cbindgen, proptest.

**Spec:** `docs/superpowers/specs/2026-09-22-native-client-design.md`

**Branch:** `feature/native-client` (already created; the spec commit is its first commit).

---

## Scope

This plan covers **M1 only**. M2 (CLI, ghostbridge login/logout, showcase window backends) and M3 (VA-API H.264) get their own plans once M1 lands.

M1 ends with `supports_h264 = false`. Do not implement any H.264 path.

---

## Conventions used in this plan

Code blocks show **complete, real code** for anything that is pure logic (modifier
selection, rect coalescing, input encoding, the dirty-history rules) — that code
is meant to be typed in as written, and its tests pin it.

Where a body is several hundred lines of mechanical Vulkan or wgpu setup, the plan
gives the **exact signature, the exact call sequence, and the specific traps**, and
marks the body `todo_impl()`. That is a marker, not a function: replace it with the
implementation described in the surrounding comments. Every such site names the
calls to make and the mistake to avoid, because those are the parts that are hard
to recover independently; the boilerplate around them is not.

If a `todo_impl()` site is ever unclear, the reference implementation is the web
client (`ghostframe-web-client/src/webgpu/*.ts`) — port it rather than re-deriving.

## Key domain facts

Read these before starting. They are not obvious from the code.

1. **A tile is 32x32 pixels** (`ghostframe_protocol::tile::TILE_SIZE`). 1920x1080 is a 60x34 grid = 2040 tiles.

2. **`ClientCore` has two delivery modes**, set by `ClientConfig::tile_delivery`:
   - `TileDelivery::Decoded` emits `Event::TileReady { rgba: Vec<u8> }` — the tile already decoded to RGBA by the Rust CPU decoder.
   - `TileDelivery::Payload` emits `Event::TilePayload { data: TileData, .. }` plus `Event::PaletteUpdated` — undecoded, for a GPU decoder.

   **This is the differential oracle used in Task 13.** The same captured datagrams through both modes must produce identical pixels. That directly tests the "two decoders" concern that motivated this work.

3. **`TileData` variants** (`ghostframe-client-core/src/event.rs`):
   - `Raw(Vec<u8>)` — BGRA wire bytes, unswizzled, <= 4096 bytes, multiple of 4.
   - `Solid([u8; 4])` — one BGRA quad, expanded by the shader to the full tile.
   - `PalRle { palette_id, count, indices }` — `indices` is exactly 512 bytes, two 4-bit palette indices per byte, **low nibble first**.
   - `Cdf53 { pass_idx, bit_planes, present_passes }` — `bit_planes` is exactly 384 bytes, 3 channels x 128, packed **B, G, R**. `present_passes` is `Some` only on pass 0.

4. **Palette colours are BGRA**, matching the wire. The framebuffer is RGBA. The shaders do the swizzle (`bgra.zyxw` in `solid.wgsl`). Do not swizzle twice.

5. **PalRle dispatch shape is `dispatchWorkgroups(num_tiles, 2, 2)`** with `@workgroup_size(16, 16, 1)`. Four workgroups per tile in a 2x2 arrangement. This is deliberate: WebGPU's portable `maxComputeInvocationsPerWorkgroup` is 256, so one workgroup per 32x32 tile (1024 invocations) fails pipeline validation silently on common adapters. Do not "simplify" this.

6. **Cross-GPU dma-buf PRIME import is broken here.** Importing a VKMS-produced dmabuf into a discrete AMD GPU yields stale or scrambled bytes. Test readback must use CPU `mmap` with `DMA_BUF_IOCTL_SYNC`, never a second Vulkan device.

7. **CI names `--test` targets one by one** in `.github/workflows/*.yml`. A new `tests/*.rs` does not run in CI until a workflow names it. This is the mechanism used here to keep GPU-requiring tests out of CI while always running them locally — the skip lives in the workflow, never in `#[ignore]` or a runtime self-skip.

8. **Never `git add -A`.** Stage explicit paths. Screenshots and scratch files live in the repo root.

9. **Local gates must match CI.** `cargo clippy` alone is not green. CI also runs `cargo fmt --all -- --check` and an env-read guard. Run `just ci-local` before declaring a task done.

---

## File Structure

### `ghostframe-client-gpu` (new)

| File | Responsibility |
|---|---|
| `src/lib.rs` | Crate root, re-exports, `GpuError` |
| `src/config.rs` | The only place in the crate that reads env |
| `src/wgpu_ctx.rs` | `WgpuContext`: wgpu device with `VULKAN_EXTERNAL_MEMORY_DMA_BUF`, plus the raw-device escape hatch |
| `src/export.rs` | `ExportedImage`: dmabuf-backed `VkImage`, fd export, plane layout, test-only CPU map |
| `src/ring.rs` | `ExportBuffer` and the export ring, fill policy |
| `src/dirty.rs` | `DirtyGrid`, `DirtyHistory` |
| `src/coalesce.rs` | `Rect`, `coalesce()` |
| `src/framebuffer.rs` | The private persistent framebuffer, preserve-on-resize |
| `src/pipelines/mod.rs` | Pipeline registry |
| `src/pipelines/solid.rs` | Solid render pipeline |
| `src/pipelines/palrle.rs` | PalRle compute pipeline + palette atlas |
| `src/pipelines/cdf53.rs` | Cdf53 integrate + inverse l1/l1_pass2/l2/l3 |
| `src/pipelines/raw.rs` | Raw tile upload |
| `src/renderer.rs` | `Renderer`: ties framebuffer + pipelines + ring + dirty together |
| `tests/coalesce.rs` | Pure-logic tests (**runs in CI**) |
| `tests/dirty.rs` | Pure-logic tests (**runs in CI**) |
| `tests/gpu_export.rs` | Requires a GPU (**not named in CI**) |
| `tests/gpu_pipelines.rs` | Requires a GPU (**not named in CI**) |
| `tests/gpu_oracle.rs` | CPU-vs-GPU differential (**not named in CI**) |

### `ghostframe-client-native` (new)

| File | Responsibility |
|---|---|
| `src/lib.rs` | Crate root, `Client`, `ClientError` |
| `src/config.rs` | The only place in the crate that reads env |
| `src/event.rs` | `ClientEvent`, the queue, the eventfd |
| `src/bootstrap.rs` | `/config.json` fetch and cert-hash pinning |
| `src/net_thread.rs` | epoll loop, `ClientNet` + `ClientCore` ownership |
| `src/render_thread.rs` | Owns `Renderer`, consumes tile work |
| `src/input.rs` | Input push, wrapping `ghostframe_client_core::input` |
| `tests/queue.rs` | Event queue / eventfd (**runs in CI**) |

### `ghostframe-client-capi` (new)

| File | Responsibility |
|---|---|
| `src/lib.rs` | `extern "C"` shim, no logic |
| `src/types.rs` | `#[repr(C)]` structs and enums |
| `build.rs` | cbindgen header generation |
| `cbindgen.toml` | cbindgen config |

### `ghostframe-cli` (new, scaffold only in M1)

| File | Responsibility |
|---|---|
| `src/main.rs` | Arg parsing skeleton; `connect` prints "not implemented until M2" |

### Modified

- `Cargo.toml` — workspace members and pinned dependency versions
- `tests/containers/test-server/Dockerfile` — `COPY` lines for the new members
- `.github/workflows/e2e.yml` — name the CI-safe test targets
- `ghostframe-e2e/tests/native_client.rs` — M1 acceptance test (new, **not named in CI**)
- `ghostframe-e2e/Cargo.toml` — dev-dependency on `ghostframe-client-native`
- `shaders/client/` — WGSL moved here from `ghostframe-web-client/src/webgpu/shaders/`
- `ghostframe-web-client/src/webgpu/*.ts` — updated `?raw` import paths
- `ghostframe-web-client/vite.config.ts` — allow the new shader directory if needed

---

# Phase A — Foundation

## Task 1: Workspace scaffolding

**Files:**
- Modify: `Cargo.toml`
- Create: `ghostframe-client-gpu/Cargo.toml`, `ghostframe-client-gpu/src/lib.rs`
- Create: `ghostframe-client-native/Cargo.toml`, `ghostframe-client-native/src/lib.rs`
- Create: `ghostframe-client-capi/Cargo.toml`, `ghostframe-client-capi/src/lib.rs`
- Create: `ghostframe-cli/Cargo.toml`, `ghostframe-cli/src/main.rs`
- Modify: `tests/containers/test-server/Dockerfile`

- [ ] **Step 1: Verify wgpu builds on the pinned toolchain before committing to a version**

The workspace pins Rust 1.96.1 in `rust-toolchain.toml`. Find the newest wgpu that compiles on it:

```bash
cd /tmp && cargo new --lib wgpu-msrv-probe && cd wgpu-msrv-probe
echo 'wgpu = { version = "27", default-features = false, features = ["vulkan", "wgsl"] }' >> Cargo.toml
echo 'wgpu-hal = { version = "27", default-features = false, features = ["vulkan"] }' >> Cargo.toml
echo 'ash = "0.38"' >> Cargo.toml
rustup run 1.96.1 cargo build 2>&1 | tail -20
```

Expected: builds clean. If it fails on MSRV, step down a major version and retry. **Record the version that worked** — every later task uses it. Do not bump `rust-toolchain.toml`; that is a separate deliberate PR.

- [ ] **Step 2: Add the pinned dependencies to the workspace manifest**

In `Cargo.toml`, under `[workspace.dependencies]`, following the existing comment style (every pin in this file explains itself):

```toml
# wgpu <VERSION> — the newest release that compiles on the pinned 1.96.1
# toolchain (verified 2026-09-22). Vulkan-only: the client's output is a
# dmabuf, so the other backends are dead weight in the shipped library.
wgpu = { version = "=<VERSION>", default-features = false, features = ["vulkan", "wgsl"] }

# wgpu-hal — needed for the escape hatch only: wgpu exposes no dmabuf export,
# so we build the VkDevice ourselves and hand it up via from_hal. Must track
# wgpu's version exactly.
wgpu-hal = { version = "=<VERSION>", default-features = false, features = ["vulkan"] }
```

`ash` is already pinned at the workspace level.

- [ ] **Step 3: Add the four crates to the workspace members list**

In `Cargo.toml`, add to `members`, keeping the existing ordering convention (protocol/tsnet first, clients next, server after):

```toml
    "ghostframe-client-gpu",
    "ghostframe-client-native",
    "ghostframe-client-capi",
    "ghostframe-cli",
```

- [ ] **Step 4: Create the four crate manifests**

`ghostframe-client-gpu/Cargo.toml`:

```toml
[package]
name = "ghostframe-client-gpu"
version = "0.1.0"
edition = "2021"

# GPU decode and dmabuf export for the native client.
#
# Deliberately knows nothing about networking: it takes decoded or
# undecoded tile payloads and produces an exported dmabuf. That boundary
# is what lets the real WGSL be tested with no transport at all, which
# `ghostframe-client-core/tests/oracle_gpu_sparse.rs` explicitly cannot do.

[dependencies]
ghostframe-protocol = { path = "../ghostframe-protocol" }
ghostframe-client-core = { path = "../ghostframe-client-core" }
wgpu = { workspace = true }
wgpu-hal = { workspace = true }
ash = { workspace = true }
libc = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
proptest = "1"
```

`ghostframe-client-native/Cargo.toml`:

```toml
[package]
name = "ghostframe-client-native"
version = "0.1.0"
edition = "2021"

[dependencies]
ghostframe-protocol = { path = "../ghostframe-protocol" }
ghostframe-client-core = { path = "../ghostframe-client-core" }
ghostframe-client-net = { path = "../ghostframe-client-net" }
ghostframe-client-gpu = { path = "../ghostframe-client-gpu" }
ghostframe-tsnet = { path = "../ghostframe-tsnet" }
libc = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
```

`ghostframe-client-capi/Cargo.toml`:

```toml
[package]
name = "ghostframe-client-capi"
version = "0.1.0"
edition = "2021"

[lib]
name = "ghostframe_client"
crate-type = ["cdylib", "staticlib", "rlib"]

[dependencies]
ghostframe-client-native = { path = "../ghostframe-client-native" }
libc = { workspace = true }
tracing = { workspace = true }

[build-dependencies]
cbindgen = { workspace = true }
```

`ghostframe-cli/Cargo.toml`:

```toml
[package]
name = "ghostframe-cli"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "ghostframe"
path = "src/main.rs"

[dependencies]
ghostframe-client-capi = { path = "../ghostframe-client-capi" }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
```

- [ ] **Step 5: Create minimal crate roots so the workspace resolves**

`ghostframe-client-gpu/src/lib.rs`:

```rust
//! GPU decode and dmabuf export for the native ghostframe client.
//!
//! ## Catch-all match arms
//!
//! `clippy::wildcard_enum_match_arm` is on for non-test code, matching
//! `ghostframe-client-core`. A `_ =>` arm that silently absorbs unknown
//! variants has twice hidden real defects in this tree; listing variants
//! makes adding one a compile error at every site that must decide.
#![cfg_attr(not(test), warn(clippy::wildcard_enum_match_arm))]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GpuError {
    #[error("vulkan: {0}")]
    Vulkan(String),
    #[error("no adapter supports the required dmabuf export extensions")]
    NoSuitableAdapter,
    #[error("no DRM format modifier is supported by both this device and the consumer")]
    NoCommonModifier,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
```

`ghostframe-client-native/src/lib.rs` and `ghostframe-client-capi/src/lib.rs`: empty for now (`// Populated in Phase E.`).

`ghostframe-cli/src/main.rs`:

```rust
fn main() {
    eprintln!("ghostframe CLI is implemented in M2; see \
               docs/superpowers/specs/2026-09-22-native-client-design.md");
    std::process::exit(1);
}
```

- [ ] **Step 6: Add the new members to the test-server Dockerfile**

The Dockerfile `COPY`s manifests per crate; a missing member makes `docker buildx` fail before any test runs. In `tests/containers/test-server/Dockerfile`, after the existing `ghostframe-client-net` lines (around line 49), following the same pattern:

```dockerfile
COPY ghostframe-client-gpu/Cargo.toml ghostframe-client-gpu/Cargo.toml
COPY ghostframe-client-gpu/src/ ghostframe-client-gpu/src/
COPY ghostframe-client-native/Cargo.toml ghostframe-client-native/Cargo.toml
COPY ghostframe-client-native/src/ ghostframe-client-native/src/
COPY ghostframe-client-capi/Cargo.toml ghostframe-client-capi/Cargo.toml
COPY ghostframe-client-capi/src/ ghostframe-client-capi/src/
COPY ghostframe-cli/Cargo.toml ghostframe-cli/Cargo.toml
COPY ghostframe-cli/src/ ghostframe-cli/src/
```

The server image never *builds* these crates (`cargo build -p ghostframe-xdaemon` does not compile siblings), but Cargo must parse every workspace manifest, so they have to be present.

Note: `ghostframe-client-capi/build.rs` does not exist yet, so do not `COPY` it. Task 17 adds both the file and its `COPY` line.

- [ ] **Step 7: Verify the workspace resolves and the container still builds**

```bash
cargo metadata --format-version 1 >/dev/null && echo "workspace OK"
just containers-build
```

Expected: both succeed. The container build is the one that catches a forgotten `COPY`, and it fails in a way that looks unrelated, so do not skip it.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock \
  ghostframe-client-gpu/Cargo.toml ghostframe-client-gpu/src/lib.rs \
  ghostframe-client-native/Cargo.toml ghostframe-client-native/src/lib.rs \
  ghostframe-client-capi/Cargo.toml ghostframe-client-capi/src/lib.rs \
  ghostframe-cli/Cargo.toml ghostframe-cli/src/main.rs \
  tests/containers/test-server/Dockerfile
git commit -m "feat(client): scaffold the four native-client crates

Manifests, crate roots and the Dockerfile COPY lines. The server image
never builds these, but Cargo parses every workspace manifest, so a
missing COPY fails the image build before any test runs."
```

---

## Task 2: Acquire a wgpu device with dmabuf export capability

> **REVISED 2026-09-22 after Task 1 landed on wgpu 30.0.1.** The original
> plan assumed wgpu would never enable the dmabuf extensions, so it built the
> `VkInstance`/`VkDevice` by hand and handed them up through `from_hal`. That is
> no longer necessary. wgpu 30 has a first-class feature,
> `wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF` (bit 63), which enables
> `VK_EXT_external_memory_dma_buf`, `VK_EXT_image_drm_format_modifier` and
> `VK_KHR_external_memory_fd` on the device. Verified by reading
> `wgpu-hal-30.0.1/src/vulkan/adapter.rs:1349-1356` and
> `wgpu-types-30.0.1/src/features.rs:1246`.
>
> This removes the milestone's single biggest risk. What it does **not** give us
> is export: wgpu-hal 30 has `texture_from_dmabuf_fd` (import) and **no export
> counterpart**, so Task 3 still writes the export path in ash — but against
> wgpu's own `VkDevice`, reached through `Device::as_hal`, rather than one we
> built. The hand-built `from_hal` route remains a documented fallback if the
> feature turns out to be unavailable on the target hardware.

**Files:**
- Create: `ghostframe-client-gpu/src/wgpu_ctx.rs`
- Create: `ghostframe-client-gpu/tests/gpu_export.rs`
- Modify: `ghostframe-client-gpu/src/lib.rs`

Note there is no longer a `src/vulkan.rs` in this task. The crate's file structure
changes accordingly: drop that row from the plan's table.

- [ ] **Step 1: Write the failing test**

`ghostframe-client-gpu/tests/gpu_export.rs`:

```rust
//! Tests that require a real GPU. Deliberately NOT named in any CI
//! workflow: CI runners have no suitable device, and the project rule is
//! that CI exempts itself in the workflow file rather than the test
//! carrying an #[ignore] that would hide it from developers too.

use ghostframe_client_gpu::wgpu_ctx::WgpuContext;

#[test]
fn wgpu_device_has_dmabuf_export_capability() {
    let ctx = WgpuContext::new().expect("create wgpu context with dmabuf support");

    // The whole point: without this feature the device cannot allocate
    // memory that can leave the process as a dmabuf.
    assert!(
        ctx.device
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF),
        "device lacks VULKAN_EXTERNAL_MEMORY_DMA_BUF"
    );

    // And it must still be a usable wgpu device.
    let buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("spike"),
        size: 256,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&buf, 0, &[0xABu8; 256]);
    ctx.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");

    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    ctx.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    assert_eq!(slice.get_mapped_range()[0], 0xAB);
}

#[test]
fn raw_vulkan_device_is_reachable_for_the_export_path() {
    // Task 3 needs the raw VkDevice to allocate exportable images, because
    // wgpu-hal 30 offers dmabuf IMPORT but no export. Prove the escape
    // hatch is reachable before building on it.
    let ctx = WgpuContext::new().expect("wgpu context");
    let reached = ctx.with_raw_device(|_raw_device, _phys| true);
    assert_eq!(reached, Some(true), "could not reach the raw VkDevice via as_hal");
}
```

Check `poll`'s exact shape against wgpu 30 (`PollType` vs the older `Maintain`);
adjust if the compiler disagrees and note what you used, because the rest of the
plan's test code uses whichever form you settle on.

- [ ] **Step 2: Run it to confirm it fails**

```bash
cargo test -p ghostframe-client-gpu --test gpu_export -- --nocapture
```

Expected: FAIL to compile, `unresolved import ghostframe_client_gpu::wgpu_ctx`.

- [ ] **Step 3: Implement `WgpuContext`**

`ghostframe-client-gpu/src/wgpu_ctx.rs`:

```rust
use crate::GpuError;

/// A wgpu device that can export its images as dmabufs.
///
/// Ordinary wgpu construction: we request
/// `Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF`, which makes wgpu enable
/// VK_EXT_external_memory_dma_buf, VK_EXT_image_drm_format_modifier and
/// VK_KHR_external_memory_fd on the device.
///
/// wgpu-hal 30 can IMPORT a dmabuf (`texture_from_dmabuf_fd`) but cannot
/// export one, so `with_raw_device` exposes the underlying VkDevice for
/// the export path in `export.rs`. That is the only unsafe surface here.
pub struct WgpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl WgpuContext {
    pub fn new() -> Result<Self, GpuError> {
        // NB: wgpu 30 takes the descriptor by value, not by reference.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });

        // Pick an adapter that actually advertises the feature rather than
        // taking the default and failing later: a client that cannot export
        // should refuse to start, not produce an unusable buffer.
        let adapter = pollster_block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .map_err(|_| GpuError::NoSuitableAdapter)?;

        if !adapter
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
        {
            return Err(GpuError::NoSuitableAdapter);
        }

        let (device, queue) = pollster_block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ghostframe-client"),
            required_features: wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF,
            // downlevel_defaults caps maxComputeInvocationsPerWorkgroup at
            // the portable 256 that palrle_decode.wgsl was designed around,
            // so a limit the browser would not have is not silently
            // available here.
            required_limits: wgpu::Limits::downlevel_defaults(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .map_err(|e| GpuError::Vulkan(format!("request_device: {e}")))?;

        Ok(WgpuContext { instance, adapter, device, queue })
    }

    /// Run `f` with the raw Vulkan device and physical device.
    ///
    /// Returns `None` if the backend is not Vulkan. Used only by the
    /// export path, which wgpu does not provide.
    pub fn with_raw_device<R>(
        &self,
        f: impl FnOnce(&ash::Device, ash::vk::PhysicalDevice) -> R,
    ) -> Option<R> { todo_impl() }
}
```

`with_raw_device` uses `Device::as_hal`, whose wgpu 30 signature **returns** the
device rather than taking a closure:

```rust
pub unsafe fn as_hal<A: hal::Api>(&self) -> Option<impl Deref<Target = A::Device>>
```

So the body is:

```rust
        // SAFETY: the returned device must not outlive `self`, and we must
        // never destroy anything wgpu owns -- we only allocate our own images.
        let hal_dev = unsafe { self.device.as_hal::<wgpu_hal::api::Vulkan>() }?;
        Some(f(hal_dev.raw_device(), hal_dev.raw_physical_device()))
```

Accessors verified present on `wgpu_hal::vulkan::Device`
(`wgpu-hal-30.0.1/src/vulkan/device.rs:826-842`): `raw_device() -> &ash::Device`,
`raw_physical_device() -> vk::PhysicalDevice`, `raw_queue() -> vk::Queue`,
`shared_instance()`.

You need a small block-on helper since wgpu's request functions are async and
this crate has no runtime. Either add the `pollster` crate (tiny, no transitive
deps) or write a three-line parking-lot-free executor. Prefer `pollster`, and
pin it in `[workspace.dependencies]` with a comment explaining why, matching the
file's convention. Report which you chose.

- [ ] **Step 4: Wire the module up**

In `ghostframe-client-gpu/src/lib.rs`, add:

```rust
pub mod config;
pub mod wgpu_ctx;
```

- [ ] **Step 5: Run the test**

```bash
cargo test -p ghostframe-client-gpu --test gpu_export -- --nocapture
```

Expected: PASS, both tests.

**If `VULKAN_EXTERNAL_MEMORY_DMA_BUF` is not available on this machine's adapter,
STOP and report** rather than falling back. The fallback (hand-building the
device and using `from_hal`) is a real option but it is a design decision, not an
implementer's call. Include in your report: the adapter name, its backend, and
the full feature set it does advertise.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-client-gpu/src/wgpu_ctx.rs ghostframe-client-gpu/src/lib.rs \
        ghostframe-client-gpu/tests/gpu_export.rs Cargo.toml Cargo.lock
git commit -m "feat(client-gpu): wgpu device with dmabuf export capability

wgpu 30 has a first-class VULKAN_EXTERNAL_MEMORY_DMA_BUF feature that
enables VK_EXT_external_memory_dma_buf, VK_EXT_image_drm_format_modifier
and VK_KHR_external_memory_fd, so the device is built with ordinary wgpu
rather than by hand and handed up through from_hal.

Adapter selection requires the feature up front: a client that cannot
export should refuse to start rather than hand its consumer a buffer it
cannot import.

wgpu-hal 30 can import a dmabuf but not export one, so with_raw_device
exposes the VkDevice for the export path in the next task. That is the
crate's only unsafe surface."
```

---


# Phase B — Export path

## Task 3: `ExportedImage` — a dmabuf-backed VkImage

> **REVISED with Task 2.** All Vulkan calls here run against **wgpu's own
> `VkDevice`**, reached with `WgpuContext::with_raw_device`. This task allocates
> and exports only — it does not create a device. Everywhere the sketch below
> says `vk_ctx`, use the raw device from `with_raw_device`.
>
> **There are two export paths and you must implement both**, selected by
> `WgpuContext::explicit_modifiers`:
>
> - **`true` — explicit modifier path.** Tiling
>   `vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT`, with
>   `VkImageDrmFormatModifierListCreateInfoEXT` narrowed to the negotiated
>   modifier. Read plane layouts with `vkGetImageSubresourceLayout` using the
>   **`MEMORY_PLANE_0_EXT`** aspect (using `COLOR` here returns a zero stride),
>   and confirm the modifier the driver actually chose with
>   `vkGetImageDrmFormatModifierProperties`.
>
> - **`false` — linear path.** `VK_EXT_image_drm_format_modifier` is absent, so
>   there is nothing to negotiate: tiling is `vk::ImageTiling::LINEAR`, the
>   modifier is implicitly `DRM_FORMAT_MOD_LINEAR` (0), and plane layout comes
>   from `vkGetImageSubresourceLayout` with the ordinary **`COLOR`** aspect.
>   This is the pre-modifier dmabuf export path and every consumer can import
>   it.
>
> **The dev machine takes the linear path** (RADV Polaris, Mesa 26.1.7), so that
> is the branch the tests will actually exercise. Write the explicit-modifier
> branch anyway — it is what makes tiled, faster buffers possible on hardware
> that has the extension — but do not let it go untested silently: log which
> path was taken at `info`, and assert the reported modifier in the test.
>
> `GpuError::NoCommonModifier` now carries `{ device: Vec<u64>, requested:
> Vec<u64> }`. On the linear path, a consumer that demands anything other than
> 0 gets that error with `device: vec![0]`.


**Files:**
- Create: `ghostframe-client-gpu/src/export.rs`
- Modify: `ghostframe-client-gpu/tests/gpu_export.rs`
- Modify: `ghostframe-client-gpu/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Append to `ghostframe-client-gpu/tests/gpu_export.rs`:

```rust
use ghostframe_client_gpu::export::ExportedImage;

#[test]
fn exported_image_yields_a_usable_dmabuf_fd_and_layout() {
    let ctx = WgpuContext::new().expect("wgpu context");

    // Empty preference list means "library picks"; it must still succeed.
    let img = ExportedImage::new(&ctx, 256, 128, &[]).expect("export image");

    assert_eq!(img.width, 256);
    assert_eq!(img.height, 128);
    assert!(!img.planes.is_empty(), "no plane layout reported");
    // A dmabuf fd is a real fd; -1 would mean the export silently failed.
    assert!(img.raw_fd() >= 0, "invalid dmabuf fd");
    // Stride must cover the row; a zero stride is the classic symptom of
    // reading the layout off the wrong subresource.
    assert!(
        img.planes[0].stride >= 256 * 4,
        "implausible stride {}",
        img.planes[0].stride
    );
}

#[test]
fn unsatisfiable_modifier_preference_fails_loudly() {
    let ctx = WgpuContext::new().expect("wgpu context");
    // A reserved-invalid modifier no device supports.
    let err = ExportedImage::new(&ctx, 64, 64, &[0x00ff_ffff_ffff_fffe]);
    assert!(
        matches!(err, Err(ghostframe_client_gpu::GpuError::NoCommonModifier)),
        "expected NoCommonModifier, got {err:?}"
    );
}
```

The second test is the one that matters in the field: handing a consumer a buffer it cannot import is the failure mode the spec chose to make loud.

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test -p ghostframe-client-gpu --test gpu_export -- --nocapture
```

Expected: FAIL, `unresolved import ... export`.

- [ ] **Step 3: Implement `ExportedImage`**

`ghostframe-client-gpu/src/export.rs`:

```rust
use std::os::fd::{FromRawFd, OwnedFd, AsRawFd};
use ash::vk;
use crate::{wgpu_ctx::WgpuContext, GpuError};

/// Byte layout of one dmabuf plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneLayout {
    pub offset: u64,
    pub stride: u64,
}

/// A `VkImage` whose memory is exported as a dmabuf.
pub struct ExportedImage {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    pub planes: Vec<PlaneLayout>,
    fd: OwnedFd,
}

impl ExportedImage {
    /// `preferred` is the consumer's modifier list, most-preferred first.
    /// Empty means "library picks", which prefers DRM_FORMAT_MOD_LINEAR
    /// because it is the one every consumer can import.
    pub fn new(
        ctx: &crate::wgpu_ctx::WgpuContext,
        width: u32,
        height: u32,
        preferred: &[u64],
    ) -> Result<Self, GpuError> {
        let supported = Self::supported_modifiers(vk_ctx)?;
        let modifier = Self::choose_modifier(&supported, preferred)
            .ok_or(GpuError::NoCommonModifier)?;
        // 1. create_image with VkExternalMemoryImageCreateInfo
        //    (DMA_BUF_BIT_EXT) + VkImageDrmFormatModifierListCreateInfoEXT
        //    limited to [modifier], tiling DRM_FORMAT_MODIFIER_EXT.
        // 2. allocate_memory with VkExportMemoryAllocateInfo
        //    (DMA_BUF_BIT_EXT) + VkMemoryDedicatedAllocateInfo.
        // 3. bind_image_memory.
        // 4. get_memory_fd (VK_KHR_external_memory_fd) -> OwnedFd.
        // 5. get_image_drm_format_modifier_properties -> actual modifier.
        // 6. get_image_subresource_layout per plane, using aspect
        //    MEMORY_PLANE_0_EXT.. for the DRM-modifier tiling, NOT
        //    COLOR — reading COLOR here is the classic way to get a
        //    zero stride back.
        todo_impl()
    }

    pub fn raw_fd(&self) -> i32 { self.fd.as_raw_fd() }

    /// Modifiers this device supports for RGBA8 with the usages we need.
    fn supported_modifiers(ctx: &WgpuContext) -> Result<Vec<u64>, GpuError> {
        // vkGetPhysicalDeviceFormatProperties2 with
        // VkDrmFormatModifierPropertiesListEXT chained, then filter each
        // by vkGetPhysicalDeviceImageFormatProperties2 with
        // VkPhysicalDeviceImageDrmFormatModifierInfoEXT so we only keep
        // modifiers actually usable with our usage flags.
        todo_impl()
    }

    /// First consumer preference the device also supports; if `preferred`
    /// is empty, LINEAR when available, else the device's first.
    fn choose_modifier(supported: &[u64], preferred: &[u64]) -> Option<u64> {
        const DRM_FORMAT_MOD_LINEAR: u64 = 0;
        if preferred.is_empty() {
            if supported.contains(&DRM_FORMAT_MOD_LINEAR) {
                return Some(DRM_FORMAT_MOD_LINEAR);
            }
            return supported.first().copied();
        }
        preferred.iter().copied().find(|m| supported.contains(m))
    }
}

impl Drop for ExportedImage {
    fn drop(&mut self) {
        // destroy_image then free_memory. The OwnedFd closes itself.
        // Order matters: the image must go before the memory it is bound to.
    }
}
```

Note `choose_modifier` is fully written because it is pure logic with a unit test in Step 5; the Vulkan calls are described precisely enough to implement directly.

- [ ] **Step 4: Add a pure unit test for modifier choice**

At the bottom of `src/export.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_preference_prefers_linear() {
        assert_eq!(ExportedImage::choose_modifier(&[7, 0, 9], &[]), Some(0));
    }

    #[test]
    fn empty_preference_falls_back_to_first_when_no_linear() {
        assert_eq!(ExportedImage::choose_modifier(&[7, 9], &[]), Some(7));
    }

    #[test]
    fn consumer_preference_order_wins_over_device_order() {
        // Device lists 7 first, but the consumer prefers 9.
        assert_eq!(ExportedImage::choose_modifier(&[7, 9], &[9, 7]), Some(9));
    }

    #[test]
    fn no_overlap_is_none() {
        assert_eq!(ExportedImage::choose_modifier(&[7, 9], &[1, 2]), None);
    }
}
```

- [ ] **Step 5: Run both test sets**

```bash
cargo test -p ghostframe-client-gpu --lib
cargo test -p ghostframe-client-gpu --test gpu_export -- --nocapture
```

Expected: both PASS.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-client-gpu/src/export.rs ghostframe-client-gpu/src/lib.rs \
        ghostframe-client-gpu/tests/gpu_export.rs
git commit -m "feat(client-gpu): export a VkImage as a dmabuf with an explicit modifier

Modifier selection is a real negotiation between the device's supported
set and the consumer's preference list, and fails with NoCommonModifier
rather than handing back a buffer the consumer cannot import.

Plane layouts are read with the MEMORY_PLANE_n_EXT aspect, not COLOR;
reading COLOR under DRM-modifier tiling returns a zero stride."
```

---

## Task 4: Wrap the exported image as a `wgpu::Texture`

**Files:**
- Modify: `ghostframe-client-gpu/src/export.rs`
- Modify: `ghostframe-client-gpu/tests/gpu_export.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn exported_image_can_be_used_as_a_wgpu_render_target() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let img = ExportedImage::new(&ctx, 64, 64, &[]).expect("export image");
    let tex = img.as_wgpu_texture(&ctx.device).expect("wrap as wgpu texture");

    assert_eq!(tex.width(), 64);
    assert_eq!(tex.height(), 64);
    assert_eq!(tex.format(), wgpu::TextureFormat::Rgba8Unorm);
}
```

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test -p ghostframe-client-gpu --test gpu_export exported_image_can_be_used -- --nocapture
```

Expected: FAIL, no method `as_wgpu_texture`.

- [ ] **Step 3: Implement the wrap**

Add to `impl ExportedImage`:

```rust
    /// Wrap this image as a wgpu texture so blits are ordinary wgpu.
    ///
    /// The texture borrows the image: `drop_callback` is `Some(noop)` so
    /// wgpu-hal does NOT take ownership of the VkImage. `ExportedImage`
    /// keeps it and destroys it in its own Drop. Handing ownership to
    /// wgpu here would double-free against that Drop.
    pub fn as_wgpu_texture(&self, device: &wgpu::Device)
        -> Result<wgpu::Texture, GpuError>
    {
        let desc = wgpu::TextureDescriptor {
            label: Some("ghostframe-export"),
            size: wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        };

        // SAFETY: self.image was created to match `desc`; the no-op drop
        // callback keeps image ownership with self, and TextureMemory::External
        // tells wgpu-hal the backing memory is not its to free.
        let hal_tex = unsafe {
            device.as_hal::<wgpu_hal::api::Vulkan, _, _>(|hal_dev| {
                let hal_dev = hal_dev.expect("vulkan backend");
                hal_dev.texture_from_raw(
                    self.image,
                    &hal_texture_descriptor(&desc),
                    Some(Box::new(|| {})),
                    wgpu_hal::vulkan::TextureMemory::External,
                )
            })
        };

        // SAFETY: hal_tex was created from this device. The image is
        // freshly created and still in VK_IMAGE_LAYOUT_UNDEFINED, which is
        // exactly what TextureUses::UNINITIALIZED tells wgpu's tracker.
        Ok(unsafe {
            device.create_texture_from_hal::<wgpu_hal::api::Vulkan>(
                hal_tex,
                &desc,
                wgpu::TextureUses::UNINITIALIZED,
            )
        })
    }
```

`hal_texture_descriptor` converts a `wgpu::TextureDescriptor` into the distinct
`wgpu_hal::TextureDescriptor`. They are NOT the same type. The hal one is
(`wgpu-hal-30.0.1/src/lib.rs:2200`):

```rust
pub struct TextureDescriptor<'a> {
    pub label: Label<'a>,
    pub size: wgt::Extent3d,
    pub mip_level_count: u32,
    pub sample_count: u32,
    pub dimension: wgt::TextureDimension,
    pub format: wgt::TextureFormat,
    pub usage: wgt::TextureUses,      // NB: TextureUses, not TextureUsages
    pub memory_flags: MemoryFlags,
    pub view_formats: Vec<wgt::TextureFormat>,  // owned Vec, not a slice
}
```

The `usage` field takes hal-level `TextureUses` bits (`COPY_SRC`/`COPY_DST`),
not the `wgpu::TextureUsages` of the public descriptor, and `view_formats` is an
owned `Vec` rather than a borrowed slice.

The ownership arguments are the important part, and there are two of them in
wgpu-hal 30:

- A `None` **drop callback** makes wgpu-hal destroy the `VkImage`, which then
  double-frees against `ExportedImage::drop`. Pass a no-op callback.
- **`TextureMemory`** is the fourth parameter (new in wgpu-hal 30; older docs show
  a three-argument form). `External` means "memory not owned by wgpu", which is
  exactly our case — the `VkDeviceMemory` was allocated by `export.rs` and is
  freed there. Passing `Dedicated(memory)` would hand wgpu-hal ownership and
  double-free.

Useful to know for M3: wgpu-hal 30 *does* provide `texture_from_dmabuf_fd` for the
**import** direction, but it is **single-plane only**. That is not a problem for
the planned NV12 path, which deliberately imports the same dmabuf twice — plane 0
as `R8Unorm`, plane 1 as `Rg8Unorm` — so each import is single-plane. There is no
export counterpart, which is why this task exists.

- [ ] **Step 4: Run the test**

```bash
cargo test -p ghostframe-client-gpu --test gpu_export -- --nocapture
```

Expected: PASS, and no validation errors printed.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-gpu/src/export.rs ghostframe-client-gpu/tests/gpu_export.rs
git commit -m "feat(client-gpu): wrap an exported dmabuf image as a wgpu texture

Uses a no-op drop callback so wgpu-hal borrows the VkImage instead of
taking ownership; ExportedImage::drop remains the single owner. A None
callback here double-frees."
```

---

## Task 5: Private framebuffer and a full blit into an export image

**Files:**
- Create: `ghostframe-client-gpu/src/framebuffer.rs`
- Modify: `ghostframe-client-gpu/src/export.rs` (test-only CPU map)
- Modify: `ghostframe-client-gpu/tests/gpu_export.rs`

- [ ] **Step 1: Add the test-only CPU readback**

Cross-GPU PRIME import is broken in this tree, so readback must be a CPU `mmap` with an explicit DMA-BUF sync, never a second Vulkan device.

Add to `src/export.rs`:

```rust
    /// Map the dmabuf and copy its bytes out. Test and diagnostic use only.
    ///
    /// Deliberately CPU-mmap rather than importing into a second Vulkan
    /// device: cross-device PRIME import yields stale or scrambled bytes
    /// on this hardware, which previously read as a decode bug.
    pub fn map_read(&self) -> Result<Vec<u8>, GpuError> {
        // DMA_BUF_IOCTL_SYNC with DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ,
        // mmap PROT_READ MAP_SHARED, copy, munmap, then SYNC_END|READ.
        // Skipping the sync ioctls returns plausible-looking stale bytes.
        todo_impl()
    }
```

The `DMA_BUF_IOCTL_SYNC` request code is `_IOW('b', 0, struct dma_buf_sync)` where the struct is a single `u64` flags field; `DMA_BUF_SYNC_READ = 1 << 0`, `DMA_BUF_SYNC_START = 0`, `DMA_BUF_SYNC_END = 1 << 2`.

- [ ] **Step 2: Write the failing test**

```rust
use ghostframe_client_gpu::framebuffer::Framebuffer;

#[test]
fn framebuffer_blits_into_the_exported_dmabuf() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);

    // Paint the whole framebuffer a colour that cannot be confused with
    // zeroed memory or with a channel-order mistake: R, G and B all differ.
    fb.debug_fill(&ctx.device, &ctx.queue, [0x11, 0x22, 0x33, 0xFF]);

    let img = ExportedImage::new(&ctx, 64, 64, &[0]).expect("linear export image");
    let tex = img.as_wgpu_texture(&ctx.device).expect("wrap");
    fb.blit_full(&ctx.device, &ctx.queue, &tex);
    ctx.device.poll(wgpu::Maintain::Wait);

    let bytes = img.map_read().expect("map dmabuf");
    let stride = img.planes[0].stride as usize;

    // Check a pixel away from the origin: an origin-only check passes even
    // when the stride is wrong.
    let off = 40 * stride + 20 * 4;
    assert_eq!(
        &bytes[off..off + 4],
        &[0x11, 0x22, 0x33, 0xFF],
        "wrong pixel at (20,40); channel order or stride is wrong"
    );
}
```

- [ ] **Step 3: Run to confirm it fails**

```bash
cargo test -p ghostframe-client-gpu --test gpu_export framebuffer_blits -- --nocapture
```

Expected: FAIL, unresolved import `framebuffer`.

- [ ] **Step 4: Implement `Framebuffer`**

`ghostframe-client-gpu/src/framebuffer.rs`:

```rust
/// The private persistent framebuffer.
///
/// Never exported. Tiles patch it in place and it is blitted into an
/// export buffer when a frame is published. Mirrors the web client's
/// `Framebuffer` in `ghostframe-web-client/src/webgpu/framebuffer.ts`,
/// including the preserve-on-resize copy: without that copy, tiles
/// written before a late sentinel-driven resize are lost.
pub struct Framebuffer {
    texture: wgpu::Texture,
    pub width: u32,
    pub height: u32,
}

impl Framebuffer {
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self { todo_impl() }

    pub fn texture(&self) -> &wgpu::Texture { &self.texture }

    /// Resize, preserving already-rendered content.
    pub fn resize(&mut self, device: &wgpu::Device, queue: &wgpu::Queue,
                  width: u32, height: u32) { todo_impl() }

    /// Copy the whole framebuffer into `dst`.
    pub fn blit_full(&self, device: &wgpu::Device, queue: &wgpu::Queue,
                     dst: &wgpu::Texture) { todo_impl() }

    /// Copy only `rects` (pixel coordinates) into `dst`.
    pub fn blit_rects(&self, device: &wgpu::Device, queue: &wgpu::Queue,
                      dst: &wgpu::Texture, rects: &[crate::coalesce::Rect]) { todo_impl() }

    /// Fill the whole framebuffer with one RGBA colour. Test use only.
    pub fn debug_fill(&mut self, device: &wgpu::Device, queue: &wgpu::Queue,
                      rgba: [u8; 4]) { todo_impl() }
}
```

Texture usage must be `STORAGE_BINDING | TEXTURE_BINDING | RENDER_ATTACHMENT | COPY_DST | COPY_SRC`, matching `framebuffer.ts` — the compute decoders bind it as a storage texture, the solid pipeline as a render attachment.

`blit_full` and `blit_rects` are `encoder.copy_texture_to_texture` calls submitted on `queue`.

- [ ] **Step 5: Run the test**

```bash
cargo test -p ghostframe-client-gpu --test gpu_export -- --nocapture
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-client-gpu/src/framebuffer.rs ghostframe-client-gpu/src/export.rs \
        ghostframe-client-gpu/src/lib.rs ghostframe-client-gpu/tests/gpu_export.rs
git commit -m "feat(client-gpu): private framebuffer with a full blit to the export dmabuf

Readback in tests is a CPU mmap with explicit DMA_BUF_IOCTL_SYNC, not a
second Vulkan device: cross-device PRIME import returns stale bytes on
this hardware and previously read as a decode bug.

The pixel assertion is deliberately off-origin and uses three distinct
channel values, so a wrong stride or a channel-order slip fails."
```

---

# Phase C — Damage tracking

## Task 6: `DirtyGrid` and `DirtyHistory`

Pure logic, no GPU. **This test target runs in CI.**

**Files:**
- Create: `ghostframe-client-gpu/src/dirty.rs`
- Create: `ghostframe-client-gpu/tests/dirty.rs`

- [ ] **Step 1: Write the failing test**

`ghostframe-client-gpu/tests/dirty.rs`:

```rust
use ghostframe_client_gpu::dirty::{DirtyGrid, DirtyHistory};

#[test]
fn grid_records_and_reports_set_tiles() {
    let mut g = DirtyGrid::new(60, 34);
    assert!(g.is_empty());
    g.set(5, 7);
    g.set(59, 33);
    assert!(!g.is_empty());
    assert!(g.get(5, 7));
    assert!(g.get(59, 33));
    assert!(!g.get(5, 8));
    let mut set: Vec<_> = g.iter_set().collect();
    set.sort_unstable();
    assert_eq!(set, vec![(5, 7), (59, 33)]);
}

#[test]
fn history_unions_every_generation_after_the_buffer_was_filled() {
    let mut h = DirtyHistory::new(4, 4, 8);

    h.current_mut().set(0, 0);
    let g0 = h.advance();
    h.current_mut().set(1, 1);
    let g1 = h.advance();
    h.current_mut().set(2, 2);
    let _g2 = h.advance();

    // A buffer last filled at g0 must receive everything after g0.
    let u = h.union_since(Some(g0)).expect("union");
    assert!(!u.get(0, 0), "generation at or before the fill must be excluded");
    assert!(u.get(1, 1));
    assert!(u.get(2, 2));

    // A buffer filled at g1 sees only what came after.
    let u = h.union_since(Some(g1)).expect("union");
    assert!(!u.get(1, 1));
    assert!(u.get(2, 2));
}

#[test]
fn never_filled_buffer_requests_a_full_blit() {
    let mut h = DirtyHistory::new(4, 4, 8);
    h.current_mut().set(0, 0);
    h.advance();
    // None means "never filled" -> caller must do a full blit.
    assert!(h.union_since(None).is_none());
}

#[test]
fn buffer_older_than_the_ring_requests_a_full_blit() {
    let mut h = DirtyHistory::new(4, 4, 4); // capacity 4
    let old = h.advance();
    for _ in 0..8 {
        h.current_mut().set(1, 1);
        h.advance();
    }
    // `old` has been evicted; we cannot reconstruct the union, so the
    // caller must blit everything rather than silently under-copying.
    assert!(
        h.union_since(Some(old)).is_none(),
        "an evicted generation must force a full blit, not a partial one"
    );
}
```

That last test is the one that protects against the nastiest possible bug here: silently under-copying produces a buffer that looks almost right.

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test -p ghostframe-client-gpu --test dirty
```

Expected: FAIL to compile.

- [ ] **Step 3: Implement**

`ghostframe-client-gpu/src/dirty.rs`:

```rust
/// One bit per tile. 1920x1080 is 60x34 = 2040 bits = 255 bytes, so a
/// generation of history is cheap enough to keep many of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyGrid {
    cols: u32,
    rows: u32,
    words: Vec<u64>,
}

impl DirtyGrid {
    pub fn new(cols: u32, rows: u32) -> Self { todo_impl() }
    pub fn set(&mut self, tx: u32, ty: u32) { todo_impl() }
    pub fn get(&self, tx: u32, ty: u32) -> bool { todo_impl() }
    pub fn is_empty(&self) -> bool { todo_impl() }
    pub fn clear(&mut self) { todo_impl() }
    pub fn union_with(&mut self, other: &DirtyGrid) { todo_impl() }
    pub fn iter_set(&self) -> impl Iterator<Item = (u32, u32)> + '_ { todo_impl() }
    pub fn cols(&self) -> u32 { self.cols }
    pub fn rows(&self) -> u32 { self.rows }
}

/// A bounded ring of per-generation dirty maps.
///
/// Dirty is recorded by the renderer when it writes a tile, NOT derived
/// from `frame_seq`: a CDF 5/3 refinement pass changes the framebuffer
/// without a new frame, so protocol-derived damage would silently drop
/// those updates.
pub struct DirtyHistory {
    ring: Vec<DirtyGrid>,
    /// Generation of the entry currently being accumulated.
    current_gen: u64,
    capacity: usize,
}

impl DirtyHistory {
    pub fn new(cols: u32, rows: u32, capacity: usize) -> Self { todo_impl() }

    /// The map for the generation being accumulated now.
    pub fn current_mut(&mut self) -> &mut DirtyGrid { todo_impl() }

    /// Seal the current generation and start a new one. Returns the
    /// generation just sealed.
    pub fn advance(&mut self) -> u64 { todo_impl() }

    /// Union of every sealed generation strictly after `since`.
    ///
    /// `None` means the caller must do a full blit: either the buffer was
    /// never filled (`since` is `None`) or `since` has been evicted from
    /// the ring. Returning a partial union in the evicted case would
    /// under-copy and leave stale pixels that look almost right.
    pub fn union_since(&self, since: Option<u64>) -> Option<DirtyGrid> { todo_impl() }

    pub fn current_gen(&self) -> u64 { self.current_gen }

    /// Drop all history. Called on resize, when the grid shape changes.
    pub fn reset(&mut self, cols: u32, rows: u32) { todo_impl() }
}
```

- [ ] **Step 4: Run the tests**

```bash
cargo test -p ghostframe-client-gpu --test dirty
```

Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-gpu/src/dirty.rs ghostframe-client-gpu/src/lib.rs \
        ghostframe-client-gpu/tests/dirty.rs
git commit -m "feat(client-gpu): per-generation dirty-tile tracking

union_since returns None for both the never-filled and the evicted case,
forcing a full blit. Returning a partial union when history has been
evicted would under-copy and leave stale pixels that look almost right,
which is far harder to notice than a visibly wrong frame."
```

---

## Task 7: Rect coalescing

**Files:**
- Create: `ghostframe-client-gpu/src/coalesce.rs`
- Create: `ghostframe-client-gpu/tests/coalesce.rs`

**This test target runs in CI.**

- [ ] **Step 1: Write the failing tests, including the invariant proptest**

`ghostframe-client-gpu/tests/coalesce.rs`:

```rust
use ghostframe_client_gpu::coalesce::{coalesce, Rect};
use ghostframe_client_gpu::dirty::DirtyGrid;
use proptest::prelude::*;
use std::collections::HashSet;

#[test]
fn a_horizontal_run_becomes_one_rect() {
    let mut g = DirtyGrid::new(8, 8);
    for x in 2..6 { g.set(x, 3); }
    assert_eq!(coalesce(&g), vec![Rect { x: 2, y: 3, w: 4, h: 1 }]);
}

#[test]
fn stacked_identical_runs_merge_vertically() {
    let mut g = DirtyGrid::new(8, 8);
    for y in 1..4 { for x in 2..6 { g.set(x, y); } }
    assert_eq!(coalesce(&g), vec![Rect { x: 2, y: 1, w: 4, h: 3 }]);
}

#[test]
fn runs_with_different_extents_do_not_merge() {
    let mut g = DirtyGrid::new(8, 8);
    for x in 2..6 { g.set(x, 1); }
    for x in 3..6 { g.set(x, 2); }
    let rects = coalesce(&g);
    assert_eq!(rects.len(), 2, "got {rects:?}");
}

#[test]
fn empty_grid_yields_no_rects() {
    assert!(coalesce(&DirtyGrid::new(8, 8)).is_empty());
}

proptest! {
    /// The invariant that actually matters: the rects must cover exactly
    /// the dirty tiles. Covering too few leaves stale pixels; covering too
    /// many wastes bandwidth and, worse, hands the host a damage region
    /// that claims more changed than did.
    #[test]
    fn rects_cover_exactly_the_dirty_set(
        tiles in prop::collection::hash_set((0u32..16, 0u32..16), 0..80)
    ) {
        let mut g = DirtyGrid::new(16, 16);
        for &(x, y) in &tiles { g.set(x, y); }

        let mut covered: HashSet<(u32, u32)> = HashSet::new();
        for r in coalesce(&g) {
            for y in r.y..r.y + r.h {
                for x in r.x..r.x + r.w {
                    // No rect may overlap another.
                    prop_assert!(covered.insert((x, y)), "rects overlap at ({x},{y})");
                }
            }
        }
        prop_assert_eq!(covered, tiles);
    }
}
```

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test -p ghostframe-client-gpu --test coalesce
```

Expected: FAIL to compile.

- [ ] **Step 3: Implement**

`ghostframe-client-gpu/src/coalesce.rs`:

```rust
use crate::dirty::DirtyGrid;

/// A rectangle in TILE units. Convert to pixels with `to_pixels`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// Scale to pixel coordinates, clamped to the framebuffer. The right
    /// and bottom edges need clamping because the tile grid is a ceil
    /// division: 1080 is 33.75 tiles, so row 33 is only 24 pixels tall.
    pub fn to_pixels(self, fb_width: u32, fb_height: u32) -> Rect {
        const T: u32 = 32;
        let x = self.x * T;
        let y = self.y * T;
        Rect {
            x,
            y,
            w: ((self.w * T).min(fb_width.saturating_sub(x))),
            h: ((self.h * T).min(fb_height.saturating_sub(y))),
        }
    }
}

/// Merge dirty tiles into as few rectangles as possible.
///
/// Two passes: horizontal runs per row, then merge vertically adjacent
/// runs that share an x-extent. Cheap, and good enough on the access
/// pattern ghostframe produces (damage clusters).
pub fn coalesce(grid: &DirtyGrid) -> Vec<Rect> { todo_impl() }
```

Algorithm: for each row, walk columns emitting `(x, w)` runs. Keep `open: Vec<Rect>` of runs still growing. For row `y`, match each new run against an open rect with the same `x` and `w` whose `y + h == y`; if matched, `h += 1`; else close the open rect and start a new one. At the end close everything. Emit in row-major order of the rect origin so the output is deterministic — the equality assertions above depend on it.

- [ ] **Step 4: Run the tests**

```bash
cargo test -p ghostframe-client-gpu --test coalesce
```

Expected: PASS, including 256 proptest cases.

- [ ] **Step 5: Prove the proptest is load-bearing**

A test that passes for the wrong reason is worse than no test. Temporarily break `coalesce` — make the vertical merge accept runs whose `w` differs by one:

```rust
// TEMPORARY, revert immediately
if open.x == run.x && open.w.abs_diff(run.w) <= 1 && open.y + open.h == y {
```

```bash
cargo test -p ghostframe-client-gpu --test coalesce
```

Expected: the proptest FAILS with a covering mismatch. Revert the mutation and confirm it passes again.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-client-gpu/src/coalesce.rs ghostframe-client-gpu/src/lib.rs \
        ghostframe-client-gpu/tests/coalesce.rs
git commit -m "feat(client-gpu): coalesce dirty tiles into damage rectangles

Proptest asserts the rects cover exactly the dirty set with no overlap.
Verified load-bearing by mutating the vertical-merge condition and
confirming the property fails.

to_pixels clamps the right and bottom edges: the tile grid is a ceil
division, so at 1080 the last row is 24 pixels tall, not 32."
```

---

## Task 8: The export ring

**Files:**
- Create: `ghostframe-client-gpu/src/ring.rs`
- Modify: `ghostframe-client-gpu/tests/gpu_export.rs`

- [ ] **Step 1: Write the failing test**

```rust
use ghostframe_client_gpu::ring::ExportRing;

#[test]
fn ring_publishes_partial_updates_and_recycles_buffers() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    let mut ring = ExportRing::new(&ctx, &ctx.device, 64, 64, 3, &[0]).expect("ring");

    // Frame 1: whole surface red. First fill of a fresh buffer is full.
    fb.debug_fill(&ctx.device, &ctx.queue, [0xFF, 0x00, 0x00, 0xFF]);
    ring.mark_dirty_all();
    let a = ring.publish(&ctx.device, &ctx.queue, &fb).expect("publish 1");
    ctx.device.poll(wgpu::Maintain::Wait);
    assert_eq!(&ring.buffer(a.buffer_id).map_read().unwrap()[0..4], &[0xFF, 0, 0, 0xFF]);

    // Frame 2: paint one tile green, publish into a DIFFERENT buffer.
    fb.debug_fill_tile(&ctx.device, &ctx.queue, 1, 0, [0x00, 0xFF, 0x00, 0xFF]);
    ring.mark_dirty(1, 0);
    let b = ring.publish(&ctx.device, &ctx.queue, &fb).expect("publish 2");
    assert_ne!(a.buffer_id, b.buffer_id, "must not reuse a buffer still held");
    ctx.device.poll(wgpu::Maintain::Wait);

    let bytes = ring.buffer(b.buffer_id).map_read().unwrap();
    let stride = ring.buffer(b.buffer_id).planes[0].stride as usize;
    // Tile (1,0) starts at pixel x=32.
    assert_eq!(&bytes[32 * 4..32 * 4 + 4], &[0x00, 0xFF, 0x00, 0xFF], "new tile");
    // And the rest of the buffer must still be the red from frame 1 --
    // this is what proves the partial blit merged with buffer history
    // instead of copying only the damaged rect into a stale buffer.
    assert_eq!(&bytes[0..4], &[0xFF, 0x00, 0x00, 0xFF], "untouched region lost");
    let _ = stride;

    // Release both; the next publish must reuse one.
    ring.release(a.frame_id);
    ring.release(b.frame_id);
    let c = ring.publish(&ctx.device, &ctx.queue, &fb).expect("publish 3");
    assert!(c.buffer_id == a.buffer_id || c.buffer_id == b.buffer_id);
}

#[test]
fn publish_returns_none_when_every_buffer_is_held() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, 64, 64);
    let mut ring = ExportRing::new(&ctx, &ctx.device, 64, 64, 2, &[0]).expect("ring");
    ring.mark_dirty_all();
    assert!(ring.publish(&ctx.device, &ctx.queue, &fb).is_some());
    ring.mark_dirty_all();
    assert!(ring.publish(&ctx.device, &ctx.queue, &fb).is_some());
    ring.mark_dirty_all();
    // Nothing free: the library keeps decoding privately, it does not stall.
    assert!(ring.publish(&ctx.device, &ctx.queue, &fb).is_none());
}
```

The "untouched region lost" assertion is the one that catches the classic partial-blit bug: copying only the current frame's damage into a buffer that was last filled two frames ago.

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test -p ghostframe-client-gpu --test gpu_export ring_ -- --nocapture
```

Expected: FAIL, unresolved import `ring`.

- [ ] **Step 3: Implement**

`ghostframe-client-gpu/src/ring.rs`:

```rust
use crate::{coalesce, dirty::DirtyHistory, export::ExportedImage,
            framebuffer::Framebuffer, wgpu_ctx::WgpuContext, GpuError};

pub struct ExportBuffer {
    pub exported: ExportedImage,
    pub texture: wgpu::Texture,
    /// Generation this buffer's contents correspond to. `None` = never
    /// filled, which forces a full blit.
    pub filled_at_gen: Option<u64>,
    /// Held by the host until released.
    pub in_flight: bool,
}

/// What `publish` hands back.
pub struct PublishedFrame {
    pub frame_id: u32,
    pub buffer_id: u32,
    /// Damage in PIXEL coordinates, ready for the host.
    pub damage: Vec<coalesce::Rect>,
}

pub struct ExportRing {
    buffers: Vec<ExportBuffer>,
    history: DirtyHistory,
    next_frame_id: u32,
    /// frame_id -> buffer index, so release() can find the buffer.
    in_flight: std::collections::HashMap<u32, usize>,
    width: u32,
    height: u32,
}

impl ExportRing {
    pub fn new(ctx: &WgpuContext, device: &wgpu::Device,
               width: u32, height: u32, count: usize,
               preferred_modifiers: &[u64]) -> Result<Self, GpuError> { todo_impl() }

    pub fn mark_dirty(&mut self, tx: u32, ty: u32) { todo_impl() }
    pub fn mark_dirty_all(&mut self) { todo_impl() }
    pub fn buffer(&self, buffer_id: u32) -> &ExportedImage { todo_impl() }

    /// Fill a free buffer from `fb` and hand it out.
    ///
    /// Returns `None` when every buffer is held by the host. The library
    /// keeps decoding into its private framebuffer; nothing stalls and
    /// nothing is lost, because the damage accumulates in the history and
    /// the next released buffer picks up everything since it was written.
    pub fn publish(&mut self, device: &wgpu::Device, queue: &wgpu::Queue,
                   fb: &Framebuffer) -> Option<PublishedFrame> {
        // 1. seal the current generation: gen = history.advance()
        // 2. pick the first buffer with !in_flight; None if there is none
        // 3. rects = match history.union_since(buf.filled_at_gen) {
        //        Some(u) => coalesce(&u),
        //        None    => full-surface rect,   // never filled or evicted
        //    }
        // 4. fb.blit_rects(...) with rects converted to pixels
        // 5. buf.filled_at_gen = Some(gen); buf.in_flight = true
        // 6. device.poll(Wait) -- v1 synchronisation, see spec 5.5
        todo_impl()
    }

    pub fn release(&mut self, frame_id: u32) { todo_impl() }

    /// Reallocate every buffer at the new size and drop all history.
    pub fn resize(&mut self, ctx: &WgpuContext, device: &wgpu::Device,
                  width: u32, height: u32) -> Result<(), GpuError> { todo_impl() }
}
```

- [ ] **Step 4: Run the tests**

```bash
cargo test -p ghostframe-client-gpu --test gpu_export -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-gpu/src/ring.rs ghostframe-client-gpu/src/lib.rs \
        ghostframe-client-gpu/tests/gpu_export.rs
git commit -m "feat(client-gpu): export ring with history-aware partial blits

A buffer is filled with the union of damage since IT was last written,
not since the last frame -- otherwise recycling a two-frame-old buffer
copies only the newest rect and leaves stale pixels. Covered by asserting
an untouched region survives across a partial publish.

publish() returns None when every buffer is held: the library keeps
decoding privately rather than stalling the session on a slow host."
```

---

# Phase D — Decode pipelines

## Task 9: Move the WGSL to a shared location

**Files:**
- Create: `shaders/client/*.wgsl` (moved)
- Modify: `ghostframe-web-client/src/webgpu/*.ts` import paths
- Modify: `ghostframe-web-client/vite.config.ts` if the new path needs allowing

- [ ] **Step 1: Move the shaders with git so history follows**

```bash
mkdir -p shaders/client
git mv ghostframe-web-client/src/webgpu/shaders/*.wgsl shaders/client/
rmdir ghostframe-web-client/src/webgpu/shaders
```

- [ ] **Step 2: Update every `?raw` import**

```bash
grep -rn "shaders/.*\.wgsl?raw" ghostframe-web-client/src/
```

Rewrite each `'./shaders/foo.wgsl?raw'` to `'../../../shaders/client/foo.wgsl?raw'`. Check the depth against each importing file — `src/webgpu/*.ts` is three levels below the repo root.

- [ ] **Step 3: Verify the web client still builds**

```bash
cd ghostframe-web-client && npm run build
```

Expected: build succeeds. **Do not pipe this command** — a piped build reports the pipe's exit status, and a stale `dist/` gets embedded by `//go:embed all:dist` with no error at all.

If vite refuses to read outside its root, add the repo root to `server.fs.allow` in `vite.config.ts`.

- [ ] **Step 4: Commit**

```bash
git add shaders/client ghostframe-web-client/src ghostframe-web-client/vite.config.ts
git commit -m "refactor(shaders): move client WGSL to a shared shaders/client/

The native client and the web client must decode identically, and the
only durable way to guarantee that is one file both compile. Keeping two
copies in sync is a convention; this is structure."
```

---

## Task 10: Solid pipeline

**Files:**
- Create: `ghostframe-client-gpu/src/pipelines/mod.rs`
- Create: `ghostframe-client-gpu/src/pipelines/solid.rs`
- Create: `ghostframe-client-gpu/tests/gpu_pipelines.rs`

- [ ] **Step 1: Write the failing test**

`ghostframe-client-gpu/tests/gpu_pipelines.rs`:

```rust
//! Real-WGSL pipeline tests. Requires a GPU; NOT named in any CI workflow.

use ghostframe_client_gpu::{framebuffer::Framebuffer, pipelines::solid::SolidPipeline,
                            wgpu_ctx::WgpuContext};

fn read_tile_pixel(fb_bytes: &[u8], fb_w: u32, px: u32, py: u32) -> [u8; 4] {
    let off = ((py * fb_w + px) * 4) as usize;
    [fb_bytes[off], fb_bytes[off + 1], fb_bytes[off + 2], fb_bytes[off + 3]]
}

#[test]
fn solid_tile_fills_exactly_its_32x32_region_and_swizzles_bgra_to_rgba() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    fb.debug_fill(&ctx.device, &ctx.queue, [0, 0, 0, 0xFF]);

    let mut pipe = SolidPipeline::new(&ctx.device);
    pipe.set_canvas_size(&ctx.queue, 64, 64);

    // Wire colour is BGRA. B=0x10 G=0x20 R=0x30 -> RGBA 0x30,0x20,0x10.
    pipe.draw(&ctx.device, &ctx.queue, &fb, &[(1u8, 1u8, [0x10, 0x20, 0x30, 0xFF])]);
    ctx.device.poll(wgpu::Maintain::Wait);

    let bytes = fb.debug_read(&ctx.device, &ctx.queue);

    // Inside tile (1,1): pixels 32..63 in both axes.
    assert_eq!(read_tile_pixel(&bytes, 64, 32, 32), [0x30, 0x20, 0x10, 0xFF]);
    assert_eq!(read_tile_pixel(&bytes, 64, 63, 63), [0x30, 0x20, 0x10, 0xFF]);
    // Just outside must be untouched -- catches an off-by-one in the quad.
    assert_eq!(read_tile_pixel(&bytes, 64, 31, 32), [0, 0, 0, 0xFF]);
    assert_eq!(read_tile_pixel(&bytes, 64, 32, 31), [0, 0, 0, 0xFF]);
}
```

The swizzle assertion is deliberate: a BGRA/RGBA mix-up here is exactly the class of bug that produced the historic R-vs-B e2e flakes in this repo.

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test -p ghostframe-client-gpu --test gpu_pipelines -- --nocapture
```

Expected: FAIL to compile.

- [ ] **Step 3: Add `Framebuffer::debug_read`**

```rust
    /// Copy the framebuffer to a staging buffer and read it back as RGBA.
    /// Test and diagnostic use only.
    pub fn debug_read(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> Vec<u8> {
        // copy_texture_to_buffer with bytes_per_row padded to
        // COPY_BYTES_PER_ROW_ALIGNMENT (256), then strip the padding so
        // callers get a tight width*height*4 buffer.
        todo_impl()
    }
```

Stripping the row padding matters: forgetting it produces a buffer that looks right at the origin and is skewed everywhere else.

- [ ] **Step 4: Implement `SolidPipeline`**

`ghostframe-client-gpu/src/pipelines/solid.rs`, porting `ghostframe-web-client/src/webgpu/solid.ts`:

```rust
/// Solid codec: one instanced quad per tile.
///
/// Instance layout is 3 x u32 = 12 bytes: tile_x, tile_y, color_packed,
/// matching `shaders/client/solid.wgsl`'s @location(1..3). The colour is
/// BGRA packed LSB-first; the shader swizzles to RGBA.
pub struct SolidPipeline {
    pipeline: wgpu::RenderPipeline,
    canvas_buf: wgpu::Buffer,
    canvas_bind_group: Option<wgpu::BindGroup>,
    instance_buf: Option<wgpu::Buffer>,
    instance_capacity: usize,
}

impl SolidPipeline {
    pub fn new(device: &wgpu::Device) -> Self { todo_impl() }
    pub fn set_canvas_size(&mut self, queue: &wgpu::Queue, w: u32, h: u32) { todo_impl() }

    /// `tiles` is (tile_x, tile_y, bgra).
    pub fn draw(&mut self, device: &wgpu::Device, queue: &wgpu::Queue,
                fb: &Framebuffer, tiles: &[(u8, u8, [u8; 4])]) { todo_impl() }
}
```

Load the shader with `include_str!("../../../shaders/client/solid.wgsl")`.

The render pass must use `wgpu::LoadOp::Load` for the colour attachment, not `Clear`. The framebuffer is persistent; clearing it would wipe every tile written by a previous codec in the same frame.

- [ ] **Step 5: Run the test**

```bash
cargo test -p ghostframe-client-gpu --test gpu_pipelines -- --nocapture
```

Expected: PASS.

- [ ] **Step 6: Add the Raw-codec upload path**

`Codec::Raw` needs no shader — the payload is BGRA wire bytes for the tile.
Create `ghostframe-client-gpu/src/pipelines/raw.rs`:

```rust
/// Raw codec: BGRA wire bytes uploaded straight into the framebuffer.
///
/// `TileData::Raw` is payload-proportional and may be SHORTER than a full
/// 4096-byte tile (it is only guaranteed to be a multiple of 4). Upload
/// only the rows the payload actually covers; assuming a full tile reads
/// past the end of the slice.
pub fn upload_raw_tile(
    queue: &wgpu::Queue,
    fb: &Framebuffer,
    tile_x: u8,
    tile_y: u8,
    bgra: &[u8],
) { todo_impl() }
```

Swizzle BGRA to RGBA on the CPU before `write_texture` — the framebuffer is
`Rgba8Unorm` and there is no shader in this path to do it.

- [ ] **Step 7: Test it against a short payload**

```rust
#[test]
fn raw_tile_shorter_than_a_full_tile_uploads_only_its_rows() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    fb.debug_fill(&ctx.device, &ctx.queue, [0, 0, 0, 0xFF]);

    // Two rows' worth of BGRA: 2 * 32 * 4 = 256 bytes.
    let bgra = vec![0x10u8, 0x20, 0x30, 0xFF].repeat(64);
    ghostframe_client_gpu::pipelines::raw::upload_raw_tile(&ctx.queue, &fb, 0, 0, &bgra);
    ctx.device.poll(wgpu::Maintain::Wait);

    let bytes = fb.debug_read(&ctx.device, &ctx.queue);
    assert_eq!(read_tile_pixel(&bytes, 64, 0, 0), [0x30, 0x20, 0x10, 0xFF]);
    assert_eq!(read_tile_pixel(&bytes, 64, 0, 1), [0x30, 0x20, 0x10, 0xFF]);
    // Row 2 was not covered by the payload and must be untouched.
    assert_eq!(read_tile_pixel(&bytes, 64, 0, 2), [0, 0, 0, 0xFF]);
}
```

Run: `cargo test -p ghostframe-client-gpu --test gpu_pipelines raw_tile -- --nocapture`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add ghostframe-client-gpu/src/pipelines ghostframe-client-gpu/src/framebuffer.rs \
        ghostframe-client-gpu/src/lib.rs ghostframe-client-gpu/tests/gpu_pipelines.rs
git commit -m "feat(client-gpu): solid-codec pipeline running the real WGSL

Asserts the BGRA-to-RGBA swizzle explicitly and that neighbouring tiles
are untouched. A channel-order slip here is the same class of bug as the
historic R-vs-B e2e flakes.

The render pass loads rather than clears: the framebuffer is persistent
and shared with the other codecs within a frame."
```

---

## Task 11: PalRle pipeline

**Files:**
- Create: `ghostframe-client-gpu/src/pipelines/palrle.rs`
- Modify: `ghostframe-client-gpu/tests/gpu_pipelines.rs`

- [ ] **Step 1: Write the failing test**

```rust
use ghostframe_client_gpu::pipelines::palrle::PalRlePipeline;

#[test]
fn palrle_decodes_low_nibble_first_against_the_palette() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    fb.debug_fill(&ctx.device, &ctx.queue, [0, 0, 0, 0xFF]);

    let mut pipe = PalRlePipeline::new(&ctx.device);

    // Palette 3: slot 0 blue-ish, slot 1 red-ish. Colours are BGRA.
    let mut palette = [[0u8; 4]; 16];
    palette[0] = [0xC0, 0x10, 0x20, 0xFF]; // B=C0 G=10 R=20
    palette[1] = [0x20, 0x10, 0xC0, 0xFF]; // B=20 G=10 R=C0
    pipe.upload_palette(&ctx.queue, 3, &palette);

    // 512 bytes, two 4-bit indices per byte, LOW NIBBLE FIRST.
    // 0x10 => pixel 0 -> slot 0, pixel 1 -> slot 1.
    let indices = vec![0x10u8; 512];
    pipe.decode(&ctx.device, &ctx.queue, &fb, &[(0u8, 0u8, 3u8, indices)]);
    ctx.device.poll(wgpu::Maintain::Wait);

    let bytes = fb.debug_read(&ctx.device, &ctx.queue);
    // Pixel 0 -> slot 0 -> RGBA 0x20,0x10,0xC0
    assert_eq!(read_tile_pixel(&bytes, 64, 0, 0), [0x20, 0x10, 0xC0, 0xFF]);
    // Pixel 1 -> slot 1 -> RGBA 0xC0,0x10,0x20
    assert_eq!(read_tile_pixel(&bytes, 64, 1, 0), [0xC0, 0x10, 0x20, 0xFF]);
}
```

Nibble order is asserted directly because getting it backwards produces a plausible-looking image with the palette transposed, which survives a casual eyeball check.

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test -p ghostframe-client-gpu --test gpu_pipelines palrle -- --nocapture
```

Expected: FAIL to compile.

- [ ] **Step 3: Implement**

`ghostframe-client-gpu/src/pipelines/palrle.rs`, porting `ghostframe-web-client/src/webgpu/palrle.ts`:

```rust
/// PalRle compute decode.
///
/// Bindings match `shaders/client/palrle_decode.wgsl`:
///   0 palette_atlas : array<u32, 4096>   (256 palettes x 16 slots)
///   1 tile_work     : array<TileWork>
///   2 indices_buf   : array<u32>
///   3 framebuffer   : texture_storage_2d<rgba8unorm, write>
///   4 errors        : array<atomic<u32>>
///
/// Dispatch is (num_tiles, 2, 2) with @workgroup_size(16,16,1) -- four
/// workgroups per tile. This is NOT an arbitrary choice: WebGPU's
/// portable maxComputeInvocationsPerWorkgroup is 256, so one workgroup
/// per 32x32 tile would fail pipeline validation on common adapters and
/// write no pixels at all.
pub struct PalRlePipeline { /* ... */ }

impl PalRlePipeline {
    pub fn new(device: &wgpu::Device) -> Self { todo_impl() }

    /// `colors` is BGRA, matching the wire and `Event::PaletteUpdated`.
    pub fn upload_palette(&mut self, queue: &wgpu::Queue,
                          palette_id: u8, colors: &[[u8; 4]; 16]) { todo_impl() }

    /// `tiles` is (tile_x, tile_y, palette_id, indices) where `indices`
    /// is exactly 512 bytes.
    pub fn decode(&mut self, device: &wgpu::Device, queue: &wgpu::Queue,
                  fb: &Framebuffer,
                  tiles: &[(u8, u8, u8, Vec<u8>)]) { todo_impl() }
}
```

`TileWork` is 8 x u32 = 32 bytes: `tile_x, tile_y, palette_id, count, payload_off, _pad0, _pad1, _pad2`. Keep the padding; the shader's array stride depends on it.

- [ ] **Step 4: Run the test**

```bash
cargo test -p ghostframe-client-gpu --test gpu_pipelines -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-gpu/src/pipelines/palrle.rs ghostframe-client-gpu/src/pipelines/mod.rs \
        ghostframe-client-gpu/tests/gpu_pipelines.rs
git commit -m "feat(client-gpu): PalRle compute pipeline running the real WGSL

Asserts low-nibble-first index order directly: reversing it yields a
plausible image with the palette transposed, which survives an eyeball
check and would only surface as a subtle colour bug much later.

Keeps the 4-workgroups-per-tile dispatch. One workgroup per 32x32 tile
is 1024 invocations and fails validation on adapters reporting the
portable 256 limit, writing no pixels at all."
```

---

## Task 12: Cdf53 pipelines

**Files:**
- Create: `ghostframe-client-gpu/src/pipelines/cdf53.rs`
- Modify: `ghostframe-client-gpu/tests/gpu_pipelines.rs`

- [ ] **Step 1: Read the existing reference implementations first**

Before writing anything:

```bash
sed -n '1,120p' ghostframe-web-client/src/webgpu/cdf53.ts
sed -n '1,80p' ghostframe-client-core/tests/oracle_gpu_sparse.rs
```

`oracle_gpu_sparse.rs`'s header explains the sparse-K rule and the two-decoder problem better than anything else in the tree. The rule that matters: `computePassesProcessed` does `resolved = receivedMask | ~present`, so a pass skipped because its bit-plane was all zero counts as *known zero*, not *not yet arrived*. Getting this wrong adds a midpoint correction that should not be there — measured at 16/255 per channel on flat content.

- [ ] **Step 2: Write the failing test**

```rust
use ghostframe_client_gpu::pipelines::cdf53::Cdf53Pipeline;

#[test]
fn cdf53_pass_zero_of_a_flat_tile_reconstructs_that_flat_colour() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    fb.debug_fill(&ctx.device, &ctx.queue, [0, 0, 0, 0xFF]);

    let mut pipe = Cdf53Pipeline::new(&ctx.device);

    // A flat mid-grey tile: only the DC coefficient is non-zero, so
    // present_passes has a single bit and pass 0 is the whole tile.
    let (bit_planes, present_passes) = ghostframe_client_gpu::pipelines::cdf53::
        test_support::flat_tile_pass0([0x80, 0x80, 0x80]);

    pipe.integrate(&ctx.device, &ctx.queue, &[(0u8, 0u8, 0u8, bit_planes, Some(present_passes))]);
    pipe.inverse(&ctx.device, &ctx.queue, &fb, &[(0u8, 0u8)]);
    ctx.device.poll(wgpu::Maintain::Wait);

    let bytes = fb.debug_read(&ctx.device, &ctx.queue);
    let px = read_tile_pixel(&bytes, 64, 4, 4);
    for c in 0..3 {
        assert!(
            (px[c] as i32 - 0x80).abs() <= 2,
            "channel {c} reconstructed as {} not ~0x80; full pixel {px:?}",
            px[c]
        );
    }
}

#[test]
fn skipped_trailing_plane_counts_as_known_zero_not_missing() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    let mut pipe = Cdf53Pipeline::new(&ctx.device);

    // present_passes with a gap: plane 2 is absent because it was all
    // zero. The shader must treat it as resolved, NOT apply a midpoint
    // correction. Without `resolved = received | ~present` this is off by
    // roughly 16/255 on flat content.
    let (planes, present) = ghostframe_client_gpu::pipelines::cdf53::
        test_support::flat_tile_with_skipped_plane([0x80, 0x80, 0x80], 2);

    pipe.integrate(&ctx.device, &ctx.queue, &[(0u8, 0u8, 0u8, planes, Some(present))]);
    pipe.inverse(&ctx.device, &ctx.queue, &fb, &[(0u8, 0u8)]);
    ctx.device.poll(wgpu::Maintain::Wait);

    let px = read_tile_pixel(&fb.debug_read(&ctx.device, &ctx.queue), 64, 4, 4);
    assert!(
        (px[0] as i32 - 0x80).abs() <= 2,
        "skipped plane treated as missing: got {} expected ~0x80", px[0]
    );
}
```

- [ ] **Step 3: Run to confirm it fails**

```bash
cargo test -p ghostframe-client-gpu --test gpu_pipelines cdf53 -- --nocapture
```

Expected: FAIL to compile.

- [ ] **Step 4: Implement**

`ghostframe-client-gpu/src/pipelines/cdf53.rs`. Five compute pipelines from `shaders/client/`: `cdf53_integrate.wgsl`, `cdf53_inverse_l1.wgsl`, `cdf53_inverse_l1_pass2.wgsl`, `cdf53_inverse_l2.wgsl`, `cdf53_inverse_l3.wgsl`. Port the binding layouts and dispatch shapes from `ghostframe-web-client/src/webgpu/cdf53.ts` exactly — do not re-derive them.

```rust
pub struct Cdf53Pipeline { /* five pipelines + coefficient/state buffers */ }

impl Cdf53Pipeline {
    pub fn new(device: &wgpu::Device) -> Self { todo_impl() }

    /// `tiles` is (tile_x, tile_y, pass_idx, bit_planes, present_passes).
    /// `bit_planes` is exactly 384 bytes: 3 channels x 128, packed B,G,R.
    /// `present_passes` is Some only on pass 0.
    pub fn integrate(&mut self, device: &wgpu::Device, queue: &wgpu::Queue,
                     tiles: &[(u8, u8, u8, Vec<u8>, Option<u16>)]) { todo_impl() }

    /// Run the three inverse levels for the listed tiles into `fb`.
    pub fn inverse(&mut self, device: &wgpu::Device, queue: &wgpu::Queue,
                   fb: &Framebuffer, tiles: &[(u8, u8)]) { todo_impl() }
}

/// Fixtures that build wire-shaped Cdf53 payloads. Not `#[cfg(test)]`
/// because the integration tests in tests/ are separate crates.
pub mod test_support {
    /// Pass-0 bit planes for a uniform tile of `rgb`, plus its
    /// present_passes bitmap.
    pub fn flat_tile_pass0(rgb: [u8; 3]) -> (Vec<u8>, u16) { todo_impl() }

    /// Same, but with `skip` absent from present_passes to exercise the
    /// known-zero rule.
    pub fn flat_tile_with_skipped_plane(rgb: [u8; 3], skip: u8) -> (Vec<u8>, u16) { todo_impl() }
}
```

Build the fixtures using `ghostframe_client_core::cdf53_tile_state` and the CPU encoder so they are wire-accurate rather than hand-assembled.

- [ ] **Step 5: Run the tests**

```bash
cargo test -p ghostframe-client-gpu --test gpu_pipelines -- --nocapture
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-client-gpu/src/pipelines/cdf53.rs ghostframe-client-gpu/src/pipelines/mod.rs \
        ghostframe-client-gpu/tests/gpu_pipelines.rs
git commit -m "feat(client-gpu): Cdf53 integrate and inverse pipelines on the real WGSL

Includes a direct test of the sparse-K rule: a trailing bit-plane omitted
because it was all zero must count as known-zero, not as not-yet-arrived.
Getting that wrong adds a midpoint correction worth about 16/255 per
channel on flat content, which is exactly the kind of drift that is
invisible until someone measures it."
```

---

## Task 13: CPU-vs-GPU differential oracle

This is the test the whole milestone exists to make possible.

**Files:**
- Create: `ghostframe-client-gpu/tests/gpu_oracle.rs`

- [ ] **Step 1: Write the test**

```rust
//! The two-decoder oracle.
//!
//! `ClientCore` can deliver the same datagrams either CPU-decoded
//! (TileDelivery::Decoded -> Event::TileReady) or raw for a GPU decoder
//! (TileDelivery::Payload -> Event::TilePayload). Feeding one capture
//! through both and comparing pixels is a direct test that the Rust
//! decoder and the WGSL decoder agree.
//!
//! Requires a GPU; NOT named in any CI workflow.

use ghostframe_client_core::{ClientConfig, ClientCore, Event, TileDelivery};
use ghostframe_client_gpu::{framebuffer::Framebuffer, renderer::Renderer,
                            wgpu_ctx::WgpuContext};

/// Replay `datagrams` through a core in the given delivery mode.
fn drain(datagrams: &[Vec<u8>], delivery: TileDelivery) -> Vec<Event> {
    let mut core = ClientCore::new(
        ClientConfig { indices_raw_enabled: false, supports_h264: false, tile_delivery: delivery },
        0,
    );
    let mut events = Vec::new();
    for (i, d) in datagrams.iter().enumerate() {
        events.extend(core.handle_datagram(d, i as u64 * 1_000));
    }
    events
}

#[test]
fn gpu_decode_matches_cpu_decode_for_solid_palrle_and_cdf53() {
    let capture = ghostframe_client_gpu::testdata::mixed_codec_capture();

    // CPU reference.
    let mut expected = std::collections::HashMap::new();
    for ev in drain(&capture, TileDelivery::Decoded) {
        if let Event::TileReady { tile_x, tile_y, rgba, .. } = ev {
            expected.insert((tile_x, tile_y), rgba);
        }
    }
    assert!(!expected.is_empty(), "capture produced no decoded tiles");

    // GPU path.
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut renderer = Renderer::new(&ctx, 64, 64).expect("renderer");
    for ev in drain(&capture, TileDelivery::Payload) {
        renderer.apply_event(&ctx, &ev);
    }
    renderer.flush(&ctx);
    let fb = renderer.debug_read_framebuffer(&ctx);

    for ((tx, ty), cpu_rgba) in &expected {
        for i in 0..1024usize {
            let (px, py) = (*tx as u32 * 32 + (i as u32 % 32),
                            *ty as u32 * 32 + (i as u32 / 32));
            let off = ((py * 64 + px) * 4) as usize;
            let gpu = &fb[off..off + 4];
            let cpu = &cpu_rgba[i * 4..i * 4 + 4];
            // Cdf53 reconstruction differs in the last bit between the
            // integer CPU path and the shader's float math; 2/255 is the
            // tolerance, NOT a licence for a systematic offset.
            for c in 0..4 {
                assert!(
                    (gpu[c] as i32 - cpu[c] as i32).abs() <= 2,
                    "tile ({tx},{ty}) pixel {i} channel {c}: gpu {} cpu {}",
                    gpu[c], cpu[c]
                );
            }
        }
    }
}
```

- [ ] **Step 2: Build the capture fixture**

Add `ghostframe-client-gpu/src/testdata.rs`, exposed from `lib.rs`. It must produce datagrams for at least one Solid tile, one PalRle tile with a palette upsert, and one Cdf53 tile with two passes. Build them with `ghostframe_protocol`'s encoders so they are wire-accurate; do not hand-assemble bytes.

Check whether `ghostframe-e2e/src/harness/scene_tiles.rs` already emits something reusable before writing new fixtures.

- [ ] **Step 3: Implement `Renderer`**

`ghostframe-client-gpu/src/renderer.rs` — the façade that ties everything together:

```rust
/// Owns the framebuffer, the pipelines, the export ring and the dirty
/// history. The only type `ghostframe-client-native` talks to.
pub struct Renderer { /* ... */ }

impl Renderer {
    pub fn new(ctx: &WgpuContext, width: u32, height: u32) -> Result<Self, GpuError> { todo_impl() }

    /// Route one `ClientCore` event to the right pipeline and mark the
    /// tile dirty. Batches within a frame; `flush` submits.
    pub fn apply_event(&mut self, ctx: &WgpuContext,
                       ev: &ghostframe_client_core::Event) { todo_impl() }

    /// Submit all batched work.
    pub fn flush(&mut self, ctx: &WgpuContext) { todo_impl() }

    /// Seal a generation and hand out an export buffer, if one is free.
    pub fn publish(&mut self, ctx: &WgpuContext)
        -> Option<crate::ring::PublishedFrame> { todo_impl() }

    pub fn release(&mut self, frame_id: u32) { todo_impl() }

    pub fn debug_read_framebuffer(&self, ctx: &WgpuContext) -> Vec<u8> { todo_impl() }
}
```

`apply_event` must handle every `Event` variant explicitly — the crate warns on `wildcard_enum_match_arm`, and that lint exists here because a catch-all arm hid a wrong classifier for three months.

- [ ] **Step 4: Run the oracle**

```bash
cargo test -p ghostframe-client-gpu --test gpu_oracle -- --nocapture
```

Expected: PASS.

If a codec disagrees, **the oracle has found a real bug — investigate before adjusting the tolerance.** Widening the tolerance to make it pass is how the divergence this milestone exists to find would get buried.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-gpu/src/renderer.rs ghostframe-client-gpu/src/testdata.rs \
        ghostframe-client-gpu/src/lib.rs ghostframe-client-gpu/tests/gpu_oracle.rs
git commit -m "test(client-gpu): differential oracle between the CPU and GPU decoders

ClientCore can deliver the same datagrams CPU-decoded or raw, so one
capture through both modes compares the Rust decoder against the real
WGSL directly. This is what oracle_gpu_sparse.rs could not do -- it
reimplements the shader's arithmetic in Rust and says so in its header.

Tolerance is 2/255 for Cdf53 float-vs-integer reconstruction. It is not
licence for a systematic offset: a disagreement is a bug to investigate,
not a number to raise."
```

---

# Phase E — Orchestration and the C API

## Task 14: Event queue and eventfd

**Files:**
- Create: `ghostframe-client-native/src/event.rs`
- Create: `ghostframe-client-native/tests/queue.rs`

**This test target runs in CI** (no GPU needed).

- [ ] **Step 1: Write the failing test**

`ghostframe-client-native/tests/queue.rs`:

```rust
use ghostframe_client_native::event::{ClientEvent, EventQueue};

#[test]
fn queue_signals_its_fd_and_drains_in_order() {
    let q = EventQueue::new().expect("queue");
    assert!(q.pop().is_none());

    q.push(ClientEvent::Connected);
    q.push(ClientEvent::Resized { width: 800, height: 600 });

    // The fd must be readable now, so a host poll() wakes.
    assert!(fd_is_readable(q.as_raw_fd()), "eventfd not signalled");

    assert!(matches!(q.pop(), Some(ClientEvent::Connected)));
    assert!(matches!(q.pop(), Some(ClientEvent::Resized { width: 800, .. })));
    assert!(q.pop().is_none());
}

#[test]
fn fd_clears_once_the_queue_is_drained() {
    let q = EventQueue::new().expect("queue");
    q.push(ClientEvent::Connected);
    while q.pop().is_some() {}
    assert!(
        !fd_is_readable(q.as_raw_fd()),
        "fd still readable after drain; the host would spin at 100% CPU"
    );
}

fn fd_is_readable(fd: std::os::fd::RawFd) -> bool {
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: pfd is a valid single-element array for the duration.
    unsafe { libc::poll(&mut pfd, 1, 0) > 0 }
}
```

The second test is not decoration: an eventfd that never clears makes every host's main loop spin, and it is easy to get wrong with `EFD_SEMAPHORE`.

- [ ] **Step 2: Run to confirm it fails**

```bash
cargo test -p ghostframe-client-native --test queue
```

Expected: FAIL to compile.

- [ ] **Step 3: Implement**

`ghostframe-client-native/src/event.rs`:

```rust
/// Events handed to the host. Mirrors the C `gf_event` in the capi crate.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    Connected,
    Disconnected { reason: String },
    Resized { width: u32, height: u32 },
    FrameReady { frame_id: u32 },
    Error { message: String },
}

/// A thread-safe queue whose eventfd the host can put in its own poll set.
///
/// Created with EFD_NONBLOCK and no EFD_SEMAPHORE: one read drains the
/// whole counter. The fd is signalled while the queue is non-empty and
/// cleared once it is drained -- an fd that stays readable makes the
/// host's main loop spin at 100% CPU.
pub struct EventQueue { /* Mutex<VecDeque<ClientEvent>> + OwnedFd */ }

impl EventQueue {
    pub fn new() -> std::io::Result<Self> { todo_impl() }
    pub fn push(&self, ev: ClientEvent) { todo_impl() }
    pub fn pop(&self) -> Option<ClientEvent> { todo_impl() }
    pub fn as_raw_fd(&self) -> std::os::fd::RawFd { todo_impl() }
}
```

- [ ] **Step 4: Run the tests**

```bash
cargo test -p ghostframe-client-native --test queue
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-native/src/event.rs ghostframe-client-native/src/lib.rs \
        ghostframe-client-native/tests/queue.rs
git commit -m "feat(client-native): eventfd-backed event queue

Asserts the fd clears once drained. An eventfd that stays readable makes
every host main loop spin at 100% CPU, and EFD_SEMAPHORE gets this wrong
in a way that is invisible to a queue-contents test."
```

---

## Task 15: Bootstrap, net thread and render thread

**Files:**
- Create: `ghostframe-client-native/src/bootstrap.rs`
- Create: `ghostframe-client-native/src/net_thread.rs`
- Create: `ghostframe-client-native/src/render_thread.rs`
- Modify: `ghostframe-client-native/src/lib.rs`

- [ ] **Step 1: Write the bootstrap parser test**

Parsing is separable from I/O and is the part that can be wrong silently. In `src/bootstrap.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cert_hash_from_config_json() {
        let body = r#"{"certHash":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"}"#;
        let h = parse_cert_hash(body).expect("parse");
        assert_eq!(h[0], 0x00);
        assert_eq!(h[31], 0xff);
    }

    #[test]
    fn rejects_a_hash_of_the_wrong_length() {
        assert!(parse_cert_hash(r#"{"certHash":"00112233"}"#).is_err());
    }

    #[test]
    fn rejects_non_hex() {
        assert!(parse_cert_hash(r#"{"certHash":"zz"}"#).is_err());
    }
}
```

A short or non-hex hash must fail loudly: silently accepting one would disable certificate pinning, which is the property protecting the session.

- [ ] **Step 2: Implement bootstrap**

```rust
/// Fetch /config.json over the tailnet and pin the server certificate.
///
/// Mirrors `ghostframe-web-client/src/bootstrap.ts`. Uses
/// GhostbridgeHandle::dial_tcp: there is deliberately no direct-socket
/// path anywhere in this crate.
pub fn fetch_cert_hash(
    bridge: &ghostframe_tsnet::GhostbridgeHandle,
    host: &str,
    port: u16,
) -> Result<[u8; 32], crate::ClientError> { todo_impl() }

fn parse_cert_hash(body: &str) -> Result<[u8; 32], crate::ClientError> { todo_impl() }
```

Write a minimal HTTP/1.1 GET over the returned fd; do not add an HTTP client dependency for one request.

- [ ] **Step 3: Implement the net thread**

`src/net_thread.rs`:

```rust
/// Owns ClientNet + ClientCore and all transport timing.
///
/// Separate from the render thread so GPU stalls -- including the v1
/// poll(Wait) in ExportRing::publish -- never land inside ACK/NACK
/// timing. Much of the M3 codec stack is timing-sensitive.
///
/// epoll set: the tsnet UDP fd, a timerfd armed from
/// ClientCore::poll_timeout, and a wake eventfd for shutdown and input.
pub fn run(/* ... */) { todo_impl() }
```

Loop: wait on epoll; on UDP readable, read and feed `ClientNet`; drain `ClientNet` events into `ClientCore`; drain `ClientCore::poll_transmit` back out; forward tile events to the render thread's channel; rearm the timerfd from `poll_timeout`; on timer expiry call `on_timeout`.

Use `std::thread` and `std::sync::mpsc`, not tokio. A tokio `Runtime` dropped while spawned tasks are live is a known footgun in this tree's FFI paths.

- [ ] **Step 4: Implement the render thread**

`src/render_thread.rs`: owns `WgpuContext` and `Renderer`, consumes tile events from the channel, calls `apply_event`, and on a frame boundary `flush` then `publish`, pushing `ClientEvent::FrameReady` into the queue when `publish` returns `Some`.

- [ ] **Step 5: Wire up `Client` in `lib.rs`**

```rust
pub struct Client { /* queue, threads, renderer handle, bridge */ }

impl Client {
    pub fn new(config: Config) -> Result<Self, ClientError> { todo_impl() }
    pub fn connect(&mut self, host: &str, port: u16) -> Result<(), ClientError> { todo_impl() }
    pub fn disconnect(&mut self) -> Result<(), ClientError> { todo_impl() }
    pub fn event_fd(&self) -> std::os::fd::RawFd { todo_impl() }
    pub fn next_event(&self) -> Option<event::ClientEvent> { todo_impl() }
    pub fn acquire_frame(&mut self) -> Option<ring::PublishedFrame> { todo_impl() }
    pub fn release_frame(&mut self, frame_id: u32) { todo_impl() }
}
```

- [ ] **Step 6: Run the full crate test suite and the local gates**

```bash
cargo test -p ghostframe-client-native
just ci-local
```

Expected: PASS and green.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-client-native/src
git commit -m "feat(client-native): bootstrap, net thread and render thread

Cert-hash parsing rejects short and non-hex values loudly; silently
accepting one would disable the pinning that protects the session.

Two threads rather than one: GPU stalls, including the v1 poll(Wait) on
publish, must not land inside ACK/NACK timing. Plain epoll and std
threads rather than tokio, whose Runtime-drop-with-live-tasks behaviour
has bitten this tree's FFI paths before."
```

---

## Task 16: Input push

**Files:**
- Create: `ghostframe-client-native/src/input.rs`

- [ ] **Step 1: Write the failing test**

In `src/input.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_events_are_encoded_as_x11_keysyms_on_the_wire() {
        // XK_a is 0x61.
        assert_eq!(encode_key(0x61, true), vec![0x05, 0x04, 0x00, 0x00, 0x00, 0x61]);
        assert_eq!(encode_key(0x61, false), vec![0x05, 0x05, 0x00, 0x00, 0x00, 0x61]);
    }

    #[test]
    fn pointer_motion_is_big_endian_and_signed() {
        // -1 must round-trip as 0xFFFF, not clamp to zero.
        assert_eq!(encode_motion(-1, 2), vec![0x05, 0x01, 0xFF, 0xFF, 0x00, 0x02]);
    }
}
```

- [ ] **Step 2: Implement**

```rust
//! Input encoding. Thin wrappers over ghostframe_client_core::input so
//! there is exactly one definition of the wire format.
//!
//! Coordinates are REMOTE framebuffer pixels. The host owns the window
//! and therefore owns any scaling; the library publishes the remote
//! resolution via ClientEvent::Resized.

use ghostframe_client_core::input;

pub fn encode_key(keysym: u32, down: bool) -> Vec<u8> {
    if down { input::encode_key_down(keysym).to_vec() }
    else { input::encode_key_up(keysym).to_vec() }
}

pub fn encode_motion(x: i16, y: i16) -> Vec<u8> {
    input::encode_pointer_move(x, y).to_vec()
}

pub fn encode_button(x: i16, y: i16, button: u8, down: bool) -> Vec<u8> {
    input::encode_pointer_button(x, y, button, down).to_vec()
}

pub fn encode_wheel(dx: i16, dy: i16) -> Vec<u8> {
    input::encode_wheel(dx, dy).to_vec()
}
```

Add `Client::push_key` / `push_pointer_motion` / `push_pointer_button` / `push_wheel`, sending the encoded bytes to the net thread over the existing channel and waking its eventfd.

- [ ] **Step 3: Run and commit**

```bash
cargo test -p ghostframe-client-native
git add ghostframe-client-native/src/input.rs ghostframe-client-native/src/lib.rs
git commit -m "feat(client-native): input push

Wrappers only -- the wire format stays defined once, in
ghostframe-client-core::input. Tests pin big-endian signed coordinates,
because a negative coordinate clamped to zero is invisible until a
pointer crosses the window edge."
```

---

## Task 17: The C ABI and generated header

**Files:**
- Create: `ghostframe-client-capi/src/types.rs`, `src/lib.rs`, `build.rs`, `cbindgen.toml`
- Modify: `tests/containers/test-server/Dockerfile` (add the `build.rs` COPY)

- [ ] **Step 1: Write `cbindgen.toml`**

```toml
language = "C"
include_guard = "GHOSTFRAME_CLIENT_H"
autogen_warning = "/* Generated by cbindgen. Do not edit. */"
cpp_compat = true

[export]
prefix = "gf_"

[parse]
parse_deps = false
```

- [ ] **Step 2: Write `build.rs`**

Model it on `ghostframe-lib/build.rs`:

```rust
fn main() {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    cbindgen::generate(&crate_dir)
        .expect("generate ghostframe_client.h")
        .write_to_file("include/ghostframe_client.h");
    println!("cargo:rerun-if-changed=src");
}
```

- [ ] **Step 3: Define the `#[repr(C)]` types**

`src/types.rs`, matching spec section 4 exactly:

```rust
#[repr(C)]
pub enum gf_result { GF_OK = 0, GF_AGAIN = 1, GF_ERR_INVALID = 2,
                     GF_ERR_STATE = 3, GF_ERR_IO = 4, GF_ERR_GPU = 5 }

#[repr(C)]
pub enum gf_handle_type { GF_HANDLE_DMABUF = 0, GF_HANDLE_WIN32_NT = 1,
                          GF_HANDLE_IOSURFACE = 2, GF_HANDLE_CPU = 3 }

#[repr(C)]
pub struct gf_rect { pub x: u32, pub y: u32, pub w: u32, pub h: u32 }

#[repr(C)]
pub struct gf_plane { pub fd: i32, pub offset: u32, pub stride: u32 }

#[repr(C)]
pub struct gf_frame {
    pub struct_size: u32,
    pub frame_id: u32,
    pub buffer_id: u32,
    pub handle_type: gf_handle_type,
    pub width: u32,
    pub height: u32,
    pub drm_modifier: u64,
    pub n_planes: u32,
    pub planes: [gf_plane; 4],
    pub acquire_fence_fd: i32,
    pub n_damage: u32,
    pub damage: *const gf_rect,
}

#[repr(C)]
pub struct gf_client_config {
    pub struct_size: u32,
    pub hostname: *const libc::c_char,
    pub state_dir: *const libc::c_char,
    pub supports_h264: bool,
    pub indices_raw: bool,
    pub n_export_buffers: u32,
    pub n_preferred_modifiers: u32,
    pub preferred_modifiers: *const u64,
}
```

- [ ] **Step 4: Write the shim**

`src/lib.rs` — every function from spec section 4, each one a null-check plus a call into `ghostframe-client-native`. No logic. Each takes `*mut gf_client` (an opaque `Box<Client>`), validates `struct_size` where a struct is passed, and returns `gf_result`.

Document the three ownership rules from the spec as doc comments on the relevant functions, since cbindgen emits them into the header and that is where a consumer will read them:

- plane fds are owned by the library, valid until teardown, must not be closed;
- `acquire_fence_fd` is owned by the caller and must be closed;
- `damage` is valid only until `gf_client_release_frame`.

- [ ] **Step 5: Add the build.rs COPY to the Dockerfile**

```dockerfile
COPY ghostframe-client-capi/build.rs ghostframe-client-capi/build.rs
COPY ghostframe-client-capi/cbindgen.toml ghostframe-client-capi/cbindgen.toml
```

- [ ] **Step 6: Verify the header generates and compiles as C**

```bash
cargo build -p ghostframe-client-capi
gcc -fsyntax-only -x c ghostframe-client-capi/include/ghostframe_client.h && echo "header OK"
just containers-build
```

Expected: all three succeed. The `gcc -fsyntax-only` check is the one that catches a struct cbindgen could not express.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-client-capi tests/containers/test-server/Dockerfile
git commit -m "feat(client-capi): C ABI shim and generated header

Logic-free: every entry point is a null check plus a call into
client-native. The three ownership rules live as doc comments so cbindgen
emits them into the header, which is where a consumer will actually read
them.

The header is syntax-checked as C in CI-adjacent local gates; cbindgen
silently emitting something uncompilable is otherwise only found by the
first consumer."
```

---

## Task 18: CI wiring and the M1 acceptance test

**Files:**
- Modify: `.github/workflows/e2e.yml`
- Create: `ghostframe-client-gpu/README.md`

- [ ] **Step 1: Name the CI-safe test targets**

CI runners have no suitable GPU. The skip belongs in the workflow, not in `#[ignore]` or a runtime self-skip, so that developers always see the GPU tests run locally.

In `.github/workflows/e2e.yml`, alongside the existing `cargo test -p ghostframe-client-net` line:

```yaml
      # Pure-logic targets only. The GPU-requiring targets in this crate
      # (gpu_export, gpu_pipelines, gpu_oracle) are deliberately NOT named
      # here: runners have no suitable device. They run on every developer
      # machine via `cargo test -p ghostframe-client-gpu`, which is the
      # point -- an #[ignore] would hide them locally too.
      - run: cargo test -p ghostframe-client-gpu --test coalesce --test dirty
      - run: cargo test -p ghostframe-client-gpu --lib
      - run: cargo test -p ghostframe-client-native --test queue
      - run: cargo build -p ghostframe-client-capi
```

- [ ] **Step 2: Document the split**

`ghostframe-client-gpu/README.md`:

```markdown
# ghostframe-client-gpu

GPU decode and dmabuf export for the native client.

## Tests

| Target | Needs a GPU | Runs in CI |
|---|---|---|
| `--lib` | no | yes |
| `--test coalesce` | no | yes |
| `--test dirty` | no | yes |
| `--test gpu_export` | yes | no |
| `--test gpu_pipelines` | yes | no |
| `--test gpu_oracle` | yes | no |

Run everything locally with `cargo test -p ghostframe-client-gpu`.

The GPU targets are excluded from CI by not being named in
`.github/workflows/e2e.yml`, never by `#[ignore]`. CI exempts itself; a
developer must always see these run.
```

- [ ] **Step 3: Run the complete local gate**

```bash
just ci-local
cargo test -p ghostframe-client-gpu
cargo test -p ghostframe-client-native
```

Expected: all green. `just ci-local` covers fmt, clippy and the env-read guard — `cargo clippy` alone is not "green" here.

- [ ] **Step 4: Commit and open the PR**

```bash
git add .github/workflows/e2e.yml ghostframe-client-gpu/README.md
git commit -m "ci(client): name the CI-safe client test targets

The GPU-requiring targets are excluded by omission from the workflow,
not by #[ignore]: CI exempts itself, so developers keep seeing them run."
git push -u origin feature/native-client
gh pr create --title "M1: native client library, GPU decode and dmabuf export" \
  --body "Implements M1 of docs/superpowers/specs/2026-09-22-native-client-design.md.

Four new crates: client-gpu (wgpu-on-hand-built-Vulkan, WGSL decode,
dmabuf export ring with history-aware partial blits), client-native
(tsnet transport, net + render threads, eventfd event queue),
client-capi (C ABI + generated header), and a cli scaffold for M2.

The test worth reviewing is \`gpu_oracle\`: it replays one capture through
ClientCore in both TileDelivery modes and compares the Rust decoder
against the real WGSL pixel by pixel. That is what oracle_gpu_sparse
could not do.

supports_h264 is false throughout; VA-API decode is M3.

🤖 Generated with [Claude Code](https://claude.com/claude-code)"
```

---

## Task 19: M1 acceptance test — connect to a real server and assert pixels

Everything before this proves a piece. This proves the milestone: a native client
that joins a tailnet, connects to a live ghostframe server, decodes on the GPU and
publishes a dmabuf whose contents are correct.

**Files:**
- Create: `ghostframe-e2e/tests/native_client.rs`
- Modify: `ghostframe-e2e/Cargo.toml`

- [ ] **Step 1: Add the dev-dependency**

In `ghostframe-e2e/Cargo.toml` under `[dev-dependencies]`:

```toml
ghostframe-client-native = { path = "../ghostframe-client-native" }
ghostframe-client-gpu = { path = "../ghostframe-client-gpu" }
```

- [ ] **Step 2: Write the acceptance test**

`ghostframe-e2e/tests/native_client.rs`:

```rust
//! M1 acceptance: the native client against a live server.
//!
//! Requires Docker and a GPU. Deliberately NOT named in any CI workflow.

use ghostframe_e2e::harness::{create_preauth_key, setup_e2e_server, E2eServerSpec};

#[tokio::test(flavor = "multi_thread")]
async fn native_client_renders_the_test_pattern_into_an_exported_dmabuf() {
    // `webgpu: false` -- we are the GPU client now, no browser or Weston needed.
    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--solid-colour 0x204080",
        extra_env: &[],
        gpu: true,
        webgpu: false,
        url_query_extra: "",
    })
    .await
    .expect("bring up headscale + ghostframe server");

    // Join the tailnet as our own node, exactly as a real client would.
    // No direct-socket path exists, and none is added for tests.
    let authkey = create_preauth_key("headscale", "ghostframe")
        .await
        .expect("preauth key");

    let mut client = ghostframe_client_native::Client::new(
        ghostframe_client_native::Config {
            hostname: "native-client-test".into(),
            authkey,
            state_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
            supports_h264: false,
            indices_raw: false,
            n_export_buffers: 3,
            preferred_modifiers: vec![0], // LINEAR, so map_read is meaningful
        },
    )
    .expect("create client");

    client
        .connect(&setup.server_container_name, 443)
        .expect("connect over the tailnet");

    // Drain events until a frame arrives or we give up.
    let frame = wait_for_frame(&mut client, std::time::Duration::from_secs(30))
        .expect("no frame published within 30s");

    // The test pattern is a known solid colour, so the assertion can be
    // exact rather than a similarity score.
    let bytes = client.debug_map_frame(&frame).expect("map exported dmabuf");
    let stride = frame.planes[0].stride as usize;
    // Sample away from the origin: an origin-only check passes with a
    // wrong stride.
    let off = 100 * stride + 100 * 4;
    assert_eq!(
        &bytes[off..off + 3],
        &[0x20, 0x40, 0x80],
        "wrong pixel at (100,100); the server sent 0x204080"
    );

    // Damage on the first published frame must cover the whole surface.
    let covered: u32 = frame.damage.iter().map(|r| r.w * r.h).sum();
    assert_eq!(
        covered,
        frame.width * frame.height,
        "first frame must report full-surface damage"
    );

    client.release_frame(frame.frame_id);
    client.disconnect().expect("disconnect");
}

fn wait_for_frame(
    client: &mut ghostframe_client_native::Client,
    timeout: std::time::Duration,
) -> Option<ghostframe_client_gpu::ring::PublishedFrame> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        while let Some(ev) = client.next_event() {
            if matches!(ev, ghostframe_client_native::event::ClientEvent::FrameReady { .. }) {
                if let Some(f) = client.acquire_frame() {
                    return Some(f);
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    None
}
```

Check the exact `test_pattern_args` flag for a solid colour against
`ghostframe-test-pattern/src/main.rs` before running; if no such flag exists, use
whatever produces a deterministic region and assert on that instead. Do **not**
weaken the assertion to a similarity score — M1 renders solid/palrle/cdf53, all of
which are exact, and an exact assertion is what catches a channel-order or stride
mistake.

- [ ] **Step 3: Add `Client::debug_map_frame`**

In `ghostframe-client-native/src/lib.rs`:

```rust
    /// Map a published frame's dmabuf and copy its bytes out. Test and
    /// diagnostic use only; wraps ExportedImage::map_read, which uses a
    /// CPU mmap with DMA_BUF_IOCTL_SYNC rather than a second Vulkan
    /// device (cross-device PRIME import returns stale bytes here).
    pub fn debug_map_frame(&self, frame: &PublishedFrame)
        -> Result<Vec<u8>, ClientError> { todo_impl() }
```

- [ ] **Step 4: Build the container and run the test**

```bash
just containers-build
cargo test -p ghostframe-e2e --test native_client -- --nocapture --test-threads=1
```

Expected: PASS.

`just containers-build` is not optional: `cargo test` does not rebuild the
`ghostframe/test-server` image, so without it the test runs the previous server
binary and any server-side change is silently absent.

- [ ] **Step 5: Do NOT name this target in CI**

It needs Docker and a GPU. Leave it out of `.github/workflows/e2e.yml` and note it
in `ghostframe-client-gpu/README.md`'s table. As with the other GPU targets, the
exclusion lives in the workflow by omission, never in an `#[ignore]`.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-e2e/tests/native_client.rs ghostframe-e2e/Cargo.toml \
        ghostframe-client-native/src/lib.rs ghostframe-client-gpu/README.md
git commit -m "test(e2e): M1 acceptance -- native client against a live server

Joins the tailnet as its own node, connects over tsnet, decodes on the
GPU and asserts exact pixels in the exported dmabuf. No direct-socket
path is added for the test.

Samples away from the origin, because an origin-only assertion passes
with a wrong stride. Also asserts the first published frame reports
full-surface damage."
```

---

## Definition of done for M1

- [ ] `cargo test -p ghostframe-client-gpu` passes on a real GPU, including `gpu_oracle`
- [ ] `cargo test -p ghostframe-e2e --test native_client` passes against a live server
- [ ] `cargo test -p ghostframe-client-native` passes
- [ ] `just ci-local` green (fmt, clippy, env-read guard)
- [ ] `just containers-build` succeeds
- [ ] `ghostframe_client.h` generates and passes `gcc -fsyntax-only`
- [ ] The coalescing proptest has been shown to fail under a deliberate mutation
- [ ] No `#[ignore]` anywhere in the new crates
