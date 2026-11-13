# Stats Panel PR 2: Client Stats Model and the Leptos Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The browser client shows a retractable stats panel (versions, RTT, bandwidth, mode, loss) built from a `ClientStats` model in `ghostframe-client-core` and a new Leptos-only crate `ghostframe-web-ui`, mounted from `main.ts` through `ghostframe-client-wasm`, with the server stream (PR 1) read on the feedback stream's return half.

**Architecture:** `ghostframe-client-core::stats::ClientStats` owns two sources behind one `StatsView`: server metrics (schema + 1 Hz samples, 60-sample rings, counter-to-rate, 3 s staleness) and local metrics (sampled once a second from counters the core bumps). `ClientCore` owns it (no `Rc`/`RefCell` in core). `WasmClientCore` holds the core as `Rc<RefCell<ClientCore>>` so the panel (`ghostframe-web-ui`, the only crate that depends on Leptos) shares it through the `PanelBackend` trait; the panel never touches the transport, it only calls `set_stats_subscribed` (which queues `STATS_SUBSCRIBE` on the core's outbox) and reads `view()` once a second. Plain Rust modules (`format`, `spark`, `storage::parse_open`) are tested with `cargo test` on the host; one `wasm-bindgen-test` in headless Chrome mounts and toggles the real panel over a real `ClientCore`.

**Tech Stack:** Rust 1.96.1 (pinned), `leptos =0.9.0-rc1` (`csr` only; 0.9.0 final is not published, `cargo search leptos` on 2026-11-12 shows `0.9.0-rc1` as latest), `wasm-bindgen` 0.2.129 (already locked), `web-sys`, `wasm-bindgen-test =0.3.79` (matches the locked `js-sys 0.3.106`), `wasm-pack` 0.15.0 for the browser test, TypeScript/vitest for `main.ts`.

**Branch:** `feat/stats-panel-client`, in a worktree. **Prerequisite:** PR 1 (`feat/stats-panel-protocol-server`) is merged to `master`: `ghostframe-protocol/src/stats.rs` exists exactly per the shared contract. Task 1 checks this and stops if it is false.

**Exit status.** A pipe reports the last command's status, and a masked build failure has shipped a stale `dist/` here before. Every gating command in this plan that is piped into `tail`/`grep` is followed by `echo "rc=${PIPESTATUS[0]}"`, and its step says which rc it expects; a step that does not mention rc expects `0`. Unpiped commands use `echo "rc=$?"`.

**Spec:** `docs/specs/stats-panel-design.md` sections 1.3, 1.4 (client half), 2, 3 (client errors, mount failure), 4 (client stats, panel logic, browser test), 5 (Leptos pin, workspace member, Dockerfile), 6 item 2.

## Findings that shaped this plan (verified on this branch, 2026-11-12)

- **Leptos 0.9.0-rc1 compiles** for `wasm32-unknown-unknown` with the repo's `rust-toolchain.toml` (1.96.1) and on the host (needed: `cargo clippy --workspace --all-targets` and `cargo test --workspace --lib` compile every member natively). Adding it to a copy of the repo lockfile kept `wasm-bindgen 0.2.129` and added 68 packages without bumping existing ones.
- **`wasm-pack test --headless --chrome` works here** (Chromium 156 and chromedriver 156 are installed; wasm-pack downloads the matching `wasm-bindgen-test-runner`). No `wasm-bindgen-test-runner` is on `PATH`; wasm-pack does not need it there.
- **Size (spec gap, see Self-review):** `ghostframe-client-wasm` with the panel linked is about 487 KB after `wasm-pack build --target web` (default release profile), 423 KB with `opt-level=s`, LTO, one codegen unit and `panic=abort`, against 168 KB today. A trivial Leptos mount alone is 45 KB; the panel's `format!`, `String` and `Vec` use is the rest. The spec's original 320 KB cap could not hold. **Owner decision (2026-11-13): the cap is 512 KiB raw (524,288 bytes), and Task 15b shrinks the panel by size-optimising only the Leptos stack.** This plan records the numbers (Task 16); PR 3 adds the guard.
- **No JS-called export can re-enter while a panel borrow is held.** The panel borrows the core only inside `CoreBackend::view` and `CoreBackend::set_subscribed`, both of which return an owned value or `()` and call no JS inside the borrow (`view` reads the clock *before* it borrows: `performance.now()` is a JS import); browser timers and click handlers run as separate tasks, never inside a `WasmClientCore` call, and no `WasmClientCore` method calls back into JS (they only build values with `serde_wasm_bindgen`). The `try_borrow*` forms are used anyway so a failed borrow costs one refresh instead of trapping the module.
- **Residual risk: a panic.** The panel shares the wasm instance with the session, and a Rust panic traps the module (`ghostframe-client-wasm/src/lib.rs`: "every later call traps"). `main.ts`'s try/catch contains a *mount* error only; a panic in a panel timer or click handler would end the session. The panel code is therefore written panic-free (no `unwrap`/`expect`/unchecked indexing on view data; Task 14 greps for it) and the limitation is stated in the PR description.
- **Version string.** `.git` is a file in a worktree, so `build.rs` asks `git rev-parse --git-path` for `HEAD`, `index` and `refs/heads` instead of hard-coding `.git/...`. `git status --porcelain` is run with `--untracked-files=no`: a developer checkout always has untracked files (this one has four), which would make every local build "dirty". The flag is display-only and best-effort: `versions_match` ignores it (PR 1's rule, Task 3).
- **`ci/check-env-reads.sh`** scans `ghostframe-client-*/src` only; `build.rs` and `ghostframe-web-ui` are outside it, and no new `env::var` appears in any `src/`.
- **CI** runs `cargo test -p ghostframe-protocol -p ghostframe-client-core` package-wide (`.github/workflows/client-core.yml`), so the new `tests/stats_core.rs` is picked up without being named. `wasm-pack test` runs every test file in `ghostframe-web-ui`; it needs its own job (Task 12).

## File structure

| File | Action | Responsibility |
|---|---|---|
| `Cargo.toml` (workspace) | Modify | Add `ghostframe-web-ui` to `members`. |
| `Cargo.lock` | Modify | New packages only; no existing version may move (checked in Task 2). |
| `tests/containers/test-server/Dockerfile` | Modify | Per-crate manifest/source `COPY` for `ghostframe-web-ui` (a workspace member the image never builds). |
| `ghostframe-client-core/src/stats.rs` | Create | `ClientStats`, `StatsView`/`GroupView`/`RowView`, `Ring`, `Series`, `versions_match`; server schema/sample handling and the render-ready view. |
| `ghostframe-client-core/src/stats_local.rs` | Create | `LocalCounters`, `LocalStats`, the local metric table and per-second sampling. |
| `ghostframe-client-core/src/lib.rs` | Modify | `pub mod stats`, re-exports, `ClientCore.stats` field, `set_stats_subscribed`, `handle_server_stream`, `stats`, `record_paints`, `set_client_version`, NACK counting in `poll_transmit`, sampling in `on_timeout`/`poll_timeout`. |
| `ghostframe-client-core/src/reassembly.rs` | Modify | Count received bytes and the codec mix in `handle_datagram`. |
| `ghostframe-client-core/tests/stats_core.rs` | Create | Core-level tests: subscribe message, server stream into the view, local metrics without a subscription. |
| `ghostframe-web-ui/Cargo.toml` | Create | The only crate depending on `leptos` (exact pin, `csr`). |
| `ghostframe-web-ui/src/lib.rs` | Create | `PanelBackend`, `mount_stats_panel`. |
| `ghostframe-web-ui/src/backend.rs` | Create | `CoreBackend`: `PanelBackend` over `Rc<RefCell<ClientCore>>`. |
| `ghostframe-web-ui/src/components.rs` | Create | `StatsPanel`, `Tab` (inside it), `Group`, `MetricRow`, `Sparkline`, `VersionRow`. |
| `ghostframe-web-ui/src/format.rs` | Create | SI / three-significant-digit value formatting (plain Rust). |
| `ghostframe-web-ui/src/spark.rs` | Create | Sparkline geometry (plain Rust). |
| `ghostframe-web-ui/src/storage.rs` | Create | `localStorage` open state; every failure means "closed". |
| `ghostframe-web-ui/src/style.css` | Create | The panel's hand-written CSS, `include_str!`'d. |
| `ghostframe-web-ui/tests/panel_mount.rs` | Create | The `wasm-bindgen-test` (headless Chrome). |
| `Justfile` | Modify | `test-web-ui` recipe; step in `ci-local`. |
| `.github/workflows/client-core.yml` | Modify | `web-ui` job (`wasm-pack test`), host tests, wasm build line. |
| `ghostframe-client-wasm/Cargo.toml` | Modify | Depend on `ghostframe-web-ui` and `web-sys`. |
| `ghostframe-client-wasm/build.rs` | Modify | Also emit `GHOSTFRAME_CLIENT_VERSION`. |
| `ghostframe-client-wasm/src/lib.rs` | Modify | `CLIENT_VERSION`, `clientVersion()`, `pub mod panel`, version-shape test. |
| `ghostframe-client-wasm/src/core.rs` | Modify | `inner: Rc<RefCell<ClientCore>>`; `handleServerStream`, `recordPaints`; `shared()`. |
| `ghostframe-client-wasm/src/panel.rs` | Create | `mountStatsPanel(container, core)`. |
| `ghostframe-web-client/src/server_stream.ts` | Create | `pumpServerStream`: read the bidi readable half in a loop. |
| `ghostframe-web-client/src/main.ts` | Modify | Keep `bidi.readable`, pump it into the core, mount the panel, `recordPaints` per painted tick. |
| `ghostframe-web-client/index.html` | Modify | `<div id="stats-panel">`. |
| `ghostframe-web-client/tests/server_stream.test.ts` | Create | Vitest for the pump. |
| `ghostframe-web-client/tests/wasm_smoke.test.ts` | Modify | New exports exist, hostile server bytes do not trap, version shape. |
| `docs/specs/wasm-bundle-baseline.md` | Modify | Append the before/after sizes (a record, as that file says). |
| `DEVELOPERS.md` | Modify | Crate table row and the `test-web-ui` recipe. |

## Task 1: Branch, worktree, and preflight against PR 1

**Model:** sonnet

**Files:** none changed.

- [ ] **Step 1: Create the worktree (superpowers:using-git-worktrees).**

```bash
cd ~/work/ghostframe
git fetch origin master
git worktree add ../gf-stats-client -b feat/stats-panel-client master
cd ~/work/gf-stats-client
git status --short
```

Expected: clean tree (no output), branch `feat/stats-panel-client`. From here every command runs in `~/work/gf-stats-client`.

- [ ] **Step 1b: Install the web client's dependencies and check the toolchain.** A fresh worktree has no `node_modules`; without them `npx vitest` and `npx tsc` in Tasks 14, 15 and 17 fail or offer to download packages. Use `npm ci` (never `npm install`, which can rewrite `package-lock.json`, and Task 17 step 4 flags that file as stray).

```bash
cd ~/work/gf-stats-client/ghostframe-web-client
npm ci; echo "npm ci rc=$?"
cd ..
wasm-pack --version
git status --short
```

Expected: `npm ci rc=0`; `wasm-pack 0.15.0` (`npm run build:wasm` needs exactly that on `PATH`; if it is missing, `cargo install wasm-pack --version 0.15.0 --locked`); `git status --short` still prints nothing (`node_modules/` is gitignored and `npm ci` leaves the lockfile alone).

- [ ] **Step 2: Confirm PR 1 is on this branch.**

```bash
cd ~/work/gf-stats-client
test -f ghostframe-protocol/src/stats.rs && echo "stats.rs present" || echo "MISSING: PR 1 is not merged, stop"
grep -n "STATS_SUBSCRIBE_MSG_TYPE" ghostframe-protocol/src/feedback_stream.rs | head -3
```

Expected: `stats.rs present` and a `STATS_SUBSCRIBE_MSG_TYPE: u8 = 0x0B` line. If either is missing, stop: this plan assumes PR 1 is merged.

- [ ] **Step 3: Verify the contract surface this plan compiles against.**

```bash
cd ~/work/gf-stats-client
grep -n "pub const SERVER_MSG_STATS_SCHEMA\|pub const SERVER_MSG_STATS_SAMPLE\|pub const SERVER_FRAME_MAX_LEN" ghostframe-protocol/src/stats.rs
grep -n -B1 "pub enum MetricKind\|pub enum MetricGroup\|pub struct MetricDescriptor\|pub struct StatsSchema\|pub struct StatsSample\|pub struct StatsSubscribe\|pub struct ServerFrameReader" ghostframe-protocol/src/stats.rs
grep -n "pub fn encode_frame\|pub fn decode_payload\|pub fn encode(&self)\|pub fn push\|pub fn next_frame" ghostframe-protocol/src/stats.rs
```

Expected, all of these (this plan relies on every one):

- `MetricKind` and `MetricGroup` derive `Debug, Clone, Copy, PartialEq, Eq`. `ClientStats` copies them out of descriptors and `RowView` compares them.
- `MetricDescriptor` and `StatsSchema` derive `Clone, PartialEq`; `StatsSample` derives `Clone, PartialEq`; `StatsSubscribe` has `pub on: bool`.
- `ServerFrameReader` implements `Default` (the plan calls `ServerFrameReader::default()`), with `push(&mut self, &[u8])` and `next_frame(&mut self) -> Option<Result<(u8, Vec<u8>), StatsWireError>>`.
- `StatsSchema::encode_frame`, `StatsSchema::decode_payload(&[u8])`, `StatsSample::encode_frame`, `StatsSample::decode_payload(&[u8], &StatsSchema)`, `StatsSubscribe::encode(&self) -> [u8; 2]`.

If a derive or `Default` is missing, add it in `ghostframe-protocol/src/stats.rs`, run `cargo test -p ghostframe-protocol`, and commit it on its own: `git add ghostframe-protocol/src/stats.rs && git commit -m "fix(protocol): derive what the client stats model needs" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"`. If anything else differs (a renamed type, a different signature), stop and report it: that is a contract change, not something to patch around here.

- [ ] **Step 4: Record the baseline (tests and wasm size, before any change).**

```bash
cd ~/work/gf-stats-client
cargo test -p ghostframe-client-core 2>&1 | grep "test result" | sort | uniq -c; echo "rc=${PIPESTATUS[0]}"
( cd ghostframe-web-client && npm run build:wasm 2>&1 | tail -2; echo "rc=${PIPESTATUS[0]}" )
ls -l ghostframe-web-client/pkg-web/ghostframe_client_wasm_bg.wasm
gzip -c ghostframe-web-client/pkg-web/ghostframe_client_wasm_bg.wasm | wc -c
```

Expected: every `test result` line `ok`. Write down the two sizes (raw and gzip; about 168 KB raw on 2026-11-07): Task 16 compares against them. (`pkg-web/` is gitignored; this builds it inside the worktree.)

## Task 2: `ghostframe-web-ui` skeleton, workspace member, Dockerfile, lockfile check

**Model:** haiku

**Files:**
- Modify: `Cargo.toml` (workspace members)
- Create: `ghostframe-web-ui/Cargo.toml`, `ghostframe-web-ui/src/lib.rs` (empty placeholder, replaced in Task 9)
- Modify: `tests/containers/test-server/Dockerfile`
- Modify: `Cargo.lock` (by cargo)

- [ ] **Step 1: Add the member.** In `Cargo.toml`, after the `"ghostframe-client-wasm",` line in `members`, add `"ghostframe-web-ui",`:

```toml
    "ghostframe-client-wasm",
    "ghostframe-web-ui",
    "ghostframe-client-gpu",
```

- [ ] **Step 2: Create the manifest.** `ghostframe-web-ui/Cargo.toml`:

```toml
[package]
name = "ghostframe-web-ui"
version = "0.1.0"
edition = "2021"
publish = false
description = "The browser client's DOM UI (stats panel). The only crate that depends on Leptos."

[dependencies]
ghostframe-client-core = { path = "../ghostframe-client-core" }
# Pinned exactly: Leptos 0.9 is a release candidate and moves between
# candidates. `csr` is the only rendering mode a browser-only client needs.
leptos = { version = "=0.9.0-rc1", features = ["csr"] }
wasm-bindgen = "0.2"
web-sys = { version = "0.3", features = ["Window", "Storage", "Performance", "HtmlElement", "console"] }

[dev-dependencies]
ghostframe-protocol = { path = "../ghostframe-protocol" }
wasm-bindgen-test = "=0.3.79"
web-sys = { version = "0.3", features = ["Document", "Element", "Node", "EventTarget", "MouseEvent", "MouseEventInit"] }
```
The `HtmlElement` feature in `[dependencies]` is needed by `mount_stats_panel`'s signature; the extra `[dev-dependencies]` features are only what the browser test touches (cargo unifies them for test builds). `wasm-bindgen-test` is pinned to `=0.3.79` because it must agree with the locked `wasm-bindgen 0.2.129` / `js-sys 0.3.106`; wasm-pack picks the runner by the locked `wasm-bindgen` version.

- [ ] **Step 3: Placeholder source** so the member builds (Task 9 replaces it):

```bash
mkdir -p ghostframe-web-ui/src ghostframe-web-ui/tests
printf '//! The browser client DOM UI. Filled in by the next tasks.\n' > ghostframe-web-ui/src/lib.rs
```

- [ ] **Step 4: Dockerfile.** `tests/containers/test-server/Dockerfile` copies every workspace member's manifest and sources before building (a member it does not build still has to parse). After the three `COPY ghostframe-client-wasm/...` lines (`Cargo.toml`, `src/`, `build.rs`), add:

```dockerfile
COPY ghostframe-web-ui/Cargo.toml ghostframe-web-ui/Cargo.toml
COPY ghostframe-web-ui/src/ ghostframe-web-ui/src/
```

(`style.css` lives under `src/`, so it is covered. `tests/` is not copied: the image never builds test targets.)

- [ ] **Step 5: Build both targets and check the lockfile moved only by additions.**

```bash
cd ~/work/gf-stats-client
cargo check -p ghostframe-web-ui
cargo check -p ghostframe-web-ui --target wasm32-unknown-unknown
git diff --stat Cargo.lock
git diff Cargo.lock | grep '^-[^-]' | grep -v '^-dependencies\|^-  *"' || echo "no removed or changed package entries"
cargo tree -p ghostframe-server -i leptos 2>&1 | head -2
grep -A1 'name = "wasm-bindgen"$' Cargo.lock
ci/check-release-features.sh; echo "check-release-features rc=$?"
```

Expected: both checks pass; the lockfile diff is additions only (about 68 new packages; the only `-` lines are `dependencies = [` lists of members that gained a dependency, none for `version =`); `cargo tree -p ghostframe-server -i leptos` says the package ID did not match (the daemon never sees Leptos); `wasm-bindgen` stays `0.2.129`; the release-feature check prints `check-release-features rc=0`. If a `version =` line was removed or `wasm-bindgen` moved, stop: a Leptos dependency pulled an existing package, and `wasm-pack` would then need a different CLI than `Cargo.lock` implies.

- [ ] **Step 6: Commit.**

```bash
git add Cargo.toml Cargo.lock ghostframe-web-ui/Cargo.toml ghostframe-web-ui/src/lib.rs tests/containers/test-server/Dockerfile
git commit -m "build: add the ghostframe-web-ui crate (Leptos 0.9.0-rc1, csr only)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

## Task 3: `ClientStats`, server half (schema, samples, rings, rates, staleness, view)

**Model:** sonnet

**Files:**
- Create: `ghostframe-client-core/src/stats.rs`
- Modify: `ghostframe-client-core/src/lib.rs` (module and re-exports)

Design decisions this task fixes (each has a test below):

- A **counter** sample is a running total; the view shows its per-second rate from consecutive samples' *server* timestamps. A total that goes backwards, or a timestamp that does not advance, restarts the rate: nothing is pushed and `current` is `None` for that second.
- **Staleness** uses the *client's* arrival time passed to `handle_stream_bytes`/`apply_sample`: more than 3 s (strictly) since the last accepted sample, or no sample since the last schema, makes every server row `stale`. History is kept.
- A **new `schema_id`** clears the server series. Row ids embed the `schema_id` (`s<schema_id>.<metric id>`), so a keyed list in the panel rebuilds.
- Everything the server stream gets wrong (decode error, count or `schema_id` mismatch, a sample before any schema, an oversized frame) increments one counter, `samples_rejected`, and changes nothing else. Unknown frame types are skipped by their length (the reader does that).
- `versions_match` is **PR 1's rule** (its Task 8 preamble and its Contract deviations, both marked binding on this PR): parse `<pkg> (<commit>[, dirty])` on both sides and compare the crate version and the commit **only**. `dirty` is shown but never fails the match: the server's flag is computed when `ghostframe-lib`'s build script reruns and the client's when the wasm crate rebuilds, so on a dirty tree where only client files changed the two strings legitimately disagree about `dirty` while the code is current. An `unknown` commit on either side, an unparsable string, and no server version yet all count as a match (nothing can be judged). A different crate version is still a mismatch.

- [ ] **Step 1: Register the module and write the failing tests.** In `ghostframe-client-core/src/lib.rs`, add `pub mod stats;` after `mod reassembly;` and the re-exports before `pub use tile_decoder::...`:

```rust
mod reassembly;
pub mod stats;
pub mod tile_decoder;
```

```rust
pub use ghostframe_protocol::feedback_stream::ResetReason;
pub use ghostframe_protocol::stats::{MetricGroup, MetricKind};
pub use h264_gate::{H264Gate, H264Outcome, RefuseCause, ResetCause, Verdict};
pub use stats::{ClientStats, GroupView, RowView, StatsView};
pub use tile_decoder::{BatchStep, TileDecoder};
```

Create `ghostframe-client-core/src/stats.rs` containing only the tests (the implementation arrives in step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ghostframe_protocol::stats::MetricDescriptor;

    fn desc(id: u16, kind: MetricKind, group: MetricGroup, unit: &str) -> MetricDescriptor {
        MetricDescriptor {
            id,
            name: format!("test.m{id}"),
            unit: unit.to_string(),
            kind,
            group,
            label: format!("Metric {id}"),
        }
    }

    fn schema(schema_id: u16, metrics: Vec<MetricDescriptor>) -> StatsSchema {
        StatsSchema {
            schema_id,
            server_version: "0.1.0 (aab2fa2d)".to_string(),
            host_name: "evangeline".to_string(),
            metrics,
        }
    }

    /// Gauge, counter, text: one of each kind, in one group each.
    fn three_kinds(schema_id: u16) -> StatsSchema {
        schema(
            schema_id,
            vec![
                desc(1, MetricKind::Gauge, MetricGroup::Connection, "ms"),
                desc(2, MetricKind::Counter, MetricGroup::Loss, "1"),
                desc(3, MetricKind::Text, MetricGroup::Video, "1"),
            ],
        )
    }

    fn sample(schema_id: u16, at_us: u64, gauge: f64, counter: f64, text: &str) -> StatsSample {
        StatsSample {
            schema_id,
            timestamp_us: at_us,
            values: vec![
                MetricValue::Num(gauge),
                MetricValue::Num(counter),
                MetricValue::Text(text.to_string()),
            ],
        }
    }

    fn row(view: &StatsView, label: &str) -> RowView {
        view.groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .find(|r| r.label == label)
            .unwrap_or_else(|| panic!("no row {label}: {view:?}"))
            .clone()
    }

    fn fed(now: u64) -> ClientStats {
        let mut s = ClientStats::new(0);
        s.apply_schema(three_kinds(1));
        s.apply_sample(&sample(1, 1_000_000, 10.0, 100.0, "h264"), now);
        s
    }

    #[test]
    fn ring_keeps_the_newest_sixty_in_order() {
        let mut r = Ring::default();
        for i in 0..(RING_LEN + 7) {
            r.push(i as f64);
        }
        let v = r.to_vec();
        assert_eq!(r.len(), RING_LEN, "bounded");
        assert_eq!(v.first(), Some(&7.0), "oldest 7 were dropped");
        assert_eq!(v.last(), Some(&((RING_LEN + 6) as f64)), "newest is last");
        assert!(v.windows(2).all(|w| w[0] < w[1]), "order preserved");
    }

    #[test]
    fn gauge_and_text_samples_reach_the_view() {
        let s = fed(1_000_000);
        let v = s.view(1_000_000);
        let g = row(&v, "Metric 1");
        assert_eq!(g.current, Some(10.0));
        assert_eq!(g.history, vec![10.0]);
        assert!(!g.stale);
        assert_eq!(row(&v, "Metric 3").text.as_deref(), Some("h264"));
        assert_eq!(v.server_host.as_deref(), Some("evangeline"));
        assert_eq!(v.server_version.as_deref(), Some("0.1.0 (aab2fa2d)"));
    }

    #[test]
    fn counter_is_shown_as_a_per_second_rate() {
        let mut s = fed(1_000_000);
        // 250 more in half a second of server time = 500/s.
        s.apply_sample(&sample(1, 1_500_000, 11.0, 350.0, "h264"), 1_500_000);
        let c = row(&s.view(1_500_000), "Metric 2");
        assert_eq!(c.current, Some(500.0));
        assert_eq!(
            c.history,
            vec![500.0],
            "the first total only set the baseline"
        );
    }

    #[test]
    fn counter_going_backwards_restarts_the_rate_and_never_goes_negative() {
        let mut s = fed(1_000_000);
        s.apply_sample(&sample(1, 2_000_000, 1.0, 200.0, "x"), 2_000_000);
        // Server restarted: the total fell from 200 to 5.
        s.apply_sample(&sample(1, 3_000_000, 1.0, 5.0, "x"), 3_000_000);
        let c = row(&s.view(3_000_000), "Metric 2");
        assert_eq!(
            c.current, None,
            "restart shows no value, not a negative one"
        );
        assert_eq!(c.history, vec![100.0], "nothing pushed for the restart");
        // And it recovers from the new baseline.
        s.apply_sample(&sample(1, 4_000_000, 1.0, 25.0, "x"), 4_000_000);
        let c = row(&s.view(4_000_000), "Metric 2");
        assert_eq!(c.current, Some(20.0));
        assert!(c.history.iter().all(|r| *r >= 0.0));
    }

    #[test]
    fn server_rows_go_stale_after_three_seconds_and_keep_their_history() {
        let s = fed(10_000_000);
        let at_limit = s.view(10_000_000 + STALE_AFTER_US);
        assert!(
            !row(&at_limit, "Metric 1").stale,
            "exactly 3 s is still fresh"
        );
        let past = s.view(10_000_000 + STALE_AFTER_US + 1);
        let g = row(&past, "Metric 1");
        assert!(g.stale);
        assert_eq!(g.history, vec![10.0], "history is kept");
    }

    #[test]
    fn rows_are_stale_before_the_first_sample() {
        let mut s = ClientStats::new(0);
        s.apply_schema(three_kinds(1));
        assert!(row(&s.view(0), "Metric 1").stale);
    }

    #[test]
    fn a_new_schema_id_clears_the_server_rings_but_a_repeat_keeps_them() {
        let mut s = fed(1_000_000);
        s.apply_schema(three_kinds(1));
        assert_eq!(row(&s.view(1_000_000), "Metric 1").history, vec![10.0]);
        s.apply_schema(three_kinds(2));
        let v = s.view(1_000_000);
        assert!(row(&v, "Metric 1").history.is_empty());
        assert_eq!(row(&v, "Metric 1").current, None);
        // A sample of the old schema is now refused.
        s.apply_sample(&sample(1, 2_000_000, 5.0, 1.0, "x"), 2_000_000);
        assert_eq!(s.samples_rejected(), 1);
    }

    #[test]
    fn row_ids_change_with_the_schema_so_a_keyed_list_rebuilds() {
        let mut s = fed(1_000_000);
        let before = row(&s.view(1_000_000), "Metric 1").id;
        s.apply_schema(three_kinds(2));
        let after = row(&s.view(1_000_000), "Metric 1").id;
        assert_ne!(before, after);
    }

    #[test]
    fn mismatched_samples_are_dropped_and_counted() {
        let mut s = fed(1_000_000);
        // Wrong schema id.
        s.apply_sample(&sample(9, 2_000_000, 99.0, 1.0, "x"), 2_000_000);
        // Wrong value count.
        let mut short = sample(1, 2_000_000, 99.0, 1.0, "x");
        short.values.pop();
        s.apply_sample(&short, 2_000_000);
        assert_eq!(s.samples_rejected(), 2);
        let g = row(&s.view(1_000_000), "Metric 1");
        assert_eq!(g.history, vec![10.0], "a rejected sample changes nothing");
    }

    #[test]
    fn a_sample_before_any_schema_is_rejected() {
        let mut s = ClientStats::new(0);
        s.handle_stream_bytes(&sample(1, 1, 1.0, 1.0, "x").encode_frame(), 0);
        assert_eq!(s.samples_rejected(), 1);
    }

    #[test]
    fn non_finite_numbers_are_ignored() {
        let mut s = fed(1_000_000);
        s.apply_sample(
            &sample(1, 2_000_000, f64::NAN, f64::INFINITY, "x"),
            2_000_000,
        );
        let v = s.view(2_000_000);
        assert_eq!(row(&v, "Metric 1").history, vec![10.0]);
        assert!(row(&v, "Metric 2").history.is_empty());
    }

    #[test]
    fn frames_split_across_reads_are_reassembled_and_unknown_types_skipped() {
        let mut s = ClientStats::new(0);
        let mut bytes = three_kinds(1).encode_frame();
        // An unknown frame type, skipped by its length.
        bytes.extend_from_slice(&[0x7F, 2, 0, 0xAA, 0xBB]);
        bytes.extend_from_slice(&sample(1, 1_000_000, 10.0, 100.0, "h264").encode_frame());
        for chunk in bytes.chunks(3) {
            s.handle_stream_bytes(chunk, 1_000_000);
        }
        assert_eq!(s.samples_rejected(), 0);
        assert_eq!(row(&s.view(1_000_000), "Metric 1").current, Some(10.0));
    }

    #[test]
    fn an_oversized_frame_is_counted_and_the_reader_recovers() {
        let mut s = ClientStats::new(0);
        s.handle_stream_bytes(&[SERVER_MSG_STATS_SAMPLE, 0xFF, 0xFF], 0);
        assert_eq!(s.samples_rejected(), 1);
        s.handle_stream_bytes(&three_kinds(1).encode_frame(), 0);
        assert!(
            s.view(0).server_version.is_some(),
            "the next frame still parses"
        );
    }

    #[test]
    fn groups_come_in_display_order_with_other_last() {
        let mut s = ClientStats::new(0);
        s.apply_schema(schema(
            1,
            vec![
                desc(1, MetricKind::Gauge, MetricGroup::Other, "1"),
                desc(2, MetricKind::Gauge, MetricGroup::Loss, "1"),
                desc(3, MetricKind::Gauge, MetricGroup::Connection, "ms"),
                desc(4, MetricKind::Gauge, MetricGroup::Video, "1"),
                desc(5, MetricKind::Gauge, MetricGroup::Bandwidth, "bit/s"),
            ],
        ));
        let titles: Vec<&str> = s.view(0).groups.iter().map(|g| g.title).collect();
        assert_eq!(
            titles,
            vec![
                "Connection",
                "Bandwidth",
                "Video",
                "Loss and repair",
                "Other"
            ]
        );
    }

    #[test]
    fn version_comparison_ignores_dirty_and_unknown_commits() {
        let clean = "0.1.0 (aab2fa2d)";
        let dirty = "0.1.0 (aab2fa2d, dirty)";
        assert!(versions_match(clean, Some(clean)));
        assert!(versions_match(dirty, Some(dirty)));
        assert!(
            versions_match(clean, Some(dirty)),
            "dirty is shown, never a mismatch"
        );
        assert!(versions_match(dirty, Some(clean)));
        assert!(!versions_match(clean, Some("0.1.0 (0bad1dea)")));
        assert!(
            !versions_match(dirty, Some("0.1.0 (0bad1dea, dirty)")),
            "a different commit differs whatever the dirty flags say"
        );
        assert!(
            !versions_match(clean, Some("0.2.0 (aab2fa2d)")),
            "a different crate version differs"
        );
        assert!(versions_match("0.1.0 (unknown)", Some(clean)));
        assert!(versions_match(clean, Some("0.1.0 (unknown)")));
        assert!(
            versions_match("unknown", Some(clean)),
            "an unparsable string cannot be judged (the client's value before set_client_version)"
        );
        assert!(versions_match(clean, Some("")));
        assert!(
            versions_match(clean, None),
            "no server version yet: cannot judge"
        );
    }
}
```
- [ ] **Step 2: Run them and see them fail.**

```bash
cd ~/work/gf-stats-client
cargo test -p ghostframe-client-core --lib stats 2>&1 | tail -15; echo "rc=${PIPESTATUS[0]}"
```

Expected: compile errors (`cannot find type ClientStats`, `Ring`, `versions_match`, `StatsView`, `RowView` in this scope). A test target that does not compile is a failure here, not a skip.

- [ ] **Step 3: Implement.** Put this above the `#[cfg(test)] mod tests` block in `ghostframe-client-core/src/stats.rs`:

```rust
//! Connection statistics the client keeps for its stats panel.
//!
//! Two sources feed one view. **Server metrics** arrive as `STATS_SAMPLE`
//! frames on the feedback stream's return half, described by the latest
//! `STATS_SCHEMA`. **Local metrics** are measured by this client; see
//! [`crate::stats_local`].
//!
//! Plain data, no `Rc`/`RefCell`: the native client may move the core between
//! threads. Time is injected (`now_us`) like everywhere else in this crate.

use std::collections::VecDeque;

use ghostframe_protocol::stats::{
    MetricGroup, MetricKind, MetricValue, ServerFrameReader, StatsSample, StatsSchema,
    SERVER_MSG_STATS_SAMPLE, SERVER_MSG_STATS_SCHEMA,
};

/// Samples kept per metric: one minute at the 1 Hz cadence.
pub const RING_LEN: usize = 60;

/// No server sample for this long and the server rows are stale.
pub const STALE_AFTER_US: u64 = 3_000_000;

/// A fixed-capacity history, oldest first.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ring {
    buf: VecDeque<f64>,
}

impl Ring {
    pub(crate) fn push(&mut self, v: f64) {
        if self.buf.len() == RING_LEN {
            self.buf.pop_front();
        }
        self.buf.push_back(v);
    }

    pub(crate) fn to_vec(&self) -> Vec<f64> {
        self.buf.iter().copied().collect()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buf.len()
    }
}

/// One metric's state: its history, what to show now, and the counter state
/// a rate is derived from.
#[derive(Debug, Clone, Default)]
struct Series {
    ring: Ring,
    current: Option<f64>,
    text: Option<String>,
    /// Counters only: the previous `(value, time_us)`.
    prev: Option<(f64, u64)>,
}

impl Series {
    fn push_gauge(&mut self, v: f64) {
        if v.is_finite() {
            self.ring.push(v);
            self.current = Some(v);
        }
    }

    /// Feed a running total taken at `at_us`. A total that went backwards (a
    /// server restart, a session reset) or a clock that did not advance
    /// restarts the rate: nothing is pushed and the current value is cleared,
    /// so the panel shows a dash for a second rather than a negative rate.
    fn push_counter(&mut self, total: f64, at_us: u64) {
        if !total.is_finite() {
            return;
        }
        match self.prev {
            Some((prev, prev_at)) if total >= prev && at_us > prev_at => {
                let rate = (total - prev) / ((at_us - prev_at) as f64 / 1e6);
                self.ring.push(rate);
                self.current = Some(rate);
            }
            Some(_) => self.current = None,
            None => {}
        }
        self.prev = Some((total, at_us));
    }
}

/// What `ClientStats` knows, shaped for a renderer to draw without further
/// logic. Built by [`ClientStats::view`].
#[derive(Debug, Clone, PartialEq)]
pub struct StatsView {
    /// Non-empty groups in display order: Connection, Bandwidth, Video,
    /// Loss and repair, then Other.
    pub groups: Vec<GroupView>,
    pub client_version: String,
    pub server_version: Option<String>,
    pub server_host: Option<String>,
    /// `false` only when both versions are known and differ.
    pub versions_match: bool,
    /// Server frames and samples refused so far (decode errors, count or
    /// `schema_id` mismatch, a sample before any schema).
    pub samples_rejected: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupView {
    /// Stable ordering key (the group's wire byte).
    pub key: u8,
    pub title: &'static str,
    pub rows: Vec<RowView>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowView {
    /// Unique and stable while the schema lasts: `s<schema_id>.<id>` for a
    /// server metric, `c.<key>` for a local one.
    pub id: String,
    pub label: String,
    pub unit: String,
    pub kind: MetricKind,
    /// Measured by this client rather than reported by the server.
    pub local: bool,
    /// The value to show: a gauge as is, a counter as a per-second rate.
    pub current: Option<f64>,
    /// A `Text` metric's value.
    pub text: Option<String>,
    /// Oldest first, at most [`RING_LEN`] samples. Empty for text.
    pub history: Vec<f64>,
    /// Server rows only: no sample for [`STALE_AFTER_US`].
    pub stale: bool,
}

impl Default for StatsView {
    /// Nothing known yet. Versions count as matching: there is nothing to
    /// compare, and an alert before the first sample would be noise.
    fn default() -> Self {
        StatsView {
            groups: Vec::new(),
            client_version: String::new(),
            server_version: None,
            server_host: None,
            versions_match: true,
            samples_rejected: 0,
        }
    }
}

impl StatsView {
    /// The row with this id, if any group holds it.
    pub fn row(&self, id: &str) -> Option<&RowView> {
        self.groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .find(|r| r.id == id)
    }

    /// The group with this key, if it has rows.
    pub fn group(&self, key: u8) -> Option<&GroupView> {
        self.groups.iter().find(|g| g.key == key)
    }
}

/// `0.1.0 (aab2fa2d, dirty)` as `("0.1.0", "aab2fa2d")`; `None` when the
/// string is not of the form `<pkg> (<commit>[, dirty])`.
fn parse_version(s: &str) -> Option<(&str, &str)> {
    let (pkg, rest) = s.split_once(" (")?;
    let inner = rest.strip_suffix(')')?;
    let commit = inner.strip_suffix(", dirty").unwrap_or(inner);
    Some((pkg, commit))
}

/// Whether two version strings (`0.1.0 (aab2fa2d)`, `0.1.0 (aab2fa2d, dirty)`)
/// describe the same build, by the rule PR 1 fixed: the crate version and the
/// commit are compared, `dirty` never matters (the two sides compute it at
/// different build-script runs, so it can legitimately differ on current
/// code), and anything that cannot be judged (no server version yet, an
/// `unknown` commit, an unparsable string) matches.
pub fn versions_match(client: &str, server: Option<&str>) -> bool {
    let Some(server) = server else {
        return true;
    };
    match (parse_version(client), parse_version(server)) {
        (Some((client_pkg, client_commit)), Some((server_pkg, server_commit))) => {
            client_pkg == server_pkg
                && (client_commit == "unknown"
                    || server_commit == "unknown"
                    || client_commit == server_commit)
        }
        _ => true,
    }
}

fn group_order(g: &MetricGroup) -> (u8, &'static str) {
    match g {
        MetricGroup::Connection => (0, "Connection"),
        MetricGroup::Bandwidth => (1, "Bandwidth"),
        MetricGroup::Video => (2, "Video"),
        MetricGroup::Loss => (3, "Loss and repair"),
        MetricGroup::Other => (255, "Other"),
    }
}

pub struct ClientStats {
    reader: ServerFrameReader,
    schema: Option<StatsSchema>,
    server: Vec<Series>,
    last_sample_us: Option<u64>,
    rejected: u64,
    client_version: String,
}

impl ClientStats {
    pub fn new(_now_us: u64) -> Self {
        ClientStats {
            reader: ServerFrameReader::default(),
            schema: None,
            server: Vec::new(),
            last_sample_us: None,
            rejected: 0,
            client_version: "unknown".to_string(),
        }
    }

    pub fn set_client_version(&mut self, version: impl Into<String>) {
        self.client_version = version.into();
    }

    /// Server frames and samples refused so far.
    pub fn samples_rejected(&self) -> u64 {
        self.rejected
    }

    /// Feed bytes read from the feedback stream's return half, in any
    /// chunking. Complete frames are applied; an unknown frame type is
    /// skipped by its length; anything malformed is counted and dropped.
    pub fn handle_stream_bytes(&mut self, bytes: &[u8], now_us: u64) {
        self.reader.push(bytes);
        while let Some(item) = self.reader.next_frame() {
            match item {
                Err(_) => self.rejected = self.rejected.saturating_add(1),
                Ok((SERVER_MSG_STATS_SCHEMA, payload)) => {
                    match StatsSchema::decode_payload(&payload) {
                        Ok(schema) => self.apply_schema(schema),
                        Err(_) => self.rejected = self.rejected.saturating_add(1),
                    }
                }
                Ok((SERVER_MSG_STATS_SAMPLE, payload)) => {
                    let decoded = self
                        .schema
                        .as_ref()
                        .map(|schema| StatsSample::decode_payload(&payload, schema));
                    match decoded {
                        Some(Ok(sample)) => self.apply_sample(&sample, now_us),
                        Some(Err(_)) | None => self.rejected = self.rejected.saturating_add(1),
                    }
                }
                Ok(_unknown_type) => {}
            }
        }
    }

    /// Adopt a schema. A new `schema_id` clears every server ring: ids may now
    /// mean different metrics. The same id with the same metric count keeps
    /// them.
    pub fn apply_schema(&mut self, schema: StatsSchema) {
        let same = self
            .schema
            .as_ref()
            .is_some_and(|old| old.schema_id == schema.schema_id)
            && self.server.len() == schema.metrics.len();
        if !same {
            self.server = vec![Series::default(); schema.metrics.len()];
            self.last_sample_us = None;
        }
        self.schema = Some(schema);
    }

    /// Apply one sample that already decoded against the current schema.
    /// `now_us` is when it arrived, on the client's clock; it drives
    /// staleness. Counter rates use the server's own timestamps.
    pub fn apply_sample(&mut self, sample: &StatsSample, now_us: u64) {
        let Some(schema) = self.schema.as_ref() else {
            self.rejected = self.rejected.saturating_add(1);
            return;
        };
        if sample.schema_id != schema.schema_id || sample.values.len() != schema.metrics.len() {
            self.rejected = self.rejected.saturating_add(1);
            return;
        }
        for ((desc, value), series) in schema
            .metrics
            .iter()
            .zip(&sample.values)
            .zip(self.server.iter_mut())
        {
            match (&desc.kind, value) {
                (MetricKind::Gauge, MetricValue::Num(v)) => series.push_gauge(*v),
                (MetricKind::Counter, MetricValue::Num(v)) => {
                    series.push_counter(*v, sample.timestamp_us)
                }
                (MetricKind::Text, MetricValue::Text(s)) => series.text = Some(s.clone()),
                (MetricKind::Gauge | MetricKind::Counter, MetricValue::Text(_))
                | (MetricKind::Text, MetricValue::Num(_)) => {}
            }
        }
        self.last_sample_us = Some(now_us);
    }

    fn is_stale(&self, now_us: u64) -> bool {
        match self.last_sample_us {
            None => true,
            Some(t) => now_us.saturating_sub(t) > STALE_AFTER_US,
        }
    }

    /// Everything a renderer needs, as of `now_us`.
    pub fn view(&self, now_us: u64) -> StatsView {
        let stale = self.is_stale(now_us);
        let mut groups: Vec<GroupView> = Vec::new();
        let mut add = |key_title: (u8, &'static str), row: RowView| match groups
            .iter_mut()
            .find(|g| g.key == key_title.0)
        {
            Some(g) => g.rows.push(row),
            None => groups.push(GroupView {
                key: key_title.0,
                title: key_title.1,
                rows: vec![row],
            }),
        };
        if let Some(schema) = &self.schema {
            for (desc, series) in schema.metrics.iter().zip(&self.server) {
                add(
                    group_order(&desc.group),
                    RowView {
                        id: format!("s{}.{}", schema.schema_id, desc.id),
                        label: desc.label.clone(),
                        unit: desc.unit.clone(),
                        kind: desc.kind,
                        local: false,
                        current: series.current,
                        text: series.text.clone(),
                        history: series.ring.to_vec(),
                        stale,
                    },
                );
            }
        }
        groups.sort_by_key(|g| g.key);
        let server_version = self.schema.as_ref().map(|s| s.server_version.clone());
        StatsView {
            groups,
            versions_match: versions_match(&self.client_version, server_version.as_deref()),
            client_version: self.client_version.clone(),
            server_version,
            server_host: self.schema.as_ref().map(|s| s.host_name.clone()),
            samples_rejected: self.rejected,
        }
    }
}
```
- [ ] **Step 4: Run and pass.**

```bash
cd ~/work/gf-stats-client
cargo test -p ghostframe-client-core --lib stats 2>&1 | tail -25; echo "rc=${PIPESTATUS[0]}"
cargo fmt -p ghostframe-client-core
```

Expected: 15 tests pass (`ring_keeps_the_newest_sixty_in_order` through `version_comparison_ignores_dirty_and_unknown_commits`).

- [ ] **Step 5: Commit (so each mutation below can be undone with `git checkout`).**

```bash
cd ~/work/gf-stats-client
git add ghostframe-client-core/src/stats.rs ghostframe-client-core/src/lib.rs
git commit -m "feat(client-core): ClientStats keeps the server's stats in 60-sample rings" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

- [ ] **Step 6: Break the code and watch the tests fail (restore after each).**

| Mutation in `stats.rs` | Test that must go red |
|---|---|
| `push_counter`: change `Some((prev, prev_at)) if total >= prev && at_us > prev_at` to `Some((prev, prev_at)) if at_us > prev_at` | `counter_going_backwards_restarts_the_rate_and_never_goes_negative` (a negative rate is pushed) |
| `is_stale`: change `> STALE_AFTER_US` to `>= STALE_AFTER_US` | `server_rows_go_stale_after_three_seconds_and_keep_their_history` (the exact-3 s assertion) |
| `apply_schema`: make `same` always `true` | `a_new_schema_id_clears_the_server_rings_but_a_repeat_keeps_them` |
| `apply_sample`: in the `sample.schema_id != schema.schema_id || sample.values.len() != ...` branch, delete the `self.rejected = ...` line | `mismatched_samples_are_dropped_and_counted` |
| `Ring::push`: delete the `pop_front` branch | `ring_keeps_the_newest_sixty_in_order` |
| `handle_stream_bytes`: change the `Err(_)` arm to `Err(_) => {}` | `an_oversized_frame_is_counted_and_the_reader_recovers` |
| `versions_match`: replace `client_commit == server_commit` with `true` (drop the commit comparison) | `version_comparison_ignores_dirty_and_unknown_commits` (the `0bad1dea` assertions) |
| `versions_match`: compare the whole strings instead (`client == server`, the pre-review rule) | `version_comparison_ignores_dirty_and_unknown_commits` (dirty vs clean of one commit must match) |

For each: apply the one-line change, run `cargo test -p ghostframe-client-core --lib stats`, confirm the named test fails and no unrelated one does, then `git checkout -- ghostframe-client-core/src/stats.rs`. Finish with a green run and a clean `git status`.

(`ClientStats::new` names its argument `_now_us` until Task 4 uses it.)

## Task 4: Local metrics (`stats_local.rs`) and their rows in the view

**Model:** sonnet

**Files:**
- Create: `ghostframe-client-core/src/stats_local.rs`
- Modify: `ghostframe-client-core/src/stats.rs` (two patches below)
- Modify: `ghostframe-client-core/src/lib.rs` (`mod stats_local;`)

The local metrics (spec 1.3), all sampled once a second from running totals the core bumps (Tasks 5 and 6):

| Row | Group | Source |
|---|---|---|
| Received throughput (bit/s) | Bandwidth | datagram bytes x 8 per second |
| H.264 frame size (By) | Video | mean access-unit payload over the second, 0 with none |
| Painted frames (fps) | Video | `record_paints` per second |
| Solid tiles, PalRLE tiles, CDF 5/3 passes, H.264 frames (per second) | Video | the codec mix |
| Wire loss before FEC, FEC recoveries (per second), Longest loss run | Loss and repair | `WireLossStats` |
| NACKs sent, Decode errors (per second) | Loss and repair | counted by the core |

The first sample only sets the baseline, so a rate never includes the time before the first call. A local row is never stale.

- [ ] **Step 1: Add the tests first.** Two edits to `ghostframe-client-core/src/stats.rs` (as Task 3 created it; no patch file, so nothing depends on line numbers or on `cargo fmt`'s exact output).

(a) In `server_rows_go_stale_after_three_seconds_and_keep_their_history`, find the line `assert_eq!(g.history, vec![10.0], "history is kept");` and insert directly after it (local rows are never stale):

```rust
        assert!(
            !row(&past, "Received throughput").stale,
            "local rows are never stale"
        );
```

(b) In `mod tests`, insert the following three tests directly before the test `version_comparison_ignores_dirty_and_unknown_commits` (find `fn version_comparison_ignores_dirty_and_unknown_commits() {`, step up to the `#[test]` line above it, and insert the block above that line; each test below carries its own `#[test]`):

```rust
    #[test]
    fn local_metrics_are_measured_from_the_counters_and_are_non_zero() {
        let mut s = ClientStats::new(0);
        s.sample_local(1_000_000, WireLossStats::default()); // baseline
        s.local.counters.rx_bytes += 1_250_000;
        s.local.counters.nacks_sent += 30;
        s.local.counters.paints += 60;
        let loss = WireLossStats {
            lost_pre_fec: 4,
            recovered: 2,
            max_run: 3,
            ..WireLossStats::default()
        };
        s.sample_local(2_000_000, loss);
        let v = s.view(2_000_000);
        assert_eq!(row(&v, "Received throughput").current, Some(10_000_000.0));
        assert_eq!(row(&v, "NACKs sent").current, Some(30.0));
        assert_eq!(row(&v, "Painted frames").current, Some(60.0));
        assert_eq!(row(&v, "Wire loss before FEC").current, Some(4.0));
        assert_eq!(row(&v, "FEC recoveries").current, Some(2.0));
        assert_eq!(row(&v, "Longest loss run").current, Some(3.0));
        assert_eq!(
            row(&v, "Longest loss run").history,
            vec![3.0],
            "the baseline call pushes nothing"
        );
        assert!(row(&v, "Received throughput").local);
    }

    #[test]
    fn h264_frame_size_is_the_mean_of_the_second() {
        let mut s = ClientStats::new(0);
        s.sample_local(1_000_000, WireLossStats::default());
        s.local.counters.frames_h264 += 4;
        s.local.counters.h264_bytes += 4_000;
        s.sample_local(2_000_000, WireLossStats::default());
        assert_eq!(
            row(&s.view(2_000_000), "H.264 frame size").current,
            Some(1_000.0)
        );
        // A second with no H.264 frames reads 0, not the previous second's size.
        s.sample_local(3_000_000, WireLossStats::default());
        assert_eq!(
            row(&s.view(3_000_000), "H.264 frame size").current,
            Some(0.0)
        );
    }

    #[test]
    fn the_local_sample_deadline_advances() {
        let mut s = ClientStats::new(5);
        assert_eq!(
            s.next_local_sample_us(),
            5 + crate::stats_local::LOCAL_SAMPLE_INTERVAL_US
        );
        s.sample_local(2_500_000, WireLossStats::default());
        assert_eq!(s.next_local_sample_us(), 3_500_000);
    }

```

```bash
cd ~/work/gf-stats-client
cargo test -p ghostframe-client-core --lib stats 2>&1 | tail -15; echo "rc=${PIPESTATUS[0]}"
```

Expected: compile errors (`no method named sample_local found for struct ClientStats`, `no field local`, `no method next_local_sample_us`).

- [ ] **Step 2: Create `ghostframe-client-core/src/stats_local.rs`** and add `mod stats_local;` after `pub mod stats;` in `ghostframe-client-core/src/lib.rs`:

```rust
//! The client's own measurements, taken once a second whether or not the
//! stats panel is open, so a sparkline is already full when it opens.
//!
//! The core bumps [`LocalCounters`] as it works (one add per event, no
//! allocation); [`LocalStats::sample`] turns them into per-second rates.

use ghostframe_protocol::stats::{MetricGroup, MetricKind};

use crate::stats::{RowView, Series};
use crate::wire_loss::WireLossStats;

/// Local metrics are sampled this often.
pub const LOCAL_SAMPLE_INTERVAL_US: u64 = 1_000_000;

/// How a local metric is derived from the raw numbers taken each second.
#[derive(Debug, Clone, Copy)]
enum LocalSource {
    /// Per-second rate of a running total, times a scale (8 turns bytes into bits).
    Rate(f64),
    /// The raw number as is.
    Gauge,
}

struct LocalDef {
    key: &'static str,
    label: &'static str,
    unit: &'static str,
    kind: MetricKind,
    group: MetricGroup,
    source: LocalSource,
}

/// Indices into the raw numbers one sample takes; also `LOCAL_DEFS` order.
const RX_BYTES: usize = 0;
const FRAME_SIZE: usize = 1;
const PAINTS: usize = 2;
const TILES_SOLID: usize = 3;
const TILES_PALRLE: usize = 4;
const PASSES_CDF53: usize = 5;
const FRAMES_H264: usize = 6;
const WIRE_LOST: usize = 7;
const FEC_RECOVERED: usize = 8;
const LONGEST_RUN: usize = 9;
const NACKS_SENT: usize = 10;
const DECODE_ERRORS: usize = 11;
const LOCAL_COUNT: usize = 12;

static LOCAL_DEFS: [LocalDef; LOCAL_COUNT] = [
    LocalDef {
        key: "rx_throughput",
        label: "Received throughput",
        unit: "bit/s",
        kind: MetricKind::Gauge,
        group: MetricGroup::Bandwidth,
        source: LocalSource::Rate(8.0),
    },
    LocalDef {
        key: "frame_size",
        label: "H.264 frame size",
        unit: "By",
        kind: MetricKind::Gauge,
        group: MetricGroup::Video,
        source: LocalSource::Gauge,
    },
    LocalDef {
        key: "paints",
        label: "Painted frames",
        unit: "fps",
        kind: MetricKind::Gauge,
        group: MetricGroup::Video,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "tiles_solid",
        label: "Solid tiles",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Video,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "tiles_palrle",
        label: "PalRLE tiles",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Video,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "passes_cdf53",
        label: "CDF 5/3 passes",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Video,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "frames_h264",
        label: "H.264 frames",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Video,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "wire_lost",
        label: "Wire loss before FEC",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Loss,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "fec_recovered",
        label: "FEC recoveries",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Loss,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "longest_run",
        label: "Longest loss run",
        unit: "1",
        kind: MetricKind::Gauge,
        group: MetricGroup::Loss,
        source: LocalSource::Gauge,
    },
    LocalDef {
        key: "nacks_sent",
        label: "NACKs sent",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Loss,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "decode_errors",
        label: "Decode errors",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Loss,
        source: LocalSource::Rate(1.0),
    },
];

/// Running totals the core bumps as it works. Cheap: one add per event.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LocalCounters {
    pub rx_bytes: u64,
    pub tiles_solid: u64,
    pub tiles_palrle: u64,
    pub passes_cdf53: u64,
    pub frames_h264: u64,
    pub h264_bytes: u64,
    pub nacks_sent: u64,
    pub decode_errors: u64,
    pub paints: u64,
}

/// The local metrics' state.
pub(crate) struct LocalStats {
    series: Vec<Series>,
    pub(crate) counters: LocalCounters,
    next_sample_us: u64,
    last_us: Option<u64>,
    prev_h264: (u64, u64),
}

impl LocalStats {
    pub(crate) fn new(now_us: u64) -> Self {
        LocalStats {
            series: vec![Series::default(); LOCAL_COUNT],
            counters: LocalCounters::default(),
            next_sample_us: now_us + LOCAL_SAMPLE_INTERVAL_US,
            last_us: None,
            prev_h264: (0, 0),
        }
    }

    /// The deadline of the next sample, for `poll_timeout`.
    pub(crate) fn next_sample_us(&self) -> u64 {
        self.next_sample_us
    }

    /// Take the measurements. The first call only sets the baseline the
    /// rates are measured from.
    pub(crate) fn sample(&mut self, now_us: u64, loss: WireLossStats) {
        self.next_sample_us = now_us + LOCAL_SAMPLE_INTERVAL_US;
        let c = self.counters;
        let (frames, bytes) = (c.frames_h264, c.h264_bytes);
        let frame_size = if frames > self.prev_h264.0 {
            (bytes - self.prev_h264.1) as f64 / (frames - self.prev_h264.0) as f64
        } else {
            0.0
        };
        self.prev_h264 = (frames, bytes);
        let mut raw = [0.0f64; LOCAL_COUNT];
        raw[RX_BYTES] = c.rx_bytes as f64;
        raw[FRAME_SIZE] = frame_size;
        raw[PAINTS] = c.paints as f64;
        raw[TILES_SOLID] = c.tiles_solid as f64;
        raw[TILES_PALRLE] = c.tiles_palrle as f64;
        raw[PASSES_CDF53] = c.passes_cdf53 as f64;
        raw[FRAMES_H264] = c.frames_h264 as f64;
        raw[WIRE_LOST] = loss.lost_pre_fec as f64;
        raw[FEC_RECOVERED] = loss.recovered as f64;
        raw[LONGEST_RUN] = loss.max_run as f64;
        raw[NACKS_SENT] = c.nacks_sent as f64;
        raw[DECODE_ERRORS] = c.decode_errors as f64;

        let first = self.last_us.is_none();
        self.last_us = Some(now_us);
        for ((def, series), value) in LOCAL_DEFS.iter().zip(self.series.iter_mut()).zip(raw) {
            match def.source {
                LocalSource::Gauge => {
                    if !first {
                        series.push_gauge(value)
                    }
                }
                LocalSource::Rate(scale) => series.push_counter(value * scale, now_us),
            }
        }
    }

    /// Every local metric as a row, with the group it belongs to.
    pub(crate) fn rows(&self) -> Vec<(&'static MetricGroup, RowView)> {
        LOCAL_DEFS
            .iter()
            .zip(&self.series)
            .map(|(def, series)| {
                (
                    &def.group,
                    RowView {
                        id: format!("c.{}", def.key),
                        label: def.label.to_string(),
                        unit: def.unit.to_string(),
                        kind: def.kind,
                        local: true,
                        current: series.current(),
                        text: None,
                        history: series.history(),
                        stale: false,
                    },
                )
            })
            .collect()
    }
}
```
- [ ] **Step 3: Edit `stats.rs`.** Ten edits to `ghostframe-client-core/src/stats.rs`, each an anchor (text that exists exactly once in the file as Task 3 left it) and what goes there. Together they make `Series` shareable (`pub(crate)` plus `current()`/`history()` accessors), add the `local: LocalStats` field, `next_local_sample_us`, `sample_local`, the local rows in `view`, and update the module doc. If an anchor does not match, the file drifted from Task 3: find the nearest equivalent line, do not guess.

1. **Module doc.** Replace

```rust
//! `STATS_SCHEMA`. **Local metrics** are measured by this client; see
//! [`crate::stats_local`].
```

with

```rust
//! `STATS_SCHEMA`. **Local metrics** are measured here, once a second, from
//! counters the core already keeps; they run whether or not the panel is
//! open so their sparklines are full when it opens.
```

2. **Imports.** After the `use ghostframe_protocol::stats::{ ... };` block (the one ending `    SERVER_MSG_STATS_SAMPLE, SERVER_MSG_STATS_SCHEMA,\n};`) insert:

```rust

use crate::stats_local::LocalStats;
use crate::wire_loss::WireLossStats;
```

3. **`Series` visibility.** Replace `struct Series {` with `pub(crate) struct Series {`.

4. **`Series` accessors.** Replace `    fn push_gauge(&mut self, v: f64) {` with:

```rust
    pub(crate) fn current(&self) -> Option<f64> {
        self.current
    }

    pub(crate) fn history(&self) -> Vec<f64> {
        self.ring.to_vec()
    }

    pub(crate) fn push_gauge(&mut self, v: f64) {
```

and replace `    fn push_counter(&mut self, total: f64, at_us: u64) {` with `    pub(crate) fn push_counter(&mut self, total: f64, at_us: u64) {`.

5. **The field.** In `pub struct ClientStats`, replace `    rejected: u64,\n    client_version: String,\n}` with `    rejected: u64,\n    client_version: String,\n    pub(crate) local: LocalStats,\n}`.

6. **The constructor.** Replace `pub fn new(_now_us: u64) -> Self {` with `pub fn new(now_us: u64) -> Self {`, and in its struct literal, after `            client_version: "unknown".to_string(),` add `            local: LocalStats::new(now_us),`.

7. **The deadline.** Insert before the doc comment line `    /// Feed bytes read from the feedback stream's return half, in any`:

```rust
    /// The deadline of the next local sample, for `poll_timeout`.
    // Used by the core from Task 6; the attribute is removed there.
    #[allow(dead_code)]
    pub(crate) fn next_local_sample_us(&self) -> u64 {
        self.local.next_sample_us()
    }

```

8. **The sampler.** Insert before `    fn is_stale(&self, now_us: u64) -> bool {`:

```rust
    /// Take the once-a-second local measurements. The first call only sets
    /// the baseline the rates are measured from.
    // Used by the core from Task 6; the attribute is removed there.
    #[allow(dead_code)]
    pub(crate) fn sample_local(&mut self, now_us: u64, loss: WireLossStats) {
        self.local.sample(now_us, loss);
    }

```

9. **The rows.** In `view`, insert directly before `        groups.sort_by_key(|g| g.key);`:

```rust
        for (group, row) in self.local.rows() {
            add(group_order(group), row);
        }
```

(`add` is the closure `view` already uses for the server rows; the local rows go through it so they land in the same groups.)

The two `#[allow(dead_code)]` attributes exist because `sample_local` and `next_local_sample_us` have no non-test caller until Task 6, and this task's commit must be warning-free under `-D warnings`.
- [ ] **Step 4: Run and pass.**

```bash
cd ~/work/gf-stats-client
cargo fmt -p ghostframe-client-core
cargo test -p ghostframe-client-core --lib stats 2>&1 | tail -30; echo "rc=${PIPESTATUS[0]}"
```

Expected: 18 tests pass in `stats::tests` (the 15 from Task 3 plus the 3 local ones); `local_metrics_are_measured_from_the_counters_and_are_non_zero` is among them.

Then `cargo clippy -p ghostframe-client-core --all-targets -- -D warnings; echo "rc=$?"` must print `rc=0` (this is what the two `#[allow(dead_code)]` attributes are for).

- [ ] **Step 5: Commit (so each mutation below can be undone with `git checkout`).**

```bash
git add ghostframe-client-core/src/stats_local.rs ghostframe-client-core/src/stats.rs ghostframe-client-core/src/lib.rs
git commit -m "feat(client-core): local metrics (throughput, loss, codec mix, paints) in the stats view" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

- [ ] **Step 6: Break the code and watch the tests fail (restore each with `git checkout -- <file>`).**

| Mutation | Test that must go red |
|---|---|
| `stats_local.rs`: the `rx_throughput` entry `LocalSource::Rate(8.0)` -> `LocalSource::Rate(1.0)` | `local_metrics_are_measured_from_the_counters_and_are_non_zero` (1 250 000, not 10 000 000) |
| `sample`: `raw[RX_BYTES] = 0.0;` after its assignment | the same test (the throughput is zero, so the "measurement is non-zero" premise trips) |
| `sample`: `let first = self.last_us.is_none();` -> `let first = false;` | the same test, on `the baseline call pushes nothing` (`Longest loss run` history is `[0.0, 3.0]`) |
| `sample`: `let frame_size = if frames > self.prev_h264.0 {` -> `if true {` | `h264_frame_size_is_the_mean_of_the_second` (an empty second yields NaN, which is ignored, so the row keeps the old 1000 instead of 0) |
| `rows()`: `stale: false` -> `stale: true` | `server_rows_go_stale_after_three_seconds_and_keep_their_history` |

Finish with a green run and a clean `git status`.

## Task 5: `ClientCore` API: subscribe, server stream, stats accessor, paints, version

**Model:** sonnet

**Files:**
- Modify: `ghostframe-client-core/src/lib.rs`
- Create: `ghostframe-client-core/tests/stats_core.rs`

Adds exactly the contract's methods (`set_stats_subscribed`, `handle_server_stream`, `stats`, `record_paints`) plus `set_client_version` (the wasm crate's build-time version must reach `ClientStats`; see Contract deviations). `ClientCore` owns the stats by value: no `Rc`/`RefCell`.

- [ ] **Step 1: Write the failing integration tests.** Create `ghostframe-client-core/tests/stats_core.rs`:

```rust
//! `ClientCore`'s side of the stats panel: the subscribe message, the server
//! stream, and the local measurements.

use ghostframe_client_core::{ClientConfig, ClientCore, PollOutput, RowView, StatsView};
use ghostframe_protocol::stats::{
    MetricDescriptor, MetricGroup, MetricKind, MetricValue, StatsSample, StatsSchema,
};

fn test_core() -> ClientCore {
    let mut core = ClientCore::new(ClientConfig::default(), 0);
    while core.poll_transmit(0).is_some() {} // the Hello
    core
}

fn row(view: &StatsView, label: &str) -> RowView {
    view.groups
        .iter()
        .flat_map(|g| g.rows.iter())
        .find(|r| r.label == label)
        .unwrap_or_else(|| panic!("no row {label}: {view:?}"))
        .clone()
}

#[test]
fn subscribe_and_unsubscribe_are_queued_on_the_feedback_stream() {
    let mut core = test_core();
    core.set_stats_subscribed(true);
    core.set_stats_subscribed(false);
    assert_eq!(
        core.poll_transmit(0),
        Some(PollOutput::Stream(vec![0x0B, 1]))
    );
    assert_eq!(
        core.poll_transmit(0),
        Some(PollOutput::Stream(vec![0x0B, 0]))
    );
    assert_eq!(core.poll_transmit(0), None);
}

#[test]
fn the_server_stream_reaches_the_view() {
    let mut core = test_core();
    core.set_client_version("0.1.0 (aab2fa2d)");
    let schema = StatsSchema {
        schema_id: 7,
        server_version: "0.1.0 (aab2fa2d)".to_string(),
        host_name: "evangeline".to_string(),
        metrics: vec![MetricDescriptor {
            id: 1,
            name: "ghostframe.rtt".to_string(),
            unit: "ms".to_string(),
            kind: MetricKind::Gauge,
            group: MetricGroup::Connection,
            label: "Round trip".to_string(),
        }],
    };
    let sample = StatsSample {
        schema_id: 7,
        timestamp_us: 1_000_000,
        values: vec![MetricValue::Num(18.4)],
    };
    let mut bytes = schema.encode_frame();
    bytes.extend(sample.encode_frame());
    // Delivered in two reads, split mid-frame.
    let (a, b) = bytes.split_at(bytes.len() / 2);
    core.handle_server_stream(a, 2_000_000);
    core.handle_server_stream(b, 2_000_000);
    let view = core.stats().view(2_000_000);
    assert_eq!(row(&view, "Round trip").current, Some(18.4));
    assert_eq!(view.server_host.as_deref(), Some("evangeline"));
    assert_eq!(view.client_version, "0.1.0 (aab2fa2d)");
    assert!(view.versions_match);
    assert_eq!(core.stats().samples_rejected(), 0);
}
```

- [ ] **Step 2: Run and see it fail.**

```bash
cd ~/work/gf-stats-client
cargo test -p ghostframe-client-core --test stats_core 2>&1 | tail -15; echo "rc=${PIPESTATUS[0]}"
```

Expected: compile errors (`no method named set_stats_subscribed`, `handle_server_stream`, `stats`, `set_client_version` found for `ClientCore`).

- [ ] **Step 3: Implement.** Three edits in `ghostframe-client-core/src/lib.rs`.

(a) Add the field at the end of `struct ClientCore` (after `pub(crate) datagrams_frame: u64,`):

```rust
    /// Connection statistics for the stats panel; see [`stats`].
    pub(crate) stats: stats::ClientStats,
```

(b) Initialise it at the end of the struct literal in `ClientCore::new` (after `datagrams_frame: 0,`):

```rust
            stats: stats::ClientStats::new(now_us),
```

(c) Add these methods to `impl ClientCore`, directly after `poll_transmit`:

```rust
    /// Ask the server to start (`true`) or stop (`false`) sending stats
    /// samples. Queued like any other feedback-stream message; the server
    /// answers every request with a fresh schema.
    pub fn set_stats_subscribed(&mut self, on: bool) {
        self.outbox.push_back(PollOutput::Stream(
            ghostframe_protocol::stats::StatsSubscribe { on }
                .encode()
                .to_vec(),
        ));
    }

    /// Feed bytes read from the feedback stream's return half, in whatever
    /// chunks the transport produced. Never panics on malformed input: bad
    /// frames are counted in [`stats::ClientStats::samples_rejected`].
    pub fn handle_server_stream(&mut self, bytes: &[u8], now_us: u64) {
        self.stats.handle_stream_bytes(bytes, now_us);
    }

    /// The connection statistics: server metrics from the stats stream and
    /// the client's own measurements.
    pub fn stats(&self) -> &stats::ClientStats {
        &self.stats
    }

    /// Note `n` frames painted. Pushed by the host, which owns the renderer
    /// until it moves into Rust.
    pub fn record_paints(&mut self, n: u32) {
        self.stats.local.counters.paints = self
            .stats
            .local
            .counters
            .paints
            .saturating_add(u64::from(n));
    }

    /// The build this client came from, shown beside the server's in the
    /// stats panel. Defaults to `unknown`.
    pub fn set_client_version(&mut self, version: impl Into<String>) {
        self.stats.set_client_version(version);
    }
```

- [ ] **Step 4: Run and pass.**

```bash
cd ~/work/gf-stats-client
cargo fmt -p ghostframe-client-core
cargo test -p ghostframe-client-core --test stats_core 2>&1 | tail -8; echo "rc=${PIPESTATUS[0]}"
```

Expected: `2 passed`.

- [ ] **Step 5: Commit, then break it.**

```bash
git add ghostframe-client-core/src/lib.rs ghostframe-client-core/tests/stats_core.rs
git commit -m "feat(client-core): ClientCore queues STATS_SUBSCRIBE and feeds the server stream to ClientStats" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

| Mutation | Test that must go red |
|---|---|
| `set_stats_subscribed`: `StatsSubscribe { on: true }` | `subscribe_and_unsubscribe_are_queued_on_the_feedback_stream` (second message is `[0x0B, 1]`, not `[0x0B, 0]`) |
| `handle_server_stream`: empty body | `the_server_stream_reaches_the_view` (`no row Round trip`) |
| `set_client_version`: empty body | `the_server_stream_reaches_the_view` (`client_version` is `unknown`) |

Restore each with `git checkout -- ghostframe-client-core/src/lib.rs`.

## Task 6: Local counters wired into the core (bytes, codec mix, NACKs, sampling clock)

**Model:** sonnet

**Files:**
- Modify: `ghostframe-client-core/src/lib.rs` (`poll_transmit`, `poll_timeout`, `on_timeout`, a unit test)
- Modify: `ghostframe-client-core/src/reassembly.rs` (`handle_datagram`, `tally_events`)
- Modify: `ghostframe-client-core/src/stats.rs` (drop the two temporary `#[allow(dead_code)]` from Task 4)
- Modify: `ghostframe-client-core/tests/stats_core.rs` (one more test)

Where each number comes from, and why there:

- **Received bytes:** `handle_datagram`, every inbound datagram (including ones the core drops): "datagram bytes" is what the link delivered.
- **Codec mix and decode errors:** counted on the *payload* events inside `handle_datagram`, before the pixel decode turns them into `TileReady`, so `TileDelivery::Decoded` (native) and `Payload` (browser) count the same. These are core-detected decode errors; WebGPU errors the renderer reports through `WasmDecodeErrorBatcher` are not counted here (spec gap noted in the Self-review).
- **NACKs sent:** in `poll_transmit`, where every outbound NACK datagram passes, summing the envelope's entry count: three queuing sites produce NACKs, one drain point sends them. Counted at hand-off to the wire.
- **Sampling clock:** `on_timeout` samples when `now_us` reaches the local deadline; `poll_timeout` includes the deadline so the contract "call `on_timeout` by `poll_timeout`" stays true (today the 100 ms feedback deadline is always earlier, so this `min` is structurally redundant and no test can tell it apart; it is there so the contract does not silently depend on that).

- [ ] **Step 1: Write the failing tests.**

Append to `ghostframe-client-core/tests/stats_core.rs`, and add the `fragment_tile` imports at the top (`use ghostframe_protocol::protocol::{fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG};`):

```rust
fn solid_datagram(frame_seq: u32, x: u8) -> Vec<u8> {
    let mut dgs = fragment_tile(
        &TileFragmentInputs {
            frame_seq: frame_seq | TILE_DATAGRAM_FLAG,
            tile_x: x,
            tile_y: 0,
            codec: Codec::Solid,
            generation: 1,
            pass: 0,
            timestamp_us: 0,
        },
        &[0x11, 0x22, 0x33, 0xFF],
        1200,
    );
    assert_eq!(dgs.len(), 1);
    dgs.remove(0)
}

#[test]
fn local_metrics_are_measured_without_any_subscription() {
    let mut core = test_core();
    core.on_timeout(1_000_000); // baseline
    let mut bytes = 0u64;
    for x in 0..5u8 {
        let dg = solid_datagram(1, x);
        bytes += dg.len() as u64;
        core.handle_datagram(&dg, 1_500_000);
    }
    core.record_paints(30);
    core.on_timeout(2_000_000);
    let view = core.stats().view(2_000_000);
    assert_eq!(row(&view, "Solid tiles").current, Some(5.0));
    assert_eq!(row(&view, "Painted frames").current, Some(30.0));
    let bits = row(&view, "Received throughput").current.expect("measured");
    assert_eq!(bits, (bytes * 8) as f64);
    assert!(bits > 0.0, "the measurement fired");
    assert_eq!(row(&view, "PalRLE tiles").current, Some(0.0));
}
```

Append this module at the very end of `ghostframe-client-core/src/lib.rs` (it needs `queue_pass_nack`, which is `pub(crate)`):

```rust
#[cfg(test)]
mod stats_tests {
    use super::*;

    fn row_rate(core: &ClientCore, now_us: u64, label: &str) -> Option<f64> {
        core.stats()
            .view(now_us)
            .groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .find(|r| r.label == label)
            .unwrap_or_else(|| panic!("no row {label}"))
            .current
    }

    /// NACKs are counted as they leave through `poll_transmit`, whichever of
    /// the three queuing sites produced the datagram.
    #[test]
    fn nacks_sent_counts_entries_as_they_leave() {
        let mut core = ClientCore::new(ClientConfig::default(), 0);
        while core.poll_transmit(0).is_some() {}
        core.on_timeout(1_000_000); // local baseline
        for x in 0..3u8 {
            core.queue_pass_nack(1, x, 0, 0, 1_100_000);
        }
        core.on_timeout(1_100_000 + NACK_DEBOUNCE_US);
        let mut nack_datagrams = 0;
        while let Some(out) = core.poll_transmit(0) {
            if let PollOutput::Datagram(d) = out {
                if d.first() == Some(&ghostframe_protocol::protocol::TILE_NACK_ENVELOPE) {
                    nack_datagrams += 1;
                }
            }
        }
        assert_eq!(nack_datagrams, 1, "premise: one NACK datagram went out");
        core.on_timeout(2_000_000);
        assert_eq!(row_rate(&core, 2_000_000, "NACKs sent"), Some(3.0));
    }
}
```

- [ ] **Step 2: Run and see them fail.**

```bash
cd ~/work/gf-stats-client
cargo test -p ghostframe-client-core --test stats_core local_metrics 2>&1 | tail -12; echo "rc=${PIPESTATUS[0]}"
cargo test -p ghostframe-client-core --lib stats_tests 2>&1 | tail -12; echo "rc=${PIPESTATUS[0]}"
```

Expected: `local_metrics_are_measured_without_any_subscription` fails (`no row Solid tiles` is not the failure: the row exists; it panics on `Some(5.0)` vs `Some(0.0)`); `nacks_sent_counts_entries_as_they_leave` fails (`left: Some(0.0)`, `right: Some(3.0)`).

- [ ] **Step 3: Implement.** Edits in `ghostframe-client-core/src/lib.rs`:

(a) Replace `poll_transmit`:

```rust
    /// Drain one pending outbound message; call until None.
    pub fn poll_transmit(&mut self, _now_us: u64) -> Option<PollOutput> {
        let out = self.outbox.pop_front()?;
        // Counted where every NACK passes on its way out, so the tally cannot
        // miss one of the three places that queue them.
        if let PollOutput::Datagram(d) = &out {
            if d.len() >= 2 && d[0] == ghostframe_protocol::protocol::TILE_NACK_ENVELOPE {
                self.stats.local.counters.nacks_sent = self
                    .stats
                    .local
                    .counters
                    .nacks_sent
                    .saturating_add(u64::from(d[1]));
            }
        }
        Some(out)
    }
```

(b) In `poll_timeout`, just before the final `Some(deadline)`:

```rust
        deadline = deadline.min(self.stats.next_local_sample_us());
```

and extend its doc comment's list with `, next local stats sample`.

(c) In `on_timeout`, right after `self.tail_sweep(now_us);`:

```rust
        if now_us >= self.stats.next_local_sample_us() {
            self.stats.sample_local(now_us, self.wire_loss.stats());
        }
```

(d) Both methods now have a caller: in `ghostframe-client-core/src/stats.rs` delete the two `// Used by the core from Task 6; the attribute is removed there.` comments and the `#[allow(dead_code)]` lines above `next_local_sample_us` and `sample_local`, and add `ghostframe-client-core/src/stats.rs` to this task's `git add`.

Edits in `ghostframe-client-core/src/reassembly.rs`: in `handle_datagram`, after `let events = self.handle_datagram_payloads(bytes, now_us);` insert:

```rust
        self.stats.local.counters.rx_bytes = self
            .stats
            .local
            .counters
            .rx_bytes
            .saturating_add(bytes.len() as u64);
        self.tally_events(&events);
```

and add this method to the same `impl ClientCore` block, before `fn handle_datagram_payloads`:

```rust
    /// Count what a datagram produced for the stats panel's codec mix.
    /// Runs on the payload events, before the pixel decode turns them into
    /// `TileReady`, so `Decoded` and `Payload` delivery count the same.
    fn tally_events(&mut self, events: &[Event]) {
        let c = &mut self.stats.local.counters;
        for e in events {
            match e {
                Event::TilePayload { data, .. } => match data {
                    TileData::Solid(_) => c.tiles_solid = c.tiles_solid.saturating_add(1),
                    TileData::PalRle { .. } => c.tiles_palrle = c.tiles_palrle.saturating_add(1),
                    TileData::Cdf53 { .. } | TileData::Cdf53Wire { .. } => {
                        c.passes_cdf53 = c.passes_cdf53.saturating_add(1)
                    }
                },
                Event::NeedsH264 { payload, .. } => {
                    c.frames_h264 = c.frames_h264.saturating_add(1);
                    c.h264_bytes = c.h264_bytes.saturating_add(payload.len() as u64);
                }
                Event::DecodeError { .. } => c.decode_errors = c.decode_errors.saturating_add(1),
                Event::TileReady { .. }
                | Event::FrameDimensions { .. }
                | Event::PaletteUpdated { .. }
                | Event::Evicted { .. } => {}
            }
        }
    }
```

(`TileData` and `Event` are already imported in `reassembly.rs`. The match lists every `Event` and `TileData` variant on purpose: this crate warns on `_ =>` over enums, and a new variant must force a decision here.)

- [ ] **Step 4: Run and pass, then the whole crate.**

```bash
cd ~/work/gf-stats-client
cargo fmt -p ghostframe-client-core
cargo test -p ghostframe-client-core 2>&1 | grep -E "test result|FAILED|panicked" | sort | uniq -c; echo "rc=${PIPESTATUS[0]}"
cargo clippy -p ghostframe-client-core --all-targets -- -D warnings 2>&1 | tail -5; echo "rc=${PIPESTATUS[0]}"
```

Expected: every `test result` is `ok` (the existing suites are untouched), clippy clean.

- [ ] **Step 5: Commit, then break it.**

```bash
git add ghostframe-client-core/src/lib.rs ghostframe-client-core/src/reassembly.rs ghostframe-client-core/src/stats.rs ghostframe-client-core/tests/stats_core.rs
git commit -m "feat(client-core): count bytes, codec mix and NACKs for the stats panel" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

| Mutation | Test that must go red |
|---|---|
| `poll_transmit`: delete the NACK-counting `if` | `nacks_sent_counts_entries_as_they_leave` |
| `tally_events`: replace `TileData::Solid(_) => c.tiles_solid = ...` with `TileData::Solid(_) => {}` | `local_metrics_are_measured_without_any_subscription` (`Solid tiles`) |
| `handle_datagram`: delete the `rx_bytes` assignment | the same test (throughput `0`, so the "non-zero" assertion trips) |
| `on_timeout`: delete the `sample_local` call | the same test (`no value`: `expect("measured")` panics) and the NACK test |

Restore each with `git checkout -- <file>`; finish green.

## Task 7: `format.rs` (SI prefixes, three significant digits, row text)

**Model:** sonnet

**Files:**
- Create: `ghostframe-web-ui/src/format.rs`
- Modify: `ghostframe-web-ui/src/lib.rs` (register the module)

Rules fixed here (spec 2.4): three significant digits with trailing zeros dropped (`42.1 Mbit/s`, `18.4 ms`, `12 /s`); SI prefixes (decimal, up to T) for `bit/s`, `By` (shown `B`), bare counts (`1`) and unknown units; `ms` becomes seconds at a thousand; `s` is an uptime (`3m 07s`); a counter's value is already a per-second rate (`ClientStats::view` computed it) and gets `/s`; non-finite shows a dash; a stale row shows a dash. A number that rounds up a decade (`9.996`, `999 960 bit/s`) must carry (`10`, `1 Mbit/s`), never print four digits: `sig3` gets the first for free (the added zero is a trailing zero and is dropped), `si_scale` needs an explicit check for the second.

- [ ] **Step 1: Write the failing tests.** Create `ghostframe-web-ui/src/format.rs` with only this (the implementation comes in step 3) and register the module by replacing the placeholder `ghostframe-web-ui/src/lib.rs` with `pub mod format;`:

```rust
use ghostframe_client_core::{MetricKind, RowView};

#[cfg(test)]
mod tests {
    use super::*;

    fn g(v: f64, unit: &str) -> String {
        format_value(v, unit, &MetricKind::Gauge)
    }
    fn c(v: f64, unit: &str) -> String {
        format_value(v, unit, &MetricKind::Counter)
    }

    #[test]
    fn three_significant_digits() {
        assert_eq!(sig3(42.1234), "42.1");
        assert_eq!(sig3(18.4), "18.4");
        assert_eq!(sig3(1.5), "1.5");
        assert_eq!(sig3(123.456), "123");
        assert_eq!(sig3(0.01234), "0.0123");
        assert_eq!(sig3(0.0), "0");
        assert_eq!(sig3(-5.0), "-5");
    }

    #[test]
    fn rounding_up_a_decade_keeps_three_digits() {
        assert_eq!(sig3(9.996), "10");
        assert_eq!(sig3(99.96), "100");
    }

    #[test]
    fn si_prefixes_on_rates_and_sizes() {
        assert_eq!(g(42_100_000.0, "bit/s"), "42.1 Mbit/s");
        assert_eq!(g(950.0, "bit/s"), "950 bit/s");
        assert_eq!(g(1_234_567.0, "By"), "1.23 MB");
        assert_eq!(g(2_500_000_000.0, "bit/s"), "2.5 Gbit/s");
    }

    #[test]
    fn a_value_that_rounds_up_to_a_thousand_takes_the_next_prefix() {
        assert_eq!(g(999_960.0, "bit/s"), "1 Mbit/s");
    }

    #[test]
    fn milliseconds_switch_to_seconds_at_a_thousand() {
        assert_eq!(g(18.4, "ms"), "18.4 ms");
        assert_eq!(g(0.0, "ms"), "0 ms");
        assert_eq!(g(1500.0, "ms"), "1.5 s");
        assert_eq!(g(999.96, "ms"), "1 s");
    }

    #[test]
    fn counters_are_per_second_rates() {
        assert_eq!(c(12.0, "1"), "12 /s");
        assert_eq!(c(1500.0, "1"), "1.5 k/s");
        assert_eq!(c(0.0, "1"), "0 /s");
        assert_eq!(c(2048.0, "By"), "2.05 kB/s");
    }

    #[test]
    fn plain_counts_have_no_unit() {
        assert_eq!(g(3.0, "1"), "3");
        assert_eq!(g(1500.0, "1"), "1.5 k");
    }

    #[test]
    fn durations_read_as_uptime() {
        assert_eq!(g(7.0, "s"), "7 s");
        assert_eq!(g(187.0, "s"), "3m 07s");
        assert_eq!(g(3723.0, "s"), "1h 02m");
        assert_eq!(g(90_000.0, "s"), "1d 01h");
    }

    #[test]
    fn non_finite_values_show_a_dash() {
        assert_eq!(g(f64::NAN, "ms"), DASH);
        assert_eq!(c(f64::INFINITY, "1"), DASH);
    }

    fn row(kind: MetricKind, current: Option<f64>, text: Option<&str>, stale: bool) -> RowView {
        RowView {
            id: "s1.1".to_string(),
            label: "x".to_string(),
            unit: "ms".to_string(),
            kind,
            local: false,
            current,
            text: text.map(str::to_string),
            history: vec![],
            stale,
        }
    }

    #[test]
    fn rows_show_a_dash_when_stale_or_empty() {
        assert_eq!(
            format_row(&row(MetricKind::Gauge, Some(18.4), None, false)),
            "18.4 ms"
        );
        assert_eq!(
            format_row(&row(MetricKind::Gauge, Some(18.4), None, true)),
            DASH
        );
        assert_eq!(format_row(&row(MetricKind::Gauge, None, None, false)), DASH);
        assert_eq!(
            format_row(&row(MetricKind::Text, None, Some("h264"), false)),
            "h264"
        );
        assert_eq!(format_row(&row(MetricKind::Text, None, None, false)), DASH);
        assert_eq!(
            format_row(&row(MetricKind::Text, None, Some("h264"), true)),
            DASH
        );
    }
}
```
- [ ] **Step 2: Run and see it fail.**

```bash
cd ~/work/gf-stats-client
cargo test -p ghostframe-web-ui --lib format 2>&1 | tail -12; echo "rc=${PIPESTATUS[0]}"
```

Expected: compile errors (`cannot find function format_value`, `format_row`, `sig3`, `DASH`).

- [ ] **Step 3: Implement.** Replace the one-line `use` at the top of `format.rs` with this (the tests stay below it):

```rust
//! Number and row formatting for the stats panel. Plain Rust: no DOM, so it
//! is tested with `cargo test` on the host.

use ghostframe_client_core::{MetricKind, RowView};

/// Shown where there is no value: stale, not yet measured, or not a number.
pub const DASH: &str = "\u{2014}";

const PREFIXES: [&str; 5] = ["", "k", "M", "G", "T"];

/// Units that are not scaled with SI prefixes (`ms` has its own rule, `s` is
/// a duration).
const UNPREFIXED: [&str; 5] = ["ms", "s", "%", "fps", "Hz"];

/// `v` to three significant digits, trailing zeros dropped: `42.1`, `18.4`,
/// `1.5`, `12`, `123`, `0.0123`. A value that rounds up a decade (`9.996`)
/// prints `10`: the extra zero the rounding adds is one of the trailing zeros
/// that are dropped.
pub fn sig3(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        return "0".to_string();
    }
    let a = v.abs();
    let sign = if v < 0.0 { "-" } else { "" };
    let mag = a.log10().floor() as i32;
    let decimals = (2 - mag).max(0) as usize;
    let text = format!("{:.*}", decimals, a);
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        &text
    };
    format!("{sign}{text}")
}

/// What `sig3` would print, as a number: used to see a carry coming.
fn rounded_sig3(v: f64) -> f64 {
    sig3(v).parse().unwrap_or(v)
}

/// Scale `v` into `[1, 1000)` and name the SI prefix: `42_100_000` is
/// `(42.1, "M")`. Stops at tera.
fn si_scale(v: f64) -> (f64, &'static str) {
    let mut scaled = v;
    let mut e = 0;
    while e + 1 < PREFIXES.len() && (scaled.abs() >= 1000.0 || rounded_sig3(scaled).abs() >= 1000.0)
    {
        scaled /= 1000.0;
        e += 1;
    }
    (scaled, PREFIXES[e])
}

/// `1h 02m`, `3m 07s`, `12 s`: an uptime, not a measurement.
pub fn format_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return DASH.to_string();
    }
    let s = secs as u64;
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        3600..=86_399 => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d {:02}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// `v` in `unit` for display. A counter's `v` is already a per-second rate
/// (`ClientStats::view` computed it), so it gets a `/s`.
pub fn format_value(v: f64, unit: &str, kind: &MetricKind) -> String {
    if !v.is_finite() {
        return DASH.to_string();
    }
    let per_second = matches!(kind, MetricKind::Counter);
    if unit == "s" && !per_second {
        return format_duration(v);
    }
    let (number, suffix) = if unit == "ms" {
        if rounded_sig3(v).abs() >= 1000.0 {
            (sig3(v / 1000.0), "s".to_string())
        } else {
            (sig3(v), "ms".to_string())
        }
    } else if UNPREFIXED.contains(&unit) {
        (sig3(v), unit.to_string())
    } else {
        let (scaled, prefix) = si_scale(v);
        // UCUM spells bytes `By`; people write `B`. `1` is a bare count.
        let base = match unit {
            "1" => "",
            "By" => "B",
            other => other,
        };
        (sig3(scaled), format!("{prefix}{base}"))
    };
    let suffix = if per_second {
        format!("{suffix}/s")
    } else {
        suffix
    };
    if suffix.is_empty() {
        number
    } else {
        format!("{number} {suffix}")
    }
}

/// The text a metric row shows for its value.
pub fn format_row(row: &RowView) -> String {
    if row.stale {
        return DASH.to_string();
    }
    match row.kind {
        MetricKind::Text => row.text.clone().unwrap_or_else(|| DASH.to_string()),
        MetricKind::Gauge | MetricKind::Counter => row
            .current
            .map(|v| format_value(v, &row.unit, &row.kind))
            .unwrap_or_else(|| DASH.to_string()),
    }
}
```
- [ ] **Step 4: Run and pass.**

```bash
cd ~/work/gf-stats-client
cargo fmt -p ghostframe-web-ui
cargo test -p ghostframe-web-ui --lib format 2>&1 | tail -20; echo "rc=${PIPESTATUS[0]}"
```

Expected: 10 tests pass.

- [ ] **Step 5: Commit, then break it.**

```bash
git add ghostframe-web-ui/src/format.rs ghostframe-web-ui/src/lib.rs
git commit -m "feat(web-ui): SI value formatting for the stats panel" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

| Mutation in `format.rs` | Test that must go red |
|---|---|
| `si_scale`: delete `|| rounded_sig3(scaled).abs() >= 1000.0` | `a_value_that_rounds_up_to_a_thousand_takes_the_next_prefix` |
| `"By" => "B"` -> `"By" => "By"` | `si_prefixes_on_rates_and_sizes` and `counters_are_per_second_rates` |
| `format_value`: `per_second` forced `false` | `counters_are_per_second_rates` |
| `format_row`: drop the `if row.stale` early return | `rows_show_a_dash_when_stale_or_empty` |
| `format_value`: delete the `if !v.is_finite()` guard | `non_finite_values_show_a_dash` |

Restore each with `git checkout -- ghostframe-web-ui/src/format.rs`.

## Task 8: `spark.rs` (sparkline geometry)

**Model:** sonnet

**Files:**
- Create: `ghostframe-web-ui/src/spark.rs`
- Modify: `ghostframe-web-ui/src/lib.rs`

Spec 2.1: an inline SVG polyline over the 60-sample ring scaled to the ring's own min and max; a flat series draws a midline; an empty one draws nothing. Choices made here: the 60-sample window is right-aligned (a young series grows in from the right edge, the way time does); a single sample counts as flat; non-finite samples are skipped.

- [ ] **Step 1: Write the failing tests.** Create `ghostframe-web-ui/src/spark.rs` with only this, and add `pub mod spark;` to `lib.rs`:

```rust
use ghostframe_client_core::stats::RING_LEN;

pub const WIDTH: f64 = 120.0;
pub const HEIGHT: f64 = 24.0;
const PAD: f64 = 2.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_series_draws_nothing() {
        assert!(sparkline_points(&[], WIDTH, HEIGHT).is_empty());
        assert!(sparkline_points(&[f64::NAN], WIDTH, HEIGHT).is_empty());
        assert_eq!(points_attr(&[]), "");
    }

    #[test]
    fn flat_and_single_series_draw_a_midline() {
        let mid = HEIGHT / 2.0;
        for h in [vec![5.0], vec![5.0, 5.0, 5.0], vec![0.0, 0.0]] {
            let p = sparkline_points(&h, WIDTH, HEIGHT);
            assert_eq!(p, vec![(0.0, mid), (WIDTH, mid)], "{h:?}");
        }
    }

    #[test]
    fn a_moving_series_spans_the_box_between_its_min_and_max() {
        let p = sparkline_points(&[1.0, 3.0, 2.0], WIDTH, HEIGHT);
        assert_eq!(p.len(), 3);
        let ys: Vec<f64> = p.iter().map(|(_, y)| *y).collect();
        assert_eq!(ys[0], HEIGHT - PAD, "min at the bottom");
        assert_eq!(ys[1], PAD, "max at the top");
        assert!(ys[2] > ys[1] && ys[2] < ys[0], "between");
        assert!(p.windows(2).all(|w| w[0].0 < w[1].0), "x increases");
    }

    #[test]
    fn the_window_is_right_aligned_and_full_series_span_the_width() {
        let few = sparkline_points(&[1.0, 2.0], WIDTH, HEIGHT);
        assert_eq!(
            few.last().map(|p| p.0),
            Some(WIDTH),
            "newest at the right edge"
        );
        assert!(few[0].0 > 0.0, "a young series does not start at the left");
        let full: Vec<f64> = (0..RING_LEN).map(|i| i as f64).collect();
        let p = sparkline_points(&full, WIDTH, HEIGHT);
        assert_eq!(p.first().map(|p| p.0), Some(0.0));
        assert_eq!(p.last().map(|p| p.0), Some(WIDTH));
    }

    #[test]
    fn non_finite_samples_are_skipped() {
        let p = sparkline_points(&[1.0, f64::NAN, 3.0], WIDTH, HEIGHT);
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn attribute_has_one_decimal_per_coordinate() {
        assert_eq!(
            points_attr(&[(0.0, 22.0), (120.0, 2.04)]),
            "0.0,22.0 120.0,2.0"
        );
    }
}
```
(The three constants are in the test-only file so `use super::*` resolves; the functions are what is missing.)

- [ ] **Step 2: Run and see it fail.**

```bash
cd ~/work/gf-stats-client
cargo test -p ghostframe-web-ui --lib spark 2>&1 | tail -12; echo "rc=${PIPESTATUS[0]}"
```

Expected: `cannot find function sparkline_points` / `points_attr`.

- [ ] **Step 3: Implement.** Replace everything above `#[cfg(test)]` in `spark.rs` with:

```rust
//! Sparkline geometry: pure arithmetic from a history to points in a
//! `WIDTH` x `HEIGHT` box. No DOM, so `cargo test` runs it on the host.

use ghostframe_client_core::stats::RING_LEN;

pub const WIDTH: f64 = 120.0;
pub const HEIGHT: f64 = 24.0;
const PAD: f64 = 2.0;

/// Points for `history` (oldest first), scaled to its own min and max. The
/// window is `RING_LEN` samples wide and right-aligned, so a young series
/// grows in from the right the way time does.
///
/// * empty (or no finite sample): no points, nothing is drawn;
/// * flat (all samples equal, or one sample): a midline across the box;
/// * otherwise: the polyline, `max` at the top and `min` at the bottom.
pub fn sparkline_points(history: &[f64], width: f64, height: f64) -> Vec<(f64, f64)> {
    let finite: Vec<f64> = history.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.is_empty() {
        return Vec::new();
    }
    let min = finite.iter().copied().fold(f64::INFINITY, f64::min);
    let max = finite.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mid = height / 2.0;
    if finite.len() < 2 || max - min <= f64::EPSILON * max.abs().max(1.0) {
        return vec![(0.0, mid), (width, mid)];
    }
    let n = finite.len().min(RING_LEN);
    let finite = &finite[finite.len() - n..];
    let step = width / (RING_LEN - 1) as f64;
    let first = (RING_LEN - n) as f64 * step;
    let inner = height - 2.0 * PAD;
    finite
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = first + i as f64 * step;
            let y = PAD + (1.0 - (v - min) / (max - min)) * inner;
            (x, y)
        })
        .collect()
}

/// The SVG `points` attribute for `pts`: `"x,y x,y ..."`, one decimal.
pub fn points_attr(pts: &[(f64, f64)]) -> String {
    pts.iter()
        .map(|(x, y)| format!("{x:.1},{y:.1}"))
        .collect::<Vec<_>>()
        .join(" ")
}
```
- [ ] **Step 4: Run and pass.**

```bash
cd ~/work/gf-stats-client
cargo fmt -p ghostframe-web-ui
cargo test -p ghostframe-web-ui --lib spark 2>&1 | tail -12; echo "rc=${PIPESTATUS[0]}"
```

Expected: 6 tests pass.

- [ ] **Step 5: Commit, then break it.**

```bash
git add ghostframe-web-ui/src/spark.rs ghostframe-web-ui/src/lib.rs
git commit -m "feat(web-ui): sparkline geometry" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

| Mutation in `spark.rs` | Test that must go red |
|---|---|
| `let first = (RING_LEN - n) as f64 * step;` -> `let first = 0.0;` | `the_window_is_right_aligned_and_full_series_span_the_width` |
| delete the `if finite.len() < 2 \|\| max - min <= ...` flat branch | `flat_and_single_series_draw_a_midline` (a divide by zero gives NaN points) |
| `let y = PAD + (1.0 - ...` -> `let y = PAD + ((v - min) / (max - min)) * inner;` (flipped) | `a_moving_series_spans_the_box_between_its_min_and_max` |
| delete the `.filter(\|v\| v.is_finite())` | `non_finite_samples_are_skipped` and `empty_series_draws_nothing` |

Restore each with `git checkout -- ghostframe-web-ui/src/spark.rs`.

## Task 9: `storage.rs`, the `PanelBackend` trait, and `CoreBackend`

**Model:** sonnet

**Files:**
- Create: `ghostframe-web-ui/src/storage.rs`, `ghostframe-web-ui/src/backend.rs`
- Modify: `ghostframe-web-ui/src/lib.rs`

`storage.rs` keeps the open state; "try/catch" in Rust is: `local_storage()` is `Err` when access is denied and `Ok(None)` when there is none, `get_item`/`set_item` return `Result`; every `Err`/`None` means "start closed" and is dropped. The pure part (`parse_open`) is host-tested; the browser part is exercised by the wasm test (Task 11).

- [ ] **Step 1: Write the failing test.** Create `ghostframe-web-ui/src/storage.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::parse_open;

    #[test]
    fn only_an_explicit_one_is_open() {
        assert!(parse_open(Some("1")));
        assert!(!parse_open(Some("0")));
        assert!(!parse_open(Some("true")));
        assert!(!parse_open(Some("")));
        assert!(!parse_open(None));
    }
}
```

and put this in `ghostframe-web-ui/src/lib.rs` (replacing its contents; `components` and `mount_stats_panel` arrive in Task 11):

```rust
//! The browser client's DOM UI. Leptos lives here and nowhere else in the
//! workspace: swapping the framework means rewriting this crate.
//!
//! The video path (WebTransport, WebCodecs, WebGPU, input) stays
//! framework-free. This crate draws the stats panel beside it and talks to
//! the core only through [`PanelBackend`].

use ghostframe_client_core::StatsView;

pub mod backend;
pub mod format;
pub mod spark;
mod storage;

pub use backend::CoreBackend;

/// What the panel needs from the session, and all it may touch.
pub trait PanelBackend {
    /// Everything to draw, as of now.
    fn view(&self) -> StatsView;
    /// The panel opened (`true`) or closed (`false`): start or stop the
    /// server's samples.
    fn set_subscribed(&self, on: bool);
}
```

- [ ] **Step 2: Run and see it fail.**

```bash
cd ~/work/gf-stats-client
cargo test -p ghostframe-web-ui --lib storage 2>&1 | tail -8; echo "rc=${PIPESTATUS[0]}"
```

Expected: `unresolved import super::parse_open` and `file not found for module backend`.

- [ ] **Step 3: Implement.** Replace `storage.rs` with:

```rust
//! The panel's open state, remembered in `localStorage`. Every failure (no
//! storage, a private window, a quota error, a thrown accessor) means "start
//! closed" and is otherwise ignored: the panel must work without it.

const KEY: &str = "ghostframe.statsPanel.open";

/// `"1"` is open; anything else, or nothing, is closed.
pub(crate) fn parse_open(stored: Option<&str>) -> bool {
    stored == Some("1")
}

fn storage() -> Option<web_sys::Storage> {
    // `local_storage()` is `Err` when access is denied and `Ok(None)` when
    // there is none; both are "no storage".
    web_sys::window()?.local_storage().ok()?
}

// Called from the components in Task 11; the attribute is removed there so
// this task's commit is warning-free under `-D warnings`.
#[allow(dead_code)]
pub(crate) fn load_open() -> bool {
    let stored = storage().and_then(|s| s.get_item(KEY).ok()).flatten();
    parse_open(stored.as_deref())
}

// Same as `load_open`: called from Task 11, attribute removed there.
#[allow(dead_code)]
pub(crate) fn save_open(open: bool) {
    if let Some(s) = storage() {
        let _ = s.set_item(KEY, if open { "1" } else { "0" });
    }
}

#[cfg(test)]
mod tests {
    use super::parse_open;

    #[test]
    fn only_an_explicit_one_is_open() {
        assert!(parse_open(Some("1")));
        assert!(!parse_open(Some("0")));
        assert!(!parse_open(Some("true")));
        assert!(!parse_open(Some("")));
        assert!(!parse_open(None));
    }
}
```
Create `ghostframe-web-ui/src/backend.rs`:

```rust
//! The panel's window onto the core.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ghostframe_client_core::{ClientCore, StatsView};

use crate::PanelBackend;

/// Microseconds on the page's `performance.now()` clock: the clock `main.ts`
/// gives every `now_us` it passes the core, so staleness compares like with
/// like.
fn now_us() -> u64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| (p.now() * 1000.0) as u64)
        .unwrap_or(0)
}

