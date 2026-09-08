# Library Configuration Struct Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace call-time `GHOSTFRAME_*` environment reads inside `ghostframe-lib` with a `LibConfig` struct parsed once at the executable boundary and threaded through constructors, so tests configure behaviour by constructing values instead of mutating process-global state.

**Architecture:** A new `ghostframe-lib/src/config.rs` holds three per-subsystem slices (`ClassifierConfig`, `TransportConfig`, `DiagnosticsConfig`) composed into a `LibConfig`. `LibConfig::from_env()` is the only place the crate touches `std::env`, called by `ghostframe-xdaemon`'s `main` and the FFI entry. Both `Classifier::default()` and `IoBridge::new` already snapshot their env reads into fields in a single struct literal, so each conversion changes where those fields come from, not the code that consumes them.

**Tech Stack:** Rust 2021, toolchain pinned to 1.96.1. Spec: `docs/superpowers/specs/2026-09-07-lib-config-design.md`.

---

## Global Constraints

- **No behaviour change.** Same inputs must produce same outputs. Every converted test must keep asserting exactly what it asserts today; a test whose meaning changes is a red flag, not a cleanup.
- **Variable names and parsing are frozen.** The e2e suite drives the containerised daemon through these exact names (`harness/scene.rs:251-256` forwards the `TEST_*` set; `tests/e2e.rs:745` passes `GHOSTFRAME_TEST_FORCE_FRAME_MODE=h264`). Moving *where* env is read must not change *whether* a variable is honoured.
- **Preserve existing `#[cfg]` gating.** All five `TEST_*` reads are already behind `#[cfg(any(test, feature = "test-loss-injection"))]` — `classifier.rs:385,390,397,408` and `io_bridge.rs:950`. `from_env()` keeps the same gating. Config *fields* exist unconditionally so unit tests can set them; only parsing is gated.
- **Sequencing matters.** Convert reads first, verify, then delete `test_env.rs` (Task 8). Removing the lock earlier reintroduces the flake.
- **Never `git add -A`.** Stage explicit paths.
- Run `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` before each commit. `clippy.toml` disallows `std::time::Instant::elapsed` — use `a.duration_since(b)`.
- Known flakes, do not investigate: `encoder::h264_vaapi::tests::full_frame_keyframe_interval` (VA-API contention, now mutex-serialised) and any residual env-var race until Task 8 lands.

## File Structure

**Create:**

| Path | Responsibility |
|---|---|
| `ghostframe-lib/src/config.rs` | The three slices, `LibConfig`, and the crate's only `std::env` reads |

**Modify:**

| Path | Change |
|---|---|
| `ghostframe-lib/src/lib.rs` | `pub mod config;` |
| `ghostframe-lib/src/tile/classifier.rs` | `Classifier::new(ClassifierConfig)`; `Default` delegates |
| `ghostframe-lib/src/tile/classifier_decide_tests.rs` | 9 tests → config literals |
| `ghostframe-lib/src/transport/io_bridge.rs` | Constructors take config; delete the six `*_from_env` helpers; 9 tests converted |
| `ghostframe-lib/src/transport/bandwidth_cap.rs` | `from_env()` → `new(bytes_per_sec)`; 2 tests converted |
| `ghostframe-lib/src/server.rs` | `GhostframeServer::new` takes `LibConfig` |
| `ghostframe-lib/src/ffi.rs` | Calls `LibConfig::from_env()` |
| `ghostframe-xdaemon/src/main.rs` | Calls `LibConfig::from_env()` |
| `ghostframe-lib/src/test_env.rs` | Deleted in Task 8 |

---

### Task 1: `config.rs` — types and defaults

**Files:**
- Create: `ghostframe-lib/src/config.rs`
- Modify: `ghostframe-lib/src/lib.rs`

- [ ] **Step 1: Write the failing test**

