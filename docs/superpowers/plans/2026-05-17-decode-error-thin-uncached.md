# `e2e_decode_error_thin_uncached` Closure Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close `e2e_decode_error_thin_uncached` (M3.2c A5/B6 carry-over) by adding a server-side `GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1` hook that simulates the production failure mode (server retains delivered state across what it thought was a stable session; client loses palette shadow on tab discard / WebGPU device loss) via `page.reload()`, then rewriting the test to drive the natural pipeline end-to-end.

**Architecture:** Three units of work. (1) `pal_rle.rs::on_session_reset` gains a `preserve_delivered: bool` parameter (TDD-driven new unit test). (2) `io_bridge.rs` gets a cfg-gated env-var parser + struct field + call-site update at line 1435. (3) `solid_per_tile.rs` motion region switches from 256-color cycle to 2-color flip so palette IDs match across the page reload; e2e test gets rewritten with the new env-var and assertions.

**Tech Stack:** Rust (lib + test-pattern + e2e tests), TypeScript (web client — no client-side changes), chromiumoxide `page.reload()`.

**Spec:** `docs/superpowers/specs/2026-05-17-decode-error-thin-uncached-design.md`

---

## File Structure

- `ghostframe-lib/src/encoder/pal_rle.rs` — `on_session_reset` signature change (`preserve_delivered: bool`), call-site fixes in tests, new unit test.
- `ghostframe-lib/src/transport/io_bridge.rs` — new cfg-gated struct field, env-var parser, constructor initializations (production + 2 test constructors), call-site update at line 1435, new parser unit test.
- `ghostframe-test-pattern/src/solid_per_tile.rs` — motion-region pixel calculation: 256-color cycle → 2-color flip.
- `ghostframe-lib/tests/e2e.rs` — rewrite `e2e_decode_error_thin_uncached`; drop the `#[ignore]`; extract docker-logs ANSI-strip helper.
- `ghostframe-lib/tests/e2e/helpers.rs` — new `read_server_logs_stripped()` helper used by three call sites.
- `~/.claude/projects/-home-cedric-work-ghostframe/memory/reference_e2e_diagnose.md` — document the new env-var.

---

## Task 1: Add `preserve_delivered` parameter to `pal_rle::on_session_reset`

**Files:**
- Modify: `ghostframe-lib/src/encoder/pal_rle.rs` (function around line 195; tests around 705, 724)

- [ ] **Step 1: Write the failing test for the preserve-delivered branch**

In `ghostframe-lib/src/encoder/pal_rle.rs`, in the `mod tests` block (near the existing `on_session_reset_preserves_bytes_clears_tracking` test around line 705), add:

```rust
#[test]
fn on_session_reset_preserve_delivered_keeps_bit() {
    let mut t = PaletteTable::new();
    // Allocate and "deliver" a palette.
    let p = PaletteEntry {
        count: 2,
        colors: { let mut c = [[0u8; 4]; 16]; c[0] = [0xFF, 0, 0, 0xFF]; c[1] = [0, 0, 0xFF, 0xFF]; c },
    };
    let id = t.acquire_or_allocate(&p).unwrap();
    t.delivered.insert(id);
    t.in_flight_carrying[id as usize] = 3;
    t.ref_count[id as usize] = 1;
    assert!(t.delivered.contains(id), "precondition: delivered set");

    // preserve_delivered=true: delivered bit stays, other per-session state still resets.
    t.on_session_reset(true);
    assert!(t.delivered.contains(id),
        "preserve_delivered=true must keep the delivered bit set");
    assert_eq!(t.in_flight_carrying[id as usize], 0,
        "in_flight_carrying still resets");
    assert_eq!(t.ref_count[id as usize], 0,
        "ref_count still resets");
    assert!(t.entries[id as usize].is_some(),
        "palette bytes preserved (warm cache)");
}
```

