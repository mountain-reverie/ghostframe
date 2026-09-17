# Transport-Identity ACKs: acknowledging transmissions, not content

**Date:** 2026-09-17
**Status:** Approved design

## Problem

goog_cc's loss-based estimator has never engaged. `PacketResult::is_received()`
is `!receive_time.is_plus_infinity()` and `lost_packets()` filters on exactly
that, so a feedback report built solely from ACK arrivals contains zero losses
by construction. Measured on a link stepping 16 -> 4 Mbps while traffic flows:
in 5 runs of 6 the estimate stayed pinned at its pre-drop 16.36 Mbps under
~750 kB of loss — a sustained 4x overestimate of a degraded link, which is the
dangerous direction.

Reporting losses from client NACKs and RTO firings (landed on
`investigate/degradation-detection`) improved the signal but not the outcome:
goog_cc sees a loss fraction of 0.4-3% where the link sheds ~7% by bytes. The
ratio is computed from *complete* acknowledgements and *partial* losses, so it
is systematically too low to trigger backoff.

The under-reporting has one cause: **we conflate two different questions.**

1. *Did this content arrive?* An application question, keyed by `EmitKey`
   = `(frame_seq, tile_x, tile_y, pass_idx)`. Cancelling it on supersession is
   correct — the content genuinely stopped mattering.
2. *Did this packet arrive?* A transport question. The bytes went on the wire
   and either arrived or did not, regardless of whether anyone still wants
   them.

The congestion controller asks question 2 and we only answer question 1, so
supersession erases the answer. On a churning screen supersession is the common
case, which is why most losses are invisible.

## Goals

- Every emitted datagram is eventually classified received or lost, exactly
  once, independent of whether its content was superseded.
- The loss *ratio* goog_cc computes is the real one.
- Retransmission stops being ambiguous.

## Non-goals

- Changing FEC's recovery behaviour (its interaction is addressed, not
  redesigned).
- Reporting loss for datagrams the server chose never to send.

## Design

### `wire_seq` becomes a transmission identifier

`wire_seq` already exists at `bytes[8..12]`, is allocated per datagram, and is
already parsed by the client for the parity decoder. It is *nearly* what we
need, with one defect: **retransmits reuse it.** Neither `tick` (RTO) nor
`on_nack` calls `alloc.allocate()`; both re-queue the cached bytes with the
original `wire_seq` intact.

Retransmissions therefore get a fresh `wire_seq`. That is the change that makes
the field mean "this transmission" rather than "this fragment of this pass",
and it is what removes the ambiguity that forced the Karn revert (a filter that
discarded 99% of timing samples on a congested link) and the
`entry.probe = None` workaround for probe-cluster contamination.

### ACK entries carry `wire_seq`

The entry becomes `wire_seq: u32` in place of
`(frame_seq: u32, tile_x: u8, tile_y: u8, pass_idx: u8)` — 4 bytes rather than
7, plus the existing arrival timestamp.

The server maps back. It allocated both identifiers, so a
`wire_seq -> EmitKey` map recovers the content identity every existing consumer
(`FragmentCoverageMap`, Cdf53 ACK counting, PalRle palette tracking, retransmit
cancellation) already expects. Those consumers do not change.

Entry count may rise, since a tile-pass spanning two fragments now produces two
entries where it produced one. Against 3 bytes saved per entry this is roughly
neutral, and the client's existing batching is unchanged.

### The transmission ledger

A structure separate from the retransmit cache, because the retransmit cache is
about content and is legitimately emptied by supersession.

On emit, record `wire_seq -> (emit_us, wire_bytes, EmitKey)`. On ACK, resolve
and remove, reporting a received `PacketResult`. Anything still present after
the loss horizon is reported lost and removed.

`cancel_for_tile` does not touch the ledger. That is the entire point: the
packet's fate is still owed to the estimator even once its content is garbage.

**Bounds.** Entries are evicted by the horizon, so the ledger is sized by
send rate x horizon rather than by session length — roughly 2000 packets/s at
20 Mbps, about 40 kB at a 2 s horizon. A hard cap backstops a pathological peer,
evicting oldest first and counting the eviction, since a stale entry describes a
send time the estimator has long since moved past.

