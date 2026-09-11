import { describe, it, expect } from 'vitest';
import { applyCdf53Arrival } from '../pkg-node/ghostframe_client_wasm.js';

/** Snake_case shape returned by wasm's `applyCdf53Arrival` entry. */
interface WasmCoverageEntry {
  generation: number;
  frame_seq: number;
  pass_mask: number;
  nacked_mask: number;
  last_change_us: bigint;
}

/**
 * Thin wrapper around wasm's ten flattened positional parameters, so the
 * suite reads close to the old `applyCdf53Arrival(prev, gen, pass, seq, now,
 * ok)` call shape. `prev` is `undefined` for a first arrival, or the
 * previous call's `entry` threaded back in. `last_change_us` crosses the
 * boundary as a BigInt; this wrapper accepts/returns plain numbers and
 * converts at the edges, since every caller in this suite treats the "now"
 * axis as an opaque tick counter, not real microseconds.
 */
function apply(
  prev: WasmCoverageEntry | undefined,
  gen: number,
  passIdx: number,
  frameSeq: number,
  now: number,
  ok: boolean,
): { entry: WasmCoverageEntry; nackPasses: number[] } {
  const out = applyCdf53Arrival(
    prev?.generation,
    prev?.frame_seq ?? 0,
    prev?.pass_mask ?? 0,
    prev?.nacked_mask ?? 0,
    prev?.last_change_us ?? 0n,
    gen,
    passIdx,
    frameSeq,
    BigInt(now),
    ok,
  );
  return { entry: out.entry, nackPasses: out.nack_passes };
}

