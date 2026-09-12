# Wasm Cutover: `main.ts` Protocol Surface Map

**Date:** 2026-09-11
**Git rev:** `dcca06e`
**Branch:** `feat/wasm-cutover`

This is the review artefact for Phase 4a of
`docs/superpowers/plans/2026-09-11-wasm-cutover-final.md` (Task 1). It records
every protocol function currently inlined in `main.ts`, its disposition once
`WasmClientCore` takes over, the real `WasmClientCore` boundary signatures
(verified against the generated `.d.ts`, not assumed), and the ten
protocol-derived `window.__*` globals the e2e suite depends on. No code
changes in this task — `main.ts` is unmodified.

`ghostframe-web-client/src/main.ts` is 1,427 lines, all inside a single
`async function main()`.

## Part 1: every protocol function, classified

```
$ grep -n '^  \(async \)\?function ' src/main.ts
291:  function onSessionReset() {
464:  function finishAssembly(asmKey: string, asm: TileAssembly) {
758:  function scanForAssemblyTimeouts(now: number, partialAssemblies: Iterable<TileAssembly>) {
818:  function flushPendingNacks() {
842:  function queuePassNack(frameSeq: number, tileX: number, tileY: number, passIdx: number): void {
856:  function tick() {
1099:  function handleSourceTileDatagram(bytes: Uint8Array) {
```

Line numbers match the plan's known set exactly — no drift since `4cd68df`.

| Function | Lines | Size | Disposition | Replacement |
|---|---|---:|---|---|
| `onSessionReset` | 291–311 | 21 | STAYS | Unchanged. Clears `feedbackInterval`, calls `renderer.onSessionReset()`, closes `fullFrameDecoder`. None of this touches protocol state — a fresh `WasmClientCore` per reconnect is a Task 2 decision, not this function's. |
| `finishAssembly` | 464–751 | 288 | SPLITS | See below — the largest and most tangled split in the file. |
| `scanForAssemblyTimeouts` | 758–769 | 12 | LEAVES | `core.onTimeout(nowUs)` — assembly-timeout NACK generation is internal to `ClientCore::on_timeout` (`ghostframe-client-core/src/lib.rs`). |
| `flushPendingNacks` | 818–841 | 24 | LEAVES | `core.onTimeout(nowUs)` — the debounce (`NACK_DEBOUNCE_US`) is reimplemented in `ClientCore` with the same 50 ms constant (`lib.rs:41-42`, comment cites `main.ts:806` by line number). |
| `queuePassNack` | 842–849 | 8 | LEAVES | Folded into `ClientCore::queue_pass_nack` (`lib.rs:246`), called internally from the Cdf53 dispatch arm in `reassembly.rs:409`. Nothing in `main.ts` calls this directly after cutover. |
| `tick` | 856–1089 | 234 | SPLITS | Per the plan: rAF loop, `renderer.encodeAndPresentFrame(...)`, and `diag.recordRafTick` STAY. The assembly-timeout scan (line 862), the tail-fallback sweep (876–903), and the stats-line's dependence on TS-only state (`__cdf53Coverage`, `__cdf53PrevalidateFails`, etc.) LEAVE or get re-derived — see Part 3. |
| `handleSourceTileDatagram` | 1099–1276 | 178 | LEAVES | `core.handleDatagram(bytes, nowUs)`. Covers dgram-header/tile-header decode, per-tile ACK (both the on-receipt path for non-Cdf53 and the deferred post-prevalidate path for Cdf53 — both present verbatim in `reassembly.rs:96-102` and `:442-451`), stale-assembly eviction, legacy fragment-level parity recovery, and the frame-dimensions-sentinel special case. |

### `finishAssembly` split in detail

This function mixes at least four concerns and is the largest single hunk
Task 2/3 will touch — larger and more tangled than `tick()`:

1. **Frame-dimensions sentinel + fallback-expand resize** (496–521) — STAYS
   as platform (canvas resize), but the *decision* of when dimensions are
   known moves into wasm's `FrameDimensions` event; `main.ts` keeps only the
   `renderer.resize(...)` call and `diag.recordResize`.
