# H264 → TileCodec Lossy-to-Lossless Handoff — Design

**Date:** 2026-05-25
**Milestone:** Pre-M3.3 backlog cleanup. Establishes the architectural invariant that every H264→TileCodec classifier transition triggers a one-shot lossless full-repaint. Closes `e2e_palrle_session_reset` as a side effect.
**Predecessor:** `docs/superpowers/specs/2026-05-20-palrle-session-reset-design.md` — that closure design was reverted (commit `f4e3d3a`) because it tried to solve the closure inside a single test task. This design re-derives the architecture properly: the session-reset closure depends on a general invariant that applies to every H264→TileCodec transition, not just session resets.
**Umbrella reference:** `docs/superpowers/specs/2026-04-28-m3-codec-suite-design.md` § D1 "Frame-mode switch (no mixing)".

## Background

After `fire_session_reset` (and on cold boot), the classifier is in `FrameMode::H264` with `request_keyframe()` queued. For ≥ `exit_sustain_frames` (= 30) frames the encoder emits a lossy H.264 IDR plus P-frames carrying visible content cheaply over the wire. This is correct behaviour for QUIC slow-start: ~5-20 KB of H.264 datagrams reach the client before the congestion window has opened up, giving the user immediate visual feedback.

Once the classifier exits H.264 — because content is static and `exit_h264` has held for `exit_sustain_frames` — the system moves to TileCodec mode. The intent is that subsequent TileCodec emissions deliver **lossless** content (PalRle for low-color tiles, Solid for uniform tiles, Raw for the rest, eventually CDF 5/3 refinement bit-planes in M3.3) over the H.264 lossy render, **upgrading the canvas to pixel-perfect**.

That upgrade does not happen today. The GPU pipeline's Vulkan SAD compares the current captured frame against `prev_image`, which has been continuously snapshotted during the H.264 phase. For static content, on the first TileCodec frame after the mode flip, SAD reports zero dirty tiles. `process_frame_gpu`'s `dirty_xy.is_empty()` short-circuit returns early. The TileCodec path never emits, and the canvas stays at the H.264 lossy result indefinitely. The lossless content the architecture promises is never delivered.

The implementer of the prior closure attempt (commit `8c27784`, reverted) ran into this and worked around it by calling `reset_for_session(1)` at the H264→TileCodec transition. That intuition was correct in mechanism but wrong in framing: it was buried inside an e2e closure task with no spec, conflated with session-reset state cleanup, and never recognized as a general architectural invariant.

## Goal

Add the lossy-to-lossless handoff as an explicit, designed property of the codec-switch path:

> **Every H264 → TileCodec classifier transition invalidates the GPU SAD baseline so the next TileCodec frame re-emits every tile, overwriting the H.264 lossy render with lossless content.**

This applies to both:
- The cold-boot path (classifier starts in `H264`, exits after `exit_sustain` frames of low cost-ratio).
- The session-reset path (`fire_session_reset` forces `frame_mode = H264`; classifier later exits).

End state: `e2e_palrle_session_reset` closes with its **original** assertions (post-reset legibility + `codec_list.contains(&Codec::PalRle)`); the M3 architecture has a clean, named property covering the lossy/lossless transition; the implementation primitive (`GpuFrameProcessor::reset_for_session`) is renamed to `invalidate_baseline` so its name describes the mechanism, not one of its two trigger sites.

## Architecture

### The invariant

```
classifier transitions H264 → TileCodec
                ⇓
GPU SAD baseline (prev_image) invalidated
                ⇓
next captured frame: first-frame path runs, all tiles reported dirty
                ⇓
classifier classifies every dirty tile (PalRle / Solid / Raw / etc.)
                ⇓
TileCodec emission: lossless tiles flow, overwriting the H.264 result
                ⇓
subsequent frames: SAD-based dirty detection resumes from the freshly committed baseline
```

This invariant is the simplest expression of "TileCodec mode is the lossless mode, and entering it after a lossy H.264 phase must repaint the screen." No new concept beyond "lossy mode → lossless mode = repaint required".

### Why no inverse invariant on TileCodec → H264

