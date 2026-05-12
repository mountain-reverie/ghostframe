# GPU Tile-Analysis Implementation Plan (pre-M3.2)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Vulkan compute shader that emits per-tile `(unique_color_count, edge_density, palette colors[16])` into a HOST_VISIBLE buffer, and populate `TileMetrics.unique_colors` / `.edge_density` from it on the GPU path. After this lands, the M3.2 PalRLE classifier rule can fire on live content without test scaffolding.

**Architecture:** Mirror the existing SAD + NV12 compute pipeline pattern in `ghostframe-lib/src/capture/gpu_pipeline.rs`. Add a third compute pipeline (`analysis_pipeline`), a third descriptor set layout, a third HOST_VISIBLE output buffer (`analysis_buffer`), and one extra dispatch in the same command buffer (no inter-dispatch barrier needed — all three read `current_frame` read-only and write to disjoint output buffers). The shader uses a 32-slot XOR-masked hash set in shared memory for unique-color extraction with an overflow guard at count > 16, followed by a Sobel-style gradient density pass over summed RGB.

**Tech Stack:** Rust + `ash` 0.38 (raw Vulkan), GLSL 450 compute shaders compiled with `glslangValidator -V` (Vulkan SDK), DMA-BUF import already in place. Tests use existing `make_memfd` helper which gracefully skips when no GPU.

**Source spec:** `docs/superpowers/specs/2026-05-11-gpu-tile-analysis-design.md`

---

## File map

**Create:**
- `ghostframe-lib/src/capture/shaders/tile_analysis.comp` — GLSL compute shader source
- `ghostframe-lib/src/capture/shaders/tile_analysis.spv` — compiled SPIR-V binary (committed as artifact, matches `tile_sad.spv` convention)

**Modify:**
- `ghostframe-lib/src/capture/gpu_pipeline.rs` — add `TileAnalysis` struct, `analysis_*` pipeline fields, allocation in `new_inner`, descriptor pool bump, dispatch in `process_frame_with_imported`, `FrameAnalysis` extension, unit tests
- `ghostframe-lib/src/transport/io_bridge.rs` — populate `TileMetrics` from `FrameAnalysis.tile_analysis_slice()` in `process_frame_gpu` between `record_frame` and the `classify_tile` loop
- `ghostframe-lib/tests/gpu_pipeline.rs` — integration test exercising the new buffer on a real DMA-BUF

**Untouched (verified by tests):**
- `ghostframe-lib/src/tile/classifier.rs` — already handles `u16::MAX` and `NaN` as "no match" sentinels; no changes needed
- `ghostframe-lib/src/tile/mod.rs` — `TileMetrics` struct unchanged

---

## Prerequisites

- `glslangValidator` available on PATH (Vulkan SDK; `pacman -S vulkan-tools` on Arch, `apt install glslang-tools` on Debian). Verify: `glslangValidator -v` prints a version string.
- Local GPU with Vulkan + DMA-BUF support to run the GPU-backed unit tests. Lacking that, the tests skip with an `eprintln!` (existing pattern at `gpu_pipeline.rs:1641`).

---

## Task 1: Author the shader source

**Files:**
- Create: `ghostframe-lib/src/capture/shaders/tile_analysis.comp`

- [ ] **Step 1: Write the GLSL source**

Create `ghostframe-lib/src/capture/shaders/tile_analysis.comp` with the following content:

```glsl
// tile_analysis.comp
// Per-tile analysis: unique-color count + palette extraction + edge density.
//
// Dispatch: one workgroup per tile (ceil(width/32) × ceil(height/32)).
// Each thread handles one pixel within the 32×32 tile.
//
// Outputs per tile (std430 binding 1):
//   uint  count;             // 1..16 valid, 17 = overflow
//   uint  edge_density_thou; // 0..1000 per-mille
//   uint  _pad0; _pad1;      // 16-byte alignment for colors[]
//   uint  colors[16];        // packed BGRA, only valid when count <= 16

#version 450

layout(local_size_x = 32, local_size_y = 32, local_size_z = 1) in;

layout(binding = 0, rgba8) readonly uniform image2D current_frame;

struct TileAnalysis {
    uint  count;
    uint  edge_density_thou;
    uint  _pad0;
    uint  _pad1;
    uint  colors[16];
};

layout(binding = 1, std430) buffer AnalysisOutput {
    TileAnalysis tiles[];
};

layout(push_constant) uniform PushConstants {
    uint frame_width;
    uint frame_height;
    uint cols;
};

// XOR mask: lets stored value 0 mean "empty slot" without colliding with
// real BGRA values. Note: BGRA value 0xA5A5A5A5 maps to stored 0 and is
// unrepresentable. In production X11 buffers, alpha is always 0xFF, so
// alpha=0xA5 never occurs.
const uint MASK = 0xA5A5A5A5u;

// Knuth multiplicative hash, truncated to 5 bits → 32 slots.
uint hash5(uint value) {
    return (value * 2654435761u) >> 27;
}

const uint EDGE_GRAD_THRESHOLD = 48u;  // see design O2

shared uint slot_color[32];
shared uint count;
shared uint overflow_flag;
shared uint edge_count;
shared uint pixels_in_bounds;

uint pack_bgra(vec4 p) {
    uint b = uint(p.b * 255.0 + 0.5);
    uint g = uint(p.g * 255.0 + 0.5);
    uint r = uint(p.r * 255.0 + 0.5);
    uint a = uint(p.a * 255.0 + 0.5);
    return b | (g << 8) | (r << 16) | (a << 24);
}

int rgb_sum(vec4 p) {
    return int(p.r * 255.0 + 0.5)
         + int(p.g * 255.0 + 0.5)
         + int(p.b * 255.0 + 0.5);
}

void main() {
    uint local_idx = gl_LocalInvocationID.y * 32u + gl_LocalInvocationID.x;
    uint tile_x = gl_WorkGroupID.x;
    uint tile_y = gl_WorkGroupID.y;
    int px = int(tile_x * 32u + gl_LocalInvocationID.x);
    int py = int(tile_y * 32u + gl_LocalInvocationID.y);
    bool in_bounds = (px < int(frame_width)) && (py < int(frame_height));

    // ---- Init shared state (cooperative). ----
    if (local_idx < 32u) {
        slot_color[local_idx] = 0u;
    }
    if (local_idx == 0u) {
        count = 0u;
        overflow_flag = 0u;
        edge_count = 0u;
        // Per-tile pixels-in-bounds for the edge_density denominator.
        uint w_in = min(32u, frame_width  - tile_x * 32u);
        uint h_in = min(32u, frame_height - tile_y * 32u);
        pixels_in_bounds = w_in * h_in;
    }
    barrier();

    // ---- Phase 1: unique-color hash set. ----
    if (in_bounds) {
        vec4 p = imageLoad(current_frame, ivec2(px, py));
        uint value = pack_bgra(p);
        uint masked = value ^ MASK;
        uint h = hash5(value);
        for (uint probe = 0u; probe < 32u; ++probe) {
            // Early bail-out: another thread saw overflow.
            if (overflow_flag != 0u) break;
            uint slot = (h + probe) & 31u;
            uint prev = atomicCompSwap(slot_color[slot], 0u, masked);
            if (prev == 0u) {
                // We claimed the slot.
                uint new_count = atomicAdd(count, 1u) + 1u;
                if (new_count >= 17u) {
                    overflow_flag = 1u;
                }
                break;
            }
            if (prev == masked) {
                // Duplicate of our own value already present — done.
                break;
            }
            // Collision with a different value — probe next slot.
        }
    }
    barrier();

    // ---- Phase 1 writeback (thread 0). ----
    if (local_idx == 0u) {
        uint tile_idx = tile_y * cols + tile_x;
        uint final_count = min(count, 17u);
        tiles[tile_idx].count = final_count;
        // Scan slots and emit up-to-16 colors in slot-traversal order.
        uint emitted = 0u;
        for (uint s = 0u; s < 32u && emitted < 16u; ++s) {
            uint v = slot_color[s];
            if (v != 0u) {
                tiles[tile_idx].colors[emitted] = v ^ MASK;
                emitted += 1u;
            }
        }
        // Zero any unused slots so consumers see deterministic data.
        for (uint s = emitted; s < 16u; ++s) {
            tiles[tile_idx].colors[s] = 0u;
        }
    }

    // ---- Phase 2: Sobel-style edge density on summed RGB. ----
    int s_self  = 0, s_left = 0, s_right = 0, s_up = 0, s_down = 0;
    if (in_bounds) {
        int xm = clamp(px - 1, 0, int(frame_width)  - 1);
        int xp = clamp(px + 1, 0, int(frame_width)  - 1);
        int ym = clamp(py - 1, 0, int(frame_height) - 1);
        int yp = clamp(py + 1, 0, int(frame_height) - 1);
        s_self  = rgb_sum(imageLoad(current_frame, ivec2(px, py)));
        s_left  = rgb_sum(imageLoad(current_frame, ivec2(xm, py)));
        s_right = rgb_sum(imageLoad(current_frame, ivec2(xp, py)));
        s_up    = rgb_sum(imageLoad(current_frame, ivec2(px, ym)));
        s_down  = rgb_sum(imageLoad(current_frame, ivec2(px, yp)));
        int gx = abs(s_right - s_left);
        int gy = abs(s_down  - s_up);
        int grad = gx + gy;
        if (grad > int(EDGE_GRAD_THRESHOLD)) {
            atomicAdd(edge_count, 1u);
        }
    }
    barrier();

    if (local_idx == 0u) {
        uint tile_idx = tile_y * cols + tile_x;
        uint denom = max(pixels_in_bounds, 1u);
        tiles[tile_idx].edge_density_thou = (edge_count * 1000u) / denom;
    }
}
```

