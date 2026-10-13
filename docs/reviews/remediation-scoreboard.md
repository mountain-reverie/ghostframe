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
| A1 | `security/session-gating` | P0-1, P0-2, transport SEC-M/L | #129 | merged; endpoint slab leak, slot-holding, PalRLE count clamp recorded as not fixed |
| A2 | `security/client-core-hardening` | P0-3, P0-4, client-core SEC-M | #120 | merged |
| A3 | `hardening/ffi-xdaemon-cli` | P0-5, xdaemon/cli availability + quality | #126 | merged |
| A4 | `capi/contracts` | P0-6, P0-7, capi F4–F11 | #119 | merged |
| A5 | `perf/native-client` | native PC-1, PC-M1/M3–M7/M9–M11, Q-M10, published-frame leak | #122 | merged; PC-4, PC-M8, Q-M7 delivery deferred |
| A6 | `web/constants` → `web/demolition` → `web/perf` | R-1, R-2, Q-H1–H3, web PC-1, PC-3 | #121 (constants), #127 (demolition), #131 (perf) | merged |
| A7 | `config/lib-plumbing` | R-3, Q-M4 | #123 | merged |
| A8 | `harness/burst-loss-and-scenes` | protocol §5 instruments | #128 | merged; §5.3 competitive baseline dropped (optional) |
| A9 | `crypto/pqc-provider` | protocol §4.1, SEC-L5 | #124 | merged; Chrome's hashed-WebTransport path stays classical |

## Found during Wave A, scheduled later

| Item | Where | Scheduled |
|---|---|---|
| BWE startup ladder truncated, cwnd ↔ budget loop, slow settling | [findings](2026-10-11-bwe-startup-findings.md) | WS-D1, after Wave B |
| Browserless harness drains at 100 ms, bills 33 ms | same | WS-D1 step 1 |
| `native_client_renders_the_test_pattern_into_an_exported_dmabuf` reads blue for red on the dev host, on master | PR #124 comment | unscheduled |
| Web `cdf53.ts` early-returns on a same-tile-count resize before rebuilding bind groups | PR #122 review | WS-A6 `web/perf` |
| `browserless_determinism` flakes 1 in 64 on master (quinn packet-number skip) | PR #122 | fixed by #122 |
| `e2e_palrle_session_reset_chromium` fails on master (`saw codecs: []`); CI skips it | PR #129 | unscheduled |
| `e2e_polled_globals_are_live` needs VKMS, so CI skips the guard | PR #127 | unscheduled; needs a non-GPU variant |
| `?e2e` gating of web recorders and pixel readbacks (PC-2 remainder) | PR #127 | WS-A6 `web/perf` |
| AGENTS.md and `docs/specs/budget-floor-deployment-check.md` describe stats-line fields #127 removes | PR #127 | unscheduled, docs |
| Scenes: estimate never follows a 5→15 Mbit step; settles at 0.37 of a 10 Mbit drop-tail link | PR #128 | WS-D1 entry evidence |
| Scroll scene: H.264 on 99 % of frames while scrolling, exact 9.4 s after (n=1) | PR #128 | WS-D2 entry evidence |
| Idle-gap scene is not wire-idle (retransmit drain), so it does not isolate slow-start-after-idle | PR #128 | WS-D1 step 1 |
| Web renderer paints in fixed codec order with no `frame_seq` in its queues: two frames in one rAF can leave an older Solid/H.264 draw over a newer CDF53 tile | PR #131 review | WS-C7 `web/render-queue-frame-seq` |
| Web dimension clamp rejects 8K (32 400 tiles > 65 535-workgroup L1 dispatch needs chunking) | PR #131 | unscheduled |
| `HELLO_DEADLINE` is 20 s armed at accept; arming at session-ready would allow a short one | PR #129 | owner decision; candidate for WS-B1 |
| quinn endpoint slab entry leaks for connections removed before `Drained`; PalRLE GPU counts unclamped; connect/HELLO/close loop not rate-limited | PR #129 review | unscheduled |

## Wave B

Status at 2026-10-12. Base 9d01c41.

