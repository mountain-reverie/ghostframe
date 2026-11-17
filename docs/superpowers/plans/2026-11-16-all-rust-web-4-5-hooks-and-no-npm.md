# All-Rust Web Client PR 4 + PR 5: E2E Hooks in Rust, No npm — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Each task header carries a model tag:
> - **[haiku]** is mechanical: the code is given whole, or it is a deletion or rename.
> - **[sonnet]** needs judgement: browser globals, build scripts, CI.
>
> Opus reviews every task, per the owner's model split.

**Goal:** The browser client is Rust end to end. Every `window.__ghostframe*` global the e2e suite and the bench read is served by `ghostframe-client-wasm`, under `?e2e` only, with its old name and JSON shape. The TypeScript (`src/`, `tests/`), `package.json`, Vite, vitest and Node are gone from the repository, CI and docs. The web client is built by one script: `cargo` builds the wasm, the `wasm-bindgen` CLI makes the JS glue, `wasm-opt` optimises it, and the result is copied into `ghostframe-web-client/dist/`. The full local GPU e2e suite passes. CI does not run that suite, so this PR runs it and fixes what it finds.

**Owner decision (2026-11-16):** PR 4 and PR 5 ship as one PR, gated by a full local GPU e2e run.

**Architecture.**

**The hooks.** PR 3 left a queue of `HookCall`s that `app/client.rs` hands to JS (`hooks.ts`) after each step, with no borrow held. This plan keeps that queue and its call sites, and swaps the JS target for Rust:
- **`app/recorder.rs`** (host-tested) turns each `HookCall` into counter updates and log entries. It ports `cdf53_globals.ts`, `diagnostics.ts`'s recorders and `hooks.ts`'s bookkeeping.
- **`app/health.rs`** (host-tested) ports `health.ts`.
- **`app/stats_line.rs`** (host-tested) ports `stats_log.ts`'s line building.
- **`app/debug_hooks.rs`** (wasm32) installs the globals on `window` under `?e2e`. Each log is a real JS array that Rust appends to, keeping `appendCapped`'s cap-to-twice-cap trimming, so an e2e poll costs what it costs today. The scalars and small objects are getters over the Rust state, and the functions are Rust closures.
- **The ~2 s stats line** is page UI, not an e2e hook, so it runs in production too, as it does today.

**The build.**
- `ci/build-web.sh` (one script for `just build-web` and every CI job):
  1. Checks the pinned tool versions.
  2. Builds the release wasm.
  3. Runs `wasm-bindgen --target web`.
  4. Runs `wasm-opt` with the flags wasm-pack used.
  5. Names both files by content hash.
  6. Writes `dist/index.html` from a template whose inline module script imports the glue and calls `startClient()`.
- `just web-tools` installs the pinned `wasm-bindgen-cli` and binaryen into `target/web-tools/`. That directory is repo-local and gitignored, so nothing touches the developer's home.
- The browser tests keep `wasm-pack test` (spec decision 4 is about the build; replacing the test runner is a separate, optional step, listed as out of scope).

**Tech Stack:**
- Rust 1.96.1;
- `wasm-bindgen` 0.2.129 (crate and CLI pinned to the same version);
- binaryen `version_117`, the `wasm-opt` that wasm-pack 0.15.0 uses today;
- `wasm-bindgen-test =0.3.79` via `wasm-pack` 0.15.0 for the browser tests only;
- bash for the build script.

