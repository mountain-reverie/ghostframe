# Native client on Mali/GLES with V4L2 stateless decode — design

**Status:** approved for planning
**Date:** 2026-09-25
**Predecessors:** [M1](2026-09-22-native-client-design.md), [M2](2026-09-23-native-client-m2-design.md), [M3](2026-09-23-native-client-m3-design.md), [M4a](2026-09-24-native-client-m4a-design.md)
**Reference hardware:** Pinebook Pro — Rockchip RK3399, Mali-T860 (Midgard), mainline kernel 6.7, Mesa 24.0.2

The native client as built through M4 requires two things this machine does not
have and cannot get: a Vulkan device and a VA-API decoder. This design adds a
second backend pair — wgpu's GLES backend for render, `cros-codecs` over V4L2
stateless for decode — without disturbing the Vulkan/VA-API path.

Every hardware claim below was measured on the reference machine. §10 lists the
probes so nobody has to re-derive them.

---

## 1. Scope

**In:** a native client that renders through GLES 3.1 and decodes H.264 on
rkvdec via the V4L2 Request API, on a mainline-kernel Arm SoC with no Vulkan and
no VA-API. Shared-shader changes needed to fit Mali's compute limits.

**Out:** the server. Nothing here touches `ghostframe-lib`, capture, or
encoding. Also out: making one binary support both backends at runtime (§4.6),
and any Wayland/X11 windowing change — `ghostframe-cli`'s window backends
consume a dmabuf and keep doing so.

### 1.1 Why this is worth doing

The motivation is not portability for its own sake. A remote-desktop *client*
wants to be the cheap, light, long-battery machine you carry; the server is the
one that should have the GPU. The current requirements invert that: the client
needs a Vulkan device and a VA-API decoder, which in practice means an
x86 laptop with a discrete or recent integrated GPU — the same class of machine
you would rather be running the session *on*.

The reference machine is the shape the client should support: passive cooling,
a hardware H.264 decoder, and a GPU that can composite but not much more.

---

## 2. Vulkan on Midgard is not a driver-version problem

The reference machine's GPU is Mali-T860 (`dmesg`: `mali-t860 id 0x860`,
500 MHz, 4 shader cores), which is **Midgard**. Mesa's PanVK hardware matrix
starts Vulkan at Bifrost; Midgard has no Vulkan row at all, and Panfrost
exposes OpenGL ES only. PanVK is conformant solely on Mali-G610.

There is nothing to upgrade to. The 2021 Collabora PanVK announcement covered
"Midgard and Bifrost", but that was a prototype and Midgard never became usable.

This matters because the reference machine *appears* to have Vulkan and does
not. Its only ICD is a hand-built artifact:

```
/usr/local/lib/libvulkan_panfrost.so          34 MB, dated 2021-07-23
/usr/local/share/vulkan/icd.d/panfrost_icd.aarch64.json
```

owned by no package, implementing loader interface v3 where the loader requires
v5. Because it is the *only* ICD, the loader finds it, rejects it, and every
Vulkan application fails with `ERROR_INITIALIZATION_FAILED` rather than the
honest "no devices". That is also why `cargo test -p ghostframe-client-gpu`
fails 13 of 13 in `gpu_export` with `NoVulkanAdapter` rather than skipping.

**Rejected: lavapipe (`vulkan-swrast`).** It is a CPU rasteriser, so it cannot
deliver a remote desktop; and Manjaro ARM ships Mesa 24.0.2, which predates
lavapipe's `VK_EXT_external_memory_dma_buf` (Mesa 24.1). It would be a
correctness harness at best, and the GLES path gives a real one.

---

## 3. The dmabuf seam already exists, and it is what makes this affordable

This port is tractable because M2/M3 already put the backend-neutral boundary in
the right place. `ghostframe-client-h264/src/descriptor.rs:60`:

```rust
pub struct DmabufPlanes {
    pub fd: i32,
    pub modifier: u64,
    // + luma / chroma PlaneDesc { offset, pitch }
}
```

Its own doc comment records that it is "the plain literal-construction type
synthetic tests build by hand with no `AVFrame` (and no VA-API) behind them at
all." There is no FFmpeg type, no VA-API type, and no Vulkan type in the seam.

