# Remediation scoreboard

Tracks the [remediation orchestration plan](2026-10-10-remediation-orchestration-plan.md).
States: `open` · `in-progress` · `in-review` (PR open) · `merged` · `closed-already-fixed`.

Base for Wave A: origin/master `36cac03` (2026-10-10).

## Closed before the campaign started

| Finding | State | Evidence |
|---|---|---|
| PS-1 O(dirty×backlog) scans | closed-already-fixed | `by_tile` index, fddb711 |
| PC-2 diag `Array.shift()` stall | closed-already-fixed | 9fa65ef |
| PS-M6 scheduler duplicate retry paths | closed-already-fixed | 7b013da, 1abf0c5, 464ae0f |
| Sender-side loss / suspension | closed-already-fixed | e655a20, 3665f69 |
| H.264 oversize-parity fragmenting | closed-already-fixed | 466a3b1 |

## Wave A

| WS | Branch | Findings | PR | State |
|---|---|---|---|---|
| A1 | `security/session-gating` | P0-1, P0-2, transport SEC-M/L | — | in-progress |
| A2 | `security/client-core-hardening` | P0-3, P0-4, client-core SEC-M | — | in-progress |
| A3 | `hardening/ffi-xdaemon-cli` | P0-5, xdaemon/cli availability + quality | — | in-progress |
| A4 | `capi/contracts` | P0-6, P0-7, capi F4–F11 | — | in-progress |
| A5 | `perf/native-client` | PC-4, native PC-1, PC-M1/M3–M11, Q-M7/M10 | — | open (waits on A2) |
| A6 | `web/constants` → `web/demolition` → `web/perf` | R-1, R-2, Q-H1–H3, web PC-1, PC-3 | — | in-progress |
| A7 | `config/lib-plumbing` | R-3, Q-M4 | — | open (queued) |
| A8 | `harness/burst-loss-and-scenes` | protocol §5 instruments | — | in-progress |
| A9 | `crypto/pqc-provider` | protocol §4.1, SEC-L5 | — | open (queued) |

## Waves B–D

Not started. Wave B is gated on every Wave A PR being merged.
