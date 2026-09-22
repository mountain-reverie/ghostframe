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

### C. Shrink the send buffer — BLOCKED, and not for the reason expected

**Status: measured and not done. The default stays 16 MiB.**

Shrinking is clearly right on a slow link. Swept at production scale over a
2 Mbit path (`probe_send_buffer_sizes`):

| size | tiles | stale | bytes | retransmits |
|---|---|---|---|---|
| 32 KiB | 2040 | **51** | 2.65 M | **345** |
| 64 KiB | 2040 | 388 | 2.67 M | 684 |
| 128 KiB | 2040 | 398 | 2.67 M | 742 |
| 256 KiB | 2040 | 1,831 | 4.72 M | 16,742 |
| 1 MiB | 2040 | 12,236 | 6.56 M | 29,646 |
| 16 MiB | 2040 | 7,355 | 3.58 M | 7,733 |

Every size renders the full screen -- step A made them all safe -- and 32 KiB
is best on every axis, with 144x less staleness than 16 MiB. Note also that
mid sizes are *worse* than either extreme: 256 KiB and 1 MiB thrash, because
the clamp scales drains with buffer space, so they pop enough work to fill
the buffer and then reject it.

Then the same sweep on a **saturated** 50 Mbit path
(`probe_saturating_fast_link`, offering far more work than the link carries):

| size | bytes delivered | stale | retransmits |
|---|---|---|---|
| 32 KiB | 4,031,406 | 0 | 0 |
| 64 KiB | 7,774,183 | 2,689 | 17,921 |
| 256 KiB | 4,189,475 | 62 | 827 |
| 1 MiB | 5,178,564 | 804 | 1,903 |
| 16 MiB | **24,330,181** | 8,140 | 23,072 |

16 MiB delivers **6x more** than 32 KiB. A single-frame fast-link probe had
missed this entirely -- every size delivered the same ~2.66 MB, because that
was the offered work rather than the link's capacity. A throughput ceiling is
invisible until offered load exceeds what the link can carry.

The cause is the clamp, not the buffer. Every drain is limited to
`QUINN_SEND_BUFFER_SAFETY_FRACTION * send_buffer_space()`, so throughput is
bounded by roughly `drains_per_second * 0.8 * buffer_size`. At 32 KiB and
~30 drains/s that is ~780 KB/s -- about 6 Mbit. **The send buffer is
currently the throughput governor**, which is why it cannot be shrunk on its
own.

This is the same clamp that made step B unnecessary. It is doing two jobs at
once: bounding how much the scheduler pops (useful) and setting the
transmission rate (not its business).

### C'. Decouple the drain budget from the buffer, then shrink

The prerequisite step C needs, and the redesign this whole line of work has
been converging on.

The drain budget should come from the bandwidth estimate, and resumption from
`Event::DatagramsUnblocked` -- which step A made reachable for the first time,
because `Blocked` now actually happens. Then a small buffer bounds latency
(its only job) while throughput stays bandwidth-limited.

Two known obstacles, both already measured:

- `base_budget_bytes` is `bytes_per_us * SCHEDULER_TICK_INTERVAL_US`
  floored at `SCHEDULER_TICK_BUDGET_FLOOR_BYTES` (256 KB), and the floor
  dominates every realistic interval, so the bandwidth term currently never
  decides anything. Elapsed-time billing was tried and measured as a no-op
  for exactly this reason.
- A continuation is only created when a drain stops with budget left
  (`scheduler_continuation_after_drain`), which does not happen while the
  clamp sets the budget to what the drain then spends. Resumption needs the
  budget to be the bandwidth's, not the buffer's, before it has anything to
  resume.

**Acceptance:** the slow-link sweep keeps 32 KiB's staleness (~51) while the
saturated fast-link sweep keeps 16 MiB's throughput (~24 MB).

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

---

## Bandwidth as an axis: the budget floor is the dominant term

**Date:** 2026-09-21

Two static link speeds (2 Mbit, 50 Mbit) were used for every measurement
above. That turns out to have hidden the largest effect in the system.

`base_budget_bytes` is

