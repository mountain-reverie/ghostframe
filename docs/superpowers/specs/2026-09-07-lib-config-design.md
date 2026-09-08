# Library Configuration Struct — Design

**Date:** 2026-09-07
**Status:** Approved design
**Author:** Claude (design synthesis); review by Cedric

## Problem

`ghostframe-lib` reads `GHOSTFRAME_*` environment variables at call time, deep
inside the classifier, the I/O bridge, the bandwidth cap, and the GPU frame
path. Environment variables are process-global while `cargo test` runs tests as
threads in one process, so tests that set them race each other: measured at
roughly two failures per six full-suite runs, each passing reliably in
isolation.

Commit `c543bc8` locked that down with a shared `test_env::lock_env()` mutex
across 20 tests, which stopped the bleeding but left the underlying design in
place — ambient global state read at a distance from where it is configured. A
known residual remains: `Classifier::default()` reads the same variables and is
constructed by nearly every test in two files, so the lock does not cover every
path that could observe a concurrent write.

A correction to an earlier draft of this spec: the `TEST_*` variables are
**already** feature-gated. All five reads — four in `classifier.rs` (`:385`,
`:390`, `:397`, `:408`) and `TEST_FORCE_BYTES_PER_US` in `io_bridge.rs:950` —
sit behind `#[cfg(any(test, feature = "test-loss-injection"))]`, so a production
build does not compile them and cannot be steered by a stray variable. That
property is correct today and this work must preserve it, not introduce it.

## Goals

- One place in the library where `std::env` is read, at the executable boundary.
- Tests configure behaviour by constructing values, not by mutating process
  state — making the race structurally impossible rather than locked out.
- Test-only tuning knobs remain unavailable to a production build (already true;
  must survive the refactor).
- No change to the environment-variable names or their meaning.

## Non-goals

- Converting `ghostframe-xdaemon`'s own deployment variables (`WEB_TLS_*`,
  `LOG_FORMAT`, `X11_CAPTURE_ONLY`, `NO_X11_WAIT`, `FORCE_ROOT_GET_IMAGE`,
  `CAPTURE_DUMP_DIR`, `CAPTURE_HEARTBEAT_EVERY`). Those are genuine process
  configuration, read once at startup, and race nothing.
- Converting harness-side variables in `ghostframe-e2e`
  (`E2E_FIREFOX_BIN`, `BLESS_GOLDENS`) — a separate process.
- Changing the VA-API test mutex (`92421c9`), which addresses GPU resource
  contention, not ambient state.
- Any behaviour change. This is a refactor: the same inputs must produce the
  same outputs.

## Binding constraint: the wire-visible variable names must keep working

The e2e suite drives the containerised daemon through these exact variables —
`GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY`, `GHOSTFRAME_INBOUND_LOSS_PREDICATE`,
`GHOSTFRAME_OUTBOUND_LOSS_SEED`, and friends — set on the test-server container
rather than in-process. `tests/containers/test-server/Dockerfile` also builds
with `test-loss-injection` and `cdf53-diag` enabled specifically so those
variables take effect.

This work moves *where* the environment is read. It must not change *whether*
each variable is honoured, nor its name, nor its parsing. An e2e scenario that
passes today must pass unchanged afterwards.

## Decisions

| # | Decision | Rationale |
|---|---|---|
| D1 | Scope: library-internal reads only | Fixes every known race and gives the library one config type, at roughly half the total surface, so it lands in one reviewable pass. xdaemon's deployment variables stay where they are. |
| D2 | Explicit constructor threading | No ambient state anywhere in the library, so races become impossible rather than locked out, and `test_env.rs` can be deleted. A `OnceLock` would keep the race; a thread-local would break across the tokio runtime's threads. |
| D3 | Per-subsystem config slices, composed into a root | `classifier.rs` should not see loss-injection knobs. Each type takes only what it uses. |
| D4 | Preserve the existing `test-loss-injection` gating on `TEST_*` parsing | Already the case at all five read sites; `from_env()` must keep the same `#[cfg]` so a production daemon stays unsteerable. The *fields* exist unconditionally so unit tests can set them regardless of features — only parsing is gated. |
| D5 | `Default` == production behaviour | Tests write `ClassifierConfig { force_frame_mode: Some(..), ..Default::default() }` and get today's production semantics for everything they do not mention. |

## Architecture

```
  ghostframe-xdaemon main ─┐
                           ├─► LibConfig::from_env()  ── the only std::env read
  FFI entry point ─────────┘            │
                                        ▼
                              ┌──── LibConfig ────┐
                              │                   │
                ClassifierConfig   TransportConfig   DiagnosticsConfig
                        │                 │                 │
                        ▼                 ▼                 ▼
                   Classifier         IoBridge         IoBridge +
                                   BandwidthCap        gpu_pipeline
```

