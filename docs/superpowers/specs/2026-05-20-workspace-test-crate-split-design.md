# Workspace test-crate split — design

**Date:** 2026-05-20
**Scope:** `ghostframe-lib`'s integration tests + benches → two new workspace crates
**Status:** Draft → ready for implementation planning

## Background

`cargo test --lib --package ghostframe-lib` and `cargo build --tests --package
ghostframe-lib` link **16 dev-dependencies** into the test compilation unit. Of
these, **6 heavy crates serve only integration tests or benches** and are not
used anywhere in `ghostframe-lib/src/`:

| Dep | Used in lib src? | Used in tests/e2e.rs | Used in tests/loopback_h3.rs | Used in benches/ |
|---|---|---|---|---|
| `chromiumoxide` | 0 | ✓ | 0 | 0 |
| `testcontainers` | 0 | ✓ | 0 | 0 |
| `axum` | 0 | 0 | ✓ | 0 |
| `tower-http` | 0 | 0 | ✓ | 0 |
| `criterion` | 0 | 0 | 0 | ✓ (2 files) |
| `iai-callgrind` | 0 | 0 | 0 | ✓ (1 file) |
| `image` | 0 | ✓ | 0 | 0 |
| `image-compare` | 0 | ✓ | 0 | 0 |

A recent structural refactor of `gpu_pipeline.rs` invalidated the build cache
and triggered a cold-build compilation that peaked rustc at **19 GB RSS**,
killing user processes via the kernel OOM-killer. Steady-state incremental
builds peak at ~900 MB; the cold-build spike is amplified by the size of the
test compilation unit, which has to monomorphize and link all dev-deps into
one rustc invocation.

Splitting these dev-deps into separate workspace crates puts them in their
own rustc invocations (smaller per-process peak) and removes them entirely
from `cargo test --lib --package ghostframe-lib`.

## Goals and non-goals

**Goals.**
- Remove 8 dev-deps from `ghostframe-lib`'s `[dev-dependencies]` (the 6 heavy
  ones above, plus `image` and `image-compare` which serve e2e).
- Preserve test/bench discoverability: `cargo test --workspace` still runs
  everything; individual packages can be tested in isolation.
- Land in small, individually revertible commits.

**Non-goals.**
- Changing any test logic, bench logic, or test/bench names.
- Touching `ghostframe-lib/src/` (the production code).
- Changing `tests/gpu_pipeline.rs` (it has no heavy deps; stays in `ghostframe-lib`).
- Changing public API of `ghostframe-lib` (integration tests already only see `pub` items).
- Adding new tests or benches.

## Target workspace layout

```
ghostframe/
├── ghostframe-lib/
│   ├── src/                       # unchanged
│   ├── tests/
│   │   └── gpu_pipeline.rs        # stays — no heavy deps
│   └── Cargo.toml                 # [dev-dependencies] shrinks by 8
├── ghostframe-test-pattern/       # unchanged
├── ghostframe-xdaemon/            # unchanged
├── ghostframe-e2e/                # NEW
│   ├── Cargo.toml
│   ├── src/lib.rs                 # empty placeholder
│   └── tests/
│       ├── e2e.rs                 # MOVED from ghostframe-lib/tests/
│       ├── e2e/                   # MOVED — helpers.rs + golden/ fixtures
│       │   ├── helpers.rs
│       │   └── golden/
│       └── loopback_h3.rs         # MOVED
├── ghostframe-bench/              # NEW
│   ├── Cargo.toml
│   ├── src/lib.rs                 # empty placeholder
│   └── benches/
│       ├── codec_callgrind.rs     # MOVED
│       ├── codec_latency.rs       # MOVED
│       ├── pipeline_throughput.rs # MOVED
│       └── fixtures/              # MOVED
└── Cargo.toml                     # workspace members += ghostframe-e2e, ghostframe-bench
```

### `ghostframe-e2e/Cargo.toml`

```toml
[package]
name = "ghostframe-e2e"
version = "0.1.0"
edition = "2021"

[dependencies]
ghostframe-lib = { path = "../ghostframe-lib", features = ["..."] }
ghostframe-test-pattern = { path = "../ghostframe-test-pattern" }

[dev-dependencies]
anyhow = { workspace = true }
chromiumoxide = "..."
testcontainers = "..."
axum = "..."
tower-http = "..."
image = "..."
image-compare = "..."
serde_json = "..."
tokio = { workspace = true, features = [...] }
tracing-subscriber = { workspace = true }
url = "..."
futures = "..."
```

