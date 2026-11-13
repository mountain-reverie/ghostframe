# Design: connection stats side panel (Leptos)

**Status:** design, not implemented. **Date:** 2026-11-12 · **Base:** master at aab2fa2d
**Context:** first step of moving the web client to Rust only. The wider plan
splits into five projects: (A) a Rust/Leptos build of the web client, (B) a
stats panel, (C) a server-to-client stats message, (D) porting the remaining
TypeScript (renderer, decoder, input) to Rust, and (E) replacing ghostbridge
with tailscale-rs. **This spec covers A, B and C.** D and E get their own specs.

## Goal

A side panel in the browser client that shows the state of the current
connection: versions, RTT, bandwidth, frame mode and codec, loss and repair.
It is retracted by default and opened with a `<` / `>` tab on the right edge.
It is built so that metrics added later (OpenTelemetry) appear in it without
client changes.

## Decisions taken

| Question | Decision |
|---|---|
| UI framework | Leptos, used only for DOM UI. The video path (WebTransport, WebCodecs, WebGPU, input) stays framework-free. |
| Where Leptos lives | A new library crate `ghostframe-web-ui`, linked into `ghostframe-client-wasm`. No other crate depends on Leptos, so swapping it out means rewriting one crate. |
| Metric groups in v1 | Connection, Bandwidth, Video, Loss and repair. |
| Display | Current value plus a 60-second sparkline for the metrics that move. |
| Server send policy | Only while the panel is open (client subscribes and unsubscribes). |
| Version | Crate version plus git commit and a dirty flag, for server and client. |
| Layout | Right edge. Docks in spare width, overlays otherwise. The canvas never moves. |
| `<` / `>` | The label of a clickable tab. There is no keyboard shortcut: every key belongs to the remote session. |

Rejected alternatives: panel code directly in `ghostframe-client-wasm` (Leptos
would leak into the JS-boundary crate); a second wasm module for the panel (a
second copy of the core types, and every stat crossing through JS); egui in the
canvas (draws over the remote image, fights the renderer's idle skip and the
keyboard capture).

## 1. Architecture and data flow

```
 server (ghostframe-lib)                      browser (one wasm module)
 +---------------------------+   STATS_SCHEMA  +------------------------------+
 | stats table (descriptors) | --------------> | ghostframe-client-core       |
 | sampler, 1 Hz, per session|   STATS_SAMPLE  |  stats::ClientStats          |
 |  only while subscribed    | --------------> |   server metrics + local     |
 +---------------------------+                 |   metrics, 60 s rings        |
            ^   STATS_SUBSCRIBE {on/off}       +--------------+---------------+
            +------------------------------------ open/close  | Rc<RefCell<..>>
                                               +--------------v---------------+
 TS (until project D): paints --recordPaints-->| ghostframe-web-ui (Leptos)   |
                                               |  1 Hz timer -> signals -> DOM|
                                               +------------------------------+
```

### 1.1 Wire messages

Three new message types on the existing reliable stream, defined in
`ghostframe-protocol`. There is no capability bit and no compatibility path:
the web client is embedded in the server binary, so both sides always ship
together (project rule: nothing in production, no legacy wire paths).

- **`STATS_SUBSCRIBE`** (client to server): one byte, `1` = on, `0` = off.
- **`STATS_SCHEMA`** (server to client): sent in answer to every `on`, even a
  repeated one. Contents:
  - `schema_id` (u16, incremented on each schema the session sends);
  - server version string (`0.1.0 (aab2fa2d)` or `0.1.0 (aab2fa2d, dirty)`);
  - server host name;
  - a list of descriptors, each: `id` (u16), `name` (string, OTel style,
    e.g. `ghostframe.bwe.estimate`), `unit` (UCUM string: `bit/s`, `ms`, `1`,
    `By`), `kind` (`gauge`, `counter` or `text`), `group` (`connection`,
    `bandwidth`, `video`, `loss`), and a display label.
- **`STATS_SAMPLE`** (server to client): once a second while subscribed.
  `schema_id`, a timestamp (server monotonic, microseconds), then one value per
  descriptor in schema order: `f64` for `gauge` and `counter` (counters are
  running totals), a length-prefixed string for `text`.

All lengths are bounded (descriptor count, string lengths) and checked on
decode. Every new type is registered in the protocol's single table of message
discriminators, so the collision tests cover it; no catch-all arm routes it.

### 1.2 Server

- A **descriptor table** in `ghostframe-lib` (internal, `pub(crate)`, so it does
  not reach the cbindgen header) lists the server metrics. It is the one list a
  later OpenTelemetry exporter will read, so the panel and the exporter cannot
  drift apart.
