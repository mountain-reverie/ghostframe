# Wasm Cutover Step 3b — Retargeted Vitest Suites Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Point the nine protocol vitest suites at the wasm exports instead of the TypeScript modules, and make them pass, while both implementations are still live.

**Architecture:** Each suite keeps its assertions and loses its subject. The TS is callback-and-fake-timer shaped; the Rust is poll-and-injected-time. A small shared harness per unit translates between them, so the tests change at their imports and setup lines, not at their `expect`s. The suites' constants must come from wasm too — exported in Phase 1 — or the tests stop tracking the Rust.

**Tech Stack:** Rust, `wasm-bindgen`, `wasm-pack` 0.15.0, vitest 2, TypeScript 5.5.

---

## What this step is for

Read this before starting. It determines what "done" means.

`ghostframe-client-core/tests/oracle_*.rs` **already contains a faithful port
of all nine suites** — 136 tests, near 1:1 with the TypeScript. A survey
before writing this plan found `oracle_ack.rs` covering every one of
`ack.test.ts`'s nine cases, `parity_decoder` covered more thoroughly in Rust
(11 tests vs 5), and the only TS cases without Rust counterparts being
`tile_key`'s two string-format assertions, which have no Rust analogue by
construction.

So this step is **not** how `client-core`'s behaviour gets verified. That
already happened. What it buys:

1. **The wasm boundary.** Everything added in step 3a — the shims,
   `serde-wasm-bindgen` serialisation, `BigInt` conversion — is new code
   currently guarded by four smoke tests. The retargeted suites exercise it
   against assertions written before it existed.
2. **An executable check that the oracles are faithful.** If an
   `oracle_*.rs` test mis-ported a TS behaviour *and* `client-core`
   implemented the mis-ported version, both would agree and neither would
   notice. A TS assertion pointed at wasm is the only thing that catches
   that. A manual spot-check found no such case; this converts the
   spot-check into a test run.

**Therefore: a suite that passes only after its assertion was changed has
failed at its job.** If a retargeted assertion does not hold, that is the
finding this step exists to produce — stop and report it. Do not adjust the
expectation to match the Rust. The spec's own risk row says to budget
debugging, not a clean port.

Naming, spelling and shape may change freely. Asserted values may not.

---

## Scope

**In:** the nine *retarget then delete* suites — `ack`, `nack`,
`cdf53_coverage`, `decode_error_batcher`, `feedback`, `palette_shadow`,
`parity_decoder`, `prevalidate`, `prevalidate_cdf53` — plus `tile_key` and
the encoding half of `input`, which the spec files under *split or
reconsider*.

**Out:** `bootstrap`, `diagnostics`, `renderer_idle_skip`, `sanity`,
`solid_pack` — their subjects survive the cutover and they are not touched.
`lossless_golden` is also untouched: it is a TS mirror of
`ghostframe-test-pattern`'s generator, not protocol code, and the spec
records deleting it as adjacent scope.

**Nothing is deleted in this plan.** Both implementations stay live. Step 4
deletes the TS modules and these suites together.

---

## Phase 1 — the exports the suites need

The suites do not only import classes and functions. They import **constants**
(`MAX_ACK_ENTRIES`, `ACK_OVERLAP_COUNT`, `NACK_BATCH_FLUSH_MS`,
`HELLO_MSG_TYPE`, the ten `ERR_*` codes) and **test helpers**
(`parseNackEnvelopeForTest`, `parseParityEnvelope`,
`encodeParityEnvelopeForTest`, `encodeHello`, `encodeDecodeError`,
`rleDecode`). Step 3a exported none of these.

Hardcoding them in the tests would be the obvious shortcut and would defeat
the purpose: the constants would stop tracking the Rust, and a value drifting
on the Rust side would leave the tests green.

### Task 1: Name the protocol constants that are currently literals in Rust

**Files:**
- Modify: `ghostframe-client-core/src/loss_tracker.rs`
- Modify: `ghostframe-client-core/src/decode_error_batcher.rs`
- Modify: `ghostframe-client-core/src/nack_batcher.rs`
- Modify: `ghostframe-client-core/src/ack_batcher.rs`

The TypeScript names these; the Rust port inlined them as magic numbers.
Exporting them requires naming them first, which is an improvement
independent of this step.

- [ ] **Step 1: Find every literal**

```bash
cd /home/cedric/work/ghostframe/ghostframe-client-core
grep -n '0x03' src/loss_tracker.rs
grep -n '0x04' src/decode_error_batcher.rs
grep -n 'const FLUSH_INTERVAL_US\|const NACK_BATCH_MAX' src/nack_batcher.rs
grep -n 'const FLUSH_INTERVAL_US\|const MAX_RECENT' src/ack_batcher.rs
```

