# `e2e_palrle_session_reset` Closure Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the `#[ignore]`'d `e2e_palrle_session_reset` test by giving the GPU path a session-reset cushion that matches the CPU path's `force_dirty_frames` no-commit semantics.

**Architecture:** Add `force_all_dirty_remaining: u32` to `GpuFrameProcessor` plus a combined `reset_for_session(force_frames)` public API that drops `prev_image` AND sets the counter. The existing first-frame branch at `capture/gpu_pipeline/frame.rs:233` gates the snapshot copy on `counter == 0` — during the cushion every frame runs all 8 GPU compute stages but skips the snapshot, leaving `prev_image = None`. `fire_session_reset` calls the new API; the test's structurally-unreachable `codec_list.contains(&2)` assertion is dropped (warm-cache-bundling coverage queued as separate follow-up).

**Tech Stack:** Rust, Vulkan (ash bindings), tokio, Vitest, chromiumoxide, testcontainers.

**Spec:** `docs/superpowers/specs/2026-05-20-palrle-session-reset-design.md`

---

## File Structure

**Modify:**
- `ghostframe-lib/src/capture/gpu_pipeline/mod.rs` — add `force_all_dirty_remaining` field; add public `reset_for_session` method.
- `ghostframe-lib/src/capture/gpu_pipeline/frame.rs` — change `run_first_frame_passes` signature to `Option<&PrevFrame>`, gate the snapshot-copy command on `Some`; update the single caller at line ~243 to use the cushion counter.
- `ghostframe-lib/src/capture/gpu_pipeline/tests.rs` — add unit test for the cushion behaviour (Vulkan-soft-skip pattern).
- `ghostframe-lib/src/transport/io_bridge.rs` — wire `reset_for_session(20)` into `fire_session_reset`; unconditional `self.force_dirty_frames = 20`.
- `ghostframe-e2e/tests/e2e.rs` — remove `#[ignore]` from `e2e_palrle_session_reset`; drop the codec-list assertion; update docstring.

**Container rebuild required** before re-running the e2e test (server-side Rust code changes flow into the test-server container image).

---

## Task 1: Add `force_all_dirty_remaining` field + `reset_for_session` API

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/mod.rs:365-384` (struct fields), `:468-490` (impl block)
- Test: `ghostframe-lib/src/capture/gpu_pipeline/tests.rs` (append)

- [ ] **Step 1: Write the failing test**

Append to `ghostframe-lib/src/capture/gpu_pipeline/tests.rs`:

```rust
#[test]
fn reset_for_session_drops_prev_image_and_sets_counter() {
    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping reset_for_session test (no Vulkan GPU?): {e}");
            return;
        }
    };

    // Counter starts at 0; prev_image starts None.
    assert_eq!(processor.force_all_dirty_remaining, 0);
    assert!(processor.prev_image.is_none());

    // Drive one frame to populate prev_image.
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;
    let pixel: [u8; 4] = [0, 0, 255, 255];
    unsafe {
        let fd = make_memfd(width, height, pixel);
        let result = processor.diff(fd, width, height, stride);
        libc::close(fd);
        match result {
            Ok(_) => {}
            Err(e) => {
                eprintln!("Skipping reset_for_session test (memfd not DMA-BUF?): {e}");
                return;
            }
        }
    }
    assert!(processor.prev_image.is_some(), "diff should populate prev_image");

    // The API under test.
    processor.reset_for_session(20);

    assert!(processor.prev_image.is_none(), "reset_for_session must drop prev_image");
    assert_eq!(processor.force_all_dirty_remaining, 20);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ghostframe-lib --lib gpu_pipeline::tests::reset_for_session_drops_prev_image_and_sets_counter`

Expected: compilation error — `field 'force_all_dirty_remaining' on type 'GpuFrameProcessor'` doesn't exist; or `method 'reset_for_session' not found`.

- [ ] **Step 3: Add the field**

In `ghostframe-lib/src/capture/gpu_pipeline/mod.rs`, find the `GpuFrameProcessor` struct (line 246, fields ending around 384). Add the new field next to `last_height`:

```rust
    last_width: u32,
    last_height: u32,

    /// Counter for the post-session-reset cushion. While > 0, the first-frame
    /// branch in `process_frame_with_imported` skips the snapshot copy and
    /// leaves `prev_image = None`, so every frame is reported fully dirty
    /// without committing a new baseline. Mirrors the CPU path's
    /// `force_dirty_frames` no-commit semantics.
    force_all_dirty_remaining: u32,
}
```

In the constructor — find `new_inner` in `setup.rs` and the struct-literal initialization (look for `prev_image: None,` around line 603). Add the field initialization:

```rust
            prev_image: None,
            // ...
            last_width: 0,
            last_height: 0,
            force_all_dirty_remaining: 0,
        })
