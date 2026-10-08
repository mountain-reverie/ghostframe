# `gst-dmabuf-rs` — does GStreamer hand out a dmabuf we can actually import?

A standalone probe, not part of the workspace build. It exists because
"GStreamer decodes H.264 in hardware on this box" was already known, and is the
wrong question. The question is whether the decoded buffer arrives as a **dmabuf**
with a layout precise enough to build a `DmabufPlanes` from — one fd, luma and
chroma offsets, row pitches, modifier, total size — because that is what
`import_gles.rs` needs.

It reads the decoded NV12 back **through the exported fd** and writes I420, so a
byte comparison against `ffmpeg -pix_fmt yuv420p` validates the fd and the layout
together, not merely that something decoded.

## Measured on GStreamer 1.26.5 / rkvdec, 2026-10-08

Both with the dmabuf caps **required**, so a silent fallback to a CPU copy is
impossible rather than merely unobserved:

| | display | coded | chroma offset | strides | dmabuf size | modifier | result |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 640x480 | 640x480 | 480 | 307200 | 1920/1920 → 640/640 | 614400 | 0 (LINEAR) | 60 frames bit-exact |
| 1920x1080 | 1920x1080 | **1088** | 2088960 | 1920/1920 | 4177920 | 0 (LINEAR) | 30 frames bit-exact |

11 pooled dmabuf fds in both cases, so an importer wants to import once per fd
and look it up thereafter.

`drm-format` comes back as `NV12:0x0000000000000000` — fourcc `NV12`, modifier
**0, i.e. LINEAR**. That is reported by the decoder rather than assumed, which is
the one thing the cros-codecs path could not do: there, linearity was inferred
from luma bytes happening to match a software decode.

## Three requirements, none of them obvious

Each of these was a dead end first, and each failed in a way that pointed
somewhere else entirely.

**1. Require the feature, do not over-specify the format.** Ask for
`video/x-raw(memory:DMABuf)` and let the decoder fill in `format=DMA_DRM` and
`drm-format=NV12` itself. Pinning `format=DMA_DRM` as well narrows the
intersection and fails with a bare `not-negotiated (-4)`.

**2. The sink MUST advertise `GstVideoMeta` support in `propose_allocation`.**
`v4l2codecs` refuses otherwise and says so precisely:

```
DMABuf caps negotiated without the mandatory support of VideoMeta
```

It is mandatory for a sound reason — a dmabuf's plane offsets and strides live in
that meta, so a sink that cannot read it has no way to interpret the buffer it
asked for. A default `appsink` does not advertise it, and the resulting failure
surfaces as `not-negotiated` reported by `h264parse`, three elements upstream of
the actual problem.

**3. `GstVideoMeta` carries the CODED size. The display size comes from caps.**
At 1080p the meta says `height: 1088`. Using that as the display height writes
eight extra rows of luma and shifts everything after it — output that is the
right shape and the wrong content. Take strides and offsets from the meta;
take width and height from `VideoInfo`.

This is the same display-vs-coded distinction the cros-codecs path had to make,
relocated: there the coded height had to be read off the driver in order to
*find* the chroma offset, and here GStreamer has already applied it, so the
hazard is using it for the wrong thing.

## Version floor: GStreamer >= 1.24.1

Not a preference. On **1.22.10**, which this machine ran until 2026-10-08:

- `v4l2slh264dec` advertised only plain `video/x-raw`; dmabuf export was decided
  by allocation negotiation and could not be required. Naming the feature failed
  to **link**, not to negotiate.
- The result was resolution-dependent and silent: dmabuf at 640x480, and
  `SystemMemory` (3110400 bytes) at 1920x1080 — a full-frame CPU copy per frame,
  at exactly the resolution sessions run at.
- Enlarging the pool and pinning `format=NV12` changed nothing.

GStreamer **1.24.1** added *"v4l2codecs: decoders: Add DMA_DRM caps support"*,
which is what makes requirement 1 possible at all. The crate therefore builds
with gstreamer-rs's `v1_24` features, which also makes the floor explicit at
link time instead of discovering it at run time.

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

# And 1080p, which is a different answer on an older GStreamer and a different
# code path even on a new one -- the coded height stops matching the display one:
ffmpeg -f lavfi -i "testsrc2=size=1920x1080:rate=30:duration=1" \
       -c:v libx264 -preset veryfast -profile:v high -pix_fmt yuv420p \
       -bsf:v h264_mp4toannexb -f h264 clip1080.h264
ffmpeg -i clip1080.h264 -pix_fmt yuv420p -f rawvideo sw1080.i420
cargo run --release -- clip1080.h264 hw1080.i420
cmp hw1080.i420 sw1080.i420
```

A `NOT dmabuf-backed` line means the stack cannot do zero-copy at that
resolution. It is the probe working, not failing.

**Read the bus.** Without draining it, a negotiation failure is completely
silent: `try_pull_sample` returns `None`, the probe reports zero frames, and
nothing says why. Every failure above was invisible until the bus was checked,
and the `VideoMeta` requirement was only ever stated there.
