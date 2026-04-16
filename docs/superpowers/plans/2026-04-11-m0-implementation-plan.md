# M0: QUIC Ping + Automated E2E Test

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove that a browser can send a WebTransport datagram to a server running quinn-proto over a libtailscale userspace netstack, and receive a datagram back. No kernel sockets, no GPU code, no video.

**Architecture:** A custom Go bridge (`ghostbridge`) exposes `tsnet.Server.ListenPacket()` as C functions with datagram framing. The Rust I/O bridge reads framed UDP packets from ghostbridge, feeds them into `quinn-proto::Endpoint::handle()`, and writes outgoing packets back. On top of quinn-proto, we implement minimal HTTP/3 + WebTransport handshake using protocol types from the `web-transport-proto` crate. The browser client opens a WebTransport session and exchanges datagrams. The xdaemon calls `ghostframe-lib` via the Rust API (`IoBridge`). The C FFI (`gf_server_new` etc.) is deferred to M1 when the API surface stabilizes.

**Tech Stack:** Rust (ghostframe-lib, ghostframe-xdaemon), Go (ghostbridge C archive), TypeScript (browser client), Docker (headscale, test-server, test-client), chromiumoxide (E2E testing)

---

## File Structure

```
ghostframe/
├── Cargo.toml                          # Workspace root
├── Justfile                             # Build + test commands
├── ghostbridge/                         # Go package: C archive exposing raw UDP via tsnet
│   ├── go.mod
│   ├── go.sum
│   ├── main.go                          # Go entry point: tsnet.Server + ListenPacket + framing protocol
│   ├── bridge.h                         # Generated C header
│   └── Makefile                         # go build -buildmode=c-archive
├── ghostframe-lib/
│   ├── Cargo.toml
│   ├── build.rs                         # Builds ghostbridge.a, links it, runs cbindgen
│   └── src/
│       ├── lib.rs                       # Re-exports, crate setup
│       └── transport/
│           ├── mod.rs                    # Pub modules
│           ├── ghostbridge.rs            # Rust FFI to ghostbridge C functions
│           ├── quic.rs                   # Quinn-proto state machine wrapper (Endpoint, Connection, config)
│           ├── io_bridge.rs              # Event loop: ghostbridge ↔ quinn-proto ↔ timers
│           ├── webtransport.rs           # HTTP/3 SETTINGS exchange + WebTransport CONNECT handshake
│           └── protocol.rs              # Datagram format definitions (ping/pong for M0)
├── ghostframe-xdaemon/
│   ├── Cargo.toml
│   └── src/
│       └── main.rs                       # Calls gf_server_new(), sleeps. Ping server for M0.
├── ghostframe-web-client/
│   ├── package.json
│   ├── tsconfig.json
│   ├── vite.config.ts
│   ├── index.html                       # Entry page
│   └── src/
│       └── main.ts                       # WebTransport connect, send ping, receive pong
├── tests/
│   ├── e2e/
│   │   ├── mod.rs                       # Test module root
│   │   ├── helpers.rs                   # headscale setup, cert-hash log scraper, TestNode, UdpForwarder
│   │   └── ping.rs                      # M0 E2E test (chromiumoxide + embedded tsnet)
│   └── containers/
│       ├── headscale/
│       │   ├── config/
│       │   │   └── config.yaml
│       │   └── Dockerfile (or use upstream image)
│       └── test-server/
│           ├── Dockerfile
│           └── entrypoint.sh
└── docs/
    ├── specs/
    ├── design/
    └── superpowers/
        ├── specs/
        └── plans/
```

---

## Task 0: Pin Dependency Versions and Verify APIs

**Rationale:** Several code snippets in Tasks 1–9 were drafted against best-guess API shapes. Every wrong API call costs an executing agent 10+ minutes of debugging and context-rotting retries. This task verifies the real API surface of each third-party crate against current docs and updates the snippets *in this plan file* before Task 1 begins. Budget: ~1 hour.

**Files:**
- Create: `Cargo.toml` with exact version pins (just the file, no workspace members yet — those come in Task 1)
- Modify: this plan file, in-place, with any corrected snippets

- [ ] **Step 1: Pin exact versions**

Check crates.io for the latest compatible release and pin exact (no `^`) versions in the workspace dependencies table:
- `quinn-proto` (0.11.x)
- `rustls` (0.23.x) — decide between `ring` and `aws-lc-rs` crypto provider; `ring` is simpler for M0
- `rcgen` (0.13.x)
- `web-transport-proto` (0.6.x or latest)
- `tokio` (1.x) with features `["rt-multi-thread", "macros", "sync", "time", "net", "io-util"]`
- `bytes`, `thiserror`, `tracing`, `tracing-subscriber`, `libc`, `cbindgen`

- [ ] **Step 2: Verify quinn-proto 0.11.x**
  - `Endpoint::new` signature and return type
  - Correct path for the rustls crypto adapter (likely `quinn_proto::crypto::rustls::QuicServerConfig::try_from(rustls::ServerConfig)`, **not** `crypto::rustls::server`)
  - `ServerConfig::with_crypto(Arc<dyn ServerConfig>)` — confirm trait object wrapping
  - `TransportConfig::datagram_receive_buffer_size` / `datagram_send_buffer_size` — confirm `Option<usize>` shape
  - `Endpoint::handle(now, remote, local_ip, ecn, data, buf)` — confirm argument list and `DatagramEvent` return shape
  - `Connection::datagrams().send(Bytes, drop_on_full)` / `.recv() -> Option<Bytes>`
  - `Connection::poll_transmit(now, max_datagrams, buf) -> Option<Transmit>`
  - `Connection::poll_timeout() -> Option<Instant>`
  - `Connection::handle_timeout(now)`
  - `Connection::poll() -> Option<Event>` (app events)
  - Update Task 4 and Task 5 snippets to match

- [ ] **Step 3: Verify rustls 0.23.x**
  - ALPN configured via `ServerConfig::alpn_protocols = vec![b"h3".to_vec()]`
  - A default `CryptoProvider` must be installed before building the config (`rustls::crypto::ring::default_provider().install_default()`)
  - Cert/key types are `rustls::pki_types::CertificateDer<'static>` and `PrivateKeyDer<'static>`, not `rustls::Certificate` / `rustls::PrivateKey` (those are pre-0.22)
  - Update Task 4 snippet

- [ ] **Step 4: Verify rcgen 0.13.x**
  - `generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])` returns `CertifiedKey { cert, key_pair }`
  - Cert DER via `cert.cert.der().to_vec()` (not `cert.serialize_der()`)
  - Private key DER via `cert.key_pair.serialize_der()`
  - **Important:** include `127.0.0.1` in SAN so the browser can verify when the E2E test's UDP forwarder proxies from localhost (see Task 9)
  - Update Task 4 snippet

- [ ] **Step 5: Verify web-transport-proto 0.6.x**
  - Read the crate's source: identify which types are pure encode/decode (`Settings`, `ConnectRequest`, `ConnectResponse`, `VarInt`, `Frame`) vs which are wired to async I/O.
  - If the buffer-based primitives exist, use them directly in Task 6.
  - If everything is async-wrapped, fork the relevant `encode`/`decode` functions into `ghostframe-lib/src/transport/webtransport.rs` — attribute in a comment. Budget ~1 day of Task 6 for this.
  - Confirm the QPACK dependency situation: h3-datagram sessions need minimal QPACK (static table only for `:status`, `:method`, `:protocol`). If `web-transport-proto` pulls in `qpack`, great. If not, hand-roll the encoding for the 4–5 headers we need.

- [ ] **Step 6: Verify tokio 1.x**
  - `tokio::io::unix::AsyncFd` available (requires the `net` feature)
  - `tokio::net::UnixStream::from_std(std::os::unix::net::UnixStream)` — needed to wrap the ghostbridge socketpair fd for async I/O
  - `tokio::time::Instant::from_std(std::time::Instant)` for bridging quinn-proto's `std::time::Instant` timeouts into tokio's timer wheel
  - `tokio::select!` with `biased` mode for deterministic poll order in the event loop

- [ ] **Step 7: Verify headscale CLI for preauth key JSON output**
  - `docker exec headscale headscale users create testuser`
  - `docker exec headscale headscale preauthkeys create --user testuser --reusable --expiration 1h --output json`
  - Confirm the JSON schema (the `key` field name) by running it once against a local `headscale/headscale:0.28.0` container
  - Note findings in Task 9 so `create_preauth_key` can parse it correctly

- [ ] **Step 8: Verify chromiumoxide 0.7.x (replaces playwright-rs)**
  - `chromiumoxide` is a pure-Rust CDP client with no Node.js dependency, unlike `playwright-rs`, which transitively needs a Node install. This simplifies CI.
  - Confirm: `Browser::launch(BrowserConfig::builder().arg("--ignore-certificate-errors-spki-list=...").build())`
  - Confirm page navigation + `evaluate_expression` for reading DOM text
  - Update Tasks 9 and 11 to use chromiumoxide

- [ ] **Step 9: Patch this plan file in place**

Apply all corrections discovered in Steps 2–8 directly to the code snippets in Tasks 1–9. Do not leave a single API call in the plan that you know is wrong — the whole point of Task 0 is that downstream tasks can be executed mechanically.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml docs/superpowers/plans/2026-04-11-m0-implementation-plan.md
git commit -m "chore: pin dependency versions and correct API references in M0 plan"
```

---

## Task 1: Cargo Workspace + Justfile

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `ghostframe-lib/Cargo.toml`
- Create: `ghostframe-lib/src/lib.rs`
- Create: `ghostframe-xdaemon/Cargo.toml`
- Create: `ghostframe-xdaemon/src/main.rs`
- Create: `Justfile`

- [ ] **Step 1: Create workspace root Cargo.toml**

```toml
[workspace]
resolver = "2"
members = [
    "ghostframe-lib",
    "ghostframe-xdaemon",
]

[workspace.dependencies]
# Exact versions pinned in Task 0 after API verification.
quinn-proto = "0.11"
tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros", "sync", "time", "net", "io-util", "process"] }
bytes = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

- [ ] **Step 2: Create ghostframe-lib Cargo.toml**

