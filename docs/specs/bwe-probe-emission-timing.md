# Probe clusters cannot fill under frame-quantised emission

**Date:** 2026-09-13
**Status:** fix implemented (`drain_for_probe_window_open`, `io_bridge.rs`).
See "Outcome" below.
**Context:** BWE Stage 2.4 (PR #77) landed probe clusters. Scene runs showed
`probes_completed = 0`, `probes_abandoned = 30` across 30 runs. This is the
investigation into why, and it is not the reason the PR assumed.

## The measurement

Running the baseline scene with `io_bridge=debug`:

```
bwe: probe cluster window opened     probe_id=2 target_rate_bps=12000000
                                     duration_ms=15 min_probes=5 min_bytes=22500
bwe: probe cluster window abandoned  probe_id=2 packets_sent=0 min_probes=5
                                     bytes_sent=0 min_bytes=22500
```

**`packets_sent=0`.** Not under-filled — nothing was tagged at all. The window
opened and closed with no emission inside it.

PR #77 recorded the abandonment as the designed consequence of the no-padding
decision: an idle link legitimately under-fills. That explanation is wrong for
this data. An under-filled probe has `0 < bytes_sent < min_bytes`. Zero means
no emission opportunity ever fell inside the window.

## The cause

Emission is **frame-quantised**. Tiles leave only when a scheduler tick or a
frame injection runs, and a probe window is shorter than the gap between them:

| Emission driver | Interval | Opportunities per 15 ms window |
|---|---:|---:|
| Production scheduler tick (`SCHEDULER_TICK_INTERVAL_US`) | 33.3 ms | **0.45** |
| Browserless injection (`FRAME_SPACING_US`) | 16.0 ms | **0.94** |

goog_cc's initial exponential probes request a 15 ms `target_duration`. Under
`start_paused` the virtual clock jumps timer-to-timer, so a window can open
and close between two emission events with nothing in between — which is
exactly what the zero shows.

**This is not a harness artifact.** Production is strictly worse: a 33.3 ms
tick against a 15 ms window means more than half of all probe windows contain
no emission opportunity at all, and the rest contain exactly one.

So probe clusters as shipped in #77 cannot reliably fill **in production
either**. The plumbing is correct — the bench proves a consumed cluster moves
the estimate 1,000,000 -> 4,655,000 bps — but nothing on the live path fills a
window.

## Why the bench passes and the scene does not

The bench drives `GoogCcDriver` directly, feeding tagged `AckArrival` records
without going through the scheduler. It proves the estimator accepts a
correctly-tagged cluster, which is a real and necessary result. It cannot
detect that the emission path never produces one, because it does not use the
emission path.

That is a fair division of labour — it is precisely why the plan also required
a scene measurement — but it means the bench's green cannot stand in for
end-to-end evidence. Reading them together is what surfaced this.

## Proposed fix: emit when the window opens

A pacer should send on its own schedule, not only when a frame arrives.
libwebrtc's paces in ~5 ms bursts for this reason; ours emits once per frame
tick.

The bounded version: when `poll_probe_window` opens a window, **drain the
scheduler once immediately** with the probe budget, rather than waiting for the
next tick or injection. That guarantees at least one emission inside every
window, which is the minimum for a cluster to have any chance of filling.

It stays inside the existing structure — the same
`drain_scheduler_into_quinn` call, the same probe budget, and
`clamp_to_quinn_capacity` still last. It does not introduce sub-frame pacing
generally; it adds one emission at the one moment that needs it.

**`min_probes = 5` still has to be met**, so a single drain must emit at least
five tagged datagrams. With ~500 B CDF53 passes and a 22,500 B `min_bytes`,
that is ~45 passes — well within one tick's probe budget (12 Mbit/s x 33.3 ms
= 50 KB) provided the queue holds the work. So a warm queue is still required;
this fix removes the structural blocker, not the demand requirement.

## What would prove it

- A browserless scene reaching `probes_completed >= 1` **reliably**, not once.
- The existing negative control still moving `probes_abandoned` on a scene
  that should not fill — otherwise the positive assertion can be satisfied by
  accident.
- The `last_sent_at -> ACK` guard unchanged. An extra immediate drain is a
  plausible way to *cause* wire queueing, which is the failure this metric
  exists to catch. Reference: critical 26.4 ms, refinement 38.4 ms, ratio
  0.688-0.698.

## If the fix is declined

Then probe clusters should be recorded as **structurally inert on the live
path** rather than left looking functional. The counters make that visible —
`probes_abandoned` climbing with `probes_completed` at zero is an accurate
signal — but a reader seeing the feature merged would reasonably assume it
works. It does not, and the reason is emission granularity rather than
anything in the probe code itself.

## Outcome (2026-09-13)