**Branch:** `feat/web-no-npm`, worktree `~/work/gf-web-nonpm`, off master `579de2ac` (PR 3, #320, merged). **Spec:** `docs/specs/all-rust-web-client-design.md` §5.5, §5.6, the PR 4 and PR 5 rows of §6, and the PR 4/5 rows of §8.

**Exit status.** A pipe reports the last command's status, and a masked build failure has shipped a stale `dist/` here before. Every gating command in this plan that is piped into `tail` or `grep` is followed by `echo "rc=${PIPESTATUS[0]}"`, and its step names the rc it expects. A step that does not mention rc expects `0`.

**Paths and locks.**
- The repository root is the worktree root.
- `~/work/gf-gpu-run` is the GPU guard. `just ci-local` is always `~/work/gf-gpu-run just ci-local`.
- `just test-e2e` and `just containers-build` take `flock -o ~/work/.gf-heavy.lock` outside the guard.
- `export TMPDIR=~/work/gf-tmp-webr` before every build.
- Browser tests (`wasm-pack test`, `just test-web-renderer`, `just test-web-ui`, `just ci-local`) and the e2e run **only in Task 13**, which the controller runs (the owner asked for the full GPU e2e in this PR). Implementer and reviewer agents compile the browser tests (`just lint-web`) but do not run them.

## Findings that shaped this plan (verified 2026-11-16 on master 579de2ac)

- **What the e2e suite and the bench read.** `grep -rhoE 'window\.__[A-Za-z0-9_]+' ghostframe-e2e ghostframe-bench` finds:

  | Group | Globals |
  |---|---|
  | Recorder logs | `__ghostframeRecordedTiles`, `__ghostframeRecordedCodecs`, `__ghostframeRecordedFlags`, `__ghostframeRecordedResizes`, `__ghostframe_framePaints`, `__h2_clientPaletteWrites`, `__h5_tilePushLog` |
  | Counters and small objects | `__ghostframeStats`, `__ghostframeRafTicks`, `__ghostframeStatsSamples`, `__ghostframeStatsFrames`, `__cdf53DispatchSeen`, `__cdf53PushedToQueue` |
  | Functions | `__ghostframeHealth`, `__ghostframeRendererState`, `__ghostframeTile`, `__ghostframePaintUs`, `__ghostframeTickUs`, `__cdf53CoverageSummary`, `__readPixel`, `__readPixelRect`, `__readGradientGolden` |
  | Raw handles | `__ghostframeRenderer` (`texture.width` / `.height`) |

  `__events` is the harness's own.
- **Read by no e2e or bench code, not ported:** `__cdf53PrevalidateFails` and `__cdf53LastFailCode`. Only the stats line reads them, and it reads the Rust counters directly. List both in the PR body (spec §5.5: "a global with no reader is not ported; it is listed in the PR").
- **How the e2e suite reads the logs.** It treats them as JS arrays: `.length`, `.slice(-200)`, `.filter(...)` inside `evaluate`, and `JSON.stringify(...)`. It also polls them (`e2e_polled_globals_are_live`, the pixel and codec waits).
  - A getter that serialised a 32,768-entry Rust log on every read would put tens of milliseconds of main-thread work into every poll, next to the paint loop that several e2e tests time.
  - So the logs are real JS arrays that Rust appends to, one small object per event, exactly as the TypeScript did, with `appendCapped`'s trimming (trim to `cap` once the array reaches `2 * cap`; never `shift()`, memory: the V8 `Array.shift` cliff).
- **Logs only in JS, counters in Rust.** The counters live in Rust and are exposed by getters, which are cheap: a number, or a two-field object. Rust keeps a cheap count per log (pushes since the last trim) so it never reads `length` back.
- **The `?e2e` gate.** Until now `__ghostframeHealth`, the recorders and `health.record` ran in production too (PR 3 Findings). Spec §5.5 installs them "only under `?e2e`". In production a swallowed error goes to `console.error`, as it did with no hooks attached.
- **The console ring.** `health.ts` wrapped `console.log`/`info`/`warn`/`error` to mirror lines into a ring. A Rust closure cannot take JS rest arguments directly, so the wrapper is built with `js_sys::Function::new_with_args("orig, note", "return function (...a) { try { note(a.map(String).join(' ')) } catch (_) {} return orig.apply(this, a) }")` and given a Rust `note` closure. This is JS text inside Rust: no `.ts` file and no build step. The only other script on the page is `index.html`'s inline module.
- **What the deleted vitest suites become:**
  - `cdf53_globals.test.ts`, `diagnostics.test.ts` and `health.test.ts` become host tests of `recorder.rs` and `health.rs`.
  - `protocol_constants.test.ts` and `protocol_literals.test.ts` are obsolete: they stopped TS from retyping Rust constants, and no TS is left.
  - `sanity.test.ts` and `wasm_smoke.test.ts` are obsolete (spec §8): the build and the hooks browser test replace them. `wasm_smoke`'s protocol-stamp check compared a `pkg-node` build against the sources; with one cargo-driven build there is no second artefact to go stale.
- **JS-facing exports with no caller after this PR.** Every `#[wasm_bindgen]` export not reached from `index.html`, `startClient` or the browser tests goes, which also takes code out of the wasm (PR 3 is at 884,097 bytes raw). That means:
  - `units.rs`, whose module doc says it exists "so the existing vitest suites can be retargeted";
  - `constants.rs`, the getters `protocol_constants.ts` read;
  - the JS methods of `WasmClientCore` (`handleDatagram`, `pollTransmit`, ...);
  - `panel.rs`'s `mountStatsPanel`;
  - `app/debug_api.rs`, whose only caller was `hooks.ts`;
  - `WebRenderer`'s JS getters that only `hooks.ts` called.

  Keep what the Rust code or the browser tests still call. **Task 9's grep proves each removal unused before it goes.**
- **The build today.** `npm run build` runs `wasm-pack build --target web` and `tsc && vite build`. wasm-pack 0.15.0 runs binaryen `version_117`'s `wasm-opt -O` on the release wasm (its default; `ghostframe-client-wasm/Cargo.toml` has no `[package.metadata.wasm-pack]`). The cached binary is `~/.cache/.wasm-pack/wasm-opt-*/bin/wasm-opt`, version 117. Vite then hashes the asset names (`dist/assets/index-<hash>.js`, `ghostframe_client_wasm_bg-<hash>.wasm`). `ci/check-wasm-size.sh` globs `dist/assets/*.wasm`, and AGENTS.md tells developers to look for the bundle hash in the daemon binary, so the new build keeps hashed names under `dist/assets/`.
- **Who serves the page and builds the image.**
  - ghostbridge serves `dist/` from `//go:embed all:dist` (`web_dist.go`) with no CSP header, so an inline module script is allowed.
  - The test-server Docker image only `COPY`s the prebuilt `dist/` (`.dockerignore` keeps `ghostframe-web-client/dist/**`), so Docker needs no Node and no change beyond comments.
- **CI does not have `just`.** It is used nowhere in `.github/workflows`. Hence `ci/build-web.sh`, called by `just build-web` and by CI alike. `nightly.yml` already uses `taiki-e/install-action`.
- **Where npm and Node appear**, from grepping for `npm|npx|node_modules|vite|vitest|wasm-pack|setup-node`:
  - Justfile: `build-web`, and `ci-local`'s `npx tsc`.
  - `.github/workflows/_build-web-client/action.yml`.
  - `ci.yml`: the `web-client` job, with `tsc` and the artefact upload.
  - `e2e.yml`: four uses of the action, plus a fallback build with `setup-node`.
  - `nightly.yml`.
  - `client-core.yml`: the `wasm-module` job (vitest).
  - `.github/dependabot.yml`: the npm ecosystem.
  - `.gitignore` and `.dockerignore`.
  - `ghostbridge/Makefile`'s hint.
  - `ghostframe-tsnet/build.rs` comments.
  - DEVELOPERS.md, README.md, AGENTS.md, `docs/ci.md`, `docs/m0-manual-smoke-test.md`.
  - Comments in `ghostframe-client-core/tests/oracle_*.rs`, `ghostframe-protocol/src/codec/cdf53.rs`, `ghostframe-render/src/pipelines/solid.rs`, `ghostframe-web-ui/tests/panel_mount.rs`, and `ghostframe-client-wasm`.
- **The full GPU e2e is local only.** `ci/skip-list.txt` excludes the VKMS-gated tests and every H.264 e2e from CI. PR 3 merged without that run. Task 13 runs `just test-e2e` and `just test-e2e-stats` and fixes what they find. A failure is compared against master (`579de2ac`, built the same way) on the same machine before it is called a regression. Known flakes are in AGENTS.md. **Capture each failing test name before any rerun.**

## Gates that stop the work

| Gate | Where | What the agent does |
|---|---|---|
| G1 | Task 10 | The wasm `ci/build-web.sh` produces differs in raw size from PR 3's vite build of the same commit by more than 2%. Stop and report both numbers, and the `wasm-opt` flags of each. A size change must come from Task 9's export removals, which is measured separately, not from a changed optimisation. |
| G2 | Task 13 | An e2e test fails on this branch and passes on master on the same machine, and the cause is not obvious from its failure dump. Report it with the dump before changing anything. A fix that edits the e2e Rust code is the owner's decision: the spec says the e2e code is "not edited to accommodate the port". |
| G3 | Task 13 | The run needs a tool the machine lacks (Firefox, geckodriver). Skip that slice and say so. Never install system packages. |

## File structure

New, in `ghostframe-client-wasm/src/app/` (MIT OR Apache-2.0, SPDX first line):

| File | Target | Responsibility |
|---|---|---|
| `capped.rs` | host | `CappedCount`: the append-and-trim rule of `appendCapped`, as numbers (when to trim, to what) |
| `health.rs` | host | `Health`: swallowed-error counts, the recent list, the console ring, tallies, marks; `snapshot()` |
| `recorder.rs` | host | `Recorder`: `HookCall` → counter updates and `LogEntry`s; frame-paint bookkeeping; the protocol counters |
| `stats_line.rs` | host | `StatsLine`: the ~2 s page-log lines and their dedup |
| `debug_hooks.rs` | wasm32 | the `window` globals: JS-array logs, getters, functions; the console ring |

Modified:
- `app/hooks.rs` keeps `HookCall` and replaces the `Hooks` extern type and `dispatch` with a Rust sink.
- `app/client.rs`: who is called, the e2e gate, and the stats line in production.
- `app/mod.rs`.
- `lib.rs`, with `units.rs`, `constants.rs`, `panel.rs` and `app/debug_api.rs` deleted.
- `core.rs`, which loses its JS methods.
- `web/renderer.rs`, which loses its JS-only getters.

New elsewhere:
- `ci/build-web.sh`;
- `ghostframe-web-client/index.html`, which becomes the template;
- `ghostframe-client-wasm/tests/debug_hooks.rs`, the browser test of the hooks.

Deleted from `ghostframe-web-client/`:
- `src/`, `tests/` and `scripts/`;
- `package.json`, `package-lock.json`;
- `tsconfig.json`, `tsconfig.test.json`;
- `vite.config.ts`, `vitest.config.ts`.

What stays there: `index.html`, `.gitignore` and the built `dist/` (gitignored).

---

### Task 0: Baseline [haiku]

**Files:** none changed.

- [ ] **Step 1: Base.** Run `git -C ~/work/gf-web-nonpm log --oneline -2`. Expect HEAD to be the plan commit on top of `579de2ac`, with a clean tree.
- [ ] **Step 2: Record the PR 3 build.**

```bash
cd ~/work/gf-web-nonpm && export TMPDIR=~/work/gf-tmp-webr
just build-web > ~/work/gf-nonpm-build0.log 2>&1; echo "rc=$?"
ls -l ghostframe-web-client/dist ghostframe-web-client/dist/assets
f=$(ls ghostframe-web-client/dist/assets/*.wasm); stat -c %s "$f"; gzip -c "$f" | wc -c
cargo test -p ghostframe-client-wasm --lib 2>&1 | grep 'test result'; echo "rc=${PIPESTATUS[0]}"
```
Expected: rc=0 for both. Put in the report:
- the dist file list;
- the wasm's raw and gzip sizes;
- the host test count.

Task 10 compares against these.

No commit.

---

### Task 1: `CappedCount` and `Health` [haiku]

**Files:**
- Create `ghostframe-client-wasm/src/app/capped.rs` and `src/app/health.rs`.
- Modify `src/app/mod.rs` (add `pub mod capped;` and `pub mod health;`).

- [ ] **Step 1: Write `src/app/capped.rs`:**

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The trimming rule of the e2e logs (`appendCapped` in the old
//! `diagnostics.ts`): a log keeps its most recent `cap` entries, and is trimmed
//! back to `cap` only once it reaches twice that, so a reader sees between
//! `cap` and `2 * cap` entries, newest last. Never one-out-one-in: dropping the
//! head of a 32,768-element JS array costs V8 a move of the other 32,767
//! (64 us a call, measured), which once stalled the datagram loop for seconds.

/// Entries every e2e log keeps (sized for the busiest e2e: ~80 dirty tiles x
/// 30 fps x ~5 s settle, plus headroom).
pub const MAX_RECORDED_ENTRIES: usize = 32_768;

/// The length of a log, tracked on the Rust side so the JS array's `length`
/// is never read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CappedCount {
    len: usize,
    cap: usize,
}

impl CappedCount {
    pub fn new(cap: usize) -> Self {
        CappedCount { len: 0, cap: cap.max(1) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// One entry was appended. `Some(n)`: remove the `n` oldest entries now
    /// (the log reached twice its cap); the length is back to `cap`.
    pub fn push(&mut self) -> Option<usize> {
        self.len += 1;
        if self.len >= 2 * self.cap {
            let drop = self.len - self.cap;
            self.len = self.cap;
            Some(drop)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_between_cap_and_twice_cap_newest_last() {
        // diagnostics.test.ts "keeps the newest entries, between cap and twice cap".
        let mut c = CappedCount::new(4);
        let mut log: Vec<u32> = Vec::new();
        for i in 0..20 {
            log.push(i);
            if let Some(n) = c.push() {
                log.drain(..n);
            }
            assert!(c.len() == log.len() && log.len() < 8, "{log:?}");
        }
        assert!(log.len() >= 4);
        assert_eq!(*log.last().unwrap(), 19);
        assert_eq!(log, (20 - log.len() as u32..20).collect::<Vec<_>>());
    }

    #[test]
    fn trims_rarely_not_per_entry() {
        // "costs the same per entry at the production cap as at a small one":
        // one trim per `cap` pushes once full, never one per push.
        let mut c = CappedCount::new(MAX_RECORDED_ENTRIES);
        let trims = (0..10 * MAX_RECORDED_ENTRIES).filter(|_| c.push().is_some()).count();
        assert_eq!(trims, 9);
    }
}
```

- [ ] **Step 2: Write `src/app/health.rs`.** Port `ghostframe-web-client/src/health.ts`, the `Health` class, field for field:
  - **Constants:** `MAX_RECENT = 20`, `MAX_CONSOLE = 300`, `MAX_CONSOLE_HEAD = 100`.
  - **Methods:**
    - `mark(tile_x, tile_y, frame_seq, what)` keeps at most 12.
    - `tally(key)` keeps at most 64 distinct keys; a new key past that is dropped and an existing key still counts.
    - `record(site, message)` stores `format!("{site}: {message}")`.
    - `note(line)`.
    - `snapshot() -> HealthSnapshot`.
  - **`HealthSnapshot`:** `#[derive(Serialize)]`, `#[serde(rename_all = "camelCase")]`, with fields `swallowed`, `by_site` (a `BTreeMap<String, u32>`, serialised as `bySite`), `first`, `recent`, `console`, `console_head`, `tallies`, and `marks: Vec<Mark>`. `Mark` has `tile_x`, `tile_y`, `frame_seq`, `what`, also camelCase.
  - **The rings:**
    - `consoleRing` and `recent` drop their oldest entry when full. Use `VecDeque`, because in Rust that is O(1); the V8 caution does not apply.
    - `consoleHead` keeps the first 100 lines.
  - **Tests,** ported from `tests/health.test.ts` (read it on master):
    - `counts_swallowed_errors_per_site_and_keeps_the_first_message`;
    - `bounds_the_recent_list_and_the_console_ring`: 25 records keep the last 20; 350 notes keep 300 in the ring and 100 in the head;
    - `tallies_and_marks_are_bounded`: 70 distinct keys keep 64; 15 marks keep 12.

    The TS test "mirrors console.error and console.warn into the ring" is a browser behaviour. It moves to Task 7's browser test.

- [ ] **Step 3: Run the tests.** `cargo test -p ghostframe-client-wasm --lib app::capped app::health` must pass. Then show each test fails under one mutation, reverting each:
  - change `>= 2 * self.cap` to `> 2 * self.cap`;
  - remove the 64-key bound.

- [ ] **Step 4: Commit.**

```bash
git add ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/capped.rs ghostframe-client-wasm/src/app/health.rs
git commit -m "feat(web): e2e log trimming rule and the health record in Rust"
```

---

### Task 2: The recorder [sonnet]

**Files:** Create `ghostframe-client-wasm/src/app/recorder.rs`; modify `src/app/mod.rs`.

Port the bookkeeping of `hooks.ts` (`handleRecord`, `noteFrameTileReceived`, `beforePaint`, `datagramCounts`, `rafTick`, `serverStream`, `swallowed`, `rendererError`), `cdf53_globals.ts` (`recordProtocolEvent`) and `diagnostics.ts` (`recordTile`, `recordResize`, `recordFramePainted`, `recordPaletteWrite`). Read all of them on master first. The behaviour must match field for field. The JSON names of every entry are the ones the TS pushed.

- [ ] **Step 1: Write the types.**

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The e2e bookkeeping (port of `hooks.ts`, `cdf53_globals.ts` and the
//! recorders of `diagnostics.ts`). Pure: `apply` turns one `HookCall` into
//! counter updates here and `LogEntry`s for the browser side to append to the
//! `window` arrays (`debug_hooks.rs`). Only built under `?e2e`.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::app::health::Health;
use crate::app::hooks::HookCall;

/// `window.__ghostframeRecordedTiles` entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TileRecord {
    pub seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub codec: u8,
    pub payload_len: usize,
    pub fb_width: u32,
    pub fb_height: u32,
    pub first_recv_ms_client: f64,
    pub last_paint_ms_client: f64,
}

/// `window.__ghostframeRecordedResizes` entry; `trigger` is always `"sentinel"`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResizeRecord {
    pub seq: u32,
    pub old_w: u32,
    pub old_h: u32,
    pub new_w: u32,
    pub new_h: u32,
    pub trigger: &'static str,
}

/// `window.__ghostframe_framePaints` entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FramePaint {
    pub seq: u32,
    pub raf_ms_client: f64,
}

/// `window.__h2_clientPaletteWrites` entry: `fp` is the 8-hex-digit
/// `palette_fingerprint_bgra`, `c0` the first colour's 4 bytes as hex.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaletteWrite {
    pub id: u8,
    pub len: usize,
    pub fp: String,
    pub c0: String,
    pub ts: f64,
}

/// `window.__h5_tilePushLog` entry: `seq` is -1 for a decode error, `c0` the
/// first 8 payload bytes as hex (`LEN=0` when empty) or `ERR:<code>`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TilePush {
    pub seq: i64,
    pub tx: u8,
    pub ty: u8,
    pub codec: u8,
    pub c0: String,
    pub ts: f64,
}

/// One append to one `window` log.
#[derive(Debug, Clone, PartialEq)]
pub enum LogEntry {
    Tile(TileRecord),
    Codec(u8),
    Flag(u8),
    Resize(ResizeRecord),
    FramePaint(FramePaint),
    PaletteWrite(PaletteWrite),
    TilePush(TilePush),
}
```

- [ ] **Step 2: Write `Recorder`.**

```rust
/// What the hooks keep, and what the getters read.
#[derive(Debug, Default)]
pub struct Recorder {
    pub health: Health,
    pub raf_ticks: u32,
    pub tile_datagrams: u32,
    pub frame_datagrams: u32,
    pub stats_samples: u32,
    pub stats_frames: u32,
    pub cdf53_dispatch_seen: u32,
    pub cdf53_pushed_to_queue: u32,
    /// Read by the stats line only (not published: no e2e reader).
    pub cdf53_prevalidate_fails: u32,
    pub cdf53_last_fail_code: Option<u8>,
    /// The framebuffer size the server last announced (applied).
    fb: (u32, u32),
    first_recv_ms: BTreeMap<u32, f64>,
    first_recv_order: std::collections::VecDeque<u32>,
    latest_tile_frame_seq: u32,
    pending_frame_paint: BTreeSet<u32>,
    /// `true` under `?e2e`: the solid-colour tallies (main.ts gated them).
    pub diagnostics: bool,
}
```

  `impl Recorder` has `pub fn apply(&mut self, call: HookCall, out: &mut Vec<LogEntry>)`. It does what each hook method of `hooks.ts` did:

  - **`Events { at_ms, records }`.** For each `Routed`, do the `recordProtocolEvent` part first, for `TilePayload` and `DecodeError`:
    - Push a `TilePush`. For a tile, use the codec discriminant of `ghostframe_protocol::protocol::Codec` for the slim data's codec, never a literal, and the hex prefix of the slim sample bytes (up to 8, or `LEN=<len>` when there are none). For a decode error, use `seq: -1` and `c0: "ERR:<code>"`.
    - A Cdf53 tile bumps `cdf53_dispatch_seen` and `cdf53_pushed_to_queue`.
    - A decode error whose codec is `Codec::Cdf53` bumps `cdf53_dispatch_seen`. If its code is one of the three CDF53 `DecodeErrorCode`s (from `ghostframe_client_core::DecodeErrorCode`, compared to the enum, never 8/9/10), it also bumps `cdf53_prevalidate_fails` and sets `cdf53_last_fail_code`.

    Then the per-kind bookkeeping of `handleRecord`:
    - **TilePayload:**
      - an `error` gives `health.record("renderer.apply", format!("tile ({x},{y}) seq {s}: {e}"))`;
      - a Solid whose `bytes[2] > 0x80` gives `health.mark(x, y, seq, "solid-red")`;
      - under `diagnostics`, a Solid gives `health.tally(format!("solid:{hex}"))`;
      - then `note_frame_tile_received(seq, at_ms)`;
      - then push `Codec(codec)`, `Flag(wire_flags)` for PalRle only, and `Tile(TileRecord{…, fb_width: self.fb.0, fb_height: self.fb.1, first_recv_ms_client, last_paint_ms_client: at_ms })`. The order is codecs, flags, tiles, as `recordTile` did.
    - **PaletteUpdated:**
      - an `error` gives `health.record("renderer.palette", format!("palette {id}: {e}"))`;
      - push `PaletteWrite`, with `fp` from `ghostframe_protocol::codec::pal_rle::palette_fingerprint_bgra` over the colours flattened as BGRA. That is the function `constants.rs`'s `paletteFingerprint` wraps; confirm the path with grep.
    - **FrameDimensions with `applied`:** push `Resize(ResizeRecord { seq: 0, old: self.fb, new: (w, h), trigger: "sentinel" })` and set `self.fb`.
  - **`DatagramCounts { tile, frame }`:** set the two counters.
  - **`RafTick(n)`:** set `raf_ticks`.
  - **`BeforePaint { at_ms }`:** for every pending frame, ascending, push `FramePaint { seq, raf_ms_client: at_ms }` and forget its `first_recv_ms`; then clear the pending set.
  - **`AfterPaint`:** nothing here. The stats line handles it (Task 4).
  - **`Swallowed { site, message }`:** `health.record(site, message)`.
  - **`RendererError { tick, message }`:** `health.record("renderer", format!("tick {tick}: {message}"))` and `health.tally(format!("gpu:{}", first 80 chars))`. Truncate by chars, not bytes.
  - **`ServerStream { samples, frames }`:** set the two counters.

  `note_frame_tile_received` is the TS function with the same names and the same `MAX_PENDING_FRAME_STAMPS = 1024` bound:
  - A frame two or more sequences behind `latest_tile_frame_seq` returns `at_ms` and records nothing.
  - On first sight, it stamps the frame. Past the bound, the oldest stamp is forgotten, together with its pending entry.
  - A new latest sequence marks every stamped frame two behind as pending.

  `first_recv_order` keeps insertion order, as the JS `Map` did. Use `saturating_add` and `checked_sub` where the TS relied on JS numbers.

- [ ] **Step 3: Tests.** Build the `Routed` values directly; Task 7 of PR 3 shows how, in `app/session.rs`'s tests. Port the cases below, each test's doc line naming its source:
  1. `the_five_protocol_counters_move_on_a_valid_and_a_malformed_cdf53_tile` (`cdf53_globals.test.ts`).
     - A Cdf53 `TilePayload` makes `dispatch_seen` and `pushed_to_queue` 1.
     - A Cdf53 `DecodeError` with `ERR_CDF53_TRUNCATED` makes `dispatch_seen` 2, `prevalidate_fails` 1 and `last_fail_code` set.
     - Two `TilePush` entries are pushed, the second with `seq: -1` and `c0` starting with `ERR:`.
  2. `a_non_cdf53_decode_error_does_not_count_as_a_cdf53_dispatch`.
  3. `a_tile_records_codec_flag_and_tile_in_order`:
     - a PalRle tile gives `[TilePush, Codec, Flag, Tile]`;
     - a Solid tile gives no `Flag`;
     - the `TileRecord` carries the last applied frame size.
  4. `a_rejected_resize_is_not_recorded_and_an_applied_one_is`, with `old_w`/`old_h` from the previous applied size.
  5. `frame_paints_are_recorded_two_sequences_later_at_the_paint_time` (diagnostics.test "recordFramePainted"). Tiles of seq 1 at t=10 and seq 3 at t=20, then `BeforePaint { at_ms: 30 }`, give exactly one `FramePaint { seq: 1, raf_ms_client: 30 }`.
  6. `pending_frame_stamps_are_bounded`: 2,000 distinct increasing frames, each one sequence apart, never leave more than 1,024 stamps.
  7. `palette_writes_carry_the_server_fingerprint`. `fp` equals `format!("{:08x}", palette_fingerprint_bgra(&bytes))`, and `c0` is the first colour.
  8. `renderer_errors_and_swallowed_errors_reach_health`, including the 80-character truncation of a long multibyte message.
  9. `solid_tallies_only_under_diagnostics`.

- [ ] **Step 4: Run the tests and break the code.**
  - Run `cargo test -p ghostframe-client-wasm --lib app::recorder`. All must pass.
  - Then try each mutation below; each must fail a test. Revert after each.
    - Swap the `Codec` and `Tile` push order.
    - Drop the `+ 2 <=` stale check.
    - Compare the decode-error codec with a literal `5` instead of the enum, after changing the enum's value in a scratch edit.

    The third proves the test uses the constant. If changing the enum is impractical, say so and show instead that the test names `Codec::Cdf53`.

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/recorder.rs
git commit -m "feat(web): the e2e recorder in Rust (cdf53 counters, tile and palette logs, frame paints)"
```

---

### Task 3: Hook calls go to Rust [sonnet]

**Files:**
- Modify `ghostframe-client-wasm/src/app/hooks.rs` and `src/app/client.rs`.
- Delete `src/app/debug_api.rs`, and remove it from `src/app/mod.rs`.

- [ ] **Step 1: Replace the JS target with Rust.**
  - In `hooks.rs`, keep `HookCall` (it now also needs `Clone`). Delete the `Hooks` extern type, `attach`, `records_js` and the JS `dispatch`.
  - Add the trait below. It is the one seam between the client and what consumes its hook calls:

```rust
/// What consumes the client's hook calls: the e2e recorder and the page's
/// stats line. Called with no `App` or renderer borrow held.
pub trait HookSink {
    fn dispatch(&self, calls: Vec<HookCall>);
}
```

- [ ] **Step 2: Rewire `client.rs`.** Replace every use of the JS hooks:
  - the `HOOKS` thread-local, `dispatch_calls`, `hooks_enabled`, the `startClient(hooks)` parameter and `DebugApi`.

  Use these instead:
  - **`thread_local! { static SINK: RefCell<Option<Rc<dyn HookSink>>> }`.** It is set once by `start_client`, the one global, documented as `HOOKS` was.
  - **`startClient()`** takes no argument.
  - **`App` queues hook calls always.** The stats line needs `AfterPaint` in production. The sink drops what it does not need. `AppParts::hooks_enabled` goes; the browser test that built `AppParts` is updated to match.
  - **`App::swallowed`.** Without `?e2e` (no recorder), it logs `console.error("handleEvent failed; continuing", site, message)` as well as queueing the call, so a production error is not silent.
  - **Everything `hooks.ts`'s `attach` installed** is now installed by `debug_hooks::install` (Task 7) under `flags.e2e`. That covers the `window` functions over the renderer, the core, the tick samples and the recorder. Until Task 7 lands, add a `todo!()`-free stub, `fn install_debug_hooks(..) {}`, with a comment naming Task 7. This keeps the crate compiling.

  The sink built in `start_client` is a `PageSink` in `client.rs`:
  - It owns `Option<Rc<RefCell<Recorder>>>`, which is `Some` under `?e2e`, plus `Rc<RefCell<StatsLine>>` and what the stats line reads: the renderer and core `Rc`s, the `PageLog`, and the `?e2e` readback flag.
  - Its `dispatch` hands every call to the recorder, pushing the resulting `LogEntry`s to `debug_hooks::append` (a no-op stub until Task 7). It hands `AfterPaint` to the stats line (a no-op until Task 4).
  - Each borrow is a `try_borrow_mut`. On a conflict it drops the call and logs once.

- [ ] **Step 3: Delete `src/app/debug_api.rs`.** Its functions move into `debug_hooks.rs` in Task 7.

- [ ] **Step 4: Run the gates.**

```bash
cargo test -p ghostframe-client-wasm --lib 2>&1 | grep 'test result'; echo "rc=${PIPESTATUS[0]}"
cargo clippy -p ghostframe-client-wasm --all-targets -- -D warnings 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
just lint-web 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
```
Expected: rc=0 for each. `main.ts`/`hooks.ts` now call a `startClient` with the wrong arity, and `DebugApi` no longer exists, so `just build-web` (npm) is expected to break until Task 8 replaces it. Say so in the report; do not fix it in TS.

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-client-wasm/src/app ghostframe-client-wasm/tests/app_client.rs
git commit -m "refactor(web): hook calls go to a Rust sink; startClient takes no hooks"
```

  `git add` of the `app` directory also stages the deletion; read `git status` first.

---

### Task 4: The stats line in Rust [sonnet]

**Files:**
- Create `ghostframe-client-wasm/src/app/stats_line.rs`.
- Modify `src/app/mod.rs` and `src/app/client.rs` (`PageSink`'s `AfterPaint`).

- [ ] **Step 1: Port `stats_log.ts`.** Read it on master. The pure part goes in `stats_line.rs`:

```rust
/// What one `AfterPaint` tells the stats line, plus what it reads elsewhere.
pub struct StatsInputs<'a> {
    pub now_ms: f64,
    pub drained: Drained,
    pub raf_ticks: u32,
    pub submit_count: u32,
    pub fb: (u32, u32),
    pub cdf53_fails: u32,
    pub cdf53_last: Option<u8>,
    pub nack_sent: u32,
    /// `core.cdf53_coverage_summary()`'s preformatted line and the two counts.
    pub coverage_line: &'a str,
    pub partial: u64,
    pub gave_up: u64,
    /// The worst incomplete tiles (only read when `partial` or `gave_up`).
    pub incomplete: &'a [(u8, u8, u16, u32)], // (tile_x, tile_y, missing, sweep_attempts)
}

