# Tailnet-Served Web Client: Design

**Date:** 2026-06-09
**Status:** Design approved
**Predecessors:** none directly; this is post-M3 feature work, independent of the codec suite.

---

## Context

Today's first-connection UX is hostile:

1. SSH to the host, `journalctl --user -u ghostframe-xdaemon | grep CERT_HASH_SHA256` to find the WebTransport cert hash.
2. Stand up a local Python `http.server` on `127.0.0.1:8000` to serve `ghostframe-web-client/dist/`.
3. Open `http://127.0.0.1:8000/?host=<hostname>-ghostframe.<tailnet>.ts.net:4443&certHash=<64-hex>`.

The daemon already speaks tsnet — it has a `tsnet.Server` joined to the tailnet and a self-signed WebTransport cert in memory. There's no reason the same daemon shouldn't also serve the static client bundle and tell the client its own cert hash. Tailnet membership already gates access, and Tailscale's MagicDNS integration provisions real Let's Encrypt certs for `*.ts.net` names, so we can do this without manual cert distribution or browser interstitials.

After this change, "first connection" is: open `https://<hostname>.<tailnet>.ts.net/` in Chrome. Nothing else.

---

## Decision Register

| # | Decision | Rationale |
|---|---|---|
| D1 | Full replacement of the manual flow — no URL-param fallback in the web client | One supported path; avoids dual-mode bootstrap code |
| D2 | HTTPS via tsnet's `LocalClient` Let's Encrypt integration (not self-signed) | Real cert avoids browser interstitials; required for WebTransport secure-context |
| D3 | Web server lives in `ghostbridge` (Go), not Rust | tsnet listener and the LE cert hook are both Go-side; avoids straddling the TLS handshake across FFI |
| D4 | Static assets embedded into the ghostbridge Go binary via `//go:embed` | Single binary; no install-path coordination; build-time freshness guaranteed |
| D5 | QUIC moves from `:4443` to `:443` (same number as the HTTPS listener; different transport) | Wire URL collapses to bare-hostname HTTPS; no explicit port at all |
| D6 | Cert hash delivered to the client via `GET /config.json` (JSON), not templated into `index.html` | `dist/` stays a static blob; no Go templating; one extra `fetch()` at page load |
| D7 | Fail-fast at startup if `tsnet.Server.CertDomains()` is empty (HTTPS Certs not enabled in tailnet admin) | Configuration error must be surfaced before users get a confusing browser failure |
| D8 | E2E uses real Go web server code; substitutes only the TLS cert source via env vars | Maximises code coverage; only the LE step (which CI can't reach) is bypassed |
| D9 | `CERT_HASH_SHA256=…` stdout line is removed | E2E reads the hash from the same `/config.json` prod uses |
| D10 | Build flow: `npm run build` must run before `cargo build`; ghostbridge Makefile fails fast with a clear message; Justfile recipe orchestrates | Keeps `cargo build` pure; avoids dragging Node into every incremental cargo invocation |

---

## Architecture

### Components

**Modified:**
- `ghostbridge/main.go` — new exported FFI `gbridge_start_web_server`; new `embed.FS` for `dist/`; new `:443` HTTPS listener with `tsnet`-sourced cert; new `:80` redirect listener; new HTTP handlers for `/config.json` and static files.
- `ghostframe-lib/src/transport/ghostbridge.rs` — Rust binding for the new FFI export.
- `ghostframe-lib/src/server.rs` — calls the new FFI after the QUIC cert is generated; threads cert hash through.
- `ghostframe-lib/src/transport/quic.rs` and `ghostframe-xdaemon/src/main.rs` — QUIC port `:4443` → `:443`.
- `ghostframe-web-client/src/` (bootstrap module — exact file TBD during implementation) — stop parsing `host`/`certHash` URL params; fetch `/config.json` on load; construct `WebTransport` same-origin.
- `ghostframe-xdaemon/src/main.rs` — drop the `println!("CERT_HASH_SHA256=…")` line.
- `README.md` — rewrite "First connection" section to a single URL.
- `packaging/install.sh` — drop manual cert-hash and Python-server instructions; add "enable HTTPS Certificates in tailnet admin" note in final output.
- `Justfile` — add `build-web` recipe (`cd ghostframe-web-client && npm ci && npm run build`); add `build` recipe (chains `build-web` then cargo).

**No changes to:** capture pipeline, codec suite, QUIC/WebTransport protocol layers, scheduler, classifier, renderer.

### Data flow at startup

```
Rust  GhostframeServer::new(config, ":443")
  ├─ generates self-signed WebTransport cert (rcgen, Ed25519, ≤14d)
  ├─ ghostbridge_create(config) ─→ Go: tsnet.Server.Up(); join tailnet
  ├─ gbridge_listen_udp(":443")   ─→ Go: tsnet.Server.ListenPacket("udp", ":443")
  └─ gbridge_start_web_server(cert_hash_hex)   [NEW]
        Go:
         ├─ check CertDomains() non-empty
         │     └─ if empty AND no test-cert env vars set:
         │           return GBRIDGE_STATUS_HTTPS_CERTS_DISABLED
         ├─ build http.ServeMux:
         │     GET /config.json → JSON {certHash}
         │     GET /*           → http.FileServer(http.FS(embedded dist/))
         ├─ tsnet.Server.Listen("tcp", ":443")
         │     TLS via LocalClient.GetCertificate (prod)
         │            or static cert from env vars (e2e)
         ├─ tsnet.Server.Listen("tcp", ":80")
         │     redirect handler → 301 to https://Host/path
         └─ run both http.Servers in goroutines; return OK
```

### Data flow at first connection

```
Browser  https://<host>.<tailnet>.ts.net/
  └─ TCP :443 → tsnet TCP listener → http.Server → embedded index.html
       └─ JS bootstrap:
            ├─ fetch('/config.json') → {certHash: "<hex>"}
            ├─ hex → Uint8Array
            └─ new WebTransport(`https://${location.host}/`, {
                  serverCertificateHashes: [{algorithm: "sha-256", value: bytes}]
               })
                  └─ UDP :443 → tsnet UDP listener → existing QUIC/WT path
