import presentBlitWgsl from './shaders/present_blit.wgsl?raw';
import { createLabeledShaderModule } from './shader_module';

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
    // Same WGSL source for both stages — share the module so we only get
    // one compilation-info log per pipeline (and so the diagnostic name is
    // unambiguous).
    const blitModule = createLabeledShaderModule(device, 'present_blit', presentBlitWgsl);
    this.blitPipeline = device.createRenderPipeline({
      layout: 'auto',
      vertex: {
        module: blitModule,
        entryPoint: 'vs_main',
      },
      fragment: {
        module: blitModule,
        entryPoint: 'fs_main',
        targets: [{ format: this.presentFormat }],
      },
      primitive: { topology: 'triangle-list' },
    });
  }

  resize(width: number, height: number): void {
    if (this.texture && this.width === width && this.height === height) return;
    const oldTexture = this.texture;
    const oldW = this.width;
    const oldH = this.height;
    this.texture = this.device.createTexture({
      size: { width, height },
      format: 'rgba8unorm',
      usage:
        GPUTextureUsage.STORAGE_BINDING |
        GPUTextureUsage.TEXTURE_BINDING |
        GPUTextureUsage.RENDER_ATTACHMENT |
        GPUTextureUsage.COPY_DST |
        GPUTextureUsage.COPY_SRC,
    });
    // Preserve already-rendered content across resize. Without this copy,
    // any PalRle / Solid / H264 tiles written into the framebuffer before the
    // sentinel-driven resize (which can land late on a slow QUIC startup)
    // are lost when the texture is destroyed — the new texture is
    // zero-initialized per spec.  M3.2c B1 root cause.
    if (oldTexture && oldW > 0 && oldH > 0 && width > 0 && height > 0) {
      const copyW = Math.min(oldW, width);
      const copyH = Math.min(oldH, height);
      const encoder = this.device.createCommandEncoder();
      encoder.copyTextureToTexture(
        { texture: oldTexture },
        { texture: this.texture },
        { width: copyW, height: copyH },
      );
      this.device.queue.submit([encoder.finish()]);
    }
    if (oldTexture) oldTexture.destroy();
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
