# Scheduler-owned tile repair

**Date:** 2026-09-18
**Status:** design approved, not implemented
**Supersedes the mechanism analysed in:** `docs/specs/retransmit-storm-root-cause.md`,
`docs/specs/is-rto-still-needed.md`

## Goal

Make the scheduler the single owner of tile delivery state and repair, so
that repairing a tile is impossible once its content is superseded, and no
repair deadline is sized on a clock that does not measure what it is racing.

## Why

Five overlapping repair mechanisms run today: the emitter's RTO timer
(40 ms, content-blind), the scheduler's 2xRTT `InFlight` retry, IoBridge's
Phase 1.5-B stranded-tile escalation, receiver coverage NACK plus tail sweep
(Cdf53 only), and FEC parity. Measured consequences:

- On a **lossless** 70 ms link the emitter's timer produced **1788
  retransmissions carrying nothing new — 44% of all bytes sent**
  (386,844 with the timer, 215,751 without).
- Under 10% loss it fired 1280 times against **89 NACK hits with zero NACK
  misses**: the receiver-driven path asked for 89 repairs and got all 89.
- Disabling the timer leaves the browserless suite at **17/17**, and at 40%
  loss it is 5/5 without the timer versus 4/5 with it — spurious repairs
  compete for a capped link.

Four structures hold overlapping identity for a single emission:

| structure | key | carries |
|---|---|---|
| `transmission_ledger` | `wire_seq` | `EmitKey(frame_seq, tile, pass)` |
| `fragment_coverage` | `(frame_seq, tile, pass)` | generation, codec, palette_id, palette_bundled |
| emitter `cache` | `EmitKey` | payload bytes |
| scheduler queues | `(tile, gen, pass)` | the same payload bytes |

That split is not incidental. The PalRle `in_flight_carrying` underflow fixed
in PR #92 existed because `palette_bundled` had to be re-derived from payload
bytes — the fact lived in a different structure from the work item it
described. `fragment_coverage`'s own header already records its intended
retirement and lists the steps; this design subsumes them.

## Architecture

| unit | responsibility | holds |
|---|---|---|
| `Scheduler` | what to send, what needs repair, when to give up | `TileWork` (payload + state) until terminal; per-tile ACK bitmap |
| `ReliableTileEmitter` | frame, stamp `wire_seq`, FEC group, pace | nothing across calls |
| `TransmissionLedger` | `wire_seq -> Handle`; BWE loss signal | live records plus tombstones |
| `AckLatencyTracker` (new) | defines what "late" means | rolling emit->ACK distribution; p95/p99 |

**Deleted:** the emitter retransmit cache, the RTO timer wheel,
`rto_for_attempt` and its 50 ms ceiling, `cancel_for_tile` (supersession is
already a scheduler state transition), IoBridge's Phase 1.5-B escalation, and
`fragment_coverage`.

**The unifying rule: no repair deadline is sized on RTT.** Every deadline that
races an acknowledgement is sized on the measured acknowledgement
distribution. One rule fixes the ledger horizon (236 ms against an ACK p90 of
251 ms) and the scheduler's `2 x rtt` (140 ms against an ACK p50 of 145 ms),
which are the same defect in two places.

Repair becomes generation-aware for free: a superseded tile transitions to
`Superseded` and leaves the queue, so repairing stale content is structurally
impossible.

## Index structure

Required, not an optimisation. Measured today on the 6x6 scene: `mark_acked`
is called 2016 times and scans **0** items, because
`drain_refinement_pass_major` removes entries at emit. Holding work until ACK
deletes exactly that property. Projected to 1080p (2040 tiles, ~57x): ~29K
queued entries against ~115K `mark_acked` calls, or ~3.3e9 comparisons.

Two further scans hide behind the same change: `bump_generation` does a full
`cdf53_passes_acked` scan per bump to drop one tile's rows, and
`drain_refinement_pass_major` recomputes `queue.iter().map(pass_idx).min()`
per pass level.

Every key is a small bounded integer — tiles <= ~2040, passes = 14,
generation = 4 bits — and at most one generation per tile is live, since
`bump_generation` supersedes the rest.

```
slots: Vec<TileSlot>              // indexed tile_y * cols + tile_x
  TileSlot { current_gen: u8, acked_mask: u16, passes: [Option<Handle>; 14] }

slab:  Slab<TileWork>             // owns payloads; versioned handles
order: VecDeque<Handle>           // priority work
       [VecDeque<Handle>; 14]     // refinement, bucketed by pass
in_flight: VecDeque<Handle>       // send order, for the safety sweep
```