```
bytes_per_us * 33.3 ms * 0.90   .max(SCHEDULER_TICK_BUDGET_FLOOR_BYTES)
```

and the floor is 256 KiB **per 33.3 ms tick**, i.e. **7.86 MB/s**. With the
0.90 fraction the bandwidth-derived term only overtakes it above
**~70 Mbit/s**. Every link this system has been measured on -- and every link
it realistically serves -- is below that. **The bandwidth term has never
bound. The budget has been a constant all along.**

This is why billing the budget by elapsed time measured as a no-op, and why
step C' looked like new machinery: it is not. The machinery exists; the floor
sits above it.

### Measured: a saturating 60x34 scene, 30 s, 50 ms RTT

`floor=8K` lowers the floor to 8 KiB so the bandwidth term actually binds.

| link | floor | bytes | stale | retx | send_errs | emit_q_peak | qlat_max |
|---|---|---|---|---|---|---|---|
| 1 Mbit | 256K | 3,600,948 | 0 | 102,954 | 2,019 | **77,150** | 131 ms |
| 1 Mbit | 8K | 3,672,688 | 6,015 | 20,032 | 0 | **642** | 598 ms |
| 2 Mbit | 256K | 7,438,217 | 332 | 217,557 | 3,889 | **168,335** | 116 ms |
| 2 Mbit | 8K | 4,889,063 | 2,118 | 10,951 | 0 | **642** | 599 ms |
| 5 Mbit | 256K | 18,565,865 | 1,060 | 511,473 | 8,495 | **389,053** | 114 ms |
| 5 Mbit | 8K | 6,824,734 | 1,897 | 18,831 | 0 | **1,206** | 4.72 s |
| 10 Mbit | 256K | 6,973,279 | 150 | 20,291 | 0 | 3,904 | 6.40 s |
| 10 Mbit | 8K | 7,815,289 | 2,639 | 16,396 | 0 | 1,617 | 9.17 s |
| 25 Mbit | 256K | 20,822,935 | 5,839 | 20,379 | 0 | 3,929 | 29.46 s |
| 25 Mbit | 8K | 10,231,763 | 5,834 | 18,072 | 0 | 1,932 | 13.35 s |
| 50 Mbit | 256K | 24,267,070 | 6,451 | 22,110 | 0 | 6,004 | 29.33 s |
| 50 Mbit | 8K | 9,107,585 | **0** | **0** | 0 | 3,017 | **153 ms** |
| 100 Mbit | 256K | 46,462,927 | **85,601** | 106,904 | 776 | 26,912 | **28.88 s** |
| 100 Mbit | 8K | 8,591,378 | **0** | **0** | 0 | 2,823 | **97 ms** |

200 Mbit did not run: the harness bailed. Not investigated; see
`feedback-browserless-scene-stall-flake`.

### What it says

1. **`tiles_rendered` is 2040 in every single row.** The screen always fully
   converges. The choice is entirely about waste and latency, never coverage
   -- so any test gating on "did the screen arrive" cannot see this at all.

2. **The production floor's damage is worst at *low* bandwidth**, which is the
   opposite of where the earlier sweeps looked hardest. At 5 Mbit it drives
   **511,473 retransmit attempts and an emitter queue peaking at 389,053
   entries**. Lowering the floor cuts those to 18,831 (27x) and 1,206 (323x).

3. **At high bandwidth the low floor is better on every quality metric at
   once.** At 100 Mbit: staleness 85,601 -> 0, retransmits 106,904 -> 0,
   queued latency 28.88 s -> 97 ms.

4. **8 KiB is still too blunt.** It is itself ~246 KB/s (~2 Mbit), so on a
   1 Mbit link it still overdrives while now also throttling -- which is why
   the 1-5 Mbit rows trade retransmits away for staleness and latency rather
   than winning outright. The fix is not a smaller constant; it is no constant.

