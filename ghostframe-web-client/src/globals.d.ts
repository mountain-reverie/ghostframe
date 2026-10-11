/// <reference types="@webgpu/types" />

declare module '*.wgsl?raw' {
  const content: string;
  export default content;
}

// Type augmentation for test-instrumentation / e2e-readback properties
// exposed on `window` by diagnostics.ts and renderer.ts.
// Polled over CDP by the ghostframe e2e suite (`ghostframe-e2e/tests/e2e.rs`;
// `e2e_polled_globals_are_live` guards that each stays non-trivial).

interface Window {
  // ----- stats counters (set at startup, re-published from the core) -----
  /** Datagram counts re-published from the wasm core each loop iteration. */
  __ghostframeStats: { tileDatagrams: number; frameDatagrams: number } | undefined;
  /** rAF tick counter; incremented every requestAnimationFrame callback. */
  __ghostframeRafTicks: number | undefined;

  // ----- renderer reference (set per-frame in renderer.ts) --------------------
  __ghostframeRenderer: { device: GPUDevice; texture: GPUTexture } | undefined;

  // ----- recorder arrays (FIFO-capped, pushed from diagnostics.ts) ------------
  __ghostframeRecordedCodecs: number[] | undefined;
  __ghostframeRecordedFlags: number[] | undefined;
  __ghostframeRecordedResizes: Array<{
    seq: number;
    oldW: number; oldH: number;
    newW: number; newH: number;
    trigger: 'sentinel' | 'fallback-expand';
  }> | undefined;
  __ghostframeRecordedTiles: Array<{
    seq: number;
    tileX: number; tileY: number;
    codec: number;
    payloadLen: number;
    fbWidth: number;
    fbHeight: number;
    // M3.5 bench instrumentation — optional fields.
    firstRecvMsClient?: number;
    lastPaintMsClient?: number;
  }> | undefined;
  /** M3.5 bench: per-frame paint timestamps, keyed by frame seq. */
  __ghostframe_framePaints: Array<{ seq: number; rafMsClient: number }> | undefined;

  // ----- GPU readback helpers (set at startup by diagnostics.ts) ---------------
  __readPixel(x: number, y: number): Promise<number[]>;
  __readPixelRect(x: number, y: number, w: number, h: number): Promise<number[]>;
  __readGradientGolden(): Promise<{
    ok: boolean;
    mismatches: Array<{ x: number; y: number; got: number[]; want: number[] }>;
  }>;

  // ----- cdf53 protocol counters (cdf53_globals.ts, fed from core events) ----
  __cdf53DispatchSeen: number | undefined;
  __cdf53PushedToQueue: number | undefined;
  __cdf53PrevalidateFails: number | undefined;
  __cdf53LastFailCode: number | undefined;

  /** Core-side CDF53 coverage + the worst incomplete tiles (main.ts). */
  __cdf53CoverageSummary(): { summary: unknown; incomplete: unknown };

  // ----- ?debug only (cdf53_debug.ts); absent in a normal session ------------
  __cdf53TestInverse?: (coeffsI16: number[], tileIdx?: number) => Promise<number[]>;
  __cdf53TestIntegrate?: (
    encodedPasses: number[][],
  ) => Promise<{ coefficients: number[]; signs: number[] }>;
  __cdf53DumpTileState?: (
    tileIdx: number,
  ) => Promise<{ tileGen: number; coefficients: number[]; signs: number[] }>;
  __cdf53SetTileWatcher?: (tileX: number, tileY: number) => void;
  __cdf53GetTileWatcher?: () => unknown;
  __cdf53Probe?: () => unknown;

  // ----- [H2-DIAG] client-side palette atlas writes -------------------------
  /** Per-call record of palrlePipeline.upsertPalette(); FIFO-capped. */
  __h2_clientPaletteWrites: Array<{
    /** palette slot id (0..255). */
    id: number;
    /** byteLength of the bgra buffer the client uploaded. */
    len: number;
    /** FNV-1a fingerprint of the bgra bytes; matches server-side fp. */
    fp: string;
    /** First 4 bytes of bgra as hex (the first colour). */
    c0: string;
    /** performance.now() at the call. */
    ts: number;
  }> | undefined;

  // ----- [H5-DIAG] per-tile push log to detect OOO arrivals -----------------
  /**
   * Every finishAssembly() that routes a tile to a renderer queue pushes
   * one entry. Used to detect "older Solid(BLACK) arrives after newer
   * PalRle for the same tile" — that pattern proves an out-of-order
   * delivery wipes the PalRle in the same rAF.
   */
  __h5_tilePushLog: Array<{
    /** frameSeq (without TILE_DATAGRAM_FLAG). */
    seq: number;
    /** tile column. */
    tx: number;
    /** tile row. */
    ty: number;
    /** TileHeader.codec value (1=Solid, 2=PalRle, 5=Cdf53, etc.). */
    codec: number;
    /** For Solid: first 4 BGRA bytes as hex; '' for other codecs. */
    c0: string;
    /** performance.now() at queue push. */
    ts: number;
  }> | undefined;
}
