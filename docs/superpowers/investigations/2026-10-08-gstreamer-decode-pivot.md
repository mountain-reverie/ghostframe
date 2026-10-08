# Replacing the cros-codecs decode backend with GStreamer

**Date:** 2026-10-08
**Hardware:** Pinebook Pro — RK3399, Mali-T860, kernel 6.7.9, Manjaro unstable
**Stack:** GStreamer 1.26.5, Mesa 25.1.7, FFmpeg 7.1.1
**Replaces:** the `v4l2` backend added in #105, and the five patches in #106
**Probe:** [`tools/hw-probe/gst-dmabuf-rs`](../../../tools/hw-probe/gst-dmabuf-rs/)

The cros-codecs backend worked and was bit-exact. It was replaced anyway, because
what it cost was not the code but the five carried patches against a crate whose
upstream has been dormant since March 2025 — and because the alternative turned
out to be better on the merits, not merely better maintained.

---

## 1. What the pivot bought

| | cros-codecs | GStreamer |
| --- | --- | --- |
| Carried patches | 5, against a dormant upstream | **0** |
| Device selection | ours to get wrong; `/dev/videoN` is not stable across boots | GStreamer's |
| NV12 plane layout | derived by hand from `v4l2_pix_format_mplane` | **reported** by `GstVideoMeta` |
| DRM modifier | inferred from luma bytes matching a software decode | **reported** as `drm-format=NV12:0x0` |
| Buffer pool | ours to manage, keyed by V4L2 buffer index | GStreamer's |
| x86 CI compile check | **impossible** — the crate does not build off aarch64 | runs on every PR |
| Lines of backend code | 1116 (`decoder_v4l2` + `v4l2_frame` + `v4l2_device`) | 535 + 110 kept for the probe's ground truth |

The last two matter most. CI now compile-checks the ARM client's decode path on
an x86 runner, which it never could before — the previous backend gated a NEON
path on a Cargo feature rather than on the target architecture, so no flag
combination built it off aarch64. And the plane-layout arithmetic that produced
*plausible but wrong* output twice is simply not ours any more.

## 2. What it cost

A heavy runtime dependency (`gstreamer`, `gst-plugins-base`, `gst-plugins-bad`)
and a **hard version floor of GStreamer 1.24.1**, which is where `v4l2codecs`
gained DMA_DRM caps. Below that floor dmabuf cannot be *required*, and the
measured consequence on 1.22.10 was a silent per-frame CPU copy at 1080p and not
at 640x480 — the kind of defect that looks like a performance mystery. The
`v1_24` features on the gstreamer crates make the floor a link-time fact, and
`probe::MIN_GSTREAMER` states it in a log line.

## 3. Three negotiation requirements, none obvious

Each was a dead end first, and each failed somewhere other than its cause.
`tools/hw-probe/gst-dmabuf-rs/README.md` reproduces all three.

1. **Require the dmabuf *feature*; do not pin the format.** Adding
   `format=DMA_DRM` to the sink caps narrows the intersection and fails with a
   bare `not-negotiated (-4)`.
2. **The sink must advertise `GstVideoMeta` in `propose_allocation`.**
   `v4l2codecs` refuses outright — *"DMABuf caps negotiated without the mandatory
   support of VideoMeta"* — and reports it from `h264parse`, two elements
   upstream of the cause. A default `appsink` does not advertise it.
3. **`GstVideoMeta` carries the CODED size.** At 1080p it reports height 1088
   against a display height of 1080. Strides and offsets come from the meta;
   width and height come from the caps. Mixing them writes eight extra rows of
   luma — right shape, wrong content, and the first 1080p run did exactly that.

## 4. The pool constraint, which is a real API difference

**A caller must not hold more frames than the decoder's pool**, measured at ~11
buffers here. Each `HwFrame` pins one, so a caller holding a poolful stalls the
decoder that would produce the rest.

This is not a theoretical caveat. The first implementation had `decode()`
returning nothing on every call — a pull immediately after a push finds nothing,
because the pipeline is asynchronous — so all 16 frames of a test clip queued up
for `finish()`, where the seventh exhausted the pool and the drain deadlocked.
The oracle reported `7 passed` against a 16-frame golden.

Two things came out of that:

- `decode()` is now fed by `appsink`'s `new_sample` callback and drains a queue
  instead of pulling, so it never blocks and frames flow during decode. Waiting
  instead would have put the decoder's latency on the render thread — up to a
  frame budget per access unit, precisely when there is nothing to show.
- `finish()` is documented as "call until it returns empty", because returning
  the whole tail in one `Vec` is impossible for any stream longer than the pool.
  **This is the one place the two backends' contracts genuinely differ**: ffmpeg
  grows its frame pool on demand, a V4L2 pool is fixed at negotiation.

## 5. A test that was passing for the wrong reason

`frames_come_out_once_each_and_in_order` passed while the decoder was losing nine
of sixteen frames. Seven frames that happen to be golden `0..6` map onto
`0..hw.len()` perfectly, so the ordering assertion was satisfied by a truncated
run. It now asserts the count first, and the mutation check for that is recorded:
dropping the `finish()` tail makes it fail, where before it did not.

A reminder that "asserts a property rather than a magic number" is necessary but
not sufficient — the property has to be one a degraded run cannot satisfy.

## 6. Verification

On the reference machine, 11 tests, none skipping (checked with `--nocapture`):

- Hardware vs software **bit-exact at 640x480 and 1920x1080**, read back through
  the exported fd.
- Display size from caps, coded size from the meta, asserted structurally so it
  fails even on hardware where the two agree.
- Exported dmabuf LINEAR, planes inside the allocation.
- Every frame once, in order, with the count asserted.
- An executable mutation check: chroma read one row late must not match.

**Mutation checks run, not merely claimed:** perturbing the chroma offset by one
row fails exactly the two exactness oracles; truncating the `finish()` tail fails
the ordering test. Both reverted after measuring.

## 7. What did not change

- **Still no Vulkan** on Mesa 25.1.7 (`vulkaninfo`: *failed to detect any valid
  GPUs*), so the GLES render backend remains the only option.
- **Still no `v4l2request` hwaccel in FFmpeg 7.1.1**, confirmed against the
  installed binary. That route would have dropped the Rust dependency entirely
  and reused the existing ffmpeg decoder, but no distro build enables it.
- `DmabufPlanes` is untouched, again. Two backend replacements have now gone
  through that seam without changing it.

## 8. Open

- **Throughput is unmeasured.** Correctness only; the oracles CPU-map every frame
  to compare bytes, which production never does. No fps claim.
- **`import_gles.rs` is still a stub**, so `NV12_IMPORT_IMPLEMENTED` is still
  `false` and H.264 is correctly not advertised on the GLES build. That is the
  next piece.
- **`nv12_oracle tier_b` still fails on Mali**, untouched here.
