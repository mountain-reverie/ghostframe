// Was tile_key.test.ts, which pinned the string shape produced by the old
// TS `tileKey()` in decoder.ts: `` `${frameSeq}:${tileX}:${tileY}:${passIdx}` ``.
//
// Rust has no string key. `TileKey` (ghostframe-client-core/src/event.rs) is
// a `#[derive(Hash, Eq)]` struct — frame_seq/tile_x/tile_y/pass_idx used
// directly as a HashMap key — which makes the cross-pass collision this
// suite guards impossible by construction rather than merely tested against.
// `ghostframe-client-core/tests/oracle_tile_key.rs` proves that on the Rust
// side with the struct directly.
//
// What's left to prove here is that the same isolation holds *across the
// wasm boundary* — that reassembly.rs actually keys its assembly map this
// way, not just that the struct hashes correctly in isolation. So these
// cases now drive `WasmClientCore.handleDatagram` with real tile-datagram
// bytes and assert on the resulting `TileReady` events, instead of calling
// a key-construction function directly.
//
// Two of the original eight cases had no Rust analogue and are dropped:
//   - "includes passIdx as a fourth segment" (string-format assertion)
//   - "keeps frameSeq at index 0 so split(':')[0] still works" (string-format
//     assertion; the eviction sweep it documented reads `TileKey.frame_seq`
//     directly in Rust, no string parsing involved)
import { describe, it, expect } from 'vitest';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';
import { fragmentTile } from './helpers/wasm.js';

const CDF53_CODEC = 5; // ghostframe_protocol::protocol::Codec::Cdf53

/** A valid CDF 5/3 pass payload body: three channels (B, G, R) of
 * `[len=2][literal][zero run]`, each with its own literal so a swapped,
 * spliced or reordered assembly changes the bytes. */
function channels(lits: [number, number, number]): Uint8Array {
  const p = new Uint8Array(12);
  lits.forEach((lit, ch) => p.set([0, 2, lit, 0xfe], ch * 4));
  return p;
}

/** The 384 bit-plane bytes the core hands up for `channels(lits)`: each
 * channel's 128-byte plane has its literal first and zeros after. */
function planes(lits: [number, number, number]): Uint8Array {
  const out = new Uint8Array(384);
  lits.forEach((lit, ch) => (out[ch * 128] = lit));
  return out;
}

/** Fresh core in `TileDelivery::Payload` mode: a completed tile surfaces as a
 * `TilePayload` whose `bit_planes` mirror the payload bytes directly, so the
 * assertions below compare real content, not a decode that could be constant. */
function newCore(): InstanceType<typeof wasm.WasmClientCore> {
  return new wasm.WasmClientCore(false, false, true, 0n);
}

function payloadEvents(core: InstanceType<typeof wasm.WasmClientCore>, dg: Uint8Array): any[] {
  return (core.handleDatagram(dg, 0n) as any[]).filter((e) => e.kind === 'TilePayload');
}

describe('tile assembly bucket isolation (cross-pass collision regression)', () => {
  it('fragments of the same (frameSeq, tileX, tileY, passIdx) assemble into one tile', () => {
    // The same tuple must route both fragments into the same assembly bucket.
    const core = newCore();
    const lits: [number, number, number] = [0x05, 0x06, 0x07];
    const frags = fragmentTile(7, 3, 4, CDF53_CODEC, /*gen*/ 1, /*pass*/ 9, channels(lits), 8);
    expect(frags.length).toBe(2);

    expect(payloadEvents(core, frags[0])).toEqual([]);
    const ready = payloadEvents(core, frags[1]);
    expect(ready.length).toBe(1);
    expect(ready[0].frame_seq).toBe(7);
    expect(ready[0].tile_x).toBe(3);
    expect(ready[0].tile_y).toBe(4);
    expect(ready[0].data.pass_idx).toBe(9);
    expect(new Uint8Array(ready[0].data.bit_planes)).toEqual(planes(lits));
  });

  it('two passes of the same tile in the same frameSeq own separate assemblies', () => {
    // Pass 0 and pass 1 of the same tile both have multi-fragment payloads, and
    // pass 1's first fragment arrives before pass 0's second fragment has
    // landed. Before pass_idx was in the key, pass 1's frag 0 was silently
    // dropped and the completion spliced pass 0's frag 0 with pass 1's frag 1.
    const core = newCore();
    const lits0: [number, number, number] = [0x01, 0x02, 0x03];
    const lits1: [number, number, number] = [0x40, 0x41, 0x42];
    // Pass 0 carries the 2-byte `present_passes` prefix naming passes 0 and 1.
    const pass0Payload = new Uint8Array([0x00, 0x03, ...channels(lits0)]);
    const pass0 = fragmentTile(0x1234, 10, 30, CDF53_CODEC, 1, 0, pass0Payload, 8);
    const pass1 = fragmentTile(0x1234, 10, 30, CDF53_CODEC, 1, 1, channels(lits1), 8);
    expect(pass0.length).toBe(2);
    expect(pass1.length).toBe(2);

    // Interleave: pass0 frag0, pass1 frag0, pass1 frag1 (completes pass1),
    // pass0 frag1 (completes pass0).
    expect(payloadEvents(core, pass0[0])).toEqual([]);
    expect(payloadEvents(core, pass1[0])).toEqual([]);
    const readyPass1 = payloadEvents(core, pass1[1]);
    const readyPass0 = payloadEvents(core, pass0[1]);

    expect(readyPass1.length).toBe(1);
    expect(readyPass0.length).toBe(1);
    expect(readyPass1[0].data.pass_idx).toBe(1);
    expect(readyPass0[0].data.pass_idx).toBe(0);
    expect(new Uint8Array(readyPass1[0].data.bit_planes)).toEqual(planes(lits1));
    expect(new Uint8Array(readyPass0[0].data.bit_planes)).toEqual(planes(lits0));
  });

  it('same tile coordinates under different frameSeq occupy separate assemblies', () => {
    const core = newCore();
    const litsA: [number, number, number] = [0x11, 0x12, 0x13];
    const litsB: [number, number, number] = [0x44, 0x45, 0x46];
    const seqA = fragmentTile(100, 2, 3, CDF53_CODEC, 1, 4, channels(litsA), 8);
    const seqB = fragmentTile(101, 2, 3, CDF53_CODEC, 1, 4, channels(litsB), 8);

    expect(payloadEvents(core, seqA[0])).toEqual([]);
    expect(payloadEvents(core, seqB[0])).toEqual([]);
    const readyB = payloadEvents(core, seqB[1]);
    const readyA = payloadEvents(core, seqA[1]);

    expect(readyB.length).toBe(1);
    expect(readyB[0].frame_seq).toBe(101);
    expect(new Uint8Array(readyB[0].data.bit_planes)).toEqual(planes(litsB));
    expect(readyA.length).toBe(1);
    expect(readyA[0].frame_seq).toBe(100);
    expect(new Uint8Array(readyA[0].data.bit_planes)).toEqual(planes(litsA));
  });
});
