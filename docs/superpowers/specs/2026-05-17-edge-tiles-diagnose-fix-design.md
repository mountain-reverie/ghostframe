# Edge-Tiles Diagnose-Then-Fix — Design

**Date**: 2026-05-17
**Milestone**: M3.2c W5 closure (the carry-over from the 2026-05-15 verification milestone)
**Predecessor**: `2026-05-15-m3.2c-verification-design.md` W5 section + commit `9e56fdd` ("diag(w5): e2e_edge_tiles 700×500 partial-tile rendering carry-over")

## Background

`e2e_edge_tiles` drives an Xorg-on-dummy display at 700×500 (a non-tile-aligned resolution), paints solid red on the X11 root, and reads back four edge-pixel coordinates on the WebGPU client to verify partial tiles at the right column (28 px wide, vs the standard 32) and bottom row (20 px tall, vs the standard 32) render correctly.

As of commit `9e56fdd`, the test infrastructure works end-to-end:
- The setup-helper `XORG_CONF` override-order bug is fixed; `extra_env` correctly propagates.
- Switching the test from `setup_e2e_webgpu_gpu_with_env` to `setup_e2e_webgpu_with_env` makes xorg-dummy at 700×500 the actual capture source (not VKMS at 1024×768).
- The 8-point `__readPixel` diagnostic readback confirms canvas dimensions = 700×500 (matching the X11 root) and full-tile pixels at `(16,16)`, `(350,250)` render red as expected.

But: partial-tile pixels at `(695,240)`, `(320,495)`, `(695,495)`, `(699,499)` all return `[0,0,0,0]` (transparent black). Server-side `TileGrid::extract_tile` zero-pads partial tiles to 32×32, so the wire payloads are well-formed. The bug is on the client renderer side, root cause unknown.

The W5 1-day hard-stop from the original M3.2c plan was reached during the diagnostic phase. This spec resumes the work post-M3.2c-near-complete.

## Goal

Identify and fix the root cause of partial edge tiles rendering as transparent black on the WebGPU client when the captured frame's width or height isn't a multiple of 32. End state: `e2e_edge_tiles` passes without `#[ignore]`.

## Approach

Two phases, single spec. Phase 1 is fully specified; Phase 2 is a decision tree keyed off Phase 1's findings, with code templates for the two most-likely buckets pre-written and a TBD branch for the rest.

### Phase 1 — Diagnose

#### Client-side recorders in `main.ts`

Two new `window.*` arrays, attached at the same instrumentation block as the existing `__ghostframeRecordedCodecs` recorder (around `main.ts:340-347`):

```typescript
// Per-tile event log: pushed for every arrived tile, captures the
// framebuffer dimensions at the moment the tile is queued. Tells us
// whether partial tiles arrive before or after the canvas resize.
const w = window as unknown as {
  __ghostframeRecordedTiles?: Array<{
    seq: number;
    tileX: number;
    tileY: number;
    codec: number;
    payloadLen: number;
    fbWidth: number;
    fbHeight: number;
  }>;
  __ghostframeRecordedResizes?: Array<{
    seq: number;
    oldW: number;
    oldH: number;
    newW: number;
    newH: number;
    trigger: 'sentinel' | 'fallback-expand';
  }>;
};
w.__ghostframeRecordedTiles ??= [];
w.__ghostframeRecordedResizes ??= [];
w.__ghostframeRecordedTiles.push({
  seq: asm.header.frameSeq,
  tileX: tX,
  tileY: tY,
  codec: asm.header.codec,
  payloadLen: payload.byteLength,
  fbWidth: renderer.framebuffer.width,
  fbHeight: renderer.framebuffer.height,
});
```

The resize recorder wraps the two `renderer.resize(...)` call sites in `main.ts` (the sentinel branch around line 321 and the fallback-expand branch around line 333). Each call site pushes `{seq, oldW, oldH, newW, newH, trigger}` before delegating to renderer.resize.

#### Test changes in `e2e.rs`

`e2e_edge_tiles` keeps its existing 8-point `__readPixel` diagnostic matrix. After that block, two new evaluate-and-dump calls:

```rust
let tiles: serde_json::Value = setup.page
    .evaluate("window.__ghostframeRecordedTiles || []").await?.into_value()?;
let resizes: serde_json::Value = setup.page
    .evaluate("window.__ghostframeRecordedResizes || []").await?.into_value()?;
eprintln!("e2e_edge_tiles tiles: {}", serde_json::to_string_pretty(&tiles).unwrap());
eprintln!("e2e_edge_tiles resizes: {}", serde_json::to_string_pretty(&resizes).unwrap());
```

The test stays `#[ignore]`'d for the diagnose run — `cargo test ... --ignored --nocapture` runs it explicitly. Pixel-readback assertions still fail; we're collecting evidence, not asserting yet.

#### Analysis

After the diagnostic run, manually classify the data into one of five root-cause buckets:

| Bucket | Signature in the recorded data | Likely fix |
|---|---|---|
| **R1. Resize-shrink loses content** | Edge tiles for col 21 / row 15 arrive at `fbWidth=704` / `fbHeight=512`, then a resize event 704→700 / 512→500 happens. Partial-tile bytes at `x ≥ 700` / `y ≥ 500` get destroyed during the shrink. | Round fallback-expand up to 32-multiples to avoid the shrink; OR have framebuffer.resize() preserve `min(old, new)` bytes (already does this — verify it works for shrinks). |
| **R2. Edge tiles never dispatched** | `__ghostframeRecordedTiles` has no entries for `tileX=21` or `tileY=15`. | Server-side: revisit `TileGrid::iter_coords`, scheduler dispatch, or dirty-tracker per-tile bounds. |
| **R3. Edge tiles dispatched as Raw with OOB write** | Edge tiles for col 21 / row 15 arrive with `codec=5` (Raw); `payloadLen=4096`; `fbWidth=700`. `writeRawTile` silently fails the WebGPU validation `origin.x + 32 > fb.width` check. | Clip `writeRawTile`'s extent and source-row count to `min(32, fb.width - tileX*32)` × `min(32, fb.height - tileY*32)`. |
| **R4. Edge tiles dispatched as PalRle with shader out-of-bounds writes** | Edge tiles arrive with `codec=2`; `payloadLen` reasonable; `fbWidth=700`. GPU dispatch runs but `textureStore` at out-of-bounds pixels is no-op per WebGPU spec. But in-bounds pixels (e.g. (695, 240)) should still paint — if they don't, this is unexpected and the spec gets an addendum. | TBD pending data; likely shader change clipping `pixel_x_in_tile`/`pixel_y_in_tile` to in-bounds before `textureStore`. |
| **R5. Solid quad clipped by canvas-edge fragment culling** | Edge tiles arrive with `codec=4` (Solid); NDC quad at right edge is correctly within `[-1, +1]` but fragments at `pixel_x ∈ [672, 699]` don't fire. | TBD; deeper Solid pipeline bug. |

### Phase 2 — Fix (decision tree)

The spec ships with pre-written code templates for R1 and R3 (the two most-likely buckets). R2, R4, R5 get TBD entries that fill in after Phase 1 produces data.

#### R1 template (resize-shrink content loss)

In `framebuffer.ts::resize`, the existing `copyTextureToTexture` already copies `min(oldW, newW) × min(oldH, newH)`. The bug, if R1 fires, is that the SHRINK path destroys the bytes from the old texture's `[newW..oldW]` range — which is exactly the partial-tile region we want.

Two complementary fixes:

1. **Round fallback-expand up to 32-multiples** in `main.ts` (fallback-expand branch around line 333):
   ```typescript
   const minWidth = (tX + 1) * TILE_SIZE;
   const minHeight = (tY + 1) * TILE_SIZE;
   if (canvasEl.width < minWidth || canvasEl.height < minHeight) {
     // Round to 32-multiple so subsequent sentinel-shrink doesn't truncate
     // partial edge tiles we already painted.
     const grownW = Math.max(canvasEl.width, minWidth);
     const grownH = Math.max(canvasEl.height, minHeight);
     renderer.resize(
       Math.ceil(grownW / TILE_SIZE) * TILE_SIZE,
       Math.ceil(grownH / TILE_SIZE) * TILE_SIZE,
     );
   }
   ```

