// The browser reports what its decoder did with each H.264 access unit, and
// the wasm core turns that into the frame ack (0x0A), the reference gate's
// state and the paced IDR request (0x09) -- the same ClientCore as native.
import { beforeEach, describe, expect, it } from 'vitest';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';
import {
  FullFrameDecoder,
  H264_DECODED,
  H264_DECODE_ERROR,
  H264_NOT_OUTPUT,
} from '../src/decoder';

const NO_ACK = 0xffffffff;

function u32be(n: number): number[] {
  return [(n >>> 24) & 0xff, (n >>> 16) & 0xff, (n >>> 8) & 0xff, n & 0xff];
}

function frame(seq: number, idr: boolean, ref: number): Uint8Array {
  return Uint8Array.from([
    ...u32be(seq), 0, 0, 0, 1, ...u32be(seq), ...u32be(0), // wire_seq = seq: tracked
    idr ? 0x01 : 0x00, 0x00, ...u32be(idr ? NO_ACK : ref), 7, 7,
  ]);
}

function newCore() {
  const core = new wasm.WasmClientCore(true, true, true, 0n);
  const hello: any[] = [];
  for (let o = core.pollTransmit(0n); o !== undefined; o = core.pollTransmit(0n)) hello.push(o);
  return { core, hello };
}

function frameAcks(core: any, nowUs: bigint) {
  core.onTimeout(nowUs);
  const acks: Array<{ newest: number; decoded: bigint; refused: bigint }> = [];
  const resets: Array<[number, number]> = [];
  const order: number[] = [];
  for (let o = core.pollTransmit(nowUs); o !== undefined; o = core.pollTransmit(nowUs)) {
    if (o.kind === 'Datagram') order.push(o.bytes[0]);
    if (o.kind === 'Datagram' && o.bytes[0] === 0x0a) {
      const v = new DataView(o.bytes.buffer, o.bytes.byteOffset, o.bytes.byteLength);
      acks.push({
        newest: v.getUint32(1, true),
        decoded: v.getBigUint64(5, true),
        refused: v.getBigUint64(13, true),
      });
    } else if (o.kind === 'Stream' && o.bytes[0] === 0x09) {
      resets.push([o.bytes[1], new DataView(o.bytes.buffer, o.bytes.byteOffset).getUint32(2, true)]);
    }
  }
  return { acks, resets, order };
}

describe('the wasm core sends the v2 H.264 feedback', () => {
  it('sets HELLO 0x04 next to 0x02', () => {
    const { hello } = newCore();
    const stream = hello.find((o) => o.kind === 'Stream' && o.bytes[0] === 0x03);
    expect(stream).toBeDefined();
    expect(stream.bytes[1] & 0x04).toBe(0x04);
    expect(stream.bytes[1] & 0x02).toBe(0x02);
  });

  it('acks a decoded frame, little-endian, and a refused one in the refused mask', () => {
    const { core } = newCore();
    const ev = core.handleDatagram(frame(1, true, 0), 1_000n) as any[];
    expect(ev.find((e) => e.kind === 'NeedsH264').reset_decoder).toBe(true);
    expect(frameAcks(core, 1_000n).acks).toEqual([]); // fed, not yet decoded
    core.h264Result(1, H264_DECODED, 2_000n);
    // The ack rides the ack batch: nothing before the batch is due, then the
    // batch (0x06) and the frame ack (0x0A) in one drain.
    expect(frameAcks(core, 2_000n).order).toEqual([]);
    const first = frameAcks(core, 6_000n);
    expect(first.order).toEqual([0x06, 0x0a]);
    expect(first.acks).toEqual([{ newest: 1, decoded: 1n, refused: 0n }]);
    // Frame 2 never arrives; 3 names it: refused at the gate, no decoder involved.
    const ev3 = core.handleDatagram(frame(3, false, 2), 10_000n) as any[];
    expect(ev3.some((e) => e.kind === 'NeedsH264')).toBe(false);
    const { acks, resets } = frameAcks(core, 16_000n);
    expect(acks).toEqual([{ newest: 3, decoded: 0b100n, refused: 0b001n }]);
    expect(resets).toEqual([[4, 3]]); // GateReset, legacy stream: asked at once
  });

  it('turns a decode error into one paced decoder-failure request', () => {
    const { core } = newCore();
    core.handleDatagram(frame(1, true, 0), 0n);
    core.h264Result(1, H264_DECODED, 0n);
    core.handleDatagram(frame(2, false, 1), 1_000n);
    core.h264Result(2, H264_DECODE_ERROR, 1_000n);
    expect(frameAcks(core, 1_000n).resets).toEqual([[1, 2]]);
    core.h264Result(3, H264_DECODE_ERROR, 2_000n);
    expect(frameAcks(core, 2_000n).resets).toEqual([]);
  });

  it('counts access units that came out with no picture of their own', () => {
    const { core } = newCore();
    core.handleDatagram(frame(1, true, 0), 0n);
    core.h264Result(1, H264_NOT_OUTPUT, 0n);
    core.handleDatagram(frame(2, false, 1), 1_000n);
    core.h264Result(2, H264_NOT_OUTPUT, 1_000n);
    expect(Array.from(core.h264NotOutputStats())).toEqual([2, 2]);
    core.handleDatagram(frame(3, false, 2), 2_000n);
    core.h264Result(3, H264_DECODED, 2_000n);
    expect(Array.from(core.h264NotOutputStats())).toEqual([2, 0]);
  });

  it('ignores an outcome it does not know', () => {
    const { core } = newCore();
    core.handleDatagram(frame(1, true, 0), 0n);
    core.h264Result(1, 99, 0n);
    expect(frameAcks(core, 5_000n).acks).toEqual([]);
  });
});

