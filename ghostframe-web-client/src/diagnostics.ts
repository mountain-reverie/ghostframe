// SPDX-License-Identifier: MIT OR Apache-2.0
/**
 * diagnostics.ts — test and e2e instrumentation surface.
 *
 * Owns the `window.__ghostframe*` recorders and the `window.__read*` pixel
 * readbacks, so that main.ts is free of scattered `(window as any)` casts.
 * The readbacks are one-line wrappers over `WebRenderer`: the GPU work lives
 * in Rust (`ghostframe-client-wasm/src/web`). The `typeof window !==
 * "undefined"` guards keep the module safe under Vitest/Node.
 */

import { protocol } from './protocol_constants.js';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Shape returned by initDiagnostics. */
export interface DiagnosticsHandle {
  /** Mutable stats counters — main.ts increments these directly. */
  stats: { tileDatagrams: number; frameDatagrams: number };

  /** Record a framebuffer resize. */
  recordResize(params: {
    seq: number;
    oldW: number; oldH: number;
    newW: number; newH: number;
    trigger: 'sentinel' | 'fallback-expand';
  }): void;

  /** Record an applied tile. */
  recordTile(params: {
    seq: number;
    tileX: number; tileY: number;
    codec: number;
    payloadLen: number;
    fbWidth: number; fbHeight: number;
    /** The PalRLE payload flags byte; only pushed when given. */
    palRleFlag?: number;
    // M3.5 bench instrumentation — optional, default undefined.
    // performance.now() values; DOMHighResTimeStamp, millisecond resolution.
    /** performance.now() at first fragment receipt for this tile. */
    firstRecvMsClient?: number;
    /** performance.now() when the tile was handed to the renderer. */
    lastPaintMsClient?: number;
  }): void;

  /** Update the rAF tick counter on window. */
  recordRafTick(n: number): void;

  // M3.5 bench instrumentation: called from rAF callback after a frame's
  // last tile has been painted.
  recordFramePainted(params: {
    seq: number;
    /** performance.now() inside the rAF callback. */
    rafMsClient: number;
  }): void;
}

/** The slice of `WebRenderer` the readbacks need. */
export interface PixelReader {
  /** RGBA bytes of a framebuffer rectangle. */
  readPixelRect(x: number, y: number, w: number, h: number): Promise<Uint8Array>;
  /** Writes a known gradient into the framebuffer and reports mismatches. */
  readGradientGolden(): Promise<{
    ok: boolean;
    mismatches: Array<{ x: number; y: number; got: number[]; want: number[] }>;
  }>;
}

// ---------------------------------------------------------------------------
// FIFO cap
// ---------------------------------------------------------------------------

// Cap test-instrumentation recorder arrays to bound memory + CDP
// serialisation cost in long-running tests. Sized for the busiest e2e
// (e2e_cdf53_mixed_codecs at 1920x1080: ~80 dirty tiles × 30 fps × ~5 s
// settle ≈ 12 000 records, plus headroom). FIFO drop keeps the most
// recent observations when exceeded.
export const MAX_RECORDED_ENTRIES = 32768;

/**
 * Append to a log that keeps its most recent `cap` entries.
 *
 * The log is trimmed back to `cap` only once it reaches twice that, so a
 * reader sees between `cap` and `2 * cap` entries, newest last.
 *
 * It used to be `push` then `shift`, one out for each one in. Dropping the
 * head of a 32768-element array makes V8 move the other 32767: 64 us a
 * call, measured, against 0.5 us at the 4096 this cap had until it was
 * raised. Every tile is recorded in up to four of these logs, so once a
 * session had drawn its first ~30,000 tiles each further one cost a quarter
 * of a millisecond of bookkeeping -- on a screen redrawing 23,000 tiles a
 * second, the datagram loop fell two seconds behind and stopped
 * acknowledging, and the server read a loss-free link as dead.
 */
export function appendCapped<T>(arr: T[], item: T, cap: number = MAX_RECORDED_ENTRIES): void {
  arr.push(item);
  if (arr.length >= 2 * cap) arr.splice(0, arr.length - cap);
}

function fifoAppend<T>(arr: T[], item: T): void {
  appendCapped(arr, item);
}

/**
 * Mirror every palette atlas write into a test-readable global so the e2e
 * test can dump it post-run and compare against the server's write_bytes log
 * to detect (id, palette) drift. The fingerprint is the one the server logs
 * for the same palette, computed by the Rust function itself so the two
 * sides cannot drift. (The atlas itself is written inside wasm.)
 */
