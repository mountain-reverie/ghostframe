// The CDF53 inverse must lift only the tiles that received a pass in the
// batch being presented (PC-1), and must not run at all when none did.
//
// There is no GPU under vitest, so this drives Cdf53Pipeline against a
// recording fake device. That is enough to pin the two things this change
// is about -- which gen buffer the inverse binds, what it contains, and how
// many compute passes get encoded -- while the pixel-exact consequences are
// covered by the lossless-buildup and progressive-refinement e2e tests.
import { beforeAll, describe, expect, it } from 'vitest';
import { Cdf53Pipeline } from '../src/webgpu/cdf53';

type Buf = { size: number; usage: number; data: Uint32Array; label: string };

class FakeQueue {
  writes: Array<{ buf: Buf; offsetBytes: number; words: Uint32Array }> = [];
  writeBuffer(buf: Buf, offset: number, data: ArrayBufferView, dataOffset = 0, size?: number) {
    const u8 = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    const elemSize = (data as unknown as { BYTES_PER_ELEMENT: number }).BYTES_PER_ELEMENT ?? 1;
    const start = dataOffset * elemSize;
    const len = size === undefined ? u8.byteLength - start : size * elemSize;
    const bytes = u8.slice(start, start + len);
    const words = new Uint32Array(Math.ceil(bytes.byteLength / 4));
    new Uint8Array(words.buffer).set(bytes);
    this.writes.push({ buf, offsetBytes: offset, words });
  }
}

function makeDevice() {
  const queue = new FakeQueue();
  let nextId = 0;
  const device = {
    queue,
    createShaderModule: () => ({ getCompilationInfo: () => Promise.resolve({ messages: [] }) }),
    createComputePipeline: (desc: { compute: { module: unknown } }) => ({
      id: nextId++,
      getBindGroupLayout: () => ({}),
      desc,
    }),
    createBuffer: (d: { size: number; usage: number }) => {
      const b: Buf = { size: d.size, usage: d.usage, data: new Uint32Array(d.size / 4), label: `b${nextId++}` };
      return Object.assign(b, { destroy() {} });
    },
    createBindGroup: (d: { entries: Array<{ binding: number; resource: { buffer?: Buf } }> }) => ({
      entries: d.entries,
    }),
    createCommandEncoder: () => new FakeEncoder(),
  };
  return device as unknown as GPUDevice & { queue: FakeQueue };
}

class FakeEncoder {
  passes: Array<{ pipeline: unknown; bindGroup: { entries: Array<{ binding: number; resource: { buffer?: Buf } }> }; groups: number }> = [];
  beginComputePass() {
    const rec = { pipeline: null as unknown, bindGroup: null as never, groups: 0 };
    this.passes.push(rec);
    return {
      setPipeline: (p: unknown) => { rec.pipeline = p; },
      setBindGroup: (_i: number, bg: never) => { rec.bindGroup = bg; },
      dispatchWorkgroups: (n: number) => { rec.groups = n; },
      end: () => {},
    };
  }
}

beforeAll(() => {
  (globalThis as Record<string, unknown>).GPUBufferUsage = {
    STORAGE: 1, COPY_DST: 2, COPY_SRC: 4, INDIRECT: 8, UNIFORM: 16, MAP_READ: 32,
  };
});

const COLS = 4;
const ROWS = 3;
const MAX_TILES = COLS * ROWS;

function entry(tileX: number, tileY: number, gen: number, passIdx: number) {
  return { tileX, tileY, gen, passIdx, bitPlanes: new Uint8Array(384), presentPasses: null };
}

function setup() {
  const device = makeDevice();
  const pipe = new Cdf53Pipeline(device);
  pipe.resize(MAX_TILES, {} as GPUTextureView, COLS);
  const enc = () => new FakeEncoder() as unknown as GPUCommandEncoder & FakeEncoder;
  // resize() asks for one full repaint of the (possibly new) framebuffer;
  // consume it so the tests below see steady-state behaviour.
  const first = enc();
  pipe.encodeInverse(first);
  return { device, pipe, enc, first };
}

/** The gen buffer bound at the inverse shaders' gen slot (binding 2; 0 for pass 2). */
function genBufferOf(pass: FakeEncoder['passes'][number], pipelineIdx: number): Buf {
  const binding = pipelineIdx === 3 ? 0 : 2;
  return pass.bindGroup.entries.find((e) => e.binding === binding)!.resource.buffer!;
}