At the bottom of the new `config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_production_inert() {
        let cfg = LibConfig::default();
        assert!(cfg.classifier.force_frame_mode.is_none());
        assert!(cfg.classifier.refinement_bias_us.is_none());
        assert!(cfg.classifier.headroom_min_bpus.is_none());
        assert!(cfg.classifier.loss_override_threshold.is_none());
        assert!(cfg.transport.outbound_loss.is_none());
        assert!(cfg.transport.inbound_loss.is_none());
        assert!(cfg.transport.outbound_bandwidth_cap_bps.is_none());
        assert!(cfg.transport.oob_inject_at.is_none());
        assert!(!cfg.transport.skip_palette_session_reset);
        assert!(cfg.transport.test_force_bytes_per_us.is_none());
        assert!(cfg.transport.fec_k.is_none());
        assert!(!cfg.diagnostics.diagnose_tiles);
        assert!(!cfg.diagnostics.diagnose_gpu_pipeline);
        assert!(!cfg.diagnostics.diagnose_color_hist);
        assert!(cfg.diagnostics.dump_frame_path.is_none());
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-lib --lib defaults_are_production_inert`
Expected: FAIL — module `config` does not exist.

- [ ] **Step 3: Create the module**

```rust
//! Library configuration, parsed once at the executable boundary.
//!
//! `ghostframe-lib` must never read `std::env` outside this file. Environment
//! variables are process-global while `cargo test` runs tests as threads in one
//! process, so call-time reads made tests race each other (~2 failures per 6
//! full-suite runs before this refactor). Threading configuration through
//! constructors makes that class of bug structurally impossible.
//!
//! Variable names and parsing are frozen: the e2e suite drives the
//! containerised daemon through these exact names.

use crate::tile::FrameMode;

/// Knobs the frame-mode classifier reads. All `None`/default means production
/// behaviour.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClassifierConfig {
    /// Pins the frame mode. Fed by `GHOSTFRAME_TEST_FORCE_FRAME_MODE`, or by
    /// `GHOSTFRAME_FORCE_TILECODEC=1|true` as a high-level alias for
    /// `TileCodec`.
    pub force_frame_mode: Option<FrameMode>,
    pub refinement_bias_us: Option<f32>,
    pub headroom_min_bpus: Option<f32>,
    pub loss_override_threshold: Option<f32>,
}

/// Transport-layer knobs: fault injection, pacing overrides, FEC.
#[derive(Debug, Clone, Default)]
pub struct TransportConfig {
    // `loss_injection` is itself a `#[cfg(any(test, feature =
    // "test-loss-injection"))]` module (`transport/mod.rs:17`), so these two
    // fields carry the same gate — the type does not exist otherwise. This
    // mirrors `IoBridge`'s own fields at `io_bridge.rs:388,397`. Unit tests are
    // unaffected: `any(test, ..)` means the fields exist under `cargo test`.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub outbound_loss: Option<crate::transport::loss_injection::LossInjector>,
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub inbound_loss: Option<crate::transport::loss_injection::LossInjector>,
    /// Rate in bytes/sec. Deliberately a plain `u64` rather than a
    /// `BandwidthCap` (also a gated module): the config carries data, and
    /// `IoBridge` constructs the gated type from it. That keeps this field
    /// ungated.
    pub outbound_bandwidth_cap_bps: Option<u64>,
    /// `(frame_seq, tile_index)` at which to inject an out-of-range PalRLE
    /// index, from `GHOSTFRAME_INJECT_OOB_PALRLE`.
    pub oob_inject_at: Option<(u32, u32)>,
    pub skip_palette_session_reset: bool,
    pub test_force_bytes_per_us: Option<f64>,
    pub fec_k: Option<usize>,
}

