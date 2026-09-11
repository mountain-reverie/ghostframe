import { describe, it, expect } from 'vitest';
import { WasmDecodeErrorBatcher, errorCodes } from '../pkg-node/ghostframe_client_wasm.js';

const ERR_THIN_UNCACHED_PALETTE = errorCodes().ERR_THIN_UNCACHED_PALETTE;

function collect(b: WasmDecodeErrorBatcher, emitted: Uint8Array[], ...args: Parameters<WasmDecodeErrorBatcher['report']>) {
  const out = b.report(...args);
  if (out !== undefined) emitted.push(out);
}

describe('DecodeErrorBatcher', () => {
  it('emits the first error', () => {
    const emitted: Uint8Array[] = [];
    const b = new WasmDecodeErrorBatcher();
    collect(b, emitted, 2, 3, 4, ERR_THIN_UNCACHED_PALETTE, 0n);
    expect(emitted.length).toBe(1);
  });

  it('drops a duplicate (codec, tile) within 1000 ms', () => {
    const emitted: Uint8Array[] = [];
    const b = new WasmDecodeErrorBatcher();
    collect(b, emitted, 2, 3, 4, 3, 0n);
    collect(b, emitted, 2, 3, 4, 3, 500_000n);
    expect(emitted.length).toBe(1);
  });

  it('allows the same (codec, tile) after the 1000 ms window', () => {
    const emitted: Uint8Array[] = [];
    const b = new WasmDecodeErrorBatcher();
    collect(b, emitted, 2, 3, 4, 3, 0n);
    collect(b, emitted, 2, 3, 4, 3, 1_001_000n);
    expect(emitted.length).toBe(2);
  });

  it('allows distinct (codec, tile) entries inside the window', () => {
    const emitted: Uint8Array[] = [];
    const b = new WasmDecodeErrorBatcher();
    for (let i = 0; i < 10; i++) {
      collect(b, emitted, 2, i, 4, 3, 0n);
    }
    expect(emitted.length).toBe(10);
  });

  it('drops above the global cap (32/sec)', () => {
    const emitted: Uint8Array[] = [];
    const b = new WasmDecodeErrorBatcher();
    // 40 distinct keys — per-key cap doesn't kick in, but global does at 32.
    for (let i = 0; i < 40; i++) {
      collect(b, emitted, 2, i, 0, 3, 0n);
    }
    expect(emitted.length).toBe(32);
  });

  it('replenishes the global cap after the 1000 ms window', () => {
    const emitted: Uint8Array[] = [];
    const b = new WasmDecodeErrorBatcher();
    for (let i = 0; i < 40; i++) {
      collect(b, emitted, 2, i, 0, 3, 0n);
    }
    expect(emitted.length).toBe(32);
    collect(b, emitted, 2, 100, 0, 3, 1_001_000n);
    expect(emitted.length).toBe(33);
  });
});
