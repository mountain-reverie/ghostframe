# Tailnet-Served Web Client Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Collapse first-connection UX to a single URL by serving the SPA + cert hash directly from `ghostbridge` over tsnet `:443` with Tailscale-issued LE HTTPS. QUIC moves to `:443` alongside the HTTPS TCP listener.

**Architecture:** A new Go-side HTTP server in `ghostbridge` embeds `ghostframe-web-client/dist/` via `//go:embed` and serves it through `tsnet.Server.ListenTLS` (production) or a static-PEM TLS listener (e2e). One new FFI export (`gbridge_start_web_server`) is called from Rust after the WebTransport cert is generated. The web client bootstrap stops parsing URL params and `fetch`es `/config.json` instead. E2E substitutes only the cert source, exercising the production Go server end-to-end.

**Tech Stack:** Go (`tsnet`, `net/http`, `embed`, `crypto/tls`), Rust (FFI to ghostbridge), TypeScript (vite + vitest), `axum`/`tower-http` (e2e static-asset stub still used for harness diagnostics), `chromiumoxide` (e2e browser driver).

**Spec:** `docs/superpowers/specs/2026-06-09-tailnet-served-web-client-design.md`.

**Predecessors:** none — independent of M3 codec suite.

---

## File Structure

**Created**
- `ghostbridge/web_server.go` — new file; HTTP handlers, embed.FS, `ListenTLS` wiring.
- `ghostbridge/web_server_test.go` — Go unit tests for handlers and redirect.
- `ghostframe-web-client/src/bootstrap.ts` — extracted bootstrap (`fetch('/config.json')` → `WebTransport` construction).
- `ghostframe-web-client/tests/bootstrap.test.ts` — vitest unit tests for `bootstrap.ts`.
- `ghostframe-e2e/src/harness/e2e_certs.rs` — TLS cert+key+SPKI generator for the e2e harness.

**Modified**
- `ghostbridge/main.go` — new export `gbridge_start_web_server`; new status codes.
- `ghostbridge/Makefile` — guard against missing `dist/`.
- `ghostframe-lib/src/transport/ghostbridge.rs` — extern decl + `start_web_server` method on `GhostbridgeHandle`.
- `ghostframe-lib/src/server.rs` — call `start_web_server` after cert generation; thread cert hash through.
- `ghostframe-lib/src/transport/quic.rs` — comment update (port literal not used here; `:443` lives in xdaemon).
- `ghostframe-lib/src/ffi.rs` — port literal `:4443` → `:443` (line 55).
- `ghostframe-xdaemon/src/main.rs` — port literal `:4443` → `:443`; remove `println!("CERT_HASH_SHA256=…")`.
- `ghostframe-web-client/src/main.ts` — replace URL-param block with `bootstrap()` call.
- `ghostframe-e2e/src/harness/transport.rs` — add `start_tcp_forwarder`.
- `ghostframe-e2e/src/harness/mod.rs` — re-export new helpers.
- `ghostframe-e2e/src/harness/scene.rs` — generate cert, inject env vars, both forwarders, `https://…` page URL.
- `ghostframe-e2e/tests/e2e.rs` — three inline test bodies: same migration applied.
- `Justfile` — rename `web-client-build` → `build-web`; add `build` recipe; update `test-e2e` chain.
- `README.md` — rewrite "First connection" section.
- `packaging/install.sh` — drop manual cert-hash and Python-server instructions; add HTTPS-Certs admin note.
- `.github/workflows/*.yml` — replace bare `cargo build` invocations with `just build` where needed (verify each in Task 25).

**Removed**
- Nothing wholesale; URL-param code paths in `main.ts` get deleted as part of Task 16.

---

## Task 1: Collapse QUIC port literal `:4443` → `:443`

**Files:**
- Modify: `ghostframe-xdaemon/src/main.rs:88`
- Modify: `ghostframe-lib/src/ffi.rs:55`

This is the smallest precondition change. Done first so all subsequent ports are coherent. E2E tests will still pass after this because the e2e forwarder dials `ghostframe-server:4443` — that's also updated here.

- [ ] **Step 1: Find every literal `:4443` and `4443` in production code paths.**

Run: `git grep -n '4443' -- ':!*.md' ':!docs/' ':!ghostframe-e2e/'`
Expected output mentions `ghostframe-xdaemon/src/main.rs:88` and `ghostframe-lib/src/ffi.rs:55`. No other production hits.

- [ ] **Step 2: Update both files.**

In `ghostframe-xdaemon/src/main.rs`, change line 88:
```rust
let server = GhostframeServer::new(config, ":443").await?;
```

In `ghostframe-lib/src/ffi.rs`, change line 55:
```rust
match rt.block_on(GhostframeServer::new(config, ":443")) {
```

- [ ] **Step 3: Find every `4443` in e2e and update them.**

Run: `git grep -n '4443' -- ghostframe-e2e/`

For each line found, change `4443` → `443`. There are dial calls like `test_node.dial("ghostframe-server:4443")` — these become `test_node.dial("ghostframe-server:443")`.

- [ ] **Step 4: Run lib tests.**

Run: `cargo test --workspace --lib`
Expected: PASS (the port literal is not asserted by any unit test).

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-xdaemon/src/main.rs ghostframe-lib/src/ffi.rs ghostframe-e2e/
git commit -m "$(cat <<'EOF'
refactor(transport): QUIC :4443 → :443

Precondition for the tailnet-served web client design. The HTTPS TCP
listener will share port 443 with QUIC's UDP listener; collapse the
QUIC literal first so the rest of the migration lands on a coherent
port number.
EOF
)"
```

---

## Task 2: Add `bootstrap.ts` extraction with vitest coverage

Extract the WebTransport-construction logic out of `main.ts` into a unit-testable module before touching the wire format. Module stands alone; not yet wired into `main.ts` (that happens in Task 16).

**Files:**
- Create: `ghostframe-web-client/src/bootstrap.ts`
- Create: `ghostframe-web-client/tests/bootstrap.test.ts`

- [ ] **Step 1: Write the failing test.**

Create `ghostframe-web-client/tests/bootstrap.test.ts`:
```ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { bootstrap } from '../src/bootstrap';

describe('bootstrap', () => {
  beforeEach(() => {
    vi.unstubAllGlobals();
  });

  it('fetches /config.json and constructs WebTransport same-origin', async () => {
    vi.stubGlobal('location', { origin: 'https://example.ts.net', host: 'example.ts.net' });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ certHash: '00112233445566778899aabbccddeeff' + '00112233445566778899aabbccddeeff' }),
    }));
    const ctorSpy = vi.fn();
    vi.stubGlobal('WebTransport', class { constructor(...args: unknown[]) { ctorSpy(...args); } });

    await bootstrap();

    expect(ctorSpy).toHaveBeenCalledTimes(1);
    const [url, opts] = ctorSpy.mock.calls[0];
    expect(url).toBe('https://example.ts.net/');
    expect(opts.serverCertificateHashes).toHaveLength(1);
    expect(opts.serverCertificateHashes[0].algorithm).toBe('sha-256');
    const bytes = new Uint8Array(opts.serverCertificateHashes[0].value);
    expect(bytes.byteLength).toBe(32);
    expect(bytes[0]).toBe(0x00);
    expect(bytes[1]).toBe(0x11);
    expect(bytes[31]).toBe(0xff);
  });

  it('throws a visible error when /config.json is missing certHash', async () => {
    vi.stubGlobal('location', { origin: 'https://example.ts.net', host: 'example.ts.net' });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({}),
    }));
    vi.stubGlobal('WebTransport', class {});
    await expect(bootstrap()).rejects.toThrow(/certHash/);
  });

  it('throws a visible error when /config.json returns non-OK', async () => {
    vi.stubGlobal('location', { origin: 'https://example.ts.net', host: 'example.ts.net' });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: false, status: 500 }));
    vi.stubGlobal('WebTransport', class {});
    await expect(bootstrap()).rejects.toThrow(/config.json/);
  });
});
```

- [ ] **Step 2: Run the test to verify it fails.**

Run: `cd ghostframe-web-client && npx vitest run tests/bootstrap.test.ts`
Expected: FAIL with "Cannot find module '../src/bootstrap'" or "bootstrap is not a function".

- [ ] **Step 3: Write the minimal implementation.**

Create `ghostframe-web-client/src/bootstrap.ts`:
```ts
export interface BootstrapResult {
  transport: WebTransport;
  certHash: string;
}

function hexToBytes(hex: string): Uint8Array {
  if (!/^[0-9a-f]+$/i.test(hex) || hex.length % 2 !== 0) {
    throw new Error(`bootstrap: invalid certHash hex: ${hex.slice(0, 32)}…`);
  }
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < hex.length; i += 2) {
    out[i / 2] = parseInt(hex.substr(i, 2), 16);
  }
  return out;
}

