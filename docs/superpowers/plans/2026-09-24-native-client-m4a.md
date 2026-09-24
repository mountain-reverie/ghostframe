# Native client M4a: single-client sessions — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make "exactly one attached client" an enforced invariant — a second client displaces the first, which is told why — and land two deferred VA-API items.

**Architecture:** Eviction fires when a *second* client sends HELLO. The server tells the displaced client with a control datagram carrying a reason code, reusing the sentinel-tile-coordinate idiom the frame-dimensions message already established, then closes that session. Both clients surface it: the web shows an overlay, the native logs and exits 0.

**Tech Stack:** quinn-proto/WebTransport datagrams, the existing `TileHeader` framing, `ghostframe-client-core`'s reassembly router, TypeScript for the web overlay.

**Spec:** `docs/superpowers/specs/2026-09-24-native-client-m4a-design.md`

---

## Read this first

**The handle is currently thrown away.** `io_bridge.rs:5693` collects feedback with `self.wt_sessions.values_mut()` — values, not entries — so every client's feedback merges into one anonymous stream, and `dispatch_feedback_bytes(&mut self, data: &[u8])` has no idea who sent it. Task 1 exists solely to fix that, because eviction cannot work without it. Do Task 1 first and keep it behaviour-neutral.

**Mirror the frame-dimensions idiom; do not invent framing.** `ghostframe-protocol/src/protocol.rs:379-411` already establishes "sentinel tile coordinates mean this datagram is a control message". Eviction is the second instance of that pattern, not a new one.

**The negative test matters more than the positive one.** "A second client evicts the first" would be noticed the moment it broke. "A connection that never sent HELLO evicts nobody" is the one that rots silently — and it is what stands between a port scan and someone's session dying.

**Numbers in this plan that are guesses:** three repeats of the eviction datagram, and a ~16 ms grace before close. Label them as guesses where you write them. They are not measurements and should not read as if they were.

---

## File structure

| File | Responsibility | Task |
|---|---|---|
| `ghostframe-lib/src/transport/io_bridge.rs` | Thread the handle; evict on second HELLO | 1, 3 |
| `ghostframe-e2e/tests/eviction.rs` | Two clients against a live server | 8 |
| `ghostframe-protocol/src/eviction.rs` | Sentinel, reason code, build + parse | 2 |
| `ghostframe-client-core/src/reassembly.rs` | Route the sentinel to an `Event` | 4 |
| `ghostframe-client-core/src/event.rs` | `Event::Evicted` | 4 |
| `ghostframe-client-native/src/net_thread.rs` | Map to `ClientEvent::Disconnected` | 4 |
| `ghostframe-cli/src/commands.rs` | Exit 0 on displacement | 4 |
| `ghostframe-client-wasm/src/boundary.rs` | Carry `Evicted` across the wasm boundary | 5 |
| `ghostframe-web-client/src/main.ts` | Overlay, off the `Evicted` event | 5 |
| `ghostframe-web-client/tests/eviction.test.ts` | Assert on the reason code | 5 |
| `ghostframe-client-h264/src/decoder.rs` | `AV_CODEC_FLAG_LOW_DELAY` | 6 |
| `.github/workflows/e2e.yml` | Record the VA-API CI gap | 7 |

---

## Task 1: Thread the connection handle into feedback dispatch

Pure refactor. No behaviour change. It exists because eviction needs to know who sent the HELLO, and today nothing does.

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Write the failing test**

Add to `io_bridge.rs`'s test module:

```rust
#[test]
fn dispatch_feedback_bytes_knows_which_session_sent_it() {
    let (mut bridge, handle) = test_bridge_with_one_session();
    let mut buf = Vec::new();
    crate::transport::client_caps::HelloMsg {
        caps: crate::transport::client_caps::ClientCapabilities {
            indices_raw_enabled: false,
            supports_h264: true,
        },
    }
    .encode(&mut buf);

    bridge.dispatch_feedback_bytes(handle, &buf);

    assert_eq!(
        bridge.hello_sender(),
        Some(handle),
        "the bridge must record which session advertised capabilities; \
         without it, eviction cannot tell the newcomer from the incumbent"
    );
}
```