- A **per-session sampler** in `io_bridge`, run only for subscribed sessions,
  at 1 Hz. It reads counters that already exist; it adds no per-datagram or
  per-tile work. Server metrics in v1:

| Group | Metric | Source |
|---|---|---|
| connection | host name, server version | schema message |
| connection | smoothed RTT | `rtt_estimator` |
| connection | session uptime | session start time |
| bandwidth | bandwidth estimate | goog_cc estimate |
| bandwidth | rate target | `RatePolicy` (`rate_target_bps`) |
| bandwidth | sent throughput | bytes handed to quinn |
| video | frame mode (text) | classifier mode |
| video | mode reason (text) | last `mode.decision` reason |
| video | H.264 backend (text) | `libva-direct`, `openh264`, `unavailable` |
| loss | loss-path resends (counter) | `loss_path_resends` |
| loss | loss verdicts (counter) | ledger verdicts |
| loss | NACKs received (counter) | `nack_received` |

- **Subscription state** is per session and is cleared in `forget_session`.
  quinn reuses connection handles, so stale state would otherwise leak into a
  later connection.
- A malformed `STATS_SUBSCRIBE` is logged once per session and ignored.

### 1.3 Client

`ghostframe-client-core` gains a `stats` module holding `ClientStats`:

- **Server metrics:** the current schema and one 60-sample ring per numeric
  metric, filled from `STATS_SAMPLE`.
- **Local metrics,** sampled once a second whether or not the panel is open
  (the cost is a few counters), so their sparklines are full when it opens:
  - received throughput (datagram bytes);
  - wire loss before FEC, FEC recoveries, longest loss run (`WireLossStats`);
  - NACKs sent, decode errors;
  - tile codec mix (solid, PalRLE, CDF 5/3, H.264 frames), frame size;
  - painted frames per second, pushed by TypeScript (`recordPaints`) until
    project D moves the renderer into Rust.
- `ClientStats` is shared between `WasmClientCore` and the panel through an
  `Rc<RefCell<..>>` inside the wasm module. No stats cross into JavaScript.

The native client gets the same `ClientStats` for free through the core. Exposing
it through the C API is out of scope.

### 1.4 Version capture

A `build.rs` in `ghostframe-server` and in `ghostframe-client-wasm` records
`CARGO_PKG_VERSION`, the commit (`git rev-parse --short=8 HEAD`) and a dirty
flag (`git status --porcelain` is non-empty). Outside a git checkout it records
`unknown`. It reruns when `.git/HEAD` or `.git/index` changes. No build
timestamp: builds stay reproducible.

The panel compares the two. A mismatch almost always means the browser runs a
stale cached bundle, so it says so: "versions differ — hard-reload
(Ctrl+Shift+R)".

## 2. The panel

### 2.1 Components (`ghostframe-web-ui`)

- **`StatsPanel`** (root): owns the open signal and the 1 Hz timer. Opening it
  calls the core, which queues `STATS_SUBSCRIBE{on}` on its existing stream
  outbox; closing queues `{off}`. The panel never touches the transport.
- **`Tab`**: the edge button, `<` when closed, `>` when open.
- **`Group`**: one titled section per group, in the order Connection,
  Bandwidth, Video, Loss and repair.
- **`MetricRow`**: label, formatted value with unit, and a `Sparkline`.
  Rows come from the descriptor list through a keyed `<For>`, so a metric the
  server adds shows up in its group without client code. A descriptor with an
  unknown group goes into a trailing "Other" group.
- **`Sparkline`**: an inline SVG polyline over the 60-sample ring, scaled to the
  ring's own min and max. A flat series draws a midline; an empty one draws
  nothing.
- **`VersionRow`**: client and server versions, in an alert colour with the
  hard-reload hint when they differ.

Exported entry point, from `ghostframe-client-wasm`:
`mountStatsPanel(container: HTMLElement, core: WasmClientCore)`.
`main.ts` calls it once, after the core exists. `index.html` gains an empty
`<div id="stats-panel">`.

### 2.2 Layout

- The panel is `position: fixed; top: 0; right: 0; height: 100vh`, about
  320 px wide. The canvas is never moved or resized by it.
- It covers the image only when the window is narrower than the remote screen
  plus the panel. "Dock or overlay" therefore needs no detection code. The
  background is semi-opaque, which reads well in both cases.
- Closed, only the tab shows: about 20 px wide, vertically centred on the
  right edge.
- The open state is stored in `localStorage`, every access wrapped so that a
  failure just means "start closed". A page that loads with the panel open
  subscribes at once.
- Styling: one small hand-written CSS block shipped by the crate, matching the
  current dark monospace page. No CSS framework.

