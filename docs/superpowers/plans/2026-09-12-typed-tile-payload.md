# Typed Tile Payload Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `Event::TilePayload` carry what the GPU actually needs, so PalRLE and CDF53 are prevalidated once instead of twice and the renderer stops keeping its own palette shadow.

**Architecture:** `TileData` — an enum over the four codecs — replaces the `codec` tag plus raw `payload` pair. `reassembly.rs` already computes each codec's product; it stops discarding it. The browser's second prevalidation and the renderer's `PaletteShadow` are then deleted, and with them five TypeScript modules.

**Tech Stack:** Rust, `wasm-bindgen`, `serde-wasm-bindgen`, TypeScript 5.5, vitest 2.

Design: `docs/superpowers/specs/2026-09-12-typed-tile-payload-design.md`. Read it before starting — especially "Why the drain-time ordering constraint dissolves", which is the one genuine behavioural change here.

---

## The contract change is atomic

`Event::TilePayload` is matched exhaustively in `ghostframe-client-wasm/src/boundary.rs`. Changing its shape breaks that crate until the mirror is updated, so **Task 1 changes `event.rs`, `reassembly.rs` and `boundary.rs` together.** There is no smaller green step; do not try to manufacture one by leaving a compatibility shim, which would be code written only to be deleted two tasks later.

Every later task leaves the workspace building.

## What "done" means for the risky part

The renderer currently upserts a Bundled palette mid-drain so later thin entries in the same rAF see it. That constraint exists only because the renderer prevalidates. After this change the core prevalidates in wire order, `PaletteUpdated` precedes its `TilePayload`, and `main.ts` applies each upsert on arrival — strictly before the rAF drain.

**A mistake here shows up as wrong colours, not an error.** Task 5 exists to test it directly rather than trusting the e2e suite to notice.

---

## Task 1: `TileData`, emission, and the wasm mirror

**Files:**
- Modify: `ghostframe-client-core/src/event.rs`
- Modify: `ghostframe-client-core/src/reassembly.rs`
- Modify: `ghostframe-client-wasm/src/boundary.rs`

- [ ] **Step 1: Define `TileData` in `event.rs`**

```rust
/// What a completed tile actually hands the GPU.
///
/// The four codecs produce genuinely different things, so this is an enum
/// rather than a byte slice plus a codec tag the consumer has to interpret.
/// `PalRle` and `Cdf53` arrive prevalidated: `reassembly.rs` computes these
/// products anyway, to drive palette state and pass coverage, and emitting
/// the raw wire bytes instead forced every consumer to redo that work.
#[derive(Debug, Clone, PartialEq)]
pub enum TileData {
    /// BGRA wire bytes, unswizzled. Length is payload-proportional
    /// (<= 4096, a multiple of 4).
    Raw(Vec<u8>),
    /// One BGRA quad, expanded to the full tile by the shader.
    Solid([u8; 4]),
    /// `indices` is 512 bytes: two 4-bit palette indices per byte, low
    /// nibble first. The palette itself arrives separately as
    /// `Event::PaletteUpdated` — the GPU upload path never reads an upsert
    /// from here.
    PalRle {
        palette_id: u8,
        count: u8,
        indices: Vec<u8>,
    },
    /// `bit_planes` is 384 bytes: 3 channels x 128, packed B, G, R.
    Cdf53 { pass_idx: u8, bit_planes: Vec<u8> },
}
```

- [ ] **Step 2: Reshape `Event::TilePayload`**

Replace the existing variant (`event.rs:58-66`) with:

```rust
    TilePayload {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        /// Tile generation, for superseding. Stays on the event rather than
        /// in `TileData`: superseding is codec-independent.
        generation: u8,
        data: TileData,
    },
```

`pass_idx` and `codec` are gone — `pass_idx` moves into `TileData::Cdf53`, and the codec is now the variant itself.

- [ ] **Step 3: Emit it from all three sites in `reassembly.rs`**

There are exactly three `Event::TilePayload` pushes — at lines **257** (the `Raw | Solid` early return), **342** (PalRle), and **428** (Cdf53). Re-derive those line numbers; they drift.

- **257** splits on codec: `Codec::Raw => TileData::Raw(payload)`, `Codec::Solid => TileData::Solid(<4-byte quad>)`. Solid's payload is 4 bytes; convert with `try_into` and handle the error rather than indexing — this is wire-derived data. If a length check already guarantees 4, say so in a comment instead of adding a redundant branch.
- **342** has `validated` in scope from `prevalidate_pal_rle`. Use `validated.palette_id`, `validated.count`, `validated.indices`. **Do not** pass `validated.palette_upsert` — see the design's "Why PalRle carries no upsert".
- **428** has `pre` in scope from `prevalidate_cdf53`. Use `pre.pass_idx` and `pre.bit_planes`.

