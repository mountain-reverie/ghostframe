# H264 → TileCodec Lossy-to-Lossless Handoff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the architectural invariant that every H264→TileCodec classifier transition invalidates the GPU SAD baseline so lossless TileCodec content overwrites the H.264 lossy render. Close `e2e_palrle_session_reset` as a side effect.

**Architecture:** Rename `GpuFrameProcessor::reset_for_session(N)` to `invalidate_baseline(N)` so the name describes the mechanism (the function is used from two call sites — session-reset cushion and mode-flip handoff). Add a new call site in `process_frame_gpu` that fires `invalidate_baseline(1)` when the classifier transitions from `FrameMode::H264` to `FrameMode::TileCodec`. The e2e test's original `codec_list.contains(&Codec::PalRle)` post-reset assertion becomes reachable because PalRle tiles flow ~1s post-reset (after exit_sustain elapses and the handoff fires).

**Tech Stack:** Rust, Vulkan (ash bindings), tokio, chromiumoxide, testcontainers.

**Spec:** `docs/superpowers/specs/2026-05-25-h264-tilecodec-handoff-design.md`

---

## File Structure

**Modify:**
- `ghostframe-lib/src/capture/gpu_pipeline/mod.rs` — rename `reset_for_session` → `invalidate_baseline` (API + doc comment).
- `ghostframe-lib/src/capture/gpu_pipeline/tests.rs` — rename two unit tests + their internal call sites + skip messages.
- `ghostframe-lib/src/transport/io_bridge.rs` — rename the call site in `fire_session_reset`; update its comment block; add new call site in `process_frame_gpu` for the H264→TileCodec mode-flip handoff.
- `ghostframe-e2e/tests/e2e.rs` — remove `#[ignore]` from `e2e_palrle_session_reset`; update its docstring; restore the codec_list assertion (already there since revert).

**Container rebuild required** before re-running the e2e test (the io_bridge change is server-side).

---

## Task 1: Rename `reset_for_session(N)` → `invalidate_baseline(N)`

Pure rename across 5 files. No behavior change. Tasks 2-3 then use the new name.