export async function bootstrap(): Promise<BootstrapResult> {
  const resp = await fetch('/config.json');
  if (!resp.ok) {
    throw new Error(`bootstrap: /config.json returned HTTP ${resp.status}`);
  }
  const cfg = await resp.json();
  if (typeof cfg.certHash !== 'string' || cfg.certHash.length === 0) {
    throw new Error('bootstrap: /config.json missing certHash');
  }
  const bytes = hexToBytes(cfg.certHash);
  const url = `https://${location.host}/`;
  const transport = new WebTransport(url, {
    serverCertificateHashes: [{ algorithm: 'sha-256', value: bytes.buffer }],
  });
  return { transport, certHash: cfg.certHash };
}
```

- [ ] **Step 4: Run the test to verify it passes.**

Run: `cd ghostframe-web-client && npx vitest run tests/bootstrap.test.ts`
Expected: PASS, 3/3.

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-web-client/src/bootstrap.ts ghostframe-web-client/tests/bootstrap.test.ts
git commit -m "$(cat <<'EOF'
feat(web-client): bootstrap.ts — fetch /config.json + construct WT

Extract the WebTransport construction path out of main.ts into a
unit-testable bootstrap module ahead of the URL-param removal. Reads
the cert hash from a same-origin /config.json instead of URL params.
EOF
)"
```

---

## Task 3: Justfile + Makefile guard for missing `dist/`

Lock in the build-order constraint before the embed lands. After this task, `cargo build` of `ghostframe-xdaemon` fails with a clear message when `ghostframe-web-client/dist/index.html` doesn't exist.

**Files:**
- Modify: `Justfile`
- Modify: `ghostbridge/Makefile`

- [ ] **Step 1: Rewrite `Justfile`.**

Replace the existing `web-client-build`, `test-e2e`, and `ci-local` recipes plus add `build`:
```makefile
build: build-web
    cargo build

build-release: build-web
    cargo build --release

test-unit:
    cargo test --lib

# Run from a clean checkout: builds the web client SPA (vite) into
# ghostframe-web-client/dist/, which ghostbridge //go:embeds at compile
# time. A stale or missing dist/ now fails the ghostbridge build with a
# clear message rather than silently embedding nothing.
build-web:
    cd ghostframe-web-client && npm install && npm run build

test-e2e: build-web containers-build
    cargo test --test e2e

containers-build:
    cargo build --release -p ghostframe-xdaemon -p ghostframe-test-pattern
    docker build -t ghostframe/test-server -f tests/containers/test-server/Dockerfile .
    docker build -t ghostframe/test-headscale -f tests/containers/headscale/Dockerfile tests/containers/headscale/

lint:
    cargo clippy --workspace --all-targets -- -D warnings

fmt-check:
    cargo fmt --all -- --check

fmt:
    cargo fmt --all

ci-local:
    @echo "=== fmt-check ==="
    just fmt-check
    @echo "=== clippy ==="
    cargo clippy --workspace --all-targets -- -D warnings
    @echo "=== unit tests ==="
    cargo test --workspace --lib
    @echo "=== web client build ==="
    just build-web
    cd ghostframe-web-client && npx tsc --noEmit
    @echo "=== release build ==="
    cargo build --workspace --release --exclude ghostframe-e2e
    @echo "=== cbindgen header up-to-date ==="
    cargo check -p ghostframe-lib
    git diff --exit-code ghostframe-lib/include/ghostframe.h
    @echo "=== go vet + build ==="
    cd ghostbridge && go vet ./... && go build ./...
    @echo "=== ci-local passed ==="
```