### The loss horizon

An unacknowledged `wire_seq` is declared lost after
`clamp(LOSS_HORIZON_RTTS x smoothed_rtt, LOSS_HORIZON_FLOOR, LOSS_HORIZON_CEIL)`.

Too short and reordering reads as loss; too long and backoff lags the
degradation the horizon exists to catch.

**Adaptive**, because the spread is too wide for one number: measured RTT on
this system ranges from ~20 ms on a clean link to 147 ms through the
bufferbloated wifi bottleneck. A constant sized for the slow case delays
reporting several-fold on the fast one; sized for the fast case, late
acknowledgements on a slow path read as loss.

**Floored**, because the client's 5 ms ACK batching is RTT-independent and does
not shrink on a fast link. The floor covers that quantum plus a batch interval.

**Capped**, because the adaptation has a perverse direction: under bufferbloat
RTT inflates *because of* the congestion being detected, which would stretch
the horizon at exactly the moment loss should be reported fastest. The 20 ms ->
147 ms figure above is that effect, measured. The ceiling must still clear a
legitimately high-RTT path, so it bounds the pathology without truncating
honest latency.

There is precedent for distrusting an RTT-derived value here:
`GoogCcDriver::note_path_rtt` already cross-checks the estimator's derived RTT
against quinn's measured path RTT and counts `implausible_rtt_samples`.

Initial values are a starting point to be measured against the degradation
scene, not a tuned result.

### Reporting to goog_cc

`packet_feedbacks` gains the lost entries with `receive_time` of
`Timestamp::plus_infinity()`, sorted with the received ones by send time.

**A report must never contain only losses.** `on_transport_packets_feedback`
unwraps the max receive time over a report's *received* packets and panics
otherwise — found the hard way. Losses continue to wait for an acknowledgement
to accompany them.

### FEC interaction

The parity decoder keys its source window on `wire_seq` and forms groups from
consecutive allocations at submit time. Retransmissions do not pass through
`submit_one`, so a retransmitted datagram's fresh `wire_seq` joins no parity
group, and the original `wire_seq` stays missing in the client's window.

That is correct rather than merely tolerable: the original transmission really
did not arrive, and the group's recovery arithmetic should continue to say so.
The retransmission delivers the content directly. What is lost is the ability
for a retransmission to fill the original slot for *parity* purposes, which was
never something the decoder relied on.

## Error handling

- A `wire_seq` acknowledged twice (overlap entries, duplicate delivery)
  resolves once; the second finds no ledger entry and is ignored, not counted.
- A `wire_seq` the server never emitted is ignored and counted — it means a
  confused or hostile peer, and silence would hide that.
- A `wire_seq` acknowledged after its horizon expired has already been reported
  lost. It is ignored rather than retracted: goog_cc has no retraction, and a
  late arrival is genuinely a late arrival.

## Testing

- **Unit:** ledger classifies exactly once; supersession leaves the ledger
  untouched; horizon expiry reports loss; late ACK after expiry does not
  double-report.
- **Wire:** ACK round-trip including the overlap entries; a rejected batch from
  a stale peer fails loud on the message-type byte rather than mis-parsing.
- **Property (required):** for any interleaving of emits, acknowledgements,
  supersessions and horizon expiries, every emitted `wire_seq` is reported
  exactly once — never twice, never not at all. This is the invariant the whole
  design rests on and the one most likely to break under an ordering nobody
  thought of.
- **Acceptance (the point of the exercise):** on the 16 -> 4 Mbps degradation
  scene, the estimate settles to within 1.5x of the new capacity, in every run
  rather than 1 in 6. The loss fraction goog_cc observes should approach the
  link's real shed rate; that number is the direct evidence the accounting is
  complete.

## Wire compatibility

A break, which is acceptable pre-release. The message-type byte bumps so a
stale binary fails loud with `WrongMsgType` rather than mis-parsing, following
the precedent set at 0x03 -> 0x04.
