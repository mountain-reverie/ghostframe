# Microsecond arrival timestamps in the ACK envelope — Design

**Status:** proposed
**Motivation:** goog_cc discards 80% of our probe measurements, and the binding
cause is wire-format resolution.

## The problem, measured

`ProbeBitrateEstimator` logs why it rejects a cluster. With a subscriber
installed, one step-up scene reports:

```
 23  Probing successful
 66  Probing unsuccessful, invalid send/receive interval
 32  Probing unsuccessful, receive/send ratio too high
```

goog_cc computes `receive_rate = size / (last_receive - first_receive)` and
rejects a cluster whose interval is zero or whose receive/send ratio is
implausible. Our ACK envelope carries `arrival_time_ms_lo16: u16` —
**milliseconds** — so a 15 ms probe cluster whose packets land within 1-2 ms
has 50-100% error in its receive rate, and one that lands inside a single
millisecond has an interval of exactly zero.

The consequence is not subtle: the bandwidth estimate never adopts what probes
find, and takes 5-7 s to discover tripled capacity. See
`docs/specs/bwe-googcc-review.md` for how this was isolated, including the
three explanations that were tried and refuted first.

**Both sides of the ratio must be precise.** The server also quantises its own
send timestamps — `Timestamp::from_millis(server_emit_us / 1_000)` — while
holding microseconds. Fixing only that was measured: rejections moved between
categories (invalid-interval 66 -> 40, ratio-too-high 32 -> 46) with net
successes *falling* 23 -> 15. Either both sides are microsecond-precise or
neither is worth changing.

## Goals

Carry client arrival times at microsecond resolution, without materially
growing the ACK datagram, without losing any entry, and without introducing a
representable-range failure mode.

**Non-goals.** Changing what is ACKed, when batches flush, the overlap
mechanism's size or purpose, or anything about tile datagrams. This is a
timestamp-resolution change.

## Wire format (`ACK_BATCH_MSG_TYPE = 0x06`)

```text
[0]      message_type = 0x06
[1]      count_fresh: u8      (0..=64, MAX_FRESH_ENTRIES_PER_BATCH)
[2]      count_overlap: u8    (0..=8,  ACK_OVERLAP_COUNT)
[3..7]   base_arrival_us: u32 little-endian

  count_fresh × 9 bytes:
     [0..4]  frame_seq: u32 little-endian
     [4]     tile_x: u8
     [5]     tile_y: u8
     [6]     pass_idx: u8
     [7..9]  arrival_delta_us: u16 little-endian   (arrival - base_arrival_us)

  count_overlap × 11 bytes:
     [0..4]  frame_seq: u32 little-endian
     [4]     tile_x: u8
     [5]     tile_y: u8
     [6]     pass_idx: u8
     [7..11] arrival_us: u32 little-endian          (absolute, low 32 bits)
```

Worst case (64 fresh + 8 overlap): **671 bytes**, against 650 today. Four bytes
of header and three bytes per overlap entry buy a thousandfold resolution
increase.

## Why two sections

The two groups have genuinely different temporal characteristics, and the
encoding should say so.

**Fresh entries are dense by construction.** `FLUSH_INTERVAL_US` is 5,000, so a
batch's fresh entries span at most 5 ms — comfortably inside a `u16` of
microseconds (65.535 ms), with an order of magnitude to spare. They arrive in
ascending order already, so `base_arrival_us` is simply the first one's arrival
and every delta is non-negative without sorting anything.

**Overlap entries are arbitrarily old.** They come from `recent`, which retains
up to `ACK_OVERLAP_COUNT * 4` entries across several previous batches. On a
sparse link they can be seconds old. Giving them absolute timestamps means
**no representable-range case exists** — the edge case is eliminated rather
than handled.

### The alternative that was rejected

