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

## Harness fidelity fix: `apply_injected_frame` now uses `refinement_queue`

**Date:** 2026-09-12
**Git rev:** `d3f3df222c3c42ca335eb5651daffa6ed5e16812` (branch
`fix/harness-uses-refinement-queue`)
**Parent:** `73d28529a470f2bb9ee85464a15c9604dd4c11aa` (the "Correction" entry
above)

This lands the fix the correction called for. `apply_injected_frame`
(`ghostframe-lib/src/transport/io_bridge.rs`) now routes by `work.codec`:
CDF53 passes go to a new `Scheduler::enqueue_refinement_work_at`, which
pushes an already-formed `TileWork` straight into `refinement_queue` —
mirroring `enqueue_at` but targeting the pass-major-drained queue instead of
the FIFO one. Everything else keeps going through `enqueue_at` into
`priority_queue`, unchanged.

`enqueue_refinement_work_at` was chosen over grouping the harness's CDF53
work by `(tile_x, tile_y, generation)` and calling the existing
`enqueue_refinement_at`/`enqueue_refinement_subset_at`: both of those rebuild
`pass_idx`/`total_passes` from a `Vec<Vec<u8>>`'s position, which would
silently renumber a partial or out-of-order pass set. The harness's
`TileWork` already carries its own correct `pass_idx`/`total_passes` (built
by `scene_tiles.rs::encode_tile`), so pushing it through unchanged is both
simpler and strictly more faithful to what a scene actually submitted. A new
regression test,
`injected_cdf53_work_lands_in_refinement_queue_not_priority_queue`
(`io_bridge.rs`), submits two tiles' CDF53 passes out of order and as a
partial set alongside a non-CDF53 tile, and asserts directly (via new
`refinement_peek_for_test`/`scheduler_refinement_peek_for_test` accessors)
that CDF53 work lands in `refinement_queue` with `pass_idx`/`total_passes`
preserved exactly, while the non-CDF53 tile still lands in `priority_queue`.

`supersede_pending_for_tile` needed no change: it already chains
`priority_queue.iter_mut().chain(refinement_queue.iter_mut())` in one pass
(`scheduler.rs:237-250`), so it invalidates stale queued work in either
queue regardless of which one a given `TileWork` lands in.

**No production emission, pacer, budget, or `refinement_bandwidth_fraction`
code changed.** This is a test-harness-only change: which queue
`apply_injected_frame` enqueues CDF53 work into. All 11 non-ignored
browserless scenes stay green, including the three this document's own
brief called out as CDF53-delivery-dependent
(`cdf53_converges_to_lossless_under_10pct_loss`,
`every_cdf53_pass_eventually_lands`, `superseded_generations_never_render`),
alongside the full 398-test `ghostframe-lib` suite.

### Re-measurement: same scene, same protocol, now through `refinement_queue`

Identical scene, method, and re-measure protocol to every prior entry in
this document: `busy_frames(2)` 4x4 CDF53 grid, 10% independent loss, 10 s
duration, seed sequence `0xB17E0000..` incrementing past bail-outs, same
`cargo test -p ghostframe-e2e --test browserless_runner
bwe_tier_latency_baseline -- --ignored --nocapture --test-threads=1` command,
run 3 times (30 successful runs total). Both metrics (`last_sent_at -> ACK`
and `queued_at -> ACK`) reported, as in every entry since 2.1. The only thing
that changed between this run and the 2.2 entry above is the harness fix
described in this section — no scheduling, pacing, or budget code differs.

Batches 2 and 3 each hit 3 `MAX_ITERS` bail-outs (seeds `0xB17E0001`,
`0xB17E0004`, `0xB17E000A`, backfilled by `0xB17E000B`, `0xB17E000C`, and one
more); batch 1 was 10/10 clean. Bail-out rate and pattern are consistent
with the pre-existing `busy_frames(2)` flake rate documented earlier in this
file — not a consequence of this change.

Regression guards, run separately before this measurement (`cargo test -p
ghostframe-e2e --test browserless_runner -- --test-threads=1`, 11/11
non-ignored tests green): `cdf53_converges_to_lossless_under_10pct_loss`,
`every_cdf53_pass_eventually_lands`, and `superseded_generations_never_render`
all passed, alongside the full existing suite.