Consequently the Vulkan-specific surface is **3 of the 14 files** in
`ghostframe-client-gpu`:

| File | Lines | Fate |
| --- | --- | --- |
| `wgpu_ctx.rs` | 199 | gains a GLES sibling (§4.1) |
| `export.rs` | 782 | gains a GLES sibling (§4.2) |
| `import.rs` | 732 | gains a GLES sibling (§4.3) |
| `renderer.rs`, `framebuffer.rs`, `dirty.rs`, `coalesce.rs`, `ring.rs`, … | ~2150 | unchanged |

And the shaders are WGSL compiled by naga, which has a GLSL-ES backend, so the
11 shaders are reused rather than rewritten — subject to §5.

---

## 4. Render: wgpu's GLES backend

### 4.1 Every EGL extension the port needs is already present

Measured on the reference machine (`eglinfo`, and `glprobe.c` in §10):

| Requirement | Extension | Present |
| --- | --- | --- |
| compute shaders | GLES 3.1 / GLSL ES 3.10 | yes (`Mali-T860 (Panfrost)`) |
| dmabuf **export** | `EGL_MESA_image_dma_buf_export` | yes |
| dmabuf **import** | `EGL_EXT_image_dma_buf_import` (+`_modifiers`) | yes |
| explicit sync | `EGL_KHR_fence_sync`, `EGL_ANDROID_native_fence_sync` | yes |

Export being available is the load-bearing fact. Without
`EGL_MESA_image_dma_buf_export` there would be no way to hand the window backend
a buffer, and the design would stop here.

### 4.2 Export: `eglExportDMABUFImageMESA` replaces the raw-Vulkan path

`export.rs` exists because wgpu can import a dmabuf but not export one, so it
reaches for the raw `VkDevice` via `WgpuContext::with_raw_device`. The GLES
sibling does the same thing one layer over: create an `EGLImage` from the GL
texture (`eglCreateImageKHR` with `EGL_GL_TEXTURE_2D_KHR`), then
`eglExportDMABUFImageQueryMESA` / `eglExportDMABUFImageMESA` for the fd, stride,
offset and modifier — which is exactly `DmabufPlanes`.

wgpu-hal 30.0.1 provides the hatch (`src/gles/egl.rs`):
`AdapterContext::egl_instance()`, `raw_display()`, `raw_context()`,
`egl_config()`. So `WgpuContext` grows a `with_raw_egl` accessor mirroring
`with_raw_device`, and stays the crate's only unsafe surface.

### 4.3 Import: `EGLImage` from dmabuf, wrapped with `texture_from_raw`

`import.rs`'s Vulkan path imports a fresh dmabuf per decoded frame. The GLES
path is `eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT)` →
`glEGLImageTargetTexture2DOES` into a GL texture name →
`wgpu_hal::gles::Device::texture_from_raw(name: NonZeroU32, …)`.

Per-frame import goes away entirely — see §6.4. Imports happen once per pool
frame at startup, which is strictly less work than the Vulkan path does today.

### 4.4 `explicit_modifiers` becomes the common case, not the fallback

`wgpu_ctx.rs`'s type doc explains that wgpu only reports
`VULKAN_EXTERNAL_MEMORY_DMA_BUF` when `VK_EXT_image_drm_format_modifier` is also
present, and that RADV-on-Polaris lacking it is why the code requires only
`VULKAN_EXTERNAL_MEMORY_FD` and tracks modifiers separately.

On GLES the question is `EGL_EXT_image_dma_buf_import_modifiers`, which *is*
present here. The `explicit_modifiers` flag stays — it is the right abstraction
— but the GLES backend sets it from the EGL extension string rather than a wgpu
feature bit. `DRM_FORMAT_MOD_LINEAR` remains the universal fallback, and note
that GBM reports modifier `0x0` (linear) for what it can allocate (§10).

### 4.5 Thread affinity is a real constraint, not a theoretical one

EGL contexts are current per-thread. `ghostframe-client-native` runs a dedicated
`render_thread` (`render_thread.rs`) and a `net_thread`, and only the former
touches the GPU — so the design is already compatible. But this must be made
explicit rather than left as an accident: the GLES `WgpuContext` is created *on*
the render thread and must not be `Send`-moved afterwards.