Note: `ci-local` now runs `build-web` **before** the release build (so ghostbridge's embed has something to embed) — the order is reversed from the current file.

- [ ] **Step 2: Modify `ghostbridge/Makefile`.**

Replace with:
```make
.PHONY: archive clean check-dist sync-dist

WEB_SRC = ../ghostframe-web-client/dist
WEB_LOCAL = dist

check-dist:
	@test -f $(WEB_SRC)/index.html || ( \
	  echo "error: ghostframe-web-client/dist not built" >&2; \
	  echo "  ghostbridge //go:embeds the SPA at compile time;" >&2; \
	  echo "  run: just build-web" >&2; \
	  echo "  or:  cd ghostframe-web-client && npm ci && npm run build" >&2; \
	  exit 1 )

# Go's //go:embed does not follow symlinks and does not allow `..` in
# patterns, so we copy the SPA tree into a sibling dist/ that embed can
# read. rsync --delete keeps stale files from accumulating across builds.
sync-dist: check-dist
	@mkdir -p $(WEB_LOCAL)
	@rsync -a --delete $(WEB_SRC)/ $(WEB_LOCAL)/

archive: sync-dist
	go build -buildmode=c-archive -o libghostbridge.a .
	# This also generates libghostbridge.h

clean:
	rm -f libghostbridge.a libghostbridge.h
	rm -rf $(WEB_LOCAL)
```

- [ ] **Step 3: Add `ghostbridge/dist/` to `.gitignore`.**

Append to `ghostbridge/.gitignore` (create the file if missing):
```
dist/
```

- [ ] **Step 4: Extend `ghostframe-lib/build.rs` to watch the source dist.**

In `ghostframe-lib/build.rs`, after the existing `println!("cargo:rerun-if-changed=...");` for the ghostbridge directory, add:
```rust
    // The SPA tree lives outside ghostbridge/ and is rsync'd in by the
    // Makefile. Without this rerun-if-changed, edits to the web client
    // won't trigger a re-link.
    let web_dist = PathBuf::from(&manifest_dir).join("../ghostframe-web-client/dist");
    println!("cargo:rerun-if-changed={}", web_dist.display());
```

- [ ] **Step 5: Verify the guard fires when source dist/ is absent.**

Run: `mv ghostframe-web-client/dist /tmp/dist.bak && (cd ghostbridge && make archive 2>&1 | head -5); ec=$?; mv /tmp/dist.bak ghostframe-web-client/dist; echo "exit=$ec"`
Expected output contains "error: ghostframe-web-client/dist not built" and `exit=` is non-zero.

- [ ] **Step 6: Verify the chained `just build` works.**

Run: `just build 2>&1 | tail -3`
Expected: cargo finishes successfully (`Finished` line). `ghostbridge/dist/index.html` exists after the build (`ls ghostbridge/dist/index.html`).

- [ ] **Step 7: Commit.**

```bash
git add Justfile ghostbridge/Makefile ghostbridge/.gitignore ghostframe-lib/build.rs
git commit -m "$(cat <<'EOF'
build: guard ghostbridge against a missing web-client dist/

The upcoming //go:embed lands ghostframe-web-client/dist/ into the
ghostbridge c-archive. Add a Makefile precondition that fails with a
clear remediation message when the SPA tree is absent, rsync it into
a sibling dist/ that //go:embed can read (embed does not follow
symlinks or `..` paths), and have build.rs watch the source tree so
cargo re-runs the Makefile on SPA changes. Also rename
web-client-build → build-web so `just build` orchestrates the full
build in the right order.
EOF
)"
```

---

## Task 4: Add Go `embed.FS` + static-file handler with test

**Files:**
- Create: `ghostbridge/web_server.go`
- Create: `ghostbridge/web_server_test.go`

- [ ] **Step 1: Write the failing test.**

Create `ghostbridge/web_server_test.go`:
```go
package main

import (
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestServeIndex(t *testing.T) {
	mux := newWebMux("deadbeef")
	srv := httptest.NewServer(mux)
	defer srv.Close()

	resp, err := http.Get(srv.URL + "/")
	if err != nil {
		t.Fatalf("GET /: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		t.Fatalf("GET /: status %d, want 200", resp.StatusCode)
	}
	body, _ := io.ReadAll(resp.Body)
	if !strings.Contains(string(body), "<html") {
		t.Fatalf("GET /: body %q does not look like HTML", string(body[:min(80, len(body))]))
	}
}

func TestServeConfigJSON(t *testing.T) {
	mux := newWebMux("deadbeefcafebabe1234567890abcdef0011223344556677889900aabbccddeeff")
	srv := httptest.NewServer(mux)
	defer srv.Close()

	resp, err := http.Get(srv.URL + "/config.json")
	if err != nil {
		t.Fatalf("GET /config.json: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		t.Fatalf("GET /config.json: status %d, want 200", resp.StatusCode)
	}
	if ct := resp.Header.Get("Content-Type"); !strings.HasPrefix(ct, "application/json") {
		t.Fatalf("GET /config.json: content-type %q, want application/json", ct)
	}
	body, _ := io.ReadAll(resp.Body)
	want := `{"certHash":"deadbeefcafebabe1234567890abcdef0011223344556677889900aabbccddeeff"}`
	if strings.TrimSpace(string(body)) != want {
		t.Fatalf("GET /config.json: body %q, want %q", string(body), want)
	}
}

func min(a, b int) int { if a < b { return a } ; return b }
```

- [ ] **Step 2: Run the test to verify it fails.**

Run: `cd ghostbridge && go test ./... 2>&1 | tail -10`
Expected: FAIL with "undefined: newWebMux".

- [ ] **Step 3: Write the implementation.**

Create `ghostbridge/web_server.go`:
```go
package main

import (
	"embed"
	"encoding/json"
	"io/fs"
	"net/http"
)

//go:embed all:dist
var webDist embed.FS

// distFS returns the dist tree rooted at "dist/" so handlers can serve
// "/index.html" instead of "/dist/index.html". Errors at startup are a
// build-config bug (missing //go:embed sources), not a runtime concern.
func distFS() fs.FS {
	sub, err := fs.Sub(webDist, "dist")
	if err != nil {
		panic("ghostbridge: dist/ subtree missing from embed: " + err.Error())
	}
	return sub
}

// newWebMux builds the HTTP handler mux for the embedded SPA + config.
// certHashHex is the lowercase-hex SHA-256 of the WebTransport server cert.
func newWebMux(certHashHex string) *http.ServeMux {
	mux := http.NewServeMux()
	mux.HandleFunc("/config.json", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("Cache-Control", "no-store")
		_ = json.NewEncoder(w).Encode(struct {
			CertHash string `json:"certHash"`
		}{CertHash: certHashHex})
	})
	mux.Handle("/", http.FileServer(http.FS(distFS())))
	return mux
}
```

Note: the `//go:embed all:dist` directive reads from `ghostbridge/dist/`, which the Makefile's `sync-dist` target (Task 3) `rsync`s from `../ghostframe-web-client/dist/`. Symlinks don't work — Go's embed deliberately refuses to follow them.

- [ ] **Step 4: Run the test to verify it passes.**

Run: `cd ghostframe-web-client && npm install && npm run build` (ensures source dist/ exists)
Run: `cd ghostbridge && make sync-dist && go test -run 'TestServe' -v ./...`
Expected: PASS, both tests.

- [ ] **Step 5: Commit.**

```bash
git add ghostbridge/web_server.go ghostbridge/web_server_test.go
git commit -m "$(cat <<'EOF'
feat(ghostbridge): embedded SPA + /config.json handler

Adds newWebMux: embedded ghostframe-web-client/dist via //go:embed plus
a /config.json endpoint that reports the WebTransport cert hash. Not
yet wired to any listener — Task 5 adds the redirect, Task 6 wires the
TLS listener and the FFI export. The Makefile's sync-dist rsync step
(Task 3) populates ghostbridge/dist before go build runs.
EOF
)"
```

---

## Task 5: Add `:80` → `:443` redirect handler with test

**Files:**
- Modify: `ghostbridge/web_server.go`
- Modify: `ghostbridge/web_server_test.go`

- [ ] **Step 1: Write the failing test.**

Append to `ghostbridge/web_server_test.go`:
```go
func TestRedirectHandler(t *testing.T) {
	h := newRedirectHandler()
	srv := httptest.NewServer(h)
	defer srv.Close()

	client := &http.Client{
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}
	resp, err := client.Get(srv.URL + "/some/path?q=1")
	if err != nil {
		t.Fatalf("GET: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != 301 {
		t.Fatalf("status %d, want 301", resp.StatusCode)
	}
	loc := resp.Header.Get("Location")
	// httptest binds to 127.0.0.1:<port>; Host header echoes it.
	if !strings.HasPrefix(loc, "https://") {
		t.Fatalf("Location %q does not start with https://", loc)
	}
	if !strings.HasSuffix(loc, "/some/path?q=1") {
		t.Fatalf("Location %q does not preserve path+query", loc)
	}
}
```

- [ ] **Step 2: Run the test to verify it fails.**

Run: `cd ghostbridge && go test -run TestRedirectHandler ./...`
Expected: FAIL with "undefined: newRedirectHandler".

- [ ] **Step 3: Write the implementation.**

Append to `ghostbridge/web_server.go`:
```go
// newRedirectHandler returns an HTTP handler that 301-redirects every
// request to the same host:path on https://. Used for the tsnet :80
// listener so users can paste the bare-hostname URL.
func newRedirectHandler() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		target := "https://" + r.Host + r.URL.RequestURI()
		http.Redirect(w, r, target, http.StatusMovedPermanently)
	})
}
```

- [ ] **Step 4: Run the test to verify it passes.**

Run: `cd ghostbridge && go test -run TestRedirectHandler -v ./...`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add ghostbridge/web_server.go ghostbridge/web_server_test.go
git commit -m "$(cat <<'EOF'
feat(ghostbridge): :80 → :443 redirect handler

Plain 301 redirect preserving path and query. Used by the upcoming :80
listener so bare-hostname URLs (without https://) reach the SPA.
EOF
)"
```

---

## Task 6: Add `gbridge_start_web_server` FFI export (scaffold)

Add the export, return OK without actually starting any listener yet. This unblocks the Rust binding in the next task — TLS-listener wiring is two tasks ahead so the diff stays small.

**Files:**
- Modify: `ghostbridge/main.go`

- [ ] **Step 1: Add new status codes and the export.**

In `ghostbridge/main.go`, after the existing `gbridge_close` export (around line 176), add:
```go
// Status codes for the web server start path. Negative-numbered to match
// the existing convention used by gbridge_new / gbridge_up / etc.
const (
	gbridgeWebStatusOK                  = 0
	gbridgeWebStatusInvalidHandle       = -1
	gbridgeWebStatusInvalidArg          = -20
	gbridgeWebStatusHTTPSCertsDisabled  = -21
	gbridgeWebStatusListenFailed        = -22
)

//export gbridge_start_web_server
func gbridge_start_web_server(sd C.int32_t, cCertHashHex *C.char) C.gbridge_status {
	h := lookup(int32(sd))
	if h == nil {
		return gbridgeWebStatusInvalidHandle
	}
	certHash := C.GoString(cCertHashHex)
	if len(certHash) != 64 {
		log.Printf("ghostbridge: gbridge_start_web_server: certHash length %d, want 64", len(certHash))
		return gbridgeWebStatusInvalidArg
	}
	// Listener wiring lands in Task 8; this scaffold returns OK so the
	// Rust binding can land independently of the TLS plumbing.
	_ = h
	_ = newWebMux(certHash)
	return gbridgeWebStatusOK
}
```

- [ ] **Step 2: Regenerate the c-archive header.**

Run: `cd ghostbridge && make clean && make archive`
Expected: builds `libghostbridge.a` and `libghostbridge.h` with no errors. The new `gbridge_start_web_server` declaration appears in the header.

- [ ] **Step 3: Run existing Go tests.**

Run: `cd ghostbridge && go test ./...`
Expected: PASS — all 3 web tests plus any pre-existing.

- [ ] **Step 4: Commit.**

```bash
git add ghostbridge/main.go ghostbridge/libghostbridge.a ghostbridge/libghostbridge.h
git commit -m "$(cat <<'EOF'
feat(ghostbridge): gbridge_start_web_server FFI scaffold

Exports the entry point Rust will call after the WebTransport cert is
generated. Validates the cert-hash hex length and returns OK; the
actual TCP/TLS listener wiring lands in the next task pair so the FFI
binding can land independently.
EOF
)"
```

---

## Task 7: Wire :80 and :443 listeners via `tsnet.Server.ListenTLS` (production path)

**Files:**
- Modify: `ghostbridge/main.go`
- Modify: `ghostbridge/web_server.go`

- [ ] **Step 1: Add the listener-starting helper to `web_server.go`.**

Append to `ghostbridge/web_server.go`:
```go
import (
	"context"
	"crypto/tls"
	"errors"
	"log"
	"net"

	"tailscale.com/tsnet"
)

