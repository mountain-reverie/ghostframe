# B2 — `indices_raw` Wire Emission Assertion — Design

**Date**: 2026-05-19
**Milestone**: Closes M3.2c B2 follow-up (the last remaining un-validated item from the original M3.2c plan now that A5/B6 + W5 + W3/B3/B7 + W4 + W1+B1 have all landed).
**Predecessor**: `docs/superpowers/specs/2026-05-15-m3.2c-verification-design.md` § B2 (line 99), originally deferred because the natural pipeline couldn't produce thin emissions under text_grid_drm's paint-once design + the latent on_session_reset / LRU bugs.

## Background

`e2e_indices_raw_handshake` (e2e.rs:2031) currently uses `--solid-red` (no PalRle ever emits) and only verifies:
1. HELLO message arrived from the client.
2. Server parsed it and the `indices_raw=true` capability is logged.

The test's own docstring (lines 2026-2029) flags the missing wire-level assertion:
> The companion "indices_raw emitted" assertion (that PalRle thin tiles flip to the new wire variant) is deferred to M3.2c — it requires the test-pattern → Xorg-on-VKMS → modesetting-FB capture path to produce PalRle-feasible content, which it currently does not.

That blocker is now resolved:
- `--solid-per-tile --drm-direct` (commit `033faa9`) produces continuous PalRle for the central motion region.
- Bug B fix (commit `d41900a`) keeps both flip palettes resident in `find_matching` indefinitely, so the natural bundled-then-thin sequence works.
- Bug A fix doesn't directly apply (first-connect path is sufficient — no `page.reload` needed for B2 since we only need the first session's ACK→delivered→thin transition to fire).

## Goal

Extend `e2e_indices_raw_handshake` so that, in addition to the HELLO + caps log assertions, it verifies at least one PalRle wire payload received by the client has the `indices_raw` flag bit set (flags byte bit 1 = 0x02). End state: B2 is closed; the M3.2c verification milestone has full wire-level assertions across all the wire variants the protocol emits.

## Approach

### Client-side recorder

In `ghostframe-web-client/src/main.ts`, the existing test-instrumentation block (around line 340-347) already pushes `asm.header.codec` to `window.__ghostframeRecordedCodecs`. Extend it to also push the flags byte for PalRle tiles:

```typescript
const w = window as unknown as {
  __ghostframeRecordedCodecs?: number[];
  __ghostframeRecordedFlags?: number[];
};
if (!w.__ghostframeRecordedCodecs) {
  w.__ghostframeRecordedCodecs = [];
}
if (!w.__ghostframeRecordedFlags) {
  w.__ghostframeRecordedFlags = [];
}
w.__ghostframeRecordedCodecs.push(asm.header.codec);
if (asm.header.codec === Codec.PalRle) {
  w.__ghostframeRecordedFlags.push(asm.payload[0]);
}
```

The flags byte is `asm.payload[0]` — the very first byte of every PalRle wire payload per `palrle-codec-design.md`:
- Bit 0 (`0x01`) = bundled (the payload also contains the palette block before the indices)
- Bit 1 (`0x02`) = indices_raw (the payload contains 512 raw nibble-packed indices instead of nibble-RLE bytes)

We only record for PalRle because for other codecs the payload's first byte means something completely different (or nothing at all).

### Test-side change

Replace `e2e_indices_raw_handshake`'s body. The test stays in its current location and keeps its name; the existing HELLO + caps log assertions remain.

