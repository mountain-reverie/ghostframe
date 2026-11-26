# UF: unsafe and FFI soundness

Re-verified against the worktree at `origin/master` **e85fb005**. All line
numbers below are at that commit. Nothing here was built or run; the counts in
UF-8 come from a grep/AST-free script (method in UF-8), and an implementer should
take the exact list of sites from clippy itself before committing.

Cross-group ownership:

- **EC** owns the `[workspace.lints]` table (none exists at e85fb005: no
  `[workspace.lints]` in `Cargo.toml`, no crate has a `[lints]` table, and every
  crate is `edition = "2021"`). UF owns the per-crate burn-down and the SAFETY
  comment work (UF-8). The hand-over is spelled out in UF-8.
- **LD** may also refactor `ghostframe-lib/src/transport/io_bridge/frame_backend.rs`
  and `ghostframe-lib/src/capture/gpu_pipeline/`. UF-1, UF-4 and UF-5 touch
  both. See "Suggested PRs for UF" for the order.

---

## UF-1: `FrameAnalysis` hands out safe slices over raw pointers nothing ties to the processor (Medium)

### Status at e85fb005: CONFIRMED

- `ghostframe-lib/src/capture/gpu_pipeline/mod.rs:107-156`: `pub struct
  FrameAnalysis` has seven `pub` raw-pointer fields (`nv12_data`,
  `tile_analysis`, `palrle_compact_list`, `frame_palette_set`,
  `per_tile_frame_palette_id`, `folded_into`, `index_buffer`) and `pub` lengths.
- `mod.rs:164-259`: **safe** `pub fn`s (`tile_analysis_slice`,
  `palrle_compact_list_slice`, `frame_palette_set_slice`,
  `per_tile_frame_palette_id_slice`, `folded_into_slice`,
  `index_buffer_slice_for`) call `std::slice::from_raw_parts` on those fields.
  The SAFETY comments state a contract ("the caller must not hold this
  FrameAnalysis across a subsequent process_frame() call") that the type does
  not enforce.
- `mod.rs:261-267`: `unsafe impl Send for FrameAnalysis {}`, justified by
  "consumed before the next `process_frame` call ... used from a single tokio
  task". Both halves are convention.
- `gpu_pipeline` is `pub mod` (`capture/mod.rs:3`, `lib.rs:17`), so from safe
  code in any crate: build a `FrameAnalysis` with arbitrary pointers; or keep
  one, drop the `GpuFrameProcessor`, call an accessor; or keep one across a
  second `process_frame*` at a new size, where `ensure_nv12_buffer`
  (`gpu_pipeline/frame.rs:165-180`) unmaps and frees the NV12 buffer the old
  `nv12_data` points into. Each is UB reached without `unsafe`.
- Related, same shape: `GpuFrameProcessor` has `pub` raw-pointer fields
  `cdf53_compact_list_ptr` (`mod.rs:321`), `cdf53_compact_count_ptr` (`:326`),
  `cdf53_coefficients_ptr` (`:342`), `cdf53_escalation_coefficients_ptr`
  (`:355`). Dereferencing those needs `unsafe` at the use site, so they are not
  a hole in themselves, but they are the same "valid until the next
  process_frame" contract, read by hand in `frame_backend.rs:674-736`.
- Hand-built instances: `io_bridge/tests.rs:5707`, `gpu_pipeline/tests.rs:207`
  and `:2583` (heap-`Vec`-backed, to test the accessors without Vulkan).
- In-crate readers that go around the accessors:
  `io_bridge/frame.rs:66-78` (`palrle_index_slice`, its own
  `from_raw_parts` over `analysis.index_buffer`) and
  `frame_backend.rs:574-580` (the NV12 slice for H.264).

Production obeys the contract today, which is why this is Medium and not High:
`GpuBackend::analyse` clears `held` before it calls the processor
(`frame_backend.rs:482`), and `finish_*` `take()`s it.

### How the data actually flows (this decides the design)

- `GpuBackend` (`frame_backend.rs:438-446`) owns **both** `processor:
  GpuFrameProcessor` and `held: Option<FrameAnalysis>`, in the same struct.
- The whole `GpuBackend` is moved once into a dedicated OS thread
  (`frame_worker.rs`, module docs lines 6-30; `FrameBackend: Send`,
  `frame_backend.rs:275`). `FrameAnalysis` itself never crosses a channel: only
  the owned `Analysed` does (`frame_backend.rs:71-83`).
- Between `ToWorker::Analyse` and `ToWorker::Finish`, the worker applies queued
  `Control`s with `&mut` access to the processor (`frame_worker.rs:381-382`:
  `InvalidateBaseline` destroys `prev_image` after `device_wait_idle`,
  `ForgetDamage`), and a `ToWorker::Forward` can run `forward_cdf53_tiles`
  (`&mut self`). None of those touch the analysis buffers, but all of them
  need `&mut GpuFrameProcessor` while the analysis is held.
- `finish_tile` reads the analysis, then calls
  `self.processor.forward_cdf53_tiles(&refs)` (`&mut`) at `frame_backend.rs:768`
  after it is done reading.

### Design

**Chosen: a pointer-free, generation-checked `FrameAnalysis` plus a borrowed
`FrameOutputs<'p>` view obtained from the processor.**

1. `FrameAnalysis` keeps only plain data: `dirty_tiles`, the NV12 geometry
   (`nv12_width/height/y_stride/uv_stride/uv_offset`), `tile_analysis_len`,
   `palrle_compact_count`, `frame_palette_set_count` (all may stay `pub`), plus
   one **private** field `token: FrameToken { processor_id: u64, generation:
   u64, escalation_count: u32 }`. No raw pointers, so it is `Send` and `Sync`
   automatically and `unsafe impl Send for FrameAnalysis` is deleted. The private
   field makes a struct literal impossible outside `gpu_pipeline` (forgery
   closed); inside the crate a forged token is harmless (point 3).
2. `GpuFrameProcessor` gains `id: u64` (from a process-wide `AtomicU64`
   counter in `new`) and `generation: u64`. **Every** entry that can reallocate
   or rewrite the output buffers bumps `generation` first, before any fallible
   step: `process_frame`, `process_frame_from_pixels`,
   `process_frame_from_pixels_damaged`, `process_frame_unchanged`, `diff`
   (i.e. the `process_frame_inner*` family in `frame.rs:329`, `:418`, `:440`,
   `:576`). A failed frame therefore also invalidates the previous analysis
   (its NV12 buffer may already have been freed by `ensure_nv12_buffer`).
   `invalidate_baseline`, `forget_damage`, `set_escalation_candidates` and
   `forward_cdf53_tiles` do not bump it (they do not touch the analysis or
   escalation output buffers; `forward_cdf53_tiles` has its own buffers, per
   the comment at `mod.rs:357-362`).
3. `GpuFrameProcessor::outputs(&self, a: &FrameAnalysis) ->
   Result<FrameOutputs<'_>, StaleAnalysis>` checks `a.token` against
   `(self.id, self.generation)` and builds the view. All slice **lengths come
   from the processor's own allocations** (the `HostBuffer` capacities sized
   from `max_tiles` in `setup.rs`, `PALETTE_HASH_SLOTS`, the `NV12Buffer.size`,
   `MAX_ESCALATION_PER_FRAME * CDF53_TOTAL_COEFFS`), with the analysis's counts
   only ever *clamped* to them. That is the property that makes a forged or
   stale-but-matching token unable to cause an out-of-bounds read. This is the
   single `unsafe` site in the module (one `from_raw_parts` per buffer, one
   SAFETY comment each).
