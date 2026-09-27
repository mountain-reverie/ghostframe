# Native client on Mali/GLES with V4L2 stateless decode — Implementation Plan

**Goal:** Make `ghostframe connect` work on a machine with no Vulkan and no VA-API — reference hardware Pinebook Pro (RK3399, Mali-T860, mainline 6.7). Render through wgpu's GLES backend, decode H.264 on rkvdec through the V4L2 Request API.

**Architecture:** A second backend pair behind a Cargo feature, not a replacement. `DmabufPlanes` stays the seam, so `renderer.rs`, `framebuffer.rs`, `dirty.rs`, `coalesce.rs`, `ring.rs` and all six `pipelines/*` are untouched. Render: `EGL_MESA_image_dma_buf_export` replaces `export.rs`'s raw-Vulkan path; `eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT)` + `glEGLImageTargetTexture2DOES` + `wgpu_hal::gles::Device::texture_from_raw` replaces `import.rs`'s. Decode: `cros-codecs` over `v4l2r`, with ghostframe owning the frame pool so dmabufs are imported once at setup rather than per frame.

**Tech Stack:** wgpu/wgpu-hal 30.0.1 `gles` backend, `khronos-egl`, naga's GLSL-ES output, `cros-codecs =0.0.6` (`v4l2` feature), `v4l2r 0.0.5`.

**Spec:** `docs/superpowers/specs/2026-09-25-native-client-gles-v4l2-design.md`

---

## Read this first

**Task 4 was the schedule risk and no longer is — read
[`../investigations/2026-09-26-h264-v4l2-expbuf-feasibility.md`](../investigations/2026-09-26-h264-v4l2-expbuf-feasibility.md)
before Task 4 or Task 5.** H.264 now decodes on rkvdec through cros-codecs into
dmabufs, byte-identical to a software golden at 640x480 and 1920x1080, with a
runnable reference implementation in `tools/hw-probe/v4l2-expbuf-rs/`. What that
changes: the frame type is **downstream** ghostframe code, not a fork; the two
fork patches are 35 lines total; and three traps that produce plausible-looking
wrong output are named there with their symptoms. Tasks 1–2 remain the low-risk
warm-up, verifiable on an x86 box today.

**Do not touch `DmabufPlanes`.** `ghostframe-client-h264/src/descriptor.rs:60` is
what makes this port cheap: it names no FFmpeg, VA-API or Vulkan type. If you
find yourself adding a field or a lifetime to it to make a backend fit, stop —
that is the design failing, not the struct. The tests that construct it as a
plain literal with no `AVFrame` behind them are the regression net for exactly
this.

**Every shader has two dispatch sites, in two languages.** `shaders/client/*.wgsl`
is shared with the browser client. A workgroup-size change has to stay in step
with *both* `ghostframe-client-gpu/src/pipelines/{cdf53,palrle}.rs` and
`ghostframe-web-client/src/webgpu/{cdf53,palrle}.ts`. For the reshape in Task 2
the workgroup *count* does not change, so neither dispatch site changes — but
verify that rather than assume it.

**`cargo test -p <crate>` hides test binaries that fail to compile** and still
reports `0 failed` (AGENTS.md). Clippy `--all-targets` is the gate. On this
hardware `cargo test -p ghostframe-client-gpu` also fails 13 of 13 in
`gpu_export` with `NoVulkanAdapter` before Task 7 and after it under the default
feature — that is the environment, not a regression. Know which failures are
expected before you start, or you will chase one of them.

**CI cannot run any of the new paths.** No runner has a Mali GPU or a stateless
V4L2 decoder. The only automated protection the GLES backend gets is
`cargo check --features gles` (Task 10). A feature nobody builds is already
broken — land Task 10 with Task 7, not at the end.

**If `ghostframe-client-capi`'s public surface moves**, cbindgen regenerates
`ghostframe-client-capi/include/ghostframe_client.h` and `just ci-local` diffs
it. Nothing in this plan should change that surface; if the header moves,
something leaked out of the GPU crate that should not have.

**Numbers in this plan that are guesses, not measurements:** "roughly one row
per two invocations" as the cost model for the Task 2 reshape. Label them as
guesses where you act on them. Every *hardware* number below was measured —
`tools/hw-probe/` reproduces them.