```toml
[package]
name = "ghostframe-lib"
version = "0.1.0"
edition = "2021"
crate-type = ["cdylib", "staticlib", "rlib"]

[dependencies]
quinn-proto = { workspace = true }
tokio = { workspace = true }
bytes = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
libc = "0.2"

[build-dependencies]
cbindgen = "0.27"
```

- [ ] **Step 3: Create ghostframe-lib/src/lib.rs**

```rust
pub mod transport;
```

- [ ] **Step 4: Create ghostframe-lib/src/transport/mod.rs**

```rust
pub mod ghostbridge;
pub mod quic;
pub mod io_bridge;
pub mod webtransport;
pub mod protocol;
```

- [ ] **Step 5: Create ghostframe-xdaemon Cargo.toml**

```toml
[package]
name = "ghostframe-xdaemon"
version = "0.1.0"
edition = "2021"

[dependencies]
ghostframe-lib = { path = "../ghostframe-lib" }
tokio = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
```

- [ ] **Step 6: Create ghostframe-xdaemon/src/main.rs (ping server stub)**

```rust
fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("ghostframe-xdaemon starting (M0: ping server mode)");
    // Replaced in Task 8 with full I/O bridge
    std::thread::park();
}
```

- [ ] **Step 7: Create Justfile**

```just
build:
    cargo build

build-release:
    cargo build --release

test-unit:
    cargo test --lib

test-e2e:
    cargo test --test e2e

containers-build:
    docker build -t ghostframe/test-server tests/containers/test-server/
    docker build -t ghostframe/test-client tests/containers/test-client/

lint:
    cargo clippy -- -D warnings

fmt:
    cargo fmt -- --check
```

- [ ] **Step 8: Verify workspace compiles (with empty module stubs)**

```rust
// ghostframe-lib/src/transport/ghostbridge.rs
pub struct GhostbridgeConfig {
    pub authkey: String,
    pub hostname: String,
    pub state_dir: String,
    pub control_url: String,
}

pub struct GhostbridgeHandle { /* TODO */ }

impl GhostbridgeHandle {
    pub fn connect(_config: &GhostbridgeConfig) -> Result<Self, String> {
        Err("not implemented".into())
    }
}
```

```rust
// ghostframe-lib/src/transport/quic.rs
pub struct QuicEndpoint { /* TODO */ }

impl QuicEndpoint {
    pub fn new() -> Result<Self, String> {
        Err("not implemented".into())
    }
}
```

```rust
// ghostframe-lib/src/transport/io_bridge.rs
pub struct IoBridge { /* TODO */ }
```

```rust
// ghostframe-lib/src/transport/webtransport.rs
pub struct WebTransportSession { /* TODO */ }
```

```rust
// ghostframe-lib/src/transport/protocol.rs
pub const PING_PAYLOAD: &[u8; 4] = b"ping";
pub const PONG_PAYLOAD: &[u8; 4] = b"pong";
```

Run: `cargo build`
Expected: compiles with no errors (warnings about unused code are OK)

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "feat: initialize Cargo workspace with ghostframe-lib and ghostframe-xdaemon stubs"
```

---

## Task 2: Ghostbridge — Go C Archive with Raw UDP

**Files:**
- Create: `ghostbridge/go.mod`
- Create: `ghostbridge/main.go`
- Create: `ghostbridge/Makefile`

This task builds the Go C archive that bridges tsnet's PacketConn to C-callable functions. This is the highest-risk component and the core novelty of the architecture.

**Ghostbridge C API:**

```c
typedef int32_t gbridge_status;  // 0 = success, <0 = error (see error codes below)

// Create and start a tsnet server node. Writes an opaque handle into *sd_out on success.
gbridge_status gbridge_new(const char* hostname, const char* authkey, const char* state_dir, const char* control_url, int32_t* sd_out);

// Block until the node is connected to the tailnet.
gbridge_status gbridge_up(int32_t sd);

// Start listening for UDP packets on addr (e.g., ":4443").
// Writes a non-blocking socketpair fd into *fd_out for reading/writing framed packets.
// The returned fd already has O_NONBLOCK set so it can be wrapped in tokio::net::UnixStream.
gbridge_status gbridge_listen_udp(int32_t sd, const char* addr, int32_t* fd_out);

// Dial a remote UDP address over tsnet. Writes a non-blocking socketpair fd into *fd_out.
// Used by the E2E test (Task 9) to proxy browser UDP traffic through the tailnet.
gbridge_status gbridge_dial_udp(int32_t sd, const char* remote_addr, int32_t* fd_out);

// Close the server and release the handle.
gbridge_status gbridge_close(int32_t sd);

// Get the node's Tailscale IP addresses (comma-separated). Writes into buf, null-terminated.
gbridge_status gbridge_getips(int32_t sd, char* buf, size_t buflen);

// Error codes:
//   0   success
//  -1   invalid handle
//  -2   tailnet Up failed
//  -3   ListenPacket/Dial failed
//  -4   socketpair creation failed
//  -5   IP list too long for buf
```

**Framing protocol over the fd:**

Each datagram written to/read from the fd uses this framing:
```
[4 bytes: total_len (big-endian, includes these 4 bytes)]
[4 bytes: payload_len (big-endian)]
[payload_len bytes: UDP payload]
[2 bytes: port (big-endian)]
[remaining bytes: IP address string, null-terminated]
```

This allows quinn-proto to send/receive UDP datagrams with full address information through a single fd.

- [ ] **Step 1: Create ghostbridge/go.mod**

```
module github.com/ghostframe/ghostbridge

go 1.23

require tailscale.com v1.94.1
```

(Use `go mod tidy` after creating main.go to resolve the exact version.)

- [ ] **Step 2: Create ghostbridge/main.go**

The Go code will:
1. Maintain a map of `int32` handles to `*tsnet.Server` instances
2. `gbridge_new`: create a `tsnet.Server`, configure it with authkey, hostname, state_dir, control_url
3. `gbridge_up`: call `server.Up(context.Background())` to wait for connectivity
4. `gbridge_listen_udp`: call `server.ListenPacket("udp", addr)` to get a `net.PacketConn`, then spawn goroutines that bridge between the PacketConn and a `socketpair` using the framing protocol above
5. `gbridge_close`: shut down the server

The `socketpair` approach:
- Create a `syscall.Socketpair(AF_UNIX, SOCK_STREAM, 0)` — one end for Go, one end for C
- Goroutine 1: read from PacketConn (ReadFrom), frame the packet, write to socketpair
- Goroutine 2: read from socketpair, unframe the packet, write to PacketConn (WriteTo)
- The C side gets the other end of the socketpair as an `int32` fd

```go
package main

/*
#include <stdint.h>
#include <string.h>

typedef int32_t gbridge_status;
*/
import "C"

import (
	"context"
	"encoding/binary"
	"io"
	"net"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"unsafe"

	"tailscale.com/tsnet"
)

var (
	servers    = make(map[int32]*serverHandle)
	serversMu  sync.Mutex
	nextHandle int32 = 1
)

type serverHandle struct {
	server *tsnet.Server
	cancel context.CancelFunc
}

// setNonblock sets O_NONBLOCK on a raw fd. The Rust side wraps this fd in
// tokio::net::UnixStream, which requires non-blocking mode.
func setNonblock(fd int) error {
	return syscall.SetNonblock(fd, true)
}

//export gbridge_new
func gbridge_new(cHostname, cAuthkey, cStateDir, cControlURL *C.char, sdOut *C.int32_t) C.gbridge_status {
	hostname := C.GoString(cHostname)
	authkey := C.GoString(cAuthkey)
	stateDir := C.GoString(cStateDir)
	controlURL := C.GoString(cControlURL)

	ctx, cancel := context.WithCancel(context.Background())
	s := &tsnet.Server{
		Hostname: hostname,
		AuthKey:  authkey,
		Dir:      stateDir,
	}
	if controlURL != "" {
		s.ControlURL = controlURL
	}

	serversMu.Lock()
	id := nextHandle
	nextHandle++
	servers[id] = &serverHandle{server: s, cancel: cancel}
	serversMu.Unlock()

	_ = ctx
	*sdOut = C.int32_t(id)
	return 0
}

//export gbridge_up
func gbridge_up(sd C.int32_t) C.gbridge_status {
	h := lookup(int32(sd))
	if h == nil {
		return -1
	}
	if _, err := h.server.Up(context.Background()); err != nil {
		return -2
	}
	return 0
}

//export gbridge_listen_udp
func gbridge_listen_udp(sd C.int32_t, cAddr *C.char, fdOut *C.int32_t) C.gbridge_status {
	h := lookup(int32(sd))
	if h == nil {
		return -1
	}
	conn, err := h.server.ListenPacket("udp", C.GoString(cAddr))
	if err != nil {
		return -3
	}
	goFd, cFd, err := makeSocketpair()
	if err != nil {
		conn.Close()
		return -4
	}
	spawnPacketBridge(conn, goFd)
	*fdOut = C.int32_t(cFd)
	return 0
}

//export gbridge_dial_udp
func gbridge_dial_udp(sd C.int32_t, cRemote *C.char, fdOut *C.int32_t) C.gbridge_status {
	h := lookup(int32(sd))
	if h == nil {
		return -1
	}
	// tsnet.Server.Dial returns a net.Conn; for UDP it wraps the packet conn.
	netConn, err := h.server.Dial(context.Background(), "udp", C.GoString(cRemote))
	if err != nil {
		return -3
	}
	// Wrap as PacketConn-like: reads come from this one peer, writes go to it.
	pc := &dialedPacketConn{Conn: netConn}
	goFd, cFd, err := makeSocketpair()
	if err != nil {
		netConn.Close()
		return -4
	}
	spawnPacketBridge(pc, goFd)
	*fdOut = C.int32_t(cFd)
	return 0
}

//export gbridge_close
func gbridge_close(sd C.int32_t) C.gbridge_status {
	serversMu.Lock()
	h, ok := servers[int32(sd)]
	if ok {
		delete(servers, int32(sd))
	}
	serversMu.Unlock()
	if !ok {
		return -1
	}
	h.cancel()
	h.server.Close()
	return 0
}

