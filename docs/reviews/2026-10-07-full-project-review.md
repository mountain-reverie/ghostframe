# Ghostframe Full Project Review

**Date:** 2026-10-07 · **Scope:** all 15 workspace crates (~105k lines Rust), ghostframe-web-client (TypeScript), ghostbridge (Go), shaders (WGSL/GLSL), packaging · **Dimensions:** security, performance/efficiency, code quality, reusability/configurability

Ten specialized review passes read the code directly; every finding below carries a verified file:line. The four highest-severity security claims were independently re-verified against master before publication. Severity is rated within the project's stated trust model (tailnet membership is the only authentication; single-client by design per M4a).

---

## Executive summary

| Dimension | Verdict |
|---|---|
| **Security — wire parsing** | Strong. Every decoder in ghostframe-protocol and client-core bounds-checks; no network-reachable panics found in either direction's parsers. |
| **Security — session model** | Weak. The single-client design is an assumption, not an enforcement: any tailnet peer can silently watch the desktop and inject input (P0-1, P0-2). |
| **Security — client robustness** | Two one-datagram DoS holes against a hostile server (P0-3, P0-4); everything else is defensive and clean. |
| **Performance — server** | Correct-but-quadratic: O(dirty × backlog) scans per frame, a blocked event loop during GPU/encode, ~8 copies per outgoing datagram. |
| **Performance — client** | The browser does per-datagram work that should be per-frame, and the GPU re-runs the full inverse wavelet grid every frame despite a dirty-list that already exists but is disconnected. |
| **Code quality** | Bimodal. Periphery (protocol, scheduler, emitter, classifier, config) is exemplary — documented decisions, pinned regression tests. The center — `io_bridge.rs` at 10,570 lines / ~70 fields, gpu_pipeline `frame.rs` with ~600 duplicated lines — carries nearly all the debt. |
| **Reusability/config** | The web client bypasses ghostframe-protocol entirely (~20 hand-retyped wire constants); the most field-debugged tuning values (RTO ceiling, cost model) are compile-time constants; the fix infrastructure for both already exists and is unused. |

**The five actions with the highest value-to-effort ratio:**

1. Gate all feedback (input, display, ACK/NACK) and the datagram broadcast on the HELLO'd session (P0-1/P0-2) — a few hours, closes desktop eavesdropping and remote input injection.
2. Cap `frag_total` and clamp `FrameDimensions` in client-core (P0-3/P0-4) — both clients share the fix point.
3. Wire the already-existing dirty-tile list into the CDF53 inverse dispatch (PC-1) — removes the dominant steady-state GPU cost in both clients.
4. Add a per-tile index to the retransmit cache / scheduler / coverage map (PS-1) — removes the frame-time cliff during every first paint and congestion event.
5. Extend `ghostframe-client-wasm/src/constants.rs` getters to production TS and add a `feedback_stream.rs` module to ghostframe-protocol (R-1/R-2) — kills the constant-drift class that has already caused two recorded incidents.

---

## P0 — Security, fix before next release

### P0-1. Un-HELLO'd sessions receive the full desktop broadcast (eavesdropping)
`ghostframe-lib/src/transport/io_bridge.rs:1767-1796` — `send_to_all_sessions` sends every datagram to every `wt.is_connected()` session. Eviction is keyed on HELLO (`io_bridge.rs:6616-6627`, deliberately leaving un-identified sessions alone at `:6669-6680`). A tailnet peer fetches `/config.json` (unauthenticated, `ghostbridge/web_server.go:44-51`), completes the WebTransport CONNECT, sends no HELLO, and silently mirrors the operator's desktop — in attach mode, their real session — indefinitely, without displacing the legitimate client.
**Fix:** unicast tile/frame datagrams to `last_hello_from` only; close sessions that have not sent HELLO within a short deadline (e.g. 5 s).