`test_bridge_with_one_session()` does not exist yet — build it from the existing test helpers in this module. Look for how other tests in `io_bridge.rs` construct a bridge with a session (search for `wt_sessions.insert` in the test module, e.g. near line 7357) and reuse that shape rather than inventing a new one.

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p ghostframe-lib --lib dispatch_feedback_bytes_knows_which_session
```

Expected: FAIL to compile — `dispatch_feedback_bytes` takes one argument, and `hello_sender` does not exist.

- [ ] **Step 3: Thread the handle through**

Change the signature at `io_bridge.rs:3220`:

```rust
    pub(crate) fn dispatch_feedback_bytes(&mut self, from: ConnectionHandle, data: &[u8]) {
```

Change the HELLO arm at `:3262` to pass it on:

```rust
                    if let Some(msg) = HelloMsg::decode(&buf[offset..]) {
                        self.apply_hello(from, msg);
                    }
```

Change `apply_hello` at `:6269`:

```rust
    pub(crate) fn apply_hello(&mut self, from: ConnectionHandle, msg: crate::transport::client_caps::HelloMsg) {
        self.hello_sender = Some(from);
```

Add the field to the struct (near `wt_sessions` at `:579`) and initialise it at both construction sites (`:1314` and `:5909`):

```rust
    /// Which session most recently advertised capabilities. `None` until the
    /// first HELLO. Task 3 uses this to tell the incumbent from a newcomer;
    /// it is also what makes `apply_hello`'s singular state (§1.1 of the
    /// design doc) attributable to a specific client rather than to whoever
    /// spoke last.
    hello_sender: Option<ConnectionHandle>,
```

```rust
            hello_sender: None,
```

Add the accessor next to the other `pub(crate)` accessors:

```rust
    pub(crate) fn hello_sender(&self) -> Option<ConnectionHandle> {
        self.hello_sender
    }
```

- [ ] **Step 4: Stop discarding the handle at the call site**

Replace `io_bridge.rs:5693-5700`:

```rust
        // Process any feedback data received on non-session bidi streams.
        // `iter_mut`, not `values_mut`: the handle identifies which client
        // sent each buffer, which eviction (and any future per-client state)
        // needs. Collecting first avoids holding a borrow of `wt_sessions`
        // across the `dispatch_feedback_bytes` calls, which take `&mut self`.
        let feedback_data: Vec<(ConnectionHandle, Vec<u8>)> = self
            .wt_sessions
            .iter_mut()
            .flat_map(|(handle, wt)| {
                let h = *handle;
                wt.drain_feedback().into_iter().map(move |d| (h, d))
            })
            .collect();
        for (handle, data) in &feedback_data {
            self.dispatch_feedback_bytes(*handle, data);
        }
```

- [ ] **Step 5: Fix the other call sites**

`io_bridge.rs:6947` and any other test caller now need a handle. Use whatever handle the surrounding test already has; if a test has no session, construct one the same way `test_bridge_with_one_session` does.

- [ ] **Step 6: Run the tests**

```bash
cargo test -p ghostframe-lib --lib
```

Expected: all pass, including the new one. **If any pre-existing test changes behaviour, stop** — this task is meant to be behaviour-neutral, and a behavioural change here means the handle was load-bearing somewhere unexpected.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "refactor(transport): attribute inbound feedback to its session

dispatch_feedback_bytes took only bytes, and the call site collected them
with values_mut(), so every client's feedback merged into one anonymous
stream and HELLO could not be attributed to a sender. Eviction needs that
attribution; so would any future per-client state.

Behaviour-neutral: the handle is recorded and otherwise unused."
```

---

## Task 2: The eviction control datagram

**Files:**
- Create: `ghostframe-protocol/src/eviction.rs`
- Modify: `ghostframe-protocol/src/lib.rs`

- [ ] **Step 1: Write the failing test**

```rust
// ghostframe-protocol/src/eviction.rs  (tests at the bottom)
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_the_reason() {
        let dg = build_eviction_datagram(EvictionReason::DisplacedByNewSession);
        assert_eq!(
            parse_eviction(&dg),
            Some(EvictionReason::DisplacedByNewSession)
        );
    }

    #[test]
    fn a_normal_tile_datagram_is_not_an_eviction() {
        // Sentinel coordinates are what route this message. An ordinary tile
        // must not be mistaken for one, or a busy screen would disconnect
        // the client.
        let inputs = crate::protocol::TileFragmentInputs {
            frame_seq: 7 | crate::protocol::TILE_DATAGRAM_FLAG,
            tile_x: 3,
            tile_y: 4,
            codec: crate::protocol::Codec::Solid,
            generation: 0,
            pass: 0,
            timestamp_us: 0,
        };
        let dg = crate::protocol::fragment_tile(&inputs, &[0u8; 4], 4)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(parse_eviction(&dg), None);
    }

    #[test]
    fn an_unknown_reason_code_parses_as_unknown_rather_than_none() {
        // A future server may add reasons this client predates. Treating an
        // unknown code as "not an eviction" would leave the client connected
        // to a server that has already dropped it; treating it as Unknown
        // still disconnects, just without a specific message.
        let mut dg = build_eviction_datagram(EvictionReason::DisplacedByNewSession);
        let last = dg.len() - 1;
        dg[last] = 0xEE;
        assert_eq!(parse_eviction(&dg), Some(EvictionReason::Unknown));
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p ghostframe-protocol eviction
```

Expected: FAIL to compile — the module does not exist.

- [ ] **Step 3: Implement it**

```rust
// ghostframe-protocol/src/eviction.rs
//! The server telling a client it has been displaced.
//!
//! Carried as a control datagram using sentinel tile coordinates, the same
//! idiom `protocol::build_frame_dimensions_datagram` established: tile
//! coordinates are `u8`, and a value this large is structurally impossible
//! at any sensible resolution, so the receiver can route on it without a
//! new datagram type or a version negotiation.
//!
//! **This is best-effort.** Datagrams are lossy and the session closes
//! shortly after, so a client may never see it and will fall back to a
//! generic disconnect. That is degraded, not broken. The design doc (§3)
//! records why a reliable stream was not used and what would change the
//! decision.

use crate::protocol::{fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG};

/// Sentinel tile coordinates marking an eviction notice. Distinct from
/// `FRAME_DIMENSIONS_SENTINEL_*` (0xFF) so the two control messages cannot
/// be confused for one another.
pub const EVICTION_SENTINEL_X: u8 = 0xFE;
pub const EVICTION_SENTINEL_Y: u8 = 0xFE;

/// Why the server closed this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EvictionReason {
    /// Another client connected. This server serves one client at a time.
    DisplacedByNewSession = 1,
    /// A reason this build does not know. Still an eviction: the session is
    /// going away regardless, and a client that ignored it would sit
    /// connected to a server that has dropped it.
    Unknown = 0xFF,
}

impl EvictionReason {
    fn from_byte(b: u8) -> Self {
        match b {
            1 => EvictionReason::DisplacedByNewSession,
            _ => EvictionReason::Unknown,
        }
    }
}

/// Build a single eviction datagram. Always fits one datagram: the payload
/// is one byte.
pub fn build_eviction_datagram(reason: EvictionReason) -> Vec<u8> {
    let payload = [reason as u8];
    let inputs = TileFragmentInputs {
        frame_seq: TILE_DATAGRAM_FLAG,
        tile_x: EVICTION_SENTINEL_X,
        tile_y: EVICTION_SENTINEL_Y,
        codec: Codec::Skip,
        generation: 0,
        pass: 0,
        timestamp_us: 0,
    };
    let datagrams = fragment_tile(&inputs, &payload, /* max_fragment_payload */ 1);
    debug_assert_eq!(datagrams.len(), 1, "an eviction notice must fit one datagram");
    datagrams.into_iter().next().unwrap()
}

/// Returns the reason if `datagram` is an eviction notice, `None` if it is
/// any other datagram.
pub fn parse_eviction(datagram: &[u8]) -> Option<EvictionReason> {
    let th = crate::protocol::TileHeader::decode(datagram)?;
    if th.tile_x != EVICTION_SENTINEL_X || th.tile_y != EVICTION_SENTINEL_Y {
        return None;
    }
    let payload = datagram.get(crate::protocol::TILE_HEADER_SIZE..)?;
    Some(EvictionReason::from_byte(*payload.first()?))
}
```

Add to `ghostframe-protocol/src/lib.rs`:

```rust
pub mod eviction;
```

**Check `TileHeader::decode`'s real signature before writing this** — it may return `Option`, `Result`, or a tuple with a consumed length. Match what exists; the shape above is the intent, not a promise about the API.

- [ ] **Step 4: Run the tests**

```bash
cargo test -p ghostframe-protocol eviction
```

Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-protocol/src/eviction.rs ghostframe-protocol/src/lib.rs
git commit -m "feat(protocol): eviction notice datagram

Sentinel tile coordinates (0xFE,0xFE), mirroring the frame-dimensions
idiom rather than adding a datagram type. An unknown reason code parses as
Unknown rather than None: the session is going away either way, and a
client that ignored it would sit connected to a server that dropped it."
```

---

## Task 3: Evict the incumbent when a second client says HELLO

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_second_hello_evicts_the_incumbent() {
    let (mut bridge, first, second) = test_bridge_with_two_sessions();
    bridge.apply_hello(first, hello(true));
    assert!(bridge.wt_sessions.contains_key(&first));

    bridge.apply_hello(second, hello(true));

    assert!(
        !bridge.wt_sessions.contains_key(&first),
        "the incumbent must be dropped when a second client identifies itself"
    );
    assert!(
        bridge.wt_sessions.contains_key(&second),
        "the newcomer must survive its own arrival"
    );
}

#[test]
fn a_connection_that_never_said_hello_evicts_nobody() {
    // THE test of this milestone. A port scan, an abandoned handshake, or a
    // client that dies mid-negotiation must not kill a working session.
    let (mut bridge, incumbent, silent) = test_bridge_with_two_sessions();
    bridge.apply_hello(incumbent, hello(true));

    // `silent` exists as a session but never sends HELLO. Drive whatever the
    // bridge does per tick; nothing here should touch `incumbent`.
    let _ = silent;

    assert!(
        bridge.wt_sessions.contains_key(&incumbent),
        "a session that never identified itself must not displace one that did"
    );
}

#[test]
fn re_hello_from_the_same_session_does_not_evict_itself() {
    // A client may re-send HELLO (reconnect within the same session, or a
    // capability change). Treating that as a newcomer would have the client
    // evict itself.
    let (mut bridge, only, _) = test_bridge_with_two_sessions();
    bridge.apply_hello(only, hello(false));
    bridge.apply_hello(only, hello(true));

    assert!(bridge.wt_sessions.contains_key(&only));
    assert!(
        bridge.current_client_caps().supports_h264,
        "the second HELLO's capabilities must still be applied"
    );
}
```

Build `test_bridge_with_two_sessions()` and `hello(supports_h264: bool)` from the existing helpers; follow the shape the module already uses.

- [ ] **Step 2: Run them to verify they fail**

```bash
cargo test -p ghostframe-lib --lib evicts
```

Expected: `a_second_hello_evicts_the_incumbent` FAILS (both sessions still present). The other two should already pass — they assert behaviour that exists — which is fine and worth noting: they are regression guards, not drivers.

- [ ] **Step 3: Implement eviction**

In `apply_hello`, before applying capabilities:

```rust
        // One client at a time. A second client identifying itself displaces
        // the incumbent, which is told why (design doc §2, §3).
        //
        // Keyed on HELLO rather than on session accept: a connection that
        // never identifies itself -- a port scan, an abandoned handshake, a
        // client that died mid-negotiation -- must not be able to kill a
        // working session.
        if let Some(incumbent) = self.hello_sender {
            if incumbent != from {
                self.evict_session(incumbent, EvictionReason::DisplacedByNewSession);
            }
        }
        self.hello_sender = Some(from);
```

And the method itself:

```rust
    /// Tell `handle` it has been displaced, then drop it.
    ///
    /// Best-effort: the notice is a datagram, so it can be lost, and the
    /// client then falls back to a generic disconnect. Sent three times
    /// because a single loss should not swallow it.
    ///
    /// **Three is a guess, not a measurement.** It survives two independent
    /// losses at the ~1% rates this project tests under. If it proves
    /// insufficient the fix is a reliable stream (design doc §3), not a
    /// larger number.
    fn evict_session(&mut self, handle: ConnectionHandle, reason: EvictionReason) {
        const EVICTION_REPEATS: usize = 3;

        let datagram = build_eviction_datagram(reason);
        if let Some(wt) = self.wt_sessions.get_mut(&handle) {
            for _ in 0..EVICTION_REPEATS {
                // Ignore send errors: the session may already be gone, which
                // is the same outcome we are driving towards.
                let _ = wt.send_datagram(&datagram);
            }
        }
        tracing::info!(?handle, ?reason, "evicting session");
        self.wt_sessions.remove(&handle);
        self.session_resets_fired.remove(&handle);
    }
```

**Check `WebTransportServer`'s real send method name** — `send_datagram` is the intent; match what exists (look at how tile datagrams are sent, around `io_bridge.rs:1596`).

**On the grace period:** the design doc calls for ~16 ms between the notice and the close. `wt_sessions.remove` drops our bookkeeping but does not itself close the QUIC connection — check what actually tears the connection down, and whether the datagrams are flushed before it does. If removal alone leaves the connection open until the peer notices, the repeats have time to arrive and no explicit sleep is needed; say so in a comment. **If it does close immediately, that is a finding** — report it rather than adding a blocking sleep on the event loop.

Also clear `hello_sender` when a session is lost, in the `Event::ConnectionLost` arm near `io_bridge.rs:5625`:

```rust
                    if self.hello_sender == Some(handle) {
                        self.hello_sender = None;
                    }
```

Without this, a client that disconnects normally leaves a stale `hello_sender`, and the *next* client to connect would evict a session that no longer exists — harmless today, but it makes the next reader distrust the invariant.

- [ ] **Step 4: Run the tests**

```bash
cargo test -p ghostframe-lib --lib
```

Expected: all pass.

- [ ] **Step 5: Prove the negative test is load-bearing**

Temporarily move the eviction call so it fires on session accept rather than on HELLO, and confirm `a_connection_that_never_said_hello_evicts_nobody` FAILS. Revert. A guard that cannot fail is not a guard.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "feat(transport): one client at a time, with a reason

A second client identifying itself displaces the incumbent, which gets an
eviction notice naming the cause before its session is dropped.

Keyed on HELLO, not on session accept: a connection that never identifies
itself must not be able to kill a working session. That guard has its own
test, and it is the one most likely to rot in a refactor -- so it was
mutation-checked by moving eviction to accept and confirming it fails."
```

---

## Task 4: Native client surfaces the reason and exits 0

**Files:**
- Modify: `ghostframe-client-core/src/event.rs`, `ghostframe-client-core/src/reassembly.rs`, `ghostframe-client-native/src/net_thread.rs`, `ghostframe-cli/src/commands.rs`

- [ ] **Step 1: Write the failing test**

```rust
// ghostframe-client-core/src/reassembly.rs  (tests at the bottom)
#[test]
fn an_eviction_datagram_becomes_an_event() {
    let mut core = test_core();
    let dg = ghostframe_protocol::eviction::build_eviction_datagram(
        ghostframe_protocol::eviction::EvictionReason::DisplacedByNewSession,
    );

    let events = core.on_datagram(&dg);

    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Evicted {
                reason: ghostframe_protocol::eviction::EvictionReason::DisplacedByNewSession
            }
        )),
        "an eviction datagram must surface as an event, not be silently \
         dropped as an unknown tile: got {events:?}"
    );
}
```

Use whatever this module's tests already use to build a core and feed it a datagram; do not invent `test_core`/`on_datagram` if differently named.

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p ghostframe-client-core eviction
```

