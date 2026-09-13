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

## This metric is the least sensitive of three, and Stage 2 needs a second one

Recorded after the baseline was measured, on further reading of the emission
path. It does not invalidate the numbers above, but it changes what they can
be expected to show.

**Emission order already prioritises the tiers.**
`Scheduler::drain_refinement_pass_major` (`scheduler.rs:649`) drains
**pass-major**:

```rust
while let Some(min_pass) = queue.iter().map(|w| w.pass_idx).min() {
```

It takes the lowest `pass_idx` present, emits every tile's work at that pass,
then moves to the next. So passes 0-3 already leave before passes 4-13. The
ordering Stage 2 is meant to introduce partly exists.

**But this metric cannot see that.** It measures `last_sent_at -> ACK
receipt` — a round trip that begins *when the pass leaves*. Pass 0 and pass
13 each take their own ~35 ms from their own emit to their own ACK, however
far apart those emissions were. Scheduler ordering changes *when* a pass is
sent, not how long its ACK takes afterwards, so it is largely invisible here.
That is a second, more specific reason the two tiers measure the same.

Three metrics are available in principle, in increasing sensitivity to what a
pacer changes:

| Metric | Start point | Captures | Available? |
|---|---|---|---|
| `last_sent_at -> ACK` | last (re)transmission | wire round trip, queueing *on the link* | **yes — this baseline** |
| `first_sent_at -> ACK` | first transmission | the above, plus retransmit delay | yes, `CacheEntry::first_sent_at` exists |
| `queued_at -> ACK` | when the work was known | the above, plus **scheduler queueing** | **no** — `queued_at` is on `TileWork` and never reaches `CacheEntry` |

The third is the one that expresses the deliverable. "Pass 0-3 latency drops"
most naturally means time from when a pass became available to when it was
confirmed delivered, and that is precisely the interval scheduler
prioritisation shortens.

**So Stage 2's first task is to plumb `queued_at` into `CacheEntry` and
baseline `queued_at -> ACK` as well.** Without it, a working pacer and a
broken one would both show a flat emit-to-ACK ratio, and the natural reading
would be "the pacer did nothing" — a false negative rather than the false
positive this document was written to prevent.

Keep measuring `last_sent_at -> ACK` too: it is the one that would reveal a
pacer *causing* wire queueing by overfilling the link, which is a real way
for this work to go wrong.

## BWE Stage 2.1: `queued_at -> ACK` re-baseline

**Date:** 2026-09-12
**Git rev:** `369dec12ce5b2d21bdc5ede6c03fb0ce77d92e41` (branch `spec/bwe-stage2`)
**Parent:** `2cfea5c` (`docs(spec): BWE Stage 2 design`)

This section answers the question the previous section raised: with
`queued_at -> ACK` actually measurable, does the critical/refinement ratio
move off the ~0.99 the `last_sent_at -> ACK` metric reported? **No
scheduling code changed for this measurement** — only the plumbing
(`TileWork::queued_at` -> `CacheEntry::queued_at` -> `BweSample` -> a second
`TierLatencyStats` pair) and the ignored `bwe_tier_latency_baseline` test's
printed output changed. The commit that landed the plumbing carries no
functional diff to `Scheduler`, the emitter's drain order, or any budget.

### What is being measured

Identical scene, method, and test to the Stage 2.0 baseline above — same
`busy_frames(2)` 4x4 CDF53 grid, 10% independent loss, 10 s duration, same
seed sequence `0xB17E0000..`, same "10 successful runs per batch, retry on
`MAX_ITERS` bail-out" protocol, same
`cargo test -p ghostframe-e2e --test browserless_runner bwe_tier_latency_baseline -- --ignored --nocapture --test-threads=1`
command, run 3 times. The only difference is the metric: `queued_at -> ACK`
is reported alongside the existing `last_sent_at -> ACK`, both read off
`BrowserlessResult` from the same runs.