// startWebListeners brings up the :80 (redirect) and :443 (TLS) listeners
// on the given tsnet.Server. In production mode (staticCert == nil) it
// uses tsnet's LE-backed ListenTLS; in e2e mode it uses a plain Listen
// wrapped with the supplied static cert.
//
// Returns immediately after the listeners are bound; the http.Serve
// goroutines run for the lifetime of the process.
func startWebListeners(
	ctx context.Context,
	srv *tsnet.Server,
	certHashHex string,
	staticCert *tls.Certificate,
) error {
	// :443 — TLS
	var tlsLn net.Listener
	if staticCert != nil {
		raw, err := srv.Listen("tcp", ":443")
		if err != nil {
			return err
		}
		tlsLn = tls.NewListener(raw, &tls.Config{
			Certificates: []tls.Certificate{*staticCert},
		})
	} else {
		// Production: tsnet's automatic LE provisioning for *.ts.net.
		var err error
		tlsLn, err = srv.ListenTLS("tcp", ":443")
		if err != nil {
			return err
		}
	}

	// :80 — plain HTTP redirect
	redirLn, err := srv.Listen("tcp", ":80")
	if err != nil {
		tlsLn.Close()
		return err
	}

	mux := newWebMux(certHashHex)
	go func() {
		err := http.Serve(tlsLn, mux)
		if !errors.Is(err, net.ErrClosed) {
			log.Printf("ghostbridge: :443 serve exited: %v", err)
		}
	}()
	go func() {
		err := http.Serve(redirLn, newRedirectHandler())
		if !errors.Is(err, net.ErrClosed) {
			log.Printf("ghostbridge: :80 serve exited: %v", err)
		}
	}()

	_ = ctx // reserved for graceful-shutdown wiring
	return nil
}
```

- [ ] **Step 2: Replace the scaffold body in `gbridge_start_web_server`.**

In `ghostbridge/main.go`, replace the body of `gbridge_start_web_server` from Task 6 with:
```go
//export gbridge_start_web_server
func gbridge_start_web_server(sd C.int32_t, cCertHashHex *C.char) C.gbridge_status {
	h := lookup(int32(sd))
	if h == nil {
		return gbridgeWebStatusInvalidHandle
	}
	certHash := C.GoString(cCertHashHex)
	if len(certHash) != 64 {
		log.Printf("ghostbridge: gbridge_start_web_server: certHash length %d, want 64", len(certHash))
		return gbridgeWebStatusInvalidArg
	}

	staticCert, err := loadStaticCertFromEnv()
	if err != nil {
		log.Printf("ghostbridge: gbridge_start_web_server: static cert env vars set but invalid: %v", err)
		return gbridgeWebStatusInvalidArg
	}

	if staticCert == nil {
		// Production: HTTPS Certs must be enabled in the tailnet.
		if domains := h.server.CertDomains(); len(domains) == 0 {
			log.Printf("ghostbridge: gbridge_start_web_server: tailnet has no HTTPS-eligible domains")
			log.Printf("ghostbridge:   enable HTTPS at https://login.tailscale.com/admin/dns")
			return gbridgeWebStatusHTTPSCertsDisabled
		}
	}

	if err := startWebListeners(context.Background(), h.server, certHash, staticCert); err != nil {
		log.Printf("ghostbridge: gbridge_start_web_server: listener bind failed: %v", err)
		return gbridgeWebStatusListenFailed
	}
	return gbridgeWebStatusOK
}
```

- [ ] **Step 3: Add `loadStaticCertFromEnv` to `web_server.go`.**

Append to `ghostbridge/web_server.go`:
```go
import (
	"crypto/tls"
	"fmt"
	"os"
)

// loadStaticCertFromEnv reads GHOSTFRAME_WEB_TLS_CERT_PEM and
// GHOSTFRAME_WEB_TLS_KEY_PEM and returns a parsed certificate when both
// are set. Used by the e2e harness to substitute a self-signed cert
// for tsnet's LE provisioning, which headscale-backed e2e cannot reach.
//
// Returns (nil, nil) when neither env var is set (production path).
// Returns an error if exactly one is set (clear misconfiguration) or
// if parsing fails.
func loadStaticCertFromEnv() (*tls.Certificate, error) {
	certPEM := os.Getenv("GHOSTFRAME_WEB_TLS_CERT_PEM")
	keyPEM := os.Getenv("GHOSTFRAME_WEB_TLS_KEY_PEM")
	if certPEM == "" && keyPEM == "" {
		return nil, nil
	}
	if certPEM == "" || keyPEM == "" {
		return nil, fmt.Errorf("GHOSTFRAME_WEB_TLS_{CERT,KEY}_PEM must be set together")
	}
	cert, err := tls.X509KeyPair([]byte(certPEM), []byte(keyPEM))
	if err != nil {
		return nil, fmt.Errorf("X509KeyPair: %w", err)
	}
	return &cert, nil
}
```

Consolidate the imports at the top of `web_server.go` (Go's parser will reject the duplicate `import` blocks added above):
```go
import (
	"context"
	"crypto/tls"
	"embed"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"log"
	"net"
	"net/http"
	"os"

	"tailscale.com/tsnet"
)
```

- [ ] **Step 4: Verify go build still works.**

Run: `cd ghostbridge && go vet ./... && go build ./...`
Expected: clean.

- [ ] **Step 5: Verify the existing tests still pass.**

Run: `cd ghostbridge && go test -run 'TestServe|TestRedirect' ./...`
Expected: PASS — `newWebMux` and `newRedirectHandler` are unchanged.

- [ ] **Step 6: Regenerate c-archive.**

Run: `cd ghostbridge && make clean && make archive`
Expected: succeeds.

- [ ] **Step 7: Commit.**

```bash
git add ghostbridge/main.go ghostbridge/web_server.go ghostbridge/libghostbridge.a ghostbridge/libghostbridge.h
git commit -m "$(cat <<'EOF'
feat(ghostbridge): wire :80 + :443 listeners via tsnet ListenTLS

Production path uses tsnet.Server.ListenTLS for automatic LE cert
provisioning; e2e path substitutes a static cert from
GHOSTFRAME_WEB_TLS_{CERT,KEY}_PEM env vars and skips the
CertDomains() fail-fast. Listeners run on background goroutines for
the lifetime of the process.
EOF
)"
```

---

## Task 8: Rust FFI binding for `gbridge_start_web_server`

**Files:**
- Modify: `ghostframe-lib/src/transport/ghostbridge.rs`

- [ ] **Step 1: Write the failing test.**

Append to the `tests` module in `ghostframe-lib/src/transport/ghostbridge.rs`:
```rust
    #[test]
    fn web_status_codes_are_distinct() {
        // Sanity check the status-code constants match ghostbridge/main.go.
        // If ghostbridge renumbers, this test fails loudly instead of the
        // Rust caller silently treating "HTTPS certs disabled" as success.
        assert_eq!(super::WEB_STATUS_OK, 0);
        assert_eq!(super::WEB_STATUS_INVALID_HANDLE, -1);
        assert_eq!(super::WEB_STATUS_INVALID_ARG, -20);
        assert_eq!(super::WEB_STATUS_HTTPS_CERTS_DISABLED, -21);
        assert_eq!(super::WEB_STATUS_LISTEN_FAILED, -22);
    }
```

- [ ] **Step 2: Run the test to verify it fails.**

Run: `cargo test -p ghostframe-lib --lib transport::ghostbridge::tests::web_status_codes_are_distinct`
Expected: FAIL with "cannot find value `WEB_STATUS_OK`".

- [ ] **Step 3: Add the extern declaration, status constants, and method.**

In `ghostframe-lib/src/transport/ghostbridge.rs`, extend the `extern "C"` block:
```rust
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
    fn gbridge_start_web_server(sd: c_int, cert_hash_hex: *const c_char) -> c_int;
}

pub(crate) const WEB_STATUS_OK: c_int = 0;
pub(crate) const WEB_STATUS_INVALID_HANDLE: c_int = -1;
pub(crate) const WEB_STATUS_INVALID_ARG: c_int = -20;
pub(crate) const WEB_STATUS_HTTPS_CERTS_DISABLED: c_int = -21;
pub(crate) const WEB_STATUS_LISTEN_FAILED: c_int = -22;
```

Add a method on `GhostbridgeHandle` (place it just before `impl Drop`):
```rust
    /// Start the embedded HTTPS web server on tsnet `:443` plus a `:80`
    /// → `:443` redirect listener.
    ///
    /// `cert_hash_hex` must be the lowercase-hex SHA-256 of the
    /// WebTransport server cert; it is exposed at `/config.json` so the
    /// browser can construct `new WebTransport(..., {serverCertificateHashes})`.
    ///
    /// Returns a typed error for the two cases callers must surface to
    /// the user differently:
    /// - [`WebServerError::HttpsCertsDisabled`] — tailnet admin needs to
    ///   enable HTTPS Certificates.
    /// - [`WebServerError::ListenFailed`] — port bind / cert load failed.
    pub fn start_web_server(&self, cert_hash_hex: &str) -> Result<(), WebServerError> {
        let c_hash = CString::new(cert_hash_hex).map_err(|_| WebServerError::InvalidArg)?;
        let rc = unsafe { gbridge_start_web_server(self.sd, c_hash.as_ptr()) };
        match rc {
            WEB_STATUS_OK => Ok(()),
            WEB_STATUS_HTTPS_CERTS_DISABLED => Err(WebServerError::HttpsCertsDisabled),
            WEB_STATUS_LISTEN_FAILED => Err(WebServerError::ListenFailed),
            WEB_STATUS_INVALID_ARG => Err(WebServerError::InvalidArg),
            other => Err(WebServerError::Other(other)),
        }
    }