### P0-2. Input injection and control messages accepted from any connection
`ghostframe-lib/src/transport/io_bridge.rs:3371-3523` — `dispatch_feedback_bytes` uses the `from` handle only for `apply_hello`; INPUT (`:3485-3513` → XTest injection), `DisplayModeMsg` (`:3454`), and `DecodeErrorMsg` (`:3422`) dispatch regardless of sender. ACK batches (`:3033`) and NACKs (`:3332`) take no handle at all; `wire_seq` is predictable, so a second peer can forge ACKs to release the live client's retransmit entries and poison the BWE, or flood NACKs whose retransmissions are then broadcast (P0-1) into the victim's downlink.
**Fix:** in `dispatch_feedback_bytes`, drop every message type except HELLO when `from != last_hello_from`; pass the handle into `dispatch_ack_datagram`/`dispatch_tile_nack_datagram` and apply the same gate.

### P0-3. `frag_total` from the wire sizes allocations unbounded (client memory exhaustion)
`ghostframe-client-core/src/lib.rs:161-175` — `Assembly::new` does `vec![None; frag_total as usize]` ×2 with the attacker-controlled u16; `reassembly.rs:210-213` inserts on first datagram. Assemblies are keyed by `(frame_seq, tile_x, tile_y, pass)` — all attacker-chosen — and evicted only when `frame_seq` advances; the timeout scan NACKs but never removes. A server that pins `frame_seq` and fans out keys with `frag_total = 65535` forces ~2.3 MB per datagram, nothing ever evicts, and both native and browser clients OOM.
**Fix (in client-core, shared by both clients):** reject `frag_total` above a protocol cap (a 32×32 tile pass fragments into single digits — 64 is generous), and bound total live assemblies/buffered bytes.

### P0-4. `FrameDimensions` is an unclamped network u32 driving texture allocation
`ghostframe-client-core/src/reassembly.rs:186-194` — width/height pass straight through. Native: `render_thread.rs:91-118` → `Framebuffer::new`/`Cdf53Pipeline::resize`; no `on_uncaptured_error` handler exists anywhere, so an invalid `create_texture` panics the render thread; a large-but-valid size allocates multi-GB. Browser: `webgpu/renderer.ts:118-141` sets canvas size and allocates `maxTiles`-scaled buffers. One datagram with `width = height = 0xFFFFFFFF` kills either client.
**Fix:** validate in client-core against a hard ceiling (8192² or the negotiated display cap) and drop the event; belt-and-suspenders clamp in both renderers against `limits.max_texture_dimension_2d`.

### P0-5. FFI integer overflow at the frame-submission boundary
`ghostframe-lib/src/ffi.rs:86` — `let size = (stride * height) as usize;` wraps in u32 (debug panic / silent wrong-length `from_raw_parts` in release). Also none of the three `extern "C"` fns catch unwind; on Rust ≥ 1.81 an internal panic aborts the daemon.
**Fix:** `(stride as usize).checked_mul(height as usize)` returning `-1` on overflow; wrap bodies in `catch_unwind`. ffi.rs has zero tests — add one for the overflow path.

### P0-6. C ABI: server-driven resize closes dmabuf fds the host still holds (use-after-close)
Header contract (`ghostframe-client-capi/include/ghostframe_client.h:153-155`) promises fds live until client destroy; `ghostframe-client-gpu/src/ring.rs:274-301` drops every `ExportedImage` (closing fds, ignoring `in_flight`) on `Event::FrameDimensions`. A resize between `gf_client_acquire_frame` and the host's EGL/Vulkan import leaves the host importing a recycled fd number — classic fd-confusion, remotely triggerable. Related: `ring.rs:300` resets `next_frame_id` on resize, so a held frame's `damage` pointer is dangled by `HashMap::insert` on the reused id (`capi/src/lib.rs:397`).
**Fix:** keep a graveyard of in-flight-at-resize buffers drained by `release`; never reset `next_frame_id` (monotonic ids cost nothing and fix both layers).

### P0-7. C ABI: no threading contract; concurrent calls alias `&mut Client` (UB)
`ghostframe-client-capi/src/lib.rs:318-504` take `&mut *c` while `gf_client_next_event`/`gf_client_event_fd` take `&*c`; the header says nothing about threads, and the API shape (event-fd poll thread + UI input thread) invites exactly the aliasing Rust forbids.
**Fix:** `Client::push_key/push_pointer_*/push_wheel/acquire_frame/release_frame` (`ghostframe-client-native/src/lib.rs:492-632`) only clone senders, write an eventfd, and lock a mutex — change them to `&self`, use `&*c` in the shim, and document that connect/disconnect/destroy require external serialization.