Expected: FAIL — `Event::Evicted` does not exist.

- [ ] **Step 3: Add the event and route the sentinel**

In `ghostframe-client-core/src/event.rs`:

```rust
    /// The server has dropped this session and said why. Terminal: no
    /// further frames will arrive.
    Evicted {
        reason: ghostframe_protocol::eviction::EvictionReason,
    },
```

In `reassembly.rs`, route it at the same place the frame-dimensions sentinel is handled inline (near `:91`) — the eviction payload is one byte and always arrives as a single fragment, so it must be caught on the inline path, not only in the multi-fragment mirror at `:253`. Add alongside the existing sentinel check:

```rust
        if let Some(reason) = ghostframe_protocol::eviction::parse_eviction(datagram) {
            events.push(Event::Evicted { reason });
            return;
        }
```

- [ ] **Step 4: Map it in the native client, through a testable seam**

`net_thread.rs:365-395` maps core events to host events inline inside the
thread loop, which nothing can call from a test. Extract the mapping — both
the existing `FrameDimensions` case and the new one — into a pure function
beside it:

```rust
/// Core events that the host embedder needs to see, as `ClientEvent`s.
/// `None` for events that only concern the render thread.
///
/// Extracted from the thread loop so it can be tested directly: the loop
/// itself needs a live network client and a render thread to run at all.
pub(crate) fn host_event_for(core_ev: &ghostframe_client_core::Event) -> Option<ClientEvent> {
    use ghostframe_client_core::Event;
    match core_ev {
        Event::FrameDimensions { width, height } => Some(ClientEvent::Resized {
            width: *width,
            height: *height,
        }),
        // `ClientEvent::Disconnected` already exists and already carries a
        // reason string; eviction is a disconnect with a known cause, not a
        // new kind of host event.
        Event::Evicted { reason } => Some(ClientEvent::Disconnected {
            reason: format!("{reason:?}"),
        }),
        _ => None,
    }
}
```

