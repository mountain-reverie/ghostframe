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