Implemented as designed: `IoBridge::drain_for_probe_window_open`, called
once from `poll_probe_window`'s open branch, reusing the most recently
drained frame's identity (`IoBridge::last_drain_frame_context`, recorded
unconditionally at the top of `drain_scheduler_into_quinn` regardless of
which of its three call sites triggered it). With no prior drain on record
the method is a no-op — no fabricated `seq`, matching the original design.
`clamp_to_quinn_capacity` still runs last.

### Mechanism, proven directly

Two `ghostframe-lib` unit tests exercise `drain_for_probe_window_open`
without going through goog_cc, quinn, or the browserless harness at all —
just a real `IoBridge`, a real `Scheduler`, and a hand-built `ActiveProbe`:

- `drain_for_probe_window_open_fills_from_existing_backlog`: seeds
  `last_drain_frame_context` via one zero-budget drain, queues six `Solid`
  tiles directly into the scheduler, opens a probe, and confirms the
  immediate drain tags >= 5 datagrams and advances `bytes_sent`, with the
  queued work visibly flipping `Pending -> InFlight`.
- `drain_for_probe_window_open_is_a_noop_without_a_prior_drain`: same
  setup, but skips the seeding drain. Confirms zero emission and that the
  queued backlog is left completely untouched — proving the skip path
  doesn't quietly consume work it isn't allowed to tag.

Both pass deterministically. The fix's mechanism is real: given a valid
frame identity and a non-empty queue, it fills.

### End-to-end: harder to isolate than expected

The original `bwe_tier_latency_baseline` scene (`busy_frames(2)`, 4x4 grid,
10% loss) — the exact scene that produced this doc's `packets_sent=0`
evidence — **still shows `probes_completed=0` after the fix**, 0/10 and
10/10 abandoned across two 10-run batches. Direct instrumentation of
`drain_for_probe_window_open` confirmed why: at the instant this scene's one
early probe window opens, the scheduler queue is genuinely empty
(`drained_bytes=0, drained_count=0` on every single run) — the frame's own
dispatch, running with an effectively unbounded quinn send buffer under the
browserless harness's near-zero-RTT socketpair, already drained everything
before the window had a chance to observe any backlog. This is the "demand
requirement" the design flagged as separate from the structural blocker: the
fix cannot fill a window from a queue that has nothing in it, and this
specific scene's queue has nothing in it by the time the window opens. This
scene now serves as the regression's **negative control**
(`probe_windows_are_abandoned_on_a_demand_starved_link`).

A busier scene (`busy_frames_grid(8, 8, 8)`: 8 frames, 8x8 CDF53 grid) does
complete probe windows reliably — 7-8 of 10 seeds per batch, aggregate
`probes_completed >= 1` across three separate 10-run batches (7, 7, and 8
completions respectively; see
`probe_windows_can_complete_on_a_busy_link`). But a control run with
`drain_for_probe_window_open`'s call site disabled produced statistically
indistinguishable results on the *same* scene (7/10). On this harness, a
heavy backlog triggers a rapid burst of ordinary
`Event::DatagramsUnblocked` -> `resume_scheduler_continuation` activity that
independently lands emissions inside the 15 ms window, regardless of this
fix. The near-zero simulated RTT (`start_paused`, socketpair, no real
network delay) makes it easy for that burst to coincidentally cover a
window and hard to construct a scene where it reliably does not — unlike
production, where quinn's per-tick AIMD budget and a real network genuinely
bound how fast backlog drains.

Net: the browserless harness cannot cleanly attribute a completion to this
fix specifically versus the pre-existing continuation path, because its
near-zero latency defeats the exact throttling that makes the fix necessary
in production. The unit tests above are the load-bearing proof that the
mechanism itself is correct; the e2e tests demonstrate the outcome-level
acceptance criterion (a real session *can* complete a probe cluster, and a
demand-starved one still correctly abandons without padding), which is what
was asked for, with this attribution caveat recorded rather than hidden.

### Guard metric: `last_sent_at -> ACK` did not rise

Re-ran the `bwe_tier_latency_baseline` measurement twice (10 successful runs
each) after the fix:

| Run | Critical mean | Refinement mean |
|---|---:|---:|
| Reference (pre-fix baseline, this doc) | 26.4 ms | 38.4 ms |
| Post-fix, batch 1 | 29.0 ms | 39.5 ms |
| Post-fix, batch 2 | 27.8 ms | 39.2 ms |

Both post-fix batches land within the batch's own seed-to-seed noise band
(per-seed critical means ranged 10.1-74.0 ms within a *single* batch — see
`feedback_browserless_not_seed_reproducible`), so this is not read as a rise
attributable to the fix. No systematic increase in wire queueing was
observed.

### Verification

- `cargo test -p ghostframe-lib`: 403 passed (401 pre-existing + 2 new),
  0 failed.
