import presentBlitWgsl from './shaders/present_blit.wgsl?raw';

export class Framebuffer {
  texture!: GPUTexture;
  view!: GPUTextureView;
  width: number = 0;
  height: number = 0;

  private blitPipeline: GPURenderPipeline;
  private sampler: GPUSampler;
  private blitBindGroup!: GPUBindGroup;

  constructor(
    private device: GPUDevice,
    private presentFormat: GPUTextureFormat,
  ) {
    this.sampler = device.createSampler({
      magFilter: 'nearest',
      minFilter: 'nearest',
      addressModeU: 'clamp-to-edge',
      addressModeV: 'clamp-to-edge',
    });
    this.blitPipeline = device.createRenderPipeline({
      layout: 'auto',
      vertex: {
        module: device.createShaderModule({ code: presentBlitWgsl }),
        entryPoint: 'vs_main',
      },
      fragment: {
        module: device.createShaderModule({ code: presentBlitWgsl }),
        entryPoint: 'fs_main',
        targets: [{ format: this.presentFormat }],
      },
      primitive: { topology: 'triangle-list' },
    });
  }

  resize(width: number, height: number): void {
    if (this.texture && this.width === width && this.height === height) return;
    if (this.texture) this.texture.destroy();
    this.texture = this.device.createTexture({
      size: { width, height },
      format: 'rgba8unorm',
      usage:
        GPUTextureUsage.STORAGE_BINDING |
        GPUTextureUsage.TEXTURE_BINDING |
        GPUTextureUsage.RENDER_ATTACHMENT |
        GPUTextureUsage.COPY_DST,
    });
    this.view = this.texture.createView();
    this.width = width;
    this.height = height;
    this.blitBindGroup = this.device.createBindGroup({
      layout: this.blitPipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: this.view },
        { binding: 1, resource: this.sampler },
      ],
    });
  }

  encodePresentBlit(encoder: GPUCommandEncoder, swapchainView: GPUTextureView): void {
    const pass = encoder.beginRenderPass({
      colorAttachments: [{
        view: swapchainView,
        loadOp: 'clear',
        clearValue: { r: 0, g: 0, b: 0, a: 1 },
        storeOp: 'store',
      }],
    });
    pass.setPipeline(this.blitPipeline);
    pass.setBindGroup(0, this.blitBindGroup);
    pass.draw(6, 1, 0, 0);
    pass.end();
  }
}
