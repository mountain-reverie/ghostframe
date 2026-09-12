# Typed Tile Payload — Design

**Status:** proposed
**Follows on from** `2026-09-11-wasm-cutover-design.md`, whose step 4b could
not delete six TypeScript modules for the reason below.

## Problem

`Event::TilePayload` carries the raw wire payload for every codec:

```rust
TilePayload { frame_seq, tile_x, tile_y, pass_idx, generation, codec, payload }
```

For `Raw` and `Solid` that is right — the GPU consumes those bytes directly.
For `PalRle` and `Cdf53` it is wrong, and the cost compounds:

1. `reassembly.rs` **already computes** the product the GPU needs — the
   expanded 512-byte `indices` for PalRle, the 384-byte `bit_planes` for
   Cdf53 — then emits the raw payload and drops it.
2. So the browser re-derives it. `main.ts` calls `prevalidateCdf53` a second
   time for `pushCdf53`, and `webgpu/renderer.ts:294` calls
   `prevalidatePalRle` a second time at drain.
3. Because the renderer prevalidates, it needs its own `PaletteShadow`
   (`renderer.ts:63`) — a second copy of protocol state the core already owns.
4. Because the renderer imports `prevalidatePalRle` and `PaletteShadow` as
   runtime values, **six TypeScript modules survived the cutover**:
   `prevalidate.ts`, `prevalidate_cdf53.ts`, `palette_shadow.ts`,
   `feedback.ts`, `decode_error_batcher.ts`, `decoder.ts`.

That is drift surviving a migration whose entire purpose was removing drift.
The cutover accepted it to keep the irreversible step small; this closes it.

## Goals

- One prevalidator per codec, in Rust.
- One palette shadow, in the core.
- Delete the TypeScript modules the renderer's duplication keeps alive.

## Non-goals

- Touching any WGSL shader or GPU pipeline logic. Signatures change; what runs
  on the GPU does not.
- `TileDelivery::Decoded`. No native consumer uses `Payload` —
  `ghostframe-lib` and `ghostframe-e2e` run `Decoded`, and only tests exercise
  `Payload` — so this is a browser-path change.
- Bundle size. This removes duplicated *work*, not meaningful bytes.

## Approach

`TilePayload` keeps its name and gains a typed body. The codec discriminant
and the per-codec fields collapse into one enum:

```rust
/// What a completed tile actually hands the GPU. The four codecs produce
/// genuinely different things, so this is an enum rather than a byte slice
/// plus a codec tag the consumer has to interpret.
pub enum TileData {
    /// BGRA wire bytes, unswizzled. Length is payload-proportional
    /// (<= 4096, a multiple of 4).
    Raw(Vec<u8>),
    /// One BGRA quad, expanded to the tile by the shader.
    Solid([u8; 4]),
    /// Prevalidated. `indices` is 512 bytes, two 4-bit indices per byte,
    /// low nibble first. The palette itself arrives separately as
    /// `PaletteUpdated` — see "Why PalRle carries no upsert".
    PalRle { palette_id: u8, count: u8, indices: Vec<u8> },
    /// Prevalidated. `bit_planes` is 384 bytes: 3 channels x 128, packed
    /// B, G, R.
    Cdf53 { pass_idx: u8, bit_planes: Vec<u8> },
}

pub enum Event {
    TilePayload {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        generation: u8,
        data: TileData,
    },
    // ... other variants unchanged
}
```

**`pass_idx` moves into `Cdf53`.** It is meaningless for the other three,
which have a single pass. Verified: the only TypeScript reads of `pass_idx`
are in `main.ts`'s Cdf53 branch.

**`generation` stays on the event.** It is a property of the tile assembly,
not of the codec product, and the superseding logic is codec-independent.

**Name retained.** It is still the tile's payload; only its type improves.
Renaming would churn every call site for no gain.

### Why PalRle carries no upsert

`webgpu/palrle.ts`'s `uploadBatch` reads `paletteId`, `count` and `indices` —
**never** `paletteUpsert`. Palette writes go through `upsertPalette`, which
`main.ts` already calls from `PaletteUpdated` (`main.ts:632`). The renderer's
own upsert (`renderer.ts:302`) is therefore a duplicate of a write that has
already happened — the cutover noted it as "idempotent but redundant".

`variant` drops for the same reason: the renderer used it only to decide
whether to upsert, and that decision moves to the core.

