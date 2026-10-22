// A paint tick must never be rejected because the previous tick's PalRLE
// errors readback still holds `errorsStaging`.
//
// Every tick with PalRLE work copied the errors buffer into `errorsStaging`,
// and the first such tick also started `errorsStaging.mapAsync`. WebGPU
// rejects a whole `queue.submit` when any buffer it uses is not unmapped, and
// the staging buffer stays pending, then mapped, until the readback's `.then`
// unmaps it -- under load, later than the next rAF. The next tick with PalRLE
// work then copied into it, and the submit that also carried that tick's Solid
// draws, CDF 5/3 integrate and lift, PalRLE dispatch and present was dropped.
// Its tiles were already acknowledged, so nothing resent them. In the e2e
// failure Chrome said "[Buffer (unlabeled)] used in submit while mapped" once,
// at paint tick 2, and the solid and PalRLE regions stayed black and the CDF 5/3
// region wrong while the core held every pass.
//
// No GPU under vitest. The fake device enforces the one rule this is about:
// a submit using a buffer that is not unmapped is rejected, and none of its
// commands run.
import { beforeAll, beforeEach, describe, expect, it } from 'vitest';
import { WebGpuRenderer } from '../src/webgpu/renderer';
import { PalRlePipeline } from '../src/webgpu/palrle';

type FakeBuffer = {
  size: number;
  usage: number;
  mapState: 'unmapped' | 'pending' | 'mapped';
  resolveMap?: () => void;
  mapAsync(mode: number, offset?: number, size?: number): Promise<void>;
  getMappedRange(offset?: number, size?: number): ArrayBuffer;
  unmap(): void;
  destroy(): void;
};

type Op = { kind: string; tile?: [number, number] };

class FakeGpu {
  /** Commands that actually ran, in queue order. */
  ran: Op[] = [];
  rejected: string[] = [];
  buffers: FakeBuffer[] = [];

  device = {
    queue: {
      writeBuffer: () => {},
      writeTexture: () => {},
      submit: (cmds: Array<{ ops: Op[]; used: FakeBuffer[] }>) => {
        for (const c of cmds) {
          const busy = c.used.find((b) => b.mapState !== 'unmapped');
          if (busy) {
            this.rejected.push(`buffer used in submit while ${busy.mapState}`);
            return;
          }
        }
        for (const c of cmds) this.ran.push(...c.ops);
      },
    },
    createShaderModule: () => ({ getCompilationInfo: () => Promise.resolve({ messages: [] }) }),
    createComputePipeline: () => ({ getBindGroupLayout: () => ({}) }),
    createBindGroup: () => ({}),
    createBuffer: (d: { size: number; usage: number }) => {
      const b: FakeBuffer = {
        size: d.size,
        usage: d.usage,
        mapState: 'unmapped',
        mapAsync() {
          b.mapState = 'pending';
          return new Promise<void>((resolve) => {
            b.resolveMap = () => {
              b.mapState = 'mapped';
              resolve();
            };
          });
        },
        getMappedRange: (_o?: number, size?: number) => new ArrayBuffer(size ?? d.size),
        unmap() { b.mapState = 'unmapped'; },
        destroy() {},
      };
      this.buffers.push(b);
      return b;
    },
    createCommandEncoder: () => {
      const enc = {
        ops: [] as Op[],
        used: [] as FakeBuffer[],
        beginComputePass: () => ({
          setPipeline() {},
          setBindGroup() {},
          dispatchWorkgroups() { enc.ops.push({ kind: 'palrle' }); },
          end() {},
        }),
        beginRenderPass: () => ({ enc, end() {} }),
        copyBufferToBuffer: (src: FakeBuffer, _so: number, dst: FakeBuffer) => {
          enc.used.push(src, dst);
          enc.ops.push({ kind: 'errors-copy' });
        },
        finish: () => ({ ops: enc.ops, used: enc.used }),
      };
      return enc;
    },
  };

  /** Let the GPU catch up: every pending map completes. */
  async drainMaps(): Promise<void> {
    for (const b of this.buffers) if (b.mapState === 'pending') b.resolveMap!();
    await new Promise((r) => setTimeout(r, 0));
  }
}