Expected: `encode_hello` builds `[0x03, caps]`; `DecodeErrorBatcher::report`
builds `vec![0x04, codec_byte, tile_x, tile_y, code as u8]`;
`nack_batcher.rs` has private `FLUSH_INTERVAL_US: u64 = 5_000` and
`NACK_BATCH_MAX: usize = 64`; `ack_batcher.rs` has private
`FLUSH_INTERVAL_US: u64 = 5_000` and `MAX_RECENT`.

- [ ] **Step 2: Add named constants in `loss_tracker.rs`**

```rust
/// Client capability announcement, sent once on the feedback stream at
/// construction. Wire layout `[HELLO_MSG_TYPE, caps]`, where caps bit0 =
/// indices_raw_enabled and bit1 = supports_h264.
pub const HELLO_MSG_TYPE: u8 = 0x03;

/// Encoded length of a Hello message.
pub const HELLO_SIZE: usize = 2;
```

and use `HELLO_MSG_TYPE` in `encode_hello` instead of the literal.

- [ ] **Step 3: Add named constants in `decode_error_batcher.rs`**

```rust
/// Decode-error report, sent on the feedback stream. Wire layout
/// `[DECODE_ERROR_MSG_TYPE, codec, tile_x, tile_y, code]`.
///
/// Shares the value 0x04 with `ACK_BATCH_MSG_TYPE`, which is unambiguous
/// only because the two travel on different channels: ACK batches are
/// datagrams, decode errors are stream messages. Do not merge them.
pub const DECODE_ERROR_MSG_TYPE: u8 = 0x04;

/// Encoded length of a decode-error report.
pub const DECODE_ERROR_SIZE: usize = 5;
```

and use it in `report` instead of the literal.

- [ ] **Step 4: Make the batcher constants public**

In `nack_batcher.rs` change `const FLUSH_INTERVAL_US` and
`const NACK_BATCH_MAX` to `pub const`. In `ack_batcher.rs` change
`const FLUSH_INTERVAL_US` to `pub const`. Leave `MAX_RECENT` private — no
suite imports it.

- [ ] **Step 5: Verify nothing changed behaviourally**

```bash
cd /home/cedric/work/ghostframe && cargo test -p ghostframe-client-core 2>&1 | grep 'test result' | tail -3
```

Expected: the same 193-test pass count as before, zero failures. This task
renames; it must not alter a byte.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-client-core/src/
git commit -m "refactor(client-core): name the protocol constants the TS already named

encode_hello and DecodeErrorBatcher::report carried bare 0x03/0x04
literals where the TypeScript had HELLO_MSG_TYPE and
DECODE_ERROR_MSG_TYPE. Names are needed to export them; naming them is
worth doing regardless."
```

### Task 2: Export the constants through wasm, and pin them against the TS

**Files:**
- Create: `ghostframe-client-wasm/src/constants.rs`
- Modify: `ghostframe-client-wasm/src/lib.rs`
- Create: `ghostframe-web-client/tests/constants_parity.test.ts`

- [ ] **Step 1: Export them**

`wasm-bindgen` cannot export a `const` directly; it exports functions.
Getters keep the values single-sourced from Rust. Create
`ghostframe-client-wasm/src/constants.rs`:

```rust
//! Protocol constants, re-exported so the retargeted vitest suites read
//! them from Rust rather than hardcoding them.
//!
//! Hardcoding would leave the suites green while a value drifted on the
//! Rust side, which is the exact failure these suites exist to catch.
//!
//! `wasm-bindgen` cannot export a `const`, so each is a getter.

use ghostframe_client_core::{
    ack_batcher::FLUSH_INTERVAL_US as ACK_FLUSH_INTERVAL_US,
    decode_error_batcher::{DECODE_ERROR_MSG_TYPE, DECODE_ERROR_SIZE},
    loss_tracker::{HELLO_MSG_TYPE, HELLO_SIZE},
    nack_batcher::{FLUSH_INTERVAL_US as NACK_FLUSH_INTERVAL_US, NACK_BATCH_MAX},
};
use ghostframe_protocol::ack::{
    ACK_BATCH_MSG_TYPE, ACK_ENTRY_SIZE, ACK_OVERLAP_COUNT, MAX_FRESH_ENTRIES_PER_BATCH,
};
use ghostframe_protocol::protocol::{TILE_NACK_ENVELOPE, TILE_PARITY_ENVELOPE};
use wasm_bindgen::prelude::*;

macro_rules! export_const {
    ($js:literal, $fn_name:ident, $ty:ty, $value:expr) => {
        #[wasm_bindgen(js_name = $js)]
        pub fn $fn_name() -> $ty {
            $value
        }
    };
}