```rust
/// M3.2b/B2: HELLO + caps + wire-level indices_raw emission.
///
/// Three assertions land in one test:
///   1. The client sends HELLO immediately after `transport.ready` — verified
///      by "HELLO received" tracing line in server logs.
///   2. The server's `dispatch_feedback_bytes` → `apply_hello` path updates
///      per-bridge `caps.indices_raw_enabled` — verified by "indices_raw=true"
///      in server logs.
///   3. At least one PalRle wire payload received by the client has flags
///      bit 1 set (indices_raw, 0x02) — verified by reading back
///      `window.__ghostframeRecordedFlags` from the client and asserting
///      `flags.iter().any(|&f| (f & 0x02) != 0)`.
///
/// Closed M3.2c B2 follow-up. Required infrastructure: `--solid-per-tile
/// --drm-direct` for continuous PalRle emission, Bug B fix (commit d41900a)
/// for stable palette caching across the 2-color flip, and Bug A fix
/// (commit 30ee414) — actually, Bug A is only needed for the reconnect
/// path; first-session HELLO+caps+thin works regardless.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_indices_raw_handshake() -> Result<()> {
    let setup = setup_e2e_webgpu_gpu("--solid-per-tile --drm-direct").await?;

    // Allow time for: page load → WebGPU init → WebTransport.ready →
    // HELLO write → server parse → first PalRle bundled emission →
    // client ACK → server delivered=true → next dirty pass emits thin +
    // indices_raw. 5s comfortably covers QUIC slow-start + initial
    // H264-startup phase + several 2-color flip cycles.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Assertions 1 + 2 (HELLO + caps): unchanged from the prior version.
    let logs = helpers::read_server_logs_stripped("ghostframe-server");
    assert!(
        logs.contains("HELLO received"),
        "expected 'HELLO received' tracing line in server logs; got:\n{logs}"
    );
    assert!(
        logs.contains("indices_raw=true"),
        "expected 'indices_raw=true' in server logs (caps payload); got:\n{logs}"
    );

    // Assertion 3 (B2 wire emission): at least one PalRle payload had the
    // indices_raw flag bit set.
    let flags: Vec<u8> = setup
        .page
        .evaluate("window.__ghostframeRecordedFlags || []")
        .await?
        .into_value()?;
    assert!(
        flags.iter().any(|&f| (f & 0x02) != 0),
        "expected at least one PalRle tile with indices_raw flag (bit 1) set; got: {:?}",
        flags
    );

    Ok(())
}
```

### Why thin emission fires naturally

For `--solid-per-tile --drm-direct`:
1. Frame 1: motion region paints color A. Classifier picks PalRle for each motion-region tile. Server's `acquire_or_allocate(palette_A)` returns slot 0 via `find_empty_slot` (post-Bug-B-fix). `bundled = !delivered.contains(0) = true`. Server emits bundled(slot=0).
2. Client receives bundled(slot=0), pushes to renderer queue, ACKs. Client's `__ghostframeRecordedFlags` now contains `0x01` (bundled flag).
3. ACK arrives at server. `delivered[0] = true`. `in_flight_carrying[0]--`.
4. Frame 2: motion region paints color B. Classifier still PalRle. `acquire_or_allocate(palette_B)` → slot 1 (also via find_empty_slot). `bundled = true`. Server emits bundled(slot=1).
5. (Repeat with palette_A on frame 3 via find_matching slot 0, etc.)
6. Once both palettes have delivered=true (after their respective ACKs), the next frames going to either slot have `bundled = !delivered.contains(id) = false`. Encoder path: `if !p.bundled && caps.indices_raw_enabled` (io_bridge.rs:1702) → `encode_pal_rle_payload_indices_raw` → wire payload starts with `0x02`.
7. Client receives, `__ghostframeRecordedFlags` push appends `0x02`.

Within 5 seconds at the test's flip cadence (~30 Hz), dozens of bundled-then-thin transitions happen across multiple flip cycles. The `iter().any(|&f| f & 0x02 != 0)` assertion needs only ONE such tile to succeed; the actual run will have ~hundreds.

## Files Touched

- **`ghostframe-web-client/src/main.ts`**: ~6 lines added in the existing instrumentation block.
- **`ghostframe-lib/tests/e2e.rs`**: `e2e_indices_raw_handshake` body rewritten — switch setup helper, add the third assertion. Docstring updated to reflect the closure of the M3.2c B2 deferral.

## Testing

- **The test itself is the test.** The recorder is trivial (~6 lines of TypeScript that mirror the existing codecs recorder); the assertion is a single `iter().any()`. The server-side encoder behavior is already covered by:
  - `phase_b_emits_indices_raw_when_caps_enabled_and_thin` (io_bridge.rs:2878) — unit test verifies the encoder path emits `0x02` as flags byte when both caps.indices_raw_enabled AND !p.bundled.
  - `indices_raw_payload_layout_thin` (pal_rle.rs:1062) — unit test verifies the wire-byte layout.
- **No new unit tests needed.**
- **Suite regression**: full e2e suite stays at 23 passed, 0 failed, 2 ignored. `e2e_indices_raw_handshake` stays passing — it just gains a stronger assertion.

## Out of Scope

- **Asserting the bundled flag (bit 0) too**. The recorder ALSO captures bundled emissions (bit 0 set, bit 1 clear). We could add a secondary assertion `flags.iter().any(|&f| (f & 0x01) != 0)` to verify the bundled emission happens, but it's redundant — bundled emission is the default first-frame path that every PalRle test implicitly exercises. The B2-specific assertion is about indices_raw, the new wire variant introduced in M3.2b that the existing test never verified end-to-end.
- **Server-side log substring assertion**. The server logs `palrle.wire: indices_raw emitted` at INFO level for every indices_raw emission. We could grep for it. But the client-side wire-byte recorder is strictly stronger (asserts the bytes arrived, not just the server's intent to emit them). β alone would not catch a hypothetical case where indices_raw payloads were emitted but dropped before reaching the client.
- **Counting indices_raw emissions** with a threshold like "≥ 10 indices_raw payloads in 5s". The `any()` assertion is sufficient: if even one fires, the wire variant is exercised end-to-end. A counting assertion would add false-negative risk under flaky timings without adding signal.
- **Test renaming**. `e2e_indices_raw_handshake` is fine — "handshake" historically referred to HELLO+caps; with the wire emission check added it now covers the full protocol-feature handshake (advertisement + emission). Renaming would churn git blame for no readability benefit.
- **Closing the `palrle_exact` Tasks 10-11 follow-up**. Different concern (exact-pixel verification with a known-nibble payload). Optional 1-day follow-up.

## Risk

- **Test pattern doesn't emit thin within 5s**: low. With Bug B fixed, the 2-color flip cycle produces ACKs within ~RTT-class times (< 100 ms post-first-bundled), and thin emission starts on the very next dirty pass for the same palette. Empirically, `e2e_palrle_oob_index` (which uses the same test pattern) needs ~6s post-startup before the OOB injection target tile reliably emits PalRle, but that test waits for STEADY-state PalRle which requires the classifier to exit its initial H.264 phase. For B2, even a single transient thin emission counts — we'd need a much narrower failure window than empirical evidence suggests.
- **Container DRM passthrough required**: switching from `setup_e2e_webgpu` (no GPU) to `setup_e2e_webgpu_gpu` (with /dev/dri bind-mount + privileged) means the test now needs the host to have VKMS available — same requirement as `e2e_solid_per_tile_pixels`, `e2e_palrle_5pct_loss`, `e2e_palrle_oob_index`, `e2e_decode_error_thin_uncached`, all of which currently pass on this host. No new infra.
- **Recorder push timing race**: the test reads the recorder array via `setup.page.evaluate(...)` while the page is potentially still receiving more tiles. The read is a snapshot of whatever's been pushed up to that moment. With 5s of runtime and PalRle firing from ~1s in, the array will have hundreds of entries by the time evaluate runs. No race-induced false negative under realistic conditions.
- **Recorder loses entries on session reset**: not applicable to this test (single-session, no page.reload).

## Pointers

- Original B2 design: `docs/superpowers/specs/2026-05-15-m3.2c-verification-design.md` § B2.
- Wire format reference: `docs/superpowers/specs/2026-05-13-palrle-codec-design.md`.
- Bug A + Bug B closure that unblocked this: `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_decode_error_thin_diagnosis.md`.
- Existing similar test infrastructure (test pattern + DRM setup): `e2e_solid_per_tile_pixels` (e2e.rs:865), `e2e_decode_error_thin_uncached` (e2e.rs:2091 post-closure).
- Existing recorders on `window`: `__ghostframeRecordedCodecs` (main.ts:340-347), `__ghostframeRecordedTiles` (commit `8bf4ab0`), `__ghostframeRecordedResizes` (commit `8bf4ab0`).