```

- [ ] **Step 4: Add the public API**

In `ghostframe-lib/src/capture/gpu_pipeline/mod.rs`, inside `impl GpuFrameProcessor` (starts line 468), after the `diff` method (around line 510), add:

```rust
    /// Reset per-session state on a new WebTransport session, matching the
    /// CPU path's `dirty_tracker.reset() + force_dirty_frames = N` shape.
    ///
    /// Drops the cached `prev_image` (freeing its Vulkan resources) AND sets
    /// the no-commit cushion to `force_frames` frames. During the cushion,
    /// every frame is reported as fully dirty without snapshotting
    /// `prev_image`, so datagrams dropped by QUIC slow-start naturally
    /// resurface as dirty on subsequent frames.
    pub fn reset_for_session(&mut self, force_frames: u32) {
        unsafe {
            // device_wait_idle ensures no in-flight GPU work still references
            // the prev_image we're about to destroy. The Drop impl uses the
            // same pattern.
            self.device.device_wait_idle().ok();
            if let Some(prev) = self.prev_image.take() {
                self.destroy_prev_frame(prev);
            }
        }
        self.force_all_dirty_remaining = force_frames;
    }
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p ghostframe-lib --lib gpu_pipeline::tests::reset_for_session_drops_prev_image_and_sets_counter`

Expected: PASS (or "Skipping ..." print if no Vulkan device is available on the host — that's also a pass on this host).

- [ ] **Step 6: Run existing gpu_pipeline tests to verify no regression**

Run: `cargo test -p ghostframe-lib --lib gpu_pipeline::`

Expected: all existing tests still pass (or skip with the "no Vulkan GPU" message).

- [ ] **Step 7: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline/mod.rs ghostframe-lib/src/capture/gpu_pipeline/setup.rs ghostframe-lib/src/capture/gpu_pipeline/tests.rs
git commit -m "$(cat <<'EOF'
feat(gpu_pipeline): add reset_for_session API

GpuFrameProcessor gains force_all_dirty_remaining: u32 and a public
reset_for_session(force_frames) that drops prev_image (freeing its
Vulkan resources via the existing destroy_prev_frame helper) and sets
the counter. Counter consumption lands in Task 3; this task ships the
API surface plus a unit test that covers field initialization, the
prev_image drop, and counter set.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Refactor `run_first_frame_passes` to accept `Option<&PrevFrame>`

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/frame.rs:989` (signature) and the snapshot-copy command block inside it; `:243` (sole caller)

This is a behaviour-preserving refactor. The existing first-frame branch always passes `Some(&snapshot)`, so all existing tests continue to exercise the same path. The new `None` arm is wired up in Task 3.

- [ ] **Step 1: Find and read the function**

Open `ghostframe-lib/src/capture/gpu_pipeline/frame.rs` around line 989. Read the full body of `run_first_frame_passes`. Locate the snapshot-copy section (search inside the function for `prev_image` or `copy_image`; per the spec the snapshot copy step is one of the recorded commands, likely near the end of the command-buffer block, around the same area as the `// 5a. Snapshot copy:` comment at line 689 in `process_frame_with_imported` — the first-frame variant should have a similar block).

- [ ] **Step 2: Change the signature**

Replace the function signature:

```rust
    unsafe fn run_first_frame_passes(
        &self,
        current: &PrevFrame,
        snapshot: &PrevFrame,
        geom: FrameGeometry,
        nv12: Nv12OutputLayout,
    ) -> Result<(), Box<dyn std::error::Error>> {
```

with:

```rust
    unsafe fn run_first_frame_passes(
        &self,
        current: &PrevFrame,
        snapshot: Option<&PrevFrame>,
        geom: FrameGeometry,
        nv12: Nv12OutputLayout,
    ) -> Result<(), Box<dyn std::error::Error>> {
```

- [ ] **Step 3: Gate the snapshot-copy command block on `Some`**

Inside the function, the snapshot-copy section spans approximately `frame.rs:1364` (comment `// Snapshot copy: current (GENERAL) → snapshot (TRANSFER_DST_OPTIMAL).`) through `frame.rs:1448` (end of the post-copy `cmd_pipeline_barrier` that restores the snapshot to `GENERAL`). It comprises three logical parts, all of which reference `snapshot.image`:

1. `snap_barriers` array + `cmd_pipeline_barrier` (current → `TRANSFER_SRC_OPTIMAL`, snapshot → `TRANSFER_DST_OPTIMAL`).
2. `copy_region` + `cmd_copy_image` (the actual `current` → `snapshot` blit).
3. `post_copy_barrier` + `cmd_pipeline_barrier` (snapshot back to `GENERAL` for the next frame's SAD pass).

Wrap the entire block (all three parts) in:

```rust
if let Some(snap) = snapshot {
    // ... existing snap_barriers / cmd_pipeline_barrier ...
    // ... existing cmd_copy_image ...
    // ... existing post_copy_barrier / cmd_pipeline_barrier ...
}
```

Rename the inner usages from `snapshot.image` to `snap.image` to avoid shadowing the outer `snapshot: Option<&PrevFrame>` parameter with a `&PrevFrame` of the same name.

All other stages (NV12 at line ~1056, tile_analysis, palrle_compact, palrle_indirect_args, palette_fold, palette_subset_fold_init/fold, pal_rle_index, and the trailing `buf_barrier`) stay outside the conditional — they only read from `current` and write to HOST-visible output buffers, never touching `snapshot`. The `current.image` layout is `GENERAL` going into the conditional and remains `GENERAL` if the block is skipped (no transition to `TRANSFER_SRC_OPTIMAL` happens). The subsequent `buf_barrier` doesn't depend on `current`'s layout, so the skip is layout-safe.

- [ ] **Step 4: Update the sole caller**

At `frame.rs:243` (inside the `if self.prev_image.is_none()` branch), the current call:

```rust
            self.run_first_frame_passes(current, &snapshot, geom, nv12_layout)?;
```

becomes:

```rust
            self.run_first_frame_passes(current, Some(&snapshot), geom, nv12_layout)?;
```

Don't change anything else yet — Task 3 introduces the conditional that may pass `None`.

- [ ] **Step 5: Run existing gpu_pipeline tests**

Run: `cargo test -p ghostframe-lib --lib gpu_pipeline::`

Expected: all pass. The refactor is behaviour-preserving; existing `identical_frames_produce_no_dirty_tiles` and friends still construct the first-frame snapshot via `Some(&snapshot)`.

- [ ] **Step 6: Run the broader lib test suite for confidence**

Run: `cargo test -p ghostframe-lib --lib`

Expected: same number of passes as before this task.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline/frame.rs
git commit -m "$(cat <<'EOF'
refactor(gpu_pipeline): make snapshot optional in run_first_frame_passes

Threads Option<&PrevFrame> through run_first_frame_passes so the
snapshot-copy command block can be skipped when no snapshot is provided.
Behaviour-preserving refactor — the sole caller still passes
Some(&snapshot). The None arm is wired up in the next commit (session
cushion).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Wire the counter into the first-frame branch

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/frame.rs:233-247` (first-frame branch in `process_frame_with_imported`)
- Test: `ghostframe-lib/src/capture/gpu_pipeline/tests.rs` (append cushion test)

- [ ] **Step 1: Write the failing cushion-behaviour test**

Append to `ghostframe-lib/src/capture/gpu_pipeline/tests.rs`:

```rust
#[test]
fn reset_for_session_cushion_keeps_all_dirty_and_prev_image_none() {
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;
    let pixel: [u8; 4] = [0, 0, 255, 255];
    let tile_count = (width / 32) * (height / 32); // 2 * 2 = 4

    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping cushion test (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        // Prime: first frame populates prev_image and reports all dirty.
        let fd = make_memfd(width, height, pixel);
        let dirty = match processor.diff(fd, width, height, stride) {
            Ok(v) => v,
            Err(e) => {
                libc::close(fd);
                eprintln!("Skipping cushion test (memfd not DMA-BUF?): {e}");
                return;
            }
        };
        libc::close(fd);
        assert_eq!(dirty.len() as u32, tile_count, "first frame: all dirty");
        assert!(processor.prev_image.is_some(), "first frame populates prev_image");

        // Sanity: second identical frame produces 0 dirty.
        let fd = make_memfd(width, height, pixel);
        let dirty = processor.diff(fd, width, height, stride).unwrap();
        libc::close(fd);
        assert_eq!(dirty.len(), 0, "identical content: SAD reports 0 dirty");

        // Engage the cushion.
        processor.reset_for_session(3);
        assert!(processor.prev_image.is_none());

        // Cushion frames 1..=3: all dirty, prev_image stays None.
        for i in 1..=3 {
            let fd = make_memfd(width, height, pixel);
            let dirty = processor.diff(fd, width, height, stride).unwrap();
            libc::close(fd);
            assert_eq!(
                dirty.len() as u32,
                tile_count,
                "cushion frame {i}: should be all dirty"
            );
            assert!(
                processor.prev_image.is_none(),
                "cushion frame {i}: prev_image must stay None during cushion"
            );
        }
        assert_eq!(processor.force_all_dirty_remaining, 0);

        // Frame 4: counter is 0, prev_image is still None → first-frame-with-snapshot.
        // All dirty AND prev_image becomes Some.
        let fd = make_memfd(width, height, pixel);
        let dirty = processor.diff(fd, width, height, stride).unwrap();
        libc::close(fd);
        assert_eq!(
            dirty.len() as u32,
            tile_count,
            "post-cushion first commit frame: all dirty"
        );
        assert!(
            processor.prev_image.is_some(),
            "post-cushion first commit frame: prev_image must be Some"
        );

        // Frame 5: normal SAD against the just-committed snapshot.
        let fd = make_memfd(width, height, pixel);
        let dirty = processor.diff(fd, width, height, stride).unwrap();
        libc::close(fd);
        assert_eq!(dirty.len(), 0, "normal SAD resumed: 0 dirty for unchanged content");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ghostframe-lib --lib gpu_pipeline::tests::reset_for_session_cushion_keeps_all_dirty_and_prev_image_none`

Expected: FAIL — likely on the first cushion-frame assertion (`prev_image.is_none()`), because the existing first-frame branch still unconditionally allocates and commits the snapshot.

- [ ] **Step 3: Update the first-frame branch**

In `ghostframe-lib/src/capture/gpu_pipeline/frame.rs`, find the `if self.prev_image.is_none()` block (around line 233, inside `process_frame_with_imported`). The current shape is:

```rust
        if self.prev_image.is_none() {
            let all_dirty: Vec<u32> = (0..tile_count).collect();

            // Allocate the owned snapshot image once. It persists for the
            // lifetime of the processor (until resolution changes).
            let snapshot = self.allocate_owned_image(width, height)?;

            // Run NV12 conversion + tile_analysis + snapshot copy in one cmd
            // buffer. The snapshot is what we will compare future frames
            // against.
            self.run_first_frame_passes(current, Some(&snapshot), geom, nv12_layout)?;

            let mut snap = snapshot;
            snap.layout = vk::ImageLayout::GENERAL;
            self.prev_image = Some(snap);

            return Ok(FrameAnalysis {
                dirty_tiles: all_dirty,
                ...
            });
        }
```

Replace with:

```rust
        if self.prev_image.is_none() {
            let all_dirty: Vec<u32> = (0..tile_count).collect();

            // Session-reset cushion: while the counter is > 0, run all stages
            // but DO NOT snapshot the current frame as the new baseline.
            // prev_image stays None so the next frame hits this branch again
            // and re-emits all tiles dirty. Mirrors the CPU path's
            // `force_dirty_frames` no-commit semantics — datagrams dropped
            // during QUIC slow-start re-surface as dirty until a real
            // commit-frame snapshot lands.
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

            return Ok(FrameAnalysis {
                dirty_tiles: all_dirty,
                nv12_data: nv12_ptr,
                nv12_width: width,
                nv12_height: height,
                nv12_y_stride,
                nv12_uv_stride,
                nv12_uv_offset,
                tile_analysis: self.analysis_ptr as *const TileAnalysis,
                tile_analysis_len: cols * rows,
                palrle_compact_list: self.palrle_compact_list_ptr as *const u32,
                palrle_compact_count: *self.palrle_compact_count_ptr,
                frame_palette_set: self.frame_palette_set_ptr as *const FramePaletteEntryRaw,
                frame_palette_set_count: *self.frame_palette_count_ptr,
                per_tile_frame_palette_id: self.per_tile_frame_palette_id_ptr as *const u8,
                folded_into: self.folded_into_ptr as *const u32,
                index_buffer: self.index_buffer_ptr as *const u8,
            });
        }
```

The `FrameAnalysis` field initializers are unchanged from the existing code — they're already populated by the stages that ran above this point regardless of snapshot mode. Only the snapshot-related lines (allocation, `run_first_frame_passes` arg, `prev_image` assignment) and the counter decrement are new.

- [ ] **Step 4: Run the new test to verify it passes**

Run: `cargo test -p ghostframe-lib --lib gpu_pipeline::tests::reset_for_session_cushion_keeps_all_dirty_and_prev_image_none`

Expected: PASS (or "Skipping ..." if no Vulkan device).

- [ ] **Step 5: Run all gpu_pipeline tests + broader lib tests**

Run: `cargo test -p ghostframe-lib --lib gpu_pipeline::` then `cargo test -p ghostframe-lib --lib`

Expected: all pass with the same count as before, plus the new test.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline/frame.rs ghostframe-lib/src/capture/gpu_pipeline/tests.rs
git commit -m "$(cat <<'EOF'
feat(gpu_pipeline): consume force_all_dirty_remaining cushion in first-frame path

While the counter is > 0, the first-frame branch in process_frame_with_imported
skips snapshot allocation + commit, so prev_image stays None and the next
frame hits this branch again. Mirrors the CPU path's force_dirty_frames
no-commit semantics — N frames of all-tiles-dirty + the (N+1)th frame is
the first real baseline.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Wire `reset_for_session(20)` into `fire_session_reset`

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs:858-865` (the existing CPU-gated `force_dirty_frames` block inside `fire_session_reset`)

- [ ] **Step 1: Read the current block**

In `ghostframe-lib/src/transport/io_bridge.rs`, find `fn fire_session_reset` (around line 827). The relevant block is lines 858-865:

```rust
        // `force_dirty_frames` is consumed only by
        // `process_frame_cpu` (no_commit slow-start mitigation).
        // The GPU path doesn't read it — skip setting it when a
        // GPU processor is active to avoid implying behavior that
        // doesn't fire on that branch.
        if self.gpu_frame_processor.is_none() {
            self.force_dirty_frames = 20;
        }
```

- [ ] **Step 2: Replace with the symmetric block**

```rust
        // Slow-start cushion: 20 frames of all-tiles-dirty so a tile
        // dropped during QUIC slow-start re-emerges as dirty on the next
        // frame. Each path has its own implementation — CPU's
        // `force_dirty_frames` runs dirty_tracker in no-commit mode;
        // GPU's `reset_for_session` drops prev_image and runs the
        // no-snapshot first-frame path for N frames. Set both
        // unconditionally — `process_frame_gpu` ignores `force_dirty_frames`
        // and `gpu_frame_processor` is None on the CPU-only path, so the
        // wrong-path setter is harmless either way.
        self.force_dirty_frames = 20;
        if let Some(p) = self.gpu_frame_processor.as_mut() {
            p.reset_for_session(20);
        }
```

- [ ] **Step 3: Run lib tests (sanity check)**

Run: `cargo test -p ghostframe-lib --lib`

Expected: same count as before — this is a wiring change, no new lib test asserts it directly. The e2e test in Task 5 is the integration check.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "$(cat <<'EOF'
feat(io_bridge): wire reset_for_session into fire_session_reset

Both the CPU and GPU pipelines now get their 20-frame slow-start cushion
on every session reset. Drops the previous CPU-only gate — process_frame_gpu
ignores force_dirty_frames so setting it unconditionally is harmless, and
the GPU path needs the analogous reset_for_session call to drop its Vulkan
SAD prev_image. Closes the gap that caused e2e_palrle_session_reset to
fail with a blank post-reset canvas (static content + stale prev_image
→ 0 dirty → early return → queued IDR never consumed).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Close out the e2e test

**Files:**
- Modify: `ghostframe-e2e/tests/e2e.rs:1025-1100` (`e2e_palrle_session_reset` test + docstring)

- [ ] **Step 1: Rebuild the test-server container with the new lib code**

The xdaemon binary inside the container links the lib code we just changed.

```bash
cd /home/cedric/work/ghostframe
cargo build --release -p ghostframe-xdaemon -p ghostframe-test-pattern
docker build -t ghostframe/test-server -f tests/containers/test-server/Dockerfile .
```

Expected: container builds successfully.

- [ ] **Step 2: Clear any stale Xvfb sockets**

```bash
for n in $(seq 99 200); do rm -f /tmp/.X11-unix/X$n /tmp/.X$n-lock 2>/dev/null; done
```

- [ ] **Step 3: Update the docstring, remove the ignore, drop the codec assertion**

In `ghostframe-e2e/tests/e2e.rs` at the `e2e_palrle_session_reset` test (around line 1025-1100), apply this rewrite:

```rust
/// Force a client reconnect mid-stream; verify the post-reset rendering
/// pipeline survives — text remains legible after `page.reload()`.
///
/// Closure path: `fire_session_reset` calls
/// `GpuFrameProcessor::reset_for_session(20)`, which drops the Vulkan SAD
/// `prev_image` and runs the no-snapshot first-frame path for 20 frames
/// (mirroring the CPU path's `force_dirty_frames` cushion). On reset the
/// classifier is forced to `FrameMode::H264` with a one-shot `request_keyframe`,
/// so the post-reset emission is an H.264 IDR carrying the text content;
/// PalRle wire emission does not fire for static content under this load-
/// shedding rule. The warm-cache-bundling scenario (post-reset bundled
/// PalRle re-delivery) needs dynamic content to fire any PalRle at all and
/// is covered by a separate follow-up test.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_palrle_session_reset() -> Result<()> {
    use ghostframe_test_pattern::text_grid::SAMPLES;

    let setup = setup_e2e_webgpu_gpu("--text-grid --drm-direct").await?;

    // Phase 1: let the initial connection settle and render text.
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Capture a baseline luminance reading.
    let pair = &SAMPLES[0];
    let probe_js = format!(
        r#"
        (async () => {{
            const ink = await window.__readPixel({ix}, {iy});
            const bg  = await window.__readPixel({bx}, {by});
            return {{
                ink: {{ r: ink[0], g: ink[1], b: ink[2] }},
                bg:  {{ r: bg[0],  g: bg[1],  b: bg[2]  }},
            }};
        }})()
        "#,
        ix = pair.ink.0, iy = pair.ink.1,
        bx = pair.bg.0,  by = pair.bg.1,
    );
    let baseline: serde_json::Value =
        setup.page.evaluate(probe_js.as_str()).await?.into_value()?;
    let baseline_ink_lum = luminance(&baseline["ink"]);
    let baseline_bg_lum = luminance(&baseline["bg"]);
    assert!(
        baseline_ink_lum - baseline_bg_lum > 80.0,
        "baseline text not legible — pre-reset assertion failed"
    );

    // Phase 2: force a reconnect by reloading the page.
    setup.page.reload().await?;

    // Allow the new session to settle + re-render.
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Phase 3: assert post-reset legibility. Post-reset emission is the
    // H.264 IDR triggered by fire_session_reset; H.264 is lossy but text
    // contrast survives.
    let post: serde_json::Value =
        setup.page.evaluate(probe_js.as_str()).await?.into_value()?;
    let post_ink_lum = luminance(&post["ink"]);
    let post_bg_lum = luminance(&post["bg"]);
    assert!(
        post_ink_lum - post_bg_lum > 80.0,
        "post-reset text not legible — session reset broke the rendering pipeline (ink={post_ink_lum:.0}, bg={post_bg_lum:.0})"
    );

    Ok(())
}
```

Key changes vs. existing file:
- Replace the `// M3.2c follow-up: ...` comment block + `#[ignore = "..."]` with the new docstring (no `#[ignore]`).
- Drop the `// Protocol-layer: post-reset codec stream should include PalRle.` block at the end (the `let codec_list ... assert!(codec_list.contains(&2u8), ...)` lines).
- Adjust the post-reset legibility assertion's failure message to reflect the actual failure mode being guarded against.

- [ ] **Step 4: Compile-check**

Run: `cargo check -p ghostframe-e2e --tests`

Expected: clean compile, no warnings about unused `setup` or similar from the dropped block.

- [ ] **Step 5: Run the test**

Run:

```bash
cargo test -p ghostframe-e2e --test e2e e2e_palrle_session_reset -- --test-threads=1 --nocapture
```

Expected: PASS. Both pre-reset and post-reset luminance probes succeed.

If FAIL on post-reset: check `docker logs ghostframe-server` for whether the H.264 IDR was emitted on the new session (look for `seq=1` ish lines + `is_keyframe: true`). If no IDR, the wiring in Task 4 may not be effective — verify the commits from Tasks 1-4 are all present in the container by checking the build log timestamps.

- [ ] **Step 6: Run the full e2e suite to verify no regression**

```bash
for n in $(seq 99 200); do rm -f /tmp/.X11-unix/X$n /tmp/.X$n-lock 2>/dev/null; done
cargo test -p ghostframe-e2e --test e2e -- --test-threads=1
```

Expected: all previously-passing e2e tests still pass; `e2e_palrle_session_reset` now passes; `e2e_resolution_change` remains the single `#[ignore]`'d test.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-e2e/tests/e2e.rs
git commit -m "$(cat <<'EOF'
test(e2e): close e2e_palrle_session_reset

Removes the #[ignore]; the GPU SAD prev_image is now reset on session
reset (commits in this branch). Drops the codec_list contains(PalRle)
assertion — under the existing H264-on-reset load-shedding rule, static
text content never emits PalRle post-reset; the assertion was
structurally unreachable. The test's load-bearing post-reset legibility
probe remains; warm-cache-bundling coverage is queued as a separate
follow-up requiring a dynamic content test pattern.

Docstring rewritten to reflect the actual closure mechanism (H.264 IDR
post-reset via fire_session_reset → reset_for_session(20)) and the
deferred warm-cache test.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review against spec

- **CPU semantics match** (spec § "CPU path semantics" + § "GPU path matching semantics"): Tasks 1+3 produce 20 cushion frames of all-dirty + prev_image-stays-None, plus a 21st commit frame, matching CPU's 20 no-commit + frame-21-first-commit shape. Test in Task 3 step 1 explicitly exercises this with N=3 and verifies the post-cushion commit + normal-SAD resume.
- **Combined API D2**: Task 1 ships `reset_for_session(force_frames)` as one method, not two.
- **`Option<&PrevFrame>` D3**: Task 2 changes the signature; Task 3 uses `None` during the cushion.
- **Unconditional `force_dirty_frames = 20` D4**: Task 4 step 2.
- **Force-frames = 20 D5**: Task 4 step 2 passes 20.
- **Test docstring update + drop codec assertion D7**: Task 5 step 3.
- **EMA side-effect D6**: documented in the spec; no code change planned. Verified.
- **No placeholders, no TBDs, no "similar to Task N"**: each task's code is self-contained. Verified.

---

## Out of scope (not in this plan, queued elsewhere)

- `e2e_palrle_warm_cache_after_reset` (the deferred follow-up test that asserts bundled-then-thin PalRle re-delivery against a dynamic test pattern).
- `e2e_resolution_change` closure (separate brainstorm cycle, next on the backlog).
- Audit other `GpuFrameProcessor` cross-session state beyond `prev_image`.
