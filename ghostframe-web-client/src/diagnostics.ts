/**
 * diagnostics.ts — test and e2e instrumentation surface.
 *
 * Owns all `window.__ghostframe*` / `window.__read*` globals so that
 * main.ts is free of scattered `(window as any)` casts.  The module
 * wires the globals onto `window` unconditionally (the
 * `typeof window !== "undefined"` guard keeps it safe in Vitest /
 * Node environments).  E2E tests poll the same property names as
 * before; nothing on the wire-protocol side changes.
 *
 * Call order in main.ts:
 *   1. const diag = initDiagnostics({ canvas, getRenderer });
 *   2. Use diag.stats.tileDatagrams++ etc. in the datagram loop.
 *   3. Call diag.recordResize / diag.recordTile / diag.recordRafTick
 *      from finishAssembly / the rAF tick as needed.
 */

import debugGradientWgsl from './webgpu/shaders/debug_gradient.wgsl?raw';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Shape returned by initDiagnostics. */
export interface DiagnosticsHandle {
  /** Mutable stats counters — main.ts increments these directly. */
  stats: { tileDatagrams: number; frameDatagrams: number };

  /** Record a framebuffer resize (called from finishAssembly). */
  recordResize(params: {
    seq: number;
    oldW: number; oldH: number;
    newW: number; newH: number;
    trigger: 'sentinel' | 'fallback-expand';
  }): void;

  /** Record an assembled tile (called from finishAssembly). */
  recordTile(params: {
    seq: number;
    tileX: number; tileY: number;
    codec: number;
    payloadLen: number;
    fbWidth: number; fbHeight: number;
    /** First byte of payload; only pushed when codec === PalRle (2). */
    palRleFlag?: number;
  }): void;

  /** Update the rAF tick counter on window. */
  recordRafTick(n: number): void;
}