2. **Sentinel-resize allows over-allocation** (i.e. accept that the framebuffer texture may be slightly larger than the canvas's display dimensions; only the present-blit crops to canvas size):
   ```typescript
   if (tX === FRAME_DIMENSIONS_SENTINEL_X && tY === FRAME_DIMENSIONS_SENTINEL_Y) {
     if (payload.byteLength >= 8) {
       const view = new DataView(payload.buffer, payload.byteOffset, 8);
       const w = view.getUint32(0, false);
       const h = view.getUint32(4, false);
       // Allocate to the tile-aligned ceiling so partial-tile writes from
       // earlier in the burst aren't truncated by an exact-size shrink.
       const fbW = Math.ceil(w / TILE_SIZE) * TILE_SIZE;
       const fbH = Math.ceil(h / TILE_SIZE) * TILE_SIZE;
       renderer.resize(fbW, fbH);
       // Inform the present-blit to crop to (w, h) — see fix below.
       renderer.setDisplayCrop(w, h);
       frameDimensionsKnown = true;
     }
     return;
   }
   ```
   And in `framebuffer.ts::encodePresentBlit`, add a `setDisplayCrop(w, h)` that constrains the blit's source rect to `(0, 0, w, h)` so the viewer only sees the actual frame, not the tile-aligned over-allocation.

#### R3 template (Raw writeTexture out-of-bounds)

In `renderer.ts::writeRawTile`:

```typescript
writeRawTile(tileX: number, tileY: number, bgra: Uint8Array): void {
  if (bgra.length !== 32 * 32 * 4) {
    throw new Error(`writeRawTile: payload length ${bgra.length} != 4096`);
  }
  // Clip the destination extent to the framebuffer bounds.
  const fbW = this.framebuffer.width;
  const fbH = this.framebuffer.height;
  const dstX = tileX * 32;
  const dstY = tileY * 32;
  if (dstX >= fbW || dstY >= fbH) return; // tile entirely outside framebuffer
  const copyW = Math.min(32, fbW - dstX);
  const copyH = Math.min(32, fbH - dstY);

  // BGRA -> RGBA swap, only for the bytes we'll actually upload.
  const rgba = new Uint8Array(copyW * copyH * 4);
  for (let row = 0; row < copyH; row++) {
    for (let col = 0; col < copyW; col++) {
      const srcOff = (row * 32 + col) * 4;
      const dstOff = (row * copyW + col) * 4;
      rgba[dstOff + 0] = bgra[srcOff + 2];
      rgba[dstOff + 1] = bgra[srcOff + 1];
      rgba[dstOff + 2] = bgra[srcOff + 0];
      rgba[dstOff + 3] = bgra[srcOff + 3] === 0 ? 255 : bgra[srcOff + 3];
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

Parallel clip on the PalRle dispatch shape and the Solid quad pixel extent is straightforward if R4 or R5 also fires.

#### R2 / R4 / R5 branches

Each gets a TBD section. After Phase 1, the spec is appended with a `## Phase 1 findings` section documenting what was observed, which bucket matched, and the chosen code template. If R2/R4/R5 fires, that template gets designed from scratch then.

### Files Touched

**Phase 1**:
- `ghostframe-web-client/src/main.ts` — add tile + resize recorders (~25 lines added)
- `ghostframe-lib/tests/e2e.rs` — extend `e2e_edge_tiles` diagnostic readback (~10 lines added)

**Phase 2** (depends on findings, but expected):
- `ghostframe-web-client/src/webgpu/renderer.ts` — `writeRawTile` clip (R3 template)
- `ghostframe-web-client/src/webgpu/framebuffer.ts` — `setDisplayCrop` + resize shrink semantics (R1 template)
- `ghostframe-web-client/src/main.ts` — fallback-expand round-up + sentinel-resize over-allocate (R1 template)
- `ghostframe-lib/tests/e2e.rs` — drop `#[ignore]` line on `e2e_edge_tiles`

### Testing

- **Phase 1**: `cargo test -p ghostframe-lib --test e2e e2e_edge_tiles -- --test-threads=1 --ignored --nocapture`. Test still fails (pixel assertions unchanged); diagnostic dump prints to stderr.
- **Phase 2**: drop `#[ignore]` on `e2e_edge_tiles`. Test runs as part of normal suite. Expected: PASS at 700×500. The existing pixel-readback assertions in the test catch regressions automatically.

If the fix lands without `#[ignore]` removed, that's a bug. Reviewer must verify the diff drops the ignore line.

### Out of Scope

- Refactoring the canvas-grow fallback wholesale. The fallback exists because the FRAME_DIMENSIONS sentinel can arrive after some tiles (per `main.ts:329` comment); we keep that and just round up.
- Multi-monitor / non-rectangular frames. M3.2c only covers single-display capture.
- Touching the H.264 viewport (`h264.ts:53` hardcodes 32×32) — H.264 tiles are full-frame, not per-tile in M3.2c, so partial-tile concerns don't apply.
- Removing the diagnostic recorders after the fix lands. They cost ~25 lines and may help future debugging — keep them. (Match the precedent set by `GHOSTFRAME_DIAGNOSE_TILES` in `reference_e2e_diagnose.md`.)

### Risk

- **Phase 1 produces ambiguous data**: more than one bucket plausibly matches. Mitigation: the recorders capture enough state (`fbWidth/Height` at tile-queue time, resize trigger reason) that ambiguity is unlikely; if it happens, run twice with `--nocapture` to look for non-determinism.
- **Phase 2 fix breaks full-tile-aligned resolutions**: the R3 clip is a no-op for full tiles (copyW=32, copyH=32); the R1 over-allocation grows the texture but doesn't affect content. Mitigation: re-run the full e2e suite after the fix, not just `e2e_edge_tiles`.
- **R4/R5 fires** and Phase 2 doesn't have a template: the spec gets an addendum and Phase 2 stretches. Mitigation: accept this; the diagnose-first approach is precisely the bet that an undiagnosed defensive fix is more risk than a brief delay to design a targeted one.

### Pointers

- Plan (to be created next via writing-plans skill).
- Predecessor session memory: `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m32c_near_complete.md`
- Original M3.2c verification design: `docs/superpowers/specs/2026-05-15-m3.2c-verification-design.md` § W5
- Diagnostic env-var conventions reference: `~/.claude/projects/-home-cedric-work-ghostframe/memory/reference_e2e_diagnose.md`

## Phase 1 findings (2026-05-18)

**Bucket matched:** R3 (Raw writeTexture out-of-bounds).

**Evidence (from `/tmp/edge_diag.log`):**

```
e2e_edge_tiles tiles: [
  { codec: 5, fbWidth: 700, fbHeight: 500, payloadLen: 4096, seq: 12, tileX: 21, tileY: 0 },
  { codec: 5, fbWidth: 700, fbHeight: 500, payloadLen: 4096, seq: 12, tileX: 21, tileY: 1 },
  ... (every col 21 tile classified as Codec::Raw, full 32×32 payload)
]
```

Canvas is sized correctly to 700×500 (matching the test pattern). Server emits Raw with the full 32×32 zero-padded tile payload (per `TileGrid::extract_tile`). The client's `writeRawTile` tries `writeTexture(origin.x = 21*32 = 672, extent.width = 32)` against a 700-wide framebuffer — `origin.x + width = 704 > 700` → WebGPU validation error → silent drop → pixel stays at the default (transparent black).

(Side note: `resizes` was empty in this run. The canvas reaches 700×500 through some path that doesn't go through the `recordingResize` helper, OR the recorder loads after the first resize. Not blocking — the tile classification is enough to confirm R3.)

**Chosen Phase 2 path:** R3 template — clip `writeRawTile`'s extent to `min(32, fb.width - tileX*32) × min(32, fb.height - tileY*32)` and pack the source bytes tightly (`bytesPerRow = copyW * 4`) so writeTexture sees a contiguous source.
