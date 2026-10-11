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

Status at 2026-10-11.

| WS | Branch | Findings | PR | State |
|---|---|---|---|---|
| A1 | `security/session-gating` | P0-1, P0-2, transport SEC-M/L | — | in-progress (local work, no PR) |
| A2 | `security/client-core-hardening` | P0-3, P0-4, client-core SEC-M | #120 | merged |
| A3 | `hardening/ffi-xdaemon-cli` | P0-5, xdaemon/cli availability + quality | — | done locally, gates green, not pushed |
| A4 | `capi/contracts` | P0-6, P0-7, capi F4–F11 | #119 | merged |
| A5 | `perf/native-client` | native PC-1, PC-M1/M3–M7/M9–M11, Q-M10, published-frame leak | #122 | in-review; PC-4, PC-M8, Q-M7 delivery deferred |
| A6 | `web/constants` → `web/demolition` → `web/perf` | R-1, R-2, Q-H1–H3, web PC-1, PC-3 | #121 (constants) | constants merged; demolition + perf local, no PR |
| A7 | `config/lib-plumbing` | R-3, Q-M4 | #123 | merged |
| A8 | `harness/burst-loss-and-scenes` | protocol §5 instruments | — | in-progress (local work, no PR) |
| A9 | `crypto/pqc-provider` | protocol §4.1, SEC-L5 | #124 | merged; Chrome's hashed-WebTransport path stays classical |

## Found during Wave A, scheduled later

| Item | Where | Scheduled |
|---|---|---|
| BWE startup ladder truncated, cwnd ↔ budget loop, slow settling | [findings](2026-10-11-bwe-startup-findings.md) | WS-D1, after Wave B |
| Browserless harness drains at 100 ms, bills 33 ms | same | WS-D1 step 1 |
| `native_client_renders_the_test_pattern_into_an_exported_dmabuf` reads blue for red on the dev host, on master | PR #124 comment | unscheduled |
| Web `cdf53.ts` early-returns on a same-tile-count resize before rebuilding bind groups | PR #122 review | WS-A6 `web/perf` |
| `browserless_determinism` flakes 1 in 64 on master (quinn packet-number skip) | PR #122 | fixed by #122 |

## Waves B–D

Not started. Wave B is gated on every Wave A PR being merged.
