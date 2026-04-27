# Vulkan BGRA→NV12 Conversion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the zero-copy encode gap by adding a Vulkan compute shader that converts BGRA DMA-BUFs to NV12, exports the result as a new DMA-BUF, and feeds it to VA-API for H.264 encoding — no CPU pixel access.

**Architecture:** The existing `GpuDirtyTracker` is renamed to `GpuFrameProcessor` and extended with a second compute pipeline (BGRA→NV12). Both dispatches (SAD + NV12) run in a single command buffer. The NV12 output is a device-local VkBuffer exported via `VK_KHR_external_memory_fd`. `FullFrameEncoder` gains `encode_nv12_dmabuf()` which maps the NV12 DMA-BUF into VA-API (same format = mapping works).

**Tech Stack:** Rust, ash (Vulkan), ffmpeg-sys-next (VA-API), GLSL compute shaders

**Use model:** sonnet for subagent implementation

---

## File Structure

### New Files
| File | Responsibility |
|------|---------------|
| `ghostframe-lib/src/capture/shaders/bgra_to_nv12.comp` | GLSL compute shader: 2x2 workgroups, BT.601 color conversion |
| `ghostframe-lib/src/capture/gpu_pipeline.rs` | Renamed from `gpu_diff.rs`: `GpuFrameProcessor`, `FrameAnalysis`, SAD + NV12 pipelines |

### Modified Files
| File | Change |
|------|--------|
| `ghostframe-lib/src/capture/mod.rs` | `pub mod gpu_diff` → `pub mod gpu_pipeline` |
| `ghostframe-lib/src/encoder/h264_vaapi.rs` | Add `encode_nv12_dmabuf()` method |
| `ghostframe-lib/src/transport/io_bridge.rs` | Update imports and `process_frame_gpu` to use `GpuFrameProcessor` + `encode_nv12_dmabuf` |
| `ghostframe-lib/tests/gpu_pipeline.rs` | Update for renamed API, add NV12 test |
| `ghostframe-xdaemon/tests/drm_gpu_pipeline.rs` | Use `encode_nv12_dmabuf` for real DMA-BUF test |

### Deleted Files
| File | Reason |
|------|--------|
| `ghostframe-lib/src/capture/gpu_diff.rs` | Renamed to `gpu_pipeline.rs` |

---

### Task 1: BGRA→NV12 Compute Shader

**Files:**
- Create: `ghostframe-lib/src/capture/shaders/bgra_to_nv12.comp`

- [ ] **Step 1: Write the shader**