/** Subset of the renderer the GPU read-back helpers need. */
interface RendererRef {
  device: GPUDevice;
  texture: GPUTexture;
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

function fifoAppend<T>(arr: T[], item: T): void {
  arr.push(item);
  if (arr.length > MAX_RECORDED_ENTRIES) arr.shift();
}

// ---------------------------------------------------------------------------
// Initialiser
// ---------------------------------------------------------------------------

/**
 * Wire all diagnostic globals onto `window` and return a handle that
 * main.ts uses for per-event recording.
 *
 * @param getRenderer  Zero-arg callback that returns the current
 *                     {device, texture} pair from the renderer, or null
 *                     if the framebuffer hasn't been configured yet.
 *                     The readback helpers call this on every invocation
 *                     so they always use the up-to-date texture.
 * @param canvas       Fallback canvas for the 2-D read-back path (used
 *                     when the renderer isn't available yet).
 */
export function initDiagnostics({
  getRenderer,
  canvas,
}: {
  getRenderer: () => RendererRef | null;
  canvas: HTMLCanvasElement;
}): DiagnosticsHandle {
  // ---- stats counter -------------------------------------------------------
  const stats: { tileDatagrams: number; frameDatagrams: number } = {
    tileDatagrams: 0,
    frameDatagrams: 0,
  };

  if (typeof window !== 'undefined') {
    window.__ghostframeStats = stats;
  }

  // ---- __readPixel ---------------------------------------------------------
  if (typeof window !== 'undefined') {
    window.__readPixel = async (x: number, y: number): Promise<number[]> => {
      const ref = getRenderer();
      if (!ref) {
        const tmp = document.createElement('canvas');
        tmp.width = 1; tmp.height = 1;
        const ctx = tmp.getContext('2d')!;
        ctx.drawImage(canvas, x, y, 1, 1, 0, 0, 1, 1);
        return Array.from(ctx.getImageData(0, 0, 1, 1).data);
      }
      const { device, texture } = ref;
      // Row size must be a multiple of 256 bytes.
      const bytesPerRow = 256;
      const staging = device.createBuffer({
        size: bytesPerRow,
        usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
      });
      const enc = device.createCommandEncoder();
      enc.copyTextureToBuffer(
        { texture, origin: { x, y } },
        { buffer: staging, bytesPerRow },
        [1, 1],
      );
      device.queue.submit([enc.finish()]);
      await staging.mapAsync(GPUMapMode.READ);
      const view = new Uint8Array(staging.getMappedRange(0, 4));
      const result = Array.from(view) as number[];
      staging.unmap();
      staging.destroy();
      return result; // [R, G, B, A]
    };
  }

  // ---- __readPixelRect -----------------------------------------------------
  if (typeof window !== 'undefined') {
    window.__readPixelRect = async (
      x: number, y: number, w: number, h: number,
    ): Promise<number[]> => {
      if (w <= 0 || h <= 0) return [];
      const ref = getRenderer();
      if (!ref) {
        const tmp = document.createElement('canvas');
        tmp.width = w; tmp.height = h;
        const ctx = tmp.getContext('2d')!;
        ctx.drawImage(canvas, x, y, w, h, 0, 0, w, h);
        return Array.from(ctx.getImageData(0, 0, w, h).data);
      }
      const { device, texture } = ref;
      // bytesPerRow must be multiple of 256
      const bytesPerRow = Math.ceil(w * 4 / 256) * 256;
      const staging = device.createBuffer({
        size: bytesPerRow * h,
        usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
      });
      const enc = device.createCommandEncoder();
      enc.copyTextureToBuffer(
        { texture, origin: { x, y } },
        { buffer: staging, bytesPerRow },
        [w, h],
      );
      device.queue.submit([enc.finish()]);
      await staging.mapAsync(GPUMapMode.READ);
      // Compact: remove row padding
      const raw = new Uint8Array(staging.getMappedRange());
      const result: number[] = [];
      for (let row = 0; row < h; row++) {
        for (let col = 0; col < w * 4; col++) {
          result.push(raw[row * bytesPerRow + col]);
        }
      }
      staging.unmap();
      staging.destroy();
      return result;
    };
  }

  // ---- __readGradientGolden ------------------------------------------------
  // Debug-only: dispatch a compute pipeline that writes a known gradient
  // (r=x&255, g=y&255, b=0, a=255) to the framebuffer, then read back via
  // __readPixel at sample points.  Used by e2e_readpixel_correctness to
  // verify __readPixel before any production-codec pixel assertion is
  // trusted.  See spec/2026-05-15-m3.2c-verification-design.md §W2.
  if (typeof window !== 'undefined') {
    window.__readGradientGolden = async (): Promise<{
      ok: boolean;
      mismatches: Array<{ x: number; y: number; got: number[]; want: number[] }>;
    }> => {
      const ref = getRenderer();
      if (!ref) {
        return { ok: false, mismatches: [{ x: -1, y: -1, got: [], want: [] }] };
      }
      const { device, texture } = ref;
      const fbW = texture.width;
      const fbH = texture.height;
      if (fbW === 0 || fbH === 0) {
        return { ok: false, mismatches: [{ x: -1, y: -1, got: [], want: [] }] };
      }
      // Build the debug compute pipeline (fresh per call — cheap, debug-only).
      const module = device.createShaderModule({ code: debugGradientWgsl });
      const pipeline = device.createComputePipeline({
        layout: 'auto',
        compute: { module, entryPoint: 'debug_gradient' },
      });
      const bindGroup = device.createBindGroup({
        layout: pipeline.getBindGroupLayout(0),
        entries: [{ binding: 0, resource: texture.createView() }],
      });
      const encoder = device.createCommandEncoder();
      const pass = encoder.beginComputePass();
      pass.setPipeline(pipeline);
      pass.setBindGroup(0, bindGroup);
      pass.dispatchWorkgroups(Math.ceil(fbW / 8), Math.ceil(fbH / 8), 1);
      pass.end();
      device.queue.submit([encoder.finish()]);
      await device.queue.onSubmittedWorkDone();

      // Sample 16 points — corners, edges, near-256 wrap boundaries, center.
      const samplePoints: Array<[number, number]> = [
        [0, 0], [1, 0], [255, 0], [256, 0],
        [0, 1], [0, 255], [0, 256],
        [100, 200], [16, 16], [31, 31], [32, 32], [63, 63],
        [Math.floor(fbW / 2), Math.floor(fbH / 2)],
        [fbW - 1, fbH - 1],
        [Math.min(255, fbW - 1), Math.min(255, fbH - 1)],
        [Math.min(256, fbW - 1), Math.min(256, fbH - 1)],
      ];
      const mismatches: Array<{ x: number; y: number; got: number[]; want: number[] }> = [];
      for (const [x, y] of samplePoints) {
        if (x >= fbW || y >= fbH || x < 0 || y < 0) continue;
        const got = await window.__readPixel(x, y);
        const want = [x & 0xFF, y & 0xFF, 0, 255];
        if (got[0] !== want[0] || got[1] !== want[1] || got[2] !== want[2] || got[3] !== want[3]) {
          mismatches.push({ x, y, got, want });
        }
      }
      return { ok: mismatches.length === 0, mismatches };
    };
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
    });
  }

  function recordRafTick(n: number): void {
    if (typeof window !== 'undefined') {
      window.__ghostframeRafTicks = n;
    }
  }

  return { stats, recordResize, recordTile, recordRafTick };
}
