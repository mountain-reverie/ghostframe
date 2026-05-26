import h264Wgsl from './shaders/h264_blit.wgsl?raw';

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

  /** Draw a full-frame H.264 VideoFrame into the current pass. */
  drawFullFrame(
    pass: GPURenderPassEncoder,
    frame: VideoFrame,
    canvasWidth: number,
    canvasHeight: number,
  ): void {
    const ext = this.device.importExternalTexture({ source: frame });
    const bindGroup = this.device.createBindGroup({
      layout: this.pipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: ext },
        { binding: 1, resource: this.sampler },
      ],
    });
    pass.setPipeline(this.pipeline);
    pass.setBindGroup(0, bindGroup);
    pass.setViewport(0, 0, canvasWidth, canvasHeight, 0, 1);
    pass.draw(6, 1, 0, 0);
  }
}
