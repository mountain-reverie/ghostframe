# One Emission Path — Design

**Status:** proposed
**Motivation:** four harness-fidelity defects in Stage 2, all from the same
split. This removes the split rather than patching the next symptom.

## The pattern

`IoBridge` has two paths that put tiles on the wire:

- `dispatch_dirty_tiles_via_scheduler` — production. Capture produces dirty
  tiles, they are enqueued, a budget is derived, the scheduler drains.
- `apply_injected_frame` — the browserless harness. Pre-encoded `TileWork`
  arrives, is enqueued, a budget is derived, the scheduler drains.

The front halves genuinely differ: faking capture and encode in a test is the
whole point of the harness. **The back halves are the same operation, written
twice.**

Both currently carry this, character for character apart from one argument:

```rust
let pre_clamp_budget = match &self.active_probe {
    Some(probe) => pacer_tick_budget_bytes(probe.target_rate_bps, SCHEDULER_TICK_INTERVAL_US),
    None => combine_pacing_budget(
        pacing_mode,
        /* aimd_budget   on the dispatch path */
        /* inj.budget_bytes on the injected path */
        bwe_snap.pacer_rate_bps,
        SCHEDULER_TICK_INTERVAL_US,
    ),
};
self.clamp_to_quinn_capacity(pre_clamp_budget)
```

## What the split has cost

Every one of these was found by measurement, after a wrong conclusion had
already been drawn and acted on:

| # | Defect | Consequence |
|---|---|---|
| 1 | Harness enqueued CDF53 via `enqueue_at` (`priority_queue`, FIFO) instead of `enqueue_refinement_at` (`refinement_queue`, pass-major) | Every tier-latency measurement ran on a path with the *inverse* of production's ordering. Retired Stage 2.3, which had been scoped against it. |
| 2 | Stage 2.2 had to add `combine_pacing_budget` at both sites | Nearly shipped applying it to one, which would have left the harness blind to the change being measured. |
| 3 | Stage 2.4 had to add the probe override at both sites | Same shape. |
| 4 | Harness injects `budget_bytes: usize::MAX` | Queue fully drains each injection, so probe windows always open on an empty queue. Blocks end-to-end probe evidence entirely. |

Three of the four produced a *published, wrong* conclusion about production
behaviour before being caught. The split does not merely risk drift — it has
generated it consistently.

## Design

Extract the shared tail:

```rust
/// The one emission tail. Probe override -> pacing combine -> quinn clamp ->
/// drain -> continuation bookkeeping.
///
/// Both the capture path and the harness injection path call this, so they
/// cannot drift. Anything that must differ between them belongs *before*
/// this call, in how the work is produced and enqueued — never in how it is
/// emitted.
fn emit_via_scheduler(
    &mut self,
    seq: u32,
    timestamp_us: u32,
    max_frag: usize,
    base_budget_bytes: usize,
) -> (SchedulerStats, usize, usize)
```

`dispatch_dirty_tiles_via_scheduler` passes its AIMD-modulated budget.
`apply_injected_frame` passes the same thing.

### Delete `InjectedFrame.budget_bytes`

It is `usize::MAX` at both of its only two call sites
(`browserless.rs:734`, `:763`). No scene has ever set anything else, so it is
not a knob anyone uses — it is the mechanism by which the harness diverges.

Replacing it with a *realistic constant* would be the smaller change and the
wrong one: the duplication would survive, and the next divergence would be
free to happen. **Deleting the field is what closes the class.**

Scenes that need to constrain bandwidth already do so properly through
`NetProfile`'s token-bucket `cap` — `a_tighter_cap_sheds_more_traffic` is the
existing example. That is the right layer for it: it constrains the *link*,
not the emitter's private idea of a budget.

### What stays different

Only the front half, and deliberately:

- the harness injects pre-encoded `TileWork` rather than running capture/encode
- `apply_injected_frame` emits frame dimensions first, mirroring
  `process_frame_cpu`/`process_frame_gpu`, because it bypasses the capture
  path that would otherwise do it
- the AIMD `tick_budget_multiplier` is fed by observed send errors on the
  dispatch path; the injected path has no equivalent feedback signal

The third is a real asymmetry, not an oversight. It should be passed *into*
`emit_via_scheduler` as part of `base_budget_bytes` rather than branched on
inside it — the shared function should not know which caller it has.

## Expected consequences

**Scene timing changes.** The harness stops draining unboundedly, so backlog
accumulates as in production. Every browserless scene's emission timing
shifts. This needs a full re-baseline, not a spot check.

**Some scenes may need adjusting**, and that is a finding rather than a
nuisance: a scene that only passed because emission was unbounded was not
testing what it claimed. Any such scene should be fixed with its reason
recorded, never by restoring the unbounded budget.

**Probe completion may become demonstrable.** With real backlog, a probe
window opening mid-queue has work to drain — which is the condition
`drain_for_probe_window_open` was built for and could never meet in the
harness. That would let PR #78's caveat be replaced with evidence. It is a
likely side effect, not the goal; the goal is that defects 1–4 cannot recur.

## Testing

- **Every browserless scene re-run repeatedly**, not once — the harness is not
  seed-reproducible and this repo has documented contention flakes.
- **Full tier-latency re-baseline** per `docs/specs/bwe-tier-latency-baseline.md`,
  appended with the new numbers. The ratio is the stable figure; absolute
  means swing ~2.9x across batches.
- **`last_sent_at -> ACK` must not rise.** Backlog that previously drained
  instantly will now queue, which is exactly the condition this metric exists
  to detect. Reference: critical 26.4 ms, refinement 38.4 ms, ratio 0.688-0.698.
- **The regression guards must stay green**:
  `cdf53_converges_to_lossless_under_10pct_loss` (emission stalling) and
  `every_cdf53_pass_eventually_lands` (starvation). These are the two most
  likely to move, since both depend on everything eventually draining.

## Risks

| Risk | Mitigation |
|---|---|
| Scenes fail because they relied on unbounded emission | Expected; fix each with its reason recorded, never by restoring the knob |
| Re-baseline conflates this change with Stage 2.2/2.4 | Re-baseline immediately before and after on the same commit range |
| The shared function grows caller-specific branches | It takes `base_budget_bytes` and nothing caller-identifying; a `match` on caller inside it means the extraction failed |
| Backlog causes the queueing the pacer prevents | `last_sent_at -> ACK` guard |

## Out of scope

Tier ordering (Stage 2.3 is retired). Padding. Concurrent probe clusters.
The `pad_window` field, unread since Stage 2.2. Capture and encode — the
harness should keep faking those.
