# Remediation plan, 2026-11-25

**Input:** [full review 2026-11-25](2026-11-25-full-review.md) (57 findings, 3
stale) · **Designs:** [AV](2026-11-25-remediation/AV.md),
[WP](2026-11-25-remediation/WP.md), [UF](2026-11-25-remediation/UF.md),
[LD](2026-11-25-remediation/LD.md), [NC](2026-11-25-remediation/NC.md),
[EC](2026-11-25-remediation/EC.md) · **State:**
[scoreboard](remediation-scoreboard.md#campaign-2026-11-25) · **Base:**
`origin/master` `e85fb005`.

This file is the orchestration layer. **The design sections are the
specification.** Each finding there has its verified location, the chosen
design and the rejected alternatives, numbered implementation steps, the tests
with a mutation check, gates, risks and a size. An implementer works from the
section. This file says what runs when, who owns which file, which owner
decisions block what, and how the sections' overlaps are resolved.

The campaign has about 85 PRs: AV 8, WP 7 (some split), UF 15, LD 31, NC 8,
EC 12 plus per-crate lint and edition PRs. Most are size S. The large items are
the LD-1 `IoBridge` decomposition (a stack of pure moves), UF-8's ghostframe-lib
SAFETY burn-down and LD-6's `gpu_pipeline` splits.

---

## 0. Before anything spawns

1. **Sync.** The owner's main checkout was 275 commits behind `origin/master`
   at review time. Every worktree branches from a freshly fetched
   `origin/master`. Never branch from the local `master`.
2. **Land this plan** as a docs-only PR (`docs/review-2026-11-25`): the review,
   this plan, the six sections, `data/2026-11-25/count_unsafe.py` and the
   scoreboard section.
3. **Re-verify first (every workstream, first task, Haiku).** Re-check each
   assigned finding's Status line against the branch's base commit. If it is
   already fixed, record the commit on the scoreboard and drop the task. Master
   moves fast: the sections were verified at `e85fb005`, not at your base.
4. **Owner decisions** (section 3): D1, D2 and D6 block Wave A or B items. Ask
   for them on day one. Everything without a decision dependency can start
   anyway.

## 1. Mechanics

**Roles and models** (owner rule: Haiku for trivial, Sonnet by default, Opus
for reviews only):
- **Top orchestrator (Opus).** Owns this plan, wave gating, merge order, the
  `io_bridge` track (section 2.1), conflict arbitration and the scoreboard.
  Runs the gate tables and verifications itself. Never writes product code.
- **Workstream implementer (Sonnet), one per worktree.** Follows its design
  section and runs the gates.
- **Haiku** handles verification greps, `.gitignore`, comment sweeps (LD-10),
  dead-code deletion and pin-policy edits.
- **Opus** does every PR review, adversarially: break the fix and watch the new
  test fail. It also does a pre-implementation design check on items marked
  `[opus-design]` below.

**Worktree protocol.**
```bash
git fetch origin
git worktree add ../gf-<branch-slug> -b <branch> origin/master
```
- Give each worktree its own target dir. A/B builds must not share one
  (memory: two worktrees, one target dir).
- e2e images share one global tag, so only one worktree runs e2e at a time.
- GPU tests run through `gf-gpu-run`, never two at once. Parallel GPU load has
  reset amdgpu on the dev host.

**Gates per PR.** These come from AGENTS.md and the memory notes. Each section
lists the subset that applies.
1. `gf-gpu-run just ci-local` (or `just ci-client` for client-only PRs) before
   pushing. Clippy alone is not green: fmt, the env-read guard, the home-path
   guard and the named test targets all gate too.
2. Any ghostframe-lib or ghostframe-server change: `just containers-build`
   before e2e, then the e2e tests the section names.
3. Wire or embedded-web change: `just build-release` (never bare
   `cargo build`, and never piped into `tail`). Confirm the bundle hash changed
   in the binary. Touch all four consumers: protocol, lib, client-core,
   client-wasm.
4. A `pub` item change in ghostframe-lib or client-capi: regenerate and commit
   the cbindgen header.
5. A new `tests/*.rs` target: add it by name to the CI workflow.
6. Every new test passes the section's **mutation check**: revert the fix and
   watch the test fail. Prove that a probe is non-zero.
7. Stage explicit paths, never `git add -A`. After a batch of auto-merges,
   check that master compiles (memory: rebase-merge semantic conflicts).
8. Browserless transport-behaviour changes: loop the probe scenes 12 times
   against master and report three or more seeds.
9. Capture a flake's test name before re-running it.

**PR conventions.** Conventional-commit title. The body lists the finding IDs
(for example "review 2026-11-25 AV-1"). The PR updates its scoreboard row. One
logical change per PR; pure-move PRs say so and carry their move script.

**New tests go in new files.** `io_bridge/tests.rs` is about 9,900 lines and
every group appends to it. New IoBridge tests go in a new
`io_bridge/<topic>_tests.rs` module, which removes the commonest textual
conflict in this campaign.

---

## 2. Waves

A wave starts when its predecessor's **gating items** are merged, not when
every PR in it is. Inside a wave, rows on different tracks run in parallel.
Rows on one track are serial.

### 2.1 The `io_bridge` track (one tenant at a time)

`ghostframe-lib/src/transport/io_bridge.rs`, `io_bridge/session.rs` and
`io_bridge/frame.rs` are the contention points of the whole campaign. The top
orchestrator gives the track to one PR at a time, in this order:

| # | PR | Section | Wave |
|---|---|---|---|
| 1 | `fix/av-decode-error-grid-bounds` (AV-1) | AV PR 1 | A |
| 2 | `fix/av-xtest-worker` (AV-3, AV-6, AV-7 input warnings) | AV PR 4 | B |
| 3 | `fix/av-release-held-input` (AV-4) | AV PR 5 | B |
| 4 | `fix/av-session-start-bucket` (AV-8) | AV PR 6 | B |
| 5 | `fix/av-log-rate-limits` (AV-7, Rust half) | AV PR 7 | B |
| 6 | `wp/seq31` (WP-1; two one-line hunks in `frame.rs` plus `h264_tx.rs`) | WP PR 4 | B |
| 7 | `ld/config-threading` (LD-8; one hunk in `base_with`) | LD PR 2 | B |
| 8 | `fix/uf-frame-outputs-view` (UF-1/UF-4; `frame_backend.rs`/`frame.rs`) | UF PR 1 | B |
| 9 | `wp/feedback-codecs-to-protocol` 5a, then 5b (about 190 lines of `dispatch_feedback_bytes`) | WP PR 5 | C |
| 10 | `wp/protocol-consistency` 7a/7b/7c | WP PR 7 | C |
| 11 | **LD-C stack** #9 to #25 (bridge errors, `TileCoord`, clock, harness hooks, stat lines, the `IoBridge` decomposition) | LD | D (freeze) |

Rows 6, 7 and 8 touch other files under `io_bridge/` and may overlap one
another, but none may overlap rows 2 to 5. During row 11 non-urgent io_bridge
changes are frozen. Urgent fixes land on master, and the open LD PR rebases
onto them by re-running its move script.

### 2.2 Wave A: stop the outages (all parallel except the io_bridge row)

Gating items for Wave B: **AV-1, AV-2, NC-A.**

| Track | PRs, in order | Notes |
|---|---|---|
| io_bridge | AV PR 1 `fix/av-decode-error-grid-bounds` | `[opus-design]`. Smallest and most urgent. |
| server | AV PR 2 `fix/av-capture-exit-nonzero`, then AV PR 3 `fix/av-x11-wait-retryable` | Both edit `capture.rs`: serial, or fold into one. PR 2 merges right after AV-1. D6 decided: PR 2 carries the restart policy, and PR 3 uses a hidden `capture --x11-wait-ms` test flag instead of a public variable. |
| native GPU | NC PR A `fix/native-texture-limits` | First a GPU test at 2560 and 4K proves the panic, then the fix. Serialised through `gf-gpu-run`. |
| client | WP PR 1 `wp/palrle-index-oob`; WP PR 2 `wp/client-net-connect-bounds`; WP PR 3 `wp/h264-drop-refcontinuity` | Disjoint crates. |
| small unsafe | UF PR 3 `fix/uf-openh264-trace-unwind`; UF PR 4 `fix/uf-export-drop-leak-on-busy`; UF PR 5 `fix/uf-gst-download-bounds` | Disjoint files. |
| go | AV PR 7's Go half as `fix/av-ghostbridge-log-rate-limit` | Independent of the Rust half. |
| repo | EC PR 11 `ec/repo-hygiene` | Haiku. |

### 2.3 Wave B: robustness and the workspace foundation

Gating items for Wave C: **EC-7, EC-1, NC-B, AV-4, UF-1.**

| Track | PRs, in order | Notes |
|---|---|---|
| io_bridge | rows 2 to 8 of section 2.1 | Serial. |
| native client | NC PR B `fix/native-thread-supervision` (NC-1, NC-4, `WakeFd`, `RenderThreadArgs`), then NC PR C `fix/native-gpu-errors`, then NC PR D `fix/native-event-queue-bounds` | Same files (`render_thread.rs`, `lib.rs`), so serial. NC-B is `[opus-design]`. |
| manifests | EC PR 1 `ec/manifest-policy` (quiet window, announced), then EC PR 2 `ec/test-feature-isolation`, then EC PR 3 `ec/crypto-single-provider` | One `Cargo.lock`-changing PR in flight at a time. EC-1 rewrites about 36 root `cargo test --test` commands: the before/after list of named targets must be identical (memory: CI runs tests by name). If PR 1 cannot land quickly, swap PRs 1 and 2. |
| workspace (parallel after EC PR 1) | EC PR 5 `ec/cargo-deny` (per D3); EC PR 6 `ec/go-hermetic` | EC PR 6 has no dependencies and can start in Wave A. |
| gpu_pipeline | UF PR 1 (io_bridge row 8), then UF PR 2 `fix/uf-borrowed-fd` | Same files: serial, and through `gf-gpu-run`. |
| lib leaf | LD PR 1 `ld/dead-code`, then LD PR 2 (io_bridge row 7); LD PR 4 `ld/quic-errors` | `quic.rs` is LD's alone in this wave. |
| server install | AV PR 8 `feat/av-install-records-display` | D6 decided: record the installing shell's `DISPLAY`, `--display` overrides, `:0` with a warning otherwise (AV-5 part 2 as designed). |

### 2.4 Wave C: consolidation

Gating items for Wave D: **EC-4 step 6, UF-8 7a–7f, WP-5 (5b), LD-B #5–#8, NC-E.**

| Track | PRs, in order | Notes |
|---|---|---|
| io_bridge | rows 9 and 10 of section 2.1 | WP PR 5 needs D1; WP PR 7a needs D5. |
| fuzz | WP PR 6 `wp/fuzz-nightly` | After WP PRs 1 and 5b; needs D4. It lands before 7b so the fuzzers guard the `Result` conversion. |
| lints | EC PR 8 `ec/lints-table`, then `ec/lints-<crate>` steps 1 to 6 | **Step 6** opts lib, tsnet and capi in with temporary reasoned `#![allow(...)]`s and must land before any UF-8 or LD lint work (R2). |
| SAFETY burn-down | UF PRs 7a–7f `chore/uf-unsafe-lints-<crate>`, in UF-8's crate order | Each removes the crate's temporary allow. 7a (the CLI) comes after NC PR E (R1). |
| gpu_pipeline refactor | LD PRs 5 `ld/gpu-error`, 6 `ld/gpu-setup-split` (pure move), 7 `ld/gpu-setup-guard`, 8 `ld/gpu-frame-split` (pure move) | Only after UF PRs 1 and 2. SAFETY comments move with the code. |
| native, C API, CLI | NC PR E `refactor/client-gpu-options` (NC-6, NC-5 step 5, **plus UF-7**), then NC PR F `feat/capi-abi-v1`, then NC PR G `fix/cli-state-dir` (D1), then NC PR H `chore/client-hygiene` | NC-F is `[opus-design]` (ABI). NC-H comes after WP PR 5a (R4). |
| shared syntax | LD PR 3 `ld/h264-syntax-crate` (LD-11) | Needs D1. A new crate must be added to the e2e Dockerfile's manifest COPY list. |
| CI | EC PR 7 `ec/ci-single-source`, then EC PR 9 `ec/spirv-gate`, then EC PR 10 `ec/e2e-dedupe` | EC PR 7 owns the workflow files while it is open (R5). |

### 2.5 Wave D: decomposition and the surface

| Track | PRs, in order | Notes |
|---|---|---|
| io_bridge | the LD-C stack, LD PRs 9 to 25 (row 11) | Freeze window, about 2 to 3 weeks. Every pure move is checked against the `DETERMINISM-DIGEST` hashes, an unchanged test count and byte-identical stats lines (LD-6's golden test, PR 15, lands first). |
| lib unsafe | UF PRs 8a–8d `chore/uf-unsafe-lints-lib-*` | After LD-B, and never in flight alongside an LD PR on `gpu_pipeline/frame.rs`. |
| surface | LD PR 26 `ld/pub-surface`, then LD PR 27 `ld/c-api` (per D2; R3), then LD PRs 28–31 comment sweeps (Haiku) | After row 11. |
| edition | EC PR 12 `ec/edition-2024-<crate>`, one crate per PR, ghostframe-lib last | Check that cbindgen accepts `#[unsafe(no_mangle)]`; that support is UNVERIFIED. Keep lib and capi on 2021 if the header diff is not empty. |

AV-9 (a view-only role) is not scheduled. It becomes a design doc if D7 says
so.

---

## 3. Owner decisions

| ID | Decision | Blocks | Recommendation (from the sections) |
|---|---|---|---|
| **D1** | Relicense code moved from AGPL server files into MIT/Apache crates: the feedback, HELLO, display and input codecs into `ghostframe-protocol` (WP-6); H.264 syntax into a shared crate (LD-11); the state-dir and lock helpers into `ghostframe-tsnet` (NC-10). | WP PR 5, LD PR 3, NC PR G | **Approved 2026-11-25** for all three. The owner is the sole author of the moved files. Each PR changes the moved files' SPDX headers and cites this decision in its body. |
| **D2** | The server C API (`ffi.rs`, `ghostframe.h`, cdylib and staticlib). | EC PR 4, LD PR 27 | **Decided 2026-11-25: keep.** The C API is the planned route to a headless Wayland server, where a compositor embeds the server. LD PR 27 moves `ffi.rs` into a new AGPL crate `ghostframe-server-capi` (LD-7 (b)), which owns the cdylib/staticlib, cbindgen and the header gate. Its header exports only the API, not lib's tuning constants. ghostframe-lib becomes rlib only in the same PR (EC PR 4 is folded in). The API's growth (dma-buf submit, damage, input and display-mode callbacks) is a separate design, not part of this campaign; see section 4, R10. |
| **D3** | Should cargo-deny advisories block merges? | EC PR 5 | Advisories `warn` on PRs and `deny` on the scheduled run; bans and sources `deny`. |
| **D4** | A nightly toolchain in CI for `cargo fuzz` and Miri. | WP PR 6 | Yes, in `nightly.yml` only. |
| **D5** | The wire epoch scheme (in the CONNECT URL, per WP-7), deleting ping/pong, and whether a FIN after `Accepted` means `ConnectionLost` (WP-3 step 5). | WP PRs 7a, 7c, 2 | Epoch in the URL; delete ping; yes to FIN. No compatibility is needed (owner rule: no production deployments). |
| **D6** | systemd restart policy after AV-2; a wait-time knob for X11; how `install` records `DISPLAY`. | AV PRs 2, 3, 8 | **Decided 2026-11-25.** (1) The unit never gives up: `StartLimitIntervalSec=0`, `Restart=on-failure`, `RestartSec=2`, `RestartSteps=6`, `RestartMaxDelaySec=60s`; exit 2 is still permanent. Older systemd should ignore the two unknown keys and keep a fixed 2 s delay (UNVERIFIED, check on 252). This is in AV PR 2. (2) No public `GHOSTFRAME_X11_WAIT_MS`; tests use a hidden `capture --x11-wait-ms` flag, and the default wait is 30 s. (3) `install` records `DISPLAY` as AV-5 part 2 designs. |
| **D7** | A view-only role (AV-9: (a) document full control, (b) per-identity role, (c) operator confirmation of takeovers). | nothing scheduled | (a) now; (b) as its own design if wanted. |
| **D8** | Version pins: caret ranges by default, `=` only for documented couplings. Is rustls on the `=` list? | EC PR 1 | Caret by default; keep rustls and quinn-proto pinned with the ML-KEM reason. |
| **D9** | Stop building `ghostframe-client-net` for wasm in CI (NC-8)? | NC PR H | Yes, nothing uses it on wasm. |
| **D10** | A C-compiler check of `ghostframe_client.h` in CI (NC-5)? | NC PR F | Yes, a small `cc -fsyntax-only` step. |
| **D11** | Frames wider than 8128 px: cap the grid at 254 tiles (LD-5) or widen the wire to u16? | LD PR 10 | Cap now; widening is a four-crate wire change for a resolution nobody serves. |
| **D12** | The io_bridge freeze window for the LD-C stack. | Wave D | Schedule it after Wave C and announce dates. |

---

## 4. Resolutions between sections

The sections were written in parallel. Where they overlap, this table is
authoritative.

| ID | Overlap | Resolution |
|---|---|---|
| **R1** | UF-7 (remove the CLI `set_var`) and NC-6 (pass GPU knobs through constructors) both delete the same `set_var` block. | **NC PR E owns it.** UF PR 6 is dropped. NC-E carries UF-7's `Config::dump` design, its test and the `check-env-reads.sh` `set_var` ban for `ghostframe-cli/src`. UF-8's CLI step (7a) follows NC-E. |
| **R2** | Where the unsafe lints live (UF-8 offers two placements; EC-4 proposes `[workspace.lints]`). | **EC-4's path.** EC lands the table. Its step 6 opts in lib, tsnet and capi with reasoned temporary `#![allow(clippy::undocumented_unsafe_blocks, ...)]` (and `unreachable_pub` for lib, owned by LD). UF and LD PRs only ever *remove* an allow. A local `warn` phase is meaningless because CI runs `-D warnings`. |
| **R3** | The C API: EC-6 (crate type) and LD-7 #27 (delete or move). | **One change (D2 decided: move).** LD PR 27 creates `ghostframe-server-capi` and moves `ffi.rs`, `include/ghostframe.h`, the cbindgen config and the header gate there. In the same PR it drops ghostframe-lib's `crate-type` line. EC PR 4 is dropped. Add the new crate to the workspace members, the e2e Dockerfile manifest COPY list and `check-licenses.py`'s AGPL side. It can land before LD PR 26 if the header leak should stop sooner. |
| **R4** | `DecodeErrorCode` `TryFrom<u8>` (WP-6, NC-8). | WP PR 5a defines it once in `ghostframe-protocol`. NC PR H migrates the render and wasm call sites. |
| **R5** | Workflow edits: UF-1 (a doctest step), WP-5 (fuzz and Miri jobs), NC-H (`client-core.yml`), EC-8 (restructure). | Small additive hunks land before EC PR 7 opens, or after it merges using its recipes. Never while EC PR 7 is open. EC PR 7 must keep every named test target and the new doctest step. |
| **R6** | Frame-size limits: AV-1 (grid bounds), LD-5 (254-tile cap), NC-2 (adapter texture limit), client-core `MAX_FRAME_DIMENSION = 8192`. | AV-1 checks against the **current** grid and drops out-of-grid messages. LD PR 10 defines the single server limit (254 tiles × 32 = 8128 px) in `ghostframe-protocol` and applies it to display negotiation and attach. The client keeps `min(protocol limit, adapter max_texture_dimension_2d)` (NC PR A). A frame bigger than either is refused with an error, never a panic. |
| **R7** | Licensing: three separate asks. | Consolidated as D1, one answer. |
| **R8** | AV-7 rate-limits `client decode error`, which the bench counts. | Per AV-7's design: the warn is rate-limited, but a debug-level line with the same message on `ghostframe::bench` is kept for every occurrence, and AV-7's test asserts that 200 errors still give 200 lines (the bench contract). |
| **R10** | The server C API stays as the headless Wayland route (D2), but today it accepts only CPU pixels (`gf_server_submit_frame`) and has no input, display-mode or cursor callbacks. | Out of scope here, but the campaign must not close doors for it. (1) Session-end input release (AV-4) is tracked in ghostframe-lib's `HeldInput`, not only in the XTest worker, so an embedder's injector gets it. (2) AV-3's injector worker sits behind the `InputInjector` trait, so a C callback can implement it. (3) UF-5's `BorrowedFd` APIs are the dma-buf intake a compositor needs. (4) The new capi crate applies NC-5's ABI rules from the start: `struct_size`, `uint32_t` fields instead of C enums, version macros, a fourcc on frames. |
| **R9** | LD-1's field table vs fields added by Wave B and C PRs. | LD's table is a snapshot at `e85fb005`. The first LD-C PR regenerates it with the section's script and lists every field added since. |

---

## 5. Acceptance

The campaign is done when:
1. Every scoreboard row is `merged`, `closed-already-fixed`, `stale` or
   `decided` (an owner decision recorded with its date).
2. An Opus re-review of the final `origin/master` against the findings index
   finds no CONFIRMED finding open. The method is the same as the review's:
   read the code at the commit, and record file:line for each closure.
3. The structural guards exist and fail when their regression is reintroduced.
   Each was mutation-checked once and recorded in its PR:
   - `check-release-features.sh` covers both the e2e dependency and the ring
     path.
   - cargo-deny runs.
   - `[workspace.lints]` is on in every crate with no temporary allows left.
   - Fuzz and Miri run nightly.
   - The SPIR-V gate exists.
   - The home-path guard passes on a release binary built with `-trimpath`.
4. AGENTS.md traps made obsolete by structure are rewritten: the
   workspace-release trap (EC-1), the cbindgen `pub const` trap (D2), and the
   ring note (EC-2). Do not leave text describing a trap that can no longer
   happen.
