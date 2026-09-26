# Hardware probes

Four small programs that answer the questions
[the GLES + V4L2 design doc](../../docs/superpowers/specs/2026-09-25-native-client-gles-v4l2-design.md)
is built on. They exist so that document's "Measured baseline" table can be
re-derived on a new machine instead of trusted.

They are diagnostics, not part of the build. Nothing in the workspace links
them and CI does not run them — a runner has neither a Mali GPU nor a stateless
V4L2 decoder, which is the whole reason these are hand-run.

```bash
cd tools/hw-probe
make run          # builds and runs the three C probes
```

| Probe | Answers | Needs |
| --- | --- | --- |
| `glprobe.c` | Which GLES version, and do the compute limits fit our shaders? Is `EGL_MESA_image_dma_buf_export` there? | libEGL, libGLESv2 |
| `gbmprobe.c` | Can GBM allocate the NV12 buffers a video decoder wants, and under which usage flags? | libgbm |
| `expbuf_probe.c` | Can a V4L2 stateless decoder allocate its own buffers and export them as dmabufs? | nothing (raw ioctls) |
| `dmabuf_probe.py` | Does GStreamer's `v4l2slh264dec` hand out dmabuf-backed buffers? | PyGObject, gst-plugins-bad |

## Why each one exists

**`glprobe.c`** — `MAX_COMPUTE_WORK_GROUP_INVOCATIONS` is the number that
decides whether `shaders/client/*.wgsl` can run unmodified. The spec minimum for
GLES 3.1 is 128 and WebGPU guarantees 256, so the answer is per-driver and
cannot be assumed. It also prints `MAX_COMPUTE_SHADER_STORAGE_BLOCKS`, which
must be at least 7 for `cdf53_integrate.wgsl`.

**`gbmprobe.c`** — a video decoder needs NV12 buffers, and the obvious way to
get dmabuf-backed ones is GBM. Whether that works is entirely driver-dependent:
a GPU can render perfectly well and still refuse to allocate NV12. The probe
tries several usage-flag combinations, including ChromeOS's
`GBM_BO_USE_HW_VIDEO_DECODER` (`1 << 13`), which upstream Mesa does not define —
so a bare failure does not tell you whether the format or the flag was rejected.

**`expbuf_probe.c`** — the fallback when GBM cannot allocate: let the driver
allocate (`V4L2_MEMORY_MMAP`, which is also what a `dma_contig` decoder
requires) and export each buffer with `VIDIOC_EXPBUF`. It also prints the
capture queue's `bytesperline` / `sizeimage`, which is how you discover the
buffer is height-padded — computing the chroma offset from the visible
resolution instead gives a plausible image with shifted colour rather than an
error.

**`dmabuf_probe.py`** — checks whether GStreamer's stateless decoder yields
dmabufs. Worth keeping even though the design rejects GStreamer: it is the
proof that the hardware path works at all, independent of any Rust, and
GStreamer remains the documented fallback. Note it reports `dmabuf=True` while
the caps say plain `video/x-raw` — GStreamer 1.22 has no `memory:DMABuf`
capsfeature, so caps are not the thing to check.

```bash
# needs an Annex-B H.264 elementary stream
ffmpeg -f lavfi -i testsrc=size=640x480:rate=30:duration=2 \
    -c:v libx264 -preset ultrafast -tune zerolatency -g 30 \
    -pix_fmt yuv420p -f h264 -y clip.h264
python3 dmabuf_probe.py clip.h264
```

## Checking a hardware decoder is actually correct

Not a probe, but the measurement that matters most, and the one that caught a
padding mistake during this investigation. Compare hardware against software
output — **through `videoconvert`**, which honours `GstVideoMeta`, because
dumping the raw buffer includes the driver's padding and the checksums will
differ for a decode that is in fact bit-exact:

```bash
gst-launch-1.0 filesrc location=clip.h264 ! h264parse ! v4l2slh264dec \
    ! videoconvert ! video/x-raw,format=I420 ! filesink location=hw.i420
ffmpeg -i clip.h264 -pix_fmt yuv420p -f rawvideo -y sw.i420
md5sum hw.i420 sw.i420      # must match
```