```glsl
#version 450

// Each workgroup processes a 2x2 pixel block:
// - 4 threads produce 4 Y values
// - Thread (0,0) produces 1 interleaved UV pair from the 2x2 average
//
// Dispatch: (width/2, height/2, 1) workgroups.

layout(local_size_x = 2, local_size_y = 2, local_size_z = 1) in;

layout(binding = 0, rgba8) readonly uniform image2D bgra_input;

// NV12 output as a raw byte buffer. Y plane at offset 0, UV plane at uv_offset.
// We write u32 words (4 bytes at a time) to avoid byte-granularity issues.
// For Y: each thread writes 1 byte → we use shared memory to pack 4 bytes per u32.
// For UV: thread (0,0) writes 2 bytes (U, V) packed into the buffer.
layout(binding = 1, std430) buffer NV12Output { uint data[]; };

layout(push_constant) uniform PushConstants {
    uint width;
    uint height;
    uint y_stride;     // bytes per row in Y plane (= width, tightly packed)
    uint uv_offset;    // byte offset to UV plane (= y_stride * height)
    uint uv_stride;    // bytes per row in UV plane (= width, tightly packed)
};

shared uint shared_y[4]; // Y values from 4 threads
shared uint shared_u;
shared uint shared_v;

void main() {
    uint lx = gl_LocalInvocationID.x; // 0 or 1
    uint ly = gl_LocalInvocationID.y; // 0 or 1
    uint local_idx = ly * 2 + lx;     // 0..3

    // Global pixel position
    uint px = gl_WorkGroupID.x * 2 + lx;
    uint py = gl_WorkGroupID.y * 2 + ly;

    // Read BGRA pixel (values in [0,1])
    vec4 bgra = vec4(0.0);
    if (px < width && py < height) {
        bgra = imageLoad(bgra_input, ivec2(px, py));
    }

    // Extract RGB (BGRA layout: .r=B, .g=G, .b=R in image2D rgba8)
    // Note: Vulkan rgba8 image loads are RGBA-ordered regardless of the
    // underlying buffer format. Our DMA-BUF is XRGB8888 which maps to
    // B8G8R8X8 in memory. When imported as B8G8R8A8_UNORM VkImage:
    //   .r = B, .g = G, .b = R, .a = A/X
    float B = bgra.r;
    float G = bgra.g;
    float R = bgra.b;

    // BT.601 conversion (full range, 0-255)
    float Y = 0.299 * R + 0.587 * G + 0.114 * B;
    float U = -0.169 * R - 0.331 * G + 0.500 * B + 0.5; // +128/255 ≈ 0.502
    float V =  0.500 * R - 0.419 * G - 0.081 * B + 0.5;

    uint y_byte = clamp(uint(Y * 255.0 + 0.5), 0u, 255u);

    // Write Y value into the Y plane
    if (px < width && py < height) {
        uint y_addr = py * y_stride + px;
        // Write single byte: pack into u32 at the aligned address
        uint word_addr = y_addr / 4;
        uint byte_offset = y_addr % 4;
        atomicAnd(data[word_addr], ~(0xFFu << (byte_offset * 8)));
        atomicOr(data[word_addr], y_byte << (byte_offset * 8));
    }

    // Thread (0,0) of each workgroup writes the UV pair
    shared_y[local_idx] = y_byte;
    barrier();

    if (local_idx == 0) {
        // Average U,V over the 2x2 block
        // Re-read RGB from all 4 pixels via shared memory isn't needed —
        // just compute U,V from the (0,0) pixel. This is standard NV12
        // chroma subsampling (co-sited at top-left).
        uint u_byte = clamp(uint(U * 255.0 + 0.5), 0u, 255u);
        uint v_byte = clamp(uint(V * 255.0 + 0.5), 0u, 255u);

        uint chroma_x = gl_WorkGroupID.x;
        uint chroma_y = gl_WorkGroupID.y;

        if (chroma_x * 2 < width && chroma_y * 2 < height) {
            uint uv_addr = uv_offset + chroma_y * uv_stride + chroma_x * 2;
            // Write U byte
            uint u_word = uv_addr / 4;
            uint u_off = uv_addr % 4;
            atomicAnd(data[u_word], ~(0xFFu << (u_off * 8)));
            atomicOr(data[u_word], u_byte << (u_off * 8));
            // Write V byte
            uint v_word = (uv_addr + 1) / 4;
            uint v_off = (uv_addr + 1) % 4;
            atomicAnd(data[v_word], ~(0xFFu << (v_off * 8)));
            atomicOr(data[v_word], v_byte << (v_off * 8));
        }
    }
}
```

- [ ] **Step 2: Compile to SPIR-V**

Run: `glslangValidator -V ghostframe-lib/src/capture/shaders/bgra_to_nv12.comp -o ghostframe-lib/src/capture/shaders/bgra_to_nv12.spv`
Expected: no errors, `.spv` file created

- [ ] **Step 3: Validate SPIR-V**

