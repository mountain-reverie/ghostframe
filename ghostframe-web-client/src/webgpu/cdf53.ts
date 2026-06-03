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
 * - passesProcessedBuffer: max_tiles × u32 (highest-seen pass_idx + 1, per
 *                      tile). Used by the inverse shaders' read_coeff
 *                      midpoint-reconstruction: for K passes processed
 *                      (1 ≤ K < 14), the unknown low magnitude bits of any
 *                      significant coefficient are estimated as the midpoint
 *                      of [0, 2^(14-K)) rather than 0. Halves expected
 *                      reconstruction error per pass and makes the SSIM
 *                      curve monotonic in K. Mirrors the Rust
 *                      `cdf53::decode_passes` SPIHT-style correction.
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
  passesProcessedBuffer!: GPUBuffer;
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
  // Per-tile last-seen gen, host-side. When a queue entry's gen differs from
  // this, the host issues a writeBuffer that zeros that tile's coefficient
  // and sign-buffer regions BEFORE the integrate dispatch. This replaces the
  // intra-shader clear (which had a cross-workgroup race when multiple
  // passes for the same tile shared a single batch).
  private lastSeenGen: Uint32Array = new Uint32Array(0);
  // Reusable zero buffers, sized at resize() to avoid per-clear allocation.
  private coefZeroPerTile = new Uint8Array(0);
  private signZeroPerTile = new Uint8Array(0);
  // CPU-side mirror of passesProcessedBuffer. Updated in uploadBatch each
  // time an entry arrives (max with passIdx + 1), then writeBuffer'd to the
  // GPU before the integrate dispatch.
  private passesProcessedCpu: Uint32Array = new Uint32Array(0);
  private cols: number = 0;

  // M3.3b diagnostic: per-tile capture. When (watcherX, watcherY) is set,
  // every entry whose (tileX, tileY) matches gets snapshot-cloned into
  // tileWatcherCaptures at uploadBatch time — i.e., the exact bytes the
  // shader will OR-integrate. Separates JS-handoff correctness from
  // downstream integrate-shader correctness.
  private tileWatcherX: number | null = null;
  private tileWatcherY: number | null = null;
  tileWatcherCaptures: Array<{
    batchSize: number;
    entryIdx: number;
    tileX: number;
    tileY: number;
    gen: number;
    passIdx: number;
    bitPlanesOffset: number;
    bitPlanes: Uint8Array;
  }> = [];
  // Stats kept while a watcher is set. uploadBatchCalls counts every
  // uploadBatch invocation (including zero-length ones); totalEntries is
  // the sum of entries.length across them; seenTiles is the set of distinct
  // (tileX, tileY) tuples observed, encoded as `${x},${y}` strings.
  uploadBatchCalls = 0;
  totalEntries = 0;
  // Split: how many uploadBatch calls observed watcherX null vs set.
  uploadBatchWithWatcherNull = 0;
  uploadBatchWithWatcherSet = 0;
  entriesWhileWatcherNull = 0;
  entriesWhileWatcherSet = 0;
  seenTilesAddCalls = 0;
  seenTiles = new Set<string>();

  setTileWatcher(tileX: number, tileY: number): void {
    this.tileWatcherX = tileX;
    this.tileWatcherY = tileY;
    this.tileWatcherCaptures = [];
    // Don't reset uploadBatchCalls/totalEntries — we want lifetime totals so
    // we can see what happened BEFORE the watcher was set too.
    this.seenTiles = new Set<string>();
  }
  clearTileWatcher(): void {
    this.tileWatcherX = null;
    this.tileWatcherY = null;
    this.tileWatcherCaptures = [];
  }

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
      size: 32, // Uniforms { cols: u32, _pad: vec3<u32> } — vec3 alignment forces stride 32.
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
    this.cols = frameCols;
    if (maxTiles === this.maxTiles) return;
    this.lastSeenGen = new Uint32Array(maxTiles);
    this.coefZeroPerTile = new Uint8Array(6144);
    this.signZeroPerTile = new Uint8Array(384);
    // Destroy old buffers if any.
    if (this.coefficientBuffer) this.coefficientBuffer.destroy();
    if (this.signBuffer) this.signBuffer.destroy();
    if (this.tileGenBuffer) this.tileGenBuffer.destroy();
    if (this.passesProcessedBuffer) this.passesProcessedBuffer.destroy();
    if (this.dirtyTilesBuffer) this.dirtyTilesBuffer.destroy();
    if (this.dirtyTilesCount) this.dirtyTilesCount.destroy();
    if (this.tileWorkBuffer) this.tileWorkBuffer.destroy();
    if (this.bitPlanesBuffer) this.bitPlanesBuffer.destroy();
    if (this.workAreaBuffer) this.workAreaBuffer.destroy();

    this.coefficientBuffer = this.device.createBuffer({
      size: maxTiles * 6144,
      usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST | GPUBufferUsage.COPY_SRC,
    });
    this.signBuffer = this.device.createBuffer({
      size: maxTiles * 384,
      usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST | GPUBufferUsage.COPY_SRC,
    });
    this.tileGenBuffer = this.device.createBuffer({
      size: maxTiles * 4,
      usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST | GPUBufferUsage.COPY_SRC,
    });
    this.passesProcessedBuffer = this.device.createBuffer({
      size: maxTiles * 4,
      usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST | GPUBufferUsage.COPY_SRC,
    });
    this.passesProcessedCpu = new Uint32Array(maxTiles);
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
        { binding: 2, resource: { buffer: this.tileGenBuffer } },  // was dirtyTilesBuffer
        { binding: 3, resource: { buffer: this.workAreaBuffer } },
        { binding: 4, resource: { buffer: this.passesProcessedBuffer } },
      ],
    });
    this.inverseL2BindGroup = this.device.createBindGroup({
      layout: this.inverseL2Pipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: { buffer: this.coefficientBuffer } },
        { binding: 1, resource: { buffer: this.signBuffer } },
        { binding: 2, resource: { buffer: this.tileGenBuffer } },  // was dirtyTilesBuffer
        { binding: 3, resource: { buffer: this.workAreaBuffer } },
        { binding: 4, resource: { buffer: this.passesProcessedBuffer } },
      ],
    });
    this.inverseL1BindGroup = this.device.createBindGroup({
      layout: this.inverseL1Pipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: { buffer: this.coefficientBuffer } },
        { binding: 1, resource: { buffer: this.signBuffer } },
        { binding: 2, resource: { buffer: this.tileGenBuffer } },  // was dirtyTilesBuffer
        { binding: 3, resource: { buffer: this.workAreaBuffer } },
        { binding: 4, resource: { buffer: this.passesProcessedBuffer } },
      ],
    });
    this.inverseL1Pass2BindGroup = this.device.createBindGroup({
      layout: this.inverseL1Pass2Pipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: { buffer: this.tileGenBuffer } },  // was dirtyTilesBuffer
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
    const passesZeros = new Uint8Array(this.maxTiles * 4);
    this.device.queue.writeBuffer(this.coefficientBuffer, 0, coefZeros);
    this.device.queue.writeBuffer(this.signBuffer, 0, signZeros);
    this.device.queue.writeBuffer(this.tileGenBuffer, 0, genZeros);
    this.device.queue.writeBuffer(this.passesProcessedBuffer, 0, passesZeros);
    this.lastSeenGen.fill(0);
    this.passesProcessedCpu.fill(0);
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
    // M3.3b unconditional counters (not gated on watcher state). Used to
    // determine whether `entries` is non-empty when uploadBatch is called.
    this.uploadBatchCalls++;
    this.totalEntries += entries.length;
    if (this.tileWatcherX === null) {
      this.uploadBatchWithWatcherNull++;
      this.entriesWhileWatcherNull += entries.length;
    } else {
      this.uploadBatchWithWatcherSet++;
      this.entriesWhileWatcherSet += entries.length;
    }
    if (entries.length === 0) return 0;
    if (this.tileWatcherX !== null) {
      for (const e of entries) {
        this.seenTilesAddCalls++;
        this.seenTiles.add(`${e.tileX},${e.tileY}`);
      }
    }
    if (entries.length > this.maxTiles * 14) {
      throw new Error(
        `Cdf53 batch ${entries.length} exceeds capacity ${this.maxTiles * 14}`,
      );
    }
    // tileWork: 5 u32 per entry — tileX, tileY, gen, passIdx, bitPlanesOffset.
    const tileWork = new Uint32Array(entries.length * 5);
    // bitPlanes: 96 u32 = 384 bytes per entry, concatenated.
    const bitPlanes = new Uint8Array(entries.length * 384);
    // Per-tile gen transitions: clear those tiles BEFORE the integrate pass
    // sees them. writeBuffer is queued onto the device queue and ordered
    // before any subsequent submit, so the integrate compute pass observes
    // the zeroed state. Doing this avoids the cross-workgroup race the
    // intra-shader clear had (HL2 cols 4/5 sign-bit loss for ch=0).
    const clearedTiles = new Set<number>();
    const touchedTiles = new Set<number>();
    for (let i = 0; i < entries.length; i++) {
      const e = entries[i];
      const tileIdx = e.tileY * this.cols + e.tileX;
      if (this.lastSeenGen[tileIdx] !== e.gen && !clearedTiles.has(tileIdx)) {
        this.device.queue.writeBuffer(this.coefficientBuffer, tileIdx * 6144, this.coefZeroPerTile);
        this.device.queue.writeBuffer(this.signBuffer, tileIdx * 384, this.signZeroPerTile);
        // Gen bump also resets pass-count tracking for the tile.
        this.passesProcessedCpu[tileIdx] = 0;
        this.lastSeenGen[tileIdx] = e.gen;
        clearedTiles.add(tileIdx);
      }
      // Track highest-seen pass index for the midpoint-reconstruction in
      // the inverse shaders. Stores pass_idx + 1 so the GPU side reads
      // "number of processed passes" (matches Rust decode_passes semantics).
      const candidate = e.passIdx + 1;
      if (candidate > this.passesProcessedCpu[tileIdx]) {
        this.passesProcessedCpu[tileIdx] = candidate;
      }
      touchedTiles.add(tileIdx);
    }
    // Upload the (possibly sparse) passes-processed updates. For batch sizes
    // that touch many tiles this is more efficient than writing the whole
    // array; for small batches the per-tile writeBuffer overhead may be
    // higher than a full upload — left as a future optimization.
    for (const tileIdx of touchedTiles) {
      const tmp = new Uint32Array([this.passesProcessedCpu[tileIdx]]);
      this.device.queue.writeBuffer(this.passesProcessedBuffer, tileIdx * 4, tmp);
    }
    for (let i = 0; i < entries.length; i++) {
      const e = entries[i];
      tileWork[i * 5 + 0] = e.tileX;
      tileWork[i * 5 + 1] = e.tileY;
      tileWork[i * 5 + 2] = e.gen;
      tileWork[i * 5 + 3] = e.passIdx;
      tileWork[i * 5 + 4] = i * 96; // u32 offset = i * 96 u32 = i * 384 bytes
      bitPlanes.set(e.bitPlanes, i * 384);
      if (
        this.tileWatcherX !== null &&
        e.tileX === this.tileWatcherX &&
        e.tileY === this.tileWatcherY
      ) {
        this.tileWatcherCaptures.push({
          batchSize: entries.length,
          entryIdx: i,
          tileX: e.tileX,
          tileY: e.tileY,
          gen: e.gen,
          passIdx: e.passIdx,
          bitPlanesOffset: i * 96,
          bitPlanes: new Uint8Array(e.bitPlanes),
        });
      }
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
  /**
   * Encode the four-pass inverse compute chain.
   *
   * Always dispatches one workgroup per possible tile slot. Each inverse
   * shader uses `wg.x` directly as `tile_idx` and early-returns when
   * `tileGen[tile_idx] == 0` (tile has never received any integrate pass).
   * This keeps the framebuffer fresh across rAF ticks even when no new
   * passes arrive this frame — the coefficient state for previously-
   * integrated tiles is still in `coefficientBuffer + signBuffer`, so
   * re-running the inverse reproduces the same pixels.
   */
  encodeInverse(encoder: GPUCommandEncoder): void {
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