export_const!("ackBatchMsgType", ack_batch_msg_type, u8, ACK_BATCH_MSG_TYPE);
export_const!("ackEntrySize", ack_entry_size, usize, ACK_ENTRY_SIZE);
export_const!("ackOverlapCount", ack_overlap_count, usize, ACK_OVERLAP_COUNT);
export_const!("maxAckEntries", max_ack_entries, usize, MAX_FRESH_ENTRIES_PER_BATCH);
export_const!("ackFlushIntervalMs", ack_flush_interval_ms, u64, ACK_FLUSH_INTERVAL_US / 1000);

export_const!("tileNackEnvelope", tile_nack_envelope, u8, TILE_NACK_ENVELOPE);
export_const!("nackBatchFlushMs", nack_batch_flush_ms, u64, NACK_FLUSH_INTERVAL_US / 1000);
export_const!("nackBatchMax", nack_batch_max, usize, NACK_BATCH_MAX);

export_const!("tileParityEnvelope", tile_parity_envelope, u8, TILE_PARITY_ENVELOPE);

export_const!("helloMsgType", hello_msg_type, u8, HELLO_MSG_TYPE);
export_const!("helloSize", hello_size, usize, HELLO_SIZE);
export_const!("decodeErrorMsgType", decode_error_msg_type, u8, DECODE_ERROR_MSG_TYPE);
export_const!("decodeErrorSize", decode_error_size, usize, DECODE_ERROR_SIZE);
```

Declare `pub mod constants;` in `lib.rs`.

> `MAX_FRESH_ENTRIES_PER_BATCH` is the Rust name for what the TS calls
> `MAX_ACK_ENTRIES`. They are the same value (64) and the same concept —
> the fresh-entry cap that forces an immediate flush. The JS-facing name
> keeps the TS spelling so the suites read unchanged; the Rust name is more
> precise and stays.

> `TILE_PARITY_ENVELOPE` is 0x04 and `TILE_NACK_ENVELOPE` is 0x05 on the
> datagram channel; `ACK_BATCH_MSG_TYPE` is also 0x04 but on datagrams, and
> `DECODE_ERROR_MSG_TYPE` is 0x04 on the stream. **Verify each value against
> its Rust definition rather than assuming** — if `TILE_PARITY_ENVELOPE` is
> not where the `use` above expects it, find it and report the path.

- [ ] **Step 2: Export the `ERR_*` codes**

The ten `ERR_*` constants are `DecodeErrorCode` discriminants. Rather than
ten getters, export one object so a drifting discriminant is visible in one
place. Append to `constants.rs`:

```rust
use ghostframe_client_core::DecodeErrorCode;
use serde::Serialize;

/// The `DecodeErrorCode` discriminants, mirroring the TS `ERR_*` constants
/// in `feedback.ts`. Exported as one object so a drifting discriminant
/// shows up in a single assertion rather than ten.
#[derive(Serialize)]
#[allow(non_snake_case)]
struct ErrorCodes {
    ERR_PAYLOAD_TOO_SHORT: u8,
    ERR_COUNT_OUT_OF_RANGE: u8,
    ERR_THIN_UNCACHED_PALETTE: u8,
    ERR_BUNDLED_TRUNCATED: u8,
    ERR_INDEX_OOB: u8,
    ERR_RLE_OVERSHOOT: u8,
    ERR_RLE_UNDERSHOOT: u8,
    ERR_CDF53_BAD_PASS: u8,
    ERR_CDF53_TRUNCATED: u8,
    ERR_CDF53_RLE_LENGTH: u8,
}

