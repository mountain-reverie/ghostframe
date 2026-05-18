# `e2e_decode_error_thin_uncached` Closure — Design

**Date**: 2026-05-17
**Milestone**: M3.2c A5/B6 closure (carry-over from the 2026-05-15 verification milestone)
**Predecessor**: `2026-05-15-m3.2c-verification-design.md` § A5/B6 + commit `15607cb` ("test(e2e): re-ignore session_reset + decode_error as M3.2c follow-ups")

## Background

`e2e_decode_error_thin_uncached` (e2e.rs:2106) is `#[ignore]`'d. The test's *intent* is end-to-end verification of the ERR_THIN_UNCACHED_PALETTE error path:

1. Server emits thin PalRle payload referencing a palette the client doesn't have.
2. Client's `prevalidatePalRle` returns `errorCode: 3 (ERR_THIN_UNCACHED_PALETTE)`.
3. Client's `DecodeErrorBatcher` writes a DECODE_ERROR message on the FEEDBACK bidi stream.
4. Server's `handle_decode_error` logs `"client decode error"` (WARN) and `"force_rebundle: ..."` (INFO), then clears the `delivered` bit.
5. Next emission for that palette is bundled again; client's shadow recovers; steady state resumes.

The test's current implementation uses `GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY=1.0 GHOSTFRAME_OUTBOUND_LOSS_PREDICATE=palrle_bundled` expecting the server to "eventually emit thin" after enough bundled drops. But that's not how the server works: it only emits thin when `palette_table.delivered.contains(id)` is true, which requires an ACK, which requires the client to receive the bundled — which 100% loss prevents. The server and client always stay in lockstep on palette state.

After analysis (documented in chat session 2026-05-17), there is **no natural pipeline mechanism in current code** that produces "server thinks palette delivered AND client doesn't have it in shadow":

- Loss on bundled → client never gets palette, client never ACKs, server retries bundled forever.
- Loss on ACK only → server keeps bundling, never advances to thin.
- Reconnect alone → server calls `palette_table.on_session_reset()` (io_bridge.rs:1435), which clears `delivered`; both sides reset in sync.
- Multi-client → out of scope for M3.2c (single-client invariant per design).

The error code exists because in production a client *could* lose palette atlas state outside the protocol's awareness (e.g., browser tab discard + restore, WebGPU device loss without client-side session reset). The server then thinks state is intact and emits thin to an empty shadow.

## Goal

Close `e2e_decode_error_thin_uncached` by giving the e2e a deterministic way to simulate the production divergence (server retains delivered state, client loses palette shadow) via the natural pipeline, with no modification to the encoder's bundled-vs-thin decision path.

## Approach

New cfg-gated server-side env-var `GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1`. When set, the server's new-session branch passes a `preserve_delivered: bool` flag to `palette_table.on_session_reset(true)`, which performs every cleanup step EXCEPT clearing the `delivered` BitSet. All other per-session state (`in_flight_carrying`, `ref_count`, `free_lru`, `slot_state`, `stats_frame`) resets normally. This isolates the divergence to exactly the bit the test wants to preserve and avoids the brittleness of leaving stale ref_count / in_flight counters across sessions.

The test must use a test pattern whose PalRle palette is **stable across the page reload** — otherwise session 2 allocates fresh palettes (delivered=false), bundled emissions fire, and the error path never triggers. The default `solid_per_tile.rs` cycles its motion region through 256 distinct colors over ~8.5 s, so palette IDs in session 2 likely won't match session 1's. This spec includes a small test-pattern change: pin the motion region to a 2-color flip (e.g., red ↔ blue at the existing 16 ms cadence) so the same 1-color palette is hit on both sides of the reload.

### End-to-end flow with this hook

Container started with `GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1` and `TEST_PATTERN=--solid-per-tile --drm-direct`:

1. **Session 1 startup**: motion region (central 64×64) produces continuous PalRle. Server allocates palette P, emits bundled(P). Client receives, processes, ACKs.
2. **ACK roundtrip**: server marks `delivered[P]=true`. Client's `paletteShadow.put(P, count)`.
3. **Steady state Session 1**: subsequent emissions for P are thin; client decodes against its shadow. All fine.
4. **Test triggers reload**: `setup.page.reload().await?`. WebTransport session closes; server logs `connection lost`. Renderer's `onSessionReset()` clears client palette shadow + zeroes GPU palette atlas (renderer.ts:107).
5. **Session 2 opens**: server's new-session handler runs:
   - Dirty tracker reset ✓ (so all tiles are dirty in the first frame)
   - Metrics tracker reset ✓
   - Classifier reset ✓
   - Scheduler clear ✓
   - **`palette_table.on_session_reset()` skipped** (env-var gate) — `delivered[P]` stays true
   - Frame mode → H264 (initial keyframe)
   - Dimensions retransmit primed
