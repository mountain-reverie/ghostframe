# Decode-Error Thin-Uncached Latent-Bug Closure — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close `e2e_decode_error_thin_uncached` by fixing two latent production bugs (Bug A: on_session_reset gate dead code; Bug B: 4-way ladder LRU thrashing on small palette working sets) with unit-test coverage per fix, then validate via the e2e.

**Architecture:** Bug A: move session-reset state from "did the transition happen during THIS event handler?" to an IoBridge-owned `HashSet<ConnectionHandle>` of "have we fired the reset yet for this handle?". Gate fires once from EITHER `Stream(Opened)` or `Stream(Readable)`. Bug B: reorder `acquire_or_allocate` ladder so `find_empty_slot` runs BEFORE `find_eligible_free_slot`. Each fix gets a focused unit test using a minimal test seam (`WebTransportServer::test_set_connected` for A, none needed for B). The e2e validates end-to-end.

**Tech Stack:** Rust (lib + e2e tests). No new dependencies.

**Spec:** `docs/superpowers/specs/2026-05-18-decode-error-thin-latent-bugs-design.md`

---

## File Structure

- `ghostframe-lib/src/transport/webtransport.rs` — new `#[cfg(test)] pub(crate) fn test_set_connected` (~3 lines, Task 1).
- `ghostframe-lib/src/transport/io_bridge.rs`:
  - New struct field `session_resets_fired: HashSet<ConnectionHandle>` (Task 2).
  - Extracted `fire_session_reset` method (Task 2; pure code move from existing inline body).
  - New `maybe_fire_session_reset` method (Task 3, TDD).
  - Wired into `Stream(Opened)` + `Stream(Readable)` + `ConnectionLost` (Task 4).
  - New unit test in `mod tests` (Task 3).
- `ghostframe-lib/src/encoder/pal_rle.rs`:
  - Reordered ladder in `acquire_or_allocate` (Task 5).
  - New unit test (Task 5, TDD).
- `docs/superpowers/specs/2026-05-13-palrle-codec-design.md` — ladder-order rationale update (Task 6).
- `ghostframe-lib/tests/e2e.rs` — drop `#[ignore]` on `e2e_decode_error_thin_uncached` + update doc comment (Task 7).
- Memory files (Task 8).

---

## Task 1: Add `WebTransportServer::test_set_connected` seam

**Files:**
- Modify: `ghostframe-lib/src/transport/webtransport.rs`

- [ ] **Step 1: Locate the `connected` field on WebTransportServer**

```bash
grep -n "connected\|pub struct WebTransportServer\|impl WebTransportServer\|fn is_connected" /home/cedric/work/ghostframe/ghostframe-lib/src/transport/webtransport.rs | head -10
```

Identify the field name (likely `connected: bool` or similar) and which `impl` block holds `is_connected`.

- [ ] **Step 2: Add the test-only setter as a sibling of `is_connected`**

In the same `impl WebTransportServer` block as `is_connected`, append:

```rust
/// Direct internal-state setter for unit tests. Production callers must
/// NOT use this — `connected` should derive from the actual handshake
/// state machine via `on_stream_readable` / `on_stream_opened`. This
/// exists as a minimal seam to unit-test `IoBridge::maybe_fire_session_reset`
/// without driving a real QUIC handshake. If `WebTransportServer`'s
/// internal state ever splits into multi-field "connected"-derivation,
/// this setter must be updated.
#[cfg(test)]
pub(crate) fn test_set_connected(&mut self, value: bool) {
    self.connected = value;
}
```

If the field name differs (e.g., `is_connected_flag`, `state`, etc.), adjust the setter body — the goal is to make `wt.is_connected()` return `value` after the call. If `connected` is derived from multi-field state, set whichever field(s) cause `is_connected()` to return the desired value, and update the doc comment accordingly.

- [ ] **Step 3: Verify the lib still compiles in both cfg profiles**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build -p ghostframe-lib --tests 2>&1 | tail -5
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build -p ghostframe-lib --release --no-default-features 2>&1 | tail -5
```

Expected: both succeed. The `#[cfg(test)]` gate means the production build doesn't see the function.

- [ ] **Step 4: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/src/transport/webtransport.rs
git commit -m "$(cat <<'EOF'
test(webtransport): cfg-gated test_set_connected seam

Adds WebTransportServer::test_set_connected as a minimal #[cfg(test)]
setter for the internal `connected` flag. Lets unit tests of
IoBridge::maybe_fire_session_reset (Task 3) drive the gate without
needing a real QUIC handshake. Comment flags the seam as brittle —
must be updated if `connected` becomes a derived field.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Add `session_resets_fired` field + extract `fire_session_reset` method

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Add the `HashSet` import (if not already imported)**

```bash
grep -n "use std::collections::HashSet\|use std::collections::HashMap" /home/cedric/work/ghostframe/ghostframe-lib/src/transport/io_bridge.rs | head -3
```

If `HashSet` is not in the imports, add it. The existing `HashMap` import probably looks like:
```rust
use std::collections::HashMap;
```

Change to:
```rust
use std::collections::{HashMap, HashSet};
```

- [ ] **Step 2: Add the struct field on `IoBridge`**

Locate the `IoBridge` struct (around line 87 — search `pub struct IoBridge`). Add a new field next to the existing `wt_sessions` field:

```rust
    /// Per-handle "have we already fired on_session_reset for this
    /// connection?" tracking. Set when `maybe_fire_session_reset` runs
    /// for a handle; cleared on `Event::ConnectionLost` so a rare
    /// reconnect-with-same-handle re-fires the reset.
    session_resets_fired: HashSet<ConnectionHandle>,
```

NOT cfg-gated — production behavior change.

- [ ] **Step 3: Initialize in the production constructor `IoBridge::new`**

In `IoBridge::new` (around line 310), inside the `Ok(Self { ... })` literal, add:

```rust
session_resets_fired: HashSet::new(),
```

Place it next to the existing `wt_sessions: HashMap::new(),` line.

- [ ] **Step 4: Initialize in the two test-only constructors**

Same change in both `new_with_stream_for_test` and `new_with_frames_for_test`. Add:

```rust
session_resets_fired: HashSet::new(),
```

next to the `wt_sessions: HashMap::new(),` line in each.

- [ ] **Step 5: Extract `fire_session_reset` method**

Locate the existing inline reset body inside `Stream(Readable)` event handler — search for `self.palette_table.on_session_reset(`:

```bash
grep -n "palette_table.on_session_reset" /home/cedric/work/ghostframe/ghostframe-lib/src/transport/io_bridge.rs
```

It's the `Stream(Readable)` branch, around line 1454-1490. The current shape:

```rust
Event::Stream(StreamEvent::Readable { id }) => {
    if let (Some(wt), Some(conn)) = (
        self.wt_sessions.get_mut(&handle),
        self.server.connections.get_mut(&handle),
    ) {
        let was_connected = wt.is_connected();
        wt.on_stream_readable(conn, id);
        if !was_connected && wt.is_connected() {
            // ... 30+ lines of reset body: dirty_tracker, metrics_tracker,
            // classifier, scheduler, palette_table, frame_mode, etc.
        }
    }
}
```

Extract everything inside the `if !was_connected && wt.is_connected() {` body into a new method on IoBridge. Replace the inline body with a method call (we'll wire `maybe_fire_session_reset` in Task 4; for this task, replace with a direct `self.fire_session_reset(handle)` call so the existing tests don't break):

```rust
Event::Stream(StreamEvent::Readable { id }) => {
    if let (Some(wt), Some(conn)) = (
        self.wt_sessions.get_mut(&handle),
        self.server.connections.get_mut(&handle),
    ) {
        let was_connected = wt.is_connected();
        wt.on_stream_readable(conn, id);
        if !was_connected && wt.is_connected() {
            self.fire_session_reset(handle);
        }
    }
}
```

Then add the new method on `impl IoBridge` (near the other private methods, e.g., right after `handle_decode_error` around line 705):

```rust
/// Reset per-session state on a new WebTransport session. Called by
/// `maybe_fire_session_reset` once per new connection handle.
/// Preserves cross-session palette bytes (warm cache) and, when the
/// cfg-gated test hook GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1 is
/// active, also preserves `palette_table.delivered`.
fn fire_session_reset(&mut self, handle: ConnectionHandle) {
    self.dirty_tracker.reset();
    self.metrics_tracker.reset();
    self.classifier.reset();
    self.scheduler.clear();
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
    self.frame_mode = crate::tile::FrameMode::H264;
    self.dimensions_retransmits_left = FRAME_DIMENSIONS_RETRANSMITS;
    if self.gpu_frame_processor.is_none() {
        self.force_dirty_frames = 20;
    }
    if let Some(enc) = self.full_frame_encoder.as_mut() {
        enc.request_keyframe();
    }
    tracing::debug!(?handle, "new session connected, dirty tracker reset");
}
```

Copy the body line-by-line from the existing inline block — make NO logic changes. (Comments inside the body can be condensed if desired since they all describe the same overall "new session" behavior; the doc comment on the method captures the gist.)

