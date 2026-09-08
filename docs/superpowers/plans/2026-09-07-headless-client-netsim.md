# Headless Client + Netsim + Browserless E2E Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A headless Rust client that speaks real QUIC + WebTransport to the real `IoBridge` in one process, plus a seeded impairment layer and a browserless test suite that asserts convergence, generation safety, and goodput under a bandwidth cap.

**Architecture:** One new crate, `ghostframe-client-net`, holds a sans-IO QUIC/WebTransport client session wrapping `ClientCore` — no sockets, no runtime, no dial API. The harness runs the real `IoBridge` on the peer end of a `UnixStream::pair()`, speaking ghostbridge's own framing, with a seeded netsim in between deciding each datagram's fate. Time is virtual: `tokio::time::pause()` plus a clock helper that makes `IoBridge` read tokio's clock.

**Tech Stack:** Rust 2021, quinn-proto 0.11.14, web-transport-proto 0.6.0, rustls 0.23.38, tokio 1.52 (`test-util`), proptest (dev). Design: `docs/superpowers/specs/2026-09-07-headless-client-netsim-design.md`.

---

## Global Constraints

- **No sockets anywhere in `ghostframe-client-net`.** No `std::net::UdpSocket`, no `tokio::net`, no dial API. Datagrams enter through `handle_udp` and leave through `poll_transmit`. This is the tsnet security boundary; a reviewer will check for it.
- **No `std::time::Instant::now()` in new code.** Time is injected as `u64` microseconds (client side) or read from `io_bridge::now_std()` (server side).
- **The browser e2e tier is not modified.** Nothing in `ghostframe-e2e/tests/e2e.rs` changes; `ci/skip-list.txt` is untouched.
- Every test that uses the netsim prints its seed on failure.
- Run `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` before each commit; the repo's pre-commit hook enforces both.

## File Structure

**Create:**

| Path | Responsibility |
|---|---|
| `ghostframe-client-net/Cargo.toml` | new workspace member |
| `ghostframe-client-net/src/lib.rs` | `ClientNet` — public sans-IO API, owns `ClientCore` |
| `ghostframe-client-net/src/endpoint.rs` | quinn-proto client endpoint driver (promoted from `loopback_h3.rs`) |
| `ghostframe-client-net/src/handshake.rs` | HTTP/3 SETTINGS + WebTransport CONNECT, response parsing |
| `ghostframe-client-net/src/tls.rs` | `PinnedCertVerifier` — accepts a cert iff its SHA-256 matches |
| `ghostframe-client-net/src/event.rs` | `ClientNetEvent`, `UdpOut`, error types |
| `ghostframe-e2e/src/netsim/mod.rs` | `NetSim` — per-datagram fate decisions |
| `ghostframe-e2e/src/netsim/rng.rs` | `DetRng` (SplitMix64) |
| `ghostframe-e2e/src/netsim/profile.rs` | `NetProfile`, `CapTimeline`, token bucket |
| `ghostframe-e2e/src/netsim/pump.rs` | socketpair reader/writer using ghostbridge framing |
| `ghostframe-e2e/src/harness/browserless.rs` | scene runner: bridge + netsim + client, returns results |
| `ghostframe-e2e/src/harness/scene_tiles.rs` | encodes scene tiles into `TileWork` batches |
| `ghostframe-e2e/tests/browserless.rs` | the assertion scenes |
| `ghostframe-client-net/fuzz/fuzz_targets/handle_udp.rs` | cargo-fuzz target |

**Modify:**

| Path | Change |
|---|---|
| `Cargo.toml` | add `ghostframe-client-net` to `members` |
| `ghostframe-lib/Cargo.toml` | add `browserless-harness` feature; add `tokio/test-util` to dev-deps |
| `ghostframe-lib/src/transport/io_bridge.rs` | `now_std()` helper + 15 call-site conversions; expose test constructors under the feature; tile-injection channel |
| `ghostframe-e2e/Cargo.toml` | enable `browserless-harness`; add `ghostframe-client-net` dep |
| `ghostframe-e2e/src/harness/mod.rs` | `pub mod browserless; pub mod scene_tiles;` |
| `ghostframe-e2e/src/lib.rs` | `pub mod netsim;` |
| `.github/workflows/client-core.yml` | add the two new crates to the test job |

---

# Phase A — Seams in `ghostframe-lib`

### Task 1: `browserless-harness` feature exposes the `IoBridge` constructors

`IoBridge::new_with_stream_for_test` and `new_with_frames_for_test` are `#[cfg(test)] pub(crate)`, so a separate crate cannot call them. Follow the existing `test-loss-injection` precedent (`transport/mod.rs:17`).

**Files:**
- Modify: `ghostframe-lib/Cargo.toml`
- Modify: `ghostframe-lib/src/transport/io_bridge.rs:4149`, `:4225`
- Modify: `ghostframe-e2e/Cargo.toml`
- Test: `ghostframe-e2e/tests/browserless.rs` (create)

- [ ] **Step 1: Write the failing test**

Create `ghostframe-e2e/tests/browserless.rs`:

```rust
//! Browserless e2e: real IoBridge + netsim + headless client, no browser,
//! no containers, no GPU. See
//! docs/superpowers/specs/2026-09-07-headless-client-netsim-design.md

use ghostframe_lib::transport::io_bridge::IoBridge;
use ghostframe_lib::transport::quic::QuicServer;
use tokio::net::UnixStream;

#[tokio::test]
async fn io_bridge_constructs_from_a_socketpair() {
    let (ours, _peer) = UnixStream::pair().expect("UnixStream::pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let bridge = IoBridge::new_with_stream_for_test(ours, server);
    assert_eq!(
        bridge.cert_hash_sha256().len(),
        64,
        "cert hash must be 64 hex chars"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-e2e --test browserless`
Expected: FAIL — `function `new_with_stream_for_test` is private`.

- [ ] **Step 3: Add the feature and widen the cfg**

In `ghostframe-lib/Cargo.toml`, under `[features]`:

```toml
# Exposes the socketpair constructors and the tile-injection channel outside
# of cfg(test), for the browserless e2e harness in ghostframe-e2e.
browserless-harness = []
```

In `ghostframe-lib/src/transport/io_bridge.rs`, replace both attribute pairs:

```rust
#[cfg(any(test, feature = "browserless-harness"))]
pub fn new_with_stream_for_test(stream: TokioUnixStream, server: QuicServer) -> Self {
```

```rust
#[cfg(any(test, feature = "browserless-harness"))]
pub fn new_with_frames_for_test(
    stream: TokioUnixStream,
    server: QuicServer,
    frame_rx: mpsc::Receiver<FrameSubmission>,
) -> Self {
```

In `ghostframe-e2e/Cargo.toml`, extend the existing feature list:

```toml
ghostframe-lib = { path = "../ghostframe-lib", features = ["test-loss-injection", "browserless-harness"] }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-e2e --test browserless`
Expected: PASS, 1 test.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(lib): browserless-harness feature exposes IoBridge socketpair constructors"
```

---

### Task 2: `now_std()` — make `IoBridge` read tokio's clock

Under `tokio::time::pause()` the runtime auto-advances to the next timer, but only for code that reads tokio's clock. `IoBridge` computes deadlines from `std::time::Instant::now()` in 15 places, so today it would read wall time and never advance.

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs` (15 call sites + new helper)
- Modify: `ghostframe-lib/Cargo.toml` (dev-dep feature)
- Test: `ghostframe-lib/src/transport/io_bridge.rs` (existing `mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `io_bridge.rs`:

```rust
#[tokio::test(start_paused = true)]
async fn now_std_follows_the_virtual_clock() {
    let t0 = super::now_std();
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    let t1 = super::now_std();
    assert!(
        t1.duration_since(t0) >= std::time::Duration::from_secs(5),
        "now_std must advance with the paused clock, got {:?}",
        t1.duration_since(t0)
    );
}
```

In `ghostframe-lib/Cargo.toml`, under `[dev-dependencies]`:

```toml
tokio = { workspace = true, features = ["test-util"] }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-lib --lib now_std_follows`
Expected: FAIL — `now_std` not found.

- [ ] **Step 3: Add the helper and convert the call sites**

Near the top of `io_bridge.rs`, after the imports:

```rust
/// Current time as a `std::time::Instant`, sourced from tokio's clock.
///
/// Every deadline the bridge computes flows from here, so
/// `tokio::time::pause()` controls the whole server loop: the browserless
/// harness advances virtual time and a 60-second scene costs its event count,
/// not its duration. In production tokio's clock is the system clock, so this
/// is exactly `Instant::now()`.
pub(crate) fn now_std() -> std::time::Instant {
    tokio::time::Instant::now().into_std()
}
```

Then replace all 15 `Instant::now()` calls in this file with `now_std()`. Find them with:

```bash
grep -n 'Instant::now()' ghostframe-lib/src/transport/io_bridge.rs
```

Do not touch the ones inside `mod tests`. Leave `sleep_until(TokioInstant::from_std(deadline))` as it is — `from_std` round-trips a value that `now_std` produced.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-lib --lib now_std_follows`
Expected: PASS.

Then the full lib suite, which must be unaffected:

Run: `cargo test -p ghostframe-lib --lib`
Expected: PASS, same count as before the change.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "refactor(io_bridge): source every deadline from tokio's clock via now_std()"
```

---

### Task 3: Tile-injection channel into the scheduler

The harness supplies pre-encoded tiles, because `process_frame_cpu` emits everything as `Codec::Raw`. Injecting at `Scheduler::enqueue` means the reliable emitter, FEC, pacer, fragmentation, and ACK/NACK paths all run unmodified.

The field is always present (a `None` receiver costs a pending future); only the constructor that populates it is feature-gated, so `tokio::select!` needs no `cfg` on its branches.

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs` (struct field, constructors, select arm)
- Test: `ghostframe-lib/src/transport/io_bridge.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test(start_paused = true)]
async fn injected_tile_work_reaches_the_scheduler() {
    use crate::transport::protocol::Codec;
    use crate::transport::scheduler::{TileWork, WorkState};

    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (tx, rx) = mpsc::channel(8);
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx);

    tx.send(vec![TileWork {
        tile_x: 1,
        tile_y: 2,
        generation: 0,
        pass_idx: 0,
        total_passes: 1,
        codec: Codec::Solid,
        payload: vec![10, 20, 30, 255],
        queued_at: super::now_std(),
        last_sent_at: None,
        state: WorkState::Pending,
    }])
    .await
    .expect("send injection");

    bridge.drain_injection_for_test().await;

    let queued = bridge.scheduler_peek_for_test();
    assert_eq!(queued.len(), 1, "one work item must be queued");
    assert_eq!((queued[0].tile_x, queued[0].tile_y), (1, 2));
    assert_eq!(queued[0].codec, Codec::Solid);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-lib --lib injected_tile_work`
Expected: FAIL — `new_with_injection_for_test` not found.

- [ ] **Step 3: Implement the channel**

Add the field to `struct IoBridge` (near `frame_rx`):

```rust
/// Pre-encoded tile work injected by the browserless harness, bypassing
/// capture and classification. `None` in production.
inject_rx: Option<mpsc::Receiver<Vec<crate::transport::scheduler::TileWork>>>,
```

Initialise it to `None` in every existing constructor.

Add the constructor and the two test accessors:

```rust
#[cfg(any(test, feature = "browserless-harness"))]
pub fn new_with_injection_for_test(
    stream: TokioUnixStream,
    server: QuicServer,
    inject_rx: mpsc::Receiver<Vec<crate::transport::scheduler::TileWork>>,
) -> Self {
    let mut bridge = Self::new_with_stream_for_test(stream, server);
    bridge.inject_rx = Some(inject_rx);
    bridge
}

/// Drain one batch of injected work into the scheduler. Returns the number
/// of items enqueued; 0 if the channel is empty or absent.
#[cfg(any(test, feature = "browserless-harness"))]
pub async fn drain_injection_for_test(&mut self) -> usize {
    let batch = match self.inject_rx.as_mut() {
        Some(rx) => match rx.recv().await {
            Some(b) => b,
            None => return 0,
        },
        None => return 0,
    };
    let n = batch.len();
    for work in batch {
        self.scheduler.enqueue(work);
    }
    n
}

#[cfg(any(test, feature = "browserless-harness"))]
pub fn scheduler_peek_for_test(&self) -> Vec<crate::transport::scheduler::TileWork> {
    self.scheduler.peek_for_test()
}
```

`Scheduler::peek_for_test` is `#[cfg(test)]` today (`scheduler.rs:197`); widen it to `#[cfg(any(test, feature = "browserless-harness"))]` as well.

Add the select arm in `IoBridge::run`'s `tokio::select!`, alongside the `frame_rx` arm:

```rust
batch = async {
    match self.inject_rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
} => {
    match batch {
        Some(items) => {
            for work in items {
                self.scheduler.enqueue(work);
            }
        }
        None => {
            self.inject_rx = None;
        }
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-lib --lib injected_tile_work`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(io_bridge): tile-injection channel feeding the scheduler directly"
```

---

### Task 3a: `Scheduler` takes the caller's clock

Found during Task 2's review. `Scheduler::tick(budget_bytes)` (`scheduler.rs:510`)
takes no `now` and reads `Instant::now()` internally at `scheduler.rs:190`
(`enqueue`, which also overwrites the `queued_at` the caller just set), `:547`
(`drain_priority_queue`), `:601` (`drain_refinement_pass_major`), and `:720`
(`enqueue_refinement_subset`).

Under a paused clock that leaves the retry gate at `scheduler.rs:556-559`
(`now.duration_since(last_sent_at) >= 2 * rtt`) wall-frozen, so **the
retransmit layer never fires in a virtual scenario**. Task 16's convergence
and no-starvation scenes would silently assert against a scheduler that never
retries — passing or failing for reasons unrelated to what they claim to test.

**Files:** Modify `ghostframe-lib/src/transport/scheduler.rs` (signature + 4
internal reads), `ghostframe-lib/src/transport/io_bridge.rs` (call sites).

- [ ] **Step 1: Write the failing test** in `scheduler.rs`'s `mod tests`:

```rust
#[test]
fn inflight_work_becomes_retryable_after_two_rtts_of_injected_time() {
    let mut s = Scheduler::new(4, 4);
    s.set_rtt(Duration::from_millis(50));
    let t0 = Instant::now();
    s.enqueue_at(TileWork { /* ... Pending, tile (0,0) ... */ }, t0);
    // Drain once so the item goes InFlight.
    let _ = s.tick_at(usize::MAX, t0);
    assert!(s.tick_at(usize::MAX, t0).is_empty(), "must not retry immediately");
    // 2 x RTT later it is eligible again, with no wall-clock time having passed.
    let later = t0 + Duration::from_millis(100);
    assert!(!s.tick_at(usize::MAX, later).is_empty(), "must retry after 2xRTT");
}
```

Adjust the constructor calls to the real `TileWork` shape (all fields are
public; see `scheduler.rs:37-48`).

- [ ] **Step 2: Run to verify it fails** — `cargo test -p ghostframe-lib --lib inflight_work_becomes_retryable` → FAIL, `tick_at`/`enqueue_at` not found.

- [ ] **Step 3: Thread the clock through.** Add `now: Instant` parameters —
`tick_at(&mut self, budget_bytes: usize, now: Instant)` and
`enqueue_at(&mut self, work: TileWork, now: Instant)` — and have the four
internal `Instant::now()` reads use the passed value. Keep `tick`/`enqueue` as
thin wrappers that pass `Instant::now()` so non-harness callers are unchanged,
or update all callers and delete them; state which you chose and why.
`io_bridge.rs` call sites pass `now_std()`.

- [ ] **Step 4: Run** — the new test passes; `cargo test -p ghostframe-lib --lib` unchanged otherwise.

- [ ] **Step 5: Commit** — `git commit -m "refactor(scheduler): accept the caller's clock so retries work under virtual time"`

---

### Task 3b: emitter OWD stamps follow the injected clock

`ReliableTileEmitter::wall_clock_emit_us()` (`reliable_emitter/emitter.rs:296`)
reads `SystemTime::now()` and re-stamps on every retransmit (`emitter.rs:201`).
Those stamps become `server_emit_ms_lo16` in the datagram header; the client
echoes an arrival stamp, and `io_bridge.rs:1755` computes
`owd_ms_lo16 = arrival_lo16.wrapping_sub(emit_lo16)`.

In the harness the client's arrival stamp is virtual, so that subtraction mixes
a virtual millisecond with a wall one. Task 2 made the BWE *window* virtual but
left its *inputs* wall. Also seed `BweWrapper::new`'s `window.start`
(`bwe.rs:123`) from the injected clock rather than `Instant::now()`.

**Blocks:** every OWD- or goodput-based assertion, i.e. assertion class 3 in
Task 16. Do this before writing those scenes, not after they produce numbers
nobody can interpret.

**Files:** Modify `ghostframe-lib/src/transport/reliable_emitter/emitter.rs`,
`ghostframe-lib/src/transport/bwe.rs`, callers in `io_bridge.rs`.

- [ ] **Step 1: Write the failing test** — under `#[tokio::test(start_paused = true)]`,
submit a tile pass, `tokio::time::advance(Duration::from_millis(250))`, submit
another, and assert the two emit stamps differ by ~250 ms. With `SystemTime::now()`
they differ by microseconds.

- [ ] **Step 2: Run to verify it fails.**

- [ ] **Step 3:** Take the emit stamp from the `now` already passed to
`submit_one`/`submit_batch`/`drain` rather than reading the system clock, and
seed the BWE window from the caller's clock.

- [ ] **Step 4: Run** — new test passes, `cargo test -p ghostframe-lib --lib` otherwise unchanged.

- [ ] **Step 5: Commit** — `git commit -m "fix(emitter): stamp OWD samples from the injected clock"`

---

# Phase B — `ghostframe-client-net`

### Task 4: Crate skeleton and public types

**Files:**
- Create: `ghostframe-client-net/Cargo.toml`, `src/lib.rs`, `src/event.rs`
- Modify: `Cargo.toml` (workspace members)
- Test: `ghostframe-client-net/tests/skeleton.rs`

- [ ] **Step 1: Write the failing test**

Create `ghostframe-client-net/tests/skeleton.rs`:

```rust
use ghostframe_client_net::{ClientNet, ClientNetConfig};

#[test]
fn new_client_has_nothing_to_transmit_before_connect() {
    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256: [0u8; 32],
        indices_raw_enabled: true,
        supports_h264: false,
    };
    let mut client = ClientNet::new(cfg, 0).expect("ClientNet::new");
    assert!(client.poll_transmit().is_none());
    assert!(!client.is_connected());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-client-net --test skeleton`
Expected: FAIL — crate does not exist.

- [ ] **Step 3: Create the crate**

`ghostframe-client-net/Cargo.toml`:

```toml
[package]
name = "ghostframe-client-net"
version = "0.1.0"
edition = "2021"

[dependencies]
ghostframe-protocol = { path = "../ghostframe-protocol" }
ghostframe-client-core = { path = "../ghostframe-client-core" }
quinn-proto = { workspace = true }
rustls = { workspace = true }
web-transport-proto = { workspace = true }
bytes = { workspace = true }
sha2 = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
url = "2"

[dev-dependencies]
ghostframe-lib = { path = "../ghostframe-lib" }
rcgen = { workspace = true }
proptest = "1"
```

Add `"ghostframe-client-net",` to `members` in the workspace `Cargo.toml`, after `"ghostframe-client-core",`.

`ghostframe-client-net/src/event.rs`:

```rust
use ghostframe_client_core::Event as CoreEvent;
use std::net::SocketAddr;

/// A datagram to hand to the embedder's byte pump (tsnet in production, the
/// netsim in tests). This crate never owns a socket.
#[derive(Debug, Clone, PartialEq)]
pub struct UdpOut {
    pub payload: Vec<u8>,
    pub destination: SocketAddr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientNetEvent {
    /// QUIC handshake finished.
    Connected,
    /// WebTransport CONNECT accepted; datagrams may now flow.
    SessionReady,
    ConnectionLost { reason: String },
    /// Anything the client core surfaced.
    Core(CoreEvent),
}

#[derive(Debug, thiserror::Error)]
pub enum ClientNetError {
    #[error("TLS configuration failed: {0}")]
    Tls(String),
    #[error("connect failed: {0}")]
    Connect(String),
}
```

`ghostframe-client-net/src/lib.rs`:

```rust
//! Sans-IO QUIC + WebTransport client session wrapping `ClientCore`.
//!
//! There is deliberately no dial API and no socket code in this crate: the
//! embedder supplies the byte pump. In production that pump is ghostbridge's
//! `dial_udp` through tsnet, so a client can never escape the tailnet; in
//! tests it is the netsim.

mod event;

pub use event::{ClientNetError, ClientNetEvent, UdpOut};

#[derive(Debug, Clone)]
pub struct ClientNetConfig {
    /// SNI name presented in the TLS handshake.
    pub server_name: String,
    /// The only certificate this client will accept, by SHA-256 of its DER.
    pub server_cert_sha256: [u8; 32],
    pub indices_raw_enabled: bool,
    pub supports_h264: bool,
}

pub struct ClientNet {
    config: ClientNetConfig,
    connected: bool,
}

impl ClientNet {
    pub fn new(config: ClientNetConfig, _now_us: u64) -> Result<Self, ClientNetError> {
        Ok(Self {
            config,
            connected: false,
        })
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn poll_transmit(&mut self) -> Option<UdpOut> {
        None
    }

    pub fn server_name(&self) -> &str {
        &self.config.server_name
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-client-net --test skeleton`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: ghostframe-client-net skeleton with sans-IO API types"
```

---

### Task 5: Pinned-certificate verifier

The browser pins the server cert by SHA-256 (`serverCertificateHashes`). The headless client does the same, so no "accept any cert" verifier ever ships in this crate.

**Files:**
- Create: `ghostframe-client-net/src/tls.rs`
- Test: `ghostframe-client-net/tests/tls_pinning.rs`

- [ ] **Step 1: Write the failing test**

```rust
use ghostframe_client_net::tls::PinnedCertVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};