```

Add the error enum after the existing `GhostbridgeError`:
```rust
#[derive(Debug, thiserror::Error)]
pub enum WebServerError {
    #[error("tailnet has no HTTPS-eligible domains; enable HTTPS at https://login.tailscale.com/admin/dns")]
    HttpsCertsDisabled,
    #[error("ghostbridge web listener bind failed")]
    ListenFailed,
    #[error("invalid argument passed to gbridge_start_web_server")]
    InvalidArg,
    #[error("ghostbridge web server returned unexpected rc={0}")]
    Other(c_int),
}
```

- [ ] **Step 4: Verify the test passes.**

Run: `cargo test -p ghostframe-lib --lib transport::ghostbridge::tests::web_status_codes_are_distinct`
Expected: PASS.

- [ ] **Step 5: Verify the whole crate builds.**

Run: `cargo build -p ghostframe-lib`
Expected: clean. The link step pulls in `libghostbridge.a` from Task 7.

- [ ] **Step 6: Commit.**

```bash
git add ghostframe-lib/src/transport/ghostbridge.rs
git commit -m "$(cat <<'EOF'
feat(ghostframe-lib): Rust binding for gbridge_start_web_server

GhostbridgeHandle::start_web_server returns a typed WebServerError so
the daemon can surface HttpsCertsDisabled to the user with a tailnet
admin URL while still treating ListenFailed as a generic exit-2 case.
EOF
)"
```

---

## Task 9: Call `start_web_server` from `GhostframeServer::new` (with fail-fast)

**Files:**
- Modify: `ghostframe-lib/src/server.rs`
- Modify: `ghostframe-lib/src/transport/io_bridge.rs` (if necessary to expose the handle — see Step 1)

- [ ] **Step 1: Locate where the `GhostbridgeHandle` lives.**

Run: `grep -n "GhostbridgeHandle" /home/cedric/work/ghostframe/ghostframe-lib/src/transport/io_bridge.rs | head`
Expected: lines that show whether `IoBridge` owns the handle directly or hides it. The plan assumes `IoBridge` exposes it — if not, add a method `IoBridge::ghostbridge(&self) -> &GhostbridgeHandle` returning a borrow.

- [ ] **Step 2: Extend `GhostframeServer::new`.**

In `ghostframe-lib/src/server.rs`, modify `new` (around lines 61-81) so that after `cert_hash` is read from the bridge, the web server is started:
```rust
    pub async fn new(
        config: GhostbridgeConfig,
        listen_addr: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (frame_tx, frame_rx) = mpsc::channel::<FrameSubmission>(2);

        let mut bridge = IoBridge::new_with_frames(&config, listen_addr, frame_rx).await?;
        let cert_hash = bridge.cert_hash_sha256().to_owned();

        // Start the tsnet :443 (HTTPS) + :80 (redirect) listeners. Failures
        // are fatal: a misconfigured tailnet or a port bind error means the
        // first-connection URL will not work. Surface them at startup
        // rather than at user-connect time.
        bridge.ghostbridge().start_web_server(&cert_hash)?;

        let io_task = tokio::spawn(async move {
            if let Err(e) = bridge.run().await {
                tracing::error!(error = %e, "IoBridge event loop exited with error");
            }
        });

        Ok(Self { frame_tx, cert_hash, _io_task: io_task })
    }
```

If `IoBridge` does not already expose `ghostbridge()`, add it to `io_bridge.rs` as a thin accessor:
```rust
    pub fn ghostbridge(&self) -> &crate::transport::ghostbridge::GhostbridgeHandle {
        &self.ghostbridge_handle  // field name may differ — adjust to actual
    }
```

- [ ] **Step 3: Verify the lib still builds.**

Run: `cargo build -p ghostframe-lib`
Expected: clean.

- [ ] **Step 4: Verify the lib tests still pass.**

Run: `cargo test -p ghostframe-lib --lib`
Expected: PASS — no unit test currently exercises `GhostframeServer::new` end-to-end (it would need a real tsnet); the call addition does not change the surface.

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-lib/src/server.rs ghostframe-lib/src/transport/io_bridge.rs
git commit -m "$(cat <<'EOF'
feat(server): start the embedded web server after cert generation

GhostframeServer::new now wires the tsnet :443/:80 HTTPS listeners and
fails fast (typed WebServerError) when HTTPS Certificates are not
enabled in the tailnet admin DNS settings.
EOF
)"
```

---

## Task 10: xdaemon — turn `WebServerError` into a clean exit-2 with admin URL

**Files:**
- Modify: `ghostframe-xdaemon/src/main.rs`

- [ ] **Step 1: Find where `GhostframeServer::new` is awaited.**

Around line 88, the call propagates `?` into `main`. After Task 9 the error type contains a `WebServerError::HttpsCertsDisabled`. Catch it explicitly so the operator gets a clear message rather than the boxed `Display`.

- [ ] **Step 2: Replace the `?` with explicit handling.**

Replace lines 87-92 with:
```rust
    tracing::info!("Connecting to Tailscale...");
    let server = match GhostframeServer::new(config, ":443").await {
        Ok(s) => s,
        Err(e) => {
            if let Some(ws) = e.downcast_ref::<ghostframe_lib::WebServerError>() {
                if matches!(ws, ghostframe_lib::WebServerError::HttpsCertsDisabled) {
                    tracing::error!(
                        "HTTPS Certificates are not enabled for this tailnet. \
                         Enable them at https://login.tailscale.com/admin/dns and restart."
                    );
                    std::process::exit(2);
                }
            }
            return Err(e);
        }
    };
```

Note: this requires `WebServerError` to be re-exported from `ghostframe_lib`. If it is not, add to `ghostframe-lib/src/lib.rs`:
```rust
pub use transport::ghostbridge::WebServerError;
```

- [ ] **Step 3: Verify xdaemon builds.**

Run: `cargo build -p ghostframe-xdaemon`
Expected: clean.

- [ ] **Step 4: Commit.**

```bash
git add ghostframe-xdaemon/src/main.rs ghostframe-lib/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(xdaemon): friendly exit-2 when HTTPS Certs are disabled

Detect WebServerError::HttpsCertsDisabled at startup and log the exact
admin URL the operator needs to visit, instead of surfacing a generic
boxed-error backtrace.
EOF
)"
```

---

## Task 11: Drop `CERT_HASH_SHA256=` stdout line

E2E now reads the hash from `/config.json` (Task 19). Removing the println aligns prod and e2e on a single delivery channel.

**Files:**
- Modify: `ghostframe-xdaemon/src/main.rs`

- [ ] **Step 1: Remove the print.**

In `ghostframe-xdaemon/src/main.rs`, delete lines 90-92 (the println comment and call). The `server` binding becomes the next line in scope.

- [ ] **Step 2: Build.**

Run: `cargo build -p ghostframe-xdaemon`
Expected: clean. `server.cert_hash()` is no longer used by the binary; it remains available on the type for tests.

- [ ] **Step 3: Commit.**

```bash
git add ghostframe-xdaemon/src/main.rs
git commit -m "$(cat <<'EOF'
refactor(xdaemon): drop CERT_HASH_SHA256= stdout line

The hash is now served via /config.json by ghostbridge, used by both
production browsers and the e2e harness. The stdout line had no other
consumer.
EOF
)"
```

---

## Task 12: Rewrite `main.ts` bootstrap to use `bootstrap()`

Switches the production page from URL-param parsing to `fetch('/config.json')` + `bootstrap()`.

**Files:**
- Modify: `ghostframe-web-client/src/main.ts`

- [ ] **Step 1: Replace the URL-param block.**

In `ghostframe-web-client/src/main.ts`, lines 30-47 (the `hexToBuffer` helper and the `main()` opening that reads `host` / `certHash`) plus lines 279-286 (the `WebTransport` construction) collapse into a single `bootstrap()` call.

Replace lines 30-47 with:
```ts
import { bootstrap } from './bootstrap.js';
```

Delete the `hexToBuffer` helper outright (lines 30-36).

In `main()` (now starting around line 38), replace the original URL-param + `wtUrl` lines with:
```ts
async function main() {
  const url = new URL(window.location.href);
  log(`Connecting to ${location.origin}...`);

  // WebGPU init — fatal if unavailable per design D2.
  let renderer: WebGpuRenderer;
  // ... (rest of the existing WebGPU block, unchanged through line 277)
```

In the block currently at lines 279-286, replace:
```ts
  let transport: WebTransport;
  if (certHash) {
    transport = new WebTransport(wtUrl, {
      serverCertificateHashes: [{ algorithm: 'sha-256', value: hexToBuffer(certHash) }],
    });
  } else {
    transport = new WebTransport(wtUrl);
  }
```
with:
```ts
  const { transport } = await bootstrap();
```

- [ ] **Step 2: Type-check.**

Run: `cd ghostframe-web-client && npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 3: Rebuild dist.**

Run: `cd ghostframe-web-client && npm run build`
Expected: vite produces fresh `dist/` artifacts.

- [ ] **Step 4: Run vitest.**

Run: `cd ghostframe-web-client && npx vitest run`
Expected: all existing tests + the bootstrap tests pass.

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-web-client/src/main.ts ghostframe-web-client/dist
git commit -m "$(cat <<'EOF'
feat(web-client): main.ts uses bootstrap() — drop URL-param flow

The page no longer reads ?host and ?certHash from the URL. It loads
both pieces of state from a same-origin /config.json served by the
daemon, restoring the simple "open the URL" connection UX.
EOF
)"
```