//export gbridge_getips
func gbridge_getips(sd C.int32_t, cBuf *C.char, cBufLen C.size_t) C.gbridge_status {
	h := lookup(int32(sd))
	if h == nil {
		return -1
	}
	// tsnet.Server.TailscaleIPs() returns (ip4, ip6 netip.Addr), not a slice,
	// and does not return an error. Either may be zero/invalid.
	ip4, ip6 := h.server.TailscaleIPs()
	parts := make([]string, 0, 2)
	if ip4.IsValid() {
		parts = append(parts, ip4.String())
	}
	if ip6.IsValid() {
		parts = append(parts, ip6.String())
	}
	s := strings.Join(parts, ",")
	if C.size_t(len(s)+1) > cBufLen {
		return -5
	}
	// Copy bytes directly into the caller's buffer (no intermediate C.CString).
	dst := unsafe.Slice((*byte)(unsafe.Pointer(cBuf)), int(cBufLen))
	copy(dst, s)
	dst[len(s)] = 0
	return 0
}

func lookup(sd int32) *serverHandle {
	serversMu.Lock()
	defer serversMu.Unlock()
	return servers[sd]
}

func makeSocketpair() (goFd, cFd int, err error) {
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM, 0)
	if err != nil {
		return 0, 0, err
	}
	// Only the C side needs O_NONBLOCK (tokio AsyncFd expects it).
	// The Go side stays blocking for simple goroutine loops.
	if err := setNonblock(fds[1]); err != nil {
		syscall.Close(fds[0])
		syscall.Close(fds[1])
		return 0, 0, err
	}
	return fds[0], fds[1], nil
}

// dialedPacketConn adapts a connected net.Conn to a PacketConn-ish interface
// used by the packet bridge. Source address on reads is always the dialed peer.
type dialedPacketConn struct {
	net.Conn
}

func (d *dialedPacketConn) ReadFrom(p []byte) (int, net.Addr, error) {
	n, err := d.Conn.Read(p)
	return n, d.Conn.RemoteAddr(), err
}

func (d *dialedPacketConn) WriteTo(p []byte, _ net.Addr) (int, error) {
	return d.Conn.Write(p)
}

type readFromWriter interface {
	ReadFrom(p []byte) (int, net.Addr, error)
	WriteTo(p []byte, addr net.Addr) (int, error)
	Close() error
}

// spawnPacketBridge runs two goroutines that copy framed packets between a
// PacketConn (tsnet side) and a blocking socketpair fd (Rust side).
//
// Frame layout: [total_len(4)][payload_len(4)][payload][port(2)][host\0]
// total_len is the full frame size including the first 4 bytes.
func spawnPacketBridge(conn readFromWriter, goFd int) {
	// tsnet -> socketpair
	go func() {
		defer syscall.Close(goFd)
		buf := make([]byte, 65535)
		for {
			n, srcAddr, err := conn.ReadFrom(buf)
			if err != nil {
				return
			}
			host, port, err := net.SplitHostPort(srcAddr.String())
			if err != nil {
				continue
			}
			portNum, _ := strconv.Atoi(port)
			frame := encodeFrame(buf[:n], host, uint16(portNum))
			if _, err := writeAll(goFd, frame); err != nil {
				return
			}
		}
	}()

	// socketpair -> tsnet
	go func() {
		defer conn.Close()
		header := make([]byte, 8)
		for {
			if _, err := readFull(goFd, header); err != nil {
				return
			}
			totalLen := binary.BigEndian.Uint32(header[0:4])
			payloadLen := binary.BigEndian.Uint32(header[4:8])
			if totalLen < 8+payloadLen+3 {
				return // malformed
			}
			rest := make([]byte, totalLen-8)
			if _, err := readFull(goFd, rest); err != nil {
				return
			}
			payload := rest[:payloadLen]
			port := binary.BigEndian.Uint16(rest[payloadLen : payloadLen+2])
			hostBytes := rest[payloadLen+2:]
			// Trim trailing null terminator.
			if i := indexByte(hostBytes, 0); i >= 0 {
				hostBytes = hostBytes[:i]
			}
			dst, err := net.ResolveUDPAddr("udp", net.JoinHostPort(string(hostBytes), strconv.Itoa(int(port))))
			if err != nil {
				continue
			}
			conn.WriteTo(payload, dst)
		}
	}()
}

func encodeFrame(payload []byte, host string, port uint16) []byte {
	hostBytes := []byte(host)
	totalLen := 4 + 4 + len(payload) + 2 + len(hostBytes) + 1
	frame := make([]byte, totalLen)
	binary.BigEndian.PutUint32(frame[0:4], uint32(totalLen))
	binary.BigEndian.PutUint32(frame[4:8], uint32(len(payload)))
	copy(frame[8:], payload)
	off := 8 + len(payload)
	binary.BigEndian.PutUint16(frame[off:off+2], port)
	copy(frame[off+2:], hostBytes)
	frame[totalLen-1] = 0
	return frame
}

// readFull reads exactly len(p) bytes from fd, retrying on short reads.
// The Go side of the socketpair is blocking, so io.EOF means the peer closed.
func readFull(fd int, p []byte) (int, error) {
	total := 0
	for total < len(p) {
		n, err := syscall.Read(fd, p[total:])
		if err != nil {
			return total, err
		}
		if n == 0 {
			return total, io.EOF
		}
		total += n
	}
	return total, nil
}

func writeAll(fd int, p []byte) (int, error) {
	total := 0
	for total < len(p) {
		n, err := syscall.Write(fd, p[total:])
		if err != nil {
			return total, err
		}
		total += n
	}
	return total, nil
}

func indexByte(b []byte, c byte) int {
	for i, x := range b {
		if x == c {
			return i
		}
	}
	return -1
}

func main() {} // Required for c-archive build mode
```

**Notes for the implementing agent:**
- All exported functions use explicit out-params for handles/fds so that `gbridge_status` uniformly means "0 = ok, negative = error." Do not re-introduce the pattern of returning the handle as the status code.
- `readFull` / `writeAll` are mandatory. The socketpair is `SOCK_STREAM`, so short reads and writes will happen under load.
- `gbridge_dial_udp` is needed by the E2E test in Task 9; implement it alongside `gbridge_listen_udp` in this task.
- `h.server.TailscaleIPs()` returns `(ip4, ip6 netip.Addr)` — two values, no error. Check `.IsValid()` on each before using.

- [ ] **Step 3: Create ghostbridge/Makefile**

```makefile
.PHONY: archive clean

archive:
	go build -buildmode=c-archive -o libghostbridge.a .
	# This also generates libghostbridge.h

clean:
	rm -f libghostbridge.a libghostbridge.h
```

- [ ] **Step 4: Build the Go archive and verify it compiles**

```bash
cd ghostbridge
go mod tidy
make archive
ls -la libghostbridge.a libghostbridge.h
```

Expected: `libghostbridge.a` (~15MB with Go runtime) and `libghostbridge.h` are generated.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: ghostbridge Go C archive with raw UDP PacketConn bridge"
```

---

## Task 3: Ghostbridge Rust FFI

**Files:**
- Create: `ghostframe-lib/src/transport/ghostbridge.rs` (replace stub)
- Modify: `ghostframe-lib/build.rs` (compile ghostbridge.a and link)

- [ ] **Step 1: Write the Rust FFI bindings**

