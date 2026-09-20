import { describe, it, expect } from 'vitest';
import { computePassesProcessed } from '../src/webgpu/cdf53';

/**
 * `passesProcessed` is what the inverse shaders' `read_coeff` uses to decide
 * its midpoint correction: for K in [2, 14) it adds 2^(14-K-1) to every
 * significant coefficient, and skips the correction entirely at K >= 14.
 *
 * Getting K wrong is therefore not a bookkeeping detail — it is a direct
 * pixel error. The numerical consequence is pinned in
 * ghostframe-client-core/tests/oracle_gpu_sparse.rs, which measured 16/255
 * per channel on flat content before this rule was fixed.
 */
describe('computePassesProcessed', () => {
  const mask = (...passes: number[]) => passes.reduce((m, p) => m | (1 << p), 0);

  it('falls back to max(passIdx + 1) while the bitmap is unknown', () => {
    // presentPasses = 0 means pass 0 has not arrived, so nothing is known
    // about which of 1..13 to expect.
    expect(computePassesProcessed(mask(8), 0, 0, 8)).toBe(9);
    // Monotonic: a lower pass arriving later must not lower K.
    expect(computePassesProcessed(mask(8, 3), 0, 9, 3)).toBe(9);
  });

  it('reaches 14 when every present pass has landed, so no midpoint is applied', () => {
    // The production shape: sparse encoding skipped 1..7 as empty.
    const present = mask(0, 8, 9, 10, 11, 12, 13);
    expect(computePassesProcessed(present, present, 0, 13)).toBe(14);
  });

  it('reaches 14 even when the skipped planes are TRAILING', () => {
    // The regression this rule exists for. Flat content: present = {0,6,7,8},
    // so 9..13 were skipped *because they are zero*. The old
    // max(passIdx + 1) rule yielded K=9 here, and read_coeff then added a
    // midpoint of 2^4 = 16 for low bits that are known to be zero.
    const present = mask(0, 6, 7, 8);
    expect(computePassesProcessed(present, present, 0, 8)).toBe(14);
  });

  it('stops at the first genuinely missing pass', () => {
    // present = {0,6,7,8}; 6 has not arrived. Passes 1..5 are resolved
    // (skipped ⇒ known zero), so the prefix runs 0..5 and stops at 6.
    const present = mask(0, 6, 7, 8);
    const received = mask(0, 7, 8);
    expect(computePassesProcessed(received, present, 0, 8)).toBe(6);
  });

  it('counts a fully dense tile the same way the old rule did', () => {
    // No behaviour change for content that fills every plane — which is
    // why the defect stayed invisible in production, where 1300 of 1301
    // tile-generations had a highest present pass of 13.
    const present = (1 << 14) - 1;
    expect(computePassesProcessed(present, present, 0, 13)).toBe(14);
    expect(computePassesProcessed(mask(0, 1, 2), present, 0, 2)).toBe(3);
  });

  it('ignores bits above 13 in a corrupt bitmap', () => {
    const present = mask(0, 6, 7, 8) | 0xc000;
    expect(computePassesProcessed(present, present, 0, 8)).toBe(14);
  });
});
