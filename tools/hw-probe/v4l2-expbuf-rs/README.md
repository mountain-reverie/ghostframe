# `v4l2-expbuf-rs` — does cros-codecs decode H.264 into an exportable dmabuf?

A standalone probe, not part of the workspace build. It answers the one
question the GLES/V4L2 design called its schedule risk: can ghostframe get a
**dmabuf** out of `cros-codecs` on a mainline-kernel SoC, where GBM cannot
allocate NV12 at all?

It can. This probe decodes H.264 on rkvdec, exports each driver-allocated
capture buffer with `VIDIOC_EXPBUF`, reads the NV12 back **through the exported
fd**, and converts to I420 for comparison against a software golden. On a
Pinebook Pro (RK3399, mainline 6.7) the output is byte-identical to `ffmpeg`'s
at both 640x480 and 1920x1080.

See `docs/superpowers/investigations/2026-09-26-h264-v4l2-expbuf-feasibility.md`
for the measurements and what they change in the plan.

## The point of it

`V4l2ExpbufVideoFrame` in `src/main.rs` is a `cros_codecs::video_frame::VideoFrame`
implemented **downstream of the crate**. That is the finding that matters: the
decode-side frame type does not need a cros-codecs fork, only the two one-line
patches in `cros-codecs-0.0.6.patch`. It is written to be lifted into
`ghostframe-client-h264` more or less as-is.

Three things in it are load-bearing, and each was found by watching the probe
fail:

- **`num_planes()` returns 1, the V4L2 plane count, not 2.** rkvdec reports one
  plane for NV12 — one buffer holding both — and `queue_with_handles` rejects a
  QBUF whose handle count disagrees (`NumPlanesMismatch(2, 1)`). The trait
  conflates "logical planes" with "V4L2 planes", which is why upstream's
  `V4l2MmapVideoFrame` just `todo!()`s on every contiguous format.
- **The dmabuf is keyed by V4L2 buffer index, not by frame object.** With
  `V4L2_MEMORY_MMAP` the driver owns the pool and hands an index out at dequeue
  time, so one frame object sees different indices over its life. Caching the fd
  of the first index it ever saw makes it read whatever picture now occupies that
  buffer — which reads as a *frame-ordering glitch*, not as corruption: every
  frame looks like a real frame, just the wrong one. This cost 40 of 60 frames
  before it was fixed, and no single frame looked wrong.
- **The chroma offset is `bytesperline * format.height`**, the driver's coded
  height — never derived from `sizeimage`, which is scratch-padded (614400 for a
  640x480 NV12 whose pixels occupy 460800), and never from the display height,
  which differs when it isn't 16-aligned (1080 display vs 1088 coded).

## Running it

```sh
# 1. Unpack cros-codecs 0.0.6 and apply the two patches.
cargo fetch   # anywhere, to populate ~/.cargo/registry
cp -r ~/.cargo/registry/src/*/cros-codecs-0.0.6 /tmp/cros
( cd /tmp/cros && patch -p1 < .../tools/hw-probe/v4l2-expbuf-rs/cros-codecs-0.0.6.patch )

# 2. Point this crate's path dependency at it (Cargo.toml expects ../cros).
# 3. A clip and a software golden to compare against.
ffmpeg -f lavfi -i "testsrc2=size=640x480:rate=30:duration=2" \
       -c:v libx264 -profile:v high -pix_fmt yuv420p \
       -bsf:v h264_mp4toannexb -f h264 clip.h264
ffmpeg -i clip.h264 -pix_fmt yuv420p -f rawvideo sw.i420

# 4. Decode on the hardware decoder and compare.
CROS_CODECS_V4L2_DEVICE=/dev/video3 RUST_LOG=info \
  cargo run --release -- clip.h264 hw.i420
cmp hw.i420 sw.i420
```

`CROS_CODECS_V4L2_DEVICE` is what the first patch adds; without it the crate's
scan picks `/dev/video0`, the hantro *encoder*, and decode fails with
`Unrecoverable decoding error`.

**The mutation check is not optional.** A bit-exactness test that passes with a
deliberately wrong chroma offset is testing nothing, and that is precisely the
defect class here — a wrong offset yields a plausible image with shifted colour,
not an error. So:

```sh
CHROMA_SHIFT_ROWS=1 cargo run --release -- clip.h264 bad.i420
cmp bad.i420 sw.i420   # MUST differ
```

Measured: `CHROMA_SHIFT_ROWS=0` matches the golden, `=1` differs.
