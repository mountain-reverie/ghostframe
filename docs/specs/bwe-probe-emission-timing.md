# Probe clusters cannot fill under frame-quantised emission

**Date:** 2026-09-13
**Status:** finding; fix proposed, not implemented
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
