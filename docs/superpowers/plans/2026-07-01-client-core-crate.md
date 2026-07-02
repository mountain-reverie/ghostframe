# ghostframe-client-core Implementation Plan (Sub-project 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A pure sans-IO Rust crate implementing the full client half of the ghostframe protocol (parsing, reassembly, FEC, ACK/NACK/feedback, software decode to RGBA), with the web-client vitest suites ported as its test oracle.

**Architecture:** Two new workspace crates. `ghostframe-protocol` extracts the pure wire-format + codec modules that today live inside `ghostframe-lib` (whose native deps — ffmpeg/ash/quinn/tokio — are unconditional and therefore un-reusable as-is). `ghostframe-client-core` builds the sans-IO client state machine on top of it: `handle_datagram(&[u8], now_us) -> Vec<Event>`, `poll_transmit(now_us) -> Option<PollOutput>`, `poll_timeout() -> Option<u64>`, `on_timeout(now_us)`. No sockets, timers, or async inside; time is injected as `u64` microseconds (wasm-friendly — no `std::time::Instant`).

**Tech Stack:** Rust 2021, `thiserror`, `smallvec`, `proptest` (dev), workspace at `/home/cedric/work/ghostframe/Cargo.toml`. Reference TS implementation in `ghostframe-web-client/src/`; behavioral oracle in `ghostframe-web-client/tests/`.

## Global Constraints

- Wire format is fixed by `ghostframe-lib/src/transport/protocol.rs` — do not change any byte layout. Datagram/tile/frame headers and feedback are **big-endian**; ACK entries (`frame_seq` u32, `arrival_time_ms_lo16` u16) and NACK-envelope `frame_seq` are **little-endian** (protocol.rs:757, ack.rs:86).
- Constants (verify against source, never re-derive): `DATAGRAM_HEADER_SIZE=16`, `TILE_HEADER_SIZE=8`, `FRAME_HEADER_SIZE=14`, `TILE_DATAGRAM_FLAG=1<<31`, `TILE_SIZE=32`, `BPP=4`, `TILE_BYTES=4096`, `CDF53_PASS_COUNT=14`, `MAX_PALETTE_COUNT=16`, sentinel tile coords `(0xFF,0xFF)`, msg types: HELLO(stream)=0x03, FEEDBACK(stream)=0x01, DECODE_ERROR(stream)=0x04, ACK(datagram)=0x04, TILE_PARITY(datagram)=0x04, TILE_NACK(datagram)=0x05, INPUT=0x05.
- Client behavioral constants (from TS): `MAX_ACK_ENTRIES=64`, `ACK_OVERLAP_COUNT=8`, `ACK_ENTRY_SIZE=9`, ack/nack flush interval 5 ms, `NACK_BATCH_MAX=64`, parity window 40, `ASSEMBLY_TIMEOUT_MS=30`, stale threshold `latest_frame_seq - 2`, `TAIL_SWEEP_INTERVAL_MS=500`, `TAIL_FALLBACK_MS=1500`, feedback interval 100 ms, decode-error window 1000 ms / cap 32.
- Both new crates MUST NOT depend on tokio, quinn, ffmpeg, ash, str0m, or any non-pure-Rust crate. They must build for `wasm32-unknown-unknown`.
- `ghostframe-lib` must keep compiling with its existing module paths via re-exports — the Go FFI and all existing tests must pass unchanged.
- TDD throughout. Run `cargo test -p <crate>` per task; run full `cargo test --workspace` plus `cd ghostframe-web-client && npx vitest run` before each commit that touches `ghostframe-lib`.
- No `Instant`, `SystemTime`, or wall-clock reads anywhere in either crate. All APIs take `now_us: u64` (microseconds) injected by the caller.

## File Structure

```
ghostframe-protocol/            (new crate; moved-from ghostframe-lib, paths noted)
  Cargo.toml                    deps: thiserror, smallvec; dev: lz4_flex, proptest
  src/lib.rs
  src/tile.rs                   TILE_SIZE/BPP/TILE_BYTES  (from ghostframe-lib/src/tile/mod.rs consts)
  src/protocol.rs               (from ghostframe-lib/src/transport/protocol.rs)
  src/ack.rs                    (from ghostframe-lib/src/transport/ack.rs)
  src/feedback.rs               (from ghostframe-lib/src/transport/feedback.rs)
  src/fec.rs                    (from ghostframe-lib/src/transport/fec.rs)
  src/codec/mod.rs
  src/codec/cdf53.rs            (from ghostframe-lib/src/encoder/cdf53.rs; rle_decode made pub)
  src/codec/pal_rle.rs          (from ghostframe-lib/src/encoder/pal_rle.rs)
  src/codec/solid.rs            (from ghostframe-lib/src/encoder/solid.rs)

ghostframe-client-core/         (new crate)
  Cargo.toml                    deps: ghostframe-protocol, thiserror, smallvec; dev: proptest
  src/lib.rs                    ClientCore facade: handle_datagram / poll_transmit / poll_timeout / on_timeout / handle_stream_message
  src/event.rs                  Event, PollOutput, DecodeErrorCode
  src/ack_batcher.rs            port of web-client src/ack.ts
  src/nack_batcher.rs           port of src/nack.ts
  src/decode_error_batcher.rs   port of src/decode_error_batcher.ts
  src/loss_tracker.rs           port of LossTracker in src/feedback.ts + ReceiverFeedback/Hello encode
  src/palette_shadow.rs         port of src/palette_shadow.ts
  src/pal_rle_decode.rs         port of src/prevalidate.ts + RGBA materialization via codec::pal_rle
  src/cdf53_prevalidate.rs      port of src/prevalidate_cdf53.ts
  src/cdf53_coverage.rs         port of src/cdf53_coverage.ts
  src/cdf53_tile_state.rs       CPU replacement for webgpu/cdf53.ts per-tile accumulation → RGBA
  src/parity_decoder.rs         port of src/parity_decoder.ts (wire-seq FEC)
  src/fragment_parity.rs        port of src/fec.ts (legacy fragment-level FEC)
  src/reassembly.rs             port of main.ts assemblies map + handleSourceTileDatagram + finishAssembly
  src/frame_assembly.rs         port of main.ts H.264 full-frame path (emits NeedsH264)
  src/input.rs                  port of src/input/encode.ts + keymap.ts keysym mapping
  tests/oracle_*.rs             ported vitest suites (one file per suite)
  tests/loopback.rs             encode-with-server-code → decode-with-client-core integration tests
```

Key naming: reassembly buckets are keyed by `TileKey { frame_seq: u32, tile_x: u8, tile_y: u8, pass_idx: u8 }` (a struct, not a string — the vitest `tileKey` string-format tests become bucket-isolation tests on this struct).

---

### Task 1: Extract `ghostframe-protocol` crate (wire modules)

**Files:**
- Create: `ghostframe-protocol/Cargo.toml`, `ghostframe-protocol/src/lib.rs`, `ghostframe-protocol/src/tile.rs`
- Move (git mv + fix imports): `ghostframe-lib/src/transport/protocol.rs` → `ghostframe-protocol/src/protocol.rs`; same for `ack.rs`, `feedback.rs`, `fec.rs`
- Modify: root `Cargo.toml` (workspace members), `ghostframe-lib/Cargo.toml` (add dep), `ghostframe-lib/src/transport/mod.rs` (re-export shims), `ghostframe-lib/src/tile/mod.rs`

