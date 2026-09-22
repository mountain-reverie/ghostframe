# Treating `Blocked` as flow control

**Status:** design, not implemented.
**Gate:** `ghostframe-e2e/tests/browserless_blocked_path.rs`,
`work_rejected_by_a_full_send_buffer_still_reaches_the_client` (currently failing).

## The problem, measured

`datagram_send_buffer_size` is 16 MiB. On the ~1 MB/s a real session gets,
that is a queue deep enough to hold seconds of datagrams. Measured at
production scale (60x34 = 2040 tiles), 64 KiB vs 16 MiB send buffer,
everything else identical:

| | 16 MiB | 64 KiB |
|---|---|---|
| bytes delivered | 3,591,308 | 2,353,116 |
| send rejections | 0 | 4,726 |
| **stale generation tiles** | **7,316** | **1,170** |

Both columns are the point. Shrinking the buffer removes the bufferbloat —
datagrams stop waiting long enough for their tile to be superseded, and
staleness falls 6.3x. It also costs a third of delivery, because a rejected
datagram is discarded.

Separately measured, and the reason this is a redesign rather than a tweak:

- `send_datagram_errs_total = 0` on every run with the production buffer, so
  `Blocked` never happens and `Event::DatagramsUnblocked` — which quinn emits
  *only* after a rejection — has never fired in this codebase's life.
- `continuation_resumes = 0`: `resume_scheduler_continuation` is therefore
  dead code, and the leftover tick budget it exists to spend is discarded.
- Peak 6.71 MB buffered inside quinn, i.e. ~6.7 s of queueing, against
  production's reported `queued_critical_latency_max_us = 16,844,729`.

## Root cause

Two mechanisms working against each other:

1. The scheduler clamps every drain to
   `QUINN_SEND_BUFFER_SAFETY_FRACTION * send_buffer_space()` specifically so
   that `send_datagram` never returns `Blocked`.
2. quinn emits `DatagramsUnblocked` only *after* a `Blocked`.

So the avoidance in (1) disables the signal in (2). quinn's own high-level
API assumes the opposite: `Connection::send_datagram_wait` attempts the send,
takes `Blocked`, and awaits a `Notify` fed by that event. `Blocked` is
designed to be normal flow control.

It cannot be normal here, because rejection loses work. The funnel every
datagram passes through cannot report failure:

```rust
pub trait DatagramSender {
    fn send(&mut self, dg: &[u8]);        // no return value
}
```

and the implementation swallows it:

```rust
if let Err(e) = wt.send_datagram(conn, dg) {
    self.datagram_send_errs = self.datagram_send_errs.saturating_add(1);
    // datagram dropped
}
```

By then `drain_refinement_pass_major` has already popped the work with
`queue.remove(h)` — unlike `drain_priority_queue`, which retains the entry,
marks it `InFlight`, and re-offers it after `retry_after`. Refinement's only
path back is the emitter's RTO, bounded by `MAX_RETRANSMITS`. That covers 92
rejections (8x8 scene, ratio 0.899) and collapses under 4,726 (2040 tiles,
ratio 0.66).

## Why the emitter is the right layer

A queued `Emission` is a byte blob that has not left yet. Everything with a
side effect already happened in `submit_one`, *before* the send:

```rust
let wire_seq = self.alloc.allocate();          // sequence assigned
bytes[8..12].copy_from_slice(&wire_seq...);    // baked into the datagram
self.cache.insert(key, entry);                 // retransmit cache populated
self.feed_group(wire_seq, &bytes);             // FEC group fed
```

So re-queueing a rejected emission has nothing to roll back: no sequence to
un-allocate, no cache entry to revoke, no FEC group to unwind, and no
`TransmissionLedger` gap — the transmission simply happens later under the
sequence it was already given.

`EmissionQueue` is a `VecDeque<Emission>` in wire order, and `pop` uses
`next_wire_seq` only to decide parity promotion. Pushing the rejected item
back to the **front** restores the exact prior order. The "must not jump
ahead of its generation's earlier passes" question dissolves at this layer:
we put the item back where it came from.

Doing it in the scheduler instead would be worse on both counts. Retaining
refinement work the way `drain_priority_queue` does means the pass-major
bucket walk revisits thousands of ineligible `InFlight` entries per tick
against a 16k queue — reintroducing the O(n) scan `slots.rs` was written to
remove — and it raises the pass-ordering question that the emitter layer does
not have.

## The change, in three steps

Each step is separately testable, and step C is the one that pays.

### A. Make rejection lossless

```rust
pub enum SendOutcome { Sent, Rejected }

pub trait DatagramSender {
    fn send(&mut self, dg: &[u8]) -> SendOutcome;
}
```

`ReliableTileEmitter::drain` stops on the first rejection and puts the
emission back:

```rust
while let Some(emission) = self.queue.pop(next, now) {
    match sender.send(emission.bytes()) {
        SendOutcome::Sent => { /* existing parity accounting */ }
        SendOutcome::Rejected => {
            self.queue.push_front(emission);   // nothing to roll back
            self.stats.send_rejected += 1;
            return;                            // buffer is full; stop
        }
    }
}
```