wgpu-hal's gles backend guards its own GL calls with an `AdapterContext` lock,
so violations may appear to work under light load and fail later. A
debug-assertion that records the creating thread id and checks it on every entry
point is cheap and turns a latent heisenbug into a panic at the call site.

### 4.6 Backend choice is a Cargo feature, not runtime

`ghostframe-client-gpu` gains mutually-exclusive `vulkan` (default) and `gles`
features; `wgpu`/`wgpu-hal` switch between `features = ["vulkan", "wgsl"]` and
`["gles", "wgsl"]`; `ash` is dropped on GLES builds and `khronos-egl` is absent
from Vulkan builds.

**Rejected: runtime backend selection.** wgpu itself can pick a backend at
runtime, but the export and import paths share no code — one speaks raw Vulkan,
the other raw EGL — so a universal binary would link both `ash` and
`khronos-egl` and carry two unsafe surfaces. The workspace `Cargo.toml` already
states the relevant intent for wgpu's other backends: "dead weight in a library
other projects link." Compile-time selection is consistent with that.

This is worth revisiting only if someone wants to ship a single distro package
covering both, which nobody has asked for.

---

## 5. Workgroups drop to 128, and that is a shared-shader change

Measured compute limits (`glprobe.c`):

```
GL_MAX_COMPUTE_SHADER_STORAGE_BLOCKS       8      <- cdf53_integrate binds 7. fits.
GL_MAX_SHADER_STORAGE_BUFFER_BINDINGS      8
GL_MAX_COMPUTE_WORK_GROUP_INVOCATIONS    128      <- code requires 256
GL_MAX_COMPUTE_WORK_GROUP_SIZE[0], [1]   128
GL_MAX_COMPUTE_SHARED_MEMORY_SIZE      32768
GL_MAX_COMPUTE_IMAGE_UNIFORMS             32
GL_MAX_TEXTURE_SIZE                     8192
```

The storage-buffer headroom is exactly sufficient and no more:
`wgpu_ctx.rs:114-117` raises `max_storage_buffers_per_shader_stage` to 8 because
`cdf53_integrate.wgsl` binds 7, and Mali reports 8. That is a coincidence worth
recording — an eighth buffer in any compute shader would not fit on this GPU.

Invocations are the actual blocker. Three of the 11 shaders exceed 128:

| Shader | Now | Becomes |
| --- | --- | --- |
| `cdf53_inverse_l1.wgsl` | `@workgroup_size(16, 16, 1)` = 256 | ≤128 |
| `cdf53_inverse_l2.wgsl` | `@workgroup_size(16, 16, 1)` = 256 | ≤128 |
| `palrle_decode.wgsl` | `@workgroup_size(16, 16, 1)` = 256 | ≤128 |
| `cdf53_integrate.wgsl` | 64 | unchanged |
| `cdf53_inverse_l1_pass2.wgsl` | 32 | unchanged |
| `cdf53_inverse_l3.wgsl`, `debug_gradient.wgsl` | 64 | unchanged |

`16 × 8` with each invocation handling two rows is the obvious reshape, keeping
one workgroup per dirty tile.

### 5.1 Lower them everywhere, do not fork per backend

`shaders/client/*.wgsl` is shared with the web client, so this changes the
browser path too. Lower them for everyone anyway:

- WebGPU guarantees `maxComputeInvocationsPerWorkgroup` ≥ 256, so 128 is valid
  on every conformant implementation. The change cannot break a browser.
- Per-backend shader variants would mean two versions of the CDF53 inverse
  arithmetic. `ghostframe-client-core/tests/oracle_gpu_sparse.rs` already
  re-implements that arithmetic in Rust, "hand-kept in step with three shader
  sites" — and `ghostframe-client-gpu/Cargo.toml:8-14` names closing exactly
  that gap as the reason the GPU crate exists. Adding a fourth and fifth site
  is how the oracle silently drifts from the shipped WGSL.

The cost is a possible marginal throughput loss on desktop GPUs from smaller
workgroups. That is a real cost and should be measured with
`ghostframe-bench`, not assumed to be zero — but it is the right trade against
duplicating the codec's arithmetic.

### 5.2 `required_limits` must stop using `downlevel_defaults()` unmodified