```rust
use std::ffi::{c_char, c_int, CStr, CString};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

extern "C" {
    fn gbridge_new(
        hostname: *const c_char,
        authkey: *const c_char,
        state_dir: *const c_char,
        control_url: *const c_char,
        sd_out: *mut c_int,
    ) -> c_int;
    fn gbridge_up(sd: c_int) -> c_int;
    fn gbridge_listen_udp(sd: c_int, addr: *const c_char, fd_out: *mut c_int) -> c_int;
    fn gbridge_dial_udp(sd: c_int, remote_addr: *const c_char, fd_out: *mut c_int) -> c_int;
    fn gbridge_close(sd: c_int) -> c_int;
    fn gbridge_getips(sd: c_int, buf: *mut c_char, buf_len: usize) -> c_int;
}

#[derive(Debug, thiserror::Error)]
pub enum GhostbridgeError {
    #[error("ghostbridge C call failed: {0} (rc={1})")]
    Ffi(&'static str, c_int),
    #[error("invalid C string: {0}")]
    Cstring(#[from] std::ffi::NulError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid frame: {0}")]
    Frame(&'static str),
}

pub struct GhostbridgeConfig {
    pub hostname: String,
    pub authkey: String,
    pub state_dir: String,
    pub control_url: String,
}

pub struct GhostbridgeHandle {
    sd: c_int,
}

impl GhostbridgeHandle {
    pub fn connect(config: &GhostbridgeConfig) -> Result<Self, GhostbridgeError> {
        let c_hostname = CString::new(config.hostname.clone())?;
        let c_authkey = CString::new(config.authkey.clone())?;
        let c_state_dir = CString::new(config.state_dir.clone())?;
        let c_control_url = CString::new(config.control_url.clone())?;

        let mut sd: c_int = -1;
        let rc = unsafe {
            gbridge_new(
                c_hostname.as_ptr(),
                c_authkey.as_ptr(),
                c_state_dir.as_ptr(),
                c_control_url.as_ptr(),
                &mut sd,
            )
        };
        if rc < 0 {
            return Err(GhostbridgeError::Ffi("gbridge_new", rc));
        }
        Ok(Self { sd })
    }

    pub fn up(&self) -> Result<(), GhostbridgeError> {
        let rc = unsafe { gbridge_up(self.sd) };
        if rc < 0 {
            return Err(GhostbridgeError::Ffi("gbridge_up", rc));
        }
        Ok(())
    }

    /// Listen for incoming UDP packets on `addr` (e.g. `":4443"`).
    /// Returns an owned socketpair fd already in O_NONBLOCK mode.
    pub fn listen_udp(&self, addr: &str) -> Result<UdpPacketConn, GhostbridgeError> {
        let c_addr = CString::new(addr)?;
        let mut fd: c_int = -1;
        let rc = unsafe { gbridge_listen_udp(self.sd, c_addr.as_ptr(), &mut fd) };
        if rc < 0 {
            return Err(GhostbridgeError::Ffi("gbridge_listen_udp", rc));
        }
        UdpPacketConn::from_raw_fd(fd)
    }

    /// Dial a remote UDP address over the tailnet. Used by the E2E test to
    /// proxy browser packets through the test's embedded tsnet node.
    pub fn dial_udp(&self, remote_addr: &str) -> Result<UdpPacketConn, GhostbridgeError> {
        let c_remote = CString::new(remote_addr)?;
        let mut fd: c_int = -1;
        let rc = unsafe { gbridge_dial_udp(self.sd, c_remote.as_ptr(), &mut fd) };
        if rc < 0 {
            return Err(GhostbridgeError::Ffi("gbridge_dial_udp", rc));
        }
        UdpPacketConn::from_raw_fd(fd)
    }

    pub fn get_ips(&self) -> Result<Vec<std::net::IpAddr>, GhostbridgeError> {
        let mut buf = [0u8; 1024];
        let rc = unsafe { gbridge_getips(self.sd, buf.as_mut_ptr() as *mut c_char, buf.len()) };
        if rc < 0 {
            return Err(GhostbridgeError::Ffi("gbridge_getips", rc));
        }
        let ip_str = CStr::from_bytes_until_nul(&buf)
            .map_err(|_| GhostbridgeError::Frame("gbridge_getips: missing NUL"))?
            .to_str()
            .map_err(|_| GhostbridgeError::Frame("gbridge_getips: non-utf8"))?;
        if ip_str.is_empty() {
            return Ok(Vec::new());
        }
        ip_str
            .split(',')
            .map(|s| s.parse().map_err(|_| GhostbridgeError::Frame("invalid IP")))
            .collect()
    }
}

impl Drop for GhostbridgeHandle {
    fn drop(&mut self) {
        unsafe { gbridge_close(self.sd) };
    }
}

/// Framed UDP packet read from the ghostbridge fd.
#[derive(Debug, Clone)]
pub struct UdpPacket {
    pub payload: Vec<u8>,
    pub addr: SocketAddr,
}

/// A framed UDP packet connection over a ghostbridge socketpair fd.
///
/// Owns the fd (via `UnixStream`) and is `!Sync` — all methods take `&mut self`.
/// The fd is in O_NONBLOCK mode; the sync methods below exist only for unit
/// tests. Production code consumes the raw fd via `into_raw_fd()` and wraps it
/// in `tokio::net::UnixStream` (see Task 5).
pub struct UdpPacketConn {
    stream: UnixStream,
}

impl UdpPacketConn {
    fn from_raw_fd(fd: c_int) -> Result<Self, GhostbridgeError> {
        // Safety: the fd is freshly returned from ghostbridge and nothing else owns it.
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        // ghostbridge already set O_NONBLOCK, but tell std explicitly so that
        // Read/Write honor it without EAGAIN being surfaced as an error in blocking calls.
        stream.set_nonblocking(false)?; // unit tests use blocking I/O
        Ok(Self { stream })
    }

    pub fn into_raw_fd(self) -> RawFd {
        self.stream.into_raw_fd()
    }

    /// **Blocking** framed read. Only used by unit tests; production path uses tokio.
    pub fn recv_from(&mut self) -> io::Result<UdpPacket> {
        let mut header = [0u8; 8];
        read_exact(&mut self.stream, &mut header)?;
        let total_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
        let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
        if total_len < 8 + payload_len + 3 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too short"));
        }
        let mut rest = vec![0u8; total_len - 8];
        read_exact(&mut self.stream, &mut rest)?;
        parse_frame_rest(&rest, payload_len)
    }

    /// **Blocking** framed write. Only used by unit tests.
    pub fn send_to(&mut self, payload: &[u8], addr: &SocketAddr) -> io::Result<()> {
        let frame = encode_frame(payload, addr);
        self.stream.write_all(&frame)
    }
}

fn read_exact(stream: &mut UnixStream, buf: &mut [u8]) -> io::Result<()> {
    let mut off = 0;
    while off < buf.len() {
        let n = stream.read(&mut buf[off..])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
        }
        off += n;
    }
    Ok(())
}

/// Parse the post-header bytes of a frame into an `UdpPacket`.
/// Shared between the blocking path (above) and the tokio path (Task 5).
pub(crate) fn parse_frame_rest(rest: &[u8], payload_len: usize) -> io::Result<UdpPacket> {
    let payload = rest[..payload_len].to_vec();
    let port = u16::from_be_bytes(rest[payload_len..payload_len + 2].try_into().unwrap());
    let host_bytes = &rest[payload_len + 2..];
    let host_str = CStr::from_bytes_until_nul(host_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame host not null-terminated"))?
        .to_str()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame host not utf-8"))?;
    let addr: SocketAddr = format!("{}:{}", host_str, port)
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid socket address"))?;
    Ok(UdpPacket { payload, addr })
}

pub(crate) fn encode_frame(payload: &[u8], addr: &SocketAddr) -> Vec<u8> {
    let host = addr.ip().to_string();
    let host_bytes = host.as_bytes();
    let total_len = (4 + 4 + payload.len() + 2 + host_bytes.len() + 1) as u32;

    let mut frame = Vec::with_capacity(total_len as usize);
    frame.extend_from_slice(&total_len.to_be_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&addr.port().to_be_bytes());
    frame.extend_from_slice(host_bytes);
    frame.push(0);
    frame
}
```

**Notes:**
- `UdpPacketConn` owns the fd. There is no `Drop` impl because `UnixStream` closes the fd automatically.
- `parse_frame_rest` and `encode_frame` are `pub(crate)` so the tokio I/O bridge in Task 5 can reuse them without duplicating the byte layout.
- The blocking `recv_from` / `send_to` methods exist only for the framing unit test in Step 4. The production path consumes the fd via `into_raw_fd()`.

- [ ] **Step 2: Write build.rs that compiles ghostbridge and links it**

```rust
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let ghostbridge_dir = PathBuf::from(&manifest_dir).join("../ghostbridge");

    // Build ghostbridge Go C archive
    let output = Command::new("make")
        .args(["-C", ghostbridge_dir.to_str().unwrap(), "archive"])
        .output()
        .expect("Failed to build ghostbridge. Is Go installed?");

    if !output.status.success() {
        panic!("ghostbridge build failed:\n{}", String::from_utf8_lossy(&output.stderr));
    }

    // Link against the generated archive
    let lib_dir = ghostbridge_dir;
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=ghostbridge");

    // Go c-archive needs -lpthread -lm on Linux
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=m");

    // Rerun if Go source changes
    println!("cargo:rerun-if-changed={}", ghostbridge_dir.join("main.go").display());
    println!("cargo:rerun-if-changed={}", ghostbridge_dir.join("go.mod").display());

    // Generate C header with cbindgen
    let cbindgen_config = cbindgen::Config::from_root_or_default(&manifest_dir);
    cbindgen::Builder::new()
        .with_config(cbindgen_config)
        .with_crate(manifest_dir.clone())
        .with_language(cbindgen::Language::C)
        .generate()
        .unwrap()
        .write_to_file(PathBuf::from(&manifest_dir).join("include/ghostframe.h"));
}
```

- [ ] **Step 3: Verify the library builds**

```bash
cargo build
```

Expected: ghostbridge Go archive compiles, Rust FFI links against it, ghostframe-lib builds successfully.

- [ ] **Step 4: Write a unit test for the framing protocol**

In `ghostframe-lib/src/transport/ghostbridge.rs`, add a `#[cfg(test)]` module that tests `write_framed_packet` / `read_framed_packet` round-trip using a Unix socketpair (not the real ghostbridge):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::io::{Read, Write};

    #[test]
    fn framing_round_trip() {
        let (mut a, mut b) = UnixStream::pair().unwrap();

        let payload = b"hello world";
        let addr: std::net::SocketAddr = "100.64.0.2:4443".parse().unwrap();

        // Write
        let mut frame = Vec::new();
        frame.extend_from_slice(&((4 + 4 + payload.len() + 2 + 9 + 1) as u32).to_be_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&4443u16.to_be_bytes());
        frame.extend_from_slice(b"100.64.0.2");
        frame.push(0);
        a.write_all(&frame).unwrap();
        a.flush().unwrap();

        // Read
        let mut header = [0u8; 8];
        b.read_exact(&mut header).unwrap();
        let total_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
        let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
        let mut rest = vec![0u8; total_len - 8];
        b.read_exact(&mut rest).unwrap();

        assert_eq!(&rest[..payload_len], payload);
    }
}
```

Run: `cargo test --lib`
Expected: test passes

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: ghostbridge Rust FFI bindings with framing protocol"
```

---

## Task 4: Quinn-Proto QUIC Endpoint Wrapper

**Files:**
- Modify: `ghostframe-lib/src/transport/quic.rs` (replace stub)

This task creates a Rust wrapper around `quinn-proto` that:
1. Creates a server `Endpoint` with TLS configuration (self-signed cert) and transport config (datagrams enabled)
2. Accepts incoming connections, handles connection events
3. Manages the state machine driving loop (handle incoming bytes, produce outgoing bytes, handle timeouts)

- [ ] **Step 1: Write the QuicServer wrapper**

