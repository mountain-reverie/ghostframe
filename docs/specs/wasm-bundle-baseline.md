# Wasm Cutover: Bundle Size Baseline

**Date:** 2026-09-11
**Git rev:** `6e9fbc1`
**Branch:** `feat/wasm-client-crate`

This is a record. It exists so that step 4 of the wasm cutover
(`docs/superpowers/specs/2026-09-11-wasm-cutover-design.md`, deleting the TS
protocol modules and rewiring `main.ts`) can compare like-for-like instead of
against nothing. At the time of measurement both the TypeScript protocol
layer and the `ghostframe-client-wasm` crate are live in the tree, but
`main.ts` still uses only the TypeScript path — the two builds below are
independent, not additive.

Since the stats panel it has one gate: `ci/check-wasm-size.sh` fails the
build when the wasm exceeds 1 MiB, 1,048,576 bytes (see the 2026-11-16
section). The measurements in sections 1 to 3 and the 2026-09-11 and
2026-09-12 sections have no assertion attached; only that wasm limit does.

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

(Historical: `decoder.ts` has since been replaced by
`ghostframe-client-wasm/src/app/decoder.rs`.)

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

## Stats panel (2026-11-12, `feat/stats-panel-client`)

`npm run build:wasm` (`wasm-pack build --target web`, release, `wasm-opt`), same toolchain, before and after the `ghostframe-web-ui` crate (Leptos 0.9.0-rc1, `csr`) was linked into `ghostframe-client-wasm`:

| | Raw bytes | Gzip bytes |
|---|---:|---:|
| Before (master) | 167,848 | 69,616 |
| After (this branch) | 459,444 | 185,771 |

Size-optimised profile (`opt-level = "s"`, LTO, one codegen unit, `panic = "abort"`), measured through `CARGO_PROFILE_RELEASE_*` only, nothing committed: 428,152 bytes raw, 177,089 gzip.

The spec's original 320 KB cap did not hold with Leptos linked (459,444 bytes raw). PR 3 cap: 524,288 bytes raw (512 KiB), owner decision 2026-11-13.

## Stats panel (Leptos in the wasm module), 2026-11-13

**Before:** `aab2fa2d` (master before the stats panel landed)  
**After:** `20924e3b` (`feat/stats-panel-e2e`)

Both built from scratch with `just build-web` (`npm run build`), asset hashes masked.
**This section is a gate** for the wasm: `ci/check-wasm-size.sh` fails the build above
524,288 bytes (512 KiB) raw. The JS bundle has no gate.

### Before

| Asset | Raw bytes | Gzip bytes |
|---|---:|---:|
| `dist/assets/ghostframe_client_wasm_bg.[hash].wasm` | 167844 | 69619 |
| `dist/assets/index.[hash].js` | 107893 | 30616 |

### After

| Asset | Raw bytes | Gzip bytes |
|---|---:|---:|
| `dist/assets/ghostframe_client_wasm_bg.[hash].wasm` | 459892 | 185930 |
| `dist/assets/index.[hash].js` | 119602 | 34081 |

### Wasm delta

Raw: 167844 -> 459892 (+292048 bytes, 173%). Limit 524,288; headroom 64396 bytes.

## 2026-11-14: PR 2, the web renderer on wgpu

`ghostframe-client-wasm` now links wgpu's WebGPU backend, `ghostframe-render`,
and the CDF 5/3 grid (the TypeScript renderer is gone from the JS bundle).
Measured with `just build-web` on the branch and on master at bcc59e03
(the local `origin/master` ref; the fetch was not possible offline).
`strings` finds no `naga` in the branch wasm (count 0), so naga is not linked.

| Build | wasm raw | wasm gzip | JS bundle raw | JS bundle gzip |
|---|---:|---:|---:|---:|
| master | 459892 | 185928 | 120062 | 34167 |
| PR 2 | 783983 | 304658 | 79640 | 23775 |
| PR 2, wgpu `opt-level = "s"` | 785108 | 305424 | | |
| PR 2, wgpu + ghostframe-render `"s"` | 773718 | 302892 | | |
| PR 2, wgpu + wgpu-types + ghostframe-render `"s"` | 769867 | 301937 | | |
| PR 2, default + `wasm-opt -Oz` | 746643 | 305025 | | |

Gzip is `gzip -c` of the file (default level); the PR 2 row from the standalone
wasm-pack build is 783983 raw, 304649 gzip.

