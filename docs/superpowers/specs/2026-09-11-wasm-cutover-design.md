# Wasm Cutover — Design

**Date:** 2026-09-11
**Status:** Approved design
**Author:** Claude (design synthesis); review by Cedric
**Amends:** the architecture section of `2026-07-01-client-core-rearchitecture-design.md`

## Problem

The client half of the protocol is hand-mirrored in TypeScript — roughly 1147
lines across `decoder.ts`, `ack.ts`, `nack.ts`, `fec.ts`, `parity_decoder.ts`,
`prevalidate.ts`, `prevalidate_cdf53.ts`, `cdf53_coverage.ts`,
`palette_shadow.ts`, `decode_error_batcher.ts`, `feedback.ts` — duplicating
`ghostframe-client-core`. Drift between the two is a recurring source of
loss and corruption bugs.

Sub-projects 1 and 2 built the Rust core and proved it end to end. This
sub-project deletes the duplicate.

## What the umbrella spec got wrong

`2026-07-01-client-core-rearchitecture-design.md` assigns to the core:

> Software decode to RGBA: CDF53 (14 progressive passes), PalRLE, Solid, Raw

and says of the browser:

> All TS protocol/decode modules are deleted at cutover.

**The browser does not decode on the CPU.** `src/webgpu/` is 1409 lines of
TypeScript plus ten WGSL compute shaders — `cdf53_inverse_l1/l2/l3`,
`cdf53_integrate`, `palrle_decode`, `h264_blit` and others. The renderer takes
*encoded payloads* (`renderer.pushPalRle({ tileX, tileY, payload })`) and
decodes them on the GPU, accumulating CDF53's progressive passes in GPU
buffers.

Read literally, the umbrella spec replaces a parallel GPU decoder with a
single-threaded wasm CPU decoder in a 60 fps remote-desktop renderer, and
discards ten working shaders. That is not the intent.

**Amendment: the GPU remains the browser's decode path.** The core's
decode-to-RGBA serves the native and headless consumers, and remains the
reference the shaders are checked against.

## Goals

- One Rust implementation of the client *protocol*, shared by browser (wasm),
  native client, and headless tests.
- The web client keeps only platform-bound code: WebTransport I/O, WebGPU
  decode and render, WebCodecs H.264, input capture.
- No dual-maintenance period: the TS protocol modules are deleted at cutover.

## Non-goals

- Replacing, porting, or touching `src/webgpu/` or any WGSL shader.
- CPU decode in the browser.
- Compiling `ghostframe-client-net` into the browser bundle. The browser gets
  datagrams from the platform's own WebTransport API and never runs quinn;
  client-net is the *native* path. (A comment added during the netsim work
  claims otherwise and should be corrected when that file is next touched.)
- A tile atlas / zero-copy pixel path. No RGBA crosses the browser boundary,
  so it solves nothing here. It remains worth doing for the native client.

## Decisions

| # | Decision | Rationale |
|---|---|---|
| D1 | wasm owns protocol only; GPU keeps decode | Removes the duplication that causes drift without trading a parallel decoder for a serial one. |
| D2 | New crate `ghostframe-client-wasm` | `client-core` is consumed by `ghostframe-lib` and `ghostframe-e2e` on native targets; neither should acquire `wasm-bindgen` in its graph. |
| D3 | `ClientConfig.tile_delivery: TileDelivery::{Decoded, Payload}` | Browser takes `Payload`; native and e2e keep `Decoded`. One core, two consumers, no forked behaviour. |
| D4 | Retarget the existing vitest suites at wasm *before* deleting the TS | They encode the old implementation's real behaviour, so they can detect divergence that tests written alongside the new code cannot. |
| D5 | Consolidate the eleven `npm run build` sites into a composite action first | The toolchain gets added once instead of eleven times; a missed site would ship a `dist/` with no wasm. |
| D6 | Wasm built in CI, not committed | `dist/` is already a build artefact; a committed binary can drift from source silently. |

## Architecture

```
  WebTransport (browser API)  ──►  JS  ──►  wasm: ghostframe-client-wasm
                                     ▲                    │
                                     │                    ▼
                                     │         ghostframe-client-core
                                     │           reassembly, FEC/parity,
                                     │           prevalidate, ACK/NACK,
                                     │           feedback, palette shadow
                                     │                    │
                                     │     TilePayload / PaletteUpdated /
                                     │     NeedsH264 / FrameDimensions
                                     │                    │
                                     ▼                    ▼
                        poll_transmit bytes      src/webgpu/  (UNCHANGED)
                        back to the wire          solid · palrle · cdf53
                                                  shaders decode on GPU
```

