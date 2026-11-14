# Stats Panel PR 3: Chromium E2E and Wasm Bundle Guard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove the stats panel works on a live session (open the tab, a subscribe goes out, server rows show a non-zero bandwidth estimate and RTT, closing the tab stops the samples), and fail the build when the shipped wasm exceeds 512 KiB.

**Architecture:** One new test in `ghostframe-e2e/tests/e2e.rs` (the `--test e2e` target that `.github/workflows/e2e.yml` already runs by name) drives the real page through CDP: it clicks the panel tab with a real mouse click and reads three live signals: `window.__ghostframeStatsFrames` (every complete server-stream frame the core received, counted before any schema or subscription check), `window.__ghostframeStatsSamples` (samples applied to the rings, schema-checked), and the panel's own DOM rows. The raw frame count is what proves the server stays silent while closed and after close; the applied count proves a schema-checked sample crossed the wire. Neither is derived from the other, so a client that ignores samples cannot make the "server stopped" assertion pass. The bundle guard is `ci/check-wasm-size.sh` (plus a self-test), wired into `just ci-local` and the `web-client` job of `ci.yml`. Sizes before and after the panel are recorded in `docs/specs/wasm-bundle-baseline.md`.

**Tech Stack:** Rust (chromiumoxide 0.9 via `ghostframe-e2e`, `ghostframe-client-core`, `ghostframe-client-wasm`), TypeScript (`ghostframe-web-client/src/main.ts`), Leptos 0.9 in `ghostframe-web-ui` (test hooks only), bash, GitHub Actions, `just`.

**Assumes** PR 1 (protocol and server sampler) and PR 2 (client core stats, `ghostframe-web-ui`, `mountStatsPanel`, `main.ts` feedback-stream reader) are merged to `master`. Spec: `docs/specs/stats-panel-design.md` sections 4, 5, 6 item 3.

**Branch:** `feat/stats-panel-e2e`, in a worktree.

---

## File structure

| File | Create/Modify | Responsibility |
|---|---|---|
| `ghostframe-e2e/tests/e2e.rs` | Modify | New test `e2e_stats_panel_subscribe_samples_unsubscribe` and its helpers (`stats_counters`, `stats_panel_rows`, `click_stats_tab`). Runs in CI through the existing `cargo test --test e2e` step; not skip-listed. |
| `ghostframe-client-core/src/stats.rs` | Modify | `ClientStats::samples_accepted()` (samples applied to the rings, schema matched) and `ClientStats::frames_received()` (every complete server-stream frame, counted on arrival, before any check). Each skipped if PR 2 already has it. |
| `ghostframe-client-core/tests/stats_counters.rs` | Create | Integration test: `samples_accepted` counts applied samples only; `frames_received` counts every frame including schema and rejected ones; both ignore the core's own subscribed flag. Picked up by the `cargo test -p ghostframe-client-core` step of `client-core.yml` without a name (it runs every target of the package). |
| `ghostframe-client-wasm/src/core.rs` | Modify | `statsSamplesAccepted` and `statsFramesReceived` exports. |
| `ghostframe-web-client/src/main.ts` | Modify | Initialise both globals to 0 in e2e mode, and after each `core.handleServerStream(...)` publish `window.__ghostframeStatsSamples` and `window.__ghostframeStatsFrames` (e2e mode only, like the other polled globals). |
| `ghostframe-web-client/src/globals.d.ts` | Modify | Types of the two new globals. |
| `ghostframe-web-ui/src/*.rs` | Modify (hooks only) | `data-testid="stats-tab"`, `data-testid="stats-row"` with `data-label`, `data-current`, `data-stale`. Skipped for any hook PR 2 already has. |
| `ci/check-wasm-size.sh` | Create | Fails if any `.wasm` in `ghostframe-web-client/dist/assets` is over 524,288 bytes (512 KiB), or if none exists. |
| `ci/test-check-wasm-size.sh` | Create | Self-test of the guard: under, at, over the limit, and no wasm at all. |
| `Justfile` | Modify | `ci-local` runs both scripts after `just build-web`; new recipe `test-e2e-stats`. |
| `.github/workflows/ci.yml` | Modify | `web-client` job runs both scripts after `npm run build`. |
| `AGENTS.md` | Modify | One bullet on the wasm size gate and the new live global. |
| `docs/specs/wasm-bundle-baseline.md` | Modify | New section: wasm and JS sizes before and after the panel (raw and gzip), measured by a script. The header's "record, not a gate" paragraph is updated to point at the gate. |
| scratchpad `measure-wasm-baseline.sh` | Create (not in the repo, not committed) | End-to-end script that picks the "before" commit, builds both trees and appends the section. |

No CI workflow line is added for the e2e test itself. `e2e.yml` runs `cargo test --test e2e -- --test-threads=1 <skip args from ci/skip-list.txt>`, so a test inside `e2e.rs` runs unless it is skip-listed. Task 6 proves that with `--list` and a grep of the skip list, rather than assuming it.

---

## Task 1: Worktree, branch, prerequisites

**Model:** haiku

**Files:** none modified.

- [ ] **Step 1: Create the worktree** (REQUIRED SUB-SKILL: superpowers:using-git-worktrees)

```bash
cd ~/work/ghostframe
git fetch origin master
git worktree add ../gf-stats-e2e -b feat/stats-panel-e2e master
cd ../gf-stats-e2e
git log --oneline -3
```

Expected: the branch is created from `master`; the last commits include PR 2's merge.

- [ ] **Step 2: Verify PR 1 and PR 2 are on this base**

```bash
cd ~/work/gf-stats-e2e
test -f ghostframe-protocol/src/stats.rs && echo "protocol stats: ok"
test -f ghostframe-client-core/src/stats.rs && echo "client stats: ok"
test -d ghostframe-web-ui && echo "web-ui: ok"
grep -n "mountStatsPanel" ghostframe-client-wasm/src/*.rs ghostframe-web-client/src/main.ts
grep -n "handleServerStream" ghostframe-web-client/src/main.ts
grep -n "ghostframe-web-ui" tests/containers/test-server/Dockerfile
```

Expected: five "ok"/hit lines. If any line is missing, STOP: this plan assumes both PRs are merged. The Dockerfile grep must hit (`COPY ghostframe-web-ui/Cargo.toml ...`); if it does not, the e2e image build fails before any test runs, and that is PR 2's defect to fix first (spec section 5).

- [ ] **Step 3: Check what PR 2 already provides (decides which of Tasks 3 to 5 are needed)**

```bash
cd ~/work/gf-stats-e2e
grep -n "samples_accepted\|frames_received\|statsSamplesAccepted\|statsFramesReceived\|__ghostframeStatsSamples\|__ghostframeStatsFrames" -r ghostframe-client-core/src ghostframe-client-wasm/src ghostframe-web-client/src ghostframe-web-ui/src || echo "none of them exists"
grep -n "data-testid" -r ghostframe-web-ui/src || echo "no test hooks in web-ui"
```

Write down the answers: Task 3 is skipped per counter that exists (`samples_accepted`, `frames_received`), Task 4 if the exports and the globals exist, Task 5 for each hook already present. The e2e test in Task 2 depends only on the names below, so adapt the names in it (not the other way round) if PR 2 chose different ones, and list the change under "Contract deviations".

- [ ] **Step 4: Build the web client once so the worktree has a `dist/`** (a fresh worktree has none and the e2e fails with "timed out waiting for frame rendering")

```bash
cd ~/work/gf-stats-e2e
just build-web
ls -l ghostframe-web-client/dist/assets/
```

Expected: one `.wasm` and one `.js` file.

- [ ] **Step 5: No commit** (nothing changed). Move on.

---

## Task 2: The e2e test, written first and run red

**Model:** sonnet

**Files:**
- Modify: `ghostframe-e2e/tests/e2e.rs` (insert after the closing `}` of `e2e_input_forwarding_firefox`; anchor on that text, not a line number)

The test uses `setup_e2e_webgpu` (`e2e.rs:235`, a Chromium page against the container server, no GPU needed), exactly as `e2e_input_forwarding_chromium` does at `e2e.rs:5565`, so it runs on the GitHub runner too. `--solid-red --spinner` gives the server a steady stream, so the bandwidth estimate has traffic to move on.

Names the test relies on (each is added to the product in Tasks 3 to 5):

