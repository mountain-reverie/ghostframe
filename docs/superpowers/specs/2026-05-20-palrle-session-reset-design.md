# `e2e_palrle_session_reset` Closure — Design

**Date**: 2026-05-20
**Milestone**: Pre-M3.3 backlog cleanup. Closes the second-to-last `#[ignore]`'d e2e test (`e2e_resolution_change` is a separate cycle, scoped next). One step in the path to a clean backlog before opening the M3.3 (CDF 5/3 + progressive refinement) brainstorm.
**Predecessor**: `docs/superpowers/specs/2026-05-17-decode-error-thin-uncached-design.md` (closed 2026-05-18) — the prior session-reset-adjacent fix; this design completes the picture for the GPU path.

## Background

`e2e_palrle_session_reset` (`ghostframe-e2e/tests/e2e.rs:1033`, `#[ignore]`'d) drives a `--text-grid --drm-direct` session, asserts text is legible, calls `page.reload()`, waits 4 s, asserts text is *still* legible on the new session. After the reload the canvas reads pure black; `__ghostframeRecordedCodecs` post-reset is empty.

The `#[ignore]` message blames "post-reset server doesn't re-emit static content; needs session-aware re-classification". That's directionally right but mislabels the layer. Investigation 2026-05-20 traced the failure to:

> `fire_session_reset` (`ghostframe-lib/src/transport/io_bridge.rs:827`) resets all CPU-side per-session state — `dirty_tracker`, `metrics_tracker`, `classifier`, `scheduler`, `palette_table`, `frame_mode`, `dimensions_retransmits_left`, `force_dirty_frames`, IDR request — but **the GPU pipeline's Vulkan SAD `prev_image` (`ghostframe-lib/src/capture/gpu_pipeline/mod.rs:371`) is not touched**. On the GPU path, dirty detection is the compute shader at `tile_sad.comp` comparing the freshly captured frame against `prev_image`. With unchanged screen content + a new session, SAD reports zero dirty tiles, `process_frame_gpu`'s `dirty_xy.is_empty()` short-circuit at `io_bridge.rs:1162` returns early, and the queued `request_keyframe()` is never consumed.

The CPU path has slow-start mitigation for this scenario: `force_dirty_frames = 20` puts `dirty_tracker` into "no-commit" mode for 20 frames so dropped datagrams during QUIC slow-start stay dirty until either ACKed or the congestion window opens. The GPU path has no analogue.

## Goal

Add a GPU-side counterpart to the CPU `force_dirty_frames` mechanism, with **matching semantics**: for N frames after a session reset, the GPU pipeline emits all tiles as dirty without committing `prev_image`, so a tile dropped during QUIC slow-start re-emerges as dirty on the next frame. After the cushion exhausts, the next frame snapshots `prev_image` and resumes normal SAD-based dirty detection.

End state: `#[ignore]` removed from `e2e_palrle_session_reset`; the GPU path's slow-start behaviour is symmetric to the CPU path's.

## Approach

### CPU path semantics — what we're matching

`process_frame_cpu` (`io_bridge.rs:880`):
- On reset, `force_dirty_frames = 20`.
- For each of the next 20 frames: `update_no_commit` calls `update_inner(commit=false)`. Inside, `first_frame = prev_tiles.is_empty()` is true on frame 1 (post-reset clear), allocating prev_tiles to zero-fill. Every subsequent frame compares current pixels against the all-zero prev_tiles, finds every tile "different", returns all tiles dirty. Because `commit=false`, prev_tiles is never written. Twenty frames of all-tiles-dirty.
- On frame 21: `force_dirty_frames` is 0, `no_commit=false`, `update()` calls `update_inner(commit=true)`. `first_frame` is false (prev_tiles still exists, still zero-filled), so the comparison again finds every tile different → all dirty → commits current pixels into prev_tiles. Frame 21 is the first "real" baseline.
- Frame 22+: SAD-equivalent comparison against the now-current prev_tiles. Normal operation.

Net for static content: **21 frames of all-tiles-dirty, then normal**.

### GPU path matching semantics

Same shape, different storage:

- On reset, drop `prev_image` (set to `None`) AND set `force_all_dirty_remaining = 20`.
- For each of the next 20 frames: `prev_image` is `None`, so the existing first-frame branch at `capture/gpu_pipeline/frame.rs:233` fires. But because the counter is > 0, the snapshot copy step is skipped — the function runs NV12 + tile_analysis + palrle_compact + palette_fold + palette_subset_fold (init+fold) + pal_rle_index as today, but **does not allocate `prev_image` or copy current → snapshot**. `prev_image` stays `None`. All tiles reported dirty. Decrement counter.
- On frame 21: counter is 0, `prev_image` is still `None` → the same first-frame branch fires, this time with snapshot allocation enabled. Snapshot copy runs, `prev_image = Some(snap)`, all tiles reported dirty.
- Frame 22+: `prev_image.is_some()` → normal SAD-based path at `process_frame_with_imported` runs. Normal operation.

Net for static content: **21 frames of all-tiles-dirty, then normal**. Symmetric to CPU.

### Why match exactly and not "simpler override after SAD"

The override-after-SAD alternative (run SAD normally for N frames, replace its output with all-tiles-dirty) is one fewer Vulkan-resource branch but introduces a divergence between CPU and GPU semantics: GPU commits prev_image every frame during the cushion; CPU doesn't commit prev_tiles. Both achieve "20 frames of all-dirty" for static content, but the bookkeeping diverges and future readers comparing the two paths would have to re-derive that the difference is benign. Matching CPU semantics keeps the two paths mentally interchangeable.

### Public API on `GpuFrameProcessor`

One combined method:

```rust
impl GpuFrameProcessor {
    /// Reset per-session state on a new WebTransport session, matching the
    /// CPU path's `dirty_tracker.reset() + force_dirty_frames = N` shape.
    ///
    /// Drops the cached `prev_image` (freeing its Vulkan resources) AND sets
    /// the no-commit cushion to `force_frames` frames. During the cushion,
    /// every frame is reported as fully dirty without snapshotting
    /// `prev_image`, so datagrams dropped by QUIC slow-start naturally
    /// resurface as dirty on subsequent frames.
    pub fn reset_for_session(&mut self, force_frames: u32);
}
```

Combined-API rationale: a separate `take_prev_image()` + `force_all_dirty_frames(n)` pair invites the caller to set the counter without dropping prev_image, which leaves `prev_image.is_some()` and the SAD path runs normally for the first frame — defeating the cushion. The combined API removes the ordering footgun.

### Internal refactor: optional snapshot in `run_first_frame_passes`

`run_first_frame_passes` (`capture/gpu_pipeline/frame.rs:989`) currently takes `snapshot: &PrevFrame` and unconditionally records the snapshot copy in its command buffer. Make it accept `Option<&PrevFrame>` and gate the copy step on `Some`. The other stages (NV12, tile_analysis, palrle_compact, palrle_indirect_args, palette_fold, palette_subset_fold_{init,fold}, pal_rle_index) are unchanged — they read from `current` only and must run every frame during the cushion to give the classifier full per-tile metrics.

### First-frame branch update at `capture/gpu_pipeline/frame.rs:233`

```rust
if self.prev_image.is_none() {
    let all_dirty: Vec<u32> = (0..tile_count).collect();
    let no_commit = self.force_all_dirty_remaining > 0;
    let snapshot = if no_commit {
        None
    } else {
        Some(self.allocate_owned_image(width, height)?)
    };
    self.run_first_frame_passes(current, snapshot.as_ref(), geom, nv12_layout)?;
    if let Some(mut snap) = snapshot {
        snap.layout = vk::ImageLayout::GENERAL;
        self.prev_image = Some(snap);
    }
    if no_commit {
        self.force_all_dirty_remaining -= 1;
    }
    return Ok(FrameAnalysis { dirty_tiles: all_dirty, ... });
}
```

### Wire-up in `fire_session_reset`

The existing CPU-only gate at `io_bridge.rs:858-865`:

```rust
// today
if self.gpu_frame_processor.is_none() {
    self.force_dirty_frames = 20;
}
```

becomes:

```rust
// after — both paths get their cushion, symmetric to dirty_tracker.reset() above
self.force_dirty_frames = 20;
if let Some(p) = self.gpu_frame_processor.as_mut() {
    p.reset_for_session(20);
}
```

Setting `self.force_dirty_frames = 20` unconditionally is harmless on the GPU path (`process_frame_gpu` doesn't read it) and keeps the call site obvious — no branching on which pipeline is active. The CPU `dirty_tracker.reset()` already lives a few lines up at `io_bridge.rs:835`.

## Side-effect — documented, benign

20 frames of artificially-all-dirty inflate `metrics_tracker`'s `change_freq_hz` EMA (α=0.1) for every tile. After the cushion, the classifier briefly sees high frequencies on tiles that are actually static, which could bias toward H264/motion classifications.

In practice this is invisible:
- `fire_session_reset` already sets `frame_mode = H264` and `request_keyframe`. The classifier's `exit_sustain_frames = 30` keeps the stream in H264 mode for ≥30 frames regardless of the EMA inflation.
- After 30 frames of TileCodec mode for stable static content, EMA decays via α=0.1 to under 1 Hz within ~30 frames (~1 s at 30 fps).
- The EMA inflation window and the H264-mode window overlap exactly, so the inflation is masked.

Documented here so a future investigator running `change_freq_hz`-related telemetry post-reset doesn't re-discover this as a regression.

## Testing

### Unit test on `GpuFrameProcessor` (Vulkan-gated)

Gated under the existing feature flag used by Vulkan-requiring tests (the existing `gpu_pipeline` tests are the reference for which gate to use).

1. Construct processor at a known resolution.
2. Call `process_frame` once with a known frame → assert `dirty_tiles.len() == tile_count` (first-frame path) and `prev_image.is_some()`.
3. Call `process_frame` with the same pixels → assert `dirty_tiles.is_empty()` (SAD finds no change).
4. Call `reset_for_session(3)`.
5. Assert `prev_image.is_none()` after the call.
6. Call `process_frame` three times with the same unchanged pixels → assert each returns `dirty_tiles.len() == tile_count` AND `prev_image.is_none()`.
7. Call `process_frame` a fourth time → assert `dirty_tiles.len() == tile_count` AND now `prev_image.is_some()` (the snapshot frame).
8. Call `process_frame` a fifth time → assert `dirty_tiles.is_empty()` (normal SAD resumed).

### E2E

Remove the `#[ignore]` attribute from `e2e_palrle_session_reset` at `ghostframe-e2e/tests/e2e.rs:1033`. Update the docstring to reflect the closure (the comment block at 1028-1032 explains the old failure mode and points at `project_m32c_deferred.md`; replace with a one-line note that the GPU SAD reset now happens via `fire_session_reset → reset_for_session(20)`).

Existing assertions to keep:
- Pre-reset luminance probe (already passing per investigation).
- `page.reload()`.
- Post-reset luminance probe — load-bearing assertion. After the fix, the H264 IDR emitted by `fire_session_reset`'s forced `frame_mode = H264 + request_keyframe` path carries the text content. H264 is lossy but text-contrast survives the 0.95-ish SSIM the IDR provides; `post_ink_lum - post_bg_lum > 80.0` is comfortably met.

Existing assertion to drop:
- **`__ghostframeRecordedCodecs.contains(&2u8)` post-reset** (lines 1087-1097). This assertion was structurally unreachable under the existing `fire_session_reset` design and remains so after this fix. Walking the post-reset frame sequence with static text content (`--text-grid --drm-direct`):
  1. `fire_session_reset` sets `frame_mode = H264` (intentional load-shedding — 50 H264 datagrams beats 768 raw/PalRle tile datagrams during QUIC slow-start) and `request_keyframe`.
  2. Frames 1-20 (force-all-dirty cushion): classifier tentatively classifies text tiles as PalRle, but `decide_frame_mode`'s hysteresis (`exit_sustain_frames = 30`) keeps mode in H264. H264 IDR + P-frames emit; no PalRle wire emission.
  3. Frame 21: first-frame-with-snapshot, all-dirty, still in H264 mode (`exit_sustain` ticked to ~21). H264 P-frame emits.
  4. Frames 22-30: `prev_image.is_some()`, SAD compares against unchanged content → 0 dirty → empty tentative → still in H264 (exit_sustain still ticking). No emission.
  5. Frame 31+: mode flips to TileCodec, but `dirty_xy` stays empty for static content → no emission ever.

  Net: 0 PalRle wire emissions post-reset for static content. The `contains(&2)` assertion as written is unreachable.

  Originally written assuming `dirty_tracker.reset()` alone would cause post-reset dirty tiles to emit as PalRle — but `dirty_tracker` is unused on the GPU path, and even with the GPU SAD fix the H264-mode load-shedding rule dominates.

The closure replaces the codec assertion with **per the test's docstring intent** — "warm-cache palette table re-delivers palettes on the new session AND the text region renders correctly post-reset." The post-reset luminance probe covers the rendering half. The warm-cache-bundling half (PalRle bundled with palette delivered=false → ACKed → delivered=true → subsequent thin emission) is a real concern but requires *dynamic* content to fire any PalRle at all under the H264-on-reset load-shedding rule. Coverage of that warm-cache scenario is deferred to a follow-up test with a content-dirtying test pattern (e.g. `--palette-churn`, `--mode-switch-cycle`, or a new mode that combines text with periodic dirty events).

### Follow-up test deferred (not in scope for this cycle)

A new e2e — call it `e2e_palrle_warm_cache_after_reset` — would use a test pattern that produces both H264-friendly motion AND text-friendly static regions, run for long enough that exit_sustain elapses and PalRle starts flowing, then `page.reload()`, then assert that **bundled** PalRle emissions occur post-reset (palette delivered=false after `palette_table.on_session_reset(preserve_delivered=false)` → re-bundle) followed by thin emissions (delivered=true after ACK). Tracked as follow-up; not blocking this closure.

## Out of scope

- **`e2e_resolution_change`** — a separate, larger fix (xdaemon doesn't notice xrandr-driven X server resolution changes, fails with `BadMatch` on GetImage). Its own brainstorm cycle next.
- **Audit other `gpu_frame_processor` cross-session state** beyond `prev_image`. The processor owns descriptor pools, command buffers, the NV12 output buffer, the palette/RLE staging buffers, palette table view, etc.; this design assumes all of those are either stateless or already correctly per-frame. If a future investigation finds another stale-across-sessions field, it can extend `reset_for_session` with an additional teardown.
- **Replacing the CPU path's 20-frame magic constant with something derived from QUIC's congestion-window state.** Out of scope for this cycle; tracked alongside the M3.3 bandwidth-budget work.

## Decision register

| # | Decision | Rationale |
|---|---|---|
| D1 | Match CPU's no-commit semantics exactly (skip snapshot for N frames) rather than override-after-SAD | Keeps both paths mentally interchangeable; no divergence to re-derive later. |
| D2 | Combined `reset_for_session(force_frames)` API on `GpuFrameProcessor`, not two separate methods | Prevents ordering footgun where counter is set without dropping prev_image. |
| D3 | `run_first_frame_passes` takes `Option<&PrevFrame>` snapshot | Snapshot copy is now optional; cleanly expresses no-commit semantics at the type level. |
| D4 | `self.force_dirty_frames = 20` set unconditionally in `fire_session_reset`; GPU path harmlessly ignores it | Keeps the call site readable — no branching on which pipeline is active. |
| D5 | `force_frames = 20` chosen to match CPU path's existing constant | Symmetry. No measurement motivates a different value at this time. |
| D6 | Side-effect on `metrics_tracker.change_freq_hz` EMA documented but not fixed | The H264-mode lock from `fire_session_reset` covers the inflation window exactly. |
| D7 | Test's `codec_list.contains(&2)` post-reset assertion dropped, not retained-with-altered-content | Assertion was structurally unreachable under H264-on-reset load-shedding for static content; making it reachable would require a separate dynamic-content test pattern (deferred follow-up). Drop now; keep test's load-bearing luminance probe; queue warm-cache-bundling coverage as a separate cycle. |