A single base with `u16` deltas throughout is 17 bytes smaller and requires a
policy for overlap entries too old to encode. The obvious policy — drop them —
is actively harmful. Overlap exists so a single lost ACK batch does not cause a
retransmit flood (`ack.rs`: if it collapses, "every retransmit floods
MAX_RETRANSMITS without ACK"). Overlap exceeds 65 ms of age precisely when
traffic is sparse, which is precisely when the previous batch going missing
matters most and when wasted retransmits are most visible. The policy would
discard the resilience mechanism exactly in the scenario it exists for.

That approach also required sorting the whole batch into one ascending series,
changing today's `[fresh, overlap]` ordering. The two-section shape preserves
it.

## Invariants

1. `count_fresh <= MAX_FRESH_ENTRIES_PER_BATCH`, `count_overlap <=
   ACK_OVERLAP_COUNT`; the decoder rejects anything larger, as today.
2. Fresh entries are in ascending arrival order, and
   `arrival - base_arrival_us` fits a `u16` for every one of them. This follows
   from the flush interval and is asserted by the encoder rather than assumed.
3. Overlap entries carry absolute low-32-bit microseconds and are subject to no
   ordering or range constraint.
4. Deltas are computed in `u64` before truncation, so a batch straddling a
   `u32` wrap is still encoded correctly.

## Components

**Producer — `ghostframe-client-core`.** `reassembly.rs` stops discarding
precision: `arrival_time_ms_lo16: ((now_us / 1000) & 0xFFFF)` becomes an
`arrival_us` carried at full resolution on `AckEntry`. `ack_batcher.rs` keeps
its `[fresh, overlap]` assembly and hands both groups to the encoder
separately. Since the wasm cutover, client-core is the *only* ACK producer —
there is no TypeScript encoder to keep in step.

**Codec — `ghostframe-protocol/src/ack.rs`.** `AckBatch` gains the two-section
layout. `AckEntry::arrival_time_ms_lo16: u16` becomes `arrival_us: **u32**`;
the wire representation differs per section, and reconstruction (`base + delta`)
happens in the decoder so consumers see one uniform field.

> **Amended 2026-09-14, after implementation review.** This section originally
> specified `arrival_us: u64` holding a value "in low-32-bit wrapped space".
> That invariant existed only as prose, and encode narrowed it with three
> unchecked `as u32` casts: a caller passing a real wall-clock microsecond
> value got back a self-consistent but entirely different batch, with working
> deltas, plausible BWE output, and no error. `u32` makes the truncation
> unrepresentable instead of documented. The review also found the round-trip
> property test's one narrowed strategy range was precisely the range that hid
> it — a bound on a property-test strategy is a claim about what cannot
> happen, and needs justifying like any assertion.

`AckBatch`'s fields are private behind `AckBatch::new(fresh, overlap) ->
Result<Self, AckEncodeError>`, which validates every invariant up front, with
`fresh()`/`overlap()`/`entries()` accessors. `decode` validates too, so the
guarantee is total over both constructors: **if an `AckBatch` exists, it
encodes.**

**Consumers — `ghostframe-lib`.** `io_bridge.rs` reads `arrival_us` instead of
`arrival_time_ms_lo16`; `BweSample::client_arrival_ms_lo16` and `owd_ms_lo16`
become microsecond-based (`owd_ms_lo16` is currently `#[allow(dead_code)]`,
staged for the Phase 2 controller and read nowhere, so this costs nothing).
`bwe/timeline.rs`'s `Lo16Timeline` becomes a 32-bit unwrapper over microseconds,
wrapping every 71.6 minutes instead of every 65.5 seconds.

The decoder is stateless, so it cannot unwrap: it returns values in 32-bit
wrapped space — `base + delta` for fresh entries, the absolute field for
overlap ones — and the server unwraps them into its own `u64` timeline. That
means **one unwrap for the fresh section** (the base, with deltas added after)
and **one per overlap entry**, since each is an independent absolute. At most
nine unwraps per batch against seventy-two today.

The overlap entries are also the reason the unwrapper must stay a sequence-space
unwrapper rather than becoming a simple widening: an overlap entry is genuinely
older than the base, so it can sit on the other side of a wrap boundary from
it.

**The matching server-side fix — `bwe/googcc.rs`.** `send_time:
Timestamp::from_millis(send_ms)` becomes `Timestamp::from_micros(
r.server_emit_us as i64)`. Without this the wire change does not deliver, as
measured.

## Testing

**Edge cases, each its own test:**

