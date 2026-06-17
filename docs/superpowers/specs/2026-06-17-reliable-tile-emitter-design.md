# Reliable Tile Emitter — Design Spec

**Status:** Design approved, ready for implementation plan.
**Date:** 2026-06-17
**Branch:** to be created at plan time.

## 1. Summary

Add a new module, `ReliableTileEmitter`, that sits between the scheduler and
the WebTransport transport layer in `IoBridge`. The emitter takes tile-pass
work items from the scheduler and emits them with two layers of loss
protection: per-emission-batch XOR FEC parity for cheap within-window
recovery, and ACK-driven retransmission (server RTO + client NACK) for what
FEC misses. The emitter is **codec-agnostic**: any work item produced by the
scheduler — Solid, PalRle, Cdf53 pass, Raw, or a future codec — goes through
the same reliability pipeline. The H264 frame-fragment path is out of scope
for this work but the emitter is designed so the path *could* migrate later.

## 2. Motivation

Live measurement on evangeline shows the system loses ~44 % of emitted
cdf53 datagrams between the server's `wt.send_datagram()` returning `Ok` and
the browser's WebTransport reader handing the datagram to the assembler.
quinn-proto's congestion controller never sees this loss because it happens
*after* `send_datagram` returns — most likely in the kernel UDP send buffer
or the tsnet WireGuard userspace queue during the first-frame burst. Today
cdf53 has no FEC and no retransmission: the scheduler's `refinement_queue`
destructively removes items on emit, so any lost pass is lost forever.

The result is a partially-refined image (the "wizard mid-cleaning" failure
mode the user surfaced): tiles receive a random subset of their 14 cdf53
passes, distributed roughly binomially around `mean = 14 × (1 - loss_rate)`,
with 0 % probability of any tile reaching all 14 passes.

The fix needs to:

- Recover lost datagrams without depending on quinn's congestion control,
  which is blind to post-`send_datagram` loss.
- Cover every codec, including future codecs added without changes to the
  reliability layer.
- Compose cleanly with the existing M3.3d per-tile-pass ACK protocol and the
  scheduler's `bump_generation` supersession semantics.
- Be unit-testable and simulation-testable without a real network.

## 3. Goals and non-goals

**Goals**

- Every tile-pass emitted by the scheduler is delivered to the client with
  high probability at the expected steady-state loss regime
  (single-digit %, after the pacing fix). At extreme loss (≥50 %) the
  layer degrades gracefully — bounded retransmits, residuals counted in
  observability — rather than silently dropping data.
- The reliability layer is implemented once and inherited automatically by
  any codec routed through the scheduler.
- The wire format is additive: new envelope types and a 4-byte extension to
  one existing format. No existing field semantics change.
- The design is unit-testable (injected sender + clock) and
  simulation-testable (lossy mock sender, deterministic RNG seed).
- The cache memory footprint is bounded by an LRU; bursts that exceed the
  bound surface via a counter rather than silent failure.
- Retransmission honors the existing `bump_generation` supersession path:
  superseded generations are cancelled, not retried.

**Non-goals**

- Migrating the H264 frame-fragment path. The existing `fragment_frame` +
  `fec::generate_parity` + NACK retransmit machinery stays unchanged. The
  emitter is designed so the migration *could* happen in a follow-up, but
  the migration itself is not in this work.
- Cross-session optimization (don't retransmit to peer A if peer A already
  ACKed). Per-session reliability state is correct for v1; cross-session
  sharing is a future tightening.
- Replacing the scheduler. The scheduler keeps its current role
  (prioritization, generation tracking, budget allocation). The emitter is
  the layer below.
- Per-pixel error correction. FEC operates on whole datagrams.

## 4. Architecture

### 4.1 Module placement

New file: `ghostframe-lib/src/transport/reliable_emitter.rs`. Owns the
retransmit cache, FEC parity emission scheduling, RTO timer wheel, NACK
ingestion, and ACK rerouting from `dispatch_ack_datagram`.

Existing modules touched:

- `ghostframe-lib/src/transport/io_bridge.rs` — wires the emitter into the
  `drain_scheduler_into_quinn` loop and routes inbound ACK/NACK datagrams
  to the emitter.
- `ghostframe-lib/src/transport/scheduler.rs` — adds
  `cancel_pending_for_tile(tile_x, tile_y)` invoked from the existing
  `bump_generation*` paths; otherwise unchanged.
- `ghostframe-lib/src/transport/protocol.rs` — adds parsing/encoding for the
  two new envelope types and the 4-byte `wire_seq` extension to the
  tile-fragment `DatagramHeader`.
- `ghostframe-lib/src/transport/fragment_coverage.rs` — **deleted** at the
  end of the migration; the emitter's `RetransmitCache` replaces it.

