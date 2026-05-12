# GPU Tile-Analysis — Pre-M3.2 Design

**Date:** 2026-05-11
**Status:** Design approved
**Predecessors:**
- `docs/superpowers/specs/2026-04-28-m3-codec-suite-design.md` (M3 umbrella; M3.0 sentinel policy for GPU-derived metrics)
- `docs/superpowers/plans/2026-05-11-m3.1-solid-scheduler-ack.md` (M3.1 post-merge state)

---

## Context

The M3 umbrella parked `unique_colors` and `edge_density` computation in Phase M3.3, alongside CDF 5/3, on the rationale that compute-shader work should be lumped together. After M3.1 merged, that packaging is no longer the right cut.

Reasons to pull this work forward as a pre-M3.2 phase:

1. **`unique_colors` is the feasibility gate for PalRLE.** Without it, M3.2's classifier rule (`unique_colors ≤ 16 AND palette_allocate_ok → PalRle`) can only fire via `force_codec_state_for_test`, exactly the workaround M3.1 had to live with for Solid. Two phases in a row of sentinel-gated codec rules is a smell.
2. **Strategic direction is no-readback GPU compute.** Each new analysis metric that moves to GPU is a concrete step on that trajectory. Doing it for `unique_colors` now establishes the per-tile GPU-resident analysis buffer pattern that future codecs (BC1 endpoint selection, CDF 5/3 coefficient stats) will reuse.
3. **The infra cost is low.** `ash` 0.38 + Vulkan instance/device/queue/command-pool/descriptor-pool are already initialized. `tile_sad.comp` (`capture/shaders/tile_sad.comp:1`) is the dispatch-shape template — one workgroup per tile, 32×32 threads, shared-memory reduction, HOST_VISIBLE output buffer.
4. **No-readback encode is not in scope here.** PalRLE encode in M3.2 still operates on the CPU-readback BGRA path (`io_bridge.rs:719`). What changes: the encode reads its *palette* from the GPU analysis buffer directly, instead of recomputing it from the readback pixels. Moving the RLE pass and final encode itself to GPU is M4+ territory.

This phase ships:
- One new compute shader producing per-tile `(unique_color_count, edge_density, palette_colors[16])`.
- `GpuFrameProcessor` integration: new pipeline, new output buffer, dispatched alongside SAD/NV12 in the same command buffer.
- `FrameAnalysis` exposes a `tile_analysis: &[TileAnalysis]` slice.
- `TileMetrics` population from real GPU output on the GPU path; CPU path retains sentinels (degraded fallback, unchanged).

---

## Architecture

### Output contract

A new HOST_VISIBLE, HOST_COHERENT, persistently-mapped buffer sized `max_tiles × sizeof(TileAnalysis)`. The shader emits one entry per tile in row-major order, indexed `tile_y * cols + tile_x` — same convention as `sad_buffer`.

GLSL (std430, 80 bytes per tile, 16-byte aligned):

```glsl
struct TileAnalysis {
    uint  count;                 // 1..16 valid; 17 = overflow (PalRLE infeasible)
    uint  edge_density_thou;     // 0..1000 per-mille
    uint  _pad0;                 // 16-byte alignment for the colors[] array
    uint  _pad1;
    uint  colors[16];            // packed BGRA, valid up to count
};
```

Rust mirror (`#[repr(C)]`):

```rust
#[repr(C)]
pub struct TileAnalysis {
    pub count: u32,              // 1..=17
    pub edge_density_thou: u32,  // 0..=1000
    _pad: [u32; 2],
    pub colors: [u32; 16],       // packed BGRA: byte 0 = B, byte 1 = G, byte 2 = R, byte 3 = A
}
```

`FrameAnalysis` gains:

```rust
pub tile_analysis: *const TileAnalysis,  // valid until next process_frame
pub tile_analysis_len: u32,               // = cols × rows
```

