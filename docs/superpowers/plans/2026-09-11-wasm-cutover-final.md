# Wasm Cutover Step 4 — The Cutover Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `main.ts` drive `WasmClientCore` instead of its inline TypeScript protocol, then delete the TypeScript protocol layer.

**Architecture:** `main.ts` keeps everything platform-bound — WebTransport sockets, WebGPU rendering, WebCodecs H.264, input capture, diagnostics — and hands every inbound datagram to `WasmClientCore`, rendering the events it returns and writing back whatever `pollTransmit` yields. The protocol state machine leaves the browser bundle entirely.

**Tech Stack:** TypeScript 5.5, vite 6, vitest 2, `wasm-bindgen`, `wasm-pack` 0.15.0, Rust.

---

## The sequencing change, and why

The spec treats step 4 as one irreversible step. **Split it in two.**

- **4a — rewire.** `main.ts` drives `WasmClientCore`. The TypeScript protocol modules stay on disk, orphaned but present. All the risk lives here, and it is *reversible*: if the browser e2e suite finds a problem, the old implementation is still there to diff against and to fall back to.
- **4b — delete.** Remove the now-unreferenced TS modules and the retargeted suites. Mechanical, and only after 4a is green and merged.

The reason is the acceptance gate's shape. The e2e suite is **54 browser tests across Chromium and Firefox** with pixel-level assertions, and it runs in CI — iterating on a failure costs a full CI round trip. Deleting 1,147 lines of reference implementation *before* that gate has ever run against the new wiring throws away the thing you would most want during that iteration.

Nothing is lost by splitting: after 4a the TS is dead code for one PR's lifetime.

---

## Local verification is blocked — read before starting

The e2e suite is the acceptance gate, and **it cannot currently run on this machine.**

`tests/containers/test-server/Dockerfile` COPYs `ghostframe-web-client/dist/`, which ghostbridge `//go:embed`s at compile time. A web-client change therefore requires rebuilding the image. That rebuild consumed ~16 GB and died with a Docker storage-layer corruption at ~2 GB free; `docker system prune -f` recovered only ~2.5 GB, and the remaining 15.65 GB is held by another project's images (`jcore-ci-vsg`, `jcore-ci-bc`, `jcore-cpu-ci`) which are **not ours to delete**. BuildKit is also absent, so every `RUN` commits a full layer.

VKMS is loaded and `/dev/dri` is populated, so the GPU side is fine. The blocker is purely disk.

**Consequences for this plan:**
- `cargo test`, `npm test` and both wasm builds run locally and gate most of the work.
- The browser e2e suite runs **in CI only**. Do not claim it passes locally; do not mark 4a done on a green `npm test` alone.
- Before starting, ask whether space can be freed. If it can, local e2e turns a CI round trip into a minute and is worth the detour.

---

## The 21 globals — the sharpest risk in this plan

The e2e suite's only view into the page is 21 `window.__*` globals. Ten of them are defined in `main.ts` and are derived from the protocol state that is moving into wasm:

| Global | Source of truth after the cutover |
|---|---|
| `__cdf53PrevalidateFails` | count of `DecodeError` events with a CDF53 code |
| `__cdf53LastFailCode` | `code` of the most recent such event |
| `__cdf53DispatchSeen` | tiles dispatched to the GPU |
| `__cdf53PushedToQueue` | payload events enqueued for decode |
| `__cdf53Probe` | per-tile probe state |
| `__cdf53DumpTileState` | per-tile state dump |
| `__cdf53GetTileWatcher` | tile watcher accessor |
| `__cdf53TestIntegrate` | GPU integrate hook |
| `__cdf53TestInverse` | GPU inverse hook |
| `__h5_tilePushLog` | tile push log |

The other eleven live in `diagnostics.ts` and `webgpu/` — files this plan does not touch — and survive unchanged. `__h2_clientPaletteWrites` in particular lives in `webgpu/palrle.ts` and stays.

**The failure mode to design against:** a global left wired to nothing does not fail loudly. `__cdf53PrevalidateFails === 0` reads as "no prevalidation failures" when it actually means "not connected". Several e2e tests assert exactly that a counter is zero. A silently-dead global turns those tests green while proving nothing.

Task 4 exists solely to prove each of the ten is *live*, by making it move.

---

## Phase 4a — rewire

### Task 1: Map the protocol surface out of `main.ts`

**Files:** none modified — this task produces a document.

`main.ts` is a single 1,427-line `main()` with protocol logic inlined. Before changing it, establish exactly which parts leave.

