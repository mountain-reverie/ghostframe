import { initWebGpu, type WebGpuInitResult } from './init.js';
import { Framebuffer } from './framebuffer.js';
import { SolidPipeline, type SolidTile } from './solid.js';
import { H264Pipeline, type H264Tile } from './h264.js';
import { PalRlePipeline } from './palrle.js';
import { PaletteShadow } from '../palette_shadow.js';
import { prevalidatePalRle, PalRleVariant, type PalRleEntry } from '../prevalidate.js';

export interface PalRleQueued {
  tileX: number;
  tileY: number;
  payload: Uint8Array;
}

export interface RawQueued {
  tileX: number;
  tileY: number;
  bgra: Uint8Array;
}

export class WebGpuRenderer {
  framebuffer: Framebuffer;
  solidPipeline: SolidPipeline;
  h264Pipeline: H264Pipeline;
  palrlePipeline: PalRlePipeline;

  palRleQueue: PalRleQueued[] = [];
  solidQueue: SolidTile[] = [];
  rawQueue: RawQueued[] = [];
  h264Queue: H264Tile[] = [];

  paletteShadow = new PaletteShadow();
  private errorsReadbackInFlight = false;
  // Held until next tick so they aren't closed before drawing.
  private videoFramesToClose: VideoFrame[] = [];

  private constructor(
    private gpu: WebGpuInitResult,
    private canvas: HTMLCanvasElement,
  ) {
    this.framebuffer = new Framebuffer(gpu.device, gpu.presentFormat);
    this.solidPipeline = new SolidPipeline(gpu.device);
    this.h264Pipeline = new H264Pipeline(gpu.device);
    this.palrlePipeline = new PalRlePipeline(gpu.device);
  }

  static async create(canvas: HTMLCanvasElement): Promise<WebGpuRenderer> {
    const gpu = await initWebGpu(canvas);
    return new WebGpuRenderer(gpu, canvas);
  }

  get device(): GPUDevice { return this.gpu.device; }
  get context(): GPUCanvasContext { return this.gpu.context; }
  get presentFormat(): GPUTextureFormat { return this.gpu.presentFormat; }

  resize(width: number, height: number): void {
    // Zero dimensions are a no-op: we can't create valid GPU textures with 0×0
    // size, and no tiles can exist before the frame dimensions sentinel arrives.
    if (width === 0 || height === 0) return;
    if (this.canvas.width !== width || this.canvas.height !== height) {
      this.canvas.width = width;
      this.canvas.height = height;
      // Changing canvas.width / canvas.height invalidates the WebGPU canvas
      // context (per spec the current texture is expired and the swap-chain
      // must be reconfigured).  Re-call configure() so getCurrentTexture()
      // returns a correctly-sized texture on the next rAF tick.
      this.context.configure({
        device: this.gpu.device,
        format: this.gpu.presentFormat,
        alphaMode: 'opaque',
      });
    }
    this.framebuffer.resize(width, height);
    this.solidPipeline.updateCanvasSize(width, height);
    const maxTiles = Math.ceil(width / 32) * Math.ceil(height / 32);
    this.palrlePipeline.resize(maxTiles, this.framebuffer.view);
  }

