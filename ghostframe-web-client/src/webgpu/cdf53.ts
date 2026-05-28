import type { PrevalidatedCdf53 } from '../prevalidate_cdf53.js';
import integrateWgsl from './shaders/cdf53_integrate.wgsl?raw';
import inverseL3Wgsl from './shaders/cdf53_inverse_l3.wgsl?raw';
import inverseL2Wgsl from './shaders/cdf53_inverse_l2.wgsl?raw';
import inverseL1Wgsl from './shaders/cdf53_inverse_l1.wgsl?raw';
import inverseL1Pass2Wgsl from './shaders/cdf53_inverse_l1_pass2.wgsl?raw';

/**
 * Per-tile persistent state for client-side Cdf53 decode.
 *
 * Layout:
 * - coefficientBuffer: max_tiles × 3 channels × 1024 coeffs, packed as two
 *   i16 lanes per u32 ⇒ max_tiles × 1536 u32 ⇒ max_tiles × 6144 bytes.
 * - signBuffer:        max_tiles × 3 × 1024 bits, packed as u32 words
 *                      ⇒ max_tiles × 96 u32 ⇒ max_tiles × 384 bytes.
 * - tileGenBuffer:     max_tiles × u32 (most-recently-seen generation per tile).
 * - dirtyTilesBuffer:  max_tiles × u32 (tile indices visited this batch).
 * - dirtyTilesCount:   1 × u32 (atomic counter, reset each batch).
 *
 * Cleared on `clearAllState()` (called from `WebGpuRenderer.onSessionReset`).
 */
export class Cdf53Pipeline {
  // Persistent per-tile state.
  coefficientBuffer!: GPUBuffer;
  signBuffer!: GPUBuffer;
  tileGenBuffer!: GPUBuffer;
  // Per-batch scratch.
  dirtyTilesBuffer!: GPUBuffer;
  dirtyTilesCount!: GPUBuffer;
  // Per-batch upload targets (sized to maxTiles × 14 worst case).
  tileWorkBuffer!: GPUBuffer;
  bitPlanesBuffer!: GPUBuffer;
  // Inverse-transform scratch (3 channels × 32 × 32 × 4 bytes per tile).
  workAreaBuffer!: GPUBuffer;

  private maxTiles = 0;
  private framebufferView!: GPUTextureView;

  private integratePipeline!: GPUComputePipeline;
  private integrateBindGroup!: GPUBindGroup;
  private uniformsBuffer: GPUBuffer;
  private inverseL3Pipeline!: GPUComputePipeline;
  private inverseL2Pipeline!: GPUComputePipeline;
  private inverseL1Pipeline!: GPUComputePipeline;
  private inverseL1Pass2Pipeline!: GPUComputePipeline;
  private inverseL3BindGroup!: GPUBindGroup;
  private inverseL2BindGroup!: GPUBindGroup;
  private inverseL1BindGroup!: GPUBindGroup;
  private inverseL1Pass2BindGroup!: GPUBindGroup;

  constructor(private device: GPUDevice) {
    this.uniformsBuffer = device.createBuffer({
      size: 16, // 1 u32 cols + padding to 16 B
      usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST,
    });
    this.integratePipeline = device.createComputePipeline({
      layout: 'auto',
      compute: {
        module: device.createShaderModule({ code: integrateWgsl }),
        entryPoint: 'main',
      },
    });
    this.inverseL3Pipeline = device.createComputePipeline({
      layout: 'auto',
      compute: { module: device.createShaderModule({ code: inverseL3Wgsl }), entryPoint: 'main' },
    });
    this.inverseL2Pipeline = device.createComputePipeline({
      layout: 'auto',
      compute: { module: device.createShaderModule({ code: inverseL2Wgsl }), entryPoint: 'main' },
    });
    this.inverseL1Pipeline = device.createComputePipeline({
      layout: 'auto',
      compute: { module: device.createShaderModule({ code: inverseL1Wgsl }), entryPoint: 'main' },
    });
    this.inverseL1Pass2Pipeline = device.createComputePipeline({
      layout: 'auto',
      compute: { module: device.createShaderModule({ code: inverseL1Pass2Wgsl }), entryPoint: 'main' },
    });
  }