```rust
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use bytes::BytesMut;
use quinn_proto::{
    Connection, ConnectionHandle, DatagramEvent, Endpoint, EndpointConfig,
    Event, ServerConfig, Transmit, TransportConfig,
};

/// Output of exporting the self-signed cert. Used by the xdaemon to print
/// `CERT_HASH_SHA256=<hex>` at startup so the E2E test and browser client can
/// pin it.
pub struct CertInfo {
    pub sha256_hex: String,
}

pub struct QuicServer {
    pub(crate) endpoint: Endpoint,
    pub(crate) connections: HashMap<ConnectionHandle, Connection>,
    pub(crate) cert_info: CertInfo,
}

impl QuicServer {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // Generate self-signed TLS certificate with localhost + 127.0.0.1 in SAN.
        // 127.0.0.1 is required so the E2E test's UDP forwarder (Task 9) can
        // present this cert to Chromium via certificate-hash pinning.
        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".into(),
            "127.0.0.1".into(),
        ])?;
        let cert_der = cert.cert.der().to_vec();
        // rcgen 0.14.x renamed CertifiedKey::key_pair to signing_key.
        let key_der = cert.signing_key.serialize_der();

        // Compute SHA-256 of the cert for browser certificateHashes pinning.
        let sha256_hex = {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(&cert_der);
            hex::encode(digest)
        };

        let cert_chain: Vec<rustls::pki_types::CertificateDer<'static>> =
            vec![rustls::pki_types::CertificateDer::from(cert_der)];
        let private_key = rustls::pki_types::PrivateKeyDer::try_from(key_der)
            .map_err(|e| format!("invalid private key: {e}"))?;

        // rustls 0.23 with default-features=false: we supply the crypto
        // provider explicitly instead of relying on install_default().
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(cert_chain, private_key)?;
        tls.alpn_protocols = vec![b"h3".to_vec()];

        let quic_tls = quinn_proto::crypto::rustls::QuicServerConfig::try_from(tls)?;

        let mut transport_config = TransportConfig::default();
        transport_config.datagram_receive_buffer_size(Some(65536));
        transport_config.datagram_send_buffer_size(65536);

        let mut server_config = ServerConfig::with_crypto(Arc::new(quic_tls));
        server_config.transport = Arc::new(transport_config);

        let endpoint = Endpoint::new(
            Arc::new(EndpointConfig::default()),
            Some(Arc::new(server_config)),
            /* allow_mtud */ true,
            /* rng_seed */ None,
        );

        Ok(Self {
            endpoint,
            connections: HashMap::new(),
            cert_info: CertInfo { sha256_hex },
        })
    }

    pub fn cert_info(&self) -> &CertInfo {
        &self.cert_info
    }

    // --- Method surface used by the I/O bridge in Task 5 ---
    // Bodies are filled in Task 5; Task 4 only commits to the signatures so
    // that quic.rs and io_bridge.rs can be developed in parallel.

    /// Feed an inbound framed UDP packet into the endpoint state machine.
    pub fn handle_datagram(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        local_ip: SocketAddr,
        data: &[u8],
        buf: &mut BytesMut,
    ) -> Option<DatagramEvent> {
        // Task 5: call self.endpoint.handle(...) and return the event.
        let _ = (now, remote, local_ip, data, buf);
        unimplemented!("Task 5")
    }

    /// Drain pending outbound transmits across all connections and the endpoint.
    pub fn poll_transmit(&mut self, now: Instant) -> Option<Transmit> {
        let _ = now;
        unimplemented!("Task 5")
    }

    /// Earliest timeout across all connections. Feeds the tokio timer in Task 5.
    pub fn next_timeout(&self) -> Option<Instant> {
        unimplemented!("Task 5")
    }

    /// Fire timeouts whose deadline has passed.
    pub fn handle_timeout(&mut self, now: Instant) {
        let _ = now;
        unimplemented!("Task 5")
    }

    /// Drain application-level events (new connection, stream data, datagram rx, ...).
    pub fn poll_events(&mut self) -> Option<(ConnectionHandle, Event)> {
        unimplemented!("Task 5")
    }
}
```

- [ ] **Step 1b: Add dependencies**

Add to `ghostframe-lib/Cargo.toml` (versions pinned in Task 0):
```toml
rcgen = { version = "0.13", default-features = false, features = ["pem", "ring"] }
rustls = { version = "0.23", default-features = false, features = ["ring", "std"] }
sha2 = "0.10"
hex = "0.4"
```

Note: exact `quinn-proto` API names and arg lists come from Task 0 Step 2. If Task 0 finds divergence from the snippet above, patch *this* task's snippet before writing code.

- [ ] **Step 2: Add rcgen and rustls dependencies**

Add to `ghostframe-lib/Cargo.toml`:
```toml
rcgen = "0.13"
rustls = { version = "0.23", features = ["ring"] }
quinn-proto = { workspace = true }
```

- [ ] **Step 3: Write unit test for QUIC endpoint creation**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_create_endpoint() {
        let server = QuicServer::new().expect("endpoint creation should succeed");
        // Verify endpoint was created
        assert!(!server.send_buf.is_empty());
    }
}
```

Run: `cargo test --lib`
Expected: test passes

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat: quinn-proto QUIC endpoint wrapper with TLS and datagram config"
```

---

## Task 5: I/O Bridge Event Loop

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs` (replace stub)

The I/O bridge is the core async event loop that connects ghostbridge (framed UDP packets from Tailscale) with quinn-proto (QUIC state machine). Built on tokio: the ghostbridge socketpair fd is wrapped in `tokio::net::UnixStream`, and the loop `tokio::select!`s between inbound frames and the earliest QUIC timeout.

```
select!:
  - read_exact(header) from UnixStream → parse frame → endpoint.handle → drain
  - sleep_until(next_timeout)         → handle_timeout               → drain
drain:
  - while let Some(t) = poll_transmit { write_all(encode_frame(t)) }
  - process app events (new conn, datagram rx, etc.) and update any session state
```

- [ ] **Step 1: Implement the I/O bridge event loop**

```rust
use std::future::{pending, Future};
use std::io;
use std::net::SocketAddr;
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::pin::Pin;
use std::time::Instant;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream as TokioUnixStream;
use tokio::time::{sleep_until, Instant as TokioInstant};

use crate::transport::ghostbridge::{
    encode_frame, parse_frame_rest, GhostbridgeConfig, GhostbridgeError, GhostbridgeHandle,
};
use crate::transport::quic::QuicServer;

pub struct IoBridge {
    _handle: GhostbridgeHandle, // keep alive for fd lifetime
    stream: TokioUnixStream,
    server: QuicServer,
    local_addr: SocketAddr,
}

impl IoBridge {
    pub async fn new(
        ghostbridge_config: &GhostbridgeConfig,
        listen_addr: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let handle = GhostbridgeHandle::connect(ghostbridge_config)?;
        handle.up()?;
        let ips = handle.get_ips()?;
        tracing::info!(?ips, "ghostbridge node joined tailnet");

        let udp = handle.listen_udp(listen_addr)?;
        let raw_fd = udp.into_raw_fd();

        // Convert to std::os::unix::net::UnixStream → tokio::net::UnixStream.
        // ghostbridge already set O_NONBLOCK on this fd.
        let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(raw_fd) };
        std_stream.set_nonblocking(true)?;
        let stream = TokioUnixStream::from_std(std_stream)?;

        let server = QuicServer::new()?;
        tracing::info!(cert_sha256 = %server.cert_info().sha256_hex, "QUIC server ready");

