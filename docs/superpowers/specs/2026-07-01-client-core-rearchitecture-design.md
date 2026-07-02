# Client Core Rearchitecture: Shared Rust Client + Browserless E2E

**Date:** 2026-07-01
**Status:** Approved design (umbrella spec; sub-projects get their own plans)

## Problem

The client half of the protocol is hand-mirrored in ~5000 lines of TypeScript
(`ghostframe-web-client/src/decoder.ts`, `ack.ts`, `nack.ts`, `fec.ts`,
`parity_decoder.ts`, reassembly in `main.ts`), duplicating
`ghostframe-lib/src/transport/protocol.rs`. Drift between the two is a
recurring source of loss/corruption bugs (e.g. the cross-pass tileKey
collision). End-to-end testing requires real browsers, containers, GPU access,
and kernel modules (`ci/skip-list.txt` keeps growing), so the pipeline cannot
be exercised deterministically under injected loss, corruption, or bandwidth
pressure. A native client is planned and must not create a third protocol
implementation.

## Goals

- One Rust implementation of the client protocol, shared by browser (wasm),
  native client, and headless tests.
- Fully browserless e2e of the real pipeline with deterministic, replayable
  fault injection (loss, reorder, duplication, corruption, delay/jitter,
  bandwidth caps).
- Real bandwidth estimation and pacing, developed and validated against that
  harness.
- Web client keeps only platform-bound code: WebTransport socket I/O,
  WebCodecs H.264 decode, WebGPU rendering, input capture.

## Non-goals

- A windowed native client (the headless client is its foundation; windowing
  comes later).
- Replacing the browser rendering stack with wgpu.
- Changing the wire format (except as required by the BWE/pacing work already
  specified in `2026-06-27-protocol-redesign-design.md`).

## Architecture

### `ghostframe-client-core` (new workspace crate)

A sans-IO state machine, in the style of quinn-proto and the existing
`transport/webtransport.rs`:

```rust
handle_datagram(&[u8], now: Instant) -> Vec<Event>
poll_transmit(now: Instant) -> Option<Vec<u8>>   // ACK/NACK/feedback envelopes
poll_timeout() -> Option<Instant>                 // caller invokes on_timeout(now)
on_timeout(now: Instant)
```

Events include `TileReady { frame_seq, x, y, rgba }`, `FrameDims`,
`NeedsH264 { payload, meta }`, and diagnostics/metrics events.

Owns:
- Datagram/tile header parsing (`DatagramHeader`, `TileHeader`, sentinel).
- Reassembly keyed on `(frame_seq, tile_x, tile_y, pass)`, stale-assembly
  eviction, fragment coverage.
- FEC/XOR-parity recovery.
- ACK/NACK/feedback generation, including timestamped feedback for BWE.
- Software decode to RGBA: CDF53 (14 progressive passes), PalRLE, Solid, Raw,
  LZ4 payload decompression, generation handling (never surface
  stale-generation tiles).

Does NOT own: sockets, timers, async runtime, H.264 decode (emitted upward as
`NeedsH264`; WebCodecs in browser, VAAPI/ffmpeg for native later), rendering.
Time is always injected — tests control the clock.

### Consumers

- **Browser (wasm):** thin `wasm-bindgen` wrapper over the core. JS feeds
  datagrams from WebTransport, uploads emitted RGBA tiles to the existing
  WebGPU renderer, routes `NeedsH264` through WebCodecs, sends
  `poll_transmit` output back on the wire. All TS protocol/decode modules are
  deleted at cutover.
- **Native headless client (`ghostframe-client-headless`):** real QUIC via
  quinn plus a WebTransport *client* handshake (client-side counterpart of the
  existing sans-IO HTTP/3/WebTransport server code). Decodes into an in-memory
  framebuffer with pixel-assertion APIs. Serves as the e2e/fuzz vehicle and
  the base of the future windowed client.

### Netsim / fault injection

Impairment happens at the UDP layer on the loopback QUIC path, hooked via
quinn's `AsyncUdpSocket` on both ends. Deterministic SplitMix-seeded model:

- random and burst loss, reorder, duplication, corruption
- one-way delay and jitter
- token-bucket bandwidth cap, mutable mid-test (step down, ramp, oscillate)

Every test logs its seed; any failure replays exactly.

### Browserless e2e suite

Runs the entire real pipeline in one process group — test-pattern scenes →
tile classify/encode → reliable emitter → QUIC over impaired loopback →
headless client → decoded framebuffer. No browser, containers, GPU, or kernel
modules. Assertion classes:

- Pixel-exact convergence after loss; all 14 CDF53 passes eventually land.
- No stale-generation tile ever rendered.
- Goodput tracks the bandwidth cap; bounded recovery time after loss bursts.
- Fuzz tier: corrupt-datagram fuzzing at the core boundary (must never panic
  or emit garbage tiles); randomized-impairment soak tests.

Existing browser e2e shrinks to a smoke tier: WebTransport handshake, WebGPU
render, input forwarding. Most of `ci/skip-list.txt` leaves the critical path.

### BWE + pacing (in scope)

Implements the congestion control from
`2026-06-27-protocol-redesign-design.md` against the harness: replace the
placeholder EWMA in `transport/bwe.rs` with delay+loss-based estimation fed by
the core's timestamped feedback; add pacing in the reliable emitter and
priority-aware scheduling (passes 0–3 vs 4–13) within the estimated budget.
Acceptance is expressed as harness scenarios: converge to a step-changed cap
within bounded time, no pass starvation under sustained pressure,
blur-under-loss regression scenes.

## Sub-project sequencing

Each is its own spec/plan/implementation cycle:

1. **`ghostframe-client-core`** — the sans-IO crate. The existing vitest
   suites (tile_key, ack, nack, fec, parity_decoder, prevalidate,
   prevalidate_cdf53, cdf53_coverage, lossless_golden, palette_shadow,
   feedback, decode_error_batcher) are ported as the Rust test oracle.
2. **Headless client + netsim + browserless e2e** — QUIC/WT client transport,
   impairment layer, migration of meaningful e2e scenarios off the browser.
3. **BWE/pacing** — implemented and tuned against the harness.
4. **Wasm cutover** — wholesale swap of the TS protocol/decode layer for the
   wasm module (no incremental dual-maintenance period); browser smoke e2e is
   the acceptance gate. May run in parallel with 3 once 1–2 are done.

## Error handling

- The core treats all input as hostile: malformed headers, truncated
  payloads, invalid codec/pass values, and corrupt LZ4 streams produce typed
  error events (fed to the decode-error batcher equivalent), never panics.
- Wasm boundary: errors cross as structured events, not exceptions.
- Netsim corruption tests make this a permanently enforced property.

## Testing strategy summary

- **Unit/property:** core logic under proptest, plus the ported vitest oracle.
- **Fuzz:** cargo-fuzz target on `handle_datagram`.
- **E2E:** browserless QUIC-loopback suite with impairment (bulk of coverage).
- **Smoke:** small real-browser tier for WebTransport/WebGPU/input integration.