  /**
   * Upload a Raw tile's BGRA payload directly into the framebuffer's
   * 32×32 region. Swaps BGRA→RGBA on CPU before upload (framebuffer
   * format is rgba8unorm per Design D12).
   */
  writeRawTile(tileX: number, tileY: number, bgra: Uint8Array): void {
    if (bgra.length !== 32 * 32 * 4) {
      throw new Error(`writeRawTile: payload length ${bgra.length} != 4096`);
    }
    // Clip the destination extent to the framebuffer bounds — partial edge
    // tiles at non-32-aligned resolutions (e.g. 700×500 in e2e_edge_tiles)
    // would otherwise trip a WebGPU validation error
    // ("origin + size > texture.size") and silently drop the entire tile.
    const fbW = this.framebuffer.width;
    const fbH = this.framebuffer.height;
    const dstX = tileX * 32;
    const dstY = tileY * 32;
    if (dstX >= fbW || dstY >= fbH) return; // tile entirely outside framebuffer
    const copyW = Math.min(32, fbW - dstX);
    const copyH = Math.min(32, fbH - dstY);

    // BGRA→RGBA swap, only for the bytes we'll actually upload. Pack tightly
    // (bytesPerRow = copyW * 4) so writeTexture sees a contiguous source.
    const rgba = new Uint8Array(copyW * copyH * 4);
    for (let row = 0; row < copyH; row++) {
      for (let col = 0; col < copyW; col++) {
        const srcOff = (row * 32 + col) * 4;
        const dstOff = (row * copyW + col) * 4;
        rgba[dstOff + 0] = bgra[srcOff + 2]; // R from B-slot
        rgba[dstOff + 1] = bgra[srcOff + 1]; // G
        rgba[dstOff + 2] = bgra[srcOff + 0]; // B from R-slot
        rgba[dstOff + 3] = bgra[srcOff + 3] === 0 ? 255 : bgra[srcOff + 3]; // force alpha (BGRX quirk)
      }
    }
    this.device.queue.writeTexture(
      { texture: this.framebuffer.texture, origin: { x: dstX, y: dstY } },
      rgba,
      { bytesPerRow: copyW * 4, rowsPerImage: copyH },
      { width: copyW, height: copyH },
    );
  }

  /**
   * Zero the palette atlas + CPU shadow on session reset (transport.closed).
   * Framebuffer texture is left in place; next session paints over.
   *
   * Must be called BEFORE the caller closes any H264TileDecoder instances so
   * that the videoFramesToClose drain runs first and avoids double-close.
   */
  onSessionReset(): void {
    // Drain pending VideoFrames before any per-decoder close() runs.  Frames
    // here have already been submitted to the GPU; we close them eagerly so
    // that the subsequent dec.close() in main.ts doesn't hit a double-close
    // hazard on latestFrame.
    for (const f of this.videoFramesToClose) {
      try { f.close(); } catch { /* already closed — safe to swallow */ }
    }
    this.videoFramesToClose = [];

    // Clear queued-but-not-yet-drawn H.264 tiles so stale frames from a
    // previous session don't bleed into the first rAF of the next one.
    this.h264Queue.length = 0;

    const zeros = new Uint8Array(16 * 1024);
    this.device.queue.writeBuffer(this.palrlePipeline.paletteAtlas, 0, zeros);
    this.paletteShadow.clear();
  }

