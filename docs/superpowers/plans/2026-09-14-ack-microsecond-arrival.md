# Microsecond ACK Arrival Timestamps Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Carry client ACK arrival times at microsecond resolution so goog_cc stops discarding 80% of our probe measurements.

**Architecture:** ACK wire format `0x05` splits a batch into two sections. Fresh entries (dense, ≤5 ms span) carry `u16` microsecond deltas from a per-batch base; overlap entries (arbitrarily old) carry absolute low-32-bit microseconds. In memory `AckEntry` exposes one uniform `arrival_us`, so the three server consumer sites barely change. The server's own send timestamps gain matching precision.

**Tech Stack:** Rust. `ghostframe-protocol` (codec), `ghostframe-client-core` (producer), `ghostframe-lib` (consumer + goog_cc adapter).

Design: `docs/superpowers/specs/2026-09-14-ack-microsecond-arrival-design.md`. Read it first — particularly "Why two sections", which explains why the smaller single-base encoding was rejected.

---

## File structure

| File | Responsibility | Change |
|---|---|---|
| `ghostframe-protocol/src/ack.rs` | Wire codec, caps, error type | Two-section encode/decode, `0x05`, `arrival_us` |
| `ghostframe-client-core/src/reassembly.rs` | Produces `AckEntry` on datagram receipt | Stop discarding precision (2 sites) |
| `ghostframe-client-core/src/ack_batcher.rs` | Assembles `[fresh, overlap]` | Tell the batch where fresh ends |
| `ghostframe-client-core/tests/oracle_ack.rs` | Oracle round-trip | Follow the field rename |
| `ghostframe-lib/src/transport/bwe/timeline.rs` | Sequence-space unwrapper | 16-bit ms → 32-bit µs |
| `ghostframe-lib/src/transport/io_bridge.rs` | Consumes ACK batches, builds `BweSample` | Read `arrival_us`, unwrap per section |
| `ghostframe-lib/src/transport/bwe/googcc.rs` | goog_cc adapter | Microsecond send timestamps |
| `ghostframe-client-wasm/src/constants.rs` | Exports protocol constants to JS | `ackEntrySize` no longer exists; export both section sizes |
| `ghostframe-lib/src/transport/bwe/mod.rs` | `AckArrival` (estimator input) | `client_arrival_ms_lo16` → `client_arrival_us` |

## Landmarks

Re-derive these; they drift.

| Symbol | Location |
|---|---|
| `AckBatch::encode` / `decode` | `ack.rs` ~88 / ~104 |
| `arrival_time_ms_lo16` producer sites | `reassembly.rs` ~100, ~462 |
| `AckBatcher::flush` | `ack_batcher.rs` ~63 |
| ACK consumer sites (three) | `io_bridge.rs` ~2663, ~2675, ~2715 |
| `BweSample` | `io_bridge.rs` ~916 |
| `Lo16Timeline::unwrap_ms` | `timeline.rs` ~29 |
| `Timestamp::from_millis(send_ms)` | `googcc.rs` ~268 |

---

## Task 1: Wire codec

**Files:**
- Modify: `ghostframe-protocol/src/ack.rs`

- [ ] **Step 1: Write the failing tests**

Add to `ack.rs`'s existing `mod tests`:

```rust
    fn entry(frame_seq: u32, arrival_us: u64) -> AckEntry {
        AckEntry {
            frame_seq,
            tile_x: 1,
            tile_y: 2,
            pass_idx: 3,
            arrival_us,
        }
    }

    #[test]
    fn single_fresh_entry_round_trips_with_zero_delta() {
        let b = AckBatch {
            entries: vec![entry(7, 1_234_567)],
            fresh_count: 1,
        };
        let bytes = b.encode();
        assert_eq!(bytes[0], ACK_BATCH_MSG_TYPE);
        assert_eq!(bytes[1], 1, "count_fresh");
        assert_eq!(bytes[2], 0, "count_overlap");
        assert_eq!(AckBatch::decode(&bytes).unwrap(), b);
    }

    #[test]
    fn entries_sharing_a_timestamp_encode_zero_deltas() {
        let b = AckBatch {
            entries: vec![entry(1, 9_000), entry(2, 9_000), entry(3, 9_000)],
            fresh_count: 3,
        };
        let bytes = b.encode();
        // Each fresh entry's delta occupies the last two bytes of its 9.
        for i in 0..3 {
            let off = ACK_HEADER_SIZE + i * ACK_FRESH_ENTRY_SIZE;
            assert_eq!((bytes[off + 7], bytes[off + 8]), (0, 0), "entry {i}");
        }
        assert_eq!(AckBatch::decode(&bytes).unwrap(), b);
    }

    #[test]
    fn a_fresh_span_of_exactly_u16_max_still_encodes() {
        let b = AckBatch {
            entries: vec![entry(1, 1_000), entry(2, 1_000 + u16::MAX as u64)],
            fresh_count: 2,
        };
        let bytes = b.encode();
        assert_eq!(AckBatch::decode(&bytes).unwrap(), b);
    }

    #[test]
    fn a_fresh_span_beyond_u16_max_is_refused_rather_than_truncated() {
        // Cannot happen with FLUSH_INTERVAL_US = 5_000, so reaching this is a
        // caller bug. Silently truncating would feed the estimator a wrong
        // arrival time, which is the class of defect this format exists to end.
        let b = AckBatch {
            entries: vec![entry(1, 1_000), entry(2, 1_000 + u16::MAX as u64 + 1)],
            fresh_count: 2,
        };
        assert!(b.try_encode().is_err());
    }

    #[test]
    fn an_overlap_entry_seconds_old_round_trips_exactly() {
        // The whole point of giving overlap absolute timestamps: no age is
        // unrepresentable, so nothing is ever dropped to make it fit.
        let b = AckBatch {
            entries: vec![entry(1, 5_000_000), entry(2, 1_000)],
            fresh_count: 1,
        };
        assert_eq!(AckBatch::decode(&b.encode()).unwrap(), b);
    }

    #[test]
    fn an_overlap_entry_ten_minutes_old_round_trips_exactly() {
        let ten_min_us = 600_000_000u64;
        let b = AckBatch {
            entries: vec![entry(1, ten_min_us + 1_000), entry(2, 1_000)],
            fresh_count: 1,
        };
        // Both sit inside the low-32-bit space; equality is exact.
        assert_eq!(AckBatch::decode(&b.encode()).unwrap(), b);
    }

    #[test]
    fn a_batch_straddling_the_u32_wrap_decodes_exactly() {
        // base near the top of the 32-bit space, deltas pushing past it.
        let base = u32::MAX as u64 - 10;
        let b = AckBatch {
            entries: vec![entry(1, base), entry(2, base + 100)],
            fresh_count: 2,
        };
        let decoded = AckBatch::decode(&b.encode()).unwrap();
        // The second entry wrapped into the low end of 32-bit space; the
        // server's sequence-space unwrapper is what restores monotonicity.
        assert_eq!(decoded.entries[0].arrival_us, base);
        assert_eq!(decoded.entries[1].arrival_us, 89);
    }

    #[test]
    fn counts_over_their_caps_are_rejected() {
        let mut bytes = vec![ACK_BATCH_MSG_TYPE, (MAX_FRESH_ENTRIES_PER_BATCH + 1) as u8, 0];
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            AckBatch::decode(&bytes),
            Err(AckDecodeError::InvalidCount(_))
        ));

        let mut bytes = vec![ACK_BATCH_MSG_TYPE, 1, (ACK_OVERLAP_COUNT + 1) as u8];
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            AckBatch::decode(&bytes),
            Err(AckDecodeError::InvalidCount(_))
        ));
    }

    #[test]
    fn a_buffer_truncated_mid_entry_is_rejected() {
        let b = AckBatch {
            entries: vec![entry(1, 1_000), entry(2, 2_000)],
            fresh_count: 2,
        };
        let bytes = b.encode();
        for cut in 1..bytes.len() {
            assert!(
                AckBatch::decode(&bytes[..cut]).is_err(),
                "truncating to {cut} bytes must be rejected, not read past the end"
            );
        }
    }

    #[test]
    fn an_old_format_batch_is_rejected_loudly() {
        let stale = vec![0x04u8, 1, 0, 0, 0, 0];
        assert_eq!(
            AckBatch::decode(&stale),
            Err(AckDecodeError::WrongMsgType(0x04))
        );
    }

    #[test]
    fn worst_case_batch_fits_the_documented_size() {
        let mut entries: Vec<AckEntry> = (0..MAX_FRESH_ENTRIES_PER_BATCH)
            .map(|i| entry(i as u32, 1_000 + i as u64))
            .collect();
        entries.extend((0..ACK_OVERLAP_COUNT).map(|i| entry(900 + i as u32, 10 + i as u64)));
        let b = AckBatch {
            entries,
            fresh_count: MAX_FRESH_ENTRIES_PER_BATCH,
        };
        assert_eq!(b.encode().len(), 671);
        assert_eq!(AckBatch::decode(&b.encode()).unwrap(), b);
    }
```