5. **`bytes_delivered_s2c` is not goodput.** The 5 Mbit/256K row "delivers"
   18.5 MB on an 18.75 MB link while rendering the same 2040 tiles as the
   6.8 MB row: it is retransmitting, not progressing. This is the same trap as
   the byte-ratio metric retired earlier, and it means the step-C throughput
   comparison above (24.3 MB vs 4.0 MB) deserves a goodput cross-check before
   it is leaned on further.

### An unexplained lead worth keeping

At 25-100 Mbit with the production floor, `queued_critical_latency_max_us`
reaches **~29 s in a 30 s scene** -- a critical-tier tile queued early and not
ACKed until the end. At 50 and 100 Mbit the low floor collapses that to
153 ms / 97 ms.

A tile that never resolves on an otherwise-healthy link is the shape of the
original production report (blurry regions on a quiet screen) that has
resisted reproduction across every scene tried so far. **The mechanism has not
been identified and this is not yet a reproduction** -- the queue peak at
those rows is only ~4-6k, so depth alone does not explain 29 s. It is the
strongest lead so far and should be chased directly.

### Capacity transitions: the hazard was mis-specified

The concern raised before running this was that a bandwidth-derived budget
inherits goog_cc's 5-7 s convergence, repeating the source-rate token bucket's
regression (`bwe-googcc-review.md`: 752,637 -> 548,602 delivered).

**That premise was wrong.** `base_budget_bytes` reads
`adaptation_context.bytes_per_us`, and that is

```rust
// io_bridge.rs:6312, from sampled quinn path stats
let bytes_per_us = (cwnd_bytes as f32) / smoothed_rtt_us;
```

-- quinn's **congestion window over smoothed RTT**, not goog_cc's
`target_rate`. It tracks capacity at RTT timescale, not at estimator-
convergence timescale. The token bucket's failure mode does not transfer.

Measured, same scene, capacity stepping at 3 s:

| transition | floor | stale | retx | send_errs | emit_q_peak | qlat_max |
|---|---|---|---|---|---|---|
| 50 -> 5 Mbit | 256K | 9,611 | 24,422 | 0 | 6,338 | 29.44 s |
| 50 -> 5 Mbit | 8K | **0** | **0** | 0 | 2,850 | **107 ms** |
| 5 -> 50 Mbit | 256K | **833,844** | **1,213,785** | 27,476 | **478,806** | 131 ms |
| 5 -> 50 Mbit | 8K | 3,130 | 13,672 | 0 | 1,593 | 9.33 s |
| 50/5/50 | 256K | 8,686 | 23,732 | 0 | 6,357 | 29.46 s |
| 50/5/50 | 8K | 3,064 | 7,110 | 0 | 2,859 | 107 ms |

1. **Step-up with the production floor is the worst configuration measured
   anywhere: 1,213,785 retransmit attempts, 833,844 stale tiles, an emitter
   queue 478,806 deep.** A capacity *increase* is turned into a retransmit
   storm -- the 5 Mbit opening phase is overdriven ~12x, the backlog builds,
   and when the link opens the whole stale backlog floods out.

2. **The predicted regression did not appear.** Step-up is where the low floor
   is weakest, and it still beats the production floor by 266x on staleness.

3. **`qlat_max` must be read against staleness, never alone.** The 5->50/256K
   row has the *best* queued latency in the table (131 ms) and the worst
   staleness (833,844): it ACKs quickly because it is retransmitting
   constantly. Latency is only meaningful once the delivered content is right.

4. **The residual weakness is step-up queued latency** (9.33 s at floor=8K,
   against 107 ms on the other two transitions). This is the one place a
   lagging allowance is visible, and it is the case for driving the budget
   from *observed* queueing rather than from `cwnd/RTT` -- but it is a
   refinement, not a blocker.

### Where this leaves the redesign

Step C' as originally written -- "take the drain budget from the bandwidth
estimate" -- is already implemented. The work is to stop the constant floor
from masking it. That is a far smaller change than specified, and the ladder
and transition sweeps are the acceptance evidence for it.

The remaining question is what replaces 256 KiB. It cannot be a smaller
constant (8 KiB is itself ~2 Mbit and still overdrives a 1 Mbit link); the
floor's only legitimate job is to keep the budget above one MTU so a tile can
always make progress.