Two former guesses are now settled, in the opposite direction from each other.
The 150–250 line estimate for Task 4 held (~200 lines) but the code is
downstream rather than forked. The 3-frame pool depth carried from
`n_export_buffers: 3` is **wrong and not ghostframe's to choose**: the decoder's
capture pool is driver-allocated, and asking for 5 got 7 (cros-codecs adds 2,
`"+2 due to HCMP1_HHI_A.h264 needing more"`). Size the import table from what
the driver gives back.

---

## File structure

| File | Responsibility | Task |
|---|---|---|
| `ghostframe-client-gpu/src/shader_validation.rs` | Workgroup-limit guard, no GPU needed | 1 |
| `shaders/client/cdf53_inverse_l1.wgsl` | 256 → ≤128 invocations (load pass) | 2 |
| `shaders/client/cdf53_inverse_l2.wgsl` | 256 → ≤128; **has a real trap**, see Task 2 | 2 |
| `shaders/client/palrle_decode.wgsl` | 256 → ≤128 invocations | 2 |
| `cros-codecs` fork | Honour an explicit V4L2 device path | 3 |
| `cros-codecs` fork | Delegate `num_planes` through `PooledVideoFrame` | 3 |
| `ghostframe-client-h264/src/v4l2_frame.rs` | `VideoFrame` over MMAP + `VIDIOC_EXPBUF` — **downstream, not a fork** | 4 |
| `ghostframe-client-h264/src/decoder.rs` | ffmpeg/VA-API → cros-codecs | 5 |
| `ghostframe-client-h264/src/probe.rs` | `vainfo` ground truth → `S264` enumeration | 6 |
| `ghostframe-client-h264/Cargo.toml` | Backend features | 5 |
| `ghostframe-client-gpu/src/wgpu_ctx.rs` | GLES adapter/device, corrected limits | 7 |
| `ghostframe-client-gpu/src/export.rs` | Split Vulkan/GLES export | 8 |
| `ghostframe-client-gpu/src/import.rs` | Split Vulkan/GLES import; import-once pool | 9 |
| `ghostframe-client-gpu/Cargo.toml` | `vulkan` (default) / `gles` features | 7 |
| `Cargo.toml` (workspace) | Pin `cros-codecs`, `khronos-egl` | 5, 7 |
| `.github/workflows/ci.yml`, `Justfile` | `cargo check --features gles` | 10 |
| `tools/hw-probe/` | Already landed; re-run to confirm a new machine | — |

Untouched, and it should stay that way: `renderer.rs`, `framebuffer.rs`,
`dirty.rs`, `coalesce.rs`, `ring.rs`, `pipelines/*`, `descriptor.rs`,
`nv12_reference.rs`.

---

## Task 1: Guard the workgroup limit before changing any shader

Write the guard first and **watch it fail on three shaders**. That is the point:
it proves the guard sees what it claims to, and it is the only thing that will
stop someone reintroducing a 256-invocation workgroup six months from now on a
machine where it works.

**Files:**
- Modify: `ghostframe-client-gpu/src/shader_validation.rs`

That module already walks `shaders/client/` recursively and runs naga's frontend
and validator with no GPU, so `cargo test --workspace --lib` covers it in CI.
Extend the existing per-file loop: naga's parsed `Module` exposes
`entry_points[..].workgroup_size: [u32; 3]` (`naga-30.0.1/src/ir/mod.rs:2489-2501`),
so the check is the product against a constant.

Two things the guard must get right or it will pass on shaders it never looked
at:

- **Filter to `stage == ShaderStage::Compute`.** `workgroup_size` is meaningless
  on vertex and fragment entry points, and a `[0, 0, 0]` there would make the
  product zero and the assertion trivially true.
- **Reject `workgroup_size_overrides.is_some()` rather than ignoring it.** A
  shader can set its workgroup size from override expressions, in which case the
  literal array is not the real size and the guard would be reading the wrong
  number. None of the 11 shaders does this today; fail loudly if one ever
  starts, because the alternative is a guard that silently stops guarding.

```rust
/// The smallest `maxComputeInvocationsPerWorkgroup` any target we ship to
/// provides. GLES 3.1's spec minimum is 128 and Mali-T860 reports exactly
/// that (tools/hw-probe/glprobe.c); WebGPU guarantees 256, so a shader that
/// fits here fits everywhere. Raising this re-breaks the GLES client.
const MAX_WORKGROUP_INVOCATIONS: u32 = 128;
```