---

## Task 13: E2E — cert+key+SPKI generator helper

**Files:**
- Create: `ghostframe-e2e/src/harness/e2e_certs.rs`
- Modify: `ghostframe-e2e/src/harness/mod.rs`
- Modify: `ghostframe-e2e/Cargo.toml` (add `rcgen`, `sha2`, `base64` if not present)

- [ ] **Step 1: Check current dependencies.**

Run: `grep -E 'rcgen|sha2|base64' /home/cedric/work/ghostframe/ghostframe-e2e/Cargo.toml`
Expected: rcgen may already be in scope via workspace; if not, add `rcgen = "0.13"`, `sha2 = "0.10"`, `base64 = "0.22"` to `[dependencies]`. (Versions: match what `ghostframe-lib` already pulls in — check `cargo tree -p ghostframe-lib | grep -E 'rcgen|sha2|base64'`.)

- [ ] **Step 2: Write the failing test.**

Create `ghostframe-e2e/src/harness/e2e_certs.rs`:
```rust
//! Generates a self-signed TLS cert + key for the e2e harness to inject
//! into ghostbridge via GHOSTFRAME_WEB_TLS_{CERT,KEY}_PEM env vars.
//! Also derives the cert's SPKI fingerprint so Chrome can be launched
//! with `--ignore-certificate-errors-spki-list=<spki>` — surgical
//! per-cert trust rather than a blanket disable.

use sha2::{Digest, Sha256};

pub struct E2eCert {
    pub cert_pem: String,
    pub key_pem: String,
    /// Base64-without-padding SHA-256 of the cert's SubjectPublicKeyInfo,
    /// formatted as Chrome expects for `--ignore-certificate-errors-spki-list`.
    pub spki_b64: String,
}

pub fn generate(sans: &[&str]) -> anyhow::Result<E2eCert> {
    let mut params = rcgen::CertificateParams::new(
        sans.iter().map(|s| s.to_string()).collect::<Vec<_>>()
    )?;
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + time::Duration::days(13);
    let key_pair = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    // SPKI fingerprint: hash the SubjectPublicKeyInfo DER, not the whole
    // cert. rcgen exposes the SPKI DER via key_pair.public_key_der().
    let spki_der = key_pair.public_key_der();
    let spki_b64 = base64::engine::general_purpose::STANDARD_NO_PAD
        .encode(Sha256::digest(&spki_der));

    Ok(E2eCert { cert_pem, key_pem, spki_b64 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn generates_cert_with_valid_pem_and_32byte_spki() {
        let c = generate(&["localhost", "127.0.0.1"]).expect("generate");
        assert!(c.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(c.key_pem.contains("PRIVATE KEY"));
        let spki = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(&c.spki_b64).expect("base64");
        assert_eq!(spki.len(), 32, "SHA-256 is 32 bytes");
    }
}
```

- [ ] **Step 3: Wire into `mod.rs`.**

In `ghostframe-e2e/src/harness/mod.rs`, add:
```rust
pub mod e2e_certs;
```

- [ ] **Step 4: Run the test.**

Run: `cargo test -p ghostframe-e2e --lib harness::e2e_certs::tests`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-e2e/src/harness/e2e_certs.rs ghostframe-e2e/src/harness/mod.rs ghostframe-e2e/Cargo.toml
git commit -m "$(cat <<'EOF'
test(e2e): cert+key+SPKI generator for ghostbridge TLS env-var override

Mints a self-signed cert with a 13-day validity, returning PEM blobs
the harness pipes into the container via GHOSTFRAME_WEB_TLS_{CERT,KEY}
plus a base64 SPKI fingerprint Chrome accepts via
--ignore-certificate-errors-spki-list.
EOF
)"
```

---

## Task 14: E2E — TCP forwarder helper

The harness already has a UDP forwarder. Add a sibling TCP one so the browser can reach the in-container Go web server on the same `localhost:<port>` it uses for WebTransport.

**Files:**
- Modify: `ghostframe-e2e/src/harness/transport.rs`

**Why this is more involved than UDP:** tsnet runs entirely in a userspace gvisor netstack — `tsnet.Server.Dial("tcp", ...)` returns a `net.Conn` that wraps gvisor state, *not* a real kernel TCP socket. `SyscallConn()` doesn't give you a usable fd. So we mirror the existing `gbridge_listen_udp` / `gbridge_dial_udp` pattern: a `syscall.Socketpair` bridges Go-side gvisor I/O to a C-side fd that Rust treats as a `UnixStream`. For TCP that means two `io.Copy` goroutines (no framing — direct byte stream).

- [ ] **Step 1: Add `gbridge_dial_tcp` FFI export to `ghostbridge/main.go`.**

After `gbridge_dial_udp` (around line 161), add:
```go
//export gbridge_dial_tcp
func gbridge_dial_tcp(sd C.int32_t, cRemote *C.char, fdOut *C.int32_t) C.gbridge_status {
	h := lookup(int32(sd))
	if h == nil {
		return -1
	}
	remote := C.GoString(cRemote)
	log.Printf("ghostbridge: dialing tcp %q", remote)
	netConn, err := h.server.Dial(context.Background(), "tcp", remote)
	if err != nil {
		log.Printf("ghostbridge: dial tcp %q failed: %v", remote, err)
		return -3
	}
	goFd, cFd, err := makeSocketpair()
	if err != nil {
		netConn.Close()
		return -4
	}
	spawnTcpBridge(netConn, goFd)
	*fdOut = C.int32_t(cFd)
	return 0
}

// spawnTcpBridge copies bytes bidirectionally between a tsnet TCP
// connection and the Go-side socketpair fd. Mirrors spawnPacketBridge
// but without the per-packet framing — TCP is a byte stream.
func spawnTcpBridge(conn net.Conn, goFd int) {
	goFile := os.NewFile(uintptr(goFd), "gbridge-tcp-goside")
	// goFile is blocking (we only set O_NONBLOCK on the C side); io.Copy
	// blocks the goroutine on read until the peer closes, which is
	// exactly the lifecycle we want.
	go func() {
		defer goFile.Close()
		defer conn.Close()
		_, _ = io.Copy(conn, goFile)
	}()
	go func() {
		_, _ = io.Copy(goFile, conn)
	}()
}
```

- [ ] **Step 2: Regenerate the c-archive header.**

Run: `cd ghostbridge && make clean && make archive 2>&1 | tail -5`
Expected: `libghostbridge.h` now contains `gbridge_dial_tcp`.

- [ ] **Step 3: Add the Rust binding.**

In `ghostframe-lib/src/transport/ghostbridge.rs`, extend the `extern "C"` block:
```rust
    fn gbridge_dial_tcp(sd: c_int, remote_addr: *const c_char, fd_out: *mut c_int) -> c_int;
```

Add a method on `GhostbridgeHandle`. The returned fd is a Unix-domain socketpair endpoint (same shape as the UDP `dial`/`listen` path), not a kernel TCP socket — Rust must wrap it as a `UnixStream`:
```rust
    /// Dial a TCP address over the tailnet. Returns a Unix-domain
    /// stream endpoint of the Go-side socketpair that bridges to the
    /// remote TCP connection. `tokio::io::copy_bidirectional` over the
    /// returned `UnixStream` is the typical consumer (used by the E2E
    /// harness's start_tcp_forwarder).
    pub fn dial_tcp(&self, remote: &str) -> Result<std::os::unix::net::UnixStream, GhostbridgeError> {
        let c_remote = CString::new(remote)?;
        let mut fd: c_int = -1;
        let rc = unsafe { gbridge_dial_tcp(self.sd, c_remote.as_ptr(), &mut fd) };
        if rc < 0 {
            return Err(GhostbridgeError::Ffi("gbridge_dial_tcp", rc));
        }
        // Safety: fd is freshly returned, owned by this caller. O_NONBLOCK
        // already set on the Go side.
        Ok(unsafe {
            <std::os::unix::net::UnixStream as std::os::unix::io::FromRawFd>::from_raw_fd(fd)
        })
    }
```

- [ ] **Step 4: Add `dial_tcp` to `TestNode`.**

Look at how `TestNode::dial` (UDP path) is implemented:

Run: `grep -n "fn dial\b\|fn dial(" /home/cedric/work/ghostframe/ghostframe-e2e/src/harness/containers.rs`

Add a sibling method that wraps the new `GhostbridgeHandle::dial_tcp`. Signature shape (adjust field name `handle` to whatever the file uses):
```rust
    pub fn dial_tcp(&self, remote: &str) -> anyhow::Result<std::os::unix::net::UnixStream> {
        Ok(self.handle.dial_tcp(remote)?)
    }
```

- [ ] **Step 5: Append `start_tcp_forwarder` to `transport.rs`.**

In `ghostframe-e2e/src/harness/transport.rs`:
```rust
use tokio::net::TcpListener;

