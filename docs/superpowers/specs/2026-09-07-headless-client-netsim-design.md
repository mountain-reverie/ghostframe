# Headless Client, Netsim, and Browserless E2E — Design

**Date:** 2026-09-07
**Status:** Approved design (sub-project 2 of the client-core rearchitecture)
**Author:** Claude (design synthesis); review by Cedric

Umbrella spec: `2026-07-01-client-core-rearchitecture-design.md`. Sub-project 1
(`ghostframe-protocol` + `ghostframe-client-core`) landed on master in PR #35.

## Problem

`ghostframe-client-core` is a complete sans-IO client, but nothing drives it
over a real QUIC connection. Every end-to-end assertion still runs through a
browser, containers, a GPU, and in many cases the host's `vkms` module —
`ci/skip-list.txt` holds 38 of 66 e2e tests for that reason. There is no way to
exercise the pipeline under controlled loss, delay, or bandwidth pressure, and
therefore no signal to develop the BWE and pacing work of
`2026-06-27-protocol-redesign-design.md` against.

## Goals

- A headless client that speaks real QUIC + WebTransport to the real server, in
  one process, with no sockets.
- Deterministic, replayable impairment: loss, burst loss, reorder, duplication,
  corruption, delay, jitter, and a mutable bandwidth cap.
- Virtual time, so a 60-second scenario runs in milliseconds and repeats
  identically.
- Assertions the browser tier cannot make: no-stale-generation, goodput vs cap,
  pass starvation, corrupt-datagram fuzzing.

## Non-goals

- A windowed native client. No `wgpu`, no `winit`, no monitor enumeration.
- H.264 decode in the headless client. `NeedsH264` events are counted, not
  decoded.
- Replacing the browser e2e tier. It is retained (see "Coverage").
- Wire-format changes.
- Server-side CPU classification (deferred; see "Deferred").

## Binding constraint: everything stays inside tsnet

The protocol and the public API must never offer a path that dials a kernel
socket directly — that would let a client bypass the tailnet, which is a
security boundary, not a deployment detail.

This is enforced structurally rather than by convention: `ghostframe-client-net`
has no dial API and no socket code at all. It consumes and produces datagram
byte buffers, and the embedder supplies the pump. There are exactly three
embedders, present and future:

| Embedder | Byte pump |
|---|---|
| Browser | WebTransport (via the wasm cutover, sub-project 4) |
| Native client | ghostbridge `dial_udp` through tsnet |
| Test harness | in-process netsim (this document) |

## Decisions

| # | Decision | Rationale |
|---|---|---|
| D1 | Impair at the ghostbridge socketpair boundary, not at a UDP socket | That is exactly where tsnet sits in production, so the harness substitutes for the network without the server knowing. Honours the tsnet constraint. |
| D2 | Virtual time via `tokio::time::pause()` | Repeatable BWE experiments and millisecond-fast scenes, without restructuring `IoBridge`'s 6,244-line event loop. |
| D3 | Inject pre-encoded tiles; no CPU classification | `process_frame_cpu` emits every tile as `Codec::Raw` and never exercises the wavelet path, so a GPU-less run of the real classify path is not possible today. Injection covers the transport layer — which is what BWE needs — with an honest boundary. |
| D4 | Browser e2e tier is retained, not replaced | Two independent client implementations (TypeScript and Rust) exercising the same server is additional path validation. The browserless suite is additive coverage. |
| D5 | `ghostframe-client-net` is a real crate, not test code | A tsnet-backed native client later swaps the byte pump and reuses everything else. |

## Architecture

```
   scene script
        │  mpsc::Sender<FrameSubmission>
        ▼
 ┌──────────────────────────┐                    ┌────────────────────────┐
 │  IoBridge (real)         │   UnixStream::pair │ ghostframe-client-net  │
 │  quinn-proto server, WT  │◄──────────────────►│  quinn-proto client    │
 │  scheduler, reliable     │   ghostbridge      │  WebTransport CONNECT  │
 │  emitter, FEC, pacer     │   framing          │  ClientCore            │
 └──────────────────────────┘        ▲           └────────────────────────┘
                                     │                        │
                              netsim (impairment)             ▼
                              seeded, virtual clock  framebuffer + events
```