H.264 doesn't read `prev_image`. The H.264 encoder maintains its own temporal reference state (GOP, prev P-frame DPB) entirely inside FFmpeg/VA-API. Re-entry into H.264 from TileCodec is already handled by `fire_session_reset`'s `request_keyframe()` for the session-reset case, and by the per-classifier-transition `request_keyframe()` call already in `process_frame_gpu:1187-1190` for hysteresis-driven flips:

```rust
if prev_mode == FrameMode::TileCodec {
    if let Some(enc) = self.full_frame_encoder.as_mut() {
        enc.request_keyframe();
    }
}
```

So the inverse direction (TileCodec → H264) is already handled correctly at `io_bridge.rs:1195-1199` — that codepath forces an IDR so the H.264 stream gets a fresh anchor. The new H264 → TileCodec invariant is the missing complement.

### CPU path

The CPU path (`process_frame_cpu`) always emits `Codec::Raw` via `dispatch_dirty_tiles_via_scheduler(..., SchedulerEmissionPolicy::CpuRawOnly, ...)`. There is no classifier mode flip on the CPU path — every frame is `TileCodec` mode at the codec layer regardless of what `classify_tile` would say. The H264→TileCodec invariant therefore does not apply to CPU. The CPU path keeps its existing `force_dirty_frames = 20` cushion for slow-start; nothing changes for it.

## Components

### 1. API rename: `reset_for_session(N)` → `invalidate_baseline(N)`

The current API on `GpuFrameProcessor` (from Task 1 of the reverted plan, kept on master):

```rust
pub fn reset_for_session(&mut self, force_frames: u32) {
    unsafe {
        self.device.device_wait_idle().ok();
        if let Some(prev) = self.prev_image.take() {
            self.destroy_prev_frame(prev);
        }
    }
    self.force_all_dirty_remaining = force_frames;
}
```

Becomes:

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
pub fn invalidate_baseline(&mut self, force_frames: u32);
```

Same body. Internal field name `force_all_dirty_remaining` stays — it's still accurate.

The unit test `reset_for_session_drops_prev_image_and_sets_counter` is renamed to `invalidate_baseline_drops_prev_image_and_sets_counter`. The cushion test `reset_for_session_cushion_keeps_all_dirty_and_prev_image_none` is renamed to `invalidate_baseline_cushion_keeps_all_dirty_and_prev_image_none`.

### 2. New mode-flip handoff in `process_frame_gpu`

Around `io_bridge.rs:1157-1163` the classifier flip is observed:

```rust
if new_mode != self.frame_mode {
    tracing::info!(
        prev = ?self.frame_mode,
        new = ?new_mode,
        seq,
        "classifier flipped frame mode"
    );
}
```

After this trace and before the existing `prev_mode == TileCodec → request_keyframe()` block (currently at `:1195-1199` inside the `FrameMode::H264` arm), add the new H264 → TileCodec handoff:

```rust
// H264 → TileCodec architectural invariant: invalidate the GPU SAD
// baseline so the next TileCodec frame re-emits every tile, overwriting
// whatever the H.264 phase rendered. Without this, static content
// produces 0 dirty tiles on mode entry (prev_image is current relative
// to the static screen) and the lossless tile codecs never get a chance
// to upgrade the canvas from the H.264 lossy render.
//
// Symmetric to the existing `request_keyframe()` on TileCodec → H264:
// each direction of the mode flip resets the state that the entering
// mode relies on (H.264 GOP for the H264 direction, GPU SAD baseline
// for the TileCodec direction).
if prev_mode == FrameMode::H264 && new_mode == FrameMode::TileCodec {
    if let Some(p) = self.gpu_frame_processor.as_mut() {
        p.invalidate_baseline(1);
    }
}
```

`force_frames = 1` because:
- We're past QUIC slow-start by the time the classifier exits H.264 (it required 30 frames of exit-candidate cost).
- The scheduler's ACK-based retry mechanism handles individual datagram drops on the lossless tiles.
- A longer cushion would re-emit every tile multiple times for no benefit on static content.

### 3. Wire-up in `fire_session_reset`

Renamed call site only:

```rust
// before
if let Some(p) = self.gpu_frame_processor.as_mut() {
    p.reset_for_session(20);
}
// after
if let Some(p) = self.gpu_frame_processor.as_mut() {
    p.invalidate_baseline(20);
}
```

Same `force_dirty_frames = 20` unconditional set above (unchanged).

### 4. Test closure

`ghostframe-e2e/tests/e2e.rs:1025-1100` `e2e_palrle_session_reset`:

- Remove `#[ignore]` attribute.
- Replace the `// M3.2c follow-up: post-reset server doesn't re-emit static content...` comment block with a one-line note pointing at this spec.
- **Keep both** existing assertions: post-reset luminance probe AND `codec_list.contains(&2u8)`. The PalRle assertion is now reachable because the mode-flip handoff causes PalRle tiles to flow for static text content within ~1 second of session reset (post H.264 phase + the handoff).
- No client-side H.264 decode dependency — even if WebCodecs/SwiftShader can't decode the H.264 IDRs in the test environment, the assertion at t=4s succeeds because the PalRle tiles emitted post-handoff have repainted the canvas by then.