- [ ] **Step 2: Compile to SPIR-V**

Run:
```bash
cd ghostframe-lib/src/capture/shaders
glslangValidator -V tile_analysis.comp -o tile_analysis.spv
```

Expected output: `tile_analysis.comp` (no errors), creates `tile_analysis.spv` (binary, ~3-5 KB).

If `glslangValidator` is missing, `glslc tile_analysis.comp -o tile_analysis.spv` from `shaderc` is an alternative.

- [ ] **Step 3: Verify SPIR-V is well-formed**

Run:
```bash
glslangValidator -V tile_analysis.comp -o /dev/null
```

Expected: silent success (exit code 0). Any errors here are GLSL bugs to fix before continuing.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/capture/shaders/tile_analysis.comp ghostframe-lib/src/capture/shaders/tile_analysis.spv
git commit -m "feat(capture): GLSL compute shader for per-tile unique-colors + edge density

Shader source + compiled SPIR-V binary. Not yet wired into GpuFrameProcessor;
that lands in the following commits.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Add `TileAnalysis` struct and extend `FrameAnalysis`

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline.rs` (struct definitions near `FrameAnalysis`)

- [ ] **Step 1: Write the failing test**

Append to the `#[cfg(test)] mod tests` at the bottom of `ghostframe-lib/src/capture/gpu_pipeline.rs` (the existing module starts around line 1633):

```rust
    #[test]
    fn tile_analysis_struct_has_expected_layout() {
        assert_eq!(std::mem::size_of::<TileAnalysis>(), 80, "TileAnalysis must be 80 bytes");
        assert_eq!(std::mem::align_of::<TileAnalysis>(), 4, "TileAnalysis alignment");

        // Offsets match std430 layout from the shader.
        let zero = TileAnalysis { count: 0, edge_density_thou: 0, _pad: [0; 2], colors: [0; 16] };
        let base = &zero as *const _ as usize;
        assert_eq!(&zero.count             as *const _ as usize - base, 0);
        assert_eq!(&zero.edge_density_thou as *const _ as usize - base, 4);
        assert_eq!(&zero.colors            as *const _ as usize - base, 16);
    }

    #[test]
    fn frame_analysis_tile_analysis_slice_returns_correct_range() {
        // Build a fake FrameAnalysis backed by a heap Vec so we can exercise the
        // slice helper without spinning up Vulkan.
        let mut backing = vec![
            TileAnalysis { count: 1, edge_density_thou: 100, _pad: [0; 2], colors: [0xAAAAAAAAu32; 16] },
            TileAnalysis { count: 2, edge_density_thou: 200, _pad: [0; 2], colors: [0xBBBBBBBBu32; 16] },
        ];
        let analysis = FrameAnalysis {
            dirty_tiles: vec![],
            nv12_data: std::ptr::null(),
            nv12_width: 0, nv12_height: 0,
            nv12_y_stride: 0, nv12_uv_stride: 0, nv12_uv_offset: 0,
            tile_analysis: backing.as_mut_ptr() as *const TileAnalysis,
            tile_analysis_len: 2,
        };
        let slice = analysis.tile_analysis_slice();
        assert_eq!(slice.len(), 2);
        assert_eq!(slice[0].count, 1);
        assert_eq!(slice[1].edge_density_thou, 200);
        assert_eq!(slice[1].colors[0], 0xBBBBBBBBu32);
    }
```

- [ ] **Step 2: Run the test, see it fail**

Run: `cargo test -p ghostframe-lib --lib capture::gpu_pipeline::tests::tile_analysis_struct_has_expected_layout capture::gpu_pipeline::tests::frame_analysis_tile_analysis_slice_returns_correct_range -- --nocapture`

Expected: compilation error — `TileAnalysis` not in scope, `FrameAnalysis` has no `tile_analysis` field, no `tile_analysis_slice` method.

- [ ] **Step 3: Add the struct and extend `FrameAnalysis`**

In `ghostframe-lib/src/capture/gpu_pipeline.rs`, just before `pub struct FrameAnalysis {` (around line 104), add:

```rust
/// Per-tile analysis output from the `tile_analysis.comp` shader.
///
/// Layout matches the std430 struct in `shaders/tile_analysis.comp`:
/// 80 bytes per tile, 4-byte aligned. `_pad` exists to push `colors`
/// to offset 16 in std430.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TileAnalysis {
    /// Distinct color count. 1..=16 means `colors` holds that many valid
    /// entries. `17` is the overflow sentinel — PalRLE-infeasible; the
    /// `colors` array is undefined.
    pub count: u32,
    /// Per-mille (0..=1000) fraction of pixels above the gradient threshold.
    pub edge_density_thou: u32,
    pub _pad: [u32; 2],
    /// Packed BGRA values: byte 0 = B, byte 1 = G, byte 2 = R, byte 3 = A.
    /// Order is slot-traversal order (not insertion order). Valid up to `count`.
    pub colors: [u32; 16],
}
```

Then modify the existing `FrameAnalysis` struct (currently around line 104). Find:

```rust
pub struct FrameAnalysis {
    /// Flat tile indices of tiles that changed since the previous frame.
    pub dirty_tiles: Vec<u32>,
    /// Pointer to the NV12 output buffer (Y plane at offset 0, UV at `nv12_uv_offset`).
    /// Valid until the next call to `process_frame`.
    pub nv12_data: *const u8,
    pub nv12_width: u32,
    pub nv12_height: u32,
    pub nv12_y_stride: u32,
    pub nv12_uv_stride: u32,
    pub nv12_uv_offset: u32,
}
```

Add two new fields and a slice accessor:

```rust
pub struct FrameAnalysis {
    /// Flat tile indices of tiles that changed since the previous frame.
    pub dirty_tiles: Vec<u32>,
    /// Pointer to the NV12 output buffer (Y plane at offset 0, UV at `nv12_uv_offset`).
    /// Valid until the next call to `process_frame`.
    pub nv12_data: *const u8,
    pub nv12_width: u32,
    pub nv12_height: u32,
    pub nv12_y_stride: u32,
    pub nv12_uv_stride: u32,
    pub nv12_uv_offset: u32,
    /// Pointer to the per-tile analysis buffer (one `TileAnalysis` per tile,
    /// row-major, `tile_y * cols + tile_x`). Valid until the next call to
    /// `process_frame`.
    pub tile_analysis: *const TileAnalysis,
    /// Number of entries reachable via `tile_analysis` (= cols × rows).
    pub tile_analysis_len: u32,
}

impl FrameAnalysis {
    pub fn tile_analysis_slice(&self) -> &[TileAnalysis] {
        if self.tile_analysis.is_null() || self.tile_analysis_len == 0 {
            return &[];
        }
        // SAFETY: pointer is into HOST_VISIBLE mapped GPU memory owned by
        // GpuFrameProcessor; valid until the next process_frame call. Lifetime
        // tied to &self.
        unsafe { std::slice::from_raw_parts(self.tile_analysis, self.tile_analysis_len as usize) }
    }
}
```