`Limits::downlevel_defaults()` sets `max_compute_invocations_per_workgroup: 256`.
Left as-is, `request_device` fails on Mali **before any shader runs**, which
reads as "GLES backend doesn't work" rather than "one limit is too high". The
GLES path must request 128 explicitly, and the comment at `wgpu_ctx.rs:105-113`
— which explains the 256 choice — must be updated rather than left to contradict
the code.

---

## 6. Decode: `cros-codecs` over V4L2 stateless

The reference machine's decoder is rkvdec on `/dev/video3`, taking `S264`
(H.264 parsed slice data, Request API) and producing `NV12`. Hantro on
`/dev/video1` does MPEG-2 and VP8 only — **it cannot decode H.264**, which
matters for §6.3.1.

NV12 output is the format `import.rs` and the NV12 blit path already expect, so
the codec seam does not move.

### 6.1 Rejected: FFmpeg

FFmpeg's V4L2 Request API hwaccel is **not upstream**. It has lived
out-of-tree since 2018 (LibreELEC), with a v2 series posted August 2024 still
under discussion, blocked on kernel headers that are not in uapi. The reference
machine's FFmpeg 6.1.1 confirms it: hwaccels are `vdpau vaapi drm opencl
vulkan`, and the only V4L2 H.264 decoder is `h264_v4l2m2m`, the *stateful* M2M
wrapper, which cannot drive a stateless Request-API device.

Requiring a patched system FFmpeg to run the client is a worse dependency than
any amount of Rust.

### 6.2 Rejected: GStreamer `v4l2slh264dec` — and why it is still the fallback

This was measured working, and that must be recorded honestly because it is the
option this design turns down:

- `v4l2slh264dec` ships in gst-plugins-bad 1.22.10, already installed.
- It decodes on rkvdec and is **bit-exact** against FFmpeg's software decoder —
  identical md5 over 60 frames once stride-normalised (§10).
- Its buffers are **dmabuf-backed** (`v4l2codecallocator1`, `dmabuf=True`),
  despite advertising plain `video/x-raw` — 1.22 has no `memory:DMABuf`
  capsfeature, but the memory underneath is a dmabuf.

It is rejected for dependency weight — `gstreamer-rs` pulls glib/gobject into a
client that otherwise has none — and because frame ownership sits on the wrong
side (§6.4). It is **not** rejected for viability, and it is the fallback if the
§6.3.2 patch stalls.

### 6.3 `cros-codecs` needs two patches

`cros-codecs 0.0.6` ships the V4L2 stateless H.264 backend on crates.io
(`v4l2` feature → `v4l2r` + `backend`); it builds on the reference machine in
3m48s, and pulls ~12 direct crates against libgbm/libdrm that are already
present. Pin it with `=0.0.6` like every other entry in
`[workspace.dependencies]` — it is a 0.0.x crate and churn is expected.

Both patches belong upstream. Until they land, carry them via
`[patch.crates-io]` against a fork.

#### 6.3.1 Device selection (small)

`enumerate_devices()` (`src/device/v4l2/utils.rs`) returns the **first**
`/dev/videoN` with an output mplane queue and a matching media device, with no
check that the device decodes the codec in question. On RK3399 that selects
`/dev/video0` — the hantro *encoder* — and the decoder dies with
`Unrecoverable decoding error`.

`C2V4L2DecoderOptions::video_device_path` exists for exactly this and is marked
`TODO: This is currently unused`, so honouring it is the intended fix. Verified
on the reference machine: with an override in place the library selects
`/dev/video3` + `/dev/media1` correctly.

Note that device selection currently happens in `V4L2Device::new()`, before the
codec is known (`initialize_queues(format: Fourcc, …)` comes later), so
filtering by supported coded format means deferring selection or threading the
fourcc through. Honouring an explicit path is the smaller change and is enough.

#### 6.3.2 A dmabuf frame source that is not GBM (the real work)

cros-codecs' only dmabuf frame source is GBM, and **panfrost's GBM cannot
allocate NV12 at all**. Measured with `gbmprobe.c`:

```
NV12 supported (render): 0     NV12 supported (linear): 0
gbm_bo_create NV12, flags = GBM_BO_USE_HW_VIDEO_DECODER (1<<13)  FAIL
gbm_bo_create NV12, flags = LINEAR / RENDERING / LINEAR|RENDERING / 0  FAIL
gbm_bo_create XRGB8888, flags = LINEAR|RENDERING   OK  modifier=0x0
```

So it is the driver, not the ChromeOS-only `1<<13` usage flag the crate passes.
There is also no `/dev/dma_heap` on this kernel to allocate from instead.

The fix is the mechanism GStreamer uses: let the **driver** allocate (MMAP, vb2
`dma_contig`, which is what rkvdec requires) and export with `VIDIOC_EXPBUF`.
Verified directly with `expbuf_probe.c`:

```
OUTPUT  set: S264 640x480
CAPTURE set: NV12 640x480 planes=1   bytesperline=640 sizeimage=614400
REQBUFS MMAP: got 4 buffers
EXPBUF buf 0..3 plane 0 -> dmabuf fd      4 dmabuf fds exported
```

`v4l2r 0.0.5` already provides `ioctl::expbuf()`, so the ioctl is in hand. What
is missing is a cros-codecs `VideoFrame` implementation that wires MMAP+EXPBUF
into a `FrameLayout`. Estimate **150–250 lines**. `V4l2MmapVideoFrame` exists
but is CPU-mapped with no export path, and `ccdec` never exercises it — its
pool is hardcoded to GBM regardless of `--frame-memory`.

This patch is useful beyond ghostframe: it is what any mainline-kernel SoC
without ChromeOS's GBM needs, which is most of them.

### 6.4 ghostframe allocates the frames, and imports them once

cros-codecs takes a caller-supplied `alloc_cb` and returns
`PooledVideoFrame<GenericDmaVideoFrame>`. `GenericDmaVideoFrame::new(Vec<File>,
FrameLayout)` is public, and so are `FrameLayout` and `PlaneLayout`
(`buffer_index`, `offset`, `stride`, `format: (Fourcc, u64 modifier)`).

`GenericDmaVideoFrame`'s own `dma_handles`/`layout` fields are private with no
public accessor, which would be a problem if ghostframe had to *read* the fd
back out. It does not: ghostframe constructs the frames, so it already knows
every fd and layout and can build `DmabufPlanes` from its own bookkeeping.

That inverts the current per-frame import into a one-time one:

1. At pool setup, obtain N NV12 dmabufs (§6.3.2) and record fd + layout.
2. Import each **once** into GLES — `EGLImage` + texture — keyed by fd.
3. Per decoded frame, look up the already-imported texture by pool index.

The Vulkan/VA-API path imports a fresh dmabuf every frame. This is less work per
frame and removes a class of fd-lifetime bug: the `DmabufPlanes` doc comment at
`descriptor.rs:64-76` warns at length that an importer must `dup()` because
`vkImportMemoryFdKHR` takes ownership and a double close against the `AVFrame`'s
unref is silent on some drivers. Importing once from fds ghostframe owns for the
pool's lifetime sidesteps that entirely.

### 6.5 The NV12 UV offset must come from the driver, not arithmetic

rkvdec reports `num_planes=1`, `bytesperline=640`, `sizeimage=614400` for
640×480 — against 460800 for packed NV12. The buffer is height-padded, and
single-plane NV12 does **not** report the chroma offset in
`v4l2_pix_format_mplane`.

Getting this wrong yields a plausible-looking image with shifted chroma rather
than an error. It is also not hypothetical: it is what made the first
hardware-vs-software md5 comparison differ during investigation, before
`videoconvert` (which honours `GstVideoMeta`) was inserted.

The offset must be derived from the queue's reported `bytesperline` and the
driver's aligned height and then **asserted against a known-good frame**, not
computed from the visible resolution.

---

## 7. The capability probe loses its independent ground truth, and must regain it

`probe.rs` decides whether to advertise H.264 in HELLO, and its doc is explicit
about why it does a real decode rather than trusting `avcodec_get_hw_config`:
those calls "only prove libavcodec was *built* with a VA-API hwaccel for H.264,
not that the driver has an H.264 decode profile". Sessions begin in H.264 mode,
so a false positive is a black window.

`oracle_tests.rs` gates on `vainfo_reports_h264_vld` — a ground truth
independent of the crate's own probe. The V4L2 replacement must preserve both
properties:

- **The probe** decodes a small embedded clip through rkvdec and checks a frame
  came out, same as today.
- **The independent truth** becomes `VIDIOC_ENUM_FMT` on the candidate device's
  output queue reporting `V4L2_PIX_FMT_H264_SLICE` (`S264`) — computed without
  going through `cros-codecs`, so it can still disagree with the probe.

A `false` here is not an error: the server falls back to tile codecs, which is
what every session did before M3. On this hardware that fallback is the common
case for any non-H.264 content and is the path that matters most (§9).

---

## 8. Testing

**The existing oracles are the regression net and must pass unchanged.** The
tests that hand-construct `DmabufPlanes` with no AVFrame behind them
(`descriptor.rs`'s literal-construction path, `nv12_oracle_tests.rs`) are
backend-agnostic by construction. If they need editing, the seam has been
broken — treat that as a design failure, not a test update.

**New: a bit-exactness test for hardware decode.** Decode a fixture clip through
the V4L2 path and compare against a software-decoded golden, stride-normalised.
This was verified by hand during investigation (identical md5 over 60 frames)
and should not stay a one-off: it is the only test that would catch §6.5 going
wrong.

**Break it and watch it fail**, per AGENTS.md. Specifically: perturb the chroma
offset by one row and confirm the bit-exactness test fails. A test that passes
with a deliberately wrong UV offset is testing nothing, and §6.5 is exactly the
kind of defect that produces a passing-but-wrong result.

**Prove the GLES path actually ran.** `blit_h264_frame` already logs which
import path was taken. The GLES equivalent needs the same, and the test must
assert on it — a GPU test that silently fell back to a CPU copy, or skipped,
reports success indistinguishably from real success.

**What CI cannot cover.** Runners have neither a Mali GPU nor rkvdec, so the
GLES and V4L2 targets cannot run there — the same position `gpu_export` and
`gpu_h264_render` are already in (see the `browserless` job comment in
`.github/workflows/e2e.yml`). Two consequences:

- The shader-limit change in §5 *is* CI-testable under Vulkan and must be,
  because it affects the browser client.
- `cargo check --features gles` must be in CI even though the tests cannot run,
  so the GLES backend cannot rot uncompiled. A feature nobody builds is a
  feature that is already broken.

---

## 9. Performance expectation, stated up front

Mali-T860, 4 cores at 500 MHz, is a weak GPU. rkvdec handles H.264 in hardware
so decode is close to free, but the CDF53 inverse-wavelet and palette-RLE
compute passes land on that GPU, with §5's smaller workgroups.

The honest expectation: comfortable for text and mostly-static content with
partial tile updates — which is the common remote-desktop case and the one
ghostframe's tile codecs are designed for — and tight on full-screen motion.

This should be measured, not assumed, and `ghostframe-bench` is the place. If
colour conversion turns out to dominate, `/dev/video2` is `rockchip-rga`, a 2D
blitter, available as a cheap fallback. Do not design for that until a
measurement asks for it.

---

## 10. Measured baseline

All from the reference machine, 2026-09-25. Probes are in the session
scratchpad; they should be committed alongside the implementation so these
numbers can be re-derived rather than trusted.

| Fact | Value | Probe |
| --- | --- | --- |
| GPU | `mali-t860 id 0x860`, 500 MHz, 4 cores | `dmesg` |
| GL | `OpenGL ES 3.1 Mesa 24.0.2-panfrost.1.1`, GLSL ES 3.10 | `es2_info` |
| Compute invocations | **128** | `glprobe.c` |
| Compute SSBO blocks | **8** (7 needed) | `glprobe.c` |
| Shared memory | 32768 | `glprobe.c` |
| EGL dmabuf export | `EGL_MESA_image_dma_buf_export` present | `eglinfo` |
| EGL dmabuf import | `EGL_EXT_image_dma_buf_import` + `_modifiers` | `eglinfo` |
| Vulkan | none; only ICD is a 2021-07-23 PanVK build, loader iface v3 < v5 | `vulkaninfo` |
| rkvdec | `/dev/video3`, `S264`+`VP9F` in, `NV12` out | `v4l2-ctl` |
| hantro dec | `/dev/video1`, `MG2S`+`VP8F` — no H.264 | `v4l2-ctl` |
| EXPBUF on rkvdec | 4 dmabuf fds; `bytesperline=640 sizeimage=614400 planes=1` | `expbuf_probe.c` |
| GBM NV12 | unsupported, every flag; XRGB8888 OK, modifier `0x0` | `gbmprobe.c` |
| dma-heap | absent (`/dev/dma_heap` missing); CmaTotal 262144 kB | `ls`, `/proc/meminfo` |
| GStreamer HW decode | bit-exact vs software, 60 frames, dmabuf-backed — **at 640x480 only; see the row below** | `gst-launch`, `dmabuf_probe.py` |
| GStreamer at 1080p | **NOT dmabuf on 1.22.10**: `SystemMemory`, 3110400 bytes, i.e. a full-frame CPU copy per frame. Needs GStreamer ≥ 1.24.1 (`v4l2codecs: decoders: Add DMA_DRM caps support`) to require dmabuf at all | `gst-dmabuf-rs` |
| FFmpeg | 6.1.1, no `v4l2request` hwaccel, `h264_v4l2m2m` only | `ffmpeg -hwaccels` |
| FFmpeg, packaged | **no distro build enables `--enable-v4l2-request`** — checked against Arch Linux ARM's PKGBUILD at 9.0.2, so this is not a version problem that upgrading fixes | ALARM `PKGBUILD` |
| cros-codecs | `0.0.6`, `v4l2` feature builds, ~12 deps, 3m48s | `cargo build` |

---

## 11. Risks

**The §6.3.2 patch is the schedule risk.** It is the only piece with no working
precedent inside cros-codecs, and the estimate (150–250 lines) is the least
certain number in this document. Mitigation: it is sequenced first (§12) and the
GStreamer path (§6.2) is a proven fallback that needs no upstream work.

**GLES storage-texture formats are narrower than Vulkan's.** `framebuffer.rs`
may use a format combination wgpu's GL backend does not support. Not resolvable
by inspection; the §12 step-1 spike settles it.

**Thread affinity may bite late.** §4.5's failure mode is load-dependent. The
debug assertion is cheap insurance and should go in with the first GLES code,
not after the first mystery.

**Mesa 24.0.2 is old.** Manjaro ARM ships it and there is no newer package.
Anything requiring a later Mesa is out of reach on the reference machine
regardless of merit — this is what rules out lavapipe's dmabuf support, and it
could rule out other things.

**cros-codecs is 0.0.x, and upstream is dormant.** Pinning defers the churn
rather than solving it. The mitigation this document originally claimed —
"an upstreamed §6.3.2 reduces the carried delta to zero" — **is no longer
available**: `chromeos/cros-codecs`' last commits are Gerrit merges from March
2025, its last push June 2025, 0.0.6 is still the newest published version, and
all five defects we carry patches for are still present at HEAD. Contributing
also requires a Google CLA.

Corrected mitigation: the delta is a readable, re-appliable series against a
frozen version (`third_party/cros-codecs-patches/`), and a pinned dependency that
never changes is also one that never breaks underneath us. The published forks
were surveyed and are worse rather than better — see that README. The exit, if
the delta ever becomes painful, is §6.2's GStreamer path, already verified
bit-exact on the reference machine.

---

## 12. Sequencing

1. **The EXPBUF `VideoFrame` for cros-codecs** (§6.3.2), with §6.3.1 alongside.
   First because it is the highest-uncertainty item and because it is provable
   standalone — bit-exactness against a software golden, no renderer involved.
   If it fails, §6.2 is the fallback and nothing downstream has been built on
   sand.
2. **Shader workgroup lowering to ≤128** (§5). Independent of everything else,
   verifiable today on x86 under Vulkan, and it must be benchmarked for the
   desktop-regression question in §5.1.
3. **GLES `wgpu_ctx.rs` spike** (§4.1, §4.6, §5.2) — adapter, device, corrected
   limits. Settles the storage-format risk before the larger files are touched.
4. **GLES `export.rs` / `import.rs`** (§4.2, §4.3).
5. **Probe replacement** (§7) and wiring decode to render (§6.4).

Steps 1 and 2 are independent and can proceed in parallel; 3 gates 4 and 5.