The netsim sits on the socketpair, in the position tsnet occupies in
production: it reads every framed datagram leaving one end, decides its fate,
and delivers it to the other.

### `ghostframe-client-net` (new workspace crate)

Sans-IO QUIC + WebTransport client session wrapping `ClientCore`:

```rust
handle_udp(&[u8], from: SocketAddr, now_us: u64) -> Vec<Event>
poll_transmit() -> Option<(Vec<u8>, SocketAddr)>
poll_timeout() -> Option<u64>
on_timeout(now_us)
```

Owns: the quinn-proto client endpoint and connection, the HTTP/3 SETTINGS +
`CONNECT` WebTransport handshake (`web-transport-proto`), datagram and
unidirectional-stream plumbing, and the `ClientCore` it feeds. Emits
`ClientCore`'s events plus connection-level ones (`Connected`,
`ConnectionLost`).

Does not own: sockets, timers, an async runtime, H.264 decode, rendering.

Most of this scaffolding is promoted out of `ghostframe-e2e/tests/loopback_h3.rs`,
which already performs this handshake against `WebTransportServer` in memory.
That test keeps working against the extracted crate.

### `ghostframe-e2e/src/netsim/`

The impairment layer. The server end is the real `IoBridge`, constructed via
`IoBridge::new_with_stream_for_test(our_end, server)`; the harness holds the
peer end of the `UnixStream::pair()` and speaks ghostbridge's own framing
through its public `encode_frame` / `parse_frame_rest` — 8-byte header
(`total_len` u32 BE, `payload_len` u32 BE) followed by
`[payload][port u16 BE][host\0]`. Reusing those functions means the harness
cannot drift from the real framing.

Impairment applies per datagram, independently per direction:

| Knob | Model |
|---|---|
| Random loss | Bernoulli(p) |
| Burst loss | Gilbert-Elliott, good/bad states with configurable transition probabilities |
| Reorder | delay a datagram by a sampled offset within a window |
| Duplication | Bernoulli(p), duplicate delivered after a sampled delay |
| Corruption | flip `n` bits at sampled offsets |
| Delay + jitter | one-way base delay plus sampled jitter |
| Bandwidth cap | token bucket, mutable mid-scene: step, ramp, oscillate |

The RNG follows the `DetRng` shape already in
`transport/reliable_emitter/sim.rs`. Every scene carries a `seed`; every test
logs it; a failure replays exactly.

### `ghostframe-e2e/src/harness/browserless.rs`

Scene driver: builds the pair, spawns `IoBridge` on the paused runtime, feeds
`FrameSubmission`s through the existing `mpsc` channel, pumps the netsim, drives
`ghostframe-client-net`, and collects the client framebuffer, event stream, and
server telemetry.

## Clock model

The harness runs on a `current_thread` runtime with `tokio::time::pause()`.
Tokio auto-advances the virtual clock to the next timer whenever the runtime
goes idle, so scenes cost their event count, not their duration.

`IoBridge` already sleeps on `sleep_until(TokioInstant::from_std(deadline))`,
but computes deadlines from `std::time::Instant::now()` in 15 places. Those
calls convert to `tokio::time::Instant::now()` so the server reads the same
virtual clock; `from_std` conversions go away with them. Nothing below
`IoBridge` needs changing — `ReliableTileEmitter::{submit_one, submit_batch,
tick, drain}`, the scheduler, and the BWE all already take `now` as a
parameter.

Netsim delivery delays are scheduled as tokio timers, so auto-advance jumps
directly to them.

## Scene API

