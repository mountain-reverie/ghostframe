import h264Wgsl from './shaders/h264_blit.wgsl?raw';

export interface H264Tile {
  tileX: number;   // tile coords; -1 for full-frame
  tileY: number;
  frame: VideoFrame;
}

export class H264Pipeline {
  private pipeline: GPURenderPipeline;
  private sampler: GPUSampler;

  constructor(private device: GPUDevice) {
    this.sampler = device.createSampler({
      magFilter: 'linear',
      minFilter: 'linear',
      addressModeU: 'clamp-to-edge',
      addressModeV: 'clamp-to-edge',
    });
    const module = device.createShaderModule({ code: h264Wgsl });
    this.pipeline = device.createRenderPipeline({
      layout: 'auto',
      vertex: { module, entryPoint: 'vs_main' },
      fragment: {
        module,
        entryPoint: 'fs_main',
        targets: [{ format: 'rgba8unorm' }],
      },
      primitive: { topology: 'triangle-list' },
    });
  }

  /** Draw one H.264 tile (or full-frame if tileX === -1) into the current pass. */
  drawTile(
    pass: GPURenderPassEncoder,
    tile: H264Tile,
    canvasWidth: number,
    canvasHeight: number,
  ): void {
    const ext = this.device.importExternalTexture({ source: tile.frame });
    const bindGroup = this.device.createBindGroup({
      layout: this.pipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: ext },
        { binding: 1, resource: this.sampler },
      ],
    });
    pass.setPipeline(this.pipeline);
    pass.setBindGroup(0, bindGroup);
    if (tile.tileX < 0) {
      pass.setViewport(0, 0, canvasWidth, canvasHeight, 0, 1);
    } else {
      pass.setViewport(tile.tileX * 32, tile.tileY * 32, 32, 32, 0, 1);
    }
    pass.draw(6, 1, 0, 0);
  }
}
