# BWE Stage 2.0: Pre-Pacing Tier Latency Baseline

**Date:** 2026-09-12
**Git rev:** `1da68bb8d22759e720f5cd43fcdea48d045111f4` (branch `feat/bwe-tier-latency`)
**Parent (master):** `380c9cbcd75a7e58a6e4cc2332ee798c681d86eb`

This is the baseline `docs/superpowers/specs/2026-09-10-bwe-pacing-design.md`'s
"Baseline first" section calls for. It exists so Stage 2's pacer restructure
has a real pre-pacing number to beat instead of nothing. **No pacer code has
changed for this measurement** — the emission path is exactly what shipped
before this branch; only the measurement (`TierLatencyStats` in
`ghostframe-lib/src/transport/io_bridge.rs`) is new. No assertion or CI gate
is attached to these numbers.

## What is being measured

Per pass-tier (`Critical` = CDF53 passes 0-3, `Refinement` = passes 4-13),
for every ACKed pass:

```
ack_latency_us = received_at - server_emit_us
```

Both `received_at` (when the server processed the ACK) and `server_emit_us`
(when the server last sent that pass — a retransmit updates this to the
retry time, not the original send) are read from the **server's own
monotonic clock** (`IoBridge::bwe_epoch`). This is deliberately an
**emit-to-ACK-receipt round trip, not a one-way delay**: the client's echoed
`arrival_time_ms_lo16` is on the client's clock with an unknown epoch offset
relative to the server, and mixing the two would produce meaningless
absolute values (that mismatch is exactly what Stage 1's
`implausible_rtt_samples` counter exists to catch). A round trip is also the
right thing to reduce here — pacing (Stage 2) shortens *queueing* delay for
critical passes before they're even handed to quinn, and queueing delay is
part of this round trip, not the one-way wire delay.

## Scene and commands

Reuses the exact scene shape from
`retransmits_fire_under_loss_but_not_on_a_perfect_link`
(`ghostframe-e2e/tests/browserless_runner.rs`): a 4x4 (16-tile) CDF53 grid,
`busy_frames(2)` (two frames, each rewriting all 16 tiles with fresh
content), 10% independent loss, 10 s scene duration. That test's doc
comment records why `busy_frames(2)` is the traffic volume used: it is the
smallest `busy_frames(N)` that reliably produces retransmit-worthy
concurrent load without triggering `drive_session`'s `MAX_ITERS` (5000)
iteration-budget bail-out at any meaningful rate — `busy_frames(8)` bails
out on roughly 15-30% of runs purely from iteration-budget exhaustion under
`start_paused`'s high-traffic-many-iterations-per-virtual-µs behavior, while
otherwise healthy. `busy_frames(2)` still bails out occasionally (1 of 31
attempts here — see below), just far less often.