**Files:**
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/mod.rs:519-538` (method + doc comment)
- Modify: `ghostframe-lib/src/capture/gpu_pipeline/tests.rs:1578-1686` (two test names + internal call sites + skip messages)
- Modify: `ghostframe-lib/src/transport/io_bridge.rs:858-874` (call site + comment)

- [ ] **Step 1: Replace the method on `GpuFrameProcessor`**

In `ghostframe-lib/src/capture/gpu_pipeline/mod.rs`, replace lines 519-538:

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

with:

```rust
    /// Invalidate the GPU SAD baseline by dropping `prev_image` and arming the
    /// no-snapshot cushion for the next `force_frames` frames.
    ///
    /// While the cushion is active, every frame is reported as fully dirty
    /// without snapshotting a new baseline — datagrams dropped during the
    /// cushion period (e.g. QUIC slow-start, or a lossy→lossless repaint
    /// burst) naturally re-surface as dirty until the cushion exhausts and
    /// the next frame becomes the first real snapshot.
    ///
    /// Two call sites today:
    /// - `fire_session_reset` calls with `force_frames = 20` to cover QUIC
    ///   slow-start datagram loss after a new session connects.
    /// - The H264 → TileCodec mode-flip handoff in `process_frame_gpu` calls
    ///   with `force_frames = 1` to trigger a one-shot lossless full-repaint
    ///   that overwrites the H.264 lossy render.
    pub fn invalidate_baseline(&mut self, force_frames: u32) {
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

- [ ] **Step 2: Rename test 1**

In `ghostframe-lib/src/capture/gpu_pipeline/tests.rs`, replace:

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

with:

```rust
#[test]
fn invalidate_baseline_drops_prev_image_and_sets_counter() {
    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping invalidate_baseline test (no Vulkan GPU?): {e}");
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
                eprintln!("Skipping invalidate_baseline test (memfd not DMA-BUF?): {e}");
                return;
            }
        }
    }
    assert!(processor.prev_image.is_some(), "diff should populate prev_image");

    // The API under test.
    processor.invalidate_baseline(20);

    assert!(processor.prev_image.is_none(), "invalidate_baseline must drop prev_image");
    assert_eq!(processor.force_all_dirty_remaining, 20);
}
```

- [ ] **Step 3: Rename test 2**

In the same file, the second test that exercises the cushion behaviour at line 1618. Find:

```rust
#[test]
fn reset_for_session_cushion_keeps_all_dirty_and_prev_image_none() {
```

and rename to:

```rust
#[test]
fn invalidate_baseline_cushion_keeps_all_dirty_and_prev_image_none() {
```

Inside the test body, find:

```rust
        eprintln!("Skipping cushion test (no Vulkan GPU?): {e}");
```

(no rename needed — message is generic.)

Then find:

```rust
        // Engage the cushion.
        processor.reset_for_session(3);
```

and replace with:

```rust
        // Engage the cushion.
        processor.invalidate_baseline(3);
```

- [ ] **Step 4: Update the call site in `fire_session_reset`**

In `ghostframe-lib/src/transport/io_bridge.rs`, replace the existing block at lines 858-874:

```rust
        // Slow-start cushion: 20 frames of all-tiles-dirty so a tile
        // dropped during QUIC slow-start re-emerges as dirty on the
        // next frame. Each path has its own implementation — CPU's
        // `force_dirty_frames` runs dirty_tracker in no-commit mode;
        // GPU's `reset_for_session` drops prev_image and runs the
        // no-snapshot first-frame path for N frames.
        //
        // Both are set unconditionally. On the CPU-only path,
        // `gpu_frame_processor` is None so the `if let` is a no-op.
        // On the GPU path, `force_dirty_frames` is normally ignored
        // by `process_frame_gpu` — but IS consumed if the Vulkan
        // call errors and the frame falls back to `process_frame_cpu`,
        // which is exactly the correct behaviour on that error path.
        self.force_dirty_frames = 20;
        if let Some(p) = self.gpu_frame_processor.as_mut() {
            p.reset_for_session(20);
        }
```

with:

```rust
        // Slow-start cushion: 20 frames of all-tiles-dirty so a tile
        // dropped during QUIC slow-start re-emerges as dirty on the
        // next frame. Each path has its own implementation — CPU's
        // `force_dirty_frames` runs dirty_tracker in no-commit mode;
        // GPU's `invalidate_baseline` drops prev_image and runs the
        // no-snapshot first-frame path for N frames.
        //
        // Both are set unconditionally. On the CPU-only path,
        // `gpu_frame_processor` is None so the `if let` is a no-op.
        // On the GPU path, `force_dirty_frames` is normally ignored
        // by `process_frame_gpu` — but IS consumed if the Vulkan
        // call errors and the frame falls back to `process_frame_cpu`,
        // which is exactly the correct behaviour on that error path.
        self.force_dirty_frames = 20;
        if let Some(p) = self.gpu_frame_processor.as_mut() {
            p.invalidate_baseline(20);
        }
```

- [ ] **Step 5: Verify build + tests**

Run: `cargo test -p ghostframe-lib --lib gpu_pipeline::`

Expected: 20/20 tests pass (the two renamed tests run under their new names; all others unchanged).

- [ ] **Step 6: Run broader lib tests**

Run: `cargo test -p ghostframe-lib --lib`

Expected: 284 passed (same count as before the rename).

- [ ] **Step 7: Commit**

```bash
git add ghostframe-lib/src/capture/gpu_pipeline/mod.rs ghostframe-lib/src/capture/gpu_pipeline/tests.rs ghostframe-lib/src/transport/io_bridge.rs
git commit -m "$(cat <<'EOF'
refactor(gpu_pipeline): rename reset_for_session → invalidate_baseline

Pure rename across 5 sites. The method body is unchanged. With a second
call site landing next (the H264→TileCodec mode-flip handoff), the
trigger-name reset_for_session is misleading — the function is the
baseline-invalidation mechanism, not a session-reset primitive. The
trigger context lives at each call site.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Add the H264 → TileCodec mode-flip handoff

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs:1157-1167` (just after the classifier-flipped trace, before `self.frame_mode = new_mode`)

The handoff lives just after the classifier flip is detected and traced, and BEFORE `self.frame_mode = new_mode` writes the new mode. We need to read `prev_mode` (already captured at io_bridge.rs:1131) and the freshly-computed `new_mode`. The placement keeps the existing trace adjacent to the new handoff for readability.

- [ ] **Step 1: Read the current block to confirm placement**

Open `ghostframe-lib/src/transport/io_bridge.rs` around lines 1155-1170. The relevant code is:

```rust
        if new_mode != self.frame_mode {
            tracing::info!(
                prev = ?self.frame_mode,
                new = ?new_mode,
                seq,
                "classifier flipped frame mode"
            );
        }

        // Update frame mode for next frame's hysteresis reference.
        self.frame_mode = new_mode;
```

`prev_mode` is a local variable already captured at the function level (`let prev_mode = self.frame_mode;` further up at line 1131). `new_mode` is the result of `self.classifier.decide_frame_mode(&tentative, prev_mode)`.

- [ ] **Step 2: Add the handoff inside the existing `if new_mode != self.frame_mode` block**

Replace the block:

```rust
        if new_mode != self.frame_mode {
            tracing::info!(
                prev = ?self.frame_mode,
                new = ?new_mode,
                seq,
                "classifier flipped frame mode"
            );
        }

        // Update frame mode for next frame's hysteresis reference.
        self.frame_mode = new_mode;
```

with:

```rust
        if new_mode != self.frame_mode {
            tracing::info!(
                prev = ?self.frame_mode,
                new = ?new_mode,
                seq,
                "classifier flipped frame mode"
            );

            // H264 → TileCodec architectural invariant: invalidate the GPU SAD
            // baseline so the next TileCodec frame re-emits every tile,
            // overwriting whatever the H.264 phase rendered. Without this,
            // static content produces 0 dirty tiles on mode entry (prev_image
            // is current relative to the static screen) and the lossless tile
            // codecs never get a chance to upgrade the canvas from the H.264
            // lossy render. Symmetric to the existing `request_keyframe()` on
            // TileCodec → H264 at io_bridge.rs:1195-1199: each direction of the
            // mode flip resets the state that the entering mode relies on
            // (H.264 GOP for the H264 direction, GPU SAD baseline for the
            // TileCodec direction).
            //
            // force_frames = 1 (not 20) because we're past QUIC slow-start by
            // the time exit_sustain elapses; ACK-based retries handle
            // individual datagram drops on the lossless tiles.
            if prev_mode == crate::tile::FrameMode::H264
                && new_mode == crate::tile::FrameMode::TileCodec
            {
                if let Some(p) = self.gpu_frame_processor.as_mut() {
                    p.invalidate_baseline(1);
                    tracing::info!(
                        seq,
                        "H264→TileCodec handoff: invalidate_baseline(1) — lossless repaint"
                    );
                }
            }
        }

        // Update frame mode for next frame's hysteresis reference.
        self.frame_mode = new_mode;
```

Note on imports: `FrameMode` is already in scope via the existing `use crate::tile::{classifier::classify_tile, FrameMode};` at line 1119. The fully-qualified path `crate::tile::FrameMode::H264` works too if you prefer matching the style of the surrounding (unqualified) `FrameMode::H264` references at io_bridge.rs:1176, 1195. Pick whichever matches the immediate code style — they're equivalent.

- [ ] **Step 3: Compile check**

Run: `cargo check -p ghostframe-lib --lib`

Expected: clean compile (only the pre-existing dead_code warning on `phase_b_encode_payloads`).

- [ ] **Step 4: Run lib tests**

Run: `cargo test -p ghostframe-lib --lib`

Expected: 284 passed (no new tests in this task; the e2e closure in Task 3 is the integration check).

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "$(cat <<'EOF'
feat(io_bridge): H264 → TileCodec mode-flip handoff invariant

Every classifier transition from H264 to TileCodec mode now invalidates
the GPU SAD baseline (invalidate_baseline(1)) so the next TileCodec
frame re-emits every tile, overwriting the lossy H.264 render with
lossless content. Mirrors the existing TileCodec→H264 direction's
request_keyframe() pattern: each direction resets the state the entering
mode relies on (H.264 GOP, or GPU SAD baseline).

Without this, static content produces 0 dirty tiles on the first
TileCodec frame after exit_sustain elapses — the prev_image is current
relative to the static screen, SAD reports clean, and the canvas stays
at the H.264 lossy result indefinitely.

Spec: docs/superpowers/specs/2026-05-25-h264-tilecodec-handoff-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Close `e2e_palrle_session_reset`

**Files:**
- Modify: `ghostframe-e2e/tests/e2e.rs:1025-1100` (`e2e_palrle_session_reset` test + docstring)

- [ ] **Step 1: Rebuild the test-server container with the new lib code**

The xdaemon binary inside the container links the lib code we just changed.

```bash
cd /home/cedric/work/ghostframe
cargo build --release -p ghostframe-xdaemon -p ghostframe-test-pattern
docker build -t ghostframe/test-server -f tests/containers/test-server/Dockerfile .
```

Expected: container builds successfully (Dockerfile fix from commit `af51abb` for the workspace test-crate split is already in master).

- [ ] **Step 2: Clear any stale Xvfb sockets**

```bash
for n in $(seq 99 200); do rm -f /tmp/.X11-unix/X$n /tmp/.X$n-lock 2>/dev/null; done
```

- [ ] **Step 3: Update the test (remove ignore, refresh docstring, keep both assertions)**

In `ghostframe-e2e/tests/e2e.rs`, replace the existing test at lines 1025-1100. The current shape:

```rust
/// Force a client reconnect mid-stream; verify the server's warm-cache
/// palette table re-delivers palettes on the new session and the text
/// region renders correctly post-reset.
// M3.2c follow-up: post-page-reload the server doesn't re-emit static
// content (SAD sees no dirty tiles since text_grid_drm content is
// unchanged), so the new client receives nothing for text tiles and the
// canvas stays blank.  Needs server-side "new session → re-classify all
// tiles" logic (probably M3.5).  See project_m32c_deferred.md.
#[ignore = "M3.2c follow-up: post-reset server doesn't re-emit static content; needs session-aware re-classification"]
#[tokio::test(flavor = "multi_thread")]
async fn e2e_palrle_session_reset() -> Result<()> {
    use ghostframe_test_pattern::text_grid::SAMPLES;

    let setup = setup_e2e_webgpu_gpu("--text-grid --drm-direct").await?;

    // Phase 1: let the initial connection settle and render text.
    tokio::time::sleep(Duration::from_secs(4)).await;
```

Replace the docstring + ignore + opening lines (everything from `/// Force a client...` to `Phase 1: let the initial...` and the sleep that follows) with the new closure:

```rust
/// Force a client reconnect mid-stream; verify the H264 → TileCodec
/// mode-flip handoff repaints the canvas with lossless content
/// post-reset.
///
/// Closure path:
/// - `fire_session_reset` forces `frame_mode = H264` + `request_keyframe`
///   for the initial post-reset burst (lossy IDR + P-frames).
/// - After `exit_sustain_frames = 30` of static content, the classifier
///   transitions H264 → TileCodec. The new mode-flip handoff
///   (`io_bridge.rs:1166`-ish) invalidates the GPU SAD baseline via
///   `invalidate_baseline(1)`, so the next TileCodec frame reports all
///   tiles dirty and PalRle emissions repaint the canvas with lossless
///   content.
///
/// The 4s post-reset wait covers both phases (H.264 burst ~1s, handoff at
/// ~1s, lossless repaint over the next ~250ms).
///
/// Spec: docs/superpowers/specs/2026-05-25-h264-tilecodec-handoff-design.md
#[tokio::test(flavor = "multi_thread")]
async fn e2e_palrle_session_reset() -> Result<()> {
    use ghostframe_test_pattern::text_grid::SAMPLES;

    let setup = setup_e2e_webgpu_gpu("--text-grid --drm-direct").await?;

    // Phase 1: let the initial connection settle and render text.
    tokio::time::sleep(Duration::from_secs(4)).await;
```

Both assertions in the rest of the test body stay exactly as they are (the post-reset luminance probe AND the `codec_list.contains(&2u8)` PalRle assertion). The PalRle assertion is now reachable because the mode-flip handoff causes PalRle emission ~1s post-reset.

- [ ] **Step 4: Compile-check**

Run: `cargo check -p ghostframe-e2e --tests`

Expected: clean compile.

- [ ] **Step 5: Run the test**

```bash
cargo test -p ghostframe-e2e --test e2e e2e_palrle_session_reset -- --test-threads=1 --nocapture
```

Expected: PASS. Both the post-reset luminance probe and the `codec_list.contains(&2u8)` PalRle assertion succeed.

If the test fails on the codec assertion (no PalRle in codec_list): verify the server log shows the `H264→TileCodec handoff: invalidate_baseline(1) — lossless repaint` tracing line from Task 2 (look in `docker logs ghostframe-server`). If that log is absent, the handoff didn't fire — most likely the classifier never exited H.264 within the 4s window. The wait might need lengthening, or the cushion behavior needs investigation.

If the test fails on legibility but the codec assertion would have passed: PalRle tiles did flow but didn't render correctly. Check the web client's PalRle path for regressions.

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
test(e2e): close e2e_palrle_session_reset via H264 → TileCodec handoff

Removes the #[ignore]; both original assertions (post-reset legibility
+ codec_list.contains(PalRle)) now pass. The PalRle assertion is
reachable because the new H264→TileCodec mode-flip handoff (Task 2)
fires invalidate_baseline(1) when the classifier exits H.264, causing
the next TileCodec frame to re-emit every tile as PalRle/Solid/Raw
over the lossy H.264 result.

