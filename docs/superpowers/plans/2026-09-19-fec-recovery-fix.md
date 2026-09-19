# FEC Recovery Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Make FEC recovery return the missing datagram's exact bytes, and stop it fabricating datagrams that were never sent.

**Architecture:** The parity envelope carries only the *first* source's length, so a recovered source that is shorter than the group's longest comes back left-padded with zeros and nothing trims it. The envelope will carry every source's length. Separately, group membership is assumed contiguous in `wire_seq`, but retransmissions allocate `wire_seq`s without joining a group, so groups straddle permanent holes and "recover" sources that never existed. Retransmissions will join their group, and the builder will assert contiguity.

**Tech Stack:** Rust. Wire-format change to the `TILE_PARITY` (0x04) envelope. Server and wasm client ship together from this repo; the TypeScript parity branch was already removed, so there is exactly one decoder.

---

## The two defects, both measured

**1. Recovery returns a padded buffer.** `xor_payloads` left-pads shorter sources. Reproduced by `recovery_restores_the_exact_bytes_of_a_short_source` (currently `#[ignore]`d in `ghostframe-client-core/src/parity_decoder.rs`):

```
left:  [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0, 170,187,204,221]   // 20 bytes returned
right: [170,187,204,221]                                      // the real 4-byte source
```

Tile datagrams are inherently variable-length, so this is the normal case. Never caught because every other test in that file builds its group from the fixed 8-byte `src()` helper, making the padding always zero.

**2. Groups straddle holes.** `self.group.add` is called only from `submit_one`, but `wire_seq`s are also allocated in `tick` (RTO retransmit) and `on_nack`. Those never join a group, so a group's assumed range `group_first..group_first+k` contains a `wire_seq` that is never a source — permanently "missing", so `missing_count == 1` forever and `try_recover` fabricates.

**Observed consequence:** two fabricated datagrams in a *lossless* browserless scene, decoding as codec `Skip` and codec `Raw` (renderable), carrying `wire_seq` values of `2^31` and `2^31+3`. The server processed those as acknowledgements. They also poisoned an RFC 9002 replay into reporting a 34.9% false-positive rate when the true figure, excluding them, is **zero**.

---

## Task 1: Carry every source's length in the envelope

**Files:** `ghostframe-protocol/src/protocol.rs`

Current layout, `TILE_PARITY_HEADER_SIZE = 9`:

```
[0]     discriminator (0x04)
[1..5]  group_first_wire_seq  u32 BE
[5]     k
[6]     parity_idx
[7..9]  group_first_payload_len u16 BE
[9..]   parity_payload
```

New layout — `group_first_payload_len` is replaced by a `k`-entry table, of which it was the first element:

```
[0]         discriminator (0x04)
[1..5]      group_first_wire_seq  u32 BE
[5]         k
[6]         parity_idx
[7..7+2k]   source_lens: k x u16 BE
[7+2k..]    parity_payload
```

- [ ] **Step 1: Write the failing round-trip test**

```rust
    #[test]
    fn parity_envelope_round_trips_every_source_length() {
        let env = TileParityEnvelope {
            group_first_wire_seq: 0xDEAD_BEEF,
            k: 3,
            parity_idx: 0,
            source_lens: vec![20, 4, 17],
            parity_payload: vec![1, 2, 3, 4, 5],
        };
        let mut buf = Vec::new();
        env.encode(&mut buf);
        let back = TileParityEnvelope::decode(&buf).expect("round trip");
        assert_eq!(back.source_lens, vec![20, 4, 17]);
        assert_eq!(back.group_first_wire_seq, 0xDEAD_BEEF);
        assert_eq!(back.k, 3);
        assert_eq!(back.parity_payload, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_parity_envelope_whose_length_table_is_truncated_is_rejected() {
        let env = TileParityEnvelope {
            group_first_wire_seq: 1,
            k: 4,
            parity_idx: 0,
            source_lens: vec![8, 8, 8, 8],
            parity_payload: vec![9; 8],
        };
        let mut buf = Vec::new();
        env.encode(&mut buf);
        // Truncate inside the length table: k says 4 entries, bytes stop short.
        buf.truncate(7 + 2 * 3);
        assert!(
            TileParityEnvelope::decode(&buf).is_err(),
            "a k that overruns the buffer must be rejected, not indexed past the end"
        );
    }
```

