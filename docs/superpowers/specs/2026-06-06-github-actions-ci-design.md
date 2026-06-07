# GitHub Actions CI for Ghostframe

**Date:** 2026-06-06
**Status:** design
**Author:** Cedric Bail (with Claude)

## Goal

Add continuous integration on GitHub Actions for the Ghostframe repository.
Every pull request and every push to `master` must pass a fast battery of
checks (lint, format, unit tests, release build, web-client build, FFI header
consistency, Go shim build) before merge. A second, slower tier runs the full
Rust end-to-end suite. A nightly sweep re-runs the e2e suite with retries to
catch known flakes without polluting per-PR signal.

## Non-goals

- Running on macOS or Windows. Ghostframe is Linux-only by design; multi-OS CI
  would test code paths that aren't shipped.
- Running the GPU-pipeline e2e subset on GitHub-hosted runners. Those tests
  require the host kernel's `vkms` module loaded with `enable_writeback=1`,
  which GitHub-hosted runners do not allow. They remain runnable locally on a
  machine with VKMS, and a future self-hosted-runner tier (out of scope here)
  may cover them.
- Configuring branch-protection rules from the workflow itself. Branch
  protection lives outside the repo and changes are audit-logged; the spec
  recommends settings and provides the `gh` command, but applying them is a
  manual step.
- Replacing the `Justfile`. `just test-unit` / `just test-e2e` / `just lint`
  remain the canonical developer-facing entry points. CI re-uses them where
  they fit; CI does not invent parallel ones.

## Constraints

- **GitHub-hosted runners only** (`ubuntu-24.04`). Self-hosted is explicitly
  out of scope for this iteration.
- **No kernel module loading**. Rules out VKMS, which rules out the GPU-pipeline
  e2e subset.
- **Sequential e2e by construction**. The e2e harness requires
  `--test-threads=1`; splitting tests across parallel workers is a separate
  design problem and not part of this work.
- **Docker daemon available**. GitHub-hosted Ubuntu runners ship with Docker
  preinstalled, which the e2e harness needs (via `testcontainers`).

## Architecture: three workflows

```
.github/workflows/
├── ci.yml              # fast tier (PR + push to master, required)
├── e2e.yml             # e2e tier (PR + push to master, required)
└── nightly.yml         # nightly sweep (scheduled, informational)
```

The fast and e2e tiers are separate workflows (not jobs in one workflow) so
that a lint failure surfaces in minutes without the runner queue waiting on
the heavy e2e tier to start. Each workflow has its own concurrency group keyed
on `${{ github.ref }}` with `cancel-in-progress: true`, so a force-push or
amended commit immediately cancels the prior run.

```
                    pull_request / push to master
                              │
                ┌─────────────┴─────────────┐
                ▼                           ▼
            ci.yml                       e2e.yml
        (fast, parallel)             (slow, sequential)
                │                           │
                └───────────┬───────────────┘
                            ▼
                   all required checks
                       must be green
                            │
                            ▼
                         merge

                  ┌── schedule: 03:00 UTC ──┐
                  ▼                         │
              nightly.yml                   │
       (full e2e + nextest retries)         │
                  │                         │
                  └─→ opens issue on red ───┘
```

## Tier 1: `ci.yml` (fast)

Seven parallel jobs, all on `ubuntu-24.04`. Each job owns one concern, so
failure logs are isolated and a flake in one doesn't waste runner minutes
re-running the others.

| Job | Command | Wall-clock budget | Cache |
|---|---|---|---|
| `fmt` | `cargo fmt --all -- --check` | <1 min | none |
| `clippy` | `cargo clippy --workspace --all-targets -- -D warnings` | 5–10 min cold / 1–2 min warm | `Swatinem/rust-cache@v2`, shared-key `ci-fast` |
| `unit` | `cargo test --workspace --lib` | 5–10 min cold / 2–3 min warm | shares `ci-fast` |
| `build-release` | `cargo build --workspace --release --exclude ghostframe-e2e` | 8–12 min cold / 2–3 min warm | `Swatinem/rust-cache@v2`, shared-key `ci-release` |
| `web-client` | `npm ci && npm run build && npx tsc --noEmit` in `ghostframe-web-client/` | 3–5 min | `actions/setup-node@v4 cache: npm` |
| `cbindgen-check` | `cargo check -p ghostframe-lib && git diff --exit-code ghostframe-lib/include/ghostframe.h` | 5–7 min cold / 1–2 min warm | shares `ci-fast` |
| `go-vet` | `cd ghostbridge && go vet ./... && go build ./...` | 1–2 min | `actions/setup-go@v5` default Go-module cache |