- [ ] **Step 1: Identify every protocol function**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client/src
grep -n '^  \(async \)\?function ' main.ts
```

Known set (line numbers from master at `4cd68df`, expect drift):
`onSessionReset` (291), `finishAssembly` (464), `scanForAssemblyTimeouts` (758), `flushPendingNacks` (818), `queuePassNack` (842), `tick` (856, *partly* platform), `handleSourceTileDatagram` (1099).

- [ ] **Step 2: Classify every one of them**

For each, record: does it leave (protocol), stay (platform), or split? `tick()` splits — the rAF loop and rendering stay, the assembly-timeout scan and tail sweep leave.

- [ ] **Step 3: Map each to its `WasmClientCore` replacement**

The boundary is small:
- inbound datagram → `core.handleDatagram(bytes, nowUs)` → `WasmEvent[]`
- timers → `core.onTimeout(nowUs)` → `WasmEvent[]`
- outbound → `core.pollTransmit(nowUs)` until `undefined`, each `{kind: 'Datagram'|'Stream', bytes}` routed to the datagram writer or the bidi stream writer **— they must not be conflated**
- `core.pollTimeout()` → next deadline
- `core.encodeFeedback(nowUs)` → feedback stream

- [ ] **Step 4: Write it down**

Save as `docs/specs/wasm-cutover-main-ts-map.md`: a table of every protocol function, its disposition, and its replacement. Include the ten at-risk globals and where each will now get its value.

This document is the review artefact for the rewiring. A reviewer should be able to check the diff against it without re-deriving the mapping.

- [ ] **Step 5: Commit**

```bash
git add docs/specs/wasm-cutover-main-ts-map.md
git commit -m "docs: map main.ts's protocol surface onto WasmClientCore"
```

### Task 2: Construct the core and route outbound traffic

**Files:** Modify `ghostframe-web-client/src/main.ts`

- [ ] **Step 1: Import and construct**

`--target web` exports a default async `init()`. It **must** be awaited inside an async function — a top-level `await` fails the vite build with *"Top-level await is not available in the configured target environment"*. `main()` is already async, so await it there.

```ts
import init, { WasmClientCore } from '../pkg-web/ghostframe_client_wasm.js';
// inside main(), before the transport is used:
await init();
const core = new WasmClientCore(indicesRawEnabled, supportsH264, /* payload delivery */ true, nowUs());
```

`tile_delivery_payload = true` is what gives the browser validated-but-undecoded payloads for the GPU. Passing `false` would silently route it down the native `Decoded` path and hand back RGBA — wrong, and it would look like it worked.

- [ ] **Step 2: Add a microsecond clock**

Every `now_us` parameter is a **`bigint`**. Passing a `number` throws at runtime; `tsc` catches it now that `tests/` and `src/` are both typechecked.

```ts
const nowUs = (): bigint => BigInt(Math.round(performance.now() * 1000));
```

- [ ] **Step 3: Drain `pollTransmit` to the right wire**

```ts
async function drainTransmit(): Promise<void> {
  for (;;) {
    const out = core.pollTransmit(nowUs());
    if (out === undefined) break;
    if (out.kind === 'Datagram') await datagramWriter.write(out.bytes);
    else await feedbackWriter.write(out.bytes);
  }
}
```

`out.bytes` is a `Uint8Array` — confirmed after the `serde_bytes` fix in step 3b. If it ever arrives as a plain `Array`, a byte-buffer field lost its `#[serde(with = "serde_bytes")]` annotation; fix that rather than converting here.

Call `drainTransmit()` after every `handleDatagram` and every `onTimeout`.

- [ ] **Step 4: Verify the Hello still goes out**

The core queues a Hello on construction. Confirm the first `pollTransmit` yields `{kind: 'Stream', bytes: [0x03, caps]}` and that it reaches the server — a session whose Hello never arrives will fail in ways that look like codec bugs.

- [ ] **Step 5: Build and commit**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client
export PATH="$HOME/.cargo/bin:$PATH"
npm run build 2>&1 | tail -6
```

Expected: `tsc` clean, vite emits a hashed `.wasm` asset.

### Task 3: Route inbound datagrams and render the events

**Files:** Modify `ghostframe-web-client/src/main.ts`

- [ ] **Step 1: Replace the receive loop's protocol branches**

The existing loop at ~1278 hand-routes parity envelopes, ping/pong text, and tile datagrams. The wasm core handles parity and tiles internally. Keep only what is genuinely platform:

```ts
const { value, done } = await reader.read();
if (done) break;
if (!value || value.byteLength === 0) continue;

// Backward compat: small text datagrams (ping/pong). Not protocol —
// the core would reject them.
if (value.byteLength < 20) { /* unchanged */ continue; }

