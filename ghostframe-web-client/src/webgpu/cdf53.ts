import type { PrevalidatedCdf53 } from '../prevalidate_cdf53.js';

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

  private maxTiles = 0;
  private framebufferView!: GPUTextureView;

  constructor(private device: GPUDevice) {}

  /**
   * Allocate per-tile buffers for the given max-tile budget. Idempotent for
   * the same maxTiles; reallocates on growth.
   */
  resize(maxTiles: number, framebufferView: GPUTextureView): void {
    this.framebufferView = framebufferView;
    if (maxTiles === this.maxTiles) return;
    // Destroy old buffers if any.
    if (this.coefficientBuffer) this.coefficientBuffer.destroy();
    if (this.signBuffer) this.signBuffer.destroy();
    if (this.tileGenBuffer) this.tileGenBuffer.destroy();
    if (this.dirtyTilesBuffer) this.dirtyTilesBuffer.destroy();
    if (this.dirtyTilesCount) this.dirtyTilesCount.destroy();
    if (this.tileWorkBuffer) this.tileWorkBuffer.destroy();
    if (this.bitPlanesBuffer) this.bitPlanesBuffer.destroy();

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
  uploadBatch(_entries: readonly PrevalidatedCdf53[]): number {
    throw new Error('Cdf53Pipeline.uploadBatch — not yet implemented (Task 9)');
  }
  encodeIntegrate(_encoder: GPUCommandEncoder, _batchSize: number): void {
    throw new Error('Cdf53Pipeline.encodeIntegrate — not yet implemented (Task 7)');
  }
  encodeInverse(_encoder: GPUCommandEncoder): void {
    throw new Error('Cdf53Pipeline.encodeInverse — not yet implemented (Task 10)');
  }
}