The exact `features = [...]` list on `ghostframe-lib` is determined during plan
writing by grepping `tests/e2e.rs` and `tests/loopback_h3.rs` for any
`#[cfg(feature = "...")]` and `cfg!(feature = "...")` calls. At minimum,
`test-loss-injection` is likely needed.

Dep versions are copied from `ghostframe-lib/Cargo.toml`'s current
`[dev-dependencies]`. Where a dep also exists in `[workspace.dependencies]`,
prefer `{ workspace = true }` to keep versions pinned in one place.

### `ghostframe-bench/Cargo.toml`

```toml
[package]
name = "ghostframe-bench"
version = "0.1.0"
edition = "2021"

[dependencies]
ghostframe-lib = { path = "../ghostframe-lib" }
ghostframe-test-pattern = { path = "../ghostframe-test-pattern" }

[dev-dependencies]
criterion = "..."
iai-callgrind = "..."

[[bench]]
name = "codec_callgrind"
harness = false

[[bench]]
name = "codec_latency"
harness = false

[[bench]]
name = "pipeline_throughput"
harness = false
```

The `harness = false` declarations migrate from `ghostframe-lib/Cargo.toml`.

### `ghostframe-lib/Cargo.toml` `[dev-dependencies]` after split

Remove:
- `chromiumoxide`
- `testcontainers`
- `axum`
- `tower-http`
- `criterion`
- `iai-callgrind`
- `image`
- `image-compare`

Keep (still used by `ghostframe-lib/src/` `#[cfg(test)]` mods or by
`tests/gpu_pipeline.rs`):
- `anyhow`
- `futures`
- `lz4_flex`
- `proptest`
- `serde_json`
- `tracing-subscriber`
- `url`
- `ghostframe-test-pattern`

The corresponding `[[bench]]` entries in `ghostframe-lib/Cargo.toml` also
migrate out.

### Root `Cargo.toml` workspace members

```toml
[workspace]
members = [
    "ghostframe-lib",
    "ghostframe-xdaemon",
    "ghostframe-test-pattern",
    "ghostframe-e2e",       # added
    "ghostframe-bench",     # added
]
```

## Invocation semantics after the split

| Command | What runs |
|---|---|
| `cargo test --package ghostframe-lib` | Lib unit tests + `tests/gpu_pipeline.rs`. **Does NOT compile the 6 heavy deps.** |
| `cargo test --package ghostframe-e2e` | E2E + loopback_h3 tests. |
| `cargo bench --package ghostframe-bench` | Benches. |
| `cargo test --workspace` | Everything. Each package compiles in its own rustc invocation, so peak RSS per process stays bounded. |

## Migration order (5 commits)

1. **Create skeletons.** Both new crates' `Cargo.toml` + empty `src/lib.rs` + workspace member additions. No file moves. `cargo build --workspace` passes.
2. **Move e2e tests.** `ghostframe-lib/tests/{e2e.rs, e2e/, loopback_h3.rs}` → `ghostframe-e2e/tests/`. `cargo build --package ghostframe-e2e --tests` compiles. Don't yet remove deps from ghostframe-lib.
3. **Move benches.** `ghostframe-lib/benches/*` → `ghostframe-bench/benches/`. `cargo build --package ghostframe-bench --benches` compiles. `[[bench]]` declarations migrate.
4. **Trim ghostframe-lib's `[dev-dependencies]`** — remove the 8 evicted deps and the migrated `[[bench]]` entries. `cargo build --tests --package ghostframe-lib` succeeds without them. This is the payoff commit.
5. **Optional: re-measure peak RSS** to document the win. No code change.

Each step is independently revertible.

## Risks

1. **Feature flag inheritance.** Integration tests today rely on
   `ghostframe-lib`'s features being unified via the workspace's dev-dep edges.
   After the split, `ghostframe-e2e`'s path dependency on `ghostframe-lib`
   needs to enable any feature the tests use (likely `test-loss-injection`,
   possibly others). Determine the exact set by grep during plan writing.

2. **Workspace dep version drift.** New crates should use `{ workspace = true }`
   for deps already pinned in `[workspace.dependencies]` (tokio, bytes,
   quinn-proto, rcgen, rustls, thiserror, tracing, tracing-subscriber, libc,
   sha2, hex, time, anyhow, futures, ffmpeg-next, ash). Other versions copy
   verbatim from the current `ghostframe-lib/Cargo.toml`.

3. **`tests/e2e/helpers.rs` path resolution.** Current layout uses
   `#[path = "e2e/helpers.rs"] mod helpers;` (or similar) inside `tests/e2e.rs`
   to share code with sub-test files. After move, the relative path is
   identical inside `ghostframe-e2e/tests/`, so no source edits needed —
   verify on first compile.