function makeRenderer(gpu: FakeGpu): WebGpuRenderer {
  const device = gpu.device as unknown as GPUDevice;
  const palrle = new PalRlePipeline(device);
  palrle.resize(4, {} as GPUTextureView);
  let pendingSolid: Array<{ tileX: number; tileY: number }> = [];
  const r = Object.create(WebGpuRenderer.prototype) as any;
  Object.assign(r, {
    rawQueue: [], solidQueue: [], palRleQueue: [], cdf53Queue: [], h264Queue: [], h264Seqs: [],
    videoFramesToClose: [], framebufferDirty: false, submitCount: 0, errorsReadbackInFlight: false,
    h264DropWarned: false, h264Pipeline: null,
    paintTick: 0, tileCols: 0, tileCodec: new Uint8Array(0), tileSeq: new Uint32Array(0), tileTick: new Uint32Array(0),
    gpu: {
      device,
      context: { getCurrentTexture: () => ({ createView: () => ({}) }) },
    },
    framebuffer: {
      texture: {}, view: {}, width: 64, height: 64,
      encodePresentBlit: (enc: { ops: Op[] }) => { enc.ops.push({ kind: 'present' }); },
    },
    solidPipeline: {
      packAndUpload: (t: typeof pendingSolid) => { pendingSolid = [...t]; return t.length; },
      draw: (pass: { enc: { ops: Op[] } }) => {
        for (const t of pendingSolid) pass.enc.ops.push({ kind: 'solid', tile: [t.tileX, t.tileY] });
      },
    },
    palrlePipeline: palrle,
    cdf53Pipeline: { uploadBatch: () => 0, encodeIntegrate() {}, encodeInverse() {} },
  });
  return r as WebGpuRenderer;
}

const pal = (tileX: number, frameSeq: number) => ({
  tileX, tileY: 0, frameSeq, paletteId: 0, count: 1, indices: new Uint8Array(512),
});
const solid = (tileX: number, frameSeq: number) => ({
  tileX, tileY: 0, frameSeq, bgra: new Uint8Array([0, 0, 255, 255]),
});

beforeAll(() => {
  (globalThis as Record<string, unknown>).GPUBufferUsage = {
    STORAGE: 1, COPY_DST: 2, COPY_SRC: 4, INDIRECT: 8, UNIFORM: 16, MAP_READ: 32, VERTEX: 64,
  };
  (globalThis as Record<string, unknown>).GPUMapMode = { READ: 1, WRITE: 2 };
});

let gpu: FakeGpu;
let r: WebGpuRenderer;
beforeEach(() => {
  (globalThis as any).window = globalThis;
  gpu = new FakeGpu();
  r = makeRenderer(gpu);
});
const present = () => r.encodeAndPresentFrame(() => {});

describe('PalRLE errors readback', () => {
  it('does not get the next tick rejected while its map is still pending', async () => {
    r.pushPalRle(pal(1, 1));
    present();
    expect(gpu.rejected).toEqual([]);

    // The GPU is slower than rAF: the map started above has not completed
    // when the next tick, with PalRLE work of its own, is painted.
    r.pushSolid(solid(0, 2));
    r.pushPalRle(pal(1, 2));
    present();

    expect(gpu.rejected).toEqual([]);
    expect(gpu.ran.some((o) => o.kind === 'solid' && o.tile![0] === 0)).toBe(true);
    expect(gpu.ran.filter((o) => o.kind === 'present')).toHaveLength(2);

    await gpu.drainMaps();
  });

  it('does not get the next tick rejected while its map is done but not yet unmapped', async () => {
    r.pushPalRle(pal(1, 1));
    present();
    // The map completed, but the readback's continuation (which unmaps) has
    // not run before the next tick is painted.
    for (const b of gpu.buffers) if (b.mapState === 'pending') b.resolveMap!();
    r.pushSolid(solid(0, 2));
    r.pushPalRle(pal(1, 2));
    present();

    expect(gpu.rejected).toEqual([]);
    expect(gpu.ran.some((o) => o.kind === 'solid' && o.tile![0] === 0)).toBe(true);
    await gpu.drainMaps();
  });

  it('still reads errors back once the previous map has completed', async () => {
    r.pushPalRle(pal(1, 1));
    present();
    await gpu.drainMaps();

    r.pushPalRle(pal(1, 2));
    present();
    expect(gpu.rejected).toEqual([]);
    expect(gpu.ran.filter((o) => o.kind === 'errors-copy')).toHaveLength(2);
    await gpu.drainMaps();
  });
});