- [ ] **Step 2: Run the test to verify it fails to compile**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib on_session_reset_preserve_delivered_keeps_bit 2>&1 | tail -10
```

Expected: COMPILE ERROR — `on_session_reset` takes 0 args, you passed 1. Confirms the parameter doesn't exist yet.

- [ ] **Step 3: Change `on_session_reset` signature to accept `preserve_delivered`**

Find the function at line ~195:

```rust
pub fn on_session_reset(&mut self) {
    self.delivered.clear();
    self.in_flight_carrying.fill(0);
    // ...
}
```

Replace the signature + the first body line:

```rust
pub fn on_session_reset(&mut self, preserve_delivered: bool) {
    if !preserve_delivered {
        self.delivered.clear();
    }
    self.in_flight_carrying.fill(0);
    // ...
}
```

(Rest of the function body unchanged.)

- [ ] **Step 4: Update the two existing in-file unit tests**

Find:
- `on_session_reset_preserves_bytes_clears_tracking` (line ~705): the line `t.on_session_reset();` → `t.on_session_reset(false);`.
- `on_session_reset_keeps_empty_slots_empty` (line ~724): same change.

- [ ] **Step 5: Run all three on_session_reset tests**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib on_session_reset 2>&1 | tail -10
```

Expected: 3 passed; 0 failed.

- [ ] **Step 6: Fix the existing caller in io_bridge.rs**