2. **Per-codec dispatch + GPU push** (566–710: `Codec.Raw/Solid/PalRle/Cdf53`
   branches, `prevalidateCdf53`, `applyCdf53Arrival`, `queuePassNack`,
   `ackBatcher.add`) — LEAVES. This is the core of what `handleDatagram`
   replaces.
3. **Diagnostic bookkeeping interleaved with #2** (`w.__tileCounts`,
   `w.__lastTileSeq`, `w.__h5_tilePushLog`, `w.__cdf53DispatchSeen`,
   `w.__cdf53PushedToQueue`, `w.__cdf53PrevalidateFails`,
   `w.__cdf53LastFailCode`, `w.__cdf53FailDumps`) — re-derived from
   `WasmEvent`s in the new `handleEvent` dispatcher. See Part 3.
4. **M3.5 bench instrumentation** (`firstRecvMs`, `paintedTilesPerFrame`,
   `diag.recordTile`) — STAYS; keyed by `frameSeqFromKey` and tile coords
   already available from the event, no wasm dependency.

### The inbound datagram loop (lines 1278–1421)

| Branch | Lines | Disposition | Notes |
|---|---|---|---|
| `TILE_PARITY_ENVELOPE` (0x04) dispatch | 1293–1306 | LEAVES | `WasmParityDecoder`/`parseParityEnvelope` exist as wasm exports; parity recovery is internal to `handle_datagram` in the new world (a recovered source datagram is just fed back in). |
| Ping/pong text datagram (`< 20` bytes) | 1308–1317 | **STAYS** | Explicitly called out in the plan: this predates the tile protocol, the core has no concept of it, and it must stay ahead of the core's `handleDatagram` call so a short non-protocol datagram doesn't reach it. |
| Length/short-datagram guards | 1319–1322 | STAYS (trivial) | Superseded in practice — `handleDatagram` internally length-checks and won't panic on garbage, but keeping a `main.ts`-side length floor before calling in is cheap and matches the plan's Task 3 sketch. |
| Full-frame (H.264) datagram assembly | 1326–1399 (`isTileDatagram` false branch) | LEAVES | The frame-level reassembly (`decodeFrameHeader`, `frameAssemblies` map, stale eviction) is protocol logic that has no h.264-specific special-casing distinguishing it from tile reassembly — the wasm crate's `Event::NeedsH264` (`boundary.rs:51-57`) is the replacement: `main.ts` keeps only the `fullFrameDecoder.decode(payload, is_keyframe)` call, i.e. `FullFrameDecoder` (the part of `decoder.ts` the bundle-baseline doc already flags as platform glue, not deleted in 4b). |
| Tile-level datagram — `parityDecoder.recordSource` + `handleSourceTileDatagram` | 1401–1420 | LEAVES | Folds into `core.handleDatagram(bytes, nowUs)`. |

## Part 2: the boundary

Verified against `pkg-node/ghostframe_client_wasm.d.ts`, freshly rebuilt via
`npm run build:wasm:node` (not stale — output matched the pre-existing file
byte for byte).

```ts
export class WasmClientCore {
    constructor(indices_raw_enabled: boolean, supports_h264: boolean, tile_delivery_payload: boolean, now_us: bigint);
    handleDatagram(bytes: Uint8Array, now_us: bigint): any;   // -> WasmEvent[]
    onTimeout(now_us: bigint): any;                            // -> WasmEvent[]
    pollTransmit(now_us: bigint): any;                         // -> {kind:'Datagram'|'Stream', bytes:Uint8Array} | undefined
    pollTimeout(): bigint | undefined;
    encodeFeedback(now_us: bigint): Uint8Array;
}
```

Every `now_us` parameter is `bigint`, confirmed for all five methods and the
constructor — passing a `number` throws at the wasm-bindgen boundary at
runtime (there is no compile-time coercion). `nowUs()` must build a `bigint`,
e.g. `BigInt(Math.round(performance.now() * 1000))`.

