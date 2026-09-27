# Hardware H.264 on rkvdec: the schedule risk is gone

**Date:** 2026-09-26
**Hardware:** Pinebook Pro — RK3399, Mali-T860, mainline 6.7, Mesa 24.0.2
**Follows:** PR #102 (`native-client-arm-gles-v4l2`), which landed the GLES render path
**Plan:** [`../plans/2026-09-25-native-client-gles-v4l2.md`](../plans/2026-09-25-native-client-gles-v4l2.md) — Tasks 3–6, 9
**Probe:** [`tools/hw-probe/v4l2-expbuf-rs/`](../../../tools/hw-probe/v4l2-expbuf-rs/), [`tools/hw-probe/nv12import_probe.c`](../../../tools/hw-probe/nv12import_probe.c)

The plan said, of Task 4: *"the only piece with no working precedent inside
`cros-codecs` and the only estimate in this plan that could be wrong by a factor
of two."* It has a working precedent now. **H.264 decodes on rkvdec through
cros-codecs, into dmabufs, byte-identical to a software golden at 640x480 and at
1920x1080.** Everything downstream of that was already settled by reading; this
is what was not.

---

## 1. What was measured

| Question | Answer | How |
| --- | --- | --- |
| Can a dmabuf `VideoFrame` be written **downstream** of cros-codecs? | **Yes** — no fork for the frame type | `V4l2ExpbufVideoFrame` compiles and runs against `cros-codecs = { default-features = false, features = ["v4l2"] }` |
| Does rkvdec decode through it? | **Yes**, 60/60 frames | `v4l2-expbuf-rs` vs `ffmpeg` software decode |
| Is the output bit-exact? | **Byte-identical**, 640x480 and 1920x1080 | `cmp` against `ffmpeg -pix_fmt yuv420p` |
| Is the decoded NV12 linear, or tiled? | **Linear.** No detiling needed | Luma bytes match the software golden exactly at stride 640 |
| Will Mali's EGL import the planes? | **Yes** — `R8`, `GR88` *and* `NV12` all importable | `nv12import_probe.c`, 50 formats enumerated |
| How many EXPBUF calls per session? | **One per buffer index** (3), not one per frame | probe log |

