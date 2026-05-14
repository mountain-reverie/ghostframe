import solidWgsl from './shaders/solid.wgsl?raw';

export interface SolidTile {
  tileX: number;
  tileY: number;
  bgra: Uint8Array; // 4 bytes
}

export class SolidPipeline {
  private pipeline: GPURenderPipeline;
  private canvasSizeBuffer: GPUBuffer;
  private canvasSizeBindGroup!: GPUBindGroup;
  instanceBuffer!: GPUBuffer;
  instanceCapacity = 0;

  constructor(private device: GPUDevice) {
    this.canvasSizeBuffer = device.createBuffer({
      size: 8,
      usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST,
    });

    const module = device.createShaderModule({ code: solidWgsl });
    this.pipeline = device.createRenderPipeline({
      layout: 'auto',
      vertex: {
        module,
        entryPoint: 'vs_main',
        buffers: [
          // Vertex buffer slot 0 is unused (we compute positions from vertex_index)
          // — but WebGPU requires a non-empty buffers array if we use vertex stage attrs.
          // Instead we put instance attrs on slot 0 with stepMode 'instance'.
          {
            arrayStride: 12, // 3 × u32 = 12 bytes per instance
            stepMode: 'instance',
            attributes: [
              { shaderLocation: 1, format: 'uint32', offset: 0 },  // tile_x
              { shaderLocation: 2, format: 'uint32', offset: 4 },  // tile_y
              { shaderLocation: 3, format: 'uint32', offset: 8 },  // color_packed
            ],
          },
        ],
      },
      fragment: {
        module,
        entryPoint: 'fs_main',
        targets: [{ format: 'rgba8unorm' }],
      },
      primitive: { topology: 'triangle-list' },
    });
  }

  updateCanvasSize(width: number, height: number): void {
    const data = new Uint32Array([width, height]);
    this.device.queue.writeBuffer(this.canvasSizeBuffer, 0, data);
    this.canvasSizeBindGroup = this.device.createBindGroup({
      layout: this.pipeline.getBindGroupLayout(0),
      entries: [{ binding: 0, resource: { buffer: this.canvasSizeBuffer } }],
    });
  }

  /** Ensure the instance buffer can hold at least `capacity` tiles. */
  ensureCapacity(capacity: number): void {
    if (capacity <= this.instanceCapacity) return;
    if (this.instanceBuffer) this.instanceBuffer.destroy();
    this.instanceBuffer = this.device.createBuffer({
      size: capacity * 12,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
    this.instanceCapacity = capacity;
  }

  /** Pack tile data into the instance buffer; returns the count written. */
  packAndUpload(tiles: readonly SolidTile[]): number {
    if (tiles.length === 0) return 0;
    this.ensureCapacity(tiles.length);
    const data = new Uint32Array(tiles.length * 3);
    for (let i = 0; i < tiles.length; i++) {
      const t = tiles[i];
      const packed =
        t.bgra[0] |
        (t.bgra[1] << 8) |
        (t.bgra[2] << 16) |
        (t.bgra[3] << 24);
      data[i * 3 + 0] = t.tileX;
      data[i * 3 + 1] = t.tileY;
      data[i * 3 + 2] = packed >>> 0; // ensure unsigned
    }
    this.device.queue.writeBuffer(this.instanceBuffer, 0, data);
    return tiles.length;
  }

  /** Issue the instanced draw inside an existing render pass. */
  draw(pass: GPURenderPassEncoder, instanceCount: number): void {
    if (instanceCount === 0) return;
    pass.setPipeline(this.pipeline);
    pass.setBindGroup(0, this.canvasSizeBindGroup);
    pass.setVertexBuffer(0, this.instanceBuffer);
    pass.draw(6, instanceCount, 0, 0);
  }
}