The `payload` binding may become unused at 342/428. Remove it where it is genuinely dead; do not silence a warning with `_payload` if the value is simply no longer needed.

- [ ] **Step 4: Mirror it in `boundary.rs`**

`WasmEvent::TilePayload` needs a matching reshape and a `WasmTileData` mirror, serialised `#[serde(tag = "codec")]` so JS sees `{ codec: 'PalRle', palette_id, count, indices }`.

**Every `Vec<u8>` needs `#[serde(with = "serde_bytes")]`.** Without it `serde_wasm_bindgen` emits a plain JS `Array` of numbers rather than a `Uint8Array`; this already bit the cutover once, and a 512-byte indices buffer arriving as 512 boxed numbers would reach the GPU upload path.

Keep the `match` on `Event` exhaustive, and make the `match` on `TileData` exhaustive too — no `_` arm. That exhaustiveness is what turns a future variant into a compile error instead of a silently dropped tile.

- [ ] **Step 5: Verify**

```bash
cd /home/cedric/work/ghostframe
cargo build --workspace 2>&1 | tail -3
cargo build -p ghostframe-client-wasm --target wasm32-unknown-unknown 2>&1 | tail -2
cargo clippy -p ghostframe-client-core -p ghostframe-client-wasm --all-targets 2>&1 | tail -5
cargo fmt -p ghostframe-client-core -p ghostframe-client-wasm
```

`cargo test` will not pass yet — `tile_delivery.rs` still asserts the old shape. That is Task 2. Report the failures rather than fixing them here.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-client-core/src/ ghostframe-client-wasm/src/boundary.rs
git commit -m "feat(client-core): TilePayload carries a typed TileData

reassembly.rs computed each codec's GPU product and emitted the raw wire
bytes instead, so every consumer re-derived it. TileData carries the
product: prevalidated indices for PalRle, bit_planes for Cdf53, raw
bytes for Raw/Solid, which genuinely want them."
```

## Task 2: Update `tile_delivery.rs`

**Files:**
- Modify: `ghostframe-client-core/tests/tile_delivery.rs`

This file is the behavioural authority for `Payload` mode — 34 assertions across 11 tests.

- [ ] **Step 1: Repoint the assertions at `TileData`**

**Change how each value is reached; do not change any asserted value.** Two assertions in particular must survive with their meaning intact:

- `"Payload mode must not emit TileReady"` — unchanged, still true.
- `raw_in_payload_mode_passes_bytes_through`'s `payload == bgra`, *"payload must be the wire bytes, unswizzled"* — now `TileData::Raw(bytes)`, same bytes.

The PalRLE and CDF53 tests previously asserted on the raw payload. They now assert on the prevalidated product, which is a **stronger** assertion — it checks what the GPU receives rather than what arrived on the wire. Where a test asserted a raw payload round-tripped, assert the prevalidated fields instead and say so in a comment.

- [ ] **Step 2: Run**

```bash
cd /home/cedric/work/ghostframe
cargo test -p ghostframe-client-core 2>&1 | grep 'test result' | tail -3
```

Expected: all green. Report the assertion count before and after — it should not fall. If it does, say which assertion you dropped and why its subject no longer exists.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-client-core/tests/tile_delivery.rs
git commit -m "test(client-core): assert on TileData rather than raw payloads"
```

## Task 3: The `main.ts` dispatcher

**Files:**
- Modify: `ghostframe-web-client/src/main.ts`
- Modify: `ghostframe-web-client/src/cdf53_globals.ts`

- [ ] **Step 1: Switch on `data.codec` instead of `ev.codec`**

The dispatcher currently reads `ev.codec` against the TS `Codec` enum and, for CDF53, calls `prevalidateCdf53` a second time. Both go:

```ts
case 'TilePayload': {
  const d = ev.data;
  switch (d.codec) {
    case 'Raw':
      renderer.pushRaw({ tileX: ev.tile_x, tileY: ev.tile_y, bgra: d.bytes });
      break;
    case 'Solid':
      renderer.pushSolid({ tileX: ev.tile_x, tileY: ev.tile_y, bgra: d.bytes });
      break;
    case 'PalRle':
      renderer.pushPalRle({
        tileX: ev.tile_x, tileY: ev.tile_y,
        paletteId: d.palette_id, count: d.count, indices: d.indices,
      });
      break;
    case 'Cdf53':
      renderer.pushCdf53({
        tileX: ev.tile_x, tileY: ev.tile_y,
        gen: ev.generation, passIdx: d.pass_idx, bitPlanes: d.bit_planes,
      });
      break;
  }
  break;
}
```