fn self_signed() -> Vec<u8> {
    let params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    params.self_signed(&key).unwrap().der().to_vec()
}

#[test]
fn accepts_the_pinned_cert_and_rejects_others() {
    let der = self_signed();
    let mut hasher = Sha256::new();
    hasher.update(&der);
    let hash: [u8; 32] = hasher.finalize().into();

    let verifier = PinnedCertVerifier::new(hash);
    let cert = CertificateDer::from(der.clone());
    let name = ServerName::try_from("localhost").unwrap();
    assert!(verifier
        .verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
        .is_ok());

    let other = CertificateDer::from(self_signed());
    assert!(verifier
        .verify_server_cert(&other, &[], &name, &[], UnixTime::now())
        .is_err());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-client-net --test tls_pinning`
Expected: FAIL — module `tls` not found.

- [ ] **Step 3: Implement the verifier**

`ghostframe-client-net/src/tls.rs`:

```rust
//! Certificate pinning, mirroring the browser's `serverCertificateHashes`.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct PinnedCertVerifier {
    expected: [u8; 32],
    provider: rustls::crypto::CryptoProvider,
}

impl PinnedCertVerifier {
    pub fn new(expected: [u8; 32]) -> Self {
        Self {
            expected,
            provider: rustls::crypto::ring::default_provider(),
        }
    }
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let mut hasher = Sha256::new();
        hasher.update(end_entity.as_ref());
        let actual: [u8; 32] = hasher.finalize().into();
        if actual == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General("server certificate hash mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
```

Add `pub mod tls;` to `lib.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-client-net --test tls_pinning`
Expected: PASS, 1 test.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(client-net): SHA-256 pinned certificate verifier"
```

---

### Task 6: quinn-proto client endpoint driver

Promoted from `ghostframe-e2e/tests/loopback_h3.rs:78-235` (`TestEndpoint`, `split_transmit`), narrowed to the client role: one connection, no `accept`.

**Files:**
- Create: `ghostframe-client-net/src/endpoint.rs`
- Test: `ghostframe-client-net/tests/handshake.rs`

- [ ] **Step 1: Write the failing test**

```rust
use ghostframe_client_net::{ClientNet, ClientNetConfig, ClientNetEvent};
use ghostframe_lib::transport::quic::QuicServer;
use std::net::{Ipv6Addr, SocketAddr};

/// The server's cert hash, as the client pins it.
fn pinned_hash(server: &QuicServer) -> [u8; 32] {
    let mut hash = [0u8; 32];
    hex::decode_to_slice(&server.cert_info().sha256_hex, &mut hash).expect("cert hash hex");
    hash
}

/// Shuttle datagrams between the client and a real `QuicServer` until the
/// handshake settles or `max_steps` is exhausted.
///
/// `QuicServer`'s public surface is already what the I/O bridge drives
/// (`handle_datagram`, `poll_transmit`, `next_timeout`, `handle_timeout`,
/// `drain_endpoint_events`), so no test-only helpers are needed.
fn pump(
    client: &mut ClientNet,
    server: &mut QuicServer,
    base: Instant,
    now_us: &mut u64,
    max_steps: usize,
) {
    let server_addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443);
    let client_addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 5000);
    let mut buf = Vec::with_capacity(2048);

    for _ in 0..max_steps {
        let now = base + Duration::from_micros(*now_us);
        let mut moved = false;

        while let Some(out) = client.poll_transmit() {
            let resp = server.handle_datagram(
                now,
                client_addr,
                None,
                None,
                BytesMut::from(&out.payload[..]),
                &mut buf,
            );
            if let Some(t) = resp {
                client.handle_udp(&buf[..t.size], server_addr, *now_us);
            }
            buf.clear();
            moved = true;
        }

        server.drain_endpoint_events();
        while let Some(t) = server.poll_transmit(now, 10, &mut buf) {
            client.handle_udp(&buf[..t.size], server_addr, *now_us);
            buf.clear();
            moved = true;
        }

        if server.next_timeout().is_some_and(|d| d <= now) {
            server.handle_timeout(now);
            moved = true;
        }

        *now_us += 1_000;
        if !moved {
            break;
        }
    }
}

#[test]
fn quic_handshake_completes_against_the_real_server() {
    let mut server = QuicServer::new().expect("QuicServer::new");
    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256: pinned_hash(&server),
        indices_raw_enabled: true,
        supports_h264: false,
    };
    let base = Instant::now();
    let mut now_us = 0u64;
    let mut client = ClientNet::new(cfg, now_us).expect("ClientNet::new");
    client
        .connect(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443), now_us)
        .expect("connect");

    pump(&mut client, &mut server, base, &mut now_us, 64);

    assert!(
        client.take_events().contains(&ClientNetEvent::Connected),
        "client must report Connected"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-client-net --test handshake`
Expected: FAIL — `connect`, `handle_udp`, and `take_events` are not defined on `ClientNet`.

- [ ] **Step 3: Implement the driver**

`ghostframe-client-net/src/endpoint.rs` holds a `ClientEndpoint` with the same shape as `TestEndpoint` in `loopback_h3.rs` minus `accept`:

```rust
use bytes::{Bytes, BytesMut};
use quinn_proto::{
    ClientConfig, Connection, ConnectionEvent, ConnectionHandle, DatagramEvent, Endpoint,
    EndpointConfig, Transmit,
};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Instant;

pub(crate) struct ClientEndpoint {
    pub(crate) endpoint: Endpoint,
    pub(crate) conn: Option<(ConnectionHandle, Connection)>,
    pub(crate) outbound: VecDeque<(Transmit, Bytes)>,
    pub(crate) timeout: Option<Instant>,
}

impl ClientEndpoint {
    pub(crate) fn new() -> Self {
        Self {
            endpoint: Endpoint::new(Arc::new(EndpointConfig::default()), None, false, None),
            conn: None,
            outbound: VecDeque::new(),
            timeout: None,
        }
    }

    pub(crate) fn connect(
        &mut self,
        now: Instant,
        cfg: ClientConfig,
        remote: SocketAddr,
        server_name: &str,
    ) -> Result<(), quinn_proto::ConnectError> {
        let (ch, conn) = self.endpoint.connect(now, cfg, remote, server_name)?;
        self.conn = Some((ch, conn));
        Ok(())
    }

    /// Feed one inbound datagram.
    pub(crate) fn handle_udp(&mut self, now: Instant, from: SocketAddr, packet: BytesMut) {
        let buf_size = self.endpoint.config().get_max_udp_payload_size() as usize;
        let mut buf = Vec::with_capacity(buf_size);
        match self.endpoint.handle(now, from, None, None, packet, &mut buf) {
            Some(DatagramEvent::ConnectionEvent(ch, event)) => {
                if let Some((h, conn)) = self.conn.as_mut() {
                    if *h == ch {
                        conn.handle_event(event);
                    }
                }
            }
            Some(DatagramEvent::Response(transmit)) => {
                let size = transmit.size;
                self.outbound.extend(split_transmit(transmit, &buf[..size]));
            }
            _ => {}
        }
    }

    /// Drain queued connection events and transmits. Narrowed from
    /// `TestEndpoint::drive_outgoing` (`loopback_h3.rs:130-175`): there is
    /// exactly one connection, so the outer `for (ch, conn)` loop collapses.
    pub(crate) fn drive(&mut self, now: Instant) {
        let buf_size = self.endpoint.config().get_max_udp_payload_size() as usize;
        let mut buf = Vec::with_capacity(buf_size);

        loop {
            let (ch, conn) = match self.conn.as_mut() {
                Some(c) => (c.0, &mut c.1),
                None => return,
            };

            if self.timeout.is_some_and(|t| t <= now) {
                self.timeout = None;
                conn.handle_timeout(now);
            }

            let mut endpoint_events = Vec::new();
            while let Some(event) = conn.poll_endpoint_events() {
                endpoint_events.push(event);
            }

            while let Some(transmit) = conn.poll_transmit(now, 10, &mut buf) {
                let size = transmit.size;
                self.outbound.extend(split_transmit(transmit, &buf[..size]));
                buf.clear();
            }

            self.timeout = conn.poll_timeout();

            if endpoint_events.is_empty() {
                return;
            }
            for event in endpoint_events {
                if let Some(conn_event) = self.endpoint.handle_event(ch, event) {
                    if let Some((_, conn)) = self.conn.as_mut() {
                        conn.handle_event(conn_event);
                    }
                }
            }
        }
    }
}
```

Copy `split_transmit` verbatim from `loopback_h3.rs:205-235`.

Time conversion: `ClientNet` stores a `base: Instant` captured in `new()` and converts injected microseconds with `base + Duration::from_micros(now_us)`, so the crate stays driven by injected time while quinn-proto gets the `Instant` it requires.

Wire `ClientNet::connect`, `handle_udp`, `poll_transmit`, and `take_events` on top, building the `ClientConfig` from `PinnedCertVerifier` with `alpn_protocols = vec![b"h3".to_vec()]` and datagram buffers of 65536, matching `loopback_h3.rs:355-372`.

`handle_udp` returns `()`: events accumulate in an internal `Vec<ClientNetEvent>` that `take_events()` drains. Keeping one drain point means callers can't miss events emitted by a timer rather than a datagram.

No changes to `ghostframe-lib/src/transport/quic.rs` are needed — `QuicServer` already exposes `handle_datagram`, `poll_transmit`, `next_timeout`, `handle_timeout`, `drain_endpoint_events`, and a public `connections` map, which is the whole surface the pump and later the harness require.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-client-net --test handshake`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(client-net): quinn-proto client endpoint driver + QUIC handshake"
```

---

### Task 7: WebTransport CONNECT

**Files:**
- Create: `ghostframe-client-net/src/handshake.rs`
- Test: `ghostframe-client-net/tests/handshake.rs` (extend)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn webtransport_session_becomes_ready() {
    let mut server = QuicServer::new().expect("QuicServer::new");
    let mut wt = ghostframe_lib::transport::webtransport::WebTransportServer::new();
    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256: pinned_hash(&server),
        indices_raw_enabled: true,
        supports_h264: false,
    };
    let base = Instant::now();
    let mut now_us = 0u64;
    let mut client = ClientNet::new(cfg, now_us).expect("ClientNet::new");
    client
        .connect(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443), now_us)
        .expect("connect");

    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 128);

    assert!(wt.is_connected(), "server must accept the CONNECT");
    assert!(
        client.take_events().contains(&ClientNetEvent::SessionReady),
        "client must report SessionReady"
    );
}
```

`pump_with_wt` takes the same arguments as `pump` plus `&mut WebTransportServer`, and extends it by calling `wt.on_new_connection`, `wt.on_stream_opened`, and `wt.on_stream_readable` on server-side events exactly as `loopback_h3.rs:475-495` does.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-client-net --test handshake webtransport_session`
Expected: FAIL — client never opens the streams.