/// The ~2 s page-log summary. `on_tick` returns the lines to log, or `None`
/// when the interval has not passed or nothing a reader cares about changed.
#[derive(Debug, Default)]
pub struct StatsLine { /* drain totals, last totals, last_ms, last_key */ }
```

  The line strings must be exactly the TS ones:
  - `stats: Δdrain{s:… p:… c:… h:…} fb:WxH cdf53fails:N(last=…) raf:N submit:N`, where `last` is `-` when there is none;
  - `cdf53-coverage: <line>`;
  - the optional `cdf53-incomplete: (x,y) missing=0b<bits padded to CDF53_PASS_COUNT> sweeps=N ...`, where the pass count is the protocol constant;
  - `nack_sent=N`.

  The behaviour rules:
  - **Interval:** the line fires only when more than `STATS_INTERVAL_MS = 2000` has passed since the last one.
  - **Dedup key:** drains, fb, fails and nacks. `raf` is excluded.
  - **When suppressed:** an unchanged key returns `None`.

  Host tests:
  - `nothing_before_two_seconds`;
  - `the_first_line_after_two_seconds_names_the_drain_since_the_last`;
  - `an_unchanged_line_is_suppressed`;
  - `incomplete_tiles_are_listed_with_their_missing_mask` (with a `missing` of `0b101` and a 5-pass count, the text is `missing=0b00101`; read the real pass count from the protocol constant in the test, too);
  - `a_missing_last_fail_code_prints_a_dash`.

- [ ] **Step 2: Hook it up in `PageSink`.**
  - **On `AfterPaint`:** gather `StatsInputs` with `try_borrow` on the renderer (`fb_width`, `fb_height`, `submit_count`) and the core (`cdf53_coverage_summary()`, `cdf53_incomplete_tiles(8)`). Read the fail counters from the recorder when it exists, else zeros. Log each returned line through `PageLog`.
  - **The framebuffer probe:** under `?e2e`, when lines were logged, start the four-point probe of `stats_log.ts`: points (16,16), (960,540), (768,992), (1900,1060), each clamped to the framebuffer. Use `WebRenderer::read_pixel_rect` and log `fb-pixels: tl(16,16)=[r,g,b,a] ...` when all four resolve, or `fb-pixels readback failed: <e>`.

  Check the coverage summary's field names with grep in `ghostframe-client-core`.

- [ ] **Step 3: Gates.** Run the host tests and both clippy runs, as in Task 3. Mutation: drop the dedup check. `an_unchanged_line_is_suppressed` must fail. Revert.

- [ ] **Step 4: Commit.**

```bash
git add ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/stats_line.rs ghostframe-client-wasm/src/app/client.rs
git commit -m "feat(web): the page's ~2 s stats line in Rust"
```

---

### Task 5: Port the vitest cases [haiku]

**Files:** none new. This task is a check-and-record.

- [ ] **Step 1: Check every case is covered.** For each `it(...)` of `ghostframe-web-client/tests/{cdf53_globals,diagnostics,health}.test.ts`, find the Rust test from Tasks 1, 2 or 4 that covers it. Write the mapping into the task report as a table: TS case, Rust test. If a case is not covered, add the test to the right module and say so.

  The one browser behaviour, "mirrors console.error and console.warn into the ring", is Task 7's test. List it as such.

- [ ] **Step 2: Commit.** Only if tests were added: `git add` the changed `src/app/*.rs` by name, then commit with the message `test(web): cover the remaining vitest cases in Rust`.

---

### Task 6: The `index.html` template and the build script [sonnet]

**Files:**
- Create `ci/build-web.sh` (MIT OR Apache-2.0; the SPDX line goes after the shebang).
- Rewrite `ghostframe-web-client/index.html`.
- Modify the `Justfile` (`build-web`, plus a new `web-tools` recipe) and the root `.gitignore`.

- [ ] **Step 1: The template.** In `ghostframe-web-client/index.html`, replace `<script type="module" src="/src/main.ts"></script>` with the inline module below. Keep the rest of the page unchanged.

```html
    <script type="module">
      // Built by ci/build-web.sh, which replaces the two @...@ names with the
      // content-hashed files it writes under assets/.
      import init, { startClient } from './assets/@GF_JS@';
      try {
        await init({ module_or_path: new URL('./assets/@GF_WASM@', import.meta.url) });
      } catch (e) {
        const status = document.getElementById('status');
        if (status) status.textContent = `Error: ${e instanceof Error ? e.message : e}`;
        throw e;
      }
      // Errors from here on are shown on the page by the client itself.
      startClient();
    </script>
```

- [ ] **Step 2: Write `ci/build-web.sh`.**

```bash
#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Builds the browser client into ghostframe-web-client/dist/ (ghostbridge
# //go:embeds it): cargo builds the wasm, the wasm-bindgen CLI writes the JS
# glue, wasm-opt optimises (the flags wasm-pack 0.15.0 used), and both files
# are named by content hash so a browser never runs a stale pair. One script
# for `just build-web` and every CI job. No Node.
#
# Tools: wasm-bindgen-cli at exactly the version in Cargo.lock, binaryen
# version_117. `just web-tools` installs both into target/web-tools/; a
# wasm-bindgen / wasm-opt on PATH is used when it is the right version.
set -euo pipefail
cd "$(dirname "$0")/.."

BINARYEN_VERSION=117
want_bindgen=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)
tools=target/web-tools
PATH="$PWD/$tools/bin:$PWD/$tools/binaryen-version_${BINARYEN_VERSION}/bin:$PATH"

have_bindgen=$(wasm-bindgen --version 2>/dev/null | awk '{print $2}' || true)
if [[ "$have_bindgen" != "$want_bindgen" ]]; then
    echo "error: wasm-bindgen CLI is '${have_bindgen:-missing}', Cargo.lock has $want_bindgen; run: just web-tools" >&2
    exit 1
fi
have_opt=$(wasm-opt --version 2>/dev/null | sed -n 's/.*version \([0-9]*\).*/\1/p' || true)
if [[ "$have_opt" != "$BINARYEN_VERSION" ]]; then
    echo "error: wasm-opt is '${have_opt:-missing}', want binaryen version_$BINARYEN_VERSION; run: just web-tools" >&2
    exit 1