Field names on the renderer side are Task 4's to settle — match whatever `renderer.ts` ends up taking, and keep the two tasks consistent.

The serde tag names (`'Raw'`, `'Solid'`, …) come from `#[serde(tag = "codec")]` in Task 1. **Check the generated `.d.ts` rather than assuming** — if Task 1 chose a different tag key, use that.

- [ ] **Step 2: Update `cdf53_globals.ts`**

It reads `ev.codec` to decide whether an event is CDF53. That now comes from `ev.data.codec === 'Cdf53'`. Its `recordProtocolEvent` signature takes a `cdf53Codec` discriminant argument that becomes unnecessary — remove it and update `tests/cdf53_globals.test.ts` accordingly.

Keep `__cdf53DispatchSeen`'s formula — `TilePayload{Cdf53} + DecodeError{Cdf53}`. It is easy to lose while refactoring the codec check.

- [ ] **Step 3: Verify**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client
export PATH="$HOME/.cargo/bin:$PATH"
rm -rf pkg-web pkg-node dist
npm test 2>&1 | tail -12
```

Clean the `pkg` directories first — a stale one masked a missing build step earlier in this project and let a broken tree look green.

`npm run build` will fail until Task 4: `renderer.pushPalRle` still expects the old shape. Expected; report it.

- [ ] **Step 4: Commit**

## Task 4: Remove the renderer's prevalidation and palette shadow

**Files:**
- Modify: `ghostframe-web-client/src/webgpu/renderer.ts`
- Modify: `ghostframe-web-client/src/webgpu/palrle.ts`
- Modify: `ghostframe-web-client/src/webgpu/cdf53.ts`

This is the `src/webgpu/` change the design argues for explicitly. **Signatures and the prevalidation block only. No shader, no pipeline logic, no buffer layout.** If you find yourself editing WGSL or `uploadBatch`'s body, stop — that is out of scope.

- [ ] **Step 1: Change what `pushPalRle` accepts**

It currently takes `PalRleQueued = { tileX, tileY, payload }` and prevalidates at drain (`renderer.ts:294`). It should take the prevalidated entry directly. Define a local type — the imported `PalRleEntry` disappears with `prevalidate.ts`:

```ts
/** A prevalidated PalRLE tile, ready for the GPU. Produced by the core,
 *  not derived here — the renderer no longer prevalidates. */
export interface PalRleTile {
  tileX: number;
  tileY: number;
  paletteId: number;
  count: number;
  /** 512 bytes: two 4-bit indices per byte, low nibble first. */
  indices: Uint8Array;
}
```

- [ ] **Step 2: Delete the drain-time prevalidation loop**

The loop at `renderer.ts:292-306` becomes a direct move of the queue into the batch. With it go:
- the `prevalidatePalRle` call and its `onDecodeError` branch — the core reports decode errors as `Event::DecodeError` now
- the `upsertPalette` + `paletteShadow.put` block — `main.ts` already applies palettes from `PaletteUpdated`, so this was a duplicate write
- the `paletteShadow` field (`:63`) and its `clear()` (`:224`)

`uploadBatch` reads only `paletteId`, `count` and `indices`, so it needs no change.

- [ ] **Step 3: Repoint `pushCdf53`'s type**

`PrevalidatedCdf53` is imported from `../prevalidate_cdf53.js`, which Task 6 deletes. Move the interface into `webgpu/cdf53.ts` as a local type. It is a plain data shape; the move is mechanical.

- [ ] **Step 4: Verify**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client
export PATH="$HOME/.cargo/bin:$PATH"
rm -rf pkg-web pkg-node dist
npm run build 2>&1 | tail -6
npm test 2>&1 | tail -8
```

Both must pass now. Report the results.

- [ ] **Step 5: Commit**

## Task 5: Test the palette ordering directly

**Files:**
- Create or modify: `ghostframe-client-core/tests/tile_delivery.rs`

The design's sharpest risk: a Bundled tile and a thin tile referencing the same palette arriving close together. Previously the renderer's drain loop guaranteed the upsert landed first. Now the core's wire-order prevalidation does.

