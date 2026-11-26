# LD: ghostframe-lib design and code quality

Re-verified at `e85fb005`, worktree `ghostframe-review-2026-11-25`. Every number
below comes from that tree (grep/awk over sources; nothing was compiled).
"Non-test" means `*tests*.rs` files and `#[cfg(test)]` blocks are excluded.

Two facts drive the scheduling of this whole group:

1. Almost every LD item edits `ghostframe-lib/src/transport/io_bridge.rs` or a
   file under `io_bridge/`. LD-1 rewrites how every one of those files reaches
   its state, so **it conflicts with every other open io_bridge change**. It
   goes after the small security fixes (AV), after the wire-format moves (WP)
   and after the UF FrameAnalysis work has landed in `frame_backend.rs` /
   `frame.rs`. LD items that shrink LD-1's surface (LD-3, LD-2's BridgeError,
   LD-5, LD-9) go before it.
2. The oracle for every pure move already exists:
   `ghostframe-e2e/tests/browserless_determinism.rs` prints
   `DETERMINISM-DIGEST <name> <hash>`. The hash covers every datagram (offer
   instant, direction, length, verdict), every counter and the final
   framebuffer, and it is stable across processes. A pure-move PR must leave
   all three digests unchanged next to master. Record them in the PR body.

Standard gates, referred to below as **G-lib**:
`gf-gpu-run just ci-local` (fmt, env-read guard, clippy `--workspace
--all-targets -D warnings`, unit tests, release build, cbindgen header diff, go
vet; `gf-gpu-run` because ci-local touches VA-API), then `just
containers-build` and `just test-e2e`, then the browserless suite
(`cargo test --release -p ghostframe-e2e --test browserless_determinism
--test browserless_runner --test browserless_h264 -- --test-threads=1`). Pure
moves add **G-move**: (a) `cargo test -p ghostframe-lib --lib -- --list | wc -l`
gives the same count on master and branch (1653 `#[test]`/`#[tokio::test]`
attributes in `ghostframe-lib/src` today); (b) the three DETERMINISM-DIGEST
values are identical; (c) `git diff --stat` shows no new test logic, only
renamed paths.

---

## LD-1 (High): IoBridge is a god object

**Impact.** Every subsystem reaches every other subsystem's state through
`self`. Nothing is encapsulated, so no invariant can be stated locally, every
io_bridge change conflicts with every other one, and tests depend on the
layout: they poke 195 distinct fields and methods of `IoBridge` across 2105
field-access lines.

**Status at e85fb005.**
- `pub struct IoBridge` is `ghostframe-lib/src/transport/io_bridge.rs:514-1255`:
  **164 fields**, **33 cfg-gated** (12 `any(test, feature =
  "test-loss-injection")`, 12 `any(test, feature = "browserless-harness")`, 9
  `test`). The older review counted about 135.
- **19 `impl IoBridge` blocks in 18 files** (`io_bridge.rs:1424` plus
  `io_bridge/{ack_dispatch,block_tx,bundle_tx,bwe_glue,display_mode,emit,frame,
  h264_rate,h264_refs,h264_tx,harness,model_glue,pacing,repair_status,session,
  transport_probe}.rs` and two in `stats_service.rs`). The module docs of
  session.rs, pacing.rs, bwe_glue.rs, ack_dispatch.rs, emit.rs, frame.rs and
  model_glue.rs each say "Owns, as logic over `IoBridge` fields that stay in
  `io_bridge.rs`". The ownership is written down but not enforced.
- Non-test code makes 1487 `self.<field>` accesses. Test files make 2105
  `b.`/`bridge.` accesses. The heaviest are `h264` 228, `reliable_emitter` 184,
  `palette_table` 162, `scheduler` 117, `metrics_tracker` 109 and
  `transmission_ledger` 106 (for example
  `io_bridge/cdf53_fallback_tests.rs:128`
  `b.metrics_tracker.get_mut(2, 1).codec_state = ...`).
- The constructor `base_with` (`io_bridge.rs:1476`) is 278 lines of field
  initialisers.
- The partial precedent: `H264Tx` (`io_bridge/h264_tx.rs:368`) groups 30-odd
  fields, but every field is `pub(super)` and every method is still
  `impl IoBridge`. `LoopState` (`io_bridge.rs:492`) is the one fully private
  sub-state.

**Root cause.** The bridge grew one subsystem at a time, and each one was split
out by file (`impl IoBridge` in a new module), never by ownership.

**Coupling measured per file.** These are `self.<field>` hits per cluster from
the table below, which says which methods can move:

| file | clusters it touches (hits) | move methods onto a sub-struct? |
|---|---|---|
| display_mode.rs | host 5, core 1 | yes, all |
| block_tx.rs | fec 72, wire 17, rate 4 | yes, with `&mut Wire` passed in |
| model_glue.rs | rate 14, telemetry 4 | yes |
| h264_rate.rs / h264_refs.rs | h264 19/104, wire 4/15, rate 11/2 | yes, with `&mut Wire` + rate snapshot passed in |
| stats_service.rs | stats 21, few others | yes, inputs passed as a `StatsInputs` |
| transport_probe.rs | loss 63, fec 34, wire 20 | mostly (Telemetry + LossPath), read-only refs passed in |
| session.rs, frame.rs, emit.rs, ack_dispatch.rs, bwe_glue.rs, pacing.rs, bundle_tx.rs | 6-12 clusters each | no: these are the coordinators and stay on `IoBridge` |

**Design (chosen).** Three phases. Phases 1 and 2 are pure moves.

*Phase 1, grouping (pure move).* Replace the flat fields with owned sub-structs,
one cluster per PR. Methods stay on `IoBridge`; `self.scheduler` becomes
`self.tile.scheduler`. Disjoint-field borrows keep compiling:
`self.wire.reliable_emitter.x(&mut self.loss.transmission_ledger)` borrows two
different fields, exactly as before. Sub-struct fields start as `pub(super)` so
the move stays mechanical.

*Phase 2, leaf methods (pure move).* Move each method whose body touches one
cluster (plus explicit parameters) into `impl <SubStruct>`. Use the table
above: display_mode, block_tx, model_glue, h264_rate/h264_refs, stats_service,
transport_probe. `IoBridge` keeps a one-line forwarding method only where the
forwarder has external callers.

*Phase 3, close the fields.* Make the sub-struct fields private to their owning
module. Tests switch to accessors on the sub-struct
(`b.tile.metrics_mut().get_mut(2, 1)`) or to a `#[cfg(test)] impl` in the
owning module. This phase is not a pure move (visibility changes only), but it
changes no behaviour either.

Field to sub-struct table. All 164 fields are covered exactly once; checked by
script against the struct.

| sub-struct (module) | fields |
|---|---|
| *stays on `IoBridge`* (9) | `_handle, stream, server, local_addr, loop_state, frame_rx, fatal, epoch, diagnostics` |
| `Sessions` (session.rs, 18) | `wt_sessions, last_hello_from, close_deadlines, non_live_feedback_drops, non_live_datagram_drops, unroutable_inbound_datagrams, has_seen_prior_session, session_started_at, last_disconnect_at, last_takeover_at, session_epoch, client_caps, connected_session_count, feedback_recv_bufs, status_streams, was_idle, last_emitted_dimensions, dimensions_retransmits_left` |
| `Access` (session.rs or new access.rs, 5) | `rechecker, peer_lookup_tx, peer_lookup_rx, peer_access, origin_policy` |
| `HostIo` (display_mode.rs, 5) | `input_injector, display_controller, feedback, display_scale_milli, pending_display_mode` |
| `FramePath` (frame.rs/frame_backend.rs, 13) | `held, frame_seq, dirty_tracker, frame_pipeline, in_flight, pending_controls, precomputed_fallback, deferred_decode_errors, force_dirty_frames, gpu_cushion_left, gpu_analyse_failures, cdf53_escalation_candidates_this_frame, cdf53_stranded_candidates_this_frame` |
| `Mode` (frame.rs/bwe_glue.rs, 4) | `classifier, frame_mode, adaptation_context, recent_suspension_flags` |
| `TileTx` (emit.rs, 6) | `scheduler, metrics_tracker, palette_table, fragment_coverage, stuck_sweep_counter, last_drain_frame_context` |
| `Wire` (bundle_tx.rs, 10) | `reliable_emitter, packer, emitted_meta, pending_bundles, last_enqueued_was_short, datagram_send_errs, last_refused_bundle, datagram_send_err_first_logged, last_max_datagram_size, recent_bundles` |
| `FecTx` (block_tx.rs/repair_status.rs, 8) | `fec_mode, fec_live, block_table, fec_adapt, pending_repairs, block_tx, fec_in, offered_wire_bytes` |
| `LossPath` (bwe_glue.rs/transport_probe.rs, 12) | `transmission_ledger, delivery_sampler, departures, departures_handle, black_holes_seen, discarded_unstamped, queue_loss, loss_confirm, pending_losses, late_acked, pending_losses_retracted, loss_window` |
| `RateCtl` (pacing.rs/bwe_glue.rs/model_glue.rs, 14) | `bwe, bwe_epoch, bwe_samples_buffer, bwe_samples_at_session_reset, active_probe, probes_completed, probes_abandoned, admission, gate, admission_wake, tick_budget_floor_bytes, last_admission_term, model, bwe_log_last_at` |
| `H264Tx` (existing, h264_tx.rs, +7) | `h264` (becomes the struct itself) plus `last_h264_frame_seq, last_reset_request_idr_at, reset_requests_received, reset_requests_honoured, reset_requests_rate_limited, frame_acks_received, frame_acks_malformed` |
| `Telemetry` (transport_probe.rs/frame.rs, 15) | `transport_probe, instr, cumulative_datagrams_emitted, last_cumulative_emit_log_frame, color_histogram_accumulator, bump_count_accumulator, bytes_emitted_critical, bytes_emitted_refinement, bytes_emitted_h264, bytes_emitted_snapshot, critical_latency_stats, refinement_latency_stats, queued_critical_latency_stats, queued_refinement_latency_stats, ack_latency_tracker` |
| `StatsService` (stats_service.rs, 3) | `stats_sessions, stats_counters, stats_schema_seq` |
| `HarnessHooks` (cfg; LD-3, harness.rs, 17) | `inject_rx, outbound_loss, inbound_loss, ack_blackout, acks_blacked_out, frame_ack_loss, frame_acks_dropped, ignore_tile_nacks, outbound_bandwidth_cap, test_force_bytes_per_us, oob_inject_at, skip_palette_session_reset, drop_plan, inbound_read_log, hold_log, arm_picked, inline_peer_lookups` |
| replaced by `watch::Sender<BridgeSnapshot>` (LD-3, 8) | `send_errs_publish, h264_stats_publish, transport_probe_publish, bwe_publish, emitter_stats_publish, latency_stats_publish, queued_latency_stats_publish, probe_stats_publish` |
| `TestHooks` (`#[cfg(test)]`, 9) | `sent_log_for_test, forced_fifo_depth_for_test, forced_datagram_limit_for_test, forced_raw_datagram_size_for_test, loop_arm_trace, stats_capture_for_test, test_commit_dg_size, sent_frames_for_test, test_wire` |