**Interfaces:**
- Consumes: nothing (first task).
- Produces: crate `ghostframe-protocol` exporting `protocol::{DatagramHeader, TileHeader, FrameHeader, Codec, TileFragmentInputs, fragment_tile, decode_tile_datagram, decode_frame_datagram, build_frame_dimensions_datagram, build_parity_datagrams, max_fragment_payload, is_tile_datagram, classify_inbound, NackMessage, TileParityEnvelope, TileNackEnvelope, constants…}`, `ack::{AckEntry, AckBatch, ACK_BATCH_MSG_TYPE, ACK_ENTRY_SIZE, MAX_FRESH_ENTRIES_PER_BATCH, ACK_OVERLAP_COUNT}`, `feedback::{ReceiverFeedback, FEEDBACK_MSG_TYPE, FEEDBACK_SIZE}`, `fec::*`, `tile::{TILE_SIZE, BPP, TILE_BYTES}`.

- [ ] **Step 1: Create the crate and move files**

```toml
# ghostframe-protocol/Cargo.toml
[package]
name = "ghostframe-protocol"
version = "0.1.0"
edition = "2021"

[dependencies]
thiserror = "1"
smallvec = "1"

[dev-dependencies]
lz4_flex = "0.11"
```

```rust
// ghostframe-protocol/src/lib.rs
pub mod ack;
pub mod fec;
pub mod feedback;
pub mod protocol;
pub mod tile;
```

```rust
// ghostframe-protocol/src/tile.rs — copy the three consts verbatim from ghostframe-lib/src/tile/mod.rs:1-3
pub const TILE_SIZE: u32 = 32;
pub const BPP: u32 = 4;
pub const TILE_BYTES: usize = (TILE_SIZE * TILE_SIZE * BPP) as usize;
```

```bash
git mv ghostframe-lib/src/transport/protocol.rs ghostframe-protocol/src/protocol.rs
git mv ghostframe-lib/src/transport/ack.rs      ghostframe-protocol/src/ack.rs
git mv ghostframe-lib/src/transport/feedback.rs ghostframe-protocol/src/feedback.rs
git mv ghostframe-lib/src/transport/fec.rs      ghostframe-protocol/src/fec.rs
```

Fix intra-crate paths in the moved files (they reference `crate::tile::TILE_SIZE` etc. — now `crate::tile::*` inside ghostframe-protocol). Add `"ghostframe-protocol"` to root `Cargo.toml` `[workspace] members`.

- [ ] **Step 2: Shim ghostframe-lib**

In `ghostframe-lib/Cargo.toml` add `ghostframe-protocol = { path = "../ghostframe-protocol" }`. In `ghostframe-lib/src/transport/mod.rs`, replace the four `mod` declarations with re-exports so every existing `crate::transport::protocol::...` path keeps working:

```rust
pub use ghostframe_protocol::ack;
pub use ghostframe_protocol::fec;
pub use ghostframe_protocol::feedback;
pub use ghostframe_protocol::protocol;
```

In `ghostframe-lib/src/tile/mod.rs`, replace the three consts with `pub use ghostframe_protocol::tile::{TILE_SIZE, BPP, TILE_BYTES};`.

- [ ] **Step 3: Build and run the full test suite**