---

## Security — remaining findings

### Server (Medium)
- **Shared feedback buffer splices messages across connections.** `io_bridge.rs:916, 3388-3389` — one `feedback_recv_buf` for all sessions; a partial message from A completed by bytes from B misattributes HELLOs and desyncs the parser. Fix: `HashMap<ConnectionHandle, Vec<u8>>`, cleared in `forget_session`.
- **`ConnectionLost` on any stale handle wipes the live session's retransmit cache.** `io_bridge.rs:5917-5946` (and `evict_session` `:6783`) call `clear_cache()` unconditionally — the already-documented global-cache-wipe defect, confirmed still on master, and exploitable by open-and-abandon connection loops. Fix: scope the clear to `last_hello_from`.
- **Unbounded connection acceptance.** `quic.rs:232-247` auto-accepts every Incoming, no cap, no `retry()` address validation; each session costs a 16 MiB datagram buffer and joins the broadcast fan-out. Fix: cap concurrent connections (2–4), `retry()` unvalidated addresses, actually `close()` in `evict_session`.
- **Pre-handshake stream buffers grow without bound.** `webtransport.rs:65, 241, 468-487` — a CONNECT frame with a huge declared length accumulates forever while flow-control credit keeps being returned. Fix: 16 KiB cap per pre-handshake stream, close on overflow.
- **Reconnect fires a full session reset (amplification).** `io_bridge.rs:3671-3772` — any new CONNECT triggers scheduler clear + 20 force-dirty frames + IDR. Fix: fire only when the HELLO'd session changes; rate-limit.

### Client (Medium)
- **H.264 AU reassembly allows ~78 MB/frame** (`frame_assembly.rs:67-100`, `frag_total × MTU`, self-limited to ~3 in flight). Cap reassembled AU size.
- **Latent OOB in GPU Cdf53 from wire tile coords.** `client-gpu/src/pipelines/cdf53.rs:423-456` indexes `self.tracks[tile_y*cols+tile_x]` with unvalidated u8 coords — unreached in shipped configs (native uses `TileDelivery::Decoded`, browser uses TS pipelines) but one config flag from a one-datagram crash. Validate coords against the grid in client-core so all consumers are covered.
- **C ABI hygiene:** out-struct references formed before the `struct_size` check (`capi/src/lib.rs:256, 356`); `gf_result_str` takes a Rust enum by value from C (`:541` — invalid discriminant is UB; take u32); docs promise forward-compat truncation the code doesn't implement.

### Low / hardening (both sides)
Fault-injection and dump env vars ungated in release builds (`GHOSTFRAME_NO_RTO`, `GHOSTFRAME_DUMP_FRAME` et al. — `config.rs:361-468`); tsnet node keys default to `/tmp/ghostframe-ts` when run manually (`xdaemon/src/main.rs:100`; systemd units are correct); auth key accepted on argv (`cli.rs:15-18`); operator keystrokes logged at `debug` with keysyms (`io_bridge.rs:3511`); native cert pin ignores the validity window (`client-net/src/tls.rs:42-59`); the absence of CORS on `/config.json` is load-bearing against drive-by browser access — add an explicit Origin allowlist before anyone "adds CORS"; `cdf53_count` from GPU-written memory should be clamped to allocation size before `from_raw_parts` (`io_bridge.rs:4693-4711`); network-controlled error strings reach C hosts with escape sequences intact (strip < 0x20 in `fill_message`).

### What's genuinely good
ghostframe-protocol length-checks every decode path with capped counts (ACK ≤ 72, NACK ≤ 64, palette ≤ 16) and a discriminator-collision test registry; TLS keys are generated per-run and never touch disk, 13-day validity matches the W3C cap; all listeners bind tsnet addresses only; Go bridge promotes to u64 before length checks and Rust caps frames at 72 KiB; env reads are centralized with a CI guard; capi panic containment is complete and pinned by tests; client-h264's unsafe V4L2/dmabuf layer validates every driver-reported count, offset, and extent with checked arithmetic.

