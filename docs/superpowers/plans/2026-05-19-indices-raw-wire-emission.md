# B2 — `indices_raw` Wire Emission Assertion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend `e2e_indices_raw_handshake` with a wire-level assertion that at least one PalRle payload received by the client has the `indices_raw` flag bit (0x02) set, closing the M3.2c B2 deferral.

**Architecture:** Add a `window.__ghostframeRecordedFlags: number[]` recorder to the client (parallel to existing `__ghostframeRecordedCodecs`), populated with `asm.payload[0]` for every PalRle tile. Switch the e2e from `--solid-red` (which never emits PalRle) to `--solid-per-tile --drm-direct` (which produces continuous PalRle for the 2-color flip motion region). Add a third assertion that reads back the recorder array and verifies `iter().any(|&f| f & 0x02 != 0)`.

**Tech Stack:** TypeScript (web client), Rust (e2e test).

**Spec:** `docs/superpowers/specs/2026-05-19-indices-raw-wire-emission-design.md`

---

## File Structure

- `ghostframe-web-client/src/main.ts` — add `__ghostframeRecordedFlags` push next to the existing `__ghostframeRecordedCodecs` push (~6 lines added in the existing instrumentation block).
- `ghostframe-lib/tests/e2e.rs` — rewrite the body of `e2e_indices_raw_handshake`: switch setup helper, keep HELLO + caps assertions, add the wire-emission assertion (~25 lines changed). Update docstring.

---

## Task 1: Add `__ghostframeRecordedFlags` client-side recorder

**Files:**
- Modify: `ghostframe-web-client/src/main.ts` (existing instrumentation block around line 388-401)

- [ ] **Step 1: Read the current instrumentation block**

```bash
sed -n '385,402p' /home/cedric/work/ghostframe/ghostframe-web-client/src/main.ts
```

Confirm the existing block looks like (paraphrased; the exact field-name list may have grown — `__ghostframeRecordedTiles` was added in commit `8bf4ab0`):

```typescript
if (typeof window !== "undefined") {
  const w = window as unknown as {
    __ghostframeRecordedCodecs?: number[];
    __ghostframeRecordedTiles?: Array<...>;
  };
  if (!w.__ghostframeRecordedCodecs) {
    w.__ghostframeRecordedCodecs = [];
  }
  if (!w.__ghostframeRecordedTiles) {
    w.__ghostframeRecordedTiles = [];
  }
  w.__ghostframeRecordedCodecs.push(asm.header.codec);
  w.__ghostframeRecordedTiles.push({ ...snip... });
}
```

- [ ] **Step 2: Extend the type cast + initializer + push**

In the same `if (typeof window !== "undefined")` block:

1. Add `__ghostframeRecordedFlags?: number[];` to the cast type literal.
2. Add the `if (!w.__ghostframeRecordedFlags) { w.__ghostframeRecordedFlags = []; }` initializer.
3. Add a guarded push: after the existing `w.__ghostframeRecordedCodecs.push(asm.header.codec);` line, add:

```typescript
if (asm.header.codec === Codec.PalRle) {
  w.__ghostframeRecordedFlags.push(asm.payload[0]);
}
```

The `Codec.PalRle` enum is already imported in this file (verified by the existing `else if (asm.header.codec === Codec.PalRle)` branch at line 414).

After your edit the block should look (only showing the changed lines for clarity):

```typescript
if (typeof window !== "undefined") {
  const w = window as unknown as {
    __ghostframeRecordedCodecs?: number[];
    __ghostframeRecordedTiles?: Array<...>;
    __ghostframeRecordedFlags?: number[];
  };
  if (!w.__ghostframeRecordedCodecs) { w.__ghostframeRecordedCodecs = []; }
  if (!w.__ghostframeRecordedTiles) { w.__ghostframeRecordedTiles = []; }
  if (!w.__ghostframeRecordedFlags) { w.__ghostframeRecordedFlags = []; }
  w.__ghostframeRecordedCodecs.push(asm.header.codec);
  if (asm.header.codec === Codec.PalRle) {
    w.__ghostframeRecordedFlags.push(asm.payload[0]);
  }
  w.__ghostframeRecordedTiles.push({ ...snip... });
}
```

