# gpu_pipeline.rs decomposition — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split `ghostframe-lib/src/capture/gpu_pipeline.rs` (4090 lines) into a directory module along a lifecycle seam, then extract the 9 per-stage GPU compute dispatches in `frame.rs` into per-stage helper methods on `GpuFrameProcessor`.

**Architecture:** Phase 1 is a pure file relocation (one commit, zero behavior change). Phase 2 extracts one helper per compute stage (nine commits, each independently revertible). Submodules of `gpu_pipeline` see `GpuFrameProcessor`'s private fields by default — no visibility widening needed. The 9 per-stage helpers each allocate one descriptor set (returning the RAII guard), write descriptors, bind pipeline + push constants, and dispatch; barriers stay in the orchestrator methods.

**Tech Stack:** Rust, ash (Vulkan bindings), the existing `pipeline_builder` helpers (`alloc_host_buffer`, `alloc_host_buffer_mapped`, `build_compute_pipeline`, `find_memory_type`, `BindingSpec`).

**Branch:** master. (Consistent with the user's established workflow throughout the cleanup batch — direct commits on master, no worktree.)

**Spec reference:** `docs/superpowers/specs/2026-05-20-gpu-pipeline-decomposition-design.md`.

---

## File Structure (post-Phase-2)

```
ghostframe-lib/src/capture/
  gpu_pipeline/
    mod.rs       ~700 lines  — constants, data structs (TileAnalysis,
                                FramePaletteEntryRaw, FrameAnalysis with slice
                                helpers, Nv12OutputLayout, FrameGeometry,
                                NV12Buffer, PrevFrame), GpuFrameProcessor
                                struct, the three public entry methods
                                (new/process_frame/diff), impl Drop, submodule
                                decls
    setup.rs     ~1000 lines — impl GpuFrameProcessor { unsafe fn new_inner }
                                + module doc
    frame.rs     ~1300-1500  — RAII guards (ScopedDescriptorSets,
                                ScopedCommandBuffers, ScopedFence), impl
                                GpuFrameProcessor with process_frame_inner,
                                process_frame_with_imported, run_first_frame_passes,
                                ensure_nv12_buffer, import_dmabuf,
                                destroy_prev_frame, allocate_owned_image, AND
                                the 9 new per-stage helpers
    tests.rs     ~1575 lines — relocated from ../gpu_pipeline_tests.rs,
                                content unchanged
  pipeline_builder.rs       unchanged
  dmabuf.rs                 unchanged
```

External callers (anyone importing from `crate::capture::gpu_pipeline`) see no change in API surface. `capture/mod.rs` continues to declare `pub mod gpu_pipeline;` — the directory module shares its name with the old file.

---

## Phase 1 — pure file split (single commit)

### Task 1: Split gpu_pipeline.rs into directory module

**Files:**
- Delete: `ghostframe-lib/src/capture/gpu_pipeline.rs`
- Delete: `ghostframe-lib/src/capture/gpu_pipeline_tests.rs`
- Create: `ghostframe-lib/src/capture/gpu_pipeline/mod.rs`
- Create: `ghostframe-lib/src/capture/gpu_pipeline/setup.rs`
- Create: `ghostframe-lib/src/capture/gpu_pipeline/frame.rs`
- Create: `ghostframe-lib/src/capture/gpu_pipeline/tests.rs`

**Suggested implementer model:** `sonnet` (this is a multi-step mechanical move that benefits from careful tracking; cheap models tend to drop sections).

- [ ] **Step 1: Capture baseline.**

```bash
cd /home/cedric/work/ghostframe
wc -l ghostframe-lib/src/capture/gpu_pipeline.rs ghostframe-lib/src/capture/gpu_pipeline_tests.rs
cargo test --workspace --lib -- --test-threads=1 2>&1 | grep "^test result"
```

Record the line counts (expected `4090` and `1575`, summing to `5665`) and the test count (`282 passed`). These are the verification targets for Step 7+.

- [ ] **Step 2: Identify section boundaries in `gpu_pipeline.rs`.**

Run:

```bash
grep -nE "^(unsafe |pub |    pub |    unsafe |    fn |impl |// ---|use |const |struct |#\[)" ghostframe-lib/src/capture/gpu_pipeline.rs | head -80
```

Use the output to confirm the line ranges for:

- Module doc comment and `use` statements (top of file through the imports).
- Constants block (`TILE_SIZE`, `PALETTE_HASH_SLOTS`, `PER_TILE_INDEX_BYTES`, `PALETTE_SLOT_U32_BYTES`).
- RAII guard structs and their `Drop` impls: `ScopedDescriptorSets`, `ScopedCommandBuffers`, `ScopedFence`.
- Data structs and their inherent impls: `TileAnalysis`, `FramePaletteEntryRaw`, `FrameAnalysis` (with all six `*_slice` helpers), `Nv12OutputLayout`, `FrameGeometry`, `NV12Buffer`, `PrevFrame`.
- `pub struct GpuFrameProcessor` (the ~70 fields).
- `impl GpuFrameProcessor`:
  - `pub fn new` + `unsafe fn new_inner` (setup.rs candidates)
  - `unsafe fn ensure_nv12_buffer` (frame.rs)
  - `pub fn process_frame` + `unsafe fn process_frame_inner` (split: `pub fn process_frame` to mod.rs, `unsafe fn process_frame_inner` to frame.rs)
  - `unsafe fn process_frame_with_imported` (frame.rs)
  - `unsafe fn run_first_frame_passes` (frame.rs)
  - `pub fn diff` (mod.rs)
  - `unsafe fn import_dmabuf`, `unsafe fn destroy_prev_frame`, `unsafe fn allocate_owned_image` (frame.rs)
- `impl Drop for GpuFrameProcessor` (mod.rs).
- The cfg(test) test mod declaration at the bottom (replaced — tests now live in `gpu_pipeline/tests.rs`).

Write the exact line ranges to a scratchpad before cutting. Mistakes here are the main risk in this task.

- [ ] **Step 3: Create the directory and build mod.rs.**

```bash
mkdir -p ghostframe-lib/src/capture/gpu_pipeline
```

Construct `ghostframe-lib/src/capture/gpu_pipeline/mod.rs` containing, in order:

1. The original module doc comment (`//! GPU-accelerated dirty tile detection ...`).
2. The original `use` statements (`use ash::vk;`, `use std::ffi::CStr;`, `use std::io;`, `use super::pipeline_builder::{...};`).
3. The four constants (`TILE_SIZE`, `PALETTE_HASH_SLOTS`, `PER_TILE_INDEX_BYTES`, `PALETTE_SLOT_U32_BYTES`) with their doc comments.
4. All data structs + their inherent impls (`TileAnalysis`, `FramePaletteEntryRaw`, `FrameAnalysis` and its `impl` block with the six slice helpers, `Nv12OutputLayout` with `from_buffer`, `FrameGeometry` with `from_dims` and `tile_count`, `NV12Buffer`, `PrevFrame`).
5. The `pub struct GpuFrameProcessor` definition (~70 fields).
6. `impl GpuFrameProcessor` containing only the three public methods. `new` and `process_frame` are thin wrappers; `diff` is small. Use this exact form:

```rust
impl GpuFrameProcessor {
    pub fn new(max_tiles: u32) -> Result<Self, Box<dyn std::error::Error>> {
        unsafe { Self::new_inner(max_tiles) }
    }

    pub fn process_frame(
        &mut self,
        fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<FrameAnalysis, Box<dyn std::error::Error>> {
        unsafe { self.process_frame_inner(fd, width, height, stride) }
    }

    pub fn diff(
        &mut self,
        fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
        // body unchanged from current pub fn diff
        let analysis = self.process_frame(fd, width, height, stride)?;
        Ok(analysis.dirty_tiles)
    }
}
```

(If the current `pub fn diff` body does something more than the above, preserve it exactly.)

7. `impl Drop for GpuFrameProcessor { ... }` — the entire ~190-line teardown body, byte-identical.
8. Submodule declarations at the bottom:

```rust
mod setup;
mod frame;

#[cfg(test)]
mod tests;
```

- [ ] **Step 4: Create setup.rs.**

Construct `ghostframe-lib/src/capture/gpu_pipeline/setup.rs` containing:

```rust
//! Construction of [`GpuFrameProcessor`].
//!
//! Holds the giant `unsafe fn new_inner` that allocates the Vulkan device,
//! the descriptor pool, all 9 compute pipelines, and the persistent buffers
//! used across frames. Helpers for individual buffers and pipelines live in
//! [`super::super::pipeline_builder`].

use ash::vk;
use std::ffi::CStr;
use std::io;

use super::super::pipeline_builder::{self, alloc_host_buffer, alloc_host_buffer_mapped, BindingSpec, find_memory_type};
use super::*;

impl GpuFrameProcessor {
    // <PASTE: the entire `unsafe fn new_inner(...)` from the current file,
    //  byte-identical body. The function is currently the largest one in
    //  the file (~1000 lines). DO NOT modify it; just move it.>
}
```

The exact `use` list may need adjustment based on which items the function body references — if the file fails to compile after this step, the missing imports come from the parent `super::*` glob and should resolve once mod.rs is in place. Where it doesn't, add explicit imports.

- [ ] **Step 5: Create frame.rs.**

Construct `ghostframe-lib/src/capture/gpu_pipeline/frame.rs` containing:

```rust
//! Per-frame compute dispatch path for [`GpuFrameProcessor`].
//!
//! Owns the orchestrator methods (`process_frame_inner`,
//! `process_frame_with_imported`, `run_first_frame_passes`) that record the
//! 9-stage compute pipeline into a command buffer, the per-frame Vulkan
//! resource lifecycle (`ensure_nv12_buffer`, `import_dmabuf`,
//! `destroy_prev_frame`, `allocate_owned_image`), and the RAII guards used
//! to keep per-frame descriptor-set / command-buffer / fence allocations
//! safely scoped.

use ash::vk;
use std::io;

use super::super::pipeline_builder::{self, alloc_host_buffer, alloc_host_buffer_mapped, BindingSpec, find_memory_type};
use super::*;

// <PASTE: ScopedDescriptorSets struct + impl Drop, byte-identical from
//  the current file>
// <PASTE: ScopedCommandBuffers struct + impl Drop, byte-identical>
// <PASTE: ScopedFence struct + impl Drop, byte-identical>

impl GpuFrameProcessor {
    // <PASTE: unsafe fn process_frame_inner, byte-identical>
    // <PASTE: unsafe fn process_frame_with_imported, byte-identical>
    // <PASTE: unsafe fn run_first_frame_passes, byte-identical>
    // <PASTE: unsafe fn ensure_nv12_buffer, byte-identical>
    // <PASTE: unsafe fn import_dmabuf, byte-identical>
    // <PASTE: unsafe fn destroy_prev_frame, byte-identical>
    // <PASTE: unsafe fn allocate_owned_image, byte-identical>
}
```

- [ ] **Step 6: Move the test file.**

```bash
git mv ghostframe-lib/src/capture/gpu_pipeline_tests.rs ghostframe-lib/src/capture/gpu_pipeline/tests.rs
```

The test file's content is unchanged. Its `use super::*;` at the top now resolves to `crate::capture::gpu_pipeline` (the directory module), which is structurally identical to what it referenced before (the file as a module). Verify by grepping that `use super::*;` is still on the first non-blank line.

- [ ] **Step 7: Delete the old monolith.**

```bash
git rm ghostframe-lib/src/capture/gpu_pipeline.rs
```

Confirm with `ls ghostframe-lib/src/capture/gpu_pipeline.rs 2>&1` that the file is gone (exit 1) and that `ghostframe-lib/src/capture/gpu_pipeline/` is the only `gpu_pipeline*` entry.

- [ ] **Step 8: Build and fix any missing imports.**

```bash
cargo build --package ghostframe-lib --lib 2>&1 | tail -20
```

Expected outcome: clean build, no errors. If errors surface, they are almost always one of:

- "cannot find type X in this scope" inside setup.rs or frame.rs → add `use super::*;` if missing, or import the specific type explicitly.
- "function or associated item not found" on a `pipeline_builder` helper → confirm the `use super::super::pipeline_builder::...` import line includes the helper.

Fix imports in place; do NOT move code between files at this step (the boundary is fixed by Steps 3-5). If the same error pattern repeats across many sites, the import line is wrong rather than the code.

- [ ] **Step 9: Build the test crate too.**

```bash
cargo build --package ghostframe-lib --tests 2>&1 | tail -10
```

Expected: clean. If the new `tests.rs` fails to compile, it's almost certainly because something it references from `super::*` is now hidden by the boundary — e.g., a private free function it called that should have gone to mod.rs but ended up in setup.rs. Re-check the boundary by reading the test file's `use super::*;` consumers.

- [ ] **Step 10: Run the GPU test subset.**

```bash
cargo test --lib --package ghostframe-lib capture::gpu_pipeline -- --test-threads=1 2>&1 | tail -5
```

Expected: `test result: ok. 18 passed; 0 failed; 0 ignored`.

If a test name shows up under a different module path (e.g. `capture::gpu_pipeline::tests::tests::xxx`), the `mod tests;` declaration in mod.rs has a nesting bug — re-check that mod.rs declares it as `mod tests;` not `mod tests { mod tests; }`.

- [ ] **Step 11: Run the full lib suite.**

```bash
cargo test --workspace --lib -- --test-threads=1 2>&1 | grep "^test result"
```

Expected: `test result: ok. 282 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out`.

- [ ] **Step 12: Confirm clippy still clean.**

```bash
cargo clippy --package ghostframe-lib --lib 2>&1 | grep -c "too_many_arguments"
```

Expected output: `0`. (No new `too_many_arguments` introduced.)

- [ ] **Step 13: Verify the line-count math.**

```bash
wc -l ghostframe-lib/src/capture/gpu_pipeline/*.rs
```

Expected: total ≈ 5665 ± 10 (the small drift is from new `use` lines added in setup.rs and frame.rs).

- [ ] **Step 14: Stage and commit.**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline/
git status --short
```

Expected `git status --short` output (the `git mv` and `git rm` from Steps 6-7 should already have staged the deletions):

```
D  ghostframe-lib/src/capture/gpu_pipeline.rs
D  ghostframe-lib/src/capture/gpu_pipeline_tests.rs   (replaced by R because of git mv)
A  ghostframe-lib/src/capture/gpu_pipeline/mod.rs
A  ghostframe-lib/src/capture/gpu_pipeline/setup.rs
A  ghostframe-lib/src/capture/gpu_pipeline/frame.rs
R  ghostframe-lib/src/capture/gpu_pipeline_tests.rs -> ghostframe-lib/src/capture/gpu_pipeline/tests.rs
```

If anything else is staged (e.g. `.claude/` files), unstage with `git reset HEAD <path>`. NOTHING outside `ghostframe-lib/src/capture/gpu_pipeline*` should appear in this commit.

Commit:

```bash
git commit -m "$(cat <<'EOF'
refactor(capture): split gpu_pipeline.rs into directory module

Pure relocation: turn ghostframe-lib/src/capture/gpu_pipeline.rs into
gpu_pipeline/{mod,setup,frame,tests}.rs. No symbol renames, no logic
changes, no test changes. mod.rs holds the struct + data types + public
API + Drop; setup.rs holds new_inner; frame.rs holds the per-frame
dispatch path + RAII guards; tests.rs is the relocated
gpu_pipeline_tests.rs.

Submodules of gpu_pipeline see GpuFrameProcessor's private fields
without any pub(super) widening, so encapsulation is unchanged.

Phase 1 of 2; the per-stage helper extraction lands in subsequent
commits.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 15: End-of-phase e2e gate.**

Run before declaring Phase 1 done:

```bash
cargo test --workspace --tests -- --test-threads=1 2>&1 | grep "^test result"
```

Takes ~10 minutes. Expected: `282 passed` (lib) and `24 passed; 0 failed; 2 ignored` (e2e). If the e2e suite regresses, `git revert HEAD` to roll back; the split was structural and a regression here indicates a missed import or a subtle boundary error that wasn't caught by unit tests.

---

## Phase 2 — per-stage helper extraction (nine commits)

Each task below has the same shape:

1. Identify the inline block in `process_frame_with_imported` [and `run_first_frame_passes`, if applicable] that handles one stage.
2. Add a private method on `impl GpuFrameProcessor` in `frame.rs`.
3. Replace the inline block(s) with a call to the new method.
4. Verify behavior parity.
5. Commit.

The per-stage helper signature follows this template (refined in the first task and reused by the others):

```rust
unsafe fn run_<stage>_stage<'a>(
    &'a self,
    cmd: vk::CommandBuffer,
    // stage-specific image views / buffer handles passed in from the orchestrator
    <stage_inputs>,
    // small u32 array carrying the stage's push constants (current convention)
    push_constants: &[u32],
) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>>
```

Each helper:
- Allocates ONE descriptor set from `self.descriptor_pool` (wraps in `ScopedDescriptorSets`).
- Writes the stage's descriptors via `update_descriptor_sets` (the exact `WriteDescriptorSet` chain from the pre-extraction inline code).
- Binds the stage's pipeline (`self.<stage>_pipeline`) and pipeline layout (`self.<stage>_pipeline_layout`) via `cmd_bind_pipeline` + `cmd_bind_descriptor_sets`.
- Pushes constants via `cmd_push_constants` (if the stage has any).
- Dispatches via `cmd_dispatch(cmd, x, y, z)` with the same workgroup counts as the inline code.
- Returns the `ScopedDescriptorSets` guard so the orchestrator can hold it until the command buffer is submitted and the fence has signalled.

**Barriers stay in the orchestrator.** Each task's commit message should explicitly state: "Barriers between stages remain in process_frame_with_imported [and run_first_frame_passes]."

**Open implementation decisions** (resolve when extracting SAD, then keep consistent across the other 8 stages):
- Parameter ordering: from-self resource handles first (DSL, pipeline, etc.) — actually, prefer passing only ORCHESTRATOR-OWNED handles (image views, transient buffers); the helper reads its own pipeline/DSL/layout from `self.*` fields. Avoids passing `vk::Pipeline` etc. as args.
- `descriptor_pool` source: read from `self.descriptor_pool` directly. Don't pass as a parameter.
- No `enum PalRleStage` discriminator. Helpers are independent methods.

### Task 2: Extract `run_sad_stage`

**Files:** Modify `ghostframe-lib/src/capture/gpu_pipeline/frame.rs`.

**Suggested implementer model:** `sonnet` (the first stage extraction establishes the pattern; worth using a stronger model to get the signature shape right).

- [ ] **Step 1: Locate the SAD inline block in `process_frame_with_imported`.**

```bash
grep -n "SAD\|sad_set\|sad_pipeline\|sad_buffer" ghostframe-lib/src/capture/gpu_pipeline/frame.rs | head -20
```

Identify the contiguous block that:
1. Allocates `sad_set` from the descriptor pool with `sad_set_layouts = [self.descriptor_set_layout]`.
2. Calls `update_descriptor_sets` with 3 `WriteDescriptorSet` entries (current frame image, prev frame image, sad output buffer).
3. Calls `cmd_bind_pipeline(cmd, COMPUTE, self.pipeline)`, `cmd_bind_descriptor_sets(cmd, COMPUTE, self.pipeline_layout, 0, &[sad_set], &[])`, `cmd_push_constants(cmd, self.pipeline_layout, COMPUTE, 0, bytemuck::cast_slice(&sad_push))` where `sad_push: [u32; 3] = [width, height, cols]`.
4. Calls `cmd_dispatch(cmd, cols, rows, 1)` (or similar — confirm from current code).

Note: `self.pipeline` (without prefix) is the SAD pipeline in the current naming. Same for `self.pipeline_layout` and `self.descriptor_set_layout`. The other stages have prefixed names (`self.nv12_pipeline`, etc.).

- [ ] **Step 2: Add the helper method to `impl GpuFrameProcessor` in `frame.rs`.**

Append at the bottom of the existing `impl GpuFrameProcessor` block in `frame.rs`:

```rust
/// SAD (Sum of Absolute Differences) compute dispatch.
///
/// Reads the current frame image (binding 0) and the previous-frame
/// owned-image snapshot (binding 1), writes per-tile SAD scores to
/// the SAD output buffer (binding 2). Push constants are
/// `[width, height, cols]` (12 bytes).
///
/// Caller must invoke `cmd_pipeline_barrier` AFTER this call to make
/// the SAD output buffer visible to downstream stages.
///
/// # Safety
///
/// Caller must ensure: `cmd` is currently recording; the image views
/// passed are in `vk::ImageLayout::GENERAL` and remain valid for the
/// duration of the recording; the workgroup count `(cols, rows, 1)`
/// matches the SAD shader's expected dispatch shape.
unsafe fn run_sad_stage<'a>(
    &'a self,
    cmd: vk::CommandBuffer,
    current_view: vk::ImageView,
    prev_view: vk::ImageView,
    push_constants: &[u32],
    workgroups: (u32, u32, u32),
) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
    // <PASTE: the descriptor-set allocation block from the pre-extraction
    //  inline code, with the local `sad_set_layouts` and `sad_ds_alloc`
    //  built using `self.descriptor_set_layout` and `self.descriptor_pool`.
    //  The guard owns the allocated set.>

    // <PASTE: the `update_descriptor_sets` call with the 3 WriteDescriptorSet
    //  entries — current_view at binding 0 (STORAGE_IMAGE),
    //  prev_view at binding 1 (STORAGE_IMAGE),
    //  self.sad_buffer at binding 2 (STORAGE_BUFFER).>

    // Bind + push + dispatch.
    self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
    self.device.cmd_bind_descriptor_sets(
        cmd,
        vk::PipelineBindPoint::COMPUTE,
        self.pipeline_layout,
        0,
        &[guard.sets[0]],
        &[],
    );
    self.device.cmd_push_constants(
        cmd,
        self.pipeline_layout,
        vk::ShaderStageFlags::COMPUTE,
        0,
        bytemuck::cast_slice(push_constants),
    );
    self.device.cmd_dispatch(cmd, workgroups.0, workgroups.1, workgroups.2);

    Ok(guard)
}
```

Fill the `<PASTE: ...>` blocks by copying the exact existing inline code, adjusting variable names to local `let guard = ScopedDescriptorSets { ... }` form. The `WriteDescriptorSet` entries' image/buffer-info attachments now reference the parameters (`current_view`, `prev_view`) and `self.sad_buffer`.

- [ ] **Step 3: Replace the inline block in `process_frame_with_imported`.**

In `process_frame_with_imported`, find the SAD block and replace it with:

```rust
let sad_push: [u32; 3] = [width, height, cols];
let sad_ds_guard = self.run_sad_stage(
    cmd,
    current.view,
    prev_view,
    &sad_push,
    (cols, rows, 1),
)?;
```

(Use whichever local names the surrounding code uses for `current.view` and `prev_view`. The workgroup counts come from the pre-extraction code — confirm before substituting.)

Keep all `cmd_pipeline_barrier` calls before/after the extracted block exactly where they were. Do not touch barriers.

`run_first_frame_passes` does NOT need a change in this task — the first-frame path doesn't run SAD.

- [ ] **Step 4: Build.**

```bash
cargo build --package ghostframe-lib --lib 2>&1 | tail -5
```

Expected: clean. Common failure: helper visibility — confirm the method is in `impl GpuFrameProcessor { ... }` inside frame.rs, not a free function. Another common failure: forgetting to mark the helper `unsafe fn`.

- [ ] **Step 5: Run GPU tests.**

```bash
cargo test --lib --package ghostframe-lib capture::gpu_pipeline -- --test-threads=1 2>&1 | tail -5
```

Expected: `18 passed; 0 failed`. If any GPU-dependent test fails (or returns Vulkan validation errors via tracing output), the helper's descriptor writes or dispatch shape diverged from the pre-extraction code. Diff side-by-side against `git show HEAD~1` to find the difference.

- [ ] **Step 6: Run the full lib suite.**

```bash
cargo test --workspace --lib -- --test-threads=1 2>&1 | grep "^test result"
```

Expected: `282 passed`.

- [ ] **Step 7: Commit.**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline/frame.rs
git status --short
```

Should show only `M  ghostframe-lib/src/capture/gpu_pipeline/frame.rs`. Nothing else.

```bash
git commit -m "$(cat <<'EOF'
refactor(capture): extract run_sad_stage helper

Extract the SAD (Sum of Absolute Differences) compute dispatch from
process_frame_with_imported into a private method on
GpuFrameProcessor. The helper allocates one descriptor set from
self.descriptor_pool (RAII-guarded), writes the three descriptors
(current image, prev image, sad buffer), binds the SAD pipeline,
pushes [width, height, cols], and dispatches.

Behavior invariants verified: same descriptor pool, same descriptor
writes, same push constants, same workgroup count. Barriers between
stages remain in process_frame_with_imported. run_first_frame_passes
is not affected (SAD doesn't run on the first frame).

First of nine per-stage helper extractions.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 3: Extract `run_nv12_stage`

**Files:** Modify `ghostframe-lib/src/capture/gpu_pipeline/frame.rs`.

**Suggested implementer model:** `sonnet` (touches both orchestrators — first cross-cutting extraction).

- [ ] **Step 1: Locate the NV12 inline blocks.**

```bash
grep -n "nv12_set\|nv12_pipeline\|nv12_descriptor_set_layout" ghostframe-lib/src/capture/gpu_pipeline/frame.rs | head -20
```

There are TWO inline blocks for NV12: one in `process_frame_with_imported`, one in `run_first_frame_passes`. Both follow the same pattern: allocate descriptor set from pool using `self.nv12_descriptor_set_layout`, write descriptors (current image at binding 0, NV12 output buffer at binding 1), bind `self.nv12_pipeline` + `self.nv12_pipeline_layout`, push `[width, height, y_stride, uv_offset, uv_stride]` (20 bytes), dispatch `(width.div_ceil(2), height.div_ceil(2), 1)` or similar.

Note: in the first-frame path, the "current image" view is `current.view` (the imported DMA-BUF); in the steady-state path it's also `current.view`. The NV12 helper takes the view as a parameter.

- [ ] **Step 2: Add the helper method.**

Append to `impl GpuFrameProcessor` in frame.rs:

```rust
/// NV12 (BGRA → NV12) compute dispatch.
///
/// Reads the current frame image (binding 0, STORAGE_IMAGE), writes
/// to the NV12 HOST_VISIBLE output buffer (binding 1, STORAGE_BUFFER).
/// Push constants are `[width, height, y_stride, uv_offset, uv_stride]`
/// (20 bytes).
///
/// Caller must invoke `cmd_pipeline_barrier` AFTER this call to make
/// the NV12 buffer's HOST_VISIBLE memory writes visible to the host
/// (HOST_READ | HOST_WRITE access for the readback).
///
/// # Safety
///
/// Caller must ensure: `cmd` is currently recording; `current_view` is
/// in `vk::ImageLayout::GENERAL`; `nv12_buffer` is bound to memory and
/// sized at least to fit `width * height + uv_offset + (width *
/// height / 2)` bytes; the workgroup count matches the shader's
/// half-resolution NV12 dispatch shape.
unsafe fn run_nv12_stage<'a>(
    &'a self,
    cmd: vk::CommandBuffer,
    current_view: vk::ImageView,
    nv12_buffer: vk::Buffer,
    push_constants: &[u32],
    workgroups: (u32, u32, u32),
) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
    // <PASTE: descriptor set allocation from `self.nv12_descriptor_set_layout`
    //  via `self.descriptor_pool`, wrapped in ScopedDescriptorSets>

    // <PASTE: update_descriptor_sets with 2 entries —
    //  current_view at binding 0, nv12_buffer at binding 1>

    self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.nv12_pipeline);
    self.device.cmd_bind_descriptor_sets(
        cmd,
        vk::PipelineBindPoint::COMPUTE,
        self.nv12_pipeline_layout,
        0,
        &[guard.sets[0]],
        &[],
    );
    self.device.cmd_push_constants(
        cmd,
        self.nv12_pipeline_layout,
        vk::ShaderStageFlags::COMPUTE,
        0,
        bytemuck::cast_slice(push_constants),
    );
    self.device.cmd_dispatch(cmd, workgroups.0, workgroups.1, workgroups.2);

    Ok(guard)
}
```

- [ ] **Step 3: Replace the inline blocks in BOTH orchestrators.**

In `process_frame_with_imported`:

```rust
let nv12_push: [u32; 5] = [width, height, nv12_y_stride, nv12_uv_offset, nv12_uv_stride];
let nv12_ds_guard = self.run_nv12_stage(
    cmd,
    current.view,
    nv12_buffer,
    &nv12_push,
    (width.div_ceil(2), height.div_ceil(2), 1),
)?;
```

In `run_first_frame_passes`, do the SAME substitution at the corresponding location. Both should produce identical call patterns.

Confirm the workgroup formula matches the pre-extraction code — `width.div_ceil(2)` is the most common NV12 layout but check exactly.

- [ ] **Step 4: Build + test.**

```bash
cargo build --package ghostframe-lib --lib 2>&1 | tail -3
cargo test --lib --package ghostframe-lib capture::gpu_pipeline -- --test-threads=1 2>&1 | tail -3
cargo test --workspace --lib -- --test-threads=1 2>&1 | grep "^test result"
```

Expected: clean, `18 passed`, `282 passed`.

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline/frame.rs
git commit -m "$(cat <<'EOF'
refactor(capture): extract run_nv12_stage helper

Extract the BGRA→NV12 compute dispatch from
process_frame_with_imported AND run_first_frame_passes into a private
method on GpuFrameProcessor.

First cross-orchestrator extraction: both per-frame paths now call the
same helper, eliminating one of the audit-flagged duplicated blocks.

Behavior invariants verified: same descriptor pool, same descriptor
writes, same push constants ([w, h, y_stride, uv_offset, uv_stride]),
same workgroup count. Barriers between stages remain in the
orchestrators.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 4: Extract `run_analysis_stage`

**Suggested implementer model:** `sonnet`.

Follows the same pattern as `run_nv12_stage` — called from both orchestrators.

- [ ] **Step 1: Locate analysis inline blocks.**

```bash
grep -n "analysis_set\|analysis_pipeline\|analysis_descriptor_set_layout" ghostframe-lib/src/capture/gpu_pipeline/frame.rs | head -20
```

Two blocks (process_frame_with_imported + run_first_frame_passes). Each: allocate DS using `self.analysis_descriptor_set_layout`, write 2 descriptors (current image at binding 0 STORAGE_IMAGE, analysis output buffer `self.analysis_buffer` at binding 1 STORAGE_BUFFER), bind `self.analysis_pipeline` + `self.analysis_pipeline_layout`, push `[width, height, cols]` (12 bytes), dispatch `(cols, rows, 1)`.

- [ ] **Step 2: Add the helper.**

Append to `impl GpuFrameProcessor`:

```rust
/// Per-tile color analysis compute dispatch.
///
/// Reads the current frame image (binding 0, STORAGE_IMAGE), writes
/// per-tile `TileAnalysis` entries to the analysis buffer (binding 1,
/// STORAGE_BUFFER). Push constants are `[width, height, cols]` (12 bytes).
///
/// # Safety
///
/// Caller must ensure: `cmd` is recording; `current_view` is in GENERAL
/// layout; workgroup count is `(cols, rows, 1)` to dispatch one workgroup
/// per tile.
unsafe fn run_analysis_stage<'a>(
    &'a self,
    cmd: vk::CommandBuffer,
    current_view: vk::ImageView,
    push_constants: &[u32],
    workgroups: (u32, u32, u32),
) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
    // <PASTE descriptor-set alloc from self.analysis_descriptor_set_layout>
    // <PASTE update_descriptor_sets: current_view@0, self.analysis_buffer@1>

    self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.analysis_pipeline);
    self.device.cmd_bind_descriptor_sets(
        cmd,
        vk::PipelineBindPoint::COMPUTE,
        self.analysis_pipeline_layout,
        0,
        &[guard.sets[0]],
        &[],
    );
    self.device.cmd_push_constants(
        cmd,
        self.analysis_pipeline_layout,
        vk::ShaderStageFlags::COMPUTE,
        0,
        bytemuck::cast_slice(push_constants),
    );
    self.device.cmd_dispatch(cmd, workgroups.0, workgroups.1, workgroups.2);

    Ok(guard)
}
```

- [ ] **Step 3: Replace inline blocks in both orchestrators.**

```rust
let analysis_push: [u32; 3] = [width, height, cols];
let analysis_ds_guard = self.run_analysis_stage(
    cmd,
    current.view,
    &analysis_push,
    (cols, rows, 1),
)?;
```

Substitute at corresponding locations in `process_frame_with_imported` and `run_first_frame_passes`.

- [ ] **Step 4: Build + test + commit.**

Same verification as Task 3. Commit message:

```
refactor(capture): extract run_analysis_stage helper