**Do not leave a catch-all in the caller.** Replace the two `if let`s in the
`ClientNetEvent::Core` arm with:

```rust
                ClientNetEvent::Core(core_ev) => {
                    if let Some(host_ev) = host_event_for(&core_ev) {
                        queue.push(host_ev);
                    }
                    if render_tx.send(RenderMsg::Core(core_ev)).is_err() {
```

leaving the rest of that arm unchanged.

- [ ] **Step 5: Test the mapping**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ghostframe_client_core::Event;
    use ghostframe_protocol::eviction::EvictionReason;

    #[test]
    fn eviction_becomes_a_disconnect_naming_the_cause() {
        let ev = host_event_for(&Event::Evicted {
            reason: EvictionReason::DisplacedByNewSession,
        });
        match ev {
            Some(ClientEvent::Disconnected { reason }) => assert!(
                reason.contains("DisplacedByNewSession"),
                "the reason must name the cause so an embedder can tell \
                 displacement from a network failure, got {reason:?}"
            ),
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }

    #[test]
    fn frame_dimensions_still_maps_to_resized() {
        // Regression guard on the extraction itself.
        assert_eq!(
            host_event_for(&Event::FrameDimensions { width: 800, height: 600 }),
            Some(ClientEvent::Resized { width: 800, height: 600 })
        );
    }
}
```

`ClientEvent` needs `PartialEq` for the second assertion — add the derive if
it is missing.

- [ ] **Step 6: Exit cleanly in the CLI**

In `ghostframe-cli/src/commands.rs`'s window loop, treat `ClientEvent::Disconnected` as a clean exit:

```rust
            ClientEvent::Disconnected { reason } => {
                // Exit 0: being displaced by another client is an expected
                // outcome, not a failure. A non-zero code would make an
                // ordinary hand-off look like a crash to any supervisor or
                // script wrapping this binary.
                tracing::info!(%reason, "session ended by the server");
                println!("ghostframe: disconnected — {reason}");
                return Ok(LoopAction::Quit);
            }