The push order doesn't matter; placing the new push right after the codecs push keeps related operations together.

- [ ] **Step 3: Verify the web-client builds + vitest still passes**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client && npm run build 2>&1 | tail -5
cd /home/cedric/work/ghostframe/ghostframe-web-client && npm test 2>&1 | tail -5
```

Expected: both succeed. The `npm run build` emits `dist/assets/index-*.js`. The 27 vitest tests pass (the recorder is JS-only, no behavior change to production codec paths).

- [ ] **Step 4: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-web-client/src/main.ts
git commit -m "$(cat <<'EOF'
diag(client): __ghostframeRecordedFlags recorder for B2 wire assertion

Push asm.payload[0] (the flags byte) to window.__ghostframeRecordedFlags
for every PalRle tile decoded. Parallel to the existing
__ghostframeRecordedCodecs recorder. Task 2 wires the test-side
assertion.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Rewrite `e2e_indices_raw_handshake` body + assert wire emission

**Files:**
- Modify: `ghostframe-lib/tests/e2e.rs` (existing `e2e_indices_raw_handshake` at lines 2017-2051)

- [ ] **Step 1: Locate the current test**

```bash
grep -nB18 "fn e2e_indices_raw_handshake" /home/cedric/work/ghostframe/ghostframe-lib/tests/e2e.rs | head -25
```

Confirm:
- Doc comment block at lines 2017-2030 (mentions "deferred to M3.2c" — that's the gate we're closing).
- `#[tokio::test(flavor = "multi_thread")]` at line 2031.
- Body uses `setup_e2e_webgpu("--solid-red")` at line 2033.
- Body asserts on `logs.contains("HELLO received")` and `logs.contains("indices_raw=true")` only.

- [ ] **Step 2: Replace the entire test (docstring + body)**