New file: `ghostframe-web-client/src/parity_decoder.ts`. Owns the
sliding-window source buffer, parity reception, and XOR recovery.

New file: `ghostframe-web-client/src/nack.ts`. Mirror of the existing
`ack.ts`; batches missing-fragment identifiers and emits them as envelope
`0x05`.

Existing client modules touched:

- `ghostframe-web-client/src/main.ts` — adds parity envelope routing, hooks
  the `ParityDecoder` and `NackBatcher` into the per-rAF tick, threads
  recovered fragments back through the source-arrival path.
- `ghostframe-web-client/src/ack.ts` — adds `ACK_OVERLAP_COUNT = 8` trailing
  overlap to each batch.

### 4.2 Server-side data flow

```
process_frame_gpu
   │
   ▼
scheduler.tick(budget) ──Vec<TileWork>──▶ ReliableTileEmitter::submit_batch
                                                  │
                                                  ├── tag each work item with monotonic wire_seq
                                                  ├── fragment payloads via fragment_tile (unchanged)
                                                  ├── emit source fragments via send_to_all_sessions
                                                  ├── accumulate fragments in groups of K
                                                  ├── on K-th source, queue an XOR parity datagram
                                                  │   at offset +2K in the emission stream
                                                  ├── cache (frame_seq, tile, pass) → fragment bytes
                                                  └── schedule RTO per cache entry

   Event::DatagramsUnblocked  ──▶ unchanged (pacing layer's continuation hook)
   ACK envelope 0x03 inbound  ──▶ emitter.on_ack(batch) — drops cache entries, marks scheduler
   NACK envelope 0x05 inbound ──▶ emitter.on_nack(batch) — looks up cache, re-emits one fragment
   RTO fires for a key        ──▶ emitter.retransmit(key) — re-emits all fragments of the tile-pass
   scheduler.bump_generation* ──▶ emitter.cancel_for_tile(x, y) — drops cache entries
```

### 4.3 Client-side data flow

```
WebTransport datagram
   │
   ▼
classify by envelope byte ─┬─ source tile fragment ──▶ ParityDecoder.recordSource(wire_seq, bytes)
                           │                          └▶ FragmentAssembler (existing)
                           │                                │
                           │                                ├── on partial-assembly timeout ──▶ NackBatcher
                           │                                └── on full assembly ──▶ codec queue + AckBatcher
                           │
                           └─ FEC parity (0x04) ──▶ ParityDecoder.receiveParity(parity)
                                                          │
                                                          └── if recovered ──▶ FragmentAssembler
                                                              (synthetic source arrival, same code path)
```

### 4.4 Key types

- `EmitKey = (frame_seq: u32, tile_x: u8, tile_y: u8, pass_idx: u8)` —
  matches the M3.3d ACK key exactly. Used for ACK, NACK, RTO, and
  `cancel_for_tile`.
- `WireSeq = u32` — per-session monotonic counter, allocated by the
  emitter at source-datagram emission time. The FEC group key. Wraps at
  `u32::MAX` (~155 days at 30 fps × 100 datagrams/frame — outside session
  lifetimes).
- `RetransmitCache: LruCache<EmitKey, CacheEntry>` — capacity 8192 entries.
- `CacheEntry { fragments: SmallVec<[Bytes; 2]>, wire_seqs:
  SmallVec<[u32; 2]>, first_sent_at: Instant, last_sent_at: Instant,
  attempts: u8, rto_deadline: Instant }`.

## 5. Wire protocol additions

Three changes. All additive — old fields keep their meaning; old clients
without parity/NACK support still receive source datagrams correctly.

### 5.1 Source tile-fragment datagram: add `wire_seq` to `DatagramHeader`

Before:
```
[frame_seq u32 BE | frag_idx u16 BE | frag_total u16 BE | timestamp_us u32 BE]
                       12 bytes total
[TileHeader 8B][payload]
```

After:
```
[frame_seq u32 BE | frag_idx u16 BE | frag_total u16 BE | wire_seq u32 BE | timestamp_us u32 BE]
                                          16 bytes total
[TileHeader 8B][payload]
```

`wire_seq` identifies the source datagram inside its FEC group; the parity
datagram references a range of `wire_seq` values. Cost: 4 bytes per
tile-fragment datagram, ~0.8 % overhead at typical payloads. The
`FrameHeader` used by H264 frame-fragment datagrams is unchanged.

### 5.2 New envelope `0x04 = TILE_PARITY`

```
[0x04][group_first_wire_seq u32 BE][K u8][parity_idx u8][group_first_payload_len u16 BE][parity_payload]
```

Fields:

- `group_first_wire_seq` — `wire_seq` of the first source in the group.
- `K` — number of source datagrams covered. Client looks for sources with
  `wire_seq ∈ [group_first_wire_seq, group_first_wire_seq + K)`.