/// A [`PanelBackend`] over the core the wasm module shares with the panel.
///
/// The core sits in an `Rc<RefCell<..>>` because both the JS-facing
/// `WasmClientCore` and the panel's timer and click handlers own it. Those
/// callbacks run as separate browser tasks, never inside a `WasmClientCore`
/// call (no export calls back into JS), so a borrow here cannot overlap one
/// there. The `try_` forms exist anyway: a failed borrow must cost one
/// refresh, not trap the module.
pub struct CoreBackend {
    core: Rc<RefCell<ClientCore>>,
    /// The last view that was read, served if a borrow fails.
    last: RefCell<StatsView>,
    /// Subscribe requests that could not be queued; see `set_subscribed`.
    dropped: Cell<u32>,
}

impl CoreBackend {
    pub fn new(core: Rc<RefCell<ClientCore>>) -> Self {
        CoreBackend {
            core,
            last: RefCell::new(StatsView::default()),
            dropped: Cell::new(0),
        }
    }

    /// Subscribe requests lost to a busy core. Zero in practice.
    pub fn dropped_requests(&self) -> u32 {
        self.dropped.get()
    }
}

impl PanelBackend for CoreBackend {
    fn view(&self) -> StatsView {
        // Read the clock first: `performance.now()` is a JS call, and the
        // borrow below must not span one.
        let now = now_us();
        if let Ok(core) = self.core.try_borrow() {
            let view = core.stats().view(now);
            *self.last.borrow_mut() = view.clone();
            return view;
        }
        self.last.borrow().clone()
    }