The measurement itself is a new `#[ignore]`'d test,
`bwe_tier_latency_baseline`, added alongside that scene in the same file. It
runs the scene 10 times (seeds `0xB17E0000..0xB17E0009`, incrementing on a
bail-out so a skipped attempt doesn't cost a data point) and prints each
run's `TierLatencyStats` summary from `BrowserlessResult`. Reproduce with:

```bash
cd /home/cedric/work/ghostframe
cargo test -p ghostframe-e2e --test browserless_runner \
  bwe_tier_latency_baseline -- --ignored --nocapture --test-threads=1
```

Run three times (three separate invocations of the command above, i.e.
three independent batches of 10 successful scene runs each = 30 total data
points) to characterize spread, since the harness is not seed-reproducible
(`feedback_browserless_not_seed_reproducible`: identical seeds still
diverge run to run) — a single invocation's numbers would be
noise, not signal.

## Results (30 scene runs, pooled)

| Tier | Runs | Samples | Pooled mean | Max | Per-run mean range |
|---|---:|---:|---:|---:|---:|
| Critical (passes 0-3) | 30 | 4,598 | 67.9 ms | 630.0 ms | 15.4 ms – 339.2 ms |
| Refinement (passes 4-13) | 30 | 11,501 | 68.4 ms | 630.0 ms | 15.5 ms – 332.5 ms |

One run (batch 2, seed `0xB17E000A`) is a heavy-tail outlier at both tiers
(mean ~335 ms, several samples past 500 ms) — a plausible consequence of
10% independent loss occasionally stacking multiple RTO backoffs on the
same pass within one run, not a measurement bug (per-run counts scale up
correspondingly: 479/1221 samples vs the typical ~130-200/320-500). Pooled
stats **with that run excluded** (29 runs):

| Tier | Runs | Samples | Pooled mean | Max |
|---|---:|---:|---:|---:|
| Critical (passes 0-3) | 29 | 4,119 | 36.4 ms | 181.0 ms |
| Refinement (passes 4-13) | 29 | 10,280 | 37.0 ms | 206.0 ms |

Both tables are reported because Stage 2 will hit both kinds of runs — an
average pacer needs to help the typical case (~36-37 ms) without leaving the
heavy-tail case (~330 ms means, 600+ ms max) as bad or worse. The pooled
"all 30 runs" table is the more honest single number to compare against,
since excluding "bad" runs by hand is exactly the kind of cherry-picking a
pacer's regression check must not get away with either.

### Bucket distribution (all 30 runs pooled, ms)

| Bucket | Critical count | Critical % | Refinement count | Refinement % |
|---|---:|---:|---:|---:|
| 0-5 | 74 | 1.6% | 171 | 1.5% |
| 5-10 | 317 | 6.9% | 773 | 6.7% |
| 10-20 | 629 | 13.7% | 1,459 | 12.7% |
| 20-50 | 2,342 | 50.9% | 6,018 | 52.3% |
| 50-100 | 618 | 13.4% | 1,525 | 13.3% |
| 100-200 | 281 | 6.1% | 701 | 6.1% |
| 200-500 | 243 | 5.3% | 604 | 5.3% |
| 500+ | 94 | 2.0% | 250 | 2.2% |

## The headline finding: critical and refinement are statistically the same

Pooled mean, max, and bucket-percentage distributions are nearly identical
between `Critical` (passes 0-3) and `Refinement` (passes 4-13) — within
noise of each other at every bucket. This is the expected, and diagnostic,
pre-pacing result — but the reason is more specific than "no priority
ordering exists", and Stage 2 should start from the specific version.

`Scheduler` **does** already have two queues, `priority_queue` and
`refinement_queue` (`scheduler.rs:77`, `:80`), with a split byte budget and a
defined drain order. They just do not split along the tier axis:

- `enqueue_at` (`:209`) puts fresh, non-CDF53 tile work in `priority_queue`.
- `enqueue_refinement_at` (`:735`) pushes **every** CDF53 pass — `pass_idx` 0
  through 13 alike — into `refinement_queue`, in arrival order.

So `Critical` (0-3) and `Refinement` (4-13) are both in the *same* queue,
drained FIFO. The existing split is "CDF53 progressive passes vs everything
else", which is a different axis from the one this instrumentation measures.
That is why the two tiers come out identical: nothing has ever ordered them
relative to each other.

**This makes Stage 2 smaller than "pacer restructure" suggests.** The
machinery — two queues, a split budget, a drain order — already exists. What
is missing is tier-awareness *inside* `refinement_queue`: either split it in
two, or order its drain by `pass_tier`. Starting from a rewrite would
discard working code and a working budget split. **This
is precisely the gap Stage 2's six-priority-queue pacer is meant to close**
— if the pacer works, `Critical` should separate from `Refinement` in favor
of `Critical`, at least in the tail (the 100-500+ ms buckets, which is where
queueing delay under load shows up). A post-pacer measurement using this
same test should show critical-tier mean/p95/max dropping while
refinement-tier throughput (not measured here — see
`bytes_emitted_critical`/`bytes_emitted_refinement` and the existing
`every_cdf53_pass_eventually_lands` starvation guard) does not regress.

## Spread across the three batches (10 runs each)

| Batch | Bail-outs | Critical pooled mean | Critical max | Refinement pooled mean | Refinement max |
|---|---:|---:|---:|---:|---:|
| 1 | 0 | 39.3 ms | 181.0 ms | 39.5 ms | 181.0 ms |
| 2 | 1 (seed `0xB17E0007`, backfilled by `0xB17E000A`) | 114.9 ms | 630.0 ms | 115.3 ms | 630.0 ms |
| 3 | 0 | 39.8 ms | 181.0 ms | 40.7 ms | 206.0 ms |

Batch 2's pooled mean is ~3x the other two, entirely because it's the batch
that drew the heavy-tail outlier run (seed `0xB17E000A`, mean ~335 ms — see
above); batches 1 and 3 agree closely with each other (~39-40 ms) and with
the "outlier excluded" pooled table above (~36-37 ms). Batch-to-batch means
otherwise vary by run-to-run luck under the same seed sequence — consistent
with `feedback_browserless_not_seed_reproducible`. This is why the report above
pools all 30 runs rather than trusting any single batch, and why Stage 2's
comparison should also pool multiple runs rather than compare one
before-run to one after-run.


## Use the within-run ratio, not the absolute latency

The absolute numbers above swing too widely to hold Stage 2 to. Across the
three batches the pooled critical mean moved 39.3 -> 114.9 ms, a **2.92x**
range, driven entirely by which runs happened to hit a heavy tail. A pacer
that genuinely improved critical latency by 30% could easily land inside that
noise, and one that regressed it could easily look like an improvement.

The **critical / refinement ratio within the same run** does not have that
problem:

| Batch | Critical mean | Refinement mean | Ratio |
|---|---:|---:|---:|
| 1 | 39.3 ms | 39.5 ms | 0.995 |
| 2 | 114.9 ms | 115.3 ms | 0.997 |
| 3 | 39.8 ms | 40.7 ms | 0.978 |

| Metric | Spread across batches |
|---|---|
| Absolute critical mean | 39.3-114.9 ms (**2.92x**) |
| Critical / refinement ratio | 0.978-0.997 (**1.019x**) |

Both tiers ride the same link, the same loss draws and the same queue in any
given run, so pairing them cancels almost all of the run-to-run variance —
the ratio is roughly 150x more stable than either absolute value.

It also states the goal directly. Stage 2's pacer exists to make passes 0-3
arrive *sooner than* passes 4-13; the ratio measures exactly that, whereas an
absolute figure measures it tangled up with whatever the link was doing.

**Recommended Stage 2 acceptance criterion**

- **Baseline: ratio = 0.99 (0.978-0.997).** Effectively 1.0 — no
  prioritisation, as expected.
- **Pass: ratio drops materially below 1.0**, reproducibly across batches,
  with the tail (100-500+ ms buckets) shifting in critical's favour.
- **Guard:** refinement throughput must not regress. The ratio alone can be
  improved the wrong way — by making refinement *worse* rather than critical
  *better* — so it must be read alongside `bytes_emitted_refinement` and the
  existing `every_cdf53_pass_eventually_lands` starvation scene. **A falling
  ratio with flat critical latency is a regression wearing a success.**

Report absolute numbers too, for the record. Just do not gate on them.

## What this does NOT show

- This is not a one-way delay measurement — see "What is being measured"
  above. A GoogCC-driven pacer reduces queueing delay, which is part of
  this round trip; it does not reduce the network's actual propagation
  delay, which this number can't isolate from queueing anyway.
- `busy_frames(2)` is a synthetic, uniformly-lossy (10%) scene on a
  simulated socketpair link with no real network jitter. It exercises the
  retransmit path (RTO + occasional NACK) enough to produce a meaningful
  distribution, but it is not a claim about any particular real-world link.
- Refinement-tier *throughput* (bytes/sec) is not part of this report; it
  already has its own telemetry (`bytes_emitted_refinement`,
  `bps_refinement` in the periodic `ghostframe::bwe` log target) and its
  own regression guard (`every_cdf53_pass_eventually_lands`). Stage 2's
  acceptance criterion needs both numbers, not just this one.