- [ ] **Step 6: Verify build + run all lib tests**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build -p ghostframe-lib --tests 2>&1 | tail -5
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build -p ghostframe-lib --release --no-default-features 2>&1 | tail -5
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib 2>&1 | tail -5
```

Expected: all three succeed. Lib tests: 280 passed (no new tests yet).

- [ ] **Step 7: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "$(cat <<'EOF'
refactor(io_bridge): extract fire_session_reset + add session_resets_fired field

Pure refactor: pulls the 25-line new-session reset body out of the
inline Stream(Readable) handler into a fire_session_reset(handle)
method. Adds session_resets_fired: HashSet<ConnectionHandle> to
IoBridge (initialized empty in all three constructors). The field is
unused so far; Task 3 wires it into a new maybe_fire_session_reset
gate. No behavior change.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Add `maybe_fire_session_reset` + TDD unit test

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Write the failing unit test FIRST**

In the `mod tests` block of `io_bridge.rs`, near the existing `oob_injector_from_env_parses` and `skip_palette_session_reset_from_env_parses` tests, add this verbatim:

```rust
#[test]
fn maybe_fire_session_reset_fires_exactly_once_per_handle() {
    use crate::transport::quic::QuicServer;
    use crate::transport::webtransport::WebTransportServer;
    use tokio::net::UnixStream as TokioUnixStream;
    use quinn_proto::ConnectionHandle;

    let (stream, _peer) = TokioUnixStream::pair().expect("UnixStream::pair");
    let server = QuicServer::test_default();
    let mut bridge = IoBridge::new_with_stream_for_test(stream, server);

    // Seed an observable side-effect: delivered bit on palette 7.
    // fire_session_reset is expected to clear delivered (preserve_delivered
    // defaults to false in this test path; the cfg-gated skip hook is off).
    bridge.palette_table.delivered.insert(7);

    let handle = ConnectionHandle(0);
    bridge.wt_sessions.insert(handle, WebTransportServer::default());

    // Pre-condition: WT not yet connected → maybe_fire_session_reset must NOT fire.
    bridge.maybe_fire_session_reset(handle);
    assert!(
        bridge.palette_table.delivered.contains(7),
        "not-yet-connected: reset must not fire"
    );

    // Promote: now connected. First call → fires (delivered cleared).
    bridge
        .wt_sessions
        .get_mut(&handle)
        .expect("wt session present")
        .test_set_connected(true);
    bridge.maybe_fire_session_reset(handle);
    assert!(
        !bridge.palette_table.delivered.contains(7),
        "first call after connected: reset must fire (delivered cleared)"
    );

    // Re-arm delivered. Second call must NOT re-fire (gated by session_resets_fired).
    bridge.palette_table.delivered.insert(7);
    bridge.maybe_fire_session_reset(handle);
    assert!(
        bridge.palette_table.delivered.contains(7),
        "second call: reset must not re-fire (gated by session_resets_fired)"
    );
}
```

Verify the imports exist or add them at the top of the `mod tests` block. The existing test functions in this file should give the right pattern — search the existing tests for how `IoBridge::new_with_stream_for_test`, `QuicServer::test_default`, and `WebTransportServer::default` are constructed.

If `QuicServer::test_default()` doesn't exist by that name, find the right constructor by searching:
```bash
grep -nE "QuicServer.*for_test|QuicServer::new|fn test_default" /home/cedric/work/ghostframe/ghostframe-lib/src/transport/quic.rs | head -5
```
And substitute the right one.

If `WebTransportServer::default()` isn't derived — check by searching for `Default` impl. If not derived, instantiate via whatever the canonical zero-state constructor is (`WebTransportServer::new()` or similar).

- [ ] **Step 2: Run the test to verify it fails to compile**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib maybe_fire_session_reset_fires_exactly_once_per_handle 2>&1 | tail -15
```

Expected: COMPILE ERROR — `maybe_fire_session_reset` method does not exist on `IoBridge`.

- [ ] **Step 3: Implement `maybe_fire_session_reset`**

Add as a new method on `impl IoBridge`, immediately before or after `fire_session_reset` (which was added in Task 2):

```rust
/// Gate for `fire_session_reset`: fires exactly once per new connection
/// handle when its `WebTransportServer` reports `is_connected() == true`.
/// Both `Stream(Opened)` and `Stream(Readable)` event handlers call this
/// after their respective `wt.*` call; whichever event handler observes
/// the `is_connected()` promotion first fires the reset.
fn maybe_fire_session_reset(&mut self, handle: ConnectionHandle) {
    let Some(wt) = self.wt_sessions.get(&handle) else { return; };
    if wt.is_connected() && self.session_resets_fired.insert(handle) {
        self.fire_session_reset(handle);
    }
}
```

The `HashSet::insert` returns `true` if the value was NEWLY inserted, `false` if it was already present — so this fires exactly once per handle.

- [ ] **Step 4: Run the unit test again, verify it passes**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib maybe_fire_session_reset_fires_exactly_once_per_handle 2>&1 | tail -5
```

Expected: 1 passed; 0 failed.

If FAIL with `WebTransportServer` not having a `default()` or `test_set_connected` — the test seam from Task 1 isn't quite right; revisit Task 1 Step 2.

If FAIL with `palette_table.delivered.insert` not found — check the public API of `PaletteTable.delivered` (`PaletteIdBitSet`); use whatever method the existing tests use.

- [ ] **Step 5: Run all lib tests to catch regressions**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib 2>&1 | tail -5
```

Expected: 281 passed (280 baseline + 1 new). 0 failed.

- [ ] **Step 6: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "$(cat <<'EOF'
feat(io_bridge): maybe_fire_session_reset gate + exactly-once unit test

The gate predicate is `wt.is_connected() && session_resets_fired.insert(handle)`.
HashSet::insert returns true only on first insertion, so the reset
runs exactly once per handle regardless of how many event-handler
sites call maybe_fire_session_reset. Unit test verifies:
  (a) gate respects is_connected
  (b) reset body's observable side effect (delivered cleared)
  (c) exactly-once firing per handle

Task 4 wires this into the actual event handlers.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Wire `maybe_fire_session_reset` into event handlers + ConnectionLost cleanup

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Locate the three event handlers to update**

```bash
grep -n "Event::Stream(StreamEvent::\(Opened\|Readable\)\|Event::ConnectionLost" /home/cedric/work/ghostframe/ghostframe-lib/src/transport/io_bridge.rs | head -10
```

