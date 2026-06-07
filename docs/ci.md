# Continuous integration

Ghostframe runs three GitHub Actions workflows. This page documents what
each one does, how to reproduce a failure locally, and the triage flow for
nightly-sweep issues.

## Workflows

### `ci.yml` — fast tier

Runs on every pull request and every push to `master`. Seven parallel jobs:

| Job | What it runs | Local equivalent |
|---|---|---|
| `fmt` | `cargo fmt --all -- --check` | `just fmt-check` |
| `clippy` | `cargo clippy --workspace --all-targets -- -D warnings` | `just lint` |
| `unit` | `cargo test --workspace --lib` | `just test-unit` |
| `build-release` | `cargo build --workspace --release --exclude ghostframe-e2e` | `cargo build --release` |
| `web-client` | `npm ci && npm run build && npx tsc --noEmit` | `just web-client-build` |
| `cbindgen-check` | `cargo check -p ghostframe-lib` + `git diff --exit-code ghostframe-lib/include/ghostframe.h` | `cargo check -p ghostframe-lib && git status` |
| `go-vet` | `go vet ./... && go build ./...` in `ghostbridge/` | `cd ghostbridge && go vet ./... && go build ./...` |

All seven are required for merge.

To run the whole fast tier locally in one shot: `just ci-local`.

### `e2e.yml` — end-to-end tier

Runs on every pull request and every push to `master`. Three jobs:

| Job | What it runs |
|---|---|
| `loopback-h3` | `cargo test --test loopback_h3` (sans-IO QUIC/H3) |
| `harness-smoke` | `cargo test --test harness_smoke` |
| `e2e` | full `cargo test --test e2e` suite minus the VKMS-gated subset |

All three are required for merge.

#### Why some e2e tests are skipped

The exact list lives in [`ci/skip-list.txt`](../ci/skip-list.txt), grouped
into two categories:

**Category 1 — VKMS-gated (~25 tests).** Tests whose body calls
`setup_e2e_webgpu_gpu(...)` or passes `--drm-direct` to the test pattern.
They require the host kernel's `vkms` module (`enable_writeback=1`) with
`/dev/dri` bind-mounted into the test-server container. GitHub-hosted
runners cannot load kernel modules.

**Category 2 — software H.264 decode too slow on 2-vCPU runners
(3 tests).** `e2e_h264_motion`, `e2e_h264_ssim_golden`, `e2e_multi_pattern`.
The GitHub-hosted `ubuntu-24.04` runner is 2 vCPU + 7 GB RAM with no real
GPU, and Chrome's software H.264 decode can't keep pace with the
server-side x264 encoder. By the time the test samples the canvas, the
decoder is still showing the first frame and the "did the spinner
animate?" assertion sees identical snapshots and fails. Chrome 148 stable
on the runner *does* support H.264 + WebCodecs (verified by the workflow's
"Probe Chromium codec support" step); the gap is purely runner-throughput.
These three remain runnable locally on any developer machine.

When a new GPU-pipeline e2e test lands, add its name to Category 1 in the
same PR. When a test no longer needs VKMS, remove it.

Developers on a machine with VKMS loaded run the full suite locally with
`just test-e2e`; the skip list is **not** consulted locally.

### `nightly.yml` — nightly sweep

Runs at 03:00 UTC daily and on `workflow_dispatch`. Same skip list as
`e2e.yml`, but uses `cargo-nextest run --retries 2` so flaky tests are
re-attempted before failing the run.

On failure, the workflow opens (or comments on) an issue labeled
`nightly-failure`. The nightly is **not required for merge** — it's a
post-merge canary.

#### Triaging a nightly-failure issue

1. Open the linked run; download the `nightly-nextest-report` artifact.
2. The report's `target/nextest/.../report.junit.xml` lists per-test
   attempts. A test that failed all three attempts is a real regression;
   one that passed on retry is a flake.
3. If the failure is in a test that's documented as flaky in
   `~/.claude/projects/-home-cedric-work-ghostframe/memory/feedback_e2e_test_isolation_flake.md`
   (or similar memory files), it's expected churn — comment on the issue
   and leave it open until the underlying flake is fixed.
4. If the failure is in a previously-stable test, it's a regression — open
   a PR that fixes it; close the issue with a reference to the fix commit.

## Reproducing CI failures locally

The CI runs `cargo` commands directly with no special environment beyond
the system packages installed by the
[`_setup-system-deps`](.github/workflows/_setup-system-deps/action.yml)
composite action. Mirroring locally:

```bash
# Install build deps (Ubuntu 24.04)
sudo apt-get install -y \
  build-essential pkg-config clang libclang-dev \
  libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavdevice-dev \
  libx264-dev \
  libx11-dev libxext-dev libxdamage-dev libdrm-dev

# Then mirror the fast tier
just ci-local

# Or mirror the e2e tier (requires Docker)
just test-e2e
```

For the e2e tier, the CI also passes `--skip` flags constructed from
`ci/skip-list.txt`. Locally, you typically want the full suite, so don't
pass `--skip` and just run `just test-e2e`.

## Configuring branch protection (one-time admin step)

The workflows produce status checks that should be marked required in the
repo's branch-protection rules for `master`. After the workflows have run
at least once (so GitHub knows the status-check names), apply:

```bash
gh api \
  --method PUT \
  -H "Accept: application/vnd.github+json" \
  repos/<OWNER>/<REPO>/branches/master/protection \
  -F required_status_checks[strict]=false \
  -F required_status_checks[contexts][]='fmt' \
  -F required_status_checks[contexts][]='clippy' \
  -F required_status_checks[contexts][]='unit' \
  -F required_status_checks[contexts][]='build-release' \
  -F required_status_checks[contexts][]='web-client' \
  -F required_status_checks[contexts][]='cbindgen-check' \
  -F required_status_checks[contexts][]='go-vet' \
  -F required_status_checks[contexts][]='loopback-h3' \
  -F required_status_checks[contexts][]='harness-smoke' \
  -F required_status_checks[contexts][]='e2e' \
  -F enforce_admins=false \
  -F required_pull_request_reviews= \
  -F restrictions=
```

(Replace `<OWNER>/<REPO>` with the actual repo path. `strict=false`
disables the "require branches to be up to date" rule that would force a
re-run of CI on every parallel merge — see the design spec for why we keep
this off.)