Docstring rewritten to reflect the actual closure mechanism (mode-flip
handoff) and point at the design spec.

Spec: docs/superpowers/specs/2026-05-25-h264-tilecodec-handoff-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review against spec

- **Architectural invariant (spec §"The invariant")**: Task 2 wires it.
- **API rename (spec §"Components 1")**: Task 1 does it.
- **Mode-flip handoff placement (spec §"Components 2")**: Task 2 places it inside the `if new_mode != self.frame_mode` block, after the existing trace, before the `self.frame_mode = new_mode` write — matching the spec.
- **`force_frames = 1` (D2)**: Task 2 calls `invalidate_baseline(1)`.
- **`fire_session_reset` keeps `invalidate_baseline(20)` (spec §"Components 3")**: Task 1 step 4 (the renamed call site keeps `20`).
- **D4 — only H264 → TileCodec direction**: Task 2's `if prev_mode == H264 && new_mode == TileCodec` enforces this.
- **D5 — both original e2e assertions kept**: Task 3 step 3 explicitly keeps both.
- **D6 — testing approach (counter or tracing probe; e2e is load-bearing)**: Task 2 emits a `tracing::info!` log on every handoff fire; Task 3's e2e is the load-bearing verification. The diagnostic failure-mode at Task 3 step 5 leverages the log to triangulate failures.
- **CPU path unchanged (spec §"Architecture — CPU path")**: no edits to `process_frame_cpu` in any task.
- **Inverse direction unchanged (spec D4)**: Task 2's `if prev_mode == H264 && new_mode == TileCodec` only fires on the new direction; the existing TileCodec→H264 `request_keyframe()` at io_bridge.rs:1195-1199 is untouched.

No placeholders, no TBDs, no "similar to Task N". All identifiers (`invalidate_baseline`, `force_all_dirty_remaining`, `prev_mode`, `new_mode`, `FrameMode::H264`, `FrameMode::TileCodec`, `gpu_frame_processor`, `Codec::PalRle (2)`) are consistent across tasks.

---

## Out of scope (queued elsewhere)

- **`e2e_resolution_change` closure** — separate brainstorm cycle, next on the backlog (xdaemon doesn't notice xrandr-driven X server resolution changes).
- **Stale per-tile `H264TileDecoder` client cleanup** — Task #7 in the task list; M3.0 cleanup that was missed on the client side. Independent cycle.
- **M4 §6.5 bandwidth estimator** to replace `exit_sustain_frames` as the trigger proxy — M4+ scope.
- **CDF 5/3 progressive refinement (M3.3)** — separate milestone; the handoff invariant in this plan sets up the substrate that refinement builds on.