- `parity_idx` — parity index within the group (0-indexed; for v1 always 0,
  since the knob `FEC_PARITY_PER_GROUP_R = 1` means one parity per group;
  reserved for future multi-parity FEC where parity_idx ∈ [0, R)).
- `group_first_payload_len` — length of the first source in the group, used
  by the decoder to extract a recovered payload of the right size (XOR
  parity is padded to the longest source in the group).
- `parity_payload` — XOR of the K source datagrams' full bytes (the
  source datagrams have no envelope byte; their first 4 bytes are the
  `frame_seq` field of `DatagramHeader`, with bit 31 set per
  `TILE_DATAGRAM_FLAG`). Each source is left-padded with zeros to the
  longest source's length in the group before XOR.

Recovery: when the client has K-1 of K source `wire_seq` values in its
window and the parity arrives, XOR all received plus the parity → recovers
the one missing source's bytes verbatim: `DatagramHeader` (including the
source's own `wire_seq`, `frag_idx`, `frag_total`) followed by `TileHeader`
followed by `payload`. The recovered byte buffer is routed through the same
source-arrival path as a real source arrival — the codec dispatcher reads
the recovered headers identically.

When the loss exceeds R=1 in a group, the parity is buffered briefly
(~50 ms) in case a re-ordered source arrives, then discarded.

### 5.3 New envelope `0x05 = TILE_NACK`

```
[0x05][count u8][count × NackEntry]
where NackEntry = [frame_seq u32 LE | tile_x u8 | tile_y u8 | pass_idx u8 | frag_idx u8]
                            8 bytes per entry
```

Max 64 entries per datagram. Mirror-symmetric with envelope `0x03` (ACK)
deliberately — same batching shape, same parsing pattern. NACK is
*fragment-grained*, not tile-pass-grained, because the client's natural
detection unit is "I had fragments 0 and 2 of (frame, tile, pass), 1 timed
out".

If the *first* fragment of a tile-pass is lost the client can't NACK (it
doesn't know the tile-pass exists). Those losses fall to server RTO.

### 5.4 Envelope-routing table

| First byte | Type | Notes |
|---|---|---|
| `0x01` | SETTINGS / HELLO | existing |
| `0x02` | ACK rev1 | existing, deprecated |
| `0x03` | ACK rev2 | existing, reused |
| `0x04` | TILE_PARITY | new |
| `0x05` | TILE_NACK | new |
| `0x10`–`0x7F` | frame-fragment (H264) | bit 31 of frame_seq = 0 |
| `0x80`–`0xFF` | tile-fragment | bit 31 of frame_seq = 1 (TILE_DATAGRAM_FLAG) |

The reserved-envelope range `0x00`–`0x0F` is unambiguous as long as
`frame_seq` stays under `0x10_000_000`. At 30 fps that's ~155 days of
continuous session, well outside any expected session lifetime.

## 6. Server-side components

### 6.1 RetransmitCache

```rust
struct RetransmitCache {
    entries: HashMap<EmitKey, CacheEntry>,
    lru: lru::LruCache<EmitKey, ()>,    // capacity = CACHE_CAPACITY
}

struct CacheEntry {
    fragments: SmallVec<[Bytes; 2]>,
    wire_seqs:  SmallVec<[u32; 2]>,
    first_sent_at: Instant,
    last_sent_at:  Instant,
    attempts: u8,
    rto_deadline: Instant,
}
```

Bounded by LRU at `CACHE_CAPACITY = 8192` entries. Memory ceiling:
~500 B × 8192 = ~4 MB per session. Eviction is equivalent to
`MAX_RETRANSMITS` reached: the tile-pass is given up on.

### 6.2 WireSeqAllocator

```rust
struct WireSeqAllocator(u32);
```

Per-session monotonic counter. Hands out the next `wire_seq` on every
source emission. Wraps at `u32::MAX`.

### 6.3 EmissionQueue with offset-interleaved parity

```rust
struct EmissionQueue {
    queue: VecDeque<Emission>,
    pending_parity: BinaryHeap<(WireSeq /* emit_after */, ParityDatagram)>,
    end_of_stream_flush_at: Option<Instant>,
}
```

On submission of K source datagrams, the parity is computed and pushed onto
`pending_parity` with `emit_after = group_first_wire_seq + 2K`. The
`emit_next` loop promotes parities whose `emit_after` is `≤ next_wire_seq`,
then pops the queue's head.

End-of-stream flush: if the queue is empty and `pending_parity` is
non-empty, the emitter starts a timer for `END_OF_STREAM_PARITY_FLUSH_MS`.
When it fires, all pending parity is promoted unconditionally. This handles
the tail of a burst where there's no following group to interleave against.