```rust
BrowserlessScene {
    seed: u64,
    frames: Vec<FrameScript>,   // per frame: which tiles change, and the codec each encodes as
    net: NetProfile,            // impairment knobs plus a bandwidth-cap timeline
    duration: Duration,         // virtual
}
```

`FrameScript` tiles are encoded up front with `ghostframe-protocol`'s CPU
`solid` / `pal_rle` / `cdf53` codecs and submitted through the real frame path,
so the scheduler, reliable emitter, FEC, pacer, and ACK/NACK machinery all run
unmodified. Palette-table state for PalRLE scenes is supplied by the scene
script, since the GPU path normally owns it.

## Assertion classes

1. **Convergence** — every tile reaches pixel-exact lossless and all 14 CDF53
   passes land, under a given impairment profile.
2. **No stale generation** — a tile from a superseded generation is never
   surfaced as `TileReady`. The browser tier can only infer this from pixels.
3. **Goodput vs cap** — emitted bytes track the token bucket across a step-down
   and a ramp, with bounded recovery after a loss burst. This is the signal
   sub-project 3 tunes against.
4. **No pass starvation** — under sustained pressure, refinement passes keep
   making progress; no tile stalls indefinitely.
5. **Fuzz** — corrupt datagrams at the `client-net` boundary never panic and
   never surface a garbage tile.

## Coverage

The browserless suite is **additive**. Nothing is deleted from the browser tier:
the TypeScript client remains an independently written implementation of the
same protocol, and running both against one server validates a second path.

| Tier | Covers |
|---|---|
| Browserless (new) | transport under impairment, BWE/pacing, generation invariants, fuzz |
| Browser + VKMS (existing) | classification, mode switching, H.264, WebGPU render, WebTransport handshake, input forwarding, TS client behaviour |

`ci/skip-list.txt` is unchanged by this sub-project. It shrinks when the CPU
classify path lands and the GPU-gated scenarios gain browserless equivalents.

## Error handling

`client-net` inherits `client-core`'s contract: all input is hostile, and
malformed headers, truncated payloads, invalid codec or pass values, and corrupt
payloads produce typed error events, never panics. Connection-level failures
(handshake timeout, `ConnectionLost`) surface as events; the harness fails the
scene with the seed in the message. Netsim corruption tests keep this property
permanently enforced.

## Testing strategy

- **Netsim unit tests** — a fixed seed produces an identical drop/delay
  sequence; measured loss and duplication rates match their configuration over a
  large sample; the token bucket's delivered rate tracks the configured cap.
- **`client-net` integration** — handshake against the real `WebTransportServer`,
  reconnection, connection loss.
- **Fuzz** — `cargo-fuzz` target on `handle_udp`.
- **Scene tests** — the five assertion classes above.

## Risks

| Risk | Mitigation |
|---|---|
| Injected tiles diverge from real GPU emission, so a GPU-side emission bug can't be caught here | The CPU codecs are the same byte-exact oracle M3.3b validated the GPU forward pass against. Closed properly by the wave-2 CPU classify path. |
| `ghostframe-lib` pulls `ffmpeg-next` and `ash` unconditionally, so browserless tests inherit both | Prerequisite (separate PR): bump `ffmpeg-next` to 9.0.0, which is released and restores local builds against system ffmpeg 9. ffmpeg touches only `encoder/h264_vaapi.rs` and `encoder/vaapi_device.rs`, so feature-gating remains available later. |
| `tokio::time::pause()` interacts badly with any blocking or `std::thread::sleep` call inside the server loop | Audited as part of the clock conversion; a scene that fails to advance is a test failure, not a hang, because the harness bounds virtual duration. |

## Deferred

- **CPU classify + encode path** (wave 2) — makes browserless e2e cover the full
  server pipeline and lets GPU-gated scenarios migrate.
- **BWE and pacing** — sub-project 3, developed against this harness.
- **Wasm cutover** — sub-project 4.
- **Windowed native client** — needs ghostbridge `dial_udp` on the client side,
  monitor enumeration, and a renderer.