## Data flow on static-text reset

```
t=0       page reload → new WebTransport session connects
          fire_session_reset:
            frame_mode = H264
            request_keyframe()
            force_dirty_frames = 20
            gpu_frame_processor.invalidate_baseline(20)

t=0..0.6s frames 1-20 (cushion active):
            GPU first-frame path runs each frame with snapshot = None
            classifier sees all tiles dirty → text classifies as PalRle
            decide_frame_mode: tentative is non-empty but tile_codec_cost
              ≪ h264_cost × 0.6 → exit-candidate → exit_sustain ticks
            BUT frame_mode = H264 → H.264 encoder consumes the IDR + P-frames
          frame 21 (cushion exhausts):
            first real prev_image snapshot
            still in H.264 mode

t=0.7..1s frames 22-30:
            SAD finds 0 dirty (static content vs snapshot)
            empty tentative → still exit-candidate → exit_sustain still ticking
            still in H.264 mode (haven't hit 30 yet)

t≈1s      frame 31: exit_sustain = 30, classifier flips to TileCodec
            ** mode-flip handoff fires: invalidate_baseline(1) **
            prev_image dropped, force_all_dirty_remaining = 1
            classifier flipped frame mode logged

t≈1s+1f   frame 32:
            first-frame path again (prev_image is None)
            cushion active (force_all_dirty_remaining = 1)
            all 768 tiles reported dirty
            classifier tentative: PalRle for text tiles, Solid for solid
              regions, Raw for the rest
            TileCodec emission: PalRle tiles flow over the wire
            client renders PalRle tiles → lossless overwrites canvas

t=1.1-4s  steady state TileCodec
            SAD finds 0 dirty (static content vs the frame-32 snapshot)
            no emission, canvas stays at the lossless render

t=4s      test assertion: codec_list contains 2 (PalRle) ✓
                          post_ink_lum - post_bg_lum > 80 ✓
```

## Testing

### Unit tests

Two existing tests get renamed (no body changes — same coverage):

- `invalidate_baseline_drops_prev_image_and_sets_counter` (renamed from `reset_for_session_drops_prev_image_and_sets_counter`)
- `invalidate_baseline_cushion_keeps_all_dirty_and_prev_image_none` (renamed from `reset_for_session_cushion_keeps_all_dirty_and_prev_image_none`)

One new test for the mode-flip handoff at the `IoBridge` level. Pattern: construct an `IoBridge` with a GPU processor, drive it through frames that force a `H264 → TileCodec` transition, observe (a) `process_frame_gpu` invoked `invalidate_baseline` on the processor, OR (b) the next frame's `dirty_xy` is the full grid. (a) is easier with a tracing or counter probe; (b) is cleaner but requires more scaffolding. Implementation chooses the cheaper path.

If unit-level testing the mode flip turns out to be infrastructure-heavy, fall back to relying on the end-to-end test for coverage and add a smaller-scope counter assertion (e.g. a `ModeFlipStats` field that increments on every H264→TileCodec call to `invalidate_baseline`, asserted in a unit test that runs a synthetic frame sequence). Either is acceptable; the e2e gate is the load-bearing verification.

### E2E test

`e2e_palrle_session_reset`:

- `#[ignore]` removed.
- Both original assertions retained:
  - Post-reset luminance probe (`post_ink_lum - post_bg_lum > 80.0`).
  - Post-reset codec recorder contains `Codec::PalRle (2)`.
