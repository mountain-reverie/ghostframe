import { describe, expect, it, beforeEach } from 'vitest';
import { MAX_RECORDED_ENTRIES, appendCapped, initDiagnostics } from '../src/diagnostics';

// diagnostics.ts gates all window writes behind `typeof window !== 'undefined'`.
// Vitest runs under Node (environment: 'node'), so we install a minimal window
// stub on globalThis before each test.  The stub is a plain object; we only
// need property-bag semantics — no DOM APIs are exercised by the three tests
// below.
function installWindowStub(): Record<string, unknown> {
  const stub: Record<string, unknown> = {};
  (globalThis as any).window = stub;
  return stub;
}

// Minimal canvas stub — satisfies the HTMLCanvasElement type-slot in
// initDiagnostics without needing a real DOM.
function fakeCanvas(): HTMLCanvasElement {
  return {} as HTMLCanvasElement;
}

function fakeGetRenderer() {
  return null;
}

describe('M3.5 bench instrumentation', () => {
  beforeEach(() => {
    // Re-install a fresh stub so each test starts with empty globals.
    installWindowStub();
  });

  it('recordTile accepts the new optional Ms-suffix timestamp fields', () => {
    const diag = initDiagnostics({ canvas: fakeCanvas(), getRenderer: fakeGetRenderer });
    diag.recordTile({
      seq: 7,
      tileX: 1,
      tileY: 2,
      codec: 4,
      payloadLen: 4,
      fbWidth: 64,
      fbHeight: 64,
      firstRecvMsClient: 1000.0,
      lastPaintMsClient: 1005.5,
    });
    const tiles = (window as any).__ghostframeRecordedTiles;
    expect(tiles).toHaveLength(1);
    expect(tiles[0].firstRecvMsClient).toBe(1000.0);
    expect(tiles[0].lastPaintMsClient).toBe(1005.5);
  });

  it('recordTile works without the optional fields (backward compat)', () => {
    const diag = initDiagnostics({ canvas: fakeCanvas(), getRenderer: fakeGetRenderer });
    diag.recordTile({
      seq: 99,
      tileX: 0,
      tileY: 0,
      codec: 0,
      payloadLen: 1,
      fbWidth: 32,
      fbHeight: 32,
    });
    const tiles = (window as any).__ghostframeRecordedTiles;
    expect(tiles).toHaveLength(1);
    expect(tiles[0].firstRecvMsClient).toBeUndefined();
    expect(tiles[0].lastPaintMsClient).toBeUndefined();
  });

  it('recordFramePainted pushes onto __ghostframe_framePaints', () => {
    const diag = initDiagnostics({ canvas: fakeCanvas(), getRenderer: fakeGetRenderer });
    diag.recordFramePainted({ seq: 42, rafMsClient: 9999.9 });
    diag.recordFramePainted({ seq: 43, rafMsClient: 10016.5 });
    const paints = (window as any).__ghostframe_framePaints;
    expect(paints).toHaveLength(2);
    expect(paints[0]).toEqual({ seq: 42, rafMsClient: 9999.9 });
    expect(paints[1]).toEqual({ seq: 43, rafMsClient: 10016.5 });
  });
});

describe('appendCapped', () => {
  it('keeps the newest entries, between cap and twice cap of them', () => {
    const log: number[] = [];
    for (let i = 0; i < 1000; i++) {
      appendCapped(log, i, 64);
      expect(log.length).toBeLessThan(128);
      expect(log[log.length - 1]).toBe(i);
      if (i >= 64) expect(log.length).toBeGreaterThanOrEqual(64);
    }
    // Contiguous, oldest first: nothing in the middle was dropped.
    for (let i = 1; i < log.length; i++) expect(log[i]).toBe(log[i - 1] + 1);
  });

  // The regression this exists for. At the production cap, taking one entry
  // off the front for each one added made V8 move 32767 elements per call:
  // 64 us against 0.5 us at a cap of 4096. A ratio rather than a duration,
  // so a slow runner slows both sides; the old code measured ~120x here.
  it('costs the same per entry at the production cap as at a small one', () => {
    const perEntry = (cap: number): number => {
      const log: object[] = [];
      for (let i = 0; i < cap; i++) log.push({ seq: i });
      let best = Infinity;
      for (let round = 0; round < 5; round++) {
        const t = performance.now();
        for (let i = 0; i < 100_000; i++) appendCapped(log, { seq: i }, cap);
        best = Math.min(best, performance.now() - t);
      }
      return best;
    };
    const small = perEntry(4096);
    const large = perEntry(MAX_RECORDED_ENTRIES);
    expect(large).toBeLessThan(small * 10 + 5);
  });
});
