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
  /**
   * Reset any pending expected-frame chunks and clear the cached buffer.
   * Call before kicking off a new __appendExpectedFrameChunk sequence.
   */
  __resetExpectedFrame(): void;
  /**
   * Append one base64 chunk of the expected RGBA buffer. Multi-MB
   * buffers must be chunked because Chromium / CDP stalls on very large
   * eval expressions. Returns the running chunk count.
   */
  __appendExpectedFrameChunk(chunkB64: string): { chunks: number };
  /**
   * Concatenate the appended chunks, base64-decode, and swap the result
   * in as the comparator's expected buffer. Returns the decoded length
   * for sanity-checking against width*height*4.
   */
  __finalizeExpectedFrame(): { length: number };
  /**
   * Seed the expected RGBA buffer used by __compareFullFrame in a single
   * shot. Only safe for small buffers — prefer the chunked
   * __append / __finalize path for full-frame buffers.
   */
  __setExpectedFrame(expectedB64: string): { length: number };
  /**
   * Compare the live framebuffer to the buffer previously seeded via
   * __setExpectedFrame. Returns a small summary suitable for
   * high-frequency polling; the framebuffer bytes never cross CDP. The
   * failure-path PNG artifact is captured Playwright-style via CDP
   * Page.captureScreenshot.
   */
  __compareFullFrame(): Promise<{
    ready: boolean;
    width: number;
    height: number;
    match: boolean;
    mismatchCount: number;
    firstX?: number;
    firstY?: number;
    gotRGBA?: [number, number, number, number];
    wantRGBA?: [number, number, number, number];
  }>;

  // ----- cdf53 coverage map (populated by main.ts datagram loop) -------------
  /** Per-tile pass-coverage Map keyed by (tileX<<8 | tileY). */
  __cdf53Coverage: Map<number, { generation: number; passMask: number }> | undefined;
  /** Coverage counts derived from __cdf53Coverage. */
  __getCdf53Coverage(): { refined: number; partial: number; total: number };
}