6. **First post-reload PalRle frame**: motion region dirty → server enters `phase_a_palette_allocation`. For palette P:
   - `acquire_or_allocate` returns P (palette bytes intact, slot was FreeButCached but still indexed by content match)
   - `bundled = !self.palette_table.delivered.contains(P) = !true = false` → **thin**.
7. **Server emits thin(P)**: encoder takes the `encode_pal_rle_payload_indices_raw` or `encode_pal_rle_payload(bundled=false)` path. Client receives.
8. **Client decodes**: `prevalidatePalRle` → `shadow.has(P) === false` (shadow was cleared at step 4) → returns `{ ok: false, errorCode: 3 }`.
9. **Client reports error**: `renderer.encodeAndPresentFrame`'s `onDecodeError` callback fires → `decodeErrorBatcher.report(...)` → next FEEDBACK flush writes a `0x04 DECODE_ERROR` message with code=3.
10. **Server processes FEEDBACK**: `dispatch_feedback_bytes` → `handle_decode_error(msg)` logs:
    - `WARN client decode error codec=2 tile_x=X tile_y=Y error_code=3`
    - `INFO force_rebundle: next emission for palette will include bundled palette block palette_id=P`
   And calls `palette_table.force_rebundle(P)` → `delivered.remove(P)`.
11. **Subsequent frame**: `bundled = !delivered.contains(P) = true` → bundled(P) emitted. Client receives, shadow.put(P, count), ACKs, delivered=true again. Steady state resumes.

The test asserts steps 10's log lines appear within an 8s wait after reload.

### Why not other approaches

Considered alternatives — explicitly rejected:

- **Force-thin hook** (`GHOSTFRAME_FORCE_PALRLE_THIN=1`): bypasses the encoder's bundled-vs-thin decision. Tests an artificial scenario where the server emits thin without ever bundling — not a real production failure mode. The encoder branch under test is contrived rather than the natural one.
- **Dynamic test pattern + selective loss**: doesn't help. The protocol's ACK-gated `delivered` flag means loss-injection alone keeps server and client in sync (server retries bundled until success, no divergence emerges).
- **Multi-client scenario**: out of scope for M3.2c single-client invariant.
- **Client-side hook to clear palette shadow mid-session**: would test the error path but requires modifying the production client to expose a test API. Server-side env-var is cleaner since cfg-gating already keeps it out of production builds.

The skip-session-reset hook is the minimal intervention that produces the natural production failure mode end-to-end.

## Files Touched

- `ghostframe-lib/src/transport/io_bridge.rs`:
  - New field on `IoBridge` (~3 lines, cfg-gated like `outbound_loss` and `oob_inject_at`):
    ```rust
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub(crate) skip_palette_session_reset: bool,
    ```
  - New parser fn (~5 lines, cfg-gated, parallel to `oob_injector_from_env`):
    ```rust
    #[cfg(any(test, feature = "test-loss-injection"))]
    fn skip_palette_session_reset_from_env() -> bool {
        matches!(
            std::env::var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET").as_deref(),
            Ok("1") | Ok("true")
        )
    }
    ```
  - Initialize the field in `IoBridge::new` (cfg-gated) and as `false` in the two test-only constructors `new_with_stream_for_test` / `new_with_frames_for_test`.
  - Pass a `preserve_delivered: bool` flag at the call site (~5 lines):
    ```rust
    #[cfg(any(test, feature = "test-loss-injection"))]
    let preserve_delivered = self.skip_palette_session_reset;
    #[cfg(not(any(test, feature = "test-loss-injection")))]
    let preserve_delivered = false;
    self.palette_table.on_session_reset(preserve_delivered);
    if preserve_delivered {
        tracing::info!(
            "test-hook: preserving palette_table.delivered across session reset (GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1)"
        );
    }
    ```
  - New unit test `skip_palette_session_reset_from_env_parses` in the `mod tests` block (parallel to `oob_injector_from_env_parses`).

