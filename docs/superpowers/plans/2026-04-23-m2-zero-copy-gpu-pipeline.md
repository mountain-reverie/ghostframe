# M2 Zero-Copy GPU Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the CPU-staged tile diff and per-tile H.264 encode with a fully GPU-resident pipeline: Vulkan compute SAD for dirty detection, full-frame VA-API H.264 encode via DMA-BUF zero-copy, and all-datagram transport with FEC and conditional NACK.

**Architecture:** DRM capture produces a DMA-BUF fd. Two GPU paths share it: (1) a Vulkan compute shader computes per-tile SAD scores against the previous frame, producing a dirty bitmap without reading pixels to CPU; (2) VA-API imports the same DMA-BUF and encodes a full-frame H.264 stream. Transport splits I-frames (higher FEC) and P-frames (standard FEC) over datagrams with conditional NACK retransmission gated on RTT.

**Tech Stack:** Rust, ash (Vulkan), ffmpeg-next (VA-API), quinn-proto (QUIC), TypeScript (WebCodecs client)

**Use model:** sonnet for subagent implementation

---

## File Structure

### New Files
| File | Responsibility |
|------|---------------|
| `ghostframe-lib/src/capture/gpu_diff.rs` | `GpuDirtyTracker` — Vulkan compute pipeline for per-tile SAD, prev-frame management, SAD buffer readback |
| `ghostframe-lib/src/capture/shaders/tile_sad.comp` | GLSL compute shader: per-tile SAD with shared memory reduction |

### Modified Files
| File | Change |
|------|--------|
| `ghostframe-lib/src/capture/mod.rs` | Add `pub mod gpu_diff;` |
| `ghostframe-lib/src/encoder/h264_vaapi.rs` | Add `FullFrameEncoder` for DMA-BUF zero-copy full-frame encode alongside existing per-tile encoder (preserved for M3) |
| `ghostframe-lib/src/transport/protocol.rs` | Add `FrameHeader` (frame-level, no tile coords), `fragment_frame()`, `decode_frame_datagram()`, discriminator constant |
| `ghostframe-lib/src/transport/io_bridge.rs` | Rewrite `process_frame` for full-frame pipeline; add NACK handling with RTT gate |
| `ghostframe-lib/src/transport/fec.rs` | Add `generate_parity_adaptive()` with I/P-frame ratio selection |
| `ghostframe-lib/src/server.rs` | Extend `FrameSubmission` to carry DMA-BUF fd instead of pixel `Vec<u8>` |
| `ghostframe-xdaemon/src/main.rs` | Pass DMA-BUF fd directly to `FrameSubmission` instead of reading back pixels |
| `ghostframe-web-client/src/decoder.ts` | Add `FullFrameDecoder` class, add `FrameHeader` decode, keep `H264TileDecoder` |
| `ghostframe-web-client/src/main.ts` | Add frame-level reassembly path alongside tile-level, stale frame discard |
| `ghostframe-web-client/src/renderer.ts` | Add `drawFullFrame()` method for full-viewport blit |

---

### Task 1: GLSL Compute Shader for Per-Tile SAD

**Files:**
- Create: `ghostframe-lib/src/capture/shaders/tile_sad.comp`

This shader is compiled offline via `glslangValidator` and loaded as SPIR-V at runtime in Task 2.

- [ ] **Step 1: Create the shaders directory**

Run: `mkdir -p ghostframe-lib/src/capture/shaders`

- [ ] **Step 2: Write the compute shader**

```glsl
// tile_sad.comp
// Compute per-tile Sum of Absolute Differences (SAD) between current and previous frame.
//
// Dispatch: one workgroup per tile (ceil(width/32) * ceil(height/32) workgroups).
// Each thread handles one pixel within the 32x32 tile.
// Shared memory reduction sums per-pixel differences into a single SAD score per tile.

#version 450

layout(local_size_x = 32, local_size_y = 32, local_size_z = 1) in;

layout(binding = 0, rgba8) readonly uniform image2D current_frame;
layout(binding = 1, rgba8) readonly uniform image2D prev_frame;

layout(binding = 2, std430) buffer SadOutput {
    uint sad_values[];
};

layout(push_constant) uniform PushConstants {
    uint frame_width;
    uint frame_height;
    uint cols;  // number of tile columns
};

shared uint shared_sad[1024]; // 32*32 = 1024 threads per workgroup

void main() {
    uint local_idx = gl_LocalInvocationID.y * 32 + gl_LocalInvocationID.x;
    uint tile_x = gl_WorkGroupID.x;
    uint tile_y = gl_WorkGroupID.y;

    // Pixel coordinates in the frame
    uint px = tile_x * 32 + gl_LocalInvocationID.x;
    uint py = tile_y * 32 + gl_LocalInvocationID.y;

    uint diff = 0;
    if (px < frame_width && py < frame_height) {
        ivec2 coord = ivec2(px, py);
        vec4 cur = imageLoad(current_frame, coord);
        vec4 prev = imageLoad(prev_frame, coord);

        // SAD over RGB channels (ignore alpha), scale from [0,1] to [0,255]
        diff = uint(abs(cur.r - prev.r) * 255.0)
             + uint(abs(cur.g - prev.g) * 255.0)
             + uint(abs(cur.b - prev.b) * 255.0);
    }

    shared_sad[local_idx] = diff;
    barrier();

    // Parallel reduction (sum)
    for (uint stride = 512; stride > 0; stride >>= 1) {
        if (local_idx < stride) {
            shared_sad[local_idx] += shared_sad[local_idx + stride];
        }
        barrier();
    }

    if (local_idx == 0) {
        uint tile_idx = tile_y * cols + tile_x;
        sad_values[tile_idx] = shared_sad[0];
    }
}
```

- [ ] **Step 3: Compile the shader to SPIR-V**

Run: `glslangValidator -V ghostframe-lib/src/capture/shaders/tile_sad.comp -o ghostframe-lib/src/capture/shaders/tile_sad.spv`
Expected: `tile_sad.spv` created, no errors

- [ ] **Step 4: Verify the SPIR-V is valid**

Run: `spirv-val ghostframe-lib/src/capture/shaders/tile_sad.spv`
Expected: no validation errors (exit code 0). If `spirv-val` is not installed, `glslangValidator` already validates during compilation — skip this step.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/capture/shaders/tile_sad.comp ghostframe-lib/src/capture/shaders/tile_sad.spv
git commit -m "feat(capture): GLSL compute shader for per-tile SAD comparison"
```

---

### Task 2: GpuDirtyTracker — Vulkan Compute Pipeline

**Files:**
- Create: `ghostframe-lib/src/capture/gpu_diff.rs`
- Modify: `ghostframe-lib/src/capture/mod.rs`

The `GpuDirtyTracker` manages Vulkan resources for running the SAD shader and reading back results. It imports a DMA-BUF as a `VkImage`, runs the compute shader comparing it against the previous frame, and returns a list of dirty tile indices.

- [ ] **Step 1: Write the test for GpuDirtyTracker**

Create `ghostframe-lib/src/capture/gpu_diff.rs` with the test at the bottom (the implementation will follow):

```rust
//! GPU-accelerated per-tile dirty detection using Vulkan compute SAD.
//!
//! `GpuDirtyTracker` imports a DMA-BUF as a VkImage each frame, runs a compute
//! shader comparing it against the previous frame, and reads back per-tile SAD
//! scores from a small GPU buffer. Only the SAD scores (~8 KB for 1920x1080)
//! cross the GPU→CPU boundary — no pixel readback.

use ash::vk;
use std::ffi::CStr;
use std::os::unix::io::RawFd;

/// Per-tile SAD score threshold. Tiles with SAD below this are considered clean.
/// Value chosen to ignore sub-pixel rounding noise while catching any visible change.
const SAD_THRESHOLD: u32 = 64;

/// GPU-accelerated dirty tile detector using Vulkan compute SAD shader.
pub struct GpuDirtyTracker {
    _entry: ash::Entry,
    instance: ash::Instance,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    queue_family_index: u32,
    command_pool: vk::CommandPool,
    /// Compiled SPIR-V compute shader module.
    shader_module: vk::ShaderModule,
    /// Compute pipeline for the SAD shader.
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    /// Previous frame stored as VkImage on GPU. Swapped each frame.
    prev_image: Option<PrevFrame>,
    /// Small host-visible buffer for SAD readback.
    sad_buffer: vk::Buffer,
    sad_memory: vk::DeviceMemory,
    /// Persistently mapped pointer to sad_buffer.
    sad_ptr: *mut u32,
    /// Maximum number of tiles this tracker was allocated for.
    max_tiles: u32,
    /// Frame dimensions from last call (to detect resolution changes).
    last_width: u32,
    last_height: u32,
}