export function recordPaletteWrite(paletteId: number, bgra: Uint8Array): void {
  if (typeof window === 'undefined') return;
  const acc = protocol().paletteFingerprint(bgra);
  const fpHex = (acc >>> 0).toString(16).padStart(8, '0');
  const c0 = bgra.length >= 4
    ? Array.from(bgra.slice(0, 4)).map(b => b.toString(16).padStart(2, '0')).join('')
    : '';
  if (!window.__h2_clientPaletteWrites) window.__h2_clientPaletteWrites = [];
  appendCapped(window.__h2_clientPaletteWrites, {
    id: paletteId,
    len: bgra.byteLength,
    fp: fpHex,
    c0,
    ts: performance.now(),
  });
}

// ---------------------------------------------------------------------------
// Initialiser
// ---------------------------------------------------------------------------

/**
 * Wire the diagnostic globals onto `window` and return a handle main.ts uses
 * for per-event recording.
 *
 * @param reader     The renderer, for the pixel readbacks.
 * @param canvas     Kept for the handle's callers; unused by the readbacks.
 * @param readbacks  Install `__readPixel`, `__readPixelRect` and
 *                   `__readGradientGolden`. Off unless the page URL has
 *                   `?e2e`: they exist for the e2e suite, and a production
 *                   session should not carry code that can map the
 *                   framebuffer to the CPU.
 */
export function initDiagnostics({
  reader,
  readbacks = false,
}: {
  canvas?: HTMLCanvasElement;
  reader?: PixelReader;
  readbacks?: boolean;
}): DiagnosticsHandle {
  // ---- stats counter -------------------------------------------------------
  const stats: { tileDatagrams: number; frameDatagrams: number } = {
    tileDatagrams: 0,
    frameDatagrams: 0,
  };

  if (typeof window !== 'undefined') {
    window.__ghostframeStats = stats;
  }

  // ---- pixel readbacks (e2e only) ------------------------------------------
  if (readbacks && reader && typeof window !== 'undefined') {
    // [R, G, B, A], the framebuffer texture's own bytes.
    window.__readPixel = async (x: number, y: number): Promise<number[]> =>
      Array.from(await reader.readPixelRect(x, y, 1, 1));
    window.__readPixelRect = async (
      x: number, y: number, w: number, h: number,
    ): Promise<number[]> => (w <= 0 || h <= 0 ? [] : Array.from(await reader.readPixelRect(x, y, w, h)));
    // Debug-only: overwrite the framebuffer with a known gradient and read it
    // back, so e2e_readpixel_correctness can verify the readbacks before any
    // pixel assertion is trusted.
    window.__readGradientGolden = () => reader.readGradientGolden();
  }

  // ---- DiagnosticsHandle implementation ------------------------------------

  function recordResize(params: {
    seq: number;
    oldW: number; oldH: number;
    newW: number; newH: number;
    trigger: 'sentinel' | 'fallback-expand';
  }): void {
    if (typeof window === 'undefined') return;
    if (!window.__ghostframeRecordedResizes) {
      window.__ghostframeRecordedResizes = [];
    }
    fifoAppend(window.__ghostframeRecordedResizes, params);
  }

  function recordTile(params: {
    seq: number;
    tileX: number; tileY: number;
    codec: number;
    payloadLen: number;
    fbWidth: number; fbHeight: number;
    palRleFlag?: number;
    firstRecvMsClient?: number;
    lastPaintMsClient?: number;
  }): void {
    if (typeof window === 'undefined') return;
    if (!window.__ghostframeRecordedCodecs) window.__ghostframeRecordedCodecs = [];
    if (!window.__ghostframeRecordedTiles) window.__ghostframeRecordedTiles = [];
    if (!window.__ghostframeRecordedFlags) window.__ghostframeRecordedFlags = [];

    fifoAppend(window.__ghostframeRecordedCodecs, params.codec);
    if (params.palRleFlag !== undefined) {
      fifoAppend(window.__ghostframeRecordedFlags, params.palRleFlag);
    }
    fifoAppend(window.__ghostframeRecordedTiles, {
      seq: params.seq,
      tileX: params.tileX,
      tileY: params.tileY,
      codec: params.codec,
      payloadLen: params.payloadLen,
      fbWidth: params.fbWidth,
      fbHeight: params.fbHeight,
      firstRecvMsClient: params.firstRecvMsClient,
      lastPaintMsClient: params.lastPaintMsClient,
    });
  }

  // ---- frame paint FIFO (M3.5 bench) -----------------------------------
  const framePaints: Array<{ seq: number; rafMsClient: number }> = [];
  if (typeof window !== 'undefined') {
    window.__ghostframe_framePaints = framePaints;
  }

  function recordFramePainted(params: { seq: number; rafMsClient: number }): void {
    fifoAppend(framePaints, params);
  }

  function recordRafTick(n: number): void {
    if (typeof window !== 'undefined') {
      window.__ghostframeRafTicks = n;
    }
  }

  return { stats, recordResize, recordTile, recordRafTick, recordFramePainted };
}
