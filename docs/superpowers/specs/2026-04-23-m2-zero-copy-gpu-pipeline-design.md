# M2 Zero-Copy GPU Pipeline Design

**Date:** 2026-04-23
**Status:** Design approved
**Addresses:** M2 deviations — CPU tile diff and CPU-staged VA-API encode

---

## Context

The M2 implementation plan specified a zero-copy pipeline: Vulkan compute shader for tile diff (SAD), and VA-API H.264 encode importing the same DMA-BUF — no CPU pixel copies. The current implementation deviates:

1. **Tile diff:** CPU byte-by-byte `memcmp` in `DirtyTracker` (requires full-frame readback to CPU)
2. **H.264 encode:** per-tile 32x32, pixels copied CPU→ffmpeg frame→NV12 scale→128x128 pad→`av_hwframe_transfer_data` upload

This design replaces both with a fully GPU-resident pipeline and changes H.264 encoding from per-tile to full-frame.

---

## Architecture

Two GPU paths share the same DMA-BUF fd from DRM capture. No pixel data touches CPU.

```
DRM framebuffer (GPU VRAM)
  → DMA-BUF fd
  ├→ Vulkan compute: SAD per tile → dirty bitmap (tiny GPU→CPU readback: ~8 KB)
  └→ VA-API: import same DMA-BUF → full-frame H.264 encode (zero-copy)
```

**H.264 is full-frame, not per-tile.** H.264's inter-prediction works across the whole frame; per-tile encoding wastes temporal redundancy and forces padding to meet hardware minimums. Lossless codecs (M3: PalRLE, Solid, BC1, CDF 5/3) remain per-tile.

The only data crossing GPU→CPU is:
- SAD values buffer (~8 KB for 1920x1080 at 2040 tiles)
- Encoded H.264 NAL units (output of VA-API encode)

---

## Vulkan Compute SAD Shader

### Shader: `capture/shaders/tile_sad.comp`

One compute dispatch per frame. Two input images (current and previous DMA-BUF), one output buffer of per-tile SAD values.

```glsl
layout(local_size_x = 32, local_size_y = 32) in;
layout(binding = 0, rgba8) readonly uniform image2D current_frame;
layout(binding = 1, rgba8) readonly uniform image2D prev_frame;
layout(binding = 2, std430) buffer SadOutput { uint sad_values[]; };
```

- One workgroup per tile (32x32 threads = 1024 threads = 1 pixel per thread)
- Each thread computes `abs(current - prev)` summing RGB channels
- Shared memory reduction produces per-tile SAD score
- Result written to `sad_values[tile_index]`

### GPU Resource Lifecycle

- **prev_frame:** `VkImage` persists on GPU across frames, swapped each frame (current becomes prev)
- **current_frame:** DMA-BUF imported as `VkImage` each frame (same import path as existing `VulkanReadback`, but no staging buffer readback)
- **sad_values:** small persistent `VkBuffer` (~8 KB), mapped persistently for CPU read via `HOST_VISIBLE` memory
- After dispatch + fence, CPU reads the SAD buffer

### Thresholding

A tile is dirty if `sad_values[i] > threshold`. For M2, only "any dirty tile?" matters (frame-level skip decision). For M3, per-tile SAD values feed into the two-axis classifier.

---

## VA-API Full-Frame Zero-Copy Encode

### DMA-BUF Direct Import

The encoder imports the DRM framebuffer's DMA-BUF fd directly as a VA-API surface. No CPU pixel staging.

Primary approach: `av_hwframe_map()` maps an external DMA-BUF into the VA-API hardware frames context. Fallback: if the driver doesn't support BGRA input surfaces, a Vulkan compute shader does BGRA→NV12 conversion on GPU, exporting a second DMA-BUF for VA-API.

### Encoder Changes (`h264_vaapi.rs`)

- Single encoder instance at full frame resolution (e.g. 1920x1080)
- `encode_frame(dmabuf_fd: RawFd, width: u32, height: u32, stride: u32)` replaces `encode(bgra_tile: &[u8])`
- Remove: per-tile BGRA→NV12 scaler, 128x128 padding, `upload_to_hw_surface` per-tile path
- Preserve: per-tile encode infrastructure as dead code for M3 lossless tile codecs
- Bitrate: target ~80% of estimated available bandwidth

### Frame Cadence

```
I P P P P P P P P P P I P P P P P P P P P P I ...
0                     10                    20
```

- 60 fps, I-frame every 11 frames (~6 I-frames/sec, ~54 P-frames/sec)
- Max corruption duration from a lost P-frame: ~167ms
- Short P-frame chains limit error propagation

---

## Transport: All-Datagram with FEC + Conditional NACK

No QUIC streams for frame data. Everything goes on datagrams for hard latency bounds.

### Frame Datagram Header