#[wasm_bindgen(js_name = errorCodes)]
pub fn error_codes() -> Result<JsValue, JsValue> {
    let codes = ErrorCodes {
        ERR_PAYLOAD_TOO_SHORT: DecodeErrorCode::PayloadTooShort as u8,
        ERR_COUNT_OUT_OF_RANGE: DecodeErrorCode::CountOutOfRange as u8,
        ERR_THIN_UNCACHED_PALETTE: DecodeErrorCode::ThinUncachedPalette as u8,
        ERR_BUNDLED_TRUNCATED: DecodeErrorCode::BundledTruncated as u8,
        ERR_INDEX_OOB: DecodeErrorCode::IndexOob as u8,
        ERR_RLE_OVERSHOOT: DecodeErrorCode::RleOvershoot as u8,
        ERR_RLE_UNDERSHOOT: DecodeErrorCode::RleUndershoot as u8,
        ERR_CDF53_BAD_PASS: DecodeErrorCode::Cdf53BadPass as u8,
        ERR_CDF53_TRUNCATED: DecodeErrorCode::Cdf53Truncated as u8,
        ERR_CDF53_RLE_LENGTH: DecodeErrorCode::Cdf53RleLength as u8,
    };
    serde_wasm_bindgen::to_value(&codes).map_err(|e| JsValue::from_str(&e.to_string()))
}
```

Also export the `PalRleVariant` discriminants the same way — `prevalidate.test.ts`
imports `PalRleVariant`. Check `ghostframe-client-core/src/pal_rle_decode.rs`
for the variant order (`Bundled`, `Thin`, `IndicesRaw`) and match the
mapping already used by `WasmPrevalidatedPalRle` in `boundary.rs`
(Bundled=0, Thin=1, IndicesRaw=2) — **they must agree**, and a mismatch here
would make every `prevalidate` variant assertion wrong in the same
direction, which is exactly the kind of consistent-but-wrong result a test
cannot catch on its own.

- [ ] **Step 3: Write the parity test — this is the load-bearing one**

While both implementations exist, assert every exported constant equals the
TS constant it replaces. This test is deleted at step 4 along with the TS.

`ghostframe-web-client/tests/constants_parity.test.ts`:

```ts
// Asserts every wasm-exported protocol constant equals the TypeScript
// constant it replaces, while both still exist. This is the cheapest
// equivalence check in the whole cutover: it runs in milliseconds and would
// catch a discriminant or message-type that drifted during the port.
//
// Deleted at step 4 together with the TS constants it reads.
import { describe, it, expect } from 'vitest';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';
import {
  ACK_BATCH_MSG_TYPE, ACK_ENTRY_SIZE, ACK_OVERLAP_COUNT, MAX_ACK_ENTRIES,
} from '../src/ack';
import { TILE_NACK_ENVELOPE, NACK_BATCH_FLUSH_MS, NACK_BATCH_MAX } from '../src/nack.js';
import { TILE_PARITY_ENVELOPE } from '../src/parity_decoder.js';
import {
  HELLO_MSG_TYPE, HELLO_SIZE, DECODE_ERROR_MSG_TYPE, DECODE_ERROR_SIZE,
  ERR_PAYLOAD_TOO_SHORT, ERR_COUNT_OUT_OF_RANGE, ERR_THIN_UNCACHED_PALETTE,
  ERR_BUNDLED_TRUNCATED, ERR_INDEX_OOB, ERR_RLE_OVERSHOOT, ERR_RLE_UNDERSHOOT,
  ERR_CDF53_BAD_PASS, ERR_CDF53_TRUNCATED, ERR_CDF53_RLE_LENGTH,
} from '../src/feedback.js';

describe('protocol constants match between TS and wasm', () => {
  it('ack', () => {
    expect(Number(wasm.ackBatchMsgType())).toBe(ACK_BATCH_MSG_TYPE);
    expect(Number(wasm.ackEntrySize())).toBe(ACK_ENTRY_SIZE);
    expect(Number(wasm.ackOverlapCount())).toBe(ACK_OVERLAP_COUNT);
    expect(Number(wasm.maxAckEntries())).toBe(MAX_ACK_ENTRIES);
  });

  it('nack', () => {
    expect(Number(wasm.tileNackEnvelope())).toBe(TILE_NACK_ENVELOPE);
    expect(Number(wasm.nackBatchFlushMs())).toBe(NACK_BATCH_FLUSH_MS);
    expect(Number(wasm.nackBatchMax())).toBe(NACK_BATCH_MAX);
  });

  it('parity', () => {
    expect(Number(wasm.tileParityEnvelope())).toBe(TILE_PARITY_ENVELOPE);
  });

  it('feedback message types', () => {
    expect(Number(wasm.helloMsgType())).toBe(HELLO_MSG_TYPE);
    expect(Number(wasm.helloSize())).toBe(HELLO_SIZE);
    expect(Number(wasm.decodeErrorMsgType())).toBe(DECODE_ERROR_MSG_TYPE);
    expect(Number(wasm.decodeErrorSize())).toBe(DECODE_ERROR_SIZE);
  });

  it('every decode-error discriminant', () => {
    expect(wasm.errorCodes()).toEqual({
      ERR_PAYLOAD_TOO_SHORT, ERR_COUNT_OUT_OF_RANGE, ERR_THIN_UNCACHED_PALETTE,
      ERR_BUNDLED_TRUNCATED, ERR_INDEX_OOB, ERR_RLE_OVERSHOOT, ERR_RLE_UNDERSHOOT,
      ERR_CDF53_BAD_PASS, ERR_CDF53_TRUNCATED, ERR_CDF53_RLE_LENGTH,
    });
  });
});
```

`Number(...)` wraps each wasm getter because `usize`/`u64` cross as `BigInt`
and `expect(1n).toBe(1)` fails. `u8` crosses as `number`. Rather than
tracking which is which, wrap them all.

- [ ] **Step 4: Run it**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client
export PATH="$HOME/.cargo/bin:$PATH"
npm test 2>&1 | tail -12
```