### 6.4 RtoTimerWheel

```rust
struct RtoTimerWheel {
    heap: BinaryHeap<RtoEntry>,    // min-heap by deadline
}

struct RtoEntry {
    deadline: Instant,
    key: EmitKey,
}
```

No tokio timer tasks. `emitter.tick(now)` is invoked from the existing
`process_frame_gpu` loop; it pops entries whose `deadline ≤ now`, validates
each against the live cache (a cancelled or already-ACKed entry is a no-op),
and retransmits live entries with the same `wire_seq` values as the original
emission (so client dedup keys remain stable).

RTO computation:

```rust
fn next_rto(&self, attempts: u8) -> Duration {
    let base = (self.smoothed_rtt * 2).max(Duration::from_millis(25));
    let base = base.min(Duration::from_millis(BASE_RTO_MS));    // base ≤ 50 ms
    base * (1u32 << attempts.min(3))    // base × {1, 2, 4, 8}
}
```

`base` is clamped to `[25 ms, BASE_RTO_MS = 50 ms]`. Exponential backoff
then multiplies by `{1, 2, 4, 8}` for attempts 0–3, giving a worst-case
deadline of `50 × 8 = 400 ms` and a best-case (low-RTT) deadline of
`25 × 1 = 25 ms` on attempt 0.

The `BASE_RTO_MS = 50` upper bound is deliberate: it sits below typical
Montreal–Vancouver RTT (~60 ms), so on continental WAN paths RTO fires
*before* a NACK round-trip could complete. On LAN/tailnet (RTT ~5–20 ms),
NACK arrives well before RTO. The two mechanisms cover the latency spectrum
without overlap.

### 6.5 ACK ingestion

The existing `dispatch_ack_datagram` flow is consolidated into one emitter
call:

```rust
fn on_ack(&mut self, batch: AckBatch) {
    for entry in batch.entries {
        let key = EmitKey::from(entry);
        if self.cache.remove(&key).is_some() {
            self.scheduler.mark_acked(key);
            self.stats.ack_hit += 1;
        } else {
            self.stats.ack_miss += 1;
        }
        if key.codec_is_cdf53() {
            self.scheduler.record_cdf53_ack(key);
        }
    }
}
```

`ack_miss` covers two cases: the entry was already evicted (LRU or
`MAX_RETRANSMITS`), or the entry was cancelled by a `bump_generation`. Both
are silent — no correctness implication, just a counter.

### 6.6 NACK ingestion

```rust
fn on_nack(&mut self, batch: NackBatch) {
    for entry in batch.entries {
        let key = EmitKey::from(entry);
        let Some(cache_entry) = self.cache.get_mut(&key) else {
            self.stats.nack_miss += 1;
            continue;
        };
        if cache_entry.attempts >= MAX_RETRANSMITS { continue; }
        let Some(frag) = cache_entry.fragments.get(entry.frag_idx as usize) else { continue; };
        let wire_seq = cache_entry.wire_seqs[entry.frag_idx as usize];
        self.send_with_wire_seq(frag, wire_seq);
        cache_entry.attempts += 1;
        cache_entry.last_sent_at = Instant::now();
        // RTO entry in heap is implicitly cancelled by the cache update:
        // when popped, validation sees last_sent_at moved forward and skips.
    }
}
```

NACK re-emits a single fragment, not the whole tile-pass. NACK arrival
implicitly cancels the next RTO retransmit by advancing `last_sent_at`.

### 6.7 Cancellation

```rust
fn cancel_for_tile(&mut self, tile_x: u8, tile_y: u8) {
    self.cache.entries.retain(|k, _| !(k.tile_x == tile_x && k.tile_y == tile_y));
    // RTO heap entries are not explicitly removed; the validation check at
    // pop time drops them when the cache lookup misses.
}
```

Called from `scheduler.bump_generation*` paths. Late ACKs/NACKs arriving
after cancellation become silent misses.

### 6.8 Plumbing into IoBridge

Three surgical changes in `io_bridge.rs`:

1. `IoBridge::new` constructs a `ReliableTileEmitter` per session.
2. `drain_scheduler_into_quinn` becomes
   `emitter.submit_batch(scheduler.tick(budget))`. The emitter handles
   fragmentation, FEC parity computation, wire emission, and caching. The
   pacing layer (quinn-buffer cap, AIMD multiplier, `DatagramsUnblocked`
   continuation) keeps operating on the emitter's *output* datagrams.
3. `dispatch_ack_datagram` and a new `dispatch_nack_datagram` route into
   `emitter.on_ack` / `emitter.on_nack`.

The existing `FragmentCoverageMap` and its callers are deleted: see §8.4.

### 6.9 Observability counters