    fn set_subscribed(&self, on: bool) {
        match self.core.try_borrow_mut() {
            Ok(mut core) => core.set_stats_subscribed(on),
            Err(_) => {
                self.dropped.set(self.dropped.get().saturating_add(1));
                web_sys::console::warn_1(
                    &"stats panel: core busy, subscribe request dropped".into(),
                );
            }
        }
    }
}
```
- [ ] **Step 4: Run and pass; check the wasm target compiles too.**

```bash
cd ~/work/gf-stats-client
cargo fmt -p ghostframe-web-ui
cargo test -p ghostframe-web-ui --lib 2>&1 | tail -6; echo "rc=${PIPESTATUS[0]}"
cargo check -p ghostframe-web-ui --target wasm32-unknown-unknown 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
```

Expected: 17 host tests pass (10 format, 6 spark, 1 storage); the wasm check finishes without warnings (`load_open` and `save_open` carry a temporary `#[allow(dead_code)]` until Task 11 calls them; rustc would otherwise warn on both targets).

- [ ] **Step 5: Commit, then break it.**

```bash
git add ghostframe-web-ui/src/storage.rs ghostframe-web-ui/src/backend.rs ghostframe-web-ui/src/lib.rs
git commit -m "feat(web-ui): PanelBackend, CoreBackend over the shared core, localStorage open state" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

Mutation: in `parse_open` change `stored == Some("1")` to `stored.is_some()`; `only_an_explicit_one_is_open` must go red. Restore with `git checkout -- ghostframe-web-ui/src/storage.rs`. (`CoreBackend` is covered by the browser test in Task 11: its mutation checks are there.)

## Task 10: The browser test, written first, and the `test-web-ui` recipe

**Model:** sonnet

**Files:**
- Create: `ghostframe-web-ui/tests/panel_mount.rs`
- Modify: `Justfile`

The test mounts the real panel over a real `ClientCore` (through the production `CoreBackend`) in headless Chrome and checks, in the DOM: closed shows only the tab; a click opens it and the core's outbox gets `[0x0B, 1]`; a second click closes it and the outbox gets `[0x0B, 0]`; `localStorage` follows; `mousedown` on the tab is cancelled; a panel that starts open subscribes at once and renders server rows (label, formatted value, a sparkline) and carries the raw `data-testid`/`data-label`/`data-current`/`data-stale` hooks PR 3's e2e reads, asserted by value for a numeric row (`18.4`, `false`) and a text row (empty `data-current`; only the not-stale side of `data-stale` is asserted in the browser, since making a row stale takes 3 real seconds, and the stale flag itself is tested in client-core); differing versions raise the alert with the hard-reload hint.

- [ ] **Step 1: Create `ghostframe-web-ui/tests/panel_mount.rs`:**

```rust
//! The panel in a real browser: mounted, toggled, and wired to a real core.
//! Run with `just test-web-ui` (headless Chrome through wasm-pack).
#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::rc::Rc;