fi

cargo build -p ghostframe-client-wasm --target wasm32-unknown-unknown --release
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
wasm-bindgen --target web --no-typescript --out-dir "$work" \
    target/wasm32-unknown-unknown/release/ghostframe_client_wasm.wasm
wasm-opt -O "$work/ghostframe_client_wasm_bg.wasm" -o "$work/opt.wasm"

hash=$(cat "$work/opt.wasm" "$work/ghostframe_client_wasm.js" | sha256sum | cut -c1-12)
js="ghostframe_client_wasm-$hash.js"
wasm="ghostframe_client_wasm_bg-$hash.wasm"
dist=ghostframe-web-client/dist
rm -rf "$dist"
mkdir -p "$dist/assets"
cp "$work/ghostframe_client_wasm.js" "$dist/assets/$js"
cp "$work/opt.wasm" "$dist/assets/$wasm"
sed -e "s/@GF_JS@/$js/" -e "s/@GF_WASM@/$wasm/" ghostframe-web-client/index.html > "$dist/index.html"
if grep -q '@GF_' "$dist/index.html"; then
    echo "error: a placeholder survived in $dist/index.html" >&2
    exit 1
fi
echo "built $dist: $js, $wasm ($(stat -c %s "$dist/assets/$wasm") bytes)"
```

  **The `wasm-opt` flags must match what wasm-pack ran.** Read wasm-pack 0.15.0's source or its verbose output (`wasm-pack build --log-level info ...` on master) to confirm what it passes: `-O` alone, or with feature flags.
  - If wasm-opt rejects the module, the fix is to pass the feature flags wasm-pack passes, not `--all-features`. A typical rejection is a validation error about bulk memory or reference types, because Rust 1.96's wasm32 target enables them.
  - Report the exact command.

- [ ] **Step 3: The `Justfile`.** Replace `build-web` and add `web-tools`:

```make
# Builds the browser client into ghostframe-web-client/dist/ (ghostbridge
# //go:embeds it at compile time): ci/build-web.sh, shared with CI. Needs the
# pinned tools: `just web-tools` once.
build-web:
    ci/build-web.sh