  /**
   * Allocate per-tile buffers for the given max-tile budget. Idempotent for
   * the same maxTiles; reallocates on growth.
   */
  resize(maxTiles: number, framebufferView: GPUTextureView, frameCols: number): void {
    this.framebufferView = framebufferView;
    const colsBuf = new Uint32Array([frameCols, 0, 0, 0]);
    this.device.queue.writeBuffer(this.uniformsBuffer, 0, colsBuf);
    if (maxTiles === this.maxTiles) return;
    // Destroy old buffers if any.
    if (this.coefficientBuffer) this.coefficientBuffer.destroy();
    if (this.signBuffer) this.signBuffer.destroy();
    if (this.tileGenBuffer) this.tileGenBuffer.destroy();
    if (this.dirtyTilesBuffer) this.dirtyTilesBuffer.destroy();
    if (this.dirtyTilesCount) this.dirtyTilesCount.destroy();
    if (this.tileWorkBuffer) this.tileWorkBuffer.destroy();
    if (this.bitPlanesBuffer) this.bitPlanesBuffer.destroy();
    if (this.workAreaBuffer) this.workAreaBuffer.destroy();

    this.coefficientBuffer = this.device.createBuffer({
      size: maxTiles * 6144,
      usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
    });
    this.signBuffer = this.device.createBuffer({
      size: maxTiles * 384,
      usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
    });
    this.tileGenBuffer = this.device.createBuffer({
      size: maxTiles * 4,
      usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
    });
    this.dirtyTilesBuffer = this.device.createBuffer({
      size: Math.max(maxTiles, 1) * 4,
      usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST | GPUBufferUsage.COPY_SRC,
    });
    this.dirtyTilesCount = this.device.createBuffer({
      size: 16, // 4 u32: count, 3 padding (also reused as indirect-dispatch args buffer in later task)
      usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST | GPUBufferUsage.COPY_SRC | GPUBufferUsage.INDIRECT,
    });
    // Per-batch budget: a batch holds at most max_tiles × 14 passes;
    // each entry needs 5 u32 of tileWork + 96 u32 of bit-planes.
    const maxBatchEntries = Math.max(maxTiles, 1) * 14;
    this.tileWorkBuffer = this.device.createBuffer({
      size: maxBatchEntries * 5 * 4,
      usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
    });
    this.bitPlanesBuffer = this.device.createBuffer({
      size: maxBatchEntries * 96 * 4,
      usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
    });
    // 3 channels × 32 × 32 × 4 bytes per tile = 12 KB per tile.
    this.workAreaBuffer = this.device.createBuffer({
      size: maxTiles * 12288,
      usage: GPUBufferUsage.STORAGE,
    });
    this.integrateBindGroup = this.device.createBindGroup({
      layout: this.integratePipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: { buffer: this.tileWorkBuffer } },
        { binding: 1, resource: { buffer: this.bitPlanesBuffer } },
        { binding: 2, resource: { buffer: this.coefficientBuffer } },
        { binding: 3, resource: { buffer: this.signBuffer } },
        { binding: 4, resource: { buffer: this.tileGenBuffer } },
        { binding: 5, resource: { buffer: this.dirtyTilesBuffer } },
        { binding: 6, resource: { buffer: this.dirtyTilesCount } },
        { binding: 7, resource: { buffer: this.uniformsBuffer } },
      ],
    });
    this.inverseL3BindGroup = this.device.createBindGroup({
      layout: this.inverseL3Pipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: { buffer: this.coefficientBuffer } },
        { binding: 1, resource: { buffer: this.signBuffer } },
        { binding: 2, resource: { buffer: this.dirtyTilesBuffer } },
        { binding: 3, resource: { buffer: this.workAreaBuffer } },
      ],
    });
    this.inverseL2BindGroup = this.device.createBindGroup({
      layout: this.inverseL2Pipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: { buffer: this.coefficientBuffer } },
        { binding: 1, resource: { buffer: this.signBuffer } },
        { binding: 2, resource: { buffer: this.dirtyTilesBuffer } },
        { binding: 3, resource: { buffer: this.workAreaBuffer } },
      ],
    });
    this.inverseL1BindGroup = this.device.createBindGroup({
      layout: this.inverseL1Pipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: { buffer: this.coefficientBuffer } },
        { binding: 1, resource: { buffer: this.signBuffer } },
        { binding: 2, resource: { buffer: this.dirtyTilesBuffer } },
        { binding: 3, resource: { buffer: this.workAreaBuffer } },
        { binding: 4, resource: framebufferView },
        { binding: 5, resource: { buffer: this.uniformsBuffer } },
      ],
    });
    this.inverseL1Pass2BindGroup = this.device.createBindGroup({
      layout: this.inverseL1Pass2Pipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: { buffer: this.dirtyTilesBuffer } },
        { binding: 1, resource: { buffer: this.workAreaBuffer } },
        { binding: 2, resource: framebufferView },
        { binding: 3, resource: { buffer: this.uniformsBuffer } },
      ],
    });
    this.maxTiles = maxTiles;
  }

  /** Zero all persistent per-tile state. Called from onSessionReset. */
  clearAllState(): void {
    if (this.maxTiles === 0) return;
    const coefZeros = new Uint8Array(this.maxTiles * 6144);
    const signZeros = new Uint8Array(this.maxTiles * 384);
    const genZeros = new Uint8Array(this.maxTiles * 4);
    this.device.queue.writeBuffer(this.coefficientBuffer, 0, coefZeros);
    this.device.queue.writeBuffer(this.signBuffer, 0, signZeros);
    this.device.queue.writeBuffer(this.tileGenBuffer, 0, genZeros);
  }

  /** Returns the framebuffer view for binding by the L1 inverse shader. */
  get framebuffer(): GPUTextureView {
    return this.framebufferView;
  }

  /** Current maxTiles allocation (for callers that need to bound work). */
  get capacity(): number {
    return this.maxTiles;
  }

  // ---- Stubs filled by later tasks ----
  uploadBatch(entries: readonly PrevalidatedCdf53[]): number {
    if (entries.length === 0) return 0;
    if (entries.length > this.maxTiles * 14) {
      throw new Error(
        `Cdf53 batch ${entries.length} exceeds capacity ${this.maxTiles * 14}`,
      );
    }
    // tileWork: 5 u32 per entry — tileX, tileY, gen, passIdx, bitPlanesOffset.
    const tileWork = new Uint32Array(entries.length * 5);
    // bitPlanes: 96 u32 = 384 bytes per entry, concatenated.
    const bitPlanes = new Uint8Array(entries.length * 384);
    for (let i = 0; i < entries.length; i++) {
      const e = entries[i];
      tileWork[i * 5 + 0] = e.tileX;
      tileWork[i * 5 + 1] = e.tileY;
      tileWork[i * 5 + 2] = e.gen;
      tileWork[i * 5 + 3] = e.passIdx;
      tileWork[i * 5 + 4] = i * 96; // u32 offset = i * 96 u32 = i * 384 bytes
      bitPlanes.set(e.bitPlanes, i * 384);
    }
    this.device.queue.writeBuffer(this.tileWorkBuffer, 0, tileWork);
    this.device.queue.writeBuffer(this.bitPlanesBuffer, 0, bitPlanes);
    // Reset per-batch atomic counter + the leading dirtyTilesBuffer region.
    const countZero = new Uint32Array([0, 0, 0, 0]);
    this.device.queue.writeBuffer(this.dirtyTilesCount, 0, countZero);
    const dirtyZeros = new Uint32Array(Math.min(entries.length, this.maxTiles));
    this.device.queue.writeBuffer(this.dirtyTilesBuffer, 0, dirtyZeros);
    return entries.length;
  }
  encodeIntegrate(encoder: GPUCommandEncoder, batchSize: number): void {
    if (batchSize === 0) return;
    const pass = encoder.beginComputePass();
    pass.setPipeline(this.integratePipeline);
    pass.setBindGroup(0, this.integrateBindGroup);
    pass.dispatchWorkgroups(batchSize);
    pass.end();
  }
  encodeInverse(encoder: GPUCommandEncoder): void {
    // Three inverse passes (L3, L2, L1), then a final L1-pass2 that writes pixels.
    // We dispatch at fixed maxTiles and let the shaders early-return on indices
    // ≥ count.  (Wasted workgroups are bounded and cheap; profile if hot.)
    const wgCap = Math.max(this.maxTiles, 1);
    {
      const pass = encoder.beginComputePass();
      pass.setPipeline(this.inverseL3Pipeline);
      pass.setBindGroup(0, this.inverseL3BindGroup);
      pass.dispatchWorkgroups(wgCap);
      pass.end();
    }
    {
      const pass = encoder.beginComputePass();
      pass.setPipeline(this.inverseL2Pipeline);
      pass.setBindGroup(0, this.inverseL2BindGroup);
      pass.dispatchWorkgroups(wgCap);
      pass.end();
    }
    {
      const pass = encoder.beginComputePass();
      pass.setPipeline(this.inverseL1Pipeline);
      pass.setBindGroup(0, this.inverseL1BindGroup);
      pass.dispatchWorkgroups(wgCap * 4);
      pass.end();
    }
    {
      const pass = encoder.beginComputePass();
      pass.setPipeline(this.inverseL1Pass2Pipeline);
      pass.setBindGroup(0, this.inverseL1Pass2BindGroup);
      pass.dispatchWorkgroups(wgCap);
      pass.end();
    }
  }
}