Surfaced via the existing periodic `cumulative emit` log line (every
60 frames, `ghostframe` target):

- `fec_parity_emitted` — count of parity datagrams sent.
- `nack_received`, `nack_hit`, `nack_miss`.
- `rto_fired`, `rto_max_retransmits_reached`.
- `cache_lru_eviction` — non-zero means in-flight tile-passes exceed
  capacity (signal, not silent failure).
- `retransmit_attempts_total`.

## 7. Client-side components

### 7.1 ParityDecoder

```typescript
class ParityDecoder {
  private window = new Map<number, Uint8Array>();   // wire_seq → raw datagram bytes
  private pendingParities = new Map<number, ParityHeader>();
  private windowOrder: number[] = [];   // for eviction order

  recordSource(wireSeq: number, bytes: Uint8Array): void;
  receiveParity(parity: ParityHeader): Uint8Array | null;
  private findSingleMissing(groupFirstWireSeq: number, K: number): number | null;
  private xorRecover(missing: number, groupFirst: number, K: number, parity: Uint8Array): Uint8Array;
  private evictOldest(): void;
}
```

Sliding window of `WIRE_SEQ_WINDOW = 40` source entries (4 × K). Buffered
parities are discarded after ~50 ms if the missing source never arrives.

Recovery: when `findSingleMissing` returns exactly one missing `wire_seq` in
the group, the decoder XORs the K-1 received sources plus the parity →
recovers the missing source's bytes. The recovered bytes are a fully-formed
tile-fragment datagram (`DatagramHeader` + `TileHeader` + payload, no
envelope byte — source tile-fragment datagrams are dispatched by the
`TILE_DATAGRAM_FLAG` bit of `frame_seq`, not by an envelope); they are
routed through the same source-arrival path as a real arrival, identical
code path.

### 7.2 FragmentAssembler timeout extension

The existing `TileAssembly` in `main.ts` gains:

```typescript
interface TileAssembly {
  fragments: (Uint8Array | null)[];
  received: number;
  fragTotal: number;
  emitKey: EmitKey;
  partialSince: number;
  nackedFragIdxs: Set<number>;
}
```

A per-rAF scan checks for assemblies with `received < fragTotal` and
`now - partialSince > ASSEMBLY_TIMEOUT_MS = 30`. Each missing
`frag_idx` not yet NACKed gets added to the `NackBatcher`. After a second
timeout interval, the dedup set is cleared (allowing one re-NACK if the
first was lost), then the entry is given up on — server RTO is the safety
net.

### 7.3 NackBatcher

Direct mirror of `AckBatcher`:

```typescript
class NackBatcher {
  private entries: NackEntry[] = [];
  add(key: EmitKey, fragIdx: number): void;
  flushNow(): void;
}
```

Flush triggers: count ≥ 64 or `NACK_BATCH_FLUSH_MS = 5` since first add.
Wire format: envelope `0x05`, 8 bytes per entry.

### 7.4 AckBatcher overlap

Trivial extension: each flushed batch includes the last
`ACK_OVERLAP_COUNT = 8` entries from prior batches at the tail. A single
lost ACK datagram now requires 9 consecutive ACK datagram losses for any
entry to be unACKed from the server's perspective. At 44 % loss, that's
~0.06 % probability — effectively eliminates the spurious-retransmit
failure mode.

### 7.5 Wiring in main.ts

```typescript
function handleDatagram(value: Uint8Array) {
  const first = value[0];
  if (first === 0x03) { handleAck(value); return; }
  if (first === 0x04) {
    const parity = parseParityEnvelope(value);
    const recovered = parityDecoder.receiveParity(parity);
    if (recovered !== null) {
      diagnostics.fecRecovered++;
      handleSourceTileDatagram(recovered);
    }
    return;
  }
  const wireSeq = readWireSeq(value);
  parityDecoder.recordSource(wireSeq, value);
  handleSourceTileDatagram(value);
}
```

Envelope `0x05` is server→client (NACK) and never received on the client
side.

### 7.6 Observability counters

Extends the existing periodic `cdf53-coverage` log line with a new
`fec-coverage` line:

- `fec_recovered` — count of parity recoveries that yielded a valid source.
- `parity_rx` — total parity datagrams received.
- `parity_unrecoverable` — parities discarded because >1 source was missing
  in the group.
- `nack_sent` — total NACK entries emitted.

## 8. Cancellation, lifecycle, and migration

### 8.1 Cancellation on `bump_generation`

`scheduler.bump_generation*` already marks pending refinement work as
`Superseded` and calls `fragment_coverage.drop_cdf53_for_tile`. The new
flow additionally calls `emitter.cancel_for_tile(tile_x, tile_y)`. Late
ACKs/NACKs for the cancelled generation become silent misses (counted but
not error-flagged).