# Installs the web build's pinned tools into target/web-tools/ (repo-local,
# nothing in $HOME): wasm-bindgen-cli at Cargo.lock's version and binaryen
# version_117 (the wasm-opt wasm-pack 0.15.0 used).
web-tools:
    #!/usr/bin/env bash
    set -euo pipefail
    v=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)
    cargo install --root target/web-tools wasm-bindgen-cli --version "$v" --locked
    b=117
    if [[ ! -x target/web-tools/binaryen-version_$b/bin/wasm-opt ]]; then
        curl -fsSL "https://github.com/WebAssembly/binaryen/releases/download/version_$b/binaryen-version_$b-x86_64-linux.tar.gz" \
            | tar -xz -C target/web-tools
    fi
```

  Also in the `Justfile`:
  - `ci-local` drops `cd ghostframe-web-client && npx tsc --noEmit`.
  - Every other recipe that runs `npm`/`npx` loses that line. Find them with `grep -n 'npm\|npx' Justfile`.

  The root `.gitignore` has no rule for `target/web-tools`; it is covered by `target/`, so confirm that with `git check-ignore target/web-tools`.

- [ ] **Step 4: Build.** The TS still exists at this point, but the new build never reads it.

```bash
export TMPDIR=~/work/gf-tmp-webr
just web-tools > ~/work/gf-nonpm-tools.log 2>&1; echo "rc=$?"
just build-web > ~/work/gf-nonpm-build1.log 2>&1; echo "rc=$?"; tail -2 ~/work/gf-nonpm-build1.log
ls -l ghostframe-web-client/dist ghostframe-web-client/dist/assets; ci/check-wasm-size.sh; echo "rc=$?"
grep -c 'startClient' ghostframe-web-client/dist/index.html
```
Expected: rc=0 for each, and one `.js` and one `.wasm` under `assets/`. `just web-tools` downloads from GitHub and crates.io: report if the network refuses.

  Report:
  - the raw and gzip sizes against Task 0's;
  - the exact `wasm-opt` command and the flags you confirmed wasm-pack used.

  **G1** applies if the raw size moved by more than 2%. At this point the exports are unchanged, so a different optimisation is the only explanation.

- [ ] **Step 5: Commit.**

```bash
git add ci/build-web.sh ghostframe-web-client/index.html Justfile
git commit -m "build(web): build the browser client with cargo, wasm-bindgen and wasm-opt (no npm)"
```

---

### Task 7: `debug_hooks.rs`: the `window` globals under `?e2e` [sonnet]

**Files:**
- Create `ghostframe-client-wasm/src/app/debug_hooks.rs` (wasm32) and `tests/debug_hooks.rs` (browser).
- Modify `src/app/mod.rs` and `src/app/client.rs` (replace the Task 3 stubs).

- [ ] **Step 1: Write `debug_hooks.rs`.**

  `pub fn install(window: &web_sys::Window, h: DebugHandles) -> Logs`, where:
  - `DebugHandles` holds `Rc`s of the renderer, `Rc<OnceCell<Rc<RefCell<ClientCore>>>>`, the recorder, and the tick samples;
  - `Logs` owns one `JsLog` per log.

  `JsLog { array: js_sys::Array, count: CappedCount }` has `push(&self, v: &JsValue)`. It pushes `v`, and on `count.push() == Some(n)` it calls `array.splice(0, n, ...)` (js-sys `Array::splice`). It never reads `length`.

  `pub fn append(logs: &Logs, entries: Vec<LogEntry>)` serialises each entry with `serde_wasm_bindgen` (the plain serializer; every struct is flat) into its log:
  - `Codec` and `Flag` go in as plain numbers;
  - `Tile` goes to `__ghostframeRecordedTiles`;
  - `Codec` to `__ghostframeRecordedCodecs`;
  - `Flag` to `__ghostframeRecordedFlags`;
  - `Resize` to `__ghostframeRecordedResizes`;
  - `FramePaint` to `__ghostframe_framePaints`;
  - `PaletteWrite` to `__h2_clientPaletteWrites`;
  - `TilePush` to `__h5_tilePushLog`.

  Each array is created by `install`, set on `window` once with `Reflect::set`, and never replaced.

  `install` also defines the rest. The getters use `Object.defineProperty(window, name, { get, configurable: true })`, through `js_sys::Object::define_property` with a descriptor object whose `get` is a Rust `Closure`. The functions are `Closure`-backed `Function`s set with `Reflect::set`. All closures live for the page (`Closure::forget`; comment why).
  - **Getters over the recorder:**
    - `__ghostframeStats` gives `{ tileDatagrams, frameDatagrams }`;
    - `__ghostframeRafTicks`, `__ghostframeStatsSamples` and `__ghostframeStatsFrames` give numbers;
    - `__cdf53DispatchSeen` and `__cdf53PushedToQueue` give numbers;
    - `__ghostframeRenderer` gives `renderer.debug_handles()`, or `undefined` while busy.
  - **Functions:**
    - `__ghostframeHealth()` gives `health.snapshot()` serialised;
    - `__ghostframeRendererState()` and `__ghostframeTile(x, y)` give the renderer's `debug_state`/`debug_tile`;
    - `__ghostframePaintUs()` gives `summarize_us` of the renderer's paint samples;
    - `__ghostframeTickUs()` gives `summarize_us` of the tick samples, as `Array`s of numbers;
    - `__cdf53CoverageSummary()` gives `{ summary, incomplete }`, using the coverage helpers `DebugApi` used (`crate::core::cdf53_coverage_js` / `cdf53_incomplete_js` with limit 16, or their replacements after Task 9), and `undefined` before the core exists;
    - `__readPixel(x, y)` gives a `Promise<number[]>` of `[r, g, b, a]`;
    - `__readPixelRect(x, y, w, h)` gives a `Promise<number[]>`, and `[]` when `w` or `h` is 0;
    - `__readGradientGolden()` gives the renderer's promise.

    Every borrow is a `try_borrow`. A conflict gives a rejected promise or a thrown JS error naming the global, never a panic.
  - **The console ring:** `install_console_ring(recorder)`, as described in the Findings. Wrap `log`, `info`, `warn` and `error` in the `console` object with the JS wrapper. `note` is a Rust closure that calls `recorder.health.note(format!("{level}: {line}"))` with `try_borrow_mut`, and does nothing on a conflict.

  Name each global with a `const` and list them all in one `pub const E2E_GLOBALS: &[&str]`, which the test iterates.

- [ ] **Step 2: Wire it in `client.rs`.**
  - **Under `flags.e2e`:** call `install` right after the renderer is created, at the point where `hooks.attach` used to run, and before bootstrap, so failure dumps still find `__ghostframeHealth` if bootstrap fails. Also install the console ring as the very first step of `start_client`.
  - **Pass the `Logs`** to `PageSink`, which calls `append`.
  - **Without `?e2e`:** nothing is installed and no recorder exists.

- [ ] **Step 3: Write the browser test `tests/debug_hooks.rs`.**
  - `every_e2e_global_exists_with_its_shape`:
    - Build a `WebRenderer` on a test canvas, a recorder, a core and the tick samples, and call `install`.
    - Every name in `E2E_GLOBALS` is defined on `window`.
    - The seven logs are arrays.
    - `__ghostframeStats` has the two numeric fields.
    - Calling `__ghostframeHealth()` returns an object with `swallowed`.
    - `__readPixelRect(0, 0, 0, 0)` resolves to `[]`.
  - `appends_reach_the_window_arrays_and_trim_at_twice_the_cap`:
    - Append two `LogEntry::Codec`; `window.__ghostframeRecordedCodecs` reads `[c1, c2]`.
    - With a test-only small-cap `JsLog`, push to twice the cap and check the length falls back to the cap with the newest last.
  - `the_console_ring_mirrors_warn_and_error`: the port of the health.test case. After `console.warn("x")`, `__ghostframeHealth().console` ends with `warn: x`.

  Install mutates the page's `window` and `console`, so the test file runs these in one page in order. Keep each test independent of the others' entries: compare deltas, not absolute lengths.

- [ ] **Step 4: Gates.** Run the host tests, clippy, and `just lint-web`. These are the browser tests' compile check.

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/debug_hooks.rs ghostframe-client-wasm/src/app/client.rs ghostframe-client-wasm/tests/debug_hooks.rs
git commit -m "feat(web): the e2e window globals served from Rust under ?e2e"
```

