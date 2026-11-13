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

## Licensing

Two licences, split by which side of the wire the code is on (owner
decision, 2026-11-12):

- **AGPL-3.0-or-later** — the server: `ghostframe-lib`, `ghostframe-server`,
  and every crate that links them (`ghostframe-e2e`, `ghostframe-bench`,
  `ghostframe-test-pattern`).
- **MIT OR Apache-2.0** — everything a client links, or anyone embedding a
  client: `ghostframe-protocol`, `ghostframe-tsnet` and `ghostbridge`, every
  `ghostframe-client-*`, `ghostframe-cli`, `ghostframe-web-client`,
  `shaders/`, and the repository's scripts and tools (`ci/`, `tests/`,
  `tools/`, code under `docs/`).

Why. The server is a network service. The GPL only applies when a binary is
distributed, so it would not reach someone who runs a modified server for
other people; AGPL section 13 does. The client has to stay embeddable:
Enlightenment (BSD-2) and EFL (BSD-2 and LGPL) are meant to link
`ghostframe-client-capi`. The MIT side lets code move into BSD or LGPL code,
and the Apache side adds an explicit patent grant. It is version 3, not 2,
because the server links Apache-2.0-only code (`aws-lc-sys`, `ring`,
`gethostname`, the TQUIC `delivery_rate.rs`), and Apache-2.0 is incompatible
with GPLv2.

**A new crate or file follows these rules:**

1. **Choose the side by what links it.** If any permissive crate will
   depend on it, it is MIT OR Apache-2.0. If it is server-only, or it links
   `ghostframe-lib`, it is AGPL. If unsure, choose permissive: permissive
   code can be moved under the AGPL later, but AGPL code cannot come back
   without every contributor's agreement.
2. **Never make a permissive crate depend on `ghostframe-lib`** or any other
   AGPL crate. If the client needs something the server has, move it into a
   permissive crate, as was done for `ghostframe-protocol`. Moving existing
   server code to the permissive side is a relicensing: ask the owner first.
3. **Declare `license = "..."` explicitly in the crate's `Cargo.toml`.**
   There is no workspace-level default, on purpose: every crate is a
   decision, and a missing field fails CI.
4. **Start every source file with its SPDX line:**
   `// SPDX-License-Identifier: <the crate's licence>`.
   - Shell and Python use `#`, and the line goes after any shebang.
   - In Go, leave a blank line after it, or it becomes part of the package
     doc comment.
   - Generated C headers get the line from the `header` key in
     `cbindgen.toml`.
   - Outside any crate, the licence is MIT OR Apache-2.0.
5. **Third-party code copied into the tree keeps its own licence.** It
   needs its own SPDX line, an entry in `FILE_EXCEPTIONS` in
   `ci/check-licenses.py`, an entry in `NOTICE`, and its licence text in
   `LICENSES/`.
6. **A dependency the check rejects is a decision for the owner.** Do not
   extend the allowlist in `ci/check-licenses.py` just to make CI pass. A
   copyleft or unknown licence anywhere in the client's link closure blocks
   the embedding use case. MPL-2.0 is allowed on the AGPL side only.

`ci/check-licenses.py` enforces 3 and 4, the containment in 2, and the
third-party allowlist. `just ci-local`, `just ci-client` and CI's clippy job
all run it. It only reads `cargo metadata` and `git ls-files`, so it is quick
to run alone.

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

**The web client's wasm has a size gate: 512 KiB.** `ci/check-wasm-size.sh`
reads `ghostframe-web-client/dist/assets/*.wasm` (so it needs `just build-web`
first), runs in `just ci-local` and the `web-client` CI job, and fails closed
when there is no wasm. Raise the limit only deliberately, and record the new
number in `docs/specs/wasm-bundle-baseline.md`.

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
`ghostframe::h264`): `h264_unavailable` (empty, `"this_size"` or `"process"`),
IDRs by cause (`idr_session`, `idr_client_reset`, `idr_reentry`,
`idr_mtu_shrink`, `idr_encoder_fault`, ...), verdicts on IDR and P fragments,
`frames_skipped_hold` (the manager held for an IDR) /
`frames_skipped_backpressure`, and the ledger identity
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

