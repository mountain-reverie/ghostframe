/// <reference types="@webgpu/types" />

declare module '*.wgsl?raw' {
  const content: string;
  export default content;
}

// Type augmentation for test-instrumentation / e2e-readback properties
// exposed on `window` by diagnostics.ts and renderer.ts.
// Polled by Playwright in the ghostframe e2e suite
// (see `ghostframe-lib/tests/e2e.rs`).

interface Window {
  // ----- stats counters (set at startup, incremented in the datagram loop) -----
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
  /** Full-framebuffer GPU readback. rgba length is width*height*4 (no row padding). */
  __readFullFrame(): Promise<{ width: number; height: number; rgba: number[] }>;

  // ----- cdf53 coverage map (populated by main.ts datagram loop) -------------
  /** Per-tile pass-coverage Map keyed by (tileX<<8 | tileY). */
  __cdf53Coverage: Map<number, { generation: number; passMask: number }> | undefined;
  /** Coverage counts derived from __cdf53Coverage. */
  __getCdf53Coverage(): { refined: number; partial: number; total: number };
}