/// Bridges a local loopback TCP listener to a tsnet TCP dial. Each
/// accepted browser connection opens a fresh tsnet-side dial via the
/// test node, and a bidirectional copy task ferries bytes both ways
/// until either side closes.
///
/// Returns the bound `SocketAddr` the browser should connect to.
pub async fn start_tcp_forwarder(
    local_bind: &str,
    test_node: std::sync::Arc<crate::harness::containers::TestNode>,
    remote: String,
) -> Result<SocketAddr> {
    let listener = TcpListener::bind(local_bind).await?;
    let local = listener.local_addr()?;

    tokio::spawn(async move {
        loop {
            let Ok((mut downstream, _)) = listener.accept().await else { return };
            let test_node = test_node.clone();
            let remote = remote.clone();
            tokio::spawn(async move {
                let upstream_std = match test_node.dial_tcp(&remote) {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::warn!(error = %e, %remote, "tcp forwarder dial failed");
                        return;
                    }
                };
                if upstream_std.set_nonblocking(true).is_err() { return }
                let Ok(mut upstream) = tokio::net::UnixStream::from_std(upstream_std) else { return };
                let _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await;
            });
        }
    });

    Ok(local)
}
```

Add `pub use transport::start_tcp_forwarder;` to the re-exports in `ghostframe-e2e/src/harness/mod.rs`.

- [ ] **Step 6: Build everything.**

Run: `cd ghostbridge && make archive && cd .. && cargo build -p ghostframe-e2e --tests`
Expected: clean.

- [ ] **Step 7: Commit.**

```bash
git add ghostbridge/main.go ghostbridge/libghostbridge.a ghostbridge/libghostbridge.h \
        ghostframe-lib/src/transport/ghostbridge.rs \
        ghostframe-e2e/src/harness/transport.rs ghostframe-e2e/src/harness/containers.rs \
        ghostframe-e2e/src/harness/mod.rs
git commit -m "$(cat <<'EOF'
test(e2e): TCP forwarder + gbridge_dial_tcp via socketpair

tsnet's gvisor netstack has no kernel-fd to hand out, so mirror the
existing UDP path: open a socketpair, run two io.Copy goroutines
between the tsnet TCP net.Conn and the Go-side fd, return the C-side
fd as a UnixStream-shaped handle. start_tcp_forwarder spawns a
copy_bidirectional task per accepted browser connection.
EOF
)"
```

---

## Task 15: E2E — migrate `scene.rs` to the new flow

**Files:**
- Modify: `ghostframe-e2e/src/harness/scene.rs`

- [ ] **Step 1: Update the imports + replace the TLS env vars + dual forwarder + Chrome flag.**

In `scene.rs`, locate the existing block starting at line 208 (`let mut server_image = GenericImage::new...`) and:

1. Generate the e2e cert before constructing the image. Two SANs so the URL can be either form:
   ```rust
   let e2e_cert = crate::harness::e2e_certs::generate(&["localhost", "127.0.0.1"])?;
   ```

2. Add env vars to `server_image`:
   ```rust
   .with_env_var("GHOSTFRAME_WEB_TLS_CERT_PEM", &e2e_cert.cert_pem)
   .with_env_var("GHOSTFRAME_WEB_TLS_KEY_PEM", &e2e_cert.key_pem)
   ```

3. Replace the `with_ready_conditions(vec![WaitFor::message_on_stdout("CERT_HASH_SHA256=")])` line — the daemon no longer prints that. Use a generic readiness probe: tail for an info line that the IoBridge always emits, e.g. `"IoBridge: spawned tasks"` or whatever exists today. Run `grep -nE 'tracing::info!.*("|started|ready' /home/cedric/work/ghostframe/ghostframe-lib/src/transport/io_bridge.rs | head -5` to find a stable line; pick one that prints exactly once per boot. Use that exact string with `WaitFor::message_on_stderr` (tracing writes to stderr in text format) or `message_on_stdout` (for json format the env sets above).

4. Remove the `let cert_hash = crate::harness::containers::read_cert_hash_from_logs(...)` call — the e2e no longer needs it because `/config.json` will deliver it to the browser.

5. Replace the single forwarder with two:
   ```rust
   // TestNode now wraps in Arc so multiple forwarders can share it.
   let test_node = std::sync::Arc::new(
       crate::harness::containers::TestNode::join(client_key, client_control_url).await?
   );
   let udp_upstream = test_node.dial("ghostframe-server:443")?;
   let udp_forwarder = crate::harness::transport::start_forwarder("127.0.0.1:0", udp_upstream).await?;

   // Bind TCP to the same port number the UDP forwarder grabbed.
   let tcp_bind = format!("127.0.0.1:{}", udp_forwarder.port());
   let tcp_forwarder = crate::harness::transport::start_tcp_forwarder(
       &tcp_bind,
       test_node.clone(),
       "ghostframe-server:443".to_string(),
   ).await?;
   ```

6. Replace the `BrowserConfig` builder block (around lines 300-314) so the SPKI flag is added:
   ```rust
   .arg(("ignore-certificate-errors-spki-list", &e2e_cert.spki_b64))
   ```

7. Replace the `page_url` formation (line 319-325) with:
   ```rust
   // Use localhost (not 127.0.0.1) so the URL matches the SAN that
   // looks most natural; both SANs are in the cert anyway.
   let page_url = format!("https://localhost:{}/", tcp_forwarder.port());
   ```

8. Drop the old `dist_dir` + `start_static_server` block entirely (lines 281-286). The harness no longer serves the SPA — the daemon does.

- [ ] **Step 2: Verify it compiles.**

Run: `cargo build -p ghostframe-e2e --tests`
Expected: clean. `TestNode::dial` may need an `.dial(&self, ...)` change to accept `&Arc<Self>`; if so, add `&` or unwrap as needed.

- [ ] **Step 3: Commit (no e2e run yet — that's the final task).**

```bash
git add ghostframe-e2e/src/harness/scene.rs
git commit -m "$(cat <<'EOF'
test(e2e): scene.rs migrates to https:// flow with env-var TLS

Generates an e2e cert, ships it to the daemon container via env vars,
runs paired TCP+UDP forwarders on the same loopback port so Chromium
can hit https://127.0.0.1:<port>/ for both the SPA and the WebTransport
handshake. Drops the local axum static server and the cert-hash
journalctl probe.
EOF
)"
```

---

## Task 16: E2E — migrate the inline tests in `tests/e2e.rs`

Three test bodies in `tests/e2e.rs` still set up their own scene without using `scene.rs`. Mirror the Task 15 changes for each.

**Files:**
- Modify: `ghostframe-e2e/tests/e2e.rs`

- [ ] **Step 1: Find the inline setups.**

Run: `grep -n 'CERT_HASH_SHA256\|certHash' /home/cedric/work/ghostframe/ghostframe-e2e/tests/e2e.rs`
Expected: about six lines across three tests (`e2e_quic_ping_pong_over_tailscale`, plus two others — exact names depend on the file).

- [ ] **Step 2: Apply the Task 15 pattern to each.**

For each test body:
1. Add `let e2e_cert = helpers::e2e_certs::generate(&["localhost", "127.0.0.1"])?;` before `GenericImage::new(...)`.
2. Append `.with_env_var("GHOSTFRAME_WEB_TLS_CERT_PEM", &e2e_cert.cert_pem)` and `.with_env_var("GHOSTFRAME_WEB_TLS_KEY_PEM", &e2e_cert.key_pem)` to the image builder.
3. Replace `WaitFor::message_on_stdout("CERT_HASH_SHA256=")` with the readiness probe chosen in Task 15.
4. Remove the `read_cert_hash_from_logs` call.
5. Add the TCP forwarder + Arc the test node.
6. Change the `page_url` to `format!("https://localhost:{}/", tcp_forwarder.port())`.
7. Add the `--ignore-certificate-errors-spki-list` arg.
8. Remove the `dist_dir` + `start_static_server` blocks.

- [ ] **Step 3: Build the tests.**

Run: `cargo build -p ghostframe-e2e --tests`
Expected: clean.

- [ ] **Step 4: Commit.**

```bash
git add ghostframe-e2e/tests/e2e.rs
git commit -m "$(cat <<'EOF'
test(e2e): migrate inline tests in tests/e2e.rs to https:// flow