`queued_at` is `TileWork::queued_at` — the instant the scheduler enqueued
the pass — carried unchanged through retransmits (unlike `last_sent_at`,
which is overwritten on every retry). So `queued_at -> ACK` additionally
counts however long a pass sat in `refinement_queue` before
`drain_refinement_pass_major` ever picked it up, on top of everything
`last_sent_at -> ACK` already counted.

Batch 1 hit one `MAX_ITERS` bail-out (seed `0xB17E0007`, backfilled by
`0xB17E000A`, same as the original Stage 2.0 baseline's batch 2 — same seed,
same known heavy-tail run); batches 2 and 3 were 10/10 clean.

### Sanity check: `queued_at -> ACK` >= `last_sent_at -> ACK`

Confirmed for every one of the 30 successful runs across all 3 batches, at
both the mean and the max, for both tiers — the re-baseline test now asserts
this directly (`bwe_tier_latency_baseline`'s four `assert!`s) and it held
without exception. `queued_at` starts at or before `last_sent_at` for the
same sample by construction, so this is the expected floor, not a
coincidence — but the brief asked for it to be checked rather than assumed,
and all 3 batches (`cargo test ... -- --ignored --nocapture --test-threads=1`,
run 3 times) passed clean.

Both new accumulators are non-zero in both tiers in every run: pooled across
all 30 runs, `queued_critical_latency_count` = 4,404 and
`queued_refinement_latency_count` = 11,013 — the same population sizes as
the existing `last_sent_at` counters, as expected since both pairs are fed
from the same ACK batches at the same drain site.

### Results (30 scene runs, pooled)

| Metric | Tier | Runs | Samples | Pooled mean | Max |
|---|---|---:|---:|---:|---:|
| `last_sent_at -> ACK` | Critical (0-3) | 30 | 4,404 | 46.7 ms | 305.0 ms |
| `last_sent_at -> ACK` | Refinement (4-13) | 30 | 11,013 | 47.5 ms | 305.0 ms |
| `queued_at -> ACK` | Critical (0-3) | 30 | 4,404 | 103.9 ms | 1546.0 ms |
| `queued_at -> ACK` | Refinement (4-13) | 30 | 11,013 | 106.8 ms | 1546.0 ms |

`queued_at -> ACK`'s pooled mean is roughly double `last_sent_at -> ACK`'s
for both tiers — consistent with the design's expectation that it adds
scheduler queueing delay on top of the wire round trip. Sample counts are
identical between the two metrics per tier, as they must be (same ACKs, two
different start points).

### Bucket distribution, `queued_at -> ACK` (all 30 runs pooled, ms)

| Bucket | Critical count | Critical % | Refinement count | Refinement % |
|---|---:|---:|---:|---:|
| 0-5 | 20 | 0.5% | 44 | 0.4% |
| 5-10 | 168 | 3.8% | 383 | 3.5% |
| 10-20 | 531 | 12.1% | 1,172 | 10.6% |
| 20-50 | 1,811 | 41.1% | 4,613 | 41.9% |
| 50-100 | 482 | 10.9% | 1,217 | 11.1% |
| 100-200 | 562 | 12.8% | 1,456 | 13.2% |
| 200-500 | 729 | 16.6% | 1,860 | 16.9% |
| 500+ | 101 | 2.3% | 268 | 2.4% |

Per-bucket percentages track each other within a point or two at every
bucket, including the 200-500 ms and 500+ tail buckets — the tail is not
shifting in critical's favour. This is the more sensitive metric the
previous section called for, and it shows the same "no separation" picture
`last_sent_at -> ACK` showed, not a hidden effect the coarser metric missed.

### The within-run ratio: still ~1.0

| Batch | `last_sent_at` critical | `last_sent_at` refinement | `last_sent_at` ratio | `queued_at` critical | `queued_at` refinement | `queued_at` ratio |
|---|---:|---:|---:|---:|---:|---:|
| 1 | 58.7 ms | 60.6 ms | 0.968 | 144.6 ms | 147.2 ms | 0.982 |
| 2 | 40.1 ms | 40.3 ms | 0.996 | 81.3 ms | 83.8 ms | 0.970 |
| 3 | 40.5 ms | 40.7 ms | 0.996 | 83.3 ms | 86.8 ms | 0.959 |

| Metric | Ratio range | Spread |
|---|---|---|
| `last_sent_at -> ACK` ratio | 0.968-0.996 | 1.029x |
| `queued_at -> ACK` ratio | 0.959-0.982 | 1.024x |

Both metrics land in the same band, both consistently a hair below 1.0,
and the `queued_at` ratio's batch-to-batch spread (1.024x) is no tighter or
looser than `last_sent_at`'s (1.029x) — neither reads as a load-bearing
effect against the noise floor the original document established (1.019x
on the same metric, same scene, different batches). Absolute means moved
as expected (`queued_at` roughly doubles `last_sent_at`, since it adds
queueing time on top of the wire round trip), but the *ratio* — the number
this document's own acceptance criterion is built on — did not move outside
where `last_sent_at -> ACK` already put it.

### Which of the three outcomes: **Ratio still ~1.0**

This is the design doc's second named outcome, not the first or third:

- Not "ratio already below 1.0" — 0.959-0.982 is the same distance from 1.0
  that the admittedly-noisy `last_sent_at` metric already showed
  (0.968-0.996 here, 0.978-0.997 in the original Stage 2.0 baseline), and
  the bucket distribution shows no tail separation at all.
- Not "ratio above 1.0" — nothing here suggests critical passes arrive
  *later*; there is no defect to stop and investigate.
- **It is "ordering is not translating into delivery latency."** Pass-major
  draining (`drain_refinement_pass_major`) does put passes 0-3 on the wire
  before 4-13 *within a given drain call*, but `queued_at -> ACK` — the
  metric built specifically to see queueing delay accumulated *before* that
  drain call runs — shows no benefit from it. The design doc's own
  hypothesis for this outcome names the likely reason:
  `refinement_bandwidth_fraction` (5-20% of the tick budget) is a small
  enough slice that a tick's budget is often consumed by refinement work
  queued from *earlier frames* before a later frame's critical passes are
  even reachable, regardless of how those critical passes are ordered once
  their own frame's work is being drained.

### What this means for 2.3

Per the design doc's own sequencing: this result means 2.3 is **not**
unnecessary (that was outcome 1's conclusion), and it is **not** chasing a
defect (outcome 3's). It is exactly the case the design doc pre-committed
to address by changing the *budget split*, not the *drain order*: a
guaranteed slice of the tick budget for `PassTier::Critical`, rather than
having all CDF53 passes — critical and refinement alike — draw from the
same `refinement_bandwidth_fraction`. `drain_refinement_pass_major`'s
ordering is real and correct; it just never gets the chance to matter while
critical and refinement passes compete for the same undifferentiated
budget slice every tick.

### Reading the 2.1 result precisely

"Still ~1.0" undersells what the numbers say. Two things are visible.

**The new metric is measurably more sensitive, as designed.** Mean ratio
across the three batches:

| Metric | Mean ratio | Range |
|---|---:|---|
| `last_sent_at -> ACK` | 0.987 | 0.968-0.996 |
| `queued_at -> ACK` | **0.970** | 0.959-0.982 |

`queued_at -> ACK` sits 1.6 points lower. It is picking up scheduler
queueing that the emit-relative metric cannot see, which is exactly the
reason it was built. That is a working instrument, not a null result.

**Pass-major ordering produces a real but small advantage: ~3%.** A ratio of
0.970 means critical passes are confirmed delivered about 3% sooner than
refinement ones. So `drain_refinement_pass_major` is not inert — it is simply
not worth much at the current budget split.

**The tail is where it fails.** Critical and refinement track within a point
or two at *every* bucket, including 200-500 ms (16.6% vs 16.9%) and 500+ ms
(2.3% vs 2.4%). Queueing delay under load shows up in the tail, and that is
precisely where the ordering is buying nothing.

That pattern — a few percent at the mean, nothing at the tail — is what the
design predicted for outcome 2: intra-drain ordering cannot help a pass that
has not been reached yet, because earlier frames' refinement work consumed
the tick budget first. Ordering within a slice does not matter when the slice
itself is the constraint.

**So Stage 2.3 targets the budget split, not the drain order**, and the
number to beat is a 0.970 mean ratio with no tail separation — not 1.0.

## BWE Stage 2.2: `PacingMode` — the estimate governs the budget

**Date:** 2026-09-12
**Git rev:** `4a321152f5a264a3148506acefd5ceb0af40171c` (branch `feat/bwe-pacing-mode`)
**Parent:** `5485afd` (`docs: sharpen the 2.1 reading`, tip of `spec/bwe-stage2`)

Stage 2.2 puts goog_cc's bandwidth estimate in charge of the per-tick
emission budget for the first time — `self.bwe` previously fed the
estimator and published a snapshot nobody consumed. `PacingMode::Paced`
additionally bounds the budget by goog_cc's `pacer_config`-derived rate
once the estimator has seen enough ACK samples to trust
(`samples_seen >= 150`); the budget is `min(aimd_budget, googcc_budget)`,
not a replacement, and `clamp_to_quinn_capacity` still runs last. See
`ghostframe-lib/src/transport/io_bridge.rs`'s `combine_pacing_budget` and
`ghostframe-lib/src/transport/bwe/googcc.rs`'s `absorb`.

**No tier-prioritisation, drain-order, or `refinement_bandwidth_fraction`
code changed for this measurement** — that is 2.3, deliberately
unspecified until this result is in. Only the budget's *source* changed.

### Method

Identical scene, method, and re-measure protocol to the 2.0/2.1 baselines
above: same `busy_frames(2)` 4x4 CDF53 grid, 10% independent loss, 10 s
duration, same seed sequence `0xB17E0000..`, same "10 successful runs per
batch, retry on `MAX_ITERS` bail-out" protocol, same
`cargo test -p ghostframe-e2e --test browserless_runner bwe_tier_latency_baseline -- --ignored --nocapture --test-threads=1`
command, run 3 times (30 total data points). Both metrics
(`last_sent_at -> ACK` and `queued_at -> ACK`) are reported, exactly as in
2.1, since both are needed to read this result correctly — see "Which
metric moved" below.

All three batches ran 10/10 clean — no `MAX_ITERS` bail-outs at all (the
2.1 baseline had one, in batch 1).

Regression guards, run separately before this measurement
(`cargo test -p ghostframe-e2e --test browserless_runner -- --test-threads=1`,
11/11 non-ignored tests): `cdf53_converges_to_lossless_under_10pct_loss` and
`every_cdf53_pass_eventually_lands` both green, alongside the full existing
suite (`retransmits_fire_under_loss_but_not_on_a_perfect_link` included).

### Results (30 scene runs, pooled)

| Metric | Tier | Runs | Samples | Pooled mean | Max |
|---|---|---:|---:|---:|---:|
| `last_sent_at -> ACK` | Critical (0-3) | 30 | 4,336 | 39.5 ms | 181.0 ms |
| `last_sent_at -> ACK` | Refinement (4-13) | 30 | 10,810 | 40.0 ms | 181.0 ms |
| `queued_at -> ACK` | Critical (0-3) | 30 | 4,336 | 84.8 ms | 656.0 ms |
| `queued_at -> ACK` | Refinement (4-13) | 30 | 10,810 | 86.4 ms | 745.0 ms |

### The guard: `last_sent_at -> ACK` did not rise

This is the metric that exists specifically to catch a pacer *causing* the
wire queueing it's meant to prevent (a real way for 2.2 to go wrong per the
design doc's risk table).

| | 2.1 baseline (pre-`PacingMode`) | 2.2 (post-`PacingMode`) | Direction |
|---|---:|---:|---|
| `last_sent_at -> ACK` pooled mean, critical | 46.7 ms | 39.5 ms | **down** |
| `last_sent_at -> ACK` pooled mean, refinement | 47.5 ms | 40.0 ms | **down** |
| `last_sent_at -> ACK` pooled max | 305.0 ms | 181.0 ms | **down** |
| `queued_at -> ACK` pooled mean, critical | 103.9 ms | 84.8 ms | **down** |
| `queued_at -> ACK` pooled mean, refinement | 106.8 ms | 86.4 ms | **down** |
| `queued_at -> ACK` pooled max | 1546.0 ms | 656.0 ms | **down** |

**Guard passes — every one of these fell, none rose.** Per this document's
own repeated caution (2.1's spread section, and the design doc's own
framing), absolute means are noisy across batches and not something to
gate on by themselves; a bounded, real-network-estimate-driven budget that
tracks the link more closely than a fixed AIMD ramp plausibly explains
less queueing (both at the wire and in the scheduler) even though nothing
about *which* pass goes first changed. Reported for the record, not as the
primary acceptance criterion — the ratio is that, and is covered next.

### The tier ratio: flat, as the design predicted it would be

| Batch | `last_sent_at` ratio | `queued_at` ratio |
|---|---:|---:|
| 1 | 1.001 | 0.985 |
| 2 | 0.976 | 0.968 |
| 3 | 0.989 | 0.991 |
| **Mean** | **0.989** | **0.981** |

| Metric | 2.1 mean ratio | 2.1 range | 2.2 mean ratio | 2.2 range |
|---|---:|---|---:|---|
| `last_sent_at -> ACK` | 0.987 | 0.968-0.996 | 0.989 | 0.976-1.001 |
| `queued_at -> ACK` | 0.970 | 0.959-0.982 | 0.981 | 0.968-0.991 |

Both ratios land inside (or a hair above, for `queued_at`) the 2.1 band —
not a load-bearing move in either direction, and both still far from the
"materially below 1.0" the design's acceptance criterion asks for. This
is the expected, stated-in-advance outcome: **"the tier ratio is not
expected to move much here. 2.2 changes *how much* is sent, not *what
order*."** A pacer that only re-sizes the budget cannot separate critical
from refinement when both tiers still draw from the same undifferentiated
`refinement_queue` and the same `refinement_bandwidth_fraction` slice —
that is exactly 2.3's job, still open. If anything, `queued_at`'s ratio
inching from 0.970 toward 0.981 is consistent with a smaller, better-fit
budget slightly *reducing* the raw queueing-delay gap 2.3 will need to
close, rather than 2.2 having incidentally done 2.3's work — but one
measurement 1.1 points inside a documented ~1.02-1.03x batch-to-batch
noise band is not evidence of that, just an observation for whoever reads
this next.

### Which metric moved, and why that's the right read

`last_sent_at -> ACK`'s absolute pooled means fell by about 15-18% and its
max nearly halved (305 ms -> 181 ms); `queued_at -> ACK`'s fell by a
similar fraction (103.9/106.8 ms -> 84.8/86.4 ms) with its max also
roughly halving (1546 ms -> 656/745 ms). The *ratio* between tiers — the
number this document's acceptance criterion is actually built on — moved
far less, staying inside the pre-existing noise band. That split is
exactly what 2.2 was supposed to produce: a budget that fits the link
better reduces queueing and retransmit-driven tail latency for *all*
traffic roughly equally, without doing anything that would make critical
passes specifically faster than refinement ones. The estimate is now
governing emission (goal 1 of the design doc); it is not yet steering
*which* tier gets the governed bytes first (goal 3, deferred to 2.3).

### What this means for 2.3

Unchanged from 2.1's conclusion, now confirmed rather than merely
predicted: 2.2 does not touch the tier axis, so it could not have made
2.3 unnecessary, and it did not. The next number to beat is still the
0.970 (`queued_at`) / 0.987 (`last_sent_at`) mean ratios from 2.1 — 2.2's
0.981 / 0.989 are statistically the same result, not an improvement.
2.3's guaranteed-slice-for-`PassTier::Critical` design (from the sequencing
doc) is still the outstanding piece, and now has a `PacingMode`-governed
budget underneath it to split rather than an unbounded AIMD ramp.

### Caveat on the 2.2 magnitude: the harness path was previously unpaced

The guard result stands — `last_sent_at -> ACK` fell rather than rose, which
is the safety-critical finding. But the **size** of the drop should not be
read as "goog_cc beats AIMD", because on the measured path it is not being
compared against AIMD at all.

`apply_injected_frame` takes its budget from `inj.budget_bytes`, and the
browserless harness passes **`usize::MAX`** (`browserless.rs:710`, `:739`) —
i.e. unpaced, drain everything. So:

| Path | Before 2.2 | After 2.2 |
|---|---|---|
| `dispatch_dirty_tiles_via_scheduler` (production) | AIMD from quinn path stats | `min(AIMD, googcc)` |
| `apply_injected_frame` (harness, and what the baseline measures) | **unpaced** | `min(usize::MAX, googcc)` = googcc |

The measurement therefore captures **unpaced -> goog_cc-paced**, which is a
larger change than production will see, since production already had AIMD
doing some of that work. Expect a smaller improvement on the dispatch path.

Applying the combine at *both* sites was still right: confining it to the
dispatch path would have left the change invisible to every browserless
measurement, and a metric that cannot see the thing it is measuring is worse
than a caveat.

Two consequences worth carrying forward:

- **Do not quote the 2.2 delta as a production figure.** The honest claim is
  "pacing did not increase wire queueing, and reduced it on a previously
  unpaced path".
- A cleaner future comparison would run the harness with the AIMD budget
  rather than `usize::MAX`, so both arms are paced and only the *source* of
  the rate differs. That is a harness change, not a production one, and it is
  not required for 2.3.

## Correction: the harness never exercises pass-major ordering

Found while designing 2.3, and it invalidates an earlier reading in this
document. **The measurements are sound; the interpretation attached to them
was not.**

Earlier text here said pass-major ordering "is not inert — it is worth about
3%". That is wrong. On the measured path, pass-major ordering **never runs**.

There are two enqueue paths and two drains, and they pair differently:

| Path | Enqueue | Queue | Drain |
|---|---|---|---|
| Production CDF53 refinement | `enqueue_refinement_at` | `refinement_queue` | `drain_refinement_pass_major` — **pass-major** |
| Browserless harness | `enqueue_at` (`io_bridge.rs:2088`) | `priority_queue` | `drain_priority_queue` — **FIFO** |

`apply_injected_frame` routes **every** work item — CDF53 passes included —
through `enqueue_at`, which pushes to `priority_queue`.
`drain_priority_queue` iterates `for work in queue.iter_mut()`: pure
insertion order, no `pass_idx` consideration anywhere.

And the insertion order is **tile-major**. `scene_tiles.rs` builds passes
0..13 for one tile before moving to the next, so the queue holds
tile A pass 0..13, then tile B pass 0..13, and so on.

**So the harness emits tile B's pass 0 — a critical pass — after tile A's
pass 13.** That is not "prioritisation that isn't paying off"; it is the
inverse of prioritisation. On a 4x4 grid the last tile's critical pass sits
behind 15 x 14 = 210 entries. A flat tier ratio is the *expected* result of
that ordering, not evidence about the production one.

The ~3% that does appear is incidental: within a single tile, FIFO happens to
emit pass 0 before pass 13.

### What this means for 2.3

**The premise 2.3 was about to be built on is unverified.** This document
previously concluded that ordering exists but the budget slice is too small
to let it pay off, and pointed 2.3 at the budget split. That conclusion came
from a path with no ordering at all, so it is not evidence for or against the
budget-split hypothesis.

Production may already prioritise critical passes well. Nothing here measures
it.

**The next step is harness fidelity, not a pacer change.** Route the
harness's CDF53 work through `enqueue_refinement_at` so it uses
`refinement_queue` and the pass-major drain, then re-measure. That is a test
change, not a production one, and it is the only way to learn whether 2.3 is
needed.

If the ratio drops sharply once the harness uses the production path, 2.3 is
unnecessary and the honest answer is that production was already doing the
right thing. That is a good outcome, and cheaper to discover now than after
building a budget split against it.
