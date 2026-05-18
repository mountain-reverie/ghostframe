# Decode-Error Thin-Uncached Latent-Bug Closure — Design

**Date**: 2026-05-18
**Milestone**: Closes M3.2c A5/B6 carry-over (`e2e_decode_error_thin_uncached`).
**Predecessor**: `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_decode_error_thin_diagnosis.md` (2026-05-17/18 diagnosis).

## Background

`e2e_decode_error_thin_uncached` is `#[ignore]`'d because the natural pipeline can't fire ERR_THIN_UNCACHED_PALETTE under current architecture. Diagnosis identified two independent latent production bugs:

- **Bug A**: `palette_table.on_session_reset(...)` is dead code. The gate at `io_bridge.rs:1446` (`!was_connected && wt.is_connected()`) never fires — `Stream(Opened)`'s eager `on_stream_opened` completes the CONNECT handshake (and flips `is_connected()` to true) BEFORE `Stream(Readable)` arrives. So `was_connected` is already true when the readable hits the gate.
  - Side effects: dirty_tracker/metrics_tracker/classifier/scheduler also miss their resets on reconnect. `force_dirty_frames` slow-start mitigation never fires (CPU path). Frame mode never forced back to H264 → no fresh IDR for reconnecting clients. `dimensions_retransmits_left` never re-primes. None catastrophic but cumulatively non-obvious.
- **Bug B**: `acquire_or_allocate`'s 4-way ladder visits `find_eligible_free_slot` (LRU oldest FreeButCached) BEFORE `find_empty_slot`. For small palette working sets (e.g., the e2e's 2-color motion-region flip), this thrashes slot 0: find_matching MISSES, find_eligible_free_slot returns oldest LRU (always slot 0 because LRU oscillates between the only two used slots), `write_bytes` overwrites and clears `delivered`. Result: every flip cycle, the first emission per palette is bundled, then thin within the same frame succeeds against the now-populated client shadow. ERR_THIN_UNCACHED never fires.
  - Side effect: 660+ `in_flight_carrying underflow — enqueue/ack pairing bug` WARN logs per ~12s test run (the in_flight tracking gets confused when delivered flips false unexpectedly).

This spec closes both bugs with the minimum-surface-area changes needed, with unit tests verifying each fix in isolation, and validates `e2e_decode_error_thin_uncached` end-to-end.

## Goal

Drop the `#[ignore]` on `e2e_decode_error_thin_uncached` and have it pass via the natural pipeline (`GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1` hook + `page.reload()` → server retains `delivered` → next emission is thin → client (empty shadow) errors → server logs `client decode error` + `force_rebundle`).

## Approach

Three changes land as one logical unit:

### 1. Bug A fix — IoBridge gate

Move the gate state out of `WebTransportServer::is_connected()`'s "did the transition happen during this event?" comparison and into IoBridge-owned per-handle state.

**New field on `IoBridge`**:
```rust
session_resets_fired: HashSet<ConnectionHandle>,
```
Production field — NOT cfg-gated. `HashSet::insert` returns `true` on first insertion (and `false` on duplicate), so the gate becomes a one-liner.

**Extract reset body into `IoBridge::fire_session_reset(&mut self, handle: ConnectionHandle)`** — pure code move from inline `Stream(Readable)` handler body, preserves all existing behavior including the `skip_palette_session_reset` cfg-gated hook.

**New gate helper**:
```rust
fn maybe_fire_session_reset(&mut self, handle: ConnectionHandle) {
    let Some(wt) = self.wt_sessions.get(&handle) else { return; };
    if wt.is_connected() && self.session_resets_fired.insert(handle) {
        self.fire_session_reset(handle);
    }
}
```