Replace the whole function — docstring through the closing `}` of the body — with:

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
///      `window.__ghostframeRecordedFlags` from the client.
///
/// Closes the M3.2c B2 deferral (originally blocked on a PalRle-emitting
/// test pattern + the latent on_session_reset / LRU bugs, all fixed by
/// 2026-05-18). Uses `--solid-per-tile --drm-direct`: motion region's
/// 2-color flip emits bundled palette in the first frame for each color,
/// the client ACKs, server marks delivered=true, and subsequent dirty
/// frames for the same palette emit thin+indices_raw (flags=0x02).
#[tokio::test(flavor = "multi_thread")]
async fn e2e_indices_raw_handshake() -> Result<()> {
    let setup = setup_e2e_webgpu_gpu("--solid-per-tile --drm-direct").await?;

    // Allow time for: page load → WebGPU init → WebTransport.ready →
    // HELLO write → server parse → first PalRle bundled emission →
    // client ACK → server delivered=true → subsequent dirty pass emits
    // thin + indices_raw. 5s comfortably covers QUIC slow-start + the
    // initial H264-startup phase + several 2-color flip cycles.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Assertions 1 + 2: HELLO arrived and caps were applied.
    let logs = helpers::read_server_logs_stripped("ghostframe-server");
    assert!(
        logs.contains("HELLO received"),
        "expected 'HELLO received' tracing line in server logs; got:\n{logs}"
    );
    assert!(
        logs.contains("indices_raw=true"),
        "expected 'indices_raw=true' in server logs (caps payload); got:\n{logs}"
    );

    // Assertion 3 (B2): at least one PalRle wire payload had the
    // indices_raw flag (bit 1) set.
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

Three substantive changes from the prior version:
1. Setup helper: `setup_e2e_webgpu("--solid-red")` → `setup_e2e_webgpu_gpu("--solid-per-tile --drm-direct")`. The new variant takes the test through the DRM-passthrough container with VKMS.
2. Local `let _setup = ...` (discarded) becomes `let setup = ...` (kept, used for the page.evaluate call in the new assertion).
3. Third assertion block appended.

- [ ] **Step 3: Verify the test compiles**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build --tests -p ghostframe-lib 2>&1 | tail -5
```

Expected: build succeeds. Pre-existing warnings (`setup_e2e`, `setup_e2e_with_env`, etc. unused) stay; no new errors.

- [ ] **Step 4: Run the test in isolation**

The web-client `dist/` was rebuilt at the end of Task 1, so the new recorder is already serving. No docker rebuild needed (the change is client-side only — the server-side encoder and `palrle.wire indices_raw emitted` log behavior are unchanged).

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_indices_raw_handshake -- --test-threads=1 --nocapture 2>&1 | tee /tmp/b2_validate.log | tail -10
```

Expected: 1 passed; 0 failed.

If the third assertion fails:
- Run `grep "palrle.wire" /tmp/b2_validate.log | head -10` — confirm the SERVER actually emitted indices_raw (`"indices_raw emitted"` log lines). If absent, the encoder never picked the thin path → recheck Bug B fix is in the running binary (verify via `docker run --rm --entrypoint=/bin/sh ghostframe/test-server:latest -c "md5sum /usr/local/bin/ghostframe-xdaemon"` and compare against the freshly-built binary; rebuild docker if stale).
- Run `grep "got: \[" /tmp/b2_validate.log | head -2` — look at the flags array contents. If full of `0x01` (bundled only) and no `0x02`, the server is still bundling (delivered hasn't transitioned to true for any palette within 5s). Bump the wait to 7s and retry.
- If the array is empty (`got: []`), no PalRle reached the client — recheck that the test-server container has DRM mounted (the test uses `_webgpu_gpu` which sets `gpu: true`).

- [ ] **Step 5: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/tests/e2e.rs
git commit -m "$(cat <<'EOF'
test(e2e): assert indices_raw wire emission (B2 closure)

Extends e2e_indices_raw_handshake with a third assertion that reads
back window.__ghostframeRecordedFlags from the client and verifies
at least one PalRle tile had the indices_raw flag bit (0x02) set.
Switches test pattern from --solid-red (no PalRle) to
--solid-per-tile --drm-direct (continuous PalRle via 2-color flip).

The first two assertions (HELLO arrived + caps updated) are unchanged.

Closes M3.2c B2 — last remaining un-validated item from the original
M3.2c milestone now that A5/B6 + W5 + W3/B3/B7 + W4 + W1+B1 are all
landed.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Suite regression + memory update

**Files:**
- Modify: `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m32c_near_complete.md`
- Modify: `~/.claude/projects/-home-cedric-work-ghostframe/memory/MEMORY.md`

- [ ] **Step 1: Run the full e2e suite for regression**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e -- --test-threads=1 2>&1 | tee /tmp/b2_full.log | tail -5
```

Expected: **23 passed, 0 failed, 2 ignored**. Same count as the post-A5/B6-closure baseline — `e2e_indices_raw_handshake` was already counted in the 23; this task gains it a stronger assertion without changing the count.

If any regression, identify and fix before moving on. Likely candidates:
- `e2e_solid_per_tile_pixels` — uses the same test pattern; if a recorder push interacts badly with timing, it might affect this test. Unlikely (the recorder push is a no-op append; doesn't touch rendering).
- `e2e_palrle_5pct_loss` — uses the same pattern. If anything, the new recorder helps debug (it'll have flags entries for the 5% loss scenario).

- [ ] **Step 2: Run lib unit tests**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib 2>&1 | tail -5
```

Expected: 282 passed (no change — this task adds no lib unit tests). 0 failed.

- [ ] **Step 3: Run vitest**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client && npm test 2>&1 | tail -5
```

Expected: 27 passed (no change).

- [ ] **Step 4: Update project memory — `project_m32c_near_complete.md`**

In `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m32c_near_complete.md`, find the "Remaining for full M3.2c closure" section. Remove the B2 bullet (it's now closed). Update the section's intro paragraph to reflect that only `palrle_exact` remains as an optional follow-up.

Concretely, find:
```
## Remaining for full M3.2c closure

- **B2** (`e2e_indices_raw_handshake` wire-level assertion via `__ghostframeRecordedFlags`): With Bug A fixed (`on_session_reset` now actually fires on reconnect), this is now tractable via the same `--solid-per-tile + GHOSTFRAME_SKIP_PALETTE_SESSION_RESET + page.reload` pattern used by `e2e_decode_error_thin_uncached`. Small follow-up.
- **palrle_exact** (Tasks 10-11 in original plan): exact-pixel test of an alternating-nibble RLE pattern, would catch nibble-swap bugs in the PalRle compute shader. Existing PalRle tests (e2e_palrle_5pct_loss, e2e_text_clarity) pass with text content which exercises nibble RLE in practice, so we have implicit coverage. Optional 1-day follow-up.
```

Replace with:
```
## Remaining for full M3.2c closure

- **palrle_exact** (Tasks 10-11 in original plan): exact-pixel test of an alternating-nibble RLE pattern, would catch nibble-swap bugs in the PalRle compute shader. Existing PalRle tests (e2e_palrle_5pct_loss, e2e_text_clarity) pass with text content which exercises nibble RLE in practice, so we have implicit coverage. Optional 1-day follow-up — not a blocker.

**B2 closed 2026-05-19** via commits `<task 1 sha>` (client recorder) and `<task 2 sha>` (test-side assertion). M3.2c's required wire-emission assertions are now complete for all wire variants (Raw, Solid, PalRle bundled, PalRle thin nibble-RLE, PalRle thin indices_raw, H.264).
```

Fill in the actual commit SHAs after Task 1 + Task 2 land (look them up via `git log --oneline -5`).

Also bump the suite-status line. Change:
```
`cargo test -p ghostframe-lib --test e2e -- --test-threads=1`: **23 passed, 0 failed, 2 ignored** (was 22/0/3 at start of 2026-05-18 latent-bug closure session).
```

To (the count is unchanged but the assertion strength grew):
```
`cargo test -p ghostframe-lib --test e2e -- --test-threads=1`: **23 passed, 0 failed, 2 ignored**. (Count unchanged from 2026-05-18; `e2e_indices_raw_handshake` gained a wire-level indices_raw assertion on 2026-05-19 — see B2 closure note below.)
```

- [ ] **Step 5: Update MEMORY.md index line**

In `~/.claude/projects/-home-cedric-work-ghostframe/memory/MEMORY.md`, update the project_m32c_near_complete line. Currently:

```
- [M3.2c near-complete (W3/B3+B7/W4/W5/A5+B6 closed)](project_m32c_near_complete.md) — Mon 2026-05-18: B3 + B7 + W4 + W5 + A5/B6 all landed (the latter via fixing 2 latent prod bugs: on_session_reset gate + LRU thrashing; companion fix: Xvfb stale-socket cleanup precondition); 23 e2e pass, 2 ignored
```

Change to:

```
- [M3.2c complete (all spec items closed)](project_m32c_near_complete.md) — 2026-05-18/19: B3 + B7 + W4 + W5 + A5/B6 + B2 all landed (the latent prod bugs (on_session_reset gate + LRU thrashing) closed Mon; B2 wire-level indices_raw assertion landed Tue); 23 e2e pass, 2 ignored (palrle_session_reset M3.5 scope, resolution_change pre-existing)
```

- [ ] **Step 6: Final commit if anything project-side remains uncommitted**

```bash
cd /home/cedric/work/ghostframe
git status --short
```

Memory files (`~/.claude/projects/-home-cedric-work-ghostframe/memory/...`) are NOT under git — their updates are persisted by the file write alone, no commit needed.

If the working tree is clean (modulo the pre-existing `.claude/`, `librust_out.rlib`, `tmux-client-25660.log` untracked entries), the milestone is closed. Done.

---

## Notes on parallelization

Task 1 must complete before Task 2 (Task 2's assertion depends on the recorder Task 1 creates). Task 3 runs last. No parallelism opportunities; 3 sequential dispatches in subagent-driven execution.

## Contingency

The only failure mode the plan anticipates is Task 2 Step 4's third assertion missing indices_raw payloads in the 5s window. Inline diagnostics + a 7s bump are in the step description. If even 7s isn't enough, fall through to a follow-up: re-add the recorder dump (`eprintln!("flags: {:?}", flags)` before the assertion) to characterize the actual emissions, then design a targeted fix (might be a server-side timing issue, might be the recorder needs to capture the bundled→thin transition over a longer window).