The `h264 refs` line (same target and cadence) is the manager's side: `plans`,
`holds`, `p_healthy` / `p_recovery`, `recoveries` / `recovered`, anchors
(`candidates_marked`, `anchors_confirmed`, `late_confirm_ignored`,
`mark_to_ack_ms_p50` / `_p95`), what the client answered (`frames_decoded_acked`,
`frames_refused`, `suspect_overridden`, `frames_dead`), and the cost of a loss:
`dead_chain_ms_max` (longest recovery episode; the longest the picture was
repaired rather than current), `reentry_bytes`, and `encode_us_p50` / `_p99`
(the encode call). Percentiles read 0 until 32 samples. Every field is listed in
`docs/design/h264-transport.md` section 8.

Every H.264 session is an **acked-reference ("refs") session** (HELLO 0x04; a
client without it is served the tile codec): the `RefManager` plans each frame
and recovers from loss from an acknowledged anchor, not an IDR. The only IDRs
are the session's, a resolution change, a client decoder failure (0x09), an MTU
shrink, a re-issue of an IDR still in flight, an encoder fault and
`backend_fallback` (`idr_session`, `idr_client_reset`, `idr_mtu_shrink`,
`idr_encoder_fault`, ... on `h264 stats`). The fallback ladder is libva-direct,
then (after its fault limit: a demotion, one `backend_fallback` IDR, no latch)
openh264, then **H.264 unavailable**, and the session stays on the lossless tile
codec. Two scopes. A **process-wide latch** when every backend tried said it is not
here (no VA-API device, no libopenh264, or `vaapi`/`openh264` alone chosen and
absent) or openh264 hit its fault limit: one warn line ("H.264 is unavailable for
the rest of this process, staying on the tile codec: ..."), `h264_unavailable="process"`
on `h264 stats`, classifier reason `h264_encoder_unavailable`. It is not re-armed
per session: **H.264 stays off until the daemon restarts**
(`systemctl --user restart ghostframe-server`). **That picture size only**
(`"this_size"`, one info line, cleared when the size changes) when any backend
refused the configuration or failed at open, whatever the other said: libva-direct
refusing a dual-4K mode with no libopenh264 installed does not shut 1080p out.

The rate target is `0.8 x` the bandwidth estimate (smoothed, floor 300 kbit/s)
and `rate_target_bps` is the last rate it was told. A stream at `0.8 x` the
estimate is not application-limited to goog_cc's ALR detector, so the bridge
declares it so (`alr_signalled`, the detector is shown a sent size halved) and
fills the probe windows goog_cc then asks for with padding when no IDR is due:
copies of an acknowledged P-frame fragment under new `wire_seq`s
(`probe_padding_windows` / `probe_padding_bytes`; the client acknowledges and
discards them). A jump of the raw estimate is held, and the estimate ceiling is
held off for a second around padding.

The rate moves in place: no reopen, no IDR, no GOP (`RatePolicy::step_in_place`).
`rate_updates`, `rate_applies_up` / `rate_applies_down` count the changes,
`rate_sends` / `rate_sends_10s` the re-sends of the rate buffers, the probe
raise's included (each costs a short dip of 1.00 / 0.56 / 0.87 times the average
frame, T4; a probe window's end dips to about 0.5 for two frames; so the policy
keeps a 250 ms floor between them, a 10 % band for a fall and 15 % for a rise,
and never re-sends inside the band). A jump of the raw estimate is held for 2 s
(`SPIKE_HOLD_IN_PLACE`), and let go early when the estimate
falls below its pre-spike level or under 0.85 of the spike while goog_cc is
loss-limited or has had a standing queue for 400 ms; the give-back is not a cut. Measure a step-up with `sus_use`
(the estimate held over the line for a second *and* the stream sending at
0.8 x the line), not `reached`, which counts a transient spike. Every frame is planned under a
`RateTarget` (`bps`, a P-frame cap of 3 average frames `max_frame_bytes`, an IDR
budget of 0.25 s of the rate with a 32 kB floor `idr_max_bytes`, and the VBV):
`frame_cap_bytes` / `idr_cap_bytes` are the last plan's, `frames_over_cap` and
`max_cap_ratio_pct` observe the (soft) cap and are never a gate. **Probe raise
(always on for acked-reference streams):** while a probe window is open (goog_cc's request or the application's own) the
P frames are planned at `RATE_SHARE x` the window's rate and tagged into the
window, so they carry part of the burst and padding tops up only the shortfall
(on the first frame of the window, sized for what the frames still to come are
expected to carry, and the whole shortfall on the last). `probe_raise_windows`,
`probe_raise_frames`, `probe_raise_bytes` on the `h264 refs` line count it;
`raised_frame_lost` (a verdict named a raised frame), `raised_frames_acked` and
`raised_delivery_us_mean` / `_max` are its cost. It
is on with no switch (no environment variable, no field): at a 600 ms buffer the
step-up uses the headroom on 70/67/71 of 96 timelines against 52/58/55 with
padding alone. The accepted cost is on links that buffer less than a window's
burst (30 and 60 ms), where it loses real frames and does somewhat worse than
padding alone: `sus_use` 26 / 23 of 48 against 30 / 31, about 2 to 4 raised
frames lost per run. Every guard is re-checked on every raised plan (a cut, a
late acknowledgement for 5 to 10 s, a raised frame or a probe pad judged lost
for 10 s, `pads_lost` on `h264 stats`); there is no wait for padding to have
been judged. The browserless harness can mutate it off
(`InjectedH264::no_probe_raise`) as a control. Measurements are in
`docs/design/h264-transport.md` section 10.

