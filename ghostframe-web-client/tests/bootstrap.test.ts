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

  it('pins every hash in certHashes, so a fetch just before a rotation still connects', async () => {
    vi.stubGlobal('location', { origin: 'https://example.ts.net', host: 'example.ts.net' });
    const a = 'aa'.repeat(32);
    const b = 'bb'.repeat(32);
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ certHash: a, certHashes: [a, b] }),
    }));
    const ctorSpy = vi.fn();
    vi.stubGlobal('WebTransport', class { constructor(...args: unknown[]) { ctorSpy(...args); } });

    const res = await bootstrap();

    const pins = ctorSpy.mock.calls[0][1].serverCertificateHashes;
    expect(pins).toHaveLength(2);
    expect(new Uint8Array(pins[0].value)[0]).toBe(0xaa);
    expect(new Uint8Array(pins[1].value)[0]).toBe(0xbb);
    expect(res.certHashes).toEqual([a, b]);
  });

  it('fetches config.json afresh on every call and forbids the HTTP cache', async () => {
    vi.stubGlobal('location', { origin: 'https://example.ts.net', host: 'example.ts.net' });
    const fetchMock = vi.fn()
      .mockResolvedValueOnce({ ok: true, json: () => Promise.resolve({ certHashes: ['11'.repeat(32)] }) })
      .mockResolvedValueOnce({ ok: true, json: () => Promise.resolve({ certHashes: ['22'.repeat(32)] }) });
    vi.stubGlobal('fetch', fetchMock);
    const ctorSpy = vi.fn();
    vi.stubGlobal('WebTransport', class { constructor(...args: unknown[]) { ctorSpy(...args); } });

    await bootstrap();
    await bootstrap();

    expect(fetchMock).toHaveBeenCalledTimes(2);
    for (const call of fetchMock.mock.calls) {
      expect(call[1]).toMatchObject({ cache: 'no-store' });
    }
    expect(new Uint8Array(ctorSpy.mock.calls[1][1].serverCertificateHashes[0].value)[0]).toBe(0x22);
  });

  it('falls back to certHash when certHashes is present but malformed', async () => {
    vi.stubGlobal('location', { origin: 'https://example.ts.net', host: 'example.ts.net' });
    const a = 'aa'.repeat(32);
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ certHash: a, certHashes: ['zz'.repeat(32)] }),
    }));
    const ctorSpy = vi.fn();
    vi.stubGlobal('WebTransport', class { constructor(...args: unknown[]) { ctorSpy(...args); } });

    const res = await bootstrap();

    expect(res.certHashes).toEqual([a]);
    expect(ctorSpy.mock.calls[0][1].serverCertificateHashes).toHaveLength(1);
  });
});