Assert per entry point, and put the shader path *and* the offending size in the
failure message — "exceeds limit" without the number sends the next reader back
to the probe.

**Verification.** On current `master` this must fail naming exactly
`cdf53_inverse_l1.wgsl`, `cdf53_inverse_l2.wgsl` and `palrle_decode.wgsl`, and
nothing else. If it names fewer, the check is not reading what you think; if it
names more, re-read the `@workgroup_size` grep before touching extra files.

---

## Task 2: Reshape the three 256-invocation shaders

One workgroup per unit of work stays the same; each invocation does twice as
much. `@workgroup_size(16, 16, 1)` → `@workgroup_size(16, 8, 1)`, with `lid.y`
covering two rows.

**Files:**
- Modify: `shaders/client/cdf53_inverse_l1.wgsl`
- Modify: `shaders/client/cdf53_inverse_l2.wgsl`
- Modify: `shaders/client/palrle_decode.wgsl`
- Verify unchanged: `ghostframe-client-gpu/src/pipelines/cdf53.rs:537-566`,
  `pipelines/palrle.rs:234`, `ghostframe-web-client/src/webgpu/cdf53.ts:525-539`,
  `webgpu/palrle.ts:148`

### 2.1 `cdf53_inverse_l1.wgsl` and `palrle_decode.wgsl` are the easy two

`cdf53_inverse_l1` is a pure load pass — each invocation handles one `(y, x)`
across three channels, and the trailing `storageBarrier()`/`workgroupBarrier()`
is vestigial since the inverse math moved to `cdf53_inverse_l1_pass2.wgsl`. Wrap
the body in a two-iteration row loop:

```wgsl
@compute @workgroup_size(16, 8, 1)
...
  let x = x_base + lid.x;
  for (var yy: u32 = 0u; yy < 2u; yy = yy + 1u) {
    let y = y_base + lid.y * 2u + yy;
    // ...existing per-(y,x) body, unchanged...
  }
```

`palrle_decode.wgsl` has **no barriers at all** and the same shape. Same
treatment.

Keep the barriers at top level and outside the new loop. A `workgroupBarrier()`
inside a loop that not every invocation runs the same number of times is
undefined behaviour, and it will not fail on the machine you test it on.

### 2.2 `cdf53_inverse_l2.wgsl` will silently produce a half-wrong image

This is the one to be careful with. Its structure is:

```
load loop over channels          (all 256 invocations)
workgroupBarrier()
if (lid.y == 0u) { ... }         vertical lifting  — indexes COLUMNS by lid.x
workgroupBarrier()
if (lid.x == 0u) { ... }         horizontal lifting — indexes ROWS by lid.y
```

Only 16 of the 256 invocations do the lifting in each block. Dropping the y
dimension to 8 leaves `if (lid.y == 0u)` fine (it indexes by `lid.x`, still
0..16) but makes `if (lid.x == 0u)` iterate `lid.y` over 0..8 — **processing
eight of sixteen rows**. Halving the x dimension instead breaks the other block.
Either axis breaks one.

The result is not a crash or a black frame. It is a tile whose lower half never
gets the horizontal inverse pass: plausible-looking output, wrong pixels,
`gpu_oracle` the only thing standing between it and a release.

**Do not patch the indices — decouple the guards from the workgroup shape.** Use
a linear thread id so the two lifting blocks stop caring about the dimensions at
all:

```wgsl
  let tid = lid.y * 16u + lid.x;   // 0..128 for @workgroup_size(16, 8, 1)
  ...
  workgroupBarrier();
  if (tid < 16u) { /* vertical lifting for column `tid` */ }
  workgroupBarrier();
  if (tid < 16u) { /* horizontal lifting for row `tid` */ }
```

That is also why this is worth doing rather than the minimal edit: the next
person to change a workgroup size cannot reintroduce the bug.

### 2.3 Verify the dispatch counts really did not change

`cdf53.rs:557` dispatches `wg_cap * 4` and `palrle.rs:234` dispatches
`(tiles.len(), 2, 2)` — counts of *workgroups*, which the reshape does not
change. Confirm by reading, and confirm the four TypeScript sites agree. If any
count does change, the web client changes too and this task grew a dependency on
`npm test`.

### 2.4 This is a browser-visible change, so measure it

Smaller workgroups may cost throughput on desktop GPUs. The design accepts that
trade but explicitly refuses to assume it is zero.

