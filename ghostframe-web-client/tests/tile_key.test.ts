import { describe, it, expect } from 'vitest';
import { tileKey } from '../src/decoder.js';

describe('tileKey', () => {
  it('includes passIdx as a fourth segment', () => {
    // Regression for the cross-pass assembly collision bug:
    // the server emits all 14 CDF53 passes for a single dirty re-encode
    // under the SAME wire frame_seq (per io_bridge.rs:1432 — every
    // drained TileWork in one tick reuses `frame_seq_with_flag`). They
    // differ only by the tile header's `pass` field. Before this fix,
    // tileKey ignored passIdx → all 14 passes collided in the same
    // assembly bucket and fragments from different passes got
    // interleaved into one Frankenstein payload that failed
    // prevalidation as ERR_CDF53_TRUNCATED / ERR_CDF53_RLE_LENGTH.
    //
    // This test pins the key shape so a regression that strips passIdx
    // or reorders the segments would fail it.
    const k = tileKey(0xCAFE, 17, 23, 5);
    expect(k).toBe('51966:17:23:5');
  });

  it('keys for two passes of the same (frameSeq, tileX, tileY) are distinct', () => {
    const a = tileKey(100, 5, 6, 0);
    const b = tileKey(100, 5, 6, 1);
    expect(a).not.toBe(b);
  });

  it('keys for the same (frameSeq, tileX, tileY, passIdx) are equal', () => {
    expect(tileKey(7, 3, 4, 9)).toBe(tileKey(7, 3, 4, 9));
  });

  it('keeps frameSeq at index 0 so split(":")[0] still works', () => {
    // The stale-assembly eviction sweep in main.ts:1162 parses the
    // frameSeq from the key with `parseInt(k.split(':')[0], 10)`. The
    // four-segment key must keep frameSeq first so that loop keeps
    // discarding old assemblies the way it did before.
    const k = tileKey(424242, 9, 11, 7);
    expect(parseInt(k.split(':')[0], 10)).toBe(424242);
  });

  it('keys with same coords but different frameSeq stay distinct', () => {
    expect(tileKey(1, 2, 3, 4)).not.toBe(tileKey(2, 2, 3, 4));
  });
});

describe('tile assembly bucket isolation (cross-pass collision regression)', () => {
  // Models the exact scenario the production bug exposed: pass 0 and
  // pass 1 of the same tile both have multi-fragment payloads. With
  // the old key shape, pass 1's frag 0 would either overwrite pass 0's
  // frag 0 (if it arrived first) or be silently rejected by the
  // `if (asm.fragments[fragIdx] === null)` guard. With the new key
  // shape every pass owns its own assembly bucket.
  it('two passes for the same tile in the same frameSeq own separate buckets', () => {
    const map = new Map<string, { fragments: (Uint8Array | null)[]; received: number }>();
    const frameSeq = 0x1234;
    const tileX = 10;
    const tileY = 30;

    // Pass 0 frag 0 arrives.
    const k0 = tileKey(frameSeq, tileX, tileY, /*passIdx*/ 0);
    map.set(k0, { fragments: [new Uint8Array([0xAA]), null], received: 1 });

    // Pass 1 frag 0 arrives BEFORE pass 0 frag 1 has landed — the
    // production interleaving that triggered the bug. With the new
    // key these end up in different buckets.
    const k1 = tileKey(frameSeq, tileX, tileY, /*passIdx*/ 1);
    map.set(k1, { fragments: [new Uint8Array([0xBB]), null], received: 1 });

    expect(map.size).toBe(2);
    expect(map.get(k0)!.fragments[0]).toEqual(new Uint8Array([0xAA]));
    expect(map.get(k1)!.fragments[0]).toEqual(new Uint8Array([0xBB]));
  });
});