Extract the per-tile color-analysis compute dispatch from both per-frame
paths into a private method on GpuFrameProcessor. Same shape as
run_sad_stage but with different bindings and a different pipeline.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
```

### Task 5: Extract `run_palrle_compact_stage`

**Suggested implementer model:** `sonnet`.

- [ ] **Step 1: Locate the palrle_compact block.**

Search for `palrle_compact_set`, `palrle_compact_pipeline`, etc. Appears in both orchestrators.

Bindings (4 STORAGE_BUFFER): SAD output (binding 0), tile analysis (binding 1), compact list output (binding 2), compact count output (binding 3). Reads from `self.sad_buffer` and `self.analysis_buffer`; writes to `self.palrle_compact_list_buffer` and `self.palrle_compact_count_buffer`. Push constants: `[cols, rows, dirty_threshold]` (12 bytes). Workgroups: `(1, 1, 1)` (single-workgroup scan).

- [ ] **Step 2: Add the helper.**

```rust
/// PalRLE compact compute dispatch.
///
/// Reads SAD output (binding 0, STORAGE_BUFFER) and tile analysis
/// (binding 1, STORAGE_BUFFER); writes the compact list of
/// PalRLE-feasible tile indices (binding 2) and the compact count
/// (binding 3). Push constants are `[cols, rows, dirty_threshold]`.
///
/// # Safety
///
/// Caller must ensure: `cmd` is recording; the SAD and analysis
/// buffers are populated and made visible via a preceding barrier;
/// workgroup count `(1, 1, 1)` matches the shader's single-workgroup
/// scan implementation.
unsafe fn run_palrle_compact_stage<'a>(
    &'a self,
    cmd: vk::CommandBuffer,
    push_constants: &[u32],
    workgroups: (u32, u32, u32),
) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
    // <PASTE descriptor-set alloc from self.palrle_compact_descriptor_set_layout>
    // <PASTE update_descriptor_sets with 4 entries:
    //   self.sad_buffer@0, self.analysis_buffer@1,
    //   self.palrle_compact_list_buffer@2, self.palrle_compact_count_buffer@3>

    // Bind, push, dispatch
    self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.palrle_compact_pipeline);
    self.device.cmd_bind_descriptor_sets(
        cmd,
        vk::PipelineBindPoint::COMPUTE,
        self.palrle_compact_pipeline_layout,
        0,
        &[guard.sets[0]],
        &[],
    );
    self.device.cmd_push_constants(
        cmd,
        self.palrle_compact_pipeline_layout,
        vk::ShaderStageFlags::COMPUTE,
        0,
        bytemuck::cast_slice(push_constants),
    );
    self.device.cmd_dispatch(cmd, workgroups.0, workgroups.1, workgroups.2);

    Ok(guard)
}
```

Note: no image-view parameters because palrle_compact reads only buffers; those are accessed from `self.*` fields.

- [ ] **Step 3: Replace inline blocks in both orchestrators.**

```rust
let palrle_compact_push: [u32; 3] = [cols, rows, /* dirty_threshold */];
let palrle_compact_ds_guard = self.run_palrle_compact_stage(
    cmd,
    &palrle_compact_push,
    (1, 1, 1),
)?;
```

The exact `dirty_threshold` value comes from the pre-extraction code — preserve it verbatim.

- [ ] **Step 4: Build + test + commit.**

Standard verification. Commit message:

```
refactor(capture): extract run_palrle_compact_stage helper

