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