- [ ] **Step 1: Write the test**

Drive a core in `Payload` mode with, in order: a **Bundled** PalRLE tile establishing palette N, then a **thin** tile referencing palette N. Assert:

1. A `PaletteUpdated` for palette N is emitted **before** the thin tile's `TilePayload` in the event sequence.
2. The thin tile produces a `TilePayload`, not a `DecodeError` — a thin tile against an unknown palette fails with `ThinUncachedPalette` (code 3), so this passing proves the shadow was updated in time.

Assertion 2 is the load-bearing one: it fails loudly if ordering regresses, whereas a colour error would not.

- [ ] **Step 2: Prove it discriminates**

Reverse the order — thin tile first, then Bundled — and assert the thin tile *does* produce `ThinUncachedPalette`. If both orderings pass, the test is not actually checking ordering. Report both outcomes.

- [ ] **Step 3: Run and commit**

```bash
cargo test -p ghostframe-client-core --test tile_delivery 2>&1 | grep 'test result'
```

## Task 6: Delete the five TypeScript modules

**Files:**
- Delete: `src/prevalidate.ts`, `src/prevalidate_cdf53.ts`, `src/palette_shadow.ts`, `src/feedback.ts`, `src/decode_error_batcher.ts`

- [ ] **Step 1: Confirm each is unreferenced**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client
for m in prevalidate prevalidate_cdf53 palette_shadow feedback decode_error_batcher; do
  printf "%-22s %s\n" "$m" "$(grep -rl "from '\./$m\|from '\.\./$m" src/ tests/ | tr '\n' ' ')"
done
```

Every line must be empty. **A non-empty line means a call site was missed — stop and report it.** That matters more than the deletion.

`decoder.ts` stays: `FullFrameDecoder` is WebCodecs glue. `Codec` became unused in `main.ts` and its import was removed in Task 3; the export stays — `decoder.ts` is not this plan's to prune.

**One call site must be repointed before `prevalidate_cdf53.ts` can go.** `main.ts`'s `__cdf53TestIntegrate` global still calls the TypeScript `prevalidateCdf53` — a GPU test hook, unrelated to the dispatcher, and **hard-asserted by `e2e_cdf53_integrate_correctness`**. It is not dead code; deleting the module under it would break a gating e2e test.

Repoint it at the wasm export `prevalidateCdf53(payload, generation, pass_idx)`, which returns the flat `{ ok, code, generation, pass_idx, bit_planes }` shape. Adapt at the call site to whatever the hook feeds the GPU, and verify it still produces the same values — an assertion reads this, not a diagnostic, so a wrong shape fails CI rather than passing quietly.

- [ ] **Step 2: Delete, verify on a clean tree, commit**

```bash
rm -rf pkg-web pkg-node dist
npm run build && npm test
```

## Task 7: Full verification and PR

- [ ] **Step 1: Everything, from clean**

```bash
cd /home/cedric/work/ghostframe
cargo test -p ghostframe-client-core -p ghostframe-client-wasm 2>&1 | grep 'test result'
cargo clippy --workspace --all-targets 2>&1 | tail -3
cargo build --workspace
cd ghostframe-web-client && rm -rf pkg-web pkg-node dist && npm test && npm run build
```

- [ ] **Step 2: Re-measure the bundle**

Append to `docs/specs/wasm-bundle-baseline.md` using that document's own commands. Deleting five modules should shrink the JS slightly; the wasm may grow marginally. Report the real numbers — this is a record, not a gate.

- [ ] **Step 3: Open the PR**

The browser e2e suite is the acceptance gate and runs only in CI. **Do not claim it passes locally.** Call out the palette-ordering change explicitly for review.

---

## Done criteria

- [ ] `cargo test`, `cargo clippy --workspace`, both wasm targets: clean.
- [ ] `npm test` and `npm run build` pass **from a clean tree**.
- [ ] `tile_delivery.rs`'s assertion count did not fall.
- [ ] The palette-ordering test was observed failing under the reversed order.
- [ ] No file under `src/` imports a deleted module.
- [ ] No WGSL shader changed. `git diff --stat` on `src/webgpu/` shows signature and prevalidation changes only.
- [ ] Browser e2e green in CI.

## What this plan does not do

- Touch any shader, pipeline, or buffer layout.
- Change `TileDelivery::Decoded` behaviour — no native consumer uses `Payload`.
- Delete `decoder.ts`.
- Chase the bundle regression recorded in `wasm-bundle-baseline.md`.