  /**
   * Drain all per-codec queues and emit one frame on the swapchain.
   * Driven by requestAnimationFrame; never blocks on missing tiles.
   * Returns counts for logging.
   */
  encodeAndPresentFrame(
    onDecodeError: (codec: number, tileX: number, tileY: number, code: number) => void,
  ): { palrle: number; solid: number; raw: number; h264: number } {
    // Guard: if the framebuffer hasn't been sized yet (resize(w,h) with w,h>0 not
    // called), skip the GPU work — no texture targets are valid yet.
    if (!this.framebuffer.texture) {
      return { palrle: 0, solid: 0, raw: 0, h264: 0 };
    }
    // Expose framebuffer for test instrumentation (__readPixel / __readPixelRect).
    (window as any).__ghostframeRenderer = { device: this.gpu.device, texture: this.framebuffer.texture };
    // ---- Steps 1-2: Drain palette updates + pre-validate PalRle batch ----
    const palRleEntries: PalRleEntry[] = [];
    for (const q of this.palRleQueue) {
      const r = prevalidatePalRle(q.payload, this.paletteShadow, q.tileX, q.tileY);
      if (!r.ok) {
        onDecodeError(2 /* Codec.PalRle */, q.tileX, q.tileY, r.errorCode);
        continue;
      }
      // Bundled upserts palette to the atlas immediately so subsequent
      // thin/indices_raw entries in the same rAF see the palette.
      if (r.entry.variant === PalRleVariant.Bundled && r.entry.paletteUpsert) {
        this.palrlePipeline.upsertPalette(r.entry.paletteId, r.entry.paletteUpsert);
        this.paletteShadow.put(r.entry.paletteId, r.entry.count);
      }
      palRleEntries.push(r.entry);
    }
    this.palRleQueue.length = 0;

    // ---- Step 3: pack Solid instances ----
    const solidCount = this.solidPipeline.packAndUpload(this.solidQueue);
    this.solidQueue.length = 0;

    // ---- Step 4: direct Raw uploads (pre-encoder) ----
    for (const r of this.rawQueue) {
      this.writeRawTile(r.tileX, r.tileY, r.bgra);
    }
    const rawCount = this.rawQueue.length;
    this.rawQueue.length = 0;

    // ---- Step 5: upload PalRle batched buffers ----
    const palRleCount = this.palrlePipeline.uploadBatch(palRleEntries);

    // ---- Step 6: encoder ----
    const encoder = this.device.createCommandEncoder();

    // ---- Step 7: PalRle compute pass ----
    this.palrlePipeline.encodeDispatch(encoder, palRleCount);

    // ---- Step 8: Solid + H264 render pass on framebuffer ----
    if (solidCount > 0 || this.h264Queue.length > 0) {
      const pass = encoder.beginRenderPass({
        colorAttachments: [{
          view: this.framebuffer.view,
          loadOp: 'load',
          storeOp: 'store',
        }],
      });
      this.solidPipeline.draw(pass, solidCount);
      for (const h of this.h264Queue) {
        this.h264Pipeline.drawTile(pass, h, this.framebuffer.width, this.framebuffer.height);
      }
      pass.end();
    }
    const h264Count = this.h264Queue.length;
    // Stage the VideoFrames for next-tick cleanup; the importExternalTexture
    // references are scoped to this submit.
    this.videoFramesToClose.push(...this.h264Queue.map((h) => h.frame));
    this.h264Queue.length = 0;

    // ---- Step 9: present blit ----
    const swapTex = this.context.getCurrentTexture();
    this.framebuffer.encodePresentBlit(encoder, swapTex.createView());

    // ---- Step 10: errors copy ----
    this.palrlePipeline.encodeErrorsReadback(encoder, palRleCount);

    // ---- Step 11: submit ----
    this.device.queue.submit([encoder.finish()]);

    // ---- Step 12: zero errors for next tick ----
    if (palRleCount > 0) {
      this.palrlePipeline.zeroErrors();
    }

    // ---- Step 13: async errors readback (one rAF latency) ----
    if (palRleCount > 0 && !this.errorsReadbackInFlight) {
      this.errorsReadbackInFlight = true;
      const batch = palRleCount;
      const staging = this.palrlePipeline.errorsStaging;
      staging.mapAsync(GPUMapMode.READ, 0, batch * 4).then(() => {
        const view = new Uint32Array(staging.getMappedRange(0, batch * 4));
        for (let i = 0; i < batch; i++) {
          if (view[i] !== 0) {
            // Map back to (tileX, tileY) via the same order palRleEntries had
            // — but we've already consumed palRleEntries. For M3.2b, we report
            // the in-shader error with batch-relative coordinates that the
            // caller can resolve from a saved index→tile map.
            // SIMPLIFICATION: emit a generic per-batch error without tile coords.
            // The DecodeErrorBatcher's per-key rate-limit still bounds spam.
            onDecodeError(2, 0xFF, 0xFF, view[i]); // 0xFF placeholder tile coords
          }
        }
        staging.unmap();
        this.errorsReadbackInFlight = false;
      }).catch(() => { this.errorsReadbackInFlight = false; });
    }

    // ---- Step 14: cleanup consumed VideoFrames ----
    for (const f of this.videoFramesToClose) f.close();
    this.videoFramesToClose.length = 0;

    return { palrle: palRleCount, solid: solidCount, raw: rawCount, h264: h264Count };
  }
}