### Notes per job

- **`fmt`** is split from `clippy` so format failures surface within ~30 seconds
  rather than after a clippy cold-cache rebuild. It is the most common
  first-PR-from-a-new-contributor stumble.
- **`clippy --all-targets`** covers `--bins`, `--tests`, `--benches`, and
  `--examples`. A clippy warning in `ghostframe-bench` therefore blocks merge,
  matching `just lint`'s scope.
- **`unit`** runs the workspace-wide library test suite (currently 332+
  tests per `MEMORY.md`). It excludes integration tests under `tests/` — those
  live in tier 2.
- **`build-release`** excludes `ghostframe-e2e` because the e2e crate is a
  test-harness library that has no release binary and pulls in heavy
  dependencies (chromiumoxide, testcontainers, image-compare) that would
  bloat the cache without catching any real release-only breakage.
- **`web-client`** runs `npm ci` (not `npm install`) for deterministic
  installs from `package-lock.json`. `tsc --noEmit` catches type errors that
  Vite's bundler tolerates. The job also uploads `ghostframe-web-client/dist/`
  as an artifact named `web-client-dist` (used by `e2e.yml`).
- **`cbindgen-check`** exploits the fact that `ghostframe-lib/build.rs`
  invokes `cbindgen` on every build and writes
  `ghostframe-lib/include/ghostframe.h`. A `cargo check` is enough to
  re-trigger generation; the subsequent `git diff --exit-code` fails if the
  committed header is stale. No separate `cbindgen` install required.
- **`go-vet`** covers the Go FFI shim. `ghostbridge` has no `_test.go` files;
  `go test ./...` would be a no-op. `go vet` + `go build` are the meaningful
  checks.

### Parallelism and total wall-clock

All seven jobs run in parallel. Cold-cache wall-clock is bounded by the
slowest single job (`build-release`, ~12 min). Warm-cache wall-clock for a
typical PR is bounded by `web-client` (~3 min) since the Rust jobs all hit
warm caches.

## Tier 2: `e2e.yml`

Three jobs. Two are small Rust integration tests that don't need Docker; the
third is the main e2e suite.

| Job | Command | Wall-clock budget |
|---|---|---|
| `loopback-h3` | `cargo test --test loopback_h3` | 2–3 min |
| `harness-smoke` | `cargo test --test harness_smoke` | 5–8 min |
| `e2e` | the full e2e suite minus the VKMS-gated subset (see below) | 25–40 min |

### `e2e` job shape

1. **Checkout** with `actions/checkout@v4` (no fetch-depth tuning needed).
2. **Install system runtime deps** via the `_setup-system-deps` composite
   action (see "Repository artifacts"): packages mirroring what the
   test-server Dockerfile installs at runtime (`ffmpeg` libs, `x264`,
   `vulkan-tools`, plus `clang`/`libclang-dev` for host-side `bindgen`).
3. **Install toolchains**: `dtolnay/rust-toolchain@stable`,
   `actions/setup-node@v4`, `actions/setup-go@v5`.
4. **Restore caches** in this order:
   - `Swatinem/rust-cache@v2` with `shared-key: e2e`. **Distinct from the
     `ci-fast` / `ci-release` keys** because the e2e crate compiles
     `ghostframe-lib` with `test-loss-injection` + `cdf53-diag` features
     (see `tests/containers/test-server/Dockerfile`), which produces different
     rlibs. A shared cache would thrash on every run.
   - `docker/setup-buildx-action@v3`, then `docker/build-push-action@v5` for
     each image (`ghostframe/test-server`, `ghostframe/test-headscale`) with
     `cache-from: type=gha,scope=test-server` /
     `cache-to: type=gha,mode=max,scope=test-server` (separate scope per
     image). `mode=max` exports all intermediate layers; the Rust-build layer
     is the one we most want cached.
   - `actions/setup-node@v4` cache for `ghostframe-web-client/node_modules`.