| case | expectation |
|---|---|
| single fresh entry | `count_fresh=1`, `delta=0`, round-trips |
| all entries share a timestamp | every delta zero |
| fresh span exactly 65,535 µs | encodes; boundary holds |
| overlap entry 1 s old | round-trips exactly, nothing dropped |
| overlap entry 10 minutes old | round-trips exactly (absolute field) |
| `base` near `u32::MAX`, batch straddles the wrap | deltas computed in `u64`, decode exact |
| `count_fresh`/`count_overlap` over their caps | decoder rejects, as 0x04 does |
| truncated buffer mid-entry | decoder rejects rather than reading past the end |

**Property test:** arbitrary `(fresh, overlap)` entry sets with arbitrary
timestamps round-trip to exactly what went in, or the encoder refuses. No
silent mutation.

**Invariant test:** a batch whose fresh entries span more than the flush
interval is an encoder bug, not a wire case — assert the encoder catches it
rather than emitting a wrong delta.

**The measurement that justifies the change.** Re-run the step-up scene with
`GF_TRACE_GOOGCC=1 RUST_LOG=goog_cc=debug` and compare the three-way probe
outcome tally against the 23/66/32 baseline. **If invalid-interval and
ratio-too-high rejections do not fall materially, the change has not worked**
and should be reported as such rather than kept for tidiness.

## Rollout

Lockstep wire break, following `0x02 -> 0x03 -> 0x04 -> 0x06`. The message-type bump
means a mismatched pair fails loud with `AckDecodeError::WrongMsgType`
carrying whichever byte was actually seen — a new server rejects an old
client's `0x04`, an old server rejects a new client's `0x06` — rather than
either mis-parsing a two-section batch as a flat one. Nothing is deployed that
cannot be upgraded together.

Coherence checklist — every site that touches the field:

- `ghostframe-protocol/src/ack.rs` (codec, doc comment, msg-type constant)
- `ghostframe-client-core/src/reassembly.rs` (two producer sites)
- `ghostframe-client-core/src/ack_batcher.rs` (batch assembly)
- `ghostframe-client-core/tests/oracle_ack.rs` (oracle round-trip)
- `ghostframe-lib/src/transport/io_bridge.rs` (consumer, `BweSample`)
- `ghostframe-lib/src/transport/bwe/timeline.rs` (unwrapper)
- `ghostframe-lib/src/transport/bwe/googcc.rs` (send-side precision)

> **Amended 2026-09-14: the message type is `0x06`, not `0x05`.** This
> document originally said `0x05`, reasoning only from the ACK codec's own
> lineage `0x02 -> 0x03 -> 0x04`. It never checked the wider discriminator
> namespace, and `0x05` was already `TILE_NACK_ENVELOPE`. Both are
> client->server datagrams, and the server dispatches `TileNack` first and
> everything else to the ACK handler — so every ACK batch would have routed
> into the NACK handler, failed to decode, and been **silently dropped**,
> disabling the entire feedback path this change exists to improve. No crash,
> no error; probe rejections would simply not have improved and we would have
> concluded the change did not work.
>
> Found by the Task 2 implementer investigating four unrelated test failures
> rather than working around them. The namespace turned out to be already
> fragile: the old `0x04` collides with `TILE_PARITY_ENVELOPE`, surviving only
> because parity falls through the dispatch's `_` arm into the ACK handler,
> and `classify_inbound` still hardcoded `0x03` for ACK batches — stale since
> the codec moved to `0x04`.
>
> The renumber fixes today's bug. The durable fix is
> `inbound_message_discriminators_do_not_collide`, a pairwise test over every
> client->server discriminator, because a collision here never fails loudly —
> it silently routes one message type into another's handler.

## Risks

| risk | mitigation |
|---|---|
| Fresh span exceeds 5 ms for an unforeseen reason, overflowing a delta | Encoder asserts rather than truncating; invariant test covers it |
| Two entry shapes make the parser easier to get wrong | Property test over both sections; decoder validates both counts and the buffer length |
| The change lands and probe rejections do not fall | Stated as the acceptance measurement above; report the null result rather than keeping the change |
| `u32` microsecond base wraps mid-session | 71.6-minute period, unwrapped in sequence space exactly as the 16-bit version was; wrap-straddling is an explicit test case |

## Out of scope

The source-rate gap (`target_rate` never limits emission) and the ramp work it
blocks — see `docs/specs/bwe-googcc-review.md`. This change is a prerequisite
for that, not a substitute: probe results have to be *accepted* before making
them more frequent or acting on them can matter.