Extract the PalRLE compact compute dispatch from both per-frame paths.
Reads sad_buffer + analysis_buffer; writes palrle_compact_list_buffer
+ palrle_compact_count_buffer. Push constants are
[cols, rows, dirty_threshold]; workgroup is (1, 1, 1).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
```

### Task 6: Extract `run_palrle_indirect_args_stage`

**Suggested implementer model:** `sonnet` (small but a no-push-constants edge case).

Pattern: 2 STORAGE_BUFFER bindings (compact count input at 0, indirect-args output at 1). No push constants. Workgroups `(1, 1, 1)`.

Reads `self.palrle_compact_count_buffer`; writes `self.palrle_indirect_args_buffer`. Pipeline is `self.palrle_indirect_args_pipeline`.

- [ ] **Step 1: Add the helper.**

```rust
/// PalRLE indirect-args compute dispatch (builds the dispatch params
/// for the pal_rle_index stage from the compact count).
///
/// Reads the compact count (binding 0, STORAGE_BUFFER); writes
/// indirect dispatch args (binding 1, STORAGE_BUFFER). No push
/// constants. Workgroups `(1, 1, 1)`.
///
/// # Safety
///
/// Caller must ensure: `cmd` is recording; the compact count buffer
/// has been populated by a preceding palrle_compact dispatch and a
/// barrier; the indirect args buffer is bound to memory.
unsafe fn run_palrle_indirect_args_stage<'a>(
    &'a self,
    cmd: vk::CommandBuffer,
) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
    // <PASTE descriptor-set alloc from self.palrle_indirect_args_descriptor_set_layout>
    // <PASTE update_descriptor_sets with 2 entries:
    //   self.palrle_compact_count_buffer@0, self.palrle_indirect_args_buffer@1>

    self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.palrle_indirect_args_pipeline);
    self.device.cmd_bind_descriptor_sets(
        cmd,
        vk::PipelineBindPoint::COMPUTE,
        self.palrle_indirect_args_pipeline_layout,
        0,
        &[guard.sets[0]],
        &[],
    );
    // No push constants for this stage.
    self.device.cmd_dispatch(cmd, 1, 1, 1);

    Ok(guard)
}
```

- [ ] **Step 2: Replace inline blocks in both orchestrators with `let g = self.run_palrle_indirect_args_stage(cmd)?;`.**

- [ ] **Step 3: Build + test + commit.**

Commit message:

```
refactor(capture): extract run_palrle_indirect_args_stage helper