use ghostframe_client_core::{ClientConfig, ClientCore, PollOutput};
use ghostframe_protocol::stats::{
    MetricDescriptor, MetricGroup, MetricKind, MetricValue, StatsSample, StatsSchema,
};
use ghostframe_web_ui::{mount_stats_panel, CoreBackend};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

const STORAGE_KEY: &str = "ghostframe.statsPanel.open";

fn document() -> web_sys::Document {
    web_sys::window().unwrap().document().unwrap()
}

fn clear_storage() {
    let s = web_sys::window().unwrap().local_storage().unwrap().unwrap();
    s.remove_item(STORAGE_KEY).unwrap();
}

fn stored() -> Option<String> {
    let s = web_sys::window().unwrap().local_storage().unwrap().unwrap();
    s.get_item(STORAGE_KEY).unwrap()
}

/// The page's `performance.now()` clock in microseconds, as `main.ts` gives it
/// to the core.
fn now_us() -> u64 {
    (web_sys::window().unwrap().performance().unwrap().now() * 1000.0) as u64
}

fn new_container() -> web_sys::HtmlElement {
    let el: web_sys::HtmlElement = document().create_element("div").unwrap().unchecked_into();
    document().body().unwrap().append_child(&el).unwrap();
    el
}

fn new_core() -> Rc<RefCell<ClientCore>> {
    let mut core = ClientCore::new(ClientConfig::default(), now_us());
    while core.poll_transmit(0).is_some() {} // the Hello
    Rc::new(RefCell::new(core))
}