---

## Performance — server hot path

### High impact
- **PS-1. O(dirty × backlog) scans per frame.** Per dirty tile, `cancel_for_tile` walks the whole retransmit cache (`reliable_emitter/cache.rs:141-152`, tens of thousands of entries at first paint), `bump_generation_collecting` walks the priority queue plus all 14 refinement buckets twice (`scheduler.rs:592-665`), and `drop_cdf53_for_tile` retains over the 60k-capacity coverage map (`fragment_coverage.rs:158-174`). 300 dirty tiles × 30k backlog ≈ 45M comparisons in exactly the frames that are already overloaded. **Fix:** secondary `HashMap<(u8,u8), SmallVec<EmitKey>>` tile index on cache and coverage; resolve scheduler bumps through the existing `SlotMap` instead of scanning (the pattern `supersede_pending_for_tile` already uses). Also answers `has_cache_entries_for_tile` (`io_bridge.rs:4128`) in O(1).
- **PS-2. GPU fence wait + VA-API encode block the entire event loop.** `capture/gpu_pipeline/frame.rs:1146-1149` waits the fence synchronously inside `IoBridge::run`; H264 mode adds ~3 ms of synchronous encode. During the stall no ACKs dispatch, no RTO sweeps run, no outbound drain happens. **Fix:** move `process_frame` to `spawn_blocking`/dedicated thread with a result channel, or poll the fence; longer-term, pipeline frame N's submit against N-1's results.
- **PS-3. ~8 userspace copies per outgoing datagram, ~5 avoidable.** The chain: scheduler emission clones full `TileWork` (`scheduler.rs:784`); `submit_one` copies the whole datagram to stamp 8 bytes (`emitter.rs:144`) then copies again for the cache (`emitter.rs:167` — `Bytes::clone` would be free); WebTransport copies to prepend a 1-2 byte varint (`webtransport.rs:291-295`); tsnet framing copies again and formats the client IP string per packet (`ghostframe-tsnet/src/lib.rs:359-372`, `io_bridge.rs:5799`). ≈ 12 MB/s redundant memcpy at 20 Mbps. **Fix:** stamp in place and share one `Bytes` between queue and cache; reserve varint prefix space in `fragment_tile`; cache the host string per `SocketAddr`; make `TileWork::payload` `Bytes`.
- **PS-4. 8 MB zeroed alloc + full-frame copy every TileCodec frame.** `io_bridge.rs:4598-4616` → `capture/dmabuf.rs:12-39`: mmap → `vec![0u8; stride*height]` → full copy → munmap, per frame (~500 MB/s at 60 fps) even when 3 tiles are dirty. **Fix:** persistent mapping per fd generation + reused buffer; copy only dirty tiles' rows (the dirty set is already known).

### Medium (small diffs, per-ACK-path wins first)
`bytes_outstanding` full-cache walk per loop iteration (`cache.rs:97-103` — keep a running counter); `FragmentCoverageMap::take` O(n) VecDeque scan per ACKed pass (`fragment_coverage.rs:129-138` — lazy tombstones); dead LRU bookkeeping on every cache op, structurally never evicting (`cache.rs:118-121` — delete it); `refinement_deficit_tiles` builds a HashSet over up to 28k queue entries every frame (`scheduler.rs:199-209` — incremental counter); CPU dirty tracker allocates 4 KB per tile per frame just to compare (`tile/mod.rs:283-295`, 8 MB/frame at 1080p — compare in place); 3–4 allocations+copies per inbound packet (`io_bridge.rs:5756`, `webtransport.rs:266` — persistent read buffers); per-packet (not per-tick) housekeeping including path-stats sampling (`io_bridge.rs:5518-5751` — rate-limit to a time cadence); `poll_transmit` capped at 1 datagram + syscall per packet (`io_bridge.rs:5788-5806` — batch); two INFO logs on per-ACK-batch and per-tile paths that survive the production filter and will re-trip the journald rate limit (`io_bridge.rs:3050, 6157` — demote to debug); FNV fingerprint + fragment copies computed before the `trace!` check on every RTO retransmit (`emitter.rs:318-327` — guard with `tracing::enabled!`); 6 KB+45 small allocs per cdf53 tile in Phase B (`io_bridge.rs:4712` — reusable scratch).

