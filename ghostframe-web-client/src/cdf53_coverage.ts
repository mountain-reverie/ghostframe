/**
 * Per-(tile, generation) CDF53 pass-coverage bookkeeping. Holds the
 * client's view of which passes have been successfully received and
 * which have been NACKed. Pure (aside from `entry` mutation) so it can
 * be unit-tested in isolation of main.ts.
 */
export interface Cdf53CoverageEntry {
  generation: number;
  frameSeq: number;
  passMask: number;
  nackedMask: number;
  lastChangeMs: number;
}

export interface Cdf53CoverageUpdate {
  /** Updated coverage entry — caller stores back in its map. */
  entry: Cdf53CoverageEntry;
  /**
   * Pass indices the caller should hand to `queuePassNack`. May contain:
   *  - the failing pass (prevalidation failure path),
   *  - lower-indexed missing passes (gap detection on the success path).
   * Each NACK request is dedup'd via `entry.nackedMask`.
   */
  nackPasses: number[];
}

/**
 * Apply one CDF53 pass arrival to the coverage entry.
 *
 * Behavior:
 *  - If `entry` is undefined OR its generation differs from `gen`,
 *    a fresh entry is created with passMask=0, nackedMask=0.
 *  - On prevalidation FAILURE: passMask stays 0 for that bit (the pass
 *    was not successfully received — gap-detection / tail-fallback
 *    must still see it as missing). The failed pass is NACKed once,
 *    dedup'd via nackedMask. Tail-fallback re-arms by clearing the
 *    bit after its quiet interval; that's the natural retry rate.
 *  - On prevalidation SUCCESS: passMask gets the bit set. If the
 *    bitmap grew, lastChangeMs advances and gap-detection scans for
 *    lower-indexed missing passes (only on existing-generation
 *    arrivals — a brand-new generation has no prior bits to gap-detect).
 *
 * `entry.frameSeq` is refreshed on existing-generation arrivals so
 * NACKs reference the most recent server-side cache entry.
 */
export function applyCdf53Arrival(
  entry: Cdf53CoverageEntry | undefined,
  gen: number,
  passIdx: number,
  frameSeq: number,
  nowMs: number,
  prevalidationOk: boolean,
): Cdf53CoverageUpdate {
  let e: Cdf53CoverageEntry;
  let isNewGeneration: boolean;
  if (!entry || entry.generation !== gen) {
    e = {
      generation: gen,
      frameSeq,
      passMask: 0,
      nackedMask: 0,
      lastChangeMs: nowMs,
    };
    isNewGeneration = true;
  } else {
    e = entry;
    e.frameSeq = frameSeq;
    isNewGeneration = false;
  }

  const nackPasses: number[] = [];

  if (!prevalidationOk) {
    // Phase 1.5-A: NACK on prevalidation failure so the pass isn't
    // silently lost. Without this, a corrupted pass leaves no signal:
    // no ACK (client didn't apply), no gap-detect NACK (passMask was
    // wrongly set in the legacy code path).
    const passBit = 1 << passIdx;
    if ((e.nackedMask & passBit) === 0) {
      nackPasses.push(passIdx);
      e.nackedMask |= passBit;
    }
    return { entry: e, nackPasses };
  }

  const before = e.passMask;
  e.passMask |= 1 << passIdx;
  if (e.passMask !== before) {
    e.lastChangeMs = nowMs;
    if (!isNewGeneration) {
      const sentinel = (1 << passIdx) - 1; // bits 0..passIdx-1
      const missingBelow = sentinel & ~e.passMask & ~e.nackedMask;
      if (missingBelow !== 0) {
        for (let p = 0; p < passIdx; p++) {
          if ((missingBelow & (1 << p)) !== 0) {
            nackPasses.push(p);
          }
        }
        e.nackedMask |= missingBelow;
      }
    }
  }
  return { entry: e, nackPasses };
}
