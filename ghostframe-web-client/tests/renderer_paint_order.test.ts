// When two server frames land in one rAF tick, the NEWER one must be what is
// on screen for a tile, whatever codec each used. The renderer used to paint
// in a fixed codec order (PalRle, Cdf53, Solid, H.264), so an older Solid
// or H.264 draw could sit over a newer CDF53 tile.
//
// No GPU under vitest: the renderer runs against fakes that record, on a
// queue-ordered timeline, which frame_seq each operation paints onto tile
// (0,0). "On screen" is the last write to that tile on the timeline. Encoder
// work only reaches the timeline at submit(); writeTexture is immediate, as it
// is on a real queue (it runs ahead of the next submit's commands).
import { beforeEach, describe, expect, it } from 'vitest';
import { WebGpuRenderer } from '../src/webgpu/renderer';
import { planPaint, seqNewer } from '../src/webgpu/paint_order';
import { protocol } from '../src/protocol_constants';

type Op = { kind: string; seq: number | 'all'; tile?: [number, number] };

class Timeline {
  ops: Op[] = [];
  screen(tx: number, ty: number): number | undefined {
    let v: number | undefined;
    for (const o of this.ops) {
      if (o.seq === 'all') v = (o as { all?: number }).all;
      else if (o.tile && o.tile[0] === tx && o.tile[1] === ty) v = o.seq;
    }
    return v;
  }
}

function makeRenderer(tl: Timeline) {
  let pendingCdf: Array<{ tileX: number; tileY: number; frameSeq: number }> = [];
  let pendingSolid: Array<{ tileX: number; tileY: number; bgra: Uint8Array }> = [];
  let pendingPal: Array<{ tileX: number; tileY: number; frameSeq: number }> = [];
  const mkEncoder = () => {
    const enc = {
      ops: [] as Op[],
      beginRenderPass: () => ({ enc, end() {} }),
      beginComputePass: () => ({ enc, end() {} }),
      finish: () => ({ ops: enc.ops }),
    };
    return enc;
  };
  const device = {
    createCommandEncoder: mkEncoder,
    queue: {
      submit: (bufs: Array<{ ops: Op[] }>) => { for (const b of bufs) tl.ops.push(...b.ops); },
      writeBuffer() {},
      writeTexture: (_d: unknown, data: Uint8Array, _l: unknown, _s: unknown) => {
        // Direct uploads carry their frame_seq in every byte.
        tl.ops.push({ kind: 'write', seq: data[0], tile: [0, 0] });
      },
    },
  };
  const r = Object.create(WebGpuRenderer.prototype) as any;
  Object.assign(r, {
    solidQueue: [], palRleQueue: [], cdf53Queue: [], h264Queue: [], h264Seqs: [],
    videoFramesToClose: [], framebufferDirty: false, submitCount: 0, errorsReadbackInFlight: false,
    h264DropWarned: false,
    paintTick: 0, tileCols: 4, tileCodec: new Uint8Array(16), tileSeq: new Uint32Array(16), tileTick: new Uint32Array(16),
    h264Floor: undefined, maxTileSeq: undefined,
    gpu: { device, context: { getCurrentTexture: () => ({ createView: () => ({}) }) } },
    framebuffer: { texture: {}, view: {}, width: 64, height: 64, encodePresentBlit() {} },
    solidPipeline: {
      packAndUpload: (t: typeof pendingSolid) => { pendingSolid = [...t]; return t.length; },
      draw: (pass: { enc: { ops: Op[] } }) => {
        for (const t of pendingSolid) pass.enc.ops.push({ kind: 'solid', seq: t.bgra[0], tile: [t.tileX, t.tileY] });
      },
    },
    palrlePipeline: {
      uploadBatch: (t: typeof pendingPal) => { pendingPal = [...t]; return t.length; },
      encodeDispatch: (enc: { ops: Op[] }) => {
        for (const t of pendingPal) enc.ops.push({ kind: 'palrle', seq: t.frameSeq, tile: [t.tileX, t.tileY] });
      },
      paletteAtlas: {},
      encodeErrorsReadback() {},
      zeroErrors() {},
      // A drained PalRle batch starts an errors readback; it never resolves here.
      errorsStaging: {
        mapAsync: () => new Promise<void>(() => {}),
        getMappedRange: () => new ArrayBuffer(0),
        unmap() {},
      },
    },
    cdf53Pipeline: {
      uploadBatch: (t: typeof pendingCdf) => { pendingCdf = [...t]; return t.length; },
      clearAllState() {},
      encodeIntegrate() {},
      encodeInverse: (enc: { ops: Op[] }) => {
        for (const t of pendingCdf) enc.ops.push({ kind: 'cdf53', seq: t.frameSeq, tile: [t.tileX, t.tileY] });
      },
    },
    h264Pipeline: {
      drawFullFrame: (pass: { enc: { ops: Op[] } }, frame: { seq: number }) => {
        pass.enc.ops.push({ kind: 'h264', seq: 'all', all: frame.seq } as Op);
      },
    },
  });
  return r as WebGpuRenderer;
}