---

## Performance — client hot path

### High impact
- **PC-1. CDF53 inverse re-runs the full tile grid every presented frame.** `webgpu/cdf53.ts:519-549` dispatches 7×wgCap workgroups per rAF whenever anything is dirty; native mirror at `client-gpu/src/pipelines/cdf53.rs:494-568`. The fix already half-exists: `dirtyTilesBuffer`/`dirtyTilesCount` are populated by the integrate shader and carry `INDIRECT` usage, but the inverse bind groups were re-pointed to `tileGenBuffer` (cdf53.ts:319-347, "was dirtyTilesBuffer"). **Fix:** dispatch the inverse indirectly over the dirty list; skip `encodeInverse` when `cdf53Count == 0`. This is the ~10 ms/tick the idle-skip comment documents.
- **PC-2. Per-tile diagnostics cost O(32768) `Array.shift()` in production.** `cdf53_globals.ts:131-180`, `diagnostics.ts:85-88`, `palrle.ts:89-113` — every tile arrival does 2–4 O(32k) array memmoves plus eager hex formatting on the JS main thread once the caps fill (~tens of seconds of busy stream). **Fix:** ring buffers with head index; gate all recording behind the `?e2e` flag.
- **PC-3. wasm↔JS boundary: ~9 copies wire→GPU, one crossing + BigInt per datagram, and an `await` serializing intake.** `main.ts:1024-1025` (per-datagram `handleDatagram` + `await drainTransmit()`, BigInt `nowUs()` each), `client-wasm/src/boundary.rs:108-199` (every event payload cloned, then copied again by serde-wasm-bindgen). **Fix:** batch datagrams and drain once per rAF (ACK batching already tolerates 5 ms); pass time as f64; serialize from `&Event` without the clone layer.
- **PC-4. Native present path fully syncs CPU↔GPU per frame.** `client-gpu/src/ring.rs:229-231` — `device.poll(wait_indefinitely())` in `publish`; the comment already names the fix (export a semaphore via wgpu-hal `add_signal_semaphore` with the dmabuf).

### Medium
128 KB allocated per inbound UDP datagram in quinn driving (`client-net/src/endpoint.rs:72, 112` — reuse a buffer field); 4-byte `writeBuffer` per touched tile / per pass entry (`cdf53.ts:462`, `cdf53.rs:452` — write the array once per batch); FEC recovery clones every fragment per attempt (`fragment_parity.rs:120` — XOR from slices); three full copies before decode (`parity_decoder.rs:71`, `reassembly.rs:216, 260` — store `Bytes`, `with_capacity`, move single fragments); per-datagram stale-assembly sweep (`reassembly.rs:133` — only when `frame_seq` advances); native renderer clones every payload at the event boundary (`renderer.rs:246-288` — take `Event` by value); tsnet frame reader drains O(burst²) (`net_thread.rs:117` — cursor parse, one drain); outbound packet copied 3× (`endpoint.rs:179` → `lib.rs:483` → `net_thread.rs:131` — carry `Bytes`); per-flush GPU view/bind-group churn and up to 5 submits/frame (`cdf53.rs:501`, `palrle.rs:191`, `framebuffer.rs:147` — cache + single encoder).

---

## Code quality

### The two concentration points

