// SPDX-License-Identifier: MIT OR Apache-2.0
// Reset requests (feedback stream 0x09) through the wasm core. Decode failures
// no longer come through here: the gate sends and paces them (see
// h264_result.test.ts).
import { describe, it, expect } from 'vitest';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';

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