- `cargo test -p ghostframe-client-gpu --test gpu_oracle --test gpu_pipelines`
  on a machine with a working Vulkan device — these must be bit-identical
  before and after. The reshape is a pure re-indexing; any pixel difference is a
  bug in it.
- `ghostframe-bench` before and after, and write the number in the PR. If it
  regresses more than noise, say so rather than burying it.
- `npm test` in `ghostframe-web-client`, since the shaders it loads changed.

**Mutation check.** Perturb one reshaped loop to cover one row instead of two
and confirm `gpu_oracle` fails. If it passes, the oracle is not covering the
region you changed and Task 2 has no test.

---

## Task 3: Teach cros-codecs which V4L2 device to use

**Files:**
- Patch: `cros-codecs` — `src/device/v4l2/utils.rs`, `src/device/v4l2/stateless/device.rs`, `src/c2_wrapper/c2_v4l2_decoder.rs`
- Modify: `Cargo.toml` (workspace) — `[patch.crates-io]` pointing at the fork

`enumerate_devices()` returns the first `/dev/videoN` with an output mplane queue
and a matching media device, with no check that the device decodes the codec. On
RK3399 that is `/dev/video0` — the hantro **encoder** — and the decoder dies with
`Unrecoverable decoding error`. Measured: with an override it selects
`/dev/video3` + `/dev/media1` correctly.

`C2V4L2DecoderOptions::video_device_path` already exists carrying
`TODO: This is currently unused`, so honouring it is the intended fix and the
shape upstream wants. Note that ghostframe drives `StatelessDecoder::new_v4l2`
directly rather than the C2 wrapper, so the override has to reach
`enumerate_devices` itself; the probe does it with an env var, which is fine for
a probe and not for the library.

**A second patch belongs with it, found the hard way:** `PooledVideoFrame`
delegates every other defaulted `VideoFrame` method but not `num_planes`, so the
override Task 4 depends on is silently discarded behind the pool and QBUF fails
`NumPlanesMismatch(2, 1)`. One line. Both patches together are 35 added lines —
`tools/hw-probe/v4l2-expbuf-rs/cros-codecs-0.0.6.patch`.

Note the ordering constraint before designing anything cleverer: device
selection happens in `V4L2Device::new()`, which takes no arguments, while the
codec only arrives later at `initialize_queues(format: Fourcc, …)`. Filtering by
supported coded format therefore means deferring selection or threading the
fourcc through. **Do not do that here** — plumb the explicit path, which is
smaller, sufficient, and what the TODO asks for.

Send it upstream. Until it lands, `[patch.crates-io]` against a fork with the
commit pinned; the design accepts carrying a delta, and an accepted patch takes
it to zero.

---

## Task 4: A dmabuf `VideoFrame` backed by MMAP + `VIDIOC_EXPBUF`

**Done once already, as a probe.** `tools/hw-probe/v4l2-expbuf-rs/src/main.rs`
is a working `V4l2ExpbufVideoFrame` that decodes 60 frames bit-exactly on this
hardware. Lift it; do not start from scratch, and do not start from upstream's
`V4l2MmapVideoFrame`, which `todo!()`s on every contiguous format.

**Files:**
- Add: `ghostframe-client-h264/src/v4l2_frame.rs` — **downstream of cros-codecs,
  not a fork.** The trait and everything it needs (`VideoFrame`, `FramePool`,
  `v4l2r` via `pub use`) are public, so no queue wiring is required.

### 4.1 Why GBM cannot be used, so nobody retries it

cros-codecs' only dmabuf frame source is GBM, and **panfrost's GBM cannot
allocate NV12 at all** (`tools/hw-probe/gbmprobe.c`):

```
NV12 supported (render): 0     NV12 supported (linear): 0
gbm_bo_create NV12: FAIL for GBM_BO_USE_HW_VIDEO_DECODER (1<<13), LINEAR,
                    RENDERING, LINEAR|RENDERING, and 0
gbm_bo_create XRGB8888, LINEAR|RENDERING: OK, modifier 0x0
```

So it is the driver, not the ChromeOS-only `1 << 13` usage flag the crate passes.
There is also no `/dev/dma_heap` on this kernel to allocate from instead.
`V4l2MmapVideoFrame` exists but is CPU-mapped with no export path, and `ccdec`
never exercises it — its pool is hardcoded to GBM regardless of
`--frame-memory`, which is why that flag appears to do nothing.

### 4.2 What to build