- `window.__ghostframeStatsFrames`: a number, every complete server-stream frame the core received (schema, sample or anything else), counted on arrival before any check. Initialised to 0 by `main.ts` in e2e mode, so a missing global is an error, not a silent 0.
- `window.__ghostframeStatsSamples`: a number, the count of samples the core applied to its rings (schema matched). Also initialised to 0.
- `[data-testid="stats-tab"]`: the clickable tab.
- `[data-testid="stats-row"]` with `data-label` (display label), `data-current` (the raw number, empty for text rows or while stale), `data-stale` (`"true"` or `"false"`).
- Labels: the bandwidth estimate row matches `/bandwidth estimate/i`, the RTT row matches `/round-trip time/i` (PR 1 labels it "Round-trip time").

- [ ] **Step 1: Write the test**

Insert this block:

```rust
// ---------------------------------------------------------------------------
// Stats panel (docs/specs/stats-panel-design.md section 4, "E2E (Chromium)").
// ---------------------------------------------------------------------------

/// The two live stats counters of the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StatsCounters {
    /// Every complete frame the core received on the server stream, counted
    /// before any schema or subscription check. This is what proves the
    /// server is silent: it does not depend on what the client chooses to
    /// apply.
    frames: u64,
    /// Samples applied to the rings (schema and count matched). Non-zero
    /// proves a schema-checked sample crossed the wire.
    samples: u64,
}

/// Reads `window.__ghostframeStatsFrames` / `__ghostframeStatsSamples`,
/// published by `main.ts` after every chunk read from the feedback stream's
/// return half and initialised to 0 at start-up (e2e pages only). No `?? 0`
/// fallback: an absent global is an error, so a broken publish cannot read as
/// "nothing arrived".
async fn stats_counters(setup: &E2eSetup) -> Result<StatsCounters> {
    let json: String = setup
        .page()
        .evaluate(
            "JSON.stringify({frames: window.__ghostframeStatsFrames, \
                             samples: window.__ghostframeStatsSamples})",
        )
        .await?
        .into_value()?;
    let v: serde_json::Value = serde_json::from_str(&json)?;
    Ok(StatsCounters {
        frames: v["frames"]
            .as_u64()
            .context("window.__ghostframeStatsFrames is not a number (main.ts did not publish it)")?,
        samples: v["samples"]
            .as_u64()
            .context("window.__ghostframeStatsSamples is not a number (main.ts did not publish it)")?,
    })
}

/// One `[data-testid="stats-row"]` of the open panel.
struct StatsRow {
    label: String,
    current: Option<f64>,
    stale: bool,
}

/// Every metric row currently in the panel's DOM (empty while it is closed).
async fn stats_panel_rows(setup: &E2eSetup) -> Result<Vec<StatsRow>> {
    let json: String = setup
        .page()
        .evaluate(
            r#"JSON.stringify(
                [...document.querySelectorAll('[data-testid="stats-row"]')].map(r => ({
                    label: r.dataset.label ?? '',
                    current: (r.dataset.current ?? '') === '' ? null : Number(r.dataset.current),
                    stale: r.dataset.stale === 'true',
                }))
            )"#,
        )
        .await?
        .into_value()?;
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json)?;
    Ok(rows
        .iter()
        .map(|r| StatsRow {
            label: r["label"].as_str().unwrap_or_default().to_string(),
            current: r["current"].as_f64(),
            stale: r["stale"].as_bool().unwrap_or(false),
        })
        .collect())
}

/// A real mouse click on the panel's edge tab (CDP scrolls it into view and
/// presses at its centre), not a synthetic `.click()`: the tab cancels
/// `mousedown`'s default to keep focus off itself (spec section 2.3), and only
/// a real press goes through that path.
///
/// Two checks around the click, so a failure names its cause instead of
/// surfacing 15 s later as "no stats sample":
///  - before: `document.elementFromPoint` at the tab's centre is the tab (or a
///    child). A canvas stacked above it would forward the press to the remote
///    session and the panel would never open;
///  - after: `document.activeElement` is not the tab or inside the panel, i.e.
///    keyboard focus did not move onto it (spec 2.3).
async fn click_stats_tab(setup: &E2eSetup) -> Result<()> {
    let tab = setup
        .page()
        .find_element(r#"[data-testid="stats-tab"]"#)
        .await
        .context("the stats panel tab is not in the DOM")?;
    tab.scroll_into_view().await?;
    let hit: String = setup
        .page()
        .evaluate(
            r#"(() => {
                const t = document.querySelector('[data-testid="stats-tab"]');
                const r = t.getBoundingClientRect();
                const e = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
                return (e && (e === t || t.contains(e))) ? 'ok'
                    : 'covered by ' + (e ? e.tagName + '#' + e.id : 'nothing');
            })()"#,
        )
        .await?
        .into_value()?;
    anyhow::ensure!(
        hit == "ok",
        "the stats tab is not what a click at its centre would hit ({hit}): \
         the press would go to the remote session instead"
    );
    tab.click().await?;
    let focus_moved: bool = setup
        .page()
        .evaluate(
            r#"(() => {
                const a = document.activeElement;
                return !!a && !!a.closest('[data-testid="stats-tab"], #stats-panel');
            })()"#,
        )
        .await?
        .into_value()?;
    anyhow::ensure!(
        !focus_moved,
        "clicking the stats tab moved keyboard focus onto the panel: typing would \
         stop reaching the remote session (spec 2.3, mousedown must be cancelled)"
    );
    Ok(())
}

/// The first row whose label matches `needle` (case-insensitive substring).
fn row_with_label<'a>(rows: &'a [StatsRow], needle: &str) -> Option<&'a StatsRow> {
    let needle = needle.to_lowercase();
    rows.iter().find(|r| r.label.to_lowercase().contains(&needle))
}

/// Opening the panel subscribes, the server answers with a schema and one
/// sample a second, the server rows carry real measurements, and closing the
/// panel stops the server.
///
/// What each phase proves, and why it cannot pass vacuously. "Frames" is the
/// raw count of server-stream frames the core received; "samples" is the count
/// it applied. The silence assertions use frames, because a client that drops
/// samples would hold the applied count still while the server kept sending.
///  - closed for 3 s -> frames 0 and samples 0: nothing is sent before a
///    subscribe (spec section 1, "Only while the panel is open"). A server
///    that sends without a subscribe produces a schema-less sample, which the
///    client rejects and which never moves `samples`; only `frames` sees it;
///  - after the click samples becomes non-zero: the subscribe reached the
///    server and a schema-checked sample came back;
///  - the bandwidth-estimate and RTT rows become non-zero and not stale: the
///    measurement itself fired (a row that exists but reads 0 is the failure
///    this guards, the same shape as the dead `window.__*` globals);
///  - samples keep rising while open (a stream, not one burst);
///  - after the second click frames stop: in-flight frames are tolerated and
///    drained first, then the count must hold still for 4 s, more than three
///    sample periods;
///  - re-opening resumes it: unsubscribe is not a terminal state.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_stats_panel_subscribe_samples_unsubscribe() -> Result<()> {
    let setup = setup_e2e_webgpu("--solid-red --spinner").await?;

    // Closed by default: nothing subscribed, so nothing arrives at all.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let before_open = stats_counters(&setup).await?;
    assert_eq!(
        before_open,
        StatsCounters { frames: 0, samples: 0 },
        "server-stream frames arrived with the panel closed: the server sent without a subscribe"
    );

    // Open the panel.
    click_stats_tab(&setup).await?;

    // The subscribe goes out and the first sample comes back.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if stats_counters(&setup).await?.samples > 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no stats sample was applied within 15 s of opening the panel: \
             the subscribe never reached the server, or the server's schema/samples \
             never reached the client core"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // The server rows show real measurements within a few seconds.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let (bandwidth, rtt) = loop {
        let rows = stats_panel_rows(&setup).await?;
        let bw = row_with_label(&rows, "bandwidth estimate")
            .and_then(|r| (!r.stale).then_some(r.current).flatten())
            .filter(|v| *v > 0.0);
        let rtt = row_with_label(&rows, "round-trip time")
            .and_then(|r| (!r.stale).then_some(r.current).flatten())
            .filter(|v| *v > 0.0);
        if let (Some(bw), Some(rtt)) = (bw, rtt) {
            break (bw, rtt);
        }
        if std::time::Instant::now() >= deadline {
            let labels: Vec<String> = rows
                .iter()
                .map(|r| format!("{:?} current={:?} stale={}", r.label, r.current, r.stale))
                .collect();
            panic!(
                "bandwidth estimate / RTT never became non-zero and fresh in 20 s. \
                 bandwidth={bw:?} rtt={rtt:?}. Rows in the panel ({}):\n{}",
                labels.len(),
                labels.join("\n")
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    println!("stats panel: bandwidth estimate {bandwidth}, rtt {rtt}");

    // A stream, not a burst: samples keep arriving while the panel is open.
    let a = stats_counters(&setup).await?;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let b = stats_counters(&setup).await?;
    assert!(
        b.samples >= a.samples + 2,
        "samples did not keep arriving while open: {a:?} then {b:?} after 4 s (1 Hz expected)"
    );

    // Close the panel; the server must go quiet.
    click_stats_tab(&setup).await?;
    // A frame may already be in flight when the unsubscribe is sent.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let settled = stats_counters(&setup).await?;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let after_close = stats_counters(&setup).await?;
    assert_eq!(
        after_close.frames, settled.frames,
        "the server kept sending after the panel was closed ({settled:?} then {after_close:?}): \
         the unsubscribe did not stop it"
    );

    // Re-opening resumes the stream.
    click_stats_tab(&setup).await?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if stats_counters(&setup).await?.samples > after_close.samples {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no sample arrived within 10 s of re-opening the panel"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    setup.browser.close().await
}
```