### 8.2 Retransmit retirement

After `MAX_RETRANSMITS = 4` attempts without ACK, the cache entry is
dropped. The tile-pass is given up on; the next `bump_generation` (if any)
will give it a fresh chance.

Math at the design's parameters. With independent per-datagram loss rate
`p`, per-attempt delivery probability after the FEC layer is

```
P(delivered | one attempt) = (1 − p)                              # source arrived directly
                           + p × (1 − p)^(K − 1) × (1 − p)        # source lost, parity + others all arrived
                           = (1 − p) × (1 + p × (1 − p)^(K − 1))
```

With `K = 10, R = 1`:

| Loss rate `p` | Per-attempt delivery | After 5 attempts |
|---|---|---|
| 5 % | 0.95 + 0.05 × 0.60 = 0.98 | 1 − 0.02⁵ ≈ 99.99999 % |
| 10 % | 0.90 + 0.10 × 0.35 = 0.935 | 1 − 0.065⁵ ≈ 99.9988 % |
| 30 % | 0.70 + 0.30 × 0.034 = 0.71 | 1 − 0.29⁵ ≈ 99.8 % |
| 50 % | 0.50 + 0.50 × 0.001 = 0.5005 | 1 − 0.4995⁵ ≈ 96.9 % |

At our expected steady-state loss (single-digit %, after the pacing fix),
delivery is essentially indistinguishable from 100 %. At today's measured
44 % loss the bound is ~98 % — visible but bounded, and the residual is
gracefully handled by the next generation bump for content that continues
to update.

The `MAX_RETRANSMITS = 4` cap is what bounds the cache memory at
`O(in_flight × retransmit_window)`. A larger cap would shift the tail-loss
asymptote further but at increasing memory cost; 4 is the design's chosen
balance.

### 8.3 Cache eviction

LRU at `CACHE_CAPACITY = 8192`. Eviction-before-MAX_RETRANSMITS is treated
identically to MAX_RETRANSMITS reached. The `cache_lru_eviction` counter
should stay at zero under the pacing-fixed steady state; non-zero indicates
the burst exceeded design parameters and is a signal to retune knobs.

### 8.4 Migration: replacing FragmentCoverageMap

The current `FragmentCoverageMap` (60K-entry LRU in
`transport/fragment_coverage.rs`) is fully subsumed by the emitter's
`RetransmitCache`. Migration steps:

1. `fragment_coverage.record(...)` → folded into
   `emitter.submit_batch(...)`'s natural recording at emit time.
2. `fragment_coverage.take(...)` → folded into
   `emitter.on_ack(...)`'s cache removal.
3. `fragment_coverage.drop_cdf53_for_tile(...)` → folded into
   `emitter.cancel_for_tile(...)`.
4. `pending_refinement_snapshot()` diagnostic accessor → exposed as
   `emitter.diagnostic_snapshot()` with the same shape.
5. The `cdf53_passes_acked` map *stays in the scheduler* — it's a different
   concern (PixelPerfect promotion). The emitter calls
   `scheduler.record_cdf53_ack(key)` from its `on_ack` path.

At the end of migration, `fragment_coverage.rs` and its tests are deleted.

### 8.5 Session lifecycle

- New session accepted (`Event::NewConnection`): the emitter constructs a
  per-session `RetransmitCache`, `WireSeqAllocator`, and `RtoTimerWheel`.
- Session drained (`Event::ConnectionLost`): the emitter drops the session's
  state. Pending retransmits are abandoned. No corruption — the shared
  scheduler is unchanged.
- The shared scheduler queue is the single source of truth for "what
  tile-pass work exists"; the emitter is per-session reliability.

Multi-session note: when send-to-all-sessions emits to N peers, each peer's
emitter records the emission with its own `wire_seq` (they coincide if all
peers connected at the same time, otherwise they diverge). ACKs from peer A
only clear peer A's cache. This is the simple, correct model. Cross-peer
optimization is a future tightening.

## 9. Knob defaults

All in `reliable_emitter.rs` as `const`:

| Knob | Default | Where it bites |
|---|---|---|
| `FEC_GROUP_SIZE_K` | 10 | Bandwidth overhead = R/K = 10 % |
| `FEC_PARITY_PER_GROUP_R` | 1 | XOR FEC, single-loss recovery per group |
| `PARITY_INTERLEAVE_OFFSET` | 2K = 20 | Burst-loss separation between parity and its sources |
| `END_OF_STREAM_PARITY_FLUSH_MS` | 5 | Tail-of-burst parity flush |
| `MAX_RETRANSMITS` | 4 | Bounded retransmit budget |
| `BASE_RTO_MS` | 50 | Upper bound on RTO base — chosen below typical continental WAN RTT |
| `RTO_BACKOFF_FACTOR` | 2 | Exponential backoff: base × {1, 2, 4, 8}, worst-case 400 ms |
| `CACHE_CAPACITY` | 8192 | LRU bound on outstanding tile-passes per session |
| `ACK_OVERLAP_COUNT` | 8 | Client ACK batch trailing overlap |
| `ASSEMBLY_TIMEOUT_MS` | 30 | Client partial-assembly NACK trigger |
| `NACK_BATCH_FLUSH_MS` | 5 | Client NACK batcher flush window |
| `WIRE_SEQ_WINDOW` | 4 × K = 40 | Client parity-buffer sliding-window size |

