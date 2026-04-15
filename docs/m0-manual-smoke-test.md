# M0 manual smoke test

This doc describes how to verify the M0 ping/pong end-to-end on a developer
machine against a real Tailscale tailnet, without the headscale-in-docker
test harness that Task 9/11 builds out.

## Prerequisites

- A Tailscale account with a reusable pre-auth key
  (https://login.tailscale.com/admin/settings/keys).
- A Chromium-based browser (Chrome, Chromium, Edge).
- The ghostframe workspace built: `cargo build --release -p ghostframe-xdaemon`.
- The web client built: `cd ghostframe-web-client && npm install && npm run build`.

## 1. Start the xdaemon

```bash
export TS_AUTHKEY=tskey-auth-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
export TS_HOSTNAME=ghostframe-m0-dev
export TS_STATE_DIR=/tmp/ghostframe-ts-m0-dev
./target/release/ghostframe-xdaemon
```

The daemon joins the tailnet and prints a line like:

```
CERT_HASH_SHA256=b3f1c28d...{64 hex chars}
```

Copy that hash — the browser page needs it to pin the self-signed cert.

In the Tailscale admin UI, confirm that a new node named `ghostframe-m0-dev`
has appeared and note its tailnet name (e.g.
`ghostframe-m0-dev.tail-scale.ts.net`).

## 2. Serve the web client over HTTP on 127.0.0.1

WebTransport requires a secure context. `file://` is not reliably secure in
Chromium, but `http://127.0.0.1` is. A one-line Python server is enough:

```bash
cd ghostframe-web-client/dist
python3 -m http.server 8000 --bind 127.0.0.1
```

## 3. Open the browser

Navigate to:

```
http://127.0.0.1:8000/index.html?host=ghostframe-m0-dev.<tailnet>.ts.net:4443&certHash=<64-hex>
```

Replace `<tailnet>` with your tailnet name and `<64-hex>` with the cert hash
printed in step 1.

Expected page state:

- `status` element transitions from `Connecting...` → `Connected` → `Ping/Pong successful!`
- The log shows:
  - `Connecting to https://.../`
  - `Connected!`
  - `Sent: ping`
  - `Received: pong (4 bytes)`
  - `M0 COMPLETE: Ping/Pong datagram round-trip verified!`

Browser DevTools → Network panel should show a WebTransport session to the
tailnet hostname.

## Troubleshooting

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| `WebTransport is not defined` | Browser too old. | Use Chrome/Chromium 97+ or Edge 97+. |
| `net::ERR_QUIC_PROTOCOL_ERROR` | Cert hash wrong or missing. | Re-copy from the xdaemon stdout line. |
| `net::ERR_CERT_DATE_INVALID` | Server cert expired; we use `generate_simple_self_signed` which issues a long-lived cert, but clock skew can still trip this. | Check system clock. |
| Page loads but never reaches `Connected`. | The daemon did not fully join the tailnet or magicDNS has not propagated. | Wait 5–10s and retry. Run `tailscale status` on another node to confirm the daemon is visible. |
| Firewall blocks UDP to tailnet IP. | Host firewall rule. | Tailscale uses UDP on ephemeral ports; make sure `ghostframe-xdaemon` is not blocked. |
| "Sent: ping" but no pong. | Either the daemon is not processing the datagram, or the browser is reading datagrams from a different session. | Check `TS_AUTHKEY` permissions; check `RUST_LOG=ghostframe=debug` output for `"ping received, sending pong"`. |

## What this smoke test proves

- Real tsnet join (via the ghostbridge Go archive).
- Real QUIC handshake with the self-signed cert pinned via
  `serverCertificateHashes`.
- Real HTTP/3 SETTINGS exchange.
- Real WebTransport CONNECT.
- Real ping/pong datagram round-trip.

It does NOT prove headscale compatibility (Task 11 does) or
multi-connection fairness (future milestones).