        let local_addr: SocketAddr = "0.0.0.0:4443".parse().unwrap();
        Ok(Self { _handle: handle, stream, server, local_addr })
    }

    /// Run the event loop until the ghostbridge fd closes.
    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut header = [0u8; 8];

        loop {
            let sleep_fut: Pin<Box<dyn Future<Output = ()> + Send>> =
                match self.server.next_timeout() {
                    Some(deadline) => {
                        Box::pin(sleep_until(TokioInstant::from_std(deadline)))
                    }
                    None => Box::pin(pending::<()>()),
                };

            tokio::select! {
                biased;

                // 1. Earliest QUIC timeout
                _ = sleep_fut => {
                    self.server.handle_timeout(Instant::now());
                }

                // 2. Inbound framed UDP packet
                read_res = self.stream.read_exact(&mut header) => {
                    match read_res {
                        Ok(_) => {
                            if let Err(e) = self.process_inbound(&header).await {
                                tracing::warn!("inbound frame error: {e}");
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                            tracing::info!("ghostbridge fd closed");
                            return Ok(());
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
            }

            // Always drain after either branch.
            self.drain_outbound().await?;
            self.drain_app_events();
        }
    }

    async fn process_inbound(&mut self, header: &[u8; 8]) -> io::Result<()> {
        let total_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
        let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
        if total_len < 8 + payload_len + 3 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too short"));
        }
        let mut rest = vec![0u8; total_len - 8];
        self.stream.read_exact(&mut rest).await?;
        let packet = parse_frame_rest(&rest, payload_len)?;

        let mut scratch = BytesMut::with_capacity(65535);
        let _event = self.server.handle_datagram(
            Instant::now(),
            packet.addr,
            self.local_addr,
            &packet.payload,
            &mut scratch,
        );
        // Endpoint::handle may return an immediate Response (e.g. retry, version
        // negotiation) — if so, `scratch` contains bytes to send to `packet.addr`.
        if !scratch.is_empty() {
            let frame = encode_frame(&scratch, &packet.addr);
            self.stream.write_all(&frame).await?;
        }
        Ok(())
    }

    async fn drain_outbound(&mut self) -> io::Result<()> {
        while let Some(transmit) = self.server.poll_transmit(Instant::now()) {
            let frame = encode_frame(&transmit.contents, &transmit.destination);
            self.stream.write_all(&frame).await?;
        }
        Ok(())
    }

    fn drain_app_events(&mut self) {
        while let Some((handle, event)) = self.server.poll_events() {
            tracing::trace!(?handle, ?event, "app event");
            // Task 6 / Task 8 hook WebTransport handshake + ping/pong logic here.
        }
    }
}
```

- [ ] **Step 2: Unit test the select-loop shutdown path**

Use `tokio::test` + a `UnixStream::pair()` to drive `process_inbound` with a synthetic frame and verify that `drain_outbound` writes a response. The framing round-trip test from Task 3 already covers `parse_frame_rest` / `encode_frame`.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: tokio-based I/O bridge event loop connecting ghostbridge to quinn-proto"
```

---

## Task 6: Minimal WebTransport Handshake

**Files:**
- Modify: `ghostframe-lib/src/transport/webtransport.rs` (replace stub)
- Add: `ghostframe-lib/Cargo.toml` dependency on `web-transport-proto`

This task implements the minimum HTTP/3 + WebTransport handshake needed for a browser to connect:
1. Accept QUIC connection
2. Exchange HTTP/3 SETTINGS (enable datagrams, enable CONNECT protocol, enable WebTransport)
3. Handle QPACK encoder/decoder streams
4. Accept WebTransport CONNECT request on a bidirectional stream
5. Send 200 OK response
6. Exchange datagrams

The `web-transport-proto` crate provides types for SETTINGS, CONNECT request/response, and VarInt encoding, but assumes an async stream I/O model. Task 0 Step 5 determines whether its encode/decode primitives are buffer-based (in which case we use them directly) or whether we need to fork the relevant encoding logic into `webtransport.rs`. **Budget ~1 day for this adaptation regardless** — QPACK encoding for the `:status`, `:method`, `:protocol`, `:path`, `:authority` headers is the part most likely to need hand-rolling.

- [ ] **Step 1: Add web-transport-proto dependency**

Add to `ghostframe-lib/Cargo.toml`:
```toml
web-transport-proto = "0.6"
```

- [ ] **Step 2: Implement WebTransport handshake over quinn-proto**

```rust
use quinn_proto::{Connection, StreamId, Dir, Event, VarInt};
use bytes::Bytes;

pub struct WebTransportSession {
    pub session_id: u64,
    pub connected: bool,
}

pub struct WebTransportServer {
    settings_sent: bool,
    control_stream_id: Option<StreamId>,
}

impl WebTransportServer {
    pub fn new() -> Self {
        Self {
            settings_sent: false,
            control_stream_id: None,
        }
    }

    /// After accepting a QUIC connection, send HTTP/3 SETTINGS on a
    /// unidirectional stream (stream type 0x00).
    /// Then accept the client's SETTINGS stream and validate.
    ///
    /// SETTINGS to send:
    ///   SETTINGS_ENABLE_CONNECT_PROTOCOL (0x08) = 1
    ///   SETTINGS_H3_DATAGRAM (0x33) = 1
    ///   SETTINGS_H3_DATAGRAM_DEPRECATED (0xFFD277) = 1 (Chrome compat)
    ///   SETTINGS_WEBTRANSPORT_MAX_SESSIONS (0xC671706A) = 1
    pub fn send_settings(&mut self, conn: &mut Connection) -> Result<(), String> {
        // 1. Open a unidirectional stream (type 0x00 = control stream)
        let stream_id = conn.streams().open(Dir::Uni)
            .ok_or("cannot open uni stream")?;

        // 2. Write stream type 0x00 (HTTP/3 control stream)
        // 3. Write SETTINGS frame with the required parameters
        // ... (implementation uses web-transport-proto types for encoding)

        self.settings_sent = true;
        self.control_stream_id = Some(stream_id);
        Ok(())
    }

    /// Accept a WebTransport CONNECT request from the client.
    /// Reads the CONNECT headers from a bidirectional stream,
    /// validates :protocol = webtransport, and responds with 200.
    pub fn accept_connect(
        &mut self,
        conn: &mut Connection,
        stream_id: StreamId,
    ) -> Result<WebTransportSession, String> {
        // Read HEADERS frame from the bidirectional stream
        // Decode QPACK-encoded headers
        // Verify :method = CONNECT, :protocol = webtransport
        // Send HEADERS response with :status = 200

        // The session ID is the stream ID of this CONNECT stream
        Ok(WebTransportSession {
            session_id: stream_id.into(),
            connected: true,
        })
    }

    /// Receive a WebTransport datagram from the connection.
    /// Strips the quarter_stream_id prefix (RFC 9297).
    pub fn recv_datagram(conn: &mut Connection) -> Option<Vec<u8>> {
        let data = conn.datagrams().recv()?;
        if data.len() < 4 {
            return None;
        }
        // Quarter stream ID is a VarInt prefix
        // For M0, we just return the datagram payload after the prefix
        // ... implementation
        Some(data.to_vec())
    }

    /// Send a WebTransport datagram.
    /// Prepends the quarter_stream_id prefix.
    pub fn send_datagram(
        conn: &mut Connection,
        session: &WebTransportSession,
        data: &[u8],
    ) -> Result<(), String> {
        let quarter_stream_id = (session.session_id / 4) as u64;
        // Encode: VarInt(quarter_stream_id) + data
        // ... implementation
        conn.datagrams().send(Bytes::copy_from_slice(&framed_data), false)
            .map_err(|e| format!("datagram send error: {:?}", e))
    }
}
```

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: minimal WebTransport handshake over quinn-proto"
```

---

## Task 7: Browser Client — Ping/Pong

**Files:**
- Create: `ghostframe-web-client/package.json`
- Create: `ghostframe-web-client/tsconfig.json`
- Create: `ghostframe-web-client/vite.config.ts`
- Create: `ghostframe-web-client/index.html`
- Create: `ghostframe-web-client/src/main.ts`

The browser client opens a WebTransport session to the server and exchanges ping/pong datagrams.

- [ ] **Step 1: Create package.json**

```json
{
  "name": "ghostframe-web-client",
  "version": "0.1.0",
  "private": true,
  "scripts": {
    "dev": "vite",
    "build": "tsc && vite build",
    "preview": "vite preview"
  },
  "dependencies": {},
  "devDependencies": {
    "typescript": "^5.5",
    "vite": "^6.0"
  }
}
```

- [ ] **Step 2: Create tsconfig.json**

```json
{
  "compilerOptions": {
    "target": "ES2022",
    "module": "ESNext",
    "moduleResolution": "bundler",
    "strict": true,
    "lib": ["ES2022", "DOM", "DOM.AsyncIterable", "WebWorker"]
  },
  "include": ["src"]
}
```

- [ ] **Step 3: Create vite.config.ts**

```typescript
import { defineConfig } from 'vite';

export default defineConfig({
  server: {
    host: true,
  },
});
```

- [ ] **Step 4: Create index.html**

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <title>Ghostframe M0</title>
</head>
<body>
  <h1>Ghostframe M0 — Ping/Pong</h1>
  <div id="status">Connecting...</div>
  <div id="log"></div>
  <script type="module" src="/src/main.ts"></script>
</body>
</html>
```

- [ ] **Step 5: Create src/main.ts**

```typescript
const statusEl = document.getElementById('status')!;
const logEl = document.getElementById('log')!;

function log(msg: string) {
  const line = document.createElement('div');
  line.textContent = msg;
  logEl.appendChild(line);
}

async function main() {
  // The server's Tailscale hostname and the ephemeral cert hash
  // will be provided at runtime via URL params or config
  const url = new URL(window.location.href);
  const serverHost = url.searchParams.get('host') || 'ghostframe-server';
  const certHash = url.searchParams.get('certHash') || '';

  const wtUrl = `https://${serverHost}:4443/`;

  log(`Connecting to ${wtUrl}...`);

  let transport: WebTransport;
  if (certHash) {
    transport = new WebTransport(wtUrl, {
      serverCertificateHashes: [{ algorithm: 'sha-256', value: hexToBuffer(certHash) }],
    });
  } else {
    transport = new WebTransport(wtUrl);
  }

  await transport.ready;
  log('Connected!');
  statusEl.textContent = 'Connected';

  // Send ping datagram
  const pingData = new Uint8Array([0x70, 0x69, 0x6E, 0x67]); // "ping"
  transport.sendDatagram(pingData);
  log('Sent: ping');

  // Receive datagrams
  const reader = transport.datagrams.readable.getReader();
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;

    const text = new TextDecoder().decode(value);
    log(`Received: ${text} (${value.byteLength} bytes)`);

    if (text === 'pong') {
      statusEl.textContent = 'Ping/Pong successful!';
      log('M0 COMPLETE: Ping/Pong datagram round-trip verified!');
    }
  }
}

function hexToBuffer(hex: string): ArrayBuffer {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < hex.length; i += 2) {
    bytes[i / 2] = parseInt(hex.substr(i, 2), 16);
  }
  return bytes.buffer;
}

main().catch((e) => {
  log(`Error: ${e.message}`);
  statusEl.textContent = `Error: ${e.message}`;
});
```

- [ ] **Step 6: Install dependencies and verify build**

```bash
cd ghostframe-web-client
npm install
npm run build
```

Expected: TypeScript compiles, Vite build produces dist/ directory.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat: browser client with WebTransport ping/pong"
```

---

## Task 8: XDaemon Ping Server

**Files:**
- Modify: `ghostframe-xdaemon/src/main.rs`

Wire up the full server: create Ghostbridge handle → connect to tailnet → listen for UDP → create I/O bridge → accept QUIC connections → WebTransport handshake → echo "ping" datagrams as "pong".

- [ ] **Step 1: Implement the ping server main**

```rust
use ghostframe_lib::transport::ghostbridge::GhostbridgeConfig;
use ghostframe_lib::transport::io_bridge::IoBridge;
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ghostframe=debug,info".into()),
        )
        .init();

    let authkey = env::var("TS_AUTHKEY").expect("TS_AUTHKEY must be set");
    let hostname = env::var("TS_HOSTNAME").unwrap_or_else(|_| "ghostframe-server".into());
    let state_dir = env::var("TS_STATE_DIR").unwrap_or_else(|_| "/tmp/ghostframe-ts".into());
    let control_url = env::var("TS_CONTROL_URL").unwrap_or_default();

    let config = GhostbridgeConfig { hostname, authkey, state_dir, control_url };

    tracing::info!("Connecting to Tailscale...");
    let mut bridge = IoBridge::new(&config, ":4443").await?;

    // Emit the cert hash on a dedicated line so the test harness and container
    // entrypoint (Task 9) can capture it with a simple regex.
    // Format is machine-parseable: `CERT_HASH_SHA256=<lowercase hex>`
    println!("CERT_HASH_SHA256={}", bridge.cert_hash_sha256());

    tracing::info!("I/O bridge created, entering event loop...");
    bridge.run().await?;
    Ok(())
}
```

Add to `ghostframe-xdaemon/Cargo.toml`:
```toml
tokio = { workspace = true, features = ["rt-multi-thread", "macros"] }
```

- [ ] **Step 2: Expose the cert hash on the I/O bridge and plumb ping→pong (PSEUDOCODE)**

Add a thin accessor on `IoBridge` that forwards to `QuicServer::cert_info().sha256_hex`:

```rust
impl IoBridge {
    pub fn cert_hash_sha256(&self) -> &str {
        &self.server.cert_info().sha256_hex
    }
}
```

The ping→pong handler lives inside `drain_app_events` (Task 5). The snippet below is **pseudocode** — the actual integration point depends on how the WebTransport session state from Task 6 is exposed on `QuicServer`. It does not compile as-is:

```rust
// PSEUDOCODE — in IoBridge::drain_app_events, after processing events:
//
// for (conn_handle, session) in self.server.active_wt_sessions_mut() {
//     while let Some(data) = session.recv_datagram() {
//         if data.as_ref() == PING_PAYLOAD {
//             tracing::info!("received ping, sending pong");
//             session.send_datagram(PONG_PAYLOAD)?;
//         }
//     }
// }
```

During execution, the implementing agent decides whether `active_wt_sessions_mut()` belongs on `QuicServer` or a separate `WebTransportLayer` wrapper. Either is fine; the contract is "pull datagrams out, push pong back in."

- [ ] **Step 3: Build and verify**

```bash
cargo build --release
```

Expected: binary compiles and links against ghostbridge.a

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat: xdaemon ping server with ghostbridge I/O bridge"
```

---

## Task 9: E2E Test Infrastructure (embedded tsnet)

**Files:**
- Create: `tests/e2e/mod.rs`
- Create: `tests/e2e/helpers.rs`
- Create: `tests/e2e/ping.rs`
- Create: `tests/containers/headscale/config/config.yaml`
- Create: `tests/containers/test-server/Dockerfile`
- Create: `tests/containers/test-server/entrypoint.sh`
- Add: test dependencies to workspace `Cargo.toml`

**Architecture — hermetic E2E with embedded tsnet:**

The test process itself joins the test headscale tailnet by creating its **own** `GhostbridgeHandle` (a second tsnet node, completely separate from the server container's node). It then runs a local UDP forwarder that proxies packets between a loopback address on the test machine and the server's address on the tailnet. The browser is launched via `chromiumoxide` and connects to `https://127.0.0.1:<port>` with the server's cert hash pinned via `serverCertificateHashes`. No host Tailscale installation is needed.

```
┌──────────────────────── host machine ────────────────────────┐
│                                                              │
│  ┌────────┐      ┌──────────────── cargo test ───────────┐   │
│  │Chromium│      │                                      │   │
│  │(oxide) │◄────►│ 127.0.0.1:N  UDP forwarder  ┐        │   │
│  └────────┘      │                             │        │   │
│                  │     GhostbridgeHandle       │        │   │
│                  │     (test tsnet node)◄──────┘        │   │
│                  └──────────┬───────────────────────────┘   │
│                             │ tailnet                         │
│                             ▼                                 │
│              ┌─────── docker network ───────┐                │
│              │  headscale     ghostframe-   │                │
│              │                server        │                │
│              └──────────────────────────────┘                │
└──────────────────────────────────────────────────────────────┘
```

Why a forwarder instead of connecting directly: WebTransport is UDP/QUIC, and Chromium's address resolution + cert verification is pinned to the URL's authority. By proxying through 127.0.0.1, we reuse the cert's `127.0.0.1` SAN (Task 4) and avoid custom DNS. The forwarder is ~60 lines of tokio.

Why this is "future-proof" (Q3:C answer): the M1+ tests will exercise multi-peer flows where the test process itself acts as a client peer. Owning a real tsnet node in the test harness is exactly that primitive, so we build it once.

- [ ] **Step 1: Add E2E test dependencies**

Add to `ghostframe-lib/Cargo.toml` (the test target lives in the workspace root `tests/` so dependencies go on a `tests`-only integration crate; simpler: put them on `ghostframe-lib` dev-deps since the test imports from it):

```toml
[dev-dependencies]
testcontainers = "0.27"   # latest stable
chromiumoxide = "0.9"     # no separate tokio feature flag; tokio is bundled
serde_json = "1"
anyhow = "1"
axum = "0.7"
tower-http = { version = "0.6", features = ["fs"] }
tokio = { workspace = true, features = ["process", "time", "rt-multi-thread", "macros"] }
```

`axum` + `tower-http::services::ServeDir` serves the built `ghostframe-web-client/dist/` directory over `http://127.0.0.1:<port>/`. Chromium treats `http://127.0.0.1` as a secure context, so `WebTransport` is permitted — unlike `file://`, which may not qualify.

Rationale for `chromiumoxide` over `playwright-rs`: pure Rust + CDP, no Node/npm dependency in CI, and it talks to a `chromium` binary already on the host (or downloaded by `chromiumoxide_fetcher`).

- [ ] **Step 2: Create headscale config**

```yaml
# tests/containers/headscale/config/config.yaml
server_url: http://headscale:8080
listen_addr: 0.0.0.0:8080
grpc_listen_addr: 0.0.0.0:50443
grpc_allow_insecure: true
noise:
  private_key_path: /var/lib/headscale/noise_private.key
prefixes:
  v4: 100.64.0.0/10
  v6: fd7a:115c:a1e0::/48
database:
  type: sqlite
  sqlite:
    path: /var/lib/headscale/db.sqlite
dns:
  magic_dns: true
  base_domain: test.tailnet
log:
  level: info
```

- [ ] **Step 3: Create test-server Dockerfile**

```dockerfile
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && curl -fsSL https://tailscale.com/install.sh | sh \
    && rm -rf /var/lib/apt/lists/*

COPY target/release/ghostframe-xdaemon /usr/local/bin/ghostframe-xdaemon
COPY tests/containers/test-server/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
```

- [ ] **Step 4: Create test-server entrypoint.sh**

The xdaemon uses its own embedded tsnet node (via ghostbridge); there is no need to run a separate `tailscaled` inside the container. The entrypoint just waits for headscale to be reachable, sets the env vars, and execs the daemon. We do not tee stdout to a file — the test reads `CERT_HASH_SHA256=` directly from `docker logs`.

```bash
#!/bin/bash
set -euo pipefail

until curl -fsS http://headscale:8080/health >/dev/null 2>&1; do
    echo "waiting for headscale..."
    sleep 1
done

export TS_HOSTNAME=${TS_HOSTNAME:-ghostframe-server}
export TS_STATE_DIR=${TS_STATE_DIR:-/tmp/ghostframe-ts}
export TS_CONTROL_URL=${TS_CONTROL_URL:-http://headscale:8080}

exec /usr/local/bin/ghostframe-xdaemon
```

- [ ] **Step 5: Create tests/e2e/helpers.rs**

Helper contract:
1. `start_headscale()` — spins up headscale container on a shared Docker network.
2. `create_preauth_key(container_name, user)` — shells out to `docker exec` to run `headscale users create` then `headscale preauthkeys create --output json`, parses the `key` field.
3. `start_server_container(authkey)` — launches `ghostframe/test-server` with the preauth key env var.
4. `read_cert_hash_from_logs(container)` — tails the container logs with a regex until it sees `CERT_HASH_SHA256=<hex>`; returns the hex string. Times out after 60s.
5. `TestNode` — owns a `GhostbridgeHandle` in the host process that joined the same headscale with its own preauth key.
6. `UdpForwarder` — tokio task that copies packets between a local `tokio::net::UdpSocket` bound on `127.0.0.1:0` and the `UdpPacketConn` returned by `TestNode::dial_udp("ghostframe-server:4443")`.

```rust
use std::net::SocketAddr;
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use ghostframe_lib::transport::ghostbridge::{
    encode_frame, parse_frame_rest, GhostbridgeConfig, GhostbridgeHandle, UdpPacketConn,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UdpSocket, UnixStream as TokioUnixStream};

pub const NETWORK_NAME: &str = "ghostframe-e2e";

pub async fn create_preauth_key(container_name: &str, user: &str) -> Result<String> {
    // Idempotent: ignore "user already exists" error.
    let _ = tokio::process::Command::new("docker")
        .args(["exec", container_name, "headscale", "users", "create", user])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;

    let out = tokio::process::Command::new("docker")
        .args([
            "exec", container_name, "headscale",
            "preauthkeys", "create",
            "--user", user,
            "--reusable",
            "--expiration", "1h",
            "--output", "json",
        ])
        .output()
        .await
        .context("running headscale preauthkeys create")?;
    if !out.status.success() {
        return Err(anyhow!(
            "preauthkeys create failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    v.get("key")
        .and_then(|k| k.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("no `key` field in preauthkey JSON: {}", String::from_utf8_lossy(&out.stdout)))
}

pub async fn read_cert_hash_from_logs(container_name: &str) -> Result<String> {
    let mut child = tokio::process::Command::new("docker")
        .args(["logs", "-f", container_name])
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let line = tokio::time::timeout(remaining, lines.next_line()).await??;
        let Some(line) = line else { break };
        if let Some(rest) = line.strip_prefix("CERT_HASH_SHA256=") {
            let hash = rest.trim().to_string();
            child.kill().await.ok();
            return Ok(hash);
        }
    }
    Err(anyhow!("cert hash not seen in container logs within 60s"))
}

/// A tsnet node running inside the test process. Joins the test headscale.
pub struct TestNode {
    handle: GhostbridgeHandle,
}

impl TestNode {
    pub async fn join(authkey: String, control_url: String) -> Result<Self> {
        let handle = GhostbridgeHandle::connect(&GhostbridgeConfig {
            hostname: "e2e-test-client".into(),
            authkey,
            state_dir: format!("/tmp/ghostframe-e2e-client-{}", std::process::id()),
            control_url,
        })?;
        handle.up()?;
        Ok(Self { handle })
    }

    pub fn dial(&self, remote: &str) -> Result<UdpPacketConn> {
        Ok(self.handle.dial_udp(remote)?)
    }
}

/// Proxies between a local loopback UDP socket (what Chromium sees) and a
/// `UdpPacketConn` that goes out over tsnet to the server container.
///
/// Returns the bound `SocketAddr` that the browser should connect to.
pub async fn start_forwarder(local_bind: &str, upstream: UdpPacketConn) -> Result<SocketAddr> {
    let sock = UdpSocket::bind(local_bind).await?;
    let local = sock.local_addr()?;

    // Wrap the upstream UdpPacketConn fd in tokio for async I/O.
    let raw_fd = upstream.into_raw_fd();
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(raw_fd) };
    std_stream.set_nonblocking(true)?;
    let mut upstream = TokioUnixStream::from_std(std_stream)?;

    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        let mut header = [0u8; 8];
        let mut client: Option<SocketAddr> = None;

        loop {
            tokio::select! {
                // Browser → tsnet
                res = sock.recv_from(&mut buf) => {
                    let Ok((n, from)) = res else { return };
                    client = Some(from);
                    // Destination address is whatever the server sees — not our concern
                    // because the far side of the socketpair is a *dialed* PacketConn
                    // (ghostbridge gbridge_dial_udp), so all writes go to the dialed peer.
                    // We still must frame with *some* addr; use the from addr as a placeholder.
                    let frame = encode_frame(&buf[..n], &from);
                    if upstream.write_all(&frame).await.is_err() { return; }
                }
                // tsnet → browser
                res = upstream.read_exact(&mut header) => {
                    if res.is_err() { return }
                    let total_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
                    let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
                    let mut rest = vec![0u8; total_len - 8];
                    if upstream.read_exact(&mut rest).await.is_err() { return }
                    let Ok(pkt) = parse_frame_rest(&rest, payload_len) else { continue };
                    if let Some(dst) = client {
                        let _ = sock.send_to(&pkt.payload, dst).await;
                    }
                }
            }
        }
    });

    Ok(local)
}
```

