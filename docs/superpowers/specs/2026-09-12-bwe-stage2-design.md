# BWE Stage 2 — The Controller Takes Charge

**Status:** proposed
**Follows** `2026-09-10-bwe-pacing-design.md` (Stage 1 landed in PR #62;
Stage 2.0 in #72; the metric correction in #73).

## Where things actually stand

Three findings from reading the emission path, each of which narrows this
work. They are recorded in `docs/specs/bwe-tier-latency-baseline.md`.

**1. The estimate steers nothing.** `self.bwe` is fed ACK arrivals and
publishes a snapshot. No code reads its output to control emission. That was
Stage 1's explicit design — "the controller observes and reports but does not
steer" — and it is the gap Stage 2 closes.

**2. The budget comes from quinn, with AIMD on top.** Per tick
(`SCHEDULER_TICK_INTERVAL_US`, 33.3 ms at 30 fps) the budget is
`adaptation_context.bytes_per_us × interval × 0.90`, floored at 256 KB,
multiplied by an AIMD term (backoff 0.5 on send error, ramp 1.1 per clean
frame, clamped to [0.05, 1.0]), and separately capped at 80% of quinn's
`send_buffer_space()`.

**3. Tier ordering already exists.** `drain_refinement_pass_major` takes the
lowest `pass_idx` in the queue, emits every tile at that pass, then moves on.
Passes 0-3 already leave before 4-13.

And from #73: **the Stage 2.0 metric cannot see (3)**, because
`last_sent_at -> ACK` starts when a pass leaves. The interval prioritisation
shortens is `queued_at -> ACK`, and `queued_at` lives on `TileWork` and never
reaches `CacheEntry`.

## goog_cc already produces what we would otherwise write

`NetworkControlUpdate` has four fields. `GoogCcDriver::absorb` reads one:

```rust
pub struct NetworkControlUpdate {
    pub congestion_window: Option<DataSize>,
    pub pacer_config: Option<PacerConfig>,             // discarded
    pub probe_cluster_configs: Vec<ProbeClusterConfig>, // discarded
    pub target_rate: Option<TargetTransferRate>,        // used
}
```

`pacer_config` is the pacing rate the spec's "pacer restructure" would
otherwise have to invent. `probe_cluster_configs` is the "probe mode",
already computed, with `target_data_rate`, `target_duration`,
`min_probe_delta` and `target_probe_count`.

So Stage 2 is mostly **consuming outputs we already generate and throw away**,
not building new control logic.

## Goals

- The bandwidth estimate governs emission.
- Probe clusters are emitted and consumed, so the estimate can find headroom.
- The tier latency ratio (baseline 0.99) drops materially below 1.0, without
  refinement throughput regressing.

## Non-goals

- Replacing quinn's send-buffer cap. It prevents `SendDatagramError::Blocked`,
  which silently drops work the scheduler already popped. It stays.
- Rewriting `Scheduler`. Two queues, a split budget and a pass-major drain
  already exist and are tested.
- Changing the client. This is server-side emission.

## Sequencing — measure before building

The order matters more than usual here, because finding (3) means **we do not
yet know whether the tier ordering needs changing at all.** The metric that
would tell us does not exist. Building a tier split first would be building
against a guess.

### 2.1 — Plumb `queued_at`, and re-baseline

Carry `TileWork::queued_at` into `CacheEntry`, and record `queued_at -> ACK`
per tier alongside the existing `last_sent_at -> ACK`.

Then re-measure. **This measurement decides what 2.3 does**, and it has three
possible outcomes, all informative:

- **Ratio already below 1.0.** Pass-major ordering is working and the spec's
  premise is partly wrong. 2.3 becomes small or unnecessary; say so and stop.
- **Ratio still ~1.0.** Ordering is not translating into delivery latency —
  most likely because the refinement budget slice (`refinement_bandwidth_fraction`,
  5-20%) is small enough that critical passes queue behind refinement work
  from *earlier frames*. 2.3 addresses the budget split, not the ordering.
- **Ratio above 1.0.** Critical passes are arriving *later*. That would be a
  real defect, and worth stopping to understand before any pacer work.

Keep both metrics permanently. `last_sent_at -> ACK` is what reveals a pacer
overfilling the link and causing wire queueing — a real way for 2.2 to go
wrong, and invisible to `queued_at -> ACK`.

### 2.2 — `PacingMode`, and the estimate in charge

```rust
pub enum PacingMode {
    /// Today's behaviour: quinn path stats with AIMD. The fallback when the
    /// estimator has not converged.
    PathStats,
    /// goog_cc's `pacer_config` governs the per-tick budget.
    Paced,
}
```

**The budget becomes `min(quinn_cap, googcc_pace)`, not a replacement.** They
constrain different things: quinn's `send_buffer_space()` is a hard local
limit whose violation drops tiles; goog_cc's pacing rate is a network
estimate. Taking the minimum respects both, and means a wrong estimate
degrades throughput rather than dropping work.

`PacingMode::PathStats` remains the startup mode and the fallback when
`samples_seen` is too low to trust — the estimator reports its seed value when
unfed, which is indistinguishable from a real estimate without that check.
Stage 1 added `bwe_samples_seen` for exactly this reason.

### 2.3 — Tier budget, only if 2.1 says so

Deliberately unspecified until 2.1 reports. If the ratio stays at 1.0, the
likely change is a guaranteed slice of the tick budget for `PassTier::Critical`
rather than having all CDF53 passes share `refinement_bandwidth_fraction`.

Writing that design now would be guessing at a number 2.1 is about to measure.

### 2.4 — Probe clusters

Consume `probe_cluster_configs`. A probe needs to put *more* bytes on the wire
than steady state, to see whether the link takes them.

**Prefer draining queued work faster over sending padding.** The queues
usually hold refinement passes that are wanted anyway; a probe that ships real
tile data costs nothing extra, whereas padding spends capacity to measure
capacity. When the queue is empty there is no demand, so declining to probe is
correct rather than a missed opportunity.

The spec's Stage 1 acceptance already warns that a silently ignored cluster
costs the ramp with nothing failing — so the tier-1 bench must assert clusters
are **emitted and consumed**, not merely requested.

## Acceptance

From `bwe-tier-latency-baseline.md`, and deliberately not absolute latency —
the absolute critical mean swings 2.92x across batches while the within-run
ratio swings 1.019x.

- **Primary:** the `queued_at -> ACK` critical/refinement ratio drops
  materially below its 2.1 baseline, reproducibly across batches.
- **Guard:** `bytes_emitted_refinement` does not regress, and
  `every_cdf53_pass_eventually_lands` stays green. **A falling ratio with flat
  critical latency is a regression wearing a success** — it means refinement
  got worse rather than critical getting better.
- **Guard:** `last_sent_at -> ACK` does not rise. That would mean the pacer is
  overfilling the link.
- **Regression:** `cdf53_converges_to_lossless_under_10pct_loss` and the
  existing convergence scenes stay green.
- Probe clusters observed emitted *and* consumed in the tier-1 bench.

## Risks

| Risk | Mitigation |
|---|---|
| We build a tier split that was never needed | 2.1 measures before 2.3 designs |
| goog_cc's estimate is wrong and starves the link | Budget is `min(quinn, googcc)`; `PathStats` fallback below a sample threshold |
| Pacing causes the wire queueing it should prevent | `last_sent_at -> ACK` is kept as a guard metric |
| A probe cluster is requested and silently dropped | Bench asserts emitted *and* consumed |
| The ratio improves by degrading refinement | Throughput and starvation guards, stated as acceptance not commentary |

## Testing

- **Tier-1 bench** (`bwe_bench.rs`): deterministic, synthetic bottleneck, pure
  function of its inputs. Extends to probe emission/consumption and the
  `min(quinn, googcc)` budget.
- **Browserless scenes:** the ratio, measured over batches. The harness is not
  seed-reproducible, so single runs are not evidence.
- **Existing scenes as regression guards:** `every_cdf53_pass_eventually_lands`
  for starvation, `cdf53_converges_to_lossless_under_10pct_loss` for a pacer
  that stalls, `retransmits_fire_under_loss_but_not_on_a_perfect_link` for the
  retransmit path the priority queues touch.