4. `FrameOutputs<'p>` holds only `&'p [T]` slices (no pointers) and carries
   today's accessor names so call sites change little:
   `tile_analysis_slice`, `palrle_compact_list_slice`,
   `per_tile_frame_palette_id_slice`, `frame_palette_set_slice`,
   `folded_into_slice`, `index_buffer_slice_for(c)`, plus the bulk
   `palrle_index(tiles)` that replaces `io_bridge/frame.rs::palrle_index_slice`,
   `nv12() -> Option<&'p [u8]>` (the span `frame_backend.rs:574-580` computes
   by hand), `cdf53_compact_list() -> &'p [u32]` (count read from the mapped
   count word and clamped), `cdf53_coefficients_for(slot) -> Option<&'p [i32]>`
   and `escalation_coefficients_for(slot) -> Option<&'p [i32]>` (bounded by
   `token.escalation_count`; this is UF-4's fix). Because the view borrows
   `&self`, no `&mut self` method (a new frame, a control, a forward) can run
   while it is alive: the use-after-free is now a borrow-check error, not a
   convention.
5. The element types handed out as slices (`u8`, `u32`, `i32`,
   `TileAnalysis`, `FramePaletteEntryRaw`) are `#[repr(C)]` aggregates of
   integers, so every bit pattern the GPU writes is a valid value. State that
   in the SAFETY comment; adding a non-POD type to the view later would need a
   new argument.
6. A `#[cfg(test)] pub(crate) fn FrameOutputs::from_slices(..)` (safe: it
   takes slices) replaces the three hand-built `FrameAnalysis` fixtures, so the
   clamp tests keep running without Vulkan.
7. The four `pub` `cdf53_*_ptr` fields on `GpuFrameProcessor` become
   `pub(super)` (only `FrameOutputs` construction and the in-module GPU tests
   read them).

`GpuBackend` keeps `held: Option<FrameAnalysis>` unchanged in shape: it is now
plain data, sitting next to the processor without borrowing it. `finish_*` do
`let out = self.processor.outputs(&analysis)` and, on `Err(StaleAnalysis)`,
`debug_assert!` and return `H264Reply::Failed` / the empty `TileOut` (the loop
already handles both). In `finish_tile`, `out` is last used before
`forward_cdf53_tiles` (NLL ends the borrow); in `finish_h264_planned`,
`self.processor.outputs(..)` and `self.direct.as_mut()` are disjoint field
borrows in one body.

**Rejected:**

- *Lifetime-bound `FrameAnalysis<'p>`* (borrowing the processor). It cannot be
  stored in `GpuBackend::held` next to the processor it borrows (a
  self-referential struct), and the worker must apply `&mut` controls and
  forwards between `Analyse` and `Finish`, which a live `&'p` borrow forbids.
  Making it work would mean merging `analyse` and `finish_*` into one call,
  which the module docs (`frame_backend.rs:21-22`) rule out: the loop
  classifies between them.
- *`unsafe fn` accessors.* Honest about the contract but enforces nothing,
  keeps forgeable pointer fields, and moves `unsafe` into every caller
  (`frame_backend.rs`, `io_bridge/frame.rs`, ~50 test sites in
  `gpu_pipeline/tests.rs`), which is the opposite direction from UF-8.
- *Owning copies at `analyse` time.* Sound and simple, but on the hot path:
  the NV12 copy alone is ~3 MB per frame at 1080p and ~12 MB at 4K for every
  H.264 frame, and the PalRle index buffer is up to 8 MB (16384 x 512 B). The
  tile-dispatch latency target (well under 15 ms at 4K and dual-screen) does
  not leave room for it. `finish_tile` already copies the PalRle inputs it
  needs into `OwnedPalRle`; that stays as is.

### Files touched

`ghostframe-lib/src/capture/gpu_pipeline/{mod.rs, frame.rs, setup.rs, tests.rs,
ingest_tests.rs, damage_tests.rs, fallback_tests.rs, cdf53_forward_gate_tests.rs,
readback_bench.rs}`, `ghostframe-lib/src/transport/io_bridge/{frame_backend.rs,
frame.rs, tests.rs}`, `ghostframe-lib/tests/gpu_pipeline.rs`.

### Implementation steps

1. In `mod.rs`, add `struct FrameToken` (private fields), `pub struct
   StaleAnalysis` (`Debug`, `Display`, `std::error::Error`), and replace the
   pointer fields of `FrameAnalysis` with `token: FrameToken`. Delete
   `unsafe impl Send for FrameAnalysis` and the six accessors.
2. Add `id` and `generation` to `GpuFrameProcessor`; initialise in
   `setup.rs::new_inner`; `id` from a `static NEXT_PROCESSOR_ID: AtomicU64`.
3. In each `process_frame_inner*` in `frame.rs`, bump `self.generation` as the
   first statement after the test lock. Record the escalation count the frame
   actually dispatched (`escalation_count_this_frame`, `frame.rs:341`; check
   each variant: the pixel and unchanged variants must record 0 if they do not
   dispatch the escalation forward). Build `FrameAnalysis` at the two literal
   sites (`frame.rs:760`, `:1026`, and whatever `process_frame_inner_unchanged`
   returns) with the token instead of pointers. Keep the
   `clamp_gpu_count(*self.palrle_compact_count.ptr, ..)` snapshot read there.
4. Write `FrameOutputs<'p>` and `GpuFrameProcessor::outputs` in `mod.rs`. Check
   in `setup.rs` whether any of the `HostBuffer` pointers can be null at
   construction (the doc comments at `mod.rs:148` and `:153` say "Null on the
   first frame"; if the buffers are always allocated, drop the `Option`s and
   correct the comments). Each `from_raw_parts` gets a SAFETY comment naming:
   the allocation and its capacity, that the GPU has been fenced (every
   `process_frame*` returns after `submit_frame_cmd_and_wait`), that `&self`
   excludes the next submit, and the POD argument from design point 5.
5. Add `FrameOutputs::from_slices` under `#[cfg(test)]`.
6. Rewrite `frame_backend.rs` `analyse` (it reads `tile_analysis_slice` for
   `dirty_analysis` and `GpuDiag`; `tile_analysis_is_null` becomes
   `tile_analysis_slice().is_empty()`), `finish_h264_planned` (use
   `out.nv12()`, delete the `unsafe` at `:574`) and `finish_tile` (use
   `out.cdf53_compact_list()`, `out.cdf53_coefficients_for(slot)`,
   `out.escalation_coefficients_for(slot)`; delete the four `unsafe` blocks at
   `:674`, `:676`, `:685`, `:728`).
7. Replace `io_bridge/frame.rs::palrle_index_slice` with
   `FrameOutputs::palrle_index(tiles)`; keep its doc's overflow rationale.
8. Make the `cdf53_*_ptr` fields `pub(super)`; port `tests.rs`,
   `cdf53_forward_gate_tests.rs`, `readback_bench.rs` to the view (they are in
   the module, so `pub(super)` still reaches them if a test needs the raw
   pointer).
9. Port the three hand-built fixtures to `from_slices`
   (`io_bridge/tests.rs:5707`, `gpu_pipeline/tests.rs:207`, `:2583`).
10. Rewrite the `unsafe impl Send for GpuFrameProcessor` comment
    (`mod.rs:646-648`, today "used from a single tokio task", which is stale:
    it is a dedicated OS thread) as a real SAFETY argument: Vulkan handles may
    be used from any thread under external synchronisation; every use goes
    through `&mut self`, or `&self` on a type that is `!Sync` because of
    `frame_fence_pending: Cell<bool>` (`mod.rs:292`), so there is never
    concurrent access; the mapped pointers point into allocations the
    processor owns and frees in `Drop`.

Sketch (signatures to be created, not existing ones):

```rust
// sketch
pub struct FrameAnalysis {
    pub dirty_tiles: Vec<u32>,
    pub nv12_width: u32, /* ... other geometry and counts, all plain ... */
    token: FrameToken,
}
impl GpuFrameProcessor {
    pub fn outputs(&self, a: &FrameAnalysis) -> Result<FrameOutputs<'_>, StaleAnalysis>;
}
pub struct FrameOutputs<'p> { tile_analysis: &'p [TileAnalysis], /* ... */ }
```

### Tests

New, non-GPU (run in CI's `cargo test --workspace --lib`):

- `gpu_pipeline::tests::frame_outputs_clamp_an_oversized_gpu_count` (port of
  `tests.rs:2583`): `from_slices` with 4 tiles of backing and a count of
  1_000_000_000; every accessor returns at most the backing length;
  `index_buffer_slice_for(4)` and `(900_000_000)` are empty. Property: no
  accessor's length exceeds its backing slice, for a sweep of counts
  `0..=8` and `u32::MAX`.
- `gpu_pipeline::tests::a_token_from_another_processor_or_generation_is_stale`:
  a pure `fn token_matches(expected: (u64, u64), got: &FrameToken) -> bool` (or
  the check factored out of `outputs`) over the four combinations.
- Port of `io_bridge/tests.rs:5707`
  (`an_out_of_range_palrle_count_is_clamped_before_it_sizes_the_index_slice`)
  onto `FrameOutputs::palrle_index`.

New, GPU (serialised: `process_frame*` already takes `gpu_test_lock`; run under
`gf-gpu-run`; add the names to CI's `--skip` list on `ci.yml:141` the way the
other GPU unit tests are):

- `gpu_pipeline::tests::outputs_of_a_superseded_frame_are_refused`: two
  `process_frame_from_pixels` calls; `outputs(&first)` is
  `Err(StaleAnalysis)`, `outputs(&second)` is `Ok`.
- `..::outputs_after_a_failed_frame_are_refused`: a good frame, then one that
  fails validation (`stride < width*4`), then `outputs(&good)` is `Err`.
- `..::outputs_of_another_processor_are_refused`: two processors (each
  `GpuFrameProcessor::new(256)`), analysis from A, `B.outputs(&a)` is `Err`.

Type-level (compile-fail) checks, as rustdoc `compile_fail` blocks on
`FrameOutputs` (compile only, no GPU needed):

1. Holding a view across a new frame:
   `let o = p.outputs(&a)?; p.process_frame_from_pixels(..)?; o.tile_analysis_slice();`
   must fail (E0502).
2. Struct literal of `FrameAnalysis` from outside the crate (private `token`)
   must fail (E0451 / E0063).

Each `compile_fail` block gets an adjacent `no_run` twin that is the same code
minus the offending line, so a typo cannot make the `compile_fail` pass
vacuously. **CI never runs doctests** (`test-unit` is `--lib` + `--bins`;
`ci.yml:141`/`:161` likewise), so this PR adds one CI step and the same line to
`just ci-local`: `cargo test -p ghostframe-lib --doc gpu_pipeline` (filtered,
so the never-run doc examples elsewhere in the crate do not become this PR's
problem).

Mutation checks:

- Remove the `generation` bump from `process_frame_inner_from_pixels`: the
  "superseded" GPU test must fail.
- Change `outputs` to take `&mut self` *and* return `FrameOutputs<'static>` via
  a transmute in a scratch branch: compile_fail 1 must start compiling (the
  doctest goes red).
- Size `index_buffer_slice_for` from the analysis's count instead of the
  backing capacity: the clamp test must fail.

### Gates

`just lint`; `just test-unit` under `gf-gpu-run`; `cargo test -p ghostframe-lib
--doc gpu_pipeline`; `cargo test -p ghostframe-lib --test gpu_pipeline` on a
GPU host (`gf-gpu-run`); browserless suite (frame path changed); cbindgen
header diff (no change expected: `FrameAnalysis`/`TileAnalysis` do not appear
in `ghostframe-lib/include/ghostframe.h`); `just containers-build` before any
e2e.

### Risks

- A missed `process_frame*` variant that does not bump `generation` keeps one
  path on the old convention. Mitigation: bump inside a single private helper
  every variant calls first, and the failed-frame GPU test.
- `outputs` adds a check per `finish_*`; negligible (two integer compares).
- LD's refactor of `frame_backend.rs` / `gpu_pipeline` conflicts textually.
- **Size: M.**

---

## UF-2: `ExportedImage::drop` frees memory the GPU may still be writing (Low)

### Status at e85fb005: CONFIRMED

`ghostframe-client-gpu/src/export.rs:885-901`:
`let _ = crate::dmabuf::dma_buf_wait_write_idle(self.fd.as_raw_fd(), 10_000);`
discards both `Ok(false)` (the 10 s timeout, `dmabuf.rs:204`) and `Err`
(`dmabuf.rs:211`), then `destroy_image` + `free_memory` unconditionally. The
comment above it says exactly why that is unsafe ("Freeing the memory under it
would write through a dangling mapping"). Only the Vulkan `ExportedImage` has
this; the GLES one (`export_gles.rs:179`) tears down from wgpu's drop callback
and has no `Drop`.

### Root cause

The wait's result was treated as advisory; the one case it exists for
(GPU still writing) falls through to the free.

### Design

**Chosen: leak on anything but a confirmed idle.** If the wait returns
`Ok(true)`, destroy and free as today. On `Ok(false)` or `Err`, do **not**
destroy the image or free the memory; log one `warn!` (with the error, the size
and `modifier`) and bump a process-wide `AtomicU64` `EXPORTS_LEAKED_BUSY`. The
`OwnedFd` still closes (closing the exported fd does not release the memory; the
`VkDeviceMemory` keeps its own reference). A 10 s GPU hang means the device is
very likely lost already; a bounded leak of one export buffer is the sound
choice.

Factor the decision out as a pure `fn release_after_wait(r: &Result<bool,
GpuError>) -> Release { Destroy, Leak }` so it is testable without a GPU.

**Rejected:**

- *`device_wait_idle()` as a fallback.* Vulkan requires host access to every
  `VkQueue` of the device to be externally synchronised during
  `vkDeviceWaitIdle`; wgpu owns the queue and may be submitting from another
  thread. That is a spec violation, and an unbounded wait on a wedged GPU.
- *Retry with an infinite poll timeout.* Hangs drop (and process exit) on a
  wedged GPU.
- *Keep today's behaviour and document it.* It is the UB the comment
  describes.

### Files touched

`ghostframe-client-gpu/src/export.rs` (and, if the counter is exposed,
`ghostframe-client-gpu/src/lib.rs`).

### Implementation steps

1. Add `enum Release { Destroy, Leak }` and `fn release_after_wait(..)` in
   `export.rs` (private, `#[cfg_attr(test, ..)]` not needed).
2. In `Drop`, match on it; keep the existing SAFETY comment on the `Destroy`
   arm and add one on why skipping is sound (nothing else owns `image` /
   `memory`; leaking Vulkan objects is not UB).
3. Add the warn line and the counter, with `pub fn exports_leaked_busy() ->
   u64` for tests and the render thread's `1s summary` if wanted.

### Tests

- `export::tests::only_a_confirmed_idle_destroys`: `Ok(true)` gives `Destroy`;
  `Ok(false)` and `Err(GpuError::Io(..))` give `Leak`. Property: `Destroy` iff
  `Ok(true)`.
- Mutation: make `Ok(false)` return `Destroy`; the test must fail.
- No GPU test can force a 10 s write hang deterministically; do not add one
  that "passes" without reaching the timeout.

### Gates

`just test-client` (clippy `--all-targets` + unit tests, client crates); the
Vulkan feature build (`just build-client`). No cbindgen impact. GPU-backed
client tests, if run, serialised.

### Risks

A leak per hung teardown; logged and counted. **Size: S.**

---

## UF-3: GStreamer CPU download bounds-checks only the chroma plane (Low)

### Status at e85fb005: CONFIRMED

`ghostframe-client-h264/src/decoder_gst.rs:143-197` (`HwFrame::download_nv12`):
the only bound is `end = offsets[1] + uv_stride * chroma_h` against `size`
(`:158-166`). The luma loop (`:185-190`) reads `offsets[0] + row * y_stride ..
+ w` for `row < h` with no check that `offsets[0] + y_stride*(h-1) + w <= size`,
nor that the luma plane precedes and does not overlap the chroma plane. The
per-row SAFETY comment ("bounds checked against `l.size` above") is false for
luma. The arithmetic is also unchecked `u64 as usize` multiply-add.

### Root cause

The check was written for the common layout (Y at 0, UV after Y), where the
chroma end implies the luma end. A driver-reported layout with a large
`offsets[0]` or `y_stride` breaks that implication, and the values come from
GStreamer's video meta, not from this crate.

### Design

**Chosen:** a pure, backend-neutral `fn nv12_plane_spans(width, height,
offsets: [u64; 2], strides: [u64; 2], size: u64) -> Result<[Range<usize>; 2],
LayoutError>` in `ghostframe-client-h264/src/descriptor.rs` (that module is
compiled in every feature set, so its tests run in the default `vaapi` build
CI uses; `decoder_gst.rs` is only compiled with `gstreamer-backend`). It
rejects zero width/height, `stride < width` (moved from `:148`), and uses
checked arithmetic for each plane's last byte
`offset + stride*(rows-1) + width`; both must be `<= size`; and the luma range
must end at or before the chroma range starts (a layout whose planes overlap is
not NV12, and the oracle would compare garbage; an overlapping read-only map is
not UB, so this is a correctness check riding along).

`download_nv12` then maps once, wraps the mapping in a small RAII guard that
`munmap`s on drop, forms **one** `&[u8]` of `size` bytes over it (the only
`unsafe`, one SAFETY comment), and copies rows by safe slice indexing. The
per-row `from_raw_parts` calls disappear, so an off-by-one becomes a panic, not
a wild read.

**Rejected:** adding a second `if luma_end > size` next to the existing check.
It fixes this instance but leaves unchecked overflow and per-row `unsafe` with
a comment that has to be kept true by hand.

### Files touched

`ghostframe-client-h264/src/descriptor.rs`, `ghostframe-client-h264/src/decoder_gst.rs`.

### Implementation steps

1. Add `LayoutError` (or reuse `H264Error` with a variant) and
   `nv12_plane_spans` to `descriptor.rs`.
2. In `download_nv12`, replace `:146-166` with a call to it; replace the
   `mmap`/loop/`munmap` with the guard plus one slice.
3. Update the doc comment of `download_nv12` to say the layout is validated.

### Tests

- `descriptor::tests::plane_spans_match_a_brute_force_oracle`: exhaustive sweep
  over small values (width, height in 1..=6; offsets in 0..=64 step 4; strides
  in width..=width+8; size in 0..=128). Oracle: compute every row's byte range
  for both planes naively in `u128`; `Ok` iff every range is inside `0..size`
  and the luma ranges all end before the first chroma byte. Assert agreement
  on every case, and assert the sweep produced both `Ok` and `Err` outcomes
  (prove it discriminates), including at least one case where chroma fits and
  luma does not (the exact bug).
- `..::plane_spans_reject_overflow`: strides/offsets near `u64::MAX` return
  `Err`, do not panic.
- Mutation: delete the luma check; the oracle test must fail on the
  "chroma fits, luma does not" cases.
- The existing GStreamer oracle (`oracle_tests_gst.rs`) keeps exercising the
  real download on an RK3399 host; it is not a CI gate.

### Gates

`just test-client`; `cargo clippy -p ghostframe-client-h264 --no-default-features
--features gstreamer-backend --all-targets -- -D warnings` (Justfile `:100-101`;
the workspace clippy never compiles `decoder_gst.rs`).

### Risks

A real driver whose layout puts chroma before luma would now be refused (and
fall back to the dmabuf import path, which does not use this function). None
known; rkvdec reports Y at 0. **Size: S.**

---

## UF-4: escalation coefficient read is bounded only by an upstream truncation (Low)

### Status at e85fb005: CONFIRMED

- `ghostframe-lib/src/transport/io_bridge/frame_backend.rs:720-736`:
  `for (slot, s) in req.escalation.iter().enumerate()` reads
  `gpu_coeffs_ptr.add(slot * CDF53_TOTAL_COEFFS)` for `CDF53_TOTAL_COEFFS`
  i32s. The buffer holds `MAX_ESCALATION_PER_FRAME` (512) slots
  (`setup.rs:796`; `mod.rs:62`).
- The only bound is the loop's: `collect_escalation_candidates` truncates to
  512 (`io_bridge/frame.rs:1421`), stores the list, and `tile_finish_action`
  (`frame.rs:1179-1196`) builds `req.escalation` from it. The processor also
  truncates on its side (`set_escalation_candidates`, `mod.rs:773`), but
  `finish_tile` does not consult that.
- A second, silent dependency: slots past what the GPU actually dispatched
  this frame hold the previous frame's coefficients (the count is consumed and
  reset in `process_frame_inner`, `frame.rs:338-343`). Equal lengths today,
  not enforced.

### Root cause

The bound lives in the transport loop; the read lives in the backend; nothing
in the type of `TileRequest::escalation` (`frame_backend.rs:133`) carries it.

### Design

**Chosen: fold into UF-1.** `FrameOutputs::escalation_coefficients_for(slot)`
returns `None` for `slot >= token.escalation_count` (what the GPU dispatched,
which is `<= MAX_ESCALATION_PER_FRAME` by `set_escalation_candidates`) and
slices from a buffer whose length is the allocation's. `finish_tile` pushes
`None` for such a slot (same as a skipped slot), with a `debug_assert!` that
`req.escalation.len() <= dispatched`, and the loop already treats `None` as
"not encoded". Out of bounds becomes impossible, and the stale-slot case
becomes visible in debug builds.

**Rejected:** an `assert!(req.escalation.len() <= MAX_ESCALATION_PER_FRAME)` in
`finish_tile`. Closes the overflow but not the stale-slot read, and a panic on
the worker is fatal to the bridge (`frame_worker.rs` module docs, "Failure").

If UF-1 slips, the stand-alone fix is the same check against the
processor-recorded count, made a `.get()`-style bound before the
`from_raw_parts`.

### Files touched

As UF-1 (`frame_backend.rs`, `gpu_pipeline/mod.rs`, `frame.rs`).

### Tests

- `gpu_pipeline::tests::escalation_coefficients_stop_at_the_dispatched_count`
  (non-GPU, `from_slices` with `escalation_count = 2` and 512 slots of
  backing): `for(0)`, `for(1)` are `Some` of length 3072; `for(2)`, `for(511)`,
  `for(512)`, `for(usize::MAX)` are `None`.
- `io_bridge::tests::finish_tile_skips_escalation_slots_the_gpu_did_not_dispatch`
  using the existing fake/injected backend if one exposes `finish_tile`;
  otherwise a GPU test (serialised) that stages 2 candidates and passes 3
  `EscalationSlot`s: the third comes back `None`.
- Mutation: bound by `MAX_ESCALATION_PER_FRAME` instead of the dispatched count;
  the first test's `for(2)` case must fail.

### Gates, risks

As UF-1. **Size: S** (on top of UF-1).

---

## UF-5: public safe functions take a `RawFd` and `borrow_raw`/`mmap` it (Low)

### Status at e85fb005: CONFIRMED (C-API half is STALE)

- `GpuFrameProcessor::process_frame(&mut self, fd: RawFd, ..)` (`gpu_pipeline/mod.rs:673-681`)
  and `diff(&mut self, fd: RawFd, ..)` (`mod.rs:746-752`) are safe and forward
  to `process_frame_inner`, which `lseek`s the fd (`frame.rs:1274`), then
  `BorrowedFd::borrow_raw(fd).try_clone_to_owned()` (`frame.rs:1344`), and on
  the CPU tier `mmap`s it (`read_dmabuf_tight`, `frame.rs:3106`).
- `capture::dmabuf::readback_dmabuf(fd: RawFd, ..)` (`dmabuf.rs:382`) and
  `readback_dmabuf_rows(fd: RawFd, ..)` (`dmabuf.rs:411`) are safe and `mmap`
  the fd into a process-wide cache keyed by inode.
- A safe caller can pass any integer: a closed fd number reused by an
  unrelated file, another component's fd. That violates I/O safety (and
  `borrow_raw`'s precondition) from safe code. `-1` is caught by the `lseek`
  failing before `borrow_raw` is reached, so the immediate harm is "map and
  read an unrelated file", not memory UB, hence Low.
- **Callers** (all hold an `OwnedFd` already):
  `frame_backend.rs:486/491` (`frame.dmabuf_fd: Option<OwnedFd>`,
  `server.rs:35`, turned into `as_raw_fd()`), `frame_backend.rs:622-627`
  (`readback_dmabuf(fd.as_raw_fd(), ..)`), tests: `ghostframe-lib/tests/gpu_pipeline.rs`
  (6 sites via `TestDmabuf::raw()`, `tests/support/dmabuf_fixture.rs:35`),
  `gpu_pipeline/ingest_tests.rs:95-213, 237, 287, 363` (incl. memfds and a
  literal `-1` at `:189`), `capture/dmabuf.rs` tests (`:493`, `:652-654`,
  `:710`, `:723`), `gpu_pipeline/tests.rs`. `ghostframe-server`,
  `ghostframe-bench` and `ghostframe-e2e` call neither function
  (`c2_loop_stall_measure.rs` only constructs a processor).
- **C API: STALE.** `ghostframe-lib/src/ffi.rs` accepts no fd at e85fb005:
  `gf_server_submit_frame` builds a `FrameSubmission` with `dmabuf_fd: None`
  (`ffi.rs:143`). There is no int-fd boundary to keep `unsafe`. If one is added
  later, it must be an `unsafe extern "C" fn` that wraps the int in a
  `BorrowedFd` with a SAFETY comment, which is the legitimate boundary.

### Design

**Chosen:** change the four signatures to take `fd: BorrowedFd<'_>`
(`std::os::fd::BorrowedFd`). Inside, use `fd.as_raw_fd()` for `lseek`/`mmap`
and `fd.try_clone_to_owned()` instead of `borrow_raw(..)`. The private helpers
(`process_frame_inner`, `import_dmabuf`, `read_dmabuf_tight`, `readback_impl`,
`map_key_and_size`, `map_ro`, `copy_synced`, `dmabuf_sync*`) may keep `RawFd`
since they are private and reached only from the `BorrowedFd` entry points;
state that invariant once in each entry point's SAFETY comment. Callers pass
`fd.as_fd()`.

**Rejected:** marking the four functions `unsafe fn`. That moves `unsafe` to
every caller, all of which already hold an `OwnedFd`; the borrowed type
expresses the requirement for free.

### Files touched

`ghostframe-lib/src/capture/gpu_pipeline/{mod.rs, frame.rs, ingest_tests.rs, tests.rs}`,
`ghostframe-lib/src/capture/dmabuf.rs`,
`ghostframe-lib/src/transport/io_bridge/frame_backend.rs`,
`ghostframe-lib/tests/gpu_pipeline.rs`, `ghostframe-lib/tests/support/dmabuf_fixture.rs`.

### Implementation steps

1. Change `process_frame`, `diff`, `readback_dmabuf`, `readback_dmabuf_rows` to
   `BorrowedFd<'_>`; update their docs ("The `fd` is **not** consumed" becomes
   implied by the type; keep the note on the process-wide mapping cache).
2. Replace `BorrowedFd::borrow_raw(fd).try_clone_to_owned()` (`frame.rs:1344`)
   by threading the `BorrowedFd` down to `import_dmabuf` (preferred: change
   `import_dmabuf`'s parameter too, it is `pub(super)`), removing that `unsafe`.
3. `frame_backend.rs`: `frame.dmabuf_fd.as_ref().map(|fd| fd.as_fd())`;
   `readback_dmabuf(fd.as_fd(), ..)`. Drop the `AsRawFd` import if unused.
4. Fixture: add `TestDmabuf::fd(&self) -> BorrowedFd<'_>`; keep `raw()` only if
   something still needs it.
5. `ingest_tests.rs:189` (`process_frame(-1, ..)`): delete the case; the type
   now makes it unrepresentable (that is the compile-fail check below). Wrap
   the memfds (`:162-168`) in `OwnedFd` (`OwnedFd::from_raw_fd` with a SAFETY
   comment: fresh fd from `memfd_create`) and pass `.as_fd()`.
6. Port `dmabuf.rs` test sites (`buf.fd` there is a test-local `RawFd`; give
   `TestBuf` an `OwnedFd`).

### Tests

- Existing ingest and readback tests keep their assertions (bad geometry,
  short fd, memfd fallback), now with `BorrowedFd`.
- Doctest `compile_fail` on `readback_dmabuf`:
  `ghostframe_lib::capture::dmabuf::readback_dmabuf(3, 1, 1, 4)` must not
  compile (an integer is not a `BorrowedFd`), with a `no_run` twin passing
  `std::io::stdin().as_fd()`. Run by the `cargo test -p ghostframe-lib --doc`
  step UF-1 adds (widen its filter to `capture::`).
- Mutation: revert one signature to `RawFd`; the doctest must go red.

### Gates

`just lint`; `just test-unit` under `gf-gpu-run`; `cargo test -p ghostframe-lib
--test gpu_pipeline` on a GPU host with `/dev/udmabuf` (tests named
`dmabuf_real_*` are excluded from CI by name, so run them locally); doc step;
cbindgen diff (no change: none are `extern "C"`). Public API change for any
out-of-tree caller of `ghostframe_lib::capture` (none in the workspace).

### Risks

Textual conflict with UF-1 and LD in the same files. **Size: S.**

---

## UF-6: openh264 trace callback has no `catch_unwind` (Info)

### Status at e85fb005: CONFIRMED, and narrower than "UB"

`ghostframe-lib/src/encoder/h264/openh264_sys.rs:385-397` is an `unsafe extern
"C" fn trace(..)` registered with openh264 (`:585`) whose body calls
`tracing::error!`/`debug!`. The pinned toolchain is Rust 1.96.1
(`rust-toolchain.toml`); since Rust 1.81 a panic unwinding out of an
`extern "C"` function aborts the process instead of being UB. So the
consequence is an **abort of the daemon** if a tracing subscriber/layer panics
inside the callback (for example a re-entrancy `RefCell` borrow panic in a
layer), not memory unsafety. The precedent for doing it right is in the tree:
`ghostframe-tsnet/src/identity.rs:169` (`peer_authorizer_trampoline`) and
`ghostframe-lib/src/ffi.rs` use `catch_unwind`. The other Rust callbacks handed
to C (`ghostframe-client-h264/src/decoder.rs:280`, `get_vaapi_format`) cannot
panic (pointer walk only) and need nothing.

### Design

Wrap the body in `std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ..))`
and discard a caught panic (there is nowhere to report it that is not the thing
that just panicked). Move the body into a safe `fn trace_line(level: c_int,
msg: &CStr)` so the `extern` function is a thin, obviously correct shim.
`AssertUnwindSafe` is justified: the closure touches no state that outlives it.

Rejected: leaving it (an abort of the server for a log line is a bad trade
for four lines).

### Files, steps

`openh264_sys.rs` only. 1. Extract `trace_line`. 2. Wrap. 3. SAFETY comment on
the `CStr::from_ptr` stays.

### Tests

- `openh264_sys::tests::trace_tolerates_null_and_text`: call `trace` with a
  null `msg` and with `c"x\n"` at `WELS_LOG_ERROR` and debug levels.
- `..::a_panicking_subscriber_does_not_abort_the_trace_callback`: install a
  `tracing` subscriber (via `tracing::subscriber::with_default`) whose
  `event` panics, call `trace(..)`; the test returning at all is the
  assertion. Mutation: remove the `catch_unwind`; the test binary aborts
  (loud: the whole lib test run fails, so do this mutation on a filtered run).
  Needs no GPU and no libopenh264.

### Gates, risks

`just lint`, `just test-unit`. None. **Size: S.**

---

## UF-7: `ghostframe-cli` mutates the environment under a false "single-threaded" comment (Info)

### Status at e85fb005: CONFIRMED

`ghostframe-cli/src/commands.rs:315-329`: three `std::env::set_var` calls
(`GHOSTFRAME_CLIENT_DUMP_FRAME`, `_EXPORT`, `_COUNT`) under
"SAFETY (soundness, not `unsafe`): single-threaded at this point". The CLI
links `ghostframe-tsnet` (`ghostframe-cli/Cargo.toml:17`), i.e. the Go
c-archive, whose runtime starts its own threads from a load-time constructor
before `main`; `set_var` racing any thread's `getenv` (libc, Mesa, Go's cgo
side) is the documented hazard (and `unsafe` in edition 2024). The readers are
`DumpConfig::from_env()` in `ghostframe-client-gpu/src/renderer.rs:298` and
`ghostframe-client-native/src/render_thread.rs:606`. The repo already bans
`set_var` in `ghostframe-server/src` (`ci/check-env-reads.sh:60`) but not in
the CLI.

### Design

**Chosen: pass the dump settings as data.** Add `pub dump: Option<DumpConfig>`
to `ghostframe_client_native::Config` (`lib.rs:83-122`); `None` keeps today's
behaviour (read the environment, which is what the C API embedder wants);
`Some` overrides. `render_thread::run` takes the resolved `DumpConfig`
(`dump.unwrap_or_else(DumpConfig::from_env)`, resolved once in
`Client::new`) instead of calling `from_env` at `:606`, and hands it to the
renderer through a new `Renderer::set_dump(&mut self, DumpConfig)` called right
after construction (a setter, because `Renderer::new` has ~39 call sites). The
CLI builds `DumpConfig { frame_prefix, export_prefix, every: 0, count:
u64::MAX }` from `--dump-dir` and deletes the three `set_var`s. Extend
`ci/check-env-reads.sh`'s `set_var|remove_var` ban to `ghostframe-cli/src`.

Rejected: keeping `set_var` and correcting the comment. The race is real and
the fix is small.

### Files touched

`ghostframe-cli/src/commands.rs`, `ghostframe-client-native/src/{lib.rs,
render_thread.rs}`, `ghostframe-client-gpu/src/renderer.rs`,
`ghostframe-client-capi/src/lib.rs` (struct literal at `:241` gets `dump:
None`), `ci/check-env-reads.sh`.

### Steps

1. Add the field and plumb it as above. 2. CLI: build the `DumpConfig`; remove
`set_var`. 3. capi literal: `dump: None`. 4. Extend the CI guard.

### Tests

- `ghostframe-cli` unit test: `--dump-dir /x` yields a `Config` whose `dump`
  is `Some` with `frame_prefix = "/x/frame"`, `export_prefix = "/x/export"`,
  `count = u64::MAX`, and the process environment has no
  `GHOSTFRAME_CLIENT_DUMP_FRAME` afterwards (read under the shared env lock if
  the CLI has one; otherwise assert on the returned `Config` only). Factor the
  `opts -> Config` step into a function if needed to test it without
  connecting.
- `ci/check-env-reads.sh` self-check: a deliberate `set_var` in a scratch
  `ghostframe-cli` file must make the script exit non-zero (run by hand; the
  script has no test harness).
- Mutation: leave one `set_var` in place; the guard must fail.

### Gates, risks

`just ci-client` (client-only gate covers cli, client-native, client-gpu,
capi); env-read guard; cbindgen: `ghostframe-client-capi` header unaffected
(`Config` is Rust-side). **Size: S.**

---

## UF-8: SAFETY-comment coverage and the `unsafe_op_in_unsafe_fn` / `undocumented_unsafe_blocks` rollout (Medium, hygiene)

### Status at e85fb005: CONFIRMED (numbers corrected)

No crate denies `unsafe_op_in_unsafe_fn`, none enables
`clippy::undocumented_unsafe_blocks`; no `[lints]` tables; all `edition =
"2021"`; `clippy.toml` has only `disallowed-methods`. Specific sites from the
review, re-checked:

- `gpu_pipeline/frame.rs`: 37 `unsafe fn`s (3342 lines), 4 explicit `unsafe {}`
  blocks, 19 SAFETY comments (the review's "3" undercounted); ~78
  `device.*(..)` calls plus pointer arithmetic sit inside `unsafe fn` bodies
  with no block, e.g. `process_frame_with_imported` (`frame.rs:675`, about 390
  lines to `:1063`).
- `GpuFrameProcessor`'s `unsafe impl Send` (`mod.rs:646-648`): "used from a
  single tokio task", stale (dedicated OS thread). Rewritten in UF-1 step 10.
- `invalidate_baseline`'s block (`mod.rs:801`): explanatory comment inside the
  block but no `SAFETY:` line above it; clippy would flag it. Same at
  `mod.rs:777` (set_escalation_candidates has one) and `:825` (Drop).

**Counts per crate** (script at the end of this section; "blocks" =
`unsafe {` in code, "unsafe fn" = named `unsafe [extern "C"] fn` definitions,
"SAFETY" = comment lines containing `SAFETY`, "# Safety" = doc sections,
"undoc" = `unsafe {`/`unsafe impl` with no `SAFETY` comment in the 6 lines
above, a lower bound on what clippy will report; "test" = `tests/`,
`*_tests.rs`, `tests.rs` and in-file `#[cfg(test)] mod`):

| crate | src blocks | src unsafe fn | src unsafe impl | src SAFETY | src # Safety | src undoc | test blocks | test SAFETY | test undoc |
|---|---|---|---|---|---|---|---|---|---|
| ghostframe-lib | 97 | 53 | 7 | 113 | 30 | 23 | 93 | 9 | 88 |
| ghostframe-client-gpu | 125 | 2 | 12 | 134 | 2 | 12 | 12 | 9 | 4 |
| ghostframe-client-h264 | 37 | 1 | 3 | 37 | 1 | 4 | 4 | 4 | 0 |
| ghostframe-client-native | 26 | 0 | 0 | 27 | 0 | 0 | 14 | 10 | 4 |
| ghostframe-client-capi | 22 | 18 | 0 | 37 | 18 | 3 | 21 | 0 | 23 |
| ghostframe-tsnet | 16 | 2 | 0 | 6 | 0 | 11 | 8 | 0 | 8 |
| ghostframe-server | 17 | 0 | 1 | 26 | 0 | 0 | 7 | 7 | 2 |
| ghostframe-cli | 8 | 0 | 0 | 9 | 0 | 0 | 0 | 0 | 0 |
| ghostframe-test-pattern | 7 | 0 | 0 | 7 | 0 | 0 | 0 | 0 | 0 |
| ghostframe-e2e | 3 | 0 | 0 | 3 | 0 | 0 | 0 | 2 | 0 |
| ghostframe-client-core | 1 | 1 | 0 | 4 | 1 | 0 | 1 | 1 | 0 |
| ghostframe-bench | 1 | 0 | 0 | 1 | 0 | 1 | 0 | 0 | 1 (3 `unsafe fn` + 1 impl in `benches/send_path.rs`) |

(The review's "244 vs 83" for ghostframe-lib counted every `unsafe` token; a raw
`grep unsafe` of non-comment lines gives 272. The table separates the kinds.)

ghostframe-lib `unsafe fn` by file (each becomes N explicit blocks under
`unsafe_op_in_unsafe_fn`): `gpu_pipeline/frame.rs` 37, `ffi.rs` 6,
`capture/pipeline_builder.rs` 5, `gpu_pipeline/mod.rs` 2,
`encoder/h264/openh264_sys.rs` 2, `gpu_pipeline/setup.rs` 1 (`new_inner`, the
whole Vulkan setup). Largest undocumented-block clusters:
`ghostframe-tsnet/src/lib.rs` (13 blocks, 2 SAFETY: every `gbridge_*` call),
`capture/dmabuf.rs` tests (28), `gpu_pipeline/tests.rs` (23),
`gpu_pipeline/damage_tests.rs` (17), `gpu_pipeline/readback_bench.rs` (12),
`ghostframe-client-capi/tests/abi.rs` (20 blocks + 2 impls, 0 SAFETY),
`ghostframe-tsnet/src/identity.rs` tests (8). `ghostframe-client-capi/src/lib.rs`
already uses explicit inner blocks in its 18 `pub unsafe extern "C" fn`s, so it
is close to `unsafe_op_in_unsafe_fn`-clean.

### Root cause

No lint ever asked for it; `unsafe fn` bodies are implicitly unsafe in edition
2021, so the biggest Vulkan file has almost no blocks to comment.

### Design

**Lints, per crate, in this order: `unsafe_op_in_unsafe_fn = "deny"` first, then
`clippy::undocumented_unsafe_blocks = "deny"`.** The order matters: the first
turns implicit unsafe bodies into explicit blocks, which the second then
requires to be documented. Doing them the other way round documents only the
blocks that exist today and leaves `frame.rs` dark.

There is no useful "warn" phase in CI: `just lint` and CI run clippy with
`-D warnings`, so a lint at `warn` fails CI exactly like `deny`. The warn phase
is local only: before flipping a crate, list its sites with
`cargo clippy -p <crate> --all-targets <feature flags> -- -W unsafe_op_in_unsafe_fn -W clippy::undocumented_unsafe_blocks`,
fix them, then commit the `deny`.

**Where the lint is set (coordination with EC).** Cargo's `[lints]` cannot mix
`workspace = true` with local entries, and crate-root `#![deny]` attributes do
not reach integration-test crates (`tests/*.rs`). So:

- *If EC has not yet landed `[workspace.lints]`:* each crate, as it becomes
  clean, gets a local `[lints.rust] unsafe_op_in_unsafe_fn = "deny"` and
  `[lints.clippy] undocumented_unsafe_blocks = "deny"` in its own
  `Cargo.toml` (covers lib, bins, tests, benches, examples). When the last
  crate is clean, EC adds both lints to `[workspace.lints]` and replaces each
  local table with `workspace = true` in the same PR.
- *If EC lands `[workspace.lints]` with `workspace = true` everywhere first:*
  UF uses crate-root `#![deny(unsafe_op_in_unsafe_fn)]` /
  `#![deny(clippy::undocumented_unsafe_blocks)]` in `lib.rs`/`main.rs` plus the
  same inner attribute at the top of each `tests/*.rs` and `benches/*.rs` of
  that crate, and EC promotes them to the workspace table at the end and
  deletes the attributes. Either way the end state is the workspace table,
  owned by EC.

Edition 2024 makes `unsafe_op_in_unsafe_fn` warn by default and `set_var`
`unsafe`, so this burn-down (and UF-7) is also the prerequisite for any edition
bump EC plans.

**Test code is in scope**, not allowed off: the test unsafe is raw fds,
`mmap`, `memfd_create` and FFI round-trips, exactly where a wrong assumption
has hurt this repo before (AGENTS.md "Testing habits"). A one-line SAFETY
comment is cheap. The single exception is a whole test module that is a
deliberate unsafe exerciser (`ghostframe-client-capi/tests/abi.rs` calls the C
ABI with hand-built pointers by design): a module-level
`#![allow(clippy::undocumented_unsafe_blocks, reason = "..")]` with a reason
naming that purpose is acceptable there, and nowhere else.

**What a SAFETY comment must say** (put in DEVELOPERS.md by EC or with the
first PR): which precondition the operation has, and why it holds *here*
(lifetimes of the pointee, bounds, synchronisation, fd ownership). "Vulkan
call" or "valid pointer" is not an argument. For the `ash` calls in `frame.rs`,
one block per recording helper with a SAFETY comment naming the handles' owner,
the external-synchronisation argument (`&mut self`), and the command buffer's
recording state is the right granularity, not one block per `cmd_*` call.

**`unsafe fn` that need not be unsafe.** While wrapping, demote an `unsafe fn`
to a safe `fn` (with internal blocks) when its precondition is fully discharged
by `&mut self` / owned handles and no caller can violate it (likely for many of
the `run_*_stage` and `record_*` helpers in `frame.rs`, and
`pipeline_builder.rs`'s `alloc_*`). Keep `unsafe fn` where a caller-supplied
precondition remains (raw pointers, external handles), with a `# Safety` doc.
Do this per file, reviewed, not mechanically.

**Burn-down order** (smallest blast radius first, to prove the mechanism; the
two large ones last and after the refactors that delete unsafe):

1. `ghostframe-cli`, `ghostframe-test-pattern`, `ghostframe-e2e`,
   `ghostframe-client-core` (0 undocumented in src; flip both lints). After
   UF-7 for the CLI.
2. `ghostframe-server` (2 test sites), `ghostframe-client-native` (4 test
   sites), `ghostframe-bench` (2 sites + 3 `unsafe fn` in `benches/send_path.rs`).
3. `ghostframe-client-h264` (4 src sites; after UF-3, which removes the
   per-row blocks). Must pass both feature sets: default `vaapi` and
   `--no-default-features --features gstreamer-backend[,test-support]`
   (Justfile `:100-101`).
4. `ghostframe-client-capi` (3 src + 23 test, 18 `unsafe extern "C" fn`
   already mostly block-explicit). Run cbindgen for that crate's header too if
   it has one under check.
5. `ghostframe-client-gpu` (12 src + 4 test; 12 `unsafe impl`s, each needs a
   real Send/Sync argument). Both GPU feature variants (`vulkan`, and `gles`
   via Justfile `:83`). After UF-2.
6. `ghostframe-tsnet` (11 src in `lib.rs` (the `gbridge_*` FFI), 8 test;
   2 `unsafe fn` in `identity.rs`). Builds Go; use the `-tags noweb` client
   build and a server-crate build to cover both.
7. `ghostframe-lib` last, after UF-1/UF-4/UF-5 (which delete 10 of its
   undocumented blocks and the `FrameAnalysis` impl) and after LD's refactor of
   `gpu_pipeline` lands or is explicitly sequenced. Split into PRs by area:
   (a) `unsafe_op_in_unsafe_fn` for `gpu_pipeline/frame.rs` + `setup.rs` +
   `pipeline_builder.rs` (the big mechanical one: ~78 device calls in 37 fns),
   (b) `encoder/h264/*` + `ffi.rs` + `transport/*`, (c) test files
   (`capture/dmabuf.rs` tests, `gpu_pipeline/{tests, damage_tests,
   ingest_tests, readback_bench, reference_tests, cdf53_forward_gate_tests}.rs`,
   `tests/support/dmabuf_fixture.rs`, `tests/gpu_pipeline.rs`), then (d) flip
   both lints in `ghostframe-lib/Cargo.toml`.

`ghostframe-protocol`, `ghostframe-render`, `ghostframe-client-net`,
`ghostframe-client-wasm`, `ghostframe-web-ui` have no `unsafe` in the count and
can take the lints with step 1 (the wasm ones need `just lint-web`, which
compiles them for `wasm32`; the host clippy does not).

### Steps for each crate PR (Sonnet implementer)

1. Run the local `-W` clippy listing above for every feature set the Justfile
   lints that crate with; save the list in the PR description (site count
   before).
2. Wrap unsafe ops in `unsafe fn` bodies; where a body is one coherent unsafe
   sequence, one block with one argument is fine. Consider demotion (above).
3. Add or rewrite SAFETY comments; fix stale ones (the `Send` impl kind:
   "single tokio task").
4. Add the two `deny` entries (per the EC coordination rule above).
5. Re-run clippy for every feature set: zero sites. Re-run that crate's tests
   (GPU crates under `gf-gpu-run`).

### Tests and mutation check

No behaviour changes, so no new behavioural tests; the gate is clippy. Mutation
check per PR: in a scratch commit, delete one SAFETY comment and add one
unwrapped unsafe op inside an `unsafe fn` in that crate (and in one of its
`tests/*.rs` files): `just lint` must fail on both (proves the lint is active
on every target, including integration tests). For the demotions, the existing
tests of those functions must still pass (they are the only evidence the
demotion did not change semantics).

### Gates

`just lint` (workspace, `--all-targets`, `-D warnings`) **and** every
feature-variant clippy line in the Justfile for the crate (`ci-client` covers
the client crates' variants; `lint-web` for wasm); `just test-unit` / `just
test-client` (GPU under `gf-gpu-run`); cbindgen header diff for
`ghostframe-lib` and capi PRs (wrapping bodies of `pub unsafe extern "C" fn`
does not change signatures, but the gate is cheap); `ci/check-local-paths.sh`.

### Risks

- Mechanical wrapping of `frame.rs` is a large diff in a file LD may be
  refactoring: sequence explicitly (below).
- `undocumented_unsafe_blocks` placement rules (comment must directly precede
  the block or its statement; attributes in between are accepted by default)
  will reject some existing comments the script counted as present: budget for
  more sites than the "undoc" column.
- Demoting `unsafe fn` to safe changes reviewability, not behaviour; keep it
  out of the mechanical PRs if review bandwidth is short.
- **Size: L** in total (about 25 documentation sites + 110 test sites + ~80
  wrapped ops in `frame.rs`), split into ~9 small PRs, most S, the
  `ghostframe-lib` (a) and (c) PRs M.

Count script (read-only):
`docs/reviews/data/2026-11-25/count_unsafe.py`,
run as `python3 -I <script> <repo root>`.

---

## Suggested PRs for UF

| # | Branch | Findings | Files (ownership) | Size | Depends on |
|---|---|---|---|---|---|
| 1 | `fix/uf-frame-outputs-view` | UF-1, UF-4 | `ghostframe-lib/src/capture/gpu_pipeline/{mod,frame,setup}.rs` + its test files, `transport/io_bridge/{frame_backend,frame}.rs`, `io_bridge/tests.rs`, `tests/gpu_pipeline.rs`, one CI step (`cargo test -p ghostframe-lib --doc gpu_pipeline`) in `.github/workflows/ci.yml` and `just ci-local` | M | coordinate with **LD** (see below) |
| 2 | `fix/uf-borrowed-fd` | UF-5 | `gpu_pipeline/{mod,frame,ingest_tests,tests}.rs`, `capture/dmabuf.rs`, `io_bridge/frame_backend.rs` (2 lines), `tests/gpu_pipeline.rs`, `tests/support/dmabuf_fixture.rs` | S | after #1 (same files) |
| 3 | `fix/uf-openh264-trace-unwind` | UF-6 | `encoder/h264/openh264_sys.rs` | S | none |
| 4 | `fix/uf-export-drop-leak-on-busy` | UF-2 | `ghostframe-client-gpu/src/export.rs` | S | none |
| 5 | `fix/uf-gst-download-bounds` | UF-3 | `ghostframe-client-h264/src/{descriptor,decoder_gst}.rs` | S | none |
| 6 | `fix/uf-cli-dump-config` | UF-7 | `ghostframe-cli/src/commands.rs`, `ghostframe-client-native/src/{lib,render_thread}.rs`, `ghostframe-client-gpu/src/renderer.rs`, `ghostframe-client-capi/src/lib.rs` (one literal), `ci/check-env-reads.sh` | S | none |
| 7a-7f | `chore/uf-unsafe-lints-<crate>` | UF-8 steps 1-6 | each crate's own files + its `Cargo.toml` `[lints]` (or crate-root attributes; see EC rule) | S each | 7a after #6; 7c after #5; 7e after #4; all coordinate with **EC** on where the lint lives |
| 8a-8d | `chore/uf-unsafe-lints-lib-{gpu,encoder,tests,flip}` | UF-8 step 7 | `ghostframe-lib` areas as listed | M, S, M, S | after #1, #2, #3 and LD's `gpu_pipeline` work |

**Contention flags.**

- `io_bridge/frame_backend.rs` and `capture/gpu_pipeline/*` are shared with
  **LD**. Recommended: land #1 first if LD's refactor is not yet started (it
  shrinks the unsafe surface LD has to carry and changes the analysis API LD
  would otherwise refactor twice); if LD is already in flight, rebase #1 onto
  LD and keep #1's diff limited to the analysis/outputs API. #2 and #8a touch
  the same files: never in flight at the same time as an LD PR on
  `gpu_pipeline/frame.rs`.
- #1 and #2 also touch `ghostframe-lib/tests/gpu_pipeline.rs` and the
  `dmabuf` fixture: serialise them (one branch after the other), and run the
  GPU suites through `gf-gpu-run` (never two GPU agents at once: parallel GPU
  load has reset amdgpu on this host).
- `.github/workflows/ci.yml` (the doctest step in #1) and every
  `Cargo.toml` `[lints]` edit in #7/#8 overlap with **EC**'s
  `[workspace.lints]` and CI work: agree with EC which of the two
  placement paths in UF-8 applies before #7a.

**Order:** #3, #4, #5, #6 in parallel (independent crates) → #1 → #2 → #7a-#7f
(in UF-8's crate order) → #8a-#8d → EC promotes the lints to
`[workspace.lints]`.