- [ ] **Step 2: Run, watch it fail** — `cargo test -p ghostframe-protocol --lib parity_envelope`. Expected: no field `source_lens`.

- [ ] **Step 3: Implement**

Replace `group_first_payload_len: u16` with `source_lens: Vec<u16>` on the struct, documented as *"byte length of each of the group's `k` sources, in `wire_seq` order starting at `group_first_wire_seq`. The decoder needs every one: XOR left-pads to the group's longest source, so a recovered source must be trimmed to its own length or it arrives with leading zeros."*

`TILE_PARITY_HEADER_SIZE` becomes 7 (the fixed part). Encode writes each length BE after the header. Decode must check `data.len() >= 7 + 2 * k as usize` **before** slicing, and return `ProtocolError::TooShort` otherwise — a wire-supplied `k` must never index past the buffer.

- [ ] **Step 4: Run the tests** — both pass.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-protocol/src/protocol.rs
git commit -m "feat(protocol): parity envelope carries every source length

XOR left-pads to the group's longest source, so a recovered source has
to be trimmed to its own length. The envelope carried only the first
source's, which is the one case that needs no trimming."
```

## Task 2: Server — record the lengths, and put retransmissions in their group

**Files:** `ghostframe-lib/src/transport/reliable_emitter/parity.rs`, `.../emitter.rs`

- [ ] **Step 1: Write the failing tests** (in `parity.rs`)

```rust
    #[test]
    fn group_result_carries_every_source_length() {
        let mut g = GroupBuilder::new(3);
        assert!(g.add(0, &[0u8; 20]).is_none());
        assert!(g.add(1, &[0u8; 4]).is_none());
        let r = g.add(2, &[0u8; 17]).expect("third source completes the group");
        assert_eq!(r.source_lens, vec![20, 4, 17]);
        assert_eq!(r.group_first_wire_seq, 0);
    }

    #[test]
    fn a_non_contiguous_wire_seq_starts_a_fresh_group() {
        // The decoder maps index i to `group_first + i`, so a gap would make
        // every length line up against the wrong source.
        let mut g = GroupBuilder::new(3);
        assert!(g.add(0, &[0u8; 8]).is_none());
        assert!(g.add(1, &[0u8; 8]).is_none());
        // 5 is not 2: the group so far is abandoned and 5 becomes the new first.
        assert!(g.add(5, &[0u8; 8]).is_none());
        assert!(g.add(6, &[0u8; 8]).is_none());
        let r = g.add(7, &[0u8; 8]).expect("the fresh group completes");
        assert_eq!(r.group_first_wire_seq, 5, "the abandoned group must not be reported");
        assert_eq!(r.source_lens.len(), 3);
    }
```

- [ ] **Step 2: Run, watch both fail.**

- [ ] **Step 3: Implement in `GroupBuilder`**

Add `source_lens: Vec<u16>` to `GroupResult`, and to the builder's state track `last_wire_seq: Option<u32>`. In `add`:

```rust
        // The decoder reconstructs membership as `group_first + 0..k`, so a
        // group must be contiguous in `wire_seq` or its length table lines up
        // against the wrong sources. A gap means some allocated `wire_seq`
        // never became a source; abandon the partial group rather than emit a
        // parity whose membership is a lie.
        if let Some(last) = self.last_wire_seq {
            if wire_seq != last.wrapping_add(1) {
                self.reset();
            }
        }
```

before the existing first-source bookkeeping, and set `self.last_wire_seq = Some(wire_seq)` after pushing. `reset` clears it.

- [ ] **Step 4: Feed retransmissions into the group** (in `emitter.rs`)

`tick` and `on_nack` each allocate a fresh `wire_seq` for the datagram they put on the wire. Both must call `self.group.add(ws, &bytes)` exactly as `submit_one` does, and emit the resulting parity envelope when a group completes. Extract the envelope-building block from `submit_one` into a private helper called from all three sites rather than copying it — three copies of parity framing is how they drift.

With this, every allocated `wire_seq` is a group member and the contiguity reset in Step 3 should never fire in production. It stays as a guard.

- [ ] **Step 5: Run the full lib suite** — `cargo test -p ghostframe-lib --lib`. Report the count.

- [ ] **Step 6: Commit**

## Task 3: Client — trim the recovery, and refuse to use a bad one

**Files:** `ghostframe-client-core/src/parity_decoder.rs`, `.../reassembly.rs`

- [ ] **Step 1: Un-ignore the reproduction**

Remove `#[ignore]` from `recovery_restores_the_exact_bytes_of_a_short_source` and update its fixture to build the envelope with `source_lens: vec![20, 4, 17]`. It must still fail before Step 2.