### 2.3 Input and focus

- Keyboard and mouse capture listen on the canvas element only; the panel is a
  sibling, so its clicks and scrolls never reach the remote desktop.
- The tab cancels `mousedown`'s default so it does not take focus: keyboard
  focus stays on the canvas and typing keeps going to the remote session.

### 2.4 Formatting and staleness

- Numbers: SI prefixes, three significant digits (`42.1 Mbit/s`, `18.4 ms`).
- `counter` metrics are shown as a per-second rate (`12 /s`), computed from
  consecutive samples; a counter that goes backwards (server restart, session
  reset) restarts the rate instead of showing a negative value.
- `text` metrics are shown as strings, with no sparkline.
- No server sample for 3 s: server rows turn grey and show "—", keeping their
  history. Local rows keep updating.
- A session reset or reconnect makes the client subscribe again (if open). A
  new `schema_id` clears the server rings, since ids may now mean different
  metrics.

## 3. Errors

- A `STATS_SAMPLE` whose value count or `schema_id` does not match the current
  schema, or that arrives before any schema, is dropped and counted
  (`stats_samples_rejected`); the rows show as stale rather than misaligned.
- Decoding errors (truncation, oversized lengths, unknown `kind`) return an
  error and are counted; they never panic.
- A panel mount failure (missing container, Leptos error) is logged to the
  console and the session carries on. A broken panel must never break the
  stream.

## 4. Testing

Every test is checked by breaking the code it guards and watching it go red.

- **Protocol:** round trips of the three messages; proptest over arbitrary
  schemas and samples; truncated buffers, count mismatches and unknown kinds
  return errors. The discriminator-collision tests cover the new types.
- **Server sampler:** nothing is sent before a subscribe; sending stops after
  an unsubscribe; a repeated subscribe re-sends the schema with a new
  `schema_id`; state is gone after `forget_session`; on a running session the
  bandwidth estimate and RTT samples are non-zero (the measurement is proven to
  fire).
- **Client stats** (plain `cargo test`): ring wraparound; counter-to-rate
  including backwards counters; staleness after 3 s; server rings cleared on a
  new `schema_id`; rejected samples counted.
- **Panel logic** (plain `cargo test`): SI formatting and units; sparkline
  scaling for normal, flat and empty series; version comparison including
  `unknown` and `dirty`.
- **Browser:** one `wasm-bindgen-test` in headless Chrome mounts the panel and
  toggles it, checking the subscribe/unsubscribe calls reach the core.
- **E2E (Chromium):** on a live session, open the panel; a subscribe goes out,
  server rows show non-zero bandwidth estimate and RTT within a few seconds,
  and after closing no further samples arrive. No Firefox variant: that slice
  already fails locally for unrelated reasons.
- **CI:** every new integration test target is added to the workflow by name
  (CI runs test targets by explicit name).

## 5. Build

- **Leptos 0.9**, pinned exactly (`=0.9.0-rc1`, or the final release if it is
  out when work starts), `csr` feature only. Starting on 0.9 avoids an
  0.8-to-0.9 migration.
- npm, vite and wasm-pack keep driving the web build until project D. The Rust
  side adds Leptos and nothing from npm.
- **Bundle size:** record wasm and JS bundle sizes before and after in the PR
  (baseline: `docs/specs/wasm-bundle-baseline.md`; today's wasm is 168 KB).
  A CI check fails the build if the wasm exceeds 320 KB.
- **New workspace member:** `ghostframe-web-ui` must also be added to the e2e
  test-server Dockerfile's per-crate manifest copies, or the image build fails
  before any test runs.
- **Gate:** `just ci-local` (fmt, env-read guard, clippy on all targets, unit
  tests, web build, tsc, release build, cbindgen header diff, release-feature
  check), run under the GPU lock.

## 6. Delivery

Three PRs, each green on its own:

1. **Protocol and server:** the three messages, the descriptor table, the
   sampler, subscription state, version capture in `ghostframe-server`. No UI;
   an e2e hook can subscribe and read samples.
2. **Client and panel:** `ClientStats` in the core, the `ghostframe-web-ui`
   crate, the client version capture, `mountStatsPanel`, the `main.ts` and
   `index.html` changes.
3. **E2E and guard:** the Chromium panel e2e and the bundle-size check.

## Out of scope

- Porting the rest of the TypeScript client (project D) and replacing
  ghostbridge (project E).
- Exposing `ClientStats` through the native client's C API.
- Charts beyond sparklines, selectable time windows, metric export.
- OpenTelemetry itself: this design only makes the descriptor table the place
  an exporter will read.
