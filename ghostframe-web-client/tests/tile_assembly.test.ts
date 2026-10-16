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

describe('tiles are one datagram', () => {
  it('a tile pass completes from its single datagram', () => {
    const core = newCore();
    const lits: [number, number, number] = [0x05, 0x06, 0x07];
    const frags = fragmentTile(7, 3, 4, CDF53_CODEC, /*gen*/ 1, /*pass*/ 9, channels(lits), 1200);
    expect(frags.length).toBe(1);
    const ready = payloadEvents(core, frags[0]);
    expect(ready.length).toBe(1);
    expect(ready[0].frame_seq).toBe(7);
    expect(ready[0].tile_x).toBe(3);
    expect(ready[0].tile_y).toBe(4);
    expect(ready[0].data.pass_idx).toBe(9);
    expect(new Uint8Array(ready[0].data.bit_planes)).toEqual(planes(lits));
  });

  it('a tile split over several datagrams is refused, whatever order it arrives in', () => {
    // A tile is one datagram: none of the pieces is stored, none completes.
    const core = newCore();
    const frags = fragmentTile(7, 3, 4, CDF53_CODEC, 1, 9, channels([1, 2, 3]), 8);
    expect(frags.length).toBe(2);
    for (const f of [...frags].reverse()) expect(payloadEvents(core, f)).toEqual([]);
  });

  it('passes, frames and tiles keep their own identity in the event', () => {
    // Different (frameSeq, tile, pass) triples each surface as their own event
    // carrying their own payload.
    const core = newCore();
    const cases: Array<[number, number, number, number, [number, number, number]]> = [
      [0x1234, 10, 30, 0, [0x01, 0x02, 0x03]],
      [0x1234, 10, 30, 1, [0x40, 0x41, 0x42]],
      [100, 2, 3, 4, [0x11, 0x12, 0x13]],
      [101, 2, 3, 4, [0x44, 0x45, 0x46]],
    ];
    for (const [seq, tx, ty, pass, lits] of cases) {
      const body = pass === 0 ? new Uint8Array([0x00, 0x03, ...channels(lits)]) : channels(lits);
      const [dg] = fragmentTile(seq, tx, ty, CDF53_CODEC, 1, pass, body, 1200);
      const ready = payloadEvents(core, dg);
      expect(ready.length).toBe(1);
      expect(ready[0].frame_seq).toBe(seq);
      expect(ready[0].tile_x).toBe(tx);
      expect(ready[0].tile_y).toBe(ty);
      expect(ready[0].data.pass_idx).toBe(pass);
      expect(new Uint8Array(ready[0].data.bit_planes)).toEqual(planes(lits));
    }
  });
});