Same migration as scene.rs applied to the three inline-setup tests:
TLS env vars on the daemon image, paired forwarders, https:// page URL,
SPKI Chrome flag. Drops the now-unused static-server and cert-hash
journalctl probes.
EOF
)"
```

---

## Task 17: Remove `read_cert_hash_from_logs` and the harness static-server helper

Both functions are dead after Tasks 15 and 16. Delete them and re-export what survives.

**Files:**
- Modify: `ghostframe-e2e/src/harness/containers.rs`
- Modify: `ghostframe-e2e/src/harness/transport.rs`
- Modify: `ghostframe-e2e/src/harness/mod.rs`

- [ ] **Step 1: Verify no consumers remain.**

Run: `git grep -n 'read_cert_hash_from_logs\|start_static_server' -- ghostframe-e2e/`
Expected: only the definitions themselves.

- [ ] **Step 2: Delete the definitions.**

In `containers.rs`, delete the `read_cert_hash_from_logs` function and the doc comment block above it (around lines 83-97).

In `transport.rs`, delete `start_static_server` (lines 63-71) and the `axum::Router` + `tower_http::services::ServeDir` imports if no longer used.

In `mod.rs`, drop `start_static_server` from the re-export list (`pub use transport::{start_forwarder, start_static_server};` → `pub use transport::{start_forwarder, start_tcp_forwarder};`).

- [ ] **Step 3: Drop unused dependencies.**

Run: `grep -E 'axum|tower-http' /home/cedric/work/ghostframe/ghostframe-e2e/Cargo.toml`
If those are listed only for the deleted helper, remove them. Otherwise leave them.

- [ ] **Step 4: Build.**

Run: `cargo build -p ghostframe-e2e --tests`
Expected: clean. Run `cargo clippy -p ghostframe-e2e --tests -- -D warnings` to catch any dead-import warnings the compiler still tolerates.

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-e2e/src/harness/containers.rs ghostframe-e2e/src/harness/transport.rs \
        ghostframe-e2e/src/harness/mod.rs ghostframe-e2e/Cargo.toml
git commit -m "$(cat <<'EOF'
chore(e2e): remove dead static-server + cert-hash-from-logs helpers

Both helpers are unused after the https:// migration; deleting plus
the axum/tower-http deps that came with start_static_server keeps the
harness surface focused.
EOF
)"
```

---

## Task 18: Update README and `packaging/install.sh`

**Files:**
- Modify: `README.md`
- Modify: `packaging/install.sh`

- [ ] **Step 1: Rewrite the "First connection" section in `README.md`.**

Replace the entire `## First connection` block (lines 76-105 in current README — verify with `grep -n '^## First connection' README.md`) with:
```markdown
## First connection

On any device on the same tailnet, open

```
https://<hostname>.<tailnet>.ts.net/
```

in Chrome / Chromium / Edge. `<hostname>` is the value of `TS_HOSTNAME`
(default `ghostframe-server`); `<tailnet>` is your tailnet's MagicDNS
suffix.

The daemon serves the web client and its WebTransport cert hash directly,
so no manual setup is required on the client device. The page uses a
Tailscale-issued Let's Encrypt certificate — make sure
**HTTPS Certificates** are enabled at
<https://login.tailscale.com/admin/dns> before connecting for the first
time.
```

- [ ] **Step 2: Update `packaging/install.sh` final output.**

Run: `grep -n 'CERT_HASH_SHA256\|python3 -m http.server\|First connection' /home/cedric/work/ghostframe/packaging/install.sh`

For each printed instruction that references the old flow, replace with a final block at the script's success path:
```bash
echo
echo "Ghostframe installed. After reboot:"
echo "  Open https://${TS_HOSTNAME}.<tailnet>.ts.net/ in Chrome/Chromium/Edge."
echo
echo "If the URL fails to load, enable HTTPS Certificates in your tailnet at"
echo "  https://login.tailscale.com/admin/dns"
echo "and refresh the page."
```

(Exact placement: replace the post-install messaging block at the end of the script. Use `TS_HOSTNAME` if the script binds that variable; otherwise inline the default `ghostframe-server`.)

- [ ] **Step 3: Verify.**

Run: `bash -n packaging/install.sh`
Expected: no syntax errors (this is a shellcheck-light pass).

- [ ] **Step 4: Commit.**

```bash
git add README.md packaging/install.sh
git commit -m "$(cat <<'EOF'
docs: README + install.sh switch to single-URL first connection

Drops the cert-hash hunt and the local Python http.server step; the
URL is now the daemon's own tsnet hostname.
EOF
)"
```

---

## Task 19: CI workflow updates

Replace bare `cargo build` invocations in the workflow that hit packages embedding the SPA. After Task 3, those builds fail without a pre-existing `dist/`.

**Files:**
- Modify: `.github/workflows/*.yml`

- [ ] **Step 1: Find affected workflow steps.**

Run: `grep -rn 'cargo build\|cargo test' .github/workflows/`
For each step that builds `ghostframe-xdaemon`, `ghostframe-e2e`, or runs `cargo build --workspace`, add an explicit `just build-web` step before it (or change the invocation to `just build` / `just test-e2e`).

- [ ] **Step 2: Apply the changes.**

For each affected step, prepend:
```yaml
- name: Build web client SPA
  run: just build-web
```
or replace the build command with the `just` recipe.

- [ ] **Step 3: Verify by running ci-local.**

Run: `just ci-local 2>&1 | tail -10`
Expected: "=== ci-local passed ===".

- [ ] **Step 4: Commit.**

```bash
git add .github/workflows/
git commit -m "$(cat <<'EOF'
ci: build web client SPA before cargo builds that embed it

ghostbridge now //go:embeds ghostframe-web-client/dist/. CI steps that
build the daemon or run e2e need npm run build to have run first.
EOF
)"
```

---

## Task 20: Full sweep — unit, lib, lint, e2e

**Files:** none — verification only.

- [ ] **Step 1: Run the unit-test gate.**

Run: `cargo test --workspace --lib`
Expected: PASS, no test failures.

- [ ] **Step 2: Run clippy.**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 3: Run vitest.**

Run: `cd ghostframe-web-client && npx vitest run`
Expected: PASS.

- [ ] **Step 4: Run Go tests.**

Run: `cd ghostbridge && go test ./...`
Expected: PASS.

- [ ] **Step 5: Rebuild containers and run e2e.**

Run: `just test-e2e 2>&1 | tail -30`
Expected: every e2e test passes. The harness now exercises the production Go web server end-to-end (only the cert source is substituted).

If any test fails, the most likely culprits and where to look:
- WebGPU init flakes → re-run; this is a known intermittent unrelated to this work.
- "timed out waiting for frame rendering" → check that `ghostframe-web-client/dist/` is current (`just build-web` first).
- HTTP 502 from `https://127.0.0.1:<port>/` → the TCP forwarder isn't reaching the daemon; verify `tsnet.Server.Listen("tcp", ":443")` is bound. `docker logs ghostframe-server | grep -i 'web\|listen\|443'` should show both `:80` and `:443` started.
- WebTransport fails with `ERR_CERT_AUTHORITY_INVALID` for the page itself → SPKI hash mismatch; recompute base64 of `Sha256::digest(spki_der)`.
- WebTransport fails with `CERTIFICATE_VERIFY_FAILED` for the QUIC handshake → `/config.json` carries the wrong hex; verify with `curl -k https://127.0.0.1:<port>/config.json` from the test harness side.

- [ ] **Step 6: If anything failed, fix, re-run, then commit any fixups.**

- [ ] **Step 7: Final commit (no-op if Step 6 was clean).**

```bash
git status
# If no uncommitted changes, skip the commit. Otherwise:
git add -p  # interactively select fix-up hunks
git commit -m "fix: e2e sweep cleanups from final verification"
```

---

## Open items resolved during planning

- **Spec Open Item #1** (tsnet cert API): Resolved — `tsnet.Server.ListenTLS("tcp", ":443")` and `tsnet.Server.CertDomains() []string` are both in v1.94+. Used in Task 7.
- **Spec Open Item #2** (TCP + UDP on `:443` simultaneously): Stays as a smoke-test concern; verified de facto in Task 20 — the e2e harness's UDP forwarder + TCP forwarder both bind to the same loopback port, and the container's `tsnet.Server` does the same on the inside.
- **Spec Open Item #3** (bootstrap module path): Resolved — `ghostframe-web-client/src/bootstrap.ts`. Wired in Tasks 2 and 12.

## Spec coverage checklist

- D1 (full replacement): Tasks 12, 16, 17, 18 — URL-param flow removed.
- D2 (LE via tsnet LocalClient): Task 7 (`ListenTLS`) + Task 18 (admin URL note).
- D3 (web server in Go): Tasks 4, 5, 7.
- D4 (`//go:embed`): Task 4.
- D5 (QUIC → `:443`): Task 1.
- D6 (`/config.json` for cert hash): Task 4 + Task 2 (client side).
- D7 (fail-fast `CertDomains()` empty): Task 7 + Task 10.
- D8 (e2e env-var cert substitution): Tasks 7, 13, 15, 16.
- D9 (drop `CERT_HASH_SHA256=` stdout): Task 11.
- D10 (build flow): Task 3.
- Failure modes (admin URL on disabled HTTPS, 502, cert rotation): Tasks 7, 10, 18; cert rotation is intentionally a "user reload fixes it" behaviour — no code change.
- E2E coverage table from spec: every row except the cert-source row stays the same code path — verified in Tasks 15-16, sweep-tested in Task 20.
