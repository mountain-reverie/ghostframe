export interface BootstrapResult {
  transport: WebTransport;
  /** The hash being served when the config was fetched. */
  certHash: string;
  /** Every hash pinned: current first, then the successor certificate. */
  certHashes: string[];
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
  // Fetched fresh on every call, never cached: the server rotates its
  // certificate, and a hash remembered from an earlier day pins a cert that
  // is gone. (`no-store` also overrides any intermediary's heuristics.)
  const resp = await fetch('/config.json', { cache: 'no-store' });
  if (!resp.ok) {
    throw new Error(`bootstrap: /config.json returned HTTP ${resp.status}`);
  }
  const cfg = await resp.json();
  // `certHashes` is the full pin list (current + next). Older servers only
  // send `certHash`.
  const hashes: string[] =
    Array.isArray(cfg.certHashes) && cfg.certHashes.length > 0
      ? cfg.certHashes
      : typeof cfg.certHash === 'string' && cfg.certHash.length > 0
        ? [cfg.certHash]
        : [];
  if (hashes.length === 0 || !hashes.every((h) => typeof h === 'string' && h.length > 0)) {
    throw new Error('bootstrap: /config.json missing certHash');
  }
  const url = `https://${location.host}/`;
  const transport = new WebTransport(url, {
    serverCertificateHashes: hashes.map((h) => ({
      algorithm: 'sha-256',
      value: hexToBytes(h).buffer as ArrayBuffer,
    })),
  });
  return { transport, certHash: hashes[0], certHashes: hashes };
}