for (const ev of core.handleDatagram(value, nowUs())) handleEvent(ev);
await drainTransmit();
```

**Keep the ping/pong branch ahead of the core.** It predates the tile protocol and the core has no concept of it.

- [ ] **Step 2: Write the event dispatcher**

```ts
function handleEvent(ev: WasmEvent): void {
  switch (ev.kind) {
    case 'TilePayload':    /* → GPU decode path, by ev.codec */ break;
    case 'PaletteUpdated': /* → palette table upload */ break;
    case 'FrameDimensions':/* → canvas resize */ break;
    case 'NeedsH264':      /* → WebCodecs VideoDecoder */ break;
    case 'DecodeError':    /* → counters, see Task 4 */ break;
    case 'TileReady':      /* should not occur under Payload delivery */ break;
  }
}
```

`PaletteUpdated` is not optional. `palrle_decode.wgsl` needs the palette table, and the shadow now lives in wasm. Without it the shader renders **wrong colours rather than an error** — the worst kind of failure to debug.

- [ ] **Step 3: Drive the timers**

Replace `scanForAssemblyTimeouts` and the tail sweep inside `tick()` with:

```ts
for (const ev of core.onTimeout(nowUs())) handleEvent(ev);
await drainTransmit();
```

Keep the rAF loop, the renderer call, and `diag.recordRafTick`.

- [ ] **Step 4: Build**

```bash
npm run build 2>&1 | tail -6
```

### Task 4: Re-wire the ten at-risk globals, and prove each is live

**Files:** Modify `ghostframe-web-client/src/main.ts`

This task is the reason the cutover can be trusted. Read the "21 globals" section above first.

- [ ] **Step 1: Wire each of the ten to the wasm event stream**

Use the table in that section. `__cdf53PrevalidateFails` and `__cdf53LastFailCode` come from `DecodeError` events whose `code` is 8, 9 or 10 (`Cdf53BadPass`, `Cdf53Truncated`, `Cdf53RleLength`).

- [ ] **Step 2: Prove each one moves**

**A global that never changes is indistinguishable from a global wired to nothing**, and several e2e tests assert a counter is zero. For each of the ten, demonstrate it takes at least two distinct values.

Add a temporary page-level harness, or drive it from Node against `pkg-node`, feeding: a valid tile, a malformed CDF53 payload, a palette update. Record each global before and after.

**Report a table of before/after values for all ten.** Any that cannot be made to move is either dead or untestable — say which, and do not mark this task done by asserting it is fine.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-web-client/src/main.ts
git commit -m "feat(web): drive WasmClientCore from main.ts

The protocol state machine leaves the browser bundle; main.ts keeps
transport, rendering, WebCodecs and input. The ten protocol-derived
test globals are re-derived from the wasm event stream, each verified
to take more than one value."
```

### Task 5: CI e2e is the gate

**Files:** none.

- [ ] **Step 1: Push and open the 4a PR**

Title it so it is obvious the TS is still present: `wasm cutover 4a: main.ts drives WasmClientCore (TS retained)`.

- [ ] **Step 2: Wait for the browser e2e suite**

54 tests, Chromium and Firefox, pixel-level assertions. **This is the gate.** Do not merge on a green `npm test`.

- [ ] **Step 3: When something fails, diff against the reference**

The TS implementation is still on disk and still correct. For a failing test, compare the two paths at the same point rather than guessing. This is the entire reason 4a and 4b are separate.

- [ ] **Step 4: Do not weaken an e2e test to make it pass**

Same rule that governed step 3b. A pixel assertion that fails is reporting a real rendering difference.

---

## Phase 4b — delete

**Only after 4a is merged and CI is green.**

### Task 6: Delete the TypeScript protocol layer — seven of ten modules

**Scope narrowed after a finding in Task 3.** `src/webgpu/` — an explicit spec
non-goal — imports three of the ten modules, two of them as **runtime values**:

```
webgpu/renderer.ts:7   import { PaletteShadow } from '../palette_shadow.js';
webgpu/renderer.ts:8   import { prevalidatePalRle, PalRleVariant, type PalRleEntry } from '../prevalidate.js';
webgpu/renderer.ts:9   import type { PrevalidatedCdf53 } from '../prevalidate_cdf53.js';
webgpu/cdf53.ts:1      import type { PrevalidatedCdf53 } from '../prevalidate_cdf53.js';
webgpu/palrle.ts:2     import type { PalRleEntry } from '../prevalidate.js';
```

