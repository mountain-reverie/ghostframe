# BWE and Pacing (Protocol Redesign Phase 2) — Design

**Date:** 2026-09-10
**Status:** Approved design
**Author:** Claude (design synthesis); review by Cedric
**Supersedes:** the controller choice in `2026-06-27-protocol-redesign-design.md`

## Problem

Emission is governed by a per-frame AIMD byte budget drained from a single FIFO
queue (`reliable_emitter/emission_queue.rs`). There is no pacing: a frame's
work is pushed to quinn as fast as the budget allows, in bursts with no
inter-packet spacing. There is no real congestion controller — Phase 1 shipped
an EWMA delivery-rate estimator (`transport/bwe.rs`) that observes but does not
steer.

AIMD-on-frame-budget is coarser than per-packet pacing, and burst emission is
what produces the `Blocked` storms and first-paint latency spikes the protocol
redesign set out to fix.

## Why this supersedes the previous controller decision

`2026-06-27-protocol-redesign-design.md` selected str0m's `bwe::Bwe` (a GoogCC
port), estimating "1-2 days" to integrate. **That was not achievable and is
still not.** str0m exposes only `pub struct Bwe<'a>(/* private fields */)` — a
borrowed handle on an `Rtc` session, with no public constructor and no
`poll_estimate()`. Verified against both 0.21 (which Phase 1 hit) and 0.23.1.
The standalone GoogCC implementation exists inside str0m but is `pub(crate)`.

Phase 1 discovered this mid-implementation and worked around it with the EWMA
estimator. This design does not repeat that: the replacement controller was
verified working **before** the design was written (see Evidence).

## Goals

- A real congestion controller driving emission rate, not observing it.
- Per-packet pacing with inter-packet spacing, replacing burst drain.
- Priority emission so base-layer passes are not queued behind refinement.
- Validated against the netsim harness under deterministic loss, reorder and
  bandwidth caps.

## Non-goals

- Adaptive FEC and per-pass NACK suppression — that is Phase 3.
- Changing RTO behaviour.
- Any wire-format change (see D3).
- Replacing the `pass_tier` 0-3 / 4-13 split; it is reused as-is.

## Decisions

| # | Decision | Rationale |
|---|---|---|
| D1 | Controller: the `goog_cc` crate (`GoogCcNetworkController`) | Standalone-constructible GoogCC port, BSD-3-Clause, 10 transitive deps. Verified working before adoption. Delay-gradient behaviour suits the WAN case, which is the hard target. |
| D2 | Probe clusters are in scope, not deferred | Without them the estimate crawls to roughly half of capacity. Measured: 1.58 Mbps vs 3.32 Mbps on a 3 Mbps link. |
| D3 | No wire-format change | GoogCC runs send-side. The server knows its own send times (retransmit cache, per `EmitKey`); the ACK envelope already carries `arrival_time_ms_lo16`. A constant clock offset cancels in `recv_delta - send_delta`. The Phase 1 plan's server send-timestamp item is **not needed** for this design. |
| D4 | The pacer replaces `EmissionQueue`, inside `reliable_emitter` | Keeps wire-seq stamping, RTO caching and parity scheduling untouched. One module changes. |
| D5 | CWND caps staging; pacing rate governs release | Matches libwebrtc. `NetworkControlUpdate` supplies both, so CWND is no longer hand-derived as `bitrate x srtt / 8`. |
| D6 | AIMD stays behind a `TransportConfig` flag | This replaces the emission policy of a working system. A/B in one scene run, revert without a rollback PR. Flag is removed once real-network data lands. |

## Evidence

A standalone probe drove `GoogCcNetworkController` against a simulated
bottleneck (3 Mbps, halved to 600 kbps at the midpoint):

```
constructed GoogCcNetworkController standalone: OK
  PROBE id=1 target=3000000 bps duration=15 ms count=5
  PROBE id=2 target=6000000 bps duration=15 ms count=5
step 100  capacity 3000000  target 3315182  backlog  -1 ms
step 299  capacity 3000000  target 2522687  backlog  -2 ms
step 300  capacity  600000  target 2522998  backlog  60 ms   <- link drops
step 400  capacity  600000  target  505560  backlog 268 ms
step 599  capacity  600000  target  537366  backlog  -4 ms
```

It finds capacity, backs off *below* the new rate on the step-down, drains the
queue, and settles. Behaviour was tested, not merely construction.

### Honouring probe clusters requires all four of these

Missing any one produces **no benefit and no failure signal** — two of the
three attempts during evaluation produced bit-identical output to not probing
at all:

1. send at the cluster's `target_data_rate`, not the current target rate
2. tag packets with `probe_cluster_id` via `PacedPacketInfo`
3. satisfy **both** `min_probes` and `min_bytes` (tagging exactly
   `target_probe_count` packets leaves the cluster short on bytes)
4. **space packets in time** — packets sharing one `send_time` give the probe
   estimator no send-interval to measure, and the cluster is discarded

Point 4 is why pacing is a prerequisite for estimation, not an independent
improvement.

## Architecture