`handleDatagram` and `onTimeout` return `any` in the `.d.ts` because
`serde_wasm_bindgen` erases the type, but the Rust source
(`ghostframe-client-wasm/src/boundary.rs`) pins the real shape — a
`#[serde(tag = "kind")]` enum, so JS sees plain tagged objects:

```ts
type WasmEvent =
  | { kind: 'TileReady';    frame_seq: number; tile_x: number; tile_y: number; rgba: Uint8Array }
  | { kind: 'TilePayload';  frame_seq: number; tile_x: number; tile_y: number; pass_idx: number; generation: number; codec: number; payload: Uint8Array }
  | { kind: 'PaletteUpdated'; palette_id: number; colors: [number,number,number,number][] }
  | { kind: 'FrameDimensions'; width: number; height: number }
  | { kind: 'NeedsH264';    frame_seq: number; timestamp_us: number; is_keyframe: boolean; payload: Uint8Array }
  | { kind: 'DecodeError';  codec: number; tile_x: number; tile_y: number; code: number };
```

`codec` crosses as the `Codec` `repr(u8)` discriminant
(`ghostframe-protocol/src/protocol.rs:58-65`: `Skip=0, H264=1, PalRle=2,
Solid=3, Raw=4, Cdf53=5` — identical to the TS `Codec` enum in
`decoder.ts:17`). `code` on `DecodeError` is `DecodeErrorCode`
(`ghostframe-client-core/src/event.rs:13-24`), 1..=10; the CDF53-specific
codes are `Cdf53BadPass=8`, `Cdf53Truncated=9`, `Cdf53RleLength=10`,
confirming the plan's claim exactly.

Outbound:

```ts
type WasmPollOutput =
  | { kind: 'Datagram'; bytes: Uint8Array }
  | { kind: 'Stream';   bytes: Uint8Array };
```

`pollTransmit(nowUs)` must be drained in a loop until it returns
`undefined`; `Datagram` goes to `transport.datagrams.writable`, `Stream`
goes to the bidi feedback stream writer. **They must not be conflated** —
writing a `Stream` buffer to the datagram writer (or vice versa) would be a
silent wire-format corruption, not a thrown error, since both are plain
`Uint8Array`s at that point.

`prevalidateCdf53(payload, generation, pass_idx)` and
`applyCdf53Arrival(...)` are also exported as standalone free functions
(`.d.ts:144, 226`) — see Part 4 for why that matters.

## Part 3: the ten at-risk globals

All ten are defined in `main.ts`; confirmed no others exist there by
cross-referencing every `__`-prefixed identifier `main.ts` assigns
(`(window as any).__foo =` and `w.__foo =`, `w` being `window as any`)
against every `__`-prefixed identifier `ghostframe-e2e/tests/e2e.rs`
evaluates. The count is exactly 21 total (10 `main.ts` + 11
`diagnostics.ts`/`webgpu/`), matching the plan's "21 globals" claim.

