# gpu_pipeline.rs decomposition — design

**Date:** 2026-05-20
**Scope:** `ghostframe-lib/src/capture/gpu_pipeline.rs`
**Status:** Draft → ready for implementation planning

## Background

`gpu_pipeline.rs` is the largest production file in the crate at 4090 lines (down
from 6131 after several rounds of mechanical cleanup in May 2026). It defines
`GpuFrameProcessor`, the Vulkan-backed dirty-detection + analysis + NV12-conversion
pipeline that runs per captured frame. The file conflates four distinct concerns:

- The public API surface (`pub fn new`, `pub fn process_frame`, `pub fn diff`) and
  the data structs they return (`FrameAnalysis` and friends).
- The 1000+ line constructor (`new_inner`) that allocates all GPU resources.
- The per-frame dispatch path (`process_frame_with_imported` ≈ 1300 lines,
  `run_first_frame_passes` ≈ 1080 lines) that records the 9-stage compute
  pipeline into a command buffer.
- Vulkan resource lifecycle plumbing (RAII guards, DMA-BUF import, owned image
  allocation, `Drop`).

A previous round of cleanup (commits `5971bca`, `89e301a`, `8b21d50`, `fc601cf`,
`e8322e8`, `6d24463`, etc.) consolidated the worst boilerplate within the file
(`build_compute_pipeline`, `alloc_host_buffer*`, named constants, `Nv12OutputLayout`,
`FrameGeometry`) but did not change file structure. A follow-up audit concluded
the next-step decomposition is now mechanical and revertible — this spec defines
that decomposition.

The companion file `io_bridge.rs` will be brainstormed and specced separately;
the two specs may then be implemented in parallel.

## Goals and non-goals

**Goals.**
- Split `gpu_pipeline.rs` along a lifecycle seam so future readers can navigate
  setup vs. per-frame work independently.
- Reduce per-frame orchestrator method length by extracting 9 per-stage dispatch
  helpers that are also shared between the steady-state and first-frame paths.
- Preserve test coverage and test paths exactly (no renames).
- Land in small, individually revertible commits.

**Non-goals.**
- Restructuring `Drop`'s teardown order. Stays verbatim in mod.rs.
- Adding new GPU stages, removing existing ones, or tuning workgroup counts.
- Changing the public API surface, field layout of any exported struct, or the
  wire format consumed by downstream encoders.