Three sites:
- `Event::Stream(StreamEvent::Opened { dir })` — around line 1429.
- `Event::Stream(StreamEvent::Readable { id })` — around line 1439 (this is where Task 2's `self.fire_session_reset(handle)` direct call lives currently).
- `Event::ConnectionLost { reason }` — around line 1493.

- [ ] **Step 2: Wire into `Stream(Opened)` handler**

Currently (post-Task 2):
```rust
Event::Stream(StreamEvent::Opened { dir }) => {
    if let (Some(wt), Some(conn)) = (
        self.wt_sessions.get_mut(&handle),
        self.server.connections.get_mut(&handle),
    ) {
        wt.on_stream_opened(conn, dir);
    }
}
```

Add a `self.maybe_fire_session_reset(handle);` call AFTER the `wt.on_stream_opened(...)` line, OUTSIDE the `if let` borrow scope (since `maybe_fire_session_reset` takes `&mut self`):

```rust
Event::Stream(StreamEvent::Opened { dir }) => {
    if let (Some(wt), Some(conn)) = (
        self.wt_sessions.get_mut(&handle),
        self.server.connections.get_mut(&handle),
    ) {
        wt.on_stream_opened(conn, dir);
    }
    self.maybe_fire_session_reset(handle);
}
```

- [ ] **Step 3: Replace the inline gate in `Stream(Readable)` with `maybe_fire_session_reset`**

Currently (post-Task 2):
```rust
Event::Stream(StreamEvent::Readable { id }) => {
    if let (Some(wt), Some(conn)) = (
        self.wt_sessions.get_mut(&handle),
        self.server.connections.get_mut(&handle),
    ) {
        let was_connected = wt.is_connected();
        wt.on_stream_readable(conn, id);
        if !was_connected && wt.is_connected() {
            self.fire_session_reset(handle);
        }
    }
}
```

Replace with:
```rust
Event::Stream(StreamEvent::Readable { id }) => {
    if let (Some(wt), Some(conn)) = (
        self.wt_sessions.get_mut(&handle),
        self.server.connections.get_mut(&handle),
    ) {
        wt.on_stream_readable(conn, id);
    }
    self.maybe_fire_session_reset(handle);
}
```

The `was_connected` shadowing is gone — `maybe_fire_session_reset` uses `session_resets_fired` as the gate state instead.

- [ ] **Step 4: Add cleanup in `ConnectionLost`**

Currently:
```rust
Event::ConnectionLost { reason } => {
    tracing::info!(?handle, %reason, "connection lost");
    self.wt_sessions.remove(&handle);
}
```

Add the `session_resets_fired` cleanup:
```rust
Event::ConnectionLost { reason } => {
    tracing::info!(?handle, %reason, "connection lost");
    self.wt_sessions.remove(&handle);
    self.session_resets_fired.remove(&handle);
}
```

- [ ] **Step 5: Verify build + all lib tests**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build -p ghostframe-lib --tests 2>&1 | tail -5
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build -p ghostframe-lib --release --no-default-features 2>&1 | tail -5
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib 2>&1 | tail -5
```

Expected: all three succeed. Lib tests: 281 passed.

- [ ] **Step 6: Quick smoke-test with an existing e2e that exercises session establishment**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_solid_color -- --test-threads=1 --nocapture 2>&1 | tee /tmp/wire_smoke.log | tail -10
grep "new session connected" /tmp/wire_smoke.log
```

Expected: `e2e_solid_color` passes; `new session connected, dirty tracker reset` debug line appears (was previously absent due to bug A).

If the test FAILS with a new behavior (e.g., panic in the H.264 path because frame_mode reset triggers something unexpected), the bug A side effects are exposing a pre-existing issue. Investigate the failure trace before continuing.

- [ ] **Step 7: Rebuild docker image (server-side change in lib affects xdaemon binary)**

```bash
cd /home/cedric/work/ghostframe && docker build -t ghostframe/test-server:latest -f tests/containers/test-server/Dockerfile . 2>&1 | tail -3
```

Expected: "Successfully tagged ghostframe/test-server:latest". If "no space left on device", run `docker system prune -a -f` first and retry.

- [ ] **Step 8: Run the full e2e suite for regression**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e -- --test-threads=1 2>&1 | tee /tmp/wire_full.log | tail -5
```

Expected: 22 passed, 0 failed, 3 ignored (baseline from `f8019a0`). The e2e_decode_error_thin_uncached is still `#[ignore]`'d at this stage — Task 7 drops it.

If regressions appear, the bug A wiring exposed a side effect that breaks something. Inspect `/tmp/wire_full.log` for the failing test's panic; common candidates: the `force_dirty_frames=20` CPU-path mitigation now actually firing and causing slow-start issues.

- [ ] **Step 9: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "$(cat <<'EOF'
fix(io_bridge): wire maybe_fire_session_reset into event handlers (Bug A)

Calls maybe_fire_session_reset from BOTH Stream(Opened) and
Stream(Readable) handlers. The gate (session_resets_fired::insert)
ensures exactly-once firing per handle regardless of which event
handler observes the is_connected() promotion. Also clears the
handle from session_resets_fired on ConnectionLost.

Eliminates the previous dead-code gate (!was_connected &&
wt.is_connected()) at Stream(Readable), which never fired because
Stream(Opened)'s eager on_stream_opened completes the CONNECT
handshake (and flips is_connected to true) BEFORE Stream(Readable)
arrives. See project_decode_error_thin_diagnosis.md.

Verified: e2e_solid_color sees the "new session connected" debug
log (previously absent). Full e2e suite: 22 passed, no regressions.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Reorder `acquire_or_allocate` ladder + TDD unit test (Bug B)

**Files:**
- Modify: `ghostframe-lib/src/encoder/pal_rle.rs`

- [ ] **Step 1: Write the failing unit test FIRST**

In the `mod tests` block of `pal_rle.rs`, near the existing `acquire_or_allocate`-related tests (search for `acquire_or_allocate_` to find the locality), add this verbatim:

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
    let id_red = t.acquire_or_allocate(&p_red).expect("alloc red");
    assert_eq!(id_red, 0);
    t.release(id_red);                  // end-of-frame release
    t.delivered.insert(id_red);         // simulate ACK arrival

    // Frame 2: blue. MUST land in empty slot 1, NOT overwrite slot 0.
    // (This is the regression we're testing — pre-fix, find_eligible_free_slot
    // would return slot 0 as oldest LRU and write_bytes would clear
    // delivered[0]. Post-fix, find_empty_slot returns slot 1 first.)
    let id_blue = t.acquire_or_allocate(&p_blue).expect("alloc blue");
    assert_eq!(
        id_blue, 1,
        "blue should land in empty slot 1, not overwrite slot 0"
    );
    assert!(
        t.delivered.contains(id_red),
        "delivered on slot 0 must survive the slot 1 allocation"
    );
    t.release(id_blue);
    t.delivered.insert(id_blue);

    // Frame 3: red again. find_matching hits slot 0; no write_bytes call.
    let id_red_2 = t.acquire_or_allocate(&p_red).expect("re-alloc red");
    assert_eq!(id_red_2, 0);
    assert!(
        t.delivered.contains(0),
        "find_matching path must preserve delivered"
    );
    t.release(id_red_2);

    // Frame 4: blue again. find_matching hits slot 1.
    let id_blue_2 = t.acquire_or_allocate(&p_blue).expect("re-alloc blue");
    assert_eq!(id_blue_2, 1);
    assert!(t.delivered.contains(1));
}
```

- [ ] **Step 2: Run the test, verify it fails with the CURRENT (pre-fix) ladder order**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib acquire_or_allocate_uses_empty_slots_before_evicting_cached 2>&1 | tail -15
```