Extract the indirect-args build dispatch into a private method. This
stage has no push constants and a fixed (1,1,1) workgroup, so the
helper signature drops both parameters compared to the SAD/NV12
template.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
```

### Task 7: Extract `run_palette_fold_stage`

**Suggested implementer model:** `sonnet`.

Pattern: 6 STORAGE_BUFFER bindings (tile analysis@0 r/o, compact list@1 r/o, frame_palette_set@2 r/w, frame_palette_count@3 r/w, hash_table@4 r/w scratch, per_tile_frame_palette_id@5 r/w). No push constants. Workgroups: `(1, 1, 1)` (single-workgroup scan).

All buffers read from `self.*`. Pipeline is `self.palette_fold_pipeline`.

- [ ] **Step 1: Add the helper.**

```rust
/// Stage 2a — palette fold compute dispatch (builds the per-frame
/// palette set by merging duplicate tile palettes).
///
/// Six STORAGE_BUFFER bindings: tile analysis (0 r/o), compact list
/// (1 r/o), frame_palette_set (2 r/w), frame_palette_count (3 r/w),
/// hash_table scratch (4 r/w), per_tile_frame_palette_id (5 r/w).
/// No push constants. Workgroups `(1, 1, 1)`.
///
/// # Safety
///
/// Caller must ensure: `cmd` is recording; preceding stages have
/// populated `analysis_buffer` and `palrle_compact_list_buffer`
/// (with barriers); `hash_table_buffer` has been cleared
/// (`vkCmdFillBuffer`) earlier in the command buffer.
unsafe fn run_palette_fold_stage<'a>(
    &'a self,
    cmd: vk::CommandBuffer,
) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
    // <PASTE descriptor-set alloc from self.palette_fold_descriptor_set_layout>
    // <PASTE update_descriptor_sets with 6 entries — see binding map above>

    self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.palette_fold_pipeline);
    self.device.cmd_bind_descriptor_sets(
        cmd,
        vk::PipelineBindPoint::COMPUTE,
        self.palette_fold_pipeline_layout,
        0,
        &[guard.sets[0]],
        &[],
    );
    self.device.cmd_dispatch(cmd, 1, 1, 1);

    Ok(guard)
}
```

- [ ] **Step 2: Replace inline blocks in both orchestrators.**

Each call: `let g = self.run_palette_fold_stage(cmd)?;`.

- [ ] **Step 3: Build + test + commit.**

Commit message:

```
refactor(capture): extract run_palette_fold_stage helper