| WS | Branch | Findings | PR | State |
|---|---|---|---|---|
| B1 | `refactor/io-bridge-split` | Q-1 steps 1–2 (test module, single `base()`) | #133 | merged |
| B1 | `refactor/io-bridge-modules` | Q-1 step 3, Q-M8 (five pure-move modules) | | in-progress, local |
| B1 | `refactor/io-bridge-functions` | Q-H2, Q-H5, Q-M1, Q-M5, Q-M7, three more moves | | local, waits on modules |
| B1 | `refactor/io-bridge-behaviour` | Q-M6, PS-M10, PS-M11 | | local, waits on functions |
| B2 | `refactor/gpu-pipeline` | Q-2 (`ComputeStage`, `HostBuffer<T>`, shared stage recording) | #134 | merged |
| B2 | `perf/gpu-capture-riders` | PS-4, PS-L6 (command buffer and fence), dead `VulkanReadback`, pool sizes, encoder move | #136 | merged; dirty-rows readback is API only, persistent descriptor sets withheld (no measured gain), raw-pointer accessors not done |
| B3 | `fix/dmabuf-import` | not in the reviews: dma-buf import assumed image pitch == stride; 21 tests skipped silently | #137 | merged; explicit-modifier and zero-copy tiers not shipped |
| — | `ci/env-read-guard-local` | env-read guard now runs in `just ci-local` | #135 | merged |