Let the **driver** allocate (`V4L2_MEMORY_MMAP`, vb2 `dma_contig`, which is what
rkvdec requires) and export each buffer. Measured working
(`tools/hw-probe/expbuf_probe.c`):

```
OUTPUT  set: S264 640x480
CAPTURE set: NV12 640x480 planes=1   bytesperline=640 sizeimage=614400
REQBUFS MMAP: got 4 buffers
EXPBUF buf 0..3 plane 0 -> dmabuf fd      4 dmabuf fds exported
```

`v4l2r 0.0.5` already provides `ioctl::expbuf()`, so the ioctl is in hand. The
new type reports a `FrameLayout` built from the capture queue's
`v4l2_pix_format_mplane`.

**It must hold the exported `File` per *buffer index*, not per frame object.**
With `V4L2_MEMORY_MMAP` the driver owns the pool and assigns an index at dequeue
time, so one frame object sees several indices over its life; caching the fd of
the first index it saw makes the frame read whatever picture now occupies that
buffer. The symptom is not corruption — every frame is a real frame, just the
wrong one, which reads as a stutter and sends you looking in the wrong place. It
cost 40 of 60 frames in the probe. Key the table `HashMap<u32, Arc<File>>` on
the index, export on first sight, and **key the GPU import the same way** (this
sharpens §5.1 below).

**`num_planes()` must return the V4L2 plane count, not the logical one.** rkvdec
reports 1 for NV12 — one buffer, both planes — and `queue_with_handles` rejects
a mismatched handle count. `get_plane_size`/`get_plane_pitch`/`map` stay
two-entry, because those *are* the logical planes.

`FrameLayout` and `PlaneLayout` are public (`cros_codecs::FrameLayout`:
`format: (Fourcc, u64 /* modifier */)`, `size`, `planes: Vec<PlaneLayout>` with
`buffer_index`, `offset`, `stride`), so nothing here needs new upstream API
surface beyond the frame type itself.

### 4.3 The UV offset is not arithmetic

rkvdec reports `num_planes=1`, so there is **one** buffer holding both planes and
V4L2 does not tell you where chroma starts. `sizeimage=614400` against 460800 for
packed 640×480 NV12 — the buffer is height-padded.

Measured, at two resolutions:

```
chroma_offset = bytesperline * format.height    // the driver's CODED height
```

Two things it is **not**. Not from `sizeimage` — 614400 against 460800 of actual
pixels, the rest zero scratch, so dividing it out gives a height of 640 for a
480-row frame. And not from the *display* height whenever that isn't 16-aligned:
at 1080p the driver reports display 1920x1080, coded 1920x1088, and chroma starts
at `1920 * 1088`. 640x480 is the case where the two agree, so **a test only at
640x480 proves nothing about this** — cover a non-16-aligned height.