| Global | Currently set by | Post-cutover source | e2e tests that read it | Gating? |
|---|---|---|---|---|
| `__cdf53PrevalidateFails` | `finishAssembly`, incremented on `prevalidateCdf53` failure (main.ts:633) | Count of `DecodeError` events with `codec === 5` (Cdf53) | `e2e_cdf53_tile_watcher` | **Diagnostic only** — read into an `eprintln!`-only `dispatch_stats` object, never asserted (see Part 4). |
| `__cdf53LastFailCode` | Same site, `r.errorCode` (main.ts:634) | `.code` of the most recent such `DecodeError` | `e2e_cdf53_tile_watcher` | Diagnostic only, same block. |
| `__cdf53DispatchSeen` | `finishAssembly`, incremented unconditionally on every `Codec.Cdf53` tile arrival, **before** prevalidation runs (main.ts:580) | **Not a single event.** Must be `TilePayload{codec:5}` count + `DecodeError{codec:5}` count — see Part 4, this is a real correction, not a relabeling. | `e2e_cdf53_tile_watcher` | Diagnostic only. |
| `__cdf53PushedToQueue` | `finishAssembly`, incremented only on prevalidation success, right before `renderer.pushCdf53` (main.ts:694) | Count of `TilePayload` events with `codec === 5` | `e2e_cdf53_tile_watcher` | Diagnostic only. |
| `__cdf53Probe` | Reads `renderer.cdf53Pipeline`/`renderer.cdf53Queue` state directly (main.ts:228-251) | **GPU-side, not protocol.** No wasm dependency — untouched by the cutover. | `e2e_cdf53_tile_watcher` | Diagnostic only. |
| `__cdf53DumpTileState` | Reads back GPU buffers (`coefficientBuffer`/`signBuffer`/`tileGenBuffer`) via staging buffers (main.ts:183-214) | **GPU-side, not protocol.** Untouched. | `e2e_cdf53_live_tile_state`, `e2e_cdf53_live_tile_state_col18` | Neither gates: `e2e_cdf53_live_tile_state` explicitly comments "Don't assert — this is a diagnostic. Always succeed so we get the eprintln output"; `e2e_cdf53_live_tile_state_col18` is `#[ignore]`'d (`"diagnostic-only: run on demand with --ignored"`). |
| `__cdf53GetTileWatcher` | Reads `renderer.cdf53Pipeline.tileWatcherCaptures`/stats (main.ts:252-272) | **GPU-side, not protocol.** Untouched. | `e2e_cdf53_tile_watcher` | **Hard-gated.** `captures.is_empty()`, per-pass-idx coverage (`n >= 1` for all 14), and `total_mismatches == 0` are real `assert!`/`assert_eq!` calls. |
| `__cdf53TestIntegrate` | Test-only hook: hand-builds RLE passes, calls **TS** `prevalidateCdf53` (from `prevalidate_cdf53.ts`) to validate them, then drives `pipe.uploadBatch` + `pipe.encodeIntegrate` directly (main.ts:131-178) | **Mostly GPU-side** (writes/reads GPU buffers directly, bypassing the wire entirely) **but has a live dependency on the TS prevalidate function that Phase 4b deletes.** Must be repointed at the wasm free function `prevalidateCdf53(payload, generation, pass_idx)` (`.d.ts:226`), not left calling into an orphaned module. | `e2e_cdf53_integrate_correctness` | **Hard-gated** (`assert_eq!` on returned coefficient/sign arrays and mismatch counts). |
| `__cdf53TestInverse` | Test-only hook: writes hand-supplied coefficients straight into GPU buffers, runs the inverse shader, reads back pixels (main.ts:83-124) | **Pure GPU-side**, no protocol dependency at all — no TS protocol function is called anywhere in this hook. Untouched. | `e2e_cdf53_bypass_integrate`, `e2e_cdf53_inverse_gradient_tile` | **Hard-gated** in both (`assert_eq!(got_rgba.len(), ...)` plus a pixel-match `assert!`). |
| `__h5_tilePushLog` | `finishAssembly`, one entry per tile of any codec, on every arrival (main.ts:542-565) | From `TilePayload`/`DecodeError` events. (`TileReady` does not occur under `Payload` delivery — see Part 4.) | `e2e_palette_eviction_chromium`, `e2e_palette_eviction_firefox` (via `e2e_palette_eviction_body`) | **Diagnostic only** — the entire read is inside `if e2e_diag_enabled()`, output goes to `e2e_diag!`/`eprintln!`, gated behind `GHOSTFRAME_E2E_DIAG`. No `assert` touches it. |

**Net count: 3 of the ten are unambiguously GPU-side** (`__cdf53Probe`,
`__cdf53DumpTileState`, `__cdf53GetTileWatcher`) and need no wiring at all —
this is the good news the plan asked to surface, and it shrinks the risk
surface by 30%. Two more (`__cdf53TestIntegrate`, `__cdf53TestInverse`) are
*also* GPU-side test harnesses, not part of the live receive path, but one of
them (`__cdf53TestIntegrate`) has a real dependency on TS protocol code that
Phase 4b deletes — flagged above and in Part 4.

