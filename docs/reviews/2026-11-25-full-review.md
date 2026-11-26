# Full project review, 2026-11-25

**Scope:** the whole workspace (15 Rust crates, about 220k lines, plus the Go
bridge in `ghostbridge/`), reviewed for security, unsafe soundness, design and
code quality, fit with the Rust ecosystem, and engineering hygiene.
**Base:** `origin/master` at `e85fb005` (2026-11-23).
**Remediation:** [plan](2026-11-25-remediation-plan.md) · design sections in
[2026-11-25-remediation/](2026-11-25-remediation/) · state in the
[scoreboard](remediation-scoreboard.md#campaign-2026-11-25).

## How the findings were made

1. Seven read-only reviewers, one per area: the server network surface, client
   and wire format, unsafe and FFI, ghostframe-lib design, the daemon and CLI,
   the client crates, and workspace hygiene. Six of them read a local checkout
   275 commits behind `origin/master`. Their line numbers are therefore not
   used anywhere.
2. Six design agents then re-checked every finding against `e85fb005`, one per
   group, and wrote the design sections. Each finding carries a **Status at
   e85fb005** line, either CONFIRMED with file:line or STALE with the closing
   commit.
3. The lead checked the High findings at `e85fb005` independently: AV-1, AV-2,
   NC-2's 2048 px texture limit, LD-5's reachability and EC-1.

Nothing was built or run during the review. Every claim comes from reading the
code, plus `cargo tree`/`cargo metadata` offline and recompiling SPIR-V into
scratch. The design sections mark anything UNVERIFIED.

## Verdict

The codebase is unusually disciplined. Authentication fails closed: the peer
allowlist, HELLO gating, Origin policy and QUIC admission limits all held up
under review. The wire parsers check bounds before indexing, and the client
crates' FFI is careful, with close to one SAFETY comment per unsafe block. CI
guards things most projects do not: feature leaks, environment reads, the
generated C headers, licences and home paths.

The debt is in two places:

- **Failure behaviour.** A panic or a dead thread becomes a silent outage. The
  daemon exits 0 and is not restarted, and the native client freezes while it
  keeps acknowledging.
- **The composition layer.** `IoBridge` has 164 fields. Errors are
  `Box<dyn Error>`, and harness hooks are woven into production types. Fixes
  are caught by guard scripts after the fact instead of being made impossible
  by structure.

## Findings index

Severity is the design section's, after re-verification. Groups:
**AV**: server availability and input. **WP**: wire protocol and client-core.
**UF**: unsafe and FFI. **LD**: ghostframe-lib design. **NC**: native client,
C API and CLI. **EC**: workspace, supply chain and CI.

### Security and availability

| ID | Sev | Finding | Status |
|---|---|---|---|
| AV-1 | **High** | Client `tile_x`/`tile_y` index the metrics grid unchecked. A 5-byte DECODE_ERROR from any admitted peer panics the IoBridge. The grid is 0x0 until the first frame, so even valid coordinates panic right after HELLO. Out-of-grid values alias in release builds. | CONFIRMED, worse than first reported |
| AV-2 | **High** | A dead IoBridge makes `capture` exit 0, so `Restart=on-failure` never restarts it. | CONFIRMED |
| NC-2 | **High** | The native device keeps wgpu's 2048 px `downlevel_defaults` texture limit and never checks it. A screen over 2048 px fails texture creation, and wgpu's default handler panics the render thread. There is no uncaptured-error or device-lost handler. | CONFIRMED, new in design pass |
| NC-1 | **High** | A dead decode or render thread freezes the picture while the net thread keeps ACKing. The host and the server are never told. | CONFIRMED |
| AV-3 | Medium | XTest injection does a synchronous round trip per event on the IoBridge loop. Wheel ticks are uncapped (i16, so about 65k round trips per message). The Wayland client sends axis pixels instead of notches. DisplayMode does a synchronous RandR call per message. | CONFIRMED |
| AV-4 | Medium | Held keys and buttons are never released when a session ends. A takeover switches live sessions before the eviction runs. | CONFIRMED |
| AV-5 | Medium | A slow X server exits 2, which the unit treats as permanent. `DISPLAY=:0` is hard-coded in the unit. | CONFIRMED |
| NC-10 | Medium | The CLI state dir is not private and has no lock. `login`/`connect`/`logout` disagree on the control URL and hostname. Ctrl-C during the auth-key prompt leaves echo off. | CONFIRMED |
| AV-6 | Low | The keysym cache is never invalidated on MappingNotify. A miss refetches the whole keymap. | CONFIRMED |
| AV-7 | Low | Journal flooding: the Go accept log runs before the auth check, and per-message warnings are not rate-limited. | CONFIRMED |
| AV-8 | Low | A connect → HELLO → close loop forces a full session reset each time. | CONFIRMED |
| AV-9 | Low | No view-only role: every admitted peer has full input and can take over. | Owner decision |

### Wire protocol and client robustness

| ID | Sev | Finding | Status |
|---|---|---|---|
| WP-0 | — | The 0x04 tile-parity envelope collision. | **STALE** (5cd35e7f removed the envelope) |
| WP-1 | Low-Med | `frame_seq` is not a closed 31-bit space: the server counter is never masked or reset, and some comparisons do not wrap. | CONFIRMED |
| WP-2 | Low | The CPU PalRle decoder never checks `index < count`, so a bad index paints a stale colour. The production native client uses this path. | CONFIRMED, wider |
| WP-3 | Low | The native CONNECT response buffer is unbounded and has no deadline. `SessionReady` can fire again after accept. | CONFIRMED, plus a latent defect |
| WP-4 | Low | `RefContinuity` has unbounded SPS/PPS tables and is unused. | CONFIRMED |
| WP-5 | Medium | Fuzzing is thin and never runs. Server feedback parsers live in ghostframe-lib, where they cannot be fuzzed. | CONFIRMED |
| WP-6 | Medium | Wire codecs are duplicated between server and client: decode error, display, HELLO, input events, and `DecodeErrorCode` mapped three times. | CONFIRMED, broader |
| WP-7 | Low | Protocol consistency: no wire epoch, mixed error types, `frag_total as u16`, mixed endianness, dead ping/pong. | CONFIRMED, partly |

### Unsafe and FFI

| ID | Sev | Finding | Status |
|---|---|---|---|
| UF-1 | Medium | `FrameAnalysis` hands out safe slices over raw pointers that nothing ties to the processor, so safe code can cause UB. Production follows the convention. | CONFIRMED |
| UF-2 | Low | `ExportedImage::drop` frees memory after a failed or timed-out GPU wait. | CONFIRMED |
| UF-3 | Low | The GStreamer CPU download bounds-checks only the chroma plane. | CONFIRMED |
| UF-4 | Low | An escalation coefficient read is bounded only by an upstream truncation. | CONFIRMED |
| UF-5 | Low | Public safe functions take `RawFd` and `mmap` it. | CONFIRMED (C-API half STALE) |
| UF-6 | Info | The openh264 trace callback has no `catch_unwind`, so a panic aborts. | CONFIRMED |
| UF-7 | Info | The CLI calls `set_var` under a false "single-threaded" comment. Folded into NC-6. | CONFIRMED |
| UF-8 | Medium | SAFETY coverage: ghostframe-lib has 74 undocumented unsafe sites in src and 37 in tests, ghostframe-tsnet 11 and 8. No `unsafe_op_in_unsafe_fn` lint. | CONFIRMED (counts: `data/2026-11-25/count_unsafe.py`) |

### ghostframe-lib design

| ID | Impact | Finding | Status |
|---|---|---|---|
| LD-1 | High | `IoBridge` is a god object: 164 fields (33 cfg-gated) and 19 `impl` blocks. Tests poke its fields on 2105 lines. | CONFIRMED |
| LD-2 | High | Type-erased errors: 58 `Box<dyn Error>`, 14 `Result<_, String>`, and control flow by downcast (`frame_backend.rs:500`). | CONFIRMED |
| LD-3 | High | Harness plumbing in production types: 212 feature gates and nine shared result cells. | CONFIRMED |
| LD-4 | Medium | A blanket `#[allow(dead_code)]` covers the whole H.264 encoder. | CONFIRMED |
| LD-5 | Medium | Unchecked `as u8` tile coordinates. **Reachable:** 8224 px wide passes the 16384-tile cap, and column 256 aliases column 0, via display negotiation or attach mode. | CONFIRMED |
| LD-6 | Medium | Monster functions. `gpu_pipeline/setup.rs::new_inner` is 886 lines with 67 early returns and no cleanup guard, so it leaks on a setup error. | CONFIRMED, plus leak |
| LD-7 | Medium | The pub surface is far wider than its users. 14 tuning constants leak into `ghostframe.h`, and nothing in the repo calls that C API. | CONFIRMED |
| LD-8 | Low/Med | `OnceLock` config globals are read deep in the code and force a process-wide env lock in tests. | CONFIRMED |
| LD-9 | Low | Two clocks inside IoBridge (`Instant::now` alongside `now_std`). | CONFIRMED |
| LD-10 | Low | About 350 comment lines narrate history ("Stage N", "M3.x", "used to"). | CONFIRMED |
| LD-11 | Medium | The H.264 syntax layer is duplicated in server and client. The client's `log2_max_frame_num` can overflow on a crafted SPS. | CONFIRMED, plus bug |

### Native client, C API and client hygiene

| ID | Sev | Finding | Status |
|---|---|---|---|
| NC-3 | Medium | Unbounded queues between threads and to the host. Tiles are ACKed on receipt, so a blocking bounded channel would deadlock or stall ACKs. | CONFIRMED |
| NC-4 | Medium | The render thread leaks when a thread spawn fails during `connect`. | CONFIRMED, narrower |
| NC-5 | Medium | C API gaps: no fourcc in `gf_frame`, C enums as struct fields, `gf_result_str(u32)`, control URL only via environment. | Partly STALE (`gf_version()` exists) |
| NC-6 | Med/Low | GPU debug knobs are read from the environment inside library constructors, and dump paths come from the environment. | CONFIRMED |
| NC-7 | Low/Med | Test-only code ships: ungated `testdata` module, `debug_*` helpers with `expect`. | Partly STALE (wasm shims gone) |
| NC-8 | Low | Duplication and idiom: eventfd write copied 6 times, an 11-argument `run`, missing `#[non_exhaustive]`, stale `prevalidate.ts` references. | CONFIRMED |
| NC-9 | — | Web and native CDF 5/3 GPU paths differ. | **STALE** (shared via `ghostframe-render`, both self-test) |

### Workspace, supply chain and CI

| ID | Impact | Finding | Status |
|---|---|---|---|
| EC-1 | High | Test features reach a workspace release build: bench → e2e is a normal dependency, and test-pattern also pulls the harness in. | CONFIRMED |
| EC-2 | Medium | The daemon links ring and aws-lc-rs: lib's rustls `ring` feature and the workspace rcgen. | CONFIRMED |
| EC-3 | Medium | No cargo-deny or cargo-audit. | CONFIRMED |
| EC-4 | Medium | No `[workspace.lints]`; edition 2021 everywhere. | CONFIRMED |
| EC-5 | Medium | The Go build is not hermetic: no `-trimpath` (1926 home-directory paths in a release binary), the SPA is copied into the source tree, and CI's Go version does not match `go.mod`. | CONFIRMED, one part STALE |
| EC-6 | Medium | ghostframe-lib builds a cdylib and a staticlib (373 MB `.a`, 161 MB `.so`) for no consumer. | CONFIRMED |
| EC-7 | Low/Med | 21 `=` pins, inconsistent workspace inheritance, no `[workspace.package]`, `publish = false` missing. | CONFIRMED |
| EC-8 | Low/Med | CI: actions pinned by tag, a floating `@main`, no `permissions` on client-core.yml, no `cargo doc` gate, drift from ci-local. | CONFIRMED |
| EC-9 | Low | 12 committed SPIR-V blobs, three different build commands, no staleness gate. | CONFIRMED |
| EC-10 | Low | Duplicate dependency versions, all in the e2e and web graphs. | CONFIRMED |
| EC-11 | Low | Repo-root hygiene (`.gitignore`). | Partly STALE |
| EC-12 | Low | Stale benchmark dev-dependencies (criterion 0.5, iai-callgrind 0.14). | CONFIRMED (upstream status UNVERIFIED) |

## Checked and found sound

- **Peer access:** `peer_auth` (tags override the user, the owner is compared by
  numeric ID, fail closed, capped caches) and the authorizer trampoline's
  `catch_unwind`.
- **QUIC and WebTransport:** admission before any `Connection` is allocated,
  per-source and table caps, Retry under load, migration off, the `Drained`
  handling, WebTransport handshake caps, and the Origin policy.
- **HELLO gating:** every non-HELLO message is gated on both the stream and the
  datagram paths.
- **Inbound parsers:** ACK, NACK, frame-ack, display, reset and FEC-feedback
  parsers. Client-core reassembly caps (`frag_total`, AU bytes,
  `FrameDimensions`), the parity and loss trackers.
- **Native TLS pinning:** a real signature check, current and successor
  hashes.
- **openh264 and libva bindings:** signatures and versioned layouts.
- **fd ownership:** at Vulkan import.
- **Server privilege handling:** root refusal, the 0700 state dir, the
  `TS_AUTHKEY` path, argv-only process spawning, and `StateLock`.

Details and evidence are in each design section's "Status" lines and in the
reviewers' notes summarised there.
