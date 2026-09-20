# How we drive goog_cc, and what that has cost

**Date:** 2026-09-14
**Scope:** a full review of `ghostframe-lib/src/transport/bwe/` and its callers,
prompted by BWE findings that were about to be written up on the strength of
inference rather than verification.

The review was worth doing on its own terms: **three of the conclusions we were
about to publish were wrong**, and two real defects turned up that nothing was
failing on. Both are fixed.

---

## What is correct

Verified by reading the adapter against goog_cc 0.1.4's own source, and by
experiment where reading was not enough.

- **`feedback_only: true`** is right for this system. We feed only
  `TransportPacketsFeedback` — there is no REMB channel and no independent
  RTT/loss report — so the controller must derive RTT and loss itself.
- **`on_network_availability` is called**, and the comment explaining why its
  returned update is discarded is accurate: `start_bitrate` is still zero at
  that point, so the call cannot yield a probe cluster. Without it
  `ProbeController` would sit in `State::Init` forever.
- **The client's 16-bit wrapped arrival clock is handled correctly.**
  `Lo16Timeline` unwraps it, and epoch offset between the two clocks cancels in
  goog_cc's inter-arrival deltas, so the delay-based estimator is fed properly
  despite the two ends having unrelated epochs.
- **Pacing genuinely binds.** Forcing the pacer rate to 10 kbps cut delivered
  bytes 3.65x, so `combine_pacing_budget` is not a no-op. This had to be tested
  because circumstantial evidence suggested otherwise — see "measurement
  traps".

## Fixed: `data_in_flight` was hardcoded to zero

`TransportPacketsFeedback::data_in_flight` was `DataSize::from_bytes(0)` on
every call. goog_cc's congestion-window pushback controller reads that field,
so it had nothing to act on and silently never engaged.

The retransmit cache already *is* the in-flight set — an entry lives there from
emission until its ACK, cancellation, or supersession — so
`RetransmitCache::bytes_outstanding` now supplies the real number.

**Measured effect: essentially none.** The estimate moved 200,000 -> 206,620 in
one run and was unchanged in another, with delivered and dropped bytes flat.
Correct now, and pinned by a test, but not what governs these scenes.

## Fixed: the probe ladder was being truncated

goog_cc's exponential probing does not request *a* cluster, it requests a
**ladder**. The driver kept only `probe_cluster_configs.last()`, on the
reasoning that a config never surfaced is indistinguishable from one never
requested. That holds for a single config and fails for the set.

Measured on the step-up scene: the first request of a session is a pair at
**6 Mbps and 12 Mbps**, and keeping the last discarded the 6 Mbps rung — the
one more likely to be answerable on a slow link. Three clusters were requested
across the scene; one was silently dropped.

Now queued in a bounded `VecDeque` and surfaced **oldest-first**, so the lower
rung is tried before the higher.

| | before | after |
|---|---|---|
| probe windows opened (step-up scene) | 1 | 2 |
| tail estimate | ~203 kbps | ~221 kbps |

Real, and small: both initial rungs target far above a 480 kbps link and fail
either way. The probe that actually moves the estimate is a later 1.7 Mbps
request.

## Open: `target_rate` never limits the source

goog_cc reports two rates. `target_rate` is what libwebrtc hands the **encoder**
as its output budget; the pacer rate, 2.5x higher, exists to drain a queue
quickly without letting the *average* exceed target.

This project consumes only the pacer rate. `bitrate_bps` is used for logging
and one test assertion, nothing else. There is no encoder to hold at target,
and the scheduler always has queued refinement work, so whatever the budget
allows, it fills.

A source-rate token bucket refilled at `target_rate` was implemented and
measured twice. It works — a budget trace shows it binding hard once the
estimate falls (`pre_clamp=825` against tokens exhausted) and not binding while
the estimate sits at its seed. It is also, today, a **net regression**:

| RTT-guard scene (40 ms RTT, uncapped) | delivered |
|---|---|
| no source bucket (current behaviour) | 752,637 |
| source bucket, before the ladder fix | 439,445 |
| source bucket, after the ladder fix | 548,602 |
| the scene's floor for "not serialised" | 600,000 |