All values are starting points, to be tuned by the bench infrastructure
once we have live measurements.

## 10. Testing strategy

### 10.1 Unit tests (pure logic, no I/O)

Each sub-component is tested in isolation:

- `WireSeqAllocator` monotonicity, u32 wrap.
- `RetransmitCache` insert/lookup/evict, LRU semantics, `cancel_for_tile`
  filtering.
- `EmissionQueue` offset insertion (parity at exactly +2K from group start),
  end-of-stream flush after `END_OF_STREAM_PARITY_FLUSH_MS`.
- `GroupBuilder` accumulates K sources, fires parity computation at K-th
  source, resets state.
- `RtoTimerWheel` heap ordering, validation-on-pop drops stale entries.
- XOR parity primitive: left-padding correctness, round-trip recovery.
- Client `ParityDecoder` window eviction, single-missing recovery,
  multi-missing rejection, parity-buffered-before-source ordering.
- Client `AckBatcher` overlap: each batch carries last N entries from
  prior batches.
- Client `NackBatcher`: count-flush and timeout-flush.

Target: ≥30 unit tests, all under 1 ms each.

### 10.2 Integration tests (emitter + mock sender + mock clock)

The emitter takes `Sender` and `Clock` as injected traits:

```rust
trait DatagramSender { fn send(&mut self, dg: &[u8]); }
trait Clock { fn now(&self) -> Instant; }
```

Tests use `MockSender(Vec<Bytes>)` + `MockClock(Instant)` to drive the
emitter synchronously and assert the exact emission sequence. Sample tests:

- Submit 25 single-fragment tile-passes → assert wire emission is
  `source × 10, parity × 1, source × 10, parity × 1, source × 5`, with the
  first parity at `wire_seq = 20`.
- Submit one tile-pass, no ACK arrives, advance clock by 50 ms → assert one
  retransmit emission with same `wire_seq`.
- Submit, retransmit 4 times, advance clock past 5th RTO → assert no 5th
  emission, `rto_max_retransmits_reached` counter +1.
- Submit, NACK arrives for `frag_idx=1` of a 3-fragment tile-pass → assert
  exactly one re-emission, that of `frag_idx=1`.
- Submit, ACK arrives, advance clock past RTO → assert no retransmit.
- Submit pass-0 of tile (5,5) gen=1, `bump_generation(5,5)`, submit gen=2 →
  assert gen=1 cache cleared, no gen=1 retransmits ever fire.

Target: ≥20 integration tests, all under 10 ms each.

### 10.3 Simulation tests (lossy sender, deterministic RNG)

The existing `LossInjector` feature is wired as a `MockSender` that drops
fraction `p` of inputs. Tests use a fixed RNG seed for byte-identical
reruns.

| Scenario | Loss | Assertion |
|---|---|---|
| Clean wire | 0 % | Zero retransmits/NACKs/FEC recoveries; bandwidth ≈ 110 % of source |
| Low loss | 5 % | ≥99 % tile-passes ACKed within 100 ms; bandwidth ≤ 125 % |
| High loss | 50 % | ≥95 % tile-passes ACKed within 500 ms (per §8.2 math); residuals counted as `rto_max_retransmits_reached`; bandwidth ≤ 220 % |
| Bursty loss | 3 consecutive drops every 50 | Groups with ≤ R losses are FEC-recovered (proves interleave-offset spreads bursts across groups); groups with > R losses fall to retransmit |
| ACK loss | 30 % inbound | Server spuriously retransmits at most one tile-pass per missed ACK (ACK_OVERLAP_COUNT mitigation); client dedups by `wire_seq` |
| Generation churn | bump every tile every 100 ms | No retransmits for superseded gens; `cache_lru_eviction = 0` |

Each scenario runs 10,000+ tile-pass submissions with `--test-threads=1`.

Target: ≥6 simulation tests.

### 10.4 Property tests (proptest)

Invariants checked against random inputs:

- `submit + arbitrary ACK/NACK/RTO/cancel sequence → eventually
  cache.is_empty()`.
- `xor_recover(K-1 of K sources, parity) == missing source` for any byte
  pattern.
