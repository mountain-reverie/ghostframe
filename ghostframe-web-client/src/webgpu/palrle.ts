import palrleWgsl from './shaders/palrle_decode.wgsl?raw';
import type { PalRleEntry } from '../prevalidate.js';

export class PalRlePipeline {
  private pipeline: GPUComputePipeline;
  paletteAtlas: GPUBuffer;
  tileWorkBuffer!: GPUBuffer;
  indicesBuffer!: GPUBuffer;
  errorsBuffer!: GPUBuffer;
  errorsStaging!: GPUBuffer;

  private maxTiles = 0;
  private bindGroup!: GPUBindGroup;
  private framebufferView!: GPUTextureView;

  constructor(private device: GPUDevice) {
    this.paletteAtlas = device.createBuffer({
      size: 16 * 1024, // 256 × 16 × u32
      usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
    });
    const module = device.createShaderModule({ code: palrleWgsl });
    this.pipeline = device.createComputePipeline({
      layout: 'auto',
      compute: { module, entryPoint: 'main' },
    });
  }

  /** Reallocate per-tile buffers for the given max-tile budget; rebuild bind group. */
  resize(maxTiles: number, framebufferView: GPUTextureView): void {
    this.framebufferView = framebufferView;
    if (maxTiles !== this.maxTiles) {
      if (this.tileWorkBuffer) this.tileWorkBuffer.destroy();
      if (this.indicesBuffer) this.indicesBuffer.destroy();
      if (this.errorsBuffer) this.errorsBuffer.destroy();
      if (this.errorsStaging) this.errorsStaging.destroy();

      this.tileWorkBuffer = this.device.createBuffer({
        size: maxTiles * 32,
        usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
      });
      this.indicesBuffer = this.device.createBuffer({
        size: maxTiles * 512,
        usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
      });
      this.errorsBuffer = this.device.createBuffer({
        size: Math.max(maxTiles, 1) * 4,
        usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_SRC | GPUBufferUsage.COPY_DST,
      });
      this.errorsStaging = this.device.createBuffer({
        size: Math.max(maxTiles, 1) * 4,
        usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.COPY_DST,
      });
      this.maxTiles = maxTiles;
    }
    this.bindGroup = this.device.createBindGroup({
      layout: this.pipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: { buffer: this.paletteAtlas } },
        { binding: 1, resource: { buffer: this.tileWorkBuffer } },
        { binding: 2, resource: { buffer: this.indicesBuffer } },
        { binding: 3, resource: framebufferView },
        { binding: 4, resource: { buffer: this.errorsBuffer } },
      ],
    });
  }

  /** Upload a palette block (count × 4 BGRA bytes) into the atlas. */
  upsertPalette(paletteId: number, bgra: Uint8Array): void {
    // Atlas stores BGRA as u32 LSB-first; the wire format already matches.
    this.device.queue.writeBuffer(
      this.paletteAtlas,
      paletteId * 64, // 16 u32 = 64 bytes per palette slot
      bgra.buffer, bgra.byteOffset, bgra.byteLength,
    );
  }

  /** Pack tile_work + indices for a batch of PalRle entries; upload to GPU. */
  uploadBatch(entries: readonly PalRleEntry[]): number {
    if (entries.length === 0) return 0;
    if (entries.length > this.maxTiles) {
      throw new Error(`PalRle batch ${entries.length} exceeds maxTiles ${this.maxTiles}`);
    }
    const tileWork = new Uint32Array(entries.length * 8);
    const indices = new Uint8Array(entries.length * 512);
    for (let i = 0; i < entries.length; i++) {
      const e = entries[i];
      tileWork[i * 8 + 0] = e.tileX;
      tileWork[i * 8 + 1] = e.tileY;
      tileWork[i * 8 + 2] = e.paletteId;
      tileWork[i * 8 + 3] = e.count;
      tileWork[i * 8 + 4] = i * 512; // payload_off
      // pads (idx 5,6,7) stay 0
      indices.set(e.indices, i * 512);
    }
    this.device.queue.writeBuffer(this.tileWorkBuffer, 0, tileWork);
    this.device.queue.writeBuffer(this.indicesBuffer, 0, indices);
    return entries.length;
  }

  /** Encode the compute pass. */
  encodeDispatch(encoder: GPUCommandEncoder, batchSize: number): void {
    if (batchSize === 0) return;
    const pass = encoder.beginComputePass();
    pass.setPipeline(this.pipeline);
    pass.setBindGroup(0, this.bindGroup);
    pass.dispatchWorkgroups(batchSize, 1, 1);
    pass.end();
  }

  /** Copy errors → staging at the end of the encoder, before submit. */
  encodeErrorsReadback(encoder: GPUCommandEncoder, batchSize: number): void {
    if (batchSize === 0) return;
    encoder.copyBufferToBuffer(
      this.errorsBuffer, 0,
      this.errorsStaging, 0,
      batchSize * 4,
    );
  }

  /** Zero the errors buffer after submit (preparing for next tick). */
  zeroErrors(): void {
    if (this.maxTiles === 0) return;
    const zeros = new Uint8Array(this.maxTiles * 4);
    this.device.queue.writeBuffer(this.errorsBuffer, 0, zeros);
  }
}