Extract the Stage 2a palette-fold compute dispatch from both per-frame
paths. Six STORAGE_BUFFER bindings, no push constants, single-workgroup
dispatch.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
```

### Task 8: Extract `run_palette_subset_fold_init_stage`

**Suggested implementer model:** `haiku` (the smallest helper — single binding, no push constants, single workgroup).

Pattern: 1 STORAGE_BUFFER binding (folded_into at 0). No push constants. Workgroups: depends on PALETTE_HASH_SLOTS — typically dispatched as `(PALETTE_HASH_SLOTS as u32, 1, 1)` or similar single-pass init.

- [ ] **Step 1: Add the helper.**

```rust
/// Stage 2b init — zero out the `folded_into` array to the default-self
/// sentinel before the subset-fold scan.
///
/// Single STORAGE_BUFFER binding: `folded_into` at binding 0.
/// No push constants. Dispatch shape matches the shader's init loop.
///
/// # Safety
///
/// Caller must ensure: `cmd` is recording; `folded_into_buffer` is bound;
/// the workgroup count provided initializes all `PALETTE_HASH_SLOTS`
/// entries (consult the shader's `@workgroup_size` attribute).
unsafe fn run_palette_subset_fold_init_stage<'a>(
    &'a self,
    cmd: vk::CommandBuffer,
    workgroups: (u32, u32, u32),
) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
    // <PASTE descriptor-set alloc from self.palette_subset_fold_init_descriptor_set_layout>
    // <PASTE update_descriptor_sets with 1 entry: self.folded_into_buffer@0>

    self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.palette_subset_fold_init_pipeline);
    self.device.cmd_bind_descriptor_sets(
        cmd,
        vk::PipelineBindPoint::COMPUTE,
        self.palette_subset_fold_init_pipeline_layout,
        0,
        &[guard.sets[0]],
        &[],
    );
    self.device.cmd_dispatch(cmd, workgroups.0, workgroups.1, workgroups.2);

    Ok(guard)
}
```

- [ ] **Step 2: Replace inline blocks.**

Substitute the workgroup tuple verbatim from the pre-extraction code.

- [ ] **Step 3: Build + test + commit.**

Commit message:

```
refactor(capture): extract run_palette_subset_fold_init_stage helper