```
FrameDatagramHeader: 12 bytes
  frame_seq: u32      — monotonic frame counter
  frag_idx: u16       — fragment index within frame
  frag_total: u16     — total fragments for this frame
  timestamp_us: u32   — capture timestamp
```

Existing per-tile `TileHeader` format preserved for M3 lossless codecs. The top bit of `frame_seq` (bit 31) serves as the discriminator: 0 = frame-level H.264 datagram, 1 = tile-level datagram (M3). This keeps the header size unchanged and lets the client branch immediately on the first 4 bytes.

### FEC

- I-frames: higher FEC parity ratio (e.g. 50%) — they're critical for decoder state
- P-frames: standard FEC parity ratio (e.g. 20%)
- Existing XOR parity FEC code applies directly

### Conditional NACK Retransmission

NACK fires only when QUIC RTT is low enough for retransmission to arrive before the next frame is due:

```
if quic_rtt < frame_interval - elapsed_since_frame_start:
    send NACK for missing fragments
else:
    skip, wait for next I-frame
```

RTT comes from quinn-proto's congestion state (already available). Effect:
- LAN (~0.2ms RTT): NACK almost always fires, near-perfect delivery
- Internet (~50-150ms RTT): NACK rarely fires, degrades to FEC-only

No configuration needed — the math self-adapts.

### Stale Frame Discard

If a new I-frame starts encoding before the previous one is fully delivered, drop the old one. The new I-frame supersedes it. Client discards fragments for `frame_seq` older than the most recently completed frame.

---

## Client-Side Changes

### WebCodecs Full-Frame Decode

- `VideoDecoder` receives full-frame H.264 NAL units (not per-tile)
- Decoded `VideoFrame` drawn to canvas via `drawImage()` covering the entire viewport
- Per-tile compositing path in `renderer.ts` preserved for M3 lossless tile overlays

### Fragment Reassembly

- Existing reassembly logic in `main.ts` reused, keyed on `frame_seq` alone (no tile coordinates)
- Stale frame discard: fragments for `frame_seq` older than latest completed frame are dropped

### M3 Tile Compositing (Preserved)

- Full-frame H.264 draws first as the base layer
- M3 lossless tiles overlay on top (higher-quality refinement over the H.264 base)

---

## Module Changes

### Modified Files

| File | Change |
|------|--------|
| `capture/dmabuf.rs` | Add `VulkanTileSad` — compute pipeline, prev-frame management, SAD buffer readback. Keep `VulkanReadback` and `readback_dmabuf` for M3 lossless path. |
| `encoder/h264_vaapi.rs` | Rewrite around full-frame encode. `encode_frame(dmabuf_fd, width, height, stride)` replaces `encode(bgra_tile)`. Single instance, no padding. Per-tile encode helpers preserved as dead code for M3. |
| `transport/protocol.rs` | Add `FrameDatagramHeader` (frame-level, no tile coords). Existing `TileHeader` preserved for M3. Add codec/type discriminator. |
| `transport/io_bridge.rs` | Rewrite `process_frame`: run Vulkan SAD → if any dirty → encode full frame via VA-API → fragment as frame datagrams with FEC. Remove per-tile encoder pool. Add NACK handling with RTT gate. |
| `transport/fec.rs` | Adjust FEC ratio: higher parity for I-frames vs P-frames. |
| `tile/mod.rs` | `DirtyTracker` stays as dead code for M3. |
| `web-client/src/main.ts` | Frame-level reassembly keyed on `frame_seq`. Stale frame discard. |
| `web-client/src/decoder.ts` | Full-frame `VideoDecoder`. `drawImage()` for entire viewport. Per-tile decode path preserved for M3. |
| `web-client/src/renderer.ts` | Full-frame draw layer beneath future M3 tile overlays. |

### New Files

| File | Purpose |
|------|---------|
| `capture/shaders/tile_sad.comp` | GLSL compute shader for per-tile SAD |
| `capture/gpu_diff.rs` | `GpuDirtyTracker` — Vulkan compute pipeline, descriptor sets, prev-frame swap, SAD buffer management |

---

## What This Does NOT Change

- Per-tile protocol format (preserved for M3)
- `DirtyTracker` CPU byte-compare (preserved for M3)
- Per-tile encoder infrastructure in `h264_vaapi.rs` (dead code, preserved for M3)
- FEC core module (`fec.rs`) — reused as-is
- Receiver feedback mechanism (`feedback.rs`) — reused as-is
- Test infrastructure (containers, headscale)

---

## Completion Gate

- Vulkan compute SAD shader correctly identifies dirty tiles on GPU
- Full-frame H.264 encode via VA-API with zero CPU pixel copies
- I-frames and P-frames delivered via datagrams with FEC
- Conditional NACK retransmission fires on LAN, skips on high-RTT links
- Full-frame decode and render in browser via WebCodecs
- No regression in existing E2E tests (adapted for full-frame protocol)