Only 5 of the ten are genuinely protocol-derived counters that need a
`handleEvent`-based rewrite: `__cdf53PrevalidateFails`, `__cdf53LastFailCode`,
`__cdf53DispatchSeen`, `__cdf53PushedToQueue`, `__h5_tilePushLog`.

**On the plan's stated failure mode** ("several e2e tests assert exactly
that a counter is zero"): as of this commit, **no test in `e2e.rs` asserts a
numeric value against any of the ten globals** except the two GPU-side test
hooks (`__cdf53TestIntegrate`, `__cdf53TestInverse`) and `__cdf53GetTileWatcher`,
none of which are "is it zero" checks — they assert non-empty/non-mismatched
data. The four dispatch-branch counters and `__h5_tilePushLog` are read only
for human-facing diagnostic dumps today. This doesn't remove the risk the
plan is guarding against — a silently-dead diagnostic is exactly the kind of
thing an on-call engineer reaches for mid-incident and would not want lying
to them — but it does mean Task 4's "prove each moves" step is not, today,
blocking a hard CI assertion for those five; it is protecting the *next*
person who adds one, or who trusts the diagnostic output during a live
regression.

## Part 4: surprises

1. **`TileReady` under `Payload` delivery — corrected.**
   An earlier draft of this document claimed `Raw` and `Solid` still emit
   `TileReady` in `Payload` mode, on the grounds that their match arms
   (`reassembly.rs:270`, `:288`) carry no `tile_delivery` guard. That reading
   missed the **early return above the match**:

   ```rust
   // reassembly.rs:254
   if self.tile_delivery == TileDelivery::Payload
       && matches!(asm.codec, Codec::Raw | Codec::Solid)
   {
       events.push(Event::TilePayload { /* ... */ payload });
       return;
   }
   ```

   Those match arms are reachable only in `Decoded` mode. Under `Payload`,
   `Raw` and `Solid` emit `TilePayload` carrying the **original wire bytes,
   unswizzled** — not a pre-converted RGBA buffer.

   `ghostframe-client-core/tests/tile_delivery.rs` asserts this directly:

   ```rust
   assert!(!events.iter().any(|e| matches!(e, Event::TileReady { .. })),
           "Payload mode must not emit TileReady");
   ```

   and `raw_in_payload_mode_passes_bytes_through` asserts
   `payload == bgra` — "payload must be the wire bytes, unswizzled".

   **So the plan's dispatcher sketch was right**, and the GPU-side contract
   for `Raw`/`Solid` does *not* change: `renderer.pushRaw`/`pushSolid` keep
   receiving the same wire payload they receive today. `TileReady` should not
   occur in the browser, and a dispatcher that treats it as unexpected is
   correct.

   Recorded rather than silently deleted because the mis-reading is an easy
   one to repeat: the guard clause sits 16 lines above the arms it governs.

2. **`__cdf53DispatchSeen`'s source is two events summed, not one.**
   The plan's own risk table (`## The 21 globals`) lists its post-cutover
   source as "tiles dispatched to the GPU" as if a single event carries
   that count. In the current code it increments on *every* Cdf53-coded
   tile arrival, before `prevalidateCdf53` runs (main.ts:577-580). Post-cutover,
   the wasm core never emits an "arrived, not yet validated" event for
   Cdf53 — only `TilePayload` (success, after validation) or `DecodeError`
   (failure, after validation) exist (`reassembly.rs:412-467`). So the
   equivalent quantity is `TilePayload{codec:5}.count + DecodeError{codec:5}.count`,
   i.e. `__cdf53PushedToQueue + __cdf53PrevalidateFails`, computed from two
   event kinds. Worth getting right in Task 4's step 1 since it's the one
   global among the ten whose formula, not just its source, changes.