Extract the Stage 2b init compute dispatch (zero folded_into to
default-self) from both per-frame paths. Single STORAGE_BUFFER
binding, no push constants.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
```

### Task 9: Extract `run_palette_subset_fold_stage`

**Suggested implementer model:** `sonnet`.

Pattern: 2 STORAGE_BUFFER bindings (frame_palette_set@0 r/o, folded_into@1 r/w). No push constants. Workgroups: `(PALETTE_HASH_SLOTS as u32, 1, 1)` per `cmd_dispatch(cmd, PALETTE_HASH_SLOTS as u32, 1, 1)`.

- [ ] **Step 1: Add the helper.**

```rust
/// Stage 2b — palette subset-fold compute dispatch (resolves subset
/// palettes into their containing supersets).
///
/// Two STORAGE_BUFFER bindings: frame_palette_set (0, r/o),
/// folded_into (1, r/w). No push constants. Workgroups
/// `(PALETTE_HASH_SLOTS, 1, 1)` so each slot evaluates its own row.
///
/// # Safety
///
/// Caller must ensure: `cmd` is recording; `frame_palette_set` is
/// populated by preceding palette_fold + barrier;
/// `palette_subset_fold_init_stage` has run earlier and a barrier
/// makes its writes visible.
unsafe fn run_palette_subset_fold_stage<'a>(
    &'a self,
    cmd: vk::CommandBuffer,
) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
    // <PASTE descriptor-set alloc from self.palette_subset_fold_descriptor_set_layout>
    // <PASTE update_descriptor_sets with 2 entries:
    //   self.frame_palette_set_buffer@0, self.folded_into_buffer@1>

    self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.palette_subset_fold_pipeline);
    self.device.cmd_bind_descriptor_sets(
        cmd,
        vk::PipelineBindPoint::COMPUTE,
        self.palette_subset_fold_pipeline_layout,
        0,
        &[guard.sets[0]],
        &[],
    );
    self.device.cmd_dispatch(cmd, PALETTE_HASH_SLOTS as u32, 1, 1);

    Ok(guard)
}
```

- [ ] **Step 2: Replace inline blocks in both orchestrators.**

`let g = self.run_palette_subset_fold_stage(cmd)?;` (no parameters; workgroup is hardcoded inside the helper since it's a constant).

- [ ] **Step 3: Build + test + commit.**

Commit message:

```
refactor(capture): extract run_palette_subset_fold_stage helper