Decisions: Q-M2 (delete the RTO probe) is closed as **kept** — WS-D1 needs it. L1–L8 do not exist as IDs in the review; its two transport Lows were already fixed (1caf15d, PR #129).

## Suspected during Wave B

Scheduled as Wave V in the [plan](2026-10-10-remediation-orchestration-plan.md), section 4b. "Read" means read from code and never run; "observed" means seen in a test run. Line numbers are from before the io_bridge split.

### Transport (WS-V1)

| ID | Suspected bug | Evidence | Source |
|---|---|---|---|
| T1 | On a `process_frame` error for a dma-buf frame, `io_bridge.rs` ~4474 falls back to `process_frame_cpu` with empty pixels and emits black tiles; the readback site ~4902 returns instead | read, flagged by two reviews | #137 |
| T2 | The GPU→CPU fallback in `process_frame_gpu` increments `frame_seq` twice and calls `emit_frame_dimensions` twice | read | WS-B1 |
| T3 | `palrle_compact_count` goes into `from_raw_parts` unclamped; the CDF53 count is clamped (same item as the #129 review's "PalRLE GPU counts unclamped") | read | WS-B1, #129 |
| T4 | A late ACK retracts from `loss_window`, but the loss already pushed to `pending_losses` still reaches goog_cc | read | WS-B1 |
| T5 | Queued-latency fallback subtracts the u32-wrapping `emit_us` from a non-wrapping u64; `BweSample::server_emit_us` is still the wrapped u32; the synthetic `AckArrival.wire_seq` can collide | read | WS-B1 |
| T6 | `handle_nack` is unused, yet `recent_frame_fragments` is cloned on every H.264 frame to feed it, with a hard-coded 60 fps interval | read | WS-B1 |
| T7 | Stale SAFETY comment on `sweep_rto_retransmits`; the `*mut IoBridge` aliasing also touches `datagram_send_err_first_logged`, `last_hello_from` and the loss-injection fields | read | WS-B1 |
| T8 | `fec_k` is write-only; the H.264 path uses a local `fec_group_size()` | read | WS-B1 |
| T9 | Hygiene: `_ =>` catch-all on `SchedulerEmissionPolicy`; `let _caps = self.client_caps;`; redundant nested cfg in `post_turn`; docs attached to the wrong item (`pacer_tick_budget_bytes`, `apply_path_stats_snapshot`, `base_budget_bytes`, `WT_VARINT_OVERHEAD`); references to a non-existent `SCHEDULER_TICK_BUDGET_FLOOR_BYTES` | read | WS-B1 |
| T10 | `post_turn`'s `poll_probe_window` and `on_timeout` each read `now_std()` separately from the rest of the turn | read; a note for WS-D1, not a bug by itself | WS-B1 |
| T11 | Q-H5 mutation gap: the dirty-path `already_escalated_this_gen = false` reorder is caught by no unit test, only by the e2e CDF53 tests | mutation run | WS-B1 |

### GPU capture (WS-V2)

| ID | Suspected bug | Evidence | Source |
|---|---|---|---|
| G1 | Odd-width NV12: UV rows are `width` bytes apart but hold ceil(w/2) pairs, so one byte is written twice (or one byte past the buffer on the last row) | observed as nondeterministic output; shader read independently | #137, #136 |
| G2 | `import_pixels` copies `min(stride, pitch)` bytes per row; it should be `min(w*4, pitch)`. A stride above the pitch overruns the row, and on the last row the allocation | read; #137's fallback passes tight rows to avoid it | #137 |
| G3 | `sad_buffer` lacks `TRANSFER_DST` but is `cmd_fill_buffer`ed on every first frame; RADV tolerates it | read (spec violation) | #136 |
| G4 | `DRAW_INDIRECT` used as a destination stage on a compute-only queue family; setup prefers such a family | read | #136 |
| G5 | Error-path leaks: first-frame snapshot image if `run_first_frame_passes` fails; `import_pixels`, `allocate_owned_image`, `alloc_host_buffer`, `new_inner` | read; the `import_dmabuf` leak is fixed by #137 | #136 |
| G6 | `FrameAnalysis` slice helpers trust GPU-written counts without clamping | read | #136 |
| G7 | `palette_fold` over-dispatches 16x; `cdf53_escalation_count_buffer` is bound to nothing; the escalation HOST_READ barrier is recorded twice | read; cost only | #136 |
| G8 | `process_frame(fd, w, h, stride)` assumes offset 0 and linear BGRA; nothing carries a DRM modifier | read | #137 |
| G9 | The `frame_palette_set_count` doc says "unique palettes"; it is a hash-slot high-water mark | observed (two tests expected the wrong value) | #137 |
| G10 | `readback_dmabuf` returns a `&[u8]` over memory the producer can still write; no retry on EINTR for `DMA_BUF_IOCTL_SYNC`; `QUEUE_FAMILY_EXTERNAL` where `FOREIGN_EXT` would be exact | read | #136, #137 |
| G11 | No content check for the known VKMS→AMD cross-device staleness on the buffer-import tier | not built | #137 |
| G12 | `tile_analysis.comp` edge density is blind to isoluminant hue edges and 1-pixel alternating patterns | read; by design, owner decision | #137 |
| G13 | `tests/gpu_pipeline.rs` is listed in no workflow; the `dmabuf_real_*` tests are skipped on CI without proof the runner lacks `/dev/udmabuf`; the xdaemon live-framebuffer assertion ran on a blank framebuffer | observed | #137 |

### E2E failing on unmodified master (WS-V3)

| ID | Test | Evidence |
|---|---|---|
| E1 | `e2e_palrle_session_reset_chromium` (`saw codecs: []`); CI skips it | fails on master, PR #129 |
| E2 | `native_client_renders_the_test_pattern_into_an_exported_dmabuf` reads blue for red | fails on master, PR #124 |
| E3 | `e2e_decode_error_thin_uncached` | 2 of 2 on 9d01c41 |
| E4 | `e2e_saturated_link_starves_tiles_into_giving_up` | 2 of 2 on 9d01c41 |
| E5 | `e2e_palrle_oob_index` ("expected 'client decode error' tracing line in server logs") | 1 of 2 on 9d01c41, 3 of 3 on the io_bridge behaviour branch: too few runs to call |
| E6 | `e2e_progressive_refinement_chromium` failed 2 of 4 on #133 under shared-host load, then 12 of 12 clean | not reproduced; record only |
| E7 | The `*_firefox` tests cannot run on the dev host (no geckodriver) | gap, not a failure |

### Build (WS-V4)

| ID | Item | Evidence |
|---|---|---|
| B1 | `--features cdf53-diag` does not build: `scheduler.rs:1826` lacks `palette_bundled`; clippy error at `io_bridge.rs:5120`. CI never builds the feature | observed on two branches; not confirmed on master |

## Waves C–D

Not started. Wave V runs first on the transport files (see the plan, section 4b).
