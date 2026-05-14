import { describe, it, expect } from 'vitest';
import { DecodeErrorBatcher } from '../src/decode_error_batcher.js';
import { ERR_THIN_UNCACHED_PALETTE } from '../src/feedback.js';

function makeClock() {
  let t = 0;
  return {
    now: () => t,
    advance: (ms: number) => { t += ms; },
  };
}

describe('DecodeErrorBatcher', () => {
  it('emits the first error', () => {
    const emitted: Uint8Array[] = [];
    const clock = makeClock();
    const b = new DecodeErrorBatcher(
      (bytes) => emitted.push(bytes),
      clock.now,
    );
    b.report({ codec: 2, tileX: 3, tileY: 4, errorCode: ERR_THIN_UNCACHED_PALETTE });
    expect(emitted.length).toBe(1);
  });

  it('drops a duplicate (codec, tile) within 1000 ms', () => {
    const emitted: Uint8Array[] = [];
    const clock = makeClock();
    const b = new DecodeErrorBatcher((bytes) => emitted.push(bytes), clock.now);
    b.report({ codec: 2, tileX: 3, tileY: 4, errorCode: 3 });
    clock.advance(500);
    b.report({ codec: 2, tileX: 3, tileY: 4, errorCode: 3 });
    expect(emitted.length).toBe(1);
  });

  it('allows the same (codec, tile) after the 1000 ms window', () => {
    const emitted: Uint8Array[] = [];
    const clock = makeClock();
    const b = new DecodeErrorBatcher((bytes) => emitted.push(bytes), clock.now);
    b.report({ codec: 2, tileX: 3, tileY: 4, errorCode: 3 });
    clock.advance(1001);
    b.report({ codec: 2, tileX: 3, tileY: 4, errorCode: 3 });
    expect(emitted.length).toBe(2);
  });

  it('allows distinct (codec, tile) entries inside the window', () => {
    const emitted: Uint8Array[] = [];
    const clock = makeClock();
    const b = new DecodeErrorBatcher((bytes) => emitted.push(bytes), clock.now);
    for (let i = 0; i < 10; i++) {
      b.report({ codec: 2, tileX: i, tileY: 4, errorCode: 3 });
    }
    expect(emitted.length).toBe(10);
  });

  it('drops above the global cap (32/sec)', () => {
    const emitted: Uint8Array[] = [];
    const clock = makeClock();
    const b = new DecodeErrorBatcher((bytes) => emitted.push(bytes), clock.now);
    // 40 distinct keys — per-key cap doesn't kick in, but global does at 32.
    for (let i = 0; i < 40; i++) {
      b.report({ codec: 2, tileX: i, tileY: 0, errorCode: 3 });
    }
    expect(emitted.length).toBe(32);
  });

  it('replenishes the global cap after the 1000 ms window', () => {
    const emitted: Uint8Array[] = [];
    const clock = makeClock();
    const b = new DecodeErrorBatcher((bytes) => emitted.push(bytes), clock.now);
    for (let i = 0; i < 40; i++) {
      b.report({ codec: 2, tileX: i, tileY: 0, errorCode: 3 });
    }
    expect(emitted.length).toBe(32);
    clock.advance(1001);
    b.report({ codec: 2, tileX: 100, tileY: 0, errorCode: 3 });
    expect(emitted.length).toBe(33);
  });
});