fn mount(core: &Rc<RefCell<ClientCore>>) -> web_sys::HtmlElement {
    let container = new_container();
    let backend = Rc::new(CoreBackend::new(core.clone()));
    mount_stats_panel(container.clone(), backend).expect("mounts");
    container
}

fn query(container: &web_sys::HtmlElement, selector: &str) -> Option<web_sys::Element> {
    container.query_selector(selector).unwrap()
}

fn tab(container: &web_sys::HtmlElement) -> web_sys::HtmlElement {
    query(container, ".gf-tab")
        .expect("the tab is always there")
        .unchecked_into()
}

/// Let Leptos run the effects a click queued.
async fn settle() {
    leptos::task::tick().await;
    leptos::task::tick().await;
}

fn next_stream(core: &Rc<RefCell<ClientCore>>) -> Option<Vec<u8>> {
    match core.borrow_mut().poll_transmit(0) {
        Some(PollOutput::Stream(b)) => Some(b),
        Some(PollOutput::Datagram(_)) | None => None,
    }
}

fn feed_server_stats(core: &Rc<RefCell<ClientCore>>, server_version: &str) {
    let schema = StatsSchema {
        schema_id: 1,
        server_version: server_version.to_string(),
        host_name: "evangeline".to_string(),
        metrics: vec![
            MetricDescriptor {
                id: 1,
                name: "ghostframe.rtt".to_string(),
                unit: "ms".to_string(),
                kind: MetricKind::Gauge,
                group: MetricGroup::Connection,
                label: "Round trip".to_string(),
            },
            MetricDescriptor {
                id: 2,
                name: "ghostframe.mode".to_string(),
                unit: String::new(),
                kind: MetricKind::Text,
                group: MetricGroup::Connection,
                label: "Mode".to_string(),
            },
        ],
    };
    let sample = StatsSample {
        schema_id: 1,
        timestamp_us: 1_000_000,
        values: vec![MetricValue::Num(18.4), MetricValue::Text("h264".to_string())],
    };
    let mut bytes = schema.encode_frame();
    bytes.extend(sample.encode_frame());
    core.borrow_mut().handle_server_stream(&bytes, now_us());
}

#[wasm_bindgen_test]
async fn the_tab_opens_and_closes_the_panel_and_the_core_is_told() {
    clear_storage();
    let core = new_core();
    let container = mount(&core);
    assert_eq!(tab(&container).text_content().as_deref(), Some("<"));
    assert!(
        query(&container, ".gf-body").is_none(),
        "closed: only the tab"
    );
    assert_eq!(next_stream(&core), None, "closed at mount: nothing to send");

    tab(&container).click();
    settle().await;
    assert_eq!(tab(&container).text_content().as_deref(), Some(">"));
    assert!(query(&container, ".gf-body").is_some());
    assert_eq!(
        next_stream(&core),
        Some(vec![0x0B, 1]),
        "subscribe reached the core"
    );
    assert_eq!(stored().as_deref(), Some("1"));

    tab(&container).click();
    settle().await;
    assert_eq!(tab(&container).text_content().as_deref(), Some("<"));
    assert!(query(&container, ".gf-body").is_none());
    assert_eq!(
        next_stream(&core),
        Some(vec![0x0B, 0]),
        "unsubscribe reached the core"
    );
    assert_eq!(stored().as_deref(), Some("0"));
}

#[wasm_bindgen_test]
fn a_mousedown_on_the_tab_is_cancelled_so_it_cannot_take_focus() {
    clear_storage();
    let container = mount(&new_core());
    let init = web_sys::MouseEventInit::new();
    init.set_bubbles(true);
    init.set_cancelable(true);
    let ev = web_sys::MouseEvent::new_with_mouse_event_init_dict("mousedown", &init).unwrap();
    tab(&container).dispatch_event(&ev).unwrap();
    assert!(ev.default_prevented());
}

#[wasm_bindgen_test]
async fn a_panel_that_starts_open_subscribes_at_once_and_shows_server_rows() {
    clear_storage();
    web_sys::window()
        .unwrap()
        .local_storage()
        .unwrap()
        .unwrap()
        .set_item(STORAGE_KEY, "1")
        .unwrap();
    let core = new_core();
    core.borrow_mut().set_client_version("0.1.0 (aab2fa2d)");
    feed_server_stats(&core, "0.1.0 (aab2fa2d)");
    let container = mount(&core);
    settle().await;
    assert_eq!(next_stream(&core), Some(vec![0x0B, 1]));
    let text = container.text_content().unwrap();
    assert!(text.contains("Round trip"), "{text}");
    assert!(text.contains("18.4 ms"), "{text}");
    assert!(text.contains("Connection"), "{text}");
    assert!(text.contains("evangeline"), "{text}");
    assert!(query(&container, ".gf-alert").is_none(), "versions match");
    assert!(
        query(&container, ".gf-spark polyline").is_some(),
        "a sparkline is drawn"
    );

    // The raw-state hooks PR 3's e2e reads. They are asserted here, in the PR
    // that ships them: PR 3 adds only the hooks this PR lacks.
    assert!(
        query(&container, "[data-testid=\"stats-tab\"]").is_some(),
        "the tab carries its e2e hook"
    );
    let rtt = query(
        &container,
        "[data-testid=\"stats-row\"][data-label=\"Round trip\"]",
    )
    .expect("the Round trip row carries data-testid and data-label");
    assert_eq!(
        rtt.get_attribute("data-current").as_deref(),
        Some("18.4"),
        "the raw, unformatted number"
    );
    assert_eq!(rtt.get_attribute("data-stale").as_deref(), Some("false"));
    let mode = query(
        &container,
        "[data-testid=\"stats-row\"][data-label=\"Mode\"]",
    )
    .expect("the Mode row");
    assert_eq!(
        mode.get_attribute("data-current").as_deref(),
        Some(""),
        "a text row has no number: the attribute is there and empty"
    );
    assert!(mode.text_content().unwrap_or_default().contains("h264"));
}

#[wasm_bindgen_test]
async fn differing_versions_raise_the_alert_with_the_hard_reload_hint() {
    clear_storage();
    web_sys::window()
        .unwrap()
        .local_storage()
        .unwrap()
        .unwrap()
        .set_item(STORAGE_KEY, "1")
        .unwrap();
    let core = new_core();
    core.borrow_mut().set_client_version("0.1.0 (aaaaaaaa)");
    feed_server_stats(&core, "0.1.0 (bbbbbbbb)");
    let container = mount(&core);
    settle().await;
    assert!(query(&container, ".gf-alert").is_some());
    assert!(container.text_content().unwrap().contains("hard-reload"));
}
```
- [ ] **Step 2: Add the recipe.** In `Justfile`, after the `ci-client` recipe's block (before `# Run from a clean checkout: builds the web client SPA`), add:

```make
# The stats panel's host tests and its headless-Chrome test (wasm-pack drives
# chromedriver; needs Chrome or Chromium installed).
test-web-ui:
    cargo test -p ghostframe-web-ui
    wasm-pack test --headless --chrome ghostframe-web-ui
```

- [ ] **Step 3: Run it and see it fail.**

```bash
cd ~/work/gf-stats-client
just test-web-ui 2>&1 | tail -15; echo "rc=${PIPESTATUS[0]}"
```

Expected: the host tests pass, then `wasm-pack test` fails to compile `panel_mount`: `unresolved import ghostframe_web_ui::mount_stats_panel`. (This is the red state: Task 11 adds it. If chromedriver is missing the failure is a different one; install Chromium + chromedriver first.)

- [ ] **Step 4: Commit the failing test with the recipe** (Task 11 turns it green in the next commit):

```bash
git add ghostframe-web-ui/tests/panel_mount.rs Justfile
git commit -m "test(web-ui): headless-Chrome test for the stats panel (red until the components land)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

## Task 11: The Leptos components, the CSS, and `mount_stats_panel`

**Model:** sonnet

**Files:**
- Create: `ghostframe-web-ui/src/components.rs`, `ghostframe-web-ui/src/style.css`
- Modify: `ghostframe-web-ui/src/lib.rs`, `ghostframe-web-ui/src/storage.rs` (drop the temporary `#[allow(dead_code)]`)

Components (spec 2.1): `StatsPanel` (root: open signal, the `<`/`>` tab, a 1 Hz refresh only while open), `Group` (keyed `<For>` of rows by id: a metric the server adds appears without client code; unknown groups land in the trailing "Other" group because `ClientStats::view` puts them there), `MetricRow` (looks its row up by id on each refresh through a `Memo`, so the keyed list keeps the DOM node while numbers move), `Sparkline` (inline SVG polyline), `VersionRow` (alert colour and the hard-reload hint when `versions_match` is false). `Tab` is the `<button class="gf-tab">` inside `StatsPanel`: a separate component would only pass three closures through. Behaviours:

- The tab cancels `mousedown` (spec 2.3) and has `tabindex="-1"`, so keyboard focus stays on the canvas.
- Open state is `localStorage` (`ghostframe.statsPanel.open`); a panel that loads open subscribes immediately.
- While closed nothing refreshes and the body is not in the DOM; the core keeps sampling local metrics regardless.
- `data-testid="stats-tab"` and `data-testid="stats-row"` (`data-label`, `data-current` the raw unformatted number or empty, `data-stale` `"true"`/`"false"`) are the hooks `docs/superpowers/plans/2026-11-12-stats-panel-3-e2e-guard.md` expects; adding them here makes that plan's hook step a no-op.
- Layout (spec 2.2): `position: fixed; top: 0; right: 0; height: 100vh`, 300 px body plus a 20 px tab; closed, only the tab; the canvas is never moved. `pointer-events: none` on the panel container and `auto` on the tab and body, so the transparent flex box never swallows clicks meant for the canvas.

- [ ] **Step 1: Create `ghostframe-web-ui/src/style.css`:**

```css
.gf-panel { position: fixed; top: 0; right: 0; height: 100vh; z-index: 1000; display: flex; align-items: center; pointer-events: none; font: 12px/1.4 monospace; color: #eee; }
.gf-tab { pointer-events: auto; width: 20px; height: 56px; padding: 0; border: 1px solid #444; border-right: 0; border-radius: 6px 0 0 6px; background: rgba(24, 24, 24, 0.92); color: #eee; font: inherit; font-size: 14px; cursor: pointer; }
.gf-tab:hover { background: #333; }
.gf-body { pointer-events: auto; box-sizing: border-box; width: 300px; height: 100%; overflow-y: auto; padding: 10px 12px; background: rgba(17, 17, 17, 0.92); border-left: 1px solid #444; }
.gf-body h3 { margin: 12px 0 4px; font-size: 11px; font-weight: normal; letter-spacing: 0.08em; text-transform: uppercase; color: #8a8a8a; }
.gf-versions { margin-bottom: 6px; padding-bottom: 6px; border-bottom: 1px solid #333; }
.gf-versions.gf-alert { color: #ffb86b; }
.gf-hint { margin-top: 4px; }
.gf-row { display: grid; grid-template-columns: 1fr auto; column-gap: 8px; align-items: center; padding: 2px 0; }
.gf-label { color: #bbb; }
.gf-value { text-align: right; font-variant-numeric: tabular-nums; }
.gf-spark { grid-column: 1 / -1; width: 100%; height: 24px; color: #6cb6ff; }
.gf-row.gf-local .gf-spark { color: #7ee787; }
.gf-row.gf-stale .gf-label, .gf-row.gf-stale .gf-value, .gf-row.gf-stale .gf-spark { color: #666; }
.gf-rejected { margin-top: 10px; color: #8a8a8a; }
```
- [ ] **Step 2: Create `ghostframe-web-ui/src/components.rs`:**