Returning rather than continuing matters: `Blocked` means "no room", so the
next emission would fail too. Requires a new `EmissionQueue::push_front`.

`send_to_all_sessions` returns `Rejected` if any session rejected. With more
than one session that re-sends to sessions that accepted; the duplicate is
wasted bandwidth rather than corruption, since the client dedups on
`wire_seq` and tile reassembly is idempotent per `frag_idx`. Worth stating
because production has one session and the cost is invisible there.

**Status: implemented.** Measured, and the credit does not go where this
design predicted. Two things changed together -- the drain now *stops* at the
first rejection, and the rejected emission is re-queued -- and the back-off is
what moves the numbers:

| | hammering (before) | stops (drop) | stops + re-queues |
|---|---|---|---|
| send rejections | 4,726 | 337 | 319 |
| retransmits | 3,424 | 746 | 691 |
| stale tiles | 1,170 | 338 | 353 |

Previously `drain` walked the whole queue offering every emission to a
transport that had already refused one, discarding each. Stopping cuts that
~14x. The re-queue is worth having -- it is the difference between losing one
datagram per drain and losing none -- but it is invisible at integration
level, because a single loss per drain is well within what RTO recovers in a
30 s scene. It is unit-tested instead
(`a_rejected_datagram_is_re_queued_not_dropped`,
`a_re_queued_datagram_keeps_its_place_and_its_wire_seq`, both
mutation-verified).

The two halves are a required pair, not independent: `push_front` without the
`return` re-pops the same emission forever. Removing the `return` hung the
gate rather than failing it.

Also measured, and the reason step C now looks safe: at 64 KiB the screen
renders in full (2040 of 2040 tiles, same as 16 MiB) using **26% fewer bytes**
(2.66 MB vs 3.60 MB, because retransmits drop 12x from 7,741 to 660) with
**21x less staleness** (348 vs 7,375). The small buffer is now strictly better
on every axis measured.

A note on metrics: the gate first compared delivered *bytes* and read as a
34% regression. That metric rewards retransmit waste -- the roomy run
"delivers" more because it retransmits 12x more. Tiles rendered is the
user-visible outcome, and by that measure the two are identical.

### B. Backpressure the scheduler on the emitter queue — NOT NEEDED

**Status: retired by measurement, not implemented.**

The concern was that step A moves the bloat rather than removing it: if the
transport keeps refusing while the scheduler keeps submitting, the emitter's
queue grows without bound. Measured instead of assumed, at production scale
(~20,400 passes enqueued):

| | send rejections | emission-queue peak |
|---|---|---|
| 16 MiB buffer | 0 | 3,904 |
| 64 KiB buffer | 323 | **1,453-1,574** |

The backlog does not migrate. The 64 KiB run's emitter queue is *shallower*
than the 16 MiB run's, and both sit far below the enqueued work, so most of
it waits in the scheduler's refinement queue -- which is exactly where it
should, because that queue can supersede a stale generation.

The reason is the clamp that already exists. Every drain is limited to
`QUINN_SEND_BUFFER_SAFETY_FRACTION * send_buffer_space()`, so a small buffer
makes the scheduler pop *less*, not more. The backpressure step B would have
added is already there, and it works better the smaller the buffer is.

Pinned by an assertion in the gate test so a future change cannot regress
into emitter-side bloat unnoticed.

### C. Shrink the send buffer

Only now is `datagram_send_buffer_size` safe to reduce toward a BDP-sized
value (~24 KB at 8 Mbit with 24 ms RTT; an order of magnitude above that is
still far below 16 MiB). `GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES` already
exists for bisecting this.

**Acceptance:** peak buffered drops from 6.71 MB to the configured size,
queued-to-ACK latency falls with it, and delivery is unchanged.

## What this does not claim

- It does not explain the originally reported stale tiles on a quiet screen.
  That symptom is still unreproduced; every scene built for it either
  converged or failed for a different, identified reason.
- Step C's latency win is measured only as buffered bytes and staleness. The
  browser e2e's settle-time measurement (~22 ms on a quiet mixed screen) is
  the end-to-end number to re-check after.

## Rejected alternatives

- **Timer-driven continuation resume.** Polls for something knowable exactly,
  and the tick budget is a fixed per-call quantum
  (`bytes_per_us * SCHEDULER_TICK_INTERVAL_US`), so a faster timer multiplies
  the send rate rather than decoupling it.
- **Resume after `drain_outbound`.** Implemented and measured:
  `continuation_resumes = 0`. No continuation is ever created, because
  `scheduler_continuation_after_drain` returns `Some` only when the drain
  stops with budget left, and these drains spend their whole allocation.
- **Elapsed-time tick budget.** Implemented and measured: no change.
  `SCHEDULER_TICK_BUDGET_FLOOR_BYTES` (256 KB) dominates every realistic
  interval (33 ms computes ~30 KB, 250 ms ~225 KB), so the elapsed term never
  decides the budget.
- **Shrinking the buffer first.** Measured: 15,944 rejections, delivery to
  11%, client coverage `complete=0`. This is step C without steps A and B.