/// Previous frame's VkImage and its backing memory (imported from DMA-BUF).
struct PrevFrame {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    width: u32,
    height: u32,
}

// SAFETY: Vulkan handles are not thread-local. GpuDirtyTracker is only used
// from the single-threaded capture loop.
unsafe impl Send for GpuDirtyTracker {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_dirty_tracker_creates_successfully() {
        match GpuDirtyTracker::new(2048) {
            Ok(tracker) => {
                assert!(tracker.max_tiles > 0);
            }
            Err(e) => {
                eprintln!("Skipping GPU diff test (no Vulkan GPU?): {e}");
            }
        }
    }

    #[test]
    fn identical_frames_produce_no_dirty_tiles() {
        let tracker = match GpuDirtyTracker::new(64) {
            Ok(t) => t,
            Err(_) => { eprintln!("Skipping: no Vulkan"); return; }
        };

        let width = 64u32;
        let height = 64u32;
        let stride = width * 4;
        let size = (stride * height) as usize;

        // Create two identical memfds
        let name = std::ffi::CString::new("test-frame").unwrap();
        let fd1 = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        assert!(fd1 >= 0);
        unsafe { libc::ftruncate(fd1, size as i64) };
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE,
                       libc::MAP_SHARED, fd1, 0)
        };
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, size) };
        for pixel in slice.chunks_exact_mut(4) {
            pixel[0] = 0; pixel[1] = 128; pixel[2] = 255; pixel[3] = 255;
        }
        unsafe { libc::munmap(ptr, size) };

        let fd2 = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        assert!(fd2 >= 0);
        unsafe { libc::ftruncate(fd2, size as i64) };
        let ptr2 = unsafe {
            libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE,
                       libc::MAP_SHARED, fd2, 0)
        };
        let slice2 = unsafe { std::slice::from_raw_parts_mut(ptr2 as *mut u8, size) };
        slice2.copy_from_slice(slice);
        unsafe { libc::munmap(ptr2, size) };

        // First frame: establishes prev_image (all tiles reported dirty)
        let mut tracker = tracker;
        let dirty1 = tracker.diff(fd1, width, height, stride).unwrap();
        assert!(!dirty1.is_empty(), "first frame should report all tiles dirty");

        // Second frame: identical content, should be clean
        let dirty2 = tracker.diff(fd2, width, height, stride).unwrap();
        assert!(dirty2.is_empty(), "identical frame should produce no dirty tiles");

        unsafe { libc::close(fd1); libc::close(fd2); }
    }

    #[test]
    fn changed_pixel_detected() {
        let tracker = match GpuDirtyTracker::new(64) {
            Ok(t) => t,
            Err(_) => { eprintln!("Skipping: no Vulkan"); return; }
        };

        let width = 64u32;
        let height = 64u32;
        let stride = width * 4;
        let size = (stride * height) as usize;

        let name = std::ffi::CString::new("test-frame").unwrap();
        let fd1 = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        assert!(fd1 >= 0);
        unsafe { libc::ftruncate(fd1, size as i64) };
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE,
                       libc::MAP_SHARED, fd1, 0)
        };
        // Fill with black
        unsafe { std::ptr::write_bytes(ptr as *mut u8, 0, size) };
        unsafe { libc::munmap(ptr, size) };

        let fd2 = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        assert!(fd2 >= 0);
        unsafe { libc::ftruncate(fd2, size as i64) };
        let ptr2 = unsafe {
            libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE,
                       libc::MAP_SHARED, fd2, 0)
        };
        // Fill with black, but change tile (1,0) — pixel at x=33, y=0
        unsafe { std::ptr::write_bytes(ptr2 as *mut u8, 0, size) };
        let slice2 = unsafe { std::slice::from_raw_parts_mut(ptr2 as *mut u8, size) };
        // Change several pixels in tile (1,0) to white
        for y in 0..32u32 {
            for x in 32..64u32 {
                let off = (y * stride + x * 4) as usize;
                slice2[off] = 255; slice2[off+1] = 255;
                slice2[off+2] = 255; slice2[off+3] = 255;
            }
        }
        unsafe { libc::munmap(ptr2, size) };

        let mut tracker = tracker;
        let _ = tracker.diff(fd1, width, height, stride).unwrap();
        let dirty = tracker.diff(fd2, width, height, stride).unwrap();

        // Tile (1,0) should be dirty, tiles (0,0), (0,1), (1,1) should be clean
        assert!(dirty.contains(&(1, 0)), "changed tile (1,0) not detected: {:?}", dirty);
        assert!(!dirty.contains(&(0, 0)), "tile (0,0) should be clean: {:?}", dirty);

        unsafe { libc::close(fd1); libc::close(fd2); }
    }
}
```

- [ ] **Step 2: Run the tests to confirm they fail**

Run: `cargo test -p ghostframe-lib capture::gpu_diff --no-default-features 2>&1 | tail -20`
Expected: compilation error — `GpuDirtyTracker::new` and `diff` methods don't exist yet.

- [ ] **Step 3: Implement GpuDirtyTracker::new()**

Add the `new()` constructor to `GpuDirtyTracker` in `gpu_diff.rs`. This sets up: Vulkan instance, physical device (requiring `VK_KHR_external_memory_fd` + `VK_EXT_external_memory_dma_buf`), logical device with a **compute**-capable queue, command pool, shader module (loaded from embedded SPIR-V), descriptor set layout (2 storage images + 1 storage buffer), pipeline layout with push constants, compute pipeline, descriptor pool, and the SAD readback buffer (host-visible, persistently mapped).

The SPIR-V binary should be embedded via `include_bytes!("shaders/tile_sad.spv")`.

Key implementation details:
- Queue must support `vk::QueueFlags::COMPUTE`
- Descriptor set layout: binding 0 and 1 are `STORAGE_IMAGE`, binding 2 is `STORAGE_BUFFER`
- Push constants: `{ frame_width: u32, frame_height: u32, cols: u32 }` = 12 bytes
- SAD buffer size: `max_tiles * 4` bytes (one `u32` per tile)
- Map the SAD buffer immediately and store the pointer in `sad_ptr`

The required Vulkan device extensions are the same as `VulkanReadback` in `dmabuf.rs` — reference that code for the device selection and extension setup pattern.

- [ ] **Step 4: Implement GpuDirtyTracker::diff()**

Add the `diff()` method:

```rust
/// Import a DMA-BUF and compute per-tile SAD against the previous frame.
///
/// Returns tile coordinates `(col, row)` for all tiles whose SAD exceeds
/// the threshold. On the first call (no previous frame), all tiles are
/// reported dirty.
///
/// The fd is NOT consumed (not closed) by this call.
pub fn diff(
    &mut self,
    fd: RawFd,
    width: u32,
    height: u32,
    stride: u32,
) -> Result<Vec<(u32, u32)>, Box<dyn std::error::Error>> {
    // ... implementation
}
```

Implementation steps within `diff()`:
1. Compute `cols = width.div_ceil(32)`, `rows = height.div_ceil(32)`, `tile_count = cols * rows`
2. If resolution changed from `last_width`/`last_height`, drop `prev_image`
3. Import the DMA-BUF fd as a VkImage (same pattern as `VulkanReadback::readback_inner` lines 195-252 in `dmabuf.rs`, but with `USAGE = STORAGE` instead of `TRANSFER_SRC`)
4. Create a `VkImageView` for the imported image
5. If `prev_image` is `None` (first frame): store current as prev, return all tiles as dirty
6. Allocate a descriptor set, update it with prev_image (binding 0), current_image (binding 1), sad_buffer (binding 2)
7. Record command buffer: pipeline barrier (both images to GENERAL layout), bind pipeline + descriptor set, push constants, dispatch `(cols, rows, 1)` workgroups, pipeline barrier (SAD buffer to HOST_READ)
8. Submit + fence wait
9. Read `sad_ptr[0..tile_count]`, collect tiles where `sad_values[i] > SAD_THRESHOLD`
10. Clean up current frame's Vulkan resources, swap: current image becomes `prev_image`
11. Return dirty tile list

- [ ] **Step 5: Implement Drop for GpuDirtyTracker**

```rust
impl Drop for GpuDirtyTracker {
    fn drop(&mut self) {
        unsafe {
            self.device.device_wait_idle().ok();
            if let Some(prev) = self.prev_image.take() {
                self.device.destroy_image_view(prev.view, None);
                self.device.destroy_image(prev.image, None);
                self.device.free_memory(prev.memory, None);
            }
            self.device.unmap_memory(self.sad_memory);
            self.device.destroy_buffer(self.sad_buffer, None);
            self.device.free_memory(self.sad_memory, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.pipeline_layout, None);
            self.device.destroy_descriptor_pool(self.descriptor_pool, None);
            self.device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            self.device.destroy_shader_module(self.shader_module, None);
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
```

- [ ] **Step 6: Add module declaration**

In `ghostframe-lib/src/capture/mod.rs`, add:
```rust
pub mod gpu_diff;
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p ghostframe-lib capture::gpu_diff 2>&1 | tail -20`
Expected: tests pass on GPU hardware, or are skipped with "no Vulkan" message.

- [ ] **Step 8: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_diff.rs ghostframe-lib/src/capture/mod.rs
git commit -m "feat(capture): GpuDirtyTracker — Vulkan compute SAD for per-tile dirty detection"
```

---

### Task 3: Full-Frame VA-API Encoder

**Files:**
- Modify: `ghostframe-lib/src/encoder/h264_vaapi.rs`

Add `FullFrameEncoder` alongside the existing `H264VaapiEncoder` (which is preserved for M3 lossless codecs).

- [ ] **Step 1: Write the test for FullFrameEncoder**

Add at the bottom of `h264_vaapi.rs`, inside the existing `mod tests`:

```rust
    #[test]
    fn full_frame_encode_from_memfd() {
        let _ = tracing_subscriber::fmt::try_init();

        let width = 640u32;
        let height = 480u32;
        let stride = width * 4;
        let size = (stride * height) as usize;

        // Create a memfd with solid red BGRA pixels
        let name = std::ffi::CString::new("test-frame").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        assert!(fd >= 0);
        unsafe { libc::ftruncate(fd, size as i64) };
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(), size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED, fd, 0,
            )
        };
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, size) };
        for pixel in slice.chunks_exact_mut(4) {
            pixel[0] = 0;   // B
            pixel[1] = 0;   // G
            pixel[2] = 255; // R
            pixel[3] = 255; // A
        }
        unsafe { libc::munmap(ptr, size) };

        let mut encoder = match FullFrameEncoder::new(width, height) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Skipping full-frame encode test: {e}");
                unsafe { libc::close(fd) };
                return;
            }
        };

        // Encode two frames — first should be I-frame, second P-frame
        let result1 = encoder.encode_frame(fd, width, height, stride).unwrap();
        assert!(result1.is_some(), "first frame should produce output");
        let enc1 = result1.unwrap();
        assert!(!enc1.payload.is_empty());
        assert!(enc1.is_keyframe, "first frame should be a keyframe");

        let result2 = encoder.encode_frame(fd, width, height, stride).unwrap();
        // VA-API may buffer — try a third frame if needed
        if result2.is_none() {
            let result3 = encoder.encode_frame(fd, width, height, stride).unwrap();
            assert!(result3.is_some(), "should produce output within 3 frames");
        }

        unsafe { libc::close(fd) };
    }

    #[test]
    fn full_frame_keyframe_interval() {
        let _ = tracing_subscriber::fmt::try_init();

        let width = 128u32;
        let height = 128u32;
        let stride = width * 4;
        let size = (stride * height) as usize;

        let name = std::ffi::CString::new("test-frame").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        assert!(fd >= 0);
        unsafe { libc::ftruncate(fd, size as i64) };
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(), size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED, fd, 0,
            )
        };
        unsafe { std::ptr::write_bytes(ptr as *mut u8, 128, size) };
        unsafe { libc::munmap(ptr, size) };

        let mut encoder = match FullFrameEncoder::new(width, height) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Skipping: {e}");
                unsafe { libc::close(fd) };
                return;
            }
        };

        // Encode 12 frames; frame 0 and 11 should be keyframes (I-frame interval = 11)
        let mut keyframes = vec![];
        for i in 0..12 {
            if let Some(enc) = encoder.encode_frame(fd, width, height, stride).unwrap() {
                if enc.is_keyframe {
                    keyframes.push(i);
                }
            }
        }

        assert!(keyframes.contains(&0), "frame 0 should be keyframe");
        // The exact second keyframe position depends on VA-API buffering,
        // but it should appear around frame 11 ±1
        assert!(keyframes.len() >= 2, "should have at least 2 keyframes in 12 frames: {:?}", keyframes);

        unsafe { libc::close(fd) };
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ghostframe-lib encoder::h264_vaapi::tests::full_frame 2>&1 | tail -20`
Expected: compilation error — `FullFrameEncoder` doesn't exist yet.

- [ ] **Step 3: Implement FullFrameEncoder**

Add the `FullFrameEncoder` struct and its methods to `h264_vaapi.rs`, before the existing `#[cfg(test)]` block:

```rust
/// Result of encoding a full frame.
pub struct FullFrameEncoded {
    pub payload: Vec<u8>,
    pub is_keyframe: bool,
}

/// Full-frame H.264 encoder using VA-API with DMA-BUF zero-copy import.
///
/// Unlike `H264VaapiEncoder` (which encodes 32x32 tiles), this encoder
/// handles full resolution frames. The DMA-BUF fd is imported directly
/// as a VA-API surface — no CPU pixel copies.
///
/// Falls back to CPU readback + software encode if VA-API DMA-BUF import
/// is not supported.
pub struct FullFrameEncoder {
    encoder: encoder::Video,
    pts: i64,
    use_vaapi: bool,
    width: u32,
    height: u32,
    /// I-frame every N frames (1 I + 10 P = 11-frame cycle, ~6 I-frames/sec at 60fps).
    keyframe_interval: i64,
    _hw_device_ctx: Option<BufRef>,
    hw_frames_ctx: Option<BufRef>,
    /// Software scaler for fallback path (BGRA→NV12).
    scaler: Option<scaling::Context>,
}

impl FullFrameEncoder {
    pub fn new(width: u32, height: u32) -> Result<Self, ffmpeg::Error> {
        FFMPEG_INIT.call_once(|| {
            ffmpeg::init().expect("ffmpeg::init() failed");
        });

        let keyframe_interval = 11i64; // I P P P P P P P P P P I ...

        // Try VA-API first
        if let Some(vaapi_codec) = encoder::find_by_name("h264_vaapi") {
            match Self::try_open_vaapi_full(vaapi_codec, width, height, keyframe_interval) {
                Ok(enc) => {
                    info!(width, height, "FullFrameEncoder: using h264_vaapi");
                    return Ok(enc);
                }
                Err(e) => {
                    warn!("FullFrameEncoder: VA-API failed ({e}), falling back to libx264");
                }
            }
        }

        // Fallback: libx264
        let x264_codec = encoder::find_by_name("libx264")
            .ok_or(ffmpeg::Error::EncoderNotFound)?;
        info!(width, height, "FullFrameEncoder: using libx264 software encoder");
        Self::try_open_sw_full(x264_codec, width, height, keyframe_interval)
    }
    // ... (VA-API open, SW open, encode_frame methods follow)
}
```

Key implementation details for `try_open_vaapi_full`:
- Create hw_device_ctx and hw_frames_ctx at full frame resolution (same pattern as `try_open_vaapi` but with `width`/`height` instead of tile sizes)
- Set `ctx.set_gop(keyframe_interval as u32)` to get I-frame every 11 frames
- No scaler needed for VA-API path

Key implementation details for `encode_frame`:
- VA-API path: `dup(fd)` → `av_hwframe_map()` or fallback to readback + `av_hwframe_transfer_data()`
  - Try `av_hwframe_map` first. If the driver doesn't support mapping external DMA-BUFs (returns error), fall back to mmap + upload.
  - For the mmap fallback: `libc::mmap` the fd, create a software BGRA frame, scale to NV12, then `av_hwframe_transfer_data`.
- Software path: mmap the fd, build BGRA frame, scale BGRA→YUV420P, send to encoder
- Force IDR when `pts % keyframe_interval == 0` by setting `AV_PICTURE_TYPE_I` and `KEY_FRAME` flag
- Check NAL types in output to set `is_keyframe` on the result

- [ ] **Step 4: Run the tests**

Run: `cargo test -p ghostframe-lib encoder::h264_vaapi::tests::full_frame 2>&1 | tail -20`
Expected: both tests pass (or skip if no encoder available)

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/encoder/h264_vaapi.rs
git commit -m "feat(encoder): FullFrameEncoder — full-frame VA-API H.264 with DMA-BUF import"
```

---

### Task 4: Frame-Level Protocol — Headers and Fragmentation

**Files:**
- Modify: `ghostframe-lib/src/transport/protocol.rs`

Add frame-level datagram format alongside existing tile-level format.

- [ ] **Step 1: Write the tests**

Add to the existing `mod tests` in `protocol.rs`:

```rust
    #[test]
    fn frame_header_roundtrip() {
        let original = FrameHeader {
            frame_seq: 0xCAFE_BABE,
            frag_idx: 5,
            frag_total: 12,
            timestamp_us: 42_000,
            is_keyframe: true,
        };
        let mut buf = Vec::new();
        original.encode(&mut buf);
        assert_eq!(buf.len(), FRAME_HEADER_SIZE);
        let decoded = FrameHeader::decode(&buf).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn frame_header_discriminator() {
        // Frame datagrams use bit 31 of frame_seq = 0 (frame-level)
        let frame_hdr = FrameHeader {
            frame_seq: 100,
            frag_idx: 0, frag_total: 1, timestamp_us: 0, is_keyframe: false,
        };
        let mut buf = Vec::new();
        frame_hdr.encode(&mut buf);
        assert!(!is_tile_datagram(&buf), "frame datagram should not be classified as tile");

        // Tile datagrams use bit 31 of frame_seq = 1
        let tile_dg = fragment_tile(100 | TILE_DATAGRAM_FLAG, 0, 0, Codec::Raw, &[0xAB], 0, 1200);
        assert!(is_tile_datagram(&tile_dg[0]), "tile datagram should be classified as tile");
    }

    #[test]
    fn fragment_frame_single() {
        let payload = vec![0xABu8; 100];
        let datagrams = fragment_frame(1, true, &payload, 5000, 1200);
        assert_eq!(datagrams.len(), 1);

        let (hdr, frag_payload) = decode_frame_datagram(&datagrams[0]).unwrap();
        assert_eq!(hdr.frame_seq, 1);
        assert_eq!(hdr.frag_idx, 0);
        assert_eq!(hdr.frag_total, 1);
        assert!(hdr.is_keyframe);
        assert_eq!(frag_payload, payload.as_slice());
    }

    #[test]
    fn fragment_frame_multiple() {
        let payload: Vec<u8> = (0u8..=255).cycle().take(5000).collect();
        let datagrams = fragment_frame(7, false, &payload, 999, 1200);

        // ceil(5000 / 1186) = 5 (1200 - FRAME_HEADER_SIZE = 1186)
        assert!(datagrams.len() > 1);

        // Reassemble and verify
        let mut reassembled = Vec::new();
        for dg in &datagrams {
            let (hdr, frag) = decode_frame_datagram(dg).unwrap();
            assert_eq!(hdr.frame_seq, 7);
            assert!(!hdr.is_keyframe);
            reassembled.extend_from_slice(frag);
        }
        assert_eq!(reassembled, payload);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ghostframe-lib transport::protocol::tests::frame_header 2>&1 | tail -10`
Expected: compilation error — `FrameHeader`, `fragment_frame`, etc. don't exist.

- [ ] **Step 3: Implement frame-level protocol types**

Add to `protocol.rs`, after the existing tile-level code:

```rust
// ---------------------------------------------------------------------------
// Frame-level datagram discriminator
// ---------------------------------------------------------------------------

/// Bit 31 of frame_seq distinguishes tile datagrams from frame datagrams.
/// Frame datagrams: bit 31 = 0. Tile datagrams: bit 31 = 1.
pub const TILE_DATAGRAM_FLAG: u32 = 1 << 31;

/// Returns true if the datagram is a tile-level datagram (bit 31 of frame_seq = 1).
pub fn is_tile_datagram(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    let first_u32 = u32::from_be_bytes(data[0..4].try_into().unwrap());
    (first_u32 & TILE_DATAGRAM_FLAG) != 0
}

// ---------------------------------------------------------------------------
// FrameHeader (14 bytes)
//   [0..4]  frame_seq:     u32 BE (bit 31 always 0 for frame datagrams)
//   [4..6]  frag_idx:      u16 BE
//   [6..8]  frag_total:    u16 BE
//   [8..12] timestamp_us:  u32 BE
//   [12]    flags:         u8  (bit 0 = is_keyframe)
//   [13]    reserved:      u8
// ---------------------------------------------------------------------------

pub const FRAME_HEADER_SIZE: usize = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub frame_seq: u32,
    pub frag_idx: u16,
    pub frag_total: u16,
    pub timestamp_us: u32,
    pub is_keyframe: bool,
}

impl FrameHeader {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        // Clear bit 31 to mark as frame-level datagram
        let seq = self.frame_seq & !TILE_DATAGRAM_FLAG;
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(&self.frag_idx.to_be_bytes());
        buf.extend_from_slice(&self.frag_total.to_be_bytes());
        buf.extend_from_slice(&self.timestamp_us.to_be_bytes());
        buf.push(if self.is_keyframe { 1 } else { 0 });
        buf.push(0); // reserved
    }

    pub fn decode(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < FRAME_HEADER_SIZE {
            return Err(ProtocolError::TooShort {
                expected: FRAME_HEADER_SIZE,
                got: data.len(),
            });
        }
        Ok(FrameHeader {
            frame_seq: u32::from_be_bytes(data[0..4].try_into().unwrap()) & !TILE_DATAGRAM_FLAG,
            frag_idx: u16::from_be_bytes(data[4..6].try_into().unwrap()),
            frag_total: u16::from_be_bytes(data[6..8].try_into().unwrap()),
            timestamp_us: u32::from_be_bytes(data[8..12].try_into().unwrap()),
            is_keyframe: (data[12] & 1) != 0,
        })
    }

    /// Returns `true` if this datagram is a parity (FEC) fragment.
    pub fn is_parity(&self) -> bool {
        self.frag_idx >= self.frag_total
    }
}

// ---------------------------------------------------------------------------
// fragment_frame
// ---------------------------------------------------------------------------

/// Fragment a full-frame H.264 payload into MTU-sized datagrams.
///
/// Each datagram = [FrameHeader (14 B)][payload_fragment].
pub fn fragment_frame(
    frame_seq: u32,
    is_keyframe: bool,
    payload: &[u8],
    timestamp_us: u32,
    max_datagram_size: usize,
) -> Vec<Vec<u8>> {
    let max_frag = max_datagram_size.saturating_sub(FRAME_HEADER_SIZE);
    assert!(max_frag > 0, "max_datagram_size too small to fit frame header");

    let chunks: Vec<&[u8]> = if payload.is_empty() {
        vec![&[]]
    } else {
        payload.chunks(max_frag).collect()
    };

    let frag_total = chunks.len() as u16;

    chunks
        .into_iter()
        .enumerate()
        .map(|(idx, chunk)| {
            let hdr = FrameHeader {
                frame_seq,
                frag_idx: idx as u16,
                frag_total,
                timestamp_us,
                is_keyframe,
            };
            let mut buf = Vec::with_capacity(FRAME_HEADER_SIZE + chunk.len());
            hdr.encode(&mut buf);
            buf.extend_from_slice(chunk);
            buf
        })
        .collect()
}

// ---------------------------------------------------------------------------
// decode_frame_datagram
// ---------------------------------------------------------------------------

/// Decode a frame-level datagram into its FrameHeader and payload slice.
pub fn decode_frame_datagram(data: &[u8]) -> Result<(FrameHeader, &[u8]), ProtocolError> {
    let hdr = FrameHeader::decode(data)?;
    let payload = &data[FRAME_HEADER_SIZE..];
    Ok((hdr, payload))
}

/// Maximum frame payload bytes that fit in one datagram.
pub fn max_frame_fragment_payload(max_datagram_size: usize) -> usize {
    max_datagram_size.saturating_sub(FRAME_HEADER_SIZE)
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p ghostframe-lib transport::protocol::tests 2>&1 | tail -20`
Expected: all protocol tests pass (both new and existing).

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/protocol.rs
git commit -m "feat(protocol): frame-level datagram header and fragmentation for full-frame H.264"
```

---

### Task 5: Adaptive FEC — I-Frame vs P-Frame Parity Ratios

**Files:**
- Modify: `ghostframe-lib/src/transport/fec.rs`

- [ ] **Step 1: Write the tests**

Add to the existing `mod tests` in `fec.rs`:

```rust
    #[test]
    fn fec_ratio_for_keyframe_is_higher() {
        let k_iframe = fec_group_size(true);
        let k_pframe = fec_group_size(false);
        // I-frame group size should be smaller (= more parity per fragment)
        assert!(k_iframe < k_pframe,
            "I-frame FEC group size ({k_iframe}) should be smaller than P-frame ({k_pframe})");
    }

    #[test]
    fn generate_parity_with_iframe_ratio() {
        // 8 fragments, I-frame ratio (K=2) → 4 parity packets
        let payloads: Vec<Vec<u8>> = (0u8..8).map(|i| vec![i * 10]).collect();
        let refs: Vec<&[u8]> = payloads.iter().map(|v| v.as_slice()).collect();
        let k = fec_group_size(true);
        let parity = generate_parity(&refs, k);
        assert_eq!(parity.len(), 4, "I-frame with K={k} and 8 frags should produce 4 parity groups");
    }

    #[test]
    fn generate_parity_with_pframe_ratio() {
        // 8 fragments, P-frame ratio (K=5) → 1 parity packet (group of 5 + group of 3)
        let payloads: Vec<Vec<u8>> = (0u8..8).map(|i| vec![i * 10]).collect();
        let refs: Vec<&[u8]> = payloads.iter().map(|v| v.as_slice()).collect();
        let k = fec_group_size(false);
        let parity = generate_parity(&refs, k);
        assert_eq!(parity.len(), 2, "P-frame with K={k} and 8 frags should produce 2 parity groups");
    }
```

- [ ] **Step 2: Run tests to confirm they fail**

Run: `cargo test -p ghostframe-lib transport::fec::tests::fec_ratio 2>&1 | tail -10`
Expected: compilation error — `fec_group_size` doesn't exist.

- [ ] **Step 3: Implement fec_group_size**

Add to `fec.rs`:

```rust
/// FEC group size for I-frames: K=2 → ~50% parity overhead (1 parity per 2 source fragments).
const FEC_K_IFRAME: usize = 2;

/// FEC group size for P-frames: K=5 → ~20% parity overhead (1 parity per 5 source fragments).
const FEC_K_PFRAME: usize = 5;

/// Returns the FEC group size based on whether the frame is a keyframe.
/// Keyframes get stronger FEC (smaller K = more parity) because they're
/// critical for decoder state recovery.
pub fn fec_group_size(is_keyframe: bool) -> usize {
    if is_keyframe { FEC_K_IFRAME } else { FEC_K_PFRAME }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p ghostframe-lib transport::fec::tests 2>&1 | tail -20`
Expected: all FEC tests pass.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/fec.rs
git commit -m "feat(fec): adaptive parity ratios — stronger FEC for I-frames vs P-frames"
```

---

### Task 6: FrameSubmission with DMA-BUF fd

**Files:**
- Modify: `ghostframe-lib/src/server.rs`

Extend `FrameSubmission` to carry a DMA-BUF fd for the zero-copy GPU path. The existing `pixels: Vec<u8>` field is preserved for the X11 capture fallback and M3 lossless codecs.

- [ ] **Step 1: Update FrameSubmission**

In `server.rs`, change `FrameSubmission`:

```rust
use std::os::unix::io::OwnedFd;

/// A single captured video frame ready for submission to the server.
pub struct FrameSubmission {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Bytes per row (may include padding beyond `width * 4`).
    pub stride: u32,
    /// BGRA pixel data; length must equal `stride * height`.
    /// Used by the X11 capture fallback and by M3 lossless tile codecs.
    /// May be empty when `dmabuf_fd` is provided and only H.264 is needed.
    pub pixels: Vec<u8>,
    /// DMA-BUF file descriptor for zero-copy GPU access.
    /// When present, the GPU pipeline uses this directly instead of `pixels`.
    pub dmabuf_fd: Option<OwnedFd>,
    /// Capture timestamp in microseconds.
    pub timestamp_us: u32,
    /// Optional damage hints as tile coordinates. If `None`, all tiles are checked.
    pub damage_tiles: Option<Vec<(u32, u32)>>,
}
```

- [ ] **Step 2: Fix compilation errors**

Adding the new field will break existing `FrameSubmission` construction sites. Fix them:

In `server.rs` test:
```rust
    #[test]
    fn frame_submission_basic() {
        let sub = FrameSubmission {
            width: 1920, height: 1080, stride: 1920 * 4,
            pixels: vec![0u8; 1920 * 1080 * 4],
            dmabuf_fd: None,
            timestamp_us: 0,
            damage_tiles: None,
        };
        // ...
    }
```

In `io_bridge.rs` tests (all `FrameSubmission` constructions need `dmabuf_fd: None`).

In `ghostframe-xdaemon/src/main.rs` — both DRM and X11 paths need `dmabuf_fd: None` for now (Task 8 will pass the real fd).

- [ ] **Step 3: Run the build**

Run: `cargo build --workspace 2>&1 | tail -20`
Expected: compiles cleanly.

- [ ] **Step 4: Run all tests**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: all existing tests pass.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/server.rs ghostframe-lib/src/transport/io_bridge.rs ghostframe-xdaemon/src/main.rs
git commit -m "feat(server): add dmabuf_fd to FrameSubmission for zero-copy GPU path"
```

---

### Task 7: Rewrite IoBridge::process_frame for Full-Frame Pipeline

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

This is the central integration task. The new `process_frame` uses `GpuDirtyTracker` for dirty detection and `FullFrameEncoder` for encoding when a DMA-BUF fd is available. Falls back to the existing per-tile path for X11 capture (no DMA-BUF).

- [ ] **Step 1: Add new imports and fields to IoBridge**

Add at the top of `io_bridge.rs`:

```rust
use std::os::unix::io::AsRawFd;
use std::time::Duration;

use crate::capture::gpu_diff::GpuDirtyTracker;
use crate::encoder::h264_vaapi::FullFrameEncoder;
use crate::transport::protocol::{
    build_frame_parity_datagram, fragment_frame, FrameHeader,
    FRAME_HEADER_SIZE, TILE_DATAGRAM_FLAG,
    is_tile_datagram, max_frame_fragment_payload,
};
use crate::transport::fec::fec_group_size;
```

Add new fields to the `IoBridge` struct:

```rust
    /// GPU-accelerated dirty tracker (Vulkan compute SAD).
    gpu_dirty_tracker: Option<GpuDirtyTracker>,
    /// Full-frame H.264 encoder (VA-API zero-copy).
    full_frame_encoder: Option<FullFrameEncoder>,
```

Initialize them in `IoBridge::new()`:

```rust
    let gpu_dirty_tracker = GpuDirtyTracker::new(2048 * 2).ok();
    if gpu_dirty_tracker.is_some() {
        tracing::info!("GPU dirty tracker initialized (Vulkan compute SAD)");
    }
    // full_frame_encoder is lazily initialized on first frame (needs resolution)
```

Also add `gpu_dirty_tracker: None` and `full_frame_encoder: None` to the test constructors.

- [ ] **Step 2: Implement the new process_frame logic**

Replace the body of `process_frame` with a dispatch:

```rust
    fn process_frame(&mut self, frame: FrameSubmission) {
        // Route to GPU zero-copy path or legacy CPU path based on dmabuf_fd
        if frame.dmabuf_fd.is_some() && self.gpu_dirty_tracker.is_some() {
            self.process_frame_gpu(frame);
        } else {
            self.process_frame_cpu(frame);
        }
    }
```

Move the existing `process_frame` body into `process_frame_cpu` (rename only, no logic changes).

Implement `process_frame_gpu`:

```rust
    fn process_frame_gpu(&mut self, frame: FrameSubmission) {
        let fd = frame.dmabuf_fd.as_ref().unwrap();
        let raw_fd = fd.as_raw_fd();

        self.frame_seq = self.frame_seq.wrapping_add(1);
        let seq = self.frame_seq;

        // 1. Compute max fragment size from connected sessions
        let max_dg_size = match self.compute_max_datagram_size() {
            Some(sz) => sz,
            None => return, // no connected sessions
        };

        // 2. GPU dirty detection
        let gpu_tracker = self.gpu_dirty_tracker.as_mut().unwrap();
        let dirty_tiles = match gpu_tracker.diff(raw_fd, frame.width, frame.height, frame.stride) {
            Ok(tiles) => tiles,
            Err(e) => {
                tracing::warn!("GPU diff failed: {e}, falling back to CPU path");
                self.process_frame_cpu(frame);
                return;
            }
        };

        if dirty_tiles.is_empty() {
            return; // frame unchanged
        }

        // 3. Lazily initialize full-frame encoder at current resolution
        if self.full_frame_encoder.is_none()
            || self.full_frame_encoder.as_ref().map(|e| (e.width(), e.height())) != Some((frame.width, frame.height))
        {
            match FullFrameEncoder::new(frame.width, frame.height) {
                Ok(enc) => { self.full_frame_encoder = Some(enc); }
                Err(e) => {
                    tracing::warn!("Full-frame encoder init failed: {e}, falling back to CPU");
                    self.process_frame_cpu(frame);
                    return;
                }
            }
        }

        // 4. Encode full frame
        let encoder = self.full_frame_encoder.as_mut().unwrap();
        let encoded = match encoder.encode_frame(raw_fd, frame.width, frame.height, frame.stride) {
            Ok(Some(enc)) => enc,
            Ok(None) => return, // buffered, no output yet
            Err(e) => {
                tracing::warn!("Full-frame encode failed: {e}");
                return;
            }
        };

        // 5. Fragment and send
        let datagrams = fragment_frame(
            seq,
            encoded.is_keyframe,
            &encoded.payload,
            frame.timestamp_us,
            max_dg_size,
        );

        for dg in &datagrams {
            self.send_to_all_sessions(dg);
        }

        // 6. FEC parity
        let fec_k = fec_group_size(encoded.is_keyframe);
        if datagrams.len() > 1 {
            let source_payloads: Vec<&[u8]> = datagrams.iter()
                .map(|dg| &dg[FRAME_HEADER_SIZE..])
                .collect();
            let parities = fec::generate_parity(&source_payloads, fec_k);
            for (_group_start, parity_payload) in &parities {
                // Build parity datagram with FrameHeader
                let parity_dg = build_frame_parity_datagram(
                    seq, encoded.is_keyframe, frame.timestamp_us,
                    datagrams.len() as u16, parity_payload,
                );
                self.send_to_all_sessions(&parity_dg);
            }
        }
    }
```

- [ ] **Step 3: Extract helper methods**

Extract `compute_max_datagram_size()` and `send_to_all_sessions()` from the existing code in `process_frame_cpu` to avoid duplication:

```rust
    /// Compute the maximum datagram size from the smallest connected session.
    /// Returns None if no sessions are connected.
    fn compute_max_datagram_size(&mut self) -> Option<usize> {
        let mut min_size: Option<usize> = None;
        for (handle, wt) in &self.wt_sessions {
            if !wt.is_connected() { continue; }
            if let Some(conn) = self.server.connections.get_mut(handle) {
                if let Some(sz) = conn.datagrams().max_size() {
                    let usable = sz.saturating_sub(Self::WT_VARINT_OVERHEAD);
                    min_size = Some(match min_size {
                        Some(prev) => prev.min(usable),
                        None => usable,
                    });
                }
            }
        }
        min_size.filter(|&sz| sz > 0)
    }

    /// Send a datagram to all connected WebTransport sessions.
    fn send_to_all_sessions(&mut self, dg: &[u8]) {
        for (handle, wt) in &mut self.wt_sessions {
            if !wt.is_connected() { continue; }
            if let Some(conn) = self.server.connections.get_mut(handle) {
                if let Err(e) = wt.send_datagram(conn, dg) {
                    tracing::trace!(?handle, error = ?e, "datagram send failed");
                }
            }
        }
    }
```

Also add `build_frame_parity_datagram` as a free function in `protocol.rs`:

```rust
/// Build a parity datagram for a full-frame fragment set.
pub fn build_frame_parity_datagram(
    frame_seq: u32,
    is_keyframe: bool,
    timestamp_us: u32,
    frag_total: u16,
    parity_payload: &[u8],
) -> Vec<u8> {
    let hdr = FrameHeader {
        frame_seq,
        frag_idx: frag_total, // parity index starts at frag_total
        frag_total,
        timestamp_us,
        is_keyframe,
    };
    let mut buf = Vec::with_capacity(FRAME_HEADER_SIZE + parity_payload.len());
    hdr.encode(&mut buf);
    buf.extend_from_slice(parity_payload);
    buf
}
```

- [ ] **Step 4: Update process_frame_cpu to use tile flag**

In `process_frame_cpu`, when calling `fragment_tile`, OR the frame_seq with `TILE_DATAGRAM_FLAG` so the client can distinguish tile datagrams:

```rust
    let datagrams = fragment_tile(
        seq | TILE_DATAGRAM_FLAG,  // set bit 31 for tile-level
        *tile_x as u8,
        // ...
    );
```

- [ ] **Step 5: Add width()/height() accessors to FullFrameEncoder**

In `h264_vaapi.rs`, add:
```rust
    pub fn width(&self) -> u32 { self.width }
    pub fn height(&self) -> u32 { self.height }
```

- [ ] **Step 6: Run the build**

Run: `cargo build --workspace 2>&1 | tail -20`
Expected: compiles cleanly.

- [ ] **Step 7: Run all tests**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: all existing tests pass plus the new ones.

- [ ] **Step 8: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs ghostframe-lib/src/transport/protocol.rs ghostframe-lib/src/encoder/h264_vaapi.rs
git commit -m "feat(io_bridge): full-frame GPU pipeline with GpuDirtyTracker + FullFrameEncoder"
```

---

### Task 8: Xdaemon — Pass DMA-BUF fd to FrameSubmission

**Files:**
- Modify: `ghostframe-xdaemon/src/main.rs`

Change the DRM capture path to pass the DMA-BUF fd directly instead of reading back pixels.

- [ ] **Step 1: Update the DRM capture path**

In `main.rs`, modify the `CaptureBackend::Drm` match arm:

```rust
CaptureBackend::Drm => match drm_capture::capture_prime_fd() {
    Ok((dmabuf_fd, geom)) => {
        let timestamp_us =
            (frame_count * frame_interval.as_micros() as u64) as u32;
        Some(FrameSubmission {
            width: geom.width,
            height: geom.height,
            stride: geom.stride,
            pixels: Vec::new(), // GPU path doesn't need CPU pixels
            dmabuf_fd: Some(dmabuf_fd),
            timestamp_us,
            damage_tiles: damage_tiles.clone(),
        })
    }
    Err(e) => {
        tracing::warn!("DRM capture failed: {e}");
        None
    }
},
```

- [ ] **Step 2: Simplify the CaptureBackend enum**

The DRM backend no longer needs a `VulkanReadback` instance (pixel readback is gone):

```rust
enum CaptureBackend {
    /// DRM/KMS capture — passes DMA-BUF fd directly for zero-copy GPU pipeline.
    Drm,
    /// X11 GetImage fallback (for containers without DRM).
    X11 { capture: Box<x11_capture::X11Capture> },
}
```

Update the backend selection code to remove VulkanReadback initialization:

```rust
let backend = match drm_capture::capture_prime_fd() {
    Ok((fd, geom)) => {
        drop(fd);
        tracing::info!(width = geom.width, height = geom.height, "DRM capture available (zero-copy GPU path)");
        CaptureBackend::Drm
    }
    Err(e) => {
        tracing::warn!("DRM capture unavailable: {e}");
        tracing::info!("Falling back to X11 capture");
        let capture = x11_capture::X11Capture::new()?;
        CaptureBackend::X11 { capture: Box::new(capture) }
    }
};
```

Remove the `use ghostframe_lib::capture::dmabuf::{readback_dmabuf, VulkanReadback};` import (no longer needed). Keep the import of `FrameSubmission` and `GhostframeServer`.

- [ ] **Step 3: Run the build**

Run: `cargo build --workspace 2>&1 | tail -20`
Expected: compiles cleanly. Warnings about unused `VulkanReadback` import are expected — remove it.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-xdaemon/src/main.rs
git commit -m "feat(xdaemon): pass DMA-BUF fd directly for zero-copy GPU pipeline"
```

---

### Task 9: Client — Full-Frame Decoder and Renderer

**Files:**
- Modify: `ghostframe-web-client/src/decoder.ts`
- Modify: `ghostframe-web-client/src/renderer.ts`

- [ ] **Step 1: Add FrameHeader decode and FullFrameDecoder to decoder.ts**

Add to `decoder.ts`:

```typescript
// ---------------------------------------------------------------------------
// Frame-level protocol (full-frame H.264)
// ---------------------------------------------------------------------------

export const FRAME_HEADER_SIZE = 14;

/// Bit 31 of frame_seq distinguishes tile vs frame datagrams.
export const TILE_DATAGRAM_FLAG = 0x80000000;

export interface FrameHeader {
  frameSeq: number;
  fragIdx: number;
  fragTotal: number;
  timestampUs: number;
  isKeyframe: boolean;
}

export function isTileDatagram(view: DataView, offset: number): boolean {
  const firstU32 = view.getUint32(offset, false);
  return (firstU32 & TILE_DATAGRAM_FLAG) !== 0;
}

export function decodeFrameHeader(view: DataView, offset: number): FrameHeader {
  return {
    frameSeq: view.getUint32(offset, false) & ~TILE_DATAGRAM_FLAG,
    fragIdx: view.getUint16(offset + 4, false),
    fragTotal: view.getUint16(offset + 6, false),
    timestampUs: view.getUint32(offset + 8, false),
    isKeyframe: (view.getUint8(offset + 12) & 1) !== 0,
  };
}

export function frameKey(frameSeq: number): string {
  return `frame:${frameSeq}`;
}

export interface FrameAssembly {
  header: FrameHeader;
  fragments: (Uint8Array | null)[];
  received: number;
}

/**
 * Full-frame H.264 decoder using WebCodecs VideoDecoder.
 * Single instance for the entire frame (not per-tile).
 */
export class FullFrameDecoder {
  private decoder: VideoDecoder;
  private latestFrame: VideoFrame | null = null;

  constructor(
    private onFrame: (frame: VideoFrame) => void,
    width: number,
    height: number,
  ) {
    this.decoder = new VideoDecoder({
      output: (frame: VideoFrame) => {
        if (this.latestFrame) {
          this.latestFrame.close();
        }
        this.latestFrame = frame;
        this.onFrame(frame);
      },
      error: (e: DOMException) => {
        console.error('Full-frame H264 decode error:', e.message);
      },
    });

    this.decoder.configure({
      codec: 'avc1.42001e',
      codedWidth: width,
      codedHeight: height,
      optimizeForLatency: true,
    });
  }

  decode(nalData: Uint8Array, isKeyframe: boolean) {
    if (this.decoder.state === 'closed') return;

    const chunk = new EncodedVideoChunk({
      type: isKeyframe ? 'key' : 'delta',
      timestamp: 0,
      data: nalData,
    });

    this.decoder.decode(chunk);
  }

  close() {
    if (this.decoder.state !== 'closed') {
      this.decoder.close();
    }
    if (this.latestFrame) {
      this.latestFrame.close();
      this.latestFrame = null;
    }
  }
}
```

- [ ] **Step 2: Add drawFullFrame to renderer.ts**

Add to `TileRenderer`:

```typescript
  /** Draw a full-frame VideoFrame covering the entire canvas. */
  drawFullFrame(frame: VideoFrame) {
    const w = frame.displayWidth;
    const h = frame.displayHeight;
    if (this.canvas.width !== w || this.canvas.height !== h) {
      this.resize(w, h);
    }
    this.ctx.drawImage(frame, 0, 0);
  }
```

- [ ] **Step 3: Commit**

```bash
git add ghostframe-web-client/src/decoder.ts ghostframe-web-client/src/renderer.ts
git commit -m "feat(web-client): FullFrameDecoder and drawFullFrame for full-frame H.264 path"
```

---

### Task 10: Client — Frame-Level Reassembly in main.ts

**Files:**
- Modify: `ghostframe-web-client/src/main.ts`

Add frame-level reassembly alongside the existing tile-level path.

- [ ] **Step 1: Add imports**

Add to the import from `./decoder`:

```typescript
import {
  // existing tile imports...
  DATAGRAM_HEADER_SIZE, TILE_HEADER_SIZE, TILE_SIZE, Codec,
  decodeDatagramHeader, decodeTileHeader, tileKey, TileAssembly, H264TileDecoder,
  // new frame imports:
  FRAME_HEADER_SIZE, TILE_DATAGRAM_FLAG, FrameAssembly,
  isTileDatagram, decodeFrameHeader, frameKey, FullFrameDecoder,
} from './decoder';
```

- [ ] **Step 2: Add frame-level state and decoder**

After the existing `h264Decoders` map:

```typescript
  // Full-frame H.264 decoder (single instance, not per-tile)
  let fullFrameDecoder: FullFrameDecoder | null = null;

  // Frame assembly state: key -> FrameAssembly
  const frameAssemblies = new Map<string, FrameAssembly>();
  let latestFullFrameSeq = 0;
```

- [ ] **Step 3: Add frame-level reassembly and decode**

In the datagram read loop, before the tile datagram processing, add:

```typescript
    const view = new DataView(value.buffer, value.byteOffset, value.byteLength);

    // Dispatch: frame-level (bit 31 = 0) or tile-level (bit 31 = 1)
    if (!isTileDatagram(view, 0)) {
      // Frame-level datagram
      if (value.byteLength < FRAME_HEADER_SIZE) {
        continue;
      }
      const frameHdr = decodeFrameHeader(view, 0);
      lossTracker.onDatagram();

      // Stale frame discard
      if (frameHdr.frameSeq < latestFullFrameSeq - 2) {
        continue;
      }
      if (frameHdr.frameSeq > latestFullFrameSeq) {
        latestFullFrameSeq = frameHdr.frameSeq;
      }

      // Evict stale frame assemblies
      for (const [k, asm] of frameAssemblies) {
        const seq = parseInt(k.split(':')[1], 10);
        if (seq < latestFullFrameSeq - 2) {
          if (asm.received < asm.fragments.length) {
            lossTracker.onStaleTile(asm.fragments.length, asm.received);
          }
          frameAssemblies.delete(k);
        }
      }

      // Parity datagram for frames
      if (frameHdr.fragIdx >= frameHdr.fragTotal) {
        // FEC parity — store for recovery (similar to tile parity)
        continue;
      }

      const fKey = frameKey(frameHdr.frameSeq);
      const payloadOffset = FRAME_HEADER_SIZE;
      const fragData = new Uint8Array(
        value.buffer, value.byteOffset + payloadOffset,
        value.byteLength - payloadOffset,
      );

      let asm = frameAssemblies.get(fKey);
      if (!asm) {
        asm = {
          header: frameHdr,
          fragments: new Array(frameHdr.fragTotal).fill(null),
          received: 0,
        };
        frameAssemblies.set(fKey, asm);
      }

      if (asm.fragments[frameHdr.fragIdx] === null) {
        asm.fragments[frameHdr.fragIdx] = fragData.slice();
        asm.received += 1;
      }

      // All fragments arrived — reassemble and decode
      if (asm.received === frameHdr.fragTotal) {
        frameAssemblies.delete(fKey);

        const totalLen = asm.fragments.reduce((acc, f) => acc + (f ? f.byteLength : 0), 0);
        const payload = new Uint8Array(totalLen);
        let off = 0;
        for (const frag of asm.fragments) {
          if (frag) {
            payload.set(frag, off);
            off += frag.byteLength;
          }
        }

        // Initialize full-frame decoder on first frame
        if (!fullFrameDecoder) {
          fullFrameDecoder = new FullFrameDecoder((frame: VideoFrame) => {
            renderer.drawFullFrame(frame);
          }, 1920, 1080); // Initial size — WebCodecs auto-adjusts from SPS
        }

        fullFrameDecoder.decode(payload, asm.header.isKeyframe);

        if (!firstTileRendered) {
          firstTileRendered = true;
          log(`First full frame: ${payload.byteLength}B ${asm.header.isKeyframe ? '(keyframe)' : ''}`);
          statusEl.textContent = 'Receiving frames';
        }
      }

      continue; // Don't fall through to tile processing
    }

    // --- Tile-level datagram processing (existing code, unchanged) ---
    // Strip the TILE_DATAGRAM_FLAG from frame_seq before processing
```

- [ ] **Step 4: Fix tile datagram frame_seq masking**

In the existing tile processing code, strip the `TILE_DATAGRAM_FLAG` bit:

```typescript
    const dgramHdr = decodeDatagramHeader(view, 0);
    // Mask off the tile discriminator bit for tile processing
    dgramHdr.frameSeq = dgramHdr.frameSeq & ~TILE_DATAGRAM_FLAG;
```

- [ ] **Step 5: Build the client**

Run: `cd ghostframe-web-client && npm run build 2>&1 | tail -10`
Expected: builds without errors.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-web-client/src/main.ts
git commit -m "feat(web-client): frame-level reassembly and full-frame decode in main.ts"
```

---

### Task 11: Conditional NACK Retransmission

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`
- Modify: `ghostframe-lib/src/transport/protocol.rs`
- Modify: `ghostframe-web-client/src/main.ts`

NACK-based retransmission for missing frame fragments, gated on QUIC RTT.

- [ ] **Step 1: Add NACK wire format to protocol.rs**

Add to `protocol.rs`:

```rust
// ---------------------------------------------------------------------------
// NACK message (sent by client on bidi stream)
// ---------------------------------------------------------------------------

/// NACK message size: frame_seq (4) + frag_idx (2) = 6 bytes.
pub const NACK_SIZE: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NackMessage {
    pub frame_seq: u32,
    pub frag_idx: u16,
}

impl NackMessage {
    pub fn encode(&self) -> [u8; NACK_SIZE] {
        let mut buf = [0u8; NACK_SIZE];
        buf[0..4].copy_from_slice(&self.frame_seq.to_be_bytes());
        buf[4..6].copy_from_slice(&self.frag_idx.to_be_bytes());
        buf
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < NACK_SIZE {
            return None;
        }
        Some(NackMessage {
            frame_seq: u32::from_be_bytes(data[0..4].try_into().unwrap()),
            frag_idx: u16::from_be_bytes(data[4..6].try_into().unwrap()),
        })
    }
}
```

- [ ] **Step 2: Server-side: store recent frame fragments and handle NACKs**

In `io_bridge.rs`, add a ring buffer for recent frame fragments:

```rust
    /// Recent frame fragments for NACK retransmission. Key: (frame_seq, frag_idx).
    /// Only kept while RTT allows retransmission.
    recent_frame_fragments: HashMap<(u32, u16), Vec<u8>>,
    /// Frame sequences currently stored (for cleanup).
    recent_frame_seqs: Vec<u32>,
```

In `process_frame_gpu`, after sending datagrams, store them:

```rust
    // Store fragments for potential NACK retransmission
    // Only keep the last 3 frames to bound memory
    self.recent_frame_fragments.retain(|(seq, _), _| {
        *seq > seq.wrapping_sub(3)
    });
    for dg in &datagrams {
        let hdr = FrameHeader::decode(dg).unwrap();
        self.recent_frame_fragments.insert((hdr.frame_seq, hdr.frag_idx), dg.clone());
    }
```

In `drain_app_events`, when processing feedback stream data, also check for NACK messages:

```rust
    // Check RTT before retransmitting
    fn handle_nack(&mut self, nack: NackMessage, handle: &ConnectionHandle) {
        if let Some(conn) = self.server.connections.get_mut(handle) {
            let rtt = conn.stats().path.rtt;
            let frame_interval = Duration::from_micros(16_667); // ~60fps

            if rtt < frame_interval {
                if let Some(dg) = self.recent_frame_fragments.get(&(nack.frame_seq, nack.frag_idx)) {
                    if let Some(wt) = self.wt_sessions.get_mut(handle) {
                        let _ = wt.send_datagram(conn, dg);
                        tracing::trace!(frame_seq = nack.frame_seq, frag_idx = nack.frag_idx, "NACK retransmit");
                    }
                }
            } else {
                tracing::trace!(
                    ?rtt, frame_seq = nack.frame_seq,
                    "NACK skipped — RTT too high for retransmission"
                );
            }
        }
    }
```

- [ ] **Step 3: Client-side: send NACKs for missing frame fragments**

In `main.ts`, add NACK sending when a frame assembly times out with missing fragments. Use a simple deadline: if a frame is 1 frame interval old and incomplete, send NACKs for missing fragments.

```typescript
    // After frame assembly check, if almost complete but missing 1-2 fragments:
    if (asm.received >= frameHdr.fragTotal - 2 && asm.received < frameHdr.fragTotal) {
      // Check if we already sent NACKs for this frame
      if (feedbackWriter && !asm.nackedSent) {
        for (let i = 0; i < asm.fragments.length; i++) {
          if (asm.fragments[i] === null) {
            const nackBuf = new Uint8Array(6);
            const nackView = new DataView(nackBuf.buffer);
            nackView.setUint32(0, frameHdr.frameSeq, false);
            nackView.setUint16(4, i, false);
            feedbackWriter.write(nackBuf).catch(() => {});
          }
        }
        (asm as any).nackedSent = true;
      }
    }
```

- [ ] **Step 4: Run the build**

Run: `cargo build --workspace 2>&1 | tail -10 && cd ghostframe-web-client && npm run build 2>&1 | tail -10`
Expected: compiles cleanly on both sides.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/protocol.rs ghostframe-lib/src/transport/io_bridge.rs ghostframe-web-client/src/main.ts
git commit -m "feat(transport): conditional NACK retransmission gated on QUIC RTT"
```

---

### Task 12: E2E Test Adaptation

**Files:**
- Modify: `ghostframe-lib/tests/e2e.rs`

Update the existing E2E test to verify full-frame encoding works end-to-end. The test uses the container infrastructure, so it validates the entire pipeline: DRM capture → DMA-BUF → Vulkan SAD → VA-API encode → QUIC → WebCodecs decode → canvas.

- [ ] **Step 1: Review the existing E2E test**

Read `ghostframe-lib/tests/e2e.rs` and understand the current test setup. The test should continue to work because:
- If the container has DRM (`/dev/dri`), it uses the GPU path (DMA-BUF → full-frame)
- If the container uses X11 capture (no DRM), it falls back to the CPU path (per-tile)

The Playwright assertion (checking pixel color on canvas) works regardless of which path was used.

- [ ] **Step 2: Verify the test still compiles**

Run: `cargo test -p ghostframe-lib --test e2e --no-run 2>&1 | tail -10`
Expected: compiles (may not run without containers/GPU).

- [ ] **Step 3: Commit (if any test changes were needed)**

```bash
git add ghostframe-lib/tests/e2e.rs
git commit -m "test(e2e): adapt E2E test for full-frame H.264 pipeline"
```

---

### Task 13: Cleanup and Verification

- [ ] **Step 1: Run all unit tests**

Run: `cargo test --workspace 2>&1 | tail -30`
Expected: all tests pass.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace 2>&1 | tail -30`
Expected: no errors. Warnings about dead code in `DirtyTracker` and `H264VaapiEncoder` are expected (preserved for M3).

- [ ] **Step 3: Build the web client**

Run: `cd ghostframe-web-client && npm run build 2>&1 | tail -10`
Expected: builds cleanly.

- [ ] **Step 4: Verify dead code annotations**

Add `#[allow(dead_code)]` comments to the preserved M3 infrastructure:
- `DirtyTracker` in `tile/mod.rs` — add a doc comment: `// Preserved for M3 lossless tile codecs`
- `H264VaapiEncoder` in `encoder/h264_vaapi.rs` — add: `// Preserved for M3 per-tile encoding`
- `H264TileDecoder` in `decoder.ts` — add: `// Preserved for M3 per-tile decoding`

- [ ] **Step 5: Final commit**

```bash
git add -A
git commit -m "chore: dead code annotations for M3-preserved infrastructure"
```