5. **Download `web-client-dist` artifact** from the `ci.yml / web-client` job.
   The e2e harness serves `ghostframe-web-client/dist/` over HTTP; building it
   twice per PR (once in `ci.yml`, once in `e2e.yml`) is waste. Cross-workflow
   artifact download via `actions/download-artifact@v4` with `run-id` is the
   standard pattern.
6. **Build containers** equivalent to `just containers-build`. Both the
   host-side `cargo build --release -p ghostframe-xdaemon -p ghostframe-test-pattern`
   (rust-cache hit) and the two `docker build` invocations (buildx-gha hit)
   are cached.
7. **Run tests**:
   ```bash
   cargo test --test e2e -- \
     --test-threads=1 \
     $(awk '/^[^#]/ {printf "--skip %s ", $1}' ci/skip-list.txt)
   ```

### VKMS-gated skip list

The skip list lives in `ci/skip-list.txt` (one substring per line, `#`
comments allowed) rather than inline in the workflow YAML. Reasons:

- The list is reviewed as part of the same PR that introduces a new GPU test,
  not as a separate workflow edit.
- A contributor wondering "why isn't my test running in CI" finds the answer
  with `grep` in one obvious place rather than scrolling through workflow YAML.

The implementation step enumerates the list by `grep -nB 20 'fn e2e_'
ghostframe-e2e/tests/e2e.rs | grep -E 'fn e2e_|--drm-direct|setup_e2e_webgpu_gpu'`
and pinning every test whose body invokes `setup_e2e_webgpu_gpu` or passes
`--drm-direct` to the test pattern. As of this spec the list is approximately
~20 tests of the 41 in the suite (roughly half), centered on
`e2e_cdf53_*`, `e2e_mode_switch`, `e2e_palrle_exact_pixels`,
`e2e_palrle_session_reset`, `e2e_text_clarity`, `e2e_progressive_refinement`,
`e2e_refinement_cancel`, and several others. The exact list is the
implementation's responsibility, not the spec's — it will rot if pinned here.