describe('Cdf53Pipeline dirty-tile inverse', () => {
  it('repaints everything once after resize, then goes quiet', () => {
    const { first, pipe, enc } = setup();
    expect(first.passes.map((p) => p.groups)).toEqual([MAX_TILES, MAX_TILES, MAX_TILES * 4, MAX_TILES]);
    const second = enc();
    pipe.encodeInverse(second);
    expect(second.passes).toHaveLength(0);
  });

  it('encodes nothing when no tile received a pass', () => {
    const { pipe, enc } = setup();
    pipe.uploadBatch([]);
    const e = enc();
    pipe.encodeInverse(e);
    expect(e.passes).toHaveLength(0);
    expect(pipe.pendingInverseTiles).toBe(0);
  });

  it('masks the inverse to exactly the touched tiles, deduplicated', () => {
    const { pipe, enc, device } = setup();
    // Tile (1,0) gets three passes, tile (2,2) one: the GPU dirty list would
    // hold tile (1,0) three times, the mask holds it once.
    pipe.uploadBatch([entry(1, 0, 5, 0), entry(1, 0, 5, 1), entry(1, 0, 5, 2), entry(2, 2, 7, 0)]);
    expect(pipe.pendingInverseTiles).toBe(2);

    const e = enc();
    pipe.encodeInverse(e);
    expect(e.passes).toHaveLength(4);

    // The inverse binds the mask buffer, not the persistent tileGen buffer.
    const bound = genBufferOf(e.passes[0], 0);
    const lastMaskWrite = [...device.queue.writes].reverse().find((w) => w.buf === bound)!;
    expect(lastMaskWrite).toBeDefined();
    const mask = lastMaskWrite.words;
    const nonZero = [...mask].map((v, i) => [i, v] as const).filter(([, v]) => v !== 0);
    expect(nonZero).toEqual([
      [0 * COLS + 1, 5],
      [2 * COLS + 2, 7],
    ]);
    for (const [i, p] of e.passes.entries()) expect(genBufferOf(p, i)).toBe(bound);
  });

  it('a later batch replaces the mask rather than accumulating', () => {
    const { pipe, enc, device } = setup();
    pipe.uploadBatch([entry(0, 0, 1, 0)]);
    pipe.encodeInverse(enc());
    pipe.uploadBatch([entry(3, 2, 2, 0)]);
    const e = enc();
    pipe.encodeInverse(e);
    const bound = genBufferOf(e.passes[0], 0);
    const mask = [...device.queue.writes].reverse().find((w) => w.buf === bound)!.words;
    expect([...mask].filter((v) => v !== 0)).toEqual([2]);
    expect(mask[0]).toBe(0);
  });

  it('encodeInverseAll still lifts every tile (debug hooks write state directly)', () => {
    const { pipe, enc } = setup();
    const e = enc();
    pipe.encodeInverseAll(e);
    expect(e.passes).toHaveLength(4);
    // ...and binds the persistent gen buffer, not the mask.
    const withMask = enc();
    pipe.uploadBatch([entry(0, 0, 1, 0)]);
    pipe.encodeInverse(withMask);
    expect(genBufferOf(e.passes[0], 0)).not.toBe(genBufferOf(withMask.passes[0], 0));
  });

  it('uploads passesProcessed once per batch, not once per touched tile', () => {
    const { pipe, device } = setup();
    const before = device.queue.writes.length;
    pipe.uploadBatch([entry(0, 0, 1, 0), entry(3, 2, 1, 0), entry(2, 1, 1, 0)]);
    const ppWrites = device.queue.writes.slice(before).filter((w) => w.buf.size === MAX_TILES * 4 && w.words.length >= 1);
    // Count writes whose target is the passes-processed buffer: it is the
    // buffer written with offset 0 span covering tiles 0..11 in one call.
    const spanWrites = ppWrites.filter((w) => w.offsetBytes === 0 && w.words.length === MAX_TILES);
    expect(spanWrites.length).toBeGreaterThanOrEqual(1);
    const singleTileWrites = ppWrites.filter((w) => w.words.length === 1);
    expect(singleTileWrites).toHaveLength(0);
  });
});