**Call site changes** — add `self.maybe_fire_session_reset(handle)` after BOTH:
- `wt.on_stream_opened(conn, dir);` (the call that's actually completing the CONNECT)
- `wt.on_stream_readable(conn, id);` (kept as a fallback for any future code path that promotes is_connected via a different event)

Both call sites use the same gate; the `HashSet::insert` ensures exactly-once firing per handle.

**Cleanup on disconnect** — in `Event::ConnectionLost`, add `self.session_resets_fired.remove(&handle);` next to the existing `self.wt_sessions.remove(&handle);`. This lets a rare reconnect-with-same-handle scenario re-fire the reset.

### 2. Bug A test seam — `WebTransportServer::test_set_connected`

To unit-test `maybe_fire_session_reset` without driving a real QUIC handshake:

```rust
#[cfg(test)]
pub(crate) fn test_set_connected(&mut self, value: bool) {
    // Direct internal-state setter for unit tests. Production callers must
    // not use this — `is_connected()` should derive from the actual
    // handshake state machine via `on_stream_readable` / `on_stream_opened`.
    self.connected = value;
}
```

Brittle by design — exists to provide a minimal seam for the gate unit test. If `WebTransportServer::connected` is later refactored into multi-field state, this setter would need updating. Comment that constraint inline.

### 3. Bug B fix — reorder allocation ladder

`pal_rle.rs::acquire_or_allocate` currently:
1. `find_matching` → reuse existing slot
2. `find_eligible_free_slot` → overwrite oldest FreeButCached
3. `find_empty_slot` → write to empty slot
4. fail

Reorder paths 2 and 3:
1. `find_matching` → reuse existing slot
2. `find_empty_slot` → write to truly-fresh slot (preserves existing FreeButCached for future find_matching hits)
3. `find_eligible_free_slot` → only evict FreeButCached when no truly-empty slot exists
4. fail

Mechanical swap. The fix is strictly an improvement: never worse than today, much better for small working sets.

Update the design doc (`docs/superpowers/specs/2026-05-13-palrle-codec-design.md` §"Per-frame allocation algorithm") to reflect the new order and add a one-line rationale: *"Prefer truly-fresh slots to extend cache lifetime; only evict FreeButCached when the cache is genuinely full. The previous order thrashed slot 0 on small palette working sets (N=2) because find_eligible_free_slot returned the same oldest slot every cycle."*

### 4. Validation

Drop `#[ignore]` on `e2e_decode_error_thin_uncached`. Update its rationale comment to describe the closure. Run the test. Expected: PASS.

If it fails: diagnose with the same recorder approach used in the original 2026-05-17/18 investigation (window-attached `__ghostframeRecordedPrevalidate`, `__ghostframeDecodeErrorReports`, etc. — added on demand, NOT committed unless we keep them as future-debug infra). Report findings + propose remediation in a follow-up addendum.

## Files Touched

- **`ghostframe-lib/src/transport/io_bridge.rs`** (Bug A):
  - New struct field `session_resets_fired: HashSet<ConnectionHandle>` (~1 line declaration + 3 init lines across `new` + 2 test constructors).
  - Extract `fire_session_reset(&mut self, handle: ConnectionHandle)` from the inline `Stream(Readable)` handler body (~25 lines moved, no logic change).
  - New `maybe_fire_session_reset(&mut self, handle: ConnectionHandle)` (~5 lines).
  - Replace existing inline gate at `Stream(Readable)` with `self.maybe_fire_session_reset(handle);`. Add the same call at `Stream(Opened)` after `wt.on_stream_opened(...)`. Net ~5 lines, mostly removed inline gate offset by helper-call additions.
  - Remove handle from `session_resets_fired` in `ConnectionLost` (~1 line).
  - New unit test `maybe_fire_session_reset_fires_exactly_once_per_handle` (~30 lines).
  - **NOTE on cfg-gating**: `session_resets_fired` is NOT cfg-gated (production behavior change). The `skip_palette_session_reset` field that `fire_session_reset` consults stays cfg-gated as before.

- **`ghostframe-lib/src/transport/webtransport.rs`** (Bug A test seam):
  - New `#[cfg(test)] pub(crate) fn test_set_connected(&mut self, value: bool)` (~3 lines).

- **`ghostframe-lib/src/encoder/pal_rle.rs`** (Bug B):
  - Swap paths 2 and 3 in `acquire_or_allocate` (~6 lines, mechanical).
  - New unit test `acquire_or_allocate_uses_empty_slots_before_evicting_cached` (~30 lines).

- **`docs/superpowers/specs/2026-05-13-palrle-codec-design.md`** (Bug B doc update):
  - Update §"Per-frame allocation algorithm" order + add rationale (~5 lines).

- **`ghostframe-lib/tests/e2e.rs`** (Validation):
  - Drop the `#[ignore]` line on `e2e_decode_error_thin_uncached` (~1 line removed).
  - Update the doc-comment block above it to describe the closure (~5 lines replacing the existing 17-line diagnosis-rationale block).

- **`~/.claude/projects/-home-cedric-work-ghostframe/memory/project_decode_error_thin_diagnosis.md`**:
  - Mark both latent bugs as closed with commit refs.

- **`~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m32c_near_complete.md`**:
  - Bump suite count to 23 passed, 2 ignored. Move `e2e_decode_error_thin_uncached` from "ignored" to "closed by".

## Testing

### Unit tests

**Bug A** (`io_bridge.rs::tests::maybe_fire_session_reset_fires_exactly_once_per_handle`):

```rust
#[test]
fn maybe_fire_session_reset_fires_exactly_once_per_handle() {
    let (stream, _peer) = TokioUnixStream::pair().unwrap();
    let server = QuicServer::test_default();
    let mut bridge = IoBridge::new_with_stream_for_test(stream, server);
    // Seed observable side effect: delivered bit on palette 7.
    bridge.palette_table.delivered.insert(7);
    let handle = ConnectionHandle(0);
    bridge.wt_sessions.insert(handle, WebTransportServer::default());
    // Pre-condition: not yet connected → must NOT fire.
    bridge.maybe_fire_session_reset(handle);
    assert!(bridge.palette_table.delivered.contains(7),
        "not-yet-connected: reset must not fire");
    // Promote: now connected. First call → fires (delivered cleared by reset body).
    bridge.wt_sessions.get_mut(&handle).unwrap().test_set_connected(true);
    bridge.maybe_fire_session_reset(handle);
    assert!(!bridge.palette_table.delivered.contains(7),
        "first call after connected: reset must fire (delivered cleared)");
    // Re-arm delivered. Second call must NOT re-fire.
    bridge.palette_table.delivered.insert(7);
    bridge.maybe_fire_session_reset(handle);
    assert!(bridge.palette_table.delivered.contains(7),
        "second call: reset must not re-fire (gated by session_resets_fired)");
}
```

Verifies: (a) gate respects `is_connected`, (b) reset body's observable side effect (delivered cleared), (c) exactly-once firing per handle.

**Bug B** (`pal_rle.rs::tests::acquire_or_allocate_uses_empty_slots_before_evicting_cached`):

```rust
#[test]
fn acquire_or_allocate_uses_empty_slots_before_evicting_cached() {
    let mut t = PaletteTable::new();
    let p_red = {
        let mut p = PaletteEntry::default();
        p.count = 1;
        p.colors[0] = [0, 0, 0xFF, 0xFF];
        p
    };
    let p_blue = {
        let mut p = PaletteEntry::default();
        p.count = 1;
        p.colors[0] = [0xFF, 0, 0, 0xFF];
        p
    };
    // Frame 1: red. Lands in empty slot 0.
    let id_red = t.acquire_or_allocate(&p_red).unwrap();
    assert_eq!(id_red, 0);
    t.release(id_red);                          // end-of-frame release
    t.delivered.insert(id_red);                  // simulate ACK
    // Frame 2: blue. MUST land in empty slot 1 — not overwrite slot 0.
    let id_blue = t.acquire_or_allocate(&p_blue).unwrap();
    assert_eq!(id_blue, 1, "blue should land in empty slot 1, not overwrite slot 0");
    assert!(t.delivered.contains(id_red),
        "delivered on slot 0 must survive the slot 1 allocation");
    t.release(id_blue);
    t.delivered.insert(id_blue);
    // Frame 3: red again. find_matching hits slot 0. No write_bytes call.
    let id_red_2 = t.acquire_or_allocate(&p_red).unwrap();
    assert_eq!(id_red_2, 0);
    assert!(t.delivered.contains(0), "find_matching path preserves delivered");
    // Frame 4: blue again. find_matching hits slot 1.
    let id_blue_2 = t.acquire_or_allocate(&p_blue).unwrap();
    assert_eq!(id_blue_2, 1);
    assert!(t.delivered.contains(1));
}
```

Verifies: empty slots get filled before any FreeButCached eviction; delivered survives across the flip; find_matching re-uses both slots stably.

### Integration test

The e2e test `e2e_decode_error_thin_uncached` itself, post-fix, validates the end-to-end pipeline. Substring asserts on server logs:
- `"client decode error"` (handle_decode_error WARN line)
- `"error_code=3"` (ERR_THIN_UNCACHED_PALETTE)
- `"force_rebundle"` (handle_decode_error INFO line)

### Suite regression

Full `cargo test -p ghostframe-lib --test e2e -- --test-threads=1` after dropping the ignore. Expected: 23 passed, 0 failed, 2 ignored (`e2e_palrle_session_reset` for separate dynamic-content reason, `e2e_resolution_change` pre-existing).

## Sequencing

1. **Bug A**: refactor + struct field + test seam + unit test + commit.
2. **Bug B**: ladder reorder + unit test + doc update + commit.
3. **Validation**: drop `#[ignore]` + run e2e + commit (only if PASS).
4. **Contingency**: if e2e fails, add window recorders, dump, propose remediation in a follow-up addendum (NOT committed as production code).
5. **Memory + final regression**: update memory files (not under git), run full suite, commit any straggler tweaks.

Subtasks 1 and 2 are file-disjoint and could theoretically run in parallel. We do them serially because (a) sequential is what the user asked for, (b) it keeps commits well-bisected if any test surprise crops up.

## Out of Scope

- **Refactoring `WebTransportServer` beyond the minimum**. The `test_set_connected` seam is the only addition. If `connected` becomes a derived field later, the seam needs updating — that's documented inline as a brittle test-only API.
- **Fixing the 660+ `in_flight_carrying underflow` warnings**. These are a SYMPTOM of bug B. Should disappear with the reorder. If they persist after the fix, that's a separate bug — file a follow-up.
- **Backporting `palrle_exact` test** (M3.2c original Task 10-11). Optional 1-day follow-up; not blocking.
- **Cleanup of any leftover client-side recorders** (`__ghostframeRecordedPrevalidate` etc.) from the prior diagnosis session. Those were already reverted. If we need them again during the contingency path, re-add then.
- **Production restructuring** of session-reset semantics (e.g., per-tile per-session tracking, multi-client invariants). The single-client invariant from M3.2a holds; this spec just fixes the gate.

## Risk

- **Bug A fix changes existing test timings**: low. The old gate never fired, so no test exercises the reset's side effects mid-session. After the fix, reconnect tests (`e2e_palrle_session_reset` once un-ignored, `e2e_decode_error_thin_uncached`) will see proper reset behavior. Other tests are single-session and unaffected.
- **Bug B fix causes regression in tests that depend on FreeButCached eviction**: low. The only test that exercises eviction-at-capacity is `e2e_palette_eviction` (`--palette-churn 300`), which fills all 256 slots → falls through to `find_eligible_free_slot` regardless of order. Unaffected.
- **Test seam `test_set_connected` over-tested**: low. The unit test is small (~30 lines) and the seam is a one-liner. Replacement cost if WebTransportServer refactors is minimal.
- **e2e still fails after both fixes**: medium. Contingency planned (diagnose-only, no production fix until findings are reviewed). The two bugs the diagnosis identified are necessary AND sufficient for the natural-pipeline trigger; any further blocker would be a third latent bug. Possible but unlikely.
- **Bug A fix introduces a regression in the reset-cleanup path** (e.g., `force_dirty_frames=20` now actually fires and breaks CPU-path tests): low. The CPU path is rarely exercised in e2e; the existing `process_frame_cpu`-using tests are `--solid-red` style and don't have palette state that would interact. The `frame_mode = H264` reset just forces an IDR, which is harmless for any test that runs past the first second.

## Pointers

- Diagnosis: `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_decode_error_thin_diagnosis.md`
- Companion spec (socket cleanup): `docs/superpowers/specs/2026-05-18-xvfb-socket-cleanup-design.md`
- Closure target test: `ghostframe-lib/tests/e2e.rs::e2e_decode_error_thin_uncached`
- Pre-fix wiring commits: `30209bf`, `1af6daa`, `496b8ee`, `033faa9`, `9183c0b`, `70efa56` (kept as-is; no revert).
- PalRle design (Bug B fix updates this): `docs/superpowers/specs/2026-05-13-palrle-codec-design.md`