```

Match the real enum and return type of the loop; `LoopAction::Quit` is the shape M2 used — check `route_window_event`'s signature rather than assuming.

- [ ] **Step 7: Run the tests**

```bash
cargo test -p ghostframe-client-core -p ghostframe-client-native -p ghostframe-cli
```

Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add ghostframe-client-core ghostframe-client-native ghostframe-cli
git commit -m "feat(client): surface eviction and exit cleanly

Event::Evicted carries the reason to the native client, which maps it onto
the ClientEvent::Disconnected that already existed. The CLI exits 0: being
displaced is expected, and a non-zero code would make an ordinary hand-off
look like a crash to a supervisor."
```

---

## Task 5: Web client overlay

**The web client no longer parses sentinels in TypeScript.** Since the wasm
cutover, `main.ts` calls `core.handleDatagram(...)` and switches on `ev.kind`
(`main.ts:543-700`); the `FRAME_DIMENSIONS_SENTINEL_*` constants in
`decoder.ts:13` are vestigial for routing. **Do not add eviction constants
there** — they would be read by nothing, which is the orphaned-diagnostics
failure this repo has already had once.

The routing work is therefore already done by Task 4's `Event::Evicted`.
What is left is to carry it across the wasm boundary and render it.

**Files:**
- Modify: `ghostframe-client-wasm/src/boundary.rs`
- Modify: `ghostframe-web-client/src/main.ts`
- Create: `ghostframe-web-client/tests/eviction.test.ts`

- [ ] **Step 1: Write the failing test**

```typescript
// ghostframe-web-client/tests/eviction.test.ts
//
// The spec asserts on the reason *code* reaching the client, not on the
// rendered wording -- the wording will be reworded, the code will not.
import { describe, it, expect } from 'vitest';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';
import { buildEvictionDatagram } from './helpers/wasm.js';

const EVICTION_REASON_DISPLACED = 1;

describe('eviction notice', () => {
  it('surfaces as an Evicted event carrying the reason code', () => {
    const core = new wasm.WasmClientCore(false, false, false, 0n);
    const events = core.handleDatagram(
      buildEvictionDatagram(EVICTION_REASON_DISPLACED),
      0n,
    ) as any[];

    const evicted = events.filter((e) => e.kind === 'Evicted');
    expect(evicted).toHaveLength(1);
    expect(evicted[0].reason).toBe(EVICTION_REASON_DISPLACED);
  });

  it('does not fire on an ordinary tile datagram', () => {
    // Sentinel coordinates are what route this. A busy screen must not
    // disconnect the client.
    const core = new wasm.WasmClientCore(false, false, false, 0n);
    const frags = fragmentTile(7, 3, 4, /*Raw*/ 4, 1, 0, new Uint8Array(8), 8);
    for (const f of frags) {
      const events = core.handleDatagram(f, 0n) as any[];
      expect(events.filter((e) => e.kind === 'Evicted')).toHaveLength(0);
    }
  });
});
```