```rust
//! The panel's Leptos components. Each owns one piece of the DOM and reads
//! the one shared `StatsView` signal; none of them touches the transport.

use std::rc::Rc;

use ghostframe_client_core::{MetricKind, StatsView};
use leptos::prelude::*;

use crate::format::{format_row, DASH};
use crate::spark::{points_attr, sparkline_points, HEIGHT, WIDTH};
use crate::{storage, PanelBackend};

const CSS: &str = include_str!("style.css");

/// How often the panel re-reads the core while open.
const REFRESH_MS: i32 = 1000;

/// A one-line inline SVG polyline over a metric's history.
#[component]
pub fn Sparkline(history: Signal<Vec<f64>>) -> impl IntoView {
    let points = move || points_attr(&sparkline_points(&history.get(), WIDTH, HEIGHT));
    view! {
        <svg class="gf-spark" viewBox=format!("0 0 {WIDTH} {HEIGHT}") preserveAspectRatio="none">
            <polyline fill="none" stroke="currentColor" stroke-width="1.5" points=points />
        </svg>
    }
}

/// One metric: label, formatted value, and (except for text) a sparkline.
/// The `data-*` attributes carry the raw state for the e2e suite: the label,
/// the unformatted number (empty when there is none) and whether the row is
/// stale. It looks its row up by `id` on every refresh, so the keyed list above it
/// keeps this component alive while the numbers move.
#[component]
pub fn MetricRow(id: String, view: RwSignal<StatsView>) -> impl IntoView {
    let row = Memo::new(move |_| view.with(|v| v.row(&id).cloned()));
    let history = Signal::derive(move || row.get().map(|r| r.history).unwrap_or_default());
    let has_chart = move || {
        row.get()
            .is_some_and(|r| !matches!(r.kind, MetricKind::Text))
    };
    view! {
        <div
            class="gf-row"
            data-testid="stats-row"
            data-label=move || row.get().map(|r| r.label).unwrap_or_default()
            data-current=move || {
                row.get().and_then(|r| r.current).map(|v| v.to_string()).unwrap_or_default()
            }
            data-stale=move || row.get().is_some_and(|r| r.stale).to_string()
            class:gf-stale=move || row.get().is_some_and(|r| r.stale)
            class:gf-local=move || row.get().is_some_and(|r| r.local)
        >
            <span class="gf-label">{move || row.get().map(|r| r.label).unwrap_or_default()}</span>
            <span class="gf-value">
                {move || row.get().map(|r| format_row(&r)).unwrap_or_else(|| DASH.to_string())}
            </span>
            <Show when=has_chart>
                <Sparkline history=history />
            </Show>
        </div>
    }
}

/// One titled section. Its rows are keyed by id, so a metric the server adds
/// appears here without client code.
#[component]
pub fn Group(key: u8, title: String, view: RwSignal<StatsView>) -> impl IntoView {
    let ids = Memo::new(move |_| {
        view.with(|v| {
            v.group(key)
                .map(|g| g.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    view! {
        <section>
            <h3>{title}</h3>
            <For
                each=move || ids.get()
                key=|id| id.clone()
                children=move |id| view! { <MetricRow id=id view=view /> }
            />
        </section>
    }
}

/// Client and server versions; an alert and the way out when they differ.
#[component]
pub fn VersionRow(view: RwSignal<StatsView>) -> impl IntoView {
    let differ = move || !view.with(|v| v.versions_match);
    view! {
        <div class="gf-versions" class:gf-alert=differ>
            <div>"Client " {move || view.with(|v| v.client_version.clone())}</div>
            <div>
                "Server "
                {move || view.with(|v| v.server_version.clone().unwrap_or_else(|| DASH.to_string()))}
                {move || view.with(|v| v.server_host.clone().map(|h| format!(" on {h}")))}
            </div>
            <Show when=differ>
                <div class="gf-hint">"versions differ \u{2014} hard-reload (Ctrl+Shift+R)"</div>
            </Show>
        </div>
    }
}

/// The root: the edge tab, and the body while open.
///
/// Opening asks the core to subscribe (it queues the message; the page's
/// transmit loop sends it) and closing to unsubscribe. A panel that starts
/// open, from `localStorage`, subscribes at once.
#[component]
pub fn StatsPanel(backend: Rc<dyn PanelBackend>) -> impl IntoView {
    // Rc is not Send; the panel runs on the one browser thread.
    let backend = StoredValue::new_local(backend);
    let open = RwSignal::new(storage::load_open());
    let stats = RwSignal::new(StatsView::default());
    let refresh = move || backend.with_value(|b| stats.set(b.view()));

    if open.get_untracked() {
        backend.with_value(|b| b.set_subscribed(true));
        refresh();
    }

    // One refresh a second while open. Closed, the core's own sampling goes
    // on and nothing here runs.
    let timer = set_interval(
        move || {
            if open.get_untracked() {
                refresh();
            }
        },
        std::time::Duration::from_millis(REFRESH_MS as u64),
    )
    .ok();
    on_cleanup(move || {
        if let Some(t) = timer {
            t.clear();
        }
    });

    let toggle = move |_| {
        let next = !open.get_untracked();
        open.set(next);
        storage::save_open(next);
        backend.with_value(|b| b.set_subscribed(next));
        if next {
            refresh();
        }
    };

    // The groups present, in display order. A memo, so the keyed list below
    // is rebuilt only when a group appears or goes, not every second.
    let groups = Memo::new(move |_| {
        stats.with(|v| {
            v.groups
                .iter()
                .map(|g| (g.key, g.title.to_string()))
                .collect::<Vec<_>>()
        })
    });

    view! {
        <style>{CSS}</style>
        <div class="gf-panel" class:gf-open=move || open.get()>
            <button
                class="gf-tab"
                data-testid="stats-tab"
                tabindex="-1"
                aria-label="Connection statistics"
                // Cancelling mousedown stops the button taking keyboard focus,
                // so keys keep going to the remote session.
                on:mousedown=move |ev| ev.prevent_default()
                on:click=toggle
            >
                {move || if open.get() { ">" } else { "<" }}
            </button>
            <Show when=move || open.get()>
                <div class="gf-body">
                    <VersionRow view=stats />
                    <For
                        each=move || groups.get()
                        key=|g| g.0
                        children=move |(key, title)| view! { <Group key=key title=title view=stats /> }
                    />
                    <Show when=move || { stats.with(|v| v.samples_rejected > 0) }>
                        <div class="gf-rejected">
                            "Rejected stats frames: " {move || stats.with(|v| v.samples_rejected)}
                        </div>
                    </Show>
                </div>
            </Show>
        </div>
    }
}
```
- [ ] **Step 2b: Drop the temporary attributes.** In `ghostframe-web-ui/src/storage.rs` delete the two `#[allow(dead_code)]` lines on `load_open` and `save_open`, with the comment lines above each (Task 9 added them; the components call both now). Add `ghostframe-web-ui/src/storage.rs` to this task's `git add`.

- [ ] **Step 3: Replace `ghostframe-web-ui/src/lib.rs`** with the final version (adds `components`, the Leptos prelude and `mount_stats_panel`):

```rust
//! The browser client's DOM UI. Leptos lives here and nowhere else in the
//! workspace: swapping the framework means rewriting this crate.
//!
//! The video path (WebTransport, WebCodecs, WebGPU, input) stays
//! framework-free. This crate draws the stats panel beside it and talks to
//! the core only through [`PanelBackend`].

use std::rc::Rc;

use ghostframe_client_core::StatsView;
use leptos::prelude::*;
use wasm_bindgen::JsValue;

pub mod backend;
pub mod components;
pub mod format;
pub mod spark;
mod storage;

pub use backend::CoreBackend;

/// What the panel needs from the session, and all it may touch.
pub trait PanelBackend {
    /// Everything to draw, as of now.
    fn view(&self) -> StatsView;
    /// The panel opened (`true`) or closed (`false`): start or stop the
    /// server's samples.
    fn set_subscribed(&self, on: bool);
}

/// Mount the stats panel into `container`. The panel lives until the page
/// does.
///
/// An error here must never break the stream: the caller logs it and carries
/// on without a panel.
pub fn mount_stats_panel(
    container: web_sys::HtmlElement,
    backend: Rc<dyn PanelBackend>,
) -> Result<(), JsValue> {
    let handle = leptos::mount::mount_to(container, move || {
        view! { <components::StatsPanel backend=backend /> }
    });
    handle.forget();
    Ok(())
}
```
- [ ] **Step 4: Run the browser test and pass.**

```bash
cd ~/work/gf-stats-client
cargo fmt -p ghostframe-web-ui
just test-web-ui 2>&1 | grep -E "^test |test result|error|panicked"; echo "rc=${PIPESTATUS[0]}"
```

Expected: 17 host tests pass; the browser run lists `4 passed`: `the_tab_opens_and_closes_the_panel_and_the_core_is_told`, `a_mousedown_on_the_tab_is_cancelled_so_it_cannot_take_focus`, `a_panel_that_starts_open_subscribes_at_once_and_shows_server_rows`, `differing_versions_raise_the_alert_with_the_hard_reload_hint`.

- [ ] **Step 5: Gate the crate.**

```bash
cd ~/work/gf-stats-client
cargo clippy -p ghostframe-web-ui --all-targets -- -D warnings 2>&1 | tail -5; echo "rc=${PIPESTATUS[0]}"
cargo clippy -p ghostframe-web-ui --target wasm32-unknown-unknown --all-targets -- -D warnings 2>&1 | tail -5; echo "rc=${PIPESTATUS[0]}"
cargo fmt --check -p ghostframe-web-ui && echo FMT_OK
```

Expected: no warnings, `FMT_OK`.

- [ ] **Step 6: Commit.**

```bash
git add ghostframe-web-ui/src/components.rs ghostframe-web-ui/src/style.css ghostframe-web-ui/src/lib.rs ghostframe-web-ui/src/storage.rs
git commit -m "feat(web-ui): the stats panel (tab, groups, metric rows, sparklines, version row)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

- [ ] **Step 7: Break the code and watch the browser test fail (restore with `git checkout -- <file>`).** Each run is `just test-web-ui 2>&1 | grep -E "^test |panicked"`:

| Mutation in `components.rs` | Test that must go red |
|---|---|
| `on:mousedown=move \|ev\| ev.prevent_default()` -> `on:mousedown=move \|_ev\| ()` | `a_mousedown_on_the_tab_is_cancelled_so_it_cannot_take_focus` |
| in `toggle`, delete `backend.with_value(\|b\| b.set_subscribed(next));` | `the_tab_opens_and_closes_the_panel_and_the_core_is_told` (`subscribe reached the core`) |
| in `StatsPanel`, delete the `if open.get_untracked() { backend...set_subscribed(true); refresh(); }` block | `a_panel_that_starts_open_subscribes_at_once_and_shows_server_rows` |
| `VersionRow`: `let differ = move \|\| !view.with(\|v\| v.versions_match);` -> `move \|\| false` | `differing_versions_raise_the_alert_with_the_hard_reload_hint` |
| `MetricRow`: delete the `data-current=...` attribute | `a_panel_that_starts_open_subscribes_at_once_and_shows_server_rows` (`rtt.get_attribute("data-current")` is `None`, not `18.4`) |
| `MetricRow`: `data-current` falls back to `"0"` instead of empty (`.unwrap_or_default()` -> `.unwrap_or_else(\|\| "0".to_string())`) | the same test (the Mode row's empty `data-current`) |
| `StatsPanel`: delete `data-testid="stats-tab"` | the same test (`the tab carries its e2e hook`) |
| `MetricRow`: delete `data-testid="stats-row"` or `data-label=...` | the same test (the row selectors find nothing) |
| `storage.rs` `save_open`: make it a no-op | `the_tab_opens_and_closes_the_panel_and_the_core_is_told` (`stored()` assertions) |

Record in the commit log or the PR description that the first two and the `data-current` row were observed red. Finish with `just test-web-ui` green and a clean `git status`.

## Task 12: Justfile gate and CI jobs for the panel

**Model:** haiku

**Files:**
- Modify: `Justfile` (`ci-local`)
- Modify: `.github/workflows/client-core.yml`

`just ci-local` is the local mirror of CI (a local gate must match CI), so the panel test goes in both. The `client-core` workflow already runs `cargo test -p ghostframe-protocol -p ghostframe-client-core` package-wide, so `tests/stats_core.rs` needs no explicit `--test` line; `wasm-pack test` is a different runner and needs its own job.

- [ ] **Step 1: `ci-local`.** In `Justfile`, after the three `cargo test ...` lines under `@echo "=== unit tests ==="` in `ci-local`, add:

```make
    @echo "=== stats panel (host tests + headless Chrome) ==="
    just test-web-ui
```

- [ ] **Step 2: Workflow.** In `.github/workflows/client-core.yml`, in the `test` job change the two `run` lines to:

```yaml
      - run: cargo test -p ghostframe-protocol -p ghostframe-client-core -p ghostframe-web-ui
      - run: cargo build -p ghostframe-protocol -p ghostframe-client-core -p ghostframe-client-net -p ghostframe-client-wasm -p ghostframe-web-ui --target wasm32-unknown-unknown
```

and append this job at the end of the file (same indentation as `wasm-module:`; it reuses that job's cached `wasm-pack` install, and ubuntu-latest ships Chrome and chromedriver):

```yaml

  web-ui:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: wasm32-unknown-unknown }
      - name: Cache wasm-pack
        id: cache-wasm-pack
        uses: actions/cache@v6
        with:
          path: ~/.cargo/bin/wasm-pack
          key: wasm-pack-0.15.0-${{ runner.os }}
      - name: Install wasm-pack
        if: steps.cache-wasm-pack.outputs.cache-hit != 'true'
        run: cargo install wasm-pack --version 0.15.0 --locked
      # Mounts the real panel over a real ClientCore in headless Chrome.
      - name: Stats panel browser test
        run: wasm-pack test --headless --chrome ghostframe-web-ui
```

- [ ] **Step 3: Validate.**

```bash
cd ~/work/gf-stats-client
python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/client-core.yml')); print('yaml ok')"
just --list | grep -E "test-web-ui|ci-local"
```

Expected: `yaml ok`; both recipes listed. (The job itself can only be proven on CI; if its first run fails on chromedriver discovery, add `chromedriver --version` as a diagnostic step before changing anything else.)

- [ ] **Step 4: Commit.**

```bash
git add Justfile .github/workflows/client-core.yml
git commit -m "ci: run the stats panel browser test in ci-local and the client-core workflow" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

## Task 13: Client version capture (`build.rs`, `CLIENT_VERSION`, `clientVersion()`)

**Model:** sonnet

**Files:**
- Modify: `ghostframe-client-wasm/build.rs`
- Modify: `ghostframe-client-wasm/src/lib.rs`

Spec 1.4 for the client half: `CARGO_PKG_VERSION` plus the short commit and a dirty flag, `unknown` outside git, no timestamp, rerun when the commit changes. Two details the spec does not cover: in a linked worktree `.git` is a file, so the rerun paths come from `git rev-parse --git-path`; a commit moves the branch ref, not `HEAD` (which says `ref: refs/heads/<branch>`), so `refs/heads` is watched too. Dirty means tracked files differ (`--untracked-files=no`): this checkout has four untracked files, and "dirty" on every local build would make the flag meaningless. The flag is best-effort and display-only (it can be stale, see the doc comment below), which is why PR 1's match rule, adopted in Task 3, ignores it. Every git call runs with `GIT_OPTIONAL_LOCKS=0`: `git status` would otherwise refresh `.git/index`, a file this script watches, and re-trigger itself.

- [ ] **Step 1: Write the failing test.** Append to `ghostframe-client-wasm/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::CLIENT_VERSION;

    #[test]
    fn client_version_has_the_documented_shape() {
        let (pkg, rest) = CLIENT_VERSION.split_once(" (").expect(CLIENT_VERSION);
        assert_eq!(pkg, env!("CARGO_PKG_VERSION"));
        let inner = rest.strip_suffix(')').expect(CLIENT_VERSION);
        let commit = inner.strip_suffix(", dirty").unwrap_or(inner);
        let hex8 = commit.len() == 8 && commit.chars().all(|c| c.is_ascii_hexdigit());
        assert!(commit == "unknown" || hex8, "{CLIENT_VERSION}");
    }
}
```

- [ ] **Step 2: Run and see it fail.**

```bash
cd ~/work/gf-stats-client
cargo test -p ghostframe-client-wasm --lib client_version 2>&1 | tail -8; echo "rc=${PIPESTATUS[0]}"
```

Expected: `cannot find value CLIENT_VERSION in the super module`.

- [ ] **Step 3: Implement.** In `ghostframe-client-wasm/src/lib.rs`, after `PROTOCOL_STAMP`, add:

```rust
/// This build's version for the stats panel: `0.1.0 (aab2fa2d)`, with
/// `, dirty` inside the parentheses for a tree with uncommitted changes and
/// `unknown` for the commit outside a git checkout. See `build.rs`.
pub const CLIENT_VERSION: &str = env!("GHOSTFRAME_CLIENT_VERSION");
```

and after `protocol_stamp()`:

```rust
/// `CLIENT_VERSION` for JS: the e2e suite and the console read it.
#[wasm_bindgen(js_name = clientVersion)]
pub fn client_version() -> String {
    CLIENT_VERSION.to_string()
}
```

Replace `ghostframe-client-wasm/build.rs` with (the stamp code is unchanged; the version code and its `main` additions are new):

```rust
//! Emits `GHOSTFRAME_PROTOCOL_STAMP`: an FNV-1a hash over the sorted
//! relative paths and contents of the crates whose sources define protocol
//! behaviour. `ghostframe-web-client/scripts/protocol_stamp.mjs` recomputes
//! the identical value in JS; a smoke test asserts they match, so a wasm
//! built from stale sources fails instead of silently serving yesterday's
//! protocol.
//!
//! Paths are hashed relative to the workspace root so the stamp does not
//! depend on the build directory.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Directories that define protocol behaviour, relative to the workspace root.
const STAMPED_DIRS: &[&str] = &["ghostframe-client-core/src", "ghostframe-protocol/src"];

/// FNV-1a 64-bit prime, 2^40 + 2^8 + 0xb3. Grouped in fours from the right
/// so a wrong digit count is visible: an extra zero here still compiles and
/// still produces a stable-looking hash, but one that diverges from the JS
/// twin only in the high bits.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: &[u8], mut hash: u64) -> u64 {
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("stamp: cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("stamp: bad dir entry").path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Run `git` with `args` in the workspace root; `None` outside a checkout, or
/// if git is missing or fails.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        // `git status` refreshes stat data in `.git/index` when it may take
        // the lock. This script watches that file, so a refresh would make
        // the build re-run itself. Never write.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(root)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `0.1.0 (aab2fa2d)`, `0.1.0 (aab2fa2d, dirty)` or `0.1.0 (unknown)`.
///
/// Untracked files do not make a tree dirty: nothing here compiles them, and
/// a developer checkout always has some. No timestamp, so builds stay
/// reproducible.
///
/// `dirty` is **best-effort and display-only**. It is evaluated only when
/// this script reruns (HEAD, the index, the branch refs, or the protocol and
/// client-core sources move), so an edit to `ghostframe-client-wasm/src` or
/// `ghostframe-web-ui` leaves it stale until the next such change. The
/// panel's version match ignores it (`ClientStats`'s `versions_match`
/// compares the crate version and the commit only), so a stale flag never
/// raises the alert.
fn client_version(root: &Path) -> String {
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let Some(commit) = git(root, &["rev-parse", "--short=8", "HEAD"]).filter(|c| !c.is_empty())
    else {
        return format!("{pkg} (unknown)");
    };
    let dirty = git(root, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|s| !s.is_empty());
    if dirty {
        format!("{pkg} ({commit}, dirty)")
    } else {
        format!("{pkg} ({commit})")
    }
}

/// Rebuild when the commit or the index moves. `--git-path` resolves the
/// right place in a linked worktree, where `.git` is a file, not a directory.
fn rerun_on_git_changes(root: &Path) {
    for name in ["HEAD", "index"] {
        if let Some(p) = git(root, &["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={}", root.join(p).display());
        }
    }
    // A commit moves the branch ref, not HEAD (which names the branch).
    if let Some(p) = git(root, &["rev-parse", "--git-path", "refs/heads"]) {
        println!("cargo:rerun-if-changed={}", root.join(p).display());
    }
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .expect("stamp: crate has no parent dir")
        .to_path_buf();

    let mut files = Vec::new();
    for rel in STAMPED_DIRS {
        let dir = root.join(rel);
        println!("cargo:rerun-if-changed={}", dir.display());
        collect(&dir, &mut files);
    }
    // Deterministic order: the hash must not depend on readdir order.
    files.sort();

    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for path in &files {
        let rel = path
            .strip_prefix(&root)
            .expect("stamp: file outside workspace root");
        // Normalise separators so the stamp matches the JS side on any host.
        let rel = rel.to_string_lossy().replace('\\', "/");
        hash = fnv1a(rel.as_bytes(), hash);
        let body = std::fs::read(path)
            .unwrap_or_else(|e| panic!("stamp: cannot read {}: {e}", path.display()));
        hash = fnv1a(&body, hash);
        println!("cargo:rerun-if-changed={}", path.display());
    }

    println!("cargo:rustc-env=GHOSTFRAME_PROTOCOL_STAMP={hash:016x}");

    println!(
        "cargo:rustc-env=GHOSTFRAME_CLIENT_VERSION={}",
        client_version(&root)
    );
    rerun_on_git_changes(&root);
}
```
- [ ] **Step 4: Run and pass; check the string and the rerun behaviour for real.**

```bash
cd ~/work/gf-stats-client
cargo fmt -p ghostframe-client-wasm
cargo test -p ghostframe-client-wasm --lib 2>&1 | grep -E "client_version|test result"; echo "rc=${PIPESTATUS[0]}"
cargo build -p ghostframe-client-wasm -vv 2>&1 | grep -o 'GHOSTFRAME_CLIENT_VERSION=[^"]*' | head -1
cargo build -p ghostframe-client-wasm -v 2>&1 | grep -c "Fresh ghostframe-client-wasm"; echo "rc=${PIPESTATUS[0]}"
git commit --allow-empty -m "chore: version-capture probe (dropped below)"
cargo build -p ghostframe-client-wasm -v 2>&1 | grep -c "build-script-build"; echo "rc=${PIPESTATUS[0]}"
git reset --soft HEAD~1
```

Expected: tests pass; the first `grep -c` (after an unchanged second build) prints `1`: the crate was `Fresh`, so the build script did not trigger itself by touching the index it watches; the printed value looks like `0.1.0 (<8 hex>)` (this worktree has no tracked changes yet at the time of the print, or `, dirty` if it does); after the empty commit the second build re-runs the build script (`grep -c` prints at least `1`), proving the commit change is noticed from inside a linked worktree. `git reset --soft HEAD~1` removes the probe commit again.

- [ ] **Step 5: Commit, then break it.**