**Q-1. `io_bridge.rs`: 10,570 lines, ~70-field god struct.** Session lifecycle, frame processing, classification glue, pacing/AIMD, BWE, probing, FEC, ACK/NACK dispatch, palette bookkeeping, display negotiation, input injection, and diagnostics all live on one struct. Concrete split (no behavior change; methods move with their state): `io_bridge/pacing.rs`, `bwe_glue.rs`, `session.rs`, `display.rs`, `ack_dispatch.rs`, plus moving the 3,540-line test mod out via `#[path]`. Supporting refactors: `process_frame_gpu` (1,375 lines, `:4030-5405`) splits along its existing comment banners into `collect_escalation_candidates` / `classify_dirty_tiles` / `pixelperfect_sweep` / `emit_h264_frame` / `emit_tile_codec_frame`; the two complete ~100-field struct literals (`:1430` production vs `:6224` test — a silent-divergence trap this project has been bitten by before) collapse into one private `base()` constructor; the ~60-line cdf53 enqueue sequence duplicated between dirty-Phase-B and escalation (`:4698-4900` vs `:4931-5022`) extracts to `enqueue_cdf53_generation`.

**Q-2. `gpu_pipeline/frame.rs`: ~600 lines of stage recording duplicated byte-for-byte** between `process_frame_with_imported` (`:250-1249`) and `run_first_frame_passes` (`:1268-2066`), plus 14 `run_*_stage` functions that are one template stamped 14 times (~1,700 lines, `:2419-4078`), on a ~90-field processor struct with a 276-line Drop. Fix in dependency order: (1) `ComputeStage {module, dsl, layout, pipeline}` and `HostBuffer<T>` structs with `destroy()` — struct drops to ~30 fields; (2) generic `record_stage(bindings: &[StageBinding], push, dispatch)` — each stage becomes ~10 lines; (3) shared `record_shared_stages()` for the common stage 1.5→3 sequence. Net: frame.rs shrinks by roughly 2,000 lines and barrier fixes stop needing to land twice.

### Availability bugs in xdaemon (cheap fixes)
- `CAPTURE_FPS=0` or non-numeric panics the daemon (`xdaemon/src/main.rs:125-128` expect + `:314` div-by-zero) — warn and default to 30.
- Persistent X11 capture failure = silent zombie: the loop warns 30×/s forever and systemd never restarts (`main.rs:429-433`) — count consecutive failures, then rebuild the connection or exit(1).
- `std::env::remove_var` called from the capture loop of a threaded daemon (`x11_capture.rs:217` — unsound on glibc) — read once at construction into a field.
- Debug-build i16 overflow in the off-screen window check (`x11_capture.rs:290-295`) — do the math in i32.
- Reachable `expect()` on the software-encode scaler init takes down the server on sws allocation failure (`encoder/h264_vaapi.rs:744, 951`) — propagate with `?`.

