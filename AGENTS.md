# Notes for agents working in this repo

Start with **[DEVELOPERS.md](DEVELOPERS.md)** — repository layout, prerequisites,
test tiers, and alternative configurations. This file only adds what is easy to
get wrong and expensive to discover, most of it learned by getting it wrong.

Every common task is a `just` recipe. Read the `Justfile` before inventing a
command.

## Build, install, deploy

```bash
just build-release      # build-web THEN cargo build --release -p ghostframe-server
                        # -- the order matters; daemon only, see below
just ci-local           # the real gate: fmt, env-read guard, clippy --workspace --all-targets,
                        # unit tests, web build, tsc, release build,
                        # cbindgen header diff, go vet
just lint               # clippy --workspace --all-targets -- -D warnings
just test-unit          # cargo test --lib
just test-e2e           # containers + Chromium over a local headscale
just containers-build   # REQUIRED after any ghostframe-lib/ghostframe-server change,
                        # or e2e silently runs the stale image
```

Native client only — a strict subset, no server, no web bundle, no Docker.
Worth reaching for on a machine that cannot afford the workspace:

```bash
just build-client       # or build-client-release
just test-client        # clippy --all-targets + unit tests, client crates
just ci-client          # the client-only analogue of ci-local
```

Deploying to a real session (run it from that user's desktop session, never as root):

```bash
just build-release
./target/release/ghostframe-server install --force   # as the session's user, no sudo
./target/release/ghostframe-server login             # only if the node has not joined yet; it then enables and starts the unit
```

`login` stops being a hand-off: once joined it enables and starts the installed
unit itself (stop the service first, since it holds the state lock).

`install --force` replaces the binary and restarts `ghostframe-server.service`
when the node has joined. `just build-release` builds `-p ghostframe-server`
only.

Then **hard-reload the browser tab** (Ctrl+Shift+R). The wasm bundle is cached
and a soft reload keeps the old one.

## Traps

**A workspace-wide release build ships test code.** Cargo unifies features
over every package in one build, and `ghostframe-bench` depends on
`ghostframe-e2e`, which turns on ghostframe-lib's `test-loss-injection` and
`browserless-harness`. So `cargo build --release` at the root (and even
`--workspace --exclude ghostframe-e2e`) compiles the daemon with them.
`just build-release` builds `-p ghostframe-server` only and runs
`ci/check-release-features.sh`, which fails if the daemon's resolved feature
set contains a test-only feature. Build a daemon you will install with that
recipe, not by hand.

**`cargo build` does not rebuild the web client, and the client is embedded at
compile time.** `ghostbridge/web_server.go` does `//go:embed all:dist`, so the
browser client is baked into the daemon binary. Building with
`cargo build -p ghostframe-server` after a protocol change ships a server that
no longer speaks the embedded client's protocol. The symptom is not a clear
error — it is a NACK storm, slow convergence and visual artifacts. Use
`just build-release`, which runs `build-web` first. After building, it is worth
confirming the bundle actually changed:
`strings target/release/ghostframe-server | grep -c <new-bundle-hash>`.

**The embed is behind `ghostframe-tsnet`'s `web-embed` feature, off by
default.** Server crates enable it (`ghostframe-lib` does); client crates do
not, and get a `-tags noweb` ghostbridge with no SPA and no npm step. A new
server crate that forgets it will not silently serve 404s — ghostbridge's
`startWebListeners` refuses to bind and says why — but it will get that far
before anyone notices. See DEVELOPERS.md, "Building only the native client".

**`cargo test -p <crate>` hides test binaries that fail to compile.** A
non-compiling test target is not a failing test; it vanishes from the summary
and the run still reports `0 failed`. Only `cargo clippy --workspace
--all-targets` sees it. Treat clippy as the gate, not the test summary. This
has hidden a real regression here more than once.

**CI is x86-64 only, and some clippy lints are architecture-dependent.**
`libc::c_char` is `i8` on x86-64 and `u8` on aarch64, so a
`… as *mut [c_char; N] as *mut [u8; N]` cast is necessary on one and a
no-op on the other — and `clippy::unnecessary_cast` fires on the no-op,
failing `-D warnings` on ARM while CI stays green. `ghostframe-client-capi`
had exactly this (fixed by using `pointer::cast`). If clippy fails locally
on something CI passes, check your arch before assuming a stale toolchain.

**The systemd units are *user* units**, installed to
`~<user>/.config/systemd/user/`. A drop-in placed in `/etc/systemd/system/` is
inert. Use `~<user>/.config/systemd/user/<unit>.service.d/` or
`/etc/systemd/user/<unit>.service.d/`. `systemctl daemon-reload` re-reads unit
files but does **not** restart anything; `ghostframe-server install` restarts the
service only when the node has joined a tailnet.

**An X session on an inactive VT fails every modeset.** If the session's VT is
not the active one (a VT switch, or a second login), every RandR mode change
fails and Xorg logs `get vblank counter failed`. Nothing is wrong with
ghostframe. `loginctl activate <session>` (find it with `loginctl
list-sessions`) brings the session back without root.

**`ghostframe-lib/include/ghostframe.h` is generated by cbindgen and checked
in.** Any `pub const` in the crate lands in the public C API. Use `pub(crate)`
for internals. `just ci-local` diffs this file, so a forgotten regeneration
fails CI.

**Four crates consume the wire format**: `ghostframe-protocol`,
`ghostframe-lib` (server), `ghostframe-client-core` (client) and
`ghostframe-client-wasm` (the JS boundary). The wasm one is easy to miss
because per-crate test runs never touch it.

## Reading a live session

The daemon logs to the journal; its default filter is `ghostframe=info`
(`ghostframe-server/src/main.rs`). `RUST_LOG=ghostframe=debug` turns DEBUG on;
the e2e harness runs at `ghostframe=trace,debug`, so a DEBUG-only cost shows up
in e2e and not in a stock deploy.

```bash
journalctl _UID=$(id -u <user>) --since "-10min" --no-pager
journalctl _UID=$(id -u <user>) --no-pager | grep 'cumulative emit' | tail -1
```

That `cumulative emit` line is the most informative single line in the log:
per-codec emission counts, `loss_path_resends` (retransmissions made by the loss path:
verdicts and probes; called `rto_fired` before the per-pass timer was removed),
`nack_received`, `emitter_ack_hits` /
`ack_misses`, `retransmit_attempts_total`, `cache_pending_entries`, and the
latency tiers.

The service is `ghostframe-server.service` (`systemctl --user status
ghostframe-server`); `login`, `logout` and the service share a lock on the state
directory, so stop the service before `logout`. A node counts as joined when
`<state_dir>/.joined` exists; started without it and without `TS_AUTHKEY`,
`capture` exits 2 and the unit does not restart it.

In H.264 mode the same cadence also logs `h264 stats` (target
`ghostframe::h264`): IDRs by cause (`idr_session`, `idr_loss`,
`idr_client_reset`, `idr_reentry`, `idr_gop_periodic`, ...), verdicts on IDR and
P fragments, `p_hold_ms` (time P frames waited for an IDR to be acknowledged),
`frames_skipped_hold` / `frames_skipped_backpressure`, and the ledger identity
`tx_recorded >= tx_acked_live + tx_judged_lost + tx_forgotten` (the gap is still
outstanding and not logged; exact only within one session, without ledger
capacity evictions). An
H.264 fragment is a ledger transmission, so `bwe_samples_seen` moves in H.264
mode; a client that does not acknowledge H.264 fragments shows up as a loss
verdict on every one. `h264_stalled_ms` and `stalls_inflight` /
`stalls_no_progress` / `stalls_reset` say how long, and why, the bridge told the
classifier to leave H.264 (`mode.decision` reason `h264_stalled`; H.264 is not
re-entered for 2 s after, doubling per repeated stall up to 30 s; 10 s of
healthy H.264 resets it). The headroom guard reads the bandwidth *estimate*
(engage below 2 Mbit/s for 500 ms, release above 3 Mbit/s for 1 s, or once the
estimate is back over 2 Mbit/s and the screen has been idle for 2 s);
`loss_override` is off unless `GHOSTFRAME_LOSS_OVERRIDE=1`.

The H.264 stream of a **legacy (Phase A)** session is CBR at `0.8 x` the
bandwidth estimate (smoothed, floor 300 kbit/s). The rate changes only by
reopening the encoder at an IDR: `rate_target_bps` is the last rate it was told,
`idr_rate_drop` the immediate reopens for a fall of more than 30 % (these hold
P frames like a loss IDR), `rate_reopens_gop` the ones that waited for the
scheduled IDR. The GOP is 3 s of frames; `GHOSTFRAME_H264_GOP_SECONDS` picks 2
to 5 (anything else is clamped with a warning). A stream at `0.8 x` the
estimate is not application-limited to goog_cc's ALR detector, so the bridge
declares it so (`alr_signalled`, the detector is shown a sent size halved) and
fills the probe windows goog_cc then asks for with padding when no IDR is due:
copies of an acknowledged P-frame fragment under new `wire_seq`s
(`probe_padding_windows` / `probe_padding_bytes`; the client acknowledges and
discards them). A rise of more than 30 % reopens at once (`idr_rate_rise`), a
jump of the raw estimate is held for a second, and the estimate ceiling is held
off for a second around padding.

An **acked-reference ("refs") session** (HELLO 0x04, the `h264 refs` line) moves
the rate in place: no reopen, no IDR, no GOP (`RatePolicy::step_in_place`).
`rate_updates`, `rate_applies_up` / `rate_applies_down` count the changes,
`rate_sends` / `rate_sends_10s` the re-sends of the rate buffers (each costs a
short dip of 1.00 / 0.56 / 0.87 times the average frame, T4, so the policy keeps
a 250 ms floor between them, a 10 % band for a fall and 15 % for a rise, and
never re-sends inside the band). A jump of the raw estimate is held for 2 s
(`SPIKE_HOLD_IN_PLACE`; 1 s on Phase A). Every frame is planned under a
`RateTarget` (`bps`, a P-frame cap of 3 average frames `max_frame_bytes`, an IDR
budget of 0.25 s of the rate with a 32 kB floor `idr_max_bytes`, and the VBV):
`frame_cap_bytes` / `idr_cap_bytes` are the last plan's, `frames_over_cap` and
`max_cap_ratio_pct` observe the (soft) cap and are never a gate. **Probe raise:**
while a probe window is open (goog_cc's request or the application's own) the
P frames are planned at `RATE_SHARE x` the window's rate and tagged into the
window, so they carry part of the burst and padding tops up only the shortfall
(on the first frame of the window, sized for what the frames still to come are
expected to carry, and the whole shortfall on the last). `probe_raise_windows`,
`probe_raise_frames`, `probe_raise_bytes` on the `h264 refs` line count it;
`raised_frame_lost` (a verdict named a raised frame), `raised_frames_acked` and
`raised_delivery_us_mean` / `_max` are its cost. It keeps off after a cut, behind
a backlog, and for 5 s after a late acknowledgement (a reordering path reads a
burst as loss). The mutation switch is the harness field
`InjectedH264::probe_raise`, not an environment variable.

With `GHOSTFRAME_DAMAGE_MODE=shadow` or `on` the daemon also logs a cumulative
`damage safety net` line every 5 s (target `ghostframe::server::damage`);
`corrections_unswept` against `dirty_tiles` is the would-have-been miss rate
(design note section 3.4, "As landed (PR 4)").

`h264 stats` also carries `rate_target_bps` and the stall counters.
`GHOSTFRAME_H264_GOP_SECONDS` (default 3, clamped 2..5) sets the scheduled IDR
interval; `GHOSTFRAME_LOSS_OVERRIDE=1` turns the off-by-default loss override
back on. Every field and both variables are explained in
[docs/design/h264-transport.md](docs/design/h264-transport.md). A zero
`frame_acks_received` is expected (no client sends the 0x0A frame ack yet); it
is on the separate `h264 client feedback` line, target
`ghostframe_lib::transport::io_bridge`, not on `h264 stats`.

Client-side diagnostics are printed by the browser console. **Some of them are
dead**: `rx{...}` and `cdf53-coverage` read `window.__tileCounts` /
`window.__cdf53Coverage`, which nothing populates since the wasm cutover, so
they always read zero. `fec-coverage` says `n/a(wasm-internal)` explicitly.
`Δdrain` and `nack_sent` are live. Do not diagnose from the dead ones.

## Diagnostic probes

Env-gated, off by default:

- `GHOSTFRAME_RTO_PROBE=1` — one line per loss-path retransmission (`retx`, with
  its cause: a verdict or the probe), ledger verdict (`lost_ws`, with its
  rule), acknowledgement, late acknowledgement and resolve miss. The `loss
  path` journal event (same cadence as `cumulative emit`) carries `k_pkt`,
  verdicts by rule, `spurious_*`, `pto_fires` and the retransmission causes.
- `GHOSTFRAME_BLOCKING_PUBLISH=1` — native client: wait for the GPU after every
  export publish, as the ring did before it stopped. The default hands the
  buffer over while the blit is still running (GLES relies on Mesa's implicit
  sync on the shared dmabuf; Vulkan imports an exportable-semaphore fence into
  it). Use it to rule the non-blocking publish in or out when frames look torn.
  `GHOSTFRAME_NO_FENCE_DEVICE=1` (Vulkan) opens the device without
  `VK_KHR_external_semaphore_fd`, which also makes publishes block.
- `GHOSTFRAME_ACK_ORDER_PROBE=1` — every acknowledged `wire_seq` in arrival
  order, on target `ghostframe::ackorder`. Replaying an RFC 9002
  packet-threshold rule against that order says whether ordering-based loss
  inference is usable on a path.
- `GHOSTFRAME_NO_RTO=1` — disables the loss-path and probe-timeout retransmissions only, leaving the
  cache and NACK path intact. Useful for attributing a retransmission to its
  mechanism. Since the client tail sweep needs gap evidence, a lost *final*
  pass of a tile has no client-side repair: with the RTO off it stays
  unrepaired until the 30 s stuck-entry retire.

- `GHOSTFRAME_NO_UPLOAD_SLABS=1` — native client only: do not create the
  upload slabs (`ghostframe-client-gpu/src/upload_pool.rs`), so every decoded
  tile travels as a `Vec` and is staged by the renderer, as before. For A/B
  runs in one binary, and as the escape hatch if a driver mishandles the
  linear texture. With `RUST_LOG=ghostframe=debug` the render thread's `1s
  summary` shows `tile_slab` against `tile_ready` (`tile_ready_nonblack` does
  not scan slab tiles) and an `upload pool` line gives `exhausted` (tiles that
  fell back to a `Vec`), per-slab `(issued, consumed)` and the decode thread's
  time inside the dma-buf sync ioctls.

- `GHOSTFRAME_NO_COMPUTE_EXPORT=1` (client) — copy damage into the export buffer
  with the render-pass blit instead of the compute kernel that writes only the
  damaged tiles (`ghostframe-client-gpu/src/export_copy.rs`). On a tiled GPU the
  blit costs its damage's bounding box, so scattered damage is where the two
  differ. The ring also falls back to the blit by itself when the export cannot
  be bound as a storage image, and per frame when the damage fills most of its
  bounding box; `ExportRing::uses_compute_copy` says which it got at creation.

- `GHOSTFRAME_CDF53_DECODE=cpu` — native client only: reconstruct CDF 5/3
  tiles on the decode thread's CPU (`Cdf53TileState`), as before the GPU grid.
  Unset (or anything but `cpu`) means the GPU grid
  (`ghostframe-client-gpu/src/pipelines/cdf53_grid.rs`) **if** a start-up
  self-test on the render thread passes (it draws two known tiles and compares
  them to the CPU codec; look for `CDF 5/3 decode on the GPU (self-test
  passed)` or `... self-test failed; decoding on the CPU` in the log). The
  choice is made once per process, before the decode thread starts; in GPU mode
  the decode thread forwards `TileData::Cdf53` passes undecoded and the render
  thread's `Cdf53GridState` schedules them (about 7 ms of GPU per flush, carry
  over to the next flush, at most 1000 tiles per submit). A frame whose plane
  store exceeds the device's storage binding limit (about 8K) falls back to
  CPU decode on the render thread. With `RUST_LOG=ghostframe=debug` the
  render thread logs `cdf53 grid, 1s summary`: `listed`, `deferred` (budget),
  `throttled`, `skipped_no_magnitude`, `skipped_same_k`, `skipped_no_pass0`,
  `dropped_superseded` and `grid_frames`. **The reconstruct shader is fragile on
  panfrost:** when register allocation tips over (`MESA_SHADER_CACHE_DISABLE=true
  MIDGARD_MESA_DEBUG=shaderdb` shows "0 registers" and hundreds of spills) the
  dispatch silently writes nothing, with no error. After any edit to
  `shaders/client/cdf53_reconstruct.wgsl`, run `cargo test -p ghostframe-client-gpu
  --no-default-features --features gles,decode-gstreamer,test-support --release
  --test cdf53_grid` and check the shaderdb line (about 930 instructions, 8
  registers, ~21:57 spills is the shipped build).

Tile XOR parity (k=10) is **off by default** (`TransportConfig::tile_fec`).
`GHOSTFRAME_TILE_FEC=1` turns it back on; a browserless scene opts in with
`RunOptions { tile_fec: true, .. }`. The client decodes parity either way, and
H.264 parity is a separate path that this switch does not touch. A counter
that reads zero (`parity`, `client_recovered`, `late_acks_fec_possible`) on a
default run is the switch, not a broken probe.

See also `reference_e2e_loss_injection` and `reference_e2e_diagnose` notes for
the e2e-side injection variables.

## Known flakes

- `encoder::h264_vaapi::tests::full_frame_keyframe_interval` — VA-API
  contention under parallel test load. Re-run with `--test-threads=1`.
- The browserless suite can bail with `.expect("scene ran")` rather than an
  assertion. That is a harness stall, not a protocol failure; it is
  load-dependent. **Capture the failing test name before re-running** — it is
  easy to lose it and then be unable to tell a flake from a regression.

## Testing habits that pay here

This codebase has repeatedly produced tests that pass while proving nothing —
fixtures built from fixed-length buffers that cannot express the failing case,
counters incremented before the code they claim to measure, assertions whose
preconditions were never reached. Before trusting a green test on something
that matters:

- **Break the code and watch the test fail.** If it still passes, it is not
  testing what its name says.
- **Prove the measurement is non-zero.** A probe that never fires, or a loop
  that never executes, reports success indistinguishable from real success.
- Prefer asserting a **property** (bounded, monotonic, terminal) over a magic
  number. A number that only holds for one fixture will hold after a
  regression too.