- `cargo test -p ghostframe-e2e --test browserless_runner -- --test-threads=1`:
  13 passed, 1 ignored (the measurement test), run three times consecutively
  with no flakes. `cdf53_converges_to_lossless_under_10pct_loss` and
  `every_cdf53_pass_eventually_lands` stayed green throughout.
- `cargo clippy -p ghostframe-lib -p ghostframe-e2e --all-targets`: clean.

## Attribution experiment (2026-09-13): the fix fires, the queue is empty

PR #78 shipped with the caveat that the browserless harness could not
attribute `drain_for_probe_window_open` — the scene completed probes with the
fix disabled. The suspected reason was the harness's near-zero-RTT socketpair
making `DatagramsUnblocked` a free emission opportunity.

**That was not the reason.** Re-running the probe scene on a realistic link
(`delay_us: 10_000`, `CapTimeline::constant(20_000_000)` — both already
supported by `NetSim`, both previously unused by the probe scenes) with the
drain instrumented directly:

```
EXPPROBE: drain_for_probe_window_open entered
          pre_clamp_budget=49999 effective_budget=49999
          queued_priority=0 queued_refinement=0
EXPPROBE: drain result  drained_bytes=0 drained_count=0
```

The fix **fires correctly**, with a 49,999-byte budget against a 22,500-byte
`min_bytes` — more than sufficient, and unclamped. It finds **both scheduler
queues empty**.

### Why the queue is empty: a third harness-fidelity gap

Three facts compose:

1. `PacingMode` only reaches `Paced` at `BWE_PACED_MODE_SAMPLE_THRESHOLD`
   = **150 ACK samples**.
2. Below that, `combine_pacing_budget` returns the AIMD budget unchanged
   (`_ => aimd_budget_bytes`).
3. The browserless harness passes **`budget_bytes: usize::MAX`**
   (`browserless.rs:734`, `:763`).

So until 150 samples accumulate, every injection drains the **entire** queue —
there is no backlog by construction. And goog_cc's initial exponential probe
fires from the **first ACK batch**, far below that threshold.

The probe therefore always opens against an empty queue, and no link profile
can change that: the emptiness comes from the harness's unlimited injection
budget, not from the network.

This is the same family as the `enqueue_at`/`refinement_queue` routing gap:
**the harness does not reproduce the production emission path**, and
conclusions drawn from it about production were wrong in the same way.
Production's frames go through `dispatch_dirty_tiles_via_scheduler` with a
real AIMD budget from the first frame, so production *does* build backlog.

### What this means for the fix

`drain_for_probe_window_open` is **correct and necessary**, and now has
direct evidence of firing with a healthy budget — stronger than PR #78's
unit-test-only attribution. What it cannot do is manufacture work that was
never queued.

### The remaining step

Make the harness inject with a **realistic budget** instead of `usize::MAX`,
so backlog accumulates the way it does in production. That is a harness
change, not a production one, and it is the prerequisite for any end-to-end
probe-completion evidence.

Until then, probe completion cannot be demonstrated end to end for a reason
that has nothing to do with probes.


## Realistic budget landed, and RTT with it (2026-09-13)

"The remaining step" above — a realistic injection budget — landed in PR #79,
which deleted `InjectedFrame.budget_bytes` and made `apply_injected_frame`
derive its budget the way the dispatch path does. Probes now complete end to
end: **7 of 10 seeds** on the 8x8 CDF53 scene, against 0 of 30 before Stage
2.4. The negative control still shows 0 completed / 5 abandoned, so the scene
discriminates on demand rather than passing everything.

PR #79 did not itself cause that. Measured on `origin/master` alone and on
master+#79, the busy scene gives identical seed-by-seed results (7/10, the
same three failures). The completions come from #78's drain; #79 neither
helps nor hurts them, which is expected — closing the divergence class was
its goal and probe completion was only a hoped-for side effect.

### The near-zero-RTT explanation was right, and was a harness defect

PR #78 disclaimed its own scene: disabling `drain_for_probe_window_open`
changed nothing, because "the harness's near-zero-RTT socketpair lets
`DatagramsUnblocked` continuation bursts supply emission opportunities inside
a window that production — 33.3ms ticks, real RTT — would not."

That reasoning was correct, and pointed at something fixable: `NetProfile`
has a `delay_us` knob no browserless scene had ever set. Setting it did not
produce a slower link. It produced no link at all:

| one-way delay | outcome |
|---|---|
| 0 (every scene) | 11.0s |
| 1ms | 11.7s |
| 5ms | 28.7s |
| 10ms | no progress in 400s |
| 20ms | no progress in 15+ min, 0.2% CPU |