### Static file server

```rust
use std::net::SocketAddr;
use std::path::Path;

use axum::Router;
use tower_http::services::ServeDir;

/// Serve a directory over HTTP on 127.0.0.1:<random>. Returns the bound address.
/// Used so Chromium loads the page from a secure-context origin (http://127.0.0.1),
/// which is required for WebTransport. `file://` origins are not a secure context.
pub async fn start_static_server(dir: impl AsRef<Path>) -> anyhow::Result<SocketAddr> {
    let app = Router::new().fallback_service(ServeDir::new(dir.as_ref()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(addr)
}
```

**Note on dialed PacketConn addressing:** `gbridge_dial_udp` wraps a connected `net.Conn`, so the Go side ignores the destination in each frame (reads from the dialed peer, writes to the dialed peer). This simplifies the forwarder to a two-way copy — no NAT table needed. The forwarder only tracks which loopback address the browser uses, so pong datagrams can be sent back to the right source.

- [ ] **Step 6: Create tests/e2e/ping.rs**

Flow (all inside one `#[tokio::test]`):
1. Start headscale via testcontainers on the `ghostframe-e2e` Docker network.
2. Call `create_preauth_key("headscale", "testuser")` → server key.
3. Call `create_preauth_key("headscale", "testclient")` → test-node key.
4. Start `ghostframe/test-server` container with the server key.
5. `read_cert_hash_from_logs("ghostframe-server")` → cert hash hex.
6. `TestNode::join(client_key, "http://127.0.0.1:<headscale-host-port>")` inside the test process.
7. `let upstream = test_node.dial("ghostframe-server:4443")?;`
8. `let local = start_forwarder("127.0.0.1:0", upstream).await?;`
9. `let static_addr = start_static_server("ghostframe-web-client/dist").await?;` — axum + `ServeDir` on `127.0.0.1:0`.
10. Launch Chromium via `chromiumoxide`.
11. Navigate to `http://<static_addr>/index.html?host=<forwarder>&certHash=<hex>`.
12. Wait for `#status` text to contain `Ping/Pong successful` (poll every 100ms, 30s timeout).
13. Assert.

```rust
use std::time::Duration;

use anyhow::Result;
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{runners::AsyncRunner, GenericImage, ImageExt};

mod helpers;
use helpers::{
    create_preauth_key, read_cert_hash_from_logs, start_forwarder, start_static_server, TestNode,
};

#[tokio::test]
async fn e2e_quic_ping_pong_over_tailscale() -> Result<()> {
    let _headscale = GenericImage::new("headscale/headscale", "0.28.0")
        .with_container_name("headscale")
        .with_network(helpers::NETWORK_NAME)
        .with_exposed_port(8080.tcp())
        // testcontainers 0.27: method is `with_ready_conditions`, not `with_wait_for`
        .with_ready_conditions(vec![WaitFor::message_on_stderr("listening")])
        .start()
        .await?;

    let server_key = create_preauth_key("headscale", "server").await?;
    let client_key = create_preauth_key("headscale", "client").await?;

    let _server = GenericImage::new("ghostframe/test-server", "latest")
        .with_container_name("ghostframe-server")
        .with_network(helpers::NETWORK_NAME)
        .with_env_var("TS_AUTHKEY", &server_key)
        .with_env_var("TS_CONTROL_URL", "http://headscale:8080")
        .with_ready_conditions(vec![WaitFor::message_on_stdout("CERT_HASH_SHA256=")])
        .start()
        .await?;

    let cert_hash = read_cert_hash_from_logs("ghostframe-server").await?;

    // headscale is reachable from the host via the mapped port.
    let headscale_host_port = _headscale.get_host_port_ipv4(8080).await?;
    let control_url = format!("http://127.0.0.1:{headscale_host_port}");

    let test_node = TestNode::join(client_key, control_url).await?;
    let upstream = test_node.dial("ghostframe-server:4443")?;
    let forwarder = start_forwarder("127.0.0.1:0", upstream).await?;

    // Serve ghostframe-web-client/dist over http://127.0.0.1:<port>.
    // Must be HTTP on a loopback address so Chromium treats it as a secure
    // context; WebTransport is not allowed from file:// origins.
    let static_addr = start_static_server("ghostframe-web-client/dist").await?;

    let (browser, mut handler) = Browser::launch(
        BrowserConfig::builder().build().unwrap(),
    ).await?;
    let _handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page_url = format!(
        "http://{}/index.html?host={}:{}&certHash={}",
        static_addr,
        forwarder.ip(),
        forwarder.port(),
        cert_hash,
    );

    let page = browser.new_page(&page_url).await?;

    // Poll for success, 30s timeout.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status: String = page
            .evaluate("document.getElementById('status').textContent")
            .await?
            .into_value()?;
        if status.contains("Ping/Pong successful") {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for pong. last status: {status}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
```

**Notes:**
- The web client's `main.ts` (Task 7) already reads `host` and `certHash` from URL params and builds the WebTransport URL as `https://${host}/`.
- Chromium treats `http://127.0.0.1` as a secure context, so WebTransport is permitted from the page served by the in-test axum server.

- [ ] **Step 7: Verify the test infrastructure compiles**

```bash
cargo test --test e2e --no-run
```

Expected: test code compiles. The test itself requires Docker running to pass.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat: E2E test infrastructure with headscale, testcontainers, chromiumoxide, embedded tsnet"
```

---

## Task 10: Integration Smoke Test — Local Manual

Before the automated E2E test is fully working, do a manual integration test:

1. Start a local Tailscale network (or use headscale locally)
2. Run `ghostframe-xdaemon` with `TS_AUTHKEY=<key>`
3. Open the browser client with the server's Tailscale hostname
4. Verify "pong" appears in the browser console

This task is a manual verification step, not an automated test.

- [ ] **Step 1: Build the server binary**

```bash
cargo build --release
```

- [ ] **Step 2: Set up a local Tailscale auth key and run the server**

```bash
export TS_AUTHKEY=tskey-auth-xxx
export TS_HOSTNAME=ghostframe-test
export TS_STATE_DIR=/tmp/ghostframe-ts
./target/release/ghostframe-xdaemon
```

- [ ] **Step 3: In a browser, navigate to the WebTransport URL**

Open `https://ghostframe-test.<tailnet-name>.ts.net:4443/?certHash=<sha256-hash>`

- [ ] **Step 4: Verify the browser console shows "Received: pong"**

- [ ] **Step 5: Document any issues found and create follow-up tasks**

---

## Task 11: Automated E2E Test — Docker Compose

Refine the E2E test to run fully automated. The browser runs on the host via chromiumoxide, the test process joins the test tailnet through an embedded ghostbridge node, and a loopback UDP forwarder proxies the browser's QUIC traffic into the tailnet.

- [ ] **Step 1: Create a Justfile target for building containers**

```just
containers-build:
    docker build -t ghostframe/test-server tests/containers/test-server/
```

- [ ] **Step 2: Create a Docker Compose file for local E2E testing**

```yaml
# tests/containers/docker-compose.yml
version: '3'
services:
  headscale:
    image: headscale/headscale:0.28.0
    container_name: headscale
    volumes:
      - ./headscale/config:/etc/headscale
      - headscale-data:/var/lib/headscale
    ports:
      - "8080:8080"
    command: serve

  test-server:
    build:
      context: ../..
      dockerfile: tests/containers/test-server/Dockerfile
    container_name: ghostframe-server
    depends_on:
      - headscale
    environment:
      - TS_AUTHKEY=${SERVER_AUTHKEY}
      - TS_CONTROL_URL=http://headscale:8080
    privileged: true

volumes:
  headscale-data:
```

- [ ] **Step 3: Run the full E2E test**

```bash
just containers-build
cargo test --test e2e
```

The test requires:
- Docker running (for the headscale + test-server containers)
- A `chromium` or `google-chrome` binary on `$PATH` (or let `chromiumoxide_fetcher` download one)
- The `ghostframe-web-client/dist/` directory built (Task 7)

No host Tailscale or Node.js needed — the test process joins the tailnet via an embedded ghostbridge node, and `chromiumoxide` talks CDP directly.

- [ ] **Step 4: Debug and fix any issues found**

This is an iterative step. Common issues:
- Tailscale authentication timing out
- QUIC/TLS handshake failures
- WebTransport ALPN negotiation
- Datagram framing issues

- [ ] **Step 5: Commit working E2E test**

```bash
git add -A
git commit -m "feat: automated E2E test passing — QUIC ping/pong over Tailscale"
```

---

## Self-Review Checklist

- [ ] **Spec coverage:** Can you point to a task that implements each M0 requirement?
  - Ghostbridge (Go C archive with raw UDP): Task 2, 3
  - Quinn-proto I/O bridge: Task 4, 5
  - WebTransport handshake: Task 6
  - Browser ping/pong client: Task 7
  - XDaemon ping server: Task 8
  - E2E test infrastructure: Task 9, 10, 11
- [ ] **Placeholder scan:** Search for "TBD", "TODO", "implement later", "fill in details", "add appropriate" — fix any found
- [ ] **Type consistency:** Verify function names and signatures are consistent across tasks
- [ ] **Build order:** Tasks 1-8 are sequential; Task 9+ can follow Task 8