- [ ] **Step 3: Implement the handshake**

`handshake.rs` drives a three-state machine once QUIC reports `Connected`:

1. Open a uni stream, write `Settings` with `enable_webtransport(1)` (`loopback_h3.rs:433-444`).
2. Open a bidi stream, write `ConnectRequest::new(Url::parse("https://<server_name>/.well-known/webtransport")?)`, remember its `StreamId` as the session stream.
3. Read that bidi stream until `web_transport_proto::ConnectResponse::decode` yields a status; `200` emits `SessionReady`, anything else emits `ConnectionLost`.

The session stream id is required by Task 8 for the quarter-id prefix, so store it as `session_stream: Option<StreamId>`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-client-net --test handshake`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(client-net): HTTP/3 SETTINGS + WebTransport CONNECT handshake"
```

---

### Task 8: Datagram path into `ClientCore`

WebTransport datagrams carry a quarter-stream-id VarInt prefix (RFC 9297); the server strips it in `WebTransportServer::recv_datagram` and prepends it in `send_datagram`. The client must mirror both.

**Files:**
- Modify: `ghostframe-client-net/src/lib.rs`
- Test: `ghostframe-client-net/tests/datagram.rs`

- [ ] **Step 1: Write the failing test**

```rust
/// A Solid tile the server sends must surface as a decoded RGBA tile.
#[test]
fn solid_tile_datagram_becomes_tile_ready() {
    let (mut client, mut server, mut wt, mut now_us) = connected_session();

    // frame_seq 1, tile (0,0), Codec::Solid, BGRA (10,20,30,255)
    let dg = ghostframe_protocol::protocol::build_tile_datagram_for_test(
        1,
        0,
        0,
        ghostframe_protocol::protocol::Codec::Solid,
        &[10, 20, 30, 255],
    );
    {
        let conn = server.connections.values_mut().next().unwrap();
        wt.send_datagram(conn, &dg).expect("send_datagram");
    }
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);

    let tiles: Vec<_> = client
        .take_events()
        .into_iter()
        .filter_map(|e| match e {
            ClientNetEvent::Core(ghostframe_client_core::Event::TileReady {
                tile_x, tile_y, rgba, ..
            }) => Some((tile_x, tile_y, rgba)),
            _ => None,
        })
        .collect();

    assert_eq!(tiles.len(), 1, "exactly one tile must decode");
    assert_eq!(&tiles[0].2[0..4], &[30, 20, 10, 255], "BGRA -> RGBA swizzle");
}
```