| operation | before | after |
|---|---|---|
| `mark_acked` | O(queue) | O(1) |
| supersede tile | O(queue) + O(ack map) | O(14) |
| `refinement_queue_holds_tile` | O(queue) | O(1) |
| pass-major drain | O(queue) per level | O(drained), no `min()` |
| per-bump ACK-map scan | O(map) | deleted (reset `acked_mask`) |

`slots` costs ~120 KB at 1080p. `cdf53_passes_acked` is deleted: the bitmap
lives in the slot, and an ACK for a non-current generation is rejected by the
slot's `current_gen`.

## Data flow

**Emit.** `tick_at` pops handles from the ordering buckets within budget. The
emitter frames the payload, allocates a fresh `wire_seq`, stamps and
FEC-groups it. The ledger records `wire_seq -> Handle`. Work stays in the
slab with `state = InFlight`, `last_sent_at = now`, and the emitted
`frame_seq` stored on the entry.

**Ack.** `wire_seq -> Handle -> slab entry`, which already carries tile,
generation, pass, codec, palette_id and palette_bundled — one lookup replaces
the ledger -> coverage -> scheduler chain. Sets `slot.acked_mask |= bit`,
frees the entry, samples emit->ACK latency into `AckLatencyTracker`.

**Nack.** `slot[tile].passes[pass] -> Handle`; validate the entry's
`frame_seq` against the NACK's and ignore stale ones; set `Pending` and push
to the front of its bucket. Re-emitted next tick with a fresh `wire_seq`.
O(1), with no dependency on a separate cache.

**Sender safety sweep.** `in_flight` is already in send order, so the sweep
pops from the front while `now - last_sent_at > ack_p95 + margin` (floor
500 ms) — O(fired), not O(queue). It re-sends **only if `slot.acked_mask == 0`
for the current generation**: nothing acked means the receiver may not know
the tile exists. If anything was acked, the receiver has coverage and will
NACK, so the sweep leaves it alone.

**Supersede.** `bump_generation` increments `slot.current_gen`, clears
`acked_mask`, frees the 14 handles. O(14).

**Loss reporting.** The deadline is `ack_p99 + margin`, not `3 x RTT`, so
false losses are rare by construction and no retraction is needed. Handle
tombstones are retained longer than the loss deadline so a late ACK still
resolves and still frees its entry. A counter records *acked after declared
lost*; on today's code that number is 1458 per 8 s scene and it must read
near zero.

## Hazards

**Versioned handles are mandatory.** The ledger keeps tombstones and therefore
outlives slab entries. A stale `wire_seq` resolving to a recycled slot would
acknowledge the wrong work — silent corruption, the same class this codebase
has already produced twice. Handles are `(index, slab_generation)` and
`resolve` rejects a version mismatch.

**Memory is capped.** At the slab cap, evict the oldest unacked entry and
count it. That abandons one repair, which is honest degradation; an unbounded
hold is worse. The worst case is the ~50K entries / ~25 MB the emitter cache
already documents for a 1080p first paint — the same bytes, relocated.

**Give-up escalates rather than abandons.** After N repair attempts the tile
is handed back as dirty and re-encoded fresh at current priority, instead of
replaying stale bytes.

## Testing

The violated invariant is *every emission reaches exactly one terminal state*.

| test | catches |
|---|---|
| storm reproduction as a gate: lossless link, repairs ~0 (today 1788) | the regression this started from |
| property: every emission ends `Acked`/`Superseded`/`Abandoned`, never immortal | the stranding class |
| bounded comparison count in `mark_acked` | the O(n^2) silently returning |
| stale `wire_seq` after slot recycling must not ack the new occupant | the ABA hazard |
| ACK after the loss deadline still frees its entry | the defect being fixed |
| partially-acked tile is not swept; `acked_mask == 0` tile is | the sweep's gating rule |
| browserless suite stays 17/17; lossless bytes drop ~44% | overall equivalence |

**Harness gap to close first.** The browserless harness bails at the loss
rates where the "receiver cannot know" case appears — confirmed at 60% and
90%, where the scene fails before running. Probabilistic loss cannot test it.
Deterministic drop injection is needed (drop *this* tile's first emission);
the e2e side has loss predicates, browserless does not.

## Open uncertainties

Carried deliberately into implementation rather than assumed away.

1. **The scheduler's existing 2xRTT retry has never been observed firing
   end-to-end** — only its unit test (`"InFlight work should retry after
   2xRTT"`). Every scene measured showed zero retries, consistent with
   nothing being lost, but not positive evidence. The design leans on that
   path being real; verify it before deleting the emitter's timer.
2. **ACK latency of p50 145 ms against an RTT-plus-batching floor of 75 ms is
   unexplained.** Every threshold here is sized on that distribution. If the
   tail is a harness artifact of the paused clock, the structure still holds
   but the constants are wrong. Measure it against a real link before
   freezing `ack_p95` / `ack_p99` margins.