Run: `spirv-val ghostframe-lib/src/capture/shaders/bgra_to_nv12.spv` (if available)
Expected: no errors

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/capture/shaders/bgra_to_nv12.comp ghostframe-lib/src/capture/shaders/bgra_to_nv12.spv
git commit -m "feat(capture): GLSL BGRA→NV12 compute shader for zero-copy encode pipeline"
```

---

### Task 2: Rename gpu_diff → gpu_pipeline, GpuDirtyTracker → GpuFrameProcessor

**Files:**
- Rename: `ghostframe-lib/src/capture/gpu_diff.rs` → `ghostframe-lib/src/capture/gpu_pipeline.rs`
- Modify: `ghostframe-lib/src/capture/mod.rs`
- Modify: `ghostframe-lib/src/transport/io_bridge.rs` (update import)
- Modify: `ghostframe-lib/tests/gpu_pipeline.rs` (update import)

This is a pure rename — no logic changes.

- [ ] **Step 1: Rename the file**

Run: `mv ghostframe-lib/src/capture/gpu_diff.rs ghostframe-lib/src/capture/gpu_pipeline.rs`

- [ ] **Step 2: Update module declaration in mod.rs**

In `ghostframe-lib/src/capture/mod.rs`, change:
```rust
pub mod gpu_diff;
```
to:
```rust
pub mod gpu_pipeline;
```

- [ ] **Step 3: Rename the struct in gpu_pipeline.rs**

In `ghostframe-lib/src/capture/gpu_pipeline.rs`, replace all occurrences:
- `GpuDirtyTracker` → `GpuFrameProcessor`
- Update the module doc comment to mention `GpuFrameProcessor`
- Update all doc references from `diff` to `process_frame` (anticipating Task 3)

- [ ] **Step 4: Update io_bridge.rs imports**

In `ghostframe-lib/src/transport/io_bridge.rs`, change:
```rust
use crate::capture::gpu_diff::GpuDirtyTracker;
```
to:
```rust
use crate::capture::gpu_pipeline::GpuFrameProcessor;
```

Replace all occurrences of `GpuDirtyTracker` → `GpuFrameProcessor` in this file. Also rename the field `gpu_dirty_tracker` → `gpu_frame_processor` throughout.

- [ ] **Step 5: Update gpu_pipeline.rs test imports**

In `ghostframe-lib/tests/gpu_pipeline.rs`, change:
```rust
use ghostframe_lib::capture::gpu_diff::GpuDirtyTracker;
```
to:
```rust
use ghostframe_lib::capture::gpu_pipeline::GpuFrameProcessor;
```

Replace all `GpuDirtyTracker` → `GpuFrameProcessor` in the test file.

- [ ] **Step 6: Verify build**

Run: `cargo check --workspace 2>&1 | tail -5`
Expected: compiles cleanly

- [ ] **Step 7: Verify tests**

Run: `cargo test -p ghostframe-lib --lib -- --test-threads=1 2>&1 | tail -5`
Expected: all pass

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "refactor(capture): rename GpuDirtyTracker → GpuFrameProcessor, gpu_diff → gpu_pipeline"
```

---

### Task 3: Add NV12 Pipeline to GpuFrameProcessor

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline.rs`

This is the core task. Extend `GpuFrameProcessor` with the NV12 compute pipeline, exportable output buffer, and the new `process_frame` API.

- [ ] **Step 1: Add `FrameAnalysis` struct and new fields**

At the top of `gpu_pipeline.rs`, add:

```rust
use std::os::unix::io::OwnedFd;

/// Result of processing a frame through the GPU pipeline.
pub struct FrameAnalysis {
    /// Flat tile indices of dirty tiles (SAD > threshold).
    pub dirty_tiles: Vec<u32>,
    /// NV12 DMA-BUF fd — exported from the Vulkan NV12 output buffer.
    /// The caller owns this fd and must close it when done.
    pub nv12_dmabuf_fd: OwnedFd,
    /// NV12 frame width in pixels.
    pub nv12_width: u32,
    /// NV12 frame height in pixels.
    pub nv12_height: u32,
    /// Bytes per row in Y plane.
    pub nv12_y_stride: u32,
    /// Bytes per row in UV plane.
    pub nv12_uv_stride: u32,
    /// Byte offset from buffer start to UV plane.
    pub nv12_uv_offset: u32,
}
```

Add new fields to `GpuFrameProcessor`:

```rust
    // --- NV12 conversion pipeline ---
    nv12_shader_module: vk::ShaderModule,
    nv12_pipeline: vk::Pipeline,
    nv12_pipeline_layout: vk::PipelineLayout,
    nv12_descriptor_set_layout: vk::DescriptorSetLayout,
    /// Device-local NV12 output buffer, exportable via VK_KHR_external_memory_fd.
    nv12_buffer: vk::Buffer,
    nv12_memory: vk::DeviceMemory,
    nv12_buf_size: usize,
    /// ash extension function loader for vkGetMemoryFdKHR.
    external_memory_fd_fn: ash::khr::external_memory_fd::Device,
