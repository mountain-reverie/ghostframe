# H.264 to screen on Mali: the GLES NV12 import

**Date:** 2026-10-08
**Hardware:** Pinebook Pro — RK3399, Mali-T860, kernel 6.7.9, Mesa 25.1.7
**Closes:** Task 9 of the GLES/V4L2 plan, and the last gap between "H.264
decodes" and "H.264 is displayed"
**Follows:** #108 (GStreamer decode)

`import_gles.rs` had been a loud stub since #102, which is why
`NV12_IMPORT_IMPLEMENTED` was `false` and the client correctly declined to
advertise H.264 at all. It is implemented now: a decoded frame reaches the
framebuffer through `eglCreateImageKHR` + `glEGLImageTargetTexture2DOES` +
`texture_from_raw`, zero-copy, on hardware with no Vulkan driver.

---

## 1. Two departures from what the plan specified

**No `dup()` of the incoming fd.** The plan said "the caller must `dup()` the
incoming fd per plane", carried over from the Vulkan path where
`vkImportMemoryFdKHR` *takes ownership* and a double close is silent on some
drivers. EGL does not take the fd — it takes its own reference to the
underlying buffer — so there is nothing to dup and nothing to close.
`export_gles.rs` already had this written down ("EGL does not take ownership of
the fd"); the plan's instruction was inherited from the wrong backend.

**No import-once-per-pool-frame cache.** §6.4 wanted each pool buffer imported
once at setup and looked up thereafter. The textures are imported per decoded
frame instead, because a cache keyed by fd needs invalidating when the decoder's
pool is rebuilt and fds are recycled — and "stale entry keyed by a recycled
identifier" is exactly the defect that cost two debugging sessions on this
project already (the dmabuf-per-frame-object bug, and the stale device
override). What *is* cached is the five EGL/GL entry points per plane, on the
context, because resolving them ten times per frame is the per-call cost this
client has already measured hurting it once.

## 2. The fencing question, asked and answered

`renderer.rs`'s `blit_h264_frame` carried a long note saying the dma-buf
hand-back was safe on amdgpu/RADV through the kernel's implicit fencing, that
this was *measured/assumed driver behaviour*, and that **"a future hardware
target should check for that fencing explicitly before trusting this path."**

This is that target, and the answer is **no**. V4L2 returns a buffer to its pool
on release with no fence a GPU reader can wait on, and EGL's dmabuf import takes
no fence either. Nothing orders the decoder's next write against our draw. The
failure would not be an error: it is a frame with a band of the *next* frame in
it, with no diagnostic attached.

So the zero-copy path now waits for its own submission before the frame is
released — `wait_for_submission`, waiting on the specific `SubmissionIndex`
rather than "the latest", because the tile codecs share this queue and an
unqualified wait would bill their work to this frame.

### What that costs, measured

1920x1080, paced 60Hz, 120 frames:

| | |
| --- | --- |
| p5 | 2.5 ms |
| p50 | **3.8 ms** |
| p90 | 5.7 ms |
| p99 | 13.7 ms |
| max | 20.8 ms |

The p99 and max are frames 2 and 3 — pipeline warmup, not steady state. The
median is ~11% of a 33ms frame budget.

**Most of that is the blit, not the fence.** 2M pixels of NV12→RGBA on a 500MHz
4-core Mali is real work; what the wait actually costs is the pipelining it
gives up. Affordable at 30fps, tight at 60.

**The optimisation, identified and not taken:** defer the frame's release by one
instead of waiting. A frame-time of slack is longer than any measured blit, so
the buffer would be safe with no stall at all, at the cost of holding one more
of the ~11 pooled buffers. It trades an explicit guarantee for a temporal one,
which deserves its own measurement rather than being bolted on at the end of
the work that found the problem.

The Vulkan path keeps its implicit-fencing assumption — different driver stack,
not re-measured. That is now stated as "not re-measured" rather than as safe.

## 3. The oracle, and the mutations that prove it

`tests/gpu_import_gles.rs`. The question is not "did I get textures of the right
size" — a wrong luma offset renders a plausible picture of the wrong memory. So
it writes known patterns into a staged dmabuf and reads the imported textures
back through the GPU.

Two deliberate choices make it able to fail:

- **The luma offset is not 0.** A real NV12 buffer's usually is, which lets an
  import that hardcodes 0 pass unnoticed. A reserved region ahead of luma,
  filled with `0xAB`, turns that bug into a readback of the filler — and the
  assertion says so by name.
- **The pitch is wider than the width** (128 against 64). With pitch == width,
  an import that strides by width instead of pitch is invisible.

**Mutations run, not claimed:**

| mutation | result |
| --- | --- |
| pass `0` for `EGL_DMA_BUF_PLANE0_OFFSET_EXT` | FAILS: *"luma (0,0) read back 0xab, expected 0x00 -- that is the reserved filler, so the plane offset was ignored"* |
| pass `width` for `EGL_DMA_BUF_PLANE0_PITCH_EXT` | FAILS |

Both reverted after measuring. Plus negative cases: a tiled modifier, a plane
past the end of the allocation, and a composed `DRM_FORMAT_NV12` fourcc are each
refused rather than imported and sampled wrongly.

## 4. A refactor that came with it

`export_gles.rs` already made these same four calls — it allocates through GBM
and then *imports* the result to get a texture. The EGL/GL declarations,
constants and loaders moved to `egl_ffi.rs` and both directions share them. The
export path's 13 GPU tests pass unchanged, which is what makes the move safe to
believe.

## 5. Verification

On the reference machine:

| suite | result |
| --- | --- |
| `gpu_import_gles` | **5 passed** — byte-exact readback at a non-zero offset, plus three refusals |
| `gpu_h264_render` | 1 passed, logging `H.264 import path: zero-copy dmabuf` |
| `gpu_export` | 13 passed — unchanged by the FFI extraction |
| `ghostframe-client-h264` (GStreamer) | 11 passed, bit-exact at 640x480 and 1080p |

## 6. Still open

- ~~`nv12_oracle tier_b` still fails~~ **Diagnosed and closed** — see the design
  doc's new §9.2a. It was a rounding-threshold difference in Mali's
  `f32 -> unorm8` write: 0.1137% of samples one step high, always up, always
  within 0.002 of a `.5` boundary, with Tier A still bit-exact so the shader
  arithmetic was never in question. No model reproduced it; notably
  `round(v * 256)` fitted every deviating sample perfectly and matched only 71%
  of the rest, which is what fitting to the failures looks like from the inside.
  Tier B now tolerates one step on a bounded share in one direction, verified
  still to catch the textbook-BT.601 mutation that §9.2 rejected plain
  tolerances over.
- **No live session yet.** Every claim here is from tests. H.264 has never been
  seen on a screen end to end, and the BGRA fix from #102 has not been seen
  either.
- **Throughput end to end is unmeasured.** The fence cost above is a
  measurement of one stage, not of achievable fps with H.264 active.