- [ ] **Step 4: Run the tests, see them pass**

Run: `cargo test -p ghostframe-lib --lib capture::gpu_pipeline::tests::tile_analysis_struct_has_expected_layout capture::gpu_pipeline::tests::frame_analysis_tile_analysis_slice_returns_correct_range -- --nocapture`

Expected: both tests PASS.

- [ ] **Step 5: Verify nothing else broke**

Run: `cargo build -p ghostframe-lib`

Expected: clean build. The new field defaults to null/0 at every `FrameAnalysis` construction site, which we'll fix in Task 4 — for now any callers of `FrameAnalysis` will fail to compile if they construct the struct literally. Search for them:

Run: `grep -rn "FrameAnalysis {" ghostframe-lib/`

Expected: every match is in `gpu_pipeline.rs` itself (the eventual production construction site) or the test we just added. If any caller outside `gpu_pipeline.rs` constructs `FrameAnalysis` literally, it now fails to compile. Patch each by adding `tile_analysis: std::ptr::null(), tile_analysis_len: 0,`.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline.rs
git commit -m "feat(capture): TileAnalysis struct + FrameAnalysis.tile_analysis_slice()

Defines the per-tile output of tile_analysis.comp. FrameAnalysis carries
the buffer pointer and length; slice helper exposes a typed view bounded
by &self. Shader still not wired into GpuFrameProcessor.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Construct the analysis pipeline and buffer in `GpuFrameProcessor::new_inner`

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline.rs`

This task adds plumbing only; no dispatch yet, so `tile_analysis` stays null in the returned `FrameAnalysis`. The test is the existing `gpu_dirty_tracker_creates_successfully` — it must still pass after we expand the constructor.

- [ ] **Step 1: Confirm baseline**

Run: `cargo test -p ghostframe-lib --lib capture::gpu_pipeline::tests::gpu_dirty_tracker_creates_successfully -- --nocapture`

Expected: PASS (or graceful skip on machines without Vulkan).

- [ ] **Step 2: Add new fields to `GpuFrameProcessor`**

In `gpu_pipeline.rs`, find the `GpuFrameProcessor` struct (around line 130) and add the analysis fields after the existing NV12 ones:

```rust
    // NV12 pipeline (existing fields above; do not touch)
    nv12_shader_module: vk::ShaderModule,
    nv12_pipeline: vk::Pipeline,
    nv12_pipeline_layout: vk::PipelineLayout,
    nv12_descriptor_set_layout: vk::DescriptorSetLayout,

    // Tile-analysis pipeline (new)
    analysis_shader_module: vk::ShaderModule,
    analysis_pipeline: vk::Pipeline,
    analysis_pipeline_layout: vk::PipelineLayout,
    analysis_descriptor_set_layout: vk::DescriptorSetLayout,
    // HOST_VISIBLE | HOST_COHERENT, persistently mapped. Layout matches
    // `TileAnalysis` (80 bytes per tile).
    analysis_buffer: vk::Buffer,
    analysis_memory: vk::DeviceMemory,
    analysis_ptr: *mut TileAnalysis,
```

- [ ] **Step 3: Build the analysis pipeline in `new_inner`**

In `new_inner` (around line 290 where the SAD shader module is loaded), after the existing NV12 shader-module line (around line 300), add the analysis shader module:

```rust
        // --- Analysis Shader module ---
        let analysis_spv = include_bytes!("shaders/tile_analysis.spv");
        let analysis_spv_words =
            ash::util::read_spv(&mut std::io::Cursor::new(analysis_spv.as_slice()))?;
        let analysis_shader_ci =
            vk::ShaderModuleCreateInfo::default().code(&analysis_spv_words);
        let analysis_shader_module =
            device.create_shader_module(&analysis_shader_ci, None)?;
```

After the NV12 compute pipeline creation (around line 394, after `let nv12_pipeline = nv12_pipelines[0];`), add the analysis descriptor set layout + pipeline:

```rust
        // --- Analysis Descriptor set layout ---
        // binding 0: STORAGE_IMAGE (current frame, read-only in shader)
        // binding 1: STORAGE_BUFFER (analysis output)
        let analysis_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let analysis_dsl_ci =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&analysis_bindings);
        let analysis_descriptor_set_layout =
            device.create_descriptor_set_layout(&analysis_dsl_ci, None)?;

        // --- Analysis Pipeline layout ---
        // Push constants: 3 x u32 = 12 bytes (frame_width, frame_height, cols) — same
        // shape as SAD's, but a distinct pipeline-layout object is required because
        // descriptor sets differ.
        let analysis_push_range = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(12)];
        let analysis_pipeline_layout_ci = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(std::slice::from_ref(&analysis_descriptor_set_layout))
            .push_constant_ranges(&analysis_push_range);
        let analysis_pipeline_layout =
            device.create_pipeline_layout(&analysis_pipeline_layout_ci, None)?;

        // --- Analysis Compute pipeline ---
        let analysis_stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(analysis_shader_module)
            .name(entry_name);
        let analysis_compute_ci = vk::ComputePipelineCreateInfo::default()
            .stage(analysis_stage)
            .layout(analysis_pipeline_layout);
        let analysis_pipelines = device
            .create_compute_pipelines(vk::PipelineCache::null(), &[analysis_compute_ci], None)
            .map_err(|(_, e)| e)?;
        let analysis_pipeline = analysis_pipelines[0];
```

- [ ] **Step 4: Bump descriptor pool capacity**

The descriptor pool currently sizes for 2 descriptor sets (SAD, NV12). The analysis set adds 1 STORAGE_IMAGE + 1 STORAGE_BUFFER. Find the descriptor pool creation (around line 398) and update it:

```rust
        // --- Descriptor pool ---
        // 3 sets: SAD (2 STORAGE_IMAGE + 1 STORAGE_BUFFER)
        //       + NV12 (1 STORAGE_IMAGE + 1 STORAGE_BUFFER)
        //       + Analysis (1 STORAGE_IMAGE + 1 STORAGE_BUFFER)
        // Total: 4 STORAGE_IMAGE, 3 STORAGE_BUFFER, 3 max_sets.
        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_IMAGE,
                descriptor_count: 4,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 3,
            },
        ];
        let dp_ci = vk::DescriptorPoolCreateInfo::default()
            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
            .max_sets(3)
            .pool_sizes(&pool_sizes);
        let descriptor_pool = device.create_descriptor_pool(&dp_ci, None)?;