Expected: FAIL with `assertion failed: id_blue == 1` (actual: 0). Confirms the bug: slot 0 gets overwritten because find_eligible_free_slot runs before find_empty_slot.

If it passes UNEXPECTEDLY, the ladder may have already been reordered by someone else (check git log on pal_rle.rs); or there's a subtle palette-content equality issue making find_matching hit on the wrong slot. Investigate before continuing.

- [ ] **Step 3: Reorder the ladder in `acquire_or_allocate`**

Locate `acquire_or_allocate`:
```bash
grep -nA20 "pub fn acquire_or_allocate" /home/cedric/work/ghostframe/ghostframe-lib/src/encoder/pal_rle.rs | head -25
```

Currently (line 228 in commit `f8019a0`):
```rust
pub fn acquire_or_allocate(&mut self, palette: &PaletteEntry) -> Option<u8> {
    // 1. find_matching → reuse
    if let Some(id) = self.find_matching(palette) {
        self.acquire(id);
        return Some(id);
    }
    // 2. find_eligible_free_slot → overwrite
    if let Some(id) = self.find_eligible_free_slot() {
        self.write_bytes(id, palette);
        self.acquire(id);
        return Some(id);
    }
    // 3. find_empty_slot → write
    if let Some(id) = self.find_empty_slot() {
        self.write_bytes(id, palette);
        self.acquire(id);
        return Some(id);
    }
    // 4. fail
    None
}
```

Swap paths 2 and 3:

```rust
pub fn acquire_or_allocate(&mut self, palette: &PaletteEntry) -> Option<u8> {
    // 1. find_matching → reuse existing slot
    if let Some(id) = self.find_matching(palette) {
        self.acquire(id);
        return Some(id);
    }
    // 2. find_empty_slot → write to truly-fresh slot
    //    (preserves existing FreeButCached entries for future
    //    find_matching hits; avoids LRU thrashing on small palette sets)
    if let Some(id) = self.find_empty_slot() {
        self.write_bytes(id, palette);
        self.acquire(id);
        return Some(id);
    }
    // 3. find_eligible_free_slot → evict oldest FreeButCached only
    //    when no truly-empty slot exists
    if let Some(id) = self.find_eligible_free_slot() {
        self.write_bytes(id, palette);
        self.acquire(id);
        return Some(id);
    }
    // 4. fail (caller falls back to Codec::Raw for the tile)
    None
}
```

- [ ] **Step 4: Re-run the test, verify it passes**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib acquire_or_allocate_uses_empty_slots_before_evicting_cached 2>&1 | tail -5
```

Expected: 1 passed; 0 failed.

- [ ] **Step 5: Run all lib tests for regression**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib 2>&1 | tail -5
```

Expected: 282 passed (281 baseline + 1 new). 0 failed.