Run: `cargo test --workspace`
Expected: PASS — no behavior change; unit tests that lived inside the moved files travel with them. If cbindgen (`ghostframe-lib/build.rs`) fails to find constants it previously exported from those modules, point its config at the re-exported paths (check `ghostframe-lib/include/ghostframe.h` is regenerated unchanged via `git diff`).

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "refactor: extract pure wire-format modules into ghostframe-protocol crate"
```

---

### Task 2: Move pure codecs into `ghostframe-protocol`

**Files:**
- Move: `ghostframe-lib/src/encoder/cdf53.rs` → `ghostframe-protocol/src/codec/cdf53.rs`; same for `pal_rle.rs`, `solid.rs`
- Create: `ghostframe-protocol/src/codec/mod.rs`
- Modify: `ghostframe-protocol/src/lib.rs` (`pub mod codec;`), `ghostframe-lib/src/encoder/mod.rs` (re-export shims)

**Interfaces:**
- Consumes: Task 1 crate layout.
- Produces: `codec::cdf53::{forward, inverse, encode_passes, decode_passes, rle_decode, CDF53_PASS_COUNT, CDF53_TOTAL_COEFFS, …}`, `codec::pal_rle::{decode_pal_rle, DecodedPalRle, PaletteEntry, PalRleDecodeError, MAX_PALETTE_COUNT, PALETTE_TABLE_SLOTS, encode_* fns}`, `codec::solid::{encode_solid, decode_solid, SolidDecodeError}`.

- [ ] **Step 1: Move files and shim**

```bash
mkdir -p ghostframe-protocol/src/codec
git mv ghostframe-lib/src/encoder/cdf53.rs   ghostframe-protocol/src/codec/cdf53.rs
git mv ghostframe-lib/src/encoder/pal_rle.rs ghostframe-protocol/src/codec/pal_rle.rs
git mv ghostframe-lib/src/encoder/solid.rs   ghostframe-protocol/src/codec/solid.rs
```

```rust
// ghostframe-protocol/src/codec/mod.rs
pub mod cdf53;
pub mod pal_rle;
pub mod solid;
```

In `ghostframe-lib/src/encoder/mod.rs` replace the three `pub mod` lines with:

```rust
pub use ghostframe_protocol::codec::cdf53;
pub use ghostframe_protocol::codec::pal_rle;
pub use ghostframe_protocol::codec::solid;
```

- [ ] **Step 2: Make `rle_decode` public**

In `codec/cdf53.rs`, change `fn rle_decode` (was private at cdf53.rs:588) to `pub fn rle_decode(bytes: &[u8]) -> Option<Vec<u8>>` — keep exact semantics; the client prevalidator needs it. If its current signature differs (e.g. returns `Vec<u8>` with expected-length arg), keep the existing signature and just add `pub`.

- [ ] **Step 3: Write a failing cross-check test for rle_decode visibility**

```rust
// ghostframe-protocol/src/codec/cdf53.rs — append to its #[cfg(test)] mod
#[test]
fn rle_decode_is_public_and_decodes_zero_run() {
    // 0x80|127 = 0xFF → 128 zeros (matches web-client prevalidate_cdf53 contract)
    let out = rle_decode(&[0xFFu8]).expect("decodes");
    assert_eq!(out, vec![0u8; 128]);
}
```

Run: `cargo test -p ghostframe-protocol rle_decode_is_public`
Expected: PASS (after Step 2). If the token scheme test fails, re-read cdf53.rs:555-586 — the contract is `0x00..0x7E` literal, `0x7F` escape-next-literal, `0x80..0xFF` zero-run of `(token & 0x7F) + 1`.

- [ ] **Step 4: Full workspace test + commit**

Run: `cargo test --workspace` — Expected: PASS.

```bash
git add -A && git commit -m "refactor: move pure codecs (cdf53, pal_rle, solid) into ghostframe-protocol"
```

---

### Task 3: `ghostframe-client-core` skeleton — events, errors, time model

**Files:**
- Create: `ghostframe-client-core/Cargo.toml`, `src/lib.rs`, `src/event.rs`
- Modify: root `Cargo.toml` (workspace member)

**Interfaces:**
- Consumes: `ghostframe-protocol`.
- Produces (used by every later task):

```rust
// src/event.rs
use ghostframe_protocol::protocol::Codec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileKey { pub frame_seq: u32, pub tile_x: u8, pub tile_y: u8, pub pass_idx: u8 }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DecodeErrorCode {
    PayloadTooShort = 1, CountOutOfRange = 2, ThinUncachedPalette = 3,
    BundledTruncated = 4, IndexOob = 5, RleOvershoot = 6, RleUndershoot = 7,
    Cdf53BadPass = 8, Cdf53Truncated = 9, Cdf53RleLength = 10,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Fully decoded 32x32 RGBA tile (4096 bytes, RGBA byte order).
    TileReady { frame_seq: u32, tile_x: u8, tile_y: u8, rgba: Vec<u8> },
    FrameDimensions { width: u32, height: u32 },
    /// Complete H.264 access unit — platform decodes (WebCodecs / ffmpeg).
    NeedsH264 { frame_seq: u32, timestamp_us: u32, is_keyframe: bool, payload: Vec<u8> },
    DecodeError { codec: Codec, tile_x: u8, tile_y: u8, code: DecodeErrorCode },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PollOutput {
    /// Send as a QUIC/WebTransport datagram (ACK batches, NACK envelopes).
    Datagram(Vec<u8>),
    /// Send on the bidirectional feedback stream (Hello, ReceiverFeedback, DecodeError).
    Stream(Vec<u8>),
}
```

```rust
// src/lib.rs — the facade every task fills in
pub struct ClientCore { /* fields added per task */ }

#[derive(Debug, Clone, Copy)]
pub struct ClientConfig { pub indices_raw_enabled: bool, pub supports_h264: bool }

impl ClientCore {
    pub fn new(config: ClientConfig, now_us: u64) -> Self;
    /// Feed one received datagram. Returns decode/render events.
    pub fn handle_datagram(&mut self, bytes: &[u8], now_us: u64) -> Vec<Event>;
    /// Drain one pending outbound message; call until None.
    pub fn poll_transmit(&mut self, now_us: u64) -> Option<PollOutput>;
    /// Earliest deadline (µs) at which on_timeout must be called, if any.
    pub fn poll_timeout(&self) -> Option<u64>;
    pub fn on_timeout(&mut self, now_us: u64) -> Vec<Event>;
}
```

- [ ] **Step 1: Write the failing skeleton test**

```rust
// ghostframe-client-core/tests/skeleton.rs
use ghostframe_client_core::{ClientCore, ClientConfig, PollOutput};

#[test]
fn new_core_emits_hello_on_stream_then_nothing() {
    let mut core = ClientCore::new(ClientConfig { indices_raw_enabled: true, supports_h264: true }, 0);
    // Hello: [0x03, caps] with bit0=indicesRaw, bit1=h264 (feedback.ts:81-86)
    assert_eq!(core.poll_transmit(0), Some(PollOutput::Stream(vec![0x03, 0x03])));
    assert_eq!(core.poll_transmit(0), None);
}

#[test]
fn empty_datagram_is_ignored() {
    let mut core = ClientCore::new(ClientConfig { indices_raw_enabled: false, supports_h264: false }, 0);
    assert!(core.handle_datagram(&[], 0).is_empty());
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ghostframe-client-core` → FAIL (crate/type not found).

- [ ] **Step 3: Implement skeleton** — Cargo.toml (`ghostframe-protocol = { path = "../ghostframe-protocol" }`, `thiserror`, `smallvec`; dev `proptest`), `event.rs` exactly as above, `lib.rs` with an internal `VecDeque<PollOutput>` outbox seeded with the Hello message in `new()`. `handle_datagram` returns empty vec for now. Add crate to workspace members.

- [ ] **Step 4: Run tests** — `cargo test -p ghostframe-client-core` → PASS.

- [ ] **Step 5: Commit** — `git add -A && git commit -m "feat: ghostframe-client-core skeleton with sans-IO API and event types"`

---

### Task 4: AckBatcher

**Files:**
- Create: `ghostframe-client-core/src/ack_batcher.rs`, `tests/oracle_ack.rs`
- Modify: `src/lib.rs` (module decl only; wiring into ClientCore happens in Task 10)

**Interfaces:**
- Consumes: `ghostframe_protocol::ack::{AckEntry, AckBatch, ACK_BATCH_MSG_TYPE, ACK_ENTRY_SIZE, ACK_OVERLAP_COUNT}`.
- Produces:

```rust
pub struct AckBatcher { /* entries: Vec<AckEntry>, recent: VecDeque<AckEntry> (cap 32), deadline_us: Option<u64>, outbox handle */ }
impl AckBatcher {
    pub fn new() -> Self;
    /// Queues entry; returns Some(encoded datagram) when the 64-entry cap forces an immediate flush.
    pub fn add(&mut self, entry: AckEntry, now_us: u64) -> Option<Vec<u8>>;
    pub fn poll_timeout(&self) -> Option<u64>;               // now + 5_000 µs after first add
    pub fn on_timeout(&mut self, now_us: u64) -> Option<Vec<u8>>;
    pub fn flush(&mut self) -> Option<Vec<u8>>;              // None if empty
}
```

Behavior contract (ack.ts:56-110, oracle `tests/ack.test.ts`): flush at 64 fresh entries immediately or 5 ms after first add; wire `[0x04][count u8][count × 9B entries]`, entry = frame_seq u32 LE, tile_x, tile_y, pass_idx, arrival_time_ms_lo16 u16 LE; each flush appends up to 8 most-recent previously-flushed entries (overlap tail) after the fresh ones; `recent` capped at 32; the very first batch has no overlap.

- [ ] **Step 1: Port the vitest oracle as failing tests** — all 9 cases from `ghostframe-web-client/tests/ack.test.ts`. Representative:

```rust
// tests/oracle_ack.rs
use ghostframe_client_core::ack_batcher::AckBatcher;
use ghostframe_protocol::ack::{AckEntry, AckBatch, ACK_ENTRY_SIZE, ACK_OVERLAP_COUNT};

fn e(frame_seq: u32, tile_x: u8, tile_y: u8, pass_idx: u8, ts: u16) -> AckEntry {
    AckEntry { frame_seq, tile_x, tile_y, pass_idx, arrival_time_ms_lo16: ts }
}

#[test]
fn encodes_msg_type_and_fields() {
    let mut b = AckBatcher::new();
    assert!(b.add(e(0x12345678, 3, 7, 13, 0), 0).is_none());
    let d = b.flush().unwrap();
    assert_eq!(d[0], 0x04);
    assert_eq!(d[1], 1);
    assert_eq!(&d[2..6], &[0x78, 0x56, 0x34, 0x12]); // LE
    assert_eq!((d[6], d[7], d[8]), (3, 7, 13));
    assert_eq!(&d[9..11], &[0, 0]);
}

#[test]
fn flushes_at_max_entries_without_timer() {
    let mut b = AckBatcher::new();
    let mut sent = None;
    for i in 0..64u32 { if let Some(d) = b.add(e(i, 0, 0, 0, 0), 0) { sent = Some(d); } }
    let d = sent.expect("64th add flushes");
    assert_eq!(d[1], 64);
    assert_eq!(d.len(), 2 + 64 * ACK_ENTRY_SIZE);
}

#[test]
fn overlap_appends_up_to_eight_prior_entries() {
    let mut b = AckBatcher::new();
    for i in 0..5u32 { b.add(e(i, 0, 0, 0, 0), 0); }
    b.flush().unwrap();
    for i in 0..3u32 { b.add(e(100 + i, 0, 0, 0, 0), 0); }
    let d = b.flush().unwrap();
    let count = d[1] as usize;
    assert_eq!(count, 3 + 5.min(ACK_OVERLAP_COUNT)); // 3 fresh + 5 overlap
    let batch = AckBatch::decode(&d).unwrap();
    assert_eq!(batch.entries[0].frame_seq, 100); // fresh first
}

#[test]
fn timer_flush_after_5ms() {
    let mut b = AckBatcher::new();
    b.add(e(1, 2, 3, 4, 0), 1_000);
    assert_eq!(b.poll_timeout(), Some(6_000)); // 1000µs + 5ms
    assert!(b.on_timeout(5_999).is_none());    // not yet due
    let d = b.on_timeout(6_000).unwrap();
    assert_eq!(d[1], 1);
    assert_eq!(b.poll_timeout(), None);
}
```

Also port: round-trips arrivalTimeMsLo16 (0xABCD), empty flush → None, overlap caps at 8 after 20 single-entry flushes (last batch = 1 fresh + 8 overlap, fresh entry first, overlap = the 8 most recent), no overlap on very first batch.

- [ ] **Step 2: Run to verify failure** — `cargo test -p ghostframe-client-core --test oracle_ack` → FAIL.

- [ ] **Step 3: Implement `ack_batcher.rs`** mirroring ack.ts:56-110. Reuse `ghostframe_protocol::ack::AckBatch::encode` for the wire bytes (it already implements the 0x04/count/9B-LE layout the server decodes); the batcher owns batching, the 5 ms deadline, and the overlap tail (fresh entries first, then `recent.iter().rev().take(8).rev()`; after flush push fresh entries into `recent`, truncate front to 32).

- [ ] **Step 4: Run tests** — PASS. **Step 5: Commit** — `git commit -am "feat(client-core): AckBatcher with overlap tail, ported ack.ts oracle"`

---

### Task 5: NackBatcher

**Files:**
- Create: `src/nack_batcher.rs`, `tests/oracle_nack.rs`

**Interfaces:**
- Consumes: `ghostframe_protocol::protocol::{TileNackEnvelope, TILE_NACK_ENVELOPE}`.
- Produces:

```rust
pub struct NackEntry { pub frame_seq: u32, pub tile_x: u8, pub tile_y: u8, pub pass_idx: u8, pub frag_idx: u8 }
pub struct NackBatcher { /* … */ }
impl NackBatcher {
    pub fn new() -> Self;
    pub fn add(&mut self, entry: NackEntry, now_us: u64) -> Option<Vec<u8>>; // flush at 64
    pub fn poll_timeout(&self) -> Option<u64>;   // first-add + 5ms
    pub fn on_timeout(&mut self, now_us: u64) -> Option<Vec<u8>>;
    pub fn flush(&mut self) -> Option<Vec<u8>>;
}
```

Wire (nack.ts:31-43 / protocol.rs:702-757): `[0x05][count][count × 8B]`, entry = frame_seq u32 **LE**, tile_x, tile_y, pass_idx, frag_idx.

- [ ] **Step 1: Port the 4 vitest cases from `tests/nack.test.ts`** — same structure as Task 4. Exact-bytes case:

```rust
#[test]
fn encodes_8_byte_entries_with_0x05_envelope() {
    let mut b = NackBatcher::new();
    b.add(NackEntry { frame_seq: 0x01020304, tile_x: 5, tile_y: 6, pass_idx: 7, frag_idx: 9 }, 0);
    let d = b.flush().unwrap();
    assert_eq!(&d[..2], &[0x05, 1]);
    assert_eq!(&d[2..6], &[0x04, 0x03, 0x02, 0x01]); // LE
    assert_eq!(&d[6..10], &[5, 6, 7, 9]);
}
```

Plus: flush at 64, timer flush after 5 ms, no flush when empty. Note: `TileNackEnvelope::encode` in ghostframe-protocol produces this layout — reuse it.

- [ ] **Step 2: FAIL** → **Step 3: implement** → **Step 4: PASS** → **Step 5: Commit** `"feat(client-core): NackBatcher, ported nack.ts oracle"`

---

### Task 6: DecodeErrorBatcher, LossTracker, feedback/Hello encoders

**Files:**
- Create: `src/decode_error_batcher.rs`, `src/loss_tracker.rs`, `tests/oracle_decode_error_batcher.rs`, `tests/oracle_feedback.rs`

**Interfaces:**
- Consumes: `Event::DecodeError` data, `ghostframe_protocol::feedback::ReceiverFeedback`.
- Produces:

```rust
pub struct DecodeErrorBatcher { /* per-key last-emit map, window token count */ }
impl DecodeErrorBatcher {
    pub fn new() -> Self;
    /// Returns the 5-byte stream message [0x04, codec, tile_x, tile_y, code] when allowed, None when rate-limited.
    pub fn report(&mut self, codec: Codec, tile_x: u8, tile_y: u8, code: DecodeErrorCode, now_us: u64) -> Option<Vec<u8>>;
}

pub struct LossTracker { /* received, lost, recovered_fec, last_datagram_us, suspension */ }
impl LossTracker {
    pub fn new() -> Self;
    pub fn on_datagram(&mut self, now_us: u64);          // suspension if gap > 100ms
    pub fn on_stale_tile(&mut self, expected: usize, received: usize);
    pub fn on_fec_recovery(&mut self);
    /// Encode ReceiverFeedback (22B, BE) and reset counters. now_ns = now_us * 1000.
    pub fn encode_feedback(&mut self, now_us: u64) -> Vec<u8>;
}

pub fn encode_hello(indices_raw: bool, supports_h264: bool) -> Vec<u8>; // [0x03, caps]
```

Contracts: decode_error_batcher.ts (dedup per (codec,tile_x,tile_y) within 1000 ms; global cap 32 per rolling 1000 ms window, replenished after 1001 ms); feedback.ts (Hello caps bit0=indicesRaw bit1=h264; DECODE_ERROR = `[0x04, codec, x, y, code]`, 5 bytes, stream not datagram).

- [ ] **Step 1: Port oracles** — all 6 cases from `tests/decode_error_batcher.test.ts` (advance a fake `now_us`: 500 ms dup dropped, 1001 ms allowed again, 10 distinct keys pass, 40 distinct → 32, replenish after 1001 ms) and all 6 from `tests/feedback.test.ts`:

```rust
#[test]
fn hello_both_caps() { assert_eq!(encode_hello(true, true), vec![0x03, 0x03]); }

#[test]
fn decode_error_is_five_bytes() {
    let mut b = DecodeErrorBatcher::new();
    let m = b.report(Codec::PalRle, 7, 13, DecodeErrorCode::ThinUncachedPalette, 0).unwrap();
    assert_eq!(m, vec![0x04, 2, 7, 13, 3]);
}
```

Add ReceiverFeedback round-trip: `LossTracker` with 3 received / 2 lost / 1 recovered at now_us=5_000_000 encodes to 22 bytes that `ReceiverFeedback::decode` reads back with `timestamp_ns == 5_000_000_000`, and counters reset after encode.

- [ ] **Step 2: FAIL** → **Step 3: implement** (reuse `ReceiverFeedback::encode`) → **Step 4: PASS** → **Step 5: Commit** `"feat(client-core): decode-error batcher, loss tracker, hello/feedback encoders"`

---

### Task 7: PaletteShadow + PalRle prevalidate/decode to RGBA

**Files:**
- Create: `src/palette_shadow.rs`, `src/pal_rle_decode.rs`, `tests/oracle_palette_shadow.rs`, `tests/oracle_prevalidate.rs`

**Interfaces:**
- Consumes: `ghostframe_protocol::codec::pal_rle::{PaletteEntry, decode_pal_rle}` (pixels), `DecodeErrorCode`.
- Produces:

```rust
pub struct PaletteShadow { /* counts: [u8; 256] */ }
impl PaletteShadow {
    pub fn new() -> Self;
    pub fn has(&self, id: u8) -> bool;
    pub fn count(&self, id: u8) -> u8;
    pub fn put(&mut self, id: u8, count: u8);
    pub fn clear(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PalRleVariant { Bundled, Thin, IndicesRaw }

pub struct PrevalidatedPalRle {
    pub variant: PalRleVariant,
    pub palette_id: u8,
    pub count: u8,
    pub indices: Vec<u8>,                 // 512 bytes, 2 pixels/byte, low nibble first
    pub palette_upsert: Option<Vec<u8>>,  // count*4 BGRA bytes for Bundled
}

/// Port of prevalidatePalRle (prevalidate.ts). Validates + expands; updates nothing.
pub fn prevalidate_pal_rle(payload: &[u8], shadow: &PaletteShadow) -> Result<PrevalidatedPalRle, DecodeErrorCode>;

/// Full pixel decode: prevalidate, apply upsert to `palettes`, expand to 4096-byte RGBA.
/// BGRA→RGBA swizzle happens here; alpha forced to 255.
pub fn decode_pal_rle_tile(
    payload: &[u8], shadow: &mut PaletteShadow,
    palettes: &mut [[ [u8; 4]; 16]; 256],
) -> Result<Vec<u8>, DecodeErrorCode>;
```

Payload layouts (prevalidate.ts): flags byte bit0=bundled, bit1=indicesRaw → Bundled `[0x01, id, count(1..=16), count×4 BGRA, rle…]`, Thin `[0x00, id, rle…]`, IndicesRaw `[0x02, id, 512 index bytes]`. RLE byte `(idx<<4)|(run-1)`, must expand to exactly 1024 pixels packed into 512 bytes low-nibble-first. Thin/IndicesRaw with `!shadow.has(id)` → `ThinUncachedPalette`.

- [ ] **Step 1: Port oracles** — 4 palette_shadow cases + all 11 prevalidate cases. Representative:

```rust
#[test]
fn bundled_count_17_out_of_range() {
    assert_eq!(prevalidate_pal_rle(&[0x01, 5, 17], &PaletteShadow::new()),
               Err(DecodeErrorCode::CountOutOfRange));
}

#[test]
fn bundled_one_color_expands_all_same() {
    let mut p = vec![0x01u8, 5, 1, 0x11, 0x22, 0x33, 0x00];
    p.extend(std::iter::repeat(0x0F).take(64)); // 64 runs of 16 × index 0
    let v = prevalidate_pal_rle(&p, &PaletteShadow::new()).unwrap();
    assert_eq!(v.variant, PalRleVariant::Bundled);
    assert_eq!((v.palette_id, v.count), (5, 1));
    assert_eq!(v.indices.len(), 512);
    assert!(v.indices.iter().all(|&b| b == 0));
}

#[test]
fn alternating_indices_pack_low_nibble_first() {
    let mut p = vec![0x00u8, 7]; // thin
    let mut shadow = PaletteShadow::new(); shadow.put(7, 2);
    p.extend((0..1024).map(|i| ((i as u8 & 1) << 4) | 0)); // run-1 runs alternating idx 0/1
    let v = prevalidate_pal_rle(&p, &shadow).unwrap();
    assert!(v.indices.iter().all(|&b| b == 0x10)); // (pixel1<<4)|pixel0
}
```

Add a pixel test for `decode_pal_rle_tile`: bundled 2-color payload → 4096-byte RGBA where pixel0 = palette color 0 swizzled BGRA→RGBA with alpha 255.

- [ ] **Step 2: FAIL** → **Step 3: implement**. Prevalidation is a fresh port of prevalidate.ts:1-148 (error-code granularity matters and differs from `pal_rle::PalRleDecodeError`); pixel expansion can then index the palette directly from `indices` (do NOT call `codec::pal_rle::decode_pal_rle` — its payload contract overlaps but error codes differ; use it only in tests as a cross-check where variants align). **Step 4: PASS** → **Step 5: Commit** `"feat(client-core): palette shadow + pal-rle prevalidate/decode, ported oracles"`

---

### Task 8: CDF53 prevalidate + coverage tracker

**Files:**
- Create: `src/cdf53_prevalidate.rs`, `src/cdf53_coverage.rs`, `tests/oracle_prevalidate_cdf53.rs`, `tests/oracle_cdf53_coverage.rs`
- Test fixture: `ghostframe-e2e/src/harness/fixtures/cdf53_fixture.json` (existing; read at test time via `include_str!` with a relative path or `env!("CARGO_MANIFEST_DIR")`)

**Interfaces:**
- Consumes: `ghostframe_protocol::codec::cdf53::rle_decode`, `DecodeErrorCode`.
- Produces:

```rust
pub struct PrevalidatedCdf53 { pub generation: u8, pub pass_idx: u8, pub bit_planes: Vec<u8> } // 384 = 3ch × 128B (B,G,R order)
pub fn prevalidate_cdf53(payload: &[u8], generation: u8, pass_idx: u8) -> Result<PrevalidatedCdf53, DecodeErrorCode>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CoverageEntry { pub generation: u8, pub frame_seq: u32, pub pass_mask: u16, pub nacked_mask: u16, pub last_change_us: u64 }
pub struct ArrivalOutcome { pub entry: CoverageEntry, pub nack_passes: Vec<u8> }
pub fn apply_cdf53_arrival(prev: Option<CoverageEntry>, generation: u8, pass_idx: u8, frame_seq: u32, now_us: u64, prevalidation_ok: bool) -> ArrivalOutcome;
```

Contracts: prevalidate_cdf53.ts:28-89 (per-channel `[u16 BE len][rle]` → exactly 128 B each; pass_idx ≥ 14 → `Cdf53BadPass`; truncation → `Cdf53Truncated`; wrong decoded length → `Cdf53RleLength`) and cdf53_coverage.ts:46-104 (fresh entry on new/differing generation; failure NACKs failing pass once via nacked_mask, does not set pass_mask, does not advance last_change; success sets pass_mask bit, advances last_change only on growth, and on existing generation gap-detects `((1<<pass)-1) & !pass_mask & !nacked_mask` → NACK list, ascending).

- [ ] **Step 1: Port oracles** — all 5 rleDecode cases + 4 prevalidateCdf53 cases (including the full 14-pass × 3-channel fixture walk asserting each decoded plane equals `bit_planes_per_pass[pass][ch]`), and all 11 cdf53_coverage cases. Representative coverage case:

```rust
#[test]
fn gap_detection_nacks_missing_lower_passes() {
    let prev = CoverageEntry { generation: 3, frame_seq: 5, pass_mask: 0b1, nacked_mask: 0, last_change_us: 42 };
    let out = apply_cdf53_arrival(Some(prev), 3, 4, 9, 100, true);
    assert_eq!(out.nack_passes, vec![1, 2, 3]);
    assert_eq!(out.entry.pass_mask, 0b0010001);
    assert_eq!(out.entry.nacked_mask, 0b0001110);
}
```

Fixture parsing: add dev-dependency `serde_json = "1"` and load `concat!(env!("CARGO_MANIFEST_DIR"), "/../ghostframe-e2e/src/harness/fixtures/cdf53_fixture.json")`.

- [ ] **Step 2: FAIL** → **Step 3: implement both modules** → **Step 4: PASS** → **Step 5: Commit** `"feat(client-core): cdf53 prevalidation + coverage/gap-detect, ported oracles incl. Rust fixture"`

---

### Task 9: CDF53 per-tile accumulation state → RGBA

**Files:**
- Create: `src/cdf53_tile_state.rs`, `tests/cdf53_tile_state.rs`

**Interfaces:**
- Consumes: `PrevalidatedCdf53`, `codec::cdf53::{decode_passes, inverse, CDF53_PASS_COUNT}`.
- Produces:

```rust
pub struct Cdf53TileState { /* per-(tile_x,tile_y): generation, stored pass planes [Option<[u8;384]>; 14] */ }
impl Cdf53TileState {
    pub fn new() -> Self;
    /// Accumulate one accepted pass. Generation change discards prior planes (stale-generation rule).
    /// Returns the freshly reconstructed 4096-byte RGBA tile (BGR from inverse() + alpha 255).
    pub fn integrate(&mut self, tile_x: u8, tile_y: u8, entry: &PrevalidatedCdf53) -> Vec<u8>;
    pub fn reset(&mut self);
}
```

This is the CPU replacement of webgpu/cdf53.ts:342-455: keep per-tile plane storage; on generation mismatch zero the tile's state first; reconstruct by feeding all currently-held planes (in pass order, contiguous-from-0 not required — `decode_passes` takes the slice of planes present; check its exact contract in codec/cdf53.rs:628 and store/reconstruct accordingly: it accepts `&[&[u8]]` of the first K passes, so reconstruct from the contiguous prefix of received passes and ignore holes beyond the first gap).

- [ ] **Step 1: Write failing roundtrip tests** using the encoder as oracle:

```rust
use ghostframe_protocol::codec::cdf53;

#[test]
fn fourteen_passes_reconstruct_exactly() {
    // Build a deterministic 32x32 BGRA tile, forward-transform + encode passes with server code.
    let tile: Vec<u8> = (0..4096).map(|i| (i * 7 % 251) as u8).collect();
    let coeffs = cdf53::forward(&tile);
    let passes = cdf53::encode_passes(&coeffs);
    let mut st = Cdf53TileState::new();
    let mut last = Vec::new();
    for (i, p) in passes.iter().enumerate() {
        let pre = prevalidate_cdf53(p, 1, i as u8).unwrap();
        last = st.integrate(0, 0, &pre);
    }
    let expected_bgr = cdf53::inverse(&coeffs);
    for px in 0..1024 {
        assert_eq!(&last[px*4..px*4+3],
                   &[expected_bgr[px*3+2], expected_bgr[px*3+1], expected_bgr[px*3]]); // RGB from BGR
        assert_eq!(last[px*4+3], 255);
    }
}

#[test]
fn generation_bump_discards_stale_planes() {
    // integrate passes 0..3 at gen 1, then pass 0 at gen 2:
    // result must equal a fresh gen-2 single-pass reconstruction, not a blend.
}
```

(Adjust `forward`/`encode_passes` call signatures to the real ones in codec/cdf53.rs — verify before writing.)

- [ ] **Step 2: FAIL** → **Step 3: implement** → **Step 4: PASS** → **Step 5: Commit** `"feat(client-core): CPU cdf53 tile accumulation and RGBA reconstruction"`

---

### Task 10: Parity decoders (wire-seq FEC + legacy fragment FEC)

**Files:**
- Create: `src/parity_decoder.rs`, `src/fragment_parity.rs`, `tests/oracle_parity_decoder.rs`

**Interfaces:**
- Consumes: `ghostframe_protocol::protocol::TileParityEnvelope`.
- Produces:

```rust
pub struct ParityDecoder { /* window: ordered map wire_seq → Vec<u8> (cap from new), pending parities */ }
impl ParityDecoder {
    pub fn new(window_capacity: usize) -> Self;  // main.ts uses 40
    /// Insert a received source datagram; may unlock a buffered parity → returns recovered source datagram.
    pub fn record_source(&mut self, wire_seq: u32, bytes: &[u8]) -> Option<Vec<u8>>;
    pub fn receive_parity(&mut self, env: &TileParityEnvelope) -> Option<Vec<u8>>;
    pub fn has_source(&self, wire_seq: u32) -> bool;
}

pub struct FragmentParity { /* legacy per-tile-assembly XOR groups, fec.ts */ }
impl FragmentParity {
    pub fn store(&mut self, key: TileKey, parity_payload: &[u8]);
    /// Try to recover fragment `missing_idx` given current fragments (None entries missing), k=4 groups.
    pub fn try_recover(&self, key: TileKey, fragments: &[Option<Vec<u8>>]) -> Option<(usize, Vec<u8>)>;
    pub fn remove(&mut self, key: &TileKey);
}
```

Contracts: parity_decoder.ts (K-group keyed by `group_first_wire_seq`; recover only when exactly one of the K sources missing; XOR right-aligned, output length = max; parity buffered when >1 missing and replayed on later `record_source`; insertion-order eviction at capacity) and fec.ts (parity payload `[group_start u16 BE][group_len u8][xor]`, groups of k=4 within a tile's fragments, zero-pad from index 0).

- [ ] **Step 1: Port all 5 parity_decoder.test.ts cases.** Build sources exactly like the vitest `fakeSource` helper: 25-byte datagrams (16B header with `frame_seq | 0x8000_0000`, frag_idx=0, frag_total=1, the wire_seq, timestamp 0; 8B tile header; 1 payload byte).

```rust
#[test]
fn recovers_single_missing_source() {
    let sources: Vec<Vec<u8>> = (0..10).map(|i| fake_source(i as u32, i as u8)).collect();
    let parity = xor_right_aligned(&sources);
    let mut d = ParityDecoder::new(64);
    for (i, s) in sources.iter().enumerate() { if i != 5 { d.record_source(i as u32, s); } }
    let env = TileParityEnvelope { group_first_wire_seq: 0, k: 10, parity_idx: 0,
        group_first_payload_len: sources[0].len() as u16, parity_payload: parity };
    assert_eq!(d.receive_parity(&env).unwrap(), sources[5]);
}
```

- [ ] **Step 2: FAIL** → **Step 3: implement both** → **Step 4: PASS** → **Step 5: Commit** `"feat(client-core): wire-seq parity decoder + legacy fragment FEC, ported oracle"`

---

### Task 11: Reassembly — handleSourceTileDatagram, eviction, sentinel, dispatch

**Files:**
- Create: `src/reassembly.rs`, `tests/oracle_tile_key.rs`, `tests/reassembly.rs`
- Modify: `src/lib.rs` — wire everything built so far into `ClientCore::handle_datagram`

**Interfaces:**
- Consumes: everything from Tasks 4–10 plus `ghostframe_protocol::protocol::{decode_tile_datagram, fragment_tile, TileFragmentInputs, Codec, TILE_DATAGRAM_FLAG, TileParityEnvelope, classify_inbound}`.
- Produces: the real `ClientCore::handle_datagram(&mut self, bytes, now_us) -> Vec<Event>` implementing the exact main.ts order of operations:

1. Empty → ignore. First byte 0x04 (and not a tile datagram, i.e. bit 31 of the first u32 unset) → parity envelope: `parity_decoder.receive_parity`; recovered datagram is recursively processed as a source tile datagram; count `on_fec_recovery`.
2. `len < 20` → ping/pong, ignore. `len < 24` → drop.
3. Not `is_tile_datagram` → frame path (Task 12).
4. Tile datagram: `parity_decoder.record_source(wire_seq, bytes)`; a recovered datagram is processed too. Then per main.ts:1099-1276:
   - decode headers, mask flag off frame_seq, `loss_tracker.on_datagram(now_us)`;
   - ACK immediately unless sentinel or `codec == Cdf53` (`ack_batcher.add` with `arrival_time_ms_lo16 = ((now_us / 1000) & 0xFFFF) as u16`, frame_seq re-flagged with `TILE_DATAGRAM_FLAG`);
   - advance `latest_frame_seq`; evict assemblies with `frame_seq < latest_frame_seq - 2` (incomplete ones feed `loss_tracker.on_stale_tile(frag_total, received)`; also drop their fragment-parity records);
   - `frag_idx >= frag_total` → legacy parity: store in `FragmentParity`; if the live assembly is missing exactly one fragment, recover and finish; return;
   - sentinel `(0xFF,0xFF)` with ≥8B payload → `Event::FrameDimensions` (u32 BE w,h); return;
   - `Codec::Skip` → return;
   - insert fragment (ignore duplicates), on `received == frag_total - 1` try `FragmentParity`, on `received == frag_total` → finish: concat payload, dispatch by codec:
     - `Raw` → `Event::TileReady` (payload is BGRA: swizzle to RGBA),
     - `Solid` (4B payload) → expand to 4096-byte RGBA tile,
     - `PalRle` → `decode_pal_rle_tile`; error → `DecodeErrorBatcher.report` (queue Stream output) + `Event::DecodeError`,
     - `Cdf53` → `prevalidate_cdf53` → `apply_cdf53_arrival` (failure: report+NACK per outcome; success: `Cdf53TileState::integrate` → `Event::TileReady`, then deferred ACK add) — NACK list from the outcome goes to `nack_batcher` with `frag_idx = 0xFF`... **verify**: in main.ts, coverage NACKs are enqueued with which fragIdx? Check main.ts:640-700 before coding; use the same value.

- [ ] **Step 1: Port `tests/tile_key.test.ts`** as bucket-isolation tests on `TileKey` (two passes of same tile occupy distinct HashMap entries; distinct frame_seq distinct) — plus reassembly tests built with **server-side `fragment_tile`** as the generator:

```rust
fn tile_datagrams(frame_seq: u32, x: u8, y: u8, codec: Codec, pass: u8, payload: &[u8], mtu_payload: usize) -> Vec<Vec<u8>> {
    ghostframe_protocol::protocol::fragment_tile(
        &TileFragmentInputs { frame_seq, tile_x: x, tile_y: y, codec, generation: 1, pass, timestamp_us: 0 },
        payload, mtu_payload)
}

#[test]
fn solid_tile_roundtrip_single_fragment() {
    let mut core = test_core();
    let dgs = tile_datagrams(1, 3, 4, Codec::Solid, 0, &[0x11, 0x22, 0x33, 0xFF], 1200);
    let evs = core.handle_datagram(&dgs[0], 1_000);
    match &evs[..] {
        [Event::TileReady { frame_seq: 1, tile_x: 3, tile_y: 4, rgba }] => {
            assert_eq!(&rgba[..4], &[0x33, 0x22, 0x11, 255]); // BGRA→RGBA
            assert_eq!(rgba.len(), 4096);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn multi_fragment_raw_tile_completes_out_of_order() { /* 4096B raw payload, mtu 1200 → 4 frags, deliver 3,0,2,1, assert one TileReady with exact bytes */ }

#[test]
fn stale_assembly_evicted_at_threshold_2() { /* start frame 1 tile incomplete; deliver frames 2,3,4 tiles; assert frame-1 bucket gone (no TileReady when its last fragment arrives late) and feedback lost counter grew */ }

#[test]
fn sentinel_emits_frame_dimensions() { /* build_frame_dimensions_datagram(7, 0, 1920, 1080) → Event::FrameDimensions{1920,1080} */ }

#[test]
fn cdf53_ack_deferred_until_prevalidation() { /* deliver a valid cdf53 pass datagram; poll_transmit must yield no ACK before handle_datagram returns, and the ACK datagram after; deliver a corrupt cdf53 payload; assert Stream decode-error output and no ACK for it */ }
```

- [ ] **Step 2: FAIL** → **Step 3: implement `reassembly.rs` + wire ClientCore** (fields: assemblies map, parity decoder(40), fragment parity, ack/nack/error batchers, loss tracker, palette shadow+tables, cdf53 coverage map + tile state, latest_frame_seq, outbox). `poll_transmit` drains outbox; batcher flushes push into outbox. → **Step 4: PASS all crate tests** → **Step 5: Commit** `"feat(client-core): full datagram reassembly pipeline with codec dispatch"`

---

### Task 12: Timers (assembly-timeout NACK, tail sweep, feedback interval) + H.264 frame path

**Files:**
- Create: `src/frame_assembly.rs`, `tests/timers.rs`, `tests/frame_path.rs`
- Modify: `src/lib.rs` (`poll_timeout`/`on_timeout`), `src/reassembly.rs`

**Interfaces:**
- Consumes: Tasks 4–11.
- Produces: `poll_timeout()` = min of (ack deadline, nack deadline, next assembly-timeout scan, next tail sweep, next feedback emit at 100 ms cadence). `on_timeout(now_us)`:
  - **Assembly timeout scan** (main.ts:756-769): any assembly with `now_us - partial_since_us >= 30_000` → NACK each missing frag_idx once (dedup via per-assembly `nacked_frag_idxs`).
  - **Tail sweep** (main.ts tick, every 500 ms): coverage entries with `pass_mask != (1<<14)-1` and `now_us - last_change_us >= 1_500_000` → re-NACK all missing passes and clear those bits from `nacked_mask`.
  - **Feedback emit**: every 100 ms push `PollOutput::Stream(loss_tracker.encode_feedback(now_us))`.
- `frame_assembly.rs`: port of main.ts:1326-1399 — H.264 full-frame fragments (`FrameHeader`, 14B); drop `frame_seq < latest_full_frame_seq - 2`; evict old; skip parity frags (`frag_idx >= frag_total`); on completion emit `Event::NeedsH264 { frame_seq, timestamp_us, is_keyframe, payload }`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn assembly_timeout_nacks_missing_fragments_once() {
    let mut core = test_core();
    let dgs = tile_datagrams(1, 0, 0, Codec::Raw, 0, &vec![7u8; 4096], 1200); // 4 frags
    core.handle_datagram(&dgs[0], 0);
    // advance past 30ms scan deadline
    let deadline = core.poll_timeout().unwrap();
    core.on_timeout(deadline.max(31_000));
    let nack = drain_datagrams(&mut core).into_iter().find(|d| d[0] == 0x05).expect("nack sent");
    assert_eq!(nack[1], 3); // frags 1,2,3 missing
    // second scan: no duplicate NACKs
    core.on_timeout(70_000);
    assert!(drain_datagrams(&mut core).into_iter().all(|d| d[0] != 0x05));
}

#[test]
fn feedback_emitted_every_100ms() { /* on_timeout at 100ms, 200ms → two Stream messages starting 0x01, 22 bytes */ }

#[test]
fn tail_sweep_renacks_stalled_cdf53_tile_after_1500ms() { /* one valid pass 0 at t=0, on_timeout sweeps at 1500ms+ → NACKs for passes 1..13 */ }

#[test]
fn h264_frame_reassembles_to_needs_h264() { /* fragment_frame from ghostframe-protocol, deliver all frags → Event::NeedsH264 with exact payload, is_keyframe flag */ }
```

- [ ] **Step 2: FAIL** → **Step 3: implement** → **Step 4: PASS** → **Step 5: Commit** `"feat(client-core): timer-driven NACK scans, tail sweep, periodic feedback, h264 frame path"`

---

### Task 13: Input encoding + keysym mapping

**Files:**
- Create: `src/input.rs`, `tests/oracle_input.rs`

**Interfaces:**
- Consumes: nothing internal (pure functions).
- Produces:

```rust
pub const INPUT_MSG_TYPE: u8 = 0x05;
pub fn encode_pointer_move(x: i16, y: i16) -> [u8; 6];               // [0x05,0x01,x BE,y BE]
pub fn encode_pointer_button(x: i16, y: i16, button: u8, down: bool) -> [u8; 8];
pub fn encode_wheel(dx: i16, dy: i16) -> [u8; 6];
pub fn encode_key_down(keysym: u32) -> [u8; 6];                      // [0x05,0x04,keysym BE]
pub fn encode_key_up(keysym: u32) -> [u8; 6];
/// Port of keymap.ts: browser KeyboardEvent.key string → X11 keysym. None for dead/unknown keys.
pub fn key_to_keysym(key: &str) -> Option<u32>;
```

- [ ] **Step 1: Port all input.test.ts cases** (exact byte arrays: `encode_pointer_move(400,-2)` → `[0x05,0x01,0x01,0x90,0xff,0xfe]`; keysyms: `"Enter"`→0xff0d, `"ArrowUp"`→0xff52, `"F1"`→0xffbe, `"a"`→0x61, `"ñ"`→0xf1, `"中"`→`0x01000000|0x4e2d`, `" "`→0x20, `"Dead"`/`"Unidentified"`/`"NotARealKey"`→None).

- [ ] **Step 2: FAIL** → **Step 3: implement** (port keymap.ts's table + the Latin-1/BMP fallback rules: single-char keys → codepoint if <0x100 else `0x01000000|cp`) → **Step 4: PASS** → **Step 5: Commit** `"feat(client-core): input event encoders and keysym mapping, ported oracle"`

---

### Task 14: Loopback integration tests + adversarial robustness

**Files:**
- Create: `tests/loopback.rs`, `tests/robustness.rs`

**Interfaces:**
- Consumes: full `ClientCore` + `ghostframe-protocol` encode side (`fragment_tile`, `codec::*::encode_*`, `build_parity_datagrams`, `TileParityEnvelope`).
- Produces: the crate's acceptance evidence. No new API.

- [ ] **Step 1: Write loopback tests** (server-encode → client-core-decode, no network):

```rust
#[test]
fn solid_palrle_cdf53_full_frame_converges_pixel_exact() {
    // Use ghostframe-e2e's lossless_golden layout re-expressed here: build 256x96 frame
    // (8x3 tiles: 2 cols solid, 3 cols palrle-style 4-color, 3 cols gradient),
    // encode each tile with the matching codec from ghostframe_protocol::codec,
    // fragment all datagrams, deliver in order, collect TileReady events,
    // composite into a framebuffer, assert byte-equality with the source frame
    // (cdf53 tiles: after all 14 passes, assert exact reconstruction).
}

#[test]
fn loss_with_nack_replay_converges() {
    // Deliver with every 3rd datagram dropped (deterministic), collect NACK datagrams
    // from poll_transmit/on_timeout, decode them with TileNackEnvelope::decode,
    // re-deliver exactly the NACKed fragments, assert eventual pixel-exact convergence.
}

#[test]
fn parity_group_recovers_without_retransmit() {
    // 10 single-fragment tiles sharing consecutive wire_seqs + one TileParityEnvelope;
    // drop one source; assert all 10 TileReady with correct pixels and
    // feedback counter datagrams_recovered_fec == 1.
}

#[test]
fn duplicated_and_reordered_delivery_is_idempotent() {
    // Shuffle datagrams deterministically (seeded), deliver each twice;
    // assert the same TileReady set as in-order single delivery, no duplicate events.
}
```

- [ ] **Step 2: Write robustness tests** (proptest):

```rust
proptest! {
    #[test]
    fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..2000)) {
        let mut core = test_core();
        let _ = core.handle_datagram(&data, 0);
        while core.poll_transmit(0).is_some() {}
    }

    #[test]
    fn truncated_valid_datagrams_never_panic(cut in 0usize..100) {
        // take a valid fragmented cdf53 datagram, truncate at `cut`, feed it
    }

    #[test]
    fn bitflipped_datagrams_never_panic_or_emit_oversized_tiles(pos in 0usize..200, bit in 0u8..8) {
        // flip one bit in a valid datagram; handle; any TileReady must have rgba.len() == 4096
    }
}
```

- [ ] **Step 3: Run** `cargo test -p ghostframe-client-core` → PASS (fix any panics found — treat each as a bug in the port, not the test). **Step 4: Commit** `"test(client-core): loopback encode/decode integration + proptest robustness"`

---

### Task 15: wasm32 compile gate + CI

**Files:**
- Create: `.github/workflows/client-core.yml`
- Modify: none

**Interfaces:**
- Consumes: finished crates.
- Produces: CI gate keeping both crates wasm-clean for sub-project 4.

- [ ] **Step 1: Verify wasm build locally**

Run:
```bash
rustup target add wasm32-unknown-unknown
cargo build -p ghostframe-protocol -p ghostframe-client-core --target wasm32-unknown-unknown
```
Expected: clean build. Any failure means a stray `std::time`/native dep sneaked in — fix it in the offending module (the Global Constraints forbid them).

- [ ] **Step 2: Add CI workflow**

```yaml
# .github/workflows/client-core.yml
name: client-core
on:
  push: { branches: [master] }
  pull_request:
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: wasm32-unknown-unknown }
      - run: cargo test -p ghostframe-protocol -p ghostframe-client-core
      - run: cargo build -p ghostframe-protocol -p ghostframe-client-core --target wasm32-unknown-unknown
```

- [ ] **Step 3: Full verification**

Run: `cargo test --workspace && (cd ghostframe-web-client && npx vitest run)`
Expected: everything green — the extraction did not disturb server or web client.

- [ ] **Step 4: Commit** — `git add -A && git commit -m "ci: test + wasm32 build gate for ghostframe-protocol and ghostframe-client-core"`

---

## Deferred / explicitly out of scope (later sub-projects)

- QUIC/WebTransport client transport, netsim impairment, browserless e2e binary → sub-project 2.
- BWE/pacing → sub-project 3.
- wasm-bindgen bindings + web-client cutover (deleting decoder.ts/ack.ts/nack.ts/fec.ts/parity_decoder.ts/prevalidate*.ts/cdf53_coverage.ts/palette_shadow.ts/decode_error_batcher.ts) → sub-project 4. Until then the TS modules stay authoritative in production; this crate must match them, not replace them yet.
- `renderer_idle_skip`, `solid_pack`, `bootstrap`, `diagnostics` vitest suites: renderer/browser glue, not ported.
- LZ4 payload support: the wire bit exists but the server never sets it (protocol.rs: always encoded false); the core should surface `lz4=true` as a DecodeError-free drop with a debug assert, nothing more.
