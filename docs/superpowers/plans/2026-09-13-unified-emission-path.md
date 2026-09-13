# One Emission Path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the browserless harness emit through the same code as production, so the two cannot drift.

**Architecture:** The shared budget-and-drain tail becomes `emit_via_scheduler`, called by both `dispatch_dirty_tiles_via_scheduler` and `apply_injected_frame`. `InjectedFrame.budget_bytes` — always `usize::MAX`, the mechanism by which the harness diverges — is deleted.

**Tech Stack:** Rust.

Design: `docs/superpowers/specs/2026-09-13-unified-emission-path-design.md`. Read it first, including the table of four defects this split has already produced.

---

## Split the refactor from the behaviour change

**Task 1 is a pure extraction and must change nothing.** Both callers keep passing exactly what they pass today, including `usize::MAX` on the injected path. Every test stays green, every scene behaves identically.

**Task 2 is the behaviour change**, and it is one line of deletion plus its consequences.

Doing them together would make "a scene moved" ambiguous between "the extraction was wrong" and "the harness now emits realistically". Kept apart, Task 1 failing means a bad refactor and Task 2 failing means a real finding.

## Landmarks

Re-derive these; they drift.

| Symbol | Line |
|---|---|
| `injected_continuation_after_drain` | ~1767 |
| `dispatch_dirty_tiles_via_scheduler` | ~1816 |
| `drain_scheduler_into_quinn` | ~2092 |
| `apply_injected_frame` | ~2313 |
| `resume_scheduler_continuation` | ~2523 |
| `InjectedFrame.budget_bytes` | ~473 |

---

## Task 1: Extract `emit_via_scheduler`

**Files:** Modify `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Identify the duplicated tail**

Both paths contain this, identical apart from one argument:

```rust
let pre_clamp_budget = match &self.active_probe {
    Some(probe) => pacer_tick_budget_bytes(probe.target_rate_bps, SCHEDULER_TICK_INTERVAL_US),
    None => combine_pacing_budget(
        pacing_mode,
        /* aimd_budget | inj.budget_bytes */,
        bwe_snap.pacer_rate_bps,
        SCHEDULER_TICK_INTERVAL_US,
    ),
};
self.clamp_to_quinn_capacity(pre_clamp_budget)
```

followed by the `drain_scheduler_into_quinn` call and the continuation
bookkeeping. Read both in full before extracting — the continuation handling
differs in shape (`injected_continuation_after_drain` vs the inline version),
and that difference needs a decision rather than a silent merge.

- [ ] **Step 2: Write the shared function**

```rust
/// The one emission tail: probe override -> pacing combine -> quinn clamp ->
/// drain -> continuation bookkeeping.
///
/// Both the capture path and the harness injection path call this, so they
/// cannot drift. Anything that must differ between them belongs *before*
/// this call — in how work is produced and enqueued — never in how it is
/// emitted.
///
/// Takes `base_budget_bytes` and nothing that identifies the caller. If this
/// function ever needs to know who called it, the extraction has failed and
/// the split has been rebuilt one level down.
fn emit_via_scheduler(
    &mut self,
    seq: u32,
    timestamp_us: u32,
    max_frag: usize,
    base_budget_bytes: usize,
) -> (SchedulerStats, usize, usize)
```

Return whatever the two call sites need; match `drain_scheduler_into_quinn`'s
existing tuple if that is sufficient.

**The AIMD `tick_budget_multiplier` stays outside.** It is fed by observed
send errors, which only the dispatch path has. Apply it to
`base_budget_bytes` at that call site, not inside the shared function. That
asymmetry is real and must not become a branch.

- [ ] **Step 3: Call it from both paths**

`dispatch_dirty_tiles_via_scheduler` passes its AIMD-modulated budget.
`apply_injected_frame` passes `inj.budget_bytes` — **still `usize::MAX` for
now**. Task 2 changes that.

`SchedulerEmissionPolicy::CpuRawOnly` bypasses budgeting with `usize::MAX` on
the dispatch path; preserve that exactly.

- [ ] **Step 4: Verify nothing moved**

```bash
cd /home/cedric/work/ghostframe
cargo test -p ghostframe-lib 2>&1 | grep 'test result' | tail -4
cargo test -p ghostframe-e2e --test browserless_runner -- --test-threads=1 2>&1 | grep 'test result'
cargo clippy -p ghostframe-lib -p ghostframe-e2e --all-targets 2>&1 | tail -3
cargo fmt -p ghostframe-lib -p ghostframe-e2e
```

**Every test must pass unchanged.** This task is behaviour-preserving; a
moved scene means the extraction is wrong, not that the harness improved.

Also confirm the duplication is actually gone:

```bash
grep -c 'let pre_clamp_budget = match &self.active_probe' \
  ghostframe-lib/src/transport/io_bridge.rs
