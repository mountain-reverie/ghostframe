// When two server frames land in one rAF tick, the NEWER one must be what is
// on screen for a tile, whatever codec each used. The renderer used to paint
// in a fixed codec order (Raw, PalRle, Cdf53, Solid, H.264), so an older Solid
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
      writeTexture: (_d: unknown, data: Uint8Array, _l: unknown, _s: unknown) => {
        // Raw uploads carry their frame_seq in every byte; see rawTile().
        tl.ops.push({ kind: 'raw', seq: data[0], tile: [0, 0] });
      },
    },
  };
  const r = Object.create(WebGpuRenderer.prototype) as any;
  Object.assign(r, {
    rawQueue: [], solidQueue: [], palRleQueue: [], cdf53Queue: [], h264Queue: [], h264Seqs: [],
    videoFramesToClose: [], framebufferDirty: false, submitCount: 0, errorsReadbackInFlight: false,
    h264DropWarned: false,
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
      encodeErrorsReadback() {},
      zeroErrors() {},
    },
    cdf53Pipeline: {
      uploadBatch: (t: typeof pendingCdf) => { pendingCdf = [...t]; return t.length; },
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
const raw = (seq: number) => ({
  tileX: 0, tileY: 0, frameSeq: seq,
  bgra: new Uint8Array(protocol().tileBytes).fill(seq),
});
const h264 = (seq: number) => ({ seq, close() {} }) as unknown as VideoFrame;

let tl: Timeline;
let r: WebGpuRenderer;
beforeEach(() => {
  (globalThis as any).window = globalThis;
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
  it('older PalRle does not cover a newer Raw', () => {
    r.pushPalRle(pal(5));
    r.pushRaw(raw(6));
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('newer Raw lands after an older CDF53 lift', () => {
    r.pushCdf53(cdf(5));
    r.pushRaw(raw(6));
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('older Raw does not cover a newer CDF53 lift', () => {
    r.pushRaw(raw(5));
    r.pushCdf53(cdf(6));
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('older Solid does not survive under a newer H.264 frame, and does not cover it', () => {
    r.pushSolid(solid(5));
    r.pushH264(h264(6), 6);
    present();
    expect(tl.screen(0, 0)).toBe(6);
  });
  it('newer Raw lands after an older H.264 frame', () => {
    r.pushH264(h264(5), 5);
    r.pushRaw(raw(6));
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

describe('planPaint', () => {
  it('serial arithmetic: 1 is newer than 0xffffffff', () => {
    expect(seqNewer(1, 0xffffffff)).toBe(true);
    expect(seqNewer(0xffffffff, 1)).toBe(false);
  });
  it('drops a pre-wrap stateless entry under a post-wrap CDF53 entry', () => {
    const p = planPaint({
      raw: [], palRle: [], h264Seqs: [],
      solid: [{ tileX: 0, tileY: 0, frameSeq: 0xffffffff }],
      cdf53: [{ tileX: 0, tileY: 0, frameSeq: 1 }],
    });
    expect(p.solid).toHaveLength(0);
  });
  it('keeps entries of other tiles', () => {
    const p = planPaint({
      raw: [], palRle: [], h264Seqs: [],
      solid: [{ tileX: 1, tileY: 0, frameSeq: 3 }],
      cdf53: [{ tileX: 0, tileY: 0, frameSeq: 9 }],
    });
    expect(p.solid).toHaveLength(1);
  });
});