- [ ] **Step 2: Run them to verify they fail**

```bash
cd /home/cedric/work/ghostframe
cargo test -p ghostframe-protocol ack 2>&1 | tail -5
```

Expected: compile errors — no `fresh_count`, no `arrival_us`, no `try_encode`.

- [ ] **Step 3: Implement the codec**

Replace the constants, error type, structs, and `impl AckBatch` in `ack.rs`:

```rust
/// Wire-acceptance cap for one ACK batch: fresh + overlap.
pub const MAX_ACK_ENTRIES_PER_BATCH: usize = MAX_FRESH_ENTRIES_PER_BATCH + ACK_OVERLAP_COUNT;
/// `msg_type` + `count_fresh` + `count_overlap` + `base_arrival_us`.
pub const ACK_HEADER_SIZE: usize = 1 + 1 + 1 + 4;
/// Fresh entries carry a `u16` microsecond delta from `base_arrival_us`.
pub const ACK_FRESH_ENTRY_SIZE: usize = 9;
/// Overlap entries carry an absolute low-32-bit microsecond timestamp, so no
/// age is unrepresentable and nothing has to be dropped to make a batch fit.
pub const ACK_OVERLAP_ENTRY_SIZE: usize = 11;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AckDecodeError {
    #[error("ack batch too short ({0} bytes)")]
    TooShort(usize),
    #[error("wrong message type: expected 0x05, got 0x{0:02x}")]
    WrongMsgType(u8),
    #[error("invalid entry count: {0}")]
    InvalidCount(u8),
}

/// A fresh entry's arrival is further from the batch base than a `u16` of
/// microseconds can express. Impossible while `FLUSH_INTERVAL_US` bounds a
/// batch's fresh span to 5 ms, so this is a caller bug rather than a wire
/// condition — and truncating instead would hand the estimator a wrong
/// arrival time, which is exactly the failure this format exists to end.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("fresh entry {index} is {delta_us}us from the batch base, over the u16 limit")]
pub struct AckEncodeError {
    pub index: usize,
    pub delta_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckEntry {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
    /// Client receive time in microseconds, in low-32-bit wrapped space (the
    /// wire carries 32 bits, wrapping every 71.6 minutes). Absolute clock skew
    /// does not matter: the BWE consumer reads relative differences, and
    /// `bwe::timeline` restores monotonicity across wraps.
    pub arrival_us: u64,
}

/// One batch: `entries[..fresh_count]` are fresh, the rest are overlap.
///
/// Kept as one vector rather than two so consumers iterate uniformly — the
/// split only matters to the encoder, which gives each section the encoding
/// that suits its temporal density.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckBatch {
    pub entries: Vec<AckEntry>,
    pub fresh_count: usize,
}

impl AckBatch {
    /// Encode, panicking if a fresh entry is too far from the base. Callers
    /// that cannot guarantee the 5 ms bound should use [`try_encode`].
    ///
    /// [`try_encode`]: AckBatch::try_encode
    pub fn encode(&self) -> Vec<u8> {
        self.try_encode().expect("fresh span within u16 microseconds")
    }

    pub fn try_encode(&self) -> Result<Vec<u8>, AckEncodeError> {
        let fresh_count = self.fresh_count.min(self.entries.len());
        let overlap_count = self.entries.len() - fresh_count;
        let base = if fresh_count > 0 {
            self.entries[0].arrival_us
        } else {
            0
        };

        let mut out = Vec::with_capacity(
            ACK_HEADER_SIZE
                + fresh_count * ACK_FRESH_ENTRY_SIZE
                + overlap_count * ACK_OVERLAP_ENTRY_SIZE,
        );
        out.push(ACK_BATCH_MSG_TYPE);
        out.push(fresh_count as u8);
        out.push(overlap_count as u8);
        out.extend_from_slice(&(base as u32).to_le_bytes());

        for (i, e) in self.entries[..fresh_count].iter().enumerate() {
            // Subtract in the 32-bit space the wire uses, so a batch
            // straddling a wrap yields a small delta rather than a huge one.
            let delta = (e.arrival_us as u32).wrapping_sub(base as u32);
            if delta > u16::MAX as u32 {
                return Err(AckEncodeError {
                    index: i,
                    delta_us: delta as u64,
                });
            }
            out.extend_from_slice(&e.frame_seq.to_le_bytes());
            out.push(e.tile_x);
            out.push(e.tile_y);
            out.push(e.pass_idx);
            out.extend_from_slice(&(delta as u16).to_le_bytes());
        }
        for e in &self.entries[fresh_count..] {
            out.extend_from_slice(&e.frame_seq.to_le_bytes());
            out.push(e.tile_x);
            out.push(e.tile_y);
            out.push(e.pass_idx);
            out.extend_from_slice(&(e.arrival_us as u32).to_le_bytes());
        }
        Ok(out)
    }

    pub fn decode(data: &[u8]) -> Result<Self, AckDecodeError> {
        if data.len() < ACK_HEADER_SIZE {
            return Err(AckDecodeError::TooShort(data.len()));
        }
        if data[0] != ACK_BATCH_MSG_TYPE {
            return Err(AckDecodeError::WrongMsgType(data[0]));
        }
        let fresh_count = data[1] as usize;
        let overlap_count = data[2] as usize;
        if fresh_count > MAX_FRESH_ENTRIES_PER_BATCH {
            return Err(AckDecodeError::InvalidCount(data[1]));
        }
        if overlap_count > ACK_OVERLAP_COUNT {
            return Err(AckDecodeError::InvalidCount(data[2]));
        }
        if fresh_count + overlap_count == 0 {
            return Err(AckDecodeError::InvalidCount(0));
        }
        let need = ACK_HEADER_SIZE
            + fresh_count * ACK_FRESH_ENTRY_SIZE
            + overlap_count * ACK_OVERLAP_ENTRY_SIZE;
        if data.len() < need {
            return Err(AckDecodeError::TooShort(data.len()));
        }
        let base = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);

        let mut entries = Vec::with_capacity(fresh_count + overlap_count);
        for i in 0..fresh_count {
            let off = ACK_HEADER_SIZE + i * ACK_FRESH_ENTRY_SIZE;
            let delta = u16::from_le_bytes([data[off + 7], data[off + 8]]);
            entries.push(AckEntry {
                frame_seq: u32::from_le_bytes([
                    data[off],
                    data[off + 1],
                    data[off + 2],
                    data[off + 3],
                ]),
                tile_x: data[off + 4],
                tile_y: data[off + 5],
                pass_idx: data[off + 6],
                arrival_us: base.wrapping_add(delta as u32) as u64,
            });
        }
        let overlap_start = ACK_HEADER_SIZE + fresh_count * ACK_FRESH_ENTRY_SIZE;
        for i in 0..overlap_count {
            let off = overlap_start + i * ACK_OVERLAP_ENTRY_SIZE;
            entries.push(AckEntry {
                frame_seq: u32::from_le_bytes([
                    data[off],
                    data[off + 1],
                    data[off + 2],
                    data[off + 3],
                ]),
                tile_x: data[off + 4],
                tile_y: data[off + 5],
                pass_idx: data[off + 6],
                arrival_us: u32::from_le_bytes([
                    data[off + 7],
                    data[off + 8],
                    data[off + 9],
                    data[off + 10],
                ]) as u64,
            });
        }
        Ok(AckBatch {
            entries,
            fresh_count,
        })
    }
}
```