Judgement calls an implementer should not reopen:
- `reliable_emitter` goes in `Wire`, not `TileTx`, because H.264 fragments go
  through it too (`submit_h264_fragment`).
- `adaptation_context` goes in `Mode`: bwe_glue writes it and the classifier
  reads it, and both now name `self.mode`.
- `epoch` stays on the core: eight files read it.

Sketch, not compiled (field types copied from the struct at `io_bridge.rs:514`):
```rust
// sketch
pub(super) struct TileTx {
    pub(super) scheduler: crate::transport::scheduler::Scheduler,
    pub(super) metrics_tracker: crate::tile::MetricsTracker,
    pub(crate) palette_table: crate::encoder::pal_rle::PaletteTable,
    pub(super) fragment_coverage: crate::transport::fragment_coverage::FragmentCoverageMap,
    pub(super) stuck_sweep_counter: u32,
    pub(super) last_drain_frame_context: Option<DrainFrameContext>,
}
```

**Rejected alternatives.**
- *Split IoBridge into actors or tasks with channels.* This changes scheduling
  and the stepped harness's determinism (see the "Stepped harness hides
  concurrency" note), so it is not a refactor.
- *Traits per subsystem over `IoBridge`.* This keeps the flat state. It renames
  the problem.
- *One big-bang PR.* Unreviewable, and it serialises every other group for its
  whole lifetime.

**Files touched.** `transport/io_bridge.rs` and every non-test file under
`transport/io_bridge/`, plus the eight `*tests*.rs` files there. Accessor
renames only in `ghostframe-e2e`, if a harness accessor's path changes (keep
the `IoBridge::*_for_test` method names stable to avoid that).

**Steps (Sonnet).** One PR per bullet. Every Phase 1 and 2 PR is a **pure
move**.
1. P1a `Sessions` + `Access` + `HostIo`. Add `struct Sessions` etc. in their
   module files. Move the fields, fix `base_with`, and rewrite `self.<f>` as
   `self.sessions.<f>` (sed per field name, then fix compile errors by hand).
   Tests: `b.<f>` becomes `b.sessions.<f>`.
2. P1b `TileTx` + `FramePath` + `Mode`.
3. P1c `Wire` + `FecTx` + `LossPath`.
4. P1d `RateCtl` + `Telemetry` + `StatsService`, plus the seven fields folded
   into `H264Tx`.
5. P2a leaf methods: `display_mode.rs` into `impl HostIo`, `model_glue.rs` into
   `impl RateCtl`, `stats_service.rs` into `impl StatsService`.
6. P2b leaf methods: `block_tx.rs` into `impl FecTx` (with `&mut Wire`,
   `&RateCtl`); `h264_rate.rs`/`h264_refs.rs` into `impl H264Tx`.
7. P2c `transport_probe.rs` into `impl Telemetry`/`impl LossPath`.
8. P3 field privacy per module, plus test accessors. One PR, or one per
   cluster if the diff is over about 1500 lines.

In every PR, do not reorder statements, merge methods or touch comments other
than field paths. The reviewer checks that `git diff -w --word-diff` shows only
`self.x` becoming `self.y.x`.

**Tests/verification.** G-move on every PR. For P3 also run `just lint` with
`-W unreachable_pub` locally to confirm nothing new leaked. Byte-identical
journal lines: the characterization test from LD-6 (`log_cumulative_emit`
golden) must stay green across P1d and P2c.

**Gates.** G-lib on each PR. The cbindgen diff must be empty.

**Risks.** Rebase pain against concurrent io_bridge work, which is why it is
scheduled last among the io_bridge-touching groups and split per cluster. The
borrow checker may reject a few call sites where a method on `IoBridge` holds
`&mut self.wire` and calls `self.method()`. Fix those by passing the sub-field
explicitly, not with a clone.

**Size.** L (8 PRs, about 1-2 days each including review).

---

## LD-2 (High): type-erased errors

**Impact.** Errors cannot be matched, so control flow depends on
`downcast_ref` and strings. The GPU errors are not `Send`, so the bridge
stringifies them. The FFI and the daemon cannot tell "no GPU" from "tailnet
refused".

**Status at e85fb005.**
- **58** `Box<dyn Error ...>` in `ghostframe-lib/src`, all non-test:
  `capture/gpu_pipeline/frame.rs` 34, `transport/quic.rs` 6 (including
  `type BoxErr` at :312), `capture/gpu_pipeline/mod.rs` 6, `capture/pipeline_builder.rs` 5,
  `transport/io_bridge.rs` 4 (`new` :1769, `new_with_frames` :1874, `run`
  :2400, `turn` :2420), `io_bridge/harness.rs:243` 1, `server.rs:114` 1,
  `gpu_pipeline/setup.rs:138` 1. The gpu_pipeline ones are
  `Box<dyn std::error::Error>`, not `Send`.
- **One** `thiserror` derive: `encoder/h264/backend.rs:16` `BackendError`.
  `thiserror` is already a dependency (`ghostframe-lib/Cargo.toml:58`).
- **14** `Result<_, String>`: `io_bridge/frame_backend.rs:173` (`TileOut::pixels`),
  `encoder/h264/va_sys.rs:339,377,387,401`, `param_sets.rs:34`,
  `gpu_pipeline/setup.rs:1060` (`choose_device`), `encoder/h264/harness.rs:386`,
  `openh264_sys.rs:409,411,430,442,523`, `vaapi_direct.rs:1035`.
- Control flow by downcast: `io_bridge/frame_backend.rs:500`
  `e.downcast_ref::<UnchangedRefused>()` turns a GPU error into
  `AnalyseError::Refused` versus `Failed`. `UnchangedRefused` is a hand-rolled
  error at `gpu_pipeline/mod.rs:589`, built at `frame.rs:589/597`. Two other
  downcasts are legitimate and stay: a panic payload
  (`frame_worker.rs:174`) and quinn's `HandshakeData` (`io_bridge.rs:3033`).
- `run()` turns the frame worker's death into a string (`fatal: Option<String>`,
  `io_bridge.rs:2535`, returned at :2627 as `e.into()`). `require_gpu_backend`
  (`io_bridge.rs:1339`) turns the GPU error into an `io::Error` with text.
- The binaries already use anyhow (`ghostframe-server/Cargo.toml:8`, e2e,
  bench). ghostframe-lib has anyhow as a dev-dependency only.

**Root cause.** `?` on heterogeneous sources (ash `vk::Result`, rcgen, rustls,
io, GhostbridgeError) was the shortest path. No seam types were ever defined.

**Design (chosen).** `thiserror` enums at the four module seams. The library
never uses `Box<dyn Error>` or anyhow; binaries keep anyhow.