- [ ] **Step 2: Trim in `try_recover`**

The missing source's index within the group is `missing - parity.group_first_wire_seq`. Its true length is `parity.source_lens[index]`. The XOR result is left-padded to `target_len`, so the real bytes are the **last** `len` bytes:

```rust
        let idx = (missing_ws.wrapping_sub(parity.group_first_wire_seq)) as usize;
        let len = *parity.source_lens.get(idx)? as usize;
        if len > out.len() {
            return None; // wire-supplied length longer than the XOR result
        }
        let start = out.len() - len;
        Some(out[start..].to_vec())
```

Note `missing` is currently discarded via `let _ = missing;` — it is needed now.

- [ ] **Step 3: Validate before returning** — still in `try_recover`, after trimming:

```rust
        // A recovered buffer is reconstructed, not received. If the group's
        // membership was ever wrong the XOR yields plausible-looking bytes
        // that are not a datagram, and everything downstream -- render,
        // acknowledge -- would treat them as one. Measured: two such
        // fabrications reached the render path in a lossless scene.
        if !is_tile_datagram(&recovered) {
            return None;
        }
```

- [ ] **Step 4: Add the guard's own test**

```rust
    #[test]
    fn a_recovery_that_is_not_a_tile_datagram_is_discarded() {
        // Sources whose XOR cannot produce a valid tile datagram: no source
        // here has the tile flag set, so neither can the reconstruction.
        let a = vec![0x01u8; 24];
        let b = vec![0x02u8; 24];
        let mut parity = vec![0u8; 24];
        xor_into(&mut parity, &a);
        xor_into(&mut parity, &b);
        let env = TileParityEnvelope {
            group_first_wire_seq: 0,
            k: 2,
            parity_idx: 0,
            source_lens: vec![24, 24],
            parity_payload: parity,
        };
        let mut d = ParityDecoder::new(64);
        d.record_source(0, &a);
        assert!(
            d.receive_parity(&env).is_none(),
            "a reconstruction that is not a tile datagram must not be returned"
        );
    }
```

- [ ] **Step 5: Run** — `cargo test -p ghostframe-client-core --lib`. All pass, nothing ignored.

- [ ] **Step 6: Commit**

## Task 4: Prove it end to end

- [ ] **Step 1: Rebuild the wasm client**

```bash
cd ghostframe-web-client && npm install && npm run build && cd ..
```

The browserless harness needs a built `dist/`; a stale one fails as "timed out waiting for frame rendering" rather than as a protocol error.

- [ ] **Step 2: Confirm no fabricated `wire_seq` reaches the server**

Re-run the lossless storm scene with the `RACKPROBE` instrumentation and assert no acknowledged `wire_seq` exceeds the number of datagrams the scene actually sent. Before this fix there were 5: `131072`, `8388608`, `2147483648`, `2147483651`.

- [ ] **Step 3: Full verification**

```bash
cargo test -p ghostframe-protocol --lib
cargo test -p ghostframe-client-core --lib
cargo test -p ghostframe-lib --lib
cargo test -p ghostframe-e2e --test browserless_runner
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Browserless must stay at 19 passed / 1 ignored. The storm gate must stay under 400.

---

## Done when

- [ ] A recovered source of non-maximal length comes back byte-exact.
- [ ] A reconstruction that is not a tile datagram is discarded, with its own test.
- [ ] Retransmissions join their FEC group; the contiguity guard exists and does not fire.
- [ ] A truncated length table is rejected rather than indexed past.
- [ ] No fabricated `wire_seq` is acknowledged in the lossless scene (was 5).
- [ ] Workspace green, browserless 19/1, storm gate under 400.