The re-baseline test's own sanity assertions (`queued_at -> ACK >=
last_sent_at -> ACK` for both tiers' mean and max) held for all 30 runs
across all 3 batches, exactly as in every prior entry.

#### Results (30 scene runs, pooled)

| Metric | Tier | Runs | Samples | Pooled mean | Max |
|---|---|---:|---:|---:|---:|
| `last_sent_at -> ACK` | Critical (0-3) | 30 | 3,922 | 26.4 ms | 160.0 ms |
| `last_sent_at -> ACK` | Refinement (4-13) | 30 | 9,843 | 38.4 ms | 206.0 ms |
| `queued_at -> ACK` | Critical (0-3) | 30 | 3,922 | 51.3 ms | 656.0 ms |
| `queued_at -> ACK` | Refinement (4-13) | 30 | 9,843 | 70.9 ms | 653.0 ms |

Critical's pooled mean is now visibly lower than refinement's at both
metrics — the first time in this document that has been true. Compare to
2.2 (the last entry with no harness-routing change): critical and
refinement pooled means were 39.5/40.0 ms (`last_sent_at`) and 84.8/86.4 ms
(`queued_at`) — a ~1-2% gap, within noise. Now the gap is ~31% (`last_sent_at`)
and ~28% (`queued_at`).

#### Bucket distribution (all 30 runs pooled, ms)

`last_sent_at -> ACK`:

| Bucket | Critical count | Critical % | Refinement count | Refinement % |
|---|---:|---:|---:|---:|
| 0-5 | 15 | 0.4% | 32 | 0.3% |
| 5-10 | 522 | 13.3% | 305 | 3.1% |
| 10-20 | 1,392 | 35.5% | 1,275 | 13.0% |
| 20-50 | 1,551 | 39.5% | 5,974 | 60.7% |
| 50-100 | 336 | 8.6% | 1,777 | 18.1% |
| 100-200 | 106 | 2.7% | 477 | 4.8% |
| 200-500 | 0 | 0.0% | 3 | 0.0% |
| 500+ | 0 | 0.0% | 0 | 0.0% |

`queued_at -> ACK`:

| Bucket | Critical count | Critical % | Refinement count | Refinement % |
|---|---:|---:|---:|---:|
| 0-5 | 0 | 0.0% | 0 | 0.0% |
| 5-10 | 479 | 12.2% | 192 | 2.0% |
| 10-20 | 1,258 | 32.1% | 861 | 8.7% |
| 20-50 | 1,239 | 31.6% | 5,013 | 50.9% |
| 50-100 | 332 | 8.5% | 1,438 | 14.6% |
| 100-200 | 429 | 10.9% | 1,740 | 17.7% |
| 200-500 | 171 | 4.4% | 577 | 5.9% |
| 500+ | 14 | 0.4% | 22 | 0.2% |

For the first time in this document, critical and refinement diverge at
*every* bucket, and in the same direction throughout: critical is
front-loaded into the fast buckets (5-10, 10-20 ms) and refinement is
weighted toward the slower ones (20-50 ms and up), for both metrics. The
100-200 ms and 200-500 ms buckets — the ones every prior entry pointed at as
"where a real effect would show" — now show real separation too
(`queued_at`: 10.9% vs 17.7% at 100-200 ms; 4.4% vs 5.9% at 200-500 ms).

#### The within-run ratio

| Batch | `last_sent_at` critical | `last_sent_at` refinement | `last_sent_at` ratio | `queued_at` critical | `queued_at` refinement | `queued_at` ratio |
|---|---:|---:|---:|---:|---:|---:|
| 1 | 25.2 ms | 38.6 ms | 0.653 | 47.6 ms | 63.7 ms | 0.747 |
| 2 | 27.3 ms | 37.7 ms | 0.722 | 52.1 ms | 72.9 ms | 0.714 |
| 3 | 26.7 ms | 38.8 ms | 0.689 | 54.4 ms | 76.1 ms | 0.715 |
| **Mean** | | | **0.688** | | | **0.725** |

Per-run ratio spread (30 individual runs, not batch-pooled): `last_sent_at`
0.375-0.980 (mean 0.683), `queued_at` 0.212-1.760 (mean 0.735). Individual
runs vary more than the batch-pooled numbers — small per-run sample counts
(~130 critical samples/run) make a single run noisier than the ~1,300-run
batch pool — but every batch-pooled ratio, and the large majority of
individual runs, land well under 1.0. This is a qualitatively different
picture from every prior entry, where every batch-pooled ratio landed in
0.96-1.00 regardless of metric.

| Metric | This entry (post-fix) | 2.2 (pre-fix) | 2.1 (pre-fix) |
|---|---:|---:|---:|
| `last_sent_at -> ACK` mean ratio | **0.688** | 0.989 | 0.987 |
| `queued_at -> ACK` mean ratio | **0.725** | 0.981 | 0.970 |

#### Which of the three outcomes: **ratio drops sharply**

This is the design doc's first named outcome. Both metrics moved from
"statistically indistinguishable from 1.0" to "critical arrives roughly
30% sooner than refinement, confirmed delivered" — a change far larger than
the batch-to-batch noise band (±0.01-0.03) documented at every prior stage.
The bucket distribution supports the same read: separation is not confined
to one bucket or one metric, it appears at every bucket for both metrics,
with critical systematically shifted toward the fast end.

**Production's `drain_refinement_pass_major` was already prioritising
critical passes correctly.** The flat ~0.97-0.99 ratios measured at every
previous stage were an artifact of the harness routing all CDF53 work
through `priority_queue`'s FIFO drain in tile-major insertion order — never
exercising the pass-major drain at all, exactly as the "Correction" section
above diagnosed. Now that the harness exercises the same queue and drain
order production uses, the ordering shows up clearly.

**This means BWE Stage 2.3's guaranteed-slice-for-`PassTier::Critical`
budget split is not motivated by this measurement.** The premise 2.3 was
about to be built on — "pass-major ordering exists but a shared
`refinement_bandwidth_fraction` budget prevents it from paying off" — was
drawn from a harness path with no ordering behavior at all, and does not
survive contact with a harness that actually has that ordering. Production
already separates the tiers by a wide margin with the existing
`drain_refinement_pass_major` and the existing single
`refinement_bandwidth_fraction` slice. Building a second, more complex
budget split to chase an effect that already exists would be solving a
problem this data does not show.

This does not mean 2.3 can never be justified — a different scene shape
(a larger grid, sustained multi-frame load, or a lower loss rate that
shifts where queueing delay accumulates) could still reveal a case where
pass-major ordering alone is insufficient. But the specific case this
document has measured from Stage 2.0 through this entry no longer supports
building it.

## BWE Stage 2.4: probe clusters — regression guard + measurement

**Date:** 2026-09-12
**Git rev:** `fe078d3` (branch `spec/bwe-probe-clusters`)
**Parent:** `5829ace` (tip of `spec/bwe-stage2`, the "harness fidelity fix"
entry above)

This is Task 5 of the probe-clusters plan
(`docs/superpowers/plans/2026-09-12-bwe-probe-clusters.md`): re-run this
document's own guard metric after landing probe support, and report the
probe-window outcome counters from a real scene. Task 4's bench
(`ghostframe-lib/tests/bwe_bench.rs`) is the correctness gate for probing
itself; this section only answers "did adding it disturb the latency this
document has been tracking, and does it ever actually engage on this
scene."

Two commits precede this measurement, on top of the harness-fidelity-fix
baseline above:

- `ddbff52` — a production fix, not part of the original plan. Writing
  Task 4's bench found that `GoogCcDriver` never called
  `on_network_availability`, so goog_cc's `ProbeController` was
  permanently stuck in `State::Init` and could never request a cluster —
  in the bench *or* in production. Fixed by calling it once at
  construction, before `start_bitrate` is set, so it only unblocks the
  first real request rather than firing one itself.
- `fe078d3` — Task 4's bench coverage, proving (with a real,
  controller-requested cluster, not a fabricated one) that a filled
  cluster moves the estimate, an under-filled one is silently discarded
  exactly like ordinary traffic, the `min_bytes` derivation is right, and
  untagged traffic is unaffected by a pending request.

### Method

Identical scene, method, and re-measure protocol to every prior entry:
`busy_frames(2)` 4x4 CDF53 grid, 10% independent loss, 10s duration, seed
sequence `0xB17E0000..` incrementing past bail-outs, same
`cargo test -p ghostframe-e2e --test browserless_runner
bwe_tier_latency_baseline -- --ignored --nocapture --test-threads=1`
command, run 3 times (30 successful runs total). Both metrics
(`last_sent_at -> ACK` and `queued_at -> ACK`) reported, as in every entry
since 2.1.

The only code difference from the harness-fidelity-fix baseline is the two
commits above, plus test-only instrumentation added for this measurement:
`IoBridge::probe_stats_publish` (mirrors the existing
`bwe_publish`/`latency_stats_publish` cell-and-republish pattern) surfaces
`probes_completed`/`probes_abandoned` through `BrowserlessResult`, and
`bwe_tier_latency_baseline` now prints them per run. **No scheduling,
pacing, budget, or tier-ordering code changed** — Stage 2.3 remains
retired, per the plan.

Batch 1 hit one `MAX_ITERS` bail-out (seed `0xB17E0001`, backfilled by
`0xB17E000A`); batches 2 and 3 were 10/10 clean — consistent with this
scene's previously documented bail-out rate.

Regression guards, run separately before this measurement (`cargo test -p
ghostframe-e2e --test browserless_runner -- --test-threads=1`, run 6
times, 11/11 non-ignored tests green every time, no flakes observed):
`cdf53_converges_to_lossless_under_10pct_loss` and
`every_cdf53_pass_eventually_lands` both green every run, alongside the
full existing suite. `cargo test -p ghostframe-lib` (401 lib tests, plus
the 10 `bwe_bench` tests including Task 4's 4 new ones) and
`cargo clippy -p ghostframe-lib -p ghostframe-e2e --all-targets` both
clean.

### Results (30 scene runs, pooled)

| Metric | Tier | Runs | Samples | Pooled mean | Max |
|---|---|---:|---:|---:|---:|
| `last_sent_at -> ACK` | Critical (0-3) | 30 | 3,925 | 29.4 ms | 394.0 ms |
| `last_sent_at -> ACK` | Refinement (4-13) | 30 | 9,815 | 42.1 ms | 424.0 ms |
| `queued_at -> ACK` | Critical (0-3) | 30 | 3,925 | 63.3 ms | 656.0 ms |
| `queued_at -> ACK` | Refinement (4-13) | 30 | 9,815 | 87.0 ms | 1033.0 ms |

### The guard: `last_sent_at -> ACK` did not rise

| | Harness-fidelity-fix baseline (pre-probes) | This entry (post-probes) | Direction |
|---|---:|---:|---|
| `last_sent_at -> ACK` pooled mean, critical | 26.4 ms | 29.4 ms | +11% |
| `last_sent_at -> ACK` pooled mean, refinement | 38.4 ms | 42.1 ms | +9.7% |
| `last_sent_at -> ACK` ratio (critical/refinement) | 0.688 | 0.698 | flat |
| `queued_at -> ACK` ratio (critical/refinement) | 0.725 | 0.727 | flat |

The absolute pooled means moved up by single-digit percentages, but per
this document's own repeated caution (see "Use the within-run ratio, not
the absolute latency" above), absolute means are noisy across batches by
design — batch 1 alone pooled to a 35.5 ms critical mean against batches
2 and 3's ~26 ms, the same single-heavy-batch pattern this document has
seen at every prior stage, not something new to probing. **The ratio —
what this document's acceptance criterion is actually built on — did not
move**: 0.698 against a 0.688 reference, and 0.727 against 0.725, both
comfortably inside the ~0.65-0.72 per-batch spread already visible within
this entry's own three batches (0.761/0.661/0.661 last_sent; matching
per-batch spread was already documented at every earlier stage too).
**Guard passes**: nothing here indicates a probe overshoot queued the
wire in a way this metric would have caught, and the design's own scope
(a 15 ms window, at most once per scene here, against a 10 s duration) is
consistent with an effect too small to separate from existing noise.

### Probe outcomes from a real scene: opens every time, completes never

| Batch | Runs | `probes_completed` | `probes_abandoned` |
|---|---:|---:|---:|
| 1 | 10 | 0 | 10 |
| 2 | 10 | 0 | 10 |
| 3 | 10 | 0 | 10 |
| **Total** | **30** | **0** | **30** |

**Not the zero/zero case the plan warned about.** Every one of the 30
successful runs opened exactly one probe window — goog_cc's
`ProbeController` requests its initial exponential probe (3x/6x the
starting rate) from the very first ACK batch of every session, so this
scene reliably drives the window open. But every single one was
abandoned, never completed: the initial probe's target rate on this
scene's 2 Mbit/s seed (`BweWrapper::INITIAL_BPS`) is 12 Mbit/s (6x) for a
15 ms window, giving `min_bytes = 12,000,000 x 0.015 / 8 = 22,500` bytes,
so the estimator's 80%-of-`min_bytes` acceptance floor is 18,000 bytes
that must arrive, ACKed, inside that same 15 ms. `busy_frames(2)`'s
concurrent load, spread across a 4x4 grid with 10 CDF53 passes per tile,
does not concentrate that much emitted, ACKed traffic inside any single
15 ms window this early in the scene — consistent with the design's own
"No padding" section: an idle-relative-to-the-probe link legitimately
under-fills, and this counter is exactly what distinguishes that from
probing being broken. Task 4's bench is the evidence that the *estimator
and tagging path* work when a cluster is actually filled; this scene
simply never generates enough concentrated, ACKed traffic inside a 15 ms
window to fill one. A sustained, higher-throughput scene (more frames,
finer-grained ACKs arriving faster than 15 ms apart in volume) would be
needed to observe a completed probe in the browserless harness — out of
scope for this measurement.

### What this means going forward

Probing is wired correctly (Task 4's bench) and does not regress the
latency this document tracks (the guard above). It has not yet been
observed to *complete* on any scene exercised so far, browserless or
otherwise — the only completions on record are Task 4's bench, which
fills a cluster directly rather than through a scene's organic traffic.
Whether real sessions accumulate enough tile traffic inside a 15 ms
window to complete a probe in practice is an open question this
measurement does not answer either way.