`cargo test --skip <name>` matches as a substring on the test path, so a
single entry like `cdf53` covers all `e2e_cdf53_*` tests. Where a single
entry exempts a whole family, use it; where two tests share no common
substring beyond `e2e_`, list each explicitly. Comment lines in
`ci/skip-list.txt` explain *why* each entry is gated (e.g. "needs VKMS for
`--drm-direct`", "needs VKMS for `setup_e2e_webgpu_gpu`").

### Already-`#[ignore]`'d tests stay ignored

Three e2e tests carry `#[ignore]` annotations on `master` today
(`e2e_resolution_change`, plus M3.3b diagnostics
`e2e_cdf53_live_tile_state_col18` and `e2e_cdf53_live_tile_state`). CI does
**not** pass `--include-ignored`, so they remain skipped regardless of the
`ci/skip-list.txt` contents.

### Failure-mode artifact upload

On `failure()`, the `e2e` job uploads:

- `docker logs ghostframe-server` and `docker logs headscale`
- Any failed-frame dumps written under `/tmp/ghostframe-*` by the harness
  (`GHOSTFRAME_DUMP_FRAME` and related diagnose env vars are not enabled in
  CI, but if a future test enables one, the dump path is captured)
- The captured stdout/stderr of the test process, which contains the
  structured JSON server logs (`GHOSTFRAME_LOG_FORMAT=json` is set by the
  harness) and the `RUST_LOG=trace,debug` tracing output

Using `actions/upload-artifact@v4` with `if-no-files-found: ignore` and
`retention-days: 7`. This is the only way to debug e2e flakes on a
GitHub-hosted runner since SSH access is not available.

## Tier 3: `nightly.yml`

Triggers: `cron: '0 3 * * *'` (03:00 UTC) plus `workflow_dispatch`.

### Differences from tier 2

1. **Retries via `cargo-nextest`**. Installed with
   `taiki-e/install-action@nextest`. The e2e suite runs as:
   ```bash
   cargo nextest run \
     --test e2e \
     --no-fail-fast \
     --retries 2 \
     --test-threads 1 \
     -E "$(./ci/skip-list-to-nextest-expr.sh)"
   ```
   The same `ci/skip-list.txt` file is reused. The helper script
   `ci/skip-list-to-nextest-expr.sh` reads each non-comment line and emits a
   single nextest filter expression of the form
   `not test(/A/) and not test(/B/) and ...`. The substring entries become
   regex fragments inside `test(/.../)`. A test that fails three times in a row is a real
   failure; a test that fails once and passes on retry is flagged in the
   report but doesn't fail the workflow. This targets the known flakes
   documented in `MEMORY.md`: `e2e_progressive_refinement` (passes in
   isolation, drifts under sequential sweep) and the classifier-hysteresis
   sensitivity from M3.6b.

2. **No caching tricks**. The nightly is a from-cold run: no
   `Swatinem/rust-cache`, no buildx-gha cache, no node_modules cache. The
   point is to catch what a fresh contributor checkout would see — cache
   poisoning or cache-key collisions would silently hide from a permanently
   cached PR workflow.

3. **Issue on failure**. On non-`success()` conclusion, the workflow opens a
   GitHub issue tagged `nightly-failure` with the run URL and the per-test
   retry summary that nextest emits as JUnit XML. Subsequent failures comment
   on the existing open issue rather than spawning duplicates (using the
   issue title as the dedup key). Closing the issue manually after a fix
   resets the cycle. Implementation via inline `gh issue create` /
   `gh issue list --label nightly-failure --state open` /
   `gh issue comment`.

### What nightly does not do

- It does not attempt the VKMS-gated GPU subset. Without `vkms` on the
  runner, retrying them is wasted minutes.
- It does not run on a different OS or runner image. Linux-only.
- It does not gate merges. A nightly red opens an issue you triage.

## Caching strategy

Three independent caches, all scoped narrowly so a poisoned cache in one
tier cannot corrupt the others.

| Cache | Action | Key strategy | Stored |
|---|---|---|---|
| Rust artifacts | `Swatinem/rust-cache@v2` | Default key (hash of `Cargo.lock` + toolchain + workflow file). **Per-workflow `shared-key`**: `ci-fast` for fmt/clippy/unit/cbindgen-check in `ci.yml`, `ci-release` for `build-release`, `e2e` for `e2e.yml`. They never share. | GHA cache (10 GB per repo) |
| Docker layers | `docker/build-push-action@v5` with `cache-from: type=gha,scope=<image>` and `cache-to: type=gha,mode=max,scope=<image>` | One scope per image: `test-server`, `test-headscale`. Buildx invalidates automatically when the `COPY` source content changes. | GHA cache (same 10 GB pool) |
| npm | `actions/setup-node@v4` with `cache: 'npm'` and `cache-dependency-path: ghostframe-web-client/package-lock.json` | Setup-node hashes the lockfile and restores `~/.npm`. `npm ci` then installs deterministically. | GHA cache |
| Go modules | `actions/setup-go@v5` default | Hashes `go.sum` automatically. | GHA cache |

### What is intentionally not cached

- The raw `target/` directory directly: `rust-cache` handles it more
  carefully than `actions/cache` would (it prunes incremental compilation
  artifacts that bloat the cache).
- The chromiumoxide browser binary: chromiumoxide downloads it on first run;
  it's a one-time per-runner cost and not worth a cache round-trip given the
  GHA cache size budget.
- The headscale base image: it's small and built from a public base; let the
  Docker pull cache handle it.

### Cache pressure and eviction

GHA cache is LRU with a 10 GB ceiling across the whole repo. Three
implications:

- **Branch isolation**: cache entries are scoped to the branch they were
  written on, with fallback to the default branch. PRs that don't yet touch
  `Cargo.lock` get `master`'s cache for free; once a PR modifies it, that PR
  gets its own per-branch cache and `master`'s stays clean.
- **No manual cache invalidation key**: no `CACHE_VERSION` env var. If a
  cache goes bad, delete it from the Actions UI or force a key change by
  bumping `Cargo.lock`.
- **Rust-cache miss is the expensive one** (~10 min full rebuild). Buildx
  and npm misses are cheap. The nightly intentionally writes nothing so it
  doesn't pollute the pool.

## Required vs informational checks

The spec recommends (does not configure) the following branch-protection
rule for `master`:

### Required for merge

- `ci.yml / fmt`
- `ci.yml / clippy`
- `ci.yml / unit`
- `ci.yml / build-release`
- `ci.yml / web-client`
- `ci.yml / cbindgen-check`
- `ci.yml / go-vet`
- `e2e.yml / loopback-h3`
- `e2e.yml / harness-smoke`
- `e2e.yml / e2e`

### Not required

- All `nightly.yml` jobs.

### Other branch-protection recommendations

- **Require status checks to pass before merging**: ✅ (the ten above).
- **Require branches to be up to date before merging**: ❌. Forces re-runs of
  CI on every merge of a parallel PR and burns minutes for no real signal on
  a solo project.
- **Require linear history**: optional; consistent with the existing
  fast-forward-merge workflow.
- **Allow force-pushes to `master`**: ❌ defensive default.
- **Restrict who can push to `master`**: just the owner.

### Applying the rule

The spec includes the exact `gh api` PUT command in `docs/ci.md` so it's one
paste away. Job names in YAML are pinned with explicit `name:` fields so a
rename can't silently break the required-checks contract.

## Repository artifacts

| Path | Commit | Purpose |
|---|---|---|
| `.github/workflows/ci.yml` | CI commit | Fast tier |
| `.github/workflows/e2e.yml` | CI commit | E2E tier |
| `.github/workflows/nightly.yml` | CI commit | Nightly sweep |
| `.github/workflows/_setup-system-deps/action.yml` | CI commit | Shared `apt-get` composite (used by `e2e.yml` and `nightly.yml`) |
| `ci/skip-list.txt` | CI commit | VKMS-gated e2e test names (substring filters), one per line, `#` comments |
| `ci/skip-list-to-nextest-expr.sh` | CI commit | Tiny shell helper that reads `ci/skip-list.txt` and emits the nextest `-E` filter expression for `nightly.yml` |
| `docs/ci.md` | CI commit | Contributor doc: which checks gate merge, how to reproduce locally, the nightly-issue triage flow, the `gh api` snippet for branch protection |
| `Justfile` (updated) | CI commit | Add `ci-local` target composing existing fast-tier recipes |
| `README.md` (updated) | CI commit | One line in the Contributing section: "PRs must pass the checks described in `docs/ci.md`." |
| `.github/dependabot.yml` | **Separate commit, after CI commit** | Dependency update PRs (see below) |

### Dependabot config (separate commit)

Five ecosystems, all weekly on Monday:

| Ecosystem | Directory | Group strategy |
|---|---|---|
| `cargo` | `/` | Minor + patch grouped into one PR; major as individual PRs |
| `npm` | `/ghostframe-web-client` | Minor + patch grouped |
| `gomod` | `/ghostbridge` | All grouped (few deps) |
| `github-actions` | `/` | All grouped (keeps action pins current) |
| `docker` | `/tests/containers/test-server` and `/tests/containers/headscale` (two entries) | All grouped |

Labels: `dependencies` plus one per ecosystem (`rust`, `npm`, `go`,
`actions`, `docker`). Open-PR limit: `5` per ecosystem (Dependabot default).
No security-only mode; repo-level security alerts fire independently.

### Why dependabot is a separate commit

The CI workflows must exist first so Dependabot's PRs land on a branch where
CI actually gates them. Order:

1. Merge the CI commit.
2. Configure branch-protection rules (manual step, `gh api` snippet in
   `docs/ci.md`).
3. Merge the Dependabot commit. Dependabot's first batch of PRs lands and
   gets gated by the now-configured rules.

## Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| GHA cache thrash between fast and e2e tiers | Cold rebuilds on every PR (~30 min wall-clock waste) | Distinct `shared-key` per tier, documented above |
| `cargo test --test e2e --skip` semantics change | Skip list silently stops working | Add a smoke assertion in `harness-smoke` that the skip-list file parses; covered by tier 2 |
| Cross-workflow artifact download (`web-client-dist`) breaks | E2E starts duplicating npm work | Fallback: if download fails, e2e rebuilds locally. Worst case is 3 extra minutes per PR. |
| Nightly false-positive issue spam | Inbox noise, ignored signal | Comment-on-existing-open-issue dedup, plus the `nightly-failure` label for easy filtering |
| Dependabot security update breaks CI | Stalled security fix | Group strategy keeps PRs small; a major-bump PR is individual and reviewed manually |
| Buildx `mode=max` cache exceeds 10 GB | Eviction churn affecting other tiers | Monitor; if it bites, fall back to `mode=min` (only the final layer is exported) |
| `setup_e2e_webgpu_gpu` renamed in a future refactor | Skip-list maintenance burden grows | The list is grepped at implementation time but maintained by hand thereafter; any rename PR must update both call sites and the skip-list comment |

## Open questions resolved during exploration

- **cbindgen header location**: confirmed `ghostframe-lib/include/ghostframe.h`,
  committed to git, regenerated by `ghostframe-lib/build.rs` on every build.
  `cbindgen-check` is `cargo check + git diff --exit-code`, no `cargo install`
  step needed.
- **Go side**: `ghostbridge` is a `main` package with no `_test.go` files.
  `go vet ./...` and `go build ./...` are the only meaningful Go-side checks.
- **GPU subset size**: ~20 of 41 tests (not 5–10 as I estimated in
  conversation). The skip list will list every one explicitly. The shape of
  the workflow design is unchanged.
- **Already-`#[ignore]`'d**: three tests
  (`e2e_resolution_change`, `e2e_cdf53_live_tile_state_col18`,
  `e2e_cdf53_live_tile_state`). They stay ignored; CI does not pass
  `--include-ignored`.

## Open questions deferred to implementation

- Exact e2e test names in `ci/skip-list.txt`. Method: grep
  `setup_e2e_webgpu_gpu` and `--drm-direct` callers; pin each by name with a
  short rationale comment.
- The exact `apt-get install` package list in
  `_setup-system-deps/action.yml`. Method: mirror the runtime stage of
  `tests/containers/test-server/Dockerfile`, minus the
  `xserver-xorg-*` packages (those run inside the container, not on the
  host).
- The `gh api` PUT payload for branch-protection. Method: pin the JSON in
  `docs/ci.md` once the workflow job names are final.

## Out of scope (explicit follow-ups)

These are recognized but deliberately not part of this design:

- **Self-hosted runner tier for GPU e2e.** Requires a dedicated Linux box with
  `vkms enable_writeback=1`, host Xorg ignore-VKMS config, and a registered
  GitHub Actions runner. Worth its own spec.
- **Cross-PR cache sharing optimizations**. The current design accepts that
  PRs starting from `master`'s warm cache pay one cold rebuild on the first
  push that touches `Cargo.lock`. A `sccache` + S3-backed-cache setup could
  improve this but adds infrastructure.
- **CI-side flake detection beyond nextest retries**. A reporting bot that
  classifies failures (compile vs assertion vs timeout) and posts trend
  graphs is out of scope.
- **Test result publishing** (e.g. JUnit XML to a dashboard). Possible
  follow-up once nextest is in place; the JSON output is already structured.
- **macOS / Windows runners**. Ruled out by the Linux-only project scope.
- **Codecov / coverage reporting**. Possible follow-up; not part of this
  initial CI.

## Spec self-review notes

- No placeholders or TBDs in the design itself; the "deferred to
  implementation" section is intentional (concrete enumeration done at
  implementation time).
- Internally consistent: caching strategy, required-checks list, and
  workflow file layout all reference the same job names.
- Scoped to one implementation plan: ~10 new files + 2 small updates, in
  two commits (CI workflows + dependabot).
- Ambiguity check: the skip-list grouping (substring vs explicit name) is
  the only place where the spec defers a tactical decision to
  implementation, and the rule is explicit (use a substring when it covers
  a whole family; list explicitly otherwise).