- Touching `io_bridge.rs` (separate spec).
- Tuning the descriptor-pool `max_sets` bound (sized for the current sequence;
  refactor doesn't change call count).
- Reworking module-level documentation beyond what new methods require.

## Target structure

After the refactor:

```
ghostframe-lib/src/capture/
  gpu_pipeline/
    mod.rs       — public surface + state (~700 lines)
    setup.rs     — construction (~1000 lines)
    frame.rs     — per-frame dispatch + per-stage helpers (~1300-1500 lines)
    tests.rs     — relocated from ../gpu_pipeline_tests.rs (~1575 lines)
  pipeline_builder.rs   — unchanged
  dmabuf.rs             — unchanged
```

### `mod.rs` contents

- Module-level doc comment (carried over from current top-of-file block).
- Constants: `TILE_SIZE`, `PALETTE_HASH_SLOTS`, `PER_TILE_INDEX_BYTES`,
  `PALETTE_SLOT_U32_BYTES`.
- Data structs + their inherent impls:
  - `TileAnalysis`, `FramePaletteEntryRaw`
  - `FrameAnalysis` with all slice helpers (`tile_analysis_slice`,
    `palrle_compact_list_slice`, `frame_palette_set_slice`,
    `per_tile_frame_palette_id_slice`, `folded_into_slice`,
    `index_buffer_slice_for`)
  - `Nv12OutputLayout` with `from_buffer`
  - `FrameGeometry` with `from_dims`, `tile_count`
  - `NV12Buffer`, `PrevFrame`
- `GpuFrameProcessor` struct definition (the ~70 fields stay private).
- `impl GpuFrameProcessor`: only the three public entry methods, each a thin
  wrapper into a submodule:
  - `pub fn new()` → `setup::new_inner`
  - `pub fn process_frame()` → `frame::process_frame_inner`
  - `pub fn diff()` → calls `process_frame` and projects.
- `impl Drop for GpuFrameProcessor` — the ~190-line teardown body, unchanged.
- Submodule declarations:
  ```rust
  mod setup;
  mod frame;
  #[cfg(test)]
  mod tests;
  ```
- Imports: `use super::pipeline_builder::{...}`, `ash::vk`, `std::*`.

### `setup.rs` contents

- Brief module doc comment.
- `impl GpuFrameProcessor { unsafe fn new_inner(max_tiles: u32) -> Result<Self, Box<dyn std::error::Error>> { ... } }`
  — the existing constructor body, byte-identical.
- Any setup-specific local helpers (currently none beyond what already lives in
  `pipeline_builder.rs`).
- `use super::*;` plus explicit imports as needed.

### `frame.rs` contents

- Brief module doc comment.
- RAII guards: `ScopedDescriptorSets`, `ScopedCommandBuffers`, `ScopedFence` —
  moved here from the top of the current file since only per-frame paths use
  them.
- `impl GpuFrameProcessor`:
  - `unsafe fn process_frame_inner` (entry from public `process_frame`).
  - `unsafe fn process_frame_with_imported` (the main dispatch path).
  - `unsafe fn run_first_frame_passes` (the first-frame variant).
  - `unsafe fn ensure_nv12_buffer` (per-frame resize).
  - `unsafe fn import_dmabuf`, `unsafe fn destroy_prev_frame`,
    `unsafe fn allocate_owned_image` (per-frame VK resource lifecycle).
  - **The 9 new per-stage helpers (Phase 2):** `run_sad_stage`, `run_nv12_stage`,
    `run_analysis_stage`, `run_palrle_compact_stage`,
    `run_palrle_indirect_args_stage`, `run_palette_fold_stage`,
    `run_palette_subset_fold_init_stage`, `run_palette_subset_fold_stage`,
    `run_pal_rle_index_stage`.
- `use super::*;` plus explicit imports as needed.

### `tests.rs` contents

Relocated from `gpu_pipeline_tests.rs` with **zero content change**. Declared
from mod.rs via `#[cfg(test)] mod tests;` (no `#[path]` indirection because the
test file now sits inside the directory module). Test paths remain
`capture::gpu_pipeline::tests::*` — unchanged.

### Visibility

Submodules of `gpu_pipeline` see `GpuFrameProcessor`'s private fields and all of
mod.rs's private items by default. No `pub(super)` widening is required.
Cross-file method calls (`self.process_frame_inner(...)` from
`mod.rs::process_frame`) work because Rust permits `impl GpuFrameProcessor` blocks
across multiple files within the same module — coherence is unaffected.

### External callers

`capture/mod.rs` continues to declare `pub mod gpu_pipeline;`. Callers importing
from `crate::capture::gpu_pipeline` see no change in API surface — the directory
module shares its name with the old file.

## Phase 1 — pure file split

Single commit, zero behavior change. Mechanical "cut and paste" across files;
no symbols renamed, no methods restructured, no logic touched.

### Mechanics

1. Create directory `ghostframe-lib/src/capture/gpu_pipeline/`.
2. Move content from the current `gpu_pipeline.rs` into the three new files per
   the boundary defined above. The split is character-identical — no whitespace
   adjustments, no `use` reorderings except those required for compilation.
3. Move `gpu_pipeline_tests.rs` to `gpu_pipeline/tests.rs` (content untouched).
4. Delete the old `gpu_pipeline.rs` and `gpu_pipeline_tests.rs`.
5. `capture/mod.rs` is unchanged — `pub mod gpu_pipeline;` resolves to a directory
   module transparently.

### Imports inside the new submodules

The bodies of `new_inner` and `process_frame_with_imported` reference
`pipeline_builder::alloc_host_buffer`, `BindingSpec`, `find_memory_type`, ash's
`vk::*`, `std::*`, and the data types defined in mod.rs. The submodules need a
`use super::*;` (or explicit imports) at the top. This is the only non-content
edit allowed in Phase 1.

### Verification before commit

```
cargo build --package ghostframe-lib --lib 2>&1 | tail -3
cargo build --package ghostframe-lib --tests 2>&1 | tail -3
cargo test --workspace --lib -- --test-threads=1 | grep "^test result"
cargo clippy --package ghostframe-lib --lib 2>&1 | grep "^warning" | wc -l
```

Expected: clean build, `282 passed; 0 failed`, no new clippy warnings.

`wc -l` confirms total lines ≈ 4090 + 1575 = 5665 distributed across the four
new files (small drift expected from `use` line additions; should be < 10 lines
net).

`git show <sha> --stat` shows: 2 deleted (`gpu_pipeline.rs`, `gpu_pipeline_tests.rs`)
+ 4 added (`gpu_pipeline/{mod,setup,frame,tests}.rs`).

### Commit message style

```
refactor(capture): split gpu_pipeline.rs into directory module

Pure relocation: turn ghostframe-lib/src/capture/gpu_pipeline.rs into
gpu_pipeline/{mod,setup,frame,tests}.rs. No symbol renames, no logic
changes, no test changes. mod.rs holds the struct + data types + public
API + Drop; setup.rs holds new_inner; frame.rs holds the per-frame
dispatch path; tests.rs is the relocated gpu_pipeline_tests.rs.

(Phase 1 of 2; the per-stage helper extraction lands in subsequent
commits.)
```

### Risks in Phase 1

Very low. Failure modes: a missed `use` statement causing a compile error
(caught immediately by `cargo build`) or a typo in the directory rename. No
runtime risk.

## Phase 2 — per-stage helper extraction

After Phase 1 lands, `frame.rs` contains two large orchestrator methods that each
dispatch a sequence of compute stages. Phase 2 extracts the 9 stages into
per-stage methods on `impl GpuFrameProcessor`, **one commit per stage**.

### Helper signature template

```rust
unsafe fn run_<stage>_stage<'a>(
    &'a self,
    cmd: vk::CommandBuffer,
    descriptor_pool: vk::DescriptorPool,
    // stage-specific image views / buffer handles passed in from the orchestrator
    <stage_inputs>,
    // small u32 array carrying the stage's push constants (current convention)
    push_constants: &[u32],
) -> Result<ScopedDescriptorSets<'a>, Box<dyn std::error::Error>>
```

The helper returns its `ScopedDescriptorSets` RAII guard. The orchestrator must
hold the guard until the command buffer is submitted **and** the fence has
signalled — only then is it safe to release the descriptor set back to the
bounded pool. The current orchestrator already holds all guards as locals until
the end of the method scope; the pattern is unchanged.

### Per-stage commit order

Nine commits in this sequence:

1. **SAD stage.** 2 image inputs, 1 buffer output, push = `[w, h, cols]`. First
   commit validates the pattern. Only `process_frame_with_imported` calls it
   (SAD never runs on the first frame).
2. **NV12 stage.** 1 image input, 1 buffer output, push = `[w, h, y_stride,
   uv_offset, uv_stride]`. Called from both orchestrators — first commit that
   touches both.
3. **Analysis stage.** Same shape as SAD; called from both.
4. **palrle_compact stage.** 3 buffers in, 2 buffers out, push = `[cols, rows,
   dirty_threshold]`.
5. **palrle_indirect_args stage.** 1 buffer in, 1 buffer out, no push.
6. **palette_fold stage.** 6 buffers in/out, no push.
7. **palette_subset_fold_init stage.** 1 buffer, no push (smallest helper).
8. **palette_subset_fold stage.** 2 buffers, no push.
9. **pal_rle_index stage.** 1 image + 5 buffers, push = `[cols]`.

**Why this order.** Simplest first (SAD is the cleanest example), then
NV12+Analysis to validate the helper template across BOTH orchestrators, then
the palrle pipeline (compact → indirect_args → index), then the palette pipeline
(fold → subset_fold_init → subset_fold). Each commit is independently
revertible.

### Per-commit verification

```
cargo build --package ghostframe-lib --lib 2>&1 | tail -3
cargo test --lib --package ghostframe-lib capture::gpu_pipeline -- --test-threads=1
cargo test --workspace --lib -- --test-threads=1 | grep "^test result"
```

Expected: clean build, 18 GPU tests pass, 282 total pass.

### Behavior invariants preserved per commit

Call out in each commit message:

- Same descriptor-set allocations from the same pool.
- Same `update_descriptor_sets` writes (same bindings, same buffer/image
  attachments, same offsets).
- Same `cmd_bind_pipeline` / `cmd_bind_descriptor_sets` / `cmd_push_constants`
  arguments.
- Same `cmd_dispatch(cmd, x, y, z)` workgroup counts.
- **Barriers stay in the orchestrator** — they are inter-stage state transitions,
  not per-stage things, and moving them would change ordering semantics.

### First-frame duplication payoff

After all 9 stages are extracted, `run_first_frame_passes` and
`process_frame_with_imported` both call the same per-stage helpers. The
duplication the audit specifically called out ("the same per-stage descriptor +
barrier blocks repeated 9 times BOTH within process_frame_with_imported AND
again in run_first_frame_passes") disappears. The two orchestrators become a
sequence of `self.run_*_stage(...)` calls with barriers between, differing only
in which stages they run.

### Estimated `frame.rs` size after Phase 2

Before Phase 2: ≈ 2400 lines. After all 9 helpers extracted: ≈ 1300-1500 lines.
The 9 helpers themselves total ~600 lines but replace ~2700 lines of inline
boilerplate. Net reduction ≈ 900-1100 lines.

### Commit message template

```
refactor(capture): extract run_<stage>_stage helper

Extract the <stage> compute dispatch (allocate descriptor set, write
descriptors, bind pipeline + push constants, dispatch) from
process_frame_with_imported [and run_first_frame_passes, if applicable]
into a dedicated method on GpuFrameProcessor.

Behavior invariants verified: same descriptor pool, same descriptor
writes, same push constants, same workgroup counts. Barriers between
stages remain in the orchestrators.

frame.rs net delta: -<N> lines.
```

## Testing strategy

### Test surfaces

| Suite | Count | Per-commit gate? | When to run |
|---|---|---|---|
| `cargo test --workspace --lib` | 282 | yes | every commit |
| `cargo test --lib capture::gpu_pipeline` | 18 | yes | every commit |
| `cargo test --test gpu_pipeline` (integration) | 5 | yes (cheap) | every commit |
| `cargo test --test e2e` | 24 + 2 ignored | no (~10 min) | end of each phase |
| `vitest` (web client) | 27 | not affected | sanity-check at end |

### Per-commit gate (Phase 1 and every Phase 2 commit)

```
cargo build --package ghostframe-lib --tests 2>&1 | tail -3
cargo test --lib --package ghostframe-lib capture::gpu_pipeline -- --test-threads=1
cargo test --workspace --lib -- --test-threads=1 | grep "^test result"
cargo clippy --package ghostframe-lib --lib 2>&1 | grep -c "too_many_arguments"   # still 0
```

### End-of-phase gate

```
cargo test --workspace --tests -- --test-threads=1
```

Full suite including e2e (~10 min). Run before declaring each phase done.

### Coverage gaps to acknowledge

1. The 18 unit tests split into two groups: ~3 pure-layout tests (e.g.
   `tile_analysis_struct_has_expected_layout`,
   `frame_analysis_tile_analysis_slice_returns_correct_range`,
   `frame_palette_entry_raw_has_expected_layout`) that run unconditionally;
   and ~15 GPU-dependent tests that gracefully *skip* (rather than fail) when
   no Vulkan device is present. Developer-machine runs cover all 18; a CI
   runner without a GPU passes the layout tests and silently skips the rest.
   For this refactor the developer running the commits is the gate — CI alone
   is **not** sufficient to catch barrier or descriptor-write regressions.
2. The e2e suite exercises the full GPU pipeline in a vkms-backed container
   with vkms-backed DRM — this is where per-stage barrier mis-ordering or
   descriptor-write regression would surface. Mandatory at end-of-phase.
3. `Drop` is implicitly exercised by every test that constructs and releases a
   `GpuFrameProcessor`. A Drop regression typically surfaces as a Vulkan
   validation error in the next test's setup, not as a quiet leak. No explicit
   Drop tests added.
4. **Untested:** resolution changes (`e2e_resolution_change` is `#[ignore]`'d —
   pre-existing) and `palrle_session_reset` (`#[ignore]`'d — M3.5 scope). Both
   out of scope for this refactor.

### Behavior-preservation discipline per Phase 2 commit

Beyond "tests pass," each per-stage extraction commit should be reviewable by
**eyeballing that the extracted helper does the SAME sequence of
`update_descriptor_sets` + `cmd_bind_*` + `cmd_dispatch` calls** as the
pre-extraction inline block. The most likely failure mode is a subtle reorder
of two descriptor-set writes that happens to compile and pass tests on the dev
machine but introduces a race on a different GPU vendor. Review the diff
side-by-side with the pre-extraction code; verify the call order is identical.

### Bisect strategy

Because each per-stage extraction is its own commit, `git bisect` over the
Phase 2 range identifies which stage's extraction broke things — usually with
`cargo test --lib capture::gpu_pipeline` as the bisect predicate. With ~9
commits in Phase 2, bisect resolves in 4 iterations.

## Risks

1. **Vulkan validation regressions on hardware we don't test on.** A barrier or
   descriptor-write reorder that's harmless on AMD/Intel/Mesa could surface on
   an untested vendor (NVIDIA, ANGLE, MoltenVK). Mitigation: per-stage
   extraction with byte-identical bind/dispatch order keeps the risk minimal;
   the e2e suite catches the most common shape of issue.
2. **Descriptor-pool exhaustion.** If a per-stage helper accidentally allocates
   an extra descriptor set per call (e.g., due to a leaked guard or copy-paste
   error), the pool would exhaust on a later frame. Mitigation: each helper
   allocates exactly one DS and returns one guard; orchestrator collects them
   in named locals. Reviewable in the diff.
3. **Drop ordering.** The single `Drop` body in mod.rs destroys resources in a
   specific order to satisfy Vulkan validation. Phase 1's split doesn't move
   `Drop`. Per-stage extraction creates new methods that hold resource
   references — if any leak a buffer or memory handle in their guard's drop
   path, validation errors fire on next teardown. Mitigation: `Drop` body is
   unchanged across the refactor; per-stage helpers don't create new owned
   resources.
4. **Phase 1's byte-identical relocation doesn't catch logical bugs already
   present.** By design — we want this refactor to be a pure structural change.
   Latent bugs in `process_frame_with_imported` ride along unchanged.
5. **Bisect cost on regressions discovered weeks later.** The directory rename
   creates a noisy diff for any bisect that crosses it. Mitigation: Phase 1
   lands separately so the rename diff is bounded; subsequent bisect works
   against the post-rename layout.

## Rollback strategy

Built into the phasing.

- **Phase 1 regression:** `git revert` the single split commit. The repo
  returns to the pre-Phase-1 state with `gpu_pipeline.rs` as a single file.
  No follow-on cleanup needed.
- **One Phase 2 commit regresses:** `git revert` just that commit. Directory
  structure stays; only that one stage's helper is rolled back to inline form.
  Subsequent stage extractions continue from there once the issue is understood.
- **Phase 2 fundamentally doesn't work** (unlikely given Phase 1's dry run):
  `git revert` all Phase 2 commits, keep Phase 1's split. Repo ends in the
  state described at the end of Phase 1 — clean directory split, no per-stage
  helpers. Item 5's bigger goal isn't met, but no regression either.

## Open implementation decisions

Pick these consistently when extracting the first helper (SAD); subsequent
helpers follow.

- Per-stage helper input parameter ordering (alphabetical? by shader binding
  order? by source — from-self vs from-orchestrator?). Suggested: from-self
  first (so callers read the orchestrator-side inputs at the call site).
- Whether each helper takes `descriptor_pool: vk::DescriptorPool` as an arg or
  reads `self.descriptor_pool` directly. Suggested: read from `self` (struct
  field), drop the parameter from the template.
- Whether to introduce a private `enum PalRleStage` discriminator for the
  related palrle methods. Suggested: no — simpler to keep them as independent
  methods.

## Out of scope

- `io_bridge.rs` decomposition (separate spec, sequential).
- Restructuring `Drop`'s 190-line body.
- Adding/removing GPU stages or changing workgroup counts.
- Changing push-constant struct shapes from `&[u32]` to typed structs.
- Tuning `max_sets` on the descriptor pool.
- Splitting `process_frame_with_imported` into multiple orchestrators.
- Documentation rewrites beyond what new methods require.
- Test reorganization beyond the file relocation.

## Done definition

- `ghostframe-lib/src/capture/gpu_pipeline.rs` no longer exists.
- `ghostframe-lib/src/capture/gpu_pipeline/{mod,setup,frame,tests}.rs` all exist
  and match the contents described in "Target structure."
- All 9 per-stage helpers exist on `impl GpuFrameProcessor` in `frame.rs`.
- `process_frame_with_imported` and `run_first_frame_passes` each consist of a
  command-buffer setup section followed by a sequence of `self.run_*_stage(...)`
  calls with `cmd_pipeline_barrier` between them, plus a submit + wait section.
- 282 lib + 24 e2e tests pass.
- No new `#[allow(clippy::too_many_arguments)]` annotations introduced.
- `frame.rs` ≤ 1500 lines.