```bash
git add ghostframe-client-wasm/build.rs ghostframe-client-wasm/src/lib.rs
git commit -m "feat(client-wasm): capture the client version (crate version, commit, dirty flag)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

Mutation: in `client_version`, change `format!("{pkg} ({commit})")` to `format!("{pkg} [{commit}]")` (and the dirty arm likewise); `client_version_has_the_documented_shape` must go red. Restore with `git checkout -- ghostframe-client-wasm/build.rs`. Second mutation: make `git()` return `None` always: `CLIENT_VERSION` becomes `0.1.0 (unknown)` and the test must still pass (the documented fallback), which shows the test accepts `unknown` and not just a hash.

## Task 14: `WasmClientCore` shares the core; `mountStatsPanel`, `handleServerStream`, `recordPaints`

**Model:** sonnet

**Files:**
- Modify: `ghostframe-client-wasm/Cargo.toml`, `ghostframe-client-wasm/src/core.rs`, `ghostframe-client-wasm/src/lib.rs`
- Create: `ghostframe-client-wasm/src/panel.rs`
- Modify: `ghostframe-web-client/tests/wasm_smoke.test.ts`

**Re-entrancy audit (the spec's "no JS-called method can re-enter while a panel borrow is held"), recorded for the reviewer:**

| Borrower | Borrow | Held across |
|---|---|---|
| `CoreBackend::view` (timer, mount) | `try_borrow` | `ClientStats::view` only; `now_us()` (`performance.now()`, a JS import) is evaluated *before* the borrow, so the region contains no JS call; returns an owned `StatsView`; the Leptos signal is set after the borrow is gone |
| `CoreBackend::set_subscribed` (click, mount) | `try_borrow_mut` | `set_stats_subscribed` only (one `VecDeque::push_back`) |
| every `WasmClientCore` export | `borrow`/`borrow_mut` | the core call, plus `serde_wasm_bindgen` building a JS value; no export invokes a JS callback; `handle_datagrams` drops its `RefMut` before it builds the event array |

Wasm is single-threaded and the panel's callbacks are separate browser tasks, so two borrows can only overlap if a borrow region calls into JS code that calls back. None does. If a later change makes an export call JS (a callback parameter, say), this audit must be redone.

**Residual risk the audit does not remove: a panic.** The panel runs in the same wasm instance as the session. A Rust panic traps the module, so every later export traps (`ghostframe-client-wasm/src/lib.rs` says so), and it leaves any `RefCell` borrow flag set. `main.ts`'s try/catch (Task 15) contains an error *returned from mounting*; it cannot contain a panic raised later from a Leptos timer or click handler. Mitigation: panel code is kept panic-free (Step 6b greps for `unwrap`/`expect`; the slice in `sparkline_points` is in bounds by construction; every borrow is a `try_borrow*`; a failed borrow serves the last view). Accepted: a panic inside Leptos itself would still end the session. State this in the PR description.

- [ ] **Step 1: Write the failing JS tests.** In `ghostframe-web-client/tests/wasm_smoke.test.ts`, inside the existing `describe('wasm module', ...)` block, add:

```ts
  it('exports the stats-panel entry points', () => {
    expect(typeof wasm.mountStatsPanel).toBe('function');
    const core = new wasm.WasmClientCore(true, true, true, 0n);
    expect(typeof core.handleServerStream).toBe('function');
    expect(typeof core.recordPaints).toBe('function');
  });

  it('survives hostile server-stream bytes and stays usable', () => {
    const core = new wasm.WasmClientCore(true, true, true, 0n);
    core.handleServerStream(new Uint8Array([]), 0n);
    core.handleServerStream(new Uint8Array([0x02, 0xff, 0xff]), 0n); // oversized length
    core.handleServerStream(new Uint8Array([0x7f, 0x02, 0x00, 0xaa, 0xbb]), 0n); // unknown type
    core.handleServerStream(new Uint8Array([0x02, 0x01, 0x00, 0x00]), 0n); // sample, no schema
    core.recordPaints(4294967295);
    core.recordPaints(1); // saturates instead of wrapping or trapping
    // Still alive: the Hello queued at construction is still there.
    const out = core.pollTransmit(0n);
    expect(out).toBeDefined();
    expect(out.kind).toBe('Stream');
  });

  it('reports a client version of the documented shape', () => {
    expect(wasm.clientVersion()).toMatch(/^\d+\.\d+\.\d+ \((unknown|[0-9a-f]{8})(, dirty)?\)$/);
  });
```

- [ ] **Step 2: Build the node package and see them fail.**

```bash
cd ~/work/gf-stats-client/ghostframe-web-client
npm run build:wasm:node 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
npx vitest run tests/wasm_smoke.test.ts 2>&1 | tail -20; echo "rc=${PIPESTATUS[0]}"
```

Expected: the three new tests fail (`typeof wasm.mountStatsPanel` is `undefined`; `core.handleServerStream is not a function`). `wasm.clientVersion` exists from Task 13, so the third test may already pass: that is fine.

- [ ] **Step 3: Cargo.toml.** In `ghostframe-client-wasm/Cargo.toml` add under `[dependencies]` (after `ghostframe-protocol`):

```toml
ghostframe-web-ui = { path = "../ghostframe-web-ui" }
```

and at the end:

```toml
web-sys = { version = "0.3", features = ["HtmlElement"] }
```

- [ ] **Step 4: core.rs.** Edits to `ghostframe-client-wasm/src/core.rs` (the existing file; each anchor below was checked to occur exactly once). It changes `inner` to `Rc<RefCell<ClientCore>>`, takes the borrow in every method (`borrow()` for `&self`, `borrow_mut()` for `&mut self`), sets the client version at construction, adds `handleServerStream` and `recordPaints`, adds the non-exported `shared()`, fixes a doc comment the new stream reader makes false, and adds a host unit test that the panel's `shared()` core is the one the exports drive.

1. **Imports.** Before `use ghostframe_client_core::{ClientConfig, ClientCore, H264Outcome, TileDelivery};` add:

```rust
use std::cell::RefCell;
use std::rc::Rc;

```

2. **The field.** Replace `    inner: ClientCore,` with:

```rust
    /// Shared with the stats panel (`ghostframe-web-ui`), which reads the
    /// stats and queues subscribe messages from its own timer and click
    /// handlers. Those run as separate browser tasks, never inside one of the
    /// calls below: no method here calls back into JS, so a `borrow_mut` here
    /// cannot find the panel's borrow held.
    inner: Rc<RefCell<ClientCore>>,
```

3. **The constructor.** Replace

```rust
        WasmClientCore {
            inner: ClientCore::new(config, now_us),
        }
    }
```

with

```rust
        let mut core = ClientCore::new(config, now_us);
        core.set_client_version(crate::CLIENT_VERSION);
        WasmClientCore {
            inner: Rc::new(RefCell::new(core)),
        }
    }

    /// Feed bytes read from the feedback stream's return half, in whatever
    /// chunks the transport produced. Carries the stats schema and samples;
    /// malformed input is counted by the core, never thrown.
    #[wasm_bindgen(js_name = handleServerStream)]
    pub fn handle_server_stream(&mut self, bytes: &[u8], now_us: u64) {
        self.inner.borrow_mut().handle_server_stream(bytes, now_us);
    }

    /// Note `n` frames painted, for the panel's painted-fps row. Called by
    /// `main.ts` until the renderer moves into Rust.
    #[wasm_bindgen(js_name = recordPaints)]
    pub fn record_paints(&mut self, n: u32) {
        self.inner.borrow_mut().record_paints(n);
    }
```

4. **Every other `self.inner` use.** One replacement each (a `&self` method takes `borrow()`, a `&mut self` method `borrow_mut()`):

| Replace | With |
|---|---|
| `self.inner.request_reset(r, frame_seq);` | `self.inner.borrow_mut().request_reset(r, frame_seq);` |
| `self.inner.h264_result(frame_seq, outcome, now_us);` | `self.inner.borrow_mut().h264_result(frame_seq, outcome, now_us);` |
| `self.inner.h264_not_output_stats()` | `self.inner.borrow().h264_not_output_stats()` |
| `self.inner.handle_datagram(bytes, now_us)` | `self.inner.borrow_mut().handle_datagram(bytes, now_us)` |
| `self.inner.on_timeout(now_us)` | `self.inner.borrow_mut().on_timeout(now_us)` |
| `match self.inner.poll_transmit(now_us) {` | `match self.inner.borrow_mut().poll_transmit(now_us) {` |
| `self.inner.poll_timeout()` | `self.inner.borrow().poll_timeout()` |
| `self.inner.cdf53_coverage_summary()` | `self.inner.borrow().cdf53_coverage_summary()` |
| `self.inner.datagram_counts()` | `self.inner.borrow().datagram_counts()` |
| `self.inner.encode_feedback(now_us)` | `self.inner.borrow_mut().encode_feedback(now_us)` |

In `cdf53_incomplete_tiles`, the chain `self\n            .inner\n            .cdf53_incomplete_tiles(limit)` becomes `self\n            .inner\n            .borrow()\n            .cdf53_incomplete_tiles(limit)`.

`handle_datagrams` takes one `RefMut` for the whole batch and drops it before it builds JS values: replace `let mut events = Vec::new();` with `let mut core = self.inner.borrow_mut();\n        let mut events = Vec::new();`, replace `events.extend(self.inner.handle_datagram(datagram, *at as u64));` with `events.extend(core.handle_datagram(datagram, *at as u64));`, and insert `drop(core);` on the line after the loop's closing `}` (directly before `let mirrored: Vec<WasmEvent> = events`).

`h264_result` and `h264_not_output_stats` are written on one line in the table: if `cargo fmt` wraps the new `h264_result` call, that is fine.

5. **A doc comment the new reader makes false.** On `handle_datagram`, replace

```rust
    /// Inbound is datagrams-only: the browser's bidi stream carries outbound
    /// feedback, and the server sends tiles, dimensions, palettes and H.264
    /// access units over `transport.datagrams`.
```

with

```rust
    /// Tiles, dimensions, palettes and H.264 access units arrive as
    /// datagrams, through here. The feedback bidi stream's return half
    /// carries the server's stats frames only, through
    /// [`WasmClientCore::handle_server_stream`].
```

6. **`shared()` and the test.** Append at the end of the file (after the last `}` of `impl WasmClientCore`):

```rust

impl WasmClientCore {
    /// The shared core, for the panel. Not exported to JS.
    pub(crate) fn shared(&self) -> Rc<RefCell<ClientCore>> {
        self.inner.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panel and the JS-facing wrapper must be looking at one core: the
    /// version set at construction, a server stream and the paint count all
    /// land in the core `shared()` hands the panel.
    #[test]
    fn the_panel_shares_the_core_the_exports_drive() {
        let mut core = WasmClientCore::new(true, true, true, 0);
        let shared = core.shared();
        assert_eq!(
            shared.borrow().stats().view(0).client_version,
            crate::CLIENT_VERSION
        );
        core.handle_server_stream(&[0x02, 0xff, 0xff], 0); // oversized frame
        assert_eq!(shared.borrow().stats().samples_rejected(), 1);
        shared.borrow_mut().on_timeout(1_000_000);
        core.record_paints(7);
        shared.borrow_mut().on_timeout(2_000_000);
        let view = shared.borrow().stats().view(2_000_000);
        let painted = view
            .groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .find(|r| r.label == "Painted frames")
            .and_then(|r| r.current);
        assert_eq!(painted, Some(7.0));
    }
}
```

Check before moving on: `grep -n "self.inner" ghostframe-client-wasm/src/core.rs` lists only lines that contain `.borrow()`, `.borrow_mut()` or `.clone()` (the 11 original uses all converted, plus the three new ones).
- [ ] **Step 5: panel.rs and lib.rs.** Create `ghostframe-client-wasm/src/panel.rs`:

```rust
//! Mounting the stats panel from JS.

use std::rc::Rc;

use ghostframe_web_ui::CoreBackend;
use wasm_bindgen::prelude::*;

use crate::core::WasmClientCore;

/// Draw the stats panel into `container`, wired to `core`. Call once, after
/// the core exists. An error means no panel; the session is unaffected, so
/// the caller logs it and carries on.
#[wasm_bindgen(js_name = mountStatsPanel)]
pub fn mount_stats_panel(
    container: web_sys::HtmlElement,
    core: &WasmClientCore,
) -> Result<(), JsValue> {
    let backend = Rc::new(CoreBackend::new(core.shared()));
    ghostframe_web_ui::mount_stats_panel(container, backend)
}
```
and add `pub mod panel;` after `pub mod input;` in `ghostframe-client-wasm/src/lib.rs`.

- [ ] **Step 6: Build, then run every JS suite against the refactor.**

```bash
cd ~/work/gf-stats-client
cargo fmt -p ghostframe-client-wasm
cargo clippy -p ghostframe-client-wasm --all-targets -- -D warnings 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
cargo clippy -p ghostframe-client-wasm --target wasm32-unknown-unknown --all-targets -- -D warnings 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
cargo test -p ghostframe-client-wasm --lib 2>&1 | grep "test result"; echo "rc=${PIPESTATUS[0]}"
cd ghostframe-web-client
npm run build:wasm:node 2>&1 | tail -2; echo "rc=${PIPESTATUS[0]}"
npx vitest run 2>&1 | tail -15; echo "rc=${PIPESTATUS[0]}"
```

(each piped line is followed by `echo "rc=${PIPESTATUS[0]}"`, see "Exit status" at the top; all must print `rc=0`.)

- [ ] **Step 6b: Keep the panel panic-free (the residual risk above).**

```bash
cd ~/work/gf-stats-client
grep -n 'unwrap()\|expect(' ghostframe-web-ui/src/*.rs ghostframe-client-wasm/src/panel.rs
```

Expected: every hit is inside a `#[cfg(test)]` module (the test helpers in `format.rs`, `spark.rs` and `storage.rs` may unwrap); no hit in component, backend, storage or mount code. Fix any other hit with `?`, `if let`, or `unwrap_or*` before committing.

Expected: both clippy runs clean; 6 host tests pass (the new `the_panel_shares_the_core_the_exports_drive` among them); the whole vitest run passes. The existing suites (`tile_assembly`, `cdf53_globals`, `eviction`, `h264_result`, `reset_request`, `datagram_batch`, ...) drive every `WasmClientCore` method through the new `borrow` calls, so they are the guard for the refactor; `wasm_smoke` includes the stamp check (`protocol_stamp`) which changed because `client-core/src` changed and passes only because `build:wasm:node` just rebuilt it.

- [ ] **Step 7: Commit, then break it.**

```bash
cd ~/work/gf-stats-client
git add ghostframe-client-wasm/Cargo.toml ghostframe-client-wasm/src/core.rs ghostframe-client-wasm/src/lib.rs ghostframe-client-wasm/src/panel.rs ghostframe-web-client/tests/wasm_smoke.test.ts Cargo.lock
git commit -m "feat(client-wasm): share the core with the stats panel; mountStatsPanel, handleServerStream, recordPaints" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

| Mutation in `core.rs` | Test that must go red |
|---|---|
| `new`: delete `core.set_client_version(crate::CLIENT_VERSION);` | `cargo test -p ghostframe-client-wasm --lib`: `the_panel_shares_the_core_the_exports_drive` (client version is `unknown`) |
| `handle_server_stream`: replace the body with `let _ = (bytes, now_us);` | the same test (`samples_rejected` is 0) |
| `record_paints`: empty body | the same test (`Painted frames` is `None`) |
| `shared()`: return a new core instead of `self.inner.clone()` | the same test |

Not testable and therefore covered by the audit table above and by the live check in Task 17: `handle_datagrams` keeping its `RefMut` while it builds JS values (delete `drop(core);` and nothing fails, since nothing else runs in between today). Say so in the PR description.

## Task 15: `main.ts`: read the return half, mount the panel, count paints

**Model:** sonnet

**Files:**
- Create: `ghostframe-web-client/src/server_stream.ts`, `ghostframe-web-client/tests/server_stream.test.ts`
- Modify: `ghostframe-web-client/src/main.ts`, `ghostframe-web-client/index.html`

Spec 1.1 says the client never read the feedback stream's return half; this task starts reading it. The reading loop is its own module so it is unit-tested without a browser (a chunk handler that throws must not end the reader; a stream error ends it quietly). `main.ts` stays a wiring file, which has no unit tests in this repo; its changes are verified by `tsc`, the live check in Task 17, and the e2e in PR 3.

- [ ] **Step 1: Write the failing test.** Create `ghostframe-web-client/tests/server_stream.test.ts`:

```ts
import { describe, it, expect } from 'vitest';
import { pumpServerStream } from '../src/server_stream';

function streamOf(chunks: Uint8Array[], failWith?: unknown): ReadableStream<Uint8Array> {
  let i = 0;
  return new ReadableStream<Uint8Array>({
    pull(controller) {
      if (i < chunks.length) controller.enqueue(chunks[i++]);
      else if (failWith !== undefined) controller.error(failWith);
      else controller.close();
    },
  });
}

describe('pumpServerStream', () => {
  it('delivers every non-empty chunk in order and resolves at the end', async () => {
    const seen: number[][] = [];
    await pumpServerStream(
      streamOf([new Uint8Array([1, 2]), new Uint8Array([]), new Uint8Array([3])]),
      (b) => seen.push(Array.from(b)),
    );
    expect(seen).toEqual([[1, 2], [3]]);
  });

  it('keeps reading after a chunk handler throws', async () => {
    const seen: number[] = [];
    const warnings: string[] = [];
    await pumpServerStream(
      streamOf([new Uint8Array([1]), new Uint8Array([2]), new Uint8Array([3])]),
      (b) => {
        if (b[0] === 2) throw new Error('boom');
        seen.push(b[0]);
      },
      (m) => warnings.push(m),
    );
    expect(seen).toEqual([1, 3]);
    expect(warnings).toHaveLength(1);
  });

  it('resolves, and warns, when the stream errors', async () => {
    const warnings: string[] = [];
    await pumpServerStream(streamOf([new Uint8Array([1])], new Error('reset')), () => {}, (m) =>
      warnings.push(m),
    );
    expect(warnings).toHaveLength(1);
  });
});
```

- [ ] **Step 2: Run and see it fail.**

```bash
cd ~/work/gf-stats-client/ghostframe-web-client
npx vitest run tests/server_stream.test.ts 2>&1 | tail -8; echo "rc=${PIPESTATUS[0]}"
```

Expected: `Failed to resolve import "../src/server_stream"`.

- [ ] **Step 3: Implement the pump.** Create `ghostframe-web-client/src/server_stream.ts`:

```ts
/**
 * Reads the feedback stream's return half and hands each chunk to `onChunk`.
 *
 * The server writes the stats schema and samples there (see
 * `ghostframe-protocol/src/stats.rs`). Chunks are arbitrary slices of that
 * byte stream; the core reassembles frames across them, so this only moves
 * bytes. One chunk that makes `onChunk` throw is logged and skipped: it must
 * not end the reader, since the next chunk may be fine.
 *
 * Resolves when the stream ends or errors (a closing session ends it).
 */
export async function pumpServerStream(
  readable: ReadableStream<Uint8Array>,
  onChunk: (bytes: Uint8Array) => void,
  warn: (msg: string, err: unknown) => void = (m, e) => console.warn(m, e),
): Promise<void> {
  const reader = readable.getReader();
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      if (!value || value.length === 0) continue;
      try {
        onChunk(value);
      } catch (e) {
        warn('server stream: chunk handler failed; continuing', e);
      }
    }
  } catch (e) {
    warn('server stream ended with an error', e);
  } finally {
    reader.releaseLock();
  }
}
```
- [ ] **Step 4: Run and pass.**

```bash
cd ~/work/gf-stats-client/ghostframe-web-client
npx vitest run tests/server_stream.test.ts 2>&1 | tail -8; echo "rc=${PIPESTATUS[0]}"
```

Expected: 3 tests pass.

- [ ] **Step 5: `main.ts`.** Four edits.

(a) Add to the imports (next to the other `./` imports, e.g. after `import { bootstrap } from './bootstrap.js';`):

```ts
import { pumpServerStream } from './server_stream.js';
```

and add `mountStatsPanel,` to the named imports from `'../pkg-web/ghostframe_client_wasm.js'` (the list containing `WasmClientCore`).

(b) Keep the return half. Replace the `feedbackWriter` block:

```ts
  const feedbackWriter = await (async () => {
    try {
      const bidi = await transport.createBidirectionalStream();
      return bidi.writable.getWriter();
    } catch {
      console.warn('Could not open feedback stream');
      return null;
    }
  })();
```

with:

```ts
  const feedback = await (async () => {
    try {
      const bidi = await transport.createBidirectionalStream();
      return { writer: bidi.writable.getWriter(), readable: bidi.readable };
    } catch {
      console.warn('Could not open feedback stream');
      return null;
    }
  })();
  const feedbackWriter = feedback ? feedback.writer : null;
```

(c) Mount the panel and start the reader. Insert directly after the `try { await drainTransmit(); } catch (e) { console.warn('HELLO write failed:', e); }` block (so the HELLO is the first thing in the outbox, and the panel's subscribe, queued at mount if it was left open, follows it):

```ts
  // Stats panel (docs/specs/stats-panel-design.md). A broken panel must never
  // break the stream: mounting is caught and logged, and the session carries
  // on without one. `?e2e=lossless` strips the page chrome so a screenshot
  // holds only the canvas; the tab would sit over it, so no panel there.
  if (!url.searchParams.getAll('e2e').includes('lossless')) {
    try {
      const host = document.getElementById('stats-panel');
      if (!host) throw new Error('#stats-panel is missing from index.html');
      mountStatsPanel(host, core);
    } catch (e) {
      console.warn('stats panel failed to mount; continuing without it', e);
    }
  }
  // The server writes stats on the feedback stream's return half. Nothing is
  // sent until the panel subscribes, so this is idle by default.
  if (feedback) {
    void pumpServerStream(feedback.readable, (bytes) => core.handleServerStream(bytes, nowUs()));
  }
