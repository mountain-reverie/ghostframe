# Group EC: workspace structure, ecosystem fit, supply chain, CI, repo hygiene

Verified against worktree `ghostframe-review-2026-11-25` at `e85fb005`. Nothing was built.
The evidence comes from `cargo tree --offline`, `cargo metadata`, grep and git, plus
recompiling the SPIR-V into the scratchpad. Anything marked **UNVERIFIED** could not be
checked offline, and the implementer must confirm it before relying on it.

Cross-group ownership. **UF** owns the SAFETY-comment burn-down in `ghostframe-lib`,
`ghostframe-tsnet` and `ghostframe-client-capi`. **LD** owns the `pub` → `pub(crate)` work
in `ghostframe-lib` and the C API surface. EC supplies the lint switches and the
mechanism; it does not fix those crates' warnings.

---

## EC-1 (High): test features reach a workspace release build

**Impact.** `cargo build --release` at the root, and also `--workspace --exclude ghostframe-e2e`,
compiles the daemon with `test-loss-injection`, `browserless-harness` and `ffmpeg-oracle`
turned on. That build links the ffmpeg oracle and keeps the loss-injection knobs. Today
only a convention (`just build-release` builds `-p ghostframe-server` alone) and two
guards stand between this and a shipped binary. CI and ci-local each build the daemon
twice to work around it. Separately, `ghostframe-test-pattern` pulls the whole e2e
harness (chromiumoxide, testcontainers, axum, fantoccini) into its normal graph.

**Status at e85fb005: CONFIRMED.**
- `ghostframe-bench/Cargo.toml:28` has `ghostframe-e2e = { path = "../ghostframe-e2e" }`
  as a normal dependency, used only by `[[bin]] codec_report` (`:71-73`). Grep over
  `src/bin/codec_report/{main,runner,aggregate}.rs` shows it is the only user of
  `ghostframe_e2e`. The bench lib, fixtures and benches never name it.
- `ghostframe-e2e/Cargo.toml:17` asks for
  `ghostframe-lib = { …, features = ["test-loss-injection", "browserless-harness"] }`.
- `ghostframe-test-pattern/Cargo.toml:19` depends normally on `ghostframe-bench`.
  `cargo tree -p ghostframe-test-pattern -e normal -i chromiumoxide` prints
  `chromiumoxide ← ghostframe-e2e ← ghostframe-bench ← ghostframe-test-pattern`.
- Resolved `ghostframe-lib` features, from
  `cargo tree -e normal,build,features -i ghostframe-lib | grep -oE 'ghostframe-lib feature "[a-z0-9-]+"'`:

  | selection | features |
  |---|---|
  | `-p ghostframe-server` | `default` |
  | `--workspace --exclude ghostframe-e2e` | `browserless-harness`, `default`, `ffmpeg-oracle`, `test-loss-injection` |

- The workarounds are in `Justfile:302-310` (ci-local builds the workspace, then
  rebuilds `-p ghostframe-server`), `.github/workflows/ci.yml:179-188` (the same),
  `ci/check-release-features.sh` and the AGENTS.md trap paragraph.

**Root cause.** A bench binary that needs the test harness is a normal-dependency
target of a crate that a production-adjacent crate (`test-pattern`) also depends on.
Resolver 2 then unifies the harness's features into every invocation that selects
`ghostframe-bench` or `ghostframe-test-pattern`.

**Resolver-2 facts the design relies on** (Cargo book, "Feature resolver version 2"):
1. Features asked for by a **dev-dependency** are activated only when the invocation
   builds a target that needs dev-dependencies: tests, examples, benches. That covers
   `cargo test`, `cargo bench`, `cargo build/check/clippy --tests|--examples|--benches|--all-targets`
   and `cargo run --example`. Plain `cargo build`/`cargo build --release` (lib + bins)
   and `cargo build -p ghostframe-server` do **not** activate them. When an invocation
   does activate them, the unification is invocation-wide: `ghostframe-lib` is compiled
   once with the test features, for every package in that invocation, the
   `ghostframe-server` bin included.
2. A workspace member that **itself** has a normal dependency with test features (here
   `ghostframe-e2e` → `ghostframe-lib[test-loss-injection,browserless-harness]`) unifies
   those features into **any** invocation that selects it, `cargo build` included.
   Turning the bench edge into a dev-dependency does not fix `cargo build --workspace`
   while `ghostframe-e2e` is a selected member. That is why `default-members` is part of
   the fix.
3. `cargo tree -e normal,build` resolves without dev units (`HasDevUnits::No` when the
   dev edge kind is absent). It shows what a plain `cargo build` resolves, which is the
   basis of the existing guard. **UNVERIFIED in source for 1.96**, but
   `check-release-features.sh` already depends on it and it matches the table above.
4. Consequence, unchanged by this fix: CI's `cargo test --workspace --lib` selects
   `ghostframe-e2e`, so `ghostframe-lib`'s unit tests in CI run with the test features
   on, while `cargo test -p ghostframe-lib` runs them with the features off. That is
   acceptable and worth knowing: a test that asserts "feature off" behaviour only
   holds under `-p`.

**Design (chosen).** Move `codec_report` into `ghostframe-e2e` as a `[[bin]]`, so
`ghostframe-bench` loses its `ghostframe-e2e` dependency entirely. Add
`default-members` to the root manifest, leaving out `ghostframe-e2e`. Extend
`check-release-features.sh` to check the default-members and `--workspace --exclude ghostframe-e2e`
selections as well as `-p ghostframe-server`.
- Why `ghostframe-e2e` is the right home: the binary is a harness driver (AGPL, like
  e2e). e2e already normal-depends on everything it uses. `CARGO_BIN_EXE_codec_report`
  keeps working for its smoke test. The bench README already records that
  `test-pattern -> bench -> e2e` blocks the reverse edge, and that blocker goes away.
- **Rejected: `[[example]]` in `ghostframe-bench` with e2e as a dev-dependency.** It
  fixes `cargo build`, but `CARGO_BIN_EXE_codec_report` does not exist for examples, so
  the smoke test would need `cargo run --example` from inside a test. Every documented
  invocation would change anyway, and bench would keep the harness coupling.
- **Rejected: `[[bench]] harness = false`.** `cargo bench` semantics (it runs with
  `--bench` args and the bench profile) are wrong for a multi-minute Docker scene
  driver.