```

Expected: **1**, down from 2.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "refactor(io_bridge): one emission tail for both paths

The capture path and the harness injection path carried the same
budget-and-drain logic written twice, differing in one argument. Four
Stage 2 defects came from feeding the two copies different inputs.

Pure extraction: both callers pass what they passed before, including
usize::MAX on the injected path. Behaviour is unchanged."
```

## Task 2: Delete `InjectedFrame.budget_bytes`

**Files:** Modify `ghostframe-lib/src/transport/io_bridge.rs`, `ghostframe-e2e/src/harness/browserless.rs`

This is the behaviour change.

- [ ] **Step 1: Confirm it is never varied**

```bash
grep -rn 'budget_bytes' ghostframe-e2e/src/ ghostframe-e2e/tests/
```

Expected: `usize::MAX` at `browserless.rs:~734` and `~763`, and nothing else.
**If any scene sets a different value, STOP and report** — the field would
then be a real knob and this plan needs revising.

- [ ] **Step 2: Remove the field and derive the budget**

`apply_injected_frame` should compute `base_budget_bytes` the way the dispatch
path does, rather than accepting one from the caller.

The dispatch path derives it from `adaptation_context.bytes_per_us x
SCHEDULER_TICK_INTERVAL_US x SCHEDULER_TICK_BUDGET_FRACTION`, floored at
`SCHEDULER_TICK_BUDGET_FLOOR_BYTES`. Reuse that derivation; extract it into a
helper if that keeps both sites honest.

**Do not** substitute a hardcoded constant for `usize::MAX`. That would keep
the harness on a private budget and preserve the divergence in a new form —
the point is that it uses production's derivation.

- [ ] **Step 3: Run everything, repeatedly**

```bash
cargo test -p ghostframe-lib 2>&1 | grep 'test result' | tail -4
for i in 1 2 3 4 5; do
  cargo test -p ghostframe-e2e --test browserless_runner -- --test-threads=1 2>&1 | grep 'test result'
done
```

The harness is not seed-reproducible and this repo has documented contention
flakes, so a single run proves little.

**Scenes may fail. That is expected and is a finding.** A scene that only
passed because emission was unbounded was not testing what it claimed. For
each failure, report which scene, what it asserted, and why unbounded
emission was load-bearing for it — then fix the scene with that reason
recorded.

**Never restore the unbounded budget to make a scene pass.** If a scene
cannot be made to work, stop and report rather than reverting the change
underneath it.

The two most likely to move are `cdf53_converges_to_lossless_under_10pct_loss`
and `every_cdf53_pass_eventually_lands` — both depend on everything
eventually draining, which is exactly what stops being instant.

- [ ] **Step 4: Commit**

## Task 3: Re-baseline

**Files:** Modify `docs/specs/bwe-tier-latency-baseline.md`

- [ ] **Step 1: Re-measure**

Follow that document's own re-measure instructions exactly — the `#[ignore]`d
`bwe_tier_latency_baseline` test, `busy_frames(2)`, 4x4 CDF53, 10% loss,
several batches. A differently-produced number is not comparable.

- [ ] **Step 2: Report against the guard**

**`last_sent_at -> ACK` must not rise.** Backlog that previously drained
instantly will now queue, which is precisely the condition this metric exists
to detect. Reference: critical 26.4 ms, refinement 38.4 ms, ratio 0.688-0.698.

Report the within-run critical/refinement **ratio** as the headline — absolute
means swing ~2.9x across batches while the ratio swings ~1.02x.

- [ ] **Step 3: Report probe counters**

`probes_completed` / `probes_abandoned` from a scene run.

With real backlog a probe window may now open against a non-empty queue,
which is the condition `drain_for_probe_window_open` was built for and could
never meet before. **If completions appear, say so plainly** — it would let
PR #78's caveat be replaced with evidence.

If they stay at zero, report that too. It is a side effect, not the goal, and
a null result here does not diminish the refactor.

- [ ] **Step 4: Append and commit**

---

## Done criteria

- [ ] `grep -c 'let pre_clamp_budget = match &self.active_probe'` returns **1**.
- [ ] `InjectedFrame.budget_bytes` no longer exists.
- [ ] `emit_via_scheduler` takes no caller-identifying argument.
- [ ] `cargo test`, clippy, `fmt` clean.
- [ ] All browserless scenes pass across repeated runs; any that were changed
      have their reason recorded.
- [ ] `last_sent_at -> ACK` did not rise.
- [ ] Tier-latency baseline re-measured and appended.
- [ ] Probe counters reported either way.

## What this plan does not do

- Change capture or encode. The harness should keep faking those.
- Touch tier ordering — Stage 2.3 is retired.
- Add padding, concurrent probe clusters, or read `pad_window`.
- Change `NetProfile`. Scenes needing bandwidth limits use its token bucket,
  which is the correct layer.