Pay particular attention to other `acquire_or_allocate_*` tests (find with `grep -n "fn acquire_or_allocate" /home/cedric/work/ghostframe/ghostframe-lib/src/encoder/pal_rle.rs`). If any pre-existing test fails:
- It probably depended on the OLD order's "prefer reused FreeButCached" semantics.
- Inspect the test's expected slot ID — if it expected slot 0 (after reset) but now sees slot 1 (because the empty slot path wins), the test was over-specifying the implementation detail and should be relaxed.

- [ ] **Step 6: Rebuild docker image (lib change affects xdaemon binary)**

```bash
cd /home/cedric/work/ghostframe && docker build -t ghostframe/test-server:latest -f tests/containers/test-server/Dockerfile . 2>&1 | tail -3
```

Expected: "Successfully tagged ghostframe/test-server:latest".

- [ ] **Step 7: Quick e2e smoke-test on a PalRle-using test**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_palrle_5pct_loss e2e_palrle_oob_index e2e_solid_per_tile_pixels -- --test-threads=1 2>&1 | tee /tmp/palrle_smoke.log | tail -5
```

Expected: 3 passed. Spot-check the log for the `in_flight_carrying underflow` warning — it should appear ZERO times now (was 660+ per test pre-fix):

```bash
grep -c "in_flight_carrying underflow" /tmp/palrle_smoke.log
```

Expected: 0.

- [ ] **Step 8: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/src/encoder/pal_rle.rs
git commit -m "$(cat <<'EOF'
fix(pal_rle): reorder acquire_or_allocate ladder (Bug B)

Swap paths 2 and 3 so find_empty_slot runs BEFORE find_eligible_free_slot.
Strictly better: never worse than today (cache still bounded at 256
slots), much better for small palette working sets where the previous
order thrashed slot 0 (find_eligible_free_slot returned the same oldest
slot every cycle in an N=2 palette flip).

Eliminates the per-frame slot overwrite + 660+ in_flight_carrying
underflow warnings observed in e2e_solid_per_tile_pixels and
e2e_decode_error_thin_uncached. Existing FreeButCached entries now
survive across many frames, enabling find_matching to keep both
2-color flip palettes resident with `delivered=true` indefinitely.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Update PalRle design doc

**Files:**
- Modify: `docs/superpowers/specs/2026-05-13-palrle-codec-design.md`

- [ ] **Step 1: Locate the "Per-frame allocation algorithm" section**

```bash
grep -nA8 "Per-frame allocation algorithm" /home/cedric/work/ghostframe/docs/superpowers/specs/2026-05-13-palrle-codec-design.md
```

Currently lines 565-572:
```
### Per-frame allocation algorithm

See `acquire_or_allocate` in Phase A above. The 4-way ladder:

1. `find_matching` — full byte-equal scan over (Held ∪ FreeButCached). Returns existing slot id; `acquire` increments ref_count.
2. `find_eligible_free_slot` — `free_lru` oldest entry that passes `overwrite_eligible`. On hit: overwrite bytes, clear delivered, zero `in_flight_carrying`, acquire.
3. `find_empty_slot` — first `Empty`. On hit: write bytes, acquire.
4. fail (return None) — caller falls back to `Codec::Raw` for the tile.
```

- [ ] **Step 2: Replace with the new order + rationale**

Replace the entire `### Per-frame allocation algorithm` section body (the 8 lines above) with:

```
### Per-frame allocation algorithm

See `acquire_or_allocate` in Phase A above. The 4-way ladder:

1. `find_matching` — full byte-equal scan over (Held ∪ FreeButCached). Returns existing slot id; `acquire` increments ref_count.
2. `find_empty_slot` — first `Empty`. On hit: write bytes, acquire.
3. `find_eligible_free_slot` — `free_lru` oldest entry that passes `overwrite_eligible`. On hit: overwrite bytes, clear delivered, zero `in_flight_carrying`, acquire.
4. fail (return None) — caller falls back to `Codec::Raw` for the tile.

**Rationale for the path-2/path-3 order**: prefer truly-fresh empty slots to extend cache lifetime; only evict a FreeButCached entry when the cache is genuinely full. An earlier version of this design (pre-2026-05-18) had paths 2 and 3 swapped, which thrashed slot 0 on small palette working sets — a 2-color flip would find_eligible_free_slot the same oldest slot every cycle, write_bytes it (clearing delivered), and the existing FreeButCached entry for the OTHER colour was never used. With the current order, an N=2 working set occupies slots 0 and 1 and find_matching hits both stably; eviction only kicks in once all 256 slots are non-Empty.
```

- [ ] **Step 3: Verify the doc still renders sensibly**

```bash
sed -n "563,580p" /home/cedric/work/ghostframe/docs/superpowers/specs/2026-05-13-palrle-codec-design.md
```

Visual scan: the section reads coherently and the surrounding sections (Phase A reference, Connection lifecycle) still make sense.

- [ ] **Step 4: Commit**

```bash
cd /home/cedric/work/ghostframe
git add docs/superpowers/specs/2026-05-13-palrle-codec-design.md
git commit -m "$(cat <<'EOF'
docs(palrle): document reordered 4-way allocation ladder (Bug B)

Updates the canonical PalRle codec design to reflect the new path order
(find_empty before find_eligible_free) and adds a one-paragraph
rationale explaining why the previous order thrashed slot 0 on small
working sets.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Drop `#[ignore]` on `e2e_decode_error_thin_uncached` + validate