```

- [ ] **Step 5: Allocate the analysis output buffer**

After the SAD buffer allocation (around line 434, after `let sad_ptr = device.map_memory(...)`), add the analysis buffer:

```rust
        // --- Analysis output buffer ---
        let analysis_entry_bytes = std::mem::size_of::<TileAnalysis>() as vk::DeviceSize;
        let analysis_buf_size = (max_tiles as vk::DeviceSize) * analysis_entry_bytes;
        let analysis_buf_ci = vk::BufferCreateInfo::default()
            .size(analysis_buf_size)
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let analysis_buffer = device.create_buffer(&analysis_buf_ci, None)?;
        let analysis_buf_reqs = device.get_buffer_memory_requirements(analysis_buffer);
        let analysis_mem_type = find_memory_type(
            &mem_props,
            analysis_buf_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or("no host-visible memory type for analysis buffer")?;

        let analysis_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(analysis_buf_reqs.size)
            .memory_type_index(analysis_mem_type);
        let analysis_memory = device.allocate_memory(&analysis_alloc, None)?;
        device.bind_buffer_memory(analysis_buffer, analysis_memory, 0)?;

        let analysis_ptr = device.map_memory(
            analysis_memory,
            0,
            analysis_buf_size,
            vk::MemoryMapFlags::empty(),
        )? as *mut TileAnalysis;
```

Note: `mem_props` is already in scope from the SAD allocation above; do not re-fetch.

- [ ] **Step 6: Add the new fields to the `Ok(Self {...})` constructor**

Find the constructor return statement (around line 443: `Ok(Self { ... })`) and add the new fields (alphabetize or group with their kind — match the field order declared on the struct):

```rust
        Ok(Self {
            // ... existing fields ...
            nv12_shader_module,
            nv12_pipeline,
            nv12_pipeline_layout,
            nv12_descriptor_set_layout,

            analysis_shader_module,
            analysis_pipeline,
            analysis_pipeline_layout,
            analysis_descriptor_set_layout,
            analysis_buffer,
            analysis_memory,
            analysis_ptr,

            // ... rest of existing fields ...
        })
```

- [ ] **Step 7: Extend the `Drop` impl**

Find `impl Drop for GpuFrameProcessor` (around line 1529). Insert two new blocks:

**(a)** After the SAD-resources block (after `self.device.free_memory(self.sad_memory, None);`, around line 1548) and **before** `destroy_descriptor_pool`, add the analysis buffer cleanup:

```rust
            // Analysis buffer
            self.device.unmap_memory(self.analysis_memory);
            self.device.destroy_buffer(self.analysis_buffer, None);
            self.device.free_memory(self.analysis_memory, None);
```

**(b)** After the SAD pipeline block (after `self.device.destroy_shader_module(self.shader_module, None);`, around line 1569) and **before** `destroy_command_pool`, add the analysis pipeline cleanup:

```rust
            // Analysis pipeline
            self.device
                .destroy_descriptor_set_layout(self.analysis_descriptor_set_layout, None);
            self.device.destroy_pipeline(self.analysis_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.analysis_pipeline_layout, None);
            self.device
                .destroy_shader_module(self.analysis_shader_module, None);
```

The placement mirrors the existing NV12 block's relative position — buffer cleanup runs before descriptor pool destruction, pipeline objects are destroyed after the descriptor pool and before the command pool.

- [ ] **Step 8: Run the existing creation test**

Run: `cargo test -p ghostframe-lib --lib capture::gpu_pipeline::tests::gpu_dirty_tracker_creates_successfully -- --nocapture`

Expected: PASS (or graceful skip if no GPU). If Vulkan validation layers complain about descriptor pool sizes or shader module compilation, address before continuing.

- [ ] **Step 9: Run the full lib test sweep to confirm no regressions**

Run: `cargo test -p ghostframe-lib --lib`

Expected: all existing tests pass. The full sweep includes the metrics_tracker tests, classifier tests, etc. — none should be affected by this change.

- [ ] **Step 10: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline.rs
git commit -m "feat(capture): construct tile_analysis pipeline + HOST_VISIBLE buffer

Adds the third compute pipeline mirroring SAD/NV12, allocates the analysis
output buffer (max_tiles × 80 bytes, persistently mapped), bumps descriptor
pool to 3 sets. Pipeline is constructed and torn down but not yet dispatched
— FrameAnalysis.tile_analysis stays null. Existing creation test passes.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Wire the dispatch into `process_frame_with_imported` and surface `tile_analysis` on `FrameAnalysis`

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline.rs`

After this task, a solid-red 32×32 input produces `FrameAnalysis.tile_analysis_slice()[0] = { count: 1, colors[0]: 0xFFFF0000, edge_density_thou: 0 }`.

- [ ] **Step 1: Write the failing behavior test**

Append to the test module (after the existing `process_frame_returns_nv12_data` test, around line 1830):

```rust
    #[test]
    fn process_frame_returns_tile_analysis_for_solid_red() {
        // Solid red 32×32 → exactly one tile, count=1, edge_density_thou=0.
        let width = 32u32;
        let height = 32u32;
        let stride = width * 4;
        // BGRA: B=0, G=0, R=255, A=255 → packed u32 = 0xFFFF0000
        let pixel: [u8; 4] = [0, 0, 255, 255];

        let mut processor = match GpuFrameProcessor::new(256) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Skipping process_frame_returns_tile_analysis (no Vulkan GPU?): {e}");
                return;
            }
        };

        unsafe {
            let fd = make_memfd(width, height, pixel);
            let analysis = match processor.process_frame(fd, width, height, stride) {
                Ok(a) => a,
                Err(e) => {
                    libc::close(fd);
                    eprintln!("Skipping (memfd not a real DMA-BUF): {e}");
                    return;
                }
            };
            libc::close(fd);

            assert!(!analysis.tile_analysis.is_null(), "tile_analysis pointer must not be null");
            assert_eq!(analysis.tile_analysis_len, 1, "32x32 frame → 1 tile");
            let slice = analysis.tile_analysis_slice();
            assert_eq!(slice.len(), 1);
            assert_eq!(slice[0].count, 1, "solid tile → count=1");
            assert_eq!(slice[0].colors[0], 0xFFFF0000u32, "BGRA(0,0,255,255) → 0xFFFF0000");
            assert_eq!(slice[0].edge_density_thou, 0, "solid tile → no edges");
        }
    }
```

- [ ] **Step 2: Run the test, see it fail**

Run: `cargo test -p ghostframe-lib --lib capture::gpu_pipeline::tests::process_frame_returns_tile_analysis_for_solid_red -- --nocapture`

Expected: FAIL with "tile_analysis pointer must not be null" — the pipeline is constructed but the dispatch isn't wired and the FrameAnalysis still carries null.

- [ ] **Step 3: Allocate the analysis descriptor set, bind, and dispatch**

Open `process_frame_with_imported` (around line 623). Find the SAD descriptor-set allocation block (around line 685-696). After the NV12 descriptor-set allocation block (around line 705-707), add a third allocation for analysis:

```rust
        let analysis_set_layouts = [self.analysis_descriptor_set_layout];
        let analysis_ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&analysis_set_layouts);
        let analysis_ds_guard = ScopedDescriptorSets {
            device: &self.device,
            pool: self.descriptor_pool,
            sets: self.device.allocate_descriptor_sets(&analysis_ds_alloc)?,
        };
        let analysis_ds = analysis_ds_guard.sets[0];
```

After the NV12 descriptor-set write block (around line 759, after `self.device.update_descriptor_sets(&nv12_writes, &[]);`), add the analysis write:

```rust
        // Write Analysis descriptor set.
        let analysis_image_info = [vk::DescriptorImageInfo::default()
            .image_view(current.view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let analysis_buffer_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.analysis_buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let analysis_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(analysis_ds)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&analysis_image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(analysis_ds)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&analysis_buffer_info),
        ];
        self.device.update_descriptor_sets(&analysis_writes, &[]);
```

After the NV12 dispatch (around line 883, after `self.device.cmd_dispatch(cmd, nv12_groups_x, nv12_groups_y, 1);`), add the analysis dispatch:

```rust
        // 4b. Analysis dispatch. No barrier needed vs SAD/NV12 — all read
        // current_frame read-only and write to disjoint buffers; the final
        // HOST-readback barrier covers all three output buffers.
        self.device.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.analysis_pipeline,
        );
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.analysis_pipeline_layout,
            0,
            &[analysis_ds],
            &[],
        );
        let analysis_push: [u32; 3] = [width, height, cols];
        let analysis_push_bytes = std::slice::from_raw_parts(
            analysis_push.as_ptr() as *const u8,
            std::mem::size_of_val(&analysis_push),
        );
        self.device.cmd_push_constants(
            cmd,
            self.analysis_pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            analysis_push_bytes,
        );
        self.device.cmd_dispatch(cmd, cols, rows, 1);
```

- [ ] **Step 4: Extend the HOST-readback barrier**

Find the barrier block around `gpu_pipeline.rs:970-997`. The existing `buf_barriers` array has two entries (SAD + NV12). Replace it with a three-entry array including the analysis buffer:

```rust
        // 5. Barriers: SAD buffer → HOST_READ; NV12 buffer → HOST_READ; analysis buffer → HOST_READ
        let buf_barriers = [
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.sad_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(nv12_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
            vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.analysis_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE),
        ];
```

The surrounding `cmd_pipeline_barrier` call (COMPUTE_SHADER → HOST) does not need to change — adding a third buffer barrier in the same array is sufficient.

- [ ] **Step 5: Surface `tile_analysis` on the `FrameAnalysis` construction sites**

There are **two** `FrameAnalysis` construction sites in `process_frame_with_imported`:

**Site A — first-frame path (around line 663).** This path returns before the SAD/NV12 dispatch, only running `run_nv12_and_snapshot`. The analysis pipeline is NOT dispatched on the first frame. Set the new fields to null/zero — io_bridge's `populate_gpu_metrics` (Task 11) handles a null pointer gracefully, leaving `TileMetrics` at its `UNIQUE_COLORS_UNKNOWN` / `EDGE_DENSITY_UNKNOWN` sentinel for the first frame. The classifier already treats sentinels as "no match", so first-frame behavior is unchanged.

Change:
```rust
            return Ok(FrameAnalysis {
                dirty_tiles: all_dirty,
                nv12_data: nv12_ptr,
                nv12_width: width,
                nv12_height: height,
                nv12_y_stride,
                nv12_uv_stride,
                nv12_uv_offset,
            });
```

to:
```rust
            return Ok(FrameAnalysis {
                dirty_tiles: all_dirty,
                nv12_data: nv12_ptr,
                nv12_width: width,
                nv12_height: height,
                nv12_y_stride,
                nv12_uv_stride,
                nv12_uv_offset,
                tile_analysis: std::ptr::null(),
                tile_analysis_len: 0,
            });
```

**Site B — subsequent-frame path (around line 1048).** This path runs the full SAD + NV12 + analysis dispatch chain. Surface the real pointer + length. Note `cols` and `rows` are already in scope from earlier in the function (the SAD dispatch uses them).

Change:
```rust
        Ok(FrameAnalysis {
            dirty_tiles: dirty,
            nv12_data: nv12_ptr,
            nv12_width: width,
            nv12_height: height,
            nv12_y_stride,
            nv12_uv_stride,
            nv12_uv_offset,
        })
```

to:
```rust
        Ok(FrameAnalysis {
            dirty_tiles: dirty,
            nv12_data: nv12_ptr,
            nv12_width: width,
            nv12_height: height,
            nv12_y_stride,
            nv12_uv_stride,
            nv12_uv_offset,
            tile_analysis: self.analysis_ptr as *const TileAnalysis,
            tile_analysis_len: cols * rows,
        })
```

- [ ] **Step 6: Run the behavior test, see it pass**

Run: `cargo test -p ghostframe-lib --lib capture::gpu_pipeline::tests::process_frame_returns_tile_analysis_for_solid_red -- --nocapture`

Expected: PASS (or graceful skip without GPU).

- [ ] **Step 7: Run the existing GPU tests to confirm no regression**

Run: `cargo test -p ghostframe-lib --lib capture::gpu_pipeline::tests -- --nocapture`

Expected: every existing GPU test (`gpu_dirty_tracker_creates_successfully`, `identical_frames_produce_no_dirty_tiles`, `changed_pixel_detected`, `process_frame_returns_nv12_data`) still passes.

- [ ] **Step 8: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline.rs
git commit -m "feat(capture): dispatch tile_analysis shader; surface results on FrameAnalysis

Allocates the third descriptor set, writes binding 0/1, dispatches the
analysis pipeline with the same (cols, rows, 1) shape as SAD. Extends the
final HOST-readback barrier to include the analysis buffer. Solid-red
unit test confirms count=1 and the expected BGRA packing 0xFFFF0000.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Unit test — two-color checkerboard

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline.rs` (tests module)

- [ ] **Step 1: Write the test**

Append to the test module:

```rust
    #[test]
    fn process_frame_tile_analysis_checkerboard() {
        // 32×32 frame with 1-pixel checkerboard: pixel (x,y) is red if (x+y)&1, else blue.
        let width = 32u32;
        let height = 32u32;
        let stride = width * 4;

        let mut processor = match GpuFrameProcessor::new(256) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Skipping checkerboard test (no Vulkan GPU?): {e}");
                return;
            }
        };

        unsafe {
            let size = (stride * height) as usize;
            let name = std::ffi::CString::new("ghost-test-checkerboard").unwrap();
            let fd = libc::memfd_create(name.as_ptr(), 0);
            assert!(fd >= 0);
            libc::ftruncate(fd, size as i64);
            let ptr = libc::mmap(
                std::ptr::null_mut(), size,
                libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0,
            );
            assert_ne!(ptr, libc::MAP_FAILED);
            let frame = std::slice::from_raw_parts_mut(ptr as *mut u8, size);
            for y in 0..height {
                for x in 0..width {
                    let offset = ((y * stride) + x * 4) as usize;
                    let bgra = if (x + y) & 1 == 0 {
                        [255, 0, 0, 255]   // blue
                    } else {
                        [0, 0, 255, 255]   // red
                    };
                    frame[offset..offset + 4].copy_from_slice(&bgra);
                }
            }
            libc::munmap(ptr, size);

            let analysis = match processor.process_frame(fd, width, height, stride) {
                Ok(a) => a,
                Err(e) => {
                    libc::close(fd);
                    eprintln!("Skipping checkerboard (memfd not a real DMA-BUF): {e}");
                    return;
                }
            };
            libc::close(fd);

            let entry = &analysis.tile_analysis_slice()[0];
            assert_eq!(entry.count, 2, "checkerboard → 2 unique colors");

            // Both colors must appear; order is slot-traversal, not specified.
            let blue: u32 = 0xFF0000FF;   // B=255, G=0, R=0, A=255 → 0xFF | 0 | 0 | 0xFF000000
            let red:  u32 = 0xFFFF0000;   // B=0, G=0, R=255, A=255
            assert!(
                (entry.colors[0] == blue || entry.colors[1] == blue) &&
                (entry.colors[0] == red  || entry.colors[1] == red),
                "expected both red and blue, got [{:#x}, {:#x}]",
                entry.colors[0], entry.colors[1]
            );

            // Most pixels border a different color on at least one cardinal axis;
            // edge pixels (clamped) reduce density slightly. Lower bound is
            // generous to absorb edge effects.
            assert!(
                entry.edge_density_thou > 700,
                "checkerboard → edge_density_thou should be high, got {}",
                entry.edge_density_thou
            );
        }
    }
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p ghostframe-lib --lib capture::gpu_pipeline::tests::process_frame_tile_analysis_checkerboard -- --nocapture`

Expected: PASS. If `edge_density_thou` comes out lower than 700, inspect the shader's gradient threshold — the test's assertion is tuned to the spec's `EDGE_GRAD_THRESHOLD = 48` over summed RGB.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline.rs
git commit -m "test(capture): tile_analysis checkerboard — 2 colors + high edge density

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Unit test — 17-color overflow

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline.rs` (tests module)