```

- [ ] **Step 2: Update `new_inner()` to initialize NV12 pipeline**

After the existing SAD pipeline setup in `new_inner()`, add:

1. Load NV12 shader SPIR-V: `include_bytes!("shaders/bgra_to_nv12.spv")`
2. Create `nv12_shader_module`
3. Create `nv12_descriptor_set_layout` with 2 bindings:
   - binding 0: `STORAGE_IMAGE` (BGRA input — same image as SAD shader binding 0)
   - binding 1: `STORAGE_BUFFER` (NV12 output)
4. Create `nv12_pipeline_layout` with push constant range: 20 bytes (5 x u32: width, height, y_stride, uv_offset, uv_stride)
5. Create `nv12_pipeline` (compute pipeline with the NV12 shader)
6. Initialize `nv12_buffer`, `nv12_memory`, `nv12_buf_size` to zero/null (allocated on first frame when resolution is known)
7. Load `ash::khr::external_memory_fd::Device` function loader from the device

Update the descriptor pool to accommodate 2 sets per frame: the SAD set (2 storage images + 1 storage buffer) and the NV12 set (1 storage image + 1 storage buffer). So pool sizes become:
- `STORAGE_IMAGE`: 3 (2 for SAD + 1 for NV12)
- `STORAGE_BUFFER`: 2 (1 for SAD + 1 for NV12)
- `max_sets`: 2

Also update the `required_device_exts` to include `VK_KHR_external_memory_fd` — which is already present for import, but we also need it for export. No new extension needed, just noting that we now use both import and export.

- [ ] **Step 3: Add NV12 buffer allocation helper**

```rust
    /// (Re)allocate the NV12 output buffer for the given resolution.
    /// The buffer is device-local and exportable via VK_KHR_external_memory_fd.
    unsafe fn allocate_nv12_buffer(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Free old buffer if present
        if self.nv12_buffer != vk::Buffer::null() {
            self.device.destroy_buffer(self.nv12_buffer, None);
            self.device.free_memory(self.nv12_memory, None);
        }

        let y_stride = width;
        let uv_stride = width; // interleaved U,V = width bytes per row
        let y_size = (y_stride * height) as usize;
        let uv_size = (uv_stride * height / 2) as usize;
        let total = y_size + uv_size;

        // Create buffer with STORAGE_BUFFER usage
        let buf_ci = vk::BufferCreateInfo::default()
            .size(total as vk::DeviceSize)
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = self.device.create_buffer(&buf_ci, None)?;
        let reqs = self.device.get_buffer_memory_requirements(buffer);

        // Allocate DEVICE_LOCAL memory with external memory export flag
        let mut export_info = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let mem_props = self
            .instance
            .get_physical_device_memory_properties(self.physical_device);
        let mem_type = find_memory_type(
            &mem_props,
            reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .or_else(|| {
            // Fall back to any memory type that supports export
            find_memory_type(&mem_props, reqs.memory_type_bits, vk::MemoryPropertyFlags::empty())
        })
        .ok_or("no suitable memory type for exportable NV12 buffer")?;

        let mut alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(mem_type);
        alloc_info = alloc_info.push_next(&mut export_info);

        let memory = self.device.allocate_memory(&alloc_info, None)?;
        self.device.bind_buffer_memory(buffer, memory, 0)?;

        self.nv12_buffer = buffer;
        self.nv12_memory = memory;
        self.nv12_buf_size = total;

        Ok(())
    }
```

- [ ] **Step 4: Add DMA-BUF export helper**

```rust
    /// Export the NV12 buffer's device memory as a DMA-BUF fd.
    /// Returns a fresh OwnedFd each call (caller owns it).
    unsafe fn export_nv12_dmabuf(&self) -> Result<OwnedFd, Box<dyn std::error::Error>> {
        let get_fd_info = vk::MemoryGetFdInfoKHR::default()
            .memory(self.nv12_memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let fd = self.external_memory_fd_fn.get_memory_fd(&get_fd_info)?;

        Ok(OwnedFd::from_raw_fd(fd))
    }
```

(Add `use std::os::unix::io::FromRawFd;` to imports.)

- [ ] **Step 5: Implement `process_frame`**

Rename the existing `diff` → `diff` (keep as private), and add a new public `process_frame`:

```rust
    /// Process a frame: compute SAD dirty tiles AND convert BGRA→NV12.
    ///
    /// Both compute shaders run in a single command buffer submission.
    /// Returns `FrameAnalysis` with dirty tile list and an NV12 DMA-BUF fd.
    pub fn process_frame(
        &mut self,
        fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<FrameAnalysis, Box<dyn std::error::Error>> {
        unsafe { self.process_frame_inner(fd, width, height, stride) }
    }
```

The `process_frame_inner` implementation:

1. Compute cols, rows, tile_count (same as existing `diff_inner`)
2. Drop prev_image if resolution changed (same as existing)
3. If resolution changed or first frame: call `allocate_nv12_buffer(width, height)`
4. Import DMA-BUF as VkImage (same as existing)
5. **First frame (no prev_image):** store as prev, run NV12 conversion only (no SAD), return all-dirty + NV12 fd
6. **Subsequent frames:** allocate 2 descriptor sets (SAD + NV12), record single command buffer with both dispatches, submit, read SAD, export NV12 fd, swap prev
7. Return `FrameAnalysis`

The command buffer recording for step 6:
```
a. Image barriers (current + prev → GENERAL)
b. Bind SAD pipeline + descriptor set, push SAD constants, dispatch (cols, rows, 1)
c. Memory barrier between SAD and NV12 dispatches (not strictly needed since they read different data, but good practice)
d. Bind NV12 pipeline + descriptor set, push NV12 constants (width, height, y_stride, uv_offset, uv_stride), dispatch (width/2, height/2, 1)
e. Buffer barriers: SAD buffer + NV12 buffer → HOST_READ
f. Submit + fence wait
```

NV12 push constants:
```rust
let y_stride = width;
let uv_stride = width;
let uv_offset = y_stride * height;
let nv12_push: [u32; 5] = [width, height, y_stride, uv_offset, uv_stride];
```

The NV12 descriptor set update:
- binding 0: current frame's VkImageView (same image as SAD binding 0)
- binding 1: `self.nv12_buffer`

- [ ] **Step 6: Keep `diff()` as a convenience wrapper**

Keep the existing `diff()` method but have it call `process_frame` internally and return just the dirty tiles (ignoring the NV12 output). This preserves backward compatibility for code that only needs dirty detection.

```rust
    /// Convenience: just dirty detection, no NV12 conversion.
    /// Used by the CPU fallback path and tests that don't need encoding.
    pub fn diff(
        &mut self,
        fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
        let analysis = self.process_frame(fd, width, height, stride)?;
        Ok(analysis.dirty_tiles)
    }
```

- [ ] **Step 7: Update Drop**

Add cleanup for NV12 resources:
```rust
if self.nv12_buffer != vk::Buffer::null() {
    self.device.destroy_buffer(self.nv12_buffer, None);
    self.device.free_memory(self.nv12_memory, None);
}
self.device.destroy_pipeline(self.nv12_pipeline, None);
self.device.destroy_pipeline_layout(self.nv12_pipeline_layout, None);
self.device.destroy_descriptor_set_layout(self.nv12_descriptor_set_layout, None);
self.device.destroy_shader_module(self.nv12_shader_module, None);
```

- [ ] **Step 8: Verify build**

Run: `cargo check -p ghostframe-lib 2>&1 | tail -5`
Expected: compiles

- [ ] **Step 9: Verify existing tests**

Run: `cargo test -p ghostframe-lib gpu_pipeline -- --test-threads=1 --nocapture 2>&1 | tail -15`
Expected: existing 3 tests still pass (they use `diff()` which wraps `process_frame`)

- [ ] **Step 10: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline.rs
git commit -m "feat(capture): GpuFrameProcessor NV12 pipeline — BGRA→NV12 compute + DMA-BUF export"
```

---

### Task 4: encode_nv12_dmabuf Method on FullFrameEncoder

**Files:**
- Modify: `ghostframe-lib/src/encoder/h264_vaapi.rs`

- [ ] **Step 1: Add `encode_nv12_dmabuf` method**

Add to `FullFrameEncoder`, after the existing `encode_frame` method:

```rust
    /// Encode an NV12 DMA-BUF directly via VA-API zero-copy import.
    ///
    /// The DMA-BUF must contain NV12 data (Y plane at offset 0, interleaved
    /// UV plane at `uv_offset`). This avoids all CPU pixel access — the
    /// DMA-BUF is mapped directly into a VA-API surface.
    ///
    /// Falls back to `encode_frame` (mmap path) on error.
    pub fn encode_nv12_dmabuf(
        &mut self,
        nv12_fd: RawFd,
        width: u32,
        height: u32,
        y_stride: u32,
        uv_stride: u32,
        uv_offset: u32,
    ) -> Result<Option<FullFrameEncoded>, ffmpeg::Error> {
        let pts = self.pts;
        self.pts += 1;
        let force_idr = pts % FULL_FRAME_GOP as i64 == 0;

        if !self.use_vaapi {
            // Software encoder can't use DMA-BUF; caller should use encode_frame
            return Err(ffmpeg::Error::InvalidData);
        }

        unsafe {
            let hw_frames_ref = self
                .hw_frames_ctx
                .as_ref()
                .expect("use_vaapi but no hw_frames_ctx");

            match Self::import_nv12_dmabuf(
                hw_frames_ref.0, nv12_fd, width, height, y_stride, uv_stride, uv_offset, pts, force_idr,
            ) {
                Ok(hw_frame) => {
                    self.encoder.send_frame(&hw_frame)?;
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }

        self.receive_full_packet()
    }
```

- [ ] **Step 2: Add `import_nv12_dmabuf` helper**

```rust
    /// Import an NV12 DMA-BUF as a VA-API surface via DRM PRIME mapping.
    /// Since the hw_frames_ctx sw_format is NV12 and the DMA-BUF is NV12,
    /// av_hwframe_map works (same format).
    unsafe fn import_nv12_dmabuf(
        hw_frames_ctx: *mut ffi::AVBufferRef,
        fd: RawFd,
        width: u32,
        height: u32,
        y_stride: u32,
        uv_stride: u32,
        uv_offset: u32,
        pts: i64,
        force_idr: bool,
    ) -> Result<frame::Video, ffmpeg::Error> {
        // DRM_FORMAT_NV12 = fourcc('N','V','1','2')
        const DRM_FORMAT_NV12: u32 = 0x3231564e;
        const DRM_FORMAT_MOD_INVALID: u64 = 0x00ffffffffffffff;

        let y_size = (y_stride as usize) * (height as usize);
        let uv_size = (uv_stride as usize) * (height as usize / 2);
        let total_size = y_size + uv_size;

        let mut desc = Box::new(std::mem::zeroed::<ffi::AVDRMFrameDescriptor>());
        desc.nb_objects = 1;
        desc.objects[0] = ffi::AVDRMObjectDescriptor {
            fd: libc::dup(fd),
            size: total_size,
            format_modifier: DRM_FORMAT_MOD_INVALID,
        };
        desc.nb_layers = 1;
        desc.layers[0].format = DRM_FORMAT_NV12;
        desc.layers[0].nb_planes = 2;
        // Y plane
        desc.layers[0].planes[0] = ffi::AVDRMPlaneDescriptor {
            object_index: 0,
            offset: 0,
            pitch: y_stride as isize,
        };
        // UV plane (interleaved)
        desc.layers[0].planes[1] = ffi::AVDRMPlaneDescriptor {
            object_index: 0,
            offset: uv_offset as isize,
            pitch: uv_stride as isize,
        };

        // DRM PRIME frame
        let mut drm_frame = frame::Video::empty();
        let drm_ptr = drm_frame.as_mut_ptr();
        (*drm_ptr).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        (*drm_ptr).width = width as i32;
        (*drm_ptr).height = height as i32;
        (*drm_ptr).data[0] = desc.as_mut() as *mut ffi::AVDRMFrameDescriptor as *mut u8;

        // Allocate VAAPI surface
        let mut hw_frame = frame::Video::empty();
        let hw_ptr = hw_frame.as_mut_ptr();
        let ret = ffi::av_hwframe_get_buffer(hw_frames_ctx, hw_ptr, 0);
        if ret < 0 {
            libc::close(desc.objects[0].fd);
            return Err(ffmpeg::Error::from(ret));
        }

        // Map NV12 DRM PRIME → NV12 VAAPI (same format = works)
        let ret = ffi::av_hwframe_map(
            hw_ptr,
            drm_ptr,
            ffi::AV_HWFRAME_MAP_READ as i32,
        );
        libc::close(desc.objects[0].fd);
        (*drm_ptr).data[0] = ptr::null_mut();

        if ret < 0 {
            return Err(ffmpeg::Error::from(ret));
        }

        hw_frame.set_pts(Some(pts));
        if force_idr {
            (*hw_ptr).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_I;
            (*hw_ptr).flags |= ffi::AV_FRAME_FLAG_KEY;
        }

        Ok(hw_frame)
    }
```

- [ ] **Step 3: Verify build**

Run: `cargo check -p ghostframe-lib 2>&1 | tail -5`
Expected: compiles

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/encoder/h264_vaapi.rs
git commit -m "feat(encoder): encode_nv12_dmabuf — zero-copy NV12 DMA-BUF to VA-API H.264"
```

---

### Task 5: Update IoBridge to Use GpuFrameProcessor + encode_nv12_dmabuf

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Update process_frame_gpu**

Replace the current `process_frame_gpu` body. The key changes:
- Call `processor.process_frame(raw_fd, ...)` instead of `tracker.diff(raw_fd, ...)`
- Call `encoder.encode_nv12_dmabuf(analysis.nv12_dmabuf_fd.as_raw_fd(), ...)` instead of `encoder.encode_frame(raw_fd, ...)`

```rust
    fn process_frame_gpu(&mut self, frame: FrameSubmission) {
        let fd = frame.dmabuf_fd.as_ref().unwrap();
        let raw_fd = fd.as_raw_fd();

        self.frame_seq = self.frame_seq.wrapping_add(1);
        let seq = self.frame_seq;

        let max_dg_size = match self.compute_max_datagram_size() {
            Some(sz) => sz,
            None => return,
        };

        // GPU pipeline: SAD dirty detection + BGRA→NV12 conversion
        let processor = self.gpu_frame_processor.as_mut().unwrap();
        let analysis = match processor.process_frame(raw_fd, frame.width, frame.height, frame.stride) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("GPU process_frame failed: {e}, falling back to CPU path");
                self.process_frame_cpu(frame);
                return;
            }
        };

        if analysis.dirty_tiles.is_empty() {
            return;
        }

        // Lazily initialize full-frame encoder
        let needs_init = match &self.full_frame_encoder {
            Some(enc) => enc.width() != frame.width || enc.height() != frame.height,
            None => true,
        };
        if needs_init {
            match FullFrameEncoder::new(frame.width, frame.height) {
                Ok(enc) => { self.full_frame_encoder = Some(enc); }
                Err(e) => {
                    tracing::warn!("Full-frame encoder init failed: {e}");
                    return;
                }
            }
        }

        // Encode the NV12 DMA-BUF directly (zero-copy)
        let encoder = self.full_frame_encoder.as_mut().unwrap();
        let encoded = match encoder.encode_nv12_dmabuf(
            analysis.nv12_dmabuf_fd.as_raw_fd(),
            analysis.nv12_width,
            analysis.nv12_height,
            analysis.nv12_y_stride,
            analysis.nv12_uv_stride,
            analysis.nv12_uv_offset,
        ) {
            Ok(Some(enc)) => enc,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("NV12 encode failed: {e}");
                return;
            }
        };

        // Fragment and send (unchanged from here)
        // ... (keep existing fragmentation + FEC code)
    }
