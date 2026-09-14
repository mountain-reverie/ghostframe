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

## Claims retracted

Stated here because they were nearly written into a spec as findings.

- ~~"The estimator does not track capacity."~~ Too strong. It separates a
  congested link from a spacious one by 10x, and it does recover after a
  capacity step-up.
- ~~"Probe results are registered momentarily and then discarded."~~ Wrong —
  a misread of a sparsely sampled series. One run reached 2.1 Mbps and held it.
- ~~"Capping the ACK path depresses the estimate."~~ Refuted by experiment:
  identical results with the return path capped and uncapped.

---

## Attempt (2026-09-14): fixing the ramp via periodic probing — and why it fails

The open gap above is blocked on the ramp, and the ramp is blocked on goog_cc
rarely asking to probe at a rate the link can answer. This is what happened
when that was attacked directly. **Nothing from this attempt was kept**; it is
recorded because the negative results are load-bearing for whatever is tried
next.

### What was tried

`ProbeController` has a periodic probing path — `time_for_network_state_probe`
— that fires whenever the estimate sits below a known link capacity. It is
disabled twice over by default: `network_state_interval` is
`TimeDelta::plus_infinity()`, and it requires a `NetworkStateEstimate` that
nothing supplies. Both are reachable through
`NetworkControllerConfig::field_trials.probing_configuration`.

Both were set: a finite 2 s interval, and a `link_capacity_upper` measured from
delivered bytes over a 1 s window — an observation, not a prediction.

### It works, and it does not help

Probing frequency tripled and stayed there: **completed clusters went from 1 to
3 per scene**, reproducibly across four runs.

The ramp did not move. Tail estimates were 277k, 223k, 215k, 207k against a
baseline of 223k, 231k, 210k, 208k — noise. Post-step maxima were comparable.

And it costs: on the congested scene, loss rose from 33,839 to 37,305 bytes
(~10%) from the extra probe traffic, with the estimate unchanged at its floor.
Measurable cost, no measurable benefit, so it was reverted.

### The circularity, confirmed by measurement

Retrying the source-rate bucket *together with* periodic probing — on the
theory that the two gaps were mutually blocking and might only close together
— produced **548,610 bytes on the RTT guard, against 548,602 without probing**.
Identical.

The reason was written into the signal's own doc comment before it was tested,
and the measurement confirmed it. With the source bucket on, emission is
limited to the estimate, so observed throughput collapses onto the estimate,
so `estimated_bitrate < link_capacity_upper` is never true, so no probe ever
fires. **A capacity signal derived from our own throughput cannot drive a ramp
whose purpose is to raise the limit on that throughput.**

### What this leaves

The two gaps are genuinely coupled:

- The source cannot be held at `target_rate` until the estimate reaches
  capacity quickly.
- The estimate cannot reach capacity quickly while sustained loss from
  emitting above capacity suppresses it.

Breaking that needs a capacity signal **independent of our own emission**.
Candidates not yet explored:

- **quinn's own congestion controller.** It runs Cubic or BBR over the same
  path with an independent algorithm, and `PathStats` is already read for RTT.
  If it exposes a bandwidth or congestion-window figure, that is a genuinely
  independent second opinion.
- **goog_cc's internal link-capacity tracker**, which `stable_target_rate`
  already reflects (`min(link_capacity_estimate, pushback_target_rate)`) — it
  is derived from probe results rather than from offered load.
- **A real `NetworkStateEstimator`**, which is what libwebrtc supplies here and
  is a substantial component in its own right.

The first is the cheapest to test and the most likely to break the circle.

## Root cause of the slow ramp (2026-09-14): goog_cc rejects most of our probes

The previous section left the ramp blocked on "goog_cc rarely asks to probe at
a rate the link can answer". That framing was wrong in an informative way. It
asks often enough once told to. **It throws away most of the answers.**

### Making goog_cc say why

`ProbeBitrateEstimator::handle_probe_and_estimate_bitrate` logs its reason for
discarding a cluster, at debug level. Installing a `tracing` subscriber in the
step-up scene (env-gated, `GF_TRACE_GOOGCC=1 RUST_LOG=goog_cc=debug`) turns the
whole question into one command:

```
 23  Probing successful
 66  Probing unsuccessful, invalid send/receive interval
 32  Probing unsuccessful, receive/send ratio too high
```

**80% of probe measurements are discarded.** That is why more probing did not
help: the clusters complete by our accounting, and goog_cc then refuses to
derive a bitrate from them, so its estimate never adopts what they found.

### Two causes, one fixable

**Send timestamps were quantised to milliseconds.** The driver did
`Timestamp::from_millis(server_emit_us / 1_000)` while holding microsecond
precision. goog_cc rejects any cluster whose `last_send - first_send` is zero,
and a probe burst drained in one scheduler tick lands inside a single
millisecond. This is not an artifact of the virtual clock — a burst takes well
under a millisecond on a real clock too.

Feeding microseconds instead moved the numbers but did not fix the problem:
invalid-interval rejections fell 66 -> 40, "ratio too high" rose 32 -> 46, and
net successes fell 23 -> 15. Not shipped, because it is not a clean win on its
own.

**Arrival timestamps are millisecond-quantised on the wire, and cannot be
fixed locally.** `ghostframe-protocol`'s ACK frame carries
`arrival_time_ms_lo16: u16` — milliseconds. goog_cc computes
`receive_rate = size / (last_receive - first_receive)`. A 15 ms probe cluster
whose packets arrive within 1-2 ms therefore has 50-100% error in its receive
rate, which is what trips the `receive/send ratio` guard.

That is the binding constraint: **probe bitrate estimation needs sub-
millisecond arrival timestamps, and the protocol provides milliseconds.**
Widening that field is a wire-format change, and it is the prerequisite for
any of this working — not more probing, not a better capacity signal.

### What was tried and reverted on the way

| change | probes | ramp | verdict |
|---|---|---|---|
| periodic network-state probing, fed by observed throughput | 1 -> 3 | unchanged | +10% loss, reverted |
| same, fed by quinn's `cwnd/rtt` | 1 -> 3 | unchanged | reverted |
| microsecond send timestamps | 3 -> 5 | unchanged | ambiguous, reverted |

### quinn's congestion controller is a good capacity signal

Worth recording even though the change was reverted, because it is reusable.
`PathStats` exposes `cwnd` and `rtt`, and `cwnd * 8 / rtt` tracked real
capacity closely — where goog_cc's own estimate did not:

| scene | true capacity | quinn `cwnd/rtt` | goog_cc estimate |
|---|---|---|---|
| congested | 480 kbps | 580-630 kbps | 200,000 (its floor) |
| spacious | 3.2 Mbps | 3.15-3.32 Mbps | 2,012,571 (its seed) |
| uncapped | — | grows to 44-62 Mbps | ~2.0 Mbps |

It is also genuinely independent in the way the throughput signal was not: it
grows with successful delivery, and when the sender is application-limited it
falls back to roughly `initial_window / rtt` rather than collapsing onto the
estimate. If a capacity signal is ever needed again, this is the one to use.