The cros-codecs patches this needs grew from two to **five** as the backend was
built and CI exercised it; they live in `third_party/cros-codecs-patches/`, with
that README recording which defect each one fixes and why the delta is long-lived
(upstream's last commit is March 2025).

## 2. Three traps, each found by watching the probe fail

Recorded because each one produces *plausible* output, which is the expensive
kind of wrong.

### 2.1 `num_planes()` means V4L2 planes, not logical planes

rkvdec reports `num_planes=1` for NV12 — one buffer holding both planes — and
`v4l2r`'s `queue_with_handles` rejects a QBUF whose handle count disagrees with
the queue: `NumPlanesMismatch(2, 1)`. But `VideoFrame::num_planes()`'s default
returns 2 for NV12, and `BufferHandles::len()` forwards to it.

This is why upstream's `V4l2MmapVideoFrame::new` opens with
`todo!("Contiguous formats are not currently supported for MMAP!")` — the trait
conflates two different meanings of "plane" and the MMAP path hit it first. The
fix is to override `num_planes()` to the V4L2 count while
`get_plane_size`/`get_plane_pitch`/`map` stay two-entry, because *those* are the
logical planes.

### 2.2 `PooledVideoFrame` silently discards defaulted-method overrides

`PooledVideoFrame` delegates `fourcc`, `resolution`, `get_plane_size`,
`get_plane_pitch`, `map`, `map_mut`, `fill_v4l2_plane`, `process_dqbuf` — and
**not** `num_planes`. So the override from §2.1 works until the frame goes into
a pool, and then quietly stops working. One line to fix, and it is the kind of
defect worth reporting upstream: any future defaulted method has the same hole.

### 2.3 The dmabuf belongs to the buffer index, not to the frame object

The one that cost real time, and the reason this document exists.

With `V4L2_MEMORY_MMAP` the **driver** owns the buffer pool; a frame object is
just a handle that gets assigned an index at dequeue time. So one frame object
sees *different* indices over its life. Exporting once per frame object and
caching that fd means the frame later reads whatever picture now occupies the
buffer it saw first.

The symptom is not corruption. Every frame is a real, correctly-decoded frame —
just the wrong one:

```
golden index of each of my frames:
0 1 2 3 4 6 7 8 9 10 9 10 11 12 13 15 16 17 18 19 21 22 23 24 25 24 25 ...
```

40 of 60 frames wrong, zero of them *looking* wrong: pixel content was a subset
of the golden's frames throughout (`in mine not golden: 0`). A reviewer eyeballing
the video would call this a stutter and go looking in the wrong place.

Keyed by index instead — `HashMap<u32, Arc<File>>`, exported on first sight of
each index — the output is byte-identical. **The GPU import must be keyed the
same way**, which sharpens the plan's §5.1 "import once per pool frame" into
*import once per V4L2 buffer index*.

## 3. The chroma offset, settled

The plan flagged this as *"not arithmetic"* and it was right to, but the rule is
now measured rather than feared:

```
chroma_offset = bytesperline * format.height      // the driver's coded height
```

Two things it is **not**:

- **Not from `sizeimage`.** rkvdec reports `sizeimage=614400` for 640x480 NV12
  whose pixels occupy 460800; the remaining 153600 bytes are zero scratch.
  Deriving a height from `sizeimage / bytesperline / 1.5` gives 640, not 480.
- **Not from the display height**, whenever that isn't 16-aligned. At 1080p the
  probe reports `display 1920x1080, coded 1920x1088`; the chroma plane starts at
  `1920 * 1088`. The 640x480 case is the one where display and coded agree,
  which is exactly why testing only at 640x480 would have proved nothing here.

Mutation-checked: `CHROMA_SHIFT_ROWS=1` (one row of chroma offset) makes the
comparison fail, `=0` makes it pass.

## 3b. Two more upstream gaps, and one hardware fact, found while building it

Added after the feasibility probe, while turning it into
`ghostframe-client-h264`'s `v4l2` backend.

### `DecodedHandle::is_ready` is `todo!()`

`backend/v4l2/decoder/stateless.rs:98`. So a caller cannot ask whether a decoded
frame is complete without panicking, which is unfortunate because the answer
would let it avoid `sync()`'s blocking path entirely. Related: `V4l2Device::sync`
gives a queued request ~250 ms and then
`panic!("there should not be a scenario where a queued frame is not returned.")`
— upstream code on our render thread. Neither is fatal to this work; both are
candidate patches, and the decoder logs at TRACE before it can block so the
panic has a precursor.

### `cros-codecs`' `v4l2` feature does not compile off aarch64

`image_processing.rs:15` is `#[cfg(feature = "v4l2")] use std::arch::aarch64::*;`
— gated on the **feature**, not the architecture — so the MM21 NEON detiling path
is unconditional whenever `v4l2` is on. Upstream targets ChromeOS ARM devices,
where the two coincide; on an x86-64 runner the build dies with
`could not find aarch64 in arch` from inside the dependency.

Found by CI, not locally, because every local build of this was on aarch64. The
same trap AGENTS.md already records for the aarch64 clippy lint, from the other
direction.

Two consequences, and the second is the expensive one:

1. **The `v4l2` backend is aarch64-only as published.** Not inherently — nothing
   in the V4L2 Request API or in `v4l2_frame.rs` is ARM-specific — so fixing the
   gate upstream would lift it. But CI builds cros-codecs from crates.io, so a
   *carried* patch cannot make an x86 build work either. `ghostframe-client-h264`
   now carries a `compile_error!` naming the reason, so the attempt fails with a
   sentence instead of with a dependency's arch error.
2. **The decode backend must be a separate feature axis from the GPU backend.**
   It was briefly folded into `gles`, on the reasoning that the machine with no
   Vulkan driver is the machine with no VA-API driver. That reasoning is sound
   about hardware and wrong about CI: cargo features are additive, so a coupled
   `gles` leaves **no flag combination** that compiles the GLES render path on an
   x86 runner. It silently destroyed the only guard that path has — the one PR
   #102 added precisely because no runner has a Mali GPU. `gles,decode-vaapi` is
   a combination no real machine runs, and that is its entire purpose.

Cross-compiling (`--target aarch64-unknown-linux-gnu`) would check the real code
on an x86 runner and is the better answer if anyone wants it, but `v4l2r` runs
bindgen in its build script, so it needs a target sysroot and libclang wired into
CI first.

### `/dev/videoN` numbering is not stable across boots

**rkvdec moved from `/dev/video3` to `/dev/video1` over a single reboot**,
swapping places with the hantro decoder. A recorded
`CROS_CODECS_V4L2_DEVICE=/dev/video3` therefore started pointing at a driver
that advertises MPEG-2 and VP8 and no H.264 at all, and the failure surfaced four
layers down as `driver does not support S264`.

Three consequences:

1. **A written-down node number is a latent bug**, not configuration. Discover it
   by enumerating `S264` — which is why `probe::default_device()` is a function
   and not a constant.
2. **"An override is set" is not evidence it is right.** The decoder's startup
   check originally asked "does the scan agree, *or* is an override set?", which
   accepts a stale override; it now predicts the node cros-codecs will actually
   open (`v4l2_device::device_cros_codecs_will_open`) and compares that.
3. **It strengthens the case for the upstream-correct fix.** Filtering by coded
   format during enumeration has no stale state to go wrong. The explicit-path
   patch is smaller and is what the upstream TODO asks for, but it inherits this
   fragility.

Worth noting what this boot also showed: with rkvdec at `/dev/video1` it is the
first node with an OUTPUT mplane queue, so the **unpatched** scan picks correctly
and the whole suite passes with no override at all. That is luck, not a fix — and
it is the kind of luck that makes a device-selection bug look intermittent.

## 4. What this changes in the plan

| Plan said | Now |
| --- | --- |
| Task 4 is a `cros-codecs` fork, "150–250 lines", highest uncertainty | ~200 lines of **downstream** ghostframe code, working, with a runnable reference |
| Two fork patches needed | Still two, but **35 lines total**, and the second is a different one than expected (`frame_pool`, not queue wiring) |
| "GBM cannot allocate NV12, and there is no `/dev/dma_heap`" | Unchanged and still the reason for this whole approach — the driver allocates instead |
| Chroma offset a hazard to be asserted | Rule measured at two resolutions, mutation-checked |
| Task 9 import risk unknown for NV12 planes | `R8`/`GR88`/`NV12` all confirmed importable by Mali's EGL |
| `V4l2MmapVideoFrame` "exists but is CPU-mapped with no export path" | True, and it also `todo!()`s on every contiguous format — do not start from it |

One item the plan did not anticipate: `StreamInfo.format` comes back as **MM21**
regardless, hardcoded at `src/backend/v4l2/decoder/stateless.rs:134`. Harmless
here — ghostframe constructs its own frames as NV12 and reads them by its own
layout — but do not route any decision through that field.

## 5. Still open

- **The decoder's own frame pool is driver-owned, so `n_export_buffers: 3` does
  not transfer.** rkvdec was asked for 5 and allocated 7 (cros-codecs adds 2:
  *"+2 due to HCMP1_HHI_A.h264 needing more"*). The import table is sized by what
  the driver gives, not by a ghostframe constant.
- **Throughput is unmeasured.** This probe optimises for correctness, and it
  CPU-maps every frame to compare bytes — which production never does. No claim
  about fps is made here.
- **No `DMA_BUF_IOCTL_SYNC` in the probe.** Reads came back correct without it
  on this platform, but production imports into the GPU rather than the CPU, so
  the question does not arise on the real path.
- **Task 6's probe replacement is untouched.** `VIDIOC_ENUM_FMT` reporting
  `S264` on the candidate device's output queue is the independent ground truth
  that must *not* go through cros-codecs; `expbuf_probe.c` already shows it is
  enumerable.
