# `gst-dmabuf-rs` — does GStreamer hand out a dmabuf we can actually import?

A standalone probe, not part of the workspace build. It exists because
"GStreamer decodes H.264 in hardware on this box" was already known, and is the
wrong question. The question is whether the decoded buffer arrives as a **dmabuf**
with a layout precise enough to build a `DmabufPlanes` from — one fd, luma and
chroma offsets, row pitches, total size — because that is what `import_gles.rs`
needs and what `ghostframe-client-h264`'s cros-codecs path had to derive by hand.

It reads the decoded NV12 back **through the exported fd** and writes I420, so a
byte comparison against `ffmpeg -pix_fmt yuv420p` validates the fd and the layout
together, not just that something decoded.

## What it measured, 2026-09-27, GStreamer 1.22.10 / rkvdec

```
640x480    layout: fd 10, offsets [0, 307200], strides [640, 640], size 614400
           60 frames over 11 distinct dmabuf fds — bit-exact through the fd

1920x1080  NOT dmabuf-backed: 3110400 bytes, allocator SystemMemory
```

Two conclusions, and the second is the reason this probe is committed rather than
thrown away:

**The good part.** `offsets[1] = 307200` is GStreamer reporting the chroma offset
itself, from `GstVideoMeta`. That is exactly the value the cros-codecs path had to
compute as `bytesperline * coded_height` and got wrong twice — so on this route
the whole chroma-offset trap belongs to GStreamer, not to us.

**The blocking part.** At 1080p — the resolution sessions actually run at — 1.22
silently hands over system memory instead, i.e. a full-frame CPU copy per frame,
which is precisely the cost the dmabuf design exists to avoid. Three levers were
tried and none of them helps:

- Requiring `video/x-raw(memory:DMABuf)`. `v4l2slh264dec` on 1.22 does not
  advertise the feature, so naming it fails to **link** rather than to negotiate.
  `dmabuf_probe.py`'s header already recorded this; it was rediscovered the slow
  way, which is a decent argument for reading sibling probes first.
- Enlarging the appsink pool past the 11 buffers the decoder cycles.
- Pinning `format=NV12`, in case a tiled format (`NV12_4L4`, `NV12_32L32`,
  `NV12_16L32S` are all offered) was being converted through the CPU.

GStreamer **1.24.1** added *"v4l2codecs: decoders: Add DMA_DRM caps support"*,
which is the mechanism for *requiring* dmabuf. So this route carries a hard
runtime floor of 1.24.1 and cannot be taken on an older stack.

## Two API lessons worth keeping

**Absence of `GstVideoMeta` is not an error.** It means "standard packing for
these caps", which is what `VideoInfo::from_caps` computes. Treating a missing
meta as a failure rejected every 1080p buffer before the system-memory problem
was even visible — the first, wrong diagnosis was "1080p has no metadata".

**Read the bus.** Without draining it, a negotiation failure is completely
silent: `try_pull_sample` returns `None`, the probe reports zero frames, and
nothing says why. The link failure above was invisible until the bus was checked.

## Running it

Needs `gstreamer`, `gst-plugins-base` and `gst-plugins-bad` (for `h264parse` and
the `v4l2codecs` plugin) plus their development headers.

```sh
ffmpeg -f lavfi -i "testsrc2=size=640x480:rate=30:duration=2" \
       -c:v libx264 -preset veryfast -profile:v high -pix_fmt yuv420p \
       -bsf:v h264_mp4toannexb -f h264 clip.h264
ffmpeg -i clip.h264 -pix_fmt yuv420p -f rawvideo sw.i420

cargo run --release -- clip.h264 hw.i420
cmp hw.i420 sw.i420

# And the case that matters, which is NOT the same answer:
ffmpeg -f lavfi -i "testsrc2=size=1920x1080:rate=30:duration=1" \
       -c:v libx264 -preset veryfast -profile:v high -pix_fmt yuv420p \
       -bsf:v h264_mp4toannexb -f h264 clip1080.h264
cargo run --release -- clip1080.h264 hw1080.i420
```

A `NOT dmabuf-backed` line is the probe working correctly and telling you this
stack cannot do zero-copy at that resolution. It is not a probe failure.