---

### Task 8: Delete the TypeScript and npm [haiku]

**Files:**
- Delete `ghostframe-web-client/src/`, `tests/` and `scripts/`.
- Delete `ghostframe-web-client/package.json`, `package-lock.json`, `tsconfig.json`, `tsconfig.test.json`, `vite.config.ts` and `vitest.config.ts`.
- Modify `ghostframe-web-client/.gitignore` (drop `pkg-web/` and `pkg-node/`, keep `dist/` and add `node_modules/` removal notes if any).

- [ ] **Step 1: Delete them with `git rm -r`.** Then remove any untracked `ghostframe-web-client/node_modules`, `pkg-web` and `pkg-node` left in the worktree (`rm -rf`, since these are build output, not tracked).
- [ ] **Step 2: Check.**

```bash
git ls-files ghostframe-web-client
git ls-files ghostframe-web-client | grep -E '\.(ts|mts|mjs|json)$'; echo "rc=$?"
```
Expected: the first lists only `index.html` and `.gitignore`. The second prints nothing, with rc=1.

- [ ] **Step 3: Rebuild.** Run `just build-web` and expect rc=0. The new build does not need the deleted files: this proves it.
- [ ] **Step 4: Commit.**

```bash
git add -u ghostframe-web-client
git commit -m "chore(web): delete the TypeScript, vitest and the npm build"
```

