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

  /** Render one rAF tick — for now, only the present blit. */
  encodeFrame(): void {
    const swapTex = this.context.getCurrentTexture();
    const encoder = this.device.createCommandEncoder();
    this.framebuffer.encodePresentBlit(encoder, swapTex.createView());
    this.device.queue.submit([encoder.finish()]);
  }
}