Extract the Stage 2b palette-subset-fold dispatch from both per-frame
paths. Two STORAGE_BUFFER bindings, no push constants. Workgroup count
(PALETTE_HASH_SLOTS, 1, 1) is hardcoded inside the helper.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
```

### Task 10: Extract `run_pal_rle_index_stage`

**Suggested implementer model:** `sonnet` (final stage, indirect-dispatch consumer).

Pattern: 6 bindings — 1 STORAGE_IMAGE (current frame@0) + 5 STORAGE_BUFFER (compact_list@1 r/o, frame_palette_set@2 r/o, per_tile_frame_palette_id@3 r/o, folded_into@4 r/o, index_buffer@5 r/w). Push constants: `[cols]` (4 bytes). Workgroups: 4×256 packed-nibble decoder, dispatched indirectly via `cmd_dispatch_indirect` reading from `self.palrle_indirect_args_buffer`.

This is the most complex helper because it uses `cmd_dispatch_indirect` instead of `cmd_dispatch`.

- [ ] **Step 1: Add the helper.**

```rust
/// Stage 3 — pal_rle_index compute dispatch (emits the nibble-packed
/// index buffer for each PalRLE-feasible tile, indirectly dispatched
/// from the count produced by palrle_indirect_args).
///
/// Bindings: current frame (0, STORAGE_IMAGE r/o), compact_list (1,
/// STORAGE_BUFFER r/o), frame_palette_set (2, r/o),
/// per_tile_frame_palette_id (3, r/o), folded_into (4, r/o),
/// index_buffer (5, r/w). Push constants `[cols]` (4 bytes).
///
/// Uses `cmd_dispatch_indirect` reading from
/// `self.palrle_indirect_args_buffer` at offset 0.
///
/// # Safety
///
/// Caller must ensure: `cmd` is recording; `current_view` is in GENERAL
/// layout; preceding stages (compact, fold, subset_fold) have populated
/// their outputs with barriers; the indirect-args buffer holds a valid
/// `(group_count_x, group_count_y, group_count_z)` triple at offset 0.
unsafe fn run_pal_rle_index_stage<'a>(
    &'a self,
    cmd: vk::CommandBuffer,
    current_view: vk::ImageView,
    push_constants: &[u32],
) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>> {
    // <PASTE descriptor-set alloc from self.pal_rle_index_descriptor_set_layout>
    // <PASTE update_descriptor_sets with 6 entries — see binding map above>

    self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pal_rle_index_pipeline);
    self.device.cmd_bind_descriptor_sets(
        cmd,
        vk::PipelineBindPoint::COMPUTE,
        self.pal_rle_index_pipeline_layout,
        0,
        &[guard.sets[0]],
        &[],
    );
    self.device.cmd_push_constants(
        cmd,
        self.pal_rle_index_pipeline_layout,
        vk::ShaderStageFlags::COMPUTE,
        0,
        bytemuck::cast_slice(push_constants),
    );
    self.device.cmd_dispatch_indirect(cmd, self.palrle_indirect_args_buffer, 0);

    Ok(guard)
}
```

- [ ] **Step 2: Replace inline blocks in both orchestrators.**

```rust
let pal_rle_index_push: [u32; 1] = [cols];
let pal_rle_index_ds_guard = self.run_pal_rle_index_stage(
    cmd,
    current.view,
    &pal_rle_index_push,
)?;
```

Note: this uses `cmd_dispatch_indirect`, not `cmd_dispatch`. Verify the pre-extraction code does the same — if it uses direct dispatch with a hardcoded count, mirror that instead.

- [ ] **Step 3: Build + test.**

Standard verification. Pay particular attention to GPU tests that exercise the PalRLE path (`process_frame_emits_correct_indices_for_two_color_tile` is the most direct one).

- [ ] **Step 4: Commit.**

```
refactor(capture): extract run_pal_rle_index_stage helper