Bump the message type and rewrite the module doc comment's wire-format block to match the spec's layout:

```rust
pub const ACK_BATCH_MSG_TYPE: u8 = 0x05;
```

Delete the now-unused `ACK_ENTRY_SIZE` constant.

- [ ] **Step 4: Run the tests**

```bash
cargo test -p ghostframe-protocol ack 2>&1 | grep 'test result'
```

Expected: all pass, including `worst_case_batch_fits_the_documented_size` at exactly 671 bytes.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-protocol/src/ack.rs
git commit -m "feat(protocol): ACK 0x05 carries microsecond arrival times

Two sections: fresh entries as u16 microsecond deltas from a per-batch
base, overlap entries as absolute low-32-bit microseconds. 671 bytes worst
case against 650. Overlap gets absolutes so no age is unrepresentable and
nothing is dropped to make a batch fit."
```

## Task 2: Client producer

**Files:**
- Modify: `ghostframe-client-core/src/reassembly.rs`
- Modify: `ghostframe-client-core/src/ack_batcher.rs`

- [ ] **Step 1: Write the failing test**

Add to `ack_batcher.rs`'s test module (create `mod tests` if absent):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn entry(frame_seq: u32, arrival_us: u64) -> AckEntry {
        AckEntry {
            frame_seq,
            tile_x: 0,
            tile_y: 0,
            pass_idx: 0,
            arrival_us,
        }
    }

    /// The batch must tell the encoder where fresh ends, or overlap entries
    /// get encoded as deltas from a base they predate.
    #[test]
    fn flush_reports_the_fresh_count() {
        let mut b = AckBatcher::new();
        // First flush: three fresh, no overlap yet.
        for i in 0..3 {
            b.add(entry(i, 1_000 + i as u64), 1_000);
        }
        let first = b.flush().expect("entries queued");
        let decoded = AckBatch::decode(&first).unwrap();
        assert_eq!(decoded.fresh_count, 3);
        assert_eq!(decoded.entries.len(), 3, "no overlap on the first batch");

        // Second flush: two fresh, plus the three previous as overlap.
        for i in 10..12 {
            b.add(entry(i, 90_000 + i as u64), 90_000);
        }
        let second = b.flush().expect("entries queued");
        let decoded = AckBatch::decode(&second).unwrap();
        assert_eq!(decoded.fresh_count, 2);
        assert_eq!(decoded.entries.len(), 5, "two fresh plus three overlap");
        // Overlap timestamps survive exactly despite predating the base by
        // far more than a u16 of microseconds could express.
        assert_eq!(decoded.entries[2].arrival_us, 1_000);
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p ghostframe-client-core flush_reports_the_fresh_count 2>&1 | tail -5
```

