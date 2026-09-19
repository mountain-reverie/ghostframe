# Is the RTO still needed?

**Date:** 2026-09-18
**Question:** with a priority-queue scheduler, receiver-side coverage
tracking, FEC parity and per-`wire_seq` acknowledgement, does the
retransmission timer still earn its place?

**Answer: yes, but only for one narrow case, and it is currently charging
roughly 44% of the link to cover it.**

---

## What recovery mechanisms actually exist

| mechanism | scope | driven by | knows what is missing? |
|---|---|---|---|
| FEC parity (XOR group) | any datagram | sender, proactive | n/a — repairs 1 loss per group |
| Coverage NACK + tail sweep | **Cdf53 only** | receiver | yes — exact pass mask |
| Assembly-timeout NACK | multi-fragment tiles | receiver | yes — exact fragment |
| RTO timer | any cached pass | sender, on elapsed time | **no** |
| Scheduler re-emit | dirty tiles | content change | n/a |

The two NACK paths are ground truth: the receiver states what did not arrive.
The emitter's own comment says so — *"a NACK is the client stating outright
that this fragment did not arrive ... unlike RTO, which fires on delay as
readily as on loss."*

Both NACK paths depend on the **same retransmit cache** the RTO uses
(`on_nack` does `cache.get_mut(&key)` and counts a miss otherwise). So the
cache is not in question here. Only the timer is.

## The gap only the RTO covers

Both receiver-driven paths need the receiver to know the tile-pass exists:

- `scan_assembly_timeouts` iterates `self.assemblies` — an assembly exists
  only once **at least one fragment has arrived**, and it NACKs only missing
  fragments of a partially-arrived tile.
- `tail_sweep` iterates `self.cdf53_coverage`, which is written **only in the
  `Codec::Cdf53` branch** of reassembly.

So for a tile sent as a **single datagram** that is **lost in its entirety**:
no assembly, no coverage entry, no NACK — ever. The receiver cannot ask for
what it does not know was sent. For Cdf53 this is usually rescued, because
some other pass of the same tile creates the coverage entry and the tail sweep
then re-NACKs the missing ones every 500 ms until the mask is full. For
**Solid, PalRle and H264 there is no coverage map at all**, and the scheduler
has no periodic refresh for an unchanged tile.

That leaves exactly one uncovered failure: **a tile's last update is lost and
the tile then goes static.** FEC repairs it if it was an isolated loss within
its XOR group; otherwise the only thing that repairs it is the RTO. It is also
the most visible failure a remote desktop can have — a permanently stale
region — and plausibly part of what was seen in the field.

**That job is real and the timer should not simply be deleted.**

## What the timer costs to cover it

Measured on the busy 6x6 grid, 70 ms RTT:

| | RTO on | RTO off |
|---|---|---|
| retransmissions, **lossless** link | 1788 | **0** |
| bytes delivered s2c | 386,844 | 215,751 |

**44% of everything the server sent on a lossless link was retransmission
carrying nothing new.** Under load with 10% loss the split between the two
mechanisms is:

| | count |
|---|---|
| RTO fires | 1280 |
| NACK hits | 89 |
| NACK misses | **0** |
| ledger expiries (stranded entries) | 2847 |

The receiver-driven path asked for 89 repairs and got all 89 — it never once
failed to find its cache entry. The timer fired 1280 times, overwhelmingly
because of the ledger-expiry bug that strands entries
(`retransmit-storm-root-cause.md`), not because anything was lost.

## Removing the timer does not fail any acceptance test

The full browserless suite, with the RTO timer disabled and cache + NACK
intact: **17/17 pass, identical to baseline.** That includes
`cdf53_converges_to_lossless_under_10pct_loss` and
`every_cdf53_pass_eventually_lands`.

It also includes `retransmits_fire_under_loss_but_not_on_a_perfect_link`,
which passes **with the timer off** — `on_nack` bumps the same
`retransmit_attempts_total` counter, so that test cannot distinguish the two
mechanisms and does not pin the RTO at all.

At the margin the timer is not even helpful. Sweeping loss on the static
single-frame scene, 5 runs per cell:

| loss | RTO on | RTO off |
|---|---|---|
| 0.40 | 4/5 | **5/5** |
| 0.45 | 0/5 | 0/5 |

(At 0.50 both fail; an earlier single run suggesting the RTO helped there was
noise — on-RTO fails 2 of 3 at that rate too.) On a capped link, spurious
retransmissions compete for capacity with the targeted ones, so the timer
slightly *hurts* convergence where it was supposed to help most.

Note also that the timer is dormant when volume is low: on the single-tile
10% loss scene it fires **zero** times and nothing expires. The storm is
load-dependent, which is why it did not show up until a busy real session.

## Conclusion

The RTO is covering one genuine case — last-write-lost-then-static on a tile
the receiver cannot know about — and is paying for it by retransmitting
indiscriminately on a timescale (40 ms, ceiling 50 ms) far below any
acknowledgement it could possibly be waiting for.

Recommended shape:

1. **Keep a repair timer, but make it a last resort, not a first response.**
   Its job is "nobody has told me this landed and nothing else will ask for
   it" — that is a seconds-scale concern, not a 40 ms one.
2. **Retire it for Cdf53 entirely.** Coverage + tail sweep already cover every
   pass of every tile the receiver has seen, with exact knowledge. The timer
   contributes nothing there and costs the 44% measured above.
3. **Give the uncovered case a receiver-side mechanism instead**, and the gap
   closes properly: a coverage map for non-Cdf53 tiles, or a periodic
   scheduler sweep that re-offers any tile with no acknowledged emission. Then
   the timer can go entirely.
4. **Fix the stranded-entry bug regardless.** 2847 expiries in an 8 s scene is
   both the source of the spurious fires and an unbounded-growth risk: the
   cache has no eviction, and a stranded entry is only ever cleared by
   supersession.

Reproduce with `GHOSTFRAME_NO_RTO=1` (disables the timer only) and
`GHOSTFRAME_RTO_PROBE=1` (per-fire / per-ACK / per-expiry / per-NACK lines).