With `GHOSTFRAME_DAMAGE_MODE=shadow` or `on` the daemon also logs a cumulative
`damage safety net` line every 5 s (target `ghostframe::server::damage`);
`corrections_unswept` against `dirty_tiles` is the would-have-been miss rate
(design note section 3.4, "As landed (PR 4)").

`h264 stats` also carries `rate_target_bps` and the stall counters.
`h264 stats` names the `backend` (`libva-direct`, `openh264`, `pcm`; the
`h264 refs` line has `backend` too, and the manager's own counters) and, for a backend
with its own reference structure (openh264: the manager runs in observed mode,
reading each access unit's slice header), `idr_backend_fallback`
(an IDR the backend made unasked, or a stream moved down a step), `marks_observed`,
`mark_unrepresentable`, `ltr_recoveries_requested`, `ltr_mark_failed_sent` and
`backend_demotions` (libva-direct at its fault limit, moved to openh264 with
`auto`). A non-zero `idr_backend_fallback` outside a demotion is the library's
own fallback (an unknown recovery frame, scene-change detection left on) and
worth a look. openh264 has no per-frame size cap: a scene change shows as
`frames_over_cap` and `max_cap_ratio_pct` on `h264 stats`, absorbed by the
pacer.

`GHOSTFRAME_H264_BACKEND` is `auto|vaapi|openh264` (the removed `ffmpeg` and
`legacy`, like a misspelt value, warn and mean `auto`): `auto` tries libva-direct,
then openh264, then H.264 is unavailable (above); `vaapi` and `openh264`
try only that one; anything but `auto`/`openh264` never loads libopenh264, which
is the "user can turn it off" condition of Cisco's licence. The library is
loaded at run time and never shipped: `libopenh264.so.8` (2.6.x), `.so.7`
(2.4.x/2.5.x, e.g. Ubuntu 24.04's `libopenh264-7`), then `.so`, or the absolute path in
`GHOSTFRAME_H264_OPENH264_LIB` (optional; a configured path that fails is an
error, not a hint). An unsupported version is refused with one line naming what
was found. Pictures above 1920x1080 or `CAPTURE_FPS` above 30 are refused with one
warning and served by the next choice: libva-direct if it can, else (there is no
next encoder) the tile codec for that picture size. Patent licensing of a distribution-built
library is the operator's decision (`NOTICE`); install the library on the
session machine like any other runtime dependency. Two traps in
`OpenH264Backend::params_for`: screen-content usage silently turns long-term
references off, and scene-change detection makes IDRs nobody asked for; both
are covered by browserless scenes that go red if they are flipped. The
`openh264_*` lib tests and the `openh264_scenes` browserless scenes fail with
an install hint when no library is present (CI installs `libopenh264-7`: 2.4.1,
the 40-byte `SLayerBSInfo` layout; a developer machine on 2.6.0 has the 56-byte
one). `just test-openh264 <path>` runs them against one library (the same
variable, read through `config`; DEVELOPERS.md says how to get the other
layout). `e2e_h264_openh264_motion` forces the backend with the GPU harness and
checks a moving spinner through the browser's WebCodecs decoder; it is local
only (every H.264 e2e is in `ci/skip-list.txt`), so CI never decodes the
observed-mode stream in a browser.
`GHOSTFRAME_LOSS_OVERRIDE=1` turns the off-by-default loss override back on.
Every field and the variable are explained in
[docs/design/h264-transport.md](docs/design/h264-transport.md). A zero
`frame_acks_received` is expected (no client sends the 0x0A frame ack yet); it
is on the separate `h264 client feedback` line, target
`ghostframe_lib::transport::io_bridge`, not on `h264 stats`.

Client-side diagnostics are printed by the browser console. **Some of them are
dead**: `rx{...}` and `cdf53-coverage` read `window.__tileCounts` /
`window.__cdf53Coverage`, which nothing populates since the wasm cutover, so
they always read zero. `fec-coverage` says `n/a(wasm-internal)` explicitly.
`Δdrain` and `nack_sent` are live. Do not diagnose from the dead ones.
`window.__ghostframeStatsSamples` and `window.__ghostframeStatsFrames` (e2e
pages only) are live: the server stats samples the core applied, and every
server-stream frame it received; both move only while the stats panel is open.

The frame mode follows the bandwidth estimate: `mode.decision` reasons
`bandwidth_short` (the tile codec's first pass stayed above 0.70 x the
estimate for 500 ms) and `bandwidth_fits` (below 0.40 x for 1 s, with the
exit repaint drainable in 1 s). Until the estimate has moved since the
session reset the reason is `estimate_unknown` and the session stays in
H.264. `first_pass_demand_bps` on the same line is the demand it compared.
`GHOSTFRAME_MODE_ENTER_SHARE` and `GHOSTFRAME_MODE_EXIT_SHARE` move the two
shares (values in (0, 1]; the exit must be below the enter, or both are ignored).

## Diagnostic probes

Env-gated, off by default:

- `GHOSTFRAME_RTO_PROBE=1` — one line per loss-path retransmission (`retx`, with
  its cause: a verdict or the probe), ledger verdict (`lost_ws`, with its
  rule), acknowledgement, late acknowledgement and resolve miss. The `loss
  path` journal event (same cadence as `cumulative emit`) carries `k_pkt`,
  `reo_wnd_us` / `reo_wnd_raises` (the RACK-style departure-gap window: zero until
  a packet or time verdict was contradicted by a late acknowledgement, then a
  packet verdict also needs the newest acked departure to be that much later,
  or, inside the window, a count of learned depth; the time rule's threshold
  grows by it; back to zero after 16 quiet loss episodes), `reo_learned_packet` /
  `reo_learned_time` (contradicted verdicts it learned from, by rule) and
  `spurious_disp_max` / `spurious_gap_us_max` (how far past a contradicted
  verdict the newest acked departure was, in departures and microseconds),
  `reo_depth` (the in-window count depth, saturating at 64), `reo_judged_depth` /
  `reo_judged_quiet` (packet verdicts judged inside the window by depth, and by
  the stream going quiet), `reo_stall_skips` (late acknowledgements that taught
  nothing: no acknowledgement advanced the reference since the verdict and the
  gap was a time threshold, i.e. a stalled ack path), verdicts by rule, `spurious_*`, `pto_fires` and the retransmission causes.
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
`RunOptions { tile_fec: true, .. }`. The client decodes parity either way. (H.264
has no parity; it was removed.) A counter that reads zero (`parity`,
`client_recovered`, `late_acks_fec_possible`) on a default run is the switch, not a broken probe.

See also `reference_e2e_loss_injection` and `reference_e2e_diagnose` notes for
the e2e-side injection variables.

## Known flakes

- `encoder::h264::vaapi_direct` tests that open a real VA-API device can contend
  under parallel test load. Re-run with `--test-threads=1`.
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