- `cache.len() ≤ CACHE_CAPACITY` after any operation sequence.
- `WireSeqAllocator` strictly increasing.
- `cancel_for_tile(x, y)` removes exactly matching entries.
- `RtoTimerWheel.pop()` always returns the earliest-deadline live entry;
  stale entries are skipped.

Target: ≥10 proptest properties, 256 random cases each.

### 10.5 Client-side tests (vitest)

New test files:

- `parity_decoder.test.ts` — feed sources + parity, assert recovered bytes
  match an out-of-band reference.
- `nack.test.ts` — mirror of `ack.test.ts`, validates flush triggers and
  wire format.

Plus extend `ack.test.ts` with overlap behavior.

Target: ≥15 new vitest tests.

### 10.6 End-to-end tests

Two new e2e tests gated to run under the existing Docker container:

- `e2e_reliable_emitter_30pct_loss` — drives the headless Chrome client
  through a session with 30 % outbound loss; asserts all cdf53 tiles reach
  `PixelPerfect` within a bounded time. Today this test would fail (no
  retransmit); after this work it passes deterministically.
- `e2e_reliable_emitter_burst_loss` — drops 10 consecutive datagrams every
  100; asserts FEC + NACK + RTO together deliver everything.

### 10.7 Live observability validation

Server `cumulative emit` + client `fec-coverage` lines together let us
verify the fix on the live deployment. Note that with retransmission in
place, `server.emitted_cdf53` (source emissions) counts every wire
attempt — including retransmits using the same `wire_seq` — whereas
`client.rx_cdf53` (deduped by `wire_seq`) counts unique sources received.
The headline check is at the *logical* level:

- `client.cdf53-coverage refined > 0`, ideally `refined ≈ total_cdf53_tiles`.
  This is the primary success signal — tiles should now reach all 14
  passes (or whatever the codec's max is).
- `server.rto_max_retransmits_reached` near zero except under genuine link
  failure (i.e., loss above the design's tolerance).
- `client.fec_recovered > 0` confirms FEC is providing value at non-zero
  loss; `client.parity_unrecoverable / client.parity_rx` ratio is the
  burst-loss indicator.
- `server.cache_lru_eviction = 0` in steady state. Non-zero means the
  burst is exceeding `CACHE_CAPACITY` and a knob retune is warranted.

For the loss-bound math, the relevant identity (unique tile-passes) is:

```
unique tile-pass submissions = unique tile-pass deliveries
                             + tile-passes evicted before delivery
                             + tile-passes hitting MAX_RETRANSMITS
```

The first term is `server.submit_batch` calls (logical); the right-hand
side terms are exposed counters. A non-zero gap is a regression signal.

### 10.8 Why this is testable end-to-end

- The emitter takes injected `Sender` and `Clock` traits → no network, no
  real time. Integration tests run in 200 ms.
- Loss injection plugs at the `Sender` trait → simulation tests don't need
  Docker.
- Cache, heap, queue are plain data structures → proptest can shake them.
- Scheduler tests stay green: we wrapped, not replaced.
- Client modules (`parity_decoder`, `nack`) are pure TypeScript → vitest
  covers them without a browser.

## 11. Open questions and future work

- **Adaptive K/R based on measured loss.** v1 ships with fixed `K=10, R=1`.
  Once we have live loss measurements, adaptive sizing (e.g. R=2 when
  measured loss > 20 %) is a follow-up.
- **H264 path migration.** The emitter is designed so the H264 frame-fragment
  path could adopt the same reliability layer in a follow-up. Not in scope
  for v1.
- **Cross-session optimization.** Per-session caching duplicates state under
  multi-client. A future optimization shares source bytes across sessions
  while keeping per-session ACK/RTO bookkeeping.
- **Reed-Solomon (multi-loss) FEC.** XOR is single-loss per group. If
  measured loss patterns suggest multi-loss per group is common,
  Reed-Solomon over GF(2^8) is a drop-in replacement for the parity
  primitive without protocol changes (R can simply rise above 1).

## 12. References

- Live measurement context: 44 % UDP loss on evangeline tailnet, confirmed
  via the diagnostics in commit `24af78c` (cumulative emit counters + bump
  counts + refinement queue depth).
- M3.3d datagram-level ACK protocol:
  `docs/superpowers/specs/2026-06-01-m3.3d-datagram-level-ack-design.md`
- M3.3c progressive refinement:
  `docs/superpowers/specs/2026-05-30-m3.3c-progressive-refinement-design.md`
- Existing H264 FEC: `ghostframe-lib/src/transport/fec.rs`,
  `ghostframe-web-client/src/fec.ts`.
- Existing fragment coverage tracking:
  `ghostframe-lib/src/transport/fragment_coverage.rs` (to be deleted post-migration).
