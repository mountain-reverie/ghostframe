# Remediation Orchestration Plan

**Date:** 2026-10-10 · **Inputs:** [full project review](2026-10-07-full-project-review.md) + [protocol comparative review](2026-10-07-protocol-comparative-review.md) · **Shape:** one Opus top-level orchestrator → one orchestrator agent per workstream → workers (Haiku for mechanical tasks, Sonnet for implementation, Opus for every review)

Each workstream is one git worktree producing one PR (or a short stack), with file ownership partitioned so concurrent PRs are unlikely to conflict. Waves serialize the one unavoidable contention point: `io_bridge.rs`.

---

## 0. Ground truth at plan time (verified 2026-10-10 against origin/master)

Master moves fast — several Oct-7 findings are **already fixed**. Do not re-implement:

| Finding | Status | Evidence |
|---|---|---|
| PS-1 O(dirty×backlog) scans | **FIXED** | `by_tile: HashMap<(u8,u8), SmallVec<[EmitKey;4]>>` in cache.rs; `bump_generation_collecting` no longer retain-scans (fddb711, PR #111/#112 line) |
| PC-2 diag `Array.shift()` stall | **FIXED** | bulk `splice(0, len-cap)` in diagnostics.ts (9fa65ef) |
| Scheduler duplicate retry paths (part of PS-M6) | **FIXED** | 7b013da "one retry path per priority tile", 1abf0c5, 464ae0f |
| Sender-side loss / suspension | **FIXED** | e655a20, 3665f69 |
| H.264 oversize-parity fragmenting | **FIXED** | 466a3b1 (landed on master 2026-10-10) |
| P0-1 un-HELLO'd broadcast, P0-2 ungated input/ACK/NACK | **OPEN** | broadcast comment + ungated INPUT arm present on origin/master (note: `dispatch_feedback_bytes` now *receives* the sender handle and a sender-attribution test exists — the gate itself is still absent) |
| P0-3 frag_total, P0-4 FrameDimensions | **OPEN** | no cap/clamp in client-core reassembly.rs |
| P0-5 ffi `stride*height` u32 wrap | **OPEN** | ffi.rs:86 unchanged |
| PQC: ring provider | **OPEN** | workspace Cargo.toml still `rustls` features `["ring", …]` |

**Rule for every workstream (first task, Haiku):** re-verify each assigned finding against the fetched base commit before implementing. If fixed, record the closing commit in the PR description and drop the task. A stale finding re-implemented is a guaranteed conflict and wasted spend.

## 1. Orchestration mechanics

**Hierarchy and models**
- **Top orchestrator (Opus):** owns this plan, wave gating, merge order, conflict arbitration, the scoreboard (`docs/reviews/remediation-scoreboard.md`, updated per PR), and final acceptance against the two reports' finding lists. Never writes code.
- **Workstream orchestrator (Sonnet), one per worktree:** decomposes its brief into tasks, dispatches workers, runs the gates, assembles the PR. Escalates to the top orchestrator on any need to touch a file outside its ownership list.
- **Workers:** **Haiku** — verification greps, dead-code deletion, const hoisting, log-level demotion, doc/comment fixes, mechanical renames. **Sonnet** — everything else: implementation, tests, refactors. **Opus** — every PR review (adversarial, against the repo's "break the code and watch the test fail" standard), plus *pre-implementation design review* where flagged `[opus-design]` below (security gates, unsafe/FFI, transport behavior changes).

**Worktree protocol**
```bash
git fetch origin
git worktree add ../gf-<ws-id> -b <branch-name> origin/master
```
Exceptions: WS-A5 branches from WS-A2's merge commit; WS-A6 is one worktree producing a 3-PR stack. Wave C/D streams branch from the post-wave-B/C origin/master.

**Non-negotiable gates per PR** (from AGENTS.md — these exist because each was learned the hard way):
1. `just lint` — clippy `--workspace --all-targets` is the real gate; `cargo test -p` silently drops test binaries that fail to compile.
2. `just test-unit`; `just ci-local` before opening the PR (fmt + clippy + unit + web build + tsc + release build + cbindgen diff + go vet).
3. Any ghostframe-lib or xdaemon change: `just containers-build` **before** running e2e, then the targeted e2e tests named in the workstream.
4. Anything touching the web client or wire format: `just build-release` (never bare `cargo build` — the SPA is `go:embed`ed) and confirm the bundle hash changed.
5. Wire-format changes touch all four consumers: ghostframe-protocol, ghostframe-lib, ghostframe-client-core, ghostframe-client-wasm.
6. New/changed `pub const` in ghostframe-lib → regenerate and commit `include/ghostframe.h`.
7. Every new test: mutate the fix out and watch it fail before trusting it. Stage explicit paths; never `git add -A`.
8. Known flakes: `full_frame_keyframe_interval` (rerun `--test-threads=1`), browserless `.expect("scene ran")` stalls — capture the failing test name before rerunning.

**PR conventions:** conventional-commit titles matching the repo style (`fix(transport): …`); PR description lists each finding ID addressed (e.g. "full review P0-1, SEC-M2") or closed-as-already-fixed; one logical change per PR.

**Wave 0 (top orchestrator, before anything spawns):** (a) land or park `fix/suspension-healthy-link` (f1ad1b5); (b) one Haiku task: commit the three untracked review docs + field report via a docs-only PR; (c) spawn Wave A.

## 2. Wave A — parallel, disjoint ownership (9 worktrees)

`io_bridge.rs` has exactly one tenant this wave (WS-A1). Everything else stays out of `ghostframe-lib/src/transport/`.

### WS-A1 `security/session-gating` — the P0 security PR `[opus-design]`
**Owns:** `ghostframe-lib/src/transport/{io_bridge.rs, quic.rs, webtransport.rs}` (sole transport tenant in Wave A).
**Findings:** P0-1 (unicast to the HELLO'd session; deadline-close sessions that never HELLO), P0-2 (drop non-HELLO feedback from `from != last_hello_from`; thread the handle into `dispatch_ack_datagram`/`dispatch_tile_nack_datagram` and gate), SEC-M per-handle feedback buffers, SEC-M scope `clear_cache` to the live session (the recorded global-cache-wipe defect), SEC-M connection cap + `incoming.retry()`, SEC-M pre-handshake stream buffer cap (16 KiB), SEC-M session-reset only on HELLO'd-session change + rate limit, SEC-L demote keystroke `debug!` log, SEC-L clamp GPU-written `cdf53_count` before `from_raw_parts`.
**Tasks:** Opus design review of the gating model first (which message types are pre-HELLO legal; eviction interaction — mind memory: connect-time prune was deliberately deleted in M4a, do not reintroduce it). Sonnet implements with a unit test per gate (positive + negative sender). Haiku does the log demotion.
**Gates:** containers-build + `e2e_eviction`, session-reset, and input e2e tests. Opus review.

### WS-A2 `security/client-core-hardening`
**Owns:** `ghostframe-client-core/src/*` (reassembly, frame_assembly, lib, cdf53 validation).
**Findings:** P0-3 (cap `frag_total` ≤ 64, bound live assemblies/bytes), P0-4 (clamp FrameDimensions ≤ 8192² in core, drop oversize), SEC-M H.264 AU size cap, SEC-M validate tile coords against the grid (closes the latent client-gpu cdf53 OOB at the shared chokepoint).
**Tasks:** Sonnet, with malformed-input unit tests per cap (hostile-server fixtures). Rust-only — the belt-and-suspenders renderer clamps go to WS-A5 (native) and WS-A6 (web) to avoid file overlap.
**Gates:** unit + wasm build (`wasm-pack` targets per memory note). Opus review (network-facing). **Merges early — WS-A5 depends on it.**

### WS-A3 `hardening/ffi-xdaemon-cli`
**Owns:** `ghostframe-lib/src/ffi.rs`, `ghostframe-xdaemon/*`, `ghostframe-cli/*`, `ghostframe-lib/src/encoder/h264_vaapi.rs` (scaler expect only).
**Findings:** P0-5 (`checked_mul` + `catch_unwind` + first ffi unit test), xdaemon CAPTURE_FPS panic→default-30, capture-loop zombie→bounded-failures-then-exit, `remove_var`→read-once field, i16 off-screen overflow, swscale `expect`→`?`, Q: xdaemon `config.rs` module (read env once; all 8 vars — this stream owns xdaemon env handling so WS-A7 doesn't collide), cli/xdaemon `util.rs` dedup (`monotonic_now_ns`, `env_flag`), doc-comment fixes (Haiku batch: M1 cli docs, L1-L9 from the cli/xdaemon review).
**Gates:** containers-build + one xdaemon e2e smoke; `xvfb-run` for X11 tests (memory). Opus review on ffi.rs only.

### WS-A4 `capi/contracts` `[opus-design]`
**Owns:** `ghostframe-client-capi/*`, `ghostframe-client-gpu/src/{ring.rs, export.rs}`, `ghostframe-client-native/src/lib.rs` (signatures only).
**Findings:** P0-6 (fd graveyard drained by `release`; stop resetting `next_frame_id` — fixes the dangling damage pointer too), P0-7 (`&self` on push_*/acquire/release in client-native; `&*c` in the shim; threading contract in the header), F4 struct_size-read-before-reference, F5 `gf_result_str(u32)`, F9 `map_client_error`, F6 modifier-slice guards, missing `gf_client_request_display_mode` export, F11 strip control bytes in `fill_message`, header doc fixes (Haiku).
**Gates:** cbindgen header regen committed (CI diffs it); abi tests extended; `clippy` on both x86-64 and note the aarch64 `c_char` trap (use `pointer::cast`). Opus review (FFI soundness).

### WS-A5 `perf/native-client` — **branches from WS-A2's merge**
**Owns:** `ghostframe-client-gpu/src/{pipelines/*, renderer.rs, framebuffer.rs}` (not ring/export — WS-A4's), `ghostframe-client-native/src/{render_thread.rs, net_thread.rs}`, `ghostframe-client-net/*`, client-core perf-only touches.
**Findings:** PC-4 (semaphore export via wgpu-hal `add_signal_semaphore`; if blocked upstream, document and defer), PC-M6 `apply_event(Event)` by value, PC-M8 cached views/bind-groups + one encoder/submit per flush, native dirty-list cdf53 inverse dispatch (native half of PC-1), PC-M9 net_thread cursor parse + reused outbound buffer, PC-M10 tally gating, PC-M11 `Bytes` outbound chain, PC-M1 endpoint buffer reuse, PC-M3/M4/M5/M7 client-core copy/scan fixes, Q-M7 native `take_errors` → DecodeError feedback, Q-M10 GPU `expect`→`ClientEvent::Error`, renderer-side dimension clamp (from WS-A2's new constant).
**Note:** verify first — recent master commits (706fcfe coalesced uploads, 4a4e906 bounded render drain) already cover parts.
**Gates:** `just ci-client`; gpu tests self-skip without hardware — run on the GPU host. Opus review on the dirty-dispatch and semaphore work.

### WS-A6 `web/constants-demolition-perf` — one worktree, 3 stacked PRs
**Owns:** `ghostframe-web-client/src/*`, `ghostframe-client-wasm/*`, `ghostframe-protocol/src/feedback_stream.rs` (new), one-line re-exports in lib/client-core, e2e readers of `window.__*`.
**PR-a (constants):** R-1 + R-2 — `feedback_stream.rs` with every feedback-stream discriminant + HELLO caps bits + distinctness test; extend wasm `constants.rs` getters (header sizes, sentinels, codec enum, eviction reasons); TS reads them at init; vitest literal-sweep; fix the stale `palette_fingerprint` pointer + export it (R-F4); TILE_SIZE redefinition cleanup (R-F5, Haiku).
**PR-b (demolition):** delete dead `decoder.ts` wire-parsing + dead `feedback.ts` surface (Haiku); unify DecodeErrorBatcher through the wasm core (Q-M2); re-feed or delete `__ghostframe_framePaints` + `__ghostframeStats` **and their scene.rs/e2e readers in the same PR** (Q-H1/H2 — decide re-feed vs delete per global with the e2e reader's value as the criterion); split main.ts (`cdf53_debug.ts` behind `?debug`, `stats_log.ts`) (Q-H3); dead-global purge + typed declarations (Q-M8/L9); add the e2e assertion that every polled `window.__*` global is non-trivial once per suite — the structural fix for three generations of orphaned diagnostics.
**PR-c (perf):** PC-1 web half (indirect dispatch over the existing dirty-tile buffers; skip `encodeInverse` at count 0), PC-3 (per-rAF datagram batching, f64 time, kill the per-event clone layer), PC-M2 batched writeBuffer, PC-L5 scratch pooling, PC-L6 gate pixel readbacks behind `?e2e`, web-side dimension clamp.
**Models:** Haiku deletions, Sonnet rest, Opus review on PR-a (wire surface) and PR-c (GPU logic). **Gates:** `npm run build` + tsc + vitest; `just build-release` + bundle-hash check; wire-format four-crate rule for PR-a; full browser e2e sweep after PR-b (it touches test readers).

### WS-A7 `config/lib-plumbing`
**Owns:** `ghostframe-lib/src/config.rs`, `tile/classifier.rs`, `reliable_emitter/{rto.rs, mod.rs}` (consts only), `include/ghostframe.h`.
**Findings:** R-3 — `EmitterConfig` (RTO ceiling, ACK deadline band, cache capacity), `CostModelConfig`, un-gate `ClassifierConfig` overrides (rename `GHOSTFRAME_TEST_*` env names), `GHOSTFRAME_VAAPI_DEVICE`, Q-M4 `pub(crate)` the six leaked header consts + regen header, extend the CI env-read guard to the client crates + dedupe `GHOSTFRAME_CLIENT_DUMP_*` parsing, DEVELOPERS.md env-var pointer (Haiku).
**Constraint:** follows config.rs's documented 4-step recipe; values stay at current defaults — this PR adds knobs, never retunes.
**Gates:** env-guard CI check; header diff. Conflicts: none (WS-A1 reads config, doesn't edit it).

### WS-A8 `harness/burst-loss-and-scenes`
**Owns:** `ghostframe-e2e/*` (minus the `__*` assertion, WS-A6's), `ghostframe-bench/*`, `ghostframe-test-pattern/*`.
**Findings:** protocol §5 — Gilbert-Elliott burst model beside the i.i.d. injection vars (same env protocol, documented in the harness notes); nested-CC scene (slow-start-after-idle + probe-under-load, asserts BWE convergence time); scrolling-text / text-under-motion scene (serves §4.4 and the scroll design note's acceptance gate); optional competitive-baseline scaffold (TigerVNC/Sunshine/RustDesk photon-to-photon — time-boxed, drop if the afternoon runs out).
**Note:** memory — browserless is not seed-reproducible and is opt-level sensitive; new scenes must loop ≥12x before being made gates. New `tests/*.rs` files are invisible to CI until the workflow names the `--test` target — add it in the same PR.
**Models:** Sonnet. This stream *produces the measuring instruments Waves C/D need* — start it first.

### WS-A9 `crypto/pqc-provider` `[opus-design]`
**Owns:** workspace `Cargo.toml`/lock (crypto deps), provider-construction sites in `transport/quic.rs` (crypto config block only) and `ghostframe-client-net/src/tls.rs`.
**Findings:** protocol §4.1 — swap ring → aws-lc-rs (rustls ≥ 0.23.22, `prefer-post-quantum`), inject the provider through quinn-proto in server and client, honor `notAfter` in the native pinning verifier (SEC-L5); verify the handshake negotiates X25519MLKEM768 against Chrome (one e2e assertion or manual capture recorded in the PR).
**Risks to check first (Sonnet):** aws-lc-rs build deps (cmake/bindgen) in the e2e Docker image and CI — if the image needs new packages, that goes in this PR with a `containers-build` note. rcgen stays on ring independently — verify compatibility.
**Conflict note:** shares quic.rs with WS-A1 in different regions (crypto config vs accept loop); whichever merges second rebases — top orchestrator sequences the merges.

## 3. Wave B — the refactors (2 parallel worktrees, gate: all Wave A merged)

### WS-B1 `refactor/io-bridge-split` — sole transport tenant
Q-1 exactly as the full review specifies, in reviewable commits: (1) test-mod extraction via `#[path]`; (2) single `base()` constructor replacing the twin ~100-field literals; (3) sub-modules `pacing.rs`, `bwe_glue.rs`, `session.rs`, `display.rs`, `ack_dispatch.rs`; (4) `process_frame_gpu` split along its banners (Q-H2); (5) cdf53-enqueue dedup (Q-H5); (6) `dispatch_ack_datagram` split (Q-M1); (7) `run()`→`post_turn()` (Q-M7); plus the Haiku batch: ~~delete RTOPROBE (Q-M2 — the storm is fixed)~~ (kept by the owner's call on 2026-10-12: WS-D1 needs it), min-RTT/datagram-size helpers (Q-M5), `required_len` for INPUT kinds (Q-M6 + the consumed-length test), const consolidation (Q-M8), L1–L8, remaining PS log demotions (PS-M10) and the FNV trace gate (PS-M11).
**Hard rule:** zero behavior change; the diff is moves + mechanical extraction. **Gates:** full unit + containers-build + the deterministic e2e sweep; Opus review checks nothing changed semantically (the twin-constructor unification is the one place divergence could hide — diff the two literals' values first and surface any that differ to the top orchestrator).

### WS-B2 `refactor/gpu-pipeline`
Q-2: `ComputeStage` + `HostBuffer<T>` (struct ~90→~30 fields, Drop collapses), generic `record_stage`, shared `record_shared_stages` (kills the ~600-line duplicate), then the perf riders on the same files: PS-4 (persistent mmap per fd generation + dirty-rows-only copy, drop the zero-fill), PS-L6 persistent command buffer/fence/descriptor sets, dmabuf.rs dead `VulkanReadback` deletion, encoder slice Mediums (pool-size derivation, `FRAME_PALETTE_SET_BYTES`, bench-only `H264VaapiEncoder` relocation, accessor-ified raw pointers Q-M on mod.rs).
**Owns:** `ghostframe-lib/src/capture/*` only. **Gates:** GPU-host test run (VKMS e2e per memory), CPU-vs-GPU oracle must stay byte-exact. Opus review.

## 4. Wave C — transport evolution on the new boundaries (gate: Wave B merged; parallel)

| WS | Branch | Owns (post-split files) | Scope | Notes |
|---|---|---|---|---|
| C1 | `perf/server-copy-chain` | `reliable_emitter/*`, `webtransport.rs`, `ghostframe-tsnet`, io_bridge send/drain fns | PS-3 (stamp-in-place, shared `Bytes`, varint prefix reserve, host-string cache, `TileWork: Bytes`), PS-M7 inbound buffer reuse, PS-M9 outbound batching, PS-M8 housekeeping cadence, PS-M12 scratch, PS-M13 dead LRU, PS-M2 coverage `take`, bytes_outstanding counter — **verify each against the PR #111/#112 line first**; several may be gone | Opus review (wire path) |
| C2 | `perf/event-loop-unblock` `[opus-design]` | `server.rs`, io_bridge run loop, gpu-glue seam | PS-2: move `process_frame` off the event loop (spawn_blocking or async fence), then frame pipelining | Depends on WS-B2; acceptance = WS-A8's nested-CC scene shows ACK latency flat during encode |
| C3 | `transport/fec-upgrade` | `ghostframe-protocol/src/fec.rs`, emitter FEC glue | Adaptive FEC rate off BWE loss+RTT (libwebrtc-table style), interleaved/column XOR or small-group RS (`reed-solomon-simd`), H.264 keyframe path K=2→per-frame RS (protocol §4.3/§4.4 + Kyber addendum) | Acceptance on WS-A8's Gilbert-Elliott model; wire change → four-crate rule + HELLO/caps gate for decoder support |
| C4 | `transport/coverage-retirement` | `fragment_coverage.rs`, cache.rs, ack_dispatch.rs | Q-M3: execute the written 4-step retirement (widen CacheEntry, consume from cache remove, delete module) | After C1 stabilizes; big simplification, Opus review |
| C5 | `encoder/h264-gop` | `encoder/*`, H.264 emission module | GOP-11 → intra-refresh, or NACK-driven reference invalidation (protocol §4.4); optional: lossless-text chroma catch-up spike behind a flag | Acceptance = WS-A8 text-under-motion scene |
| C6 | `ops/cert-rotation` | quic.rs cert block, `ghostbridge/web_server.go`, web `bootstrap.ts`, native tls.rs | Dual-hash overlap window (libp2p pattern), backdated notBefore, per-session hash fetch, no cross-day caching (protocol §6.2) | Crosses Rust/Go/TS — schedule when A6/A9 are long merged |
| C7 | `web/render-queue-frame-seq` | `ghostframe-web-client/src/webgpu/*`, `main.ts` tick, wasm event boundary; check `ghostframe-client-gpu` for the same ordering | Carry `frame_seq` in the renderer's per-codec queues and resolve each tile by newest sequence, not by fixed codec paint order. Since #131 a CDF53 tile is lifted only in the tick its pass arrives, so when two server frames land in one rAF an older Solid/H.264 draw can sit over a newer CDF53 tile (before #131 the full-grid lift had the mirror fault: stale wavelet over newer tiles). Also move `flushDatagrams()`/`onTimeout` inside the tick's try/catch | Found in the #131 review; no dependency on Wave B, placed here by the owner's call (2026-10-12). Acceptance = a test that delivers two frames with different codecs for one tile in a single tick and asserts the newer one is on screen |

## 4b. Wave V — verify the bugs suspected during Wave B (gate: the WS-B1 series merged)

Added 2026-10-12 by the owner's call. Wave B's refactors were held to zero behaviour change, so the agents reported what looked wrong in the code they moved instead of fixing it. Almost all of it was read from code, not reproduced. The full list with its source PR is in the [scoreboard](remediation-scoreboard.md), section "Suspected during Wave B".

**Standing rule for every item:** reproduce first. Write the failing test (or the measurement) against master; if it cannot be made to fail, close the item as `not-reproduced` with the reason and do not change the code. A fix lands only with the test that failed before it. Items that turn out to be dead code go to a deletion commit, separately from fixes.

| WS | Branch | Owns | Scope | Notes |
|---|---|---|---|---|
| V1 | `fix/transport-suspected` | `transport/io_bridge/*` (post-split) | T1–T9 of the scoreboard list: black tiles on a failed dma-buf `process_frame`, double `frame_seq`/dimensions on the GPU→CPU fallback, unclamped `palrle_compact_count`, late ACK vs `pending_losses`, mixed-clock queued-latency fallback, `handle_nack`/`recent_frame_fragments` dead path, stale SAFETY comment and aliasing in `sweep_rto_retransmits`, write-only `fec_k`, misattached docs | Sole transport tenant while it runs: finish before Wave C's transport streams and before D1. T4 and T5 feed the estimator, so D1 must not start until they are verified or fixed. Opus review |
| V2 | `fix/gpu-capture-suspected` | `ghostframe-lib/src/capture/*`, shaders | G1–G10: odd-width NV12 chroma race (observed), `import_pixels` row overrun, `sad_buffer` usage flag, `DRAW_INDIRECT` stage on a compute-only family, first-frame and other error-path leaks, unclamped `FrameAnalysis` counts, `palette_fold` 16x over-dispatch, no offset/modifier in `process_frame(fd, ..)`, the `frame_palette_set_count` doc, the readback slice over producer-written memory | Parallel with V1 (disjoint files). Run the Vulkan validation layer once over the golden test: it settles G3 and G4 directly. Also here: wire `readback_dmabuf_rows` into the frame path only if V1 has landed (needs transport edits) |
| V3 | `test/e2e-master-failures` | `ghostframe-e2e/*`, whatever the root cause owns | E1–E5: the five e2e tests that fail on unmodified master. Bisect each to its first failing commit before anything else | Needs the heavy lock; serial with other e2e users. A test that CI skips and that fails locally is a broken guard, not a flake |
| V4 | `build/cdf53-diag` | Cargo features, CI | B1: the `cdf53-diag` feature does not build (`scheduler.rs` missing `palette_bundled`, clippy error in io_bridge). Fix it and add a CI check, or delete the feature | Haiku-sized |

## 5. Wave D — gated experiments (serial, each behind its measurement)

- **D1 `transport/bwe-startup-and-cc-coupling` `[opus-design]`:** scope widened on 2026-10-11 by the [BWE startup findings](2026-10-11-bwe-startup-findings.md), which observed the coupling on a real path: a 2 Mbit link took ~17 s to converge with >12,000 RTO firings and an estimate at 55–65% of the link, under either key-exchange group. Deliberately scheduled **after Wave B**, so the work happens on the split `bwe_glue.rs` / `pacing.rs` rather than inside the 10k-line io_bridge. Order: (1) fix the browserless harness's post-burst cadence (drains at 100 ms, bills 33 ms) and re-baseline the affected scenes, with WS-A8's nested-CC scene; (2) make the capacity-hint window a time, not a loop-iteration count, removing the one-iteration race that truncates the startup probe ladder; (3) break the cwnd ↔ budget loop — startup must not depend on a pre-measurement cwnd, and recovery must not be bounded by the sender's own app-limited rate; expose quinn CC state to the BWE (wire-departure timestamps from the pacer, discard cwnd-clipped probes, cwnd exhaustion as a hard signal); (4) only then evaluate a custom controller via `congestion_controller_factory`. Exit gate: estimate within a stated tolerance of the link across the rate × RTT map in the findings' data, under both handshakes, each scene looped 12x (memory: knife-edge timing flakes are product bugs); time-to-full-screen and RTO counts on a shaped native-client session reported before and after. Also in scope: the client tail sweep NACKing passes the server has not sent yet.
- **D2 `feature/scroll-copyop`:** the [scroll-detection design note](../specs/scroll-detection-design-note.md), phase 1 — its own plan, entered only if the WS-A8 scrolling scene confirms the H.264-occupancy prediction.
- **Out of campaign** (feature milestones, not remediation): audio, clipboard, session-resume; MoQ/AV1-SCC/L4S/Tailscale-PQ watch items (recheck ~2027-04).

## 6. Conflict matrix and merge order

**By ownership, the only shared files are:** quic.rs (A1 accept-loop vs A9 crypto-config — different regions, orchestrator sequences the two merges), client-core (A2 then A5, serialized by branching), TS tree (A6's internal stack), Cargo.lock (A9 vs C3 — trivial, regenerate on rebase), e2e crate (A6-b's assertion vs A8's scenes — different files).

**Merge order within Wave A:** A2 → A3/A4/A7/A8 (any order) → A1 → A9 (rebase over A1's quic.rs) → A5 → A6 stack. Rationale: A2 unblocks A5; A1 is the largest io_bridge diff so later io_bridge-adjacent merges rebase onto it, not vice versa.

**Scoreboard:** the top orchestrator maintains a table (finding ID → workstream → PR → state: open / in-review / merged / closed-already-fixed) and ends the campaign with a verification pass: one Haiku sweep re-grepping every P0/High finding's signature against final master, Opus signing off the residue list.

## 7. Spend shape (indicative)

Wave A ≈ 9 orchestrators / ~25 Sonnet + ~15 Haiku + ~10 Opus-review tasks; Wave B ≈ 2 large Sonnet streams + 2 heavy Opus reviews; Wave C ≈ 6 medium streams; Wave D ≈ 2 gated. The expensive items are deliberately late: refactors land only after the security fixes stop moving the files under them, and the risky transport changes land only after Wave A built the instruments that can prove them.