```

(d) Count painted frames. In `tick()`, replace

```ts
    try {
      renderer.encodeAndPresentFrame((codec, tx, ty, code) => {
        reportDecodeError(codec, tx, ty, code);
      });
    } catch (e) {
```

with the same block preceded by `const submitsBefore = renderer.submitCount;`:

```ts
    const submitsBefore = renderer.submitCount;
    try {
      renderer.encodeAndPresentFrame((codec, tx, ty, code) => {
        reportDecodeError(codec, tx, ty, code);
      });
    } catch (e) {
```

and insert after that `try/catch` (directly before the `// Drain accounting + the ~2 s stats line live in stats_log.ts.` comment):

```ts
    // Frames actually submitted this tick (an idle tick skips and adds none),
    // for the stats panel's painted-fps row. Until the renderer moves into
    // Rust the core cannot see paints, so the host reports them.
    const painted = renderer.submitCount - submitsBefore;
    if (painted > 0) core.recordPaints(painted);
```

- [ ] **Step 6: `index.html`.** After the `<canvas id="canvas"></canvas>` line add:

```html
    <div id="stats-panel"></div>
```

(The panel is `position: fixed`, so where it sits in the DOM does not matter; keeping it after the canvas keeps the canvas first. The input capture listens on the canvas element only, so the panel's clicks and scrolls never reach the remote desktop: `src/input/wire.ts` adds every listener to `canvas`.)

- [ ] **Step 7: Typecheck and run everything.**

```bash
cd ~/work/gf-stats-client/ghostframe-web-client
npm run build:wasm 2>&1 | tail -2; echo "rc=${PIPESTATUS[0]}"
npx tsc --noEmit && echo TSC_OK
npx tsc --noEmit -p tsconfig.test.json && echo TSC_TEST_OK
npx vitest run 2>&1 | tail -8; echo "rc=${PIPESTATUS[0]}"
```

Expected: `TSC_OK`, `TSC_TEST_OK`, all vitest files pass.

- [ ] **Step 8: Commit, then break it.**

```bash
cd ~/work/gf-stats-client
git add ghostframe-web-client/src/server_stream.ts ghostframe-web-client/tests/server_stream.test.ts ghostframe-web-client/src/main.ts ghostframe-web-client/index.html
git commit -m "feat(web-client): read the feedback stream's return half, mount the stats panel, report paints" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

| Mutation in `server_stream.ts` | Test that must go red |
|---|---|
| delete the inner `try { onChunk(value) } catch` (call `onChunk(value);` bare) | `keeps reading after a chunk handler throws` (the outer catch ends the loop after chunk 2, so `seen` is `[1]`) |
| `if (!value \|\| value.length === 0) continue;` -> `if (!value) continue;` | `delivers every non-empty chunk in order...` (an empty chunk reaches the handler) |
| delete `catch (e) { warn(...) }` of the outer `try` | `resolves, and warns, when the stream errors` (the promise rejects) |

Restore each with `git checkout -- ghostframe-web-client/src/server_stream.ts`.

- [ ] **Step 9: Check the tab against the existing e2e suite** (the tab is 20x56 px with `pointer-events: auto`, right edge, vertically centred, in every session except `?e2e=lossless`, so `?e2e=1` pages carry it). Run:

```bash
cd ~/work/gf-stats-client
grep -rn 'dispatchMouse\|Input\.dispatch\|elementFromPoint\|\.click(\|click_at\|mouseMoved\|mousePressed' ghostframe-e2e/src ghostframe-e2e/tests || echo "no e2e test sends mouse input"
grep -rn 'screenshot(' ghostframe-e2e/src ghostframe-e2e/tests | grep -v 'fn screenshot\|//' 
```

Recorded when this plan was revised (re-run it; the tree may have moved): the first command prints `no e2e test sends mouse input`, so no input test can land on the tab. The second lists only full-viewport screenshots: `e2e_h264_ssim_golden` (`ghostframe-e2e/tests/e2e.rs`, golden SSIM threshold 0.85 over the whole frame: a 20x56 px tab is about 0.05 % of a 1920x1080 frame), the `browser.screenshot()` convergence loops (consecutive shots both contain the tab, so stability is unaffected) and the scroll scene's reference SSIM (`scroll_scene.rs`). None crops to the canvas. Decision: keep mounting the panel under `?e2e=1`, because PR 3's e2e must click the tab on exactly such a page; only `lossless` is exempt (its golden is pixel-exact).

Because the daemon embeds the web client, the e2e image must be rebuilt to exercise any of this locally (`just containers-build`, only if no other worktree's e2e is using the shared image tag). If it is not run locally, CI's `e2e` job is the check: after pushing, read the results of `e2e_h264_ssim_golden` and the scroll-scene tests specifically, and record the outcome (or "not run locally; CI green") in the PR description. A failure there is a finding against this task, not something to loosen a threshold for.

## Task 15b: Shrink the panel's share of the wasm (owner decision 2026-11-13)

**Model:** sonnet

The owner accepted a 512 KiB raw cap (PR 3's guard) **and** asked for the
panel to be made smaller without touching the protocol core's code
generation: `ghostframe-client-core`, `ghostframe-protocol` and
`ghostframe-client-wasm` stay at the release profile's `opt-level = 3`. Each
step below is measured; a step that saves less than 2 % of the wasm is reverted
rather than kept.

**Files:**
- Modify: `Cargo.toml` (workspace root, `[profile.release.package.*]` sections)
- Modify: `ghostframe-web-ui/src/components.rs` (or whichever file Task 11 put the components in)

- [ ] **Step 1: Baseline.**

```bash
cd ~/work/gf-stats-client/ghostframe-web-client
npm run build:wasm 2>&1 | tail -2; echo "rc=${PIPESTATUS[0]}"
stat -c %s pkg-web/ghostframe_client_wasm_bg.wasm
gzip -c pkg-web/ghostframe_client_wasm_bg.wasm | wc -c
```

Write the two numbers down (call them A_raw, A_gz).

- [ ] **Step 2: List the Leptos crates that land in the wasm.**

```bash
cd ~/work/gf-stats-client
cargo tree -p ghostframe-web-ui --target wasm32-unknown-unknown -e normal --prefix none \
  | sed 's/ v.*//' | sort -u
```

Keep the runtime crates of the Leptos stack (expect at least `leptos`,
`tachys`, `reactive_graph`, `any_spawner`, `or_poisoned`, `throw_error`,
`leptos_dom`, `hydration_context`, `oco_ref`, `send_wrapper`; trust the output,
not this list). Exclude proc-macro crates (`leptos_macro`, `*_macro`,
`syn`, `quote`, `proc-macro2`): they run on the host and do not reach the
wasm.

- [ ] **Step 3: Size-optimise only those crates plus `ghostframe-web-ui`.**
Append to the workspace `Cargo.toml`, one section per crate from step 2:

```toml
# Size over speed for the stats panel's UI stack only (owner decision
# 2026-11-13, docs/specs/stats-panel-design.md section 5). The protocol core
# (ghostframe-client-core, ghostframe-protocol, ghostframe-client-wasm) keeps
# the release profile's opt-level 3: it is on the datagram hot path.
[profile.release.package.ghostframe-web-ui]
opt-level = "s"

[profile.release.package.leptos]
opt-level = "s"

[profile.release.package.tachys]
opt-level = "s"

[profile.release.package.reactive_graph]
opt-level = "s"
```

(and so on for every crate step 2 listed). None of these crates is in the
daemon's graph, so the server build is unaffected; confirm with

```bash
cargo tree -p ghostframe-server -e normal --prefix none | grep -cE '^(leptos|tachys|reactive_graph) ' || true
```

Expected: `0`.

- [ ] **Step 4: Measure** as in step 1 (B_raw, B_gz). Keep the change if
B_raw is at least 2 % below A_raw; otherwise revert the `Cargo.toml` hunk.

- [ ] **Step 5: Type-erase the repeated views.** In the components, every
`view!` that is returned from a `<For>` row closure, from a `match` on
`MetricKind`, or from an `if`/`else` (stale versus fresh, text versus number),
ends in `.into_any()`, and the component functions that return them return
`AnyView` instead of `impl IntoView`:

```rust
use leptos::prelude::*;

#[component]
fn MetricRow(row: RowView) -> AnyView {
    match row.kind {
        MetricKind::Text => view! { <TextRow row=row/> }.into_any(),
        _ => view! { <NumberRow row=row/> }.into_any(),
    }
}
```

This stops one monomorphised copy of the renderer per view shape. Build and run
the panel's tests:

```bash
cd ~/work/gf-stats-client
cargo clippy -p ghostframe-web-ui --target wasm32-unknown-unknown --all-targets -- -D warnings
cargo test -p ghostframe-web-ui
just test-web-ui
```

Expected: clean clippy, all tests pass (the browser test from Task 10 still
finds every `data-testid` hook).

- [ ] **Step 6: Measure** (C_raw, C_gz). Keep it if C_raw is at least 2 %
below the previous kept size; otherwise revert the components hunk.

- [ ] **Step 7: Check the hot path did not move.** The core must still be
built at opt-level 3:

```bash
cd ~/work/gf-stats-client
cargo build --release -p ghostframe-client-wasm --target wasm32-unknown-unknown -v 2>&1 \
  | grep -E "Running .*--crate-name ghostframe_client_core" | grep -o "opt-level=[0-9sz]"
```

Expected: `opt-level=3`.

- [ ] **Step 8: Commit** the kept changes, with the measured numbers (A, B, C)
in the commit body:

```bash
git add Cargo.toml ghostframe-web-ui/src/components.rs
git commit -m "perf(web-ui): size-optimise the panel's UI stack only

<A_raw> -> <final_raw> bytes raw (<A_gz> -> <final_gz> gzip).
The protocol core keeps opt-level 3.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

(Stage only the files you kept changes in.) Task 16 records the final sizes.

## Task 16: Record the bundle sizes; document the crate and the recipe

**Model:** haiku

**Files:**
- Modify: `docs/specs/wasm-bundle-baseline.md`
- Modify: `DEVELOPERS.md`

- [ ] **Step 1: Measure after.**

```bash
cd ~/work/gf-stats-client/ghostframe-web-client
npm run build:wasm 2>&1 | tail -2; echo "rc=${PIPESTATUS[0]}"
ls -l pkg-web/ghostframe_client_wasm_bg.wasm
gzip -c pkg-web/ghostframe_client_wasm_bg.wasm | wc -c
npx vite build 2>&1 | grep -E "dist/assets"; echo "rc=${PIPESTATUS[0]}"
```

Then measure once with a size-optimised profile, scoped to this build through the environment (nothing is committed; `wasm-pack` builds with the workspace's release profile otherwise):

```bash
cd ~/work/gf-stats-client/ghostframe-web-client
CARGO_PROFILE_RELEASE_OPT_LEVEL=s CARGO_PROFILE_RELEASE_LTO=true CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_PANIC=abort npm run build:wasm 2>&1 | tail -2; echo "rc=${PIPESTATUS[0]}"
ls -l pkg-web/ghostframe_client_wasm_bg.wasm
gzip -c pkg-web/ghostframe_client_wasm_bg.wasm | wc -c
npm run build:wasm 2>&1 | tail -2; echo "rc=${PIPESTATUS[0]}"   # back to the default profile: leave pkg-web as the shipped build
```

Record the size-optimised numbers beside the default ones. Note that `panic = "abort"` changes the failure mode only in that unwinding is gone (a wasm panic already traps), but it is a decision for the owner, not for this PR: do not commit any profile change here.

Expected (measured on a scratch copy during planning, default release profile): the wasm is about 487 000 bytes raw / about 189 000 gzip, against the baseline from Task 1 (about 168 000 raw). With `opt-level = "s"`, LTO, `codegen-units = 1` and `panic = "abort"` it was about 423 000 raw. A trivial Leptos mount alone is about 45 000 after `wasm-opt`. Record what you measure, not these numbers.

- [ ] **Step 2: Append to `docs/specs/wasm-bundle-baseline.md`** a section (fill the angle-bracket numbers from step 1 and from Task 1 step 4):

```markdown

## Stats panel (2026-11-12, `feat/stats-panel-client`)

`npm run build:wasm` (`wasm-pack build --target web`, release, `wasm-opt`), same toolchain, before and after the `ghostframe-web-ui` crate (Leptos 0.9.0-rc1, `csr`) was linked into `ghostframe-client-wasm`:

| | Raw bytes | Gzip bytes |
|---|---:|---:|
| Before (master) | <before raw> | <before gzip> |
| After (this branch) | <after raw> | <after gzip> |

Size-optimised profile (`opt-level = "s"`, LTO, one codegen unit, `panic = "abort"`), measured through `CARGO_PROFILE_RELEASE_*` only, nothing committed: <size-opt raw> bytes raw, <size-opt gzip> gzip.

The spec's original 320 KB cap did not hold with Leptos linked (<after raw> bytes raw). Record the owner's decision here: `PR 3 cap: 524,288 bytes raw (512 KiB), decided 2026-11-13`.
```

- [ ] **Step 2b: Check the bundle against PR 3's cap.** If the final wasm (after Task 15b) is over 524,288 bytes raw, stop and report to the owner before opening the PR: PR 3's guard would fail on it.

- [ ] **Step 3: `DEVELOPERS.md`.** In the Clients table add, after the `ghostframe-client-wasm` row:

```markdown
| `ghostframe-web-ui` | The browser client's DOM UI (the stats panel), in Leptos. The only crate that depends on Leptos; linked into `ghostframe-client-wasm`. `just test-web-ui` runs its tests, including one in headless Chrome. |
```

- [ ] **Step 4: Commit.**

```bash
cd ~/work/gf-stats-client
git add docs/specs/wasm-bundle-baseline.md DEVELOPERS.md
git commit -m "docs: record the wasm size with the stats panel; document ghostframe-web-ui" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

## Task 17: Gates

**Model:** sonnet

**Files:** none changed (fix-ups go in new commits).

- [ ] **Step 1: The real gate, under the GPU lock** (`ci-local` touches VA-API; never run it bare). Run it directly, never through a pipe: a pipe reports the pipe's status and has shipped a stale `dist/` before.

```bash
cd ~/work/gf-stats-client
gf-gpu-run just ci-local
echo "ci-local rc=$?"
```

Expected: `ci-local passed`, `rc=0`. It covers fmt, the env-read guard, `check-local-paths`, `check-release-features`, `cargo clippy --workspace --all-targets -- -D warnings` (this is what catches a non-compiling test target such as `ghostframe-web-ui/tests/panel_mount.rs` or `stats_core.rs`, which `cargo test -p` would hide), unit tests, `just test-web-ui`, the web build (`npm run build` runs `wasm-pack` and `vite`), `tsc`, the release build, both cbindgen header diffs (neither header may change: nothing in this PR touches `ghostframe-lib` or the C API's types), and `go vet`.

- [ ] **Step 2: Client crates changed, so the client gate too.**

```bash
cd ~/work/gf-stats-client
just ci-client
echo "ci-client rc=$?"
git diff --exit-code ghostframe-lib/include/ghostframe.h ghostframe-client-capi/include/ghostframe_client.h && echo HEADERS_UNCHANGED
```

Expected: `ci-client passed`, `rc=0`, `HEADERS_UNCHANGED`.

- [ ] **Step 3: The wasm-specific lint that the workspace clippy does not run, and the JS suites.**

```bash
cd ~/work/gf-stats-client
cargo clippy -p ghostframe-web-ui -p ghostframe-client-wasm --target wasm32-unknown-unknown --all-targets -- -D warnings 2>&1 | tail -3; echo "clippy rc=${PIPESTATUS[0]}"
cd ghostframe-web-client && npm test 2>&1 | tail -12; echo "npm test rc=${PIPESTATUS[0]}"
```

Expected: clippy `rc=0`; `npm test` (rebuilds both wasm targets, typechecks, runs vitest) passes.

- [ ] **Step 4: Release daemon is untouched by Leptos.**

```bash
cd ~/work/gf-stats-client
cargo tree -p ghostframe-server -i leptos 2>&1 | head -2
ci/check-release-features.sh; echo "check-release-features rc=$?"
git status --short
```

Expected: no `leptos` in the daemon's tree; the feature check prints `rc=0`; a clean `git status` (no stray `Cargo.lock`, `package-lock.json` or `dist/` changes).

- [ ] **Step 5: Live check (the owner's session; not blocking CI, but required before claiming the panel works).** From the desktop session of the user the daemon runs as (AGENTS.md "Deploying to a real session"): `just build-release`, `./target/release/ghostframe-server install --force`, hard-reload the browser tab (Ctrl+Shift+R, the wasm bundle is cached). Then verify by eye: the `<` tab sits on the right edge; clicking it opens the panel with the client and server versions equal and the Connection, Bandwidth, Video and Loss groups populated within about 2 s (round-trip time and bandwidth estimate non-zero); typing still goes to the remote desktop after clicking the tab (focus stayed on the canvas); closing it and reloading keeps it closed, opening and reloading keeps it open and subscribed; `journalctl _UID=$(id -u <user>) --since "-2min"` shows no panel errors. A stale cached bundle shows the red "versions differ" row, which is the feature working.

## Task 18: Review

**Model:** opus

Use superpowers:requesting-code-review on the whole PR diff (`git diff master...feat/stats-panel-client`) against `docs/specs/stats-panel-design.md` and this plan. The reviewer checks, at minimum:

- [ ] Every spec section in the coverage table below is implemented and has a test whose mutation was observed red (look for the mutation evidence in each task's commits or the PR description; a test that cannot fail is a finding).
- [ ] The shared contract names are used exactly (`ClientStats`, `set_stats_subscribed`, `handle_server_stream`, `stats`, `record_paints`, `PanelBackend`, `mount_stats_panel`, `mountStatsPanel`, `handleServerStream`, `recordPaints`) and every deviation is listed under "Contract deviations".
- [ ] No `Rc`/`RefCell` in `ghostframe-client-core`; Leptos appears in exactly one `Cargo.toml` (`grep -rn leptos --include=Cargo.toml .`).
- [ ] The re-entrancy audit in Task 14 is still true of the final diff (no export calls into JS while a borrow is held; `CoreBackend::view` reads the clock before it borrows), and the residual panic risk is stated in the PR description (no `unwrap`/`expect` outside tests in `ghostframe-web-ui/src` and `panel.rs`).
- [ ] No `_ =>` over a project enum in non-test code of `ghostframe-client-core` (the crate warns on it); `tally_events` and the stats matches list variants.
- [ ] No `/home/<user>` paths, no `git add -A` artefacts, `git status` clean, commit messages end with the co-author line.
- [ ] The wasm size numbers in `docs/specs/wasm-bundle-baseline.md` are real measurements, Task 15b's savings are in its commit body, and the wasm is under PR 3's 524,288-byte cap.
- [ ] `versions_match` is PR 1's rule (crate version and commit only; `dirty` and `unknown` never fail it), and both mutation rows for it were observed red.
- [ ] `ghostframe-web-ui` is in the e2e Dockerfile's per-crate copies; `just containers-build` is not needed for correctness (no server change), but the image must still build with the new member: run it if no other worktree's e2e is using the shared image tag, otherwise rely on CI's e2e image build.

Address findings in new commits (never amend the reviewed commits silently), re-run Task 17 steps 1 to 3, then hand the PR to the owner. Do not merge.

---

## Self-review

### Spec coverage

| Spec section | Requirement | Task |
|---|---|---|
| 1.3 | `ClientStats` in client-core, server rings + local metrics sampled once a second regardless of the panel | 3, 4, 6 |
| 1.3 | Local metrics: received throughput, wire loss / FEC recoveries / longest run, NACKs sent, decode errors, codec mix, frame size, painted fps (`recordPaints`) | 4, 5 (`record_paints`), 6 (counting), 15 (TS reports paints) |
| 1.3 | Shared between `WasmClientCore` and the panel through `Rc<RefCell<..>>`, nothing crosses into JS | 14, 9 (`CoreBackend`) |
| 1.3 | Native client gets it through the core, C API out of scope | 5 (core owns it; no capi change; Task 17 header diffs) |
| 1.4 | Client `build.rs`: version + commit + dirty, `unknown` outside git, rerun on git change, no timestamp | 13 |
| 1.4 | Panel compares versions and says "versions differ, hard-reload (Ctrl+Shift+R)" | 3 (`versions_match`), 11 (`VersionRow`), 10 (browser test) |
| 1.1 (client half) | Subscribe through the core's outbox; read the return half; reassemble frames; unknown type skipped; oversize counted | 5, 3 (`handle_stream_bytes`), 15 (pump) |
| 2.1 | Components `StatsPanel`, `Tab`, `Group`, `MetricRow`, `Sparkline`, `VersionRow`; keyed rows; unknown group in trailing "Other" | 11, 3 (group order test) |
| 2.1 | `mountStatsPanel(container, core)`; `main.ts` calls it once; `index.html` gets `<div id="stats-panel">` | 14, 15 |
| 2.2 | Fixed right-edge layout, ~320 px, canvas never moved, closed = tab only, open state in `localStorage` with every access guarded, page loads open subscribes at once, hand-written CSS | 11, 9 (`storage.rs`), 10 |
| 2.3 | Capture listens on the canvas only; tab cancels `mousedown` | 11 (+ browser test), 15 (verified `wire.ts` binds to `canvas`) |
| 2.4 | SI prefixes, three significant digits; counters as `/s` rates, backwards counter restarts; text metrics as strings; 3 s staleness with dashes and kept history; new `schema_id` clears rings | 7, 3 |
| 2.4 | "A session reset or reconnect makes the client subscribe again (if open)" | Not needed on the web: there is no auto-reconnect (a new page load builds a new core, and the panel subscribes at mount if it was open). See open questions. |
| 3 | Mismatched / early / undecodable samples dropped and counted, rows stale not misaligned; never panic | 3 (tests), 14 (hostile-bytes vitest) |
| 3 | Mount failure logged, session carries on | 15 (`try/catch` in `main.ts`), 11 (`mount_stats_panel` returns `Result`) |
| 4 | Client stats tests (ring wraparound, counter to rate incl. backwards, staleness, clear on new schema, rejected counted) | 3 |
| 4 | Panel logic tests (SI formatting, sparkline scaling normal/flat/empty, version comparison incl. `unknown` and `dirty`, per PR 1's rule) | 7, 8, 3 |
| 4 | One `wasm-bindgen-test` in headless Chrome: mount, toggle, subscribe/unsubscribe reach the core | 10, 11 |
| 4 | New integration test targets named in CI | 12 (`web-ui` job; `tests/stats_core.rs` is covered by the package-wide `cargo test -p ghostframe-client-core`) |
| 5 | Leptos pinned exactly, `csr` only | 2 (`=0.9.0-rc1`; 0.9.0 final is not published) |
| 5 | New workspace member in the e2e Dockerfile's per-crate copies | 2 |
| 5 | Record wasm size before and after | 1, 16 |
| 5 | Gate: `just ci-local` under the GPU lock, header diff, release-feature check | 17 |
| 6 item 2 | Client and panel PR scope (ClientStats, web-ui crate, client version, mountStatsPanel, main.ts/index.html) | 3 to 16 |

### Contract deviations

All additive; no contract name is changed.

1. `ClientCore::set_client_version(impl Into<String>)` (client-core) and `StatsView.server_host: Option<String>` / `RowView.{id, text, local}` / `GroupView{key, title, rows}` are added: the contract lists the view's content but not its exact field set; the client version has to reach `ClientStats` from the wasm crate's `build.rs`, which the core cannot see.
2. `ghostframe-client-core` re-exports `MetricKind` and `MetricGroup` from the protocol (the panel needs them and should not depend on `ghostframe-protocol` outside tests).
3. `ghostframe-web-ui` also exports `CoreBackend` (the production `PanelBackend` over `Rc<RefCell<ClientCore>>`), so the browser test exercises the code that ships; `mount_stats_panel`'s `Result` is always `Ok` today (Leptos' `mount_to` cannot fail), kept so a failing mount path can be added without an API change.
4. `ghostframe-client-wasm` also exports `clientVersion()`; `WasmClientCore::shared()` is `pub(crate)`, not exported.
5. The panel carries `data-testid="stats-tab"` / `"stats-row"` with `data-label`, `data-current`, `data-stale` already in this PR (the e2e plan, PR 3, would otherwise add them).
6. Stats frames that fail to decode and samples that mismatch the schema share one counter (`samples_rejected`); the contract names one counter and the spec says decode errors are "counted".
7. `Tab` is the `<button class="gf-tab">` inside `StatsPanel`, not a separately defined Leptos component (it would only forward three closures).
9. **Version match semantics are PR 1's, adopted unchanged.** `StatsView::versions_match` parses `<pkg> (<commit>[, dirty])` on both sides and compares the crate version and the commit only; `dirty` is displayed in the version string but never fails the match; an `unknown` commit on either side, an unparsable string, or no server version yet is a match. One reading, stated: a different *crate version* is a mismatch even when a commit is `unknown` (PR 1 says an unknown *commit* never mismatches; the crate version is the other part it compares). Reason (PR 1, Task 8 preamble): the two `dirty` flags are computed at different build-script runs and legitimately disagree on current code.
8. The plan assumes PR 1 provides `Default` for `ServerFrameReader` and `Copy + PartialEq` on `MetricKind`/`MetricGroup` (its plan does); Task 1 verifies it.

### Spec gaps found

- **Size cap.** Section 5's original 320 KB cap could not hold (about 487 KB with the panel linked). Owner decision 2026-11-13: 512 KiB cap plus Task 15b's reduction.
- **Dirty flag.** `git status --porcelain` is non-empty on every developer checkout because of untracked files (four here), so "dirty" would always be set. This plan uses `--untracked-files=no`. It is best-effort and display-only: stale until the build script reruns, and ignored by the match rule (adopted from PR 1, Contract deviation 9), so a disagreement between the server's and the client's flag cannot raise a false "versions differ" alert.
- **A panel panic ends the session.** The panel shares the wasm instance with the session; only mount errors are contained by `main.ts`. See the Task 14 audit; stated in the PR description.
- **Leptos 0.9.0 final** is not published; the pin is `=0.9.0-rc1` (the spec allows this).
- **`?e2e=lossless` screenshots.** The tab would overlay the canvas the lossless-golden e2e captures; the panel is not mounted in that mode.
- **Decode errors.** The "decode errors" local metric counts errors the core detects (prevalidation); errors the WebGPU renderer reports through `WasmDecodeErrorBatcher` never reach the core, so the row undercounts until the renderer moves into Rust (project D).
- **Reconnect.** Spec 2.4's "a session reset or reconnect makes the client subscribe again" has no web trigger (no auto-reconnect by design); it would matter for the native client, which is out of scope.

### Open questions for the owner

1. **Wasm budget (a gate for PR 3, not only a question).** Raise the cap (to about 550 KB raw), accept the size, or slim the panel (use `reactive_graph`/`tachys` directly instead of the full `leptos` crate, or a size-optimised wasm profile; Task 16 measures that profile)? PR 3's `ci/check-wasm-size.sh` takes its cap from this answer (recorded as `PR 3 cap:` in `docs/specs/wasm-bundle-baseline.md`); PR 3 must not start its guard task before it is filled in.
2. Pin `=0.9.0-rc1` now, or hold the PR until 0.9.0 final?

(The earlier question about the dirty-flag policy is settled: tracked files only, display-only, never part of the match.)

## Review notes

Revision of 2026-11-12 against the plan review. Every blocker and major issue is fixed; minor ones too, with the exceptions and choices below.

- **Not done as suggested: the temporary `#[allow(dead_code)]`s** (Task 4 `sample_local`/`next_local_sample_us`, Task 9 `load_open`/`save_open`) were chosen over moving `load_open`/`save_open` into Task 11, because the review offered both and the allow keeps each task's files in one place; each is removed in the task that adds the caller (Task 6, Task 11), with the removal listed in that task's Files and `git add`.
- **`GIT_OPTIONAL_LOCKS=0` through `Command::env`**, not `--no-optional-locks` (PR 1 uses the flag in its own `build.rs`): the two are equivalent; the env form keeps every call site in this file unchanged.
- **Panic risk: acknowledged and mitigated, not eliminated.** A panic in Leptos itself still ends the session; the plan says so (Task 14 audit, Self-review) instead of claiming containment.
- **Only the `data-stale="false"` side is asserted in the browser** (Task 10): making a row stale takes 3 real seconds of wall-clock in the test; the stale flag is covered in client-core. Stated in the test description rather than hidden.
- **PR 3's plan is not edited** (out of scope here). The conflict is recorded as a precondition in Task 16 Step 2b and Open question 1; whoever owns PR 3's plan must read the cap from that decision.
- **`versions_match` vs a different crate version with an `unknown` commit:** compared on the crate version; see Contract deviation 9 for the reading.