- Docstring updated to note that the post-reset PalRle assertion is reachable via the H264→TileCodec mode-flip handoff after exit_sustain elapses (~1s post-reset).

### Negative test consideration

If a future change accidentally removed the handoff, the e2e test would fail on the codec assertion. If a future change broke the H.264 phase entirely, the e2e test would still pass via the post-handoff PalRle path — the luminance probe would succeed because PalRle delivers lossless text. This is by design: the test verifies the **TileCodec lossless upgrade** works end-to-end, not the H.264 phase specifically. (The H.264 phase has its own coverage via `e2e_h264_ssim_golden`.)

## Out of scope

- **M4 §6.5 bandwidth estimator.** Replacing `exit_sustain_frames` with a real QUIC-congestion-window-based trigger is M4 work. The `exit_sustain = 30` proxy is correct enough for now: after 30 frames of static content the wire has cleared regardless of starting bandwidth.
- **Stale per-tile `H264TileDecoder` client cleanup.** Tracked as Task #7 in the task list. The `decoder.ts:73-148` `H264TileDecoder` class, `main.ts:238-256` `h264Decoders` map / `getH264Decoder` function, and `main.ts:358-360` `case Codec.H264` routing are unreachable in practice (no server emits per-tile H.264 after M3.0) but dead-but-routed code. Removal clarifies "what's the H.264 path?" and is a separate cycle.
- **CDF 5/3 progressive refinement.** M3.3 work. Once landed, CDF 5/3 bit-planes will be the *refinement* path after the initial PalRle/Solid/Raw repaint — sharpening tiles further over time toward pixel-perfect. The H264→TileCodec invariant in this spec sets up the substrate that CDF 5/3 builds on.
- **CPU path mode flips.** CPU is Raw-only and has no classifier mode flips; no handoff applies.

## Decision register

| # | Decision | Rationale |
|---|---|---|
| D1 | Mode-flip handoff lives in `process_frame_gpu`, not in classifier or `fire_session_reset` | The handoff is a property of the GPU pipeline (drops `prev_image`); placing it in classifier would couple the pure-function classifier to a GPU side-effect. `fire_session_reset` only fires on session establishment; the handoff is a more general property than session reset. |
| D2 | `force_frames = 1` (not 20) for the mode-flip handoff | We're past QUIC slow-start by the time exit_sustain elapses; ACK-based retries handle individual datagram drops; longer cushion would re-emit static content redundantly. |
| D3 | Rename `reset_for_session(N)` → `invalidate_baseline(N)` | With two trigger sites (session reset + mode flip), the trigger-name is misleading. The mechanism name describes what the function does; the trigger context lives at the call site. |
| D4 | Only H264 → TileCodec direction has a handoff; TileCodec → H264 already handled | H.264 doesn't read `prev_image`; the inverse direction's reset (`request_keyframe()` to refresh the GOP anchor) is already wired at `io_bridge.rs:1195-1199`. |
| D5 | Test keeps both original assertions (legibility + PalRle codec) | The PalRle assertion is now reachable via the mode-flip handoff. Restoring it gives the test direct verification of the lossless-upgrade path. |
| D6 | Unit-test the mode-flip handoff via a counter or tracing probe at the IoBridge level | Pure-function unit-test of the handoff in isolation requires GPU infrastructure. A counter probe is cheaper and provides equivalent coverage. The e2e test is the load-bearing verification. |
| D7 | `exit_sustain_frames = 30` is the proxy trigger for "bandwidth stabilized" until M4's real estimator lands | Source spec §6.5 specifies a two-layer bandwidth estimator; that's M4+ scope. `exit_sustain` is a correct proxy for static content (30 consecutive frames of exit-candidate signal only happens after the slow-start burst has cleared). |
| D8 | Spec recovers and supersedes the reverted `2026-05-20-palrle-session-reset-design.md` | The reverted spec assumed H.264 IDR would carry text content end-to-end and that no TileCodec emission would be needed post-reset on static content. That assumption was load-bearing on the test environment's H.264 decode capability. This design routes the load-bearing assertion through the lossless TileCodec path instead, eliminating the H.264-decode dependency for the closure. |
