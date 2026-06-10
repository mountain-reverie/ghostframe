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
    out[i / 2] = parseInt(hex.slice(i, i + 2), 16);
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
    serverCertificateHashes: [{ algorithm: 'sha-256', value: bytes.buffer as ArrayBuffer }],
  });
  return { transport, certHash: cfg.certHash };
}