- **Rejected: a new `ghostframe-codec-report` crate.** It works, but it adds a member
  that every per-crate Dockerfile `COPY` must list (memory: "new workspace member breaks
  the e2e image") for no gain over e2e.

**Files and edits.**
1. `git mv ghostframe-bench/src/bin/codec_report ghostframe-e2e/src/bin/codec_report`, and
   `git mv ghostframe-bench/tests/codec_report_smoke.rs ghostframe-e2e/tests/codec_report_smoke.rs`.
   Inside the moved files, `use ghostframe_e2e::…` stays as it is: a bin in the same
   package refers to its own lib by crate name.
2. `ghostframe-bench/Cargo.toml`: delete the `ghostframe-e2e` line and the `[[bin]]`
   block. Delete the dependencies only `codec_report` used: `clap`, `anyhow`, `chrono`,
   `tracing`, `tracing-subscriber`, `libc`, `serde_json`. Keep `tokio`, `serde`, `bytes`,
   `lz4_flex` and `ghostframe-lib`. **Check each one before deleting it**: grep counts
   show `serde_json` and `tokio` also used in `benches/` (1 file each). Benches use
   dev-or-normal deps, so a crate used by a bench may move to `[dev-dependencies]`.
   Do that only if the benches compile (`cargo clippy -p ghostframe-bench --all-targets`).
3. `ghostframe-e2e/Cargo.toml`: add
   ```toml
   clap = { workspace = true }
   chrono = { version = "0.4", default-features = false, features = ["clock"] }
   # tokio: codec_report needs fs + process on top of the workspace set
   tokio = { workspace = true, features = ["fs"] }   # replaces `tokio = { workspace = true }`
   [[bin]]
   name = "codec_report"
   path = "src/bin/codec_report/main.rs"
   ```
   (`process`, `rt-multi-thread`, `macros`, `time` and `io-util` are already in the
   workspace tokio feature list.)
4. Root `Cargo.toml`, under `[workspace]`:
   ```toml
   # Everything except the e2e harness. Its normal dependency turns on
   # ghostframe-lib's test-loss-injection + browserless-harness, which would unify
   # into the daemon of a plain `cargo build`. Select it explicitly (-p ghostframe-e2e
   # or --workspace); ci/check-release-features.sh guards both selections.
   default-members = [ <every member except "ghostframe-e2e"> ]
   ```
5. **Command rewrite (needed by step 4).** With `default-members`, `cargo test --test e2e`
   at the root looks only in the default members and fails with "no test target named".
   Rewrite every root invocation of an e2e target to add `-p ghostframe-e2e`:
   `cargo test --test X` → `cargo test -p ghostframe-e2e --test X`, and the same for
   `cargo nextest run --test e2e`. Grep finds 36 lines: `.github/workflows/e2e.yml`
   (25 lines, some naming several `--test`), `.github/workflows/nightly.yml` (nextest),
   `Justfile` (`test-e2e`, `test-e2e-isolated`, `test-e2e-stats`, `e2e-firefox`,
   `test-openh264` already uses `-p`), `tools/web-paint-bench.sh`, `docs/ci.md` (4),
   `AGENTS.md`, `DEVELOPERS.md`. Leave `docs/superpowers/` alone: those are historical
   plans. Use
   `grep -rnE "cargo (test|nextest run)[^|]*--(test|example) " Justfile .github ci tools tests DEVELOPERS.md AGENTS.md docs/ci.md | grep -v -- '-p ghostframe'`
   until it prints nothing.
6. `codec_report` consumers: `tools/web-paint-bench.sh:32`
   (`-p ghostframe-bench --bin codec_report` → `-p ghostframe-e2e --bin codec_report`),
   the `Justfile:11-12` comment ("ghostframe-server, codec_report"), the
   `ghostframe-bench/benches/README.md:33-36` paragraph, the
   `docs/specs/m3-codec-bench-results.md` lineage note (mention the move), and the
   `ghostframe-bench/src/lib.rs` doc comment (drop "and the … codec_report binary").
7. `ci/check-release-features.sh`: keep the `-p ghostframe-server` check as it is and add
   a loop over the other selections:
   ```bash
   for sel in "" "--workspace --exclude ghostframe-e2e"; do   # "" = default-members
       # shellcheck disable=SC2086
       r=$(cargo tree $sel -e normal,build,features -i ghostframe-lib \
           | grep -oE 'ghostframe-lib feature "[a-z0-9-]+"' | sort -u)
       if grep -Eq "\"($forbidden)\"" <<<"$r"; then
           echo "::error::'cargo build ${sel:-(default-members)}' resolves a test-only ghostframe-lib feature"; exit 1
       fi
   done
   ```
   Update the header comment: the bench path is gone, and the guard now pins the
   structure.
8. Simplify the workarounds:
   - `ci.yml` `build-release`: keep `cargo build --workspace --release --exclude ghostframe-e2e`,
     then run the grep and `ci/check-release-binary.sh` against the **workspace-built**
     `target/release/ghostframe-server`. The extra `cargo build --release -p ghostframe-server`
     is no longer needed for correctness, and the existing comment block goes. Keep
     `ci/check-release-features.sh` in the `clippy` job, where it already runs.
   - `Justfile ci-local` "release build": the same.
     `just build-release` keeps `-p ghostframe-server`, which is still the smallest build.
   - AGENTS.md trap "A workspace-wide release build ships test code": rewrite to say
     "a build that selects `ghostframe-e2e` (`--workspace`, `-p ghostframe-e2e`) ships
     test code. Plain `cargo build` no longer does (default-members), and
     `ci/check-release-features.sh` holds that". Also update DEVELOPERS.md's crate
     table if it mentions codec_report.
   - `tests/containers/test-server/Dockerfile:5-10` comment: test-pattern no longer
     reaches the browserless harness. **Follow-up, not this PR:** once
     `ghostframe-test-pattern` no longer reaches `ffmpeg-oracle`, the image's
     `libav*-dev`, `clang` and `libclang-dev` may become unnecessary. Check
     `cargo tree -p ghostframe-server -p ghostframe-test-pattern --features ghostframe-lib/test-loss-injection,ghostframe-lib/cdf53-diag -e normal -i ffmpeg-sys-next`
     (expect "nothing to print"), then try a `just containers-build` without them.

**Implementer steps (Sonnet).**
1. Do steps 1–3, then `cargo metadata --offline --format-version 1 >/dev/null`. The lock
   should only drop dependency-list entries; no new packages appear, because clap and
   chrono are already locked.
2. Do step 4, then steps 5–6 with the grep loop.
3. Do step 7, then step 8.
4. Run the gates below.

**Verification.**
- `cargo tree --offline --workspace --exclude ghostframe-e2e -e normal,build,features -i ghostframe-lib | grep -oE 'ghostframe-lib feature "[a-z0-9-]+"' | sort -u`
  prints only `default`. The same holds with no selection flag (default-members) and
  with `-p ghostframe-server`.
- `cargo tree --offline -p ghostframe-test-pattern -e normal -i chromiumoxide` prints
  "nothing to print".
- **Mutation check 1:** re-add `ghostframe-e2e = { path = "../ghostframe-e2e" }` to
  `ghostframe-bench [dependencies]`, and `ci/check-release-features.sh` must exit 1 (on
  the workspace and default selections). Revert.
- **Mutation check 2:** delete `default-members`, and the script must exit 1 on the
  default selection. Revert.
- `cargo build -p ghostframe-e2e --bin codec_report` compiles.
  `cargo test -p ghostframe-e2e --test codec_report_smoke -- --ignored` passes on a
  Docker+GPU host (owner-run).
- `just ci-local` is green. In CI, the `e2e.yml` jobs run the same test names as before:
  compare each job's "Running tests/…" lines with a master run.

**Gates.** `just ci-local` (run through `gf-gpu-run`), `ci/check-release-features.sh`, the
CI e2e tier (it proves the command rewrite), and `just containers-build` (the Dockerfile
copies `ghostframe-e2e/src/`, so the moved bin is already inside the context).

**Risks / owner decisions.**
- `default-members` changes what a bare `cargo test` at the root runs: no e2e. That is
  the point, but developers used to `cargo test --test browserless` will see an error.
  Note it in DEVELOPERS.md "Running tests".
- The e2e.yml rewrite is mechanical but wide. Diff the run's test list against master.

**Size: M.**

---

## EC-2 (Med): the daemon links two crypto providers (ring and aws-lc-rs)

**Impact.** About 1 MB of extra C and assembly, a second RNG stack (`getrandom 0.2`
next to 0.3), a larger audit surface, and two providers in a binary whose policy is
"aws-lc-rs only, passed explicitly". The `Cargo.toml` comments claim ring is there only
for the e2e harness.

**Status at e85fb005: CONFIRMED.**
`cargo tree -p ghostframe-server -i ring -e features --offline` shows ring 0.17.14
reached by two edges:
- `rustls feature "ring"` ← `ghostframe-lib` (`ghostframe-lib/Cargo.toml:65`,
  `rustls = { workspace = true, features = ["aws_lc_rs", "ring"] }`, with the comment at
  `:53-54`). That pulls in `rustls-webpki feature "ring"`.
- `rcgen feature "ring"` (and through it ring's `default`) ← the workspace
  `rcgen = { version = "=0.14.7", default-features = false, features = ["crypto", "ring"] }`
  (`Cargo.toml:36`), consumed by `ghostframe-lib:66`.
- `quinn-proto` is clean: the workspace sets `default-features = false` and lib asks only
  for `rustls-aws-lc-rs`.
- `getrandom v0.2.17` comes in only through ring. `cargo tree -p ghostframe-server -d -e normal`
  lists exactly `getrandom 0.2/0.3` and `syn 2/3`, the latter a proc-macro, harmless.
- The native client (`-p ghostframe-cli`, `-p ghostframe-client-native`) already has no ring.
- The ring users the comment means are `ghostframe-e2e/src/harness/browser.rs:517`
  (`rustls::crypto::ring::default_provider().install_default()` for fantoccini's
  hyper-rustls) and `ghostframe-e2e/tests/loopback_h3.rs:70,333`. `ghostframe-e2e`
  inherits the `ring` feature on rustls by unification from `ghostframe-lib`. e2e also
  gets ring on its own through reqwest/bollard/hyper-rustls, so ring stays in the e2e
  graph whatever this PR does.
- `ghostframe-client-net` keeps ring on wasm32 only, on purpose
  (`ghostframe-client-net/Cargo.toml:29-33`). Leave it alone.

**Root cause.** A test crate's provider choice was satisfied by turning on the feature
in the production crate.

**Design (chosen).**
1. Workspace `rcgen`: `features = ["crypto", "aws_lc_rs"]`. rcgen 0.14.7 defines
   `aws_lc_rs = ["crypto", "dep:aws-lc-rs", "aws-lc-rs/aws-lc-sys", …]` (checked in the
   registry copy). `ghostframe-lib/src/transport/quic.rs:332-335` uses only
   `CertificateParams::new` and `KeyPair::generate()`, which aws-lc-rs supports
   (the default ECDSA P-256).
2. `ghostframe-lib/Cargo.toml:65`: `rustls = { workspace = true, features = ["aws_lc_rs"] }`.
   Delete the "ring stays…" comment.
3. `ghostframe-e2e/Cargo.toml`: `rustls = { workspace = true, features = ["ring"] }`, with
   the comment moved here. Also `rcgen = { workspace = true, features = ["pem", "ring"] }`
   in place of `rcgen = "0.14"`; e2e uses `cert.pem()` and `serialize_pem()` in
   `src/harness/e2e_certs.rs:33-34`. That closes one EC-7 bypass. With both rcgen
   providers enabled in e2e builds, rcgen prefers ring. **UNVERIFIED**: check
   `rcgen/src/ring_like.rs`. Either works for test certificates.
- **Rejected: porting the harness to `rustls::crypto::aws_lc_rs`.** It is also correct,
  and it would let e2e drop ring from its *own* edges. But reqwest's `rustls-tls` and
  bollard pull ring in regardless, so it buys nothing in the e2e graph. The daemon is
  what matters, and design steps 1–2 fix that. It is acceptable as a later cleanup.
- Workspace builds that select e2e will still unify `rustls/ring` into `ghostframe-lib`'s
  rustls. That is harmless (compiled, unused) and only affects test builds, which EC-1
  already marks as test builds.

**Guard (new).** Append to `ci/check-release-features.sh`:
```bash
if cargo tree -p ghostframe-server -e normal -i ring 2>/dev/null | grep -q '^ring v'; then
    echo "::error::the release daemon links ring; aws-lc-rs is its only provider (see ci/check-release-features.sh)"; exit 1
fi
```
(`cargo tree -i` on a crate missing from the graph prints a warning and nothing on
stdout, so the grep is the test.)

**Steps (Haiku can do it).** The three manifest edits, the guard, then
`cargo metadata --offline >/dev/null`. The lock drops ring from lib's deps and adds
aws-lc-rs to rcgen's; no new crates, because aws-lc-rs is already locked.

**Verification.**
- `cargo tree --offline -p ghostframe-server -i ring` prints "nothing to print", and so does
  `-i getrandom@0.2.17`. `cargo tree -p ghostframe-server -d -e normal` shows only `syn`.
- **Mutation:** put `"ring"` back on lib's rustls, and the guard fails.
- `cargo test -p ghostframe-lib --lib transport::quic` (cert generation) passes, as do
  `cargo test -p ghostframe-e2e --test loopback_h3` and `cargo test -p ghostframe-client-net`
  (its dev-deps use the workspace rcgen, now aws-lc-rs).
- `cargo build -p ghostframe-client-wasm --target wasm32-unknown-unknown` still builds:
  client-net's wasm edge is untouched, and rcgen is not in the wasm graph.

**Gates.** ci-local, and the e2e.yml `loopback-h3` and `browserless` jobs.

**Risks.** None to the wire: the server provider was already aws-lc-rs through
`pq_provider()`. **Owner decision:** none needed.

**Size: S.**

---

## EC-3 (Med): no advisory, ban or source gate (cargo-deny)

**Status at e85fb005: CONFIRMED.** There is no `deny.toml`, `audit.toml` or job anywhere
(`ls`, grep on `.github`, `Justfile`, `ci/`). Neither cargo-deny nor cargo-audit is
installed locally. `.github/dependabot.yml` covers cargo, gomod, actions and docker, but
Dependabot alerts are not a merge gate. The only git source in `Cargo.lock` (690
packages) is `git+https://github.com/Bluebugs/goog_cc?rev=14eb7a10…` (`Cargo.lock:2333`).

**Design (chosen).** Adopt cargo-deny for `advisories`, `bans` and `sources`.
**`ci/check-licenses.py` stays the source of truth for licences.** It enforces AGPL
containment direction (an MIT crate must not link an AGPL one), an AGPL-only extra
(MPL-2.0), licence-file crates (goog_cc) and SPDX headers per file. cargo-deny's flat
allow-list cannot express the direction rule, and a second list would drift. So CI runs
`cargo deny check advisories bans sources` and never `licenses`.
- Rejected: cargo-audit. It covers advisories only, and cargo-deny is a superset with
  sources and bans.
- Rejected: replacing check-licenses.py with deny's `[licenses]` plus `exceptions`. It
  loses the containment rule.

**New file `deny.toml`** (repo root; schema for cargo-deny ≥ 0.16. **UNVERIFIED** against
the exact release pinned below, so run `cargo deny check` once and fix key names):
```toml
# SPDX-License-Identifier: MIT OR Apache-2.0
# Supply-chain gate: advisories, bans, sources. Licences are NOT checked here:
# ci/check-licenses.py is the source of truth (AGPL containment direction,
# SPDX headers), which cargo-deny cannot express.
[graph]
targets = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "wasm32-unknown-unknown",
]
all-features = false

[advisories]
version = 2
yanked = "deny"
# Each ignore needs an ID, a reason and a revisit date.
ignore = []

[bans]
multiple-versions = "warn"      # EC-10 drives these down; never "deny" (e2e churn)
wildcards = "deny"
allow-wildcard-paths = true     # path deps of publish = false crates (needs EC-7)
highlight = "all"
deny = [
    { crate = "openssl-sys", reason = "rustls/aws-lc-rs only; no OpenSSL link" },
    { crate = "openssl", reason = "rustls/aws-lc-rs only" },
]
# The daemon-has-no-ring rule lives in ci/check-release-features.sh (EC-2):
# cargo-deny bans are graph-wide and e2e/wasm legitimately use ring.

[sources]
unknown-registry = "deny"
unknown-git = "deny"
allow-registry = ["https://github.com/rust-lang/crates.io-index"]
# [patch.crates-io] goog_cc: ALR caller-clock fix, see root Cargo.toml.
allow-git = ["https://github.com/Bluebugs/goog_cc"]
```
`openssl-probe` is in the lock (pulled by a TLS stack in e2e); it is not `openssl`, so
the ban is fine.

**CI job** (`ci.yml`, a new job with no Rust build):
```yaml
  deny:
    name: cargo-deny
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@<sha> # v7.x.y
      - uses: EmbarkStudios/cargo-deny-action@<sha> # v2.x.y   (resolve: gh api repos/EmbarkStudios/cargo-deny-action/commits/v2 --jq .sha)
        with:
          command: check advisories bans sources
          arguments: --workspace
```
Add a scheduled run too (nightly.yml or a cron in ci.yml): a new advisory against an
unchanged lock must surface without waiting for a PR.

**Justfile.**
```just
# Supply chain: advisories (needs the advisory DB), bans, sources.
deny:
    cargo deny --workspace check advisories bans sources
# Offline variant for ci-local: bans + sources need no network; advisories use
# the last-fetched DB (~/.cargo/advisory-dbs) and are skipped when absent.
deny-offline:
    cargo deny --workspace --offline check bans sources
    -cargo deny --workspace --offline check --disable-fetch advisories
```
**UNVERIFIED flag spellings:** `--offline` as a global flag and `check --disable-fetch`.
Check them with `cargo deny --help` on the pinned version. ci-local calls
`just deny-offline` only when `command -v cargo-deny` succeeds; otherwise it prints
"cargo-deny not installed: `cargo install cargo-deny --locked`" and continues. That
matches how ci-local treats wasm-pack today.

**Steps.**
1. Write `deny.toml`.
2. Install cargo-deny locally (`cargo install cargo-deny --locked`; record the version
   in DEVELOPERS.md prerequisites).
3. Run `cargo deny check advisories bans sources`. Triage each advisory: bump it, or
   ignore it with a reason.
4. Add the job and the recipes, and wire them into ci-local.
5. Document it in `docs/ci.md`.

**Verification.**
- **Mutation:** `cargo deny check sources` passes. Change `allow-git` to a wrong URL and
  it fails on goog_cc.
- Add a `foo = "*"` dependency to a scratch branch and `bans` fails. Revert.

**Gates.** The new CI job is required for merge, after one week green as non-required.

**Owner decision.** Whether advisories block merges, or start as `warn` and block only
on the scheduled run.

**Size: S** (M if the first run finds advisories).

---

## EC-4 (Med): no shared lint policy (`[workspace.lints]`)

**Status at e85fb005: CONFIRMED.** There is no `[lints]` or `[workspace.lints]` in any
manifest and no crate-level `#![deny]`/`#![warn]` in any `src/`. `clippy.toml` holds only
`disallowed-methods` (`Instant::elapsed`). Its entry already asks for
`#[allow(…, reason = "…")]`, so the reason style is the house style but is not enforced.

**Heuristic counts (grep over `src/` and tests, upper bounds; not compiler output):**

| crate | `unsafe {` | `// SAFETY`/`# Safety` | `#[allow]` without reason | `unsafe fn`/`extern` | `pub` items | `mem::forget`-ish |
|---|---|---|---|---|---|---|
| ghostframe-lib | 190 | 114 | 35 | 73 | 969 (1580 `pub(crate)`) | 5 |
| ghostframe-client-gpu | 137 | 140 | 21 | 9 | 178 | 1 |
| ghostframe-client-capi | 43 | 37 | 2 | 18 | 14 | 0 |
| ghostframe-client-h264 | 41 | 40 | 0 | 2 | 83 | 0 |
| ghostframe-client-native | 40 | 36 | 4 | 0 | 45 | 0 |
| ghostframe-server (bin) | 24 | 23 | 4 | 0 | 228 (all unreachable: a bin) | 0 |
| ghostframe-tsnet | 24 | **4** | 4 | 3 | 38 | 0 |
| ghostframe-cli (bin) | 8 | 9 | 0 | 0 | 49 | 0 |
| ghostframe-test-pattern | 7 | 7 | 1 | 0 | 91 | 0 |
| ghostframe-e2e | 3 | 3 | 15 | 0 | 254 | 0 |
| ghostframe-bench | 1 | 1 | 15 | 3 | 49 | 0 |
| client-core / net / wasm / protocol / render / web-ui | ≤2 each | — | 1–3 each | ≤1 | 257 / 43 / 271 / 268 / 171 / 24 | wasm 4 (`forget` on closures, expected) |

`dbg!`, `todo!` and `unimplemented!` appear **0 times** in the workspace, so those lints
can go straight to deny. On `unreachable_pub`: in the bins (`ghostframe-server`,
`ghostframe-cli`, `ghostframe-test-pattern`'s main) every `pub` is unreachable, so expect
roughly 200–350 hits, all machine-fixable. The library crates are smaller except lib
(LD's).

**Table (root `Cargo.toml`):**
```toml
[workspace.lints.rust]
rust_2018_idioms = { level = "warn", priority = -1 }   # group; individual lints override it
unsafe_op_in_unsafe_fn = "deny"                         # default-warn in edition 2024; do it first
unreachable_pub = "warn"
unused_qualifications = "warn"

[workspace.lints.clippy]
undocumented_unsafe_blocks = "warn"
allow_attributes_without_reason = "warn"
dbg_macro = "deny"
todo = "deny"
unimplemented = "deny"
mem_forget = "warn"

[workspace.lints.rustdoc]
broken_intra_doc_links = "warn"
```
CI already runs clippy with `-D warnings`, so every `warn` is a gate in CI and a nag
locally.

**Mechanics that matter.**
- Each crate opts in with `[lints] workspace = true`, and Cargo does **not** allow
  additional keys next to `workspace = true` in that table. So per-crate overrides
  cannot live in the crate's `Cargo.toml`. They are crate-root attributes
  (`#![allow(lint, reason = "…")]` / `#![warn(…)]` in `lib.rs`/`main.rs`). The
  alternative is not opting in yet.
- Clippy lints in the table reach only clippy runs; rustc ignores `clippy::` tool lints.
  Rustdoc lints act only under `cargo doc`, so pair this with EC-8's
  `RUSTDOCFLAGS="-D warnings" cargo doc` gate.
- `rust_2018_idioms` includes `elided_lifetimes_in_paths` (`fmt::Formatter` → `Formatter<'_>`).
  It is noisy but `cargo clippy --fix` and `cargo fix` apply it.
- Changing lints changes rustc flags, so expect one full rebuild of the workspace crates.

**Per-crate overrides (crate-root attributes):**
- `ghostframe-lib`, `ghostframe-tsnet`, `ghostframe-client-capi`:
  `#![allow(clippy::undocumented_unsafe_blocks, reason = "SAFETY burn-down owned by UF; remove when UF-n lands")]`.
  UF deletes the attribute in its final PR.
- `ghostframe-lib`: `#![allow(unreachable_pub, reason = "pub(crate) pass owned by LD")]`.
  LD removes it.
- `ghostframe-protocol` and `ghostframe-client-capi` add
  `#![warn(clippy::cast_possible_truncation)]` (the wire format and the C boundary are
  where a silent `as` truncation corrupts data). Grep upper bound: protocol 191 `as`
  narrowing-style casts, capi 13. Expect most to become `u16::try_from(x)?` or a
  documented `#[expect(clippy::cast_possible_truncation, reason = "…bounded by …")]`.
  Protocol likely needs its own PR.
- `ghostframe-client-wasm`: `mem_forget` on `Closure::forget` is expected (4 hits), so
  mark each with `#[expect(clippy::mem_forget, reason = "…")]`.

**Rollout order** (one PR per row; each PR = `[lints] workspace = true` plus the fixes,
mostly from `cargo clippy --fix -p <crate> --allow-dirty` and `cargo fix`):
1. Add the table to the root manifest with no crate opted in (no behaviour change), and
   opt in `ghostframe-protocol`, `ghostframe-client-core`, `ghostframe-render` and
   `ghostframe-web-ui`. These are small, with no unsafe code.
2. `ghostframe-client-net`, `ghostframe-client-wasm`, `ghostframe-client-h264`.
3. `ghostframe-client-gpu`, `ghostframe-client-native` (SAFETY comments roughly match the
   unsafe blocks, so few hits).
4. `ghostframe-cli`, `ghostframe-server`, `ghostframe-test-pattern` (bins: mainly
   `unreachable_pub` → `pub(crate)`).
5. `ghostframe-e2e` and `ghostframe-bench` (`allow_attributes_without_reason`, 30 hits).
6. `ghostframe-tsnet`, `ghostframe-client-capi`, `ghostframe-lib`, each with the UF/LD
   allow attributes above. This step must land **before** UF and LD start, so their
   PRs remove an allow instead of each adding the lint itself. Coordinate with them so
   EC-4 step 6 is first in that chain.

**Edition 2024 (a separate, later track; one PR per crate, after the lint rollout).** The
toolchain (1.96.1) supports it. Per crate: `cargo fix --edition -p <crate> --all-targets`,
set `edition = "2024"` (or `edition.workspace = true` once EC-7 lands), then clippy and
tests. Known work, from grep:
- `unsafe extern "C" {`: 8 extern blocks.
- `#[unsafe(no_mangle)]`: 21 `#[no_mangle]`, in `ghostframe-lib/src/ffi.rs` and capi. cbindgen
  must parse `#[unsafe(no_mangle)]`. **UNVERIFIED for cbindgen 0.29.2**; check that the
  header diff is empty after migration. If it is not, keep `edition = "2021"` for lib
  and capi.
- `gen` becomes reserved: 80 identifier uses (scheduler.rs 41, io_bridge/frame.rs 13,
  coverage_pin_tests.rs 20, client-gpu ring.rs 6, render dirty.rs 5). `cargo fix`
  rewrites them to `r#gen`; renaming to `generation` by hand reads better.
- `std::env::set_var`/`remove_var` become `unsafe`: 80 call sites, mostly tests under
  `test_env::lock_env()`. Wrap each in `unsafe { }` with a SAFETY line naming the lock.
- RPIT lifetime capture: 34 `-> impl …` sites. `cargo fix` adds `+ use<…>` where
  behaviour would change.
- `if let` rescoping and tail-expression drop order: `cargo fix` reports
  `if_let_rescope` and `tail_expr_drop_order`. **Review these by hand in lock-holding
  code** (io_bridge, scheduler, the client GPU ring), where a guard's lifetime changes.
  Do not accept the auto-fix blindly.
- Never-type fallback (`dependency_on_unit_never_type_fallback`) and `static mut` refs (1).
- Order: protocol, client-core, render first, then the rest. lib goes last, with the
  browserless suite looped 3× (memory: timing-sensitive scenes).

**Verification.** For each step, `cargo clippy --workspace --all-targets -- -D warnings`
is green. **Mutation:** add `dbg!(1);` to any opted-in crate and clippy fails. Remove
one `// SAFETY:` from an opted-in crate (not lib, tsnet or capi) and clippy fails.

**Size: M** across the rollout. Protocol truncation is S–M. Edition 2024 is L, spread
out.

---

## EC-5 (Med): the Go build is not hermetic

**Status at e85fb005: CONFIRMED, with one part STALE.**
- STALE: "the archive is built outside OUT_DIR". `ghostframe-tsnet/build.rs:33-38` already
  passes `OUT=$OUT_DIR/libghostbridge.a`.
- CONFIRMED, the SPA copy: with `web-embed`, `make archive` runs `sync-dist`, which does
  `rsync -a --delete ../ghostframe-web-client/dist/ dist/` **inside the source tree**
  (`ghostbridge/Makefile:23-25`). Two cargo processes with different target dirs on the
  same checkout can interleave rsync `--delete` with `go build` reading
  `//go:embed all:dist` (`web_dist.go:28`), and embed a torn SPA. Examples are a
  terminal build next to rust-analyzer or a second `CARGO_TARGET_DIR`. Cargo's
  target-dir lock does not cover this.
- CONFIRMED, no `-trimpath`: `Makefile:29,40` runs plain `go build`. The owner's
  `target/release/ghostframe-server` (2026-11-07) contains **1926** `/home/<user>/go/pkg/...`
  strings (Go module paths), against 308 `~/.cargo/registry`, 15 `~/.cargo/git` and 6
  repo paths from Rust. This does not break `check-local-paths.sh`, which checks tracked
  files, but it ships the builder's home path in every binary and makes binaries
  differ between machines.
- CONFIRMED, no `rerun-if-env-changed`: build.rs emits only `rerun-if-changed`. A change
  to `GOFLAGS`, `GOTOOLCHAIN`, `CC`, `CGO_CFLAGS`/`CGO_LDFLAGS`, `GOWORK` or `GOARCH` does
  not rebuild the archive.
- CONFIRMED, version skew: `.github/workflows/_setup-go/action.yml:28` uses
  `go-version: '1.25'`, but `ghostbridge/go.mod:3` says `go 1.27.3`. With the default
  `GOTOOLCHAIN=auto`, the first `go` command in `ghostbridge/` downloads go1.27.3. In CI
  that is `go mod download` in the same action (retried, and cached in GOMODCACHE). In
  the test-server image, `golang-go` from Ubuntu 24.04 (1.22) downloads it in the
  warm-cache layer (`Dockerfile:134-135`). Locally, any older Go downloads it from
  inside build.rs, silently, over the network.

**Design (chosen).**
1. **build.rs builds from a mirror in OUT_DIR.** It copies `ghostbridge/*.go` (except
   `*_test.go`), `go.mod` and `go.sum` into `$OUT_DIR/ghostbridge-src/`, and with
   `web-embed` mirrors `ghostframe-web-client/dist/` into `$OUT_DIR/ghostbridge-src/dist/`
   using `std::fs`: write a file only if its content differs, and delete stale files.
   It then runs `go` directly (no `make`) in that directory. OUT_DIR is per package and
   per feature set, and cargo locks it, so the race goes away and `ghostbridge/dist/` is
   no longer written by cargo.
   - Rejected: `go build -overlay`. Whether `//go:embed` honours overlay entries for
     directory patterns is **UNVERIFIED** (there are known Go issues around embed and
     overlays), and a JSON overlay is harder to debug than a directory.
   - Rejected: a lock file around rsync. That fixes the race, but leaves the build
     writing into the source tree.
2. **Command:**
   ```rust
   let go = env::var("GO").unwrap_or_else(|_| "go".into());
   let mut cmd = Command::new(go);
   cmd.current_dir(&src)
      .args(["build", "-trimpath", "-mod=readonly", "-buildmode=c-archive", "-o"])
      .arg(&archive)
      .env("GOWORK", "off")          // a stray go.work must not redirect modules
      .env("CGO_ENABLED", "1");
   if !web_embed { cmd.args(["-tags", "noweb"]); }
   cmd.arg(".");
   for v in ["GO", "GOFLAGS", "GOTOOLCHAIN", "GOPROXY", "GONOSUMDB", "GOPRIVATE",
             "GOARCH", "GOOS", "GOAMD64", "GOARM64", "CC", "CGO_CFLAGS", "CGO_LDFLAGS",
             "GOPATH", "GOMODCACHE"] {
       println!("cargo:rerun-if-env-changed={v}");
   }
   ```
   `-mod=readonly` is already the default for `go build` with a go.mod (Go ≥ 1.16); it
   is stated so a `GOFLAGS=-mod=mod` from the environment fails instead of editing
   go.sum. `-trimpath` also makes Go's build-cache keys independent of the directory, so
   building from OUT_DIR does not defeat the cache across worktrees. Rerun triggers:
   `rerun-if-changed` on each copied Go file and on `ghostbridge/` top-level entries (as
   today), plus the web dist directory when `web-embed` is on (as today). Keep the
   archive name and the link directives unchanged.
3. **Makefile:** keep `check-dist` and `sync-dist` (the `go-vet` CI job and ci-local run
   `go vet` in place). Change `archive`/`archive-noweb` to use
   `go build -trimpath -buildmode=c-archive …` for manual use, and note that cargo no
   longer calls them. Drop `rsync` from the Dockerfile's apt list only after checking
   that nothing else in the image build calls make.
4. **CI Go version:** in `_setup-go/action.yml`, use
   `go-version-file: ghostbridge/go.mod` in place of `go-version: '1.25'`, and add
   `env: GOTOOLCHAIN: local` at workflow level (or `echo GOTOOLCHAIN=local >> "$GITHUB_ENV"`
   in the action), so a future go.mod bump without a runner bump fails loudly instead
   of downloading. Update the stale "1.25" mentions in DEVELOPERS.md prerequisites if
   present.
5. **Dockerfile:** the warm layer must use the same flags, or its cache entries never
   match: `go build -trimpath -tags noweb -buildmode=c-archive -o /tmp/warm.a .`. Go
   version: either keep Ubuntu's `golang-go` with toolchain auto-download (as today, but
   say so in a comment), or install the official tarball pinned by SHA-256 (owner
   decision; dependabot's docker ecosystem will not track it).
6. **Out of scope, noted:** cross-compiling with Go. build.rs does not map
   `CARGO_CFG_TARGET_ARCH` to `GOARCH`; ARM clients build natively today. Rust-side home
   paths (323 strings) need `--remap-path-prefix` in the release build's RUSTFLAGS,
   which forces a separate fingerprint (a full rebuild) for `just build-release`. That
   is an owner decision.

**Verification.**
- After `just build-release`:
  `strings -n 8 target/release/ghostframe-server | grep -c "$HOME/go/pkg"` prints `0`
  (was 1926), and `| grep -c ghostframe-web-client` stays the same.
  `strings … | grep -c <new-bundle-hash>` ≥ 1 (the AGENTS.md check).
- `ghostbridge/dist/` is not created by `cargo build -p ghostframe-server` on a clean
  checkout (`git clean -fdX ghostbridge && cargo build -p ghostframe-server && test ! -e ghostbridge/dist`).
- **Mutation:** `GOFLAGS=-tags=noweb cargo build -p ghostframe-server` rebuilds the archive
  (rerun-if-env-changed works). With the env var unchanged, a second build does not rerun
  build.rs (`cargo build -v` shows `Fresh ghostframe-tsnet`).
- A concurrency check: two `cargo build -p ghostframe-server` runs with different
  `CARGO_TARGET_DIR`s at once both succeed, and both binaries serve a working SPA
  (e2e smoke, owner-run).
- The CI log's `go version` reads go1.27.3, and there is no "go: downloading go1.27.3" line.

**Gates.** ci-local (the `go vet` both-tags step is unchanged), `just containers-build`,
and the e2e tier (the embedded SPA is what the browser loads).

**Risks.** If build.rs's copy misses a file the embed needs, the SPA is incomplete. That
is caught by `web_dist_test.go` only if it runs against the mirror, so add a build.rs
assertion that `dist/index.html` exists in the mirror when `web-embed` is on.

**Size: M.**

---

## EC-6 (Med): `ghostframe-lib` builds as cdylib and staticlib for no consumer

**Status at e85fb005: CONFIRMED.** `ghostframe-lib/Cargo.toml:7-8` sets
`crate-type = ["cdylib", "staticlib", "rlib"]`. Nothing in the repo links the C ABI. No
Rust or C code calls `gf_server_*` outside `ghostframe-lib/src/ffi.rs` (405 lines, 5
`#[no_mangle]`). Nothing references `libghostframe_lib.so/.a`, and no Makefile, pkg-config
file or test links it. The only reference is `docs/specs/ghostframe-initial-spec.md:321,1574`
(the original "libghostframe.so + ghostframe.h" plan, with an Enlightenment module
`e_mod_ghostframe.h` as the intended consumer). The cost is real. In the owner's tree,
from the 2026-11-07 builds, `target/debug/libghostframe_lib.a` is 373 MB, `.so` 161 MB,
`target/release/libghostframe_lib.a` 125 MB and `.so` 37 MB. Their timestamps match the
`ghostframe-server` build, so cargo builds and links every listed crate type even when
lib is only a dependency. Each one links the whole Go runtime again.

**Design (chosen).** Set `crate-type = ["rlib"]` (or delete `[lib] crate-type`). Keep
`ffi.rs` and the cbindgen header compiling, which keeps the C API honest and keeps LD's
surface review meaningful. If a C consumer appears, give it a thin
`ghostframe-server-capi` crate (`cdylib` + `staticlib`, depending on lib), the same
pattern as `ghostframe-client-capi`.
- Rejected: leaving the crate types in place for a hypothetical consumer. It costs about
  0.5 GB of disk and two extra links per debug build.

**Owner decision (required).** What happens to the server C API:
(a) keep `ffi.rs` + `ghostframe.h` as a supported surface (then move it to its own capi
crate);
(b) keep it as an unsupported internal surface, rlib only, as above;
(c) delete `ffi.rs`, the header and cbindgen from lib. That also removes the "every
`pub const` lands in the C header" trap from AGENTS.md, and with it LD's constant
leakage work.
The recommendation is (b) now and (c) unless the Enlightenment-module plan is alive.

**Edits for (b).** `ghostframe-lib/Cargo.toml`: drop the crate-type line. Update
DEVELOPERS.md:22 ("Builds as `cdylib` / `staticlib` / `rlib`").

**Verification.** After `cargo build --release -p ghostframe-server`, `ls target/release/libghostframe_lib.*`
lists only `.rlib` and `.d`. The cbindgen header diff is empty (build.rs is unaffected).
ci-local is green.

**Size: S** (decision plus a one-line change; (c) is M and shared with LD).

---

## EC-7 (Low/Med): version-pin policy, `[workspace.package]`, `publish = false`

**Status at e85fb005: CONFIRMED.** `[workspace.dependencies]` has **21** `=` pins
(`grep -c '"='`): quinn-proto, rustls, rcgen, web-transport-proto, tokio, bytes,
thiserror, rusty_erasure, tracing, tracing-subscriber, libc, cbindgen, sha2, hex, wgpu,
wgpu-hal, khronos-egl, pollster, ffmpeg-next, ffmpeg-sys-next and clap. Pins outside the
table: `goog_cc = "=0.1.4"` (`ghostframe-lib:73`), `naga = "=30.0.1"` (client-gpu:75),
`web-time = "=1.1.0"` (client-net) and `leptos = "=0.9.0-rc1"` (web-ui). Crates that
bypass the workspace entry: e2e `rcgen = "0.14"`, `sha2 = "0.10"`; bench and test-pattern
`clap = "4"`; test-pattern `libc = "0.2"`. There is no `[workspace.package]`. Every crate
repeats `edition = "2021"` and none has `rust-version`. Only web-ui and client-wasm set
`publish = false`.

**Policy.** Use caret requirements by default; `Cargo.lock` is what pins builds, and
Dependabot moves it. Use `=` only for a **documented coupling**, with the reason in the
comment above the entry:
- `wgpu`, `wgpu-hal`, `naga`, `khronos-egl`: type identity across the escape hatch
  (already documented).
- `ffmpeg-next`, `ffmpeg-sys-next`: these must match the host's libav* (memory: "pin
  must match host libavcodec").
- `quinn-proto` with `web-time`: `web-time` must equal quinn-proto's own `Instant` type
  on wasm (memory note). Keep quinn-proto pinned because client-net mirrors its
  internal type.
- `rusty_erasure`: owner decision 2026-11-07.
- `cbindgen`: its output is diffed in CI, so a minor bump changes two tracked headers.
- `goog_cc`: the `[patch]` fork is 0.1.4 plus one commit. Move it into
  `[workspace.dependencies]` with the comment.
- `leptos 0.9.0-rc1`: a pre-release, so the pin is required.

Relax these to caret, which leaves the lock unchanged: `rustls` (provider selection is
explicit in code, so a caret 0.23 is safe), `rcgen`, `web-transport-proto`, `tokio`,
`bytes`, `thiserror`, `tracing`, `tracing-subscriber`, `libc`, `sha2` (its comment says
"stay on 0.10.x", which caret `0.10.9` already guarantees), `hex`, `pollster` and `clap`.
**Owner decision:** rustls. If the ML-KEM group name or behaviour is considered
version-sensitive, keep it pinned with that reason.

**Edits.**
```toml
# root Cargo.toml
[workspace.package]
edition = "2021"
rust-version = "1.96"          # == rust-toolchain.toml channel; bump together
repository = "https://github.com/mountain-reverie/ghostframe"
publish = false
# licence stays per crate (two licences; ci/check-licenses.py reads cargo metadata,
# which resolves `license.workspace` too, but there is no single value to inherit).
```
In every member `[package]`: `edition.workspace = true`,
`rust-version.workspace = true`, `repository.workspace = true` and
`publish.workspace = true`. Delete the explicit `publish = false` in web-ui and
client-wasm. Dependency lines: e2e `rcgen`/`sha2` → `{ workspace = true, … }` (rcgen per
EC-2); bench, test-pattern and e2e `clap` → `{ workspace = true }`; test-pattern `libc`
→ `{ workspace = true }`; lib `goog_cc` → `{ workspace = true }`, with
`goog_cc = { version = "=0.1.4" }` plus its comment in the workspace table; client-gpu
`naga` → workspace, next to wgpu. Add a comment above `[workspace.dependencies]` that
states the policy.

**Steps (Haiku).** Make the edits, then run
`cargo metadata --offline --format-version 1 >/dev/null`. `git diff Cargo.lock` must be
empty or show only dependency-list reordering; no version changes. Then run
`ci/check-licenses.py`.

**Verification.** `git diff --stat Cargo.lock` shows ≤ a few lines. `cargo tree -d` is
unchanged. `grep -c '"=' Cargo.toml` equals the documented count (about 11).
`ci/check-licenses.py` passes, which proves the metadata still carries each licence.

**Risk.** A crate with `rust-version` set lets cargo's MSRV-aware resolver (on by
default for resolver "3"; this workspace uses resolver "2") change future resolutions.
That has no effect today.

**Size: S** (touches every manifest; see sequencing).

---

## EC-8 (Low/Med): CI hygiene

**Status at e85fb005: CONFIRMED** (one item partly mitigated).
- Actions are pinned by tag, not SHA: `actions/checkout@v7` ×17, `actions/cache@v6`,
  `upload-artifact@v7`, `download-artifact@v8`, `setup-go@v5`,
  `docker/build-push-action@v7`, `docker/setup-buildx-action@v4`,
  `Swatinem/rust-cache@v2` ×9, `taiki-e/install-action@v2` and
  `dtolnay/rust-toolchain@stable` ×14. `jlumbroso/free-disk-space@main` ×2 (`e2e.yml:42`
  and the e2e job) floats on a branch. Mitigated: `.github/dependabot.yml` already has a
  `github-actions` ecosystem, and it maintains SHA pins with version comments once they
  exist.
- `client-core.yml` has no `permissions:`, no `concurrency:`, and uses `ubuntu-latest`
  ×3, unlike the other workflows (`ubuntu-24.04`, `contents: read`, cancel-in-progress).
- `dtolnay/rust-toolchain@stable` installs the current stable, then rustup switches to
  1.96.1 from `rust-toolchain.toml`. That is two toolchain downloads per job, and the
  `components`/`targets` inputs go to the wrong toolchain (the file lists them too, so
  it works by accident).
- ci-local does not match CI: it lacks the `gles`/`gstreamer` clippy and check variants
  (`ci.yml:51-87`; ci-local has none, only ci-client does), lacks
  `cargo test -p ghostframe-lib --test bwe_bench` (`ci.yml:174`), and runs
  `cargo test --workspace --bins` without `xvfb-run` (memory: the live DISPLAY refuses
  connections from this shell). In the other direction, ci-local runs the GPU tests CI
  skips; that is intentional.
- There is no `cargo doc` anywhere.
- Stale comments: `ci.yml:58-66`, the "decode-vaapi … cros-codecs' v4l2" justification
  for something already removed, and `ci.yml:77-80`. `Justfile:79,98` too.
  `docs/ci.md` says "seven jobs", gives `cargo build --release` as the local equivalent
  of build-release (that is the EC-1 trap), and says unit is `--lib` only.

**Design.**
1. **One source of truth.** Add Just recipes that CI calls: `ci-clippy` (every clippy and
   check line in the current `clippy` job, including render-wasm and the env, feature,
   path, licence and release-feature scripts), `ci-unit` (the `--lib` skip list,
   `xvfb-run … --bins`, `bwe_bench`, `commands`) and `ci-release`. CI installs just
   (`taiki-e/install-action` with `tool: just`, SHA-pinned) and runs `just ci-clippy`,
   and so on. `ci-local` calls the same recipes. Keep the GPU skip list in a recipe
   argument: `ci-unit skip="…"`, where CI passes the list and ci-local passes empty, so
   developers still run GPU tests.
   - Rejected: keeping two lists in step. They have already drifted, as shown above.
2. **SHA pins.** For each action, look up the SHA
   (`gh api repos/<owner>/<repo>/commits/<tag> --jq .sha`) and write
   `uses: owner/repo@<40-hex> # vX.Y.Z`. For free-disk-space, pick the latest release
   tag's SHA (**UNVERIFIED** whether it has tagged releases; if not, pin the current
   `main` SHA with a dated comment). Dependabot keeps them current.
3. **Toolchain.** Replace every `dtolnay/rust-toolchain@stable` with a composite
   `.github/workflows/_setup-rust/action.yml`:
   ```yaml
   runs:
     using: composite
     steps:
       - shell: bash
         run: |
           rustup --version
           rustup toolchain install    # reads rust-toolchain.toml (rustup >= 1.28)
           rustc --version
   ```
   **UNVERIFIED:** the runner's rustup is ≥ 1.28, where the no-argument
   `toolchain install` reads the file. Fall back to `rustup show` on older rustup, which
   auto-installs. Update `_build-web-client/action.yml:21` the same way. Remove the
   per-job `components:`/`targets:` inputs; the toolchain file already lists clippy,
   rustfmt and wasm32. Fix the stale comment in `rust-toolchain.toml` ("The workflows use
   `dtolnay/rust-toolchain@stable`").
4. **client-core.yml:** add `permissions: { contents: read }` and the same
   `concurrency:` block as ci.yml, and change `ubuntu-latest` → `ubuntu-24.04`.
5. **Docs gate.** Add to `ci-clippy`, or as its own step:
   `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --exclude ghostframe-e2e`.
   The current warning count is unknown (nothing was built), so the first PR fixes
   whatever it reports. Add `--document-private-items` later; it will be noisy.
6. **Stale text.** Rewrite `ci.yml:58-66,77-80` and `Justfile:73-80,95-98`: the
   decode-vaapi pairing stays as a compile-coverage choice, minus the cros-codecs
   history. Refresh `docs/ci.md`'s job table so it lists the actual jobs and
   `just ci-*` as the local equivalents.

**Verification.**
- `grep -nE 'uses: [^.][^@]*@(v[0-9]+|main|stable|master)\b' .github -r` prints nothing.
- A CI run shows one toolchain install per job (`rustc 1.96.1` in the log, and no
  "installing stable").
- **Parity check:** the list of commands in `just --dry-run ci-clippy` matches the old
  clippy job's `run:` lines one to one. Diff by hand in the PR description.

**Gates.** CI itself. Every job must still go green on the PR.

**Size: M.**

---

## EC-9 (Low): 12 committed SPIR-V blobs, one regeneration recipe, no staleness gate

**Status at e85fb005: CONFIRMED.** `git ls-files '*.spv'` lists 12 files under
`ghostframe-lib/src/capture/shaders/`, all `include_bytes!`'d by
`capture/gpu_pipeline/setup.rs:248-704`. Only `cdf53_forward.spv` has a recipe
(`just shaders-cdf53`). An edit to a `.comp` without regenerating ships stale SPIR-V,
and nothing fails.

**Reproducibility, measured** by recompiling into the scratchpad with local
glslang 16.6.0 and shaderc 2026.4 (the Vulkan SDK 1.4.363 packaging):
- 10 files are byte-identical with plain `glslangValidator -V <x>.comp -o <x>.spv`:
  bgra_to_nv12, cdf53_compact, cdf53_indirect_args, palette_fold, palette_subset_fold,
  palette_subset_fold_init, palrle_compact, pal_rle_index, palrle_indirect_args and
  tile_analysis.
- `tile_sad.spv` is byte-identical with `glslc tile_sad.comp -o tile_sad.spv` (no `-O`,
  default target env).
- `cdf53_forward.spv` is byte-identical with the recipe
  (`glslc -O --target-env=vulkan1.1 -DCDF53_FORWARD_ONLY=1`).

So the tree is consistent today, but with three different invocations. Byte-exactness
depends on the tool version: the SPIR-V header records the generator version, and code
generation changes between releases.

**Design.**
1. **One recipe, one manifest.** Add `ghostframe-lib/src/capture/shaders/build.list`, or
   encode the list in the Justfile, as one line per output with its exact command.
   Recipe `just shaders` regenerates all 12, including the cdf53 generator step that
   `shaders-cdf53` runs. Keep the current per-file invocations so the first run is a
   no-op diff. Unifying everything on one tool and `-O` is a separate, behaviour-changing
   PR: it needs the GPU gates (`gpu_pipeline::*` tests on the owner's GPU host).
2. **Staleness gate `just check-shaders`:** run `just shaders` into a temp directory and
   `cmp` against the tracked files. Pin the tool version, because byte equality only
   holds with the same glslang/shaderc. Options:
   - (chosen) In CI, download a pinned release of glslang
     (KhronosGroup/glslang GitHub release `16.x` Linux zip, pinned by SHA-256) and of
     shaderc (or use glslang's own `glslang -V` for `tile_sad` too after unifying), into
     `$RUNNER_TOOL_CACHE`, cached. **UNVERIFIED:** which upstream release reproduces
     these exact bytes. 16.6.0 does locally (Arch package); confirm that the upstream zip
     of the same version gives identical output.
   - Rejected: Ubuntu 24.04's apt `glslang-tools`/`glslc`. They are older (about the
     1.3.275 SDK) and will almost certainly not be byte-identical, so the gate would
     fail at once or need regeneration with old tools.
   - Fallback if pinning proves fragile: compare
     `spirv-dis --no-header --raw-id` output, which ignores the generator word, with
     spirv-dis from the same pinned package. That still breaks on codegen changes, so
     pinning stays the real fix.
3. The CI step runs in the `clippy` job (or a new small job) and calls `just check-shaders`.
   ci-local runs it when `glslangValidator` and `glslc` are on PATH at the pinned
   version, and prints a skip line otherwise.

**Verification.** `just check-shaders` passes on a clean tree. **Mutation:** change one numeric
constant in `tile_sad.comp` and the gate fails. Do not use a comment-only edit: comments
do not change the SPIR-V, so that would prove nothing. Revert.

**Size: S–M** (M because of the tool pinning).

---

## EC-10 (Low): duplicate dependency versions, all in the e2e/web graphs

**Status at e85fb005: CONFIRMED.**
`cargo tree -d --workspace -e normal --offline --depth 0` gives these pairs. **Production
trees:** `ghostframe-server` has only getrandom 0.2/0.3 (from ring, removed by EC-2) and
syn 2/3 (proc-macros: clap_derive vs displaydoc). `ghostframe-cli` has hashbrown
0.16/0.17, rustc-hash 1/2 and syn 2/3. None of these is a security issue; they cost
build time only.

**e2e graph, with what drives each pair:**

| pair | driver | fix |
|---|---|---|
| axum 0.7.9 / 0.8.9 (+ axum-core, matchit) | e2e's own `axum = "0.7"` vs tonic → bollard → testcontainers 0.27 | **bump e2e to `axum = "0.8"`**; `src/harness/transport.rs` uses `Router` + `axum::serve`, a small API diff (path syntax `/:x` → `/{x}` if used) |
| which 6.0.3 / 8.0.2 (+ rustix 0.38/1.x, linux-raw-sys) | e2e's `which = "6"` vs chromiumoxide 0.9 | **bump e2e to `which = "8"`** (3 call sites, same `which::which` API; **UNVERIFIED** no API break) |
| reqwest 0.12.28 / 0.13.2 | e2e's `reqwest = "0.12"` vs chromiumoxide 0.9 | bump e2e to 0.13 if the feature names (`rustls-tls`) still exist; **UNVERIFIED** offline; reqwest 0.13 reworked TLS features |
| http 0.2 / 1.x, thiserror 1 / 2, cookie 0.16 / 0.18, base64 0.21 | webdriver 0.50 ← fantoccini 0.21 | nothing in-tree; upstream fantoccini/webdriver. Leave as is |
| base64 0.21 | also docker_credential ← testcontainers | upstream |
| getrandom 0.4 / rand 0.10 | ferroid ← testcontainers, tempfile | upstream |
| base64 0.23, itertools 0.15, syn 3, convert_case | leptos 0.9.0-rc1 (web-ui) | wait for leptos final |
| nom 7 / 8 | x509-parser 0.18 (client-net) | upstream |

**Design.** One PR: bump e2e's `axum`, `which` and (if clean) `reqwest` to the versions
its dependencies already use. Accept the rest. cargo-deny's `multiple-versions = "warn"`
(EC-3) keeps the list visible without failing.

**Verification.** `cargo tree -d --workspace -e normal --offline --depth 0 | grep -E '^(axum|which|reqwest|rustix) '`
shows single versions. e2e.yml stays green, `browserless` included (axum serves the
harness's static files), and the Firefox slice runs locally (fantoccini path).

**Size: S.**

---

## EC-11 (Low): repo-root hygiene

**Status at e85fb005.**
- The worktree root is clean (no untracked files). The owner's main checkout
  (`git status` at session start) has untracked `2026-09-26-retransmit-storm-field-report.md`,
  `gf-handoff-2026-11-07.md`, `crates.txt` (a broken Python heredoc's output) and
  `live-2026-11-xx.log`, all one `git add -A` away from history (memory: "never
  `git add -A`").
- STALE: `librust_out.rlib`. It is already covered by `*.rlib` (`.gitignore:6`); it
  shows as `!!` (ignored) in the main checkout.
- CONFIRMED: `.gitignore` covers only `tmux-client-*.log`. There is no rule for root
  `*.log`, `live-*.log`, `crates.txt` or root-level handoff notes.

**Design.** Add to `.gitignore`:
```gitignore
# Live-session captures and scratch output dropped in the repo root.
/*.log
/crates.txt
# Local handoff / field-report notes (owner's own; keep them outside the repo,
# e.g. ../gf-notes-archive/, as reference_notes_archive does).
/gf-handoff-*.md
/[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]-*.md
```
Anchor every rule to the root (`/`) so `docs/**` dated specs and plans are unaffected.
Check that `git ls-files -i -c --exclude-standard` prints nothing after the change: no
tracked file may become ignored. **Never delete** the owner's untracked files. Recommend
in the PR description that the owner move them to `../gf-notes-archive/`.

**Verification.** In the main checkout, `git status --short` no longer lists the four
files, and `git ls-files -ci --exclude-standard` is empty.

**Size: S.**

---

## EC-12 (Low): stale benchmark dev-dependencies

**Status at e85fb005: CONFIRMED (versions), with the upstream status UNVERIFIED.**
`ghostframe-bench/Cargo.toml:43-44` has `criterion = { version = "0.5", features = ["html_reports"] }`
and `iai-callgrind = "0.14"` (locked 0.14.2, plus the `iai-callgrind-runner` binary it
needs at run time). The comment "Versions copied verbatim from ghostframe-lib/Cargo.toml"
is stale, because lib no longer has them. As far as I recall, iai-callgrind was renamed
upstream (to **gungraun**) in 2025, and criterion has 0.6 and 0.7 releases. **Both are
UNVERIFIED offline**; confirm on crates.io before acting.

**Design.** Bump criterion to the current minor (its API is stable for `criterion_group!`
and `BenchmarkId` use; check the `html_reports` feature name). If the rename is
confirmed, move `codec_callgrind` to the renamed crate in its own PR. The runner binary
must match the library version exactly, so document `cargo install <runner> --version <same>`
in `benches/README.md`. Delete the stale comment. These are dev-deps of a bench crate,
so nothing ships.

**Verification.** `cargo bench -p ghostframe-bench --no-run` compiles. One
`cargo bench --bench codec_latency -- --quick` run produces numbers within noise of
master (owner-run, GPU-free path).

**Size: S.**

---

## Suggested PRs for EC

`Cargo.toml` and `Cargo.lock` are shared with every group. Rules for all EC PRs:
- **One lock-changing PR in flight at a time per group, and merged in the order below.**
- On a lock conflict, never hand-merge it. Run
  `git checkout origin/master -- Cargo.lock && cargo metadata --offline --format-version 1 >/dev/null`
  (minimal re-resolution, no version bumps), then rebuild and test.
- After a batch of merges, check that master compiles (memory: rebase-merge semantic
  conflicts).
- Manifest-wide mechanical PRs (EC-7, EC-4 step 1) are announced to the other groups,
  land early, and are kept small. Other groups rebase onto them; conflicts are one-line
  `[package]` header edits.

| # | branch | findings | files owned | Cargo.lock? | depends on |
|---|---|---|---|---|---|
| 1 | `ec/manifest-policy` | EC-7 | every `*/Cargo.toml` `[package]` header plus the listed dependency lines; root `[workspace.package]` and the `[workspace.dependencies]` comments and pins | reorder only | none. **Land first**, in a quiet window |
| 2 | `ec/test-feature-isolation` | EC-1 | `ghostframe-bench/**`, `ghostframe-e2e/Cargo.toml`, `ghostframe-e2e/src/bin/codec_report/**`, `ghostframe-e2e/tests/codec_report_smoke.rs`, root `default-members`, `ci/check-release-features.sh`, `Justfile` (e2e recipes, ci-local release block), `.github/workflows/{ci,e2e,nightly}.yml` (the test-command rewrite and the build-release job), `tools/web-paint-bench.sh`, AGENTS.md trap, DEVELOPERS.md, `docs/ci.md`, Dockerfile comment | yes (dependency lists) | 1 |
| 3 | `ec/crypto-single-provider` | EC-2 | root `rcgen` line, `ghostframe-lib/Cargo.toml` rustls line, `ghostframe-e2e/Cargo.toml` rustls/rcgen, `ci/check-release-features.sh` (ring guard) | yes (small) | 2 (same script; rebase) |
| 4 | `ec/lib-rlib` | EC-6 | `ghostframe-lib/Cargo.toml` `[lib]`, DEVELOPERS.md:22 | no | owner decision; coordinate with LD if (c) |
| 5 | `ec/cargo-deny` | EC-3 | `deny.toml`, `ci.yml` (new job), Justfile (`deny`, `deny-offline`, ci-local hook), `docs/ci.md` | no | 1 (`publish = false` makes `allow-wildcard-paths` work) |
| 6 | `ec/go-hermetic` | EC-5 | `ghostframe-tsnet/build.rs`, `ghostbridge/Makefile`, `.github/workflows/_setup-go/action.yml`, Dockerfile warm layer and apt list | no | none (parallel with 2–5) |
| 7 | `ec/ci-single-source` | EC-8 | `.github/workflows/**` (SHA pins, `_setup-rust`, client-core.yml), Justfile (`ci-clippy`, `ci-unit`, `ci-release`, ci-local rewrite), `rust-toolchain.toml` comment, `docs/ci.md` | no | after 2, 5 and 6 (they edit the same workflow lines) |
| 8 | `ec/lints-table` then `ec/lints-<crate>` ×6 | EC-4 | root `[workspace.lints]`; each crate's `[lints]` plus its crate-root attributes and fixes | no | 1. Step 6 (lib/tsnet/capi allow attributes) lands before UF and LD start |
| 9 | `ec/spirv-gate` | EC-9 | `Justfile` (`shaders`, `check-shaders`), shader build list, a CI step | no | 7 (uses the recipe pattern) |
| 10 | `ec/e2e-dedupe` | EC-10, EC-12 | `ghostframe-e2e/Cargo.toml` (axum, which, reqwest), `ghostframe-e2e/src/harness/transport.rs`, `ghostframe-bench/Cargo.toml` dev-deps, `benches/README.md` | yes | 2 (both edit e2e/bench manifests) |
| 11 | `ec/repo-hygiene` | EC-11 | `.gitignore` | no | none (any time) |
| 12 | `ec/edition-2024-<crate>` (later track) | EC-4 edition | one crate per PR | no | 8 done for that crate; lib last |

**Order:** 1 → 2 → 3 → (4, 5, 6, 11 in parallel) → 7 → 8 (rolling) → 9 → 10 → 12.
PR 2 is the High finding and should not wait on anything except the mechanical PR 1.
If PR 1 cannot land quickly, swap them: PR 2 touches only bench and e2e manifests plus
the root `default-members`, so it conflicts with PR 1 only on those `[package]` headers.