- [ ] **Step 1: Write the test**

Append:

```rust
    #[test]
    fn process_frame_tile_analysis_overflow_at_17_colors() {
        // Place 17 distinct colors spread across the tile. Expect count=17 sentinel.
        let width = 32u32;
        let height = 32u32;
        let stride = width * 4;

        let mut processor = match GpuFrameProcessor::new(256) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Skipping overflow test (no Vulkan GPU?): {e}");
                return;
            }
        };

        unsafe {
            let size = (stride * height) as usize;
            let name = std::ffi::CString::new("ghost-test-overflow").unwrap();
            let fd = libc::memfd_create(name.as_ptr(), 0);
            assert!(fd >= 0);
            libc::ftruncate(fd, size as i64);
            let ptr = libc::mmap(
                std::ptr::null_mut(), size,
                libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0,
            );
            assert_ne!(ptr, libc::MAP_FAILED);
            let frame = std::slice::from_raw_parts_mut(ptr as *mut u8, size);
            // 17 distinct BGRA colors. Background = color 0.
            let colors: [[u8; 4]; 17] = [
                [10, 20, 30, 255],   [40, 50, 60, 255],   [70, 80, 90, 255],
                [100, 110, 120, 255], [130, 140, 150, 255], [160, 170, 180, 255],
                [190, 200, 210, 255], [220, 230, 240, 255], [5, 15, 25, 255],
                [35, 45, 55, 255],   [65, 75, 85, 255],   [95, 105, 115, 255],
                [125, 135, 145, 255], [155, 165, 175, 255], [185, 195, 205, 255],
                [215, 225, 235, 255], [245, 250, 254, 255],
            ];
            for chunk in frame.chunks_exact_mut(4) {
                chunk.copy_from_slice(&colors[0]);
            }
            // Place each of the 17 colors at distinct pixel positions.
            for (i, c) in colors.iter().enumerate() {
                let x = (i as u32 * 7) % width;   // stride 7 keeps placements spread out
                let y = (i as u32 * 11) % height;
                let off = ((y * stride) + x * 4) as usize;
                frame[off..off + 4].copy_from_slice(c);
            }
            libc::munmap(ptr, size);

            let analysis = match processor.process_frame(fd, width, height, stride) {
                Ok(a) => a,
                Err(e) => {
                    libc::close(fd);
                    eprintln!("Skipping overflow (memfd not a real DMA-BUF): {e}");
                    return;
                }
            };
            libc::close(fd);

            let entry = &analysis.tile_analysis_slice()[0];
            assert_eq!(entry.count, 17, "17 distinct colors → overflow sentinel");
            // colors[] is undefined per contract — do not assert on it.
        }
    }
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p ghostframe-lib --lib capture::gpu_pipeline::tests::process_frame_tile_analysis_overflow_at_17_colors -- --nocapture`

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline.rs
git commit -m "test(capture): tile_analysis overflow — 17 colors yields count=17 sentinel

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Unit test — frame-edge denominator

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline.rs` (tests module)

- [ ] **Step 1: Write the test**

```rust
    #[test]
    fn process_frame_tile_analysis_frame_edge_denominator() {
        // 40×40 frame → 2x2 tile grid:
        //   tile (0,0): 32×32 full
        //   tile (1,0): 8×32 right-edge partial (256 pixels in-bounds)
        //   tile (0,1): 32×8 bottom-edge partial (256 pixels in-bounds)
        //   tile (1,1): 8×8 corner partial (64 pixels in-bounds)
        //
        // Fill everything red. Inject ONE off-color pixel into tile (1,0) at a
        // non-edge interior position. Gradient triggers there.
        //
        // Expected edge_density_thou for tile (1,0):
        //   The off-color pixel and ~2 immediate neighbors register gradient.
        //   With denom=256 and ~3 gradient-pixels, density ≈ 11. With a naive
        //   denom=1024 it'd be ≈ 2. Assertion is >5 (real) vs <5 (would catch
        //   bad denominator).
        let width = 40u32;
        let height = 40u32;
        let stride = width * 4;
        let cols = 2u32;

        let mut processor = match GpuFrameProcessor::new(256) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Skipping frame-edge test (no Vulkan GPU?): {e}");
                return;
            }
        };

        unsafe {
            let size = (stride * height) as usize;
            let name = std::ffi::CString::new("ghost-test-edge").unwrap();
            let fd = libc::memfd_create(name.as_ptr(), 0);
            assert!(fd >= 0);
            libc::ftruncate(fd, size as i64);
            let ptr = libc::mmap(
                std::ptr::null_mut(), size,
                libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0,
            );
            assert_ne!(ptr, libc::MAP_FAILED);
            let frame = std::slice::from_raw_parts_mut(ptr as *mut u8, size);
            for chunk in frame.chunks_exact_mut(4) {
                chunk.copy_from_slice(&[0, 0, 255, 255]);   // red BGRA
            }
            // Inject one bright-green pixel at (x=35, y=10) — interior of tile (1,0):
            // tile (1,0) covers x=32..40, y=0..32; (35,10) is in-bounds and not on
            // the frame edge.
            let off = ((10u32 * stride) + 35 * 4) as usize;
            frame[off..off + 4].copy_from_slice(&[0, 255, 0, 255]);   // green BGRA
            libc::munmap(ptr, size);

            let analysis = match processor.process_frame(fd, width, height, stride) {
                Ok(a) => a,
                Err(e) => {
                    libc::close(fd);
                    eprintln!("Skipping frame-edge (memfd not a real DMA-BUF): {e}");
                    return;
                }
            };
            libc::close(fd);

            let slice = analysis.tile_analysis_slice();
            assert_eq!(analysis.tile_analysis_len, 4, "40x40 → 2x2 tile grid");

            // Tile (1,0) — index = 0 * cols + 1 = 1
            let edge_tile = &slice[1];
            assert_eq!(edge_tile.count, 2, "tile (1,0) has red + green");
            // With proper denominator=256: density ≈ (~3 * 1000) / 256 ≈ 11
            // With naive denominator=1024: density ≈ (~3 * 1000) / 1024 ≈ 2
            assert!(
                edge_tile.edge_density_thou > 5,
                "frame-edge tile denominator wrong: density = {} (expected > 5 with denom=256)",
                edge_tile.edge_density_thou
            );

            let _ = cols; // suppress unused warning in case future asserts drop the index calc
        }
    }
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p ghostframe-lib --lib capture::gpu_pipeline::tests::process_frame_tile_analysis_frame_edge_denominator -- --nocapture`

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline.rs
git commit -m "test(capture): tile_analysis frame-edge denominator uses pixels_in_bounds

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Unit test — XOR-mask covers `0x00000000`

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline.rs` (tests module)