```

### FFI surface

New Go export (extends the existing `gbridge_*` family in `ghostbridge/main.go`):

```c
typedef enum {
    GBRIDGE_STATUS_OK = 0,
    GBRIDGE_STATUS_INVALID_ARG = 1,
    /* … existing codes … */
    GBRIDGE_STATUS_HTTPS_CERTS_DISABLED = 10,   /* NEW */
    GBRIDGE_STATUS_WEB_LISTEN_FAILED  = 11,     /* NEW */
} gbridge_status;

/* Starts the embedded HTTPS web server on tsnet :443 + :80 redirect.
 * cert_hash_hex must be the lowercase-hex SHA-256 fingerprint of the
 * WebTransport cert. Returns OK on success.
 *
 * Cert source: by default, tsnet.Server.LocalClient().GetCertificate.
 * If env vars GHOSTFRAME_WEB_TLS_CERT_PEM / GHOSTFRAME_WEB_TLS_KEY_PEM
 * are set, those PEMs are used instead and the CertDomains() check is
 * skipped (e2e only).
 */
gbridge_status gbridge_start_web_server(
    gbridge_session_descriptor sd,
    const char* cert_hash_hex);
```

Rust side: `GhostframeServer::new` calls this once after generating the QUIC cert. Failure exits the daemon with `tracing::error!` and exit code 2.

### Wire shape

**`/config.json`:**
```json
{ "certHash": "ab12…" }
```

Lowercase hex, 64 chars. No `wtPort` field — same-origin and same port (`:443`).

**Web client construction:**
```ts
const cfg = await (await fetch('/config.json')).json();
const hashBytes = hexToBytes(cfg.certHash);
const wt = new WebTransport(`https://${location.host}/`, {
  serverCertificateHashes: [{ algorithm: "sha-256", value: hashBytes }]
});
```

The web client never reads URL params for `host` or `certHash`. The bootstrap module's responsibility shrinks to "fetch config, construct WT, hand off to existing connection logic".

### Failure modes

| Condition | Behaviour |
|---|---|
| HTTPS Certificates not enabled in tailnet admin (and no test env vars) | Daemon exits 2 at startup with `tracing::error!` pointing to `https://login.tailscale.com/admin/dns` |
| `tsnet.Server.Listen("tcp", ":443")` fails (port already bound — shouldn't happen on userspace netstack but defensively handled) | Daemon exits 2 with `GBRIDGE_STATUS_WEB_LISTEN_FAILED` |
| Let's Encrypt provisioning fails on first request | tsnet's `GetCertificate` returns an error; browser shows a TLS error; daemon logs at `warn`. Self-healing on subsequent requests once tsnet's cert cache populates |
| Browser tab open across WebTransport cert rotation (every ≤14d) | Cached `certHash` in tab memory is stale; WT reconnect fails; page reload re-fetches `/config.json`. No worse than today's stale-URL behaviour |
| Daemon restarted mid-session | Same as cert rotation |
| User loads `http://<host>.../` (no scheme) | tsnet `:80` listener returns 301 to `https://` |

---

## Build flow

ghostbridge's `//go:embed` requires `../ghostframe-web-client/dist/` to be populated and have an `index.html` before `go build` runs. We codify this in three places:

1. **`Justfile`**:
   ```
   build-web:
       cd ghostframe-web-client && npm ci && npm run build

   build: build-web
       cargo build --release -p ghostframe-xdaemon
   ```

2. **`ghostbridge/Makefile`** — fail fast with a clear message if `../ghostframe-web-client/dist/index.html` is missing:
   ```
   ../ghostframe-web-client/dist/index.html:
       @echo "error: ghostframe-web-client/dist not built" >&2
       @echo "  Run: just build-web    (or: cd ghostframe-web-client && npm ci && npm run build)" >&2
       @exit 1

   libghostbridge.a: ../ghostframe-web-client/dist/index.html main.go
       go build …
   ```

3. **`ghostbridge/build.rs`** — `cargo:rerun-if-changed=../ghostframe-web-client/dist` so changes to the built bundle re-link.

The README + DEVELOPERS install commands switch to `just build`.

---

## E2E testing strategy

The CI/Docker environment uses headscale, which does not provision Let's Encrypt certs for `*.ts.net`. To still exercise the production Go web server path, we substitute only the cert source:

- **Env vars (e2e-only):** `GHOSTFRAME_WEB_TLS_CERT_PEM` and `GHOSTFRAME_WEB_TLS_KEY_PEM`. When both are set, ghostbridge skips the `CertDomains()` check and uses these PEMs in its `tls.Config.GetCertificate`. Follows the existing `GHOSTFRAME_*` env-var test-injection convention (compare `GHOSTFRAME_DIAGNOSE_TILES`, `GHOSTFRAME_INJECT_OOB_PALRLE`).
- **E2E harness extension:** generate a self-signed cert with SAN matching the daemon's tsnet FQDN at test setup; inject as env vars into the daemon container; compute the cert's SPKI hash; launch Chrome with `--ignore-certificate-errors-spki-list=<spki>` (surgical — does not blanket-disable cert checks).
- **Test flow:** Chrome navigates to `https://<TS_HOSTNAME>.<tailnet>.ts.net/` (real headscale tsnet route) → hits the real Go `:443` listener → real embedded `index.html` → real `/config.json` → real WebTransport handshake to the same `:443` UDP.

**Coverage table:**

| Component | Prod | E2E |
|---|---|---|
| ghostbridge `:443` TCP listener on tsnet | ✓ | ✓ |
| `:80` → `:443` redirect | ✓ | ✓ |
| Embedded `dist/` file serving | ✓ | ✓ |
| `/config.json` handler | ✓ | ✓ |
| New FFI `gbridge_start_web_server` | ✓ | ✓ |
| Web client `fetch('/config.json')` → WT bootstrap | ✓ | ✓ |
| `CertDomains()` fail-fast check | ✓ | bypassed when test env vars set |
| Cert source: tsnet LE | ✓ | static self-signed |

The only e2e-prod gap is the cert *source*. Every code path the user hits in prod runs in e2e.

### New unit / integration tests

- **`ghostbridge/main_test.go`** (Go): handlers serve embedded `index.html`; `/config.json` reflects the hex passed to `gbridge_start_web_server`; `:80` returns a 301 to `:443` preserving path and query.
- **`ghostframe-web-client/tests/`**: bootstrap path fetches `/config.json`, hex-decodes correctly, constructs `WebTransport` with the expected `serverCertificateHashes`. Visible-error path when `/config.json` is malformed or absent (not a silent hang).
- **`ghostframe-lib`**: Rust round-trip of the new FFI — both success path and the new error codes.
- **`ghostframe-e2e`**: existing e2e tests adapted to the localhost-of-config-json flow (the harness now generates a cert and sets env vars; tests themselves mostly unchanged since they exercise the post-connection capture/render path).

---

## Migration

- The `CERT_HASH_SHA256=…` stdout line in `ghostframe-xdaemon/src/main.rs` is removed.
- `README.md`'s "First connection" section collapses to a single sentence + URL:

  > **First connection.** On any device on the same tailnet, open `https://<hostname>.<tailnet>.ts.net/` in Chrome / Chromium / Edge. (`<hostname>` is the value of `TS_HOSTNAME`, default `ghostframe-server`.)

- `packaging/install.sh` final output prints that URL and one line:

  > **Note:** HTTPS Certificates must be enabled in your tailnet at <https://login.tailscale.com/admin/dns>.

- The `ghostframe-web-client/dist/` build artifact is still produced, but is no longer a deployable separate from the daemon binary; its only consumer is the ghostbridge `//go:embed`.

No backwards-compatibility shims. The legacy URL-param flow is removed in the same change.

---

## Open items (to verify during implementation, not blockers)

1. **Exact tsnet cert API.** The design assumes `tsnet.Server.LocalClient().GetCertificate` (or an equivalent method on `*tsnet.Server`). Verify against the version of `tailscale.com/tsnet` currently pinned in `ghostbridge/go.mod`. If the API has moved, adjust D2's wiring; the rest of the design stands.
2. **TCP + UDP on `:443` simultaneously in tsnet's userspace netstack.** Conceptually independent transports, but worth a small smoke test (bind both, hit both) before declaring the QUIC `:4443` → `:443` move "free". If tsnet has a constraint we're not aware of, fall back to keeping QUIC on `:4443` and adding `wtPort: 4443` to `/config.json`.
3. **Web client bootstrap module path.** The bootstrap file in `ghostframe-web-client/src/` that today reads URL params is the modification target; exact path resolved during plan.
