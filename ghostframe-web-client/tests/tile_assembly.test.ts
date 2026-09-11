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

const TILE_DATAGRAM_FLAG = 0x80000000;
const RAW_CODEC = 4; // ghostframe_protocol::protocol::Codec::Raw

function u32be(n: number): number[] {
  return [(n >>> 24) & 0xff, (n >>> 16) & 0xff, (n >>> 8) & 0xff, n & 0xff];
}
function u16be(n: number): number[] {
  return [(n >>> 8) & 0xff, n & 0xff];
}

/**
 * Builds tile-datagram fragments matching `ghostframe_protocol::protocol`'s
 * `fragment_tile` wire layout exactly:
 *   [DatagramHeader 16B: frame_seq|frag_idx|frag_total|wire_seq|timestamp_us]
 *   [TileHeader 8B: tile_x|tile_y|(codec<<1|lz4)|(generation<<4|pass)|payload_len]
 *   [payload chunk]
 * `wire_seq` is UNSTAMPED_WIRE_SEQ (0); `timestamp_us` is 0 — neither is
 * read by the reassembly path these tests exercise.
 */
function fragmentTile(
  frameSeq: number,
  tileX: number,
  tileY: number,
  codec: number,
  generation: number,
  pass: number,
  payload: Uint8Array,
  maxFragmentPayload: number,
): Uint8Array[] {
  const chunks: Uint8Array[] = [];
  if (payload.length === 0) {
    chunks.push(new Uint8Array(0));
  } else {
    for (let i = 0; i < payload.length; i += maxFragmentPayload) {
      chunks.push(payload.slice(i, i + maxFragmentPayload));
    }
  }
  const fragTotal = chunks.length;
  return chunks.map((chunk, idx) => {
    const header = new Uint8Array([
      ...u32be(frameSeq | TILE_DATAGRAM_FLAG),
      ...u16be(idx),
      ...u16be(fragTotal),
      ...u32be(0), // wire_seq (UNSTAMPED_WIRE_SEQ)
      ...u32be(0), // timestamp_us
      tileX,
      tileY,
      (codec << 1) | 0, // lz4 = false
      ((generation & 0x0f) << 4) | (pass & 0x0f),
      ...u32be(payload.length),
    ]);
    const out = new Uint8Array(header.length + chunk.length);
    out.set(header, 0);
    out.set(chunk, header.length);
    return out;
  });
}

/** BGRA payload -> the RGBA bytes `Codec::Raw` decode produces (swizzle,
 * alpha passed through), matching `finish_assembly`'s `Codec::Raw` arm. */
function bgraToRgba(bgra: Uint8Array): Uint8Array {
  const rgba = new Uint8Array(bgra.length);
  for (let i = 0; i < bgra.length; i += 4) {
    rgba[i] = bgra[i + 2];
    rgba[i + 1] = bgra[i + 1];
    rgba[i + 2] = bgra[i];
    rgba[i + 3] = bgra[i + 3];
  }
  return rgba;
}

/** Fresh core in `TileDelivery::Decoded` mode, so `Codec::Raw` tiles
 * complete as `TileReady` events straight out of `handleDatagram`. */
function newCore(): InstanceType<typeof wasm.WasmClientCore> {
  return new wasm.WasmClientCore(false, false, false, 0n);
}

function tileReadyEvents(core: InstanceType<typeof wasm.WasmClientCore>, dg: Uint8Array): any[] {
  return (core.handleDatagram(dg, 0n) as any[]).filter((e) => e.kind === 'TileReady');
}

