import { initWebGpu, type WebGpuInitResult } from './init.js';
import { Framebuffer } from './framebuffer.js';
import { SolidPipeline, type SolidTile } from './solid.js';
import { H264Pipeline } from './h264.js';
import { PalRlePipeline } from './palrle.js';
import { Cdf53Pipeline } from './cdf53.js';
import { PaletteShadow } from '../palette_shadow.js';
import { prevalidatePalRle, PalRleVariant, type PalRleEntry } from '../prevalidate.js';
import type { PrevalidatedCdf53 } from '../prevalidate_cdf53.js';
import { shouldSkipEncodePresent } from './idle_skip.js';

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
  /**
   * `null` while we haven't probed yet, or permanently if the browser
   * doesn't expose `texture_external` (Firefox WebGPU as of 2026 lacks
   * the TEXTURE_EXTERNAL capability needed by h264_blit.wgsl). Any
   * H.264 VideoFrames received while this is null are dropped on the
   * draw path with a one-shot warning.
   */
  h264Pipeline: H264Pipeline | null = null;
  /** Set once the probe completes (success or failure). */
  private h264Probed = false;
  /** Set true if the probe definitively determined H.264 is unsupported. */
  private h264Unsupported = false;
  /** Whether we've already warned about dropped H.264 frames. */
  private h264DropWarned = false;
  palrlePipeline: PalRlePipeline;
  cdf53Pipeline: Cdf53Pipeline;

  palRleQueue: PalRleQueued[] = [];
  solidQueue: SolidTile[] = [];
  rawQueue: RawQueued[] = [];
  h264Queue: VideoFrame[] = [];
  cdf53Queue: PrevalidatedCdf53[] = [];

  // Bug B: idle-skip. Set true whenever the renderer mutates state that
  // affects the next presented frame (queue uploads, raw tile writes,
  // session reset). Cleared after a real GPU submit. When false AND all
  // queues are empty, encodeAndPresentFrame returns without touching the
  // GPU.
  private framebufferDirty: boolean = false;

  // Test/diag instrumentation: increments on every real device.queue.submit
  // call. Wired to window.__gpuSubmitCount in encodeAndPresentFrame so the
  // page log can show a sliding submit rate next to the existing raf:N
  // counter. An idle page should grow raf without growing submit.
  public submitCount: number = 0;

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
    this.palrlePipeline = new PalRlePipeline(gpu.device);
    this.cdf53Pipeline = new Cdf53Pipeline(gpu.device);
  }

  static async create(canvas: HTMLCanvasElement): Promise<WebGpuRenderer> {
    const gpu = await initWebGpu(canvas);
    const renderer = new WebGpuRenderer(gpu, canvas);
    await renderer.probeH264();
    return renderer;
  }

  /**
   * Build the H264Pipeline inside a `validation` error scope so a shader
   * compilation failure (e.g. Firefox missing TEXTURE_EXTERNAL capability)
   * is caught and reported without poisoning the rest of the device's
   * error scope. If the probe fails, h264Pipeline stays null and the
   * draw path silently skips H.264 frames — the rest of the codec suite
   * (solid / palrle / cdf53 / raw / present_blit) is unaffected.
   */
  private async probeH264(): Promise<void> {
    this.gpu.device.pushErrorScope('validation');
    let pipeline: H264Pipeline | null = null;
    try {
      pipeline = new H264Pipeline(this.gpu.device);
    } catch (e) {
      console.warn('[renderer] H264Pipeline construction threw:', e);
    }
    const err = await this.gpu.device.popErrorScope();
    if (err === null && pipeline !== null) {
      this.h264Pipeline = pipeline;
      console.info('[renderer] H264 pipeline available');
    } else {
      this.h264Unsupported = true;
      const reason = err ? err.message : 'pipeline constructor returned no value';
      console.warn(
        `[renderer] H264 pipeline unsupported on this browser; H.264 frames will be dropped. Reason: ${reason}`,
      );
    }
    this.h264Probed = true;
  }

  get device(): GPUDevice { return this.gpu.device; }
  get context(): GPUCanvasContext { return this.gpu.context; }
  get presentFormat(): GPUTextureFormat { return this.gpu.presentFormat; }
  /**
   * Whether the H.264 render pipeline compiled successfully at probe
   * time. Future work: surface this via the HELLO capability bits so the
   * server can elect not to send H.264 frames to clients that can't
   * decode them. For now the daemon may still send H.264 and the client
   * drops them on the floor with a one-shot warning.
   */
  get h264Supported(): boolean {
    return this.h264Pipeline !== null;
  }

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
    const cols = Math.ceil(width / 32);
    this.cdf53Pipeline.resize(maxTiles, this.framebuffer.view, cols);
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
   * Must be called BEFORE the caller closes the FullFrameDecoder so that the
   * videoFramesToClose drain runs first and avoids double-close on the
   * decoder's latestFrame.
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

    // Close queued-but-not-yet-drawn H.264 full-frame VideoFrames so their
    // backing resources are released promptly, then clear the queue so stale
    // frames from a previous session don't bleed into the first rAF of the next.
    for (const f of this.h264Queue) {
      try { f.close(); } catch { /* already closed — safe to swallow */ }
    }
    this.h264Queue.length = 0;

    const zeros = new Uint8Array(16 * 1024);
    this.device.queue.writeBuffer(this.palrlePipeline.paletteAtlas, 0, zeros);
    this.paletteShadow.clear();
    this.cdf53Pipeline.clearAllState();
    // Session reset wipes the framebuffer texture (caller is expected to
    // resize / repaint); flag dirty so the next encodeAndPresentFrame
    // actually does the present blit instead of skipping.
    this.framebufferDirty = true;
  }

  pushPalRle(entry: PalRleQueued): void {
    this.palRleQueue.push(entry);
    this.framebufferDirty = true;
  }

  pushSolid(entry: SolidTile): void {
    this.solidQueue.push(entry);
    this.framebufferDirty = true;
  }

  pushRaw(entry: RawQueued): void {
    this.rawQueue.push(entry);
    this.framebufferDirty = true;
  }

  pushCdf53(entry: PrevalidatedCdf53): void {
    this.cdf53Queue.push(entry);
    this.framebufferDirty = true;
  }

  pushH264(frame: VideoFrame): void {
    this.h264Queue.push(frame);
    this.framebufferDirty = true;
  }

  /**
   * Drain all per-codec queues and emit one frame on the swapchain.
   * Driven by requestAnimationFrame; never blocks on missing tiles.
   * Returns counts for logging.
   */
  encodeAndPresentFrame(
    onDecodeError: (codec: number, tileX: number, tileY: number, code: number) => void,
  ): { palrle: number; solid: number; raw: number; h264: number; cdf53: number } {
    // Guard: if the framebuffer hasn't been sized yet (resize(w,h) with w,h>0 not
    // called), skip the GPU work — no texture targets are valid yet.
    if (!this.framebuffer.texture) {
      return { palrle: 0, solid: 0, raw: 0, h264: 0, cdf53: 0 };
    }
    // Expose framebuffer for test instrumentation (__readPixel / __readPixelRect).
    window.__ghostframeRenderer = { device: this.gpu.device, texture: this.framebuffer.texture };
    // Bug B: idle-skip — when no queue has work AND nothing has
    // mutated the framebuffer since the last submit AND no errors
    // readback is mid-flight, the GPU pipeline would produce the same
    // swapchain contents we already showed. Skip the whole thing so
    // the RAF loop doesn't burn ~10 ms of GPU work per tick on idle
    // sessions.
    if (
      shouldSkipEncodePresent({
        rawQueueLen: this.rawQueue.length,
        solidQueueLen: this.solidQueue.length,
        palRleQueueLen: this.palRleQueue.length,
        cdf53QueueLen: this.cdf53Queue.length,
        h264QueueLen: this.h264Queue.length,
        framebufferDirty: this.framebufferDirty,
        errorsReadbackInFlight: this.errorsReadbackInFlight,
      })
    ) {
      return { palrle: 0, solid: 0, raw: 0, h264: 0, cdf53: 0 };
    }
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

    // ---- Step 7.5: upload Cdf53 batch + integrate + inverse ----
    const cdf53Count = this.cdf53Pipeline.uploadBatch(this.cdf53Queue);
    this.cdf53Pipeline.encodeIntegrate(encoder, cdf53Count);
    this.cdf53Pipeline.encodeInverse(encoder);
    this.cdf53Queue.length = 0;

    // ---- Step 8: Solid + H264 render pass on framebuffer ----
    // Drop queued H.264 frames if the browser doesn't support the
    // h264_blit shader (probed at startup). Warn once so the operator
    // sees why the stream looks frozen on Firefox.
    const drawH264 = this.h264Pipeline !== null;
    if (!drawH264 && this.h264Queue.length > 0 && !this.h264DropWarned) {
      console.warn(
        `[renderer] dropping ${this.h264Queue.length} H.264 frame(s) — pipeline unsupported on this browser`,
      );
      this.h264DropWarned = true;
    }
    if (solidCount > 0 || (drawH264 && this.h264Queue.length > 0)) {
      const pass = encoder.beginRenderPass({
        colorAttachments: [{
          view: this.framebuffer.view,
          loadOp: 'load',
          storeOp: 'store',
        }],
      });
      this.solidPipeline.draw(pass, solidCount);
      if (drawH264) {
        for (const frame of this.h264Queue) {
          this.h264Pipeline!.drawFullFrame(pass, frame, this.framebuffer.width, this.framebuffer.height);
        }
      }
      pass.end();
    }
    const h264Count = drawH264 ? this.h264Queue.length : 0;
    // Stage the VideoFrames for next-tick cleanup; the importExternalTexture
    // references are scoped to this submit. (Still need to close dropped
    // frames so they don't leak GPU memory in the WebCodecs decoder.)
    this.videoFramesToClose.push(...this.h264Queue);
    this.h264Queue.length = 0;

    // ---- Step 9: present blit ----
    const swapTex = this.context.getCurrentTexture();
    this.framebuffer.encodePresentBlit(encoder, swapTex.createView());

    // ---- Step 10: errors copy ----
    this.palrlePipeline.encodeErrorsReadback(encoder, palRleCount);

    // ---- Step 11: submit ----
    this.device.queue.submit([encoder.finish()]);
    this.submitCount++;
    this.framebufferDirty = false;
    (window as unknown as { __gpuSubmitCount?: number }).__gpuSubmitCount = this.submitCount;

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
      }).catch((err) => {
        // mapAsync rejects on device loss, OOM, or validation failure.
        // Without this log, a silently-broken errors readback path is
        // nearly impossible to diagnose — the lib looks alive but stops
        // reporting decode errors.
        console.warn('palrle errors mapAsync failed:', err);
        this.errorsReadbackInFlight = false;
      });
    }

    // ---- Step 14: cleanup consumed VideoFrames ----
    for (const f of this.videoFramesToClose) f.close();
    this.videoFramesToClose.length = 0;

    return { palrle: palRleCount, solid: solidCount, raw: rawCount, h264: h264Count, cdf53: cdf53Count };
  }
}
