#!/usr/bin/env python3
"""Does GStreamer's V4L2 stateless H.264 decoder hand out dmabuf-backed buffers?

Decodes the first few frames of an Annex-B H.264 elementary stream through
`v4l2slh264dec` and reports, per frame, whether the buffer's memory is a dmabuf
and what fd backs it.

The point is that caps are the wrong thing to check: GStreamer 1.22 has no
`memory:DMABuf` capsfeature on this element -- negotiating
`video/x-raw(memory:DMABuf)` fails to link -- yet the memory underneath is a
dmabuf anyway. Only `gst_is_dmabuf_memory()` tells you.

See README.md for how to produce a test clip.
"""

import sys

import gi

gi.require_version("Gst", "1.0")
gi.require_version("GstAllocators", "1.0")
gi.require_version("GstApp", "1.0")
from gi.repository import Gst, GstAllocators  # noqa: E402
from gi.repository import GstApp  # noqa: E402,F401  (registers appsink methods)

FRAMES = 5


def main(path):
    Gst.init(None)
    pipeline = Gst.parse_launch(
        f"filesrc location={path} ! h264parse ! v4l2slh264dec "
        "! appsink name=sink emit-signals=false sync=false max-buffers=4"
    )
    sink = pipeline.get_by_name("sink")
    pipeline.set_state(Gst.State.PLAYING)

    dmabuf_frames = 0
    n = 0
    try:
        while n < FRAMES:
            sample = sink.try_pull_sample(Gst.SECOND * 5)
            if sample is None:
                break
            buf = sample.get_buffer()
            mem = buf.peek_memory(0)
            is_dma = GstAllocators.is_dmabuf_memory(mem)
            fd = GstAllocators.dmabuf_memory_get_fd(mem) if is_dma else -1
            if n == 0:
                allocator = mem.allocator.get_name() if mem.allocator else "(none)"
                print(f"caps      : {sample.get_caps().to_string()[:110]}")
                print(f"n_memory  : {buf.n_memory()}")
                print(f"allocator : {allocator}")
            print(f"frame {n}: dmabuf={is_dma} fd={fd} size={mem.size}")
            dmabuf_frames += is_dma
            n += 1
    finally:
        pipeline.set_state(Gst.State.NULL)

    if n == 0:
        print("\nno frames decoded -- is this an Annex-B H.264 stream?")
        return 1
    print(f"\n{dmabuf_frames}/{n} frames dmabuf-backed")
    # A CPU-mapped fallback is the failure this probe exists to catch: it works,
    # it is silent, and it costs a copy per frame.
    return 0 if dmabuf_frames == n else 1


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {sys.argv[0]} <clip.h264>")
    sys.exit(main(sys.argv[1]))
