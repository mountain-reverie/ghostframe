# Firefox E2E Coverage: Design

**Date:** 2026-06-12
**Status:** Design approved
**Predecessors:** `2026-06-09-tailnet-served-web-client-design.md` (the web-server path Firefox needs to exercise)

---

## Context

The e2e suite runs Chromium-only. Adding the tailnet-served web client uncovered a Firefox-specific failure mode that Chromium would never have hit: Firefox's WGSL validator (Naga) rejects the `h264_blit.wgsl` shader because Firefox WebGPU as of mid-2026 does not expose the `TEXTURE_EXTERNAL` capability that the shader requires. The renderer fix (commit `9ae2f54`) makes the H264 pipeline lazy so the rest of the codec suite still works on Firefox, and a follow-up plumbs `h264Supported` through the HELLO message so the server can avoid selecting H.264 for Firefox clients.

The HELLO-bit work cannot be reliably implemented without a Firefox e2e to validate it. This spec adds Firefox as a first-class browser target in the existing e2e harness, doubling the test surface for every codec-relevant scenario that does not depend on H.264.

After this change, `cargo test --test e2e` runs every static-codec scenario on both Chromium (via CDP) and Firefox (via WebDriver), with a small per-test trait abstraction keeping each test body single-sourced.

---

## Decision Register

| # | Decision | Rationale |
|---|---|---|
| D1 | Side-by-side driver crates: `chromiumoxide` for Chromium (unchanged), `fantoccini` for Firefox | Existing Chromium tests untouched; CDP screenshot fidelity preserved for the SSIM threshold; WebDriver is the only protocol Firefox supports |
| D2 | Host-installed Firefox, not containerized | Matches the existing Chromium pattern (no `/dev/dri` complications, no docker network plumbing, no second tsnet client); accept Firefox version drift as a future-problem, escape hatch via env var |
| D3 | `BrowserSession` trait with 4 methods (`new_page`, `evaluate`, `screenshot`, `close`); two impls (`ChromiumSession`, `FirefoxSession`) | Smallest trait that covers every operation in current tests; keeps the abstraction layer trivial and inspectable |
| D4 | Per-test convention: scenario body is generic `async fn …_body<B: BrowserSession>(…)`; two thin `#[tokio::test]` entry points (`…_chromium`, `…_firefox`) | Naturally cargo-filter-friendly; no proc-macros; one golden PNG per scenario shared between browsers |
| D5 | Per-test temporary Firefox profile dir; cert imported via `certutil -A` | Matches the cleanliness of Chromium's per-test `--ignore-certificate-errors-spki-list`; isolates parallel tests; per-test geckodriver on a free port |
| D6 | Hard-fail in `FirefoxSession::new` when `firefox`, `geckodriver`, or `certutil` is missing | "Silently skipped on my machine but green on CI" was the failure mode this whole iteration cycle just experienced; document `--skip _firefox` as the explicit opt-out |
| D7 | H.264 tests (`e2e_h264_*`, `e2e_headroom_guard_forces_h264`, `e2e_loss_override_forces_h264`) and any test that transitions through H.264 stay Chromium-only | Known Firefox limitation; the renderer drops H.264 frames silently on Firefox so server-side telemetry assertions wouldn't match |
| D8 | Initial PR converts 7 representative scenarios with Firefox siblings + 1 Chromium-only refactor (demonstrating the H.264-exclusion pattern); remaining ~30 Firefox-eligible follow in single-test follow-up PRs | Spreads risk; the trait + plumbing land before the bulk conversion; new conversions become 5-line additions |
| D9 | CI uses `firefox-esr` (Extended Support Release) + pinned `geckodriver-v0.36.0` via direct download | ESR's WGSL/Naga moves slower than stable; pinned geckodriver avoids the moving-target problem in apt's stale package |
| D10 | Single CI job runs both browsers in one `cargo test --test e2e` invocation | Tokio parallelism handles cross-browser concurrency; splitting jobs would double headscale + ghostframe-server container startup cost with no signal benefit |

---

## Architecture

### Topology

```
host
├── test process  (cargo test)
│   ├── TestNode (tsnet client)            ──→ headscale + ghostframe-server
│   ├── Chromium (chromiumoxide / CDP)     ──→ /usr/bin/chromium   (unchanged)
│   └── geckodriver subprocess             ──→ /usr/bin/firefox via WebDriver
│         └─ fantoccini client ←→ localhost:<free port>
└── Weston compositor (existing — both browsers share it)
docker network
├── headscale          (existing)
└── ghostframe-server  (existing)
```

Firefox launches the same way Chromium does: a binary on the host, attached to the same Weston/XWayland display, reaching ghostframe-server through the same `TestNode` TCP/UDP forwarders. The only new piece is a `geckodriver` subprocess on a per-test free port.