/// Diagnostic logging and one-shot dumps.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsConfig {
    pub diagnose_tiles: bool,
    pub diagnose_gpu_pipeline: bool,
    pub diagnose_color_hist: bool,
    /// Path for the one-shot raw-BGRA frame dump. Consumed once, then cleared
    /// by the bridge — this replaces the current read-then-`remove_var`.
    pub dump_frame_path: Option<String>,
    pub cdf53_diff_tile: Option<(u8, u8)>,
    pub cdf53_dump_pending: bool,
    // Deliberately no `cdf53_skip_l2_l3` / `cdf53_skip_l3`: those two reads
    // live in the Vulkan dispatch path and are deferred (see the end of this
    // plan). Adding unused fields now would imply a wiring that does not
    // exist.
}

#[derive(Debug, Clone, Default)]
pub struct LibConfig {
    pub classifier: ClassifierConfig,
    pub transport: TransportConfig,
    pub diagnostics: DiagnosticsConfig,
}
```

Add `pub mod config;` to `ghostframe-lib/src/lib.rs`, alphabetically (between `capture` and `encoder`).

`FrameMode` lives at `ghostframe-lib/src/tile/mod.rs:114`; import it as `crate::tile::FrameMode`.

Check the remaining types before writing: `LossInjector`'s exact path, and whether `test_force_bytes_per_us` is `f64` or another type at `io_bridge.rs:951`. **If any differs from the sketch, follow the code and report it.** `TransportConfig` cannot derive `PartialEq` if `LossInjector` does not — check, and drop the derive rather than adding one to `LossInjector`.

Because two `TransportConfig` fields are `cfg`-gated, the Task 1 test asserting them compiles only under `cfg(test)` — which is where it lives, so this is fine, but do not "simplify" by removing the gates.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-lib --lib defaults_are_production_inert` → PASS.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/config.rs ghostframe-lib/src/lib.rs
git commit -m "feat(config): LibConfig types with production-inert defaults"
```

---

### Task 2: `ClassifierConfig::from_env`

**Files:**
- Modify: `ghostframe-lib/src/config.rs`

- [ ] **Step 1: Write the failing tests**

```rust
    /// Env-var parsing is the one place this crate still touches process-global
    /// state, so these tests take the shared lock until it is removed.
    #[test]
    fn classifier_config_parses_force_frame_mode() {
        let _env = crate::test_env::lock_env();
        std::env::set_var("GHOSTFRAME_TEST_FORCE_FRAME_MODE", "h264");
        assert_eq!(
            ClassifierConfig::from_env().force_frame_mode,
            Some(FrameMode::H264)
        );
        std::env::set_var("GHOSTFRAME_TEST_FORCE_FRAME_MODE", "tile");
        assert_eq!(
            ClassifierConfig::from_env().force_frame_mode,
            Some(FrameMode::TileCodec)
        );
        std::env::remove_var("GHOSTFRAME_TEST_FORCE_FRAME_MODE");
        assert_eq!(ClassifierConfig::from_env().force_frame_mode, None);
    }

    #[test]
    fn force_tilecodec_is_an_alias_for_tile_mode() {
        let _env = crate::test_env::lock_env();
        std::env::set_var("GHOSTFRAME_FORCE_TILECODEC", "1");
        assert_eq!(
            ClassifierConfig::from_env().force_frame_mode,
            Some(FrameMode::TileCodec)
        );
        std::env::set_var("GHOSTFRAME_FORCE_TILECODEC", "true");
        assert_eq!(
            ClassifierConfig::from_env().force_frame_mode,
            Some(FrameMode::TileCodec)
        );
        // Any other value is ignored, not an error.
        std::env::set_var("GHOSTFRAME_FORCE_TILECODEC", "0");
        assert_eq!(ClassifierConfig::from_env().force_frame_mode, None);
        std::env::remove_var("GHOSTFRAME_FORCE_TILECODEC");
    }

    #[test]
    fn classifier_config_filters_out_of_range_values() {
        let _env = crate::test_env::lock_env();
        // loss_override_threshold accepts (0.0, 1.0]; bias and headroom accept > 0.0
        std::env::set_var("GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD", "1.5");
        assert_eq!(ClassifierConfig::from_env().loss_override_threshold, None);
        std::env::set_var("GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD", "0.5");
        assert_eq!(
            ClassifierConfig::from_env().loss_override_threshold,
            Some(0.5)
        );
        std::env::set_var("GHOSTFRAME_TEST_HEADROOM_MIN_BPUS", "-1");
        assert_eq!(ClassifierConfig::from_env().headroom_min_bpus, None);
        std::env::remove_var("GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD");
        std::env::remove_var("GHOSTFRAME_TEST_HEADROOM_MIN_BPUS");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ghostframe-lib --lib classifier_config` → FAIL, `from_env` not found.

- [ ] **Step 3: Implement**

Move the parsing verbatim from `classifier.rs:383-418` (the four `*_override` initialisers inside `Default::default`) — the same `.ok().and_then(parse).filter(range)` chains and the same `FORCE_TILECODEC`-then-`TEST_FORCE_FRAME_MODE` precedence — into:

```rust
impl ClassifierConfig {
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub fn from_env() -> Self { /* the moved parsing */ }

    /// Production builds without `test-loss-injection` ignore the environment
    /// entirely, exactly as today: the reads are not compiled.
    #[cfg(not(any(test, feature = "test-loss-injection")))]
    pub fn from_env() -> Self {
        Self::default()
    }
}
```

Do not "improve" the parsing while moving it — same filters, same precedence, same accepted spellings (`"h264" | "H264"`, `"tile" | "TileCodec" | "tilecodec"`).

- [ ] **Step 4: Run** → PASS, and `cargo test -p ghostframe-lib --lib` unchanged otherwise.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/config.rs
git commit -m "feat(config): ClassifierConfig::from_env with the parsing moved verbatim"
```

---

### Task 3: `Classifier` takes its config

**Files:**
- Modify: `ghostframe-lib/src/tile/classifier.rs`
- Modify: `ghostframe-lib/src/tile/classifier_decide_tests.rs`

- [ ] **Step 1: Write the failing test**

In `classifier_decide_tests.rs`:

```rust
#[test]
fn classifier_takes_its_overrides_from_config_not_env() {
    // No env mutation, and therefore no lock: the whole point of the refactor.
    let cfg = ClassifierConfig {
        force_frame_mode: Some(FrameMode::H264),
        ..Default::default()
    };
    let mut classifier = Classifier::new(cfg);
    let mode = classifier.decide_frame_mode_at(0, &[], FrameMode::TileCodec);
    assert_eq!(mode, FrameMode::H264, "config override must pin the mode");
}
```

Signature verified at `classifier.rs:486`:
`pub fn decide_frame_mode_at(&mut self, now_us: u64, tentative_states: &[CodecState], prev_mode: FrameMode) -> FrameMode`
— it takes `&mut self` and returns a bare `FrameMode`, not a `(mode, reason)` tuple.

- [ ] **Step 2: Run to verify it fails** — `Classifier::new` not found.

- [ ] **Step 3: Implement**

```rust
impl Classifier {
    pub fn new(config: crate::config::ClassifierConfig) -> Self { /* fields from config */ }
}

impl Default for Classifier {
    fn default() -> Self {
        Self::new(crate::config::ClassifierConfig::from_env())
    }
}
```

Keeping `Default` as an env-reading convenience means no call site outside tests has to change in this task. The internal field names (`force_frame_mode_override`, `refinement_bias_us_override`, …) stay; only their source changes.

- [ ] **Step 4: Convert the nine locked tests**

Each test in `classifier_decide_tests.rs` that sets a `GHOSTFRAME_*` variable becomes a `Classifier::new(ClassifierConfig { .. })` construction, and loses both its `set_var`/`remove_var` pair and its `let _env = crate::test_env::lock_env();` guard.

Assertions must not change. If a test asserts something different after conversion, stop and report — that means the config path and the env path disagree, which is a bug in the conversion.

Run: `cargo test -p ghostframe-lib --lib` → same count, all pass.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/tile/classifier.rs ghostframe-lib/src/tile/classifier_decide_tests.rs
git commit -m "refactor(classifier): take overrides from ClassifierConfig"
```

---

### Task 4: `TransportConfig::from_env`

**Files:**
- Modify: `ghostframe-lib/src/config.rs`
- Modify: `ghostframe-lib/src/transport/io_bridge.rs` (move the helpers out)

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn transport_config_parses_loss_injectors() {
        let _env = crate::test_env::lock_env();
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY", "0.25");
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE", "tile");
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_SEED", "42");
        let cfg = TransportConfig::from_env();
        assert!(cfg.outbound_loss.is_some(), "probability > 0 must yield an injector");
        assert!(cfg.inbound_loss.is_none(), "unset direction must stay None");
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY", "0");
        assert!(
            TransportConfig::from_env().outbound_loss.is_none(),
            "probability 0 must yield None"
        );
        for v in [
            "GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY",
            "GHOSTFRAME_OUTBOUND_LOSS_PREDICATE",
            "GHOSTFRAME_OUTBOUND_LOSS_SEED",
        ] {
            std::env::remove_var(v);
        }
    }

    #[test]
    fn transport_config_parses_bandwidth_cap_and_flags() {
        let _env = crate::test_env::lock_env();
        std::env::set_var("GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP", "125000");
        std::env::set_var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET", "1");
        let cfg = TransportConfig::from_env();
        assert_eq!(cfg.outbound_bandwidth_cap_bps, Some(125_000));
        assert!(cfg.skip_palette_session_reset);
        std::env::set_var("GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP", "0");
        assert_eq!(
            TransportConfig::from_env().outbound_bandwidth_cap_bps, None,
            "cap of 0 means uncapped"
        );
        std::env::remove_var("GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP");
        std::env::remove_var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET");
    }
```

- [ ] **Step 2: Run to verify they fail.**

- [ ] **Step 3: Implement**

Move these six functions out of `io_bridge.rs` and into `config.rs`, unchanged apart from becoming free functions or `TransportConfig` associated functions:

- `loss_injector_from_env(direction: &str)` (`io_bridge.rs:743`) — keep the `format!("GHOSTFRAME_{direction}_LOSS_*")` construction and the predicate table exactly as-is
- `oob_injector_from_env()` (`:822`)
- `skip_palette_session_reset_from_env()` (`:837`)
- `diagnose_tiles_from_env()` (`:845`), `diagnose_gpu_pipeline_from_env()` (`:854`), `diagnose_color_histogram_from_env()` (`:867`) — these three belong to `DiagnosticsConfig` (Task 6); move them now and wire them there

`BandwidthCap::from_env()` (`bandwidth_cap.rs:25`) becomes `BandwidthCap::new(bytes_per_sec: u64)`, with the env parsing moving into `TransportConfig::from_env`. Update its two tests.

The four `io_bridge.rs` tests that call these helpers directly (`:5366`, `:5394`, `:5403`, `:5501`) move to `config.rs` alongside the code they test, keeping their `lock_env()` guards.

- [ ] **Step 4: Run** → `cargo test -p ghostframe-lib --lib` passes with the same total.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/config.rs ghostframe-lib/src/transport/io_bridge.rs ghostframe-lib/src/transport/bandwidth_cap.rs
git commit -m "feat(config): TransportConfig::from_env; BandwidthCap takes a rate"
```

---

### Task 5: `IoBridge` takes its config

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn io_bridge_takes_injection_settings_from_config() {
        let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("QuicServer::new");
        let cfg = crate::config::LibConfig {
            transport: crate::config::TransportConfig {
                skip_palette_session_reset: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let bridge = IoBridge::new_with_lib_config_for_test(ours, server, cfg);
        assert!(
            bridge.skip_palette_session_reset,
            "config value must reach the bridge without touching the environment"
        );
    }
```

- [ ] **Step 2: Run to verify it fails.**

- [ ] **Step 3: Implement**

**Move the loss injectors out of the config; do not clone them.** `LossInjector`
now derives `Clone` (so `LibConfig` can be `Debug`/`Clone`), and its `rng:
SplitMix { state: u64 }` means a clone replays the *same* drop sequence rather
than an independent one. Before that derive existed, `should_drop(&mut self)`
made two live injectors a compile error; now it is one `.clone()` away. If
per-session bridges were ever built by cloning one `LibConfig`, every session
would get bit-identical loss — a loss-injection harness that lies. Take them
with `Option::take` on a `&mut TransportConfig`, or move the whole config in by
value. Do not `.clone()` a `TransportConfig` that carries injectors.

`IoBridge::new` gains a `transport: TransportConfig` parameter, and the struct literal at `io_bridge.rs:945-958` takes its values from it instead of calling the `*_from_env` helpers. The five constructors — `new`, `new_with_frames`, `new_with_stream_for_test`, `new_with_frames_for_test`, `new_with_injection_for_test` — all thread it through; the `*_for_test` ones default to `LibConfig::default()` unless given one, so existing test call sites do not change. Add **one** `new_with_lib_config_for_test(stream, server, LibConfig)` — Task 6 reuses it rather than adding a second config constructor.

- [ ] **Step 4: Convert the remaining locked `io_bridge.rs` tests** to build a `TransportConfig` instead of setting env vars, dropping their `lock_env()` guards. Same rule: assertions must not change.

Run: `cargo test -p ghostframe-lib --lib` → same count, all pass.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "refactor(io_bridge): take transport settings from TransportConfig"
```

---

### Task 6: `DiagnosticsConfig` and the one-shot dump

**Files:**
- Modify: `ghostframe-lib/src/config.rs`, `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn dump_frame_path_is_consumed_once() {
        let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("QuicServer::new");
        let cfg = crate::config::LibConfig {
            diagnostics: crate::config::DiagnosticsConfig {
                dump_frame_path: Some("/tmp/ghostframe-test-dump.bgra".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bridge = IoBridge::new_with_lib_config_for_test(ours, server, cfg);
        assert!(bridge.take_dump_frame_path().is_some(), "first call yields the path");
        assert!(
            bridge.take_dump_frame_path().is_none(),
            "the dump is one-shot: the second call must yield None"
        );
    }
```

- [ ] **Step 2: Run to verify it fails.**

- [ ] **Step 3: Implement**

`GHOSTFRAME_DUMP_FRAME` is currently read *and then cleared with `remove_var`* to make the dump one-shot — the environment is being used as mutable state. Replace that with an owned `Option<String>` on the bridge and a `take_dump_frame_path(&mut self) -> Option<String>` that `Option::take`s it. Wire `diagnose_tiles`, `diagnose_gpu_pipeline`, `diagnose_color_hist`, `cdf53_diff_tile`, and `cdf53_dump_pending` from the config at the same time.

- [ ] **Step 4: Run** → PASS.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/config.rs ghostframe-lib/src/transport/io_bridge.rs
git commit -m "refactor(io_bridge): diagnostics from config; dump path is owned state"
```

---

### Task 7: Wire the executable boundary

**Files:**
- Modify: `ghostframe-lib/src/server.rs`, `ghostframe-lib/src/ffi.rs`, `ghostframe-xdaemon/src/main.rs`

Also convert the two remaining `Classifier::default()` call sites in
`ghostframe-lib/src/tile/classifier_cost_tests.rs` to
`Classifier::new(ClassifierConfig::default())` — they were outside the file scope
of the earlier conversion and are the last tests still picking up ambient
configuration.

- [ ] **Step 1: Thread `LibConfig` through `GhostframeServer::new`**

Its signature today is `GhostframeServer::new(config: GhostbridgeConfig, listen_addr, input_injector)` — called from `ghostframe-xdaemon/src/main.rs:171` and `ghostframe-lib/src/ffi.rs:55`. Add a `lib_config: LibConfig` parameter and pass its slices down to `IoBridge`.

Both callers pass `LibConfig::from_env()`. That is the whole point of the task: two call sites, and the library never reads env again.

- [ ] **Step 2: Run the full suite and the browserless test**

```
cargo test -p ghostframe-lib --lib
cargo test -p ghostframe-e2e --test browserless
cargo build --workspace --exclude ghostframe-e2e
```

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/src/server.rs ghostframe-lib/src/ffi.rs ghostframe-xdaemon/src/main.rs
git commit -m "feat: read LibConfig::from_env at the two executable entry points"
```

---

### Task 8: Delete the lock, and measure

**Files:**
- Delete: `ghostframe-lib/src/test_env.rs`
- Modify: `ghostframe-lib/src/lib.rs`, and any file still holding a guard

- [ ] **Step 1: Measure before**

```bash
pass=0; fail=0
for i in $(seq 1 12); do
  if cargo test -p ghostframe-lib --lib 2>&1 | grep -qE "^test result: ok\."; then pass=$((pass+1)); else fail=$((fail+1)); fi
done
echo "before: $pass passed / $fail failed"
```

Record the number. It should be 12/12 — the lock is still in place.

- [ ] **Step 2: Find the remaining guards**

```bash
grep -rn "lock_env()" ghostframe-lib/src --include='*.rs'
```

Every remaining one should be in `config.rs`, guarding a `from_env()` parsing test. Those are the only tests that still touch real environment, and they must **keep** the lock — they race each other otherwise.

If a guard remains anywhere else, that file still has an unconverted env read. Stop and report it rather than deleting the guard.

- [ ] **Step 3: Move the lock into `config.rs`**

Since the only remaining users are its own parsing tests, move `lock_env()` into `config.rs` under `#[cfg(test)]`, delete `ghostframe-lib/src/test_env.rs`, and drop the `mod test_env;` declaration.

- [ ] **Step 4: Measure after**

Run the same 12-iteration loop. Expect 12/12. Report both numbers.

- [ ] **Step 5: Commit**

```bash
git rm ghostframe-lib/src/test_env.rs
git add ghostframe-lib/src/lib.rs ghostframe-lib/src/config.rs
git commit -m "test: retire the env lock now that only config parsing reads env"
```

---

### Task 9: CI guard on the invariant

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Verify the invariant holds locally**

```bash
grep -rn 'env::var' ghostframe-lib/src --include='*.rs' \
  | grep -v '^ghostframe-lib/src/config.rs' \
  | grep -vE ':[0-9]+: *(//|///|//!)'
```

Expected: no output. The second filter drops comment lines: an earlier version of this
check matched prose, which pressured an implementer into rewording doc comments to
satisfy it. A guard that makes people write worse comments is worse than no guard.

- [ ] **Step 2: Add the check to the `clippy` job**

```yaml
      - name: ghostframe-lib reads env only in config.rs
        run: |
          # Comment lines are excluded deliberately: the guard constrains code,
          # not prose. Matching comments would push authors to stop naming
          # `env::var` in documentation, which is the opposite of useful.
          if grep -rn 'env::var' ghostframe-lib/src --include='*.rs' \
               | grep -v '^ghostframe-lib/src/config.rs' \
               | grep -vE ':[0-9]+: *(//|///|//!)'; then
            echo "::error::ghostframe-lib must read std::env only in config.rs"
            exit 1
          fi
```

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: enforce that ghostframe-lib reads env only in config.rs"
```

---

## Deferred (explicitly not this plan)

- **The two `cdf53-diag` GPU reads** (`capture/gpu_pipeline/frame.rs:3682`, `:3778`). They are behind a feature flag that production builds do not enable, no test sets them, and they race nothing — but threading config into the Vulkan dispatch path is a materially larger change than the rest of this plan combined. Task 9's CI guard must therefore allow that file, or the reads must be converted first; **decide this before implementing Task 9 and say which you chose**.
- `ghostframe-xdaemon`'s own deployment variables — genuine process configuration, read once at startup.
- `ghostframe-e2e`'s harness-side variables — a separate process.