---

### Task 9: Remove the JS-facing exports nothing calls [sonnet]

**Files:**
- Delete `ghostframe-client-wasm/src/units.rs`, `src/constants.rs` and `src/panel.rs`.
- Modify `src/lib.rs`, `src/core.rs`, `src/web/renderer.rs`, `src/boundary.rs` and `src/app/client.rs` (its `units` import).

- [ ] **Step 1: List the exports and their callers.** These are every `#[wasm_bindgen]` item in `ghostframe-client-wasm/src`:

```bash
grep -rn -B1 -A2 '#\[wasm_bindgen' ghostframe-client-wasm/src | grep -E 'pub (async )?fn|pub struct|js_name' | sort -u
```

  For each one, check whether it is called from any of these:
  - `ghostframe-web-client/index.html` (only `default`/`init` and `startClient`);
  - Rust in this crate;
  - `ghostframe-client-wasm/tests/*.rs`;
  - `ghostframe-web-ui`.

  The table in the task report, with one row per export, is the evidence: export, caller or "none", kept or removed.

- [ ] **Step 2: Remove what has no caller.** Expected removals, each to be confirmed by Step 1:
  - all of `units.rs`. Move `codec_from_u8` and `decode_error_code_from_u8` to `app/client.rs` or `boundary.rs` first; `app/client.rs` uses them.
  - `constants.rs`. It holds the JS constant getters, `errorCodes`, `codecs` and `paletteFingerprint`; the recorder calls the protocol function directly.
  - `panel.rs` (`mountStatsPanel`; the client calls `ghostframe_web_ui::mount_stats_panel`).
  - `WasmClientCore`. Keep its constructor logic as the free functions `client_config` / `new_client_core` / `feed_datagrams` (already there). PR 3's moved core tests that drive `WasmClientCore::h264_result` and `request_reset` switch to `ClientCore` with the same outcome values. Keep each test's name and assertions; only the entry point changes.
  - `WebRenderer`'s `#[wasm_bindgen]` methods that only `hooks.ts` called. Keep `create`, `read_pixel_rect` and anything the browser tests or `debug_hooks.rs` call. They are callable as plain Rust methods either way; only the `#[wasm_bindgen]` attribute goes where it is unused.
  - `boundary.rs` items left without a user (`WasmPollOutput`, the `Wasm*` mirrors only `units.rs` used). `WasmEvent` stays: `web::route` uses it.

  Keep `protocol_stamp`/`clientVersion` only if something calls them. Otherwise delete them, and with them the stamp half of `build.rs` if nothing else reads `GHOSTFRAME_PROTOCOL_STAMP`. Confirm with grep. `CLIENT_VERSION` stays: the stats panel uses it.

- [ ] **Step 3: Gates.**

```bash
cargo test -p ghostframe-client-wasm --lib 2>&1 | grep 'test result'; echo "rc=${PIPESTATUS[0]}"
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
just lint-web 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
just build-web > ~/work/gf-nonpm-build2.log 2>&1; echo "rc=$?"; ci/check-wasm-size.sh
```
Expected: rc=0 for each, and the host test count no lower than Task 0's minus the deleted `units.rs` tests (list those). Report the new wasm size against Task 6's.

- [ ] **Step 4: Commit.** Stage the deleted and modified files by name. The message is `refactor(web): remove the JS exports only the deleted TypeScript called`.

---

### Task 10: CI without Node [sonnet]

**Files:**
- Modify `.github/workflows/_build-web-client/action.yml`, `ci.yml`, `e2e.yml`, `nightly.yml`, `client-core.yml` and `.github/dependabot.yml`.

- [ ] **Step 1: The composite action.** `_build-web-client/action.yml`:
  - Keep the Rust toolchain step, with the `wasm32-unknown-unknown` target.
  - Replace wasm-pack, setup-node and npm with:
    - a cache of `target/web-tools`, keyed by `web-tools-<wasm-bindgen version>-binaryen117-${{ runner.os }}`;
    - an install step that runs the same commands as `just web-tools` when the cache misses. CI has no `just`, so move those commands into `ci/install-web-tools.sh` and have `just web-tools` call that script too: one source;
    - `ci/build-web.sh`.
  - Update the action's description: no Node, the two tools, and the script.
