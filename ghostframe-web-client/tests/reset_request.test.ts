// Decode failures ask the server for an IDR (feedback stream 0x09), paced like
// the native renderer: with a 3 s GOP, waiting for the natural keyframe leaves
// the picture broken for seconds.
import { describe, it, expect } from 'vitest';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';
import { RESET_REQUEST_MIN_INTERVAL_MS, ResetPacer } from '../src/decoder.js';

describe('requestReset through the wasm core', () => {
  it('queues a 0x09 decoder-failure request on the stream', () => {
    const core = new wasm.WasmClientCore(true, true, true, 0n);
    // Drain the HELLO.
    while (core.pollTransmit(0n) !== undefined) {}
    core.requestReset(1, 0x01020304);
    const out = core.pollTransmit(0n);
    expect(out).toBeDefined();
    expect(out.kind).toBe('Stream');
    expect(out.bytes[0]).toBe(0x09);
    expect(out.bytes[1]).toBe(1); // ResetReason::DecoderFailure
    expect(Array.from(out.bytes.slice(2, 6))).toEqual([4, 3, 2, 1]); // little-endian
  });

  it('ignores a reason it does not know', () => {
    const core = new wasm.WasmClientCore(true, true, true, 0n);
    while (core.pollTransmit(0n) !== undefined) {}
    core.requestReset(200, 1);
    expect(core.pollTransmit(0n)).toBeUndefined();
  });
});

describe('ResetPacer', () => {
  it('lets the first request out and spaces the rest by 2.2 s', () => {
    const p = new ResetPacer();
    expect(p.allow(10_000)).toBe(true);
    expect(p.allow(10_000 + 2_199)).toBe(false);
    expect(p.allow(10_000 + 2_200)).toBe(true);
    expect(p.allow(10_000 + 2_201)).toBe(false);
  });

  it('is the same interval as the native renderer', () => {
    expect(RESET_REQUEST_MIN_INTERVAL_MS).toBe(2200);
  });
});
