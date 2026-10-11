// handleDatagrams (one wasm crossing per rAF) must be indistinguishable from
// feeding the same datagrams one at a time: same events, same order, same
// core state afterwards. Anything else would make the batching a behaviour
// change rather than a cost change.
import { describe, it, expect } from 'vitest';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';
import { fragmentTile } from './helpers/wasm.js';
import { protocol } from '../src/protocol_constants';

function newCore() {
  // Decoded delivery: Raw tiles complete as TileReady events.
  return new wasm.WasmClientCore(false, false, false, 0n);
}

function scene(): Uint8Array[] {
  const out: Uint8Array[] = [];
  const raw = protocol().codec.Raw;
  for (let t = 0; t < 5; t++) {
    const payload = new Uint8Array(64).map((_, i) => (i * 7 + t * 13) & 0xff);
    // Small fragments so each tile needs several datagrams, in
    // sequence across the batch.
    out.push(...fragmentTile(1 + t, t, 2, raw, 1, 0, payload, 24));
  }
  // Interleave: round-robin over the per-tile fragment lists is enough to
  // vary arrival order versus generation order.
  return out.reverse();
}

function concat(dgs: Uint8Array[]): { bytes: Uint8Array; lens: Uint32Array } {
  const bytes = new Uint8Array(dgs.reduce((n, d) => n + d.length, 0));
  const lens = new Uint32Array(dgs.length);
  let off = 0;
  dgs.forEach((d, i) => {
    bytes.set(d, off);
    lens[i] = d.length;
    off += d.length;
  });
  return { bytes, lens };
}

describe('handleDatagrams', () => {
  it('yields the same events as per-datagram handleDatagram', () => {
    const dgs = scene();
    const seqCore = newCore();
    const expected: unknown[] = [];
    dgs.forEach((d, i) => expected.push(...(seqCore.handleDatagram(d, BigInt(1000 + i)) as unknown[])));
    // Non-vacuous: the scene must actually complete tiles.
    expect(expected.length).toBeGreaterThan(0);
    expect((expected as Array<{ kind: string }>).every((e) => e.kind === 'TileReady')).toBe(true);

    const batchCore = newCore();
    const { bytes, lens } = concat(dgs);
    const arrivals = Float64Array.from(dgs.map((_, i) => 1000 + i));
    const got = batchCore.handleDatagrams(bytes, lens, arrivals) as unknown[];
    expect(got).toEqual(expected);

    // Same core state afterwards: counters agree.
    expect(batchCore.datagramCounts()).toEqual(seqCore.datagramCounts());
  });

  it('tolerates length arrays that disagree with the buffer', () => {
    const dgs = scene().slice(0, 4);
    const { bytes, lens } = concat(dgs);
    const core = newCore();
    // Claim more length than exists in the last entry: must not throw, and
    // the consistent prefix is still processed.
    const lying = Uint32Array.from(lens);
    lying[lying.length - 1] += 1000;
    expect(() => core.handleDatagrams(bytes, lying, new Float64Array(dgs.length))).not.toThrow();
    expect(core.datagramCounts()).toEqual({ tileDatagrams: dgs.length - 1, frameDatagrams: 0 });
    // Fewer arrival stamps than lengths: processes only the paired prefix.
    const core2 = newCore();
    core2.handleDatagrams(bytes, lens, new Float64Array(2));
    expect(core2.datagramCounts()).toEqual({ tileDatagrams: 2, frameDatagrams: 0 });
    // NaN / negative stamps must not wrap or throw.
    const core3 = newCore();
    expect(() => core3.handleDatagrams(bytes, lens, Float64Array.from(dgs.map(() => NaN)))).not.toThrow();
  });
});