Getting it wrong yields a plausible image with shifted colour rather than an
error, so assert it: a frame decoded through this path must match a
software-decoded golden byte for byte (Task 5's test). Mutation-checked in the
probe — `CHROMA_SHIFT_ROWS=1` fails, `=0` passes.

### 4.4 Upstream it

The *frame type* lives in ghostframe, so there is nothing to upstream there —
that is the finding that shrank this task. What should go upstream is the pair of
patches in Task 3, and a note that `V4l2MmapVideoFrame`'s contiguous-format
`todo!()` is reachable on any mainline-kernel SoC without ChromeOS's GBM, which
is most of them.

---

## Task 5: Swap `decoder.rs` from ffmpeg/VA-API to cros-codecs

**Files:**
- Modify: `ghostframe-client-h264/src/decoder.rs` (811 lines today)
- Modify: `ghostframe-client-h264/Cargo.toml` — backend features
- Modify: `Cargo.toml` (workspace) — `cros-codecs = { version = "=0.0.6", default-features = false, features = ["v4l2"] }`
- Do **not** modify: `ghostframe-client-h264/src/descriptor.rs`

Pin exactly, like every other entry in `[workspace.dependencies]`. It is a 0.0.x
crate; churn is expected and pinning is the accepted answer.

### 5.1 ghostframe owns the pool, and imports once

`cros-codecs` takes a caller-supplied `alloc_cb` and hands back
`PooledVideoFrame<…>`. That inverts today's per-frame import:

1. At setup, allocate frames via Task 4 and record fd + `FrameLayout` per
   **V4L2 buffer index**. The count is not ghostframe's to pick: the capture
   pool is driver-allocated, and cros-codecs asks for `min_num_frames + 2`
   (`"+2 due to HCMP1_HHI_A.h264 needing more"`) — 5 requested became 7 on this
   hardware. Size the table from what came back, not from
   `Config::n_export_buffers`.
2. Import each **once** into the GPU, keyed by buffer index, on first sight
   (Task 9). Not keyed by frame object — see Task 4, where that mistake silently
   reordered 40 of 60 frames.
3. Per decoded frame, look up the already-imported texture by buffer index.

`GenericDmaVideoFrame`'s `dma_handles`/`layout` are private with no accessor —
irrelevant, because ghostframe constructs the frames and already knows every fd
and layout. Build `DmabufPlanes` from your own bookkeeping, not by reading it
back out.

This also retires a hazard the existing code documents at length.
`descriptor.rs:64-76` warns that an importer must `dup()` because
`vkImportMemoryFdKHR` takes ownership and a double close against the `AVFrame`'s
unref is silent on some drivers. Importing once, from fds ghostframe owns for the
pool's lifetime, removes that class of bug rather than re-implementing the
workaround.

### 5.2 Keep the backends separable

`ffmpeg-next`/`ffmpeg-sys-next` should not be a hard dependency of a V4L2-only
build, and VA-API must keep working on x86. Mirror the existing feature idiom in
this crate (`test-support` is already off-by-default and documented as such) with
`vaapi` (default) and `v4l2`, and route `Cargo.toml`'s ffmpeg deps behind
`vaapi`.

Note what this does *not* change: `ghostframe-client-gpu` depends on
`ghostframe-client-h264` unconditionally and `renderer.rs` drives `H264Decoder`
directly, so the *type* must exist under both features with the same shape. This
is a backend swap behind one API, not two APIs.

### 5.3 The test that matters

**A bit-exactness test against a software golden.** Decode a fixture clip through
the V4L2 path and compare to a software-decoded reference, stride-normalised.
This was verified by hand during investigation — identical md5 over 60 frames —
and it is the only test that catches §4.3 going wrong.

**Mutation check, mandatory here.** Perturb the chroma offset by one row and
confirm the test fails. A bit-exactness test that passes with a deliberately
wrong UV offset is testing nothing, and this is precisely the defect class that
produces confident, wrong output.

Reuse the existing oracle harness rather than inventing one — `oracle_tests.rs`
already has the shape, and its `gradient_clip` generator lives behind
`test-support` for exactly this.

---

## Task 6: Replace the VA-API capability probe

**Files:**
- Modify: `ghostframe-client-h264/src/probe.rs` (348 lines today)

`probe.rs` decides whether HELLO advertises H.264, and its doc is explicit about
why it runs a real decode rather than trusting `avcodec_get_hw_config`: those
calls "only prove libavcodec was *built* with a VA-API hwaccel for H.264, not
that the driver has an H.264 decode profile". Sessions begin in H.264 mode, so a
false positive is a black window. Preserve that reasoning, not just the function
signature.

Two things must both survive:

- **The probe** decodes a small embedded clip through rkvdec and checks a frame
  came out — same strategy, new backend.
- **The independent ground truth.** `oracle_tests.rs` currently gates on
  `vainfo_reports_h264_vld`, a check that does not go through this crate's own
  probe. The replacement is `VIDIOC_ENUM_FMT` on the candidate device's output
  queue reporting `V4L2_PIX_FMT_H264_SLICE` (`S264`) — computed with raw ioctls,
  **not** through `cros-codecs`, so it can still disagree with the probe. An
  oracle that shares a code path with the thing it validates is not an oracle.

`false` is not an error: the server falls back to tile codecs, which is what
every session did before M3. On this hardware that fallback is also the common
path for non-video content, so it must stay clean.

Keep the `QuietLogGuard` equivalent or delete it deliberately. Its reason —
"ffmpeg logs libva failures straight to stderr … which a C host embedding this
library cannot suppress" — applies to any backend that chatters on a failed
probe, and a failed probe is the outcome the design calls normal.

---

## Task 7: The `gles` feature and a GLES `WgpuContext`

**Files:**
- Modify: `ghostframe-client-gpu/src/wgpu_ctx.rs`
- Modify: `ghostframe-client-gpu/Cargo.toml`
- Modify: `Cargo.toml` (workspace) — `khronos-egl`, and a `gles`-featured `wgpu`/`wgpu-hal`

Mutually exclusive `vulkan` (default) and `gles`. Drop `ash` on GLES builds; keep
`khronos-egl` out of Vulkan builds. The workspace manifest already states the
intent for wgpu's other backends — "dead weight in a library other projects
link" — and this is consistent with it. Runtime selection was considered and
rejected in the design (§4.6): the export paths share no code, so a universal
binary links both and carries two unsafe surfaces.

Add a compile-time guard so the failure is a message, not a mystery:

```rust
#[cfg(all(feature = "vulkan", feature = "gles"))]
compile_error!("ghostframe-client-gpu: `vulkan` and `gles` are mutually exclusive; \
                the export paths share no code (see the GLES/V4L2 design, §4.6)");
```

### 7.1 `required_limits` must stop using `downlevel_defaults()` unmodified

`Limits::downlevel_defaults()` sets `max_compute_invocations_per_workgroup: 256`.
Left as-is, `request_device` fails on Mali **before any shader runs**, which
reads as "the GLES backend doesn't work" rather than "one limit is too high".

Request 128 on the GLES path. The comment at `wgpu_ctx.rs:105-113` explains the
256 choice and must be rewritten, not left to contradict the code — it currently
says `palrle_decode.wgsl` "was designed around" 256, which stops being true in
Task 2.

`max_storage_buffers_per_shader_stage: 8` stays exactly as it is. Mali reports
8 and `cdf53_integrate.wgsl` binds 7. That is sufficient with nothing spare —
worth a comment, because an eighth storage buffer in any compute shader would
not fit on this GPU and the failure would look unrelated.

### 7.2 `VULKAN_EXTERNAL_MEMORY_FD` is not the GLES gate

The current hard requirement (`wgpu_ctx.rs:81-88`, erroring with
`AdapterCannotExport`) is Vulkan-specific. The GLES equivalent is the presence of
`EGL_MESA_image_dma_buf_export` in the EGL extension string. Keep the
`AdapterCannotExport` error and its reasoning — "a client that cannot export at
all should refuse to start rather than hand its consumer nothing" — and change
only how the question is asked.

`explicit_modifiers` likewise stays as a concept and changes source: on GLES it
comes from `EGL_EXT_image_dma_buf_import_modifiers`, present on this hardware.
`DRM_FORMAT_MOD_LINEAR` remains the fallback, and GBM reporting modifier `0x0`
for what it *can* allocate is consistent with that.

### 7.3 Pin the context to its thread

EGL contexts are current per-thread. `ghostframe-client-native` already confines
GPU work to `render_thread`, so the design is compatible — but make it explicit
rather than incidental. Record the creating thread id and `debug_assert` it on
every entry point.

wgpu-hal's gles backend guards its own GL calls with an `AdapterContext` lock, so
a violation may appear to work under light load and fail later under a different
one. This assertion is the difference between a panic at the call site and a
load-dependent heisenbug.

**Expect this task to surface the storage-format risk.** wgpu's GL backend
supports a narrower set of storage-texture formats than Vulkan, and
`framebuffer.rs` may sit outside it. That is not resolvable by reading; it is why
this task comes before Task 8 rather than after.

---

## Task 8: GLES dmabuf export

**Files:**
- Modify: `ghostframe-client-gpu/src/export.rs` (782 lines today) — split into a
  backend-selected pair, shared signature

`export.rs` exists because wgpu can import a dmabuf but not export one, so it
reaches for the raw `VkDevice` through `WgpuContext::with_raw_device`. The GLES
sibling does the same thing one layer up: `eglCreateImageKHR` from the GL texture
(`EGL_GL_TEXTURE_2D_KHR`), then `eglExportDMABUFImageQueryMESA` /
`eglExportDMABUFImageMESA` for fd, stride, offset and modifier — which is
`DmabufPlanes` exactly.

`WgpuContext` grows a `with_raw_egl` mirroring `with_raw_device`, built on
wgpu-hal's `AdapterContext::egl_instance()` / `raw_display()` / `raw_context()` /
`egl_config()` (`wgpu-hal-30.0.1/src/gles/egl.rs:213-231, 694-710`). Keep this
the crate's only unsafe surface, as the current type doc promises.

Do not let the two backends diverge in what they promise the caller. The split is
at the bottom of the file, behind one signature; if the GLES path needs a
different *shape* of result, that belongs in `DmabufPlanes` — and per "Read this
first", that means stopping to reconsider.

`gpu_export.rs`'s 13 tests are the specification here. They currently fail on
this hardware with `NoVulkanAdapter`; under `--features gles` they must pass. If
any needs editing beyond feature gating, it was testing Vulkan rather than
export.

---

## Task 9: GLES dmabuf import, and the import-once pool

**Files:**
- Modify: `ghostframe-client-gpu/src/import.rs` (732 lines today) — same split
- Modify: wherever the decode→render handoff builds its frame pool (follow
  `renderer.rs`'s `blit_h264_frame` call path; confirm the owner before editing)

Import is `eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT)` → `glEGLImageTargetTexture2DOES`
into a GL texture name → `wgpu_hal::gles::Device::texture_from_raw(name: NonZeroU32, …)`.

Measured on this hardware (`tools/hw-probe/nv12import_probe.c`): Mali's EGL lists
50 importable dmabuf formats including `R8`, `GR88` **and** `NV12`. So the
two-image plan works, and note that GBM refusing to *allocate* NV12 says nothing
about importing it — those are separate answers from the same driver. rkvdec's
output is plain linear NV12 (its luma bytes match a software decode exactly at
the reported stride), so `DRM_FORMAT_MOD_LINEAR` is correct and there is no
detiling step — unlike the MediaTek MM21 path cros-codecs is written around.

Then collapse the per-frame import into a per-pool one, per Task 5.1: import each
pool frame once at setup, keyed by fd, and look up by pool index per decoded
frame. This is strictly less work per frame than the Vulkan path does today.

**Prove the GLES path actually ran.** `blit_h264_frame` already emits a
`tracing::info!` naming which import path was taken — that is why
`gpu_h264_render.rs` pulls in `tracing-subscriber` as a dev-dependency. The GLES
path needs the same, and the test must assert on it. A GPU test that silently
fell back, or skipped, reports success indistinguishably from real success.

---

## Task 10: Keep the `gles` feature compiling in CI

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `Justfile`

`cargo check -p ghostframe-client-gpu --features gles --no-default-features`, and
the same for `ghostframe-client-h264 --features v4l2`. The tests cannot run on a
runner, but the code must not rot uncompiled.

Land this **with Task 7**, while there is something to check. Added at the end,
it will be added after the first silent breakage instead of before it.

Also extend `just ci-client` so a developer on ARM runs the same gate locally,
and say plainly in the workflow — next to the existing `browserless` comment
about GPU targets, which is where a reader already looks for this — that the
GLES and V4L2 paths are compile-checked only and verified by hand on the
reference machine.

---

## Done means

- [ ] No shader exceeds 128 invocations, enforced by a CI test that was watched
      failing on all three offending shaders first.
- [ ] `gpu_oracle` and `gpu_pipelines` bit-identical before and after the
      reshape, and the reshape mutation-checked; `npm test` green;
      `ghostframe-bench` delta reported in the PR rather than omitted.
- [ ] `cdf53_inverse_l2.wgsl`'s lifting guards keyed off a linear thread id, so
      the half-image trap cannot come back with the next reshape.
- [ ] H.264 decodes on rkvdec through cros-codecs, **bit-exact against a
      software golden**, with the chroma-offset mutation check failing as it
      must.
- [ ] Both cros-codecs patches (device override, `PooledVideoFrame::num_planes`)
      submitted upstream, and the carried delta pinned by commit in
      `[patch.crates-io]` until they land.
- [ ] The dmabuf export table and the GPU import table are both keyed by **V4L2
      buffer index**, with a test that would catch the frame-reordering symptom
      described in Task 4 rather than only catching corruption.
- [ ] The bit-exactness test covers a **non-16-aligned height** (e.g. 1080), not
      only 640x480 where display and coded height coincide.
- [ ] The capability probe's independent ground truth is still independent — it
      does not go through `cros-codecs`.
- [ ] `ghostframe connect` renders a live session on the reference machine
      through GLES, with the log confirming the GLES import path ran rather than
      a fallback.
- [ ] `DmabufPlanes` unchanged, and the tests that build it as a plain literal
      pass untouched.
- [ ] `cargo check --features gles` in CI and in `just ci-client`; `just
      ci-local` green on x86-64 with default features.
- [ ] `tools/hw-probe/` still reproduces the design's measured baseline, and any
      number that moved is corrected in the design doc rather than left stale.