```
  ACK envelope (frame_seq, tile_x, tile_y, pass_idx, arrival_time_ms_lo16)
        │
        ▼
  arrival-time unwrap ──► PacketResult{ sent_packet, receive_time }
        │                        ▲
        │                        └── send time from the retransmit cache (EmitKey)
        ▼
  GoogCcNetworkController
    on_sent_packet / on_transport_packets_feedback / on_round_trip_time_update
        │
        ▼
  NetworkControlUpdate { pacer_config, congestion_window, probe_cluster_configs }
        │                      │                  │
        │                      │                  └──► pacer probe mode
        │                      └──► caps scheduler staging (in-flight bytes)
        └──► leaky-bucket release rate
```

### Arrival-time unwrapping

`arrival_time_ms_lo16` is the low 16 bits of client wall-clock milliseconds and
wraps every ~65.5 s. The EWMA tolerated this because it used coarse relative
differences; GoogCC's inter-arrival deltas do not. Reconstruct a monotonic
client timeline from the low bits, anchored on the server's receive time for
the enclosing ACK batch.

**Known limitation:** 1 ms resolution. libwebrtc's transport-cc uses 250 µs
ticks. On a low-RTT LAN path 1 ms may be too coarse to resolve the delay
gradient. This is measurable on the tier-1 bench; widening the field is a
future wire change and explicitly out of scope here.

## The pacer

`EmissionQueue` becomes six priority queues with time-based release:

```
P0  passes 0-3   retransmits
P1  passes 0-3   fresh
P2  passes 0-3   FEC parity
P3  passes 4-13  fresh
P4  passes 4-13  FEC parity
P5  passes 4-13  retransmits
```

Base-layer retransmits outrank everything; refinement retransmits rank last.
Tier classification reuses `pass_tier()` (`io_bridge.rs:221`).

Interface change is small: `pop(next_wire_seq, now)` already takes `now`, so it
returns `None` when nothing is due; a new `next_due(now) -> Option<Instant>`
lets the bridge event loop schedule its wakeup. Release rate comes from
`pacer_config.data_window / time_window`.

Probe mode is pacer state: while a cluster is active, release at
`target_data_rate` and tag until both thresholds are met.

**Starvation risk:** strict priority can starve P3-P5 under sustained
base-layer load. `every_cdf53_pass_eventually_lands` is the guard — it asserts
all 16 tiles converge byte-exactly under a 400 kB/s cap with 5% loss.

## Validation

**Tier 1 — deterministic controller bench.** Drive the controller against a
synthetic bottleneck; a pure function of its inputs, so failures reproduce
exactly and it runs in milliseconds.

- clean link: estimate reaches >=80% of capacity within ~2 s
- step-down: estimate falls below the new capacity; backlog drains to ~0
- probe guard: clusters are emitted **and consumed**, since a silently ignored
  cluster costs the ramp with nothing failing

**Tier 2 — browserless scenes.** Assert convergence and rendered pixels, never
byte counts: the harness is not seed-reproducible
(`feedback_browserless_not_seed_reproducible`), and byte-count assertions
survive only on order-of-magnitude margins. Existing scenes become regression
guards: `every_cdf53_pass_eventually_lands` for starvation,
`cdf53_converges_to_lossless_under_10pct_loss` for a pacer that stalls.

**Baseline first.** Phase 1's per-tier tracking already reports pass 0-3 vs
4-13 latency. The deliverable — "pass 0-3 latency drops measurably without
sacrificing pass 4-13 throughput" — needs a recorded before-number or it is
unfalsifiable.

## Sequencing

Land in two stages. They separate "the controller is correct" from "the
controller is in charge", which are different failure modes: the first shows up
as wrong numbers on a bench, the second as regressed video on a real link.

**Stage 1 — the controller is correct.** `goog_cc` replaces the EWMA behind the
existing `bwe.rs` seam; arrival-time unwrapping; tier-1 bench. The controller
observes and reports but does not steer emission, so the production path is
unchanged and the flag from D6 is not yet load-bearing. Acceptance: the bench
asserts ramp and back-off, and `bwe_estimate_bps` tracks the netsim's own token
bucket in a scene — an independently known ground truth.

**Stage 2 — the controller is in charge.** Pacer restructure, probe mode, and
`PacingMode::Paced` driving release. Acceptance: the existing convergence and
starvation scenes stay green, and pass 0-3 latency improves against the Stage 1
baseline.

Splitting here also means that if `goog_cc` disappoints under real loss
patterns, that is discovered at the end of Stage 1 — with the EWMA still
driving nothing and no pacer rewrite sunk into it.

## Risks

| Risk | Mitigation |
|---|---|
| `goog_cc` upstream last moved Dec 2024 | BSD-3-Clause and vendorable; it is a frozen algorithm port, not an evolving API. Pin the version. If it rots, vendoring is a known-cost fallback. |
| `goog_cc` docs list module paths that do not exist (`goog_cc::api::` is private) | Real paths are `goog_cc::network_control`, `goog_cc::transport`, `goog_cc::units`. Recorded here so the first task does not lose time to `E0603`. |
| 1 ms arrival resolution too coarse on LAN | Measurable on the tier-1 bench. Falls back to the EWMA path via the D6 flag; widening the field is a later wire change. |
| Priority starvation of refinement passes | `every_cdf53_pass_eventually_lands` fails if it occurs. |
| Pacing regresses throughput on clean links | D6 flag allows A/B in a single scene run and immediate revert. |