Import `fragmentTile` alongside `buildEvictionDatagram` from
`./helpers/wasm.js`. `buildEvictionDatagram` does not exist yet — add it to
that helper module next to `fragmentTile`, exporting whatever the wasm crate
exposes. If the wasm crate exposes no builder, add a small
`#[wasm_bindgen]` test-support export mirroring `fragmentTile`'s, and follow
how `fragmentTile` is gated so it does not ship in the production bundle.

- [ ] **Step 2: Run it to verify it fails**

```bash
cd ghostframe-web-client && npm test -- eviction
```

Expected: FAIL — no `Evicted` event and no helper.

- [ ] **Step 3: Carry the event across the wasm boundary**

In `ghostframe-client-wasm/src/boundary.rs`, add to `WasmEvent` (after
`FrameDimensions`, matching the order of the core enum):

```rust
    Evicted {
        /// `EvictionReason` discriminant. A code, not a string: JS renders
        /// its own wording, and tests assert on the cause rather than on
        /// text that will be reworded.
        reason: u8,
    },
```

And to the `From<&Event>` conversion:

```rust
            Event::Evicted { reason } => WasmEvent::Evicted {
                reason: *reason as u8,
            },
```

- [ ] **Step 4: Handle it in main.ts**

Add to the `handleEvent` switch, beside `case 'FrameDimensions'`:

```typescript
      case 'Evicted': {
        // Terminal: the server has already dropped this session. Do not
        // reconnect -- two clients with retry logic evict each other
        // indefinitely (design doc §4).
        showDisconnectedOverlay(
          ev.reason === EVICTION_REASON_DISPLACED
            ? 'Another session took over this desktop.'
            : 'The server ended this session.',
        );
        break;
      }
```

with the constant declared near the other protocol constants in `main.ts`:

```typescript
/** `EvictionReason::DisplacedByNewSession` — ghostframe-protocol/src/eviction.rs. */
const EVICTION_REASON_DISPLACED = 1;
```

- [ ] **Step 5: Implement the overlay**

```typescript
/**
 * Cover the canvas with a terminal message. Deliberately has no reconnect
 * button: reloading is the reconnect, and a button invites two clients
 * racing to evict each other, which the user would experience as both
 * windows flickering.
 */
function showDisconnectedOverlay(message: string): void {
  if (document.getElementById('gf-disconnected')) return;
  const el = document.createElement('div');
  el.id = 'gf-disconnected';
  el.setAttribute('role', 'status');
  el.textContent = message;
  el.style.cssText = [
    'position:fixed', 'inset:0', 'display:flex',
    'align-items:center', 'justify-content:center',
    'background:rgba(0,0,0,0.82)', 'color:#fff',
    'font:16px system-ui,sans-serif', 'z-index:9999',
    'text-align:center', 'padding:2rem',
  ].join(';');
  document.body.appendChild(el);
}
```

- [ ] **Step 6: Run the tests and build**

```bash
cd ghostframe-web-client && npm test
cd /home/cedric/work/ghostframe && PATH="$HOME/.cargo/bin:$PATH" just build-web
```

Expected: the new tests pass, the existing suite stays green, the build is
clean. `just build-web` needs `~/.cargo/bin` on PATH for `wasm-pack`. Do not
pipe the build — a pipe reports the *last* command's status and a failed
build would look green.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-client-wasm/src/boundary.rs ghostframe-web-client/src/main.ts \
        ghostframe-web-client/tests/eviction.test.ts ghostframe-web-client/tests/helpers/wasm.ts
git commit -m "feat(web): overlay when another session takes over

Routed through the wasm boundary as an Evicted event rather than parsed
from a sentinel in TS -- since the wasm cutover main.ts switches on
ev.kind, and decoder.ts's sentinel constants are vestigial for routing.

