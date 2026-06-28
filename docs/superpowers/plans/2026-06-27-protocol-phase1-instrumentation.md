# Protocol Redesign Phase 1 — Instrumentation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add congestion-control observability (server bandwidth estimator, per-tier latency/bandwidth, delay gradient) without changing emission behavior. Phase 1 of the protocol redesign defined in `docs/superpowers/specs/2026-06-27-protocol-redesign-design.md`.

**Architecture:** Repurpose the existing 4-byte `DatagramHeader.timestamp_us` field for tile datagrams to carry server emit-time microseconds (was frame capture time). Bump the ACK envelope from 7 → 9 bytes per entry to carry receiver arrival time. Add `str0m` as a dependency and wire its `bwe::Bwe` module (GoogCC port) in passive mode — it polls a bandwidth estimate but doesn't drive emission yet. All counters reported in journal logs every 1 s server-side and the existing 2 s client stats line.

**Tech Stack:** Rust (ghostframe-lib transport + io_bridge), TypeScript (ghostframe-web-client), str0m crate v0.21.

---

## File Structure

**Modify:**
- `ghostframe-lib/src/transport/ack.rs` — `ACK_ENTRY_SIZE` 7→9, encode/decode new arrival-time field, bump `ACK_BATCH_MSG_TYPE` to 0x04
- `ghostframe-lib/src/transport/protocol.rs` — docstring update for `DatagramHeader.timestamp_us` (semantics change for tile datagrams)
- `ghostframe-lib/src/transport/io_bridge.rs` — stamp server emit-time at the wire-write site, add per-tier counters, parse new ACK fields, feed records into Bwe, periodic logging
- `ghostframe-lib/Cargo.toml` — add `str0m = "0.21"` dep (use `default-features = false` to avoid pulling unnecessary WebRTC pieces; we only want `bwe`)
- `ghostframe-web-client/src/ack.ts` — `ACK_ENTRY_SIZE` 7→9, encode arrival-time, bump `ACK_BATCH_MSG_TYPE` to 0x04
- `ghostframe-web-client/src/decoder.ts` — docstring update for `DatagramHeader.timestampUs`
- `ghostframe-web-client/src/main.ts` — track per-pass arrival times keyed by emitKey, populate arrival-time field in ACK send path, per-tier recv stats, log additions to periodic stats line

**Create:**
- `ghostframe-lib/src/transport/bwe.rs` — small wrapper around `str0m::bwe::Bwe` providing a stable API + integration tests with synthetic TWCC records

---

## Wire-Format Changes Summary (read this once before starting)

### Datagram header (existing, semantics change for tile datagrams)

```
[ 0.. 4] frame_seq (u32 BE)         ── tile datagrams set TILE_DATAGRAM_FLAG (bit 31)
[ 4.. 6] frag_idx (u16 BE)
[ 6.. 8] frag_total (u16 BE)
[ 8..12] wire_seq (u32 BE)
[12..16] timestamp_us (u32 BE)      ── tile datagrams: SERVER EMIT TIME (wall-clock μs, u32 wrap)
                                       frame datagrams: frame capture time (unchanged)
```

The semantics change is server-side only. Old clients that ignored the field still ignore it.

### ACK envelope (bumped from 0x03 to 0x04)

```
[ 0]      msg_type = 0x04 (was 0x03)
[ 1]      count (u8)
[ 2..]    entries, 9 bytes each (was 7):
            [+0..+4] frame_seq (u32 LE)
            [+4]     tile_x (u8)
            [+5]     tile_y (u8)
            [+6]     pass_idx (u8)
            [+7..+9] arrival_time_ms_lo16 (u16 LE)
                     ── client's pass-receive wall-clock ms, low 16 bits
                       (65.5 s wrap window; enough for any sane RTT/jitter)
```

The 1 ms resolution comfortably exceeds GCC's 12.5 ms over-use detection threshold and stays under the 4 ms inter-group spacing expected in TWCC.

---

## Task 1: ACK envelope wire format — server-side

**Files:**
- Modify: `ghostframe-lib/src/transport/ack.rs`

- [ ] **Step 1: Write the failing test (ack.rs tests module)**

Append to the `#[cfg(test)] mod tests` block:

```rust
#[test]
fn ack_envelope_v4_carries_arrival_time() {
    let entries = vec![
        AckEntry {
            frame_seq: 0x1234_5678,
            tile_x: 7,
            tile_y: 9,
            pass_idx: 3,
            arrival_time_ms_lo16: 0xABCD,
        },
        AckEntry {
            frame_seq: 0x9999_AAAA,
            tile_x: 1,
            tile_y: 2,
            pass_idx: 13,
            arrival_time_ms_lo16: 0x0010,
        },
    ];
    let batch = AckBatch { entries: entries.clone() };
    let bytes = batch.encode();
    // 2 header bytes + 2 entries × 9 bytes = 20 bytes
    assert_eq!(bytes.len(), 20);
    assert_eq!(bytes[0], ACK_BATCH_MSG_TYPE);
    assert_eq!(bytes[0], 0x04, "msg_type bumped to 0x04");
    assert_eq!(bytes[1], 2);

    let decoded = AckBatch::decode(&bytes).expect("round-trip");
    assert_eq!(decoded.entries, entries);
}
```

- [ ] **Step 2: Run, verify failing**

Run: `cargo test -p ghostframe-lib transport::ack::tests::ack_envelope_v4_carries_arrival_time`

Expected: FAIL — `arrival_time_ms_lo16` field doesn't exist; `ACK_BATCH_MSG_TYPE` is 0x03 not 0x04; size mismatch.

- [ ] **Step 3: Update constants + AckEntry struct**

In `ghostframe-lib/src/transport/ack.rs`, find:

```rust
pub const ACK_BATCH_MSG_TYPE: u8 = 0x03;
```

Replace with:

```rust
/// ACK envelope wire-format version. Bumped 0x03 → 0x04 in 2026-06-27
/// to add a 2-byte per-entry receiver-arrival-time field (low 16 bits
/// of wall-clock milliseconds) used by the server's bandwidth-estimator
/// hook. Old (0x03) clients/servers are not wire-compatible with new —
/// both sides ship in lockstep.
pub const ACK_BATCH_MSG_TYPE: u8 = 0x04;
```

Find:

```rust
pub const ACK_ENTRY_SIZE: usize = 7;
```

Replace with:

```rust
pub const ACK_ENTRY_SIZE: usize = 9;
```

Find the `AckEntry` struct (somewhere near the top of the impl block):

```rust
pub struct AckEntry {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
}
```

Replace with:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckEntry {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
    /// Low 16 bits of the client's wall-clock millisecond receive time
    /// for this pass. 65.5 s wrap; absolute clock skew doesn't matter
    /// because the BWE consumer looks at *relative* arrival differences
    /// between packets in the same envelope.
    pub arrival_time_ms_lo16: u16,
}
```

(Re-derive Debug/Clone/PartialEq/Eq if they aren't already on the struct — `cargo build -p ghostframe-lib` will tell you.)

- [ ] **Step 4: Update encode() to write the new field (little-endian, like the others)**

Find the encode loop (around `let off = 2 + i * ACK_ENTRY_SIZE;`). After the existing 7 byte writes, add:

```rust
            // Bytes [7..9]: arrival_time_ms_lo16, little-endian.
            out[off + 7] = (entry.arrival_time_ms_lo16 & 0xFF) as u8;
            out[off + 8] = ((entry.arrival_time_ms_lo16 >> 8) & 0xFF) as u8;
```

- [ ] **Step 5: Update decode() to read the new field**

Find the decode loop (around `let off = 2 + i * ACK_ENTRY_SIZE;`). After the existing 7 byte reads, add:

```rust
        let arrival_time_ms_lo16 =
            (data[off + 7] as u16) | ((data[off + 8] as u16) << 8);