The branch was 259,695 bytes (49.5%) over the 512 KiB cap, and none of the
measured levers closes more than 14,116 bytes (`opt-level = "s"` on wgpu even
grows it). Owner decision 2026-11-14: the cap in `ci/check-wasm-size.sh` is
raised to 819,200 bytes (800 KiB) raw, about 4% headroom over 783,983, with no
size-only code changes. The extra weight is wgpu's WebGPU backend and the shared
renderer; naga is not linked.

## 2026-11-15: FEC v2 rev 6 on the wgpu renderer

FEC v2 rev 6 puts the client's Reed-Solomon decoder, block state and
repair-status stream in the wasm. On top of master's wgpu renderer
(783,983 bytes raw in the 2026-11-14 section) the default release build is
827,627 bytes raw, 322,020 gzip: +43,644 bytes (5.6 %). No large static table
is involved; the GF(2^8) arithmetic and the block bookkeeping are code.

Owner decision 2026-11-15: the cap in `ci/check-wasm-size.sh` is raised to
884,736 bytes (864 KiB) raw, about 7 % headroom over 827,627, with no
size-only code changes.

Superseded by the 2026-11-16 section below: PR 3 of the all-Rust web client
(main loop in Rust) raised the cap to 1 MiB, which covers both.

Re-measured 2026-11-16 on FEC v2 rev 6 rebased onto PR 3 (master, 884,097 bytes
raw): `just build-web` gives 925,313 bytes raw, 352,968 gzip as
`ci/check-wasm-size.sh` counts it: +41,216 bytes (4.7 %) for FEC, in line with
the 2026-11-15 figure. That is 123,263 bytes under the 1 MiB cap.

## 2026-11-16: all-Rust web client PR 3 (main loop in Rust)

PR 3 moves startup, WebTransport, the rAF loop, the WebCodecs decoder, input
and the overlays from `main.ts` into `ghostframe-client-wasm` (`src/app/`).
Measured with `just build-web` (the vite output ghostbridge embeds):

| Build | Raw bytes | Gzip bytes |
|---|---|---|
| master (PR 2 merged, 6e310e42) | 790,120 | |
| PR 3 | 884,097 | 335,294 |
| PR 3, ghostframe-client-wasm `opt-level = "s"` | 858,808 | 331,963 |
| PR 3, ghostframe-client-wasm + ghostframe-render + wgpu `"s"` | 843,901 | 327,663 |

PR 3 is 64,897 bytes over the 800 KiB cap, and the opt-level levers leave it over
too (and would undo the 2026-11-13 decision to keep the protocol crates at
opt-level 3). Owner decision 2026-11-16: the cap is raised to 1,048,576 bytes
(1 MiB) raw "for now", to be tuned later. PR 4 (the e2e hooks in Rust) is
expected to grow it again; a size pass belongs after PR 5, when the remaining
JS-facing exports can go.

## 2026-11-17: PR 4+5 (e2e hooks in Rust, no npm)

Measured with `just build-web`, which is now `ci/build-web.sh` (cargo,
`wasm-bindgen` 0.2.129, `wasm-opt -O` from binaryen version_117; no Vite).
Master is 579de2ac, built with its own Vite pipeline.

| Build | wasm raw | wasm gzip | JS raw | JS gzip |
|---|---|---|---|---|
| master (PR 3 merged, 579de2ac) | 884,096 | 335,252 | 77,694 | 21,484 |
| PR 4+5 | 891,841 | 337,716 | 110,307 | 17,730 |

The wasm grows by 7,745 bytes: the e2e recorders and the stats line moved in
from TypeScript, and the JS-only exports went out. The JS is now
wasm-bindgen's glue alone, unminified (there is no bundler); it is larger raw
and smaller gzipped than Vite's minified bundle, which carried the
TypeScript. The cap stays 1,048,576 bytes.

Re-measured after rebasing onto FEC v2 rev 6 (master 2026-11-17): 941,308
bytes raw, 357,352 gzip (JS 110,307 / 17,727), against 925,313 / 352,968 for
FEC v2 on PR 3 in the 2026-11-15 section: +15,995 bytes for PR 4+5, and
107,268 bytes under the 1 MiB cap. The build runs cargo from
`ghostframe-client-wasm/`, so its `.cargo/config.toml` (+simd128) applies;
`ci/build-web.sh` fails if the optimised wasm has no `v128` instruction.