describe('applyCdf53Arrival', () => {
  it('creates a fresh entry for a brand new (tile, generation)', () => {
    const r = apply(undefined, /*gen*/ 3, /*pass*/ 0, /*seq*/ 7, /*now*/ 100, /*ok*/ true);
    expect(r.entry.generation).toBe(3);
    expect(r.entry.frame_seq).toBe(7);
    expect(r.entry.pass_mask).toBe(1);
    expect(r.entry.nacked_mask).toBe(0);
    expect(Number(r.entry.last_change_us)).toBe(100);
    expect(r.nackPasses).toEqual([]);
  });

  it('replaces the entry when generation differs', () => {
    const stale: WasmCoverageEntry = {
      generation: 2,
      frame_seq: 1,
      pass_mask: 0x3FFF,
      nacked_mask: 0x0F,
      last_change_us: 50n,
    };
    const r = apply(stale, /*gen*/ 3, /*pass*/ 5, 7, 100, true);
    expect(r.entry.generation).toBe(3);
    expect(r.entry.frame_seq).toBe(7);
    expect(r.entry.pass_mask).toBe(1 << 5);
    expect(r.entry.nacked_mask).toBe(0);
    expect(Number(r.entry.last_change_us)).toBe(100);
    expect(r.nackPasses).toEqual([]);
  });

  it('refreshes frameSeq on existing-generation arrival', () => {
    const e: WasmCoverageEntry = {
      generation: 1,
      frame_seq: 5,
      pass_mask: 1,
      nacked_mask: 0,
      last_change_us: 0n,
    };
    const r = apply(e, /*gen*/ 1, /*pass*/ 1, /*seq*/ 9, 10, true);
    expect(r.entry.frame_seq).toBe(9);
  });

  it('runs gap detection on existing-gen success and NACKs missing lower passes', () => {
    const e: WasmCoverageEntry = {
      generation: 1,
      frame_seq: 0,
      pass_mask: 0b0000001, // only pass 0 seen
      nacked_mask: 0,
      last_change_us: 0n,
    };
    const r = apply(e, 1, /*pass*/ 4, 0, 10, true);
    // bit 4 set → missing below = {1,2,3}, all should be NACKed.
    expect(r.nackPasses).toEqual([1, 2, 3]);
    expect(r.entry.pass_mask).toBe(0b0010001);
    expect(r.entry.nacked_mask).toBe(0b0001110);
  });

  it('does not run gap detection on a fresh-generation entry', () => {
    const r = apply(undefined, /*gen*/ 0, /*pass*/ 5, 0, 0, true);
    expect(r.nackPasses).toEqual([]);
    expect(r.entry.pass_mask).toBe(1 << 5);
    expect(r.entry.nacked_mask).toBe(0);
  });

  it('dedups gap-detection NACKs via nackedMask', () => {
    const e: WasmCoverageEntry = {
      generation: 1,
      frame_seq: 0,
      pass_mask: 1, // pass 0
      nacked_mask: 0b0000010, // pass 1 already nacked
      last_change_us: 0n,
    };
    const r = apply(e, 1, /*pass*/ 3, 0, 10, true);
    // bits below 3 missing-and-not-nacked = {2}; bit 1 is in nackedMask.
    expect(r.nackPasses).toEqual([2]);
    expect(r.entry.nacked_mask).toBe(0b0000110);
  });

  it('Phase 1.5-A: NACKs the failing pass on prevalidation failure', () => {
    const r = apply(undefined, /*gen*/ 2, /*pass*/ 7, 0, 50, /*ok*/ false);
    expect(r.nackPasses).toEqual([7]);
    expect(r.entry.pass_mask).toBe(0); // bit NOT set — prevalidation failed
    expect(r.entry.nacked_mask).toBe(1 << 7); // marked as nacked
  });

  it('Phase 1.5-A: does NOT re-NACK an already-nacked failed pass', () => {
    const e: WasmCoverageEntry = {
      generation: 1,
      frame_seq: 0,
      pass_mask: 0,
      nacked_mask: 1 << 7,
      last_change_us: 0n,
    };
    const r = apply(e, 1, /*pass*/ 7, 0, 100, /*ok*/ false);
    expect(r.nackPasses).toEqual([]); // dedup'd
    expect(r.entry.pass_mask).toBe(0);
    expect(r.entry.nacked_mask).toBe(1 << 7);
  });

  it('Phase 1.5-A: failure does NOT advance lastChangeMs', () => {
    // Tail-fallback uses (now - lastChangeMs) > TAIL_FALLBACK_MS to re-NACK.
    // A failure must not reset that timer — otherwise a tile stuck on
    // deterministically-bad bytes would never trigger tail-fallback.
    const e: WasmCoverageEntry = {
      generation: 1,
      frame_seq: 0,
      pass_mask: 0,
      nacked_mask: 0,
      last_change_us: 42n,
    };
    const r = apply(e, 1, /*pass*/ 3, 0, /*now*/ 999, /*ok*/ false);
    expect(Number(r.entry.last_change_us)).toBe(42);
  });

  it('Phase 1.5-A: success on a previously-failed pass sets the bit and retains nackedMask bit', () => {
    // Server retransmits, this time the bytes are good. The success
    // path enters gap-detection because the entry pre-existed and the
    // bitmap grew — but every other pass below 5 is genuinely missing,
    // so gap detection legitimately NACKs 0..4. The previously-failed
    // bit (5) stays in nackedMask (idempotent OR).
    const e: WasmCoverageEntry = {
      generation: 1,
      frame_seq: 0,
      pass_mask: 0,
      nacked_mask: 1 << 5,
      last_change_us: 0n,
    };
    const r = apply(e, 1, /*pass*/ 5, 0, 200, /*ok*/ true);
    expect(r.entry.pass_mask).toBe(1 << 5);
    // 0..4 newly NACKed, plus the pre-existing bit 5 → 0b111111.
    expect(r.entry.nacked_mask).toBe(0b0111111);
    expect(Number(r.entry.last_change_us)).toBe(200); // advanced on bitmap growth
    expect(r.nackPasses).toEqual([0, 1, 2, 3, 4]);
  });

  it('duplicate-success arrivals do not advance lastChangeMs', () => {
    const e: WasmCoverageEntry = {
      generation: 1,
      frame_seq: 0,
      pass_mask: 1 << 3,
      nacked_mask: 0,
      last_change_us: 42n,
    };
    const r = apply(e, 1, /*pass*/ 3, 0, /*now*/ 999, true);
    expect(r.entry.pass_mask).toBe(1 << 3);
    expect(Number(r.entry.last_change_us)).toBe(42);
    expect(r.nackPasses).toEqual([]);
  });
});