Expected: compile error on `arrival_us` / `fresh_count`.

- [ ] **Step 3: Stop discarding precision, and record the split**

In `reassembly.rs`, both producer sites change from:

```rust
                arrival_time_ms_lo16: ((now_us / 1000) & 0xFFFF) as u16,
```

to:

```rust
                arrival_us: now_us & 0xFFFF_FFFF,
```

In `ack_batcher.rs`'s `flush`, replace the `AckBatch` construction:

```rust
        let fresh_count = fresh.len();
        let mut all_entries = fresh.clone();
        all_entries.extend(overlap);

        let batch = AckBatch {
            entries: all_entries,
            fresh_count,
        };
        // `try_encode` cannot fail here: fresh entries span at most
        // FLUSH_INTERVAL_US (5 ms), well inside a u16 of microseconds.
        let out = batch.encode();
```

- [ ] **Step 4: Run the tests**

```bash
cargo test -p ghostframe-client-core 2>&1 | grep 'test result'
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-core/src/reassembly.rs ghostframe-client-core/src/ack_batcher.rs
git commit -m "feat(client-core): ACK arrival times keep microsecond precision

reassembly.rs had now_us in microseconds and divided it away. The batcher
now reports where fresh entries end so the encoder can give each section
the encoding that suits it."
```