**Files:**
- Modify: `ghostframe-lib/tests/e2e.rs`

- [ ] **Step 1: Locate the test**

```bash
grep -nB18 "fn e2e_decode_error_thin_uncached" /home/cedric/work/ghostframe/ghostframe-lib/tests/e2e.rs | head -25
```

There's a multi-line `// W3 / A5 / B6` comment block above it (from commit `70efa56`) describing the prior carry-over rationale.

- [ ] **Step 2: Replace the rationale comment + drop the #[ignore]**

Replace the entire comment block above `#[ignore = ...]` and the `#[ignore]` line itself with a brief closure comment:

Before (current):
```rust
// Re-investigated 2026-05-17/18 via natural-pipeline approach (session-reset
// hook + page.reload). Diagnosis confirmed the test cannot fire
// ERR_THIN_UNCACHED via the natural pipeline under current architecture for
// two independent reasons:
//
//   (1) palette_table.on_session_reset is dead code in production: the
//       new-session gate at io_bridge.rs:1446 (!was_connected &&
//       wt.is_connected()) never fires because Stream(Opened)'s eager-read
//       completes the CONNECT handshake before Stream(Readable) runs, so
//       was_connected is already true when the readable arrives.
//
//   (2) Even with delivered preserved, a 2-color flip thrashes palette
//       slot 0 every cycle: find_matching fails, find_eligible_free_slot
//       returns slot 0 (oldest LRU), write_bytes overwrites and clears
//       delivered. The server emits BUNDLED then THIN within the same
//       flip cycle, so the client never sees thin against an empty shadow.
//
// Full diagnosis at /home/cedric/.claude/projects/-home-cedric-work-ghostframe/
// memory/project_decode_error_thin_diagnosis.md.  Closing this test cleanly
// requires fixing (1) and (2), both of which are out of M3.2c scope.
#[ignore = "M3.2c carry-over: natural pipeline blocked by latent on_session_reset dead-code + LRU thrashing on 2-palette workloads — see project_decode_error_thin_diagnosis.md"]
```

After:
```rust
// Closed 2026-05-18 by fixing the two latent prod bugs identified in
// the original diagnosis (project_decode_error_thin_diagnosis.md):
// Bug A — IoBridge gate now uses session_resets_fired (HashSet) instead
// of !was_connected; fires once per handle from EITHER Stream(Opened)
// or Stream(Readable). Bug B — acquire_or_allocate ladder reordered so
// find_empty_slot runs before find_eligible_free_slot, eliminating the
// 2-palette slot-0 thrashing. End-to-end: the natural pipeline now
// drives ERR_THIN_UNCACHED_PALETTE from server emission of thin against
// a post-reload empty client shadow.
```

Drop the entire `#[ignore = "..."]` line.

- [ ] **Step 3: Verify the test compiles**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build --tests -p ghostframe-lib 2>&1 | tail -5
```

Expected: build succeeds.

- [ ] **Step 4: Run the test**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_decode_error_thin_uncached -- --test-threads=1 --nocapture 2>&1 | tee /tmp/decode_validate.log | tail -20
```

Expected: 1 passed.

If FAIL: proceed to the contingency below. Do NOT commit until either (a) the test passes, or (b) a definite remediation path is identified and approved.

#### Contingency: if Task 7 Step 4 fails

The two latent bugs were the diagnosis's full root-cause set. If the test still fails, a third blocker exists. To diagnose:

(a) Re-add the four client-side recorders from the prior diagnosis session (`__ghostframeRecordedPrevalidate`, `__ghostframeDecodeErrorReports`, `__ghostframeDecodeErrorFlushes`, `__ghostframeFeedbackWriteErrors`) and the test-side dumps — see `/home/cedric/work/ghostframe/.claude/decode_thin_diag_findings.md` for the exact patches that were applied uncommitted in commit `70efa56`'s predecessor session.

(b) Re-run the test, classify the new failure into the hypothesis grid in that same findings file (H1: prevalidate ever fires errorCode=3? H2: DecodeErrorBatcher.report called? H3: batcher flushes? H4: feedback write errors?).

(c) Write a findings addendum to the spec at `docs/superpowers/specs/2026-05-18-decode-error-thin-latent-bugs-design.md` documenting the new blocker.

(d) Brainstorm the remediation in a separate session (do NOT continue this plan).

(e) Revert the test's `#[ignore]` change with an updated rationale citing the third blocker. Commit that revert.

- [ ] **Step 5: Commit (only if Step 4 passed)**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/tests/e2e.rs
git commit -m "$(cat <<'EOF'
test(e2e): close e2e_decode_error_thin_uncached via Bug A + Bug B fixes

The natural pipeline now fires ERR_THIN_UNCACHED_PALETTE end-to-end:
GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1 preserves delivered on session
reconnect, page.reload clears the client shadow, server emits thin
against the empty shadow, client errors, server logs and force_rebundle.

Closes the M3.2c A5/B6 carry-over.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Memory updates + final regression

**Files:**
- Modify: `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_decode_error_thin_diagnosis.md`
- Modify: `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m32c_near_complete.md`
- Modify: `~/.claude/projects/-home-cedric-work-ghostframe/memory/MEMORY.md`

- [ ] **Step 1: Mark both latent bugs as closed in the diagnosis memory**