Expected: all pass.

**If any constant disagrees, STOP and report it.** A mismatch here is a real
port defect and is the most valuable thing this entire step can find — it
means a wire constant drifted between the TS and the Rust, which no Rust
test can detect because both sides of a Rust test use the Rust value.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-wasm/src/constants.rs ghostframe-client-wasm/src/lib.rs \
        ghostframe-web-client/tests/constants_parity.test.ts
git commit -m "feat(wasm): export protocol constants, pinned against the TS

The suites import constants, not just functions. Exporting them from
Rust keeps the retargeted tests tracking the Rust; the parity test
asserts each equals the TS value while both still exist."
```

### Task 3: Export the codec and test helpers

**Files:**
- Modify: `ghostframe-client-wasm/src/units.rs`
- Modify: `ghostframe-client-wasm/src/boundary.rs`

Six helpers the suites import have no wasm export yet. All have Rust
implementations already — this is wiring, not new logic.

- [ ] **Step 1: Confirm each Rust counterpart exists**

```bash
cd /home/cedric/work/ghostframe
grep -n 'impl TileNackEnvelope' -A 40 ghostframe-protocol/src/protocol.rs | grep 'pub fn'
grep -n 'impl TileParityEnvelope' -A 20 ghostframe-protocol/src/protocol.rs | grep 'pub fn'
grep -n 'pub fn rle_decode' ghostframe-protocol/src/codec/cdf53.rs
grep -n 'pub fn encode_hello' ghostframe-client-core/src/loss_tracker.rs
```

Expected: `TileNackEnvelope::{encode, encode_clamped, decode}`,
`TileParityEnvelope::{encode, decode}`, `rle_decode`, `encode_hello`.

- [ ] **Step 2: Add the mirrors**

Append to `boundary.rs`, following the existing `WasmAckEntry` pattern:

```rust
use ghostframe_protocol::protocol::{TileNackEntry, TileParityEnvelope};

#[derive(Debug, Serialize, PartialEq)]
pub struct WasmNackEntry {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
    pub frag_idx: u8,
}

impl From<&TileNackEntry> for WasmNackEntry {
    fn from(e: &TileNackEntry) -> Self {
        WasmNackEntry {
            frame_seq: e.frame_seq,
            tile_x: e.tile_x,
            tile_y: e.tile_y,
            pass_idx: e.pass_idx,
            frag_idx: e.frag_idx,
        }
    }
}

/// Parity envelope header. `parity_payload` is included because
/// `parity_decoder.test.ts` round-trips envelopes it built itself.
#[derive(Debug, Serialize, PartialEq)]
pub struct WasmParityEnvelope {
    pub group_first_wire_seq: u32,
    pub k: u8,
    pub parity_idx: u8,
    pub group_first_payload_len: u16,
    pub parity_payload: Vec<u8>,
}