### Client crates and web client
- **Third generation of orphaned diagnostics, read by e2e as real measurements:** `window.__ghostframe_framePaints` is declared and consumed but has zero writers (`main.ts:721-728` vs `ghostframe-e2e/src/harness/scene.rs:809-841`), and `__ghostframeStats` counters are frozen at zero while four e2e sites derive datagram rates from them (`diagnostics.ts:114-121`, `e2e.rs:3088`). Re-feed or delete — and add the structural guard: an e2e-side assertion that every polled `window.__*` global is non-trivial at least once per suite run. This exact class has two prior postmortems.
- `main.ts` is a 990-line `main()` carrying five expired M3.3b GPU test hooks ("removed once the wavelet math is verified" — it was, 2026-05-29) and a 190-line stats logger — split into `cdf53_debug.ts` (e2e still uses the hooks) and `stats_log.ts`.
- `decoder.ts:1-134` is dead wire-parsing duplicating ghostframe-protocol with zero importers — delete.
- `DecodeErrorBatcher` exists in both Rust and TS, both live, sharing budget semantics that can drift (`client-core/src/decode_error_batcher.rs` vs the TS twin) — route GPU-stage errors through the wasm core and delete the TS class.
- capi gaps: no `gf_client_request_display_mode` export (C embedders can't drive M4b); native renderer never calls `palrle.take_errors` so GPU-detected decode errors silently skip the server feedback loop (`pipelines/palrle.rs:243`); GPU `expect()` on poll/map panics the render thread on device loss with no catch (`palrle.rs:266`, `ring.rs:231`) — route through the existing `ClientEvent::Error` path.
- Six internal tuning constants leak into the public C header via cbindgen (`include/ghostframe.h:28-107` — `REFINEMENT_BIAS_PER_TILE_US`, `IDLE_THRESHOLD`, etc.); make them `pub(crate)` and regenerate (the hazard `BWE_PACED_MODE_SAMPLE_THRESHOLD` already documents).

### Smaller consistency items
`run_window_loop`'s doc comment sits on `run_headless_loop` (`cli/src/commands.rs:351-393`); INPUT sub-kind wire sizes hardcoded in the dispatcher instead of owned by `input_inject` (`io_bridge.rs:3492` — add `required_len()` plus a consumed-length test); `monotonic_now_ns` and env-flag parsing duplicated across xdaemon backends; RandR first-connected-output discovery duplicated between cli probe and xdaemon with diverging crtc==0 policy (`display_probe.rs:253` vs `display.rs:61`); temporary RTOPROBE instrumentation with hand-inlined wire offsets should be deleted now the storm is fixed (`io_bridge.rs:3101-3158`); the `fragment_coverage` sidecar's written retirement plan (2026-06-17) should be executed — it removes a 60k-entry map, the dual-bookkeeping ambiguity, and finding PS-M1 in one move.

---

## Reusability & configurability

### R-1. The TypeScript client bypasses ghostframe-protocol entirely (High)
~20 wire constants hand-retyped: header sizes, `TILE_SIZE`, sentinels, the `Codec` enum, flag bits, message types, eviction reasons, HELLO caps bits (full table in the audit — `decoder.ts`, `feedback.ts`, `main.ts:24`, `cdf53_globals.ts:88`). The fix infrastructure **already exists and is only used by tests**: `ghostframe-client-wasm/src/constants.rs` exports wasm-bindgen getters precisely so "suites read them from Rust rather than hardcoding" — production TS doesn't consume it, and the `GHOSTFRAME_PROTOCOL_STAMP` drift hash is blind to TS literals.
**Plan:** extend `constants.rs` with the missing getters; read them once at wasm-init into a frozen object; cheapest drift gate is a vitest that sweeps every TS literal against the wasm getters (the lossless-golden suites already use this pattern).

### R-2. Feedback-stream message types triple-defined inside Rust (High)
The u8 namespace 0x01–0x08 is split across `ghostframe-lib/src/transport/{client_caps,decode_error,input_inject,display}.rs` and `ghostframe-client-core/src/{loss_tracker,decode_error_batcher,input}.rs` with independent `pub const`s — nothing prevents a new type added in one crate colliding with the other (and 0x04/0x05 already mean different things on the datagram path).
**Plan:** `ghostframe-protocol/src/feedback_stream.rs` with one const per type + HELLO caps bits + a `#[test]` asserting all discriminants distinct; re-export from both crates. ~1 hour, kills six duplicate sites.

### R-3. Values that should be config (the plumbing already exists)
`ghostframe-lib/src/config.rs` has a documented 4-step recipe and a CI env-read guard; route these through it:
- **`EmitterConfig`:** `RTO_BACKOFF_MAX = 5s` and the ACK deadline band (`reliable_emitter/rto.rs:80, 124`) — the exact constants in the retransmit-storm report — plus `CACHE_CAPACITY = 32768` (`reliable_emitter/mod.rs:56`). Field experiments currently require rebuilds.
- **`CostModelConfig`:** the µs costs (solid 0.05 / palrle 8 / cdf53 90 / h264 3000, `classifier.rs:54-59`) were bench-tuned on one machine; other hardware mis-classifies.
- **Un-gate `ClassifierConfig`:** the override plumbing for `REFINEMENT_BIAS_PER_TILE_US` / `HEADROOM_MIN_BYTES_PER_US` / `LOSS_OVERRIDE_THRESHOLD` exists but compiles only under `test-loss-injection` (config.rs:49, 81-91) — one cfg removal plus renaming the env vars.
- **`GpuPipelineConfig`:** K=512 (`gpu_pipeline/mod.rs:54`) sizes a 6 MiB buffer — creation-time config, not a const; `IDLE_THRESHOLD = 30` (`escalation.rs:19`) trivially.
- **xdaemon config.rs:** 8 scattered call-time env reads (`main.rs`, `display.rs`, `x11_capture.rs`) — mirror the lib pattern, parse once in `main()`, and extend the CI env-read guard to xdaemon and the client crates (`GHOSTFRAME_CLIENT_DUMP_*` is currently parsed independently in two crates).
- **VA-API device:** `/dev/dri/renderD128` is the only render node the encoders will ever open (`encoder/vaapi_device.rs:16`) — wrong on multi-GPU hosts; add `GHOSTFRAME_VAAPI_DEVICE`.

### R-4. Dangerous multi-site values (must stay in sync, currently by hand)
Port **443** in three languages (`web_server.go:128`, `xdaemon/main.rs:227`, `cli.rs:31` — pass through the existing ghostbridge FFI call); diag FIFO cap **32768** ×3 TS files (import one const); CDF53 pass count **14** as a literal in client-core and TS vs `CDF53_PASS_COUNT`; `TILE_SIZE` independently redefined 5× (`gpu_pipeline/mod.rs:40` redefines it in a crate that already depends on protocol; test-pattern and bench lack the dep; two TS copies); golden test-pattern palette/geometry parallel-implemented Rust↔TS; SPIHT midpoint reconstruction in 4 sites (`codec/cdf53.rs:808` + three inverse WGSL shaders — the proven drift mode from the 4ca63a3 fix; extract a shared WGSL prelude string prepended in both clients); `palette_fingerprint` TS copy whose "keep in sync" comment points at a file that no longer exists (`palrle.ts:91` → export through wasm).

### R-5. Copy-pasted scaffolding
Tracing init pasted ~15× across test binaries, none setting the stderr writer that PR #16's postmortem demands — add `init_test_tracing()` to the e2e harness and fold the `[phase]` marker into `setup_e2e_server`. The 13-day rcgen cert generation (the W3C/Chrome constraint) implemented 4× with the rationale documented only once — extract `self_signed_short_lived()` in ghostframe-lib.

### Env-var sprawl
~55 distinct `GHOSTFRAME_*` names; lib's are registry-documented in config.rs, xdaemon/client/test-pattern/Go vars documented nowhere. After R-3's per-crate config modules, a one-line DEVELOPERS.md pointer ("all GHOSTFRAME_* vars are documented in their crate's config.rs") becomes true and sufficient.

---

## Suggested sequencing

**Week 1 — close the holes (each ≤ a day):**
P0-1/P0-2 (HELLO gating — one function), P0-3/P0-4 (client-core caps), P0-5 (checked_mul + catch_unwind), xdaemon CAPTURE_FPS/zombie/remove_var fixes, demote the two hot-path INFO logs, delete the dead cache LRU, `bytes_outstanding` counter, `pub(crate)` the six leaked header consts.

**Weeks 2–3 — the performance cliffs and drift killers:**
PS-1 tile index (cache + coverage + slot-resolved bumps), PC-1 dirty-list inverse dispatch, PC-2 diagnostic gating, PC-3 per-rAF batching, PS-3 emitter copy chain, R-1/R-2 constants consolidation + vitest sweep, R-3 EmitterConfig/CostModelConfig/classifier un-gating, P0-6/P0-7 capi contract fixes, per-session feedback buffers.

**Month 2 — structural:**
PS-2 event-loop unblocking (then PS-4 readback reuse), Q-1 io_bridge split (do the `base()` constructor and test-mod extraction first — mechanical, immediate), Q-2 frame.rs stage recorder, fragment_coverage retirement, web-client demolition (dead decoder.ts/feedback.ts, main.ts split, diagnostics re-feed + the e2e non-trivial-global assertion), PC-4 semaphore export.

**Explicitly not recommended:** making `TILE_SIZE` or wire-layout sizes configurable (they're structural/protocol-frozen); unifying the two intentionally-different SSIM implementations; replacing the CPU↔GPU wavelet duplication (inherent to the languages — the byte-exact oracles are the right guard; just extract the shared WGSL prelude).