Open `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_decode_error_thin_diagnosis.md`. Prepend a "## Closed 2026-05-18" section at the top of the body (after the front-matter), citing the commits:

```markdown
## Closed 2026-05-18

Both latent bugs FIXED. `e2e_decode_error_thin_uncached` now passes
via the natural pipeline.

- **Bug A** closed by commits `<task 1 sha>`, `<task 2 sha>`, `<task 3 sha>`,
  `<task 4 sha>` — IoBridge::session_resets_fired gate replaces the
  dead `!was_connected && wt.is_connected()` pattern; reset fires
  exactly once per handle from EITHER Stream(Opened) or Stream(Readable).
- **Bug B** closed by commit `<task 5 sha>` — acquire_or_allocate
  reordered to prefer find_empty_slot over find_eligible_free_slot.
- **e2e validation** in commit `<task 7 sha>`.
- **PalRle design doc** updated in commit `<task 6 sha>`.

The body below remains as historical record of the 2026-05-17/18
diagnosis that uncovered these bugs.

---
```

Fill in the commit SHAs from `git log --oneline` after Task 7. If Task 7 fell into the contingency path and ended with a revert, the closure section instead documents what's STILL blocking (the new third bug) and references the addendum.

- [ ] **Step 2: Update project_m32c_near_complete.md**

In `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m32c_near_complete.md`:

- Bump the suite count line. Change `**22 passed, 0 failed, 3 ignored**` to `**23 passed, 0 failed, 2 ignored**`.
- Remove the `e2e_decode_error_thin_uncached` row from the "Ignored test" table.
- In the "Remaining for full M3.2c closure" section, the B2 bullet referenced "same blocker as A5/B6". Now that A5/B6 is closed, update the bullet — B2 should now be tractable (the same `delivered=true + subsequent dirty` pattern works post-bug-A fix). Suggest: "B2 wire-level assertion via __ghostframeRecordedFlags — was blocked on bug A (on_session_reset dead code). With bug A fixed, the natural ACK→delivered=true→subsequent thin path now works; B2 closure is a small follow-up." (Don't actually fix B2 in this plan — just note it's now possible.)
- In the "2026-05-18 session additions" section, add a sub-section "Latent bug fixes" listing the bug A + bug B commits.

- [ ] **Step 3: Update MEMORY.md index line**

In `~/.claude/projects/-home-cedric-work-ghostframe/memory/MEMORY.md`, update the line that currently reads:

```
- [M3.2c near-complete (W3/B3+B7/W4/W5 closed)](project_m32c_near_complete.md) — Sun 2026-05-17/18: B3 + B7 + W4 SSIM golden + W5 (writeRawTile clip) all landed; 22 e2e pass, 3 ignored. A5/B6 + B2 deferred pending 2 latent prod bugs (on_session_reset dead-code + LRU thrashing)
```

To:

```
- [M3.2c near-complete (W3/B3+B7/W4/W5/A5+B6 closed)](project_m32c_near_complete.md) — Mon 2026-05-18: B3 + B7 + W4 + W5 + A5/B6 all landed (the latter via fixing 2 latent prod bugs: on_session_reset gate + LRU thrashing); 23 e2e pass, 2 ignored
```

- [ ] **Step 4: Run the FULL e2e suite to confirm green**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e -- --test-threads=1 2>&1 | tee /tmp/decode_final.log | tail -10
```

Expected: 23 passed, 0 failed, 2 ignored.

- [ ] **Step 5: Run lib unit tests**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib 2>&1 | tail -5
```

Expected: 282 passed (280 prior + maybe_fire_session_reset_fires_exactly_once_per_handle + acquire_or_allocate_uses_empty_slots_before_evicting_cached). 0 failed.

- [ ] **Step 6: Run vitest sanity**

```
cd /home/cedric/work/ghostframe/ghostframe-web-client && npm test 2>&1 | tail -5
```

Expected: 27 passed (unchanged — no client-side changes in this plan).

- [ ] **Step 7: Final commit if any straggler tweaks needed**

```bash
cd /home/cedric/work/ghostframe
git status --short
```

If clean (modulo the pre-existing `.claude/`, `librust_out.rlib`, `tmux-client-25660.log` entries), the plan is complete. Memory dir is NOT under git.

If there are tree changes from earlier steps that weren't yet committed (shouldn't be the case, but verify), commit them with a `chore: cleanup` message.

---

## Notes on parallelization

Bug A (Tasks 1-4) and Bug B (Task 5) are file-disjoint. They could run in parallel. The plan sequences them serially because:
- The user explicitly asked for "first a) then b)".
- Sequential commits remain bisectable if a surprise crops up.
- The Task 6 doc update logically follows Task 5.
- Task 7 (validation) requires BOTH bug fixes; running it after either alone would still fail.

Task 8 always runs last.

Subagent dispatch: one Task per subagent invocation. Tasks within a stream depend on the prior task's commits, so a single failure mid-stream halts the rest pending fix.

## Contingency summary

The only contingency is in Task 7 Step 4 (if the e2e fails after both fixes). The path is fully spec'd in the inline "Contingency: if Task 7 Step 4 fails" block — diagnose, document, brainstorm separately, revert. Do NOT continue Task 8 if the contingency fires.