impl From<&TileParityEnvelope> for WasmParityEnvelope {
    fn from(e: &TileParityEnvelope) -> Self {
        WasmParityEnvelope {
            group_first_wire_seq: e.group_first_wire_seq,
            k: e.k,
            parity_idx: e.parity_idx,
            group_first_payload_len: e.group_first_payload_len,
            parity_payload: e.parity_payload.clone(),
        }
    }
}
```

Note the TS `parseNackEnvelopeForTest` returns a **nested** shape —
`{ key: { frameSeq, tileX, tileY, passIdx }, fragIdx }` — while the wire
entry is flat. Export the flat shape; the nesting is a TS-side convenience,
and Task 4's `parseNackEnvelopeNested` reshapes it so `nack.test.ts`'s
assertions are unchanged.

- [ ] **Step 3: Add the exports**

To `units.rs`, following the existing style. Extend the imports with
`ghostframe_protocol::protocol::{TileNackEnvelope, TileParityEnvelope}` and
`crate::boundary::{WasmNackEntry, WasmParityEnvelope}`:

```rust
/// Decodes a NACK envelope into flat entries; `undefined` if malformed.
#[wasm_bindgen(js_name = parseNackEnvelope)]
pub fn parse_nack_envelope(bytes: &[u8]) -> Result<JsValue, JsValue> {
    let entries: Option<Vec<WasmNackEntry>> = TileNackEnvelope::decode(bytes)
        .ok()
        .map(|env| env.entries.iter().map(WasmNackEntry::from).collect());
    serde_wasm_bindgen::to_value(&entries).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Decodes a parity envelope; `undefined` if malformed.
#[wasm_bindgen(js_name = parseParityEnvelope)]
pub fn parse_parity_envelope(bytes: &[u8]) -> Result<JsValue, JsValue> {
    let env: Option<WasmParityEnvelope> = TileParityEnvelope::decode(bytes)
        .ok()
        .as_ref()
        .map(WasmParityEnvelope::from);
    serde_wasm_bindgen::to_value(&env).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Builds a parity envelope — the counterpart of the TS
/// `encodeParityEnvelopeForTest`. `parity_decoder.test.ts` constructs
/// envelopes to feed the decoder, so this is required, not a convenience.
#[wasm_bindgen(js_name = encodeParityEnvelope)]
pub fn encode_parity_envelope(
    group_first_wire_seq: u32,
    k: u8,
    parity_idx: u8,
    group_first_payload_len: u16,
    parity_payload: &[u8],
) -> Vec<u8> {
    let env = TileParityEnvelope {
        group_first_wire_seq,
        k,
        parity_idx,
        group_first_payload_len,
        parity_payload: parity_payload.to_vec(),
    };
    let mut out = Vec::new();
    env.encode(&mut out);
    out
}

/// `[HELLO_MSG_TYPE, caps]`; caps bit0 = indices_raw, bit1 = supports_h264.
#[wasm_bindgen(js_name = encodeHello)]
pub fn encode_hello(indices_raw: bool, supports_h264: bool) -> Vec<u8> {
    ghostframe_client_core::loss_tracker::encode_hello(indices_raw, supports_h264)
}

/// `[DECODE_ERROR_MSG_TYPE, codec, tile_x, tile_y, code]`, unbatched and
/// unconditional — the rate-limited path is `WasmDecodeErrorBatcher::report`.
///
/// Returns `undefined` for an unrecognised `codec` or `code` discriminant
/// rather than panicking; these are wire-derived values.
#[wasm_bindgen(js_name = encodeDecodeError)]
pub fn encode_decode_error(codec: u8, tile_x: u8, tile_y: u8, code: u8) -> Option<Vec<u8>> {
    let codec = codec_from_u8(codec)?;
    let code = decode_error_code_from_u8(code)?;
    Some(vec![
        DECODE_ERROR_MSG_TYPE,
        codec as u8,
        tile_x,
        tile_y,
        code as u8,
    ])
}

/// CDF53 run-length decode, for `prevalidate_cdf53.test.ts`'s direct
/// `rleDecode` cases.
#[wasm_bindgen(js_name = rleDecode)]
pub fn rle_decode(rle: &[u8]) -> Vec<u8> {
    ghostframe_protocol::codec::cdf53::rle_decode(rle)
}
```

> `encode_decode_error` reuses `codec_from_u8`/`decode_error_code_from_u8`,
> already in `units.rs` from step 3a, and `DECODE_ERROR_MSG_TYPE` from
> Task 1. It deliberately does **not** route through `DecodeErrorBatcher` —
> `feedback.test.ts` asserts on the raw encoding, and the batcher would
> rate-limit a second identical call and return `undefined`, making the test
> mysteriously order-dependent.

> Returning `Option<Vec<u8>>` here differs from the TS `encodeDecodeError`,
> which masks with `& 0xFF` and always returns bytes. If `feedback.test.ts`
> passes an out-of-range value anywhere, that assertion will need the
> difference reconciled — report it rather than silently widening the
> export.

> `encodeDecodeError` must **not** route through `DecodeErrorBatcher` —
> `feedback.test.ts` asserts on the raw encoding, and the batcher would
> rate-limit a second identical call and return `undefined`, making the test
> mysteriously order-dependent.

- [ ] **Step 4: Verify**

```bash
cd /home/cedric/work/ghostframe
cargo build -p ghostframe-client-wasm --target wasm32-unknown-unknown 2>&1 | tail -2
cargo clippy -p ghostframe-client-wasm --all-targets 2>&1 | tail -5
cargo fmt -p ghostframe-client-wasm
cd ghostframe-web-client && export PATH="$HOME/.cargo/bin:$PATH" && npm test 2>&1 | tail -6
```

Exercise each new export from Node and check a malformed input returns a
value rather than throwing — a panic aborts the whole wasm module.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-wasm/src/
git commit -m "feat(wasm): export the codec and envelope helpers the suites need"
```

### Task 4: Shared test harness, and typecheck the suites

**Files:**
- Create: `ghostframe-web-client/tests/helpers/wasm.ts`
- Modify: `ghostframe-web-client/tsconfig.json`
- Modify: `ghostframe-web-client/package.json`

- [ ] **Step 1: Write the harness**

Nine suites need the same two adaptations: a callback-collecting sink, and
injected time replacing `vi.useFakeTimers()`. Writing it once keeps the
suites' diffs confined to imports and setup.

`ghostframe-web-client/tests/helpers/wasm.ts`:

```ts
// Adapts the wasm batchers' poll-and-injected-time API to the shape the
// pre-cutover suites were written against: a callback that appends to a
// `sent` array, and a fake-timer advance.
//
// The TS batchers took a callback and used setTimeout; the Rust ones return
// Option<Vec<u8>> and take an explicit `now_us`. Making time explicit is an
// improvement — these harnesses are where that improvement is absorbed so
// the assertions do not have to change.
import * as wasm from '../../pkg-node/ghostframe_client_wasm.js';

/// Monotonic microsecond clock. `u64` crosses as BigInt, so every time
/// value handed to wasm must be a bigint — passing a number throws.
export class Clock {
  private us = 0n;
  now(): bigint { return this.us; }
  advanceMs(ms: number): bigint {
    this.us += BigInt(Math.round(ms * 1000));
    return this.us;
  }
}

export interface AckEntryLike {
  frameSeq: number; tileX: number; tileY: number;
  passIdx: number; arrivalTimeMsLo16: number;
}

/// Stands in for `new AckBatcher(dg => sent.push(dg))`.
export class AckHarness {
  readonly sent: Uint8Array[] = [];
  private readonly inner = new wasm.WasmAckBatcher();
  private readonly clock = new Clock();

  add(e: AckEntryLike): void {
    this.collect(this.inner.add(
      e.frameSeq, e.tileX, e.tileY, e.passIdx, e.arrivalTimeMsLo16, this.clock.now(),
    ));
  }

  flush(): void { this.collect(this.inner.flush()); }

  /// Replaces `vi.advanceTimersByTime(ms)`.
  advanceMs(ms: number): void { this.collect(this.inner.onTimeout(this.clock.advanceMs(ms))); }

  private collect(out: Uint8Array | undefined): void {
    if (out !== undefined) this.sent.push(out);
  }
}
```

Add a `NackHarness` in the same shape (`add(frameSeq, tileX, tileY, passIdx,
fragIdx)`, `advanceMs`), and reshape `parseNackEnvelope`'s flat entries into
the nested `{ key: {...}, fragIdx }` form `nack.test.ts` asserts on:

```ts
export function parseNackEnvelopeNested(bytes: Uint8Array) {
  const flat = wasm.parseNackEnvelope(bytes);
  if (flat === undefined) return undefined;
  return flat.map((e: any) => ({
    key: { frameSeq: e.frame_seq, tileX: e.tile_x, tileY: e.tile_y, passIdx: e.pass_idx },
    fragIdx: e.frag_idx,
  }));
}
```

- [ ] **Step 2: Typecheck the suites**

`tsconfig.json` currently has `"include": ["src"]`, so no test file is ever
typechecked. With `now_us` typed `bigint` across the boundary, a `0` where
`0n` is required throws at runtime — and these suites are about to acquire
many such call sites. `tsc` catches it statically.

Add `tests` to `include`, then:

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client && npx tsc --noEmit
```

**Expect pre-existing errors in the untouched suites.** They have never been
typechecked. Report what you find and how many. If the count is large, put
the suites in a separate `tsconfig.test.json` covering only `tests/helpers/`
and the suites this plan touches, rather than fixing unrelated files — and
say that you did.

- [ ] **Step 3: Wire it into the test script**

Add a `typecheck` script and make `test` depend on it, so a `0`-for-`0n`
fails the run rather than surfacing as a confusing runtime throw:

```json
"typecheck": "tsc --noEmit -p tsconfig.test.json",
"test": "npm run build:wasm:node && npm run typecheck && vitest run",
```

Adjust to whichever tsconfig arrangement Step 2 settled on.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-web-client/tests/helpers/ ghostframe-web-client/tsconfig*.json \
        ghostframe-web-client/package.json
git commit -m "test(web): shared wasm harness, and typecheck the suites

now_us is bigint across the boundary; passing 0 instead of 0n throws at
runtime. The suites were never typechecked, so nine retargets would have
had no static guard against it."
```

---

## Phase 2 — retarget the nine suites

**One task per suite.** Ordered simplest-first so the harness pattern is
established before the hard cases, with `ack` early enough that a structural
problem surfaces before eight suites depend on it.

Every task follows the same shape, so it is given once here rather than
repeated nine times:

1. Read the suite and its `oracle_*.rs` counterpart side by side.
2. Change **only** the imports and the object construction. Point the imports
   at `../pkg-node/ghostframe_client_wasm.js` and `./helpers/wasm.js`.
3. Run `npm test`. Every assertion must pass **unmodified**.
4. If an assertion fails: **stop, and report it with both values.** Do not
   change the expectation. This is the finding the step exists for. Say
   whether the Rust or the TS looks correct, and why.
5. Commit the single suite.

| Task | Suite | Lines | Notes |
|---|---|---:|---|
| 5 | `palette_shadow` | 34 | No timers, no encoding. Establishes the pattern. |
| 6 | `nack` | 49 | Fake timers → `NackHarness.advanceMs`. Needs the nested reshape from Task 4. |
| 7 | `feedback` | 51 | Pure encoders: `encodeHello`, `encodeDecodeError`, constants. |
| 8 | `decode_error_batcher` | 78 | Rate limiting. Imports `ERR_THIN_UNCACHED_PALETTE` from `errorCodes()`. |
| 9 | `prevalidate_cdf53` | 107 | Also imports a JSON fixture from `ghostframe-e2e/src/harness/fixtures/`. **Leave that import alone** — it is cross-crate test data, not protocol code. |
| 10 | `parity_decoder` | 111 | Builds envelopes via `encodeParityEnvelope`. Its `receiveParity` takes raw bytes now, collapsing the TS parse-then-pass pairing. |
| 11 | `ack` | 150 | The archetype: fake timers *and* overlap semantics. `AckHarness` from Task 4. |
| 12 | `cdf53_coverage` | 153 | `applyCdf53Arrival` takes ten flattened params; thread the previous entry back in each call. |
| 13 | `prevalidate` | 160 | Needs `WasmPaletteShadow` *and* `PalRleVariant` discriminants. Rejections are `{ok:false, code}` values, not throws — the TS returned a discriminated result too, so assertions should map closely. |

---

## Phase 3 — the two reconsidered suites

### Task 14: `tile_key` — convert, do not retarget

`tests/tile_key.test.ts` asserts a **string** key shape produced by
`decoder.ts`:

```ts
const k = tileKey(0xCAFE, 17, 23, 5);
expect(k).toBe('51966:17:23:5');
```

Rust has no string key. `TileKey` is a `#[derive(Hash, Eq)]` struct whose
`pass_idx` field makes the cross-pass collision the test guards **impossible
by construction**. There is nothing to point the assertion at.

Two of its eight cases are string-format assertions (`includes passIdx as a
fourth segment`, `keeps frameSeq at index 0 so split(":")[0] still works`)
and have no analogue — drop them and say so in the commit. The remaining
behavioural cases become a test driving `WasmClientCore.handleDatagram` with
two passes under one `frame_seq`, asserting they produce separate
assemblies rather than merging.

`oracle_tile_key.rs` already covers the behavioural content in Rust (5
tests). The value here is the same as elsewhere: proving it holds *across
the boundary*.

### Task 15: `input` — split

`tests/input.test.ts` covers both halves:
- **Capture** (DOM listeners, `attachInputCapture` from `src/input/wire.ts`)
  is platform code and **stays in TypeScript, untouched**.
- **Encoding** (`src/input/encode.ts`, `keymap.ts`) moves to the wasm
  exports added in step 3a (`encodePointerMove`, `encodePointerButton`,
  `encodeWheel`, `encodeKeyDown`, `encodeKeyUp`, `keyToKeysym`).

Split the file in two: `input_capture.test.ts` (unchanged subject) and
`input_encode.test.ts` (retargeted). Only the second is deleted at step 4.

`oracle_input.rs` has 53 tests, mostly keymap coverage — the TS suite is much
smaller, so expect no gaps.

---

## Done criteria

- [ ] `cargo test -p ghostframe-client-core -p ghostframe-client-wasm` passes.
- [ ] `npm test` passes, including `constants_parity` and all nine retargeted suites.
- [ ] `npx tsc --noEmit` passes over the retargeted suites.
- [ ] **Every retargeted assertion passes unmodified**, or each modification is
      individually justified in its commit message with both values and a
      statement of which side was wrong.
- [ ] Nothing deleted: `ghostframe-web-client/src/` protocol modules still
      present and still imported by `main.ts`.
- [ ] Both `wasm-pack` targets still build.

## What this plan deliberately does not do

- Delete any TS module or suite. That is step 4.
- Touch `main.ts`. That is step 4.
- Touch `src/webgpu/` or any WGSL shader — an explicit spec non-goal.
- Retarget `bootstrap`, `diagnostics`, `renderer_idle_skip`, `sanity`,
  `solid_pack` or `lossless_golden`.
- Change any assertion to make a test pass.