**Color packing (decision O1).** Shader writes `value = B | (G << 8) | (R << 16) | (A << 24)`. On little-endian (every target we ship to), the in-memory byte order is `[B, G, R, A]`, which matches the existing wire format (`Codec::Solid` 4-byte payload, `encode_solid`) and the in-memory layout PalRLE's `PaletteEntry.colors: [[u8; 4]; 16]` will expect. Rust consumers can `bytemuck::cast_slice::<u32, [u8; 4]>` straight from `colors[]` into a `PaletteEntry`.

**Overflow semantics.** `count == 17` means "more than 16 distinct colors observed; PalRLE infeasible". The shader stops inserting into the hash set once count reaches 17, but the value 17 is the only sentinel — any consumer that sees `count > 16` should treat the `colors[]` array as undefined. The classifier's PalRLE rule reads `count` only; the PalRLE encoder reads both `count` and `colors[]` and is only entered when `count ≤ 16`.

**`TileMetrics` population.** On the GPU path (`process_frame_gpu`), after the dispatch fence:

```rust
let entry = &frame_analysis.tile_analysis_slice()[tile_idx];
metrics[tile_idx].unique_colors = entry.count.min(u16::MAX as u32) as u16;
metrics[tile_idx].edge_density  = entry.edge_density_thou as f32 / 1000.0;
```

The CPU path (`process_frame_cpu`) keeps `unique_colors = u16::MAX`, `edge_density = f32::NAN` — the classifier already handles these as "no match".

### Shader algorithm

`capture/shaders/tile_analysis.comp` — one workgroup per tile, 32×32 threads, two phases in sequence inside the workgroup.

**Phase 1 — unique-colors hash set.**

```glsl
shared uint slot_color[32];       // stored XOR-masked; 0 = empty
shared uint count;                // atomic, capped at 17
shared uint overflow_flag;
```

- All threads cooperatively init the 32-slot table to `0`, `count` to `0`, `overflow_flag` to `0`. The `0` initial value means "empty"; real BGRA values are XOR-masked on insert so they never collide with this sentinel.
- Each thread reads its pixel via `imageLoad(current_frame, ivec2(px, py))`, packs BGRA → `u32`.
- Out-of-bounds threads (px ≥ frame_width or py ≥ frame_height) skip the insert step but still execute the subsequent `barrier()` — GLSL requires all threads in a workgroup to reach every barrier.
- The XOR-mask trick: `MASK = 0xA5A5A5A5`. Insert stores `value ^ MASK` so a stored slot of `0` unambiguously means "empty" (no BGRA value xor-masks to `0`, since that would require the input to equal `MASK`, and `MASK` is XOR-masked to `0` only by itself — but `0xA5A5A5A5` is also a valid color, so we accept that this single value cannot be represented; in practice unreachable on real screen content, and the e2e/unit tests skip this color).
- Insert: hash = `(value * 2654435761u) & 31` (Knuth multiplicative); linear probe with `atomicCompSwap(slot_color[h], 0u, value ^ MASK)`. On success → `atomicAdd(count, 1u)`. On finding `value ^ MASK` already present in that slot → done (duplicate of own value). On collision with a different stored value → probe next slot (`h = (h + 1) & 31`).
- After every insert attempt, threads read `count`; if `>= 17`, set `overflow_flag` to `1` and stop probing.
- `barrier()` after the insert phase — all threads, including those that did not insert.
- Thread 0 reads `count` (clamped to 17 on output) and linearly scans `slot_color[0..32]`, XOR-unmasking each non-zero slot and writing up to 16 values into the output `colors[]` array in slot-traversal order. If `count > 16`, the output `colors[]` is undefined per contract; thread 0 still writes whatever it finds (cost: 32 reads + ≤16 writes; bounded).

Load factor at 16 valid entries in a 32-slot table is 50%; linear-probe collision chains are short. With 1024 threads, contention on `atomicCompSwap` is high for solid tiles (all threads insert the same value), but the same-value-already-present early-exit keeps the work bounded — the first successful CAS by any thread terminates probing for everyone with that value.

**Phase 2 — Sobel-style edge density.**

```glsl
shared uint edge_count;
shared uint pixels_in_bounds;
```

