// Proves the wasm module loads under vitest's node environment and that it
// was built from the current workspace sources. Everything else about the
// wasm is tested by the retargeted suites; this is the load-bearing check
// that they are testing today's Rust.
import { describe, it, expect } from 'vitest';
import { protocolStamp } from '../scripts/protocol_stamp.mjs';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';

describe('wasm module', () => {
  it('loads and constructs a core', () => {
    const core = new wasm.WasmClientCore(false, false, true, 0n);
    expect(core).toBeDefined();
  });

  it('emits the Hello message on the stream at construction', () => {
    const core = new wasm.WasmClientCore(true, true, true, 0n);
    const out = core.pollTransmit(0n);
    expect(out).toBeDefined();
    expect(out.kind).toBe('Stream');
    // [0x03, caps]; bit0 = indices_raw_enabled, bit1 = supports_h264.
    expect(out.bytes[0]).toBe(0x03);
    expect(out.bytes[1] & 0b11).toBe(0b11);
  });

  it('was built from the current client-core and protocol sources', () => {
    expect(wasm.protocol_stamp()).toBe(protocolStamp());
  });

  it('encodes input without a session', () => {
    expect(Array.from(wasm.encodePointerMove(1, 2))).toHaveLength(6);
    expect(wasm.keyToKeysym('Enter')).toBeDefined();
  });
});