### The payload event

```rust
TilePayload {
    frame_seq: u32,
    tile_x: u8,
    tile_y: u8,
    pass_idx: u8,
    generation: u8,
    codec: Codec,
    payload: Vec<u8>,
}
```

Reassembled across fragments, parity-recovered, prevalidated and
generation-checked — everything the GPU should not have to reason about —
but not decoded.

### Palette

`palrle_decode.wgsl` needs the palette table, and the palette shadow is
protocol state that moves into wasm. The boundary therefore also carries
`PaletteUpdated { palette_id, colors }`. Without it the wasm would own the
palette and the shader could not see it — which surfaces as wrong colours
rather than an error.

## Failure handling

A Rust panic in wasm aborts the module: every later call traps, so one
malformed datagram would end the session rather than drop a frame.

- Every wrapper export returns a status; no `unwrap` on wire-derived data.
- The core's existing `Result<_, DecodeErrorCode>` returns and decode-error
  batcher are preserved across the boundary as structured events.
- `console_error_panic_hook` installed so a panic that does escape is legible
  rather than a silent trap.

The netsim corruption fuzzing already exercises this on the Rust side; the
wrapper is the layer that can undo the property, so that is where the care
goes.

## Testing

**Equivalence, before deletion.** The vitest suites are retargeted at the wasm
exports and made to pass **while the TS still exists**. Their subjects and the
tests are then deleted together.

The suites split three ways:

- *Retarget then delete*: `ack`, `nack`, `cdf53_coverage`,
  `decode_error_batcher`, `feedback`, `palette_shadow`, `parity_decoder`,
  `prevalidate`, `prevalidate_cdf53`, `tile_key`, `solid_pack`.
- *Untouched*: `bootstrap`, `diagnostics`, `renderer_idle_skip`, `sanity` —
  their subjects survive the cutover.
- *Split or reconsider*: `input` (capture is platform and stays; encoding is
  `client-core::input` and moves) and `lossless_golden` (not protocol at all —
  a TS mirror of `ghostframe-test-pattern`'s generator, asserted byte-equal to
  Rust; deleting it by exporting the generator from wasm is adjacent scope,
  noted rather than absorbed).

**Impedance mismatch, expected.** The TS suites are callback-and-fake-timer
shaped (`new NackBatcher(buf => sent.push(buf))`, `vi.useFakeTimers()`); the
Rust API is poll-and-injected-time (`add(entry, now_us) -> Option<Vec<u8>>`).
Each retargeted suite needs a shim translating between them. This is real work
per suite, and it forces time to become explicit, which is an improvement.

**Acceptance.** `chromium_smoke`, `firefox_smoke` and the containerised `e2e`
suite. A green smoke run alone is not sufficient evidence — it is one path
through a large surface — which is why the equivalence check comes first.

**Staleness guard.** `dist/` is embedded by ghostbridge via `//go:embed` at
compile time. A wasm built from an older `client-core` compiles, passes CI and
silently serves yesterday's protocol. The wasm build emits a version stamp
derived from the crate source, and a test asserts the loaded module's stamp
matches the workspace — turning a silent mismatch into a failure.

## Sequencing

Four landable pieces; only the last is irreversible.

1. `_build-web-client` composite action consolidating the eleven `npm run
   build` sites. Pure refactor, independently verifiable.
2. `TileDelivery::Payload`, `TilePayload` and `PaletteUpdated` in
   `client-core`, with Rust tests. No browser involvement; `Decoded`
   consumers unaffected.
3. `ghostframe-client-wasm` plus the retargeted vitest suites. Both
   implementations live.
4. Cutover: `main.ts` rewired, TS protocol modules and tests deleted,
   toolchain added to the composite action.

## Risks

| Risk | Mitigation |
|---|---|
| Retargeted suites disagree with Rust | That is the point of D4 — it is the step most likely to expand, and the divergence is worth finding before the TS is gone. Budget debugging, not a clean port. |
| A missed build site ships a `dist/` with no wasm | D5 consolidates to one site first. |
| `dist/` wasm drifts from source | Version stamp asserted by a test. |
| Wasm panic ends the session | Status-returning exports, no `unwrap` on wire data, panic hook. |
| Bundle size or load time regresses | Measure at step 3, while both implementations are live and comparison is free. |
