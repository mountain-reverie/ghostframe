# Edge-Tiles Diagnose-Then-Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Identify and fix the root cause of partial edge tiles rendering as transparent black on the WebGPU client (e.g. cols/rows of a 700×500 capture that aren't tile-aligned), and un-ignore `e2e_edge_tiles`.

**Architecture:** Two phases. Phase 1 (Tasks 1-4) adds window-attached tile + resize recorders to main.ts, dumps them from the e2e test, manually classifies the failure into one of five buckets (R1-R5 in the spec). Phase 2 (Tasks 5-7) applies the targeted fix code corresponding to whichever bucket matched, verifies, and drops the `#[ignore]`.

**Tech Stack:** Rust (lib + e2e tests), TypeScript (web client), WebGPU compute + render pipelines, chromiumoxide for e2e.

**Spec:** `docs/superpowers/specs/2026-05-17-edge-tiles-diagnose-fix-design.md`

---

## File Structure

**Phase 1** (specified upfront):
- `ghostframe-web-client/src/main.ts` — add tile + resize recorders next to the existing `__ghostframeRecordedCodecs` block. ~25 lines added.
- `ghostframe-lib/tests/e2e.rs` — extend `e2e_edge_tiles`'s diagnostic dump. ~15 lines added.

**Phase 2** (one of the following, picked by Phase 1's classification):
- **R1 path** — `framebuffer.ts` (optional: cropping API for over-allocation) + `main.ts` (round fallback-expand and/or sentinel-resize up to tile-multiples). ~10-30 lines.
- **R3 path** — `renderer.ts::writeRawTile` clips extent to fb bounds. ~15 lines replaced.
- **R2 / R4 / R5 path** — spec addendum; plan diverges; new task list.

The fix in Phase 2 doesn't add new files. It modifies existing renderer / framebuffer / main.ts.

---

## Phase 1 — Diagnose

### Task 1: Add per-tile event recorder

**Files:**
- Modify: `ghostframe-web-client/src/main.ts` (the test-instrumentation block around lines 340-347)

- [ ] **Step 1: Read the current instrumentation block**

```bash
sed -n '335,360p' /home/cedric/work/ghostframe/ghostframe-web-client/src/main.ts
```

Confirm the existing block looks like:
```typescript
// Test instrumentation: record codecs for E2E protocol-layer assertions.
if (typeof window !== "undefined") {
  const w = window as unknown as { __ghostframeRecordedCodecs?: number[] };
  if (!w.__ghostframeRecordedCodecs) {
    w.__ghostframeRecordedCodecs = [];
  }
  w.__ghostframeRecordedCodecs.push(asm.header.codec);
}
```

- [ ] **Step 2: Extend the block to also record per-tile event**

Replace the block above with:

```typescript
// Test instrumentation: record codecs for E2E protocol-layer assertions.
// Per-tile event log adds {tileX, tileY, codec, payloadLen, fbWidth, fbHeight}
// so e2e_edge_tiles can correlate which tiles arrived against the framebuffer
// dimensions at the moment they were queued.
if (typeof window !== "undefined") {
  const w = window as unknown as {
    __ghostframeRecordedCodecs?: number[];
    __ghostframeRecordedTiles?: Array<{
      seq: number;
      tileX: number;
      tileY: number;
      codec: number;
      payloadLen: number;
      fbWidth: number;
      fbHeight: number;
    }>;
  };
  if (!w.__ghostframeRecordedCodecs) {
    w.__ghostframeRecordedCodecs = [];
  }
  if (!w.__ghostframeRecordedTiles) {
    w.__ghostframeRecordedTiles = [];
  }
  w.__ghostframeRecordedCodecs.push(asm.header.codec);
  w.__ghostframeRecordedTiles.push({
    seq: asm.header.frameSeq,
    tileX: tX,
    tileY: tY,
    codec: asm.header.codec,
    payloadLen: payload.byteLength,
    fbWidth: renderer.framebuffer.width,
    fbHeight: renderer.framebuffer.height,
  });
}
```

- [ ] **Step 3: Verify the web-client builds**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client && npm run build 2>&1 | tail -10
```

Expected: build succeeds. If TypeScript complains about `asm.header.frameSeq` not existing, replace with the actual field name (search `asm.header` references in main.ts for the canonical naming).

- [ ] **Step 4: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-web-client/src/main.ts
git commit -m "diag(client): __ghostframeRecordedTiles per-tile event recorder (W5)"
```

---

### Task 2: Add resize event recorder

**Files:**
- Modify: `ghostframe-web-client/src/main.ts` (the two `renderer.resize(...)` call sites — sentinel branch around line 321 and fallback-expand branch around line 333)

- [ ] **Step 1: Read both call sites**

```bash
sed -n '315,340p' /home/cedric/work/ghostframe/ghostframe-web-client/src/main.ts
```

Identify the two call sites:
- `renderer.resize(w, h);` inside `if (tX === FRAME_DIMENSIONS_SENTINEL_X && tY === FRAME_DIMENSIONS_SENTINEL_Y)` (sentinel)
- `renderer.resize(Math.max(canvasEl.width, minWidth), Math.max(canvasEl.height, minHeight));` (fallback-expand)

- [ ] **Step 2: Add a helper that wraps resize + records**

Near the top of `finishAssembly` (or just above where the recorders go), define a helper that closes over `renderer`:

```typescript
function recordingResize(
  newW: number,
  newH: number,
  trigger: 'sentinel' | 'fallback-expand',
  seq: number,
) {
  const oldW = renderer.framebuffer.width;
  const oldH = renderer.framebuffer.height;
  renderer.resize(newW, newH);
  if (typeof window !== "undefined") {
    const w = window as unknown as {
      __ghostframeRecordedResizes?: Array<{
        seq: number; oldW: number; oldH: number;
        newW: number; newH: number;
        trigger: 'sentinel' | 'fallback-expand';
      }>;
    };
    if (!w.__ghostframeRecordedResizes) {
      w.__ghostframeRecordedResizes = [];
    }
    w.__ghostframeRecordedResizes.push({ seq, oldW, oldH, newW, newH, trigger });
  }
}
```

The helper lives inside `finishAssembly` to keep the recorder window-attachment scoped to test instrumentation; the captured `renderer` is the outer-scope renderer instance.

- [ ] **Step 3: Replace the sentinel-branch resize call**

Find:
```typescript
const w = view.getUint32(0, false);
const h = view.getUint32(4, false);
renderer.resize(w, h);
frameDimensionsKnown = true;
```

Replace `renderer.resize(w, h);` with:
```typescript
recordingResize(w, h, 'sentinel', asm.header.frameSeq);
```

- [ ] **Step 4: Replace the fallback-expand resize call**

Find:
```typescript
if (canvasEl.width < minWidth || canvasEl.height < minHeight) {
  renderer.resize(
    Math.max(canvasEl.width, minWidth),
    Math.max(canvasEl.height, minHeight)
  );
}
```

Replace with:
```typescript
if (canvasEl.width < minWidth || canvasEl.height < minHeight) {
  recordingResize(
    Math.max(canvasEl.width, minWidth),
    Math.max(canvasEl.height, minHeight),
    'fallback-expand',
    asm.header.frameSeq,
  );
}
```

- [ ] **Step 5: Verify the web-client builds**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client && npm run build 2>&1 | tail -5
```

Expected: build succeeds.

- [ ] **Step 6: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-web-client/src/main.ts
git commit -m "diag(client): __ghostframeRecordedResizes resize-event recorder (W5)"
```

---

### Task 3: Extend e2e_edge_tiles diagnostic dump

**Files:**
- Modify: `ghostframe-lib/tests/e2e.rs` (the existing `e2e_edge_tiles` test around lines 1196-1220)

- [ ] **Step 1: Locate the existing diagnostic dump**

```bash
grep -nB2 -A20 "e2e_edge_tiles diagnostic" /home/cedric/work/ghostframe/ghostframe-lib/tests/e2e.rs
```

Confirm the existing block ends with `eprintln!("e2e_edge_tiles diagnostic: ...")` after `let diag: serde_json::Value = setup.page.evaluate(diag_js).await?.into_value()?;`.

- [ ] **Step 2: Add tile + resize dumps after the existing dump**

Immediately after the existing `eprintln!("e2e_edge_tiles diagnostic: ...")` line, add:

```rust
let tiles: serde_json::Value = setup
    .page
    .evaluate("window.__ghostframeRecordedTiles || []")
    .await?
    .into_value()?;
let resizes: serde_json::Value = setup
    .page
    .evaluate("window.__ghostframeRecordedResizes || []")
    .await?
    .into_value()?;
eprintln!(
    "e2e_edge_tiles tiles: {}",
    serde_json::to_string_pretty(&tiles).unwrap()
);
eprintln!(
    "e2e_edge_tiles resizes: {}",
    serde_json::to_string_pretty(&resizes).unwrap()
);
```

- [ ] **Step 3: Verify the test compiles**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build --tests -p ghostframe-lib 2>&1 | tail -5
```

Expected: build succeeds.

- [ ] **Step 4: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/tests/e2e.rs
git commit -m "diag(e2e): dump per-tile + resize event logs in e2e_edge_tiles (W5)"
```

---

### Task 4: Run diagnostic + classify [GATE]

**Files:** none (operational task)

- [ ] **Step 1: Rebuild the test-server container**

The client TypeScript change must land in the bundled web client assets that the test serves. Check whether the web client is rebuilt as part of the e2e setup or whether it ships pre-built in the container.

```bash
ls /home/cedric/work/ghostframe/ghostframe-web-client/dist/ 2>&1 | head -5
```

If `dist/` is present and the e2e uses it directly (search `serve_dist` / `start_static_server` in `tests/e2e/helpers.rs`), rebuild dist:
```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client && npm run build
```

If the docker image bundles the client, also rebuild the image:
```bash
cd /home/cedric/work/ghostframe && docker build -t ghostframe/test-server:latest -f tests/containers/test-server/Dockerfile . 2>&1 | tail -3
```

- [ ] **Step 2: Run e2e_edge_tiles with --ignored**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_edge_tiles -- --test-threads=1 --ignored --nocapture 2>&1 | tee /tmp/edge_diag.log | tail -30
```

Expected: test FAILS at the pixel-readback assertion (we haven't fixed anything yet), AND prints three blocks to stderr:
- `e2e_edge_tiles diagnostic: { ... }` (the 8-pixel readback matrix)
- `e2e_edge_tiles tiles: [ ... ]` (per-tile event log)
- `e2e_edge_tiles resizes: [ ... ]` (resize event log)

- [ ] **Step 3: Classify into a bucket**

Open `/tmp/edge_diag.log` and look at the dumps. Classify into one of the spec's five buckets:

| Bucket | Signature in the data |
|---|---|
| **R1** | `resizes` shows the expand-then-shrink pattern: `oldW > newW` somewhere with `trigger='sentinel'`. Edge-tile entries in `tiles` have `fbWidth > newW`. |
| **R2** | No entries for `tileX === 21` or `tileY === 15` in `tiles`. (Cols/rows for 700×500 → max tileX=21, max tileY=15.) |
| **R3** | Edge tiles for col 21 / row 15 have `codec === 5` (Raw); `payloadLen === 4096`; `fbWidth === 700`. |
| **R4** | Edge tiles for col 21 / row 15 have `codec === 2` (PalRle); `payloadLen` reasonable; `fbWidth === 700`. But in-bounds pixels still don't paint → unexpected. |
| **R5** | Edge tiles for col 21 / row 15 have `codec === 4` (Solid). |

Document the chosen bucket in the spec's "Phase 1 findings" section.

- [ ] **Step 4: Append findings to the spec**

```bash
cat >> /home/cedric/work/ghostframe/docs/superpowers/specs/2026-05-17-edge-tiles-diagnose-fix-design.md <<'EOF'

## Phase 1 findings (2026-05-17)

**Bucket matched:** [R1 / R2 / R3 / R4 / R5]

**Evidence (representative snippet from `/tmp/edge_diag.log`):**

```
[paste the relevant tile + resize entries here]
```

**Chosen Phase 2 path:** [R1 template / R3 template / new design]
EOF
```

Fill in the bucket, the evidence snippet, and the chosen path.

- [ ] **Step 5: Commit findings**

```bash
cd /home/cedric/work/ghostframe
git add docs/superpowers/specs/2026-05-17-edge-tiles-diagnose-fix-design.md
git commit -m "diag(w5): e2e_edge_tiles Phase 1 findings — bucket R<N>"
```

- [ ] **Step 6: Proceed to Phase 2**

Per the matched bucket:
- **R1**: proceed to Task 5R1
- **R3**: proceed to Task 5R3
- **R2 / R4 / R5**: STOP. The plan does not have pre-written templates for these. Brainstorm a fix using the brainstorming skill, write a new spec addendum, then add Phase 2 tasks for the chosen approach. The diagnostic data is already in `/tmp/edge_diag.log` and the spec.

---

## Phase 2 — Fix

> Run **only one** of Tasks 5R1 / 5R3 (or, if R2/R4/R5 fired, follow the escalation from Task 4 Step 6). Tasks 6 and 7 run regardless.

### Task 5R1: Resize-shrink content loss

**Files:**
- Modify: `ghostframe-web-client/src/main.ts` (sentinel-branch + fallback-expand-branch resize calls)

- [ ] **Step 1: Round the fallback-expand to a 32-multiple**

Find the fallback-expand block (now wrapped via `recordingResize` from Task 2):

```typescript
if (canvasEl.width < minWidth || canvasEl.height < minHeight) {
  recordingResize(
    Math.max(canvasEl.width, minWidth),
    Math.max(canvasEl.height, minHeight),
    'fallback-expand',
    asm.header.frameSeq,
  );
}
```

Replace with:

```typescript
if (canvasEl.width < minWidth || canvasEl.height < minHeight) {
  // Round up to a 32-multiple so a subsequent sentinel-resize doesn't
  // truncate partial edge tiles we already painted. The display blit
  // is responsible for cropping back to the actual frame dimensions.
  const grownW = Math.max(canvasEl.width, minWidth);
  const grownH = Math.max(canvasEl.height, minHeight);
  recordingResize(
    Math.ceil(grownW / TILE_SIZE) * TILE_SIZE,
    Math.ceil(grownH / TILE_SIZE) * TILE_SIZE,
    'fallback-expand',
    asm.header.frameSeq,
  );
}
```

- [ ] **Step 2: Round the sentinel resize to a 32-multiple**

Find the sentinel-branch resize:

```typescript
const w = view.getUint32(0, false);
const h = view.getUint32(4, false);
recordingResize(w, h, 'sentinel', asm.header.frameSeq);
frameDimensionsKnown = true;
```

Replace with:

```typescript
const w = view.getUint32(0, false);
const h = view.getUint32(4, false);
// Allocate to the tile-aligned ceiling so partial-tile writes from
// earlier in the burst aren't truncated by an exact-size shrink.
const fbW = Math.ceil(w / TILE_SIZE) * TILE_SIZE;
const fbH = Math.ceil(h / TILE_SIZE) * TILE_SIZE;
recordingResize(fbW, fbH, 'sentinel', asm.header.frameSeq);
frameDimensionsKnown = true;
```

- [ ] **Step 3: Verify build**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client && npm run build 2>&1 | tail -5
```

Expected: build succeeds.

- [ ] **Step 4: Rebuild test-server container (if needed)**

Same as Task 4 Step 1 — either `npm run build` is enough or the docker image also needs rebuilding.

- [ ] **Step 5: Run e2e_edge_tiles unignored**

Drop the `#[ignore]` line first (do this in Task 6), but for now do a sanity run with `--ignored`:

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_edge_tiles -- --test-threads=1 --ignored --nocapture 2>&1 | tee /tmp/edge_r1.log | tail -15
```

Expected: PASS (the diagnostic dumps still print, but all 4 pixel assertions now succeed).

If still failing: the R1 fix wasn't sufficient. The canvas is now over-allocated to (704, 512), which means the framebuffer texture has the right size. Check the new diagnostic dump for the resize history — is the over-allocation actually happening? If yes but pixels still wrong, that's an additional bug; escalate by writing a new spec addendum.

- [ ] **Step 6: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-web-client/src/main.ts
git commit -m "fix(web-client): round resize to tile-multiples to preserve partial edge tiles (W5 R1)"
```

### Task 5R3: Raw writeTexture out-of-bounds

**Files:**
- Modify: `ghostframe-web-client/src/webgpu/renderer.ts` (`writeRawTile` around lines 84-101)

- [ ] **Step 1: Replace writeRawTile with bounds-clipping variant**

Find:

```typescript
writeRawTile(tileX: number, tileY: number, bgra: Uint8Array): void {
  if (bgra.length !== 32 * 32 * 4) {
    throw new Error(`writeRawTile: payload length ${bgra.length} != 4096`);
  }
  const rgba = new Uint8Array(bgra.length);
  for (let i = 0; i < bgra.length; i += 4) {
    rgba[i + 0] = bgra[i + 2]; // R from B-slot
    rgba[i + 1] = bgra[i + 1]; // G
    rgba[i + 2] = bgra[i + 0]; // B from R-slot
    rgba[i + 3] = bgra[i + 3] === 0 ? 255 : bgra[i + 3]; // force alpha (BGRX quirk)
  }
  this.device.queue.writeTexture(
    { texture: this.framebuffer.texture, origin: { x: tileX * 32, y: tileY * 32 } },
    rgba,
    { bytesPerRow: 32 * 4, rowsPerImage: 32 },
    { width: 32, height: 32 },
  );
}
```

Replace with:

```typescript
writeRawTile(tileX: number, tileY: number, bgra: Uint8Array): void {
  if (bgra.length !== 32 * 32 * 4) {
    throw new Error(`writeRawTile: payload length ${bgra.length} != 4096`);
  }
  // Clip the destination extent to the framebuffer bounds — partial edge
  // tiles at non-32-aligned resolutions would otherwise trip a WebGPU
  // validation error ("origin+size > texture size") and silently drop.
  const fbW = this.framebuffer.width;
  const fbH = this.framebuffer.height;
  const dstX = tileX * 32;
  const dstY = tileY * 32;
  if (dstX >= fbW || dstY >= fbH) return; // tile entirely outside framebuffer
  const copyW = Math.min(32, fbW - dstX);
  const copyH = Math.min(32, fbH - dstY);

  // BGRA→RGBA swap, only for the bytes we'll actually upload. Pack tightly
  // (bytesPerRow = copyW * 4) so writeTexture sees a contiguous source.
  const rgba = new Uint8Array(copyW * copyH * 4);
  for (let row = 0; row < copyH; row++) {
    for (let col = 0; col < copyW; col++) {
      const srcOff = (row * 32 + col) * 4;
      const dstOff = (row * copyW + col) * 4;
      rgba[dstOff + 0] = bgra[srcOff + 2]; // R from B-slot
      rgba[dstOff + 1] = bgra[srcOff + 1]; // G
      rgba[dstOff + 2] = bgra[srcOff + 0]; // B from R-slot
      rgba[dstOff + 3] = bgra[srcOff + 3] === 0 ? 255 : bgra[srcOff + 3]; // force alpha
    }
  }
  this.device.queue.writeTexture(
    { texture: this.framebuffer.texture, origin: { x: dstX, y: dstY } },
    rgba,
    { bytesPerRow: copyW * 4, rowsPerImage: copyH },
    { width: copyW, height: copyH },
  );
}
```

- [ ] **Step 2: Verify build**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client && npm run build 2>&1 | tail -5
```

Expected: build succeeds.

- [ ] **Step 3: Run vitest**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client && npm test 2>&1 | tail -10
```

Expected: all 27+ vitest tests still pass. If a renderer-related test fails, the clip introduced a regression — investigate before continuing.

- [ ] **Step 4: Rebuild test-server container (if needed) + run e2e_edge_tiles with --ignored**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_edge_tiles -- --test-threads=1 --ignored --nocapture 2>&1 | tee /tmp/edge_r3.log | tail -15
```

Expected: PASS at all 4 pixel assertions.

If still failing: the diagnostic dump now shows whether the Raw fix landed. Check whether the edge tiles in the new `tiles` dump still have `codec=5` — if they switched to `codec=2` (PalRle) because the classifier reclassified after this client-side change, that's wrong (server-side classification is unaffected). More likely: there's a parallel PalRle dispatch issue. Escalate as a spec addendum (R3+R4 combined).

- [ ] **Step 5: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-web-client/src/webgpu/renderer.ts
git commit -m "fix(web-client): clip writeRawTile extent to framebuffer bounds (W5 R3)"
```

---

### Task 6: Un-ignore e2e_edge_tiles

**Files:**
- Modify: `ghostframe-lib/tests/e2e.rs` (the `#[ignore]` attribute on `e2e_edge_tiles`)

- [ ] **Step 1: Find the `#[ignore]` line**

```bash
grep -n "#\[ignore.*M3.2c carry-over.*700×500" /home/cedric/work/ghostframe/ghostframe-lib/tests/e2e.rs
```

It's around line 1199. Confirm the message is the W5-carry-over one introduced by commit `9e56fdd`.

- [ ] **Step 2: Remove the `#[ignore]` line**

Delete the entire `#[ignore = "M3.2c carry-over: ..."]` line. Also delete the block comment immediately above it that describes the W5 carry-over (the multi-line `// M3.2c W5 finding (2026-05-17): ...` block) — replace with a one-line comment referencing the fix:

```rust
// W5 closure (2026-05-17): partial edge tiles at non-tile-aligned
// resolutions now render correctly via [R1 / R3 / other] in
// `<commit-sha>`. See `docs/superpowers/specs/2026-05-17-edge-tiles-diagnose-fix-design.md`
// for the full Phase 1 + Phase 2 trail.
```

Fill in the actual commit SHA from Task 5R1 / 5R3.

- [ ] **Step 3: Verify the test now runs as part of the normal suite**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_edge_tiles -- --test-threads=1 --nocapture 2>&1 | tail -8
```

Expected: `1 passed; 0 failed; 0 ignored`.

- [ ] **Step 4: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/tests/e2e.rs
git commit -m "test(e2e): drop #[ignore] on e2e_edge_tiles — W5 carry-over closed"
```

---

### Task 7: Suite regression check

**Files:** none (operational task)

- [ ] **Step 1: Run the full e2e suite to catch regressions**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e -- --test-threads=1 2>&1 | tee /tmp/edge_final.log | tail -15
```

Expected: 21 passed (20 previously-passing + e2e_edge_tiles); 3 ignored (palrle_session_reset, decode_error_thin_uncached, resolution_change); 0 failed.

If any previously-passing test now fails: the W5 fix introduced a regression. Most likely culprits if R1 fix was applied: a test whose pixel-readback assumes exact-canvas-size framebuffer (e.g. `e2e_solid_per_tile_pixels` reads `canvas.width/height` and expects it equals the source resolution). If R3 fix was applied: a test that depends on writeRawTile painting beyond the framebuffer (none expected). Identify, then either update the dependent test or revisit the fix.

The flaky `e2e_palette_eviction` may fail intermittently in batch (passes in isolation) — not a regression from this work; the existing carry-over note in `project_m32c_near_complete.md` covers it.

- [ ] **Step 2: Run lib unit tests**

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib 2>&1 | tail -5
```

Expected: 278+ tests pass.

- [ ] **Step 3: Update memory**

Edit `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m32c_near_complete.md`. Move the e2e_edge_tiles entry from the "Ignored test" table into the "Production fixes that landed" / completed section. Update the suite-status line to `21 passed, 3 ignored`.

Edit `~/.claude/projects/-home-cedric-work-ghostframe/memory/MEMORY.md` — bump the project_m32c_near_complete summary line.

- [ ] **Step 4: Final commit**

```bash
cd /home/cedric/work/ghostframe
# the memory dir is NOT a git repo — no `git add` for those files.
# If there are project-tree changes from earlier steps that weren't committed,
# commit them now. Otherwise skip.
git status --short
```

If the working tree is clean, the milestone is closed. Otherwise commit any straggler changes with a `chore(w5): cleanup` message.

---

## Notes on parallelization

Tasks 1, 2, 3 are sequential (Task 2 builds on Task 1; Task 3 references the recorders). Task 4 is the gate. Task 5 has only ONE branch active at a time. Tasks 6, 7 are sequential after Task 5.

If a future R2/R4/R5 escalation is needed (Task 4 Step 6), that pathway requires its own new spec + plan — not parallel.