- `ghostframe-lib/src/encoder/pal_rle.rs`:
  - Modify `on_session_reset` to take `preserve_delivered: bool` (~3 lines diff):
    ```rust
    pub fn on_session_reset(&mut self, preserve_delivered: bool) {
        if !preserve_delivered {
            self.delivered.clear();
        }
        // ... rest unchanged
    }
    ```
  - Update the two existing unit tests `on_session_reset_preserves_bytes_clears_tracking` (line 705) and `on_session_reset_keeps_empty_slots_empty` (line 724) to pass `false`. Add a new unit test `on_session_reset_preserve_delivered_keeps_bit` asserting that with `true`, a previously-set `delivered` bit survives the call while other state still resets.

- `ghostframe-test-pattern/src/solid_per_tile.rs`:
  - Replace the 256-color cycling motion region with a 2-color flip. The current pixel calculation derives `motion_color` from a per-frame phase; replace with `if start.elapsed().as_millis() / 33 % 2 == 0 { RED_FLIP } else { BLUE_FLIP }` where `RED_FLIP = 0x00FF_0000` and `BLUE_FLIP = 0x0000_00FF`. Effect: motion region alternates between two 1-color palettes; both get allocated and delivered in session 1; either matches in session 2.
  - The existing corner-pixel assertions in `e2e_solid_per_tile_pixels` are unaffected (corners aren't repainted).

- `ghostframe-lib/tests/e2e.rs`:
  - Rewrite `e2e_decode_error_thin_uncached`. Replacement:
    ```rust
    /// W3 / A5 / B6 — Verify the ERR_THIN_UNCACHED_PALETTE round-trip:
    /// server emits thin against a palette the client doesn't have (after
    /// shadow reset by page reload), client reports DECODE_ERROR code 3,
    /// server logs + calls force_rebundle.
    ///
    /// The GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1 server-side hook suppresses
    /// the palette_table.on_session_reset() call that normally clears
    /// `delivered` on session reconnect. This simulates the production failure
    /// mode where the client loses palette atlas state (browser tab discard,
    /// WebGPU device loss, etc.) without the server noticing — the server's
    /// `delivered` bit lingers and the next thin emission fires against an
    /// empty client shadow.
    #[tokio::test(flavor = "multi_thread")]
    async fn e2e_decode_error_thin_uncached() -> Result<()> {
        let setup = setup_e2e_webgpu_gpu_with_env(
            "--solid-per-tile --drm-direct",
            &[("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET", "1")],
        )
        .await?;
        // Phase 1: let session 1 deliver and ACK palettes for the motion region.
        tokio::time::sleep(Duration::from_secs(5)).await;
        // Phase 2: page reload — drops client's palette shadow; server keeps
        // `delivered` due to the hook.
        setup.page.reload().await?;
        // Phase 3: post-reload, server re-emits thin for the motion region →
        // client errors → handle_decode_error fires → force_rebundle.
        tokio::time::sleep(Duration::from_secs(6)).await;

        let logs = read_and_strip_ansi_server_logs();
        assert!(
            logs.contains("client decode error"),
            "expected 'client decode error' tracing line; got:\n{logs}"
        );
        assert!(
            logs.contains("error_code=3"),
            "expected 'error_code=3' (ERR_THIN_UNCACHED_PALETTE); got:\n{logs}"
        );
        assert!(
            logs.contains("force_rebundle"),
            "expected 'force_rebundle' INFO line; got:\n{logs}"
        );
        Ok(())
    }
    ```
  - Drop the existing `#[ignore]` line.
  - `read_and_strip_ansi_server_logs` is a helper that runs `docker logs ghostframe-server`, strips ANSI escapes (same fold pattern used by `e2e_indices_raw_handshake` and `e2e_palrle_oob_index`). If the helper doesn't exist yet, extract one in `e2e/helpers.rs` rather than copy-pasting the fold three times. (Refactor scope: small. Extract once and update all three call sites.)

- `~/.claude/projects/-home-cedric-work-ghostframe/memory/reference_e2e_diagnose.md`: add `GHOSTFRAME_SKIP_PALETTE_SESSION_RESET` to the documented env-vars table.

## Testing

- **Unit**: `cargo test -p ghostframe-lib --lib skip_palette_session_reset_from_env_parses` — PASS.
- **E2E (the target test)**: `cargo test -p ghostframe-lib --test e2e e2e_decode_error_thin_uncached -- --test-threads=1 --nocapture` — PASS.
- **Suite regression**: `cargo test -p ghostframe-lib --test e2e -- --test-threads=1` — all 20 previously-passing tests still pass. Skip-session-reset is gated behind env-var, so it doesn't affect tests that don't set it.

## Out of Scope

- **Production behavior change for session reset**. The current `palette_table.on_session_reset()` on every new session connect is correct production behavior. This hook only lets tests simulate the failure mode that *can* occur outside the protocol's awareness.
- **Dropping the `#[ignore]` on `e2e_palrle_session_reset`**. That test fails for a different reason — the server doesn't re-emit static text_grid_drm content because SAD sees no dirty tiles after reset. It's the same "static content + paint-once test pattern" issue that blocks B2 / indices_raw wire assertion. This spec doesn't touch it.
- **Closing B2 (indices_raw wire assertion)**. Different blocker (needs ongoing dirty + delivered=true to fire thin+indices_raw natural emission). Could be closed in a separate spec by switching the indices_raw test to also use `--solid-per-tile` (whose motion region produces ongoing dirty and could exercise thin+indices_raw if delivered=true). Out of scope here.
- **Loss-injection combination**. Not needed; the session-reset-skip hook alone produces the deterministic trigger.
- **Multi-frame stability assertions** post-recovery. The current test only asserts the error round-trip fires. Asserting that the canvas eventually re-renders the motion region correctly post-recovery would be valuable, but is a stricter test that depends on render-path stability after the bundled re-delivery; defer to a follow-up if useful.

## Risk

- **`page.reload()` doesn't drop the WebTransport session cleanly enough for the server's new-session branch to fire**. Mitigation: the existing `e2e_palrle_session_reset` uses the exact same call. Its rendering assertions fail (different root cause), but the session-reset event itself does fire — verified by the `palette_table.on_session_reset()` already being called in current code on that test's reload. So we know the new-session handler runs.

- **`force_rebundle` cycles fast enough that we miss the error window**. After force_rebundle, the next emission is bundled, client gets it, ACKs, server emits thin again, client now has shadow, no error. So the error fires ONCE per palette in the recovery cycle. With multiple motion-region tiles using potentially distinct palettes (or the same palette across many tiles), we get multiple error logs to substring-match against. Mitigation: substring assertion only needs ONE occurrence. With 8s of wait and 30 Hz capture, dozens of post-reload frames fire — robust.

- **Server's `connection lost` event ordering**. On reload, browser closes the old session before opening the new one. If the server processes the new-session connect BEFORE the connection-lost cleanup, the dirty/metrics/classifier reset for session 2 could read stale state from session 1. Mitigation: this is existing-test territory (e2e_palrle_session_reset uses the same flow); if observed, the spec gets an addendum with serialization detail.

- **VKMS resolution drift between sessions**. The first DRM connector mode VKMS reports is host-dependent (1024×768 in our harness). Same mode is selected on both sessions. Motion-region tile coords stay the same → same palette structure → `acquire_or_allocate` returns the same slot ID → `delivered[P]=true` remains the relevant bit. No drift concern.

- **2-color flip in motion region doesn't trigger PalRle reliably** — if the flip is too slow the classifier sees low frequency and may pick Bc1; too fast and the tile classifies as H264. The existing `solid_per_tile.rs` uses 16 ms tick (≈60 Hz paint, ≈30 Hz capture) which empirically lands in the PalRle band (Rule 3 / Rule 5 path per the classifier — confirmed by Task 17's diagnose-tiles output). Keeping the same 16 ms cadence for the flip preserves that classification.
- **Both 1-color palettes don't get delivered in session 1's 5 s window** — the flip alternates ~30 times/s, so each color gets at minimum 5 s × 30 = 150 emissions, more than enough for at least one ACK round-trip per color. Robust.

## Pointers

- Predecessor session memory: `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m32c_near_complete.md` (A5/B6 deferred to M3.5 — *this* spec re-scopes it into M3.2c).
- Original M3.2c verification design: `docs/superpowers/specs/2026-05-15-m3.2c-verification-design.md` § A5 / B6.
- Diagnostic env-var conventions: `~/.claude/projects/-home-cedric-work-ghostframe/memory/reference_e2e_diagnose.md`.
- Wire format reference: `docs/superpowers/specs/2026-05-13-palrle-codec-design.md`.
- Companion spec (parallel M3.2c carry-over): `docs/superpowers/specs/2026-05-17-edge-tiles-diagnose-fix-design.md`.
