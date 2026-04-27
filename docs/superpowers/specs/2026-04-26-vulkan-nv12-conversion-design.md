# Vulkan BGRA→NV12 Compute Shader + DMA-BUF Export

**Date:** 2026-04-26
**Status:** Design approved
**Addresses:** Zero-copy encode gap — real GPU DMA-BUFs can't be mmap'd for the CPU fallback path

---

## Context

The M2 zero-copy GPU pipeline has a gap: `FullFrameEncoder::encode_frame` tries to mmap the DMA-BUF to read pixels, which fails for real GPU VRAM. `av_hwframe_map` from DRM_PRIME to VAAPI also fails because the framebuffer is XRGB8888 (BGRA) but the encoder's hw_frames_ctx expects NV12.

The fix: do BGRA→NV12 conversion on the GPU via a Vulkan compute shader, export the NV12 result as a DMA-BUF, and feed that to VA-API (NV12→NV12 mapping works).

---

## Architecture

```
DMA-BUF (BGRA, from DRM capture)
  → Vulkan import as VkImage (already exists for SAD)
  ├→ SAD compute shader → dirty tile list (existing)
  └→ BGRA→NV12 compute shader (new)
       → NV12 VkBuffer (device-local, exportable)
       → vkGetMemoryFdKHR → NV12 DMA-BUF fd
       → av_hwframe_map DRM_PRIME NV12 → VAAPI NV12 (same format)
       → H.264 encode
```

Both compute dispatches (SAD + NV12 conversion) run in the same command buffer, single submit. ffmpeg is only used for the H.264 codec. Vulkan handles all image processing.

---

## Shader: `bgra_to_nv12.comp`

NV12 layout: full-resolution Y plane followed by half-resolution interleaved UV plane.

```glsl
layout(local_size_x = 2, local_size_y = 2, local_size_z = 1) in;

layout(binding = 0, rgba8) readonly uniform image2D bgra_input;
layout(binding = 1, std430) buffer NV12Output { uint data[]; };

layout(push_constant) uniform PushConstants {
    uint width;
    uint height;
    uint y_stride;    // bytes per row in Y plane
    uint uv_offset;   // byte offset to UV plane start (y_stride * height)
    uint uv_stride;   // bytes per row in UV plane
};
```

Each workgroup processes a 2x2 pixel block:
- 4 threads compute 4 Y values: `Y = 0.299*R + 0.587*G + 0.114*B` (BT.601)
- Thread (0,0) computes 1 UV pair from the 2x2 average: `U = -0.169*R - 0.331*G + 0.500*B + 128`, `V = 0.500*R - 0.419*G - 0.081*B + 128`
- Y values written to `data[y_offset]`, UV pair written to `data[uv_offset]`

Dispatch: `(width/2, height/2, 1)` workgroups.

The NV12 output is a single contiguous `VkBuffer`:
- Y plane at offset 0, size = `y_stride * height`
- UV plane at offset `y_stride * height`, size = `uv_stride * height/2`

Total buffer size: `y_stride * height + uv_stride * height/2`

Y stride and UV stride are set to `width` (tightly packed, no padding). VA-API import requires strides to match.

---

## Module Rename: `gpu_diff.rs` → `gpu_pipeline.rs`

`GpuDirtyTracker` becomes `GpuFrameProcessor`.

### New public API:

```rust
pub struct FrameAnalysis {
    pub dirty_tiles: Vec<u32>,
    pub nv12_dmabuf_fd: OwnedFd,
    pub nv12_width: u32,
    pub nv12_height: u32,
    pub nv12_y_stride: u32,
    pub nv12_uv_stride: u32,
    pub nv12_uv_offset: u32,
}

impl GpuFrameProcessor {
    pub fn new(max_tiles: u32) -> Result<Self, Error>;
    pub fn process_frame(&mut self, fd: RawFd, width: u32, height: u32, stride: u32)
        -> Result<FrameAnalysis, Error>;
}
```

### Internal changes:

**New Vulkan resources on the struct:**
- `nv12_shader_module: vk::ShaderModule` — compiled `bgra_to_nv12.spv`
- `nv12_pipeline: vk::Pipeline` — second compute pipeline
- `nv12_descriptor_set_layout: vk::DescriptorSetLayout` — 1 storage image + 1 storage buffer
- `nv12_pipeline_layout: vk::PipelineLayout` — push constants: width, height, y_stride, uv_offset, uv_stride (20 bytes)
- `nv12_buffer: vk::Buffer` — output buffer, device-local + exportable
- `nv12_memory: vk::DeviceMemory` — allocated with `VK_KHR_external_memory_fd` export flag
- `nv12_buf_size: usize` — current allocation size
- `nv12_width: u32`, `nv12_height: u32` — current resolution

