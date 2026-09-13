# BWE Stage 2.4 — Probe Clusters

**Status:** proposed
**Refines** `2026-09-12-bwe-stage2-design.md` section 2.4, which treated this
as "consume outputs we already generate". That is true of `pacer_config`
(Stage 2.2) and **not** true of probes. This spells out why, and what the work
actually is.

## Why probes at all

goog_cc raises its estimate by observing that more data got through. Without
probing it can only learn from traffic the encoder happens to produce, so
after a capacity *increase* it ramps at AIMD's pace rather than discovering
the headroom directly. A probe deliberately sends a short burst above the
current estimate and measures what arrives.

`ProbeController` already decides when to probe and emits
`NetworkControlUpdate::probe_cluster_configs`. We currently discard them.

## What makes this bigger than reading a config

Three facts, each verified against `goog_cc-0.1.4`.

**1. A probe packet must be tagged, or it is not a probe.**
`probe_bitrate_estimator.rs:88` asserts `cluster_id != NOT_APROBE`. Identity
travels on `SentPacket.pacing_info: PacedPacketInfo`, whose fields are
`probe_cluster_id`, `probe_cluster_min_probes`, `probe_cluster_min_bytes`,
`probe_cluster_bytes_sent`. Our driver builds `SentPacket { ..Default }`, so
`probe_cluster_id` is always `-1` today.

**2. Sends are reported at ACK time, not send time.**
`googcc.rs:181` constructs `SentPacket` inside the ACK-processing loop,
synthesised from an `AckArrival` record, and calls `on_sent_packet`
immediately before `on_transport_packets_feedback`. So probe identity cannot
be attached when the packet is sent — it has to **travel with the pass** from
emit to ACK. That is exactly the plumbing Stage 2.1 built for `queued_at`:
`CacheEntry` -> `BweSample` -> `AckArrival`.

**3. An under-filled cluster is silently discarded.**
`probe_bitrate_estimator.rs:137`:

```rust
if cluster.num_probes < min_probes || cluster.size_total < min_size { /* drop */ }
```

So sending *some* extra bytes is not enough. A cluster that does not reach
both thresholds contributes nothing, and nothing fails — the exact
"silently ignored cluster costs the ramp with nothing failing" hazard the
Stage 1 spec warned about.

## Design

### Probe window state

One active cluster at a time, held in `IoBridge`:

```rust
struct ActiveProbe {
    id: i32,
    /// Target rate for the burst; the budget override for ticks inside the
    /// window.
    target_rate_bps: u64,
    /// Wall-clock end of the window, from `at_time + target_duration`.
    ends_at: Instant,
    /// Thresholds copied onto every packet tagged for this cluster, so the
    /// estimator can apply its own min-probes/min-bytes gate.
    min_probes: i64,
    min_bytes: i64,
    /// Cumulative bytes tagged so far — goog_cc's
    /// `probe_cluster_bytes_sent`.
    bytes_sent: i64,
}
```

`min_probes` comes from the config's `target_probe_count`; `min_bytes` from
`target_data_rate x target_duration`. goog_cc exposes no helper for this
derivation — `PacedPacketInfo::new` takes both as plain arguments — so the
derivation is ours and must be stated in a comment, not left implicit.

A new config **replaces** any active probe rather than queueing. Overlapping
clusters would interleave their packets and corrupt both measurements, and
`ProbeController` does not expect concurrent clusters.

### Budget during the window

Inside the window the per-tick budget becomes the probe's target rate rather
than `min(aimd, googcc)` — that is the point of a probe, to send *above* the
current estimate.

`clamp_to_quinn_capacity` still runs last and still wins. A probe that would
exceed what quinn can absorb is not worth dropping tiles for, and the clamp
exists because `scheduler.tick` is destructive.

### No padding — and the consequence

Per the Stage 2 design, a probe drains **queued work faster** rather than
generating padding. Real tile data is wanted anyway; padding spends capacity
to measure capacity.

**The consequence must be designed for, not discovered.** If the queues do
not hold `min_bytes` of work, the cluster under-fills and the estimator
discards it. That is *correct* — an empty queue means no demand, so there is
nothing to probe for — but it means probes only complete under load, and a
naive implementation would look broken on an idle link.

So: count tagged bytes, and when a window closes having missed either
threshold, **record it as an abandoned probe** rather than letting it vanish.
A counter for completed and abandoned clusters is what distinguishes "probing
works, the link was idle" from "probing is broken".

### The tagging path

Mirrors `queued_at` from Stage 2.1:

| Stage | Carries |
|---|---|
| emit (`submit_one`) | stamp active probe id + thresholds + running `bytes_sent` onto `CacheEntry` |
| ACK (`dispatch_ack_datagram`) | read them off the cache entry into `BweSample` |
| drain (`run()`) | copy onto `AckArrival` |
| driver (`googcc.rs`) | build `PacedPacketInfo` instead of `..Default::default()` |

Passes emitted outside any window keep `NOT_APROBE` (-1) and behave exactly
as today.

## Testing

**Tier-1 bench is the gate.** It is deterministic and a pure function of its
inputs, unlike the browserless harness.

- A cluster is requested, tagged packets are emitted, and the estimator
  **consumes** it — asserted by the estimate moving, not merely by the config
  being read. "Emitted and consumed" is the phrasing the Stage 1 spec used,
  and the distinction is the whole point.
- An **under-filled** cluster is discarded and counted as abandoned. Without
  this the first case could pass while every real probe silently fails.
- Packets outside a window carry `NOT_APROBE` and are unaffected.

**Browserless:** the existing guards must stay green —
`cdf53_converges_to_lossless_under_10pct_loss` (a probe that stalls emission)
and `every_cdf53_pass_eventually_lands` (a probe that starves refinement).

**Guard metric:** `last_sent_at -> ACK` must not rise. A probe deliberately
overshoots the estimate, so it is the most plausible way to *cause* wire
queueing. Baseline after Stage 2.2 and the harness fix: 26.4 ms critical,
38.4 ms refinement, ratio 0.688.

## Risks

| Risk | Mitigation |
|---|---|
| Cluster requested, never consumed — the ramp silently costs nothing | Bench asserts the estimate moves; abandoned-probe counter |
| Probe overshoot causes the queueing it measures | `clamp_to_quinn_capacity` still last; `last_sent_at -> ACK` guard |
| Under-fill looks like breakage on an idle link | Abandoned counter distinguishes the two, by design not by inference |
| Overlapping clusters corrupt both | One active probe; a new config replaces it |
| `min_bytes` derivation is wrong | Derived by us, not goog_cc; stated in a comment and asserted in the bench |

## Scope

**In:** probe window, budget override, tagging path, counters, bench coverage.

**Out:** padding (deliberate, see above). Multiple concurrent clusters.
`pad_window`, still unread since Stage 2.2. Any change to tier ordering —
Stage 2.3 is retired, since the harness fidelity fix showed production
already prioritises correctly.

## Honest assessment

This is the largest remaining Stage 2 piece and the one with the least
certain payoff. Stage 2.2 already put the estimate in charge and reduced
measured latency; probes only improve how fast it *ramps* after a capacity
increase, and only fire under load.

It is worth building if ramp speed on a recovering link matters. If it does
not, deferring is defensible and this document is the record of what it
would take.