The renderer keeps its **own** `paletteShadow` (`renderer.ts:63`) and
prevalidates PalRLE itself at drain time (`:294`), applying bundled upserts
before thin entries in the same rAF (`:303`). Deleting those modules would
require editing `src/webgpu/`.

**Decision: delete the seven that are safe; keep three for the renderer.**
The alternative — rewiring the renderer to consume wasm-prevalidated entries —
is architecturally better but moves prevalidation out of the drain-time batch,
where upsert-before-thin ordering is load-bearing and only the CI-only e2e
suite would catch a mistake. Not a trade worth making inside the irreversible
step.

**Delete (6):** `ack.ts`, `nack.ts`, `fec.ts`, `parity_decoder.ts`,
`cdf53_coverage.ts`, `decode_error_batcher.ts`.

**`feedback.ts` cannot go either** — found when checking references.
`prevalidate.ts` and `prevalidate_cdf53.ts`, both kept for the renderer,
import their `ERR_*` constants from it. Splitting those constants into a new
module to salvage one deletion is not worth the churn; `feedback.ts` joins the
kept set. Its other 18 exports (`encodeHello`, `LossTracker`, the `HELLO_*`
and `FEEDBACK_*` constants) become dead but harmless.

**Keep (4 + 1):** `feedback.ts`, `prevalidate.ts`, `prevalidate_cdf53.ts`,
`palette_shadow.ts` — renderer dependencies. And `decoder.ts` **whole**: it
holds both `Codec` (a `const enum` at :16, used by the new dispatcher) and
`FullFrameDecoder` (:136, platform glue). The earlier plan's split of that
file is unnecessary.

**Accepted residue:** PalRLE is prevalidated twice and palette state lives in
two places. That is partial drift surviving the migration, and it is now a
scoped follow-up rather than a surprise. Record it in the map doc.

- [ ] **Step 1: Confirm each of the seven is unreferenced**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client
for m in ack nack fec parity_decoder feedback cdf53_coverage decode_error_batcher; do
  printf "%-24s %s\n" "$m" "$(grep -rl "from '.*/$m'" src/ tests/ | tr '\n' ' ')"
done
```

Anything still referenced from `src/` means Task 3 missed a call site.
**Stop and fix that** — do not delete a module still in use.

- [ ] **Step 2: Delete, build, test, commit**

```bash
npm run build && npm test
```

### Task 7: Delete the retargeted suites

The step-3b suites were an equivalence check between two live implementations. With the TS gone they duplicate `oracle_*.rs`, which is the behavioural authority.

Delete: `ack`, `nack`, `cdf53_coverage`, `decode_error_batcher`, `feedback`, `palette_shadow`, `parity_decoder`, `prevalidate`, `prevalidate_cdf53`, and **`constants_parity`** — it compares against TS constants that no longer exist.

**Keep** `wasm_smoke` (module loads, stamp matches), `tile_assembly` (drives `handleDatagram`), `input` (drives the wasm encoders), and the five untouched suites.

**Keep `tests/helpers/wasm.ts`** only if a surviving suite still imports it; otherwise delete it too.

- [ ] **Step 1: Delete and verify**

```bash
npm test
```

Report the new totals and account for the delta.

- [ ] **Step 2: Commit**

### Task 8: Re-measure the bundle

`docs/specs/wasm-bundle-baseline.md` records the pre-cutover numbers and the exact commands. Re-run them and append the post-cutover figures.

Use `npm run build` this time — the wasm is now part of the bundle, which is the point.

Expect a regression: 52.6 KB gzip of wasm against 26.7 KB for the entire old bundle, and deleting 1,147 lines of TS will not close that. The measurement is a record, not a gate; report the real number.

---

## Done criteria

- [ ] `cargo test`, `cargo clippy`, both wasm targets: clean.
- [ ] `npm test` and `npm run build` pass.
- [ ] **The browser e2e suite passes in CI** — 54 tests, Chromium and Firefox.
- [ ] All ten protocol-derived globals shown to take more than one value.
- [ ] No `src/` file imports a deleted module.
- [ ] `src/webgpu/` and every WGSL shader untouched.
- [ ] Post-cutover bundle numbers appended to the baseline doc.

## What this plan deliberately does not do

- Touch `src/webgpu/` or any shader — an explicit spec non-goal.
- Delete `lossless_golden` — a TS mirror of `ghostframe-test-pattern`, not protocol; the spec records it as adjacent scope.
- Compile `ghostframe-client-net` into the browser bundle. The browser gets datagrams from the platform's WebTransport and never runs quinn.
- Chase the bundle regression. Options are recorded in the baseline doc; none are applied.
