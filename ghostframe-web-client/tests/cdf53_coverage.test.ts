import { describe, it, expect } from 'vitest';
import {
  applyCdf53Arrival,
  type Cdf53CoverageEntry,
} from '../src/cdf53_coverage';

describe('applyCdf53Arrival', () => {
  it('creates a fresh entry for a brand new (tile, generation)', () => {
    const r = applyCdf53Arrival(undefined, /*gen*/ 3, /*pass*/ 0, /*seq*/ 7, /*now*/ 100, /*ok*/ true);
    expect(r.entry.generation).toBe(3);
    expect(r.entry.frameSeq).toBe(7);
    expect(r.entry.passMask).toBe(1);
    expect(r.entry.nackedMask).toBe(0);
    expect(r.entry.lastChangeMs).toBe(100);
    expect(r.nackPasses).toEqual([]);
  });

  it('replaces the entry when generation differs', () => {
    const stale: Cdf53CoverageEntry = {
      generation: 2,
      frameSeq: 1,
      passMask: 0x3FFF,
      nackedMask: 0x0F,
      lastChangeMs: 50,
    };
    const r = applyCdf53Arrival(stale, /*gen*/ 3, /*pass*/ 5, 7, 100, true);
    expect(r.entry.generation).toBe(3);
    expect(r.entry.frameSeq).toBe(7);
    expect(r.entry.passMask).toBe(1 << 5);
    expect(r.entry.nackedMask).toBe(0);
    expect(r.entry.lastChangeMs).toBe(100);
    expect(r.nackPasses).toEqual([]);
  });

  it('refreshes frameSeq on existing-generation arrival', () => {
    const e: Cdf53CoverageEntry = {
      generation: 1,
      frameSeq: 5,
      passMask: 1,
      nackedMask: 0,
      lastChangeMs: 0,
    };
    const r = applyCdf53Arrival(e, /*gen*/ 1, /*pass*/ 1, /*seq*/ 9, 10, true);
    expect(r.entry.frameSeq).toBe(9);
  });

  it('runs gap detection on existing-gen success and NACKs missing lower passes', () => {
    const e: Cdf53CoverageEntry = {
      generation: 1,
      frameSeq: 0,
      passMask: 0b0000001, // only pass 0 seen
      nackedMask: 0,
      lastChangeMs: 0,
    };
    const r = applyCdf53Arrival(e, 1, /*pass*/ 4, 0, 10, true);
    // bit 4 set → missing below = {1,2,3}, all should be NACKed.
    expect(r.nackPasses).toEqual([1, 2, 3]);
    expect(r.entry.passMask).toBe(0b0010001);
    expect(r.entry.nackedMask).toBe(0b0001110);
  });

  it('does not run gap detection on a fresh-generation entry', () => {
    const r = applyCdf53Arrival(undefined, /*gen*/ 0, /*pass*/ 5, 0, 0, true);
    expect(r.nackPasses).toEqual([]);
    expect(r.entry.passMask).toBe(1 << 5);
    expect(r.entry.nackedMask).toBe(0);
  });

  it('dedups gap-detection NACKs via nackedMask', () => {
    const e: Cdf53CoverageEntry = {
      generation: 1,
      frameSeq: 0,
      passMask: 1, // pass 0
      nackedMask: 0b0000010, // pass 1 already nacked
      lastChangeMs: 0,
    };
    const r = applyCdf53Arrival(e, 1, /*pass*/ 3, 0, 10, true);
    // bits below 3 missing-and-not-nacked = {2}; bit 1 is in nackedMask.
    expect(r.nackPasses).toEqual([2]);
    expect(r.entry.nackedMask).toBe(0b0000110);
  });

  it('Phase 1.5-A: NACKs the failing pass on prevalidation failure', () => {
    const r = applyCdf53Arrival(undefined, /*gen*/ 2, /*pass*/ 7, 0, 50, /*ok*/ false);
    expect(r.nackPasses).toEqual([7]);
    expect(r.entry.passMask).toBe(0); // bit NOT set — prevalidation failed
    expect(r.entry.nackedMask).toBe(1 << 7); // marked as nacked
  });

  it('Phase 1.5-A: does NOT re-NACK an already-nacked failed pass', () => {
    const e: Cdf53CoverageEntry = {
      generation: 1,
      frameSeq: 0,
      passMask: 0,
      nackedMask: 1 << 7,
      lastChangeMs: 0,
    };
    const r = applyCdf53Arrival(e, 1, /*pass*/ 7, 0, 100, /*ok*/ false);
    expect(r.nackPasses).toEqual([]); // dedup'd
    expect(r.entry.passMask).toBe(0);
    expect(r.entry.nackedMask).toBe(1 << 7);
  });

  it('Phase 1.5-A: failure does NOT advance lastChangeMs', () => {
    // Tail-fallback uses (now - lastChangeMs) > TAIL_FALLBACK_MS to re-NACK.
    // A failure must not reset that timer — otherwise a tile stuck on
    // deterministically-bad bytes would never trigger tail-fallback.
    const e: Cdf53CoverageEntry = {
      generation: 1,
      frameSeq: 0,
      passMask: 0,
      nackedMask: 0,
      lastChangeMs: 42,
    };
    const r = applyCdf53Arrival(e, 1, /*pass*/ 3, 0, /*now*/ 999, /*ok*/ false);
    expect(r.entry.lastChangeMs).toBe(42);
  });

  it('Phase 1.5-A: success on a previously-failed pass sets the bit and retains nackedMask bit', () => {
    // Server retransmits, this time the bytes are good. The success
    // path enters gap-detection because the entry pre-existed and the
    // bitmap grew — but every other pass below 5 is genuinely missing,
    // so gap detection legitimately NACKs 0..4. The previously-failed
    // bit (5) stays in nackedMask (idempotent OR).
    const e: Cdf53CoverageEntry = {
      generation: 1,
      frameSeq: 0,
      passMask: 0,
      nackedMask: 1 << 5,
      lastChangeMs: 0,
    };
    const r = applyCdf53Arrival(e, 1, /*pass*/ 5, 0, 200, /*ok*/ true);
    expect(r.entry.passMask).toBe(1 << 5);
    // 0..4 newly NACKed, plus the pre-existing bit 5 → 0b111111.
    expect(r.entry.nackedMask).toBe(0b0111111);
    expect(r.entry.lastChangeMs).toBe(200); // advanced on bitmap growth
    expect(r.nackPasses).toEqual([0, 1, 2, 3, 4]);
  });

  it('duplicate-success arrivals do not advance lastChangeMs', () => {
    const e: Cdf53CoverageEntry = {
      generation: 1,
      frameSeq: 0,
      passMask: 1 << 3,
      nackedMask: 0,
      lastChangeMs: 42,
    };
    const r = applyCdf53Arrival(e, 1, /*pass*/ 3, 0, /*now*/ 999, true);
    expect(r.entry.passMask).toBe(1 << 3);
    expect(r.entry.lastChangeMs).toBe(42);
    expect(r.nackPasses).toEqual([]);
  });
});