The change at Task 1 broke `io_bridge.rs:1435`. Quick fix to keep the build green between commits — pass `false` (we'll wire the real flag in Task 3):

```bash
grep -n "palette_table.on_session_reset" /home/cedric/work/ghostframe/ghostframe-lib/src/transport/io_bridge.rs
```

Edit the line (around 1435) from:
```rust
self.palette_table.on_session_reset();
```
to:
```rust
self.palette_table.on_session_reset(false);
```

- [ ] **Step 7: Verify the lib still builds**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build -p ghostframe-lib --tests 2>&1 | tail -5
```

Expected: build succeeds.

- [ ] **Step 8: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/src/encoder/pal_rle.rs ghostframe-lib/src/transport/io_bridge.rs
git commit -m "feat(pal_rle): on_session_reset takes preserve_delivered: bool"
```

---

## Task 2: Add the `GHOSTFRAME_SKIP_PALETTE_SESSION_RESET` parser

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs` (new fn near `oob_injector_from_env` around line 252)

- [ ] **Step 1: Write the failing parser test**

In the `mod tests` block of `io_bridge.rs`, near `oob_injector_from_env_parses`, add:

```rust
#[cfg(any(test, feature = "test-loss-injection"))]
#[test]
fn skip_palette_session_reset_from_env_parses() {
    let prev = std::env::var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET").ok();
    std::env::set_var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET", "1");
    let got = IoBridge::skip_palette_session_reset_from_env();
    if let Some(p) = prev {
        std::env::set_var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET", p);
    } else {
        std::env::remove_var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET");
    }
    assert!(got, "env=1 must yield true");

    // Default (unset) is false.
    std::env::remove_var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET");
    assert!(!IoBridge::skip_palette_session_reset_from_env(), "unset must yield false");
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib skip_palette_session_reset_from_env_parses 2>&1 | tail -10
```

Expected: COMPILE ERROR — `skip_palette_session_reset_from_env` is not defined.

- [ ] **Step 3: Implement the parser**

Near `oob_injector_from_env` (around line 252), add:

```rust
#[cfg(any(test, feature = "test-loss-injection"))]
fn skip_palette_session_reset_from_env() -> bool {
    matches!(
        std::env::var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET").as_deref(),
        Ok("1") | Ok("true")
    )
}
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib skip_palette_session_reset_from_env_parses 2>&1 | tail -5
```

Expected: 1 passed.

- [ ] **Step 5: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "feat(test-hook): GHOSTFRAME_SKIP_PALETTE_SESSION_RESET parser (A5/B6)"
```

---

## Task 3: Wire the field + call-site

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs` (struct field, constructors, call site at line 1435)

- [ ] **Step 1: Add the cfg-gated struct field**

Near the existing `outbound_loss` / `inbound_loss` / `oob_inject_at` field declarations (search for `oob_inject_at` to find the right spot, ~line 120):

```rust
    /// Cfg-gated test hook: when true, the new-session handler preserves the
    /// `palette_table.delivered` bitset across the session reset (it still
    /// resets in_flight_carrying / ref_count / etc.). Drives the
    /// e2e_decode_error_thin_uncached round-trip by simulating a client
    /// that lost palette shadow without the server noticing.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub(crate) skip_palette_session_reset: bool,
```

- [ ] **Step 2: Initialize the field in the production constructor**

In `IoBridge::new` (find by searching for `outbound_loss: Self::loss_injector_from_env("OUTBOUND"),` around line 355), add next to the existing test-hook initializations:

```rust
#[cfg(any(test, feature = "test-loss-injection"))]
skip_palette_session_reset: Self::skip_palette_session_reset_from_env(),
```

- [ ] **Step 3: Initialize as `false` in the two test-only constructors**

Find `new_with_stream_for_test` and `new_with_frames_for_test` (search for `outbound_loss: None,` — there are two instances around lines 1698 and following). For each, add next to the existing `oob_inject_at: None,`:

```rust
#[cfg(any(test, feature = "test-loss-injection"))]
skip_palette_session_reset: false,
```

- [ ] **Step 4: Update the call site at line 1435 to pass the flag**

Find:
```rust
self.palette_table.on_session_reset(false);
```

(This was set in Task 1 Step 6.) Replace with:

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

- [ ] **Step 5: Verify the lib builds (test + production feature configs)**

```bash
cd /home/cedric/work/ghostframe
CARGO_INCREMENTAL=0 cargo build -p ghostframe-lib --tests 2>&1 | tail -5
CARGO_INCREMENTAL=0 cargo build -p ghostframe-lib --release --no-default-features 2>&1 | tail -5
```

Expected: both succeed. The release/no-default-features build proves the cfg-gating is correct (production has no `test-loss-injection` feature → `skip_palette_session_reset` field doesn't exist → the `let preserve_delivered = false;` branch compiles).

- [ ] **Step 6: Run all lib tests**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib 2>&1 | tail -10
```

Expected: 279 passed (278 prior + new `skip_palette_session_reset_from_env_parses`); 0 failed. The other on_session_reset tests still pass.

- [ ] **Step 7: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "feat(test-hook): wire skip_palette_session_reset to on_session_reset (A5/B6)"
```

---

## Task 4: Change `solid_per_tile` motion region to 2-color flip

**Files:**
- Modify: `ghostframe-test-pattern/src/solid_per_tile.rs` (the motion-region pixel calculation around line 196)

- [ ] **Step 1: Locate the motion-region pixel calculation**

```bash
grep -nA3 "let phase = " /home/cedric/work/ghostframe/ghostframe-test-pattern/src/solid_per_tile.rs
```

Expected: a block like:
```rust
let phase = (start.elapsed().as_millis() / 33) as u32 & 0xFF;
let motion_color: u32 = (phase << 16) | (phase.wrapping_add(85) << 8) | phase.wrapping_add(170);
// ...
let pixel = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
```

(Plus loop that paints the central 64×64 region with `pixel`.)

- [ ] **Step 2: Replace the 256-color cycle with a 2-color flip**

Find the motion-region paint loop. Replace the per-frame `pixel` derivation with:

```rust
// 2-color flip at 33ms cadence (≈30Hz). Both colours form a stable
// 1-color palette per frame; the same two palette slots get re-used
// every cycle, so the palette_table's `delivered` bit stays set for
// the slot across the test's page-reload. Required by
// e2e_decode_error_thin_uncached (A5/B6) — see
// docs/superpowers/specs/2026-05-17-decode-error-thin-uncached-design.md.
const FLIP_RED: u32 = 0x00FF_0000;
const FLIP_BLUE: u32 = 0x0000_00FF;
let phase = (start.elapsed().as_millis() / 33) as u32;
let pixel = if phase % 2 == 0 { FLIP_RED } else { FLIP_BLUE };
```

(Remove the now-unused `let motion_color` line and the now-unused `r/g/b` derivation if they remain in the function.)

- [ ] **Step 3: Verify the test pattern still builds**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build -p ghostframe-test-pattern 2>&1 | tail -5
```

Expected: build succeeds.

- [ ] **Step 4: Verify e2e_solid_per_tile_pixels is unaffected by the change**

The corner tiles aren't touched, but worth a sanity run since the test does pixel-assertions:

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_solid_per_tile_pixels -- --test-threads=1 2>&1 | tail -5
```

Expected: 1 passed. If FAIL: the motion-region change inadvertently broke the corners (shouldn't — they aren't repainted). Investigate before continuing.

If the test-server container needs rebuilding for the test pattern change to take effect, rebuild it before this step:
```bash
cd /home/cedric/work/ghostframe && docker build -t ghostframe/test-server:latest -f tests/containers/test-server/Dockerfile . 2>&1 | tail -3
```

- [ ] **Step 5: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-test-pattern/src/solid_per_tile.rs
git commit -m "feat(test-pattern): solid_per_tile motion region as 2-color flip (A5/B6)"
```

---

## Task 5: Extract the docker-logs ANSI-strip helper

**Files:**
- Modify: `ghostframe-lib/tests/e2e/helpers.rs` (add helper at the end)
- Modify: `ghostframe-lib/tests/e2e.rs` (replace inline fold patterns in `e2e_indices_raw_handshake` and `e2e_palrle_oob_index` with calls to the helper)

This task is a small refactor that the spec calls for. Two existing tests inline-copy a 13-line ANSI-strip block; the new test (Task 6) would be a third copy. Extract once.

- [ ] **Step 1: Add the helper to helpers.rs**

At the end of `/home/cedric/work/ghostframe/ghostframe-lib/tests/e2e/helpers.rs`, append:

```rust
/// Read `docker logs <container>` (stdout + stderr concatenated) and strip
/// ANSI escape sequences so substring assertions are stable across TTY /
/// non-TTY contexts. Used by tests that assert on tracing-subscriber output.
pub fn read_server_logs_stripped(container_name: &str) -> String {
    let out = std::process::Command::new("docker")
        .args(["logs", container_name])
        .output()
        .expect("running docker logs");
    let raw = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    raw.chars().fold((String::new(), false), |(mut acc, in_esc), c| {
        if in_esc {
            (acc, c != 'm')
        } else if c == '\x1b' {
            (acc, true)
        } else {
            acc.push(c);
            (acc, false)
        }
    }).0
}
```

- [ ] **Step 2: Replace the inline fold in `e2e_indices_raw_handshake`**

```bash
grep -n "fn e2e_indices_raw_handshake" /home/cedric/work/ghostframe/ghostframe-lib/tests/e2e.rs
```

Find the existing block (around line 1846-1874 — the `let out = std::process::Command::new(...)` through `}).0;`). Replace the whole 22-line block (from `let out = ...` to `}).0;`) with:

```rust
let logs = helpers::read_server_logs_stripped("ghostframe-server");
```

- [ ] **Step 3: Replace the inline fold in `e2e_palrle_oob_index`**

Find the same pattern in `e2e_palrle_oob_index` (around line 950-975). Replace the same way:

```rust
let logs = helpers::read_server_logs_stripped("ghostframe-server");
```

- [ ] **Step 4: Verify both tests still pass**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_indices_raw_handshake e2e_palrle_oob_index -- --test-threads=1 2>&1 | tail -10
```

Expected: 2 passed; 0 failed.

- [ ] **Step 5: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/tests/e2e/helpers.rs ghostframe-lib/tests/e2e.rs
git commit -m "refactor(e2e): extract read_server_logs_stripped helper"
```

---

## Task 6: Rewrite `e2e_decode_error_thin_uncached`

**Files:**
- Modify: `ghostframe-lib/tests/e2e.rs` (replace the existing `e2e_decode_error_thin_uncached` body around lines 2105-2135)

- [ ] **Step 1: Locate the existing test**

```bash
grep -nB5 "fn e2e_decode_error_thin_uncached" /home/cedric/work/ghostframe/ghostframe-lib/tests/e2e.rs
```

Confirm the existing `#[ignore = "M3.2c follow-up: test design needs multi-palette / dynamic content for thin emission"]` line is present.

- [ ] **Step 2: Replace the test body**

Replace the existing test (including its docstring comments and `#[ignore]` line) with:

```rust
/// W3 / A5 / B6 — Verify the ERR_THIN_UNCACHED_PALETTE round-trip:
/// the server emits thin against a palette the client doesn't have
/// (after the shadow is cleared by a page reload), the client reports
/// DECODE_ERROR code 3, and the server logs + calls `force_rebundle`.
///
/// The GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1 server-side hook
/// suppresses the `palette_table.on_session_reset()` clearing of the
/// `delivered` bitset on session reconnect. This simulates the
/// production failure mode where the client loses palette atlas state
/// (browser tab discard, WebGPU device loss, etc.) without the server
/// noticing — the server's `delivered` bit lingers and the next thin
/// emission fires against an empty client shadow.
///
/// Test pattern: `--solid-per-tile --drm-direct`. The motion region
/// flips between two 1-colour palettes at 33ms cadence; both palette
/// slots are bundled-and-delivered in session 1, and either is hit
/// again in session 2 → server emits thin → client errors.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_decode_error_thin_uncached() -> Result<()> {
    let setup = setup_e2e_webgpu_gpu_with_env(
        "--solid-per-tile --drm-direct",
        &[("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET", "1")],
    )
    .await?;
    // Phase 1: let session 1 deliver and ACK both flip-palettes for the
    // motion region. 5s comfortably covers QUIC slow-start + several
    // flip cycles + ACK round-trip.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Phase 2: trigger a session reconnect. Browser drops the
    // WebTransport session; renderer.onSessionReset clears the client
    // palette shadow + zeroes the GPU atlas. Server's new-session
    // handler resets dirty/metrics/classifier/scheduler but — because
    // of the env-var — preserves palette_table.delivered.
    setup.page.reload().await?;

    // Phase 3: post-reload, the next motion-region dirty pass causes
    // server to look up the (re-found) palette slot → delivered=true →
    // thin emission → client decodes against empty shadow →
    // ERR_THIN_UNCACHED_PALETTE → DECODE_ERROR → handle_decode_error
    // logs + force_rebundle. 6s covers session-handshake + several
    // thin emissions + FEEDBACK round-trip.
    tokio::time::sleep(Duration::from_secs(6)).await;

    let logs = helpers::read_server_logs_stripped("ghostframe-server");
    assert!(
        logs.contains("client decode error"),
        "expected 'client decode error' tracing line in server logs; got:\n{logs}"
    );
    assert!(
        logs.contains("error_code=3"),
        "expected 'error_code=3' (ERR_THIN_UNCACHED_PALETTE); got:\n{logs}"
    );
    assert!(
        logs.contains("force_rebundle"),
        "expected 'force_rebundle' INFO line in server logs; got:\n{logs}"
    );
    Ok(())
}
```

- [ ] **Step 3: Verify the test compiles**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build --tests -p ghostframe-lib 2>&1 | tail -5
```

Expected: build succeeds.

- [ ] **Step 4: Rebuild the test-server container**

The server-side env-var hook lands in this PR — the container needs a fresh build to include it.

```bash
cd /home/cedric/work/ghostframe && docker build -t ghostframe/test-server:latest -f tests/containers/test-server/Dockerfile . 2>&1 | tail -3
```

Expected: "Successfully tagged ghostframe/test-server:latest".

- [ ] **Step 5: Run the test**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_decode_error_thin_uncached -- --test-threads=1 --nocapture 2>&1 | tee /tmp/decode_thin.log | tail -15
```

Expected: 1 passed.

If FAIL on `expected 'client decode error'`: the post-reload PalRle emission isn't firing (or isn't going thin). Debug:
- `grep -E "client decode|palrle.frame|HELLO received|new session" /tmp/decode_thin.log | head -20` — confirm session 2 connected and palrle.frame stats show non-zero `reused_or_allocated` post-reload
- If `reused_or_allocated=0` post-reload: motion region isn't producing PalRle. Could be classifier seeing the colour change as Bc1 / H264 etc. Add `GHOSTFRAME_DIAGNOSE_TILES=1` to the test env and re-run to see per-tile codec_state.
- If `client decode error` appears but with error_code != 3: prevalidator picked a different error class. Check `prevalidate.ts` for what fires on the actual payload shape.

If FAIL on `expected 'force_rebundle'`: `handle_decode_error` received error_code=3 but didn't fire force_rebundle. Check the `metrics_tracker.get(tile_x, tile_y).codec_state` — if it's not `PalRle`, the force_rebundle branch is skipped. This would point to a different bug; escalate.

- [ ] **Step 6: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/tests/e2e.rs
git commit -m "test(e2e): close e2e_decode_error_thin_uncached via session-reset hook (A5/B6)"
```

---

## Task 7: Update reference memory + final regression

**Files:**
- Modify: `~/.claude/projects/-home-cedric-work-ghostframe/memory/reference_e2e_diagnose.md` (add the new env-var)
- Modify: `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m32c_near_complete.md` (move e2e_decode_error_thin_uncached from "Ignored" to "closed")

- [ ] **Step 1: Add the env-var to the reference memory**

Open `~/.claude/projects/-home-cedric-work-ghostframe/memory/reference_e2e_diagnose.md`. After the existing `GHOSTFRAME_INJECT_OOB_PALRLE` entry, insert:

```markdown
- `GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1` — simulates the production failure mode where a client loses palette atlas state (tab discard / WebGPU device loss) without the server noticing. When set, the server's new-session handler passes `preserve_delivered=true` to `palette_table.on_session_reset`, leaving the `delivered` bitset intact while other per-session state (`in_flight_carrying`, `ref_count`, `free_lru`, `slot_state`) still resets. Combined with `page.reload()`, this drives the natural ERR_THIN_UNCACHED_PALETTE round-trip end-to-end. Introduced M3.2c A5/B6 closure (see commits and `docs/superpowers/specs/2026-05-17-decode-error-thin-uncached-design.md`).
```

- [ ] **Step 2: Update project memory**

Open `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m32c_near_complete.md`. In the "Suite status post-session" table, REMOVE the `e2e_decode_error_thin_uncached` row from the ignored-tests table. Bump the suite count to `21 passed, 3 ignored` (assuming both edge-tiles and decode-error closures land; if only one lands, bump appropriately).

In the "Remaining for full M3.2c closure" section, REMOVE the bullet covering "B2 + same blocker as A5/B6" (since B6/A5 is now closed; B2 may still be deferred — note it separately).

- [ ] **Step 3: Run the full e2e suite for regression**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e -- --test-threads=1 2>&1 | tee /tmp/decode_final.log | tail -10
```

Expected: 21 passed (20 prior + e2e_decode_error_thin_uncached); 3 ignored (palrle_session_reset, edge_tiles unless that plan ran first too, resolution_change); 0 failed.

The flaky `e2e_palette_eviction` may fail intermittently in batch (passes in isolation) — pre-existing, not a regression from this work.

- [ ] **Step 4: Run lib unit tests**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib 2>&1 | tail -5
```

Expected: 280 tests pass (278 prior + `on_session_reset_preserve_delivered_keeps_bit` + `skip_palette_session_reset_from_env_parses`).

- [ ] **Step 5: Final commit**

```bash
cd /home/cedric/work/ghostframe
git status --short
```

The memory dir is NOT under git. The plan's project-tree changes were all committed in Tasks 1-6. If `git status` shows clean (modulo the pre-existing `.claude/`, `librust_out.rlib`, `tmux-client-25660.log` untracked entries), the milestone is closed. Otherwise commit any straggler changes with a `chore(a5,b6): cleanup` message.

---

## Notes on parallelization

Tasks 1, 2, 3 are sequential (Task 2 builds on the parser slot opened in Task 1; Task 3 depends on the parser and the signature change). Task 4 is independent of Tasks 1-3 and could run in parallel — but is small enough not to warrant the coordination overhead. Tasks 5, 6 depend on Tasks 3+4. Task 7 runs last.

Best linear order: 1 → 2 → 3 → 4 → 5 → 6 → 7. ~3 hours total assuming the container rebuild fits in the docker disk budget.