4. **`harness = false` benches.** Each bench file declares its own `main()`.
   The `[[bench]]` entries in Cargo.toml with `harness = false` MUST move with
   them, or cargo defaults to libtest harness and the bench fails to compile.

5. **CI / scripts referencing test paths.** If any CI script or Makefile
   target invokes `cargo test --package ghostframe-lib` and expects e2e tests
   in the result, it needs updating to `cargo test --workspace` (or to add
   the new package explicitly). Grep the repo during plan writing.

6. **No public API change required.** Integration tests are already a
   separate compilation unit that sees only `pub` items of `ghostframe-lib`.
   Moving them to a new crate preserves that constraint exactly.

## Verification

After each commit:
```
cargo build --workspace 2>&1 | tail -5         # must succeed
```

After commit 4 (the payoff):
```
# Measure peak RSS for the lib's test compile (incremental + cold-ish).
touch ghostframe-lib/src/lib.rs
python3 /tmp/measure_rss.py -- \
  systemd-run --user --scope -p MemoryMax=18G --quiet -- \
  cargo build --tests --package ghostframe-lib -j 1
```

Expected: peak well under 894 MB (the pre-split incremental peak). Should be
roughly proportional to the reduction in dep graph size — order-of-magnitude
expectation is 400–600 MB.

Functional verification per commit:
- After commit 2: `cargo test --package ghostframe-e2e --no-run` (compiles
  but doesn't run, since e2e tests need Docker/X server).
- After commit 3: `cargo build --package ghostframe-bench --benches` (just
  compiles).
- After commit 4: `cargo test --package ghostframe-lib -- --test-threads=1`
  (unit tests + tests/gpu_pipeline.rs). 282 + 5 = 287 tests, all pass.

## Rollback

Per-commit: `git revert <sha>`.

- Revert commit 4: dev-deps come back; `cargo test --package ghostframe-lib`
  again compiles everything. Memory peak returns to baseline. No correctness
  impact.
- Revert commit 3: benches return to ghostframe-lib; `ghostframe-bench` becomes
  an empty package. Could also revert commit 1 (skeleton creation) if rolling
  back fully.
- Revert commit 2: e2e tests return to ghostframe-lib; same pattern.
- Revert commit 1: new packages disappear from workspace.

## Done definition

- `ghostframe-e2e` and `ghostframe-bench` exist as workspace members.
- `ghostframe-lib/tests/` contains only `gpu_pipeline.rs`.
- `ghostframe-lib/benches/` no longer exists (or is empty).
- `ghostframe-lib/Cargo.toml` `[dev-dependencies]` no longer contains the 8
  evicted crates, and no `[[bench]]` entries.
- `cargo test --workspace` passes.
- `cargo build --tests --package ghostframe-lib -j 1` peaks at materially
  less RSS than 894 MB (expected: 400–600 MB).

## Post-split measurement (recorded after Task 5)

Incremental rebuild after `touch <crate>/src/lib.rs`, under
`systemd-run --user --scope -p MemoryMax=18G --quiet -- ... -j 1`:

| Command | Pre-split peak RSS | Post-split peak RSS | Delta |
|---|---|---|---|
| `cargo build --tests --package ghostframe-lib` | 894 MB | **453 MB** | **−49%** |
| `cargo build --tests --package ghostframe-e2e` | n/a (didn't exist) | 588 MB | — |

The lib's routine test-compile no longer pulls chromiumoxide,
testcontainers, axum, tower-http, criterion, iai-callgrind, image, or
image-compare into its compilation unit. `ghostframe-e2e` bears those
deps but in its own rustc invocation, so it doesn't penalise `cargo
test --package ghostframe-lib` or `cargo test --lib`.

Cold-build (full cache invalidation) peak should be roughly
proportional: the ~19 GB rustc spike that triggered the OOM during the
gpu_pipeline split is expected to be ~halved for routine lib-only
builds, and the e2e crate's spike is bounded by its own dep graph
(separate process, can't accumulate).

## Out of scope

- Reorganizing test logic or adding new tests.
- Promoting any `pub(crate)` items in `ghostframe-lib` to `pub`. If a test
  in the new e2e crate needs an item that's currently `pub(crate)`, that's a
  separate decision worth a follow-up commit (not in this batch).
- Changing the workspace structure beyond adding the two new members.
- Touching `ghostframe-web-client` or `ghostbridge`.
- Documentation rewrites beyond what new Cargo.tomls require.
