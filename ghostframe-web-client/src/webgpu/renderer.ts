import { initWebGpu, type WebGpuInitResult } from './init.js';
import { Framebuffer } from './framebuffer.js';
import { SolidPipeline, type SolidTile } from './solid.js';
import { H264Pipeline } from './h264.js';
import { PalRlePipeline } from './palrle.js';

export class WebGpuRenderer {
  framebuffer: Framebuffer;
  solidPipeline: SolidPipeline;
  h264Pipeline: H264Pipeline;
  palrlePipeline: PalRlePipeline;

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
    if (this.canvas.width !== width || this.canvas.height !== height) {
      this.canvas.width = width;
      this.canvas.height = height;
    }
    this.framebuffer.resize(width, height);
    this.solidPipeline.updateCanvasSize(width, height);
    // Recompute MAX_TILES from canvas dimensions; reallocate per-tile buffers.
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
    const rgba = new Uint8Array(bgra.length);
    for (let i = 0; i < bgra.length; i += 4) {
      rgba[i + 0] = bgra[i + 2]; // R from B-slot
      rgba[i + 1] = bgra[i + 1]; // G
      rgba[i + 2] = bgra[i + 0]; // B from R-slot
      rgba[i + 3] = bgra[i + 3] === 0 ? 255 : bgra[i + 3]; // force alpha (BGRX quirk)
    }
    this.device.queue.writeTexture(
      { texture: this.framebuffer.texture, origin: { x: tileX * 32, y: tileY * 32 } },
      rgba,
      { bytesPerRow: 32 * 4, rowsPerImage: 32 },
      { width: 32, height: 32 },
    );
  }

  /** Render one rAF tick — for now, only the present blit. */
  encodeFrame(): void {
    const swapTex = this.context.getCurrentTexture();
    const encoder = this.device.createCommandEncoder();
    this.framebuffer.encodePresentBlit(encoder, swapTex.createView());
    this.device.queue.submit([encoder.finish()]);
  }
}