## Task 3: Oracle test

**Files:**
- Modify: `ghostframe-client-core/tests/oracle_ack.rs`

- [ ] **Step 1: Update the oracle to the new field and shape**

Replace `arrival_time_ms_lo16: ts` with `arrival_us: ts`, rename
`round_trips_arrival_time_ms_lo16` to `round_trips_arrival_us`, change its
assertion to `assert_eq!(batch.entries[0].arrival_us, 0xABCD)`, and add
`fresh_count` to every `AckBatch` literal (set it to `entries.len()` unless the
case is specifically about overlap).

- [ ] **Step 2: Run it**

```bash
cargo test -p ghostframe-client-core --test oracle_ack 2>&1 | grep 'test result'
```

Expected: all pass.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-client-core/tests/oracle_ack.rs
git commit -m "test(client-core): oracle follows the 0x05 ACK shape"
```

## Task 4: Unwrapper

**Files:**
- Modify: `ghostframe-lib/src/transport/bwe/timeline.rs`

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn microsecond_timeline_unwraps_across_a_32_bit_wrap() {
        let mut t = Lo32Timeline::default();
        let near_top = u32::MAX as u64 - 1_000;
        assert_eq!(t.unwrap_us(near_top as u32), near_top);
        // 2,000 us later, having wrapped past the top of the 32-bit space.
        assert_eq!(t.unwrap_us(999), near_top + 2_000);
    }

    #[test]
    fn an_overlap_entry_behind_the_base_stays_behind_it() {
        // Overlap entries are older than the base and can sit on the far side
        // of a wrap from it; the unwrapper must read that as backwards, not as
        // a 71-minute jump forwards.
        let mut t = Lo32Timeline::default();
        assert_eq!(t.unwrap_us(5_000), 5_000);
        assert_eq!(t.unwrap_us(4_000), 4_000);
    }
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test -p ghostframe-lib timeline 2>&1 | tail -5
```

Expected: `cannot find type Lo32Timeline`.

- [ ] **Step 3: Generalise the unwrapper**

Rename `Lo16Timeline` to `Lo32Timeline` and `unwrap_ms` to `unwrap_us`, taking
`u32`, with:

```rust
const HALF_PERIOD: i64 = 1 << 31;
const PERIOD: i64 = 1 << 32;
```

The body is otherwise unchanged — it is already generic sequence-space
arithmetic. Update the module doc to say microseconds and a 71.6-minute period.

- [ ] **Step 4: Run the tests**

```bash
cargo test -p ghostframe-lib timeline 2>&1 | grep 'test result'
```

Expected: all pass, including the pre-existing wrap tests once their literals
are rescaled to the 32-bit period.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/bwe/timeline.rs
git commit -m "feat(bwe): unwrap 32-bit microsecond arrival timestamps

Same sequence-space arithmetic over a 71.6-minute period instead of a
65.5-second one."
```

## Task 5: Server consumer

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Update the sample extraction**

At the BweSample construction site (~2697), replace:

```rust
                        let arrival_lo16 = e.arrival_time_ms_lo16;
                        let emit_lo16 = ((server_emit_us / 1000) & 0xFFFF) as u16;
                        let owd_ms_lo16 = arrival_lo16.wrapping_sub(emit_lo16);
```

with:

```rust
                        // Unwrap per section: fresh entries share a base, but
                        // each overlap entry is an independent absolute and
                        // may sit on the far side of a wrap from it.
                        let client_arrival_us = self.arrival_timeline.unwrap_us(e.arrival_us as u32);
                        let owd_us = client_arrival_us.wrapping_sub(server_emit_us);
```

Rename the `BweSample` fields `client_arrival_ms_lo16: u16` → `client_arrival_us: u64`
and `owd_ms_lo16: u16` → `owd_us: u64`, updating their doc comments to say
microseconds. `owd_us` keeps its `#[allow(dead_code)]` — it is still staged for
the Phase 2 controller and read nowhere.

