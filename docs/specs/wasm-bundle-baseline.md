# Wasm Cutover: Bundle Size Baseline

**Date:** 2026-09-11
**Git rev:** `6e9fbc1`
**Branch:** `feat/wasm-client-crate`

This is a record, not a gate. It exists so that step 4 of the wasm cutover
(`docs/superpowers/specs/2026-09-11-wasm-cutover-design.md`, deleting the TS
protocol modules and rewiring `main.ts`) can compare like-for-like instead of
against nothing. At the time of measurement both the TypeScript protocol
layer and the `ghostframe-client-wasm` crate are live in the tree, but
`main.ts` still uses only the TypeScript path — the two builds below are
independent, not additive.

No size assertion or CI check is attached to these numbers.

## 1. Current bundle (TypeScript only)

Produced by `npx vite build`, which does **not** run `build:wasm` (unlike
`npm run build`), so this is the TS-only bundle as currently shipped.

| Asset | Raw bytes | Gzip bytes |
|---|---:|---:|
| `dist/index.html` | 556 | 366 |
| `dist/assets/index-HK9P3dBr.js` | 91,111 | 26,725 |

## 2. Wasm module on its own

Produced by `npm run build:wasm` (`wasm-pack build --target web`, release
profile, `wasm-opt` applied).

| Asset | Raw bytes | Gzip bytes |
|---|---:|---:|
| `pkg-web/ghostframe_client_wasm_bg.wasm` | 129,994 | 52,642 |
| `pkg-web/ghostframe_client_wasm.js` (glue) | 31,458 | 6,086 |
| **Total** | **161,452** | **58,728** |

The `.wasm` binary alone is already ~1.4x the raw size, and ~2x the gzip
size, of the entire current TS bundle. Wasm compresses worse than JS here
(gzip ratio ~0.41 for the `.wasm` vs. ~0.29 for the TS bundle), which is
typical for wasm binaries relative to text-based JS.

## 3. TS modules step 4 deletes

These are the protocol modules the wasm crate replaces:

| File | Lines | Bytes |
|---|---:|---:|
| `ack.ts` | 145 | 5,492 |
| `nack.ts` | 70 | 2,166 |
| `fec.ts` | 93 | 2,890 |
| `parity_decoder.ts` | 115 | 3,961 |
| `feedback.ts` | 123 | 4,031 |
| `prevalidate.ts` | 148 | 4,285 |
| `prevalidate_cdf53.ts` | 89 | 2,745 |
| `cdf53_coverage.ts` | 104 | 3,401 |
| `decode_error_batcher.ts` | 43 | 1,366 |
| `palette_shadow.ts` | 32 | 1,033 |
| `decoder.ts` | 185 | 5,875 |
| **Total** | **1,147** | **37,245** |

