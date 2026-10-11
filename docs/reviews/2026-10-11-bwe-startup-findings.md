# Bandwidth estimator: startup and settling findings

**Date:** 2026-10-11 · **Origin:** root-causing a CI failure on PR #124 (aws-lc-rs / hybrid key exchange) · **Status:** recorded, not fixed. Scheduled as WS-D1 in the [remediation plan](2026-10-10-remediation-orchestration-plan.md), after the Wave B refactors.

The initial bandwidth estimate and where it settles are both far from the link. This was found by accident; it is not a consequence of PR #124, and it exists on master.

Markers: **(M)** measured, **(I)** inferred and not confirmed.

## What a real session does

Native client through tsnet against the containerised server, netem 12 ms / 2 Mbit, 1920x1080 gradient. One pair of runs, not seed-reproducible.

| | Hybrid KX | Classical KX |
|---|---|---|
| Full convergence | 17.5 s | 16.5 s |
| Estimate at the end | 1.33 Mbit | 1.14 Mbit |
| `rto_fired` | 13,166 | 12,147 |
| `retransmit_attempts` | 19,978 | 18,540 |
| Mean critical latency | 264 ms | 264 ms |

Both sessions had their startup probe ladder truncated to 2.0 / 2.0 / 2.2 Mbit. The key-exchange group made no difference here. (M)

## Mechanism

1. goog_cc's `ProbeController` caps each startup probe at `max(estimate, link_capacity_upper)`. `link_capacity_upper` is the quinn `cwnd / rtt` hint io_bridge pushes through `set_transport_capacity_hint`. Before anything is measured that hint is about 288 kbit/s (12,000 bytes over quinn's 333 ms initial RTT). If it exists when the 6 / 12 Mbit ladder is emitted, the first rung collapses to the 2.0 Mbit start estimate. (M)
2. The hint appears after `CAPACITY_HINT_WINDOW = 16` samples, and a sample is taken **per IoBridge loop iteration, not per unit of time**. Whether the 16th iteration lands before the first feedback is a one-iteration race. (M) The hybrid handshake loses it more often (19 of 20 sweep rows capped, classical 6 of 20), presumably because it produces more wakeups before first feedback. (I)
3. With the ladder capped the sender never exceeds quinn's cwnd. The tick budget is `0.9 x cwnd/rtt x 33 ms`, so quinn stays app-limited and Cubic does not grow cwnd, which keeps the hint low. cwnd sat at exactly 15,755 bytes for about 3 s with zero loss. (M) This is the nested congestion-control coupling of the protocol review §4.2: the hint is derived from cwnd, and cwnd is throttled by a budget derived from the hint.
4. Recovery is slow: AIMD's minimum increase step is 1000 bps per feedback, under a ceiling of 1.5 x acked rate, and the acked rate is the sender's own app-limited rate. (M for the step, I for the ceiling.)
5. The uncapped regime is not healthy either: it over-estimates a 2 Mbit link by about 30%, with ~800 RTO firings, 24–68 stale tiles and 150–200 ms critical latency. These are two different pathologies, not good versus bad.

Causal checks (M), browserless scene, 2 Mbit: hybrid with the hint disabled → uncapped numbers; hybrid with window 64 → uncapped; classical with window 1 or 2 → capped.

## Side findings

- **NACK misses are not losses.** In the capped regime the server logged 26,040 NACK misses; all 26,040 were tile passes still `Pending` in the scheduler, never sent. The client's tail sweep NACKs every missing pass of a tile that shows no progress for 1.5 s, so a slow drain reads as a NACK storm. (M)
- **The browserless harness drains at 100 ms but bills 33 ms.** After the burst frame it drains the scheduler every `HEARTBEAT_SPACING_US` = 100 ms while each drain gets a 33 ms budget, capping the send rate near 0.3 x cwnd per RTT. That makes the cwnd lock structural in the harness. At a 33 ms cadence the lock goes away, and on a 30 Mbit link the capped regime finishes sooner with no stale tiles (0.94 s vs 2.3 s and ~1,170 stale at 12 ms delay). At 33 ms on 2 Mbit both regimes fall into a retransmit storm. (M) That 33 ms is production cadence is taken from a harness comment, not checked on a live server.
- **A probe cluster decides on a 49-byte margin.** The 2.0 → 1.02 Mbit drop at 464 ms is probe cluster 1 delivering 20,048 bytes against a 19,999-byte threshold. (M, per reviewer's numeric match.)

## Conditions → regime

Raw sweep tables: [`data/2026-10-11-bwe-startup/map1.txt`](data/2026-10-11-bwe-startup/map1.txt), [`map2.txt`](data/2026-10-11-bwe-startup/map2.txt). The capped/uncapped outcome is non-monotonic in RTT and rate, as expected of a wakeup-count race.

## Not explained

- A second estimate drop at 7.1–7.5 s with five congestion events.
- Handshake packet and loop-iteration counts for the two handshakes.
- Behaviour under handshake-time loss or delay, padded first flights, or a delayed first burst.
- The server's real post-burst cadence.

## What a fix has to deal with

- Ignoring the cwnd hint unless quinn is actually cwnd-limited is the obvious change, and it restores the overshooting regime, which measured worse at 33 ms cadence. It is not a fix on its own.
- The hint window should be a time, not an iteration count.
- Startup needs an estimate that does not depend on a pre-measurement cwnd, and a recovery path that is not bounded by the sender's own app-limited rate (RFC 7661 / RFC 9002 §7.8 territory).
- The harness cadence has to be corrected first, or the scenes that would judge a fix measure the harness. Changing it moves the 2 Mbit scenes into a retransmit storm and several thresholds need re-baselining.
- The investigation's tracing and knobs (`GF_KX`, `GF_NO_HINT`, `GF_HINT_WINDOW`, `GF_HB_US`, a native probe test) are on the local branch `investigate/hybrid-bwe`, marked not for merge. They are the starting instruments.