Add the timeline to `IoBridge` (next to the other BWE state) and both
constructors:

```rust
    /// Unwraps the client's 32-bit microsecond arrival series. One instance
    /// per series — this must not be shared with any other timestamp stream.
    arrival_timeline: crate::transport::bwe::timeline::Lo32Timeline,
```

```rust
            arrival_timeline: Default::default(),
```

- [ ] **Step 2: Feed the unwrapped value to the estimator**

Where `AckArrival` records are built for `bwe.update`, pass
`client_arrival_us` instead of the lo16 field, and update `AckArrival`'s field
(`client_arrival_ms_lo16` → `client_arrival_us: u64`) and the driver's use of
it in `googcc.rs`:

```rust
            let recv_us = r.client_arrival_us as i64;
```

with `receive_time: Timestamp::from_micros(recv_us)`.

The driver's own `Lo16Timeline` member is removed (`googcc.rs` ~8, ~30, ~138)
— unwrapping now happens once in `io_bridge` rather than again inside the
driver.

`AckArrival::client_arrival_ms_lo16` lives in `bwe/mod.rs` (~42) and is
constructed in that module's own tests (~216, ~240, ~247) as well as in
`bwe_bench.rs`. Rename the field and rescale those literals from milliseconds
to microseconds — a test that still passes with millisecond-magnitude values
after the rename is asserting nothing about the new units.

- [ ] **Step 3: Build and run the lib suite**

```bash
cargo build -p ghostframe-lib 2>&1 | grep -E '^error' -A4 | head
cargo test -p ghostframe-lib 2>&1 | grep 'test result'
```

Expected: builds clean; all lib tests pass.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs ghostframe-lib/src/transport/bwe/
git commit -m "feat(bwe): consume microsecond ACK arrival times

Unwrapping moves to io_bridge, once per batch for the fresh section and
once per overlap entry, rather than per entry inside the driver."
```

## Task 6: Server send-side precision

**Files:**
- Modify: `ghostframe-lib/src/transport/bwe/googcc.rs`

- [ ] **Step 1: Make send timestamps microsecond-precise**

Replace:

```rust
                send_time: Timestamp::from_millis(send_ms),
```

with:

```rust
                // Microseconds, not milliseconds. `server_emit_us` already
                // carries that precision, and quantising it collapses a probe
                // burst into a single timestamp -- goog_cc rejects any cluster
                // whose `last_send - first_send` is zero.
                send_time: Timestamp::from_micros(r.server_emit_us as i64),
```

`send_ms` is still needed for `send_ms_max`; leave that computation in place.

- [ ] **Step 2: Run the BWE tests**

```bash
cargo test -p ghostframe-lib bwe 2>&1 | grep 'test result'
```

Expected: pass. If `probe_request_min_bytes_is_target_rate_times_duration` or
the bench tests move, report which and why before adjusting them — a moved
assertion here is a real behaviour change, not bookkeeping.

- [ ] **Step 3: Commit**

## Task 7: The measurement that justifies the change

**Files:** none — this task produces a number, not a diff.

- [ ] **Step 1: Re-run the probe-outcome tally**

```bash
cd /home/cedric/work/ghostframe
GF_TRACE_GOOGCC=1 RUST_LOG=goog_cc=debug \
  cargo test -p ghostframe-e2e --test browserless_runner step_up -- \
  --test-threads=1 --nocapture 2>&1 \
  | grep -oE 'Probing (successful|unsuccessful[^[]*)' | sort | uniq -c
```

Baseline to beat, measured 2026-09-14 before this change:

```
 23  Probing successful
 66  Probing unsuccessful, invalid send/receive interval
 32  Probing unsuccessful, receive/send ratio too high
```

- [ ] **Step 2: Report it honestly**

**If invalid-interval and ratio-too-high rejections fall materially, the change
worked.** Record the new tally.

**If they do not, say so plainly.** The change is then not justified by its own
stated criterion, and the right move is to report the null result rather than
keep it because it is tidy. Three earlier attempts at this ramp were reverted
for exactly that reason; a fourth is not a failure, it is the process working.

- [ ] **Step 3: Also check the ramp**

```bash
cargo test -p ghostframe-e2e --test browserless_runner step_up -- \
  --test-threads=1 --nocapture 2>&1 | grep -oE 'tail\(median\)=[0-9]+ +probes=[0-9]+/[0-9]+'