Note: `setup` is deliberately not `mut`. `ChromiumSession::close(mut self)` (`ghostframe-e2e/src/harness/browser.rs:324`) consumes `setup.browser` by a partial move, so a `mut` binding would be an `unused_mut` warning, which `-D warnings` turns into an error in Step 2 and in `ci-local`. `e2e_input_forwarding_chromium` needs `mut` only because it first passes `&mut setup.browser`. `E2eSetup` has no `Drop` impl, so the partial move compiles (the existing tests do the same).

- [ ] **Step 2: Compile the test target** (clippy is the gate: a test target that does not compile vanishes from `cargo test`)

```bash
cd ~/work/gf-stats-e2e
cargo clippy -p ghostframe-e2e --tests -- -D warnings 2>&1 | tail -20
```

Expected: no warnings or errors. If `clippy::manual_*` or an unused-import warning fires, fix the block, not the lint level.

- [ ] **Step 3: Build the images so the run is against this base** (e2e silently runs a stale image otherwise)

`just containers-build` tags one global image (`ghostframe/test-server:latest`). **No other worktree may run `containers-build` or e2e while Tasks 2, 6 and 10 are in progress** (PR 1 and PR 2 work included): a second build swaps the image under this test, and a mutation run in Task 6 would then test the wrong binary. Check with `docker ps` and `pgrep -af "cargo test --test e2e"` first.

```bash
cd ~/work/gf-stats-e2e
just build-web && just containers-build
```

- [ ] **Step 4: Run it and see it fail, for the right reason**

```bash
cd ~/work/gf-stats-e2e
docker rm -f ghostframe-server headscale >/dev/null 2>&1 || true
cargo test --test e2e e2e_stats_panel_subscribe_samples_unsubscribe -- --exact --test-threads=1 --nocapture 2>&1 | tail -25
```

Expected, when Tasks 3 to 5 are still needed: FAIL, after the closed-panel assertion has passed, either with `window.__ghostframeStatsFrames is not a number` (the global is not published yet; this is the first read, so it can fail before the closed-panel assertion, which is fine) or with `the stats panel tab is not in the DOM` (the hook is missing). A failure at the closed-panel assertion (`server-stream frames arrived with the panel closed`) would mean the server sends without a subscribe: a PR 1 bug, stop and report it. If PR 2 already shipped every hook and the global, the test may pass here; in that case continue to Task 6 and treat its mutation steps as the red proof.

- [ ] **Step 5: Commit**

```bash
cd ~/work/gf-stats-e2e
git add ghostframe-e2e/tests/e2e.rs
git commit -m "test(e2e): stats panel opens, subscribes, shows server rows, unsubscribes

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

## Task 3: `ClientStats::samples_accepted` and `frames_received`

**Model:** sonnet

Two counters, with different jobs. `samples_accepted` counts samples applied to the rings. `frames_received` counts every complete frame the reader yields (schema, sample, unknown type, rejected), incremented the moment a frame is yielded and before any check. The e2e uses `frames_received` to prove the server is silent, so it must not depend on the schema, on the sample being valid, or on the core's own subscribed flag; and `samples_accepted` must apply samples whatever that flag says (a core that ignored samples while it believed itself unsubscribed would make the close assertion measure the client, not the server).

Skip a counter if Task 1 step 3 found it already in `ghostframe-client-core/src/stats.rs`; then only run steps 1, 2 (the test passes at once) and 5 and 6 for it.

**Files:**
- Create: `ghostframe-client-core/tests/stats_counters.rs`
- Modify: `ghostframe-client-core/src/stats.rs` (and `lib.rs` if `handle_server_stream` lives there)

- [ ] **Step 1: Write the failing test**

```rust
//! `ClientStats::samples_accepted` and `frames_received` are the signals the
//! web e2e polls (`window.__ghostframeStatsSamples` / `__ghostframeStatsFrames`).
//! `samples_accepted` counts exactly the samples applied to the rings: not
//! rejected ones, not schema frames, not partial reads. `frames_received`
//! counts every complete frame, before any check, so it moves even when the
//! sample is rejected and whatever the core's subscribed flag says.

use ghostframe_client_core::{ClientConfig, ClientCore};
use ghostframe_protocol::stats::{
    MetricDescriptor, MetricGroup, MetricKind, MetricValue, StatsSample, StatsSchema,
};

fn schema() -> StatsSchema {
    StatsSchema {
        schema_id: 1,
        server_version: "0.1.0 (aaaaaaaa)".to_string(),
        host_name: "host".to_string(),
        metrics: vec![MetricDescriptor {
            id: 1,
            name: "ghostframe.rtt".to_string(),
            unit: "ms".to_string(),
            kind: MetricKind::Gauge,
            group: MetricGroup::Connection,
            label: "RTT".to_string(),
        }],
    }
}

fn sample(schema_id: u16, rtt: f64) -> StatsSample {
    StatsSample {
        schema_id,
        timestamp_us: 1_000_000,
        values: vec![MetricValue::Num(rtt)],
    }
}

fn new_core() -> ClientCore {
    ClientCore::new(ClientConfig::default(), 0)
}

#[test]
fn counts_applied_samples_only() {
    let mut core = new_core();
    assert_eq!(core.stats().samples_accepted(), 0);

    // A sample before any schema is rejected, not accepted.
    core.handle_server_stream(&sample(1, 4.0).encode_frame(), 500_000);
    assert_eq!(core.stats().samples_accepted(), 0);

    core.handle_server_stream(&schema().encode_frame(), 600_000);
    assert_eq!(core.stats().samples_accepted(), 0, "a schema is not a sample");

    core.handle_server_stream(&sample(1, 4.0).encode_frame(), 1_000_000);
    core.handle_server_stream(&sample(1, 5.0).encode_frame(), 2_000_000);
    assert_eq!(core.stats().samples_accepted(), 2);

    // A sample for another schema id is rejected and not counted.
    core.handle_server_stream(&sample(9, 1.0).encode_frame(), 3_000_000);
    assert_eq!(core.stats().samples_accepted(), 2);
    assert_eq!(core.stats().view(3_000_000).samples_rejected, 2);
}

#[test]
fn frames_received_counts_every_frame_before_any_check() {
    let mut core = new_core();
    assert_eq!(core.stats().frames_received(), 0);

    // Rejected (no schema yet): still a received frame. This is the case the
    // e2e needs: a server that sends without a subscribe produces exactly
    // this, and `samples_accepted` never sees it.
    core.handle_server_stream(&sample(1, 4.0).encode_frame(), 500_000);
    assert_eq!(core.stats().frames_received(), 1);
    assert_eq!(core.stats().samples_accepted(), 0);

    core.handle_server_stream(&schema().encode_frame(), 600_000);
    assert_eq!(core.stats().frames_received(), 2, "a schema is a frame");

    core.handle_server_stream(&sample(9, 1.0).encode_frame(), 700_000);
    assert_eq!(core.stats().frames_received(), 3, "a rejected sample is a frame");
}

#[test]
fn counters_ignore_the_cores_own_subscribed_flag() {
    // The core has never been told the panel is open (`set_stats_subscribed`
    // not called), and is then told it is closed: samples are still applied
    // and frames still counted. The e2e's "server went quiet" assertion rests
    // on this: the client must not filter on its own belief.
    let mut core = new_core();
    core.set_stats_subscribed(false);
    core.handle_server_stream(&schema().encode_frame(), 0);
    core.handle_server_stream(&sample(1, 3.0).encode_frame(), 1_000_000);
    assert_eq!(core.stats().frames_received(), 2);
    assert_eq!(core.stats().samples_accepted(), 1);
}