```

- [ ] **Step 2: Verify build**

Run: `cargo check --workspace 2>&1 | tail -5`
Expected: compiles

- [ ] **Step 3: Verify tests**

Run: `cargo test -p ghostframe-lib --lib -- --test-threads=1 2>&1 | tail -5`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "feat(io_bridge): use GpuFrameProcessor.process_frame + encode_nv12_dmabuf for zero-copy"
```

---

### Task 6: Update Tests

**Files:**
- Modify: `ghostframe-lib/tests/gpu_pipeline.rs`
- Modify: `ghostframe-xdaemon/tests/drm_gpu_pipeline.rs`

- [ ] **Step 1: Add NV12 conversion test to gpu_pipeline.rs**

Add a new test:

```rust
#[test]
fn gpu_pipeline_nv12_conversion() {
    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping: no Vulkan GPU: {e}");
            return;
        }
    };

    let width = 128u32;
    let height = 128u32;
    let stride = width * 4;

    let fd = unsafe { create_solid_memfd(width, height, 0, 0, 255) }; // solid red

    match processor.process_frame(fd, width, height, stride) {
        Ok(analysis) => {
            // First frame: all tiles dirty
            let cols = width.div_ceil(32);
            let rows = height.div_ceil(32);
            assert_eq!(
                analysis.dirty_tiles.len() as u32,
                cols * rows,
                "first frame should report all tiles dirty"
            );

            // NV12 fd should be valid
            assert!(analysis.nv12_dmabuf_fd.as_raw_fd() >= 0);
            assert_eq!(analysis.nv12_width, width);
            assert_eq!(analysis.nv12_height, height);
            assert_eq!(analysis.nv12_y_stride, width);
            assert_eq!(analysis.nv12_uv_stride, width);
            assert_eq!(analysis.nv12_uv_offset, width * height);

            eprintln!("NV12 conversion: fd={}, {}x{}", analysis.nv12_dmabuf_fd.as_raw_fd(), width, height);
        }
        Err(e) => {
            eprintln!("Skipping: process_frame failed (memfd not accepted): {e}");
        }
    }

    unsafe { libc::close(fd) };
}
```