Pinning the source to `target_rate` caps throughput at the *estimate*, which is
only safe if the estimate reaches capacity quickly. Ours takes 5-7 s (see
`the_estimate_follows_a_mid_scene_capacity_step_up`). The ladder fix improved
it measurably and not enough.

**This is blocked on the ramp, not on the bucket.** Two goog_cc knobs were
tried and rejected: `enable_repeated_initial_probing` and
`requests_alr_probing` both default to `None`, and enabling them changed
nothing — still one completed probe, same ramp. ALR probing requires the sender
to be application-limited, and ours saturates.

The next lever is the `ProbeController` state machine's periodic paths
(`network_state_interval`, `est_lower_than_network_ratio`), which want a
`NetworkStateEstimate` we never supply.

## The dynamic all of this explains

From a budget trace of the congested scene: **489 KB of its ~768 KB of server
emission happens in the first ~100 ticks, while the estimate is still at its
2 Mbps seed** — roughly 1.08 Mbps offered into a 480 kbps link.

1. The session starts at the 2 Mbps seed and emits accordingly.
2. The bottleneck queue fills and overflows; drops follow.
3. The loss-based half of the controller backs the estimate off to `MIN_BPS`
   (200 kbps) and holds it there.
4. Recovery is slow, because the probe that would discover headroom fires
   rarely.

That is slow-start overshoot followed by over-correction. It is not an
estimator ignoring capacity, and the estimate sitting at its floor under
sustained congestion is plausibly *correct* behaviour given a sender that keeps
the link full.

## Measurement traps found along the way

Recorded because each cost real time and would cost it again.

- **`CapTimeline` is in bytes per second**, despite the parameter being named
  `bps` and `BweSnapshot::bitrate_bps` being *bits*. Three measurement rounds
  were run against caps 8x larger than intended before this surfaced.
- **`bytes_delivered` conflated both directions.** A run with the pacer forced
  to 10 kbps appeared to deliver 130 kbps, which looked like emission bypassing
  the budget and was the client's ACK stream. Now split into
  `bytes_delivered_s2c` / `bytes_delivered_c2s`.
- **Retransmits are not an emission bypass.** Disabling them entirely (18,726
  attempts -> 203) left server emission unchanged at 588 KB. The hypothesis was
  plausible and wrong; tested rather than assumed.
- **Fixed time windows are not safe for reading the estimate.** How long it
  sits at its seed before congestion is detected varies run to run, so a
  `[4s, 5s)` "before" window sometimes averaged the seed instead of the
  converged floor, making a test flaky 1 run in 3.
- **A threshold inside its own success distribution is a coin flip, not a
  gate.** `the_estimate_follows_a_mid_scene_capacity_step_up` failed ~50% of
  runs because its 80%-of-capacity bar (12.8 Mbps) sat inside the *passing*
  population: with the capacity hint the final estimate measures 12.0-18.2
  Mbps, and without it 7.0-7.2 Mbps. The fix was to measure both populations
  and put the bar in the gap (60%, 9.6 Mbps) — a weaker-sounding number that
  separates strictly better. Before tuning a flaky threshold, measure what
  the two sides of it actually look like; the answer is often that the
  threshold was never separating them.
- **An acceptance bound longer than the observation window is unobservable.**
  The same test ran a 20 s scene stepping at 4 s — 16 s of recovery to watch
  — against an 18 s convergence bound. No run could ever exercise the
  (16 s, 18 s] part of the range it claimed to admit.

## Claims retracted

Stated here because they were nearly written into a spec as findings.

- ~~"The estimator does not track capacity."~~ Too strong. It separates a
  congested link from a spacious one by 10x, and it does recover after a
  capacity step-up.
- ~~"Probe results are registered momentarily and then discarded."~~ Wrong —
  a misread of a sparsely sampled series. One run reached 2.1 Mbps and held it.
- ~~"Capping the ACK path depresses the estimate."~~ Refuted by experiment:
  identical results with the return path capped and uncapped.