#[test]
fn a_frame_split_across_reads_counts_once() {
    let mut core = new_core();
    core.handle_server_stream(&schema().encode_frame(), 0);

    let frame = sample(1, 7.0).encode_frame();
    let (head, tail) = frame.split_at(frame.len() / 2);
    core.handle_server_stream(head, 1_000_000);
    assert_eq!(core.stats().samples_accepted(), 0, "half a frame is no sample");
    assert_eq!(core.stats().frames_received(), 1, "half a frame is no frame");
    core.handle_server_stream(tail, 1_000_100);
    assert_eq!(core.stats().samples_accepted(), 1);
    assert_eq!(core.stats().frames_received(), 2);
}
```

`samples_rejected` is read as a field of `StatsView` (contract: `StatsView` exposes `samples_rejected`). If PR 2 made it a method, change `.samples_rejected` to `.samples_rejected()`; the integer literal `2` infers whatever integer type it is. The first (pre-schema) sample is the first rejection and the schema-9 one is the second.

- [ ] **Step 2: Run it and see it fail**

```bash
cd ~/work/gf-stats-e2e
cargo test -p ghostframe-client-core --test stats_counters 2>&1 | tail -15
```

Expected: compile error `no method named samples_accepted found for reference &ClientStats` (and `frames_received`). That is the red: the test target cannot exist without the methods.

- [ ] **Step 3: Implement**

Find the places in `ghostframe-client-core`:

```bash
cd ~/work/gf-stats-e2e
grep -n "pub struct ClientStats\|samples_rejected\|fn apply_sample\|fn apply_schema\|pub fn view\|fn handle_server_stream\|next_frame" ghostframe-client-core/src/*.rs
```

Then make these edits (the identifiers `samples_rejected`, the sample-apply function and the `next_frame` loop are the ones PR 2 defines; place the counters next to them):

1. In `pub struct ClientStats { ... }`, next to the `samples_rejected` field, add:

```rust
    /// Samples applied to the server rings (schema id and count matched).
    /// Read by the web e2e through `window.__ghostframeStatsSamples`; a
    /// rejected sample never moves it.
    samples_accepted: u64,
    /// Every complete frame the server-stream reader yielded, whatever it
    /// held and whether or not it was applied. Read by the web e2e through
    /// `window.__ghostframeStatsFrames` to prove the server is silent.
    frames_received: u64,
```

2. Initialise both to `0` in the struct's constructor (wherever `samples_rejected: 0` is).

3. `samples_accepted`: increment on the success branch of the sample-apply path only, after the values have been pushed into the rings (never before the checks, and never in the `samples_rejected += 1` branch):

```rust
        self.samples_accepted += 1;
```

4. `frames_received`: in `handle_server_stream`'s loop over `reader.next_frame()`, increment on every `Some(Ok(_))` item, as the first statement of that arm, before the match on the frame type and before any check. Add `self.stats.note_frame_received();` there with this method in `impl ClientStats`:

```rust
    pub(crate) fn note_frame_received(&mut self) {
        self.frames_received += 1;
    }
```

Do not gate either counter, or the application of a sample, on the core's own subscribed flag.

5. Add the getters in `impl ClientStats`:

```rust
    /// Samples applied to the server rings since this core was created.
    pub fn samples_accepted(&self) -> u64 {
        self.samples_accepted
    }

    /// Complete server-stream frames received since this core was created.
    pub fn frames_received(&self) -> u64 {
        self.frames_received
    }
```

- [ ] **Step 4: Run and see it pass**

```bash
cd ~/work/gf-stats-e2e
cargo test -p ghostframe-client-core --test stats_counters 2>&1 | tail -8
```

Expected: `test result: ok. 4 passed`.

- [ ] **Step 5: Break the guarded code and watch the tests fail** (AGENTS.md "Testing habits")

The implementation is not committed yet, so `git stash` is not usable to revert; make each edit by hand and undo it by hand. After each, run `cargo test -p ghostframe-client-core --test stats_counters 2>&1 | grep -E "FAILED|panicked|failed"`:

- (a) move the `samples_accepted` increment to the top of the apply function: `counts_applied_samples_only` fails (the pre-schema sample is counted);
- (b) delete the `samples_accepted += 1`: `counts_applied_samples_only`, `counters_ignore_the_cores_own_subscribed_flag` and `a_frame_split_across_reads_counts_once` fail;
- (c) move the `note_frame_received` call after the schema check (so a rejected sample is not counted): `frames_received_counts_every_frame_before_any_check` fails;
- (d) make `handle_server_stream` return early when the core's subscribed flag is false: `counters_ignore_the_cores_own_subscribed_flag` fails.

Restore after each.

- [ ] **Step 6: Re-run clean, then lint**

```bash
cd ~/work/gf-stats-e2e
cargo test -p ghostframe-client-core --test stats_counters 2>&1 | tail -4
cargo clippy -p ghostframe-client-core --all-targets -- -D warnings 2>&1 | tail -5
just ci-client 2>&1 | tail -8
```

Expected: pass, no warnings, `ci-client` green (client crate changed).

- [ ] **Step 7: Commit**

```bash
cd ~/work/gf-stats-e2e
git status --short
git add ghostframe-client-core/src ghostframe-client-core/tests/stats_counters.rs
git commit -m "feat(client-core): count stats frames received and samples applied

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

(Run `git status --short` first; if `git add ghostframe-client-core/src` would stage anything unrelated, stage explicit paths.)

---

## Task 4: Export the counters and publish the two globals

**Model:** sonnet

Skip the parts Task 1 step 3 found already present.

**Files:**
- Modify: `ghostframe-client-wasm/src/core.rs`
- Modify: `ghostframe-web-client/src/main.ts`
- Modify: `ghostframe-web-client/src/globals.d.ts`

- [ ] **Step 1: Locate where PR 2 stores the core and reads the stream**

```bash
cd ~/work/gf-stats-e2e
grep -n "inner" ghostframe-client-wasm/src/core.rs | head -12
grep -n "handleServerStream\|handle_server_stream" ghostframe-client-wasm/src/core.rs ghostframe-web-client/src/main.ts ghostframe-web-client/src/*.ts
```

PR 2's contract makes `inner` an `Rc<RefCell<ClientCore>>`. If the grep shows a plain `ClientCore`, replace `.borrow()` below with direct access. Note which function holds the `handleServerStream` call: Step 3 depends on it.

- [ ] **Step 2: Add the wasm exports**

In `ghostframe-client-wasm/src/core.rs`, inside the `#[wasm_bindgen] impl WasmClientCore` block (next to `handleDatagram`):

```rust
    /// Samples the stats rings have accepted since this core was created.
    /// `u32`, not `u64`: a `u64` crosses as a BigInt, and at one sample a
    /// second this overflows after 136 years. Published by `main.ts` as
    /// `window.__ghostframeStatsSamples` for the e2e suite.
    #[wasm_bindgen(js_name = statsSamplesAccepted)]
    pub fn stats_samples_accepted(&self) -> u32 {
        self.inner
            .borrow()
            .stats()
            .samples_accepted()
            .min(u64::from(u32::MAX)) as u32
    }

    /// Complete server-stream frames received since this core was created,
    /// counted before any check. Published as `window.__ghostframeStatsFrames`.
    #[wasm_bindgen(js_name = statsFramesReceived)]
    pub fn stats_frames_received(&self) -> u32 {
        self.inner
            .borrow()
            .stats()
            .frames_received()
            .min(u64::from(u32::MAX)) as u32
    }
```

- [ ] **Step 3: Publish both from `main.ts`**

Two edits, both gated by `diagnostics` like the other polled globals (`const diagnostics` at `main.ts:46`, inside `main()`).

(a) Directly after the `if (diagnostics) installConsoleRing();` line, initialise both, so the e2e reads 0 before anything arrives and an absent global means "main.ts did not publish":

```ts
  if (diagnostics) {
    window.__ghostframeStatsSamples = 0;
    window.__ghostframeStatsFrames = 0;
  }
```

(b) In the loop PR 2 added to read the feedback bidi stream's readable half, immediately after the line that calls `core.handleServerStream(...)`:

```ts
      if (diagnostics) {
        window.__ghostframeStatsSamples = core.statsSamplesAccepted();
        window.__ghostframeStatsFrames = core.statsFramesReceived();
      }
```

Indent to match the block that holds the call (two wasm calls per chunk, one chunk per second at most). If Step 1 showed the reader loop is outside `main()` (a transport helper or another module), `diagnostics` is not in scope there: pass it as a parameter, or re-derive it with `new URL(window.location.href).searchParams.has('e2e')`. Do not drop the gate. `npx tsc --noEmit` in Step 5 is the check that the name resolves.

- [ ] **Step 4: Type the globals**

In `ghostframe-web-client/src/globals.d.ts`, after the `__ghostframeRafTicks` declaration (line 24):

```ts
  /** Stats samples the wasm core applied to its rings (e2e only); see main.ts. */
  __ghostframeStatsSamples: number | undefined;
  /** Server-stream frames the wasm core received, counted before any check (e2e only); see main.ts. */
  __ghostframeStatsFrames: number | undefined;