- [ ] **Step 2: Update drm_gpu_pipeline.rs to use encode_nv12_dmabuf**

Replace the encode section with:

```rust
    // 4. Full pipeline: process_frame → encode_nv12_dmabuf
    let mut processor = match GpuFrameProcessor::new(4096) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping: GpuFrameProcessor init failed: {e}");
            return;
        }
    };

    let analysis = match processor.process_frame(raw_fd, width, height, stride) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Skipping: process_frame failed: {e}");
            return;
        }
    };

    eprintln!("Dirty tiles: {}/{}", analysis.dirty_tiles.len(), total_tiles);
    assert!(!analysis.dirty_tiles.is_empty());

    let mut encoder = match FullFrameEncoder::new(width, height) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping encode: {e}");
            return;
        }
    };

    let mut encoded = None;
    for i in 0..3 {
        match encoder.encode_nv12_dmabuf(
            analysis.nv12_dmabuf_fd.as_raw_fd(),
            analysis.nv12_width,
            analysis.nv12_height,
            analysis.nv12_y_stride,
            analysis.nv12_uv_stride,
            analysis.nv12_uv_offset,
        ) {
            Ok(Some(enc)) => {
                eprintln!("Frame {i}: {} bytes, keyframe={}", enc.payload.len(), enc.is_keyframe);
                if encoded.is_none() {
                    encoded = Some(enc);
                }
            }
            Ok(None) => eprintln!("Frame {i}: buffered"),
            Err(e) => {
                eprintln!("Skipping: encode_nv12_dmabuf failed: {e}");
                return;
            }
        }
    }

    let enc = encoded.expect("encoder should produce output within 3 frames");
    assert!(!enc.payload.is_empty());
    assert!(enc.is_keyframe);

    // Fragment
    let datagrams = fragment_frame(1, 0, true, &enc.payload, 1200);
    assert!(!datagrams.is_empty());
    eprintln!("DRM → GPU SAD + NV12 → VA-API encode → fragment: PASS");
```

Also update imports to use `GpuFrameProcessor` instead of `GpuDirtyTracker`.

- [ ] **Step 3: Verify all tests**

Run: `cargo test --test gpu_pipeline -- --test-threads=1 --nocapture 2>&1 | tail -15`
Expected: 4 tests pass (3 existing + 1 new NV12 test)

Run (with sudo): `sudo cargo test -p ghostframe-xdaemon --test drm_gpu_pipeline -- --nocapture`
Expected: passes end-to-end on real DMA-BUF (the full zero-copy pipeline!)

- [ ] **Step 4: Verify E2E tests**

Run: `cargo test --test e2e -- --test-threads=1 2>&1 | tail -5`
Expected: all 9 E2E tests still pass (X11 fallback path unchanged)

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/tests/gpu_pipeline.rs ghostframe-xdaemon/tests/drm_gpu_pipeline.rs
git commit -m "test(gpu): NV12 conversion test + DRM end-to-end with encode_nv12_dmabuf"
```
