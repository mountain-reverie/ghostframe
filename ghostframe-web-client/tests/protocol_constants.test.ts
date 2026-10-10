// The constants module is a thin, frozen view of what Rust exports. These
// tests pin that it really is Rust's value (not a copy), and pin the exact
// numbers that are part of the wire contract so a silent change on the Rust
// side is a visible, reviewed diff here.
import { describe, it, expect, vi } from 'vitest';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';
import { initProtocol, protocol } from '../src/protocol_constants';

describe('protocol constants', () => {
  const p = protocol();

  it('reads every value from the wasm getters', () => {
    expect(p.tileSize).toBe(wasm.tileSize());
    expect(p.datagramHeaderSize).toBe(wasm.datagramHeaderSize());
    expect(p.tileHeaderSize).toBe(wasm.tileHeaderSize());
    expect(p.frameHeaderSize).toBe(wasm.frameHeaderSize());
    expect(p.cdf53PassCount).toBe(wasm.cdf53PassCount());
    expect(p.inputMsgType).toBe(wasm.inputMsgType());
    expect(p.helloCapIndicesRaw).toBe(wasm.helloCapIndicesRaw());
  });

  it('is frozen', () => {
    expect(Object.isFrozen(p)).toBe(true);
    expect(Object.isFrozen(p.codec)).toBe(true);
  });

  it('pins the wire contract', () => {
    expect(p.tileSize).toBe(32);
    expect(p.tileBytes).toBe(32 * 32 * 4);
    expect(p.datagramHeaderSize).toBe(16);
    expect(p.tileHeaderSize).toBe(8);
    expect(p.frameHeaderSize).toBe(14);
    expect(p.tileDatagramFlag).toBe(0x80000000);
    expect(p.cdf53PassCount).toBe(14);
    expect(p.cdf53FullPassMask).toBe(0x3fff);
    expect(p.frameDimensionsSentinelX).toBe(0xff);
    expect(p.evictionSentinelX).toBe(0xfe);
    expect(p.evictionReasonDisplaced).toBe(1);
    expect(p.codec).toEqual({ Skip: 0, H264: 1, PalRle: 2, Solid: 3, Raw: 4, Cdf53: 5 });
  });

  it('feedback-stream message types are distinct', () => {
    const types = [
      p.feedbackMsgType,
      p.helloMsgType,
      p.decodeErrorMsgType,
      p.inputMsgType,
      p.displayInfoMsgType,
      p.displayModeMsgType,
    ];
    expect(new Set(types).size).toBe(types.length);
  });

  it('HELLO capability bits are disjoint single bits and agree with the core', () => {
    expect(p.helloCapIndicesRaw & p.helloCapSupportsH264).toBe(0);
    expect(p.helloCapsReservedMask | p.helloCapIndicesRaw | p.helloCapSupportsH264).toBe(0xff);
    // Cross-check against what the core actually puts on the wire.
    const core = new wasm.WasmClientCore(true, true, true, 0n);
    const hello = core.pollTransmit(0n);
    expect(hello.bytes[0]).toBe(p.helloMsgType);
    expect(hello.bytes[1]).toBe(p.helloCapIndicesRaw | p.helloCapSupportsH264);
    expect(hello.bytes.length).toBe(p.helloSize);
  });

  it('palette fingerprint matches the Rust golden', () => {
    expect(p.paletteFingerprint(new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]))).toBe(0xa9388a74);
  });

  it('protocol() before init throws loudly', async () => {
    vi.resetModules();
    const fresh = await import('../src/protocol_constants');
    expect(() => fresh.protocol()).toThrow(/before initProtocol/);
  });

  it('initProtocol is idempotent', () => {
    expect(JSON.stringify(initProtocol(wasm as never))).toBe(JSON.stringify(p));
  });
});