No reconnect button by design: reloading is the reconnect, and a button
invites two clients racing to evict each other. The test asserts on the
reason code, not the wording."
```

---

## Task 6: `AV_CODEC_FLAG_LOW_DELAY`

**Files:**
- Modify: `ghostframe-client-h264/src/decoder.rs`

- [ ] **Step 1: Set the flag**

In `H264Decoder::with_device`, before `avcodec_open2`:

```rust
            // Without LOW_DELAY, a stream whose SPS carries a non-zero
            // max_num_reorder_frames makes the decoder hold every frame for
            // one frame-time before emitting it -- invisible to a test that
            // counts frames, and a full frame of added latency to someone
            // watching a remote desktop.
            //
            // The server encodes with `tune=zerolatency` and no B-frames
            // (`ghostframe-lib/src/encoder/h264_vaapi.rs:261`), so nothing is
            // given up. If a future encoder change did emit reordered
            // frames, this flag makes the decoder refuse to buffer them --
            // they would arrive out of order rather than late.
            // SAFETY: `ctx` is an allocated, not-yet-opened codec context.
            unsafe { (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32 };
```

- [ ] **Step 2: Confirm decode still works**

```bash
cargo test -p ghostframe-client-h264
```

Expected: all pass, including the hardware-vs-software oracle. That oracle is the check that matters here — if the flag changed decode output, it would fail.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-client-h264/src/decoder.rs
git commit -m "fix(h264): set AV_CODEC_FLAG_LOW_DELAY

Without it a stream whose SPS carries reorder frames holds every frame one
frame-time -- invisible to a test that counts frames, visible to a user.
The hardware-vs-software oracle confirms decode output is unchanged."
```

---

## Task 7: Record what CI cannot see

**Files:**
- Modify: `.github/workflows/e2e.yml`

- [ ] **Step 1: Write down the gap where a workflow reader will find it**

Next to the `cargo test -p ghostframe-client-h264 --lib` line, extend the comment:

```yaml
      # WHAT THIS DOES NOT COVER. The decoder, probe and oracle tests in this
      # crate gate on `vainfo` reporting a VAProfileH264High/VAEntrypointVLD
      # pair, and runners have neither libva-utils nor a VA-API device, so
      # they SKIP here. Nothing in CI fails if hardware H.264 decode
      # regresses; those tests are developer-machine-only.
      #
      # Installing a software VA-API driver so they execute against
      # *something* was considered and rejected: they would then verify a
      # different implementation than the one they exist to check, and a
      # green run asserting a software decoder's behaviour reads as coverage
      # while providing none. An honest skip is worth more.
      #
      # What DOES run here: the descriptor tests (pure logic, hand-built
      # structs) and, in client-gpu's --lib, the shader-validation walk over
      # shaders/client/*.wgsl, which needs no GPU.
      - run: cargo test -p ghostframe-client-h264 --lib
```

- [ ] **Step 2: Confirm the parts that can run, do**

```bash
cargo test -p ghostframe-client-h264 --lib -- --list | grep -c descriptor
cargo test -p ghostframe-client-gpu --lib -- --list | grep -c shader_validation
```

Expected: both non-zero. This is the claim the comment makes; verify it rather than asserting it.

- [ ] **Step 3: Validate the YAML and commit**

```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/e2e.yml'))" && echo "yaml ok"
git add .github/workflows/e2e.yml
git commit -m "ci: record that CI cannot catch a VA-API decode regression

The decoder and oracle tests gate on vainfo and skip on runners. Installing
a software VA-API driver was rejected: it would verify a different
implementation than the one under test, and read as coverage while
providing none."
```

---

## Task 8: End-to-end eviction against a live server

> **Incidental fix while you are here.** `tests/common/client_wait.rs`'s
> header says `ghostframe-client-native` "moved from a dev-dependency to a
> normal one in `Cargo.toml` so this file can use it". That move was reverted
> during M3 — it pulled `client-gpu` into the server image's build graph,
> where `shaders/` are not copied, and broke CI. The file exists precisely
> *because* the dependency stayed a dev-dependency. Correct the comment in
> this task's commit; a future reader following it would reintroduce the
> outage.

Two native clients, one server, over tsnet. Requires Docker + GPU.

**`wait_for_frame` cannot be reused here.** It drains events and keeps only
frames, discarding `Disconnected` — so this test needs its own waiter. Put it
in this test file, not in `common/client_wait.rs`: that helper exists because
two tests needed it byte-identical, and a one-caller helper in a shared module
is worse than a local function.

**Files:**
- Create: `ghostframe-e2e/tests/eviction.rs`
- Modify: `.github/workflows/e2e.yml` (extend the exemption comment)

- [ ] **Step 1: Write the test**

```rust
//! M4a acceptance: a second client displaces the first, and the first is
//! told why.
//!
//! Requires Docker AND a GPU. Deliberately NOT named in any CI workflow.
//!
//! `TS_CONTROL_URL` must be in the PROCESS environment, not set from Rust --
//! ghostbridge's Go `init()` reads it before `main`. See the long note at
//! the top of `native_client.rs`; the same trap applies here.
//!
//! ```text
//! TS_CONTROL_URL=http://127.0.0.1:18080 \
//!   cargo test -p ghostframe-e2e --test eviction -- --nocapture --test-threads=1
//! ```

use std::time::{Duration, Instant};

use ghostframe_client_native::{Client, ClientEvent, Config};
use ghostframe_e2e::harness::{read_server_logs_stripped, setup_e2e_server, E2eServerSpec};

#[path = "common/client_wait.rs"]
mod client_wait;
use client_wait::wait_for_frame;

/// Pump events until the client reports a disconnect, returning its reason.
///
/// Unlike `wait_for_frame`, this must NOT panic on `ClientEvent::Error`:
/// a session torn down underneath the client may surface an error alongside
/// the disconnect, and panicking there would hide the very event under test.
fn wait_for_disconnect(client: &mut Client, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        while let Some(ev) = client.next_event() {
            tracing::info!(?ev, "client event");
            if let ClientEvent::Disconnected { reason } = ev {
                return Some(reason);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn client_config(hostname: &str, state_dir: &std::path::Path) -> Config {
    Config {
        hostname: hostname.into(),
        authkey: String::new(), // unused: the bridge is already up
        state_dir: state_dir.to_path_buf(),
        supports_h264: false,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers: vec![],
        debug_map_frames: false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_client_displaces_the_first_with_a_reason() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    eprintln!("[phase] starting headscale + ghostframe-server containers");
    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--solid-red",
        extra_env: &[],
        gpu: false,
        webgpu: false,
        url_query_extra: "",
    })
    .await
    .expect("bring up headscale + ghostframe server");

    // Both clients share the harness's tsnet node. A second `tsnet.Server`
    // in one process does not converge a working peer datapath (see
    // `native_client.rs` for the full diagnosis) -- and there is no
    // direct-socket alternative: staying inside the tailnet is a security
    // property of this system, not a test convenience.
    let dir_a = tempfile::tempdir().expect("tempdir");
    let dir_b = tempfile::tempdir().expect("tempdir");

    eprintln!("[phase] client A connecting");
    let mut a = Client::new(client_config("evict-client-a", dir_a.path())).expect("client A");
    a.attach_bridge(setup._test_node.bridge());
    if let Err(e) = a.connect(&setup.server_container_name, 443) {
        eprintln!(
            "--- server logs ---\n{}",
            read_server_logs_stripped(&setup.server_container_name)
        );
        panic!("client A connect failed: {e}");
    }

    // Wait for a frame, not merely for connect: a frame proves A has sent
    // HELLO and is being served, which is the state eviction must displace.
    // Without this the test could race, evicting a client that had not yet
    // identified itself and passing for the wrong reason.
    let frame = wait_for_frame(&mut a, Duration::from_secs(60))
        .expect("client A received no frame within 60s");
    a.release_frame(frame.frame_id);
    eprintln!("[phase] client A is being served; connecting client B");

    let mut b = Client::new(client_config("evict-client-b", dir_b.path())).expect("client B");
    b.attach_bridge(setup._test_node.bridge());
    b.connect(&setup.server_container_name, 443).expect("client B connect");

    let reason = wait_for_disconnect(&mut a, Duration::from_secs(30));
    eprintln!("[phase] client A disconnect reason: {reason:?}");

    // Assert on the REASON, not merely on the connection dropping. A dropped
    // connection is also what a network failure looks like; the reason code
    // is the only thing that tells them apart, and it is the whole point of
    // the eviction message.
    let reason = reason.expect(
        "client A was never told it had been displaced within 30s \
         (a silent drop is indistinguishable from a network failure)",
    );
    assert!(
        reason.contains("DisplacedByNewSession"),
        "expected the displacement reason, got {reason:?}"
    );

    // And B must survive its own arrival.
    let frame_b = wait_for_frame(&mut b, Duration::from_secs(60))
        .expect("client B received no frame after displacing A");
    b.release_frame(frame_b.frame_id);

    b.disconnect().expect("disconnect B");
}
```

- [ ] **Step 2: Rebuild the container image**

```bash
just containers-build
```

`cargo test` does not rebuild it, so without this the test runs the previous
server binary and fails in a way that looks like a protocol bug. Verify by
image timestamp (`docker images ghostframe/test-server`), not by exit code —
and do not pipe the build.

- [ ] **Step 3: Run it**

```bash
TS_CONTROL_URL=http://127.0.0.1:18080 \
  cargo test -p ghostframe-e2e --test eviction -- --nocapture --test-threads=1
```

Expected: pass. If A is never disconnected, check the server log for the
`evicting session` line from Task 3 — its absence means HELLO attribution
(Task 1) is not reaching `apply_hello`, and its presence with no client-side
event means the datagram is being sent after the session is already torn
down, which is the grace-period question Task 3 flagged.

- [ ] **Step 4: Keep it out of CI, visibly**

It needs Docker and a GPU. Extend the existing exemption comment in
`e2e.yml` to name `eviction.rs` alongside `native_client.rs`, `showcase.rs`
and `h264.rs`, then confirm no workflow runs it:

```bash
grep -rn "test eviction" .github/workflows/ ; echo "exit=$? (1 = correctly absent)"
```

- [ ] **Step 5: Commit**

```bash
git add ghostframe-e2e/tests/eviction.rs .github/workflows/e2e.yml
git commit -m "test(e2e): a second client displaces the first, with the reason

Asserts on the reason reaching client A, not merely on its connection
dropping -- a silent drop is what a network failure looks like too, and
telling them apart is the entire purpose of the eviction message."
```

---

## Done means

- [ ] A second client's HELLO evicts the incumbent; a connection that never sends HELLO evicts nobody, and that guard was mutation-checked.
- [ ] The evicted native client logs the reason and exits 0, with the mapping unit-tested; the web client shows the overlay, with the test asserting on the reason code rather than the wording.
- [ ] `just ci-local` green (clippy alone is not: fmt, the env-read guard and the per-name test targets all gate), `npm test` green in `ghostframe-web-client`, and `just containers-build` succeeds.
- [ ] The e2e test passes against a live server and is named in no workflow.
- [ ] `AV_CODEC_FLAG_LOW_DELAY` set, with the decode oracle still exact.
- [ ] The VA-API CI gap is written where a workflow reader will find it.