If `build_tile_datagram_for_test` does not exist in `ghostframe-protocol`, add it there next to the existing header encoders, gated `#[cfg(any(test, feature = "browserless-harness"))]`, building `DatagramHeader` + `TileHeader` + payload for a single-fragment tile.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-client-net --test datagram`
Expected: FAIL — no datagrams reach the core.

- [ ] **Step 3: Implement**

On receive: `conn.datagrams().recv()` → decode and discard the leading `VarInt` → `ClientCore::handle_datagram(payload, now_us)` → wrap each returned `Event` as `ClientNetEvent::Core`.

On send: for each `PollOutput::Datagram(bytes)` from `ClientCore::poll_transmit`, prepend `VarInt::from_u64(u64::from(session_stream) / 4)` and call `conn.datagrams().send(Bytes::from(buf), false)`. `false` matches the server's non-dropping behaviour, so a full send buffer surfaces as `Blocked` instead of silently discarding.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-client-net --test datagram`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(client-net): WebTransport datagram path wired into ClientCore"
```

---

### Task 9: Feedback stream and timers

`ClientCore` emits `PollOutput::Stream` for Hello, `ReceiverFeedback`, and decode errors. The server collects those from non-session bidi streams via `WebTransportServer::drain_feedback`.

**Files:**
- Modify: `ghostframe-client-net/src/lib.rs`
- Test: `ghostframe-client-net/tests/feedback.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn hello_reaches_the_server_on_the_feedback_stream() {
    let (mut client, mut server, mut wt, mut now_us) = connected_session();
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);

    let feedback = wt.drain_feedback();
    assert!(
        feedback.iter().any(|m| m.first() == Some(&0x03)),
        "server must receive the Hello message (type 0x03), got {feedback:?}"
    );
}