3. **`prevalidateCdf53` is exported as a standalone wasm free function**
   (`.d.ts:226`, `export function prevalidateCdf53(payload, generation,
   pass_idx): any`). This resolves what looked like a dead end for
   `__cdf53TestIntegrate`: that hook calls the *TS* `prevalidateCdf53` from
   `prevalidate_cdf53.ts`, a module Phase 4b deletes outright (Task 6's
   list). Rather than needing new plumbing, Task 2/3/4 can just repoint this
   one call site at the wasm export — same argument order, confirmed by the
   `.d.ts` signature.

4. **An eleventh `main.ts`-defined `window.__*` global exists and is
   invisible to the e2e suite: `__cdf53SetTileWatcher`** (main.ts:221-223).
   It's a thin wrapper around `renderer.cdf53Pipeline.setTileWatcher(x, y)`.
   A repo-wide grep (`grep -rn '__cdf53SetTileWatcher'`) finds exactly one
   hit — its own definition. Nothing calls it, including `main.ts` itself:
   the `cdf53watch` URL-param path (main.ts:71-78) calls
   `renderer.cdf53Pipeline.setTileWatcher(wx, wy)` directly, bypassing this
   global entirely. It doesn't affect the "21 globals" count (confirmed:
   exactly 21 distinct `__`-identifiers appear in `e2e.rs`), but Task 4
   should not spend effort "proving it moves" — there is nothing observing
   it to prove anything to.

5. **The reliability/timer machinery (NACK debounce, tail-fallback sweep,
   assembly-timeout scan) is a byte-for-byte port, not a re-architecture.**
   `ghostframe-client-core/src/lib.rs:41-48` defines
   `NACK_DEBOUNCE_US`/`TAIL_FALLBACK_US`/`TAIL_SWEEP_INTERVAL_US` with doc
   comments that cite the exact `main.ts` line numbers they were ported
   from (`main.ts:806`, `:807`, `:808`, `:855-901`). This is strong
   evidence the Rust side was written *against* this exact `main.ts`, which
   is reassuring for Task 2/3 but also means any local edit to those
   constants in `main.ts` before cutover (there is no reason to make one)
   would silently diverge from what the wasm crate already assumes.

6. **No dead protocol logic found.** Every function in the known set is
   live and reachable; `flushPendingNacks`/`queuePassNack` are called from
   the live NACK path, not orphaned. Nothing in `main.ts` duplicates logic
   that has already been superseded elsewhere in the file.

## Constraints observed

- No code changed. `main.ts` is untouched.
- All line numbers and signatures in this document were re-derived from the
  current tree (`dcca06e` on `feat/wasm-cutover`), not assumed from the
  plan's `4cd68df` reference numbers — they happened to match exactly, with
  zero drift.
- `pkg-node/ghostframe_client_wasm.d.ts` was rebuilt via
  `npm run build:wasm:node` before being read, per the plan's instruction to
  verify rather than trust.

## Known inefficiency: CDF53 is prevalidated twice

Under `TileDelivery::Payload`, `reassembly.rs:392` calls `prevalidate_cdf53`
to drive coverage bookkeeping, the NACK decision and the deferred ACK — then
emits `TilePayload` carrying the **raw wire payload**, discarding the
prevalidated `bit_planes` it just computed (`reassembly.rs:424-437`).

`renderer.pushCdf53` needs those bit planes, so `main.ts` must call the
standalone `prevalidateCdf53` export and RLE-decode the same 3 × 128-byte
planes a second time, per pass, per tile.

**This plan accepts the double decode.** The alternative — having
`TilePayload` carry `bit_planes` for `Cdf53` — changes what the `payload`
field means per codec, and redesigning the event contract during the one
irreversible step of this migration is the wrong trade. The standalone export
is already proven: `prevalidate_cdf53.test.ts` was retargeted at it and every
assertion held.

It is a performance question, not a correctness one, and it is bounded: the
work is an RLE expansion of 384 bytes, in wasm, on tiles that actually receive
CDF53 passes. Worth measuring before optimising — and worth measuring
alongside the bundle regression already recorded in
`wasm-bundle-baseline.md`, since both land in the same step.