### Why the drain-time ordering constraint dissolves

`renderer.ts:299` upserts a Bundled palette mid-loop "so subsequent
thin/indices_raw entries in the same rAF see the palette". That constraint
exists *because* the renderer prevalidates: a thin entry needs
`shadow.has(palette_id)` at prevalidation time.

Under this design the core prevalidates in wire order and emits
`PaletteUpdated` before the `TilePayload` it belongs to. `main.ts` applies
each upsert on arrival, which is strictly before the rAF drain that uploads
the batch. The ordering requirement is satisfied earlier, and by construction
rather than by loop order inside the renderer.

**This is the sharpest risk in the design.** It is a real behavioural change
in a path only the browser e2e suite can check, and getting it wrong shows up
as wrong colours rather than an error.

## What this deletes

| File | Why it can go |
|---|---|
| `src/prevalidate.ts` | the renderer's only caller of `prevalidatePalRle` goes |
| `src/palette_shadow.ts` | the renderer's shadow goes |
| `src/prevalidate_cdf53.ts` | `main.ts`'s second prevalidation goes; `PrevalidatedCdf53` becomes a renderer-local type |
| `src/decode_error_batcher.ts` | its remaining caller reports the renderer's own prevalidation failures, which no longer exist |
| `src/feedback.ts` | only the kept prevalidators needed its `ERR_*` constants |

`decoder.ts` stays: it holds `FullFrameDecoder` (WebCodecs glue) and `Codec`.
`Codec` may become unused in `main.ts` once the dispatcher switches on `data`,
which is a simplification rather than a deletion.

## Changes to `src/webgpu/`

The cutover spec names `src/webgpu/` a non-goal: *"Replacing, porting, or
touching `src/webgpu/` or any WGSL shader."*

This design touches it, and that should be explicit rather than quietly
reinterpreted. What changes:

- `renderer.ts`: `pushPalRle` takes a prevalidated entry; the drain-time
  prevalidation loop and `paletteShadow` are deleted; `pushCdf53`'s parameter
  type moves from an imported type to a local one.
- `palrle.ts` / `cdf53.ts`: type imports repoint.

What does not change: every shader, every pipeline, every buffer layout, and
`uploadBatch`'s logic.

The non-goal was written to stop the GPU decode path being ported into wasm,
which this does not do. Changing a TypeScript call signature so the renderer
stops duplicating protocol work is a different act. Confirmed with the project
owner before this design was written.

## Testing

- `tile_delivery.rs` is the behavioural authority for `Payload` mode and must
  be updated to assert on `TileData` variants. Its existing assertions —
  including *"Payload mode must not emit TileReady"* and Raw's *"payload must
  be the wire bytes, unswizzled"* — must survive unchanged in meaning.
- `boundary.rs`'s exhaustive `match` on `Event` makes a missed variant a
  compile error. `TileData` must be matched exhaustively there too.
- **The palette-ordering change needs a targeted test**, not just e2e
  confidence: a case where a Bundled tile and a thin tile referencing the same
  palette arrive in the same batch. `e2e_palette_eviction_*` and
  `e2e_palrle_exact_pixels` are the existing pixel-level guards.
- The browser e2e suite (54 tests, Chromium + Firefox) is the acceptance gate
  and runs only in CI.

## Risks

| Risk | Mitigation |
|---|---|
| Palette ordering regresses; shows as wrong colours, not an error | A targeted same-batch test, plus the existing PalRLE pixel assertions |
| A `TileData` variant is dropped at the wasm boundary | Exhaustive `match` in `boundary.rs` — already proven to fail the build when a variant is added |
| Byte buffers cross as `Array` not `Uint8Array` | `#[serde(with = "serde_bytes")]` on every `Vec<u8>`; this bit the cutover once already |
| Scope creep into GPU pipeline logic | The deletions are enumerated above; anything beyond them is out of scope |

## Sequencing

1. `TileData` in `client-core`, `reassembly.rs` emitting it, `tile_delivery.rs`
   updated.
2. `boundary.rs` mirrors, `main.ts` dispatcher, `renderer.ts` prevalidation
   removed.

   Steps 1 and 2 **land together** — the contract change is not separable, and
   the browser will not build against the old shape once step 1 lands.
3. Delete the five TypeScript modules.
4. Confirm in CI.