- After the unique-colors `barrier()`, all threads init `edge_count = 0` and thread 0 computes `pixels_in_bounds = min(32, frame_width - tile_x*32) * min(32, frame_height - tile_y*32)`.
- Each thread reads its pixel + its four cardinal neighbors via `imageLoad`. Out-of-bounds → `clamp` (replicate edge).
- Compute gradient as integer arithmetic on summed RGB channels (luma approximation, no float division). Define `rgb_sum(p) = int(p.r*255) + int(p.g*255) + int(p.b*255)` (range 0..765 per pixel):
  ```glsl
  int s_left  = rgb_sum(imageLoad(current_frame, ivec2(clamp(px-1, 0, int(frame_width)-1),  py)));
  int s_right = rgb_sum(imageLoad(current_frame, ivec2(clamp(px+1, 0, int(frame_width)-1),  py)));
  int s_up    = rgb_sum(imageLoad(current_frame, ivec2(px, clamp(py-1, 0, int(frame_height)-1))));
  int s_down  = rgb_sum(imageLoad(current_frame, ivec2(px, clamp(py+1, 0, int(frame_height)-1))));
  int gx = abs(s_right - s_left);   // 0..1530
  int gy = abs(s_down  - s_up);     // 0..1530
  int grad = gx + gy;                // 0..3060
  if (in_bounds && grad > EDGE_GRAD_THRESHOLD) atomicAdd(edge_count, 1u);
  ```
- `EDGE_GRAD_THRESHOLD = 48` (decision O2). The threshold is over `gx + gy` on summed RGB. Range 0..3060; 48 corresponds to ~3% per-channel difference between left/right or up/down neighbors. M3.5 retunes from real bench data.
- `barrier()`.
- Thread 0 emits `edge_density_thou = (edge_count * 1000) / max(pixels_in_bounds, 1)`.

**Frame-edge tile handling.** Tiles partially off-screen never produce out-of-bounds writes (per-thread bounds checks). Out-of-bounds pixels contribute 0 to both unique-colors and edge-count. The edge_density denominator is `pixels_in_bounds`, so a half-tile at the frame edge with a uniform color reports `edge_density_thou = 0` rather than being skewed by 0-padding.

### `GpuFrameProcessor` integration

Mirrors the existing SAD pipeline. New fields on `GpuFrameProcessor`:

```rust
// Tile-analysis pipeline
analysis_shader_module: vk::ShaderModule,
analysis_pipeline: vk::Pipeline,
analysis_pipeline_layout: vk::PipelineLayout,
analysis_descriptor_set_layout: vk::DescriptorSetLayout,

// HOST_VISIBLE | HOST_COHERENT, persistently mapped
analysis_buffer: vk::Buffer,
analysis_memory: vk::DeviceMemory,
analysis_ptr: *mut TileAnalysis,
```

Constructor (`new_inner` in `gpu_pipeline.rs`):
- Compile a third SPIR-V (`include_bytes!("shaders/tile_analysis.spv")`), create a third shader module.
- Descriptor set layout: binding 0 = `current_frame` (rgba8, read-only), binding 1 = `analysis_buffer` (storage buffer, std430). Push-constant block reuses `(frame_width, frame_height, cols)` from SAD.
- Compute pipeline created via `create_compute_pipelines` — can be batched into the same call as the existing SAD + NV12 pipelines.
- Allocate `analysis_buffer` sized `max_tiles × 80` bytes, HOST_VISIBLE | HOST_COHERENT, persistently mapped (same pattern as `sad_buffer`).

Dispatch (inside `process_frame_with_imported` or its successor — sequence currently is "SAD dispatch → NV12 dispatch"):
- Bind tile-analysis pipeline + descriptor set after SAD's bind.
- Push constants (reuse layout).
- `cmd_dispatch(cmd, cols, rows, 1)` — same shape as SAD.
- **No memory barrier between SAD / NV12 / tile_analysis dispatches.** All three read `current_frame` (read-only) and write to distinct output buffers. They can execute concurrently on the GPU subject to the implementation's queue scheduling.
- The existing fence at the end of the command buffer covers all three.

After fence wait, the analysis buffer is CPU-readable via `std::slice::from_raw_parts(self.analysis_ptr, max_tiles as usize)`. `FrameAnalysis` exposes a typed slice through a helper:

```rust
impl FrameAnalysis {
    pub fn tile_analysis_slice(&self) -> &[TileAnalysis] {
        unsafe { std::slice::from_raw_parts(self.tile_analysis, self.tile_analysis_len as usize) }
    }
}
```

Lifetime story matches `nv12_data` — valid until the next `process_frame` call.

**Resize.** `analysis_buffer` is sized to `max_tiles` at constructor time (matching `sad_buffer`'s policy). No per-frame allocation. If a runtime resolution change exceeds `max_tiles`, the buffer is reallocated together with SAD/NV12 — there's already precedent in `ensure_nv12_buffer` (`gpu_pipeline.rs:478`).

### Classifier and io_bridge integration

`io_bridge.rs::process_frame_gpu` already iterates dirty tiles to dispatch the scheduler. After the GPU dispatch completes and `FrameAnalysis` is available, populate per-tile metrics before invoking the classifier:

```rust
let analysis = frame_analysis.tile_analysis_slice();
for &(tile_x, tile_y) in &dirty_xy {
    let idx = (tile_y as usize) * (grid.cols as usize) + tile_x as usize;
    let entry = &analysis[idx];
    let m = &mut tile_metrics[idx];
    m.unique_colors = entry.count.min(u16::MAX as u32) as u16;
    m.edge_density  = entry.edge_density_thou as f32 / 1000.0;
}
// classifier.classify_tile(metrics, prev_state) now sees real values
```

The PalRLE feasibility check the classifier needs (`unique_colors ≤ 16 AND palette_allocate_or_find_succeeds(colors)`) is implemented in M3.2; this phase provides the inputs. For Solid (already in code via M3.1), the rule `unique_colors == 1` now fires on real content — `force_codec_state_for_test` becomes unnecessary in the corresponding e2e tests.

The CPU-path (`process_frame_cpu`) classifier call site leaves both fields at their sentinel values. Existing classifier code already treats `u16::MAX` and `NaN` as "no match"; behavior is unchanged on the CPU fallback.

---

## Testing

### Unit tests (`gpu_pipeline.rs`)

Extend the existing GPU test module (`gpu_pipeline.rs:1635+`). Each test allocates a test image via `make_memfd` (existing helper) and runs `process_frame` to populate `FrameAnalysis.tile_analysis_slice()`.

1. **Solid tile.** All pixels BGRA `(B=0x00, G=0x00, R=0xFF, A=0xFF)` (opaque red). Assert `entry.count == 1`, `entry.colors[0] == 0xFFFF0000` (packing: `0x00 | 0x00<<8 | 0xFF<<16 | 0xFF<<24`), `entry.edge_density_thou == 0`.
2. **Two-color checkerboard at 1-pixel granularity.** Alternating red and blue (two BGRA values). Assert `count == 2`, both colors present in `colors[0..2]` (order not specified), `edge_density_thou` close to `1000` (allow tolerance for frame-edge clamping reducing gradients on the outermost row/column).
3. **17-color overflow.** Sample 17 distinct BGRA values placed across the tile. Assert `count == 17` (overflow sentinel). `colors[]` undefined per contract.
4. **Frame-edge tile denominator.** Frame size `40 × 40` produces a `2 × 2` tile grid: tile (0,0) is full 32×32, tile (1,0) is 8×32, tile (0,1) is 32×8, tile (1,1) is 8×8. Fill all tiles with a single uniform color and inject one off-color pixel into tile (1,0) at a non-edge position. Expected `pixels_in_bounds` for that tile = `8 * 32 = 256`; a single gradient-triggering pixel contributes 1 to `edge_count`, so `edge_density_thou = (1 * 1000) / 256 = 3`. A naive denominator of 1024 would yield `≈ 1`. The factor-of-three difference is the falsifying signal.
5. **XOR-mask sentinel coverage.** Tile of all `0x00000000` (transparent black BGRA). Assert `count == 1`, `colors[0] == 0x00000000`. This confirms the XOR-mask trick correctly distinguishes the empty slot (raw `0`) from a stored `0x00000000` (which is stored as `0xA5A5A5A5`).
6. **Multi-tile independence.** 2×2 tile grid (`64 × 64` frame) where each tile has distinct content (1 color, 2 colors, 5 colors, 17-color overflow). Assert each tile's `count` and `colors[]` correspond to its content — confirms no cross-tile contamination via descriptor binding or output-buffer offset bugs.

### Integration tests (`tests/gpu_pipeline.rs`)

Existing tests in this file already run `GpuFrameProcessor::process_frame` on real DMA-BUF backed images. Add one new test:

7. **`tile_analysis_populated_from_real_dmabuf`.** Set up a known image, run `process_frame`, walk `tile_analysis_slice()`, assert at least one tile reports `count` and `edge_density_thou` consistent with the input.

### E2E behavioral checks

8. **`e2e_mode_switch` regression.** The cost-model now sees real `unique_colors` and `edge_density`. The test asserts on *behavioral* boundaries (e.g. "100% static content stays in tile-codec mode", "100% moving content transitions to H.264"), not on exact cost thresholds. Re-run; expected to pass.
9. **`e2e_solid_color_5pct_loss` simplification.** M3.1 ships this with classifier-state forcing only for Solid emission (see M3.1 plan §Task 15). With this pre-phase landed, Solid fires from the live classifier; the `force_codec_state_for_test` scaffolding can be removed and the test still passes.

If the e2e simplification is a follow-up rather than part of this phase, it lands in M3.2's per-codec PR.

### Shader correctness rationale

The hash-set algorithm is correct under the following invariants:
- The XOR-mask `0xA5A5A5A5` is not a fixed point of any reasonable BGRA value, so the empty-slot sentinel (`0` in the slot) never collides with a stored value.
- `atomicCompSwap` provides the only mutation primitive on `slot_color[]`; the "found same value already" check (compare-and-do-nothing) ensures duplicate inserts of the same color don't double-count.
- The overflow flag is read after a `barrier()`, so all threads observe a consistent decision.
- Multiplicative hash `value * 2654435761u & 31` is well-distributed for typical BGRA inputs; on pathological inputs the linear probe terminates after at most 32 steps and either finds a match, finds an empty slot, or hits the overflow guard.

---

## Out of scope (deferred)

- **No no-readback encode.** PalRLE and Solid encode in M3.2 still operate on CPU-readback BGRA from `readback_dmabuf` (`io_bridge.rs:721`). The encoder reads its *palette* from `FrameAnalysis.tile_analysis_slice()`, but the per-pixel index extraction and RLE pass remain CPU. Moving the index pass to GPU is a future increment; moving the final encode to GPU is M4+ trajectory.
- **No CDF 5/3 metrics.** The forward wavelet transform produces its own per-tile bit-plane data; that lives in `cdf53_forward.comp` and is M3.3.
- **No CPU-path metrics.** `process_frame_cpu` is the degraded fallback; sentinels remain.
- **No tunable shader constants.** `EDGE_GRAD_THRESHOLD` is `#define`d in GLSL. M3.5 retunes alongside other cost-model constants based on bench data.
- **No dirty-tiles-only dispatch.** Tile-analysis runs over all tiles regardless of dirtiness. Cost: ~1100 workgroups per frame at 1080p, each doing 1024 trivial threads — negligible on any GPU we target. Dirty-only dispatch would require either a CPU-readback synchronization point (defeats the purpose) or an indirect dispatch with a compacted tile list, which is materially more complex for no measurable gain.

---

## Decision register

| # | Decision | Rationale |
|---|---|---|
| O1 | Color packing as little-endian BGRA `u32`: `B \| G<<8 \| R<<16 \| A<<24` | Matches existing wire format (`Codec::Solid`, `encode_solid`) and the in-memory layout PalRLE's `PaletteEntry.colors: [[u8; 4]; 16]` expects. Direct `bytemuck::cast_slice::<u32, [u8; 4]>` is zero-copy. |
| O2 | `EDGE_GRAD_THRESHOLD = 48` on `\|Gx\|+\|Gy\|` over summed RGB (range 0..3060) | Starting point ~3% per-channel difference. M3.5 retunes from bench data. Compile-time constant for now. |
| O3 | Always-on dispatch (all tiles, not just dirty) | Cheap (~1100 workgroups/frame trivial work); avoids a CPU sync point or indirect dispatch infrastructure. |
| O4 | Single shader, two phases inside one workgroup | Sequential `barrier()` between unique-colors and edge-density phases. Avoids a second descriptor set + dispatch and reuses the cooperative thread group. |
| O5 | XOR-mask `0xA5A5A5A5` on in-slot color values | Lets `0` mean "empty slot" without collision against valid BGRA values (including `0x00000000` transparent black and `0xFFFFFFFF` opaque white). |
| O6 | 32-slot hash table, linear probe with `atomicCompSwap` | 50% load factor at the cap; short collision chains; standard pattern. |
| O7 | No barrier between SAD / NV12 / tile_analysis dispatches | All three read `current_frame` read-only and write to distinct output buffers; the fence at submit-time covers all three. |
| O8 | `analysis_buffer` sized to `max_tiles` at constructor time | Matches `sad_buffer` allocation policy; reuses the existing resize precedent in `ensure_nv12_buffer`. |
| O9 | GPU-path TileMetrics populated post-fence; CPU path retains sentinels | Mirrors how `process_frame_cpu` already handles being a degraded fallback. Classifier already treats sentinels as "no match". |
| O10 | Color order in output `colors[]` is slot-traversal order, not insertion order | Slot order is what thread 0 produces in a single linear pass over `slot_color[]`. Sufficient for PalRLE (palette identity is the set, not the sequence). |

---

## Work outline (for writing-plans)

1. Author `capture/shaders/tile_analysis.comp` and update the build path that compiles `.comp` → `.spv`.
2. Extend `GpuFrameProcessor` with the third pipeline + buffer (~150 lines mirroring the SAD/NV12 setup).
3. Extend `FrameAnalysis` with the new field + accessor.
4. Update `process_frame_gpu` in `io_bridge.rs` to populate `TileMetrics.unique_colors` and `.edge_density` from the new slice before classifier invocation.
5. Unit tests 1-6 in `gpu_pipeline.rs` (extending existing test module).
6. Integration test 7 in `tests/gpu_pipeline.rs`.
7. Verify `e2e_mode_switch` still passes (no code change expected).

Estimated 2-3 days of careful work. Shader is the highest-risk piece; everything else is plumbing in patterns that already exist.

---

## Future work pointers

- **No-readback PalRLE index pass.** Once the analysis buffer is established and PalRLE has shipped, a follow-up shader can take `(current_frame, palette)` and emit per-pixel indices to a third output buffer. The encoder then RLE-packs from the GPU-resident index buffer (still some CPU work for varint/nibble packing) or that too moves to GPU.
- **Analysis buffer as a multi-metric struct.** The current `TileAnalysis` is purpose-built for PalRLE. Future codecs may want additional fields — BC1 endpoint candidates (min/max BGRA per tile), CDF 5/3 coefficient stats. The struct can grow with reserved padding without breaking the existing readers.
- **Dirty-tiles-only dispatch via indirect dispatch.** If profiling later shows the all-tiles cost becomes meaningful (e.g. on 4K screens, ~8100 workgroups/frame), an indirect-dispatch path keyed on a GPU-side dirty-tile list (which the SAD output already implicitly contains via `sad_values[i] > threshold`) is the natural optimization.

---

## Document pointers

- M3 umbrella: `docs/superpowers/specs/2026-04-28-m3-codec-suite-design.md` — §M3.0 "M3.0 sentinel policy for GPU-derived metrics", §M3.2 prerequisite gate.
- M3.1 plan post-merge status: `docs/superpowers/plans/2026-05-11-m3.1-solid-scheduler-ack.md` — Tasks 18-19, remaining gaps.
- Existing GPU pipeline: `ghostframe-lib/src/capture/gpu_pipeline.rs`, shaders in `ghostframe-lib/src/capture/shaders/`.
- Source spec: `docs/specs/ghostframe-initial-spec.md` — §4.2 tile metrics, §4.3 hysteresis, classification rules table.