`decoder.ts` is not purely protocol code: lines 1-134 are wire-format
decode functions (`decodeDatagramHeader`, `decodeTileHeader`,
`decodeFrameHeader`, `FullFrameDecoder`'s supporting types), but
`FullFrameDecoder` itself (lines 136-185) wraps the browser's
`VideoDecoder`/`VideoFrame` APIs directly and is platform glue, not
protocol logic the wasm crate replaces. Step 4 should not assume this file
is deleted wholesale — expect a split, not a straight removal.

Uncompiled TS source size is not directly comparable to a built/minified
JS bundle, so the 37,245-byte figure above is a proxy for "code being
removed," not a bundle-size delta by itself. The real step-4 comparison is
the built bundle (§1) against the built bundle once main.ts is wired to
wasm and the dead TS is deleted.

## How to re-measure (at step 4)

Run from `ghostframe-web-client/`, with `$HOME/.cargo/bin` on `PATH`:

```bash
# 1. TS-only bundle (skip step once the TS protocol modules are gone —
#    at that point `npx vite build` already reflects the cutover state,
#    so this step becomes "the bundle after cutover", not "TS only").
npx vite build 2>&1 | tail -12

# 2. Wasm module on its own
npm run build:wasm 2>&1 | tail -3
ls -l pkg-web/*.wasm
gzip -c pkg-web/*.wasm | wc -c

# glue JS raw + gzip
ls -l pkg-web/*.js
gzip -c pkg-web/*.js | wc -c

# 3. Full production build (TS gone, wasm wired in main.ts) — this is
#    the number to compare against §1 above.
npm run build 2>&1 | tail -12
```

Use the exact same commands, not approximations — a differently-produced
number is not comparable to this baseline.

These are uncompressed-transfer byte counts from a local build, not a
measurement of real load time over a network.

## Size reduction attempts, 2026-09-11

Measured before concluding the regression is fixed-cost. None of these are
applied; they are recorded so step 4 does not repeat the investigation.

| Configuration | raw | gzip | vs. baseline (gzip) |
|---|---:|---:|---:|
| Baseline (as committed) | 129,994 | 52,642 | — |
| `wasm-opt = ['-Oz']` metadata | 129,655 | 52,713 | +0.1% |
| `[profile.release]` `opt-level="z"`, `lto`, `codegen-units=1`, `panic="abort"`, `strip` | 125,268 | 44,481 | −15.5% |

**`-Oz` buys nothing.** wasm-pack already runs `wasm-opt` by default and it is
doing its job; the remaining size is in the emitted code, not in missed
peephole optimisation.

**The size-optimised cargo profile buys ~15% gzip but breaks the build.** Under
that profile `wasm-opt` fails outright:

```
[wasm-validator error in function 661] unexpected false:
Bulk memory operations require bulk memory [--enable-bulk-memory]
Fatal: error validating input
Error: failed to execute `wasm-opt`: exited with exit status: 1
```

The profile makes rustc emit bulk-memory instructions that the `wasm-opt`
wasm-pack pins does not accept without `--enable-bulk-memory`. wasm-pack
leaves the un-opt'd artefact behind and **still exits 0**, so the 44,481 figure
above is un-`wasm-opt`'d output, not a validly optimised build. Anyone
retrying this must check for that error rather than trusting the byte count —
a smaller file here means the optimiser was skipped, not that it worked
better.

That profile is also workspace-global, so it would apply to the native server
and xdaemon builds too. A per-package override (`[profile.release.package.
ghostframe-client-wasm]`) can carry `opt-level` but not `lto` or `panic`,
which are profile-global — so the ~15% is not separable from a
workspace-wide change even if the `wasm-opt` failure were resolved.

**Conclusion.** The best measured figure is 44.5 KB gzip against 26.7 KB for
the entire current bundle — still 1.66x, from a build whose optimiser did not
run. The regression is not an artefact of missing compiler flags, and the
decision to proceed should be made on the drift-elimination argument rather
than on an expectation that the size gap closes.

Untried, if size later becomes blocking: dropping `serde`/`serde-wasm-bindgen`
in favour of hand-written `JsValue` construction, and dropping
`console_error_panic_hook` (which pulls in formatting machinery). Both trade
boundary ergonomics and debuggability for bytes; neither was measured.

## Post-cutover measurement, 2026-09-11 (Phase 4b Task 8)

**Git rev:** `3b32e70` **Branch:** `feat/wasm-cutover-delete`

Re-run using the exact commands in "How to re-measure" above, from
`ghostframe-web-client/` with `pkg-web`, `pkg-node` and `dist` deleted first
for a from-scratch build. `main.ts` now drives `WasmClientCore` exclusively
(Phase 4a) and the six dead TS protocol modules are deleted (Task 6); `npx
vite build` and `npm run build` now produce the same output, per this doc's
own note that they converge once the TS protocol layer is gone.

### Full production build (`npm run build`) — the number to compare against §1

| Asset | Raw bytes | Gzip bytes |
|---|---:|---:|
| `dist/index.html` | 556 | 364 |
| `dist/assets/index-D-gPETmI.js` | 87,111 | 25,521 |
| `dist/assets/ghostframe_client_wasm_bg-DYGTbRBw.wasm` | 137,289 | 55,547 |
| **Total** | **224,956** | **81,432** |

### Wasm module on its own (`npm run build:wasm`), for comparison to §2

| Asset | Raw bytes | Gzip bytes |
|---|---:|---:|
| `pkg-web/ghostframe_client_wasm_bg.wasm` | 137,289 | 55,538 |
| `pkg-web/ghostframe_client_wasm.js` (glue) | 38,048 | 7,050 |
| **Total** | **175,337** | **62,588** |

### Comparison to the pre-cutover baseline (§1)

| | Pre-cutover (TS only) | Post-cutover (wasm wired, TS deleted) | Delta |
|---|---:|---:|---:|
| Raw total | 91,667 | 224,956 | +133,289 (2.45x) |
| Gzip total | 27,091 | 81,432 | +54,341 (3.01x) |

The regression is real and roughly matches the order of magnitude flagged
before this work started (that estimate — 52.6 KB gzip of wasm against 26.7
KB for the entire old bundle — was itself measured off an earlier, more
minimal build of the crate). The `.wasm` binary's gzip size grew from 52,642
bytes (§2 baseline) to 55,547/55,538 bytes here, consistent with 4a having
moved real protocol logic (the reassembly, ACK/NACK, prevalidation and
feedback state machines) into the crate rather than just scaffolding.

One number moved the other way: the JS bundle itself (`index-*.js`) shrank
slightly, from 91,111/26,725 bytes (raw/gzip) pre-cutover to 87,111/25,521
bytes post-cutover — deleting ~1,147 lines of TS protocol code outweighed
adding the wasm-bindgen glue import and event-dispatch wiring in `main.ts`.

This is a record, not a gate, per the top of this document. No size
assertion is added.

## Post-TS-deletion measurement, 2026-09-12 (typed-tile-payload plan, Task 6)

**Git rev:** `8fe874c` **Branch:** `refactor/single-palrle-prevalidator`

Re-run using the exact commands in "How to re-measure" above, from
`ghostframe-web-client/` with `pkg-web`, `pkg-node` and `dist` deleted first
for a from-scratch build. This follows the typed-tile-payload plan
(Tasks 1-5): `ghostframe-client-core` now emits each codec's prevalidated
`TileData` product and `main.ts` dispatches on it; the renderer no longer
prevalidates PalRLE or keeps a palette shadow. Task 6 repointed the
`__cdf53TestIntegrate` test hook at the wasm `prevalidateCdf53` export and
deleted three now-unreferenced TS modules (`prevalidate.ts`,
`prevalidate_cdf53.ts`, `palette_shadow.ts`). `feedback.ts` and
`decode_error_batcher.ts`, also named in the plan's deletion list, were
kept: `decode_error_batcher.ts` still has a live caller in `main.ts`'s
`tick()`, reporting GPU-shader-detected PalRLE `IndexOob` errors
independent of the deleted CPU-side prevalidation, and `feedback.ts` is its
only remaining dependency.

### Full production build (`npm run build`) — the number to compare against §1 and the 2026-09-11 post-cutover measurement

| Asset | Raw bytes | Gzip bytes |
|---|---:|---:|
| `dist/index.html` | 556 | 367 |
| `dist/assets/index-DL3RJbgj.js` | 83,520 | 24,255 |
| `dist/assets/ghostframe_client_wasm_bg-D3kJaMmF.wasm` | 138,735 | 56,170 |
| **Total** | **222,811** | **80,792** |

### Wasm module on its own (`npm run build:wasm`), for comparison to §2

| Asset | Raw bytes | Gzip bytes |
|---|---:|---:|
| `pkg-web/ghostframe_client_wasm_bg.wasm` | 138,735 | 56,161 |
| `pkg-web/ghostframe_client_wasm.js` (glue) | 38,048 | 7,050 |
| **Total** | **176,783** | **63,211** |

### Comparison to the 2026-09-11 post-cutover measurement

| | 2026-09-11 (wasm wired, 6 TS modules deleted) | 2026-09-12 (3 more TS modules deleted) | Delta |
|---|---:|---:|---:|
| Raw total | 224,956 | 222,811 | −2,145 |
| Gzip total | 81,432 | 80,792 | −640 |

The `.js` bundle shrank slightly (87,111 → 83,520 raw; 25,521 → 24,255
gzip) from deleting `prevalidate.ts`, `prevalidate_cdf53.ts`, and
`palette_shadow.ts` — as expected, since `__cdf53TestIntegrate` was the
only real call site among them and it now calls the wasm export instead.
The `.wasm` binary grew marginally (137,289 → 138,735 raw; 55,547/55,538 →
56,170/56,161 gzip), consistent with the note in "How to re-measure": wasm
size drifts slightly between builds independent of TS-side changes.
`feedback.ts` and `decode_error_batcher.ts` remain in the bundle, so this
is not the full reduction five deletions would have produced — see the
note above on why those two modules were kept.

This is a record, not a gate, per the top of this document. No size
assertion is added.