```

And include `arrival_time_ms_lo16` in the `AckEntry` constructor used at decode.

- [ ] **Step 6: Verify the new test passes + existing tests still pass**

Run: `cargo test -p ghostframe-lib transport::ack`

Expected: every test PASS. Some existing tests may need their `AckEntry` literals updated to set `arrival_time_ms_lo16: 0`. Search for `AckEntry {` in the test module and add the field to each.

- [ ] **Step 7: Run the full ghostframe-lib suite to catch downstream impact**

Run: `cargo build -p ghostframe-lib && cargo test -p ghostframe-lib --lib`

Expected: clean compile, all PASS. Some `io_bridge` tests will likely fail on the AckEntry shape change — update them by adding `arrival_time_ms_lo16: 0` to each `AckEntry` literal.

- [ ] **Step 8: Commit**

```bash
git add ghostframe-lib/src/transport/ack.rs ghostframe-lib/src/transport/io_bridge.rs
git commit -m "ack: bump envelope v0x04, add 2 B arrival_time_ms_lo16 per entry"
```

---

## Task 2: ACK envelope wire format — client-side

**Files:**
- Modify: `ghostframe-web-client/src/ack.ts`
- Modify: `ghostframe-web-client/tests/ack.test.ts`

- [ ] **Step 1: Update the failing test for the new envelope**

In `ghostframe-web-client/tests/ack.test.ts`, find the test that round-trips an envelope. After the existing assertions, add an arrival-time field check. Specifically, find a test like:

```typescript
test('encodes and decodes a 1-entry envelope', () => {
  const entry = { frameSeq: 0x12345678, tileX: 7, tileY: 9, passIdx: 3 };
  // ...
});
```

Update it (and any others that construct `AckEntry`) to include `arrivalTimeMsLo16: 0xABCD` (or any test value) on the entry. Then assert that the parsed entry's `arrivalTimeMsLo16` matches.

- [ ] **Step 2: Run, verify failing**

Run: `cd ghostframe-web-client && npm test -- ack`

Expected: FAIL — the round-tripped entry won't have `arrivalTimeMsLo16` because it's not in the code yet.

- [ ] **Step 3: Update ack.ts: constants, type, encode, decode**

In `ghostframe-web-client/src/ack.ts`, find:

```typescript
export const ACK_BATCH_MSG_TYPE = 0x03;
```

Replace with:

```typescript
/**
 * ACK envelope wire-format version. Bumped 0x03 → 0x04 in 2026-06-27
 * to add a 2-byte per-entry arrival-time field used by the server's
 * bandwidth estimator. Old + new are not wire-compatible — server and
 * client ship in lockstep.
 */
export const ACK_BATCH_MSG_TYPE = 0x04;
```

Find:

```typescript
export const ACK_ENTRY_SIZE = 7;
```

Replace with:

```typescript
export const ACK_ENTRY_SIZE = 9;
```

Find the `AckEntry` interface (or type alias):

```typescript
export interface AckEntry {
  frameSeq: number;
  tileX: number;
  tileY: number;
  passIdx: number;
}
```

Replace with:

```typescript
export interface AckEntry {
  frameSeq: number;
  tileX: number;
  tileY: number;
  passIdx: number;
  /**
   * Low 16 bits of the client's wall-clock millisecond receive time
   * for this pass. 65.5 s wrap window. The server's BWE consumer looks
   * at *relative* arrival differences, not absolute time, so clock skew
   * is irrelevant.
   */
  arrivalTimeMsLo16: number;
}
```

In the encode method, find the loop body that writes entry bytes. After the existing 7 byte writes (frameSeq, tileX, tileY, passIdx), add:

```typescript
      // Bytes [7..9]: arrivalTimeMsLo16, little-endian.
      view.setUint16(off + 7, entry.arrivalTimeMsLo16 & 0xFFFF, true);
```

In the decode helper (`parseAckEnvelopeForTest` at the bottom of the file), add the read:

```typescript
    out.push({
      frameSeq: view.getUint32(off, /* littleEndian */ true),
      tileX: buf[off + 4],
      tileY: buf[off + 5],
      passIdx: buf[off + 6],
      arrivalTimeMsLo16: view.getUint16(off + 7, /* littleEndian */ true),
    });
```

- [ ] **Step 4: Update every `ackBatcher.add(...)` call site in main.ts to include arrivalTimeMsLo16**

Search:
```bash
grep -n "ackBatcher.add" ghostframe-web-client/src/main.ts
```

For each call site that constructs an `AckEntry`, add `arrivalTimeMsLo16: 0` for now (we'll populate it for real in Task 4).

Example: a current call like
```typescript
ackBatcher.add({
  frameSeq: dgramHdr.frameSeq | TILE_DATAGRAM_FLAG,
  tileX: tileHdr.tileX,
  tileY: tileHdr.tileY,
  passIdx: tileHdr.pass,
});
```
becomes
```typescript
ackBatcher.add({
  frameSeq: dgramHdr.frameSeq | TILE_DATAGRAM_FLAG,
  tileX: tileHdr.tileX,
  tileY: tileHdr.tileY,
  passIdx: tileHdr.pass,
  arrivalTimeMsLo16: 0, // Phase 1 Task 4 will populate
});
```

- [ ] **Step 5: Run web-client tests**

Run: `cd ghostframe-web-client && npm test && npm run build`

Expected: all tests PASS, build clean.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-web-client/src/ack.ts ghostframe-web-client/src/main.ts ghostframe-web-client/tests/ack.test.ts
git commit -m "web-client: ACK envelope v0x04 with arrival_time_ms_lo16"
```

---

## Task 3: Server emit timestamp in datagram header

**Files:**
- Modify: `ghostframe-lib/src/transport/protocol.rs`
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Update protocol.rs docstring (no code change, just clarify intent)**

Find the `DatagramHeader` struct definition near line 91. Update the docstring above `pub timestamp_us: u32,` to:

```rust
    /// **For frame (full-image H.264) datagrams:** capture time of the
    /// originating frame in microseconds, propagated from xdaemon. Used
    /// for end-to-end latency bench metrics.
    ///
    /// **For tile datagrams (`TILE_DATAGRAM_FLAG` set in `frame_seq`):**
    /// SERVER EMIT TIME in microseconds (wall-clock, u32 wrap window
    /// ≈ 71 min). Populated at the wire-write site in io_bridge. Used
    /// by the client's per-tier latency tracking AND echoed back to the
    /// server via the ACK envelope's arrival_time_ms_lo16 for the GCC
    /// delay-gradient estimator. Semantics changed in 2026-06-27 — was
    /// previously frame capture time for tile datagrams too, which was
    /// useless for inter-datagram timing.
    pub timestamp_us: u32,
```

- [ ] **Step 2: Add server emit-time stamping at the wire-write site**

In `ghostframe-lib/src/transport/io_bridge.rs`, find the existing function that hands bytes to quinn — search `send_datagram(`. The wire write happens inside `send_to_all_sessions`. Right before the `wt.send_datagram(conn, dg)` call, we need to overwrite bytes [12..16] of `dg` with the current wall-clock microseconds (but only when `dg` is a tile datagram — bit 31 of byte 0 set).

Find (around line 970):

```rust
                if let Err(e) = wt.send_datagram(conn, dg) {
                    self.datagram_send_errs = self.datagram_send_errs.saturating_add(1);
```

Immediately BEFORE this `if let Err`, insert a server-emit-time stamp. Because `dg` is `&[u8]`, we need to wrap it in a buffer we can write to. The cleanest pattern: stamp once at the outer entry-point (`send_to_all_sessions`) into a local `Vec<u8>` copy, then send that.

Actually a much cleaner approach: stamp the timestamp at the moment the *cache entry* is created, in `reliable_emitter::submit_one`, AND re-stamp on retransmit in `tick()`. Both sites already build the bytes. Each retransmit gets a fresh emit-time so the delay-gradient sees the actual wire-emit moment.

In `ghostframe-lib/src/transport/reliable_emitter/emitter.rs`, find `submit_one`:

```rust
    pub fn submit_one(&mut self, key: EmitKey, source_datagram_bytes: Bytes, now: Instant) {
        let wire_seq = self.alloc.allocate();
        let mut bytes = source_datagram_bytes.to_vec();
        if bytes.len() >= 12 {
            bytes[8..12].copy_from_slice(&wire_seq.to_be_bytes());
        }
```

Add a wall-clock emit-time stamp right after the wire_seq stamp:

```rust
    pub fn submit_one(&mut self, key: EmitKey, source_datagram_bytes: Bytes, now: Instant) {
        let wire_seq = self.alloc.allocate();
        let mut bytes = source_datagram_bytes.to_vec();
        if bytes.len() >= 12 {
            bytes[8..12].copy_from_slice(&wire_seq.to_be_bytes());
        }
        // Stamp server emit time (wall-clock μs, u32 wrap) into the
        // DatagramHeader's timestamp_us field at [12..16] for tile
        // datagrams (which set TILE_DATAGRAM_FLAG = 0x80 in byte 0).
        // Used by the client's per-tier latency tracking and echoed
        // back via the ACK envelope for the BWE delay-gradient input.
        if bytes.len() >= 16 && (bytes[0] & 0x80) != 0 {
            let emit_us = wall_clock_emit_us();
            bytes[12..16].copy_from_slice(&emit_us.to_be_bytes());
        }
```

Add the helper function at the bottom of the same file (above the test module):

```rust
/// Returns the current wall-clock microseconds, truncated to a u32 (≈ 71
/// minute wrap). Used to stamp `DatagramHeader.timestamp_us` on tile
/// datagrams. Clock skew between server and client is irrelevant —
/// consumers only look at *deltas*.
fn wall_clock_emit_us() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u32)
        .unwrap_or(0)
}
```

In `tick()` (just below `submit_one`), find the retransmit loop body that re-pushes the cached fragments:

```rust
            for bytes in frags {
                self.queue.push_source(bytes);
            }
```

Replace with:

```rust
            for mut bytes in frags {
                // Re-stamp emit time on retransmit so the BWE consumer
                // sees the actual on-wire moment, not the original send.
                if bytes.len() >= 16 && (bytes[0] & 0x80) != 0 {
                    let emit_us = wall_clock_emit_us();
                    bytes[12..16].copy_from_slice(&emit_us.to_be_bytes());
                }
                self.queue.push_source(bytes);
            }
```

- [ ] **Step 3: Build + run reliable_emitter tests**

Run: `cargo test -p ghostframe-lib transport::reliable_emitter`

Expected: all PASS. The simulation tests use synthetic frames that may or may not have the `TILE_DATAGRAM_FLAG` set; either way the new logic is a no-op when the flag isn't set.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/transport/protocol.rs ghostframe-lib/src/transport/reliable_emitter/emitter.rs
git commit -m "protocol: stamp server emit_us into tile DatagramHeader.timestamp_us"
```

---

## Task 4: Client captures arrival time per pass

**Files:**
- Modify: `ghostframe-web-client/src/main.ts`

- [ ] **Step 1: Stamp arrival time when a tile-pass finishes assembly**

In `ghostframe-web-client/src/main.ts`, find the `finishAssembly` function (around line 463). It currently extracts `frameSeqFromKey` from the assembly key. We need to record the wall-clock millisecond timestamp at this moment and thread it into the ACK call site.

Find the existing `ackBatcher.add({...})` call site that we modified in Task 2. Replace `arrivalTimeMsLo16: 0` with:

```typescript
        arrivalTimeMsLo16: (performance.now() & 0xFFFF) | 0,
```

`performance.now()` returns milliseconds. The `& 0xFFFF` truncates to 16 bits (65 s wrap). The `| 0` coerces to integer per JS bitwise-op semantics.

NOTE: `performance.now()` is a high-resolution monotonic time anchored to page-load. That's fine — we only use deltas for delay gradient, not absolute time.

- [ ] **Step 2: Build + verify web-client tests still pass**

Run: `cd ghostframe-web-client && npm test && npm run build`

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-web-client/src/main.ts
git commit -m "web-client: stamp arrival_time_ms_lo16 from performance.now()"
```

---

## Task 5: Server parses arrival time + computes per-pass delay sample

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Add the per-tier classification helper**

In `ghostframe-lib/src/transport/io_bridge.rs`, near other constants around line 155 (after `RTO_RETRANSMITS_PER_TICK`), add:

```rust
/// Visual-importance tier of a CDF53 pass.
/// - Critical: passes 0-3 — LL3 sub-band + first 3 bit-planes. Carry
///   most of the perceived image quality. NACKed aggressively, given
///   priority in the pacer.
/// - Refinement: passes 4-13 — higher-frequency detail. NACKed
///   conservatively, lower priority.
fn pass_tier(pass_idx: u8) -> PassTier {
    if pass_idx <= 3 { PassTier::Critical } else { PassTier::Refinement }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PassTier {
    Critical,
    Refinement,
}
```

- [ ] **Step 2: On ACK arrival, compute one-way-delay sample per entry**

Find the ACK handling block — search for the loop processing AckEntry (around line 1620 where `record_cdf53_ack` is called). At the top of the loop body, before any other handling, add:

```rust
                            // Compute approximate one-way delay for this
                            // tile-pass. server_emit_ms_lo16 is recovered
                            // by looking up the cache entry's stored
                            // server-emit timestamp; arrival_ms_lo16 is
                            // the client's value from the ACK envelope.
                            // Both are u16 wall-clock-ms with 65 s wrap;
                            // clock skew is irrelevant — only relative
                            // arrival differences between packets matter
                            // for the delay-gradient estimator.
                            let arrival_lo16 = entry.arrival_time_ms_lo16;
                            let server_emit_lo16 = self
                                .reliable_emitter
                                .cache
                                .get(&crate::transport::reliable_emitter::EmitKey::new(
                                    entry.frame_seq,
                                    entry.tile_x,
                                    entry.tile_y,
                                    entry.pass_idx,
                                ))
                                .and_then(|e| e.fragments.first())
                                .filter(|frag| frag.len() >= 16)
                                .map(|frag| {
                                    let ts_be: [u8; 4] = frag[12..16].try_into().unwrap();
                                    (u32::from_be_bytes(ts_be) / 1000) as u16
                                });
                            if let Some(emit_lo16) = server_emit_lo16 {
                                let tier = pass_tier(entry.pass_idx);
                                // Modular subtraction handles the wrap.
                                let owd_ms_lo16 = arrival_lo16.wrapping_sub(emit_lo16);
                                self.bwe_samples_buffer.push(BweSample {
                                    tier,
                                    server_emit_lo16: emit_lo16,
                                    client_arrival_lo16: arrival_lo16,
                                    owd_ms_lo16,
                                    received_at: Instant::now(),
                                });
                            }
```

- [ ] **Step 3: Add the BweSample buffer to IoBridge**

Find the `pub struct IoBridge` declaration. After other diagnostic-counter fields, add:

```rust
    /// Rolling buffer of one-way-delay samples derived from ACK
    /// envelopes. Drained periodically into the Bwe estimator (Task 7)
    /// and into per-tier latency histograms (Task 6). Bounded; old
    /// samples are dropped when capacity is reached.
    bwe_samples_buffer: Vec<BweSample>,
```

In `IoBridge::new` (and any test-only constructors), initialize:

```rust
            bwe_samples_buffer: Vec::with_capacity(4096),
```

Add the `BweSample` struct definition next to `BumpCountAccumulator` (around line 162):

```rust
/// One-way-delay sample observed at ACK time. Used as input to the
/// per-tier latency histograms and the str0m::bwe estimator.
#[derive(Debug, Clone, Copy)]
struct BweSample {
    tier: PassTier,
    server_emit_lo16: u16,
    client_arrival_lo16: u16,
    /// Modular u16 subtraction of arrival - emit. Used as a *delta-of-
    /// deltas* input to delay gradient estimation; absolute value is
    /// meaningless because of clock skew.
    owd_ms_lo16: u16,
    received_at: Instant,
}
```

- [ ] **Step 4: Build + run io_bridge tests**

Run: `cargo build -p ghostframe-lib && cargo test -p ghostframe-lib transport::io_bridge`

Expected: clean compile, tests PASS.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "io_bridge: BweSample buffer + per-tier classification + ACK arrival parse"
```

---

## Task 6: Per-tier byte counters (server) and per-tier recv counters (client)

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`
- Modify: `ghostframe-web-client/src/main.ts`

- [ ] **Step 1: Server: add per-tier byte counters to IoBridge**

In `ghostframe-lib/src/transport/io_bridge.rs`, find the existing cumulative-stats area. After `bwe_samples_buffer`, add:

```rust
    /// Bytes of CDF53 critical-tier (passes 0-3) datagrams emitted
    /// since startup. Reset never; the per-second logger derives rates
    /// from the delta.
    bytes_emitted_critical: u64,
    /// Bytes of CDF53 refinement-tier (passes 4-13) datagrams emitted
    /// since startup.
    bytes_emitted_refinement: u64,
    /// Snapshot taken at the previous periodic log, used to derive
    /// per-window rates without polluting the cumulative counters.
    bytes_emitted_snapshot: (u64, u64),
    /// Wall-clock instant of the previous periodic log.
    bwe_log_last_at: Option<Instant>,
```

In `IoBridge::new`, initialize all four to `0`, `0`, `(0, 0)`, `None`.

- [ ] **Step 2: Server: increment the counters at the emit site**

Find the existing tile-emission site in `dispatch_dirty_tiles_via_scheduler` where each tile's bytes are sent through quinn. We want to count bytes per tier. Search for the existing per-codec counters (`cumulative_datagrams_emitted.cdf53.saturating_add`) and add a sibling block:

```rust
                Codec::Cdf53 => {
                    self.cumulative_datagrams_emitted.cdf53 =
                        self.cumulative_datagrams_emitted.cdf53.saturating_add(n_dg);
                    // Per-tier byte attribution: critical (passes 0-3)
                    // vs refinement (4-13). Counts every successful
                    // emit including retransmits, because BWE rates
                    // should reflect actual wire utilization.
                    let bytes_this_pass = (n_dg as u64) * (avg_datagram_bytes as u64);
                    match pass_tier(work.pass_idx) {
                        PassTier::Critical => {
                            self.bytes_emitted_critical = self
                                .bytes_emitted_critical
                                .saturating_add(bytes_this_pass);
                        }
                        PassTier::Refinement => {
                            self.bytes_emitted_refinement = self
                                .bytes_emitted_refinement
                                .saturating_add(bytes_this_pass);
                        }
                    }
                }
```

`avg_datagram_bytes` is the average datagram payload size you can compute as `(total_payload_bytes_for_this_tile + 8 * n_dg) / n_dg` (the 8 includes the tile header), or just use a constant approximation like 500 if the existing code doesn't easily expose per-emit byte counts.

(Implementer judgment: if exposing exact bytes-per-emit is complex, use a constant approximation. Phase 1 is about *order-of-magnitude* observability, not perfect accounting.)

- [ ] **Step 3: Client: per-tier recv counters in main.ts**

In `ghostframe-web-client/src/main.ts`, find the existing periodic-stats block (around line 800-815 where `Δdrain` is computed). Above that block, add:

```typescript
  // Per-tier recv counters (passes 0-3 critical vs 4-13 refinement).
  // Updated in the per-pass receive path below; reported in the
  // periodic stats line.
  let bytesRecvCritical = 0;
  let bytesRecvRefinement = 0;
  let bytesRecvCriticalSnapshot = 0;
  let bytesRecvRefinementSnapshot = 0;
```

In the per-pass receive handler (just below the `__cdf53Coverage` block), add:

```typescript
      // Per-tier byte attribution for BWE-side observability.
      if (passIdx <= 3) {
        bytesRecvCritical += payload.byteLength;
      } else {
        bytesRecvRefinement += payload.byteLength;
      }
```

- [ ] **Step 4: Build + test**

Server: `cargo build -p ghostframe-lib && cargo test -p ghostframe-lib transport::io_bridge`
Client: `cd ghostframe-web-client && npm test && npm run build`

Expected: both clean.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs ghostframe-web-client/src/main.ts
git commit -m "io_bridge+web-client: per-tier (critical/refinement) byte counters"
```

---

## Task 7: Add str0m dependency + Bwe wrapper

**Files:**
- Modify: `ghostframe-lib/Cargo.toml`
- Create: `ghostframe-lib/src/transport/bwe.rs`
- Modify: `ghostframe-lib/src/transport/mod.rs`

- [ ] **Step 1: Add str0m to Cargo.toml**

In `ghostframe-lib/Cargo.toml`, under `[dependencies]`, add:

```toml
# str0m is a Rust Sans-IO WebRTC implementation. We use only its
# `bwe` (Bandwidth Estimator) module — a production port of libwebrtc's
# Google Congestion Control (GoogCC). Construct standalone via
# `Bwe::new(initial_bitrate)`; no Rtc session needed. See
# docs/superpowers/specs/2026-06-27-protocol-redesign-design.md
# for the selection rationale.
str0m = { version = "0.21", default-features = false }
```

If `default-features = false` strips too much, drop the override and accept the full default feature set — str0m's defaults are small.

- [ ] **Step 2: Create the bwe.rs wrapper module**

Create `ghostframe-lib/src/transport/bwe.rs`:

```rust
//! Thin wrapper around `str0m::bwe::Bwe` (GoogCC bandwidth estimator).
//!
//! Phase 1 of the protocol redesign uses Bwe in PASSIVE mode: we feed
//! it observed ACK arrival records and read its bandwidth estimate
//! periodically for observability, but we don't yet drive emission
//! from it. Phase 2 will plug `current_estimate_bps()` into the pacer.
//!
//! See `docs/superpowers/specs/2026-06-27-protocol-redesign-design.md`.

use std::time::Instant;

/// Tier-tagged ACK arrival record fed into the estimator.
#[derive(Debug, Clone, Copy)]
pub struct AckArrival {
    pub wire_seq: u32,
    /// Wall-clock millis (low 16 bits) of the *server* emit moment,
    /// recovered from the cache entry's stamped DatagramHeader.
    pub server_emit_ms_lo16: u16,
    /// Wall-clock millis (low 16 bits) of the *client* receive moment,
    /// carried back via the ACK envelope.
    pub client_arrival_ms_lo16: u16,
}

/// Public observability snapshot returned by `BweWrapper::snapshot()`.
#[derive(Debug, Clone, Copy)]
pub struct BweSnapshot {
    pub bitrate_bps: u64,
    pub samples_seen: u64,
}

/// Wrapper owning a single `str0m::bwe::Bwe` instance plus our own
/// rolling sample counter for logging.
pub struct BweWrapper {
    // FIXME: actual str0m::bwe::Bwe field added after consulting the
    // crate's exact API. Use `cargo doc -p str0m --open` to confirm
    // constructor signature; expected approximate shape:
    //     bwe: str0m::bwe::Bwe,
    samples_seen: u64,
}

impl BweWrapper {
    /// Initial bitrate seed. Sized for a typical broadband first-paint
    /// burst (2 Mbps); the estimator will adapt within a few hundred ms.
    pub const INITIAL_BPS: u64 = 2_000_000;

    pub fn new(_initial_bps: u64) -> Self {
        // FIXME: instantiate the real Bwe here once the crate API is
        // confirmed. The fixme intentionally fails build until step 4
        // wires the real dep.
        Self { samples_seen: 0 }
    }

    /// Feed a batch of ACK arrival records. Returns the new estimate
    /// snapshot, or None if no samples were ingested.
    pub fn update(&mut self, _records: &[AckArrival], _now: Instant) -> Option<BweSnapshot> {
        // FIXME: actual str0m::bwe::Bwe::update call here.
        self.samples_seen = self
            .samples_seen
            .saturating_add(_records.len() as u64);
        None
    }

    pub fn snapshot(&self) -> BweSnapshot {
        // FIXME: actual str0m::bwe::Bwe::poll_estimate call here.
        BweSnapshot {
            bitrate_bps: Self::INITIAL_BPS,
            samples_seen: self.samples_seen,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_starts_at_initial_bps() {
        let bwe = BweWrapper::new(BweWrapper::INITIAL_BPS);
        let snap = bwe.snapshot();
        assert_eq!(snap.bitrate_bps, BweWrapper::INITIAL_BPS);
        assert_eq!(snap.samples_seen, 0);
    }

    #[test]
    fn update_counts_records_seen() {
        let mut bwe = BweWrapper::new(BweWrapper::INITIAL_BPS);
        let records = vec![
            AckArrival { wire_seq: 1, server_emit_ms_lo16: 100, client_arrival_ms_lo16: 110 },
            AckArrival { wire_seq: 2, server_emit_ms_lo16: 120, client_arrival_ms_lo16: 135 },
        ];
        bwe.update(&records, Instant::now());
        let snap = bwe.snapshot();
        assert_eq!(snap.samples_seen, 2);
    }
}
```

- [ ] **Step 3: Register the new module**

In `ghostframe-lib/src/transport/mod.rs`, find the existing `pub mod` declarations and add:

```rust
pub mod bwe;
```

(Alphabetized with the existing modules.)

- [ ] **Step 4: Resolve the FIXMEs against the actual str0m API**

Run: `cargo doc -p str0m --open --no-deps`

In the generated docs, navigate to `str0m::bwe::Bwe`. Read the constructor, the `update()` method, and the bandwidth-estimate getter (likely `poll_estimate()` or `current_estimate()`).

Replace the `// FIXME:` blocks in `bwe.rs` with real calls. The shape will be approximately:

```rust
pub struct BweWrapper {
    bwe: str0m::bwe::Bwe,
    samples_seen: u64,
}

impl BweWrapper {
    pub fn new(initial_bps: u64) -> Self {
        let bwe = str0m::bwe::Bwe::new(str0m::Bitrate::bps(initial_bps as f64));
        Self { bwe, samples_seen: 0 }
    }

    pub fn update(&mut self, records: &[AckArrival], now: Instant) -> Option<BweSnapshot> {
        // Convert our AckArrival records into the TWCC-shaped records
        // str0m::bwe::Bwe::update expects. Exact API TBD by Step 4.
        // (Implementer: consult cargo doc.)
        self.samples_seen = self
            .samples_seen
            .saturating_add(records.len() as u64);
        // self.bwe.update(...);
        Some(BweSnapshot {
            bitrate_bps: self.bwe.poll_estimate().as_bps() as u64,
            samples_seen: self.samples_seen,
        })
    }

    pub fn snapshot(&self) -> BweSnapshot {
        BweSnapshot {
            bitrate_bps: self.bwe.poll_estimate().as_bps() as u64,
            samples_seen: self.samples_seen,
        }
    }
}
```

The exact `update()` call shape MUST be confirmed by reading `cargo doc`. If the API expects `Vec<(SeqNo, Instant)>`, convert. If it expects something else, adapt.

- [ ] **Step 5: Run tests**

Run: `cargo build -p ghostframe-lib && cargo test -p ghostframe-lib transport::bwe`

Expected: PASS. The synthetic-records test confirms `update()` and `snapshot()` round-trip.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-lib/Cargo.toml ghostframe-lib/src/transport/bwe.rs ghostframe-lib/src/transport/mod.rs Cargo.lock
git commit -m "transport: add str0m::bwe wrapper module + synthetic-records test"
```

---

## Task 8: Wire BweWrapper into IoBridge

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Add `bwe: BweWrapper` field to `IoBridge`**

In the struct definition, after `bwe_samples_buffer` and the per-tier byte counters:

```rust
    /// Bandwidth-estimator wrapper. Phase 1: runs in PASSIVE mode —
    /// we feed it ACK arrivals and read its estimate for logging, but
    /// don't yet drive emission from its output.
    bwe: crate::transport::bwe::BweWrapper,
```

In `IoBridge::new` and any test-only constructors:

```rust
            bwe: crate::transport::bwe::BweWrapper::new(
                crate::transport::bwe::BweWrapper::INITIAL_BPS,
            ),
```

- [ ] **Step 2: Drain bwe_samples_buffer into Bwe periodically**

In the run loop (search for `Event::DatagramReceived` and `process_inbound`), add a small per-tick drain. Find the closing brace of the inbound event processing block. Right after it, add:

```rust
            // Drain accumulated BWE samples (from the ACK parsing path
            // in Task 5) into the estimator. Cheap on empty.
            if !self.bwe_samples_buffer.is_empty() {
                use crate::transport::bwe::AckArrival;
                let records: Vec<AckArrival> = self
                    .bwe_samples_buffer
                    .drain(..)
                    .map(|s| AckArrival {
                        // wire_seq lookup is best-effort: the cache
                        // entry's stored wire_seq is the truth, but
                        // BweSample doesn't carry it directly. For
                        // Phase 1, encode (emit_lo16, arrival_lo16) as
                        // an opaque seq pair and let str0m handle it.
                        wire_seq: ((s.server_emit_lo16 as u32) << 16)
                            | (s.client_arrival_lo16 as u32),
                        server_emit_ms_lo16: s.server_emit_lo16,
                        client_arrival_ms_lo16: s.client_arrival_lo16,
                    })
                    .collect();
                self.bwe.update(&records, Instant::now());
            }
```

(The `wire_seq` encoding is a Phase 1 placeholder; Phase 2 will plumb the real wire_seq through `BweSample` if str0m needs it.)

- [ ] **Step 3: Build + test**

Run: `cargo build -p ghostframe-lib && cargo test -p ghostframe-lib transport::io_bridge`

Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "io_bridge: drain BWE samples into the estimator each event-loop turn"
```

---

## Task 9: Server periodic BWE logging (1 s cadence)

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Add a 1 s periodic BWE log alongside the existing cumulative log**

Find the existing cumulative-emit log site (search `"cumulative emit (datagrams handed to quinn since startup, per codec)"`). After that block, add a new periodic BWE log that fires at most once per second:

```rust
                let now = Instant::now();
                let bwe_log_due = self
                    .bwe_log_last_at
                    .is_none_or(|last| now.duration_since(last).as_millis() >= 1000);
                if bwe_log_due {
                    self.bwe_log_last_at = Some(now);
                    let bwe_snap = self.bwe.snapshot();
                    let (prev_crit, prev_refn) = self.bytes_emitted_snapshot;
                    let dt_ms = self
                        .bwe_log_last_at
                        .and_then(|now_t| {
                            self.bwe_log_last_at
                                .map(|last| now_t.duration_since(last).as_millis() as u64)
                        })
                        .unwrap_or(1000);
                    let dt_ms = dt_ms.max(1);
                    let bps_crit = ((self.bytes_emitted_critical - prev_crit) * 8 * 1000) / dt_ms;
                    let bps_refn = ((self.bytes_emitted_refinement - prev_refn) * 8 * 1000) / dt_ms;
                    self.bytes_emitted_snapshot = (
                        self.bytes_emitted_critical,
                        self.bytes_emitted_refinement,
                    );
                    tracing::info!(
                        target: "ghostframe::bwe",
                        bwe_estimate_bps = bwe_snap.bitrate_bps,
                        bwe_samples_seen = bwe_snap.samples_seen,
                        bps_critical = bps_crit,
                        bps_refinement = bps_refn,
                        cumulative_bytes_critical = self.bytes_emitted_critical,
                        cumulative_bytes_refinement = self.bytes_emitted_refinement,
                        "bwe periodic snapshot"
                    );
                }
```

- [ ] **Step 2: Build + run**

Run: `cargo build -p ghostframe-lib && cargo test -p ghostframe-lib --lib`

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "io_bridge: per-second BWE periodic log line"
```

---

## Task 10: Client per-tier stats in the periodic page log

**Files:**
- Modify: `ghostframe-web-client/src/main.ts`

- [ ] **Step 1: Extend the existing periodic stats line**

Find the existing `statsLine` template literal (around line 815). It currently includes `submit:${...} lastSeq:${...}`. Add per-tier recv stats. The block looks like:

```typescript
      const statsLine =
        `stats: rx{r:${counts.raw} s:${counts.solid} p:${counts.palrle} c:${counts.cdf53} h:${counts.h264}} ` +
        `Δdrain{r:${drainDelta.raw} s:${drainDelta.solid} p:${drainDelta.palrle} c:${drainDelta.cdf53} h:${drainDelta.h264}} ` +
        `fb:${fb.width}x${fb.height} ` +
        `cdf53fails:${cdf53Fails}(last=${cdf53Last}) ` +
        `raf:${__rafTicks} submit:${...} lastSeq:${w.__lastTileSeq ?? '-'}`;
```

Add a new line below it:

```typescript
      // Per-tier (passes 0-3 critical vs 4-13 refinement) byte counters
      // since startup, plus rate over the last stats window. Used as
      // the client-side input to the BWE-evaluation observability.
      const dtMs = Math.max(1, nowMs - __lastStatsMs);
      const bpsCrit = Math.floor(((bytesRecvCritical - bytesRecvCriticalSnapshot) * 8 * 1000) / dtMs);
      const bpsRefn = Math.floor(((bytesRecvRefinement - bytesRecvRefinementSnapshot) * 8 * 1000) / dtMs);
      bytesRecvCriticalSnapshot = bytesRecvCritical;
      bytesRecvRefinementSnapshot = bytesRecvRefinement;
      const bweTierLine =
        `bwe-tier: bps_critical=${bpsCrit} bps_refinement=${bpsRefn} ` +
        `bytes_critical=${bytesRecvCritical} bytes_refinement=${bytesRecvRefinement}`;
```

And in the existing block that emits the stats line(s), add `log(bweTierLine);` adjacent to the existing `log(statsLine);` call. Suppress when idle (no change in bytes) by including bytes in the idle-suppression key.

Find the idle-suppression block (the part that constructs `lineKey` and compares to `__lastStatsLineKey`). Update `lineKey` to include `bytes_critical` and `bytes_refinement` so the line refreshes when new data arrives:

```typescript
      const lineKey =
        `r:${counts.raw}|s:${counts.solid}|p:${counts.palrle}|c:${counts.cdf53}|h:${counts.h264}|` +
        `seq:${w.__lastTileSeq ?? '-'}|cov:${cdf53Refined}/${cdf53Partial}/${cdf53Tiles}|hist:${histCompact}|` +
        `fec:${fecRecovered}/${fecParityRx}/${fecParityUnrecoverable}/${nackSent}|` +
        `bwe:${bytesRecvCritical}/${bytesRecvRefinement}`;
```

- [ ] **Step 2: Build + test**

Run: `cd ghostframe-web-client && npm test && npm run build`

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-web-client/src/main.ts
git commit -m "web-client: per-tier BWE stats in the periodic page log"
```

---

## Task 11: Reliability sweep + manual prod check

- [ ] **Step 1: Run unit/integration tests**

Run:
```bash
cargo test -p ghostframe-lib --lib
(cd ghostframe-web-client && npm test && npm run build)
```

Expected: all PASS.

- [ ] **Step 2: Rebuild containers**

Run: `just containers-build`

Expected: tagged successfully.

- [ ] **Step 3: Run the two main e2e tests under loss + clean**

Run:
```bash
cargo test -p ghostframe-e2e --test e2e e2e_cdf53_lossless_under_congestion_chromium -- --nocapture --test-threads=1
cargo test -p ghostframe-e2e --test e2e e2e_cdf53_lossless_buildup_chromium -- --nocapture --test-threads=1
```

Expected: both PASS. Phase 1 should have no behavior change — these tests must still converge as they did before.

- [ ] **Step 4: Manual prod check**

Restart ghostframe.target on the user's machine and open the page in Firefox. Watch:

- Journal: `journalctl --since "5 minutes ago" --no-pager | grep ghostframe::bwe | tail -10` — should show one `bwe periodic snapshot` line per second with non-zero `bwe_estimate_bps`, growing `bwe_samples_seen`, and `bps_critical` / `bps_refinement` evolving as content streams.
- Page log: the new `bwe-tier:` line should appear in the periodic 2 s page-log dump with non-zero `bps_critical` and `bps_refinement` while content is streaming, zero when idle.
- Convergence behavior should be unchanged from the pre-Phase-1 state — same `nack_sent` evolution, same `pass-hist` evolution.

- [ ] **Step 5: Confirm Phase 1 done**

If both e2e tests are green AND the prod-side metrics evolve sensibly (numbers move and are in plausible ranges), Phase 1 is complete. Otherwise debug the specific gap (likely str0m API mismatch in Task 7 — read the docs carefully).

---

## Self-Review

**Spec coverage:**
- "Embed server-side send timestamp in datagram header" → Task 3 ✓
- "Receiver-side arrival timestamps in ACK envelope" → Tasks 1, 2, 4 ✓
- "Per-tier (passes 0-3 vs 4-13) latency + bandwidth tracking on both sides" → Tasks 5, 6, 9, 10 ✓
- "Add str0m::bwe as a dependency. Wire ACK arrival records into Bwe::update()" → Tasks 7, 8 ✓
- "Periodic logging — don't yet drive emission" → Tasks 9, 10 ✓
- "Verify on real networks" → Task 11 ✓

**Placeholders:** Task 4's `(performance.now() & 0xFFFF) | 0` produces an integer (not "appropriate handling"). Task 5's BweSample lookup uses an explicit EmitKey cache access (not "look up somewhere"). Task 7 step 4 explicitly directs the implementer to `cargo doc -p str0m --open` to confirm the exact API — this is a real instruction, not a placeholder, because the str0m API shape can't be hardcoded into a plan without first reading the crate's docs.

**Type consistency:**
- `ACK_BATCH_MSG_TYPE: u8 = 0x04` — Tasks 1, 2 use the same value.
- `ACK_ENTRY_SIZE: usize = 9` — Tasks 1, 2 consistent.
- `AckEntry.arrival_time_ms_lo16: u16` (server) / `arrivalTimeMsLo16: number` (client) — Tasks 1, 2 agree.
- `BweSample` struct fields — Tasks 5, 8 reference the same shape.
- `BweWrapper::new(initial_bps: u64)` / `update(records: &[AckArrival], now: Instant)` / `snapshot() -> BweSnapshot` — Tasks 7, 8 use the same API.
- `PassTier { Critical, Refinement }` — Tasks 5, 6 use the same enum.
