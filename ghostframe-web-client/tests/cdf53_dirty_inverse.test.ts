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
  it('does not force a full repaint after resize', () => {
    // A forced repaint lifts from tileGen, which still names tiles that have
    // since moved to another codec, painting stale CDF53 over them.
    const { first } = setup();
    expect(first.passes).toHaveLength(0);
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
      [0 * COLS + 1, 1],
      [2 * COLS + 2, 1],
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
    expect([...mask].filter((v) => v !== 0)).toEqual([1]);
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

  it('lifts generation-0 tiles (the 4-bit wire generation wraps to 0)', () => {
    const { pipe, enc, device } = setup();
    pipe.uploadBatch([entry(2, 1, 0, 0)]);
    const e = enc();
    pipe.encodeInverse(e);
    expect(e.passes).toHaveLength(4);
    const bound = genBufferOf(e.passes[0], 0);
    const mask = [...device.queue.writes].reverse().find((w) => w.buf === bound)!.words;
    expect(mask[1 * COLS + 2]).not.toBe(0);
  });

  it('drops out-of-grid tiles instead of throwing', () => {
    const { pipe, enc } = setup();
    expect(() => pipe.uploadBatch([entry(COLS, 0, 1, 0), entry(0, ROWS, 1, 0)])).not.toThrow();
    const e = enc();
    pipe.encodeInverse(e);
    expect(e.passes).toHaveLength(0);
    // Valid tiles in the same batch still go through.
    pipe.uploadBatch([entry(COLS + 1, 0, 1, 0), entry(1, 1, 1, 0)]);
    expect(pipe.pendingInverseTiles).toBe(1);
  });

  it('clears per-tile state when the grid width changes at the same tile count', () => {
    const { pipe, enc } = setup();
    pipe.uploadBatch([entry(1, 1, 3, 0)]);
    pipe.encodeInverse(enc());
    // 4x3 -> 3x4: same MAX_TILES, different cols.
    pipe.resize(MAX_TILES, {} as GPUTextureView, ROWS);
    // A same-generation pass on the new grid must be treated as a fresh
    // generation (state cleared), i.e. it triggers the gen-bump clear writes.
    const device2 = (pipe as unknown as { device: { queue: FakeQueue } }).device;
    const before = device2.queue.writes.length;
    pipe.uploadBatch([entry(1, 1, 3, 0)]);
    const coefClears = device2.queue.writes.slice(before).filter((w) => w.words.length === 6144 / 4);
    expect(coefClears.length).toBeGreaterThanOrEqual(1);
  });

  it('passesProcessed goes up as one span write with the right contents', () => {
    const { pipe, device } = setup();
    const before = device.queue.writes.length;
    // Tiles 5, 6 and 10 (indices); span 5..10 -> 6 words, from byte offset 20.
    pipe.uploadBatch([entry(1, 1, 1, 0), entry(2, 1, 1, 0), entry(2, 2, 1, 1)]);
    const spans = device.queue.writes
      .slice(before)
      .filter((w) => w.offsetBytes === 5 * 4 && w.words.length === 6);
    expect(spans).toHaveLength(1);
    // Tile 5 and 6 saw pass 0 -> K=1; tile 10 saw only pass 1 -> K=2 (no
    // bitmap: max(passIdx+1)). Untouched tiles in the span are 0.
    expect([...spans[0].words]).toEqual([1, 1, 0, 0, 0, 2]);
    // And no per-tile 4-byte writes remain.
    expect(device.queue.writes.slice(before).filter((w) => w.words.length === 1 && w.offsetBytes % 4 === 0 && w.buf.size === MAX_TILES * 4 && w.offsetBytes !== 0)).toHaveLength(0);
  });
});
