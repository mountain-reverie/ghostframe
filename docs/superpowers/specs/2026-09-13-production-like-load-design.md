# Production-like load in the browserless harness — Design

**Status:** proposed
**Depends on:** PR #80 (propagation delay in the browserless harness)

## Why

Measuring the probe path turned up a fifth harness-fidelity gap, and unlike
the first four it inflates a success metric rather than breaking a test.

**The harness injects every 16 ms; production dispatches every 33.3 ms**
(`FRAME_SPACING_US` vs `SCHEDULER_TICK_INTERVAL_US`). Measured on the 8x8
CDF53 probe scene, 10 seeds per cell:

| cadence | RTT | `drain_for_probe_window_open` | completed | abandoned |
|---|---|---|---|---|
| 16 ms (harness) | 0 | on | 10 | 0 |
| 16 ms (harness) | 0 | off | 10 | 0 |
| **33.3 ms (production)** | 0 | on | **0** | 10 |
| **33.3 ms (production)** | 0 | off | **0** | 10 |
| 33.3 ms | 40 ms | on | 2 | 8 |
| 33.3 ms | 40 ms | off | 4 | 6 |

At production cadence this scene never completes a probe cluster, with or
without the fix. Every probe-completion number this project has reported —
PR #78's, PR #79's, and the ones measured while landing #80 — came from a
regime roughly 2x more favourable than production.

That is **not yet a claim about production**, because the scene is
unrepresentative in a second way that compounds with cadence: it injects 8
frames and then goes quiet, sending empty 100 ms heartbeats for the
remaining ~97% of its duration, while production's capture loop free-runs.
At 33.3 ms those 8 frames spread over 266 ms instead of 128 ms, halving the
offered rate. So the table above may be measuring offered-rate sensitivity
rather than cadence granularity. Separating those two is the point of this
work.

## The assertion is wrong, not just the scene

`probes_completed` is a mechanism detail. What matters is whether the
bandwidth estimate finds the true capacity, and how quickly. If the estimate
tracks capacity while every probe is abandoned, abandonment is benign — and
ghostframe deliberately sends no padding, so under-fill on a quiet link is
designed behaviour, not a fault. If the estimate does *not* track capacity,
we have a concrete failure to fix and the drain fix finally has a test that
can attribute it.

So the flagship assertion becomes convergence against a known capacity, with
probe counters demoted to diagnostics.

## Constraints discovered while scoping this

**`process_frame_cpu` emits `Codec::Raw` only** — no classifier, no wavelet
(the comment in `process_frame` says so explicitly). Feeding synthetic pixels
through the CPU capture path in browserless would therefore destroy the
Critical/Refinement pass structure, which is precisely what the tier-latency
baseline and the probe-backlog questions measure. Full capture-path fidelity
needs a GPU, which browserless does not have.

**The two capabilities needed live in different harnesses:**

| | network control | real codecs |
|---|---|---|
| browserless | full `NetProfile` + token bucket | no — Raw only |
| Docker e2e (VKMS/Weston) | `GHOSTFRAME_*_LOSS_PROBABILITY` only | yes |

**What injection actually bypasses is narrower than it looks.** After PR #79
unified the emission tail, injection and dispatch share everything from the
scheduler down. The remaining divergence is classification and encode —
choosing a codec per tile from real pixels. For BWE, which cares about byte
volume and ACK timing rather than which codec produced the bytes, that is
probably not the binding gap.

Two invariants to respect: `grid_cols`/`grid_rows` are fixed at construction
because `Scheduler::resize` is never called mid-scene, and heartbeats exist
to keep the RTO wheel swept once frames run out.

## Design

### 1. Keep scripted scenes, add generated ones

`BrowserlessScene.frames` is an explicit `Vec<FrameScript>` — right for small
deterministic scenes, unworkable for the hundreds of frames a sustained-load
scene needs. So add a second source rather than changing the first:

```rust
pub enum SceneLoad {
    /// Today's explicit script. Every existing scene keeps this.
    Script(Vec<FrameScript>),
    /// Generated across the scene's whole duration.
    Profile(LoadProfile),
}

pub struct LoadProfile {
    /// Interval between injections. Defaults to production's 33.3 ms.
    pub cadence_us: u64,
    pub churn: Churn,
}

pub enum Churn {
    /// Every tile rewritten each tick — today's `busy_frames_grid` shape.
    FullGrid,
    /// A damage rectangle that walks the grid, mimicking a dragged window
    /// or a scrolling pane. The realistic one: production dirties regions,
    /// not uniform grids, which changes both byte volume and which tiles
    /// reach the refinement queue.
    Region { tiles_per_tick: usize },
    /// Nothing changes. Exercises the idle path and ALR probing.
    Idle,
}
```

A `Profile` scene never runs out of frames, so it may not need heartbeats at
all. Verify rather than assume — the RTO wheel still has to be swept.

### 2. Cadence default moves to 33.3 ms

Measured blast radius across the full suite: **exactly 2 scenes of 15**, both
of them the probe scenes already known to be cadence-sensitive. The other 13
are unaffected.

| scene | 16 ms | 33.3 ms |
|---|---|---|
| `probe_windows_can_complete_on_a_busy_link` | ok | FAILED |
| `probe_windows_are_abandoned_on_a_demand_starved_link` | FAILED | ok |
| other 13 | unchanged | unchanged |

The two scenes bracket the real behaviour, and production's cadence sits on
the pessimistic side. Both will need rewriting under this design anyway,
since both assert on probe counters.

### 3. Convergence assertions

- `CapTimeline` supplies a known, piecewise-constant capacity.
- Offered load is set above the cap, so backlog genuinely exists.
- Assert `bwe_estimate_bps` settles within tolerance of the cap.
- **Step-up:** cap rises mid-scene; assert the estimate follows, and bound
  how long it takes. This is the scenario probing exists for and the one no
  test covers today.
- Probe counters printed alongside as diagnostics.

## Risks

| Risk | Mitigation |
|---|---|
| Runtime: hundreds of generated frames per seed, on a capped link that drains slowly | Measure with 1 seed before designing tolerances; fall back to a smaller grid, `Region` churn, or shorter duration |
| Variance: the harness is not seed-reproducible | Multi-seed medians, not single runs; tolerance sized from measured spread |
| A convergence tolerance wide enough to never flake may be too wide to catch anything | State the tolerance's discriminating power explicitly: what regression would it catch? Verify by breaking the estimator on purpose |
| Assertions that pass for the wrong reason | Every new assertion gets an induced-failure check before it is trusted |

## Out of scope

**Porting impairment into the Docker e2e harness** (tc netem, or a
datagram-layer shim like netsim's) so the real capture -> classifier -> GPU
codec path runs under controlled conditions. That is the genuine
full-fidelity answer and needs its own design.

**An independent pacing clock.** `pacer_rate_bps` is only ever converted into
a per-dispatch byte cap, never a send schedule — there is no timer that
drains on its own. That is a real design gap, but it does not explain the
measurements above: `min_bytes = target_rate x 15 ms` while one drain is
budgeted at `target_rate x 33.3 ms`, ~2.2x what the window needs. The
abandonment measured here is backlog starvation, not budget or timing.