```

Baseline: `tail(median)` ~218k averaged over four runs, `probes=1/1` or `1/2`.
Run at least three times — one run is noise.

## Task 7b: wasm constant exports

**Files:**
- Modify: `ghostframe-client-wasm/src/constants.rs`

`ACK_ENTRY_SIZE` is exported to JavaScript as `ackEntrySize`. It has no
consumer — nothing in `ghostframe-web-client` or any test references it — but
the crate will not compile once the constant is deleted, and a single "entry
size" no longer describes a format with two section strides.

- [ ] **Step 1: Replace the export**

Change the import to:

```rust
use ghostframe_protocol::ack::{
    ACK_BATCH_MSG_TYPE, ACK_FRESH_ENTRY_SIZE, ACK_OVERLAP_COUNT, ACK_OVERLAP_ENTRY_SIZE,
    MAX_FRESH_ENTRIES_PER_BATCH,
};
```

and replace the single export with both strides:

```rust
export_const!(
    "ackFreshEntrySize",
    ack_fresh_entry_size,
    usize,
    ACK_FRESH_ENTRY_SIZE
);
export_const!(
    "ackOverlapEntrySize",
    ack_overlap_entry_size,
    usize,
    ACK_OVERLAP_ENTRY_SIZE
);
```

- [ ] **Step 2: Build for wasm**

```bash
cargo build -p ghostframe-client-wasm --target wasm32-unknown-unknown 2>&1 | grep -E '^error' -A4 | head
```

Expected: clean. The CI job that gates this target will otherwise fail after
everything else is green, which is a slow way to find it.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-client-wasm/src/constants.rs
git commit -m "feat(wasm): export both ACK section strides

ackEntrySize described a format with one entry size. 0x05 has two, so the
export becomes ackFreshEntrySize and ackOverlapEntrySize. Neither had a JS
consumer; the old name is not kept as an alias because it no longer names
anything real."
```

## Task 8: Verification and documentation

- [ ] **Step 1: Full sweep**

```bash
cargo test -p ghostframe-protocol 2>&1 | grep 'test result'
cargo test -p ghostframe-client-core 2>&1 | grep 'test result'
cargo test -p ghostframe-lib 2>&1 | grep 'test result'
cargo test -p ghostframe-e2e --test browserless_runner -- --test-threads=1 2>&1 | grep 'test result'
cargo clippy --workspace --all-targets 2>&1 | grep -E '^(error|warning)' | head
cargo fmt --all
```

- [ ] **Step 2: Record the outcome in the spec**

Append the measured probe tally to
`docs/specs/bwe-googcc-review.md`, under the section that records the 23/66/32
baseline, so the before and after sit together.

- [ ] **Step 3: Commit and open the PR**

---

## Done criteria

- [ ] `ACK_BATCH_MSG_TYPE` is `0x05`; a `0x04` batch is rejected with `WrongMsgType`.
- [ ] Worst-case batch is exactly 671 bytes.
- [ ] No entry is ever dropped to make a batch encode; an over-range fresh delta is an error, not a truncation.
- [ ] Overlap entries of any age round-trip exactly.
- [ ] A batch straddling a `u32` wrap decodes exactly.
- [ ] A truncated buffer is rejected at every cut point.
- [ ] Probe-outcome tally re-measured and reported either way.
- [ ] `cargo build -p ghostframe-client-wasm --target wasm32-unknown-unknown` clean.
- [ ] Workspace tests, clippy and fmt clean.

## What this plan does not do

- Change what is ACKed, when batches flush, or the overlap count.
- Touch tile datagrams or any other wire format.
- Address the source-rate gap or the ramp work it blocks — see
  `docs/specs/bwe-googcc-review.md`. Probe results have to be *accepted* before
  making them more frequent can matter.