Delivery was awaited at the point of sending, serialising the link to one
datagram in flight at a time. The client->server transmit drain checks
neither `MAX_ITERS` nor `overall_deadline`, so a busy scene burned
`backlog x delay_us` of virtual time inside one outer iteration with no
bail-out reachable — the loop counter froze at iteration 200 while virtual
time advanced exactly one datagram per 10ms. It also made `reorder_us`
inert: with arrivals serialised, `at_us` could only increase, so no datagram
could overtake another.

This is a **fourth** harness-fidelity gap of the same family, and the largest
in scope: BWE and pacing — subsystems whose entire purpose is reacting to
path conditions — had never been exercised against a path with any
propagation delay at all. Fixed by queueing decided datagrams in an
`InFlight` heap and delivering them from the scene loop when due.

### The fix is still not attributable in this harness

With real RTT available, the experiment PR #78 could not run, run:

| one-way delay | `drain_for_probe_window_open` | completed | abandoned |
|---|---|---|---|
| 0 | on | 30 | 0 |
| 0 | **off** | 29 | 1 |
| 20ms | on | 30 | 0 |
| 20ms | **off** | 30 | 1 |
| 40ms | on | 10 | 0 |
| 40ms | **off** | 10 | 1 |

Completions never collapse. The only difference is a single abandoned window
without the fix — and it appears at **zero delay too**, so it is not the
RTT-dependent effect the hypothesis predicted. One event across 30 runs, in a
harness documented as not seed-reproducible, is noise-scale.

**PR #78's disclaimer stands, unchanged.** Attribution lives in its two unit
tests, which call the method directly and fail if it stops working. The
scene asserts the narrower thing it always asserted: a real session can
complete a probe cluster at all.

Recording this as a null result rather than tuning the scene until the fix
looks load-bearing. The RTT work's value is the fidelity gap it closes for
BWE generally, not probe attribution.

### One number moved at zero delay

The probe scene went from 7/10 completions to 10/10 at `delay_us: 0` after
the delivery-queue change, with all scenes still passing. `deliver_s2c` now
lands at the top of the loop rather than inside the `select!` arm, which
shifts interleaving relative to event-draining and injection. The earlier
7/10 figures were themselves an artifact of serialised delivery.

## Answered (2026-09-14): abandonment is not what limits headroom discovery

The question this document has carried since PR #78 — whether probe
abandonment matters, and therefore what `drain_for_probe_window_open` is worth
— now has a measured answer. It took a harness that could model a congested
link, which is the work in
`docs/superpowers/specs/2026-09-13-production-like-load-design.md`.

### What changed in the harness

**Production cadence.** The harness injected every 16 ms where production
dispatches every 33.3 ms. Injection cadence is now per-scene and defaults to
production's. Measured blast radius across the suite: one scene.

**A bottleneck that queues.** `NetProfile.cap` was a token bucket that dropped
whatever it could not afford and never delayed anything — a lossy link, not a
congested one. Since goog_cc estimates from delay first, that left its
delay-based half inert and only the loss-based fallback running, which pins the
estimate to `MIN_BPS` whatever the capacity. `NetProfile.bottleneck` now models
a real bottleneck: queue first, tail-drop only when full, with optional CoDel.
Presets cover fibre-behind-fq_codel, consumer WiFi, and LTE bufferbloat.

**Sustained load.** A scripted scene injects its frames and then goes quiet,
sending empty heartbeats for the rest of its duration, while production's
capture loop free-runs. `SceneLoad::Profile` generates frames for the whole
scene.

### The answer

With a queueing bottleneck at production cadence, **probe clusters complete**:
1/1 in every run of the step-up scene, and 1/2 after the probe-ladder fix. At
production cadence over the old dropping cap they never completed at all.

And the estimate still ramps slowly: post-step maxima of 378k, 391k, 335k and
2,112k bits/s against 3.2 Mbps of new capacity, with the final sample equal to
the maximum in every run — still climbing when the scene ends at 12 s.

**So probing works here, and the slow ramp is not explained by abandonment.**
That bounds what `drain_for_probe_window_open` can be worth: it increases the
number of windows that fill, and filling more windows is not what the session
is short of. PR #78's decision to keep attribution in unit tests rather than
claim it from a scene remains the right call.

What *is* short is how often goog_cc asks to probe at a rate the link can
answer. See `docs/specs/bwe-googcc-review.md` for that, together with two
defects found while reviewing our use of the controller — a hardcoded
`data_in_flight` of zero, and a probe ladder truncated to its top rung — and
for the slow-start overshoot dynamic that explains the shape of all these
measurements.

### A caveat on the cadence table above

The table in "Realistic budget landed, and RTT with it" was measured over the
old dropping cap. Its conclusion stands — production cadence completes no
probes there — but the mechanism is now known to be two compounding things,
not one: the cadence, and a bottleneck that produced no queuing delay for the
controller to read.
