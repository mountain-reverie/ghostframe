const statusEl = document.getElementById('status')!;
const logEl = document.getElementById('log')!;

function log(msg: string) {
  const line = document.createElement('div');
  line.textContent = msg;
  logEl.appendChild(line);
}

async function main() {
  // The `host` parameter is expected to already include the port, because
  // the E2E test's loopback UDP forwarder binds to 127.0.0.1:<random>. If no
  // host is passed, fall back to the production tailnet hostname + default
  // port so a developer can load the page manually against a running xdaemon.
  const url = new URL(window.location.href);
  const serverHost = url.searchParams.get('host') ?? 'ghostframe-server:4443';
  const certHash = url.searchParams.get('certHash') ?? '';

  const wtUrl = `https://${serverHost}/`;

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
  const writer = transport.datagrams.writable.getWriter();
  await writer.write(pingData);
  writer.releaseLock();
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