#[test]
fn poll_timeout_fires_the_core_timers() {
    let (mut client, mut server, mut wt, mut now_us) = connected_session();
    let deadline = client.poll_timeout().expect("core always arms a timeout");
    assert!(deadline > now_us);

    now_us = deadline;
    client.on_timeout(now_us);
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 8);

    assert!(
        !wt.drain_feedback().is_empty(),
        "a fired feedback timer must produce a stream message"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-client-net --test feedback`
Expected: FAIL — no feedback stream is opened.

- [ ] **Step 3: Implement**

Open one additional bidi stream after `SessionReady` and write every `PollOutput::Stream` payload to it in order. Expose:

```rust
/// Earliest deadline (µs) at which `on_timeout` must be called: the min of
/// the core's deadline and the QUIC connection's own timer.
pub fn poll_timeout(&self) -> Option<u64>;
pub fn on_timeout(&mut self, now_us: u64);
```

`on_timeout` calls `Connection::handle_timeout` when the QUIC timer is due and `ClientCore::on_timeout` when the core's is, then drains both outboxes.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-client-net --test feedback`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(client-net): feedback stream + timer plumbing"
```

---

# Phase C — Netsim

### Task 10: Deterministic RNG and profile types

**Files:**
- Create: `ghostframe-e2e/src/netsim/rng.rs`, `ghostframe-e2e/src/netsim/profile.rs`, `ghostframe-e2e/src/netsim/mod.rs`
- Modify: `ghostframe-e2e/src/lib.rs`
- Test: `ghostframe-e2e/tests/netsim.rs`

- [ ] **Step 1: Write the failing test**

```rust
use ghostframe_e2e::netsim::{DetRng, NetProfile, NetSim, Verdict};

#[test]
fn identical_seeds_produce_identical_streams() {
    let mut a = DetRng::new(0xDEAD_BEEF);
    let mut b = DetRng::new(0xDEAD_BEEF);
    let xs: Vec<u64> = (0..64).map(|_| a.next_u64()).collect();
    let ys: Vec<u64> = (0..64).map(|_| b.next_u64()).collect();
    assert_eq!(xs, ys);

    let mut c = DetRng::new(0xDEAD_BEEE);
    let zs: Vec<u64> = (0..64).map(|_| c.next_u64()).collect();
    assert_ne!(xs, zs, "different seeds must diverge");
}

#[test]
fn measured_loss_rate_matches_the_configuration() {
    let profile = NetProfile {
        loss: 0.10,
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(profile, 42);
    let n = 100_000;
    let dropped = (0..n)
        .filter(|_| matches!(sim.decide(64, 0), Verdict::Drop))
        .count();
    let rate = dropped as f64 / n as f64;
    assert!(
        (rate - 0.10).abs() < 0.005,
        "measured loss {rate:.4} must be within 0.5% of 0.10"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-e2e --test netsim`
Expected: FAIL — module `netsim` not found.

- [ ] **Step 3: Implement**

`rng.rs` — SplitMix64:

```rust
pub struct DetRng(u64);

impl DetRng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn bernoulli(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }
}
```

`profile.rs`:

```rust
#[derive(Debug, Clone)]
pub struct NetProfile {
    /// Independent per-datagram drop probability.
    pub loss: f64,
    /// Gilbert-Elliott: probability of entering the bad state, and of leaving it.
    pub burst_enter: f64,
    pub burst_exit: f64,
    /// Drop probability while in the bad state.
    pub burst_loss: f64,
    pub duplicate: f64,
    pub corrupt: f64,
    /// One-way delay and its jitter, both in microseconds.
    pub delay_us: u64,
    pub jitter_us: u64,
    /// Reorder window in microseconds; 0 disables reordering.
    pub reorder_us: u64,
    /// Bandwidth cap timeline; empty means unlimited.
    pub cap: CapTimeline,
}

impl NetProfile {
    /// A lossless, delay-free, uncapped link.
    pub fn perfect() -> Self {
        Self {
            loss: 0.0,
            burst_enter: 0.0,
            burst_exit: 1.0,
            burst_loss: 0.0,
            duplicate: 0.0,
            corrupt: 0.0,
            delay_us: 0,
            jitter_us: 0,
            reorder_us: 0,
            cap: CapTimeline::unlimited(),
        }
    }
}

/// Piecewise bandwidth cap: each entry takes effect at `at_us`.
#[derive(Debug, Clone, Default)]
pub struct CapTimeline {
    pub points: Vec<(u64, u64)>, // (at_us, bytes_per_second)
}

impl CapTimeline {
    pub fn unlimited() -> Self { Self { points: Vec::new() } }
    pub fn constant(bps: u64) -> Self { Self { points: vec![(0, bps)] } }
    pub fn step(first_bps: u64, at_us: u64, then_bps: u64) -> Self {
        Self { points: vec![(0, first_bps), (at_us, then_bps)] }
    }
    /// Cap in effect at `now_us`; `u64::MAX` when unlimited.
    pub fn bps_at(&self, now_us: u64) -> u64 {
        self.points
            .iter()
            .rev()
            .find(|(at_us, _)| *at_us <= now_us)
            .map(|(_, bps)| *bps)
            .unwrap_or(u64::MAX)
    }
}
```

`mod.rs` holds `Verdict` and `NetSim`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Deliver at `at_us`.
    Deliver { at_us: u64 },
    /// Deliver at `at_us`, and again at `dup_at_us`.
    Duplicate { at_us: u64, dup_at_us: u64 },
    /// Deliver at `at_us` with bit `bit_index` flipped.
    Corrupt { at_us: u64, bit_index: usize },
    Drop,
}

pub struct NetSim {
    profile: NetProfile,
    rng: DetRng,
    in_burst: bool,
    tokens: f64,
    last_refill_us: u64,
    pub seed: u64,
}

impl NetSim {
    pub fn new(profile: NetProfile, seed: u64) -> Self {
        Self {
            profile,
            rng: DetRng::new(seed),
            in_burst: false,
            tokens: 0.0,
            last_refill_us: 0,
            seed,
        }
    }

    /// Decide the fate of one datagram of `len` bytes offered at `now_us`.
    ///
    /// Tasks 11 and 12 extend this with bursts, jitter, reorder, duplication,
    /// corruption, and the token bucket; Task 10 implements the loss roll and
    /// the immediate-delivery path only.
    pub fn decide(&mut self, _len: usize, now_us: u64) -> Verdict {
        if self.rng.bernoulli(self.profile.loss) {
            return Verdict::Drop;
        }
        Verdict::Deliver { at_us: now_us }
    }
}
```

Add `pub mod netsim;` to `ghostframe-e2e/src/lib.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-e2e --test netsim`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(netsim): SplitMix64 RNG, profile types, per-datagram verdicts"
```

---

### Task 11: Burst loss, duplication, corruption, delay and jitter

**Files:**
- Modify: `ghostframe-e2e/src/netsim/mod.rs`
- Test: `ghostframe-e2e/tests/netsim.rs` (extend)

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn burst_loss_clusters_drops() {
    let profile = NetProfile {
        burst_enter: 0.01,
        burst_exit: 0.20,
        burst_loss: 0.90,
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(profile, 7);
    let drops: Vec<bool> = (0..20_000)
        .map(|_| matches!(sim.decide(64, 0), Verdict::Drop))
        .collect();

    // A clustered process has a much higher P(drop | previous dropped) than
    // its unconditional drop rate.
    let total = drops.iter().filter(|d| **d).count() as f64;
    let pairs = drops.windows(2).filter(|w| w[0] && w[1]).count() as f64;
    let p_uncond = total / drops.len() as f64;
    let p_cond = pairs / total;
    assert!(
        p_cond > p_uncond * 3.0,
        "burst loss must cluster: P(drop|drop)={p_cond:.3} vs P(drop)={p_uncond:.3}"
    );
}

#[test]
fn delay_and_jitter_stay_within_bounds() {
    let profile = NetProfile {
        delay_us: 20_000,
        jitter_us: 5_000,
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(profile, 3);
    for _ in 0..1_000 {
        match sim.decide(64, 100_000) {
            Verdict::Deliver { at_us } => {
                assert!((115_000..=125_000).contains(&at_us), "at_us={at_us}");
            }
            other => panic!("expected Deliver, got {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ghostframe-e2e --test netsim`
Expected: FAIL — burst state and jitter not implemented.

- [ ] **Step 3: Implement**

In `decide`, in this order: advance the Gilbert-Elliott state (`in_burst` toggles on `bernoulli(burst_enter)` / `bernoulli(burst_exit)`); drop on `bernoulli(burst_loss)` when in the bad state or `bernoulli(loss)` otherwise; compute `at_us = now_us + delay_us + jitter` where jitter is uniform in `[-jitter_us, +jitter_us]` (clamped so `at_us >= now_us`); add a uniform `[0, reorder_us)` offset when reordering is enabled; then roll duplication and corruption.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p ghostframe-e2e --test netsim`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(netsim): Gilbert-Elliott bursts, jitter, reorder, duplication, corruption"
```

---

### Task 12: Token-bucket bandwidth cap

**Files:**
- Modify: `ghostframe-e2e/src/netsim/mod.rs`
- Test: `ghostframe-e2e/tests/netsim.rs` (extend)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn delivered_rate_tracks_the_cap_across_a_step_down() {
    let profile = NetProfile {
        cap: CapTimeline::step(1_000_000, 500_000, 250_000), // 1 MB/s, then 250 kB/s at t=0.5s
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(profile, 11);

    // Offer 1200-byte datagrams every 500 µs for one second.
    let mut delivered_before = 0usize;
    let mut delivered_after = 0usize;
    let mut now_us = 0u64;
    while now_us < 1_000_000 {
        if !matches!(sim.decide(1200, now_us), Verdict::Drop) {
            if now_us < 500_000 {
                delivered_before += 1200;
            } else {
                delivered_after += 1200;
            }
        }
        now_us += 500;
    }

    let bps_before = delivered_before as f64 * 2.0; // half a second
    let bps_after = delivered_after as f64 * 2.0;
    assert!(
        (bps_before - 1_000_000.0).abs() < 150_000.0,
        "pre-step rate {bps_before} must track 1 MB/s"
    );
    assert!(
        (bps_after - 250_000.0).abs() < 50_000.0,
        "post-step rate {bps_after} must track 250 kB/s"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-e2e --test netsim delivered_rate`
Expected: FAIL — no token bucket, everything delivers.

- [ ] **Step 3: Implement**

Refill on every `decide`: `tokens = (tokens + elapsed_s * bps_at(now_us)).min(burst_capacity)` with `burst_capacity = bps / 10` (100 ms of buffering); if `tokens >= len` subtract and continue, otherwise return `Verdict::Drop`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-e2e --test netsim`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(netsim): token-bucket bandwidth cap with a mutable timeline"
```

---

### Task 13: Socketpair pump

Bridges the netsim to the real `IoBridge` using ghostbridge's own framing, so the harness cannot drift from the production wire format.

**Files:**
- Create: `ghostframe-e2e/src/netsim/pump.rs`
- Test: `ghostframe-e2e/tests/netsim_pump.rs`

- [ ] **Step 1: Write the failing test**

```rust
use ghostframe_e2e::netsim::pump::SocketPairPump;
use ghostframe_lib::transport::ghostbridge::encode_frame;
use std::net::{Ipv6Addr, SocketAddr};

#[tokio::test(start_paused = true)]
async fn pump_round_trips_a_framed_datagram() {
    let (ours, peer) = tokio::net::UnixStream::pair().expect("pair");
    let mut pump = SocketPairPump::new(peer);
    let addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 5000);

    // Write a frame as if the bridge had sent it.
    let frame = encode_frame(b"hello", &addr);
    tokio::io::AsyncWriteExt::write_all(&mut { ours }, &frame)
        .await
        .expect("write");

    let got = pump.recv().await.expect("recv");
    assert_eq!(got.payload, b"hello");
    assert_eq!(got.addr, addr);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-e2e --test netsim_pump`
Expected: FAIL — module `pump` not found.

- [ ] **Step 3: Implement**

`SocketPairPump` owns the peer `UnixStream` and mirrors `io_bridge.rs:3637-3748`: `read_exact` an 8-byte header (`total_len` u32 BE, `payload_len` u32 BE), `read_exact` the remainder, then `ghostbridge::parse_frame_rest(&rest, payload_len)`. Sending uses `ghostbridge::encode_frame(payload, &addr)` followed by `write_all`. Both functions are already `pub`; do not reimplement the framing.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-e2e --test netsim_pump`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(netsim): socketpair pump reusing ghostbridge framing"
```

---

# Phase D — Harness and scenes

### Task 14: Scene tile encoders

**Files:**
- Create: `ghostframe-e2e/src/harness/scene_tiles.rs`
- Modify: `ghostframe-e2e/src/harness/mod.rs`
- Test: `ghostframe-e2e/tests/scene_tiles.rs`

- [ ] **Step 1: Write the failing test**

```rust
use ghostframe_e2e::harness::scene_tiles::{encode_tile, TileSpec};
use ghostframe_lib::transport::protocol::Codec;

fn solid_bgra(b: u8, g: u8, r: u8) -> Vec<u8> {
    let mut px = Vec::with_capacity(32 * 32 * 4);
    for _ in 0..(32 * 32) {
        px.extend_from_slice(&[b, g, r, 255]);
    }
    px
}

#[test]
fn solid_encodes_to_one_work_item_of_four_bytes() {
    let work = encode_tile(&TileSpec::Solid { bgra: [10, 20, 30, 255] }, 3, 4, 0);
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].codec, Codec::Solid);
    assert_eq!(work[0].payload, vec![10, 20, 30, 255]);
    assert_eq!((work[0].tile_x, work[0].tile_y), (3, 4));
}

#[test]
fn cdf53_encodes_to_fourteen_passes() {
    let work = encode_tile(&TileSpec::Cdf53 { bgra: solid_bgra(10, 20, 30) }, 0, 0, 0);
    assert_eq!(work.len(), 14, "CDF53 emits 14 progressive passes");
    assert_eq!(work[0].total_passes, 14);
    for (i, w) in work.iter().enumerate() {
        assert_eq!(w.pass_idx as usize, i);
        assert_eq!(w.codec, Codec::Cdf53);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-e2e --test scene_tiles`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement**

```rust
pub enum TileSpec {
    Solid { bgra: [u8; 4] },
    Cdf53 { bgra: Vec<u8> },          // 32*32*4 BGRA
    PalRle { bgra: Vec<u8>, palette_id: u8 },
}

pub fn encode_tile(spec: &TileSpec, tile_x: u8, tile_y: u8, generation: u8) -> Vec<TileWork>;
```

- `Solid` → `ghostframe_protocol::codec::solid::encode_solid(&bgra_tile)`, one item, `total_passes: 1`.
- `Cdf53` → `cdf53::forward(&bgra)` then `cdf53::encode_passes(&coeffs)`, one `TileWork` per pass with `pass_idx = i`, `total_passes = 14`.
- `PalRle` → build a `PaletteEntry` from the tile's distinct colours (panic if more than 16 — scenes must be authored within the palette limit), pack indices into `[u8; 512]`, and call `encode_pal_rle_payload(&packed, &palette, palette_id, true)`. **Bundled only**: the palette travels inline, so no server-side palette-table state is needed for injected tiles.

`queued_at` is `Instant::now()` at construction; the scheduler overwrites timing on enqueue.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-e2e --test scene_tiles`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(harness): scene tile encoders for Solid, Cdf53 and bundled PalRle"
```

---

### Task 15: The scene runner

**Files:**
- Create: `ghostframe-e2e/src/harness/browserless.rs`
- Test: `ghostframe-e2e/tests/browserless.rs` (extend)

- [ ] **Step 1: Write the failing test**

```rust
use ghostframe_e2e::harness::browserless::{run_browserless, BrowserlessScene, FrameScript};
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::NetProfile;
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn a_single_solid_tile_arrives_on_a_perfect_link() {
    let scene = BrowserlessScene {
        seed: 1,
        frames: vec![FrameScript {
            tiles: vec![((0, 0), TileSpec::Solid { bgra: [10, 20, 30, 255] })],
        }],
        net: NetProfile::perfect(),
        duration: Duration::from_millis(500),
    };

    let result = run_browserless(scene).await.expect("scene ran");

    let px = result.framebuffer.tile_rgba(0, 0).expect("tile (0,0) decoded");
    assert_eq!(&px[0..4], &[30, 20, 10, 255], "BGRA -> RGBA swizzle");
    assert!(result.stale_generation_tiles == 0);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ghostframe-e2e --test browserless a_single_solid_tile`
Expected: FAIL — module `browserless` not found.

- [ ] **Step 3: Implement the runner**

```rust
pub struct BrowserlessScene {
    pub seed: u64,
    pub frames: Vec<FrameScript>,
    pub net: NetProfile,
    pub duration: Duration,
}

pub struct BrowserlessResult {
    pub framebuffer: FrameBuffer,          // 32x32 RGBA tiles, keyed by (x, y)
    pub events: Vec<ClientNetEvent>,
    pub bytes_delivered: u64,
    pub bytes_dropped: u64,
    pub stale_generation_tiles: u32,
    pub seed: u64,
}

pub async fn run_browserless(scene: BrowserlessScene) -> anyhow::Result<BrowserlessResult>;
```

Wiring, in order:

1. `UnixStream::pair()`; `QuicServer::new()`; `mpsc::channel` for injection.
2. `IoBridge::new_with_injection_for_test(ours, server, inject_rx)`; run it with `LocalSet::spawn_local` so it works whether or not `IoBridge` is `Send`.
3. `SocketPairPump::new(peer)` on the other end, with a `NetSim` per direction (independent RNG streams: seed and `seed ^ 0xA5A5_A5A5_A5A5_A5A5`).
4. `ClientNet::new` with the server's pinned cert hash; `connect`; pump until `SessionReady`.
5. Send each `FrameScript`'s encoded `TileWork` batch on the injection channel, one frame per 16 ms of virtual time.
6. Loop: move datagrams through the netsim honouring each `Verdict` (a `Deliver { at_us }` in the future is queued and released when virtual time reaches it), call `client.on_timeout` when `poll_timeout` is due, and advance the clock with `tokio::time::advance` to the earliest of the next delivery, the next client timeout, and the next frame.
7. Stop at `scene.duration`; return the collected state.

Track `stale_generation_tiles` by recording the generation the scene assigned to each tile and counting any `TileReady` whose frame is older than one already rendered for that tile.

Always include `scene.seed` in every failure message.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ghostframe-e2e --test browserless`
Expected: PASS, 2 tests (Task 1's constructor test plus this one).

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(harness): browserless scene runner over the netsim"
```

---

### Task 16: The assertion scenes

**Files:**
- Modify: `ghostframe-e2e/tests/browserless.rs`

- [ ] **Step 1: Write the failing tests**

```rust
/// Assertion class 1 — convergence under loss.
#[tokio::test(start_paused = true)]
async fn cdf53_converges_to_lossless_under_10pct_loss() {
    let scene = BrowserlessScene {
        seed: 0x5EED,
        frames: vec![FrameScript {
            tiles: vec![((0, 0), TileSpec::Cdf53 { bgra: gradient_tile() })],
        }],
        net: NetProfile { loss: 0.10, ..NetProfile::perfect() },
        duration: Duration::from_secs(10),
    };
    let result = run_browserless(scene).await.expect("scene ran");
    let px = result.framebuffer.tile_rgba(0, 0).expect("tile decoded");
    assert_eq!(px, expected_rgba(&gradient_tile()), "seed 0x5EED: must converge lossless");
}

/// Assertion class 2 — no stale generation is ever rendered.
#[tokio::test(start_paused = true)]
async fn superseded_generations_never_render() {
    let scene = BrowserlessScene {
        seed: 0xB0B,
        // Same tile rewritten with a different colour on each of 20 frames.
        frames: (0..20)
            .map(|i| FrameScript {
                tiles: vec![((0, 0), TileSpec::Solid { bgra: [i * 10, 20, 30, 255] })],
            })
            .collect(),
        net: NetProfile { loss: 0.05, reorder_us: 30_000, ..NetProfile::perfect() },
        duration: Duration::from_secs(5),
    };
    let result = run_browserless(scene).await.expect("scene ran");
    assert_eq!(result.stale_generation_tiles, 0, "seed 0xB0B");
}

/// Assertion class 3 — goodput follows a step-down in the cap.
#[tokio::test(start_paused = true)]
async fn goodput_tracks_a_bandwidth_step_down() {
    let scene = BrowserlessScene {
        seed: 0xCAFE,
        frames: busy_frames(60),
        net: NetProfile {
            cap: CapTimeline::step(2_000_000, 2_000_000, 500_000),
            ..NetProfile::perfect()
        },
        duration: Duration::from_secs(4),
    };
    let result = run_browserless(scene).await.expect("scene ran");
    assert!(
        result.bytes_delivered > 0 && result.bytes_dropped > 0,
        "seed 0xCAFE: the cap must both pass and shed traffic"
    );
}

/// Assertion class 4 — no pass starves under sustained pressure.
#[tokio::test(start_paused = true)]
async fn every_cdf53_pass_eventually_lands() {
    let scene = BrowserlessScene {
        seed: 0xF00D,
        frames: vec![FrameScript {
            tiles: (0..4)
                .flat_map(|x| (0..4).map(move |y| ((x, y), TileSpec::Cdf53 { bgra: gradient_tile() })))
                .collect(),
        }],
        net: NetProfile { loss: 0.05, cap: CapTimeline::constant(400_000), ..NetProfile::perfect() },
        duration: Duration::from_secs(20),
    };
    let result = run_browserless(scene).await.expect("scene ran");
    for x in 0..4u8 {
        for y in 0..4u8 {
            assert!(
                result.framebuffer.tile_rgba(x, y).is_some(),
                "seed 0xF00D: tile ({x},{y}) never completed"
            );
        }
    }
}
```

Helpers `gradient_tile()`, `expected_rgba()`, and `busy_frames(n)` live at the bottom of the test file. `expected_rgba` performs the BGRA→RGBA swizzle with alpha forced to 255, matching `reassembly.rs::finish_assembly`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ghostframe-e2e --test browserless`
Expected: FAIL — convergence and starvation scenes time out or return incomplete tiles.

- [ ] **Step 3: Make them pass**

These exercise existing server and client behaviour, so failures here are harness bugs, not protocol bugs — most likely in the virtual-time advance loop (a scene that stops advancing looks exactly like a stalled protocol). Debug with `RUST_LOG=ghostframe::io_bridge=debug` and the recorded seed. If a failure turns out to be a genuine protocol defect, stop and report it rather than weakening the assertion.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p ghostframe-e2e --test browserless`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "test(browserless): convergence, generation safety, goodput and starvation scenes"
```

---

### Task 17: Fuzz target and CI

**Files:**
- Create: `ghostframe-client-net/fuzz/Cargo.toml`, `ghostframe-client-net/fuzz/fuzz_targets/handle_udp.rs`
- Modify: `.github/workflows/client-core.yml`

- [ ] **Step 1: Write the fuzz target**

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use ghostframe_client_net::{ClientNet, ClientNetConfig};
use std::net::{Ipv6Addr, SocketAddr};

fuzz_target!(|data: &[u8]| {
    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256: [0u8; 32],
        indices_raw_enabled: true,
        supports_h264: false,
    };
    let mut client = ClientNet::new(cfg, 0).expect("new");
    let from = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443);
    // Must never panic, whatever the bytes are.
    let _ = client.handle_udp(data, from, 0);
});
```

- [ ] **Step 2: Run it briefly**

Run: `cargo +nightly fuzz run handle_udp -- -max_total_time=60`
Expected: no crashes in 60 seconds.

If the pinned toolchain has no nightly available, skip execution and note it — the target still has to compile, which Step 3 covers in CI.

- [ ] **Step 3: Extend CI**

In `.github/workflows/client-core.yml`, extend both cargo invocations:

```yaml
      - run: cargo test -p ghostframe-protocol -p ghostframe-client-core -p ghostframe-client-net
      - run: cargo build -p ghostframe-protocol -p ghostframe-client-core -p ghostframe-client-net --target wasm32-unknown-unknown
```

`ghostframe-client-net` must stay wasm-clean: sub-project 4 compiles it into the browser bundle.

- [ ] **Step 4: Verify**

Run: `cargo build -p ghostframe-client-net --target wasm32-unknown-unknown`
Expected: clean build.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "test(client-net): cargo-fuzz target for handle_udp + CI wasm gate"
```

---

## Deferred (not this plan)

- **CPU classify + encode path** — lets GPU-gated scenarios migrate and shrinks `ci/skip-list.txt`.
- **BWE and pacing** — sub-project 3, developed against these scenes.
- **Thin (non-bundled) PalRle injection** — needs server-side palette-table synchronisation.
- **H.264 decode in the headless client** — `NeedsH264` stays counted, not decoded.
- **`ffmpeg-next` 9.0.0 bump** — prerequisite for running any of this locally; separate PR.