**Required extension:** `VK_KHR_external_memory_fd` (already required for import, now also used for export via `vkGetMemoryFdKHR`).

**Descriptor pool:** expanded to accommodate the additional descriptor set (1 storage image + 1 storage buffer per frame).

### process_frame() flow:

1. Import DMA-BUF as VkImage (existing)
2. If first frame or resolution changed: reallocate NV12 output buffer
3. If no prev_image: store as prev, run NV12 conversion only (skip SAD), return all-dirty
4. Allocate 2 descriptor sets: one for SAD, one for NV12
5. Record single command buffer:
   a. Image barriers (current + prev to GENERAL)
   b. Bind SAD pipeline, dispatch `(cols, rows, 1)`
   c. Bind NV12 pipeline, dispatch `(width/2, height/2, 1)`
   d. Buffer barriers (SAD buffer + NV12 buffer to HOST_READ)
6. Submit + fence wait
7. Read SAD values, build dirty tile list
8. Export NV12 buffer as DMA-BUF fd via `vkGetMemoryFdKHR`
9. Swap prev_image
10. Return `FrameAnalysis`

---

## Encoder Changes

New method on `FullFrameEncoder`:

```rust
pub fn encode_nv12_dmabuf(
    &mut self,
    nv12_fd: RawFd,
    width: u32,
    height: u32,
    y_stride: u32,
    uv_stride: u32,
    uv_offset: u32,
) -> Result<Option<FullFrameEncoded>, ffmpeg::Error>
```

Implementation:
1. Build `AVDRMFrameDescriptor` with format `DRM_FORMAT_NV12` (fourcc `0x3231564e`)
2. Two planes: Y at offset 0 with `pitch = y_stride`, UV at `uv_offset` with `pitch = uv_stride`
3. `av_hwframe_map` from DRM_PRIME NV12 → VAAPI NV12 (same format = succeeds)
4. Set pts, force IDR if needed
5. Send to encoder, receive packet

The old `encode_frame(fd)` method stays for the mmap fallback path (memfds in tests, X11 capture).

---

## IoBridge Changes

In `process_frame_gpu`, replace:
```rust
let dirty = tracker.diff(raw_fd, ...)?;
// ...
let encoded = encoder.encode_frame(raw_fd, ...)?;
```

With:
```rust
let analysis = processor.process_frame(raw_fd, ...)?;
if analysis.dirty_tiles.is_empty() { return; }
let encoded = encoder.encode_nv12_dmabuf(
    analysis.nv12_dmabuf_fd.as_raw_fd(),
    analysis.nv12_width, analysis.nv12_height,
    analysis.nv12_y_stride, analysis.nv12_uv_stride, analysis.nv12_uv_offset,
)?;
```

Field renames on IoBridge: `gpu_dirty_tracker` → `gpu_frame_processor`.

---

## File Changes

| File | Change |
|------|--------|
| `capture/shaders/bgra_to_nv12.comp` | New GLSL compute shader |
| `capture/gpu_diff.rs` → `capture/gpu_pipeline.rs` | Rename, add NV12 pipeline, `GpuFrameProcessor`, `FrameAnalysis` |
| `capture/mod.rs` | `pub mod gpu_diff` → `pub mod gpu_pipeline` |
| `encoder/h264_vaapi.rs` | Add `encode_nv12_dmabuf` method |
| `transport/io_bridge.rs` | Update `process_frame_gpu` to use `FrameAnalysis` |
| `tests/gpu_pipeline.rs` | Add NV12 conversion test, update existing tests for renamed API |
| `xdaemon/tests/drm_gpu_pipeline.rs` | Use `encode_nv12_dmabuf` for real DMA-BUF test |

---

## Completion Gate

- `gpu_pipeline_nv12_conversion` test passes: memfd → `process_frame` → valid NV12 DMA-BUF fd
- `drm_capture_to_gpu_pipeline` test passes with sudo: real DRM DMA-BUF → SAD + NV12 → encode → fragment
- Existing memfd-based tests continue to work (mmap fallback path unchanged)
- No regression in E2E tests (X11 capture path uses old `encode_frame`)