describe('tile assembly bucket isolation (cross-pass collision regression)', () => {
  it('fragments of the same (frameSeq, tileX, tileY, passIdx) assemble into one tile', () => {
    // Equivalent to the old `tileKey(7,3,4,9) === tileKey(7,3,4,9)`: the
    // same tuple must route both fragments into the same assembly bucket.
    const core = newCore();
    const payload = new Uint8Array([0x01, 0x02, 0x03, 0xff, 0x04, 0x05, 0x06, 0xff]); // 2 BGRA px
    const frags = fragmentTile(7, 3, 4, RAW_CODEC, /*gen*/ 1, /*pass*/ 9, payload, 4);
    expect(frags.length).toBe(2);

    expect(tileReadyEvents(core, frags[0])).toEqual([]);
    const ready = tileReadyEvents(core, frags[1]);
    expect(ready.length).toBe(1);
    expect(ready[0].frame_seq).toBe(7);
    expect(ready[0].tile_x).toBe(3);
    expect(ready[0].tile_y).toBe(4);
    expect(new Uint8Array(ready[0].rgba)).toEqual(bgraToRgba(payload));
  });

  it('two passes of the same tile in the same frameSeq own separate assemblies', () => {
    // Models the exact scenario the production bug exposed: pass 0 and
    // pass 1 of the same tile both have multi-fragment payloads, and pass
    // 1's first fragment arrives before pass 0's second fragment has
    // landed — the production interleaving that triggered the collision.
    // Before the fix (no pass_idx in the key), pass 1's frag 0 would have
    // been silently dropped (its bucket slot already filled by pass 0's
    // frag 0), and the eventual completion would have spliced pass 0's
    // frag 0 together with pass 1's frag 1 into one Frankenstein tile.
    const core = newCore();
    const frameSeq = 0x1234;
    const tileX = 10;
    const tileY = 30;

    const pass0Payload = new Uint8Array([0x01, 0x02, 0x03, 0xff, 0x04, 0x05, 0x06, 0xff]);
    const pass1Payload = new Uint8Array([0x10, 0x20, 0x30, 0xff, 0x40, 0x50, 0x60, 0xff]);
    const pass0 = fragmentTile(frameSeq, tileX, tileY, RAW_CODEC, 1, 0, pass0Payload, 4);
    const pass1 = fragmentTile(frameSeq, tileX, tileY, RAW_CODEC, 1, 1, pass1Payload, 4);
    expect(pass0.length).toBe(2);
    expect(pass1.length).toBe(2);

    // Interleave: pass0 frag0, pass1 frag0, pass1 frag1 (completes pass1),
    // pass0 frag1 (completes pass0).
    expect(tileReadyEvents(core, pass0[0])).toEqual([]);
    expect(tileReadyEvents(core, pass1[0])).toEqual([]);

    const readyPass1 = tileReadyEvents(core, pass1[1]);
    const readyPass0 = tileReadyEvents(core, pass0[1]);

    expect(readyPass1.length).toBe(1);
    expect(readyPass0.length).toBe(1);
    expect(new Uint8Array(readyPass1[0].rgba)).toEqual(bgraToRgba(pass1Payload));
    expect(new Uint8Array(readyPass0[0].rgba)).toEqual(bgraToRgba(pass0Payload));
  });

  it('same tile coordinates under different frameSeq occupy separate assemblies', () => {
    const core = newCore();
    const tileX = 2;
    const tileY = 3;

    const seqAPayload = new Uint8Array([0xaa, 0xbb, 0xcc, 0xff, 0xdd, 0xee, 0xff, 0xff]);
    const seqBPayload = new Uint8Array([0x11, 0x22, 0x33, 0xff, 0x44, 0x55, 0x66, 0xff]);
    const seqA = fragmentTile(100, tileX, tileY, RAW_CODEC, 1, 4, seqAPayload, 4);
    const seqB = fragmentTile(101, tileX, tileY, RAW_CODEC, 1, 4, seqBPayload, 4);

    expect(tileReadyEvents(core, seqA[0])).toEqual([]);
    expect(tileReadyEvents(core, seqB[0])).toEqual([]);

    const readyB = tileReadyEvents(core, seqB[1]);
    const readyA = tileReadyEvents(core, seqA[1]);

    expect(readyB.length).toBe(1);
    expect(readyB[0].frame_seq).toBe(101);
    expect(new Uint8Array(readyB[0].rgba)).toEqual(bgraToRgba(seqBPayload));

    expect(readyA.length).toBe(1);
    expect(readyA[0].frame_seq).toBe(100);
    expect(new Uint8Array(readyA[0].rgba)).toEqual(bgraToRgba(seqAPayload));
  });
});