### `ghostframe-lib/src/config.rs` (new)

```rust
#[derive(Debug, Clone, Default)]
pub struct LibConfig {
    pub classifier: ClassifierConfig,
    pub transport: TransportConfig,
    pub diagnostics: DiagnosticsConfig,
}

impl LibConfig {
    /// Parse from the process environment. The **only** place `std::env` is
    /// read in this crate; every other module takes what it needs by value.
    pub fn from_env() -> Self { .. }
}
```

**`ClassifierConfig`** — `force_frame_mode: Option<FrameMode>` (fed by both
`GHOSTFRAME_TEST_FORCE_FRAME_MODE` and its alias `GHOSTFRAME_FORCE_TILECODEC`),
`refinement_bias_us`, `headroom_min_bpus`, `loss_override_threshold`.

`Classifier::default()` already snapshots all four into fields at construction
(`classifier.rs:392-414`), so this is a change of *source*, not of structure.

**`TransportConfig`** — `fec_k`, `outbound_loss` / `inbound_loss` (the existing
loss-injector descriptors), `outbound_bandwidth_cap`, `test_force_bytes_per_us`,
`inject_oob_palrle`, `skip_palette_session_reset`.

**`DiagnosticsConfig`** — `diagnose_tiles`, `diagnose_gpu_pipeline`,
`diagnose_color_hist`, `dump_frame`, `cdf53_diff_tile`, `cdf53_dump_pending`,
`cdf53_skip_l2_l3`, `cdf53_skip_l3`.

Fields whose variables are feature-gated today keep that gating on the parsing
side; the *fields* always exist so unit tests can set them regardless of
features. Only `from_env()` differs between builds.

### Read sites converted

Nineteen call-time reads across four files (verified 2026-09-07 — re-verify
before implementing, since two earlier counts in this project were wrong):

| File | Reads |
|---|---|
| `transport/io_bridge.rs` | 11 |
| `tile/classifier.rs` | 4 |
| `capture/gpu_pipeline/frame.rs` | 2 |
| `transport/bandwidth_cap.rs` | 1 |
| loss-injector helpers | the `*_from_env()` constructors |

`GHOSTFRAME_DUMP_FRAME` needs care: it is read *and cleared* to make the dump
one-shot. That becomes a mutable field on the config the bridge owns, not an
environment write.

## Testing

- **Parsing:** one test per variable asserting `from_env()` maps it to the right
  field, including the `FORCE_TILECODEC` → `force_frame_mode` alias and the
  feature-gated fields under both cfgs. This is the only remaining test that
  touches real environment, and being single it races nothing.
- **Behaviour:** the ~20 currently-locked tests convert to config literals. Each
  must keep asserting what it asserts today — a test whose meaning changes is a
  red flag, not a cleanup.
- **Regression:** after conversion, `grep -rn 'env::var' ghostframe-lib/src`
  should match only `config.rs`. Worth asserting in CI.
- **Flake measurement:** run the lib suite 12 times before deleting
  `test_env.rs` and 12 times after; both must be clean. The pre-lock baseline
  was ~2 failures per 6 runs.

## Migration and cleanup

Once no library code reads env at call time, delete the 20 `lock_env()` guards
and `test_env.rs` itself. Keep the VA-API mutex.

Sequence matters: convert reads first, verify, then remove the lock. Removing
the lock before the reads are gone would reintroduce the flake.

## Risks

| Risk | Mitigation |
|---|---|
| An e2e scenario silently loses an override because a variable stopped being parsed | The parsing test covers every variable by name; the e2e suite is the backstop. Convert one subsystem per commit so a bisect points at the right one. |
| A `TEST_*` variable that an e2e scenario relies on becomes unavailable in an image built without `test-loss-injection` | All five `TEST_*` variables are live in e2e: `harness/scene.rs:251-256` forwards them to the container daemon and `tests/e2e.rs:745` passes `GHOSTFRAME_TEST_FORCE_FRAME_MODE=h264`. Gating is safe **only** because `tests/containers/test-server/Dockerfile` builds with `--features ghostframe-lib/test-loss-injection,ghostframe-lib/cdf53-diag`. Re-check that Dockerfile line before changing the gating; if the image ever drops the feature, these scenarios silently get default behaviour rather than failing. |
| Constructor churn touches many call sites at once | Per-subsystem slices mean each commit changes one constructor and its tests. `Default` keeps untouched call sites to `..Default::default()`. |
| The count of read sites is wrong, as it was twice before in this project | The implementation plan instructs re-grepping and reporting the real count rather than trusting this document. |