```

- [ ] **Step 5: Build and typecheck**

```bash
cd ~/work/gf-stats-e2e
cargo clippy -p ghostframe-client-wasm --all-targets -- -D warnings 2>&1 | tail -5
cargo build -p ghostframe-client-wasm --target wasm32-unknown-unknown 2>&1 | tail -3
cd ghostframe-web-client && npm run build 2>&1 | tail -6 && npx tsc --noEmit && echo TSC-OK
grep -c "statsSamplesAccepted\|statsFramesReceived" dist/assets/*.js
```

Expected: no clippy warnings, build ok, `TSC-OK`, and the grep prints a count of at least 1 (the built bundle carries the calls, so the embedded client is the new one).

- [ ] **Step 6: Run the JS suites that guard the wasm boundary**

```bash
cd ~/work/gf-stats-e2e/ghostframe-web-client && npm test 2>&1 | tail -8
```

Expected: all pass (this is the `wasm-module` job of `client-core.yml`).

- [ ] **Step 7: Commit**

```bash
cd ~/work/gf-stats-e2e
git add ghostframe-client-wasm/src/core.rs ghostframe-web-client/src/main.ts ghostframe-web-client/src/globals.d.ts
git commit -m "feat(web-client): publish stats frame and sample counts for e2e

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

## Task 5: Test hooks in the panel's DOM

**Model:** sonnet

PR 2 owns the panel's markup. The e2e needs three stable hooks, and a hook PR 2 already has (Task 1 step 3) is skipped. Recommend to the PR 2 author that these exist from the start; this task then does nothing.

**Files:**
- Modify: `ghostframe-web-ui/src/*.rs` (the component files for `Tab` and `MetricRow`)

- [ ] **Step 1: Find the elements**

```bash
cd ~/work/gf-stats-e2e
grep -n "fn Tab\|fn MetricRow" -r ghostframe-web-ui/src
```

- [ ] **Step 2: Add the hooks**

Check the syntax against the pinned Leptos version first (`grep -n leptos ghostframe-web-ui/Cargo.toml`, and a nearby `view!` in the crate). In Leptos 0.9's `view!`, a plain element takes a hyphenated attribute directly, static (`data-testid="stats-tab"`) or reactive with a closure (`data-label=move || ...`); the `attr:` prefix is for passing an attribute through a *component*, not for a plain element. If the pinned version differs, follow what its existing elements in the crate do, and let `cargo clippy` be the check.

On the `Tab` component's button element add:

```rust
data-testid="stats-tab"
```

On `MetricRow`'s root element add the following, with `row` standing for whatever binding holds the row's view data (`label`, `current: Option<f64>`, `stale: bool`; `StatsView` rows per the shared contract). The attributes are reactive closures, because rows update once a second:

```rust
data-testid="stats-row"
data-label=move || row.get().label.clone()
data-current=move || row.get().current.map(|v| v.to_string()).unwrap_or_default()
data-stale=move || row.get().stale.to_string()
```

Adapt `row.get()` to the accessor PR 2 uses (a signal, a `Memo`, or a `For` item). The rule for `data-current`: it is the unformatted number (`f64::to_string`), never the SI-formatted text, so the test does not parse formatting, and it is empty exactly when `row.current` is `None` (a text row, or a row with no value yet). A stale row sets `data-stale="true"`, whatever `current` holds; the test ignores stale rows through `data-stale`, so the hook needs no special case for them.

- [ ] **Step 3: Compile and run the crate's tests**

```bash
cd ~/work/gf-stats-e2e
cargo clippy -p ghostframe-web-ui --all-targets -- -D warnings 2>&1 | tail -5
cargo test -p ghostframe-web-ui 2>&1 | tail -6
```

Expected: no warnings, tests pass (the pure-logic ones; the `wasm-bindgen-test` runs only under the browser runner).

- [ ] **Step 4: Commit**

```bash
cd ~/work/gf-stats-e2e
git add ghostframe-web-ui/src
git commit -m "feat(web-ui): stable test hooks on the stats tab and rows

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

(`git add` names the directory because the files touched are exactly the component files found in step 1; run `git status --short` first and stage explicit paths if anything else shows.)

---

## Task 6: Green e2e, proof it is load-bearing, CI reach

**Model:** sonnet

**Files:**
- Modify: `Justfile` (new recipe `test-e2e-stats`, after `test-e2e-isolated`)

Before Step 2: no other worktree may build images or run e2e until Task 6 is done (one global image tag; see Task 2 step 3). Each mutation run below rebuilds the image from this tree, so after each one check that the mutated binary is what ran, by the failure message the step names; a different failure means a swapped image or a wrong mutation, not a result.

- [ ] **Step 1: Add a recipe for the one test** (rebuilds both images so the run is never stale)

```make
# The stats panel e2e alone: rebuilds the web client and both images first
# (the client is embedded in the daemon, and e2e runs the image, not the tree).
test-e2e-stats: build-web containers-build
    docker rm -f ghostframe-server headscale >/dev/null 2>&1 || true
    cargo test --test e2e e2e_stats_panel_subscribe_samples_unsubscribe -- --exact --test-threads=1 --nocapture
```

- [ ] **Step 2: Run it green**

```bash
cd ~/work/gf-stats-e2e
just test-e2e-stats 2>&1 | tail -20
```

Expected: `stats panel: bandwidth estimate <non-zero>, rtt <non-zero>` printed, then `test result: ok. 1 passed`. The two printed numbers are the proof the measurement fired. If it fails on the labels, the panel dump in the panic message lists every row; adapt the two label needles in the test (`"bandwidth estimate"`, `"round-trip time"`) to PR 1's labels and record it under "Contract deviations".

- [ ] **Step 3: Mutation 1, the client's unsubscribe asks to stay subscribed** (proves the "server went quiet" assertion measures the server)

Wherever `set_stats_subscribed` builds the message (`grep -n "StatsSubscribe" ghostframe-client-core/src/*.rs`), change `StatsSubscribe { on }` to `StatsSubscribe { on: true }`. The client's own local flag may still follow `on`; that is fine, because the e2e's silence check reads the raw frame count, not anything the client chose to apply. Then:

```bash
cd ~/work/gf-stats-e2e
just test-e2e-stats 2>&1 | grep -E "panicked|kept sending|test result"
```

Expected: FAIL with `the server kept sending after the panel was closed`. Revert the edit by hand, do not commit.

- [ ] **Step 4: Mutation 2, a measurement that reads zero** (proves the non-zero assertion)

Find the server's RTT sample (`grep -rn "rtt" ghostframe-lib/src/transport/stats.rs ghostframe-lib/src/transport/io_bridge.rs | head`, or the descriptor named in PR 1's table) and make the sampled value a constant `0.0`. Then `just test-e2e-stats 2>&1 | grep -E "panicked|never became|rtt=|test result"`.

Expected: FAIL with `bandwidth estimate / RTT never became non-zero and fresh in 20 s ... rtt=None`, followed by a row dump in which the RTT row reads `current=Some(0.0)`. Revert by hand.

- [ ] **Step 5: Mutations 3 and 4, in the server** (prove the closed-panel and the close assertions at the source)

Both edit PR 1's handler and sampler in `ghostframe-lib` (`grep -n "StatsSubscribe\|stats_subscribed\|subscribed" ghostframe-lib/src/transport/io_bridge.rs`). Run each on its own and revert by hand in between.

- Mutation 3, the server sends without a subscribe: make the "is this session subscribed" check in the sampler always true. Expected: FAIL at the first assertion, `server-stream frames arrived with the panel closed`. This is the case the old applied-sample counter could not see (the client rejects schema-less samples, so `samples` stays 0 and only `frames` moves); if the test passes here, the closed-panel assertion is vacuous. If the sampler sends the schema only on an `on`, the frames are schema-less samples, which is exactly the point.
- Mutation 4, the server ignores `on=0`: make the handler for `StatsSubscribe { on: false }` a no-op. Expected: FAIL at `the server kept sending after the panel was closed`.

- [ ] **Step 6: Prove CI will run the test**

```bash
cd ~/work/gf-stats-e2e
git status --short                                  # no mutation left behind
cargo test --test e2e -- --list 2>/dev/null | grep e2e_stats_panel
grep -n "e2e_stats_panel" ci/skip-list.txt || echo "not skip-listed: CI runs it"
grep -n "cargo test --test e2e" .github/workflows/e2e.yml
```

Expected: the test is listed, `not skip-listed: CI runs it`, and the workflow still has its `cargo test --test e2e` step. The test needs no VKMS: `setup_e2e_webgpu` passes `gpu: false`, the same setup as `e2e_input_forwarding_chromium`, which runs on CI.

- [ ] **Step 7: Check the polled-globals guard in `e2e_polled_globals_are_live`** (`e2e.rs:3810`)

That guard lists globals read by the suite and requires each to go non-trivial on a `--mixed` scene with the panel closed. The two stats globals would read 0 there by design, so they are NOT added to `GLOBAL_PROBES`; instead the new test is itself the liveness proof (it requires `samples` to rise above 0 and `frames` to hold still after close). Add a note to the guard's doc comment so the next reader does not "fix" the gap:

In the doc comment above `async fn e2e_polled_globals_are_live`, after the paragraph ending "...`evaluate(\"window.__...\")` calls.", add:

```rust
///
/// Two globals are deliberately absent: `__ghostframeStatsSamples` and
/// `__ghostframeStatsFrames` only move while the stats panel is open, so this
/// scene (panel closed) would read them as 0. They are also initialised to 0
/// by `main.ts`, so even an unpublished one would read as live-looking here.
/// `e2e_stats_panel_subscribe_samples_unsubscribe` is their liveness proof: it
/// requires `samples` to rise after the tab click and `frames` to stop after
/// the second click.
```

- [ ] **Step 8: Commit**

```bash
cd ~/work/gf-stats-e2e
git add Justfile ghostframe-e2e/tests/e2e.rs
git commit -m "test(e2e): just test-e2e-stats; note why the stats counter is not in the live-globals list

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

## Task 7: Bundle size guard, self-test first

**Model:** sonnet

**Files:**
- Create: `ci/test-check-wasm-size.sh`
- Create: `ci/check-wasm-size.sh`

The limit is 512 KiB = 524,288 bytes of raw wasm (spec section 5: "fails the build if the wasm exceeds 512 KiB"). The guard fails closed: no wasm in the directory is a failure, not a pass (a stale or missing `dist/` must not read as "small enough").

- [ ] **Step 1: Write the self-test**

`ci/test-check-wasm-size.sh`:

```bash
#!/usr/bin/env bash
# Self-test of ci/check-wasm-size.sh: under, at and over the limit, no wasm at
# all, and a second oversize wasm next to a small one. Run by `just ci-local`
# and the web-client CI job, so the guard cannot rot into one that never fires.
set -uo pipefail
cd "$(dirname "$0")/.."

guard=ci/check-wasm-size.sh
limit=524288
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail=0

# expect <name> <expected exit 0|1> <dir>
expect() {
    local name=$1 want=$2 dir=$3 got
    "$guard" "$dir" >/dev/null 2>&1
    got=$?
    if [[ $got -ne $want ]]; then
        echo "FAIL: $name: expected exit $want, got $got"
        fail=1
    else
        echo "ok:   $name"
    fi
}

mk() { # mk <dir> <file> <bytes>
    mkdir -p "$1"
    truncate -s "$3" "$1/$2"
}

mk "$tmp/under" a.wasm 100000
expect "100 KB passes" 0 "$tmp/under"

mk "$tmp/at" a.wasm "$limit"
expect "exactly the limit passes" 0 "$tmp/at"

mk "$tmp/over1" a.wasm $((limit + 1))
expect "one byte over fails" 1 "$tmp/over1"

mk "$tmp/over" a.wasm 400000
expect "400 KB fails" 1 "$tmp/over"

mkdir -p "$tmp/empty"
expect "no wasm at all fails (stale or missing dist)" 1 "$tmp/empty"

mk "$tmp/js_only" index.js 1000
expect "only a .js file fails (no wasm found)" 1 "$tmp/js_only"

mk "$tmp/two" small.wasm 1000
mk "$tmp/two" big.wasm 400000
expect "a small wasm does not hide a big one" 1 "$tmp/two"

exit $fail
```

```bash
cd ~/work/gf-stats-e2e
chmod +x ci/test-check-wasm-size.sh
```

- [ ] **Step 2: Run it and see it fail**

```bash
cd ~/work/gf-stats-e2e
ci/test-check-wasm-size.sh; echo "exit=$?"
```

Expected: the guard does not exist, so every run exits 127: seven `FAIL:` lines (even the three that expect exit 1 read 127) and `exit=1`.

- [ ] **Step 3: Write the guard**

`ci/check-wasm-size.sh`:

```bash
#!/usr/bin/env bash
# The web client's wasm must stay under 512 KiB (524,288 bytes, raw).
#
# docs/specs/stats-panel-design.md section 5: adding Leptos to the wasm module
# is the biggest single size risk of the web client, and the wasm is what every
# browser downloads before the first frame. Today's wasm is about 168 KB. The
# limit leaves room for the panel and fails the build when a dependency or a
# generic blows it up; raise it deliberately, in a commit that says why, and
# record the new number in docs/specs/wasm-bundle-baseline.md.
#
# Usage: ci/check-wasm-size.sh [assets-dir]
#   assets-dir defaults to ghostframe-web-client/dist/assets (the vite output,
#   i.e. what ghostbridge embeds). It must already be built: `just build-web`.
#
# Fails closed: a directory with no .wasm file is an error, never a pass, so a
# missing or stale dist/ cannot read as "small enough". Every .wasm in the
# directory is checked, so a second module cannot hide behind a small one.
set -euo pipefail
cd "$(dirname "$0")/.."

assets="${1:-ghostframe-web-client/dist/assets}"
max_bytes=524288

shopt -s nullglob
files=("$assets"/*.wasm)
if [[ ${#files[@]} -eq 0 ]]; then
    echo "::error::no .wasm file in $assets (build the web client first: just build-web)"
    exit 1
fi

status=0
for f in "${files[@]}"; do
    size=$(stat -c %s "$f")
    gz=$(gzip -c "$f" | wc -c)
    echo "$(basename "$f"): $size bytes raw, $gz bytes gzip (limit $max_bytes raw)"
    if ((size > max_bytes)); then
        echo "::error::$(basename "$f") is $size bytes, over the $max_bytes byte (512 KiB) limit (see ci/check-wasm-size.sh)"
        status=1
    fi
done
exit $status
```

```bash
cd ~/work/gf-stats-e2e
chmod +x ci/check-wasm-size.sh
```

- [ ] **Step 4: Run the self-test green, and the guard on the real build**

```bash
cd ~/work/gf-stats-e2e
ci/test-check-wasm-size.sh; echo "exit=$?"
ci/check-wasm-size.sh; echo "exit=$?"
```

Expected: seven `ok:` lines and `exit=0`; the real build prints its wasm size (the number the baseline doc records) with `exit=0`. If the real wasm is over 524,288, the panel is too big: do not raise the limit here, report it (spec question for the owner).

- [ ] **Step 5: Commit the guard, then break it and watch the self-test fail**

Commit first, so every mutation can be undone with `git checkout` and no backup file exists:

```bash
cd ~/work/gf-stats-e2e
git add ci/check-wasm-size.sh ci/test-check-wasm-size.sh
git commit -m "ci: fail the build when the web client's wasm exceeds 512 KiB

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
sed -i 's/max_bytes=524288/max_bytes=9999999/' ci/check-wasm-size.sh
ci/test-check-wasm-size.sh; echo "exit=$?"
git checkout ci/check-wasm-size.sh
```

Expected: `FAIL: one byte over fails`, `FAIL: 400 KB fails`, `FAIL: a small wasm does not hide a big one`, `exit=1`. Then repeat with the other two mutations, running `git checkout ci/check-wasm-size.sh` after each:

- `sed -i 's/((size > max_bytes))/((size >= max_bytes))/' ci/check-wasm-size.sh` must fail `exactly the limit passes`;
- delete the `if [[ ${#files[@]} -eq 0 ]]` block (hand edit): `no wasm at all fails` and `only a .js file fails` must fail.

Then confirm the tree is clean and the self-test passes:

```bash
git status --short
ci/test-check-wasm-size.sh; echo "exit=$?"
```

Expected: no output from `git status`, seven `ok:` lines, `exit=0`.

---

## Task 8: Wire the guard into `just ci-local`, CI and AGENTS.md

**Model:** haiku

**Files:**
- Modify: `Justfile` (the `ci-local` recipe, after `cd ghostframe-web-client && npx tsc --noEmit`)
- Modify: `.github/workflows/ci.yml` (the `web-client` job, after the `install + build` step)
- Modify: `AGENTS.md`

- [ ] **Step 1: `ci-local`**

In `Justfile`, directly after the line `    cd ghostframe-web-client && npx tsc --noEmit` inside `ci-local`:

```make
    @echo "=== wasm bundle under 512 KiB ==="
    ci/test-check-wasm-size.sh
    ci/check-wasm-size.sh
```

- [ ] **Step 2: CI**

In `.github/workflows/ci.yml`, in the `web-client` job, after the `install + build` step and before `upload dist artifact for e2e.yml`:

```yaml
      # The wasm is what every browser downloads before the first frame, and
      # the stats panel's Leptos is the first dependency that can blow it up.
      # The self-test keeps the guard honest (it fails closed on a missing
      # wasm and on a limit that no longer fires).
      - name: wasm bundle under 512 KiB
        run: |
          ci/test-check-wasm-size.sh
          ci/check-wasm-size.sh
```

- [ ] **Step 3: AGENTS.md**

In the "Traps" section, after the "`cargo build` does not rebuild the web client" paragraph, add:

```markdown
**The web client's wasm has a size gate: 512 KiB.** `ci/check-wasm-size.sh`
reads `ghostframe-web-client/dist/assets/*.wasm` (so it needs `just build-web`
first), runs in `just ci-local` and the `web-client` CI job, and fails closed
when there is no wasm. Raise the limit only deliberately, and record the new
number in `docs/specs/wasm-bundle-baseline.md`.
```

and in "Client-side diagnostics are printed by the browser console", after the sentence ending "`Δdrain` and `nack_sent` are live.", add: ` \`window.__ghostframeStatsSamples\` and \`window.__ghostframeStatsFrames\` (e2e pages only) are live: the server stats samples the core applied, and every server-stream frame it received; both move only while the stats panel is open.`

- [ ] **Step 4: Verify and commit**

```bash
cd ~/work/gf-stats-e2e
just --evaluate >/dev/null && just --dry-run ci-local 2>&1 | grep -n "wasm" 
python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('yaml ok')"
ci/check-local-paths.sh && echo "paths ok"
git add Justfile .github/workflows/ci.yml AGENTS.md
git commit -m "ci: run the wasm size guard in ci-local and the web-client job

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

Expected: the dry run prints the two guard lines, `yaml ok`, `paths ok`.

---

## Task 9: Record the before and after sizes

**Model:** sonnet

**Files:**
- Modify: `docs/specs/wasm-bundle-baseline.md` (append a section and update the header paragraph; the measurement sections stay untouched)
- Create: `measure-wasm-baseline.sh` in the session scratchpad (a runnable script, never committed)

The "before" is the first-parent `master` commit just before PR 2's merge, rebuilt from scratch with the same command as "after", so the numbers are like for like and do not depend on how PR 2 was merged (squash or rebase: both leave a first-parent history in which the commit that first adds `ghostframe-web-ui/Cargo.toml` is PR 2's merge or its first commit, and `^1` is master before it). Nothing in the section is typed by hand: the script writes it from measurements. Shell variables do not survive between Bash calls, so the whole procedure is one script run in one call.

- [ ] **Step 1: Write the script**

Create `measure-wasm-baseline.sh` in the scratchpad directory (absolute path; the scratchpad path of the session) with:

```bash
#!/usr/bin/env bash
# Usage: measure-wasm-baseline.sh <worktree-of-this-PR>
# Builds "before" (master just before PR 2) and "after" (this tree) from scratch
# and appends a section to docs/specs/wasm-bundle-baseline.md.
set -euo pipefail

repo=$(cd "$1" && pwd)
cd "$repo"

# First-parent master commit that first touches ghostframe-web-ui/Cargo.toml,
# then its first parent: master as it was before PR 2 landed.
first=$(git log --first-parent master --format=%H -- ghostframe-web-ui/Cargo.toml | tail -1)
[[ -n "$first" ]] || { echo "no first-parent master commit touches ghostframe-web-ui/Cargo.toml" >&2; exit 1; }
before=$(git rev-parse "${first}^1")
[[ -n "$before" ]] || { echo "empty before commit" >&2; exit 1; }
echo "PR 2 landed at: $first; before: $before"
git ls-tree -r --name-only "$before" | grep -q '^ghostframe-web-ui/' \
    && { echo "before commit already has ghostframe-web-ui: wrong base" >&2; exit 1; }

scratch="$repo/../gf-stats-before"
[[ ! -e "$scratch" ]] || { echo "$scratch exists: remove it (git worktree remove --force)" >&2; exit 1; }
git worktree add "$scratch" "$before"
trap 'git worktree remove --force "$scratch" >/dev/null 2>&1 || true' EXIT
( cd "$scratch" && just build-web ) 2>&1 | tail -5

rm -rf ghostframe-web-client/dist ghostframe-web-client/pkg-web ghostframe-web-client/pkg-node
just build-web 2>&1 | tail -3

b="$scratch/ghostframe-web-client/dist/assets"
a="$repo/ghostframe-web-client/dist/assets"

measure() { # <assets-dir>: one table row per wasm / js asset, hash masked
    for f in "$1"/*.wasm "$1"/*.js; do
        name=$(basename "$f" | sed -E 's/-[A-Za-z0-9_-]{8}\./.[hash]./')
        printf '| `dist/assets/%s` | %s | %s |\n' "$name" "$(stat -c %s "$f")" "$(gzip -c "$f" | wc -c)"
    done
}
wasm_bytes() { stat -c %s "$1"/*.wasm; }

bw=$(wasm_bytes "$b"); aw=$(wasm_bytes "$a")
echo "before wasm: $bw bytes, after wasm: $aw bytes"
# Sanity: the before wasm must be near the ~168 KB recorded in the spec
# (167,844 bytes in the dist built from master at the time of writing).
((bw > 140000 && bw < 200000)) || { echo "before wasm $bw is far from ~168 KB: wrong before commit" >&2; exit 1; }

{
    echo
    echo "## Stats panel (Leptos in the wasm module), $(date -u +%Y-%m-%d)"
    echo
    echo "**Before:** \`$(git rev-parse --short=8 "$before")\` (master before the stats panel landed)  "
    echo "**After:** \`$(git rev-parse --short=8 HEAD)\` (\`feat/stats-panel-e2e\`)"
    echo
    echo "Both built from scratch with \`just build-web\` (\`npm run build\`), asset hashes masked."
    echo "**This section is a gate** for the wasm: \`ci/check-wasm-size.sh\` fails the build above"
    echo "524,288 bytes (512 KiB) raw. The JS bundle has no gate."
    echo
    echo "### Before"
    echo
    echo "| Asset | Raw bytes | Gzip bytes |"
    echo "|---|---:|---:|"
    measure "$b"
    echo
    echo "### After"
    echo
    echo "| Asset | Raw bytes | Gzip bytes |"
    echo "|---|---:|---:|"
    measure "$a"
    echo
    echo "### Wasm delta"
    echo
    echo "Raw: $bw -> $aw (+$((aw - bw)) bytes, $(( (aw - bw) * 100 / bw ))%). Limit 524,288; headroom $((524288 - aw)) bytes."
} >> docs/specs/wasm-bundle-baseline.md
tail -30 docs/specs/wasm-bundle-baseline.md
```

- [ ] **Step 2: Run it from the PR worktree, in one call**

```bash
bash <scratchpad>/measure-wasm-baseline.sh ~/work/gf-stats-e2e
```

(Use the script's absolute scratchpad path.) Expected: it prints the PR 2 landing commit and the before commit, the two wasm sizes, then the appended section with two tables of real numbers and a wasm delta; the headroom is positive. It exits non-zero, writing nothing, if no first-parent commit is found, if the "before" tree already contains `ghostframe-web-ui`, or if the before wasm is far from the ~168 KB recorded in the spec. If it stops on the sanity check, the before commit is wrong: stop and report rather than adjusting the bounds. The scratch worktree is removed on exit.

- [ ] **Step 3: Update the baseline doc's header**

The file's header says "This is a record, not a gate" and "No size assertion or CI check is attached to these numbers." The new section says it is a gate for the wasm, so the header must not contradict it. Edit that paragraph (and only that) so the first sentence reads "This is a record. Since the stats panel it has one gate: `ci/check-wasm-size.sh` fails the build when the wasm exceeds 512 KiB; see the last section." and replace the sentence "No size assertion or CI check is attached to these numbers." with "The measurements in sections 1 to 3 and the 2026-09-11 and 2026-09-12 sections have no assertion attached; only the wasm limit in the last section does."

- [ ] **Step 4: Run the guard on the fresh build, check paths**

```bash
cd ~/work/gf-stats-e2e
ci/check-wasm-size.sh
ci/check-local-paths.sh && echo "paths ok"
git status --short
```

Expected: guard exit 0, `paths ok` (the doc must contain no home-directory path; the script writes repo-relative names only), and only `docs/specs/wasm-bundle-baseline.md` modified.

- [ ] **Step 5: Commit**

```bash
cd ~/work/gf-stats-e2e
git add docs/specs/wasm-bundle-baseline.md
git commit -m "docs: record the wasm bundle size before and after the stats panel

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

## Task 10: Full gates

**Model:** sonnet

**Files:** none modified, unless a gate finds something; fix it in the task that introduced it and re-run.

- [ ] **Step 1: The real gate, under the GPU lock** (never bare: `ci-local` touches VA-API and contends with a live VCE)

```bash
cd ~/work/gf-stats-e2e
gf-gpu-run just ci-local 2>&1 | tail -40
```

Expected: `=== ci-local passed ===`. It includes fmt, the env-read guard, `clippy --workspace --all-targets` (the gate that sees a non-compiling test target), unit tests, the web build, `tsc`, the wasm guard and its self-test, the release build and the cbindgen header diffs. Capture a failing test's name before re-running anything (AGENTS.md "Known flakes").

- [ ] **Step 2: cbindgen**

`ci-local` diffs `ghostframe-lib/include/ghostframe.h` and the capi header. Confirm nothing is dirty:

```bash
cd ~/work/gf-stats-e2e
git diff --exit-code ghostframe-lib/include/ghostframe.h ghostframe-client-capi/include/ghostframe_client.h && echo "headers clean"
```

Expected: `headers clean`. This PR adds no `pub const` to `ghostframe-lib`, so the header must not move.

- [ ] **Step 3: Client crates** (Task 3 touched `ghostframe-client-core`)

```bash
cd ~/work/gf-stats-e2e
just ci-client 2>&1 | tail -8
```

Expected: green.

- [ ] **Step 4: The e2e once more on the final tree** (no other worktree may build images or run e2e meanwhile)

```bash
cd ~/work/gf-stats-e2e
just test-e2e-stats 2>&1 | tail -6
```

Expected: `test result: ok. 1 passed`.

- [ ] **Step 5: Status check, then push**

```bash
cd ~/work/gf-stats-e2e
git status --short
git log --oneline master..HEAD
```

Expected: clean tree, one commit per task. Push and open the PR only when the owner asks.

---

## Task 11: Review

**Model:** opus

- [ ] **Step 1: Request the review** (REQUIRED SUB-SKILL: superpowers:requesting-code-review)

Review the whole PR diff (`git diff master...HEAD`) against `docs/specs/stats-panel-design.md` sections 4, 5 and 6 item 3, with these questions:

1. Does the e2e prove what the spec says: open the panel, a subscribe goes out, server rows non-zero for bandwidth estimate and RTT, no samples after close? Could any assertion pass vacuously (counter from a stale page, rows present with 0, a sample in flight at close)?
2. Are `window.__ghostframeStatsSamples` (applied) and `window.__ghostframeStatsFrames` (raw, counted before any check) live, written only from their counters, and gated like their siblings? Does the close assertion use the raw one? Does the note in `e2e_polled_globals_are_live` correctly explain its absence from `GLOBAL_PROBES`?
3. Did each mutation in Tasks 3, 6 and 7 actually turn its test red (re-run two of them)?
4. Is the guard fail-closed (no wasm, js-only, several wasm), the limit exactly 524,288, and are both `ci-local` and `ci.yml` wired?
5. Are the baseline numbers reproducible with the commands in the doc, and does the doc contain no home paths?
6. Does the e2e run in CI by name (`--test e2e` step, not skip-listed), and is nothing skip-listed or `#[ignore]`d?
7. Hooks added to `ghostframe-web-ui` (if any): minimal, no behaviour change, no Leptos outside that crate?

- [ ] **Step 2: Fix every finding in the task that introduced it, re-run that task's tests, and re-run Task 10 step 1.**

---

## Self-review

### Spec coverage

| Spec section | Requirement | Task |
|---|---|---|
| 4 E2E (Chromium) | open the panel on a live session | 2, 6 |
| 4 E2E | a subscribe goes out | 2 (closed panel yields 0 frames, click yields a schema-checked sample), 3, 4, 6 (mutation 3) |
| 4 E2E | server rows show non-zero bandwidth estimate and RTT within a few seconds | 2, 5, 6 (mutation 2) |
| 4 E2E | after closing, no further samples arrive | 2 (raw frame count), 6 (mutations 1 and 4) |
| 4 E2E | no Firefox variant | 2 (Chromium only; the test has no `_firefox` twin) |
| 4 "Every test is checked by breaking the code" | mutation proof | 3 step 5, 6 steps 3 to 5, 7 step 5 |
| 2.3 Input and focus | a tab click does not move keyboard focus onto the panel | 2 (`click_stats_tab` checks `activeElement` and `elementFromPoint`) |
| 4 CI | new test target added to the workflow by name | 6 step 6 (it lives in the already-named `--test e2e` target; verified listed and not skip-listed) |
| 5 Bundle size | record sizes before and after | 9 |
| 5 Bundle size | CI fails above 512 KiB | 7, 8 |
| 5 New workspace member in Dockerfile | `ghostframe-web-ui` COPY lines | 1 step 2 (verified, PR 2's job) |
| 5 Gate | `just ci-local` under the GPU lock | 10 |
| 6 item 3 | "E2E and guard" PR | whole plan |
| AGENTS.md traps | web dist prebuilt, containers-build, clippy is the gate, cbindgen diff, `ci-client` | 1 step 4, 2 steps 2 and 3, 10 |

### Contract deviations

All are additions to the shared contract; no contract name is changed.

1. `ClientStats::samples_accepted(&self) -> u64` and `ClientStats::frames_received(&self) -> u64` (ghostframe-client-core), `WasmClientCore::statsSamplesAccepted()` and `statsFramesReceived()` (ghostframe-client-wasm), and `window.__ghostframeStatsSamples` / `window.__ghostframeStatsFrames: number | undefined` (main.ts, e2e pages only, initialised to 0). The contract has no client-observable "a sample was applied" or "a frame arrived" signal; `StatsView` carries only `samples_rejected`, which neither counts schema frames nor detects a client that drops samples. `frames_received` is counted on arrival, before any check and independent of the core's subscribed flag. Tasks 3 and 4 add them; each is skipped if PR 2 already has it.
2. DOM test hooks in `ghostframe-web-ui`: `data-testid="stats-tab"`, and on each row `data-testid="stats-row"`, `data-label`, `data-current` (raw number, empty exactly when `current` is `None`), `data-stale`. The contract names no selectors. Recommended to be built into PR 2; Task 5 adds what is missing.
3. The e2e assumes PR 1's labels contain "bandwidth estimate" and "round-trip time" (case-insensitive): PR 1 labels the rows "Bandwidth estimate" and "Round-trip time". If the labels differ, the two needles in the test change; the failure message dumps every row to make that a one-line fix.
4. The focus check in `click_stats_tab` assumes `#stats-panel` is the container id (contract: `<div id="stats-panel">`) and `data-testid="stats-tab"` is inside or beside it.

---

## Review notes

Every review point was accepted and addressed. Where the fix differs from the reviewer's wording:

- Raw signal for the silence checks: implemented as a separate `ClientStats::frames_received` counter (frames the reader yields, counted before any check) rather than only `StatsView::samples_rejected`, because `samples_rejected` does not count schema frames or frames of unknown type. The core's applied-sample counter is kept for the "a schema-checked sample arrived" and "rows are non-zero" phases. Task 3 also adds the reviewer's alternative as a client-core unit test (`counters_ignore_the_cores_own_subscribed_flag`), and Task 6 adds the server-side mutation (mutation 4, server ignores `on=0`).
- The e2e globals are initialised to 0 in `main.ts` and the Rust helper no longer uses `?? 0`, so an unpublished global is an error rather than a silent 0.
- Focus check: asserts that `document.activeElement` is not inside the tab or panel, not that it is the canvas, because the canvas may legitimately never have been focused in this scene (spec 2.3 says only that focus does not move to the tab).
- Task 9's "before" is defined as the first-parent master parent of the commit that first touches `ghostframe-web-ui/Cargo.toml`, with a sanity check against the ~168 KB figure and a refusal if that tree already has `ghostframe-web-ui`. The Leptos attribute syntax in Task 5 could not be checked against code (the crate does not exist on this base); the step says to follow the pinned version and lets clippy decide.
- Task 7's backup file is gone: the guard is committed first, and each mutation is undone with `git checkout`.