- [ ] **Step 1: Write the test**

```rust
    #[test]
    fn process_frame_tile_analysis_xor_mask_handles_zero_bgra() {
        // BGRA = 0x00000000 (transparent black) — must round-trip through the
        // hash set despite raw 0 being the "empty slot" sentinel. The XOR mask
        // means 0x00000000 is stored as 0xA5A5A5A5 in the slot.
        let width = 32u32;
        let height = 32u32;
        let stride = width * 4;
        let pixel: [u8; 4] = [0, 0, 0, 0];

        let mut processor = match GpuFrameProcessor::new(256) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Skipping XOR-mask test (no Vulkan GPU?): {e}");
                return;
            }
        };

        unsafe {
            let fd = make_memfd(width, height, pixel);
            let analysis = match processor.process_frame(fd, width, height, stride) {
                Ok(a) => a,
                Err(e) => {
                    libc::close(fd);
                    eprintln!("Skipping XOR-mask (memfd not a real DMA-BUF): {e}");
                    return;
                }
            };
            libc::close(fd);

            let entry = &analysis.tile_analysis_slice()[0];
            assert_eq!(entry.count, 1, "transparent-black tile → count=1");
            assert_eq!(entry.colors[0], 0x00000000u32, "BGRA(0,0,0,0) survives mask");
            assert_eq!(entry.edge_density_thou, 0);
        }
    }
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p ghostframe-lib --lib capture::gpu_pipeline::tests::process_frame_tile_analysis_xor_mask_handles_zero_bgra -- --nocapture`

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline.rs
git commit -m "test(capture): tile_analysis XOR-mask handles BGRA 0x00000000 correctly

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Unit test — multi-tile independence

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline.rs` (tests module)

- [ ] **Step 1: Write the test**

```rust
    #[test]
    fn process_frame_tile_analysis_multi_tile_independence() {
        // 64×64 frame → 2x2 tile grid:
        //   tile (0,0): solid red          → count=1
        //   tile (1,0): solid blue         → count=1
        //   tile (0,1): red-blue checker   → count=2
        //   tile (1,1): three colors       → count=3
        // Verifies the per-tile output slot is correctly addressed and there
        // is no cross-talk between tiles.
        let width = 64u32;
        let height = 64u32;
        let stride = width * 4;

        let mut processor = match GpuFrameProcessor::new(256) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Skipping multi-tile test (no Vulkan GPU?): {e}");
                return;
            }
        };

        unsafe {
            let size = (stride * height) as usize;
            let name = std::ffi::CString::new("ghost-test-multitile").unwrap();
            let fd = libc::memfd_create(name.as_ptr(), 0);
            assert!(fd >= 0);
            libc::ftruncate(fd, size as i64);
            let ptr = libc::mmap(
                std::ptr::null_mut(), size,
                libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0,
            );
            assert_ne!(ptr, libc::MAP_FAILED);
            let frame = std::slice::from_raw_parts_mut(ptr as *mut u8, size);

            let red:    [u8; 4] = [0, 0, 255, 255];
            let blue:   [u8; 4] = [255, 0, 0, 255];
            let green:  [u8; 4] = [0, 255, 0, 255];
            let yellow: [u8; 4] = [0, 255, 255, 255];

            // Fill helper.
            let mut put = |x: u32, y: u32, c: [u8; 4]| {
                let off = ((y * stride) + x * 4) as usize;
                frame[off..off + 4].copy_from_slice(&c);
            };

            for y in 0..32 {
                // tile (0,0): red
                for x in 0..32 { put(x, y, red); }
                // tile (1,0): blue
                for x in 32..64 { put(x, y, blue); }
            }
            for y in 32..64 {
                // tile (0,1): red/blue checker
                for x in 0..32 {
                    put(x, y, if (x + y) & 1 == 0 { red } else { blue });
                }
                // tile (1,1): three colors striped vertically (red/green/yellow)
                for x in 32..64 {
                    let c = match (x - 32) / 11 {
                        0 => red,
                        1 => green,
                        _ => yellow,
                    };
                    put(x, y, c);
                }
            }
            libc::munmap(ptr, size);

            let analysis = match processor.process_frame(fd, width, height, stride) {
                Ok(a) => a,
                Err(e) => {
                    libc::close(fd);
                    eprintln!("Skipping multi-tile (memfd not a real DMA-BUF): {e}");
                    return;
                }
            };
            libc::close(fd);

            let slice = analysis.tile_analysis_slice();
            assert_eq!(analysis.tile_analysis_len, 4);
            // cols = 2; index = y * 2 + x
            assert_eq!(slice[0].count, 1, "tile (0,0) red");
            assert_eq!(slice[1].count, 1, "tile (1,0) blue");
            assert_eq!(slice[2].count, 2, "tile (0,1) checker");
            assert_eq!(slice[3].count, 3, "tile (1,1) three stripes");
        }
    }
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p ghostframe-lib --lib capture::gpu_pipeline::tests::process_frame_tile_analysis_multi_tile_independence -- --nocapture`

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline.rs
git commit -m "test(capture): tile_analysis multi-tile independence — no cross-tile contamination

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Integration test in `tests/gpu_pipeline.rs`

**Files:**
- Modify: `ghostframe-lib/tests/gpu_pipeline.rs`

The existing integration test file uses `memfd_create`-backed test FDs via the helper `create_solid_memfd(width, height, b, g, r)` (visible at the top of the file). Each test calls `processor.process_frame(fd, …)` and skips gracefully when the memfd isn't importable as a DMA-BUF on the current driver. We follow the same pattern — no new harness module needed.

The integration scoping (separate test binary, exercised by `cargo test --test gpu_pipeline`) is what makes this an integration test rather than a unit test; the helper pattern is identical to the unit tests in Tasks 4-9.

- [ ] **Step 1: Add the integration test**

Append to `ghostframe-lib/tests/gpu_pipeline.rs`:

```rust
#[test]
fn tile_analysis_populated_on_subsequent_frame() {
    let width = 32u32;
    let height = 32u32;
    let stride = width * 4;

    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping tile_analysis integration (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        // First frame: opaque red. analysis.tile_analysis is null on first frame
        // (the analysis pipeline isn't dispatched until the second frame, which
        // is the steady-state path).
        let fd1 = create_solid_memfd(width, height, /*B=*/0, /*G=*/0, /*R=*/255);
        let first = match processor.process_frame(fd1, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Skipping (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd1);
        assert!(first.tile_analysis.is_null(), "first-frame analysis is null by design");

        // Second frame: same opaque red. Now the analysis pipeline dispatches.
        let fd2 = create_solid_memfd(width, height, 0, 0, 255);
        let second = match processor.process_frame(fd2, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd2);
                eprintln!("Skipping second frame: {e}");
                return;
            }
        };
        libc::close(fd2);

        assert!(!second.tile_analysis.is_null(), "second-frame analysis non-null");
        assert_eq!(second.tile_analysis_len, 1, "32x32 frame → 1 tile");
        let slice = second.tile_analysis_slice();
        assert_eq!(slice.len(), 1);
        assert_eq!(slice[0].count, 1, "solid tile → count=1");
        assert_eq!(slice[0].colors[0], 0xFFFF0000u32);
        assert_eq!(slice[0].edge_density_thou, 0);
    }
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p ghostframe-lib --test gpu_pipeline tile_analysis_populated_on_subsequent_frame`

Expected: PASS on a host with Vulkan + DMA-BUF support, graceful skip elsewhere.

- [ ] **Step 3: Run the full gpu_pipeline integration test sweep**

Run: `cargo test -p ghostframe-lib --test gpu_pipeline`

Expected: every existing integration test still passes.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/tests/gpu_pipeline.rs
git commit -m "test(capture): integration test for tile_analysis on subsequent-frame path

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: Populate `TileMetrics.unique_colors` and `.edge_density` from `FrameAnalysis` in `io_bridge`

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Write the failing test**

Append to the `#[cfg(test)] mod tests` block in `ghostframe-lib/src/transport/io_bridge.rs` (the module starts around line 1230). Add a test that exercises the small free function we're about to factor out:

```rust
    #[test]
    fn populate_gpu_metrics_writes_unique_colors_and_edge_density() {
        use crate::capture::gpu_pipeline::TileAnalysis;
        use crate::tile::MetricsTracker;

        let mut tracker = MetricsTracker::new(2, 1);
        let analysis = vec![
            TileAnalysis { count: 1, edge_density_thou: 0, _pad: [0; 2], colors: [0; 16] },
            TileAnalysis { count: 17, edge_density_thou: 850, _pad: [0; 2], colors: [0; 16] },
        ];
        let dirty: Vec<(u32, u32)> = vec![(0, 0), (1, 0)];

        super::populate_gpu_metrics(&mut tracker, &dirty, 2, &analysis);

        assert_eq!(tracker.get(0, 0).unique_colors, 1);
        assert!(tracker.get(0, 0).edge_density.abs() < 1e-6);

        assert_eq!(tracker.get(1, 0).unique_colors, 17);
        assert!((tracker.get(1, 0).edge_density - 0.850).abs() < 1e-6);
    }

    #[test]
    fn populate_gpu_metrics_skips_non_dirty_tiles() {
        use crate::capture::gpu_pipeline::TileAnalysis;
        use crate::tile::MetricsTracker;

        let mut tracker = MetricsTracker::new(2, 1);
        // Pre-seed tile (1,0) to a known sentinel so we can prove it stayed.
        tracker.get_mut(1, 0).unique_colors = u16::MAX;
        tracker.get_mut(1, 0).edge_density  = f32::NAN;

        let analysis = vec![
            TileAnalysis { count: 5, edge_density_thou: 100, _pad: [0; 2], colors: [0; 16] },
            TileAnalysis { count: 9, edge_density_thou: 200, _pad: [0; 2], colors: [0; 16] },
        ];
        // Only (0,0) is dirty.
        let dirty: Vec<(u32, u32)> = vec![(0, 0)];
        super::populate_gpu_metrics(&mut tracker, &dirty, 2, &analysis);

        assert_eq!(tracker.get(0, 0).unique_colors, 5);
        assert_eq!(tracker.get(1, 0).unique_colors, u16::MAX, "non-dirty tile untouched");
        assert!(tracker.get(1, 0).edge_density.is_nan(), "non-dirty tile untouched");
    }
```

- [ ] **Step 2: Run the tests, see them fail**

Run: `cargo test -p ghostframe-lib --lib transport::io_bridge::tests::populate_gpu_metrics_writes_unique_colors_and_edge_density transport::io_bridge::tests::populate_gpu_metrics_skips_non_dirty_tiles -- --nocapture`

Expected: compilation error — `populate_gpu_metrics` doesn't exist.

- [ ] **Step 3: Add the free function**

Near the top of `io_bridge.rs`, after the existing `use` declarations (around line 60-80), add the function as a private (`pub(crate)`) helper:

```rust
/// Write GPU-derived per-tile metrics into the tracker for the dirty tiles.
///
/// Non-dirty tile entries in `tracker` are untouched. Caller is responsible
/// for ensuring `tile_analysis.len() >= cols * tracker.rows()` and that
/// every `(tx, ty)` in `dirty` is within the tracker grid.
pub(crate) fn populate_gpu_metrics(
    tracker: &mut crate::tile::MetricsTracker,
    dirty: &[(u32, u32)],
    cols: u32,
    tile_analysis: &[crate::capture::gpu_pipeline::TileAnalysis],
) {
    for &(tx, ty) in dirty {
        let idx = (ty as usize) * (cols as usize) + (tx as usize);
        let Some(entry) = tile_analysis.get(idx) else { continue; };
        let m = tracker.get_mut(tx, ty);
        m.unique_colors = entry.count.min(u16::MAX as u32) as u16;
        m.edge_density  = entry.edge_density_thou as f32 / 1000.0;
    }
}
```

- [ ] **Step 4: Run the unit tests, see them pass**

Run: `cargo test -p ghostframe-lib --lib transport::io_bridge::tests::populate_gpu_metrics_writes_unique_colors_and_edge_density transport::io_bridge::tests::populate_gpu_metrics_skips_non_dirty_tiles -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Wire the call into `process_frame_gpu`**

In `io_bridge.rs::process_frame_gpu`, find the existing block (around line 573-580):

```rust
        // Keep metrics_tracker AND scheduler grids in sync with dirty-detection.
        if self.metrics_tracker.cols() != cols || self.metrics_tracker.rows() != rows {
            self.metrics_tracker.resize(cols, rows);
            self.scheduler.resize(cols, rows);
        }
        // Always update per-tile metrics — idle_frames advances on every frame,
        // EMA decays toward 0 when tiles aren't dirty.
        self.metrics_tracker.record_frame(&dirty_xy);
```

Right after `self.metrics_tracker.record_frame(&dirty_xy);` and before the `use crate::tile::{classifier::classify_tile, ...};` line, add:

```rust
        // Populate GPU-derived metrics (unique_colors, edge_density) for the dirty
        // tiles from the tile_analysis buffer. The classifier rule for PalRLE
        // (and Solid, post-M3.1) reads these.
        populate_gpu_metrics(
            &mut self.metrics_tracker,
            &dirty_xy,
            cols,
            analysis.tile_analysis_slice(),
        );
```

- [ ] **Step 6: Run the full lib test sweep**

Run: `cargo test -p ghostframe-lib --lib`

Expected: all tests pass.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "feat(io_bridge): populate TileMetrics.unique_colors/.edge_density from GPU on GPU path

Adds populate_gpu_metrics helper and wires it into process_frame_gpu between
record_frame and the classify_tile loop. After this, the M3.1 sentinel for
unique_colors lifts on the GPU path — Solid fires from the live classifier
without force_codec_state_for_test, and M3.2's PalRLE rule will fire on
live content once it ships.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: e2e regression sweep

**Files:**
- Test only — no code changes expected.

- [ ] **Step 1: Run `e2e_mode_switch`**

Run: `cargo test -p ghostframe-lib --test e2e e2e_mode_switch -- --test-threads=1`

Expected: PASS. The test asserts on behavioral boundaries (static content stays in tile-codec mode; moving content transitions to H.264); real `unique_colors` and `edge_density` may shift exact costs but not boundary behavior.

If FAIL: inspect which boundary moved. Re-tuning is M3.5's job, but if the test asserts on something that's a pure cost-threshold value rather than a behavioral boundary, the test itself needs the assertion loosened. Loosen the test, document the loosening in the commit message, and re-run.

- [ ] **Step 2: Run `e2e_solid_color_5pct_loss`**

Run: `cargo test -p ghostframe-lib --test e2e e2e_solid_color_5pct_loss -- --test-threads=1`

Expected: PASS. The test currently uses `force_codec_state_for_test` to force Solid emission. With this plan landed, Solid should fire from the live classifier on a solid-color test pattern. The test still passes either way.

- [ ] **Step 3: Run `e2e_ack_loss`**

Run: `cargo test -p ghostframe-lib --test e2e e2e_ack_loss -- --test-threads=1`

Expected: PASS.

- [ ] **Step 4: Full e2e sweep**

Run: `cargo test -p ghostframe-lib --test e2e -- --test-threads=1`

Expected: all e2e tests pass.

- [ ] **Step 5: If anything failed, fix or document, then re-run**

No commit yet if everything passes. If a test required loosening, commit the loosening with a message explaining what changed and why (real GPU metrics now feed the classifier, prior assertion was over-tight).

---

## Task 13: Final verification and FFI header check

**Files:**
- Possibly: `ghostframe-lib/include/ghostframe.h` (only if cbindgen produces a diff)

- [ ] **Step 1: Clippy clean**

Run: `cargo clippy -p ghostframe-lib -- -D warnings`

Expected: no warnings. Fix any inline.

- [ ] **Step 2: Format check**

Run: `cargo fmt -- --check`

Expected: clean. If diff, run `cargo fmt`, commit as a separate "style: cargo fmt" commit.

- [ ] **Step 3: Proptest sweep**

Run: `cargo test -p ghostframe-lib --lib tile::tests_proptest`

Expected: all proptests still pass (C1, C2, C3, C4, C5 enabled per M3.1; C6 deferred).

- [ ] **Step 4: Full lib + e2e sweep**

Run: `cargo test -p ghostframe-lib`

Expected: green everywhere.

- [ ] **Step 5: FFI header regen check**

Run the existing cbindgen flow as `cargo build` invokes it via `build.rs` (which regenerates `ghostframe-lib/include/ghostframe.h`). After `cargo build -p ghostframe-lib`, run:

```bash
git status -- ghostframe-lib/include/
```

Expected: clean working tree — the new APIs (`TileAnalysis`, `tile_analysis_slice`) stay internal to Rust and are not `pub extern "C"`, so the header should not change. If a diff appears, inspect it: ensure no new C exports were unintentionally created. If intentional, commit the regenerated header.

- [ ] **Step 6: Final commit (only if header regen produced anything)**

```bash
git status
# If clean — done. Otherwise:
git add ghostframe-lib/include/ghostframe.h
git commit -m "chore(ffi): regenerate header (no surface changes from tile-analysis)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Out-of-scope reminder

Explicitly NOT in this plan (per design doc §Out of scope):

- **No no-readback encode.** PalRLE/Solid encode in M3.2 still operate on CPU-readback BGRA from `readback_dmabuf`. This plan only retires the GPU-derived metric sentinels.
- **No CDF 5/3 metrics.** M3.3.
- **No CPU-path metrics.** `process_frame_cpu` keeps sentinels; classifier already handles that.
- **No tunable shader constants.** `EDGE_GRAD_THRESHOLD = 48` is `#define`'d. M3.5 retunes from bench data.
- **No dirty-tiles-only dispatch.** All tiles every frame; trivial cost.

---

## Spec coverage check

| Design doc section | Task |
|---|---|
| §Output contract — `TileAnalysis` struct, `FrameAnalysis.tile_analysis_slice` | Task 2 |
| §Shader algorithm — `tile_analysis.comp` source | Task 1 |
| §Shader algorithm — XOR-mask, hash-set, overflow | Task 1 |
| §Shader algorithm — Sobel edge density | Task 1 |
| §`GpuFrameProcessor` integration — third pipeline, descriptor pool bump, buffer | Task 3 |
| §`GpuFrameProcessor` integration — descriptor set alloc + dispatch + barrier | Task 4 |
| §Classifier and io_bridge integration | Task 11 |
| §Testing — unit test 1 (solid tile) | Task 4 |
| §Testing — unit test 2 (checkerboard) | Task 5 |
| §Testing — unit test 3 (17-color overflow) | Task 6 |
| §Testing — unit test 4 (frame-edge denominator) | Task 7 |
| §Testing — unit test 5 (XOR-mask coverage) | Task 8 |
| §Testing — unit test 6 (multi-tile independence) | Task 9 |
| §Testing — integration test (real DMA-BUF) | Task 10 |
| §Testing — `e2e_mode_switch` regression | Task 12 |
| §Testing — `e2e_solid_color_5pct_loss` simplification (optional) | Task 12 |
| §Decision register O1–O10 | Tasks 1, 3, 4 |
| Final verification (clippy, fmt, ffi header) | Task 13 |

No spec gaps.