- [ ] **Step 2: `ci.yml`'s `web-client` job.** Same replacement. Drop the `tsc` step. Keep `ci/test-check-wasm-size.sh`, `ci/check-wasm-size.sh` and the `web-client-dist` artifact upload.
- [ ] **Step 3: `e2e.yml`'s fallback build** (around lines 238-272). Drop `setup-node` and wasm-pack. The fallback, which runs when the artifact is missing, calls the composite action, guarded by the same `if: hashFiles('ghostframe-web-client/dist/**') == ''`. Keep the artifact download.
- [ ] **Step 4: `nightly.yml`.** Same replacement as Step 1, without any npm cache.
- [ ] **Step 5: `client-core.yml`.** Delete the `wasm-module` job: it built `pkg-node` and ran vitest, and both are gone. If a step in it did anything else, such as building the browser-target wasm, check that the `web-renderer` job, which builds the crate for wasm32, or the composite action already covers it, and say which. `wasm-pack test` stays in the `web-ui` and `web-renderer` jobs.
- [ ] **Step 6: `dependabot.yml`.** Remove the npm ecosystem entry.
- [ ] **Step 7: Check.**

```bash
grep -rn 'npm\|npx\|setup-node\|node-version\|package-lock\|vitest\|vite\b' .github; echo "rc=$?"
```
Expected rc=1 (no matches). `wasm-pack` may remain only in the `wasm-pack test` steps. If `actionlint` is installed, run it on `.github/workflows/*.yml` and report its output. Do not install it.

- [ ] **Step 8: Commit.**

```bash
git add .github ci/install-web-tools.sh Justfile
git commit -m "ci(web): build the browser client without Node; drop the vitest job"
```

---

### Task 11: Docs and comments [haiku]

**Files:** `DEVELOPERS.md`, `README.md`, `AGENTS.md`, `docs/ci.md`, `docs/m0-manual-smoke-test.md`, `.gitignore`, `.dockerignore`, `ghostbridge/Makefile`, `ghostframe-tsnet/build.rs`, `tests/containers/test-server/Dockerfile` (comments only), and the comment-only hits in Rust sources.

- [ ] **Step 1: Find them.**

```bash
grep -rn 'npm\|npx\|node_modules\|vite\b\|vitest\|wasm-pack build\|pkg-web\|pkg-node\|main\.ts\|hooks\.ts\|diagnostics\.ts\|stats_log\.ts\|health\.ts\|cdf53_globals\.ts\|protocol_constants\.ts\|Node\.js\|nodejs' \
  --include=*.md --include=*.rs --include=*.go --include=*.yml --include=Makefile --include=Dockerfile --include=.gitignore --include=.dockerignore . \
  | grep -v '^./target\|^./docs/superpowers\|^./docs/specs\|^./docs/reviews'
```

- [ ] **Step 2: Rewrite each hit that describes how things work now.**
  - **DEVELOPERS.md:** prerequisites become `just web-tools` (wasm-bindgen-cli and binaryen, repo-local). wasm-pack plus Chrome stay for the browser tests. There is no Node.
  - **AGENTS.md:**
    - the `build-web` line says `ci/build-web.sh`;
    - the wasm gate's `dist/assets/*.wasm` stays true;
    - the "Client-side diagnostics" paragraph: the `rx{...}` reading of the dead `__tileCounts` is gone (the stats line is Rust now), and the e2e globals exist only under `?e2e`;
    - the `just ci-local` summary loses `tsc`.
  - **ghostbridge/Makefile:** the hint `cd ghostframe-web-client && npm ci && npm run build` becomes `just build-web (or ci/build-web.sh)`.
  - **`.dockerignore` and the Dockerfile comment:** the host builds `dist/` with `just build-web`.
  - **Rust comments** that name deleted TS files as the current behaviour are repointed at the Rust module. Port-provenance remarks ("port of `stats_log.ts`") may stay.

  History stays untouched in `docs/specs`, `docs/superpowers` and `docs/reviews`.

- [ ] **Step 3: Update the memory-derived trap** in `AGENTS.md`, if present, that says fresh worktrees need `npm install && npm run build`. It is now `just build-web`, which needs `just web-tools` once.
- [ ] **Step 4: Commit,** staging the files by name. The message is `docs(web): no Node: build-web, web-tools, and the e2e globals under ?e2e`.

---

### Task 12: Final review [opus]

**Files:** none changed by the reviewer.

Review the whole branch, `579de2ac..HEAD`, against this plan and spec §5.5, §5.6, §6 (PR 4 and PR 5 rows) and §8:

- **Every global in the Findings table is installed under `?e2e`.**
  - Each has the old name and JSON shape: field names, camelCase, `seq: -1` for decode errors, `trigger: "sentinel"`, numbers rather than strings.
  - None is installed without `?e2e`.
  - Check each against the TS on master and against its e2e reader (grep in `ghostframe-e2e`).
- **No `RefCell` borrow is held across a hook dispatch, an `.await` or a JS call that can re-enter.** The getters, the console ring and the stats line's readback promise are the new cases.
- **The logs are JS arrays appended to; nothing re-serialises a whole log per read.** Trimming happens at twice the cap.
- **The build.**
  - Tool versions are checked and fail with an actionable message.
  - The `wasm-opt` flags match what wasm-pack ran.
  - The hashed names, and no `@GF_` left in `dist/index.html`.
  - `ci/check-wasm-size.sh` still finds the wasm.
- **Nothing outside docs history mentions Node, npm, Vite or vitest** (Task 11's grep), and `git ls-files ghostframe-web-client` lists only `index.html` and `.gitignore`.
- **Each removed export had no caller** (Task 9's table).
- **The gates pass:**
  - `cargo test -p ghostframe-client-wasm --lib`;
  - `cargo clippy --workspace --all-targets -- -D warnings`;
  - `just lint-web`;
  - `just build-web`;
  - `ci/check-wasm-size.sh`;
  - `cargo fmt --check`.

  The browser tests and the e2e are Task 13's (controller).

---

### Task 13: The full GPU e2e, CI, and the PR [controller]

Not part of the agent run; the controller runs it.

- [ ] **Step 1: Local gate.** Run `~/work/gf-gpu-run just ci-local`, which includes `test-web-renderer`. On failure, look at the browser tests (`app_*`, `debug_hooks`) first.
- [ ] **Step 2: Build the images.** Run `flock -o ~/work/.gf-heavy.lock just containers-build`. Server-side code did not change, but the image embeds the new `dist/`.
- [ ] **Step 3: The full e2e:**

```bash
flock -o ~/work/.gf-heavy.lock ~/work/gf-gpu-run just test-e2e > ~/work/gf-nonpm-e2e.log 2>&1; echo "rc=$?"
flock -o ~/work/.gf-heavy.lock ~/work/gf-gpu-run just test-e2e-stats > ~/work/gf-nonpm-e2e-stats.log 2>&1; echo "rc=$?"
```

  Then go through every failing test name:
  1. Re-run it once alone (`cargo test --test e2e <name> -- --exact --test-threads=1`) to separate flakes from failures.
  2. Run it on a master worktree at `579de2ac`, built the same way.
  3. A branch-only failure is fixed. If the fix would touch the e2e Rust code, that is **G2**.
- [ ] **Step 4: Prove the two PR 3 feedback-bytes tests can fail.** This is PR 3's open item. Apply each mutation in turn and confirm `a_tile_is_acknowledged_and_the_hooks_see_the_tick_in_order` fails, then revert:
  - delete the `drain_transmit()` call at the end of `App::tick`;
  - skip `PollOutput::Datagram` in `drain_transmit`.
- [ ] **Step 5: Open the PR.** Push, then open the PR with a body covering:
  - the globals table;
  - the two unported globals;
  - the build;
  - the tools;
  - the wasm sizes;
  - the e2e results, as a list of the tests run with pass/fail, and what was fixed;
  - the mutations' results.

  Link it and watch CI.

## Out of scope

- **Replacing `wasm-pack test` with `cargo test` and `wasm-bindgen-test-runner`.** The build no longer uses wasm-pack; the browser tests still do.
- **A size pass on the wasm** beyond Task 9's export removals (the cap is 1 MiB, owner 2026-11-16).
- **tailscale-rs replacing ghostbridge.**