### `BrowserSession` trait

```rust
// ghostframe-e2e/src/harness/browser.rs

#[async_trait]
pub trait BrowserSession: Send {
    async fn new_page(&mut self, url: &str) -> Result<()>;
    async fn evaluate<T: DeserializeOwned + Send + 'static>(
        &mut self,
        script: &str,
    ) -> Result<T>;
    async fn screenshot(&mut self) -> Result<Vec<u8>>;
    async fn close(self) -> Result<()>;
}
```

- **`ChromiumSession`** wraps `chromiumoxide::Browser` + a `Page`. Each method delegates to the existing CDP call sites (the survey identified `Page.captureScreenshot`, `Page.evaluate`, etc.). The current launch-args path (SPKI, `--ozone-platform=x11`, `--enable-unsafe-webgpu`, `--use-vulkan`, `--ignore-gpu-blocklist`, `no-sandbox`, `with_head`) stays as-is.

- **`FirefoxSession`** wraps a `geckodriver` child process + a `fantoccini::Client`. Owns a `tempfile::TempDir` profile dir (cleaned on Drop). Launch sequence is detailed below.

Not in the trait, deliberately: `wait_for_status(selector, text)` — kept as a free function in `harness/scene.rs` built on top of `evaluate`. Network throttling and viewport overrides — CDP-specific; tests that need them stay Chromium-only.

`Drop` impls best-effort kill the underlying browser/geckodriver and let the temp dir clean itself. `close()` is the orderly path; callers invoke it at the happy-path end.

### `FirefoxSession::new` lifecycle

1. Bind `TcpListener::bind("127.0.0.1:0")`, read the port, drop. → per-test free port for geckodriver.
2. Create temp profile dir under `std::env::temp_dir().join("ghostframe-fx-{pid}-{rand}")`.
3. Write `user.js` with the prefs:
   - `dom.webgpu.enabled = true`
   - `network.webtransport.enabled = true` (defensive — default true on current Firefox)
   - `dom.security.https_only_mode = false`
   - `marionette.port = 0` (geckodriver picks its own)
