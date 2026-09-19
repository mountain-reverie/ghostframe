# The retransmission storm: a late acknowledgement has nowhere to land

**Date:** 2026-09-18
**Reproduction:** `a_lossless_link_with_a_real_rtt_does_not_retransmit`
(browserless, 70 ms RTT, `NetProfile::perfect()`, 6x6 busy grid, 8 s)
**Symptom in the field:** frames never fully stabilise; per-tile stale content
persists across many frames.

On a link that drops nothing, the server retransmitted **1788 times**.

---

## Root cause

An acknowledgement no longer names the content it acknowledges. Since
"acknowledge transmissions, not content" (PR #90), an `AckEntry` carries a
`wire_seq`, and the server must translate it back to an `EmitKey` through
`TransmissionLedger` before `ReliableTileEmitter::on_ack` can clear the
retransmit cache entry.

That translation expires. `TransmissionLedger::expire` deletes a record once
it is older than `loss_horizon()` — `3 x RTT`, clamped to `[60 ms, 600 ms]` —
so it can be reported to goog_cc as a loss.

**Nothing re-establishes the translation, and nothing else bounds the cache
entry.** `resolve` returning `None` increments `stats.unknown_acks` and is
otherwise silent; no caller reads it. The emitter cache has no eviction, no
attempt cap, and no lifetime: `cache.rs` documents that entries stay "until
ACKed, cancelled via `cancel_for_tile`, or the session ends", and a test pins
that it grows past capacity without evicting.

So when an acknowledgement loses a race against the horizon, the entry it
would have cleared becomes **immortal** and retransmits at the RTO backoff
ceiling (one every 5 s) for the rest of the session — carrying content the
receiver acknowledged long ago.

## The measurement

`GHOSTFRAME_RTO_PROBE=1` (this branch) emits one line per RTO fire, per
resolved acknowledgement, per resolve miss, and per expiry.

| | |
|---|---|
| retransmissions, lossless link | 1788 |
| transmissions expired by the ledger | 1458 |
| of those, **acknowledged after expiry** | **1458 (100%)** |
| loss horizon in force | 236 ms |
| Cdf53 emit -> ACK latency | min 85 ms, p50 145 ms, **p90 251 ms, max 256 ms** |

The horizon sits at 236 ms; the acknowledgement distribution's upper decile
sits above it. Every transmission in that tail is declared lost, and none of
them were.

**Every one of the 1016 RTO fires was codec 5 (Cdf53). Zero for Solid, PalRle
or H264.** Those three acknowledge on receipt and always beat the horizon.
Cdf53 defers its acknowledgement until the whole pass assembles and
prevalidates, which is what pushes its tail past 236 ms.

## What this is not

Each ruled out by experiment on the reproduction, not by argument.

- **Not the RTO being too short.** Raising the base RTO to **2 seconds** left
  871 fires, every entry still alive at exactly the timer value. An immortal
  entry fires whatever the timer says.
- **Not the 50 ms ceiling in `rto_for_attempt`, nor the 20 ms
  `smoothed_rtt` default.** Removing the ceiling and setting the default to
  the true 70 ms RTT: 1788 -> 1783.
  (An earlier investigation "ruled out" `smoothed_rtt` by raising it to
  100 ms and seeing no change. That experiment could not have shown
  anything: the ceiling clamps any RTT above 25 ms to 50 ms, so it was a
  10 ms change against a 45 ms shortfall. The conclusion was right for the
  wrong reason.)
- **Not prevalidation rejecting superseded passes.** The client prevalidated
  **3805 Cdf53 passes and rejected none**. It acknowledges everything it
  receives.
- **Not a missing `cancel_for_tile`.** Both `bump_generation` sites pair with
  it correctly.
- **Not sufficient to acknowledge Cdf53 on receipt.** Tried: 1788 -> 1739,
  expired 1458 -> 1426. It lowers the latency floor (85 ms -> 75 ms) but not
  the tail, so the race is still lost.

## The second consequence

Those 1458 transmissions are also handed to goog_cc as losses. On a link with
no loss, the bandwidth estimator is told 1458 packets were dropped. Whatever
the retransmit fix, the false loss reports want retracting when the late
acknowledgement arrives.

## Why it appeared when it did

The translation step is new in PR #90. Before it, `on_ack` matched a cache
entry by content identity, and a late acknowledgement still cleared its entry
— there was no intermediate mapping to expire. The live session that first
showed persistent stale tiles is the first one running that code.

## Remediation, ranked

1. **Keep the translation after expiry.** Expiry exists to report a loss, not
   to forget which pass a `wire_seq` belonged to. Retain a bounded
   `wire_seq -> EmitKey` tombstone so a late acknowledgement still clears the
   cache entry (and retracts the false loss). Fixes the immortality directly
   and is insensitive to how the horizon is tuned.
2. **Bound the emitter cache entry.** A cap on attempts or lifetime, so an
   entry that becomes unresolvable for any reason dies instead of
   retransmitting until the session ends. Defence in depth: the leak above is
   one way to strand an entry, and it should not be the last line.
3. **Size the horizon on what it is actually racing.** `3 x RTT` measures the
   network; the deadline it must beat includes client-side batching and
   assembly. Derive it from observed acknowledgement latency instead.
4. **Acknowledge Cdf53 on receipt.** Measured insufficient on its own, but it
   removes a layering violation: the deferral dates from July 2, when
   `AckEntry` named content and a single fragment could not be named. PR #90
   removed that constraint; the deferral outlived its reason.