const solid = (seq: number) => ({ tileX: 0, tileY: 0, frameSeq: seq, bgra: new Uint8Array([seq, seq, seq, 255]) });
const cdf = (seq: number) => ({ tileX: 0, tileY: 0, frameSeq: seq, gen: 0, passIdx: 0, bitPlanes: new Uint8Array(384) });
const pal = (seq: number) => ({ tileX: 0, tileY: 0, frameSeq: seq, paletteId: 0, count: 1, indices: new Uint8Array(512) });
const h264 = (seq: number) => ({ seq, close() {} }) as unknown as VideoFrame;

let tl: Timeline;
let r: WebGpuRenderer;
beforeEach(() => {
  (globalThis as any).window = globalThis;
  (globalThis as any).GPUMapMode = { READ: 1 };
  tl = new Timeline();
  r = makeRenderer(tl);
});
const present = () => r.encodeAndPresentFrame(() => {});

describe('newest frame_seq wins a tile within one tick', () => {
  it('older Solid does not cover a newer CDF53 lift', () => {
    r.pushSolid(solid(5));
    r.pushCdf53(cdf(6));
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('newer Solid still covers an older CDF53 lift', () => {
    r.pushCdf53(cdf(5));
    r.pushSolid(solid(6));
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('arrival order does not matter (CDF53 queued first)', () => {
    r.pushCdf53(cdf(6));
    r.pushSolid(solid(5));
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('older H.264 frame does not cover a newer CDF53 lift', () => {
    r.pushH264(h264(5), 5);
    r.pushCdf53(cdf(6));
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('newer H.264 frame covers an older CDF53 lift', () => {
    r.pushCdf53(cdf(5));
    r.pushH264(h264(6), 6);
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('older H.264 frame does not cover a newer Solid', () => {
    r.pushH264(h264(5), 5);
    r.pushSolid(solid(6));
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('older Solid does not cover a newer PalRle', () => {
    // The renderer draws PalRle before Solid, so only planPaint's stateless
    // rule can stop the older Solid from landing on top.
    r.pushSolid(solid(5));
    r.pushPalRle(pal(6));
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('older PalRle does not cover a newer Solid', () => {
    r.pushPalRle(pal(5));
    r.pushSolid(solid(6));
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('older Solid does not survive under a newer H.264 frame, and does not cover it', () => {
    r.pushSolid(solid(5));
    r.pushH264(h264(6), 6);
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('H.264 frames that cannot be drawn do not suppress tiles', () => {
    (r as any).h264Pipeline = null;
    r.pushSolid(solid(5));
    r.pushH264(h264(6), 6);
    present();
    expect(tl.screen(0, 0)).toBe(5);
  });
  it('keeps h264Queue and h264Seqs aligned when the CDF53 upload throws', () => {
    (r as any).cdf53Pipeline.uploadBatch = () => { throw new Error('capacity'); };
    r.pushH264(h264(5), 5);
    r.pushCdf53(cdf(6));
    expect(() => present()).toThrow();
    expect(r.h264Seqs).toEqual([5]);
    expect(r.h264Queue).toHaveLength(1);
    expect(r.cdf53Queue).toHaveLength(0);
  });
  it('never drops a CDF53 pass, even one a newer frame supersedes', () => {
    let uploaded = 0;
    (r as any).cdf53Pipeline.uploadBatch = (t: unknown[]) => { uploaded = t.length; return t.length; };
    r.pushCdf53(cdf(5));
    r.pushSolid(solid(6));
    r.pushCdf53(cdf(4));
    present();
    expect(uploaded).toBe(2);
  });
});

// Ordering across ticks. The renderer remembers what it painted; an entry, or
// an H.264 frame, older than that must not be painted over it. A tick here is
// one encodeAndPresentFrame, as a requestAnimationFrame is in the browser.
describe('newest frame_seq wins a tile across ticks', () => {
  const uploads = () => {
    const seen: number[] = [];
    (r as any).cdf53Pipeline.uploadBatch = (t: Array<{ frameSeq: number }>) => {
      for (const e of t) seen.push(e.frameSeq);
      (r as any).__pending = [...t];
      return t.length;
    };
    (r as any).cdf53Pipeline.encodeInverse = (enc: { ops: Op[] }) => {
      for (const t of (r as any).__pending ?? []) enc.ops.push({ kind: 'cdf53', seq: t.frameSeq, tile: [0, 0] });
    };
    return seen;
  };

  it('an older Solid in a later tick does not cover a newer Solid', () => {
    r.pushSolid(solid(6));
    present();
    r.pushSolid(solid(5));
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('an older PalRle in a later tick does not cover a newer Solid', () => {
    r.pushSolid(solid(6));
    present();
    r.pushPalRle(pal(5));
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('an old-frame CDF53 retransmit does not repaint over a newer Solid', () => {
    const up = uploads();
    r.pushSolid(solid(6));
    present();
    r.pushCdf53(cdf(5));
    present();
    expect(up).toEqual([]);
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('an older Solid does not cover a CDF53 tile painted in an earlier tick', () => {
    uploads();
    r.pushCdf53(cdf(6));
    present();
    r.pushSolid(solid(5));
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('equal frame_seq is painted: refinement passes share their generation\'s seq', () => {
    const up = uploads();
    r.pushCdf53(cdf(6));
    present();
    r.pushCdf53(cdf(6));
    present();
    r.pushSolid(solid(6));
    present();
    expect(up).toEqual([6, 6]);
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('a newer entry is painted, and other tiles are not affected', () => {
    r.pushSolid(solid(6));
    present();
    r.pushSolid(solid(7));
    r.pushSolid({ ...solid(5), tileX: 1 });
    present();
    expect(tl.screen(0, 0)).toBe(7);
    expect(tl.screen(1, 0)).toBe(5);
  });
  it('a late H.264 frame does not repaint over tiles painted after it was sent', () => {
    r.pushSolid(solid(6));
    present();
    r.pushH264(h264(5), 5);
    present();
    expect(tl.screen(0, 0)).toBe(6);
    expect(tl.ops.some((o) => o.kind === 'h264')).toBe(false);
  });
  it('the late H.264 frame is still closed, not leaked', () => {
    let closed = 0;
    r.pushSolid(solid(6));
    present();
    r.pushH264({ seq: 5, close() { closed++; } } as unknown as VideoFrame, 5);
    present();
    present();
    expect(closed).toBe(1);
  });
  it('a newer H.264 frame is drawn, and covers older tiles', () => {
    r.pushSolid(solid(6));
    present();
    r.pushH264(h264(7), 7);
    present();
    expect(tl.screen(0, 0)).toBe(7);
  });
  it('after an H.264 frame, older tiles and passes are dropped, older frames too', () => {
    const up = uploads();
    r.pushH264(h264(8), 8);
    present();
    r.pushSolid(solid(7));
    r.pushPalRle(pal(7));
    r.pushCdf53(cdf(7));
    r.pushH264(h264(7), 7);
    present();
    expect(up).toEqual([]);
    expect(tl.screen(0, 0)).toBe(8);
    expect(tl.ops.filter((o) => o.kind === 'h264')).toHaveLength(1);
    // A tile from the next frame lands on top of the frame.
    r.pushSolid(solid(9));
    present();
    expect(tl.screen(0, 0)).toBe(9);
  });
  it('an older frame queued with a newer one in the same tick is not drawn over it', () => {
    r.pushH264(h264(6), 6);
    r.pushH264(h264(5), 5);
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('compares frame_seq as serial numbers across a wrap', () => {
    // The wire's frame_seq is 31 bits: the wrap is 0x7fffffff -> 0.
    r.pushSolid(solid(0x7fffffff));
    present();
    r.pushSolid(solid(1));
    present();
    expect(tl.screen(0, 0)).toBe(1);
    r.pushH264(h264(0), 0);
    present();
    expect(tl.ops.some((o) => o.kind === 'h264')).toBe(false);
    r.pushSolid(solid(0x7ffffffe));
    present();
    expect(tl.screen(0, 0)).toBe(1);
  });
  it('a framebuffer resize forgets the ledger', () => {
    const r2 = r as any;
    r2.canvas = { width: 64, height: 64 };
    r2.gpu.context.configure = () => {};
    r2.gpu.presentFormat = 'bgra8unorm';
    r2.framebuffer.resize = () => {};
    r2.solidPipeline.updateCanvasSize = () => {};
    r2.palrlePipeline.resize = () => {};
    r2.cdf53Pipeline.resize = () => {};
    r.pushSolid(solid(600));
    present();
    // 128x128 is 16 tiles in 4 columns: the ledger arrays keep their shape,
    // so only the explicit reset can forget.
    r.resize(128, 128);
    r.pushSolid(solid(1));
    present();
    expect(tl.screen(0, 0)).toBe(1);
  });
  it('two entries for a tile in one tick leave the newest in the ledger', () => {
    uploads();
    r.pushCdf53(cdf(6));
    r.pushCdf53(cdf(4));
    present();
    r.pushSolid(solid(5));
    present();
    // The fake draws the pass queue in order, so the tile shows 4; what
    // matters is that Solid 5 is older than the 6 in the ledger and is dropped.
    expect(tl.screen(0, 0)).not.toBe(5);
  });
  it('a session reset forgets the ledger', () => {
    r.pushSolid(solid(600));
    r.pushH264(h264(601), 601);
    present();
    r.onSessionReset();
    r.pushSolid(solid(1));
    present();
    expect(tl.screen(0, 0)).toBe(1);
  });
});

describe('planPaint', () => {
  it('serial arithmetic: 1 is newer than 0xffffffff', () => {
    expect(seqNewer(1, 0xffffffff)).toBe(true);
    expect(seqNewer(0xffffffff, 1)).toBe(false);
  });
  it('drops a pre-wrap stateless entry under a post-wrap CDF53 entry', () => {
    const p = planPaint({
      palRle: [], h264Seqs: [],
      solid: [{ tileX: 0, tileY: 0, frameSeq: 0xffffffff }],
      cdf53: [{ tileX: 0, tileY: 0, frameSeq: 1 }],
    });
    expect(p.solid).toHaveLength(0);
  });
  it('drops an older Solid under a newer PalRle on the same tile', () => {
    const plan = planPaint({
      palRle: [{ tileX: 0, tileY: 0, frameSeq: 6 }],
      h264Seqs: [],
      solid: [{ tileX: 0, tileY: 0, frameSeq: 5 }],
      cdf53: [],
    });
    expect(plan.solid).toHaveLength(0);
    expect(plan.palRle).toHaveLength(1);
  });
  it('keeps entries of other tiles', () => {
    const p = planPaint({
      palRle: [], h264Seqs: [],
      solid: [{ tileX: 1, tileY: 0, frameSeq: 3 }],
      cdf53: [{ tileX: 0, tileY: 0, frameSeq: 9 }],
    });
    expect(p.solid).toHaveLength(1);
  });
});