Extract the Stage 3 pal_rle_index compute dispatch from both per-frame
paths. Uses cmd_dispatch_indirect reading from
palrle_indirect_args_buffer (final stage in the PalRLE pipeline).
Six bindings (1 image, 5 buffers), push constants [cols].

Final per-stage helper extraction. process_frame_with_imported and
run_first_frame_passes now each consist of command-buffer setup,
sequenced self.run_*_stage(...) calls with cmd_pipeline_barrier
between them, and submit+wait.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
```

### Task 11: End-of-phase verification + line-count gate

**Files:** None modified (verification only).

**Suggested implementer model:** `haiku`.

- [ ] **Step 1: Full e2e suite.**

```bash
cd /home/cedric/work/ghostframe
cargo test --workspace --tests -- --test-threads=1 2>&1 | grep "^test result"
```

Expected (takes ~10 min):
```
test result: ok. 282 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 24 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

If the e2e suite reports a failure, it identifies a regression introduced somewhere in Phase 2 — `git bisect` from the start of Phase 2 to HEAD, with `cargo test --lib --package ghostframe-lib capture::gpu_pipeline -- --test-threads=1` as the predicate, finds the offending commit in ~4 iterations.

- [ ] **Step 2: vitest sanity-check.**

```bash
cd ghostframe-web-client && npx vitest run 2>&1 | grep -E "Tests|Files" | head -3
```

Expected: `Tests 27 passed (27)` and `Test Files 6 passed (6)`. (Not affected by this refactor, but a useful sanity check that nothing in the shared crate spilled.)

- [ ] **Step 3: Confirm `frame.rs` size cap.**

```bash
cd /home/cedric/work/ghostframe
wc -l ghostframe-lib/src/capture/gpu_pipeline/*.rs
```

Expected: `frame.rs` ≤ 1500 lines. If it's larger, an extraction was incomplete — re-check that each of the 9 inline blocks was replaced (no copies left in `process_frame_with_imported` or `run_first_frame_passes`).

- [ ] **Step 4: Confirm no clippy regression.**

```bash
cargo clippy --package ghostframe-lib --lib 2>&1 | grep -E "too_many_arguments|warning:" | head -5
```

Expected: zero `too_many_arguments`; no new warnings beyond pre-existing ones (the `unused variable: inject_will_fire` and `unused function: phase_b_encode_payloads` warnings, both from `io_bridge.rs`, predate this refactor).

- [ ] **Step 5: Final commit listing.**

```bash
git log --oneline 8b21d50..HEAD
```

Replace `8b21d50` with whatever the pre-Phase-1 commit was. Expected output: 10 commits, in order — the file split, then 9 per-stage extractions.

If everything above passes, Phase 2 is complete.

---

## Done definition

- `ghostframe-lib/src/capture/gpu_pipeline.rs` no longer exists.
- `ghostframe-lib/src/capture/gpu_pipeline/{mod,setup,frame,tests}.rs` all exist with content matching the spec's "Target structure" section.
- All 9 per-stage helpers exist on `impl GpuFrameProcessor` in `frame.rs`.
- `process_frame_with_imported` and `run_first_frame_passes` each consist of a command-buffer setup section, a sequence of `self.run_*_stage(...)` calls with `cmd_pipeline_barrier` between them, and a submit+wait section.
- 282 lib + 24 e2e + 27 vitest tests pass.
- No new `#[allow(clippy::too_many_arguments)]` annotations introduced.
- `frame.rs` ≤ 1500 lines.

---

## Out of scope (do NOT do these as part of this plan)

- `io_bridge.rs` decomposition (separate plan, sequential).
- Restructuring `Drop`'s 190-line body.
- Adding/removing GPU stages or changing workgroup counts.
- Changing push-constant struct shapes from `&[u32]` to typed structs.
- Tuning `max_sets` on the descriptor pool.
- Splitting `process_frame_with_imported` into multiple orchestrators.
- Documentation rewrites beyond what new methods require.
- Test reorganization beyond the file relocation in Phase 1.