| enum (module) | variants (sketch) | replaces |
|---|---|---|
| `GpuError` (`capture/gpu_pipeline/error.rs`) | `Vk { call: &'static str, result: ash::vk::Result }`, `Loader(String)`, `NoDevice(String)` (from `choose_device`), `Geometry { width: u32, height: u32, why: &'static str }`, `TooManyTiles { count: u32, max: u32 }`, `UnchangedRefused(String)`, `Import(String)`, `Shader(&'static str)` | the 46 in gpu_pipeline + pipeline_builder, and the `UnchangedRefused` struct |
| `QuicSetupError` (`transport/quic.rs`) | `Cert(rcgen::Error)`, `Tls(rustls::Error)`, `Crypto(String)` (quinn's `NoInitialCipherSuite`), `Config(String)` | `BoxErr` and the five constructors |
| `BridgeError` (`transport/io_bridge/error.rs`) | `NoGpu(GpuError)`, `Ghostbridge(GhostbridgeError)`, `Quic(QuicSetupError)`, `Io(std::io::Error)`, `ListenAddr(String)`, `FrameWorker(String)`, `WebServer(WebServerError)` | `new*`, `run`, `turn`, `step_until_idle`, `require_gpu_backend`, `fatal` |
| `ServerError` = `BridgeError` re-exported, or a newtype (`server.rs`) | as above | `GhostframeServer::new` |
| `LibLoadError` (`encoder/h264/`) | `NotFound { tried: Vec<String> }`, `Symbol(&'static str)`, `Version(String)` | `Result<_, String>` in `va_sys`/`openh264_sys` |

- `GpuError` must be `Send + Sync`. `vk::Result` is `Copy`, so it is.
- `AnalyseError` gets `impl From<GpuError>`, matching
  `GpuError::UnchangedRefused(_)` to `Refused`. The downcast goes, and
  `damage_tests.rs:758/791` assert with `matches!`.
- `vk::Result` sites use a helper `fn vk(call: &'static str) -> impl FnOnce(vk::Result) -> GpuError`
  (sketch), so the failing call stays named in the message. The current
  strings ("tile_count {} exceeds max_tiles {}") become `Display` impls with
  the same text, so journal lines do not change.
- `param_sets.rs:34` returns `syntax::ParseError`. `TileOut::pixels` keeps
  `Result<PixelSource, String>`: it is warning text handed across the worker
  boundary and then logged. Name it in a doc comment and leave it.

**FFI mapping (`ffi.rs`).** No C caller exists in the repo (see LD-7), so keep
the contract as it is: `gf_server_new` returns null and the other calls return
`-1`. The typed error is logged once with `tracing::error!(error = %e, kind =
e.kind_str())` before the null is returned. Optional, only if the owner keeps
the C API: distinct negative codes `-2` no GPU, `-3` tailnet, `-4` QUIC/TLS.
Owner decision; not in the default plan.

**Rejected alternatives.**
- *anyhow inside the library.* Still type-erased, and matching on it is
  downcasting again.
- *One crate-wide `Error` enum.* It couples GPU setup to QUIC. Seams keep each
  enum small.
- *Keep the gpu_pipeline errors and just add `+ Send + Sync`.* That fixes the
  stringification but not the downcast or the unmatchable "no GPU".

**Files touched.** `capture/gpu_pipeline/{mod,frame,setup}.rs`,
`capture/pipeline_builder.rs`, `transport/quic.rs`, `transport/io_bridge.rs`,
`io_bridge/{frame_backend,harness}.rs`, `server.rs`, `ffi.rs`,
`encoder/h264/{va_sys,openh264_sys,param_sets,vaapi_direct,harness}.rs`,
`ghostframe-server/src/commands/capture.rs` (if it matches on the error),
e2e call sites (`.expect` keeps working through `Debug`).

**Steps (Sonnet).**
1. LD-2a `GpuError`. Add it with a transitional `Other(String)` variant.
   Convert `pipeline_builder.rs`, then `setup.rs`, then `frame.rs`/`mod.rs`.
   Replace the downcast at `frame_backend.rs:500`. Delete `UnchangedRefused`
   and `Other`.
2. LD-2b `QuicSetupError`. Drop `BoxErr`.
3. LD-2c `BridgeError`. Convert `new`, `new_with_frames`, `run`, `turn`,
   `step_until_idle` and `require_gpu_backend`. Make `fatal` an
   `Option<BridgeError>`. Make `GhostframeServer::new` return it, and add the
   log line in `ffi.rs`.
4. LD-2d `LibLoadError` and the remaining `Result<_, String>`.

**Tests/verification.**
- New test: an unchanged frame refused by the GPU yields
  `AnalyseError::Refused`. Mutation check: map the `UnchangedRefused` arm to
  `Failed` and watch `damage_tests` fail.
- New test: `require_gpu_backend(Err(GpuError::NoDevice(..)))` gives
  `BridgeError::NoGpu`.
- The error texts are unchanged: grep the e2e for asserted substrings ("no
  usable GPU") and keep them.
- G-lib. `damage_tests` need a GPU, so run them under `gf-gpu-run`.

**Risks.** The `?` conversions in `new_inner` interact with LD-6's split and
UF's SAFETY work. Land UF's gpu_pipeline PRs first, then LD-2a, then LD-6's
gpu_pipeline splits.

**Size.** M (4 PRs, S-M each).

---

## LD-3 (High): harness plumbing in production types

**Impact.** Production structs and the hot loop carry test-only fields, cells
and duplicated cfg/not-cfg arms. This is about a fifth of IoBridge's fields,
and it is where "test feature leaks into the daemon" bugs live (see AGENTS.md's
workspace-release trap).

**Status at e85fb005.**
- `feature = "browserless-harness"` or `"test-loss-injection"` appears **212**
  times in `ghostframe-lib/src`: `io_bridge.rs` 76, `io_bridge/harness.rs` 36,
  `config.rs` 33, `io_bridge/h264_tx.rs` 10, `scheduler.rs` 8,
  `transmission_ledger.rs` 7, `ack_dispatch.rs` 5 and others. `io_bridge.rs`
  also has 47 `cfg(test)`.
- **Eight `*_publish: Arc<Mutex<..>>` cells** (`io_bridge.rs` struct; written
  every turn in `post_turn` at `io_bridge.rs:2842-2886`). Each one has a
  `*_publish_handle()` accessor in `harness.rs:357-463` or `bwe_glue.rs:474`. A
  ninth cell, `pass0_sent_handle`, lives in the scheduler (`harness.rs:469`,
  `scheduler.rs:183/1135`).
- Consumers: all eight handles and `pass0_sent_handle` are cloned once in
  `ghostframe-e2e/src/harness/browserless.rs:1109-1130`. They are read after
  abort (:1267-1307), and two are polled **mid-run** (`transport_probe_cell`
  :1236, `bwe_cell` :1735).
- Duplicated cfg/not-cfg arms: the `H264Report` build at `io_bridge.rs:2862-2869`
  (`frame_acks_dropped` / `acks_blacked_out` 0 arms), `now_std` defined twice
  with different visibility (`io_bridge.rs:277` `pub` under harness, `:285`
  `pub(crate)` otherwise), `SchedulerEmissionPolicy::is_test_route`
  (`io_bridge.rs:1369`), `session.rs:170/172`, `frame.rs:1884/1886` and 7
  `cfg(not(...))` in `config.rs`.
- Harness-only H.264 encode paths inside `h264_tx.rs`: fields `pcm_backend`,
  `oh_source`, `pcm_log`, `raise_mutated_off` (:434-449). Functions
  `apply_injected_h264` (:894, plus a `cfg(not)` stub at :909),
  `note_injected_h264_mode` (:918), `h264_pcm_log_handle` (:928),
  `apply_injected_pcm_refs` (:938, which carries **two** doc comments, the
  first stranded above the `#[cfg]`) and `apply_injected_openh264` (:1040).
- `inject_rx` (`io_bridge.rs:567`) and its `select!` arm (:2573) are compiled
  into the daemon unconditionally.
- What e2e/bench consume from the harness surface:
  `new_with_stream_for_test`, `new_with_frames_for_test`,
  `new_with_injection_for_test`, `new_stepped_for_test`, `step_until_idle`,
  `next_wake_for_harness`, `set_drop_plan`, `set_hold_log`,
  `set_inbound_read_log`, `set_ack_blackout`, `set_frame_ack_loss`,
  `set_ignore_tile_nacks`, `set_fec_mode_for_test`,
  `set_fec_frame_notch_for_test`, `set_frame_backend`,
  `set_gpu_frame_processor_for_measurement`, `set_openh264_lib_for_test`,
  `set_peer_gate_for_test`, `set_tick_budget_floor_bytes`,
  `use_model_controller_for_test`, the nine handles, `h264_pcm_log_handle`,
  `drain_injection_for_test`, `scheduler_peek_for_test`, `now_std`, and the
  types `InjectedFrame`, `InjectedH264`, `TileRequest`, `TileOut`,
  `FrameBackend`, `MemStream`/`mem_stream_pair`, `TransportProbe`,
  `H264Report`, `FecAdaptProbe`, `IdrCause`, `DropPlan`/`DropRule`/... (full
  list under LD-7).

**Root cause.** Each harness need added its own cell and setter where it was
needed. There was never one seam.

**Design (chosen).**
- **One field** `#[cfg(any(test, feature = "browserless-harness"))] hooks:
  HarnessHooks`, defined in `io_bridge/harness.rs`. It holds the 17 fields in
  the LD-1 table, the four H.264 harness fields and the scheduler's `pass0_sent`
  Arc. `test-loss-injection`-only knobs sit in a nested
  `#[cfg(any(test, feature = "test-loss-injection"))] loss: LossHooks`, because
  that feature is enabled without the harness by the container image (e2e
  Dockerfile).
- **One snapshot channel.** `snapshot_tx: watch::Sender<BridgeSnapshot>` lives
  inside `HarnessHooks`. `BridgeSnapshot { bwe: BweSnapshot, emitter:
  EmitterStats, h264: H264Report, transport: TransportProbe, send_errs: u64,
  latency: (TierLatencyStats, TierLatencyStats), queued_latency: (..), probes:
  (u64, u64) }` is `Clone`, built once per `post_turn` and published with
  `send_replace`. `watch::Receiver::borrow()` serves the mid-run pollers, and a
  receiver still reads the last value after the sender is dropped (`abort()`),
  which covers the after-abort readers. One accessor:
  `IoBridge::snapshot_rx() -> watch::Receiver<BridgeSnapshot>`.
- **`now_std` is always `pub(crate)`.** The harness gets it through
  `harness_api` (LD-7) as `pub use`. This deletes the duplicated pair.
- **Zero-arm fields go away.** `H264Report.frame_acks_dropped` and
  `acks_blacked_out` move to `LossHooks` and into the snapshot under the same
  cfg, so production never writes a placeholder 0.
- **Injected-frame code moves to `io_bridge/harness.rs`** (or a new
  `io_bridge/harness/h264.rs` if harness.rs passes about 900 lines).
  `inject_rx`, `InjectedFrame`, `InjectedH264` and the `turn()` inject arm
  become cfg-gated. The `cfg(not)` stub of `apply_injected_h264` is deleted.
  `apply_injected_frame` moves from `emit.rs:1023`.
- **`is_test_route`.** Make `SchedulerEmissionPolicy::TestCdf53All` itself
  `#[cfg(test)]`, so the `matches!` needs no `not(test)` arm.

**Rejected alternatives.**
- *A trait object `dyn BridgeObserver`.* That puts an indirect call in the hot
  loop of production for a test need.
- *Keep the cells and only group them.* The Arc/Mutex pattern and the "call
  before the move" ritual (documented at `browserless.rs:1100-1108`) stay.
- *Move the harness to its own crate.* It needs private bridge state; a
  feature-gated module is the right seam.

**Files touched.** `io_bridge.rs`, `io_bridge/{harness,h264_tx,bwe_glue,emit,
ack_dispatch,session,frame,bundle_tx,pacing}.rs`, `scheduler.rs`,
`ghostframe-e2e/src/harness/browserless.rs` (nine handles become one
receiver), and whatever reads `h264_pcm_log_handle` under
`ghostframe-e2e/tests/browserless_h264/`.

**Steps (Sonnet).**
1. LD-3a: add `BridgeSnapshot` + the watch. Keep the eight cells for one PR
   and have post_turn fill both. Switch `browserless.rs` to the receiver.
   Delete the cells and handles. Move the scene-result field reads to
   `snapshot.borrow()`.
2. LD-3b: introduce `HarnessHooks`/`LossHooks`. Move the 17 fields
   (**pure move** of field paths plus the cfg attribute consolidation).
3. LD-3c: move the injected-frame and H.264 harness paths into harness.rs and
   cfg-gate `inject_rx` and the types (pure move plus gating).
4. LD-3d: `now_std` visibility, the `is_test_route` variant and
   `config.rs`'s seven `cfg(not)` arms. Wherever an arm returns a constant,
   replace it with a `const fn` behind one cfg.

**Tests/verification.**
- G-move for 3b/3c.
- For 3a: DETERMINISM-DIGEST unchanged. The digest includes the counters read
  from these cells, so a dropped field shows.
- Add a unit test that publishes twice and asserts the receiver sees the
  second snapshot after the sender is dropped. This is the abort property.
  Mutation check: publish only when `bwe_samples_buffer` is non-empty (the
  bug the comment at `io_bridge.rs:2835` warns about) and the
  `browserless_bwe_*` scenes must go red.
- `ci/check-release-features.sh` (run by `just build-release`) must still
  pass. `strings target/release/ghostframe-server | grep -c InjectedH264`
  should print 0 after 3c (today the type is compiled in).

**Gates.** G-lib, plus the browserless suite 3 times (the scene-stall flake
note).

**Risks.** The `watch` clone cost is only under the harness. A mid-run reader
that held the `MutexGuard` across an await would now hold a `Ref`; there is
none today (checked lines 1236/1735).

**Size.** M.

---

## LD-4 (Medium): blanket `#[allow(dead_code)]`

**Status at e85fb005.**
- `encoder/mod.rs:7` `#[allow(dead_code)] pub(crate) mod h264;`. The comment
  says "the test and oracle surface of the syntax model" is unused in
  production.
- Item-level allows: `transport/delivery_sampler.rs:141` (`last_sent_at`, only
  written, :192), `transport/transmission_ledger.rs:308` (`recorded_at`, only
  written, :556), `io_bridge/session.rs:358` (`last_hello_from()` accessor,
  test-only callers in `tests.rs:368-424`), `io_bridge/bwe_glue.rs:217`
  (`BweSample`, for `owd_ms_lo16`, "kept for Phase 2": written at
  `ack_dispatch.rs:523-532` and never read), `io_bridge/frame.rs:3032`
  (`phase_b_encode_payloads`, test-only caller `tests.rs:2533`),
  `encoder/h264/openh264.rs:85,149` (`cfg_attr(not(test))`, fine),
  `capture/gpu_pipeline/mod.rs:283` (`queue_family_index`), `:516` (`size`),
  `:615/617` (`width`/`height`), and `tile/proptest_strategies.rs:30,56,73`
  (a test module, fine).

**Design.**
- Delete the module-level allow. Gate each item the compiler then flags with
  `#[cfg(any(test, feature = "browserless-harness"))]` when the harness or
  oracle uses it, or delete it when nothing does.
- Delete `owd_ms_lo16` and `BweSample`'s allow. There is no Phase 2
  delay-gradient consumer; goog_cc gets `depart_us`. Delete `last_sent_at` and
  `recorded_at`, or wire them into a log line if a reviewer wants them (default:
  delete).
- Gate `last_hello_from()` and `phase_b_encode_payloads` with `#[cfg(test)]`.
- gpu_pipeline fields: keep `queue_family_index` (UF may need it for a queue
  ownership transfer; ask UF), delete `size`/`width`/`height` if unused after
  UF.

Rejected: `#[expect(dead_code)]` everywhere. That records the problem instead
of fixing it.

**Steps (Sonnet).**
1. Remove `encoder/mod.rs:7`. Run `cargo check -p ghostframe-lib` with no
   features, then `--features browserless-harness`, then `--all-targets`.
2. For each warning, gate or delete, and record which in the PR body.
3. Do the item-level list above.

**Tests/verification.** clippy `--workspace --all-targets` (the gate, since a
test target that stops compiling vanishes from `cargo test`). The test count is
unchanged. `cargo check -p ghostframe-lib` with no features is warning-free.

**Gates.** `just lint`, `gf-gpu-run just ci-local`. No e2e needed unless a
field in gpu_pipeline is removed, in which case G-lib.

**Risks.** A harness-only item gated with `cfg(test)` instead of
`any(test, browserless-harness)` breaks the e2e build. clippy
`--workspace` catches that.

**Size.** S.

---

## LD-5 (Medium): tile coordinates narrowed with unchecked `as u8`

**Impact.** Silent aliasing of tiles on frames wider than 8192 px. A tile at
column 256 or above is sent, acknowledged, superseded and cached as column
`col - 256`. The result is corrupted pictures and wrong generation bumps, with
no error.

**Status at e85fb005.**
- The wire is `u8`: `ghostframe-protocol/src/protocol.rs:175,228,670`
  `pub tile_x: u8`. The sentinels are `EVICTION_SENTINEL_X/Y = 0xFE`
  (`eviction.rs:23`) and `FRAME_DIMENSIONS_SENTINEL_X/Y = 255`.
- Unchecked narrowing on the production path:
  - `io_bridge/emit.rs:497-502` and `:611-625` (cancel, bump,
    settle_superseded), `:664-665`
  - `io_bridge/frame.rs:732-733, 1650-1651, 2051-2052`
    (`(idx as u32 % cols) as u8`)
  - `io_bridge/frame_backend.rs:703-704`
  - `scheduler.rs:1276/1278` (`passes.len() as u8`, bounded by 14, fine)
- The only grid limit is `capture/gpu_pipeline/mod.rs:70`
  `MAX_FRAME_TILES = 16_384`, a tile **count** checked at
  `gpu_pipeline/frame.rs:353/467`. `TileGrid::new` (`tile/mod.rs:146`) is
  infallible. `ffi.rs:181` `MAX_FRAME_DIM = 16384` covers only the C API.
- **Reachable.** 8224x1080 is 257 x 34 = 8738 tiles, under 16384, so it passes.
  Column 256 wraps to 0. Two routes get there:
  - Display negotiation clamps a client's request only to the RandR hardware
    ceiling or `GHOSTFRAME_MAX_RESOLUTION`
    (`ghostframe-server/src/capture/x11/display.rs:1123`
    `clamp_ceiling`, tests use 16384) and to `MIN_MODE_*`
    (`io_bridge/display_mode.rs:51`). There is no grid ceiling.
  - Attach mode captures whatever the operator's X screen is. Four 2560-wide
    monitors give 10240x1440 = 320 x 45 = 14400 tiles, which passes.

  The sentinel pairs (254,254)/(255,255) need both axes over 8128 px. That is
  more than 65000 tiles, which `MAX_FRAME_TILES` already rejects, so only the
  wrap is live. Found by analysis; not reproduced.
- Cast census, non-test `transport/` (76 files): **426 integer `as` casts on
  373 lines** (`u64` 167, `u32` 80, `usize` 73, `u8` 50, `i64` 31, `i32` 13,
  `u16` 9, `i16` 3) and 119 float casts.

**Root cause.** The grid is `u32` cols/rows everywhere and is narrowed at
each emission site. Nothing states the wire's 8-bit limit where a frame size
enters.

**Design (chosen).**
- `TileCoord { x: u8, y: u8 }` (newtype in `tile/mod.rs`; in protocol if WP
  prefers). It is built only by `TileGrid::coord(idx) -> TileCoord` and
  `TileGrid::coords()`.
- `TileGrid::new` becomes `TileGrid::try_new(w, h) -> Result<TileGrid,
  GridTooLarge>` with `MAX_GRID_COLS = MAX_GRID_ROWS = 254`. 254 keeps every
  column below the 0xFE sentinel on both axes, which makes the "not a tile"
  pairs unrepresentable. That is 8128 px per axis.
- Internally, emission sites take `TileCoord`. The `as u8`s at the sites listed
  above disappear, and the scheduler, emitter and slots API take `u8`s from a
  `TileCoord`.
- **Where the size enters**, each check is explicit:
  1. `IoBridge::process_frame`: a frame whose grid fails `try_new` is dropped
     with one warn-once line (rate-limited, per size) and a counter
     `frames_refused_grid` on `cumulative emit`.
  2. `display_mode.rs` `clamp_and_align_mode`: add the grid ceiling
     (`min(ceiling, 8128)`), so a client is never granted a mode the server
     cannot send.
  3. `ghostframe-server` `clamp_ceiling`: the same min, so the ceiling sent in
     DISPLAY_INFO is honest.
  4. `ffi.rs` `MAX_FRAME_DIM` drops to 8128, or uses the shared constant.
  5. Attach mode: refuse at startup with a clear message when the screen is
     wider than 8128. Cropping is a product decision for the owner.
- Clippy: enable `#![warn(clippy::cast_possible_truncation)]` at module level,
  promoted to error under CI's `-D warnings`, in `transport/io_bridge/{emit,
  frame,frame_backend}.rs`, `transport/scheduler.rs`, `tile/` and
  `transport/protocol` consumers first. Fix those modules (`u8::try_from`,
  `TileCoord`, or `#[allow(..., reason = "bounded by ...")]`). Do not enable it
  crate-wide: 426 sites, mostly `u64`/`usize` widenings the lint does not flag
  but a reviewer would have to read anyway. After LD-1, extend module by
  module. There is no `[lints]` table in `Cargo.toml` today; inner attributes
  are enough and need no workspace change.

**Rejected alternatives.**
- *Widen the wire to u16.* That is a protocol change in four crates (AGENTS.md
  lists them) for a resolution nobody serves today. Owner decision if 8K+ wide
  is wanted later.
- *`try_from` at every site.* That is 20-odd fallible sites in hot loops for an
  invariant that is checked once.

**Files touched.** `tile/mod.rs`, `transport/io_bridge/{frame,emit,frame_backend,
display_mode}.rs`, `transport/scheduler.rs` (signatures), `ffi.rs`,
`ghostframe-server/src/capture/x11/display.rs` (+ attach start-up check),
docs/design if the limit is documented.

**Steps (Sonnet).**
1. Add `TileCoord`, `GridTooLarge`, `TileGrid::try_new`, `coord()`, and unit
   tests.
2. Convert the three `TileGrid::new` call sites in io_bridge and add the drop,
   counter and warn line.
3. Replace the `as u8` sites with `TileCoord`.
4. Add the display ceiling (lib + server) and the attach check.
5. Add the module-level clippy lints and fix them.

**Tests/verification.**
- New `TileGrid::try_new(8128, 1080)` is Ok and `(8129, 1080)` is Err; same
  for height.
- New io_bridge test: a 8224x64 frame is refused, counted, and no datagram
  names column 0 for it. Mutation check: drop the check and assert the test
  sees a tile at x=0 from the right edge.
- `clamp_and_align_mode(10000, 1000, (16384, 16384))` gives width ≤ 8128.
- G-lib. The e2e display_negotiation tests stay green.

**Risks.** The scheduler's `u8` API is already `u8`, so the signatures barely
move. The emit.rs and frame.rs edits collide with UF's FrameAnalysis work, so
land this after UF's frame_backend PR and before LD-1.

**Size.** S-M.

---

## LD-6 (Medium): monster functions

**Status at e85fb005.** Function bodies measured by brace matching, non-test:

| function | file:line | lines |
|---|---|---|
| `new_inner` | `capture/gpu_pipeline/setup.rs:138` | **886** (one `unsafe fn`, 67 `?`, 0 `Defer` guards) |
| `log_cumulative_emit` | `transport/io_bridge/frame.rs:2332` | **591** (11 `tracing::info!` events, about 328 field lines) |
| `record_shared_stages` | `capture/gpu_pipeline/frame.rs:2609` | 412 |
| `emit_tile_codec_frame` | `transport/io_bridge/frame.rs:1848` | 379 |
| `process_frame_with_imported` | `capture/gpu_pipeline/frame.rs:675` | 370 |
| `base_with` | `transport/io_bridge.rs:1476` | 278 (LD-1 shrinks it) |
| `drain_scheduler_into_quinn` | `transport/io_bridge/emit.rs:716` | 266 |
| `turn` / `post_turn` / `dispatch_feedback_bytes` | `io_bridge.rs:2420/2661/1990` | 239/228/226 |

New fact: `new_inner` **leaks on error**. Each of its 67 `?` returns early
without destroying the Vulkan objects created before it. `pipeline_builder.rs`
has `Defer` with `fail_point`/`inject_failure`/`released_count` test hooks,
and it is used 9 times in `gpu_pipeline/frame.rs` and never in `setup.rs`.

**Design per function.**
- **`log_cumulative_emit`.** Split it into one snapshot struct per journal line:
  `CumulativeEmitLine`, `DeliveryRateLine`, `H264FeedbackLine`,
  `FecInboundLine`, `FecBlocksLine`, `FecAdaptLine`, `H264StatsLine`,
  `H264RefsLine`, `LossPathLine`, `BumpCountsLine`, `ColorHistogramLine`. Use
  one declarative macro `stat_line! { name, target, "message", { field: Type
  = expr, ... } }` that generates the struct and its `fn log(&self)` from **one**
  field list, so a field cannot be added to the log and forgotten in the
  struct or the other way round. Keep one `tracing::info!` per existing event
  and the field order and names exactly as they are. AGENTS.md, `docs/design/
  h264-transport.md` section 8 and `ghostframe-e2e/tests/e2e.rs:2373` parse
  these names. The same structs feed `BridgeSnapshot` (LD-3) where the harness
  wants them. `log_cumulative_emit` becomes about 30 lines of
  `XLine::from_parts(..).log()`.
  - Rejected: one `Display` message string. It keeps the text under the fmt
    subscriber but loses structured fields for any other subscriber.
  - Rejected: `valuable`. A new dependency for the same result.
- **`new_inner`.** Use staged builders that own their objects:
  1. `VkContext::new()` (entry, instance, physical device via `choose_device`,
     queue family, logical device, command pool) implements Drop.
  2. `ComputeStage::build(&ctx, bindings, push_bytes, spirv)`, used for the
     eight-plus pipelines. `ComputeStage` already exists as a struct.
  3. `HostBuffer::alloc(&ctx, size)` for the about 15 buffers.
     `HostBuffer` already exists.
  4. `Descriptors::new(&ctx, &stages, &buffers)`.

  `new_inner` then assembles the result in about 80 lines. Each stage struct
  has an explicit `destroy(&ctx)`, and a `SetupGuard` holds what has been built
  so far and destroys it on early return. Do the split as a pure move first,
  with leaks unchanged; the guard is a separate fix PR. That PR's test: add
  `fail_point("setup.<stage>")` before each stage, and for each stage assert
  `released_count()` equals the number of objects created. The mutation check
  is to remove one guard and watch it fail.
- **`record_shared_stages`.** One `record_*` method per stage, following the
  existing "Stage 1.5" and "Stage 3/4a/4b/4c" comments: `record_compact_scan`,
  `record_palrle_index`, `record_palette_fold`, `record_cdf53_compact`,
  `record_cdf53_forward`. Each returns its piece of `SharedStageGuards`.
- **`process_frame_with_imported`.** Split into `prepare_frame_resources`,
  `record_frame` (calls `record_shared_stages`), `submit_and_wait` and
  `read_back_analysis`.
- **`emit_tile_codec_frame`.** Split into `classify_and_partition` (per-codec
  buckets), `encode_palrle_phase`, `encode_cdf53_phase`, `encode_solid_phase`
  and `dispatch_partition`. This lands with or after UF's FrameAnalysis
  change, which edits the same function's inputs.
- **`drain_scheduler_into_quinn`.** Split into `drain_budget`,
  `drain_critical`, `drain_refinement` and `account_drain`. Do this after LD-1
  P1b, so the parts take `&mut TileTx` and `&mut Wire`.

**Tests/verification.**
- Before splitting `log_cumulative_emit`, write a **characterization test**
  with `crate::tracing_test_support`. Build a bridge in a fixed state with every
  counter set to a distinct value, call `log_cumulative_emit`, and assert the
  captured events' field names, order and values against a golden string. It
  must pass on master unchanged. Break it once by renaming a field, to prove it
  bites. Then refactor.
- G-move (DETERMINISM-DIGEST unchanged) for the io_bridge splits.
- gpu_pipeline splits need a GPU: under `gf-gpu-run`, run
  `cargo test -p ghostframe-lib --lib capture::gpu_pipeline` (byte-exact CDF
  5/3 oracles, `fallback_tests`, `damage_tests`, `reference_tests`) with the
  same pass count before and after.
- G-lib including e2e, since the GPU path is the daemon's only path.

**Coordination with UF.** UF adds and edits SAFETY comments and unsafe blocks
in `gpu_pipeline/{setup,frame}.rs`. Order: UF first, then LD-2a (GpuError),
then LD-6 gpu splits, then the LD-6 leak-guard fix. A pure move after UF
carries the SAFETY comments with the code. A reviewer checks that no SAFETY
comment was dropped: `grep -c SAFETY` before and after must match.

**Size.** L overall: log_cumulative_emit M, new_inner M-L, the others S each.

---

## LD-7 (Medium): public surface far wider than its users

**Status at e85fb005.**
- `ghostframe-lib/src`: 969 `pub` item lines (fn/struct/enum/const/static/type/
  trait/mod/use), 974 `pub(crate)`, 606 `pub(super)`. `lib.rs:17-30` makes
  `capture, config, encoder, feedback, ffi, frame_damage, frame_pool, server,
  tile, transport` all `pub`. `transport/mod.rs` has 30 module lines, of which
  24 are `pub mod` and 4 are `pub(crate)`.
- **Header** `ghostframe-lib/include/ghostframe.h`: 5 functions
  (`gf_server_new/submit_frame/connected_session_count/wait_for_client/destroy`)
  and **92 `#define`s**. 78 of the defines come from `ghostframe-protocol`
  through `cbindgen.toml` `extra_bindings`. **14 leak from ghostframe-lib**:
  `BweWrapper_INITIAL_BPS` (`transport/bwe/mod.rs`), `DECODE_ERROR_SIZE` and
  seven `ERR_*` (`transport/decode_error.rs`), `DISPLAY_INFO_SIZE`,
  `DISPLAY_MODE_SIZE` (`transport/display.rs`), `COVERAGE_INLINE_CAPACITY`,
  `FRAGMENT_COVERAGE_CAPACITY` (`transport/fragment_coverage.rs`) and
  `CACHE_CAPACITY` (`transport/reliable_emitter/mod.rs`).
  `FEC_GROUP_SIZE_K` and `END_OF_STREAM_PARITY_FLUSH_MS` from the old review
  are gone.
- `ERR_*`/`DECODE_ERROR_SIZE`/`DISPLAY_*_SIZE` are wire constants that sit in
  the lib. `ghostframe-client-core/src/decode_error_batcher.rs:36` redefines
  `DECODE_ERROR_SIZE`. They belong in `ghostframe-protocol` (WP's territory).
- **Who uses ghostframe-lib from outside.** Every `ghostframe_lib::` path was
  extracted by script, with use-trees expanded:
  - `ghostframe-server` (29): `GhostframeServer, FrameSubmission, FramePool,
    FrameBuf, FrameStorage, PoolSlot, Content, FrameDamage, TileBits,
    FeedbackKind, FrameFeedback, GhostbridgeConfig, GhostbridgeError,
    WebServerError, config::{LibConfig::from_env, AccessConfig,
    test_knobs_compiled}, tile::TILE_SIZE, transport::display::{DisplayController,
    DisplayError}, transport::input_inject::InputInjector,
    transport::ghostbridge::{GhostbridgeHandle, UP_NEEDS_LOGIN_RC,
    LOGIN_URL_MACHINE_AUTH_RC}`.
  - `ghostframe-client-net` (dev-dependency only, 12): `config::AccessConfig,
    transport::peer_auth::{AccessPolicy, DenialLog, PeerGate, PeerIdentity,
    PeerIdentitySource, PeerLookup, WhoIsError},
    transport::quic::{QuicServer, CertClock},
    transport::webtransport::{WebTransportServer, StatsWrite}`.
  - `ghostframe-cli`: none.
  - `ghostframe-e2e` (98, under `browserless-harness`): `transport::io_bridge::{IoBridge,
    Analysed, AnalyseError, Cdf53Encoded, Control, FecAdaptProbe, FrameBackend,
    H264Report, IdrCause, InjectedFrame, InjectedH264, MemStream,
    mem_stream_pair, now_std, OwnedPalRle, PixelSource, TileOut, TileRequest,
    TileTimings, TransportProbe, block_k_bucket_range}`,
    `transport::transmission_ledger::{TransmissionLedger, Transmission,
    LossReason, ReoBranch, Resolution}`,
    `transport::reliable_emitter::{EmitKey, emitter::EmitterStats,
    emission_queue::WireClass}`, `transport::scheduler::{TileWork, WorkState}`,
    `transport::bwe::{BweSnapshot, set_inherent_loss_guard}`,
    `transport::drop_plan::*`, `transport::model_controller::{model_window,
    RTT_WINDOW}`, `transport::instruments::BlockRepairSplit`,
    `transport::origin::OriginPolicy`, `transport::peer_auth::*`,
    `transport::quic::QuicServer`, `transport::webtransport::WebTransportServer`,
    `transport::ghostbridge::*` (framing, UdpPacketConn, MAX_FRAME_LEN),
    `transport::protocol::Codec`, `config::{LibConfig, ClassifierConfig,
    FecBlockParams, FecMode, ModelSettings, TransportConfig::from_env,
    datagram_send_buffer_bytes, AccessConfig}`,
    `capture::gpu_pipeline::GpuFrameProcessor`,
    `encoder::{cdf53::*, h264_harness::*}`,
    `tile::{CodecState, FrameMode, classifier::{Classifier, AdaptationContext}}`,
    `framing::*`, `FrameSubmission`, `FrameDamage`, `Content`.
  - `ghostframe-bench` (36, **no features of its own**; it gets the harness
    features only through feature unification with its `ghostframe-e2e`
    dependency): `encoder::{cdf53, pal_rle, solid}` (re-exports of
    ghostframe-protocol), `transport::protocol::*` (re-export),
    `transport::scheduler::{Scheduler, TileWork, WorkState}`,
    `transport::reliable_emitter::{ReliableTileEmitter, EmitKey,
    traits::{DatagramSender, SendOutcome}}`, `transport::ghostbridge::encode_frame*`,
    `tile::{BPP, DirtyTracker, TILE_SIZE}`.
  - `ghostframe-lib/tests/{bwe_bench,gpu_pipeline}.rs` (integration tests, 8):
    `transport::bwe::{BweWrapper, AckArrival, ProbeRequest}`,
    `transport::reliable_emitter::cache::ProbeTag`,
    `transport::protocol::{decode_frame_datagram, fragment_frame}`,
    `capture::gpu_pipeline::{GpuFrameProcessor, IngestTier}`.
- **The C ABI.** `crate-type = ["cdylib", "staticlib", "rlib"]`
  (`ghostframe-lib/Cargo.toml:8`). No C, Go or other caller of `gf_server_*`
  exists in the repo. `ghostframe-server` uses `GhostframeServer` from Rust, and
  `gf_server_new` appears only in old `docs/superpowers/plans`. The header is
  kept alive by `build.rs:119` and its diff gate (`Justfile:314`,
  `.github/workflows/ci.yml:229`).

**Design (chosen).**
1. `pub(crate)` is the default for every module in `lib.rs` and
   `transport/mod.rs`. Mechanism: set the module to `pub(crate)`, add
   `#![warn(unreachable_pub)]` to `lib.rs`, run `cargo fix --lib -p
   ghostframe-lib --allow-dirty` (the lint's suggestion is machine-applicable),
   then hand-fix the rest.
2. **Public API** = the 29 server items plus the 12 client-net items, as
   `pub use` in `lib.rs` (`pub mod config`, `pub mod display`, and so on,
   re-exported flat or as small facade modules). Nothing else.
3. **`#[cfg(feature = "browserless-harness")] pub mod harness_api`** re-exports
   the e2e list above (and `now_std`).
4. **Bench**:
   - Point bench at `ghostframe-protocol` directly for codec/protocol items.
   - Give it an explicit `bench-api` feature on ghostframe-lib (or reuse
     `browserless-harness`; owner's pick) for `Scheduler`,
     `ReliableTileEmitter`, `DatagramSender`, `DirtyTracker`.
   - Stop relying on unification.
5. Move `ghostframe-lib/tests/{bwe_bench,gpu_pipeline}.rs` into `src/`
   unit-test modules, or have them use `harness_api` with
   `required-features`. Default: move them in, because `required-features`
   targets need a CI listing (the "CI runs integration tests by explicit name"
   note).
6. The header shrinks to the 5 functions plus what stays `pub`.
   `BweWrapper_INITIAL_BPS`, `CACHE_CAPACITY` and the coverage capacities
   become `pub(crate)`. The wire constants move to ghostframe-protocol (WP).

**Owner decision on the C API.** Recommended:
- **(a) delete it**: `ffi.rs`, cbindgen config, `build.rs` cbindgen part,
  header, the `cdylib`/`staticlib` crate types and the header gates. This gives
  `ghostframe-lib` an rlib-only build, which is also what group EC wants.
- **(b)** if embedding the server from C is a goal: move `ffi.rs` into a new
  `ghostframe-server-capi` crate (AGPL, mirroring `ghostframe-client-capi`),
  which owns the header and the cbindgen gate. Drop `extra_bindings` there
  unless an embedder needs wire constants.

Either way ghostframe-lib stops being a cdylib. Note that a new workspace
member must be added to the e2e Dockerfile's per-crate manifest COPY list.

**Rejected alternatives.**
- *Keep `pub` and add `#[doc(hidden)]`.* Changes nothing for the header or for
  semver hygiene.
- *Separate `ghostframe-lib-harness` crate.* It needs private state, same as
  LD-3.

**Steps (Sonnet).**
1. Add facade `pub use`s and `harness_api`, and switch e2e/bench/client-net to
   them (paths only).
2. Flip modules to `pub(crate)` and run cargo fix for `unreachable_pub`.
3. Move the integration tests.
4. Regenerate the header and commit it.
5. Do the C API option the owner picks, as its own PR.

**Tests/verification.**
- clippy `--workspace --all-targets` (catches every external break, including
  test targets).
- `cargo check -p ghostframe-bench --no-default-features` on its own, which
  proves bench no longer leans on unification.
- `ci/check-licenses.py`.
- The header diff is the intended one. `ci/check-release-features.sh` passes.
- G-lib.

**Risks.**
- `dead_code` floods once items stop being `pub`. Expected; it overlaps LD-4,
  so do LD-4 first.
- A missed e2e path fails only the e2e crate's build, which clippy
  `--workspace` sees.

**Size.** M (plus S for the C-API PR).

---

## LD-8 (Low/Med): OnceLock config globals read deep

**Status at e85fb005** (`config.rs`):
- `emitter_config()` :949, read at `reliable_emitter/pto.rs:37`.
- `vaapi_device_override()` :961, read at `encoder/h264/vaapi_direct.rs:48`.
- `ack_order_probe_enabled()` :1003 and `rto_probe_enabled()` :1010, both
  re-wrapped at `reliable_emitter/mod.rs:98/107`. They are read at
  `transmission_ledger.rs:1331`, `ack_dispatch.rs:313/326/346/358` and
  `emitter.rs:1069/1264/1269`.
- `no_rto()` :1026, test-gated with a `false` twin at :1032, read at
  `emitter.rs:1494`.
- `datagram_send_buffer_bytes()` :984 is deliberately uncached and read by
  `quic.rs:249`.
- `config.rs:1062` carries its own `ENV_LOCK`.
- `LibConfig` (`config.rs:863`) already holds `classifier, transport,
  diagnostics, access` and is threaded through `IoBridge::new_with_frames`.

**Design.**
- Move `EmitterConfig` into `TransportConfig`. Move `rto_probe`,
  `ack_order_probe` and (test-gated) `no_rto` into `DiagnosticsConfig`. Move
  `vaapi_device: Option<String>` into the H.264 config that already carries
  `H264BackendChoice`/`openh264_lib` into `H264Tx`.
- Construction threads them: `ReliableTileEmitter::new(t0, &EmitterConfig,
  probes)` stores a copy, and `pto_for_attempt` becomes a method using it
  (`pto_for_attempt_with` already exists). `TransmissionLedger::new` takes
  `rto_probe: bool`. The libva-direct open receives the device path.
- Delete the OnceLocks. The free functions survive only as `from_env` parsers.
- Rejected: keep the OnceLocks with a test override. That is still global and
  still needs the lock.

**Files.** `config.rs`, `transport/reliable_emitter/{mod,emitter,pto}.rs`,
`transport/transmission_ledger.rs`, `transport/io_bridge/ack_dispatch.rs`
(reads `self.diagnostics`), `encoder/h264/vaapi_direct.rs`, `loss_path.rs`
(`ReliableTileEmitter::new` call sites, 3 in tests), `io_bridge.rs` `base_with`.

**Steps.**
1. Add the fields, parsed in the existing `from_env`/`from_lookup`.
2. Thread them through the constructors.
3. Replace the call sites.
4. Delete the globals.
5. Convert any test that set env vars for these to set config fields.

**Tests/verification.**
- New: two emitters with different `EmitterConfig` in one process give
  different PTOs. That was impossible before.
- The env-read guard (`ci/check-env-reads.sh`) passes.
- G-lib. The e2e `GHOSTFRAME_RTO_PROBE`/`NO_RTO` behaviour is unchanged
  (the container still sets the env, and `from_env` reads it).

**Risks.** `no_rto` must stay compiled out of release (`test-loss-injection`
gating). Keep the field `#[cfg]`, not just its parser.

**Size.** S. Independent of LD-1; can go early (touches only `base_with` in
io_bridge.rs).

---

## LD-9 (Low): two clocks inside IoBridge

**Status at e85fb005.** `std::time::Instant::now()` in non-test bridge state:
- `io_bridge/session.rs:296, 378, 460, 601` stamp `session_started_at` and
  `last_disconnect_at` (fields `io_bridge.rs:749/764`). These are real-clock
  values, used for `gap_secs`/`session_secs` log fields.
- `io_bridge/stats_service.rs:155` computes `uptime_s` from that real-clock
  stamp. The comment says "Real clock on purpose".
- Inside the same struct, `last_takeover_at` (`:766`) is stamped with
  `now_std()` (`session.rs:267/285`). Three `Option<Instant>` fields on one
  struct carry two clocks and nothing in their types says which.
- `transport/bandwidth_cap.rs:22/29`. `BandwidthCap` (test-loss-injection)
  refills on the wall clock, so under a paused tokio clock it refills at the
  wrong rate.
- `fec_tx/block_table.rs:510` seeds `now: Instant::now()`, then
  `self.now = self.now.max(now)` (:717) with bridge-clock values. Under a
  paused clock the seed can be ahead of virtual time, which delays tombstone
  ageing.
- Deliberate real-clock CPU probes (correct; keep):
  `io_bridge/frame_worker.rs:277/306/343/379`, `io_bridge/frame.rs:297`
  (already has an `allow` with reason at :325), `:147/2144/2183`,
  `io_bridge/frame_backend.rs:426/428` and `encoder/h264/vaapi_direct.rs:1054/1105`.
- `clippy.toml` already disallows `std::time::Instant::elapsed` workspace-wide,
  and 14 sites carry `allow(clippy::disallowed_methods, reason = ...)`.

**Design.**
- Bridge state uses `now_std()` only. `session.rs` and `stats_service.rs` switch
  to it: `gap_secs`, `session_secs` and uptime then follow virtual time in
  scenes and are identical in production.
- `BandwidthCap::new(bps, now)` and `try_consume(.., now)` take the clock from
  the caller (`bundle_tx.rs:406`).
- `BlockTable::with_caps` takes `now`.
- Enforcement: add `ghostframe-lib/clippy.toml`. Clippy reads the nearest
  `clippy.toml` from the package directory and does **not** merge it with the
  root's, so it must repeat the `Instant::elapsed` entry. Add
  `{ path = "std::time::Instant::now", reason = "bridge state is on tokio's
  clock: use now_std(); real-clock CPU probes allow with a reason" }`. Then add
  `#[allow(..., reason)]` to the CPU-probe sites above (about 12) and to
  `transport/quic_pair.rs:57`, `server.rs:151` and `tile/classifier.rs:418` after
  checking each one.
- Rejected: a newtype `BridgeInstant`. It is heavier and touches every
  signature in the transport.

**Files.** `io_bridge/{session,stats_service,bundle_tx}.rs`,
`transport/bandwidth_cap.rs`, `fec_tx/block_table.rs`, new
`ghostframe-lib/clippy.toml`, `allow`s at the probe sites.

**Tests/verification.**
- New test under `start_paused`: advance 10 s, disconnect, and assert the logged
  `session_secs` ≈ 10. Mutation check: revert to `Instant::now()` and it reads
  ≈ 0.
- `BandwidthCap` unit test with an explicit clock.
- `just lint`. `ci/check-env-reads.sh` is unaffected.
- G-lib.

**Risks.** A per-crate clippy.toml silently drops any future root entry.
Mitigation: add a comment in both files saying they must stay in sync, and a
`ci/` check that the root entries are a subset (a few lines of shell).

**Size.** S.

---

## LD-10 (Low): comments narrating history

**Status at e85fb005.** In non-test `ghostframe-lib/src`, a broad pattern gets
**350** comment lines: "Stage N" 139, "M3.x" 75, "no longer" 66, "used to" 32,
"Phase N" 20, "as before" 7, "previously" 5, "legacy" 5, "kept for" 5. A
history-only pattern (`used to|previously|formerly|was removed|until now|Phase
N|M3.x`) gets **131**.
- Heaviest files: `capture/gpu_pipeline/frame.rs` 51,
  `transport/io_bridge.rs` 36, `gpu_pipeline/mod.rs` 32, `io_bridge/frame.rs`
  28, `transport/scheduler.rs` 18.
- 18 comments cite `docs/...` paths. All exist today, but `docs/superpowers` is
  slated for deletion (end-of-waves cleanup).
- Concrete stale item: the double doc comment on `apply_injected_pcm_refs`
  (`io_bridge/h264_tx.rs:1031-1043`).
- "no longer" and "Stage N" are often legitimate: "is no longer outstanding",
  or a stage of the current GPU pipeline. They need judgement, not deletion.

**Design.** A mechanical sweep with explicit rules for a Haiku implementer,
reviewed by a human:
1. Rewrite to present tense: comments that say what code *used to* do
   ("used to", "previously", "formerly", "until now", "as before", "was
   removed") either state the current behaviour or are deleted. Keep the
   history clause only if it explains a non-obvious constraint, rephrased as
   the constraint ("must not X, because Y").
2. Replace milestone tags (`M3.x`, `Phase N`, `Wave X`, `PR #n`, `(PR-xx)`) with
   the design doc section or remove them. Never point at `docs/superpowers/*`.
3. Keep "Stage N" only where it names a current pipeline stage defined in one
   place (`gpu_pipeline/mod.rs`); otherwise remove it.
4. Never touch code, `SAFETY:` comments, or log message strings.

Scope: `ghostframe-lib/src` non-test files, one PR per top-level module
(`capture/`, `transport/io_bridge*`, `transport/` rest, `encoder/`+`tile/`).
Run it **last** in the LD stack, after LD-1/LD-6 have moved the code, so the
sweep does not conflict with the moves.

**Verification.** `git diff` contains only comment lines: check with
`git diff -U0 | grep '^[+-]' | grep -vE '^\+\+\+|^---' | grep -vE '^[+-]\s*(//|\*|$)'`,
which must print nothing. `cargo fmt --check` and `just lint`.

**Size.** M (mechanical, 4 small PRs).

---

## LD-11 (Medium): H.264 syntax duplicated in server and client

**Status at e85fb005.**
- Server: `ghostframe-lib/src/encoder/h264/syntax.rs`, 1824 lines, AGPL.
  `BitWriter` :202 (`put_bit/put_bits/put_ue/put_se/bit_len/into_bytes`),
  `ParseError` :253, `BitReader` :263, `escape_rbsp` :311, `unescape_nal` :326,
  `split_annexb` :342, `skip_scaling_list` :686, `parse_sps` :700,
  `parse_pps` :798, plus the slice-header writer/parser and the `DecoderModel`
  (server-only).
- Client: `ghostframe-client-h264/src/bitstream.rs`, 842 lines, MIT OR
  Apache-2.0. `nal_spans` :59, `unescape` :88, `escape` :103, `BitReader` :117
  (returns `Option`), `BitWriter` :168, `skip_scaling_list` :232, `skip_hrd`
  :245, `parse_sps` :261, `parse_pps_ids` :386, plus `low_latency_sps` and
  `RefContinuity` (client-only).

**Behavioural differences that must be reconciled** (diffed by reading):

| primitive | server | client | reconcile to |
|---|---|---|---|
| error style | `Result<_, ParseError{Truncated, NotASlice, Unsupported(&str)}>` | `Option<_>` ("no opinion") | `Result<_, ParseError>`; the client maps it with `.ok()` |
| `ue()` max prefix | refuses more than 32 leading zeros, so accepts 32 (value `u32::MAX`) | refuses more than 31 | 31: a 32-zero code is only `u32::MAX`, and no element read here can be that large |
| `bits(n)` | `u64`, any n | `u32`, `debug_assert!(n <= 32)` | `u64` with `n <= 64` asserted, plus a `bits_u32` helper |
| `BitWriter` | `put_bits`, `put_se`, `bit_len`; `into_bytes` without trailing bits | `ue` and `bit` only; `finish()` appends `rbsp_trailing_bits` | union: both `into_bytes()` and `finish_rbsp()` |
| `escape`/`unescape` | same algorithm | same algorithm | identical; neither appends `03` after a trailing `00` at RBSP end (spec 7.4.1). Keep the behaviour, add a test that documents it |
| `split_annexb` vs `nal_spans` | returns `&[u8]` slices | returns `NalSpan{start,end}` | spans as primitive, slices as a thin wrapper |
| `skip_scaling_list` | `(last + se + 256).rem_euclid(256)` | `(last + se + 256) % 256` (negative remainders possible) | `rem_euclid`, and **range-check `delta_scale` to -128..=127 first**: in both, `last + se + 256` can overflow `i32` on a hostile SPS (a debug-build panic) |
| `parse_sps` NAL type | checks `nal[0] & 0x1f == 7` | does not check | check |
| `parse_sps` `log2_max_frame_num` | `checked_add(4)`, so overflow-safe | `r.ue()? + 4`: **overflows** for ue ≥ 2^32-4 (debug panic, release wraps to a small value that passes `<= 16`) | `checked_add` (a client bug on a crafted stream; fix in the shared code) |
| `parse_sps` POC type 1 | rejected as `Unsupported` | parsed (offset loop, cycle ≤ 255) | parse it (client's); the server's model then rejects type 1 itself |
| interlace | rejects `frame_mbs_only == 0` | accepts, reads `mb_adaptive` | parse it; the server's model rejects it |
| chroma | rejects ≠ 4:2:0 and separate planes | records `separate_colour_plane` | parse fields; policy stays in the callers |
| cropping and size | parsed, `width`/`height` computed with checked math | skipped | parse it |
| VUI | not parsed | parsed (`bitstream_restriction`, `skip_hrd`, `mdfb_bit`) | parse it |

**Design (chosen).** A new pure crate **`ghostframe-h264-syntax`**, licensed
**MIT OR Apache-2.0**, with no dependencies beyond `thiserror`. It contains the
primitives only:
- `BitReader`, `BitWriter`, `ParseError`
- `escape_rbsp`, `unescape_nal`, `nal_spans`/`split_annexb`
- `skip_scaling_list`, `skip_hrd`
- a full `parse_sps` returning one `Sps` that is the union of both structs'
  fields, including `restriction` and `mdfb_bit`
- `parse_pps` (the server's fields; `parse_pps_ids` becomes a projection)

The slice-header writer/parser and `DecoderModel` stay in ghostframe-lib
(AGPL). `low_latency_sps` and `RefContinuity` stay in client-h264. Both crates
depend on the new one. `ci/check-licenses.py` allows that direction (AGPL can
depend on permissive, never the reverse).

- **Licensing.** The shared code must be MIT/Apache because the client uses it.
  Building it from the **client's** implementation (already MIT/Apache) plus
  the server's extra parsing needs the owner to relicense those server lines
  from AGPL. The owner is the author, so this is possible but is the
  **owner's decision**. Fallback if refused: build the crate only from the
  client's code and port the server's crop/PPS parsing into it by clean
  re-implementation from the spec.
- **Rejected:** a module in `ghostframe-protocol`. That crate is the ghostframe
  wire format and compiles into the wasm client, which never parses H.264
  (WebCodecs does). H.264 syntax is not ghostframe protocol.
- **Rejected:** the client depends on ghostframe-lib. That is forbidden by the
  licence guard.
- A new workspace member must be added to the e2e test-server Dockerfile's
  per-crate manifest COPY ("New workspace member breaks the e2e image") and to
  `Cargo.toml` members.

**Files.** New `ghostframe-h264-syntax/{Cargo.toml,src/lib.rs}`, root
`Cargo.toml`, `ghostframe-lib/src/encoder/h264/syntax.rs` (delete primitives,
`use` the crate), `ghostframe-client-h264/src/bitstream.rs` (same),
`tests/containers/test-server/Dockerfile`, `ci/check-licenses.py` (no change
expected; run it), and `DEVELOPERS.md` crate table.

**Steps (Sonnet).**
1. Create the crate with the primitives and the reconciled behaviour from the
   table. Move the round-trip proptests (`syntax.rs:1347-1368`) and the
   client's tests (`bitstream.rs:676-700`) into it.
2. Switch the client and run its tests.
3. Switch the server and run the lib tests. `pcm_oracle_tests`, `observed_tests`
   and `openh264_fixture_tests` exercise `parse_sps`/`split_annexb` on real
   streams.
4. Delete the duplicates.

**Tests/verification.**
- New tests in the crate: `ue` with 32 leading zeros is refused; a
  `log2_max_frame_num_minus4 = u32::MAX - 1` SPS returns `Err` and does not
  panic; `delta_scale` out of range returns `Err`; POC type 1 and interlaced
  SPS parse; the real x264 IDR fixture (`syntax.rs:1512`) parses identically
  through the new `parse_sps`.
- Mutation check: revert `checked_add` to `+` and run the test under debug,
  which must panic.
- `just ci-client` (the client side) and G-lib (the server side; H.264 e2e is
  local-only per `ci/skip-list.txt`, so also run
  `just test-openh264 <path>` once).

**Risks.** The server's `parse_sps` currently *rejects* POC type 1 and
interlaced streams. Moving that policy into callers must keep
`StreamParams::from_sps_pps` (`syntax.rs:845`) rejecting them. Add the
assertion test before the switch.

**Size.** M.

---

## Suggested PRs for LD

File ownership is per stack. Within a stack the order is strict. Across stacks
it follows the "after" column.

**Stack LD-A, independent of io_bridge** (can start immediately, in parallel
with AV/WP/UF):

| # | branch | scope | owns | after |
|---|---|---|---|---|
| 1 | `ld/dead-code` | LD-4 | `encoder/mod.rs`, `encoder/h264/*` allows, `delivery_sampler.rs`, `transmission_ledger.rs` (2 fields), `bwe_glue.rs` `BweSample` | none |
| 2 | `ld/config-threading` | LD-8 | `config.rs`, `reliable_emitter/*`, `transmission_ledger.rs`, `vaapi_direct.rs`, one hunk in `io_bridge.rs base_with` | 1 |
| 3 | `ld/h264-syntax-crate` | LD-11 (owner OK on licensing first) | new crate, `encoder/h264/syntax.rs`, `client-h264/src/bitstream.rs`, Dockerfile | none |
| 4 | `ld/quic-errors` | LD-2b | `transport/quic.rs` | none |

**Stack LD-B, gpu_pipeline** (after UF's gpu_pipeline unsafe/SAFETY PRs merge):

| # | branch | scope | after |
|---|---|---|---|
| 5 | `ld/gpu-error` | LD-2a (`GpuError`, removes the downcast in `frame_backend.rs:500`) | UF gpu PRs; touches `frame_backend.rs` one hunk, so also after UF FrameAnalysis |
| 6 | `ld/gpu-setup-split` | LD-6 `new_inner` staged builders (**pure move**) | 5 |
| 7 | `ld/gpu-setup-guard` | LD-6 leak guard + fail-point test (behaviour fix) | 6 |
| 8 | `ld/gpu-frame-split` | LD-6 `record_shared_stages`, `process_frame_with_imported` (**pure move**) | 6 |

**Stack LD-C, io_bridge** (serialised; starts only after **AV security fixes in
io_bridge.rs/session.rs are merged**, **WP's wire-constant moves** (decode_error,
display, ack datagrams) are merged and **UF FrameAnalysis** has landed in
`frame_backend.rs`/`frame.rs`. While LD-C is open, other groups' io_bridge
changes rebase onto it, not the reverse):

| # | branch | scope | pure move? |
|---|---|---|---|
| 9 | `ld/bridge-error` | LD-2c `BridgeError`, `GhostframeServer::new`, ffi log line | no (small) |
| 10 | `ld/tile-coord` | LD-5 `TileCoord`, grid limit, display ceiling (+ server `clamp_ceiling`), cast lint in 5 modules | no |
| 11 | `ld/bridge-clock` | LD-9 `now_std` in session/stats, BandwidthCap/BlockTable clock param, `ghostframe-lib/clippy.toml` | no (small) |
| 12 | `ld/harness-snapshot` | LD-3a watch `BridgeSnapshot` (+ e2e browserless.rs) | no |
| 13 | `ld/harness-hooks` | LD-3b/3c `HarnessHooks`, injected paths into harness.rs | yes (plus cfg gating) |
| 14 | `ld/harness-cfg-arms` | LD-3d `now_std` visibility, `is_test_route`, config `cfg(not)` arms | no (small) |
| 15 | `ld/emit-log-golden` | LD-6 characterization test for `log_cumulative_emit` (test only) | n/a |
| 16 | `ld/emit-log-lines` | LD-6 `stat_line!` structs | yes (golden green) |
| 17-20 | `ld/iob-group-{session,tile,wire,rate}` | LD-1 P1a-P1d | **yes** |
| 21-23 | `ld/iob-leaf-{host-rate-stats,fec-h264,probe}` | LD-1 P2a-P2c | **yes** |
| 24 | `ld/iob-split-emit` | LD-6 `emit_tile_codec_frame`, `drain_scheduler_into_quinn` | yes |
| 25 | `ld/iob-privacy` | LD-1 P3 | no (visibility) |

**Stack LD-D, surface and comments** (last):

| # | branch | scope | after |
|---|---|---|---|
| 26 | `ld/pub-surface` | LD-7 facade, `harness_api`, `pub(crate)` default, bench feature, header regen | 25, 1 |
| 27 | `ld/c-api` | LD-7 owner choice: delete or move to `ghostframe-server-capi` (coordinate with EC's crate-type change; one of the two PRs does it, not both) | 26 |
| 28-31 | `ld/comments-{capture,io-bridge,transport,encoder-tile}` | LD-10 sweep (Haiku) | 25, 8 |

**Sequencing against other groups, explicitly.**
- **AV first.** The security fixes in `io_bridge.rs`, `session.rs` and
  `ack_dispatch.rs` are small and urgent. They merge before #9. LD-C then
  rebases onto them; they never rebase onto LD-C.
- **WP before #9.** Moving `ERR_*`, `DECODE_ERROR_SIZE` and `DISPLAY_*_SIZE`
  into ghostframe-protocol is WP's job. It is also what removes those defines
  from the lib's header leak (LD-7 then only has to handle
  `BweWrapper_INITIAL_BPS`, `CACHE_CAPACITY` and the coverage capacities).
- **UF before #5 and before #10/#24.** UF owns `gpu_pipeline` unsafe and the
  FrameAnalysis seam in `frame_backend.rs`/`frame.rs`. LD's moves of the same
  code come after it, so SAFETY comments move with the code instead of being
  rebased through it.
- **EC.** EC's removal of `cdylib`/`staticlib` and LD-7's C-API decision are
  one change. Whichever lands first, the other drops that hunk.
- **During #17-#25** (LD-1, about 2-3 weeks), keep a freeze on non-urgent
  io_bridge changes from other groups. Urgent fixes land on master, and the
  open LD-1 PR rebases onto them. A pure-move PR is cheap to regenerate by
  re-running its sed script, so write that script into each PR body.