4. Write the static cert PEM (from `e2e_certs.rs::include_bytes!`) to `<profile_dir>/import.pem`. Run `certutil -A -n ghostframe-e2e -t "C,," -i import.pem -d sql:<profile_dir>`. Delete the PEM after import. Hard-fail if `certutil` is missing.
5. Spawn `geckodriver --port <port> --binary <firefox_bin> --profile-root <tmp>` as a child process. Tee stdout/stderr into the test's tracing output prefixed `[geckodriver]`.
6. Poll `http://127.0.0.1:<port>/status` for up to 10 s until it returns `ready: true`.
7. Construct a `fantoccini::Client` with capabilities `firefoxOptions.args = ["-profile", <profile_dir>]` and `firefoxOptions.binary = <firefox_bin>` so geckodriver uses the pre-seeded profile (passing `-profile` prevents geckodriver's default profile cloning, which would lose the certutil-imported cert).

Geckodriver `--profile-root` semantics changed around v0.32. The implementation parses `geckodriver --version` at startup and falls back to base64-encoded `firefoxOptions.profile` if the installed geckodriver is older. Documented so a CI version mismatch produces a clear error rather than a silent skip.

### Test convention

Each eligible scenario becomes:

```rust
async fn solid_red_body<B: BrowserSession>(b: &mut B, stack: &ScenarioStack) -> Result<()> {
    b.new_page(&stack.page_url()).await?;
    wait_for_status(b, "Receiving frames").await?;
    let png = b.screenshot().await?;
    assert_ssim_against_golden(&png, "solid_red.png")?;
    Ok(())
}

#[tokio::test]
async fn e2e_solid_red_chromium() -> Result<()> {
    let stack = launch_scenario_stack(scenario_solid_red()).await?;
    let mut b = ChromiumSession::new(&stack).await?;
    solid_red_body(&mut b, &stack).await?;
    b.close().await
}

#[tokio::test]
async fn e2e_solid_red_firefox() -> Result<()> {
    let stack = launch_scenario_stack(scenario_solid_red()).await?;
    let mut b = FirefoxSession::new(&stack).await?;
    solid_red_body(&mut b, &stack).await?;
    b.close().await
}
```

Cargo filters work naturally: `cargo test e2e_solid_red_` runs both, `cargo test _firefox` runs the Firefox suite, `cargo test --skip _firefox` runs the Chromium subset.

---

## Components

### New
- `ghostframe-e2e/src/harness/browser.rs` (~250 lines) — `BrowserSession` trait + `ChromiumSession` + `FirefoxSession` + `geckodriver` lifecycle + cert install helper.

### Modified
- `ghostframe-e2e/src/harness/chromium.rs` → renamed to `pixels.rs`. SSIM and golden-management helpers stay; their input becomes `&[u8]` PNG instead of `chromiumoxide::Page` so both backends feed them.
- `ghostframe-e2e/src/harness/scene.rs` — `launch_scenario_stack(...)` no longer constructs a Chromium browser inline; returns a `ScenarioStack` that owns server-side bits only. The browser is constructed in the test entry point.
- `ghostframe-e2e/tests/e2e.rs` — initial set of 8 scenarios refactored into `_body` functions + paired `_chromium` / `_firefox` entry tests.
- `ghostframe-e2e/Cargo.toml` — `fantoccini = "0.21"` added to `[dev-dependencies]`.
- `Justfile` — `firefox-bin` variable; `e2e-firefox-doctor` and `e2e-firefox` recipes.
- `.github/workflows/e2e.yml` — install `firefox-esr` + `libnss3-tools` (provides `certutil`) + pinned `geckodriver-v0.36.0`. Existing google-chrome-stable symlink at `/usr/bin/chromium` (the CI's Chromium substitute) stays.

### Initial scope: 7 Firefox conversions + 1 Chromium-only refactor

The implementation plan picks one test per codec/scenario family to convert in this PR. Specific picks finalized during implementation by inspecting `ghostframe-e2e/tests/e2e.rs` for the smallest-body member of each family:

**Get a `_firefox` sibling (7):**
- Solid (one-color tile family)
- PalRle bundled
- PalRle thin (post-session palette)
- CDF5/3 lossless build-up
- CDF5/3 progressive refinement
- Raw upload (e.g., edge-tile non-32-aligned resolution)
- Palette eviction (LRU)

**Refactored into a `_chromium` entry only, no Firefox sibling (1):**
- Mode-switch with H.264 transition — included in the initial PR so the file shape demonstrates the "no Firefox sibling for H.264-touching scenarios" pattern.

Subsequent PRs convert the remaining ~30 Firefox-eligible candidates, ~5 per PR.

---

## Data flow

For both browsers, the per-test sequence is identical:

```
1. launch_scenario_stack() — headscale + ghostframe-server containers + TestNode forwarders
2. <BrowserImpl>::new(&stack) — opens browser, ready to navigate
3. browser.new_page(stack.page_url()) — load the served HTML
4. wait_for_status(browser, "Receiving frames") — poll evaluate() against status text
5. <test-specific assertions> — evaluate() for tile counts / RAF timestamps; screenshot() + SSIM
6. browser.close() — orderly teardown
7. stack drops — containers + forwarders torn down
```

The only difference per-browser is steps 2 + 3's underlying protocol (CDP vs WebDriver). Everything else is the trait or pure host-side scenario logic.

---

## Error handling

- **Missing host prerequisite (firefox / geckodriver / certutil):** `FirefoxSession::new` returns `Err` with a one-line message naming the missing tool and the install package per major distro. No silent skip.
- **Geckodriver fails to come up within 10 s:** `FirefoxSession::new` returns `Err` with the captured `[geckodriver]` stderr tail.
- **`certutil -A` fails:** captured stdout/stderr included in the error; common cause is an unreadable cert PEM, surfaced verbatim.
- **WebDriver session creation fails:** fantoccini's error is wrapped with the geckodriver port and the profile path so a hung session is debuggable.
- **`evaluate()` returns a value that can't be deserialized into `T`:** error includes the script that ran and the raw JSON, mirroring how `chromiumoxide`'s deserialization errors flow today.

---

## Testing

The Firefox impl itself gets two kinds of coverage:

1. **The trait at the seam.** Each of the 8 initial conversions exercises `new_page`, `evaluate`, `screenshot`, and `close` against `FirefoxSession`. If any method is wrong, every Firefox test fails the same way — easy to diagnose.

2. **Cert path.** The first Firefox test that runs end-to-end implicitly verifies that the certutil import worked and that Firefox trusts the static cert. No separate unit test needed; failure mode is a TLS error, easy to spot.

No separate unit tests for `FirefoxSession`. The functional smoke is the trait surface across the 8 scenarios.

---

## Out of scope

- H.264 test conversions — known Firefox limitation; see D7. Future work plumbs `h264Supported` through HELLO so the server stops sending H.264 to Firefox; that work is unblocked by this spec but lives in its own design.
- Containerizing Firefox — only revisited if host-installed Firefox proves flaky across CI versions; D2.
- Migrating Chromium off CDP to WebDriver — explicitly rejected; D1.
- Macro-driven browser-matrix attributes — explicitly rejected; D4.
- Splitting `tests/e2e.rs` into multiple files — orthogonal refactor; this work doesn't depend on it.