class FakeChunk {
  type: string; timestamp: number; data: Uint8Array;
  constructor(i: { type: string; timestamp: number; data: Uint8Array }) {
    this.type = i.type; this.timestamp = i.timestamp; this.data = i.data;
  }
}

let decoders: any[];
let errors: Array<(e: DOMException) => void>;

beforeEach(() => {
  decoders = [];
  errors = [];
  (globalThis as any).EncodedVideoChunk = FakeChunk;
  // A decoder that answers later, like the real one: nothing comes out of
  // decode() itself; the test pushes outputs through `emit`.
  (globalThis as any).VideoDecoder = class {
    state = 'configured';
    out: (f: any) => void;
    constructor(init: { output: (f: any) => void; error: (e: DOMException) => void }) {
      this.out = init.output; errors.push(init.error); decoders.push(this);
    }
    configure() {}
    decode(_c: FakeChunk) {
      if (this.state === 'closed') throw new DOMException('closed', 'InvalidStateError');
    }
    emit(ts: number) { this.out({ timestamp: ts, close() {} }); }
    close() { this.state = 'closed'; }
  };
});

function harness() {
  const results: Array<[number, number]> = [];
  const failures: number[] = [];
  const d = new FullFrameDecoder(() => {}, 64, 64, (s) => failures.push(s), (s, o) => results.push([s, o]));
  return { d, results, failures };
}

const nal = new Uint8Array(1);

describe('FullFrameDecoder reports each chunk', () => {
  it('decoded means the output callback fired for its own timestamp', () => {
    const { d, results } = harness();
    d.decode(nal, true, 10);
    d.decode(nal, false, 11);
    expect(results).toEqual([]); // fed, nothing out yet
    decoders[0].emit(10);
    expect(results).toEqual([[10, H264_DECODED]]);
    decoders[0].emit(11);
    expect(results).toEqual([[10, H264_DECODED], [11, H264_DECODED]]);
  });

  it('an output overtakes the chunks before it: they produced nothing', () => {
    const { d, results } = harness();
    d.decode(nal, true, 10);
    d.decode(nal, false, 11);
    d.decode(nal, false, 12);
    decoders[0].emit(12);
    expect(results).toEqual([[10, H264_NOT_OUTPUT], [11, H264_NOT_OUTPUT], [12, H264_DECODED]]);
  });

  it('an async error answers the chunks in flight and reports the last as the failure', () => {
    const { d, results, failures } = harness();
    d.decode(nal, true, 1);
    d.decode(nal, false, 2);
    d.decode(nal, false, 3);
    decoders[0].state = 'closed';
    errors[0](new DOMException('boom', 'EncodingError'));
    expect(results).toEqual([[1, H264_NOT_OUTPUT], [2, H264_NOT_OUTPUT]]);
    expect(failures).toEqual([3]);
    // Deltas have nothing to reference: dropped, and said so.
    d.decode(nal, false, 4);
    expect(results.at(-1)).toEqual([4, H264_NOT_OUTPUT]);
  });

  it('a chunk the browser rejects is a failure, not a pending frame', () => {
    const { d, results, failures } = harness();
    d.decode(nal, true, 1);
    decoders[0].decode = () => { throw new DOMException('key frame required', 'DataError'); };
    expect(() => d.decode(nal, false, 2)).toThrow();
    expect(failures).toEqual([2]);
    expect(results).toEqual([[1, H264_NOT_OUTPUT]]);
  });

  it('an IDR to a healthy decoder does not rebuild it or discard the frames in flight', () => {
    const { d, results } = harness();
    d.decode(nal, true, 1);
    d.decode(nal, false, 2);
    d.decode(nal, true, 9); // a GOP IDR: just another key chunk
    expect(decoders).toHaveLength(1);
    expect(results).toEqual([]);
    decoders[0].emit(2);
    decoders[0].emit(9);
    expect(results).toEqual([
      [1, H264_NOT_OUTPUT], [2, H264_DECODED], [9, H264_DECODED],
    ]);
  });

  it('after a failure the next key chunk rebuilds the decoder', () => {
    const { d, results } = harness();
    d.decode(nal, true, 1);
    decoders[0].state = 'closed';
    errors[0](new DOMException('boom', 'EncodingError'));
    d.decode(nal, true, 9);
    expect(decoders).toHaveLength(2);
    decoders[1].emit(9);
    expect(results.at(-1)).toEqual([9, H264_DECODED]);
  });
});
