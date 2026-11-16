// SPDX-License-Identifier: MIT OR Apache-2.0
// The browser's H.264 decoder wrapper. The core half of the H.264 feedback
// (frame acks, decode-error resets) moved to Rust: see
// `moved_from_vitest` in ghostframe-client-wasm/src/core.rs.
import { beforeEach, describe, expect, it } from 'vitest';
import { FullFrameDecoder, codecStringFromSps, H264_DECODED, H264_NOT_OUTPUT } from '../src/decoder';

class FakeChunk {
  type: string; timestamp: number; data: Uint8Array;
  constructor(i: { type: string; timestamp: number; data: Uint8Array }) {
    this.type = i.type; this.timestamp = i.timestamp; this.data = i.data;
  }
}

let decoders: any[];
let configured: string[];
let refuseCodec: string | null;
let errors: Array<(e: DOMException) => void>;

beforeEach(() => {
  decoders = [];
  configured = [];
  refuseCodec = null;
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
    configure(c: { codec: string }) {
      if (refuseCodec !== null && c.codec === refuseCodec) throw new TypeError('unsupported');
      configured.push(c.codec);
    }
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

describe('the decoder is configured from the stream', () => {
  // Annex-B SPS: profile_idc, constraint flags, level_idc.
  const sps = (p: number, c: number, l: number) =>
    Uint8Array.from([0, 0, 0, 1, 0x67, p, c, l, 0xe9, 0x00]);
  const idr = Uint8Array.from([0, 0, 1, 0x65, 0x88, 0x84]);

  it('reads profile, constraints and level from the SPS', () => {
    expect(codecStringFromSps(sps(0x64, 0x00, 0x2a))).toBe('avc1.64002a');
    expect(codecStringFromSps(sps(0x4d, 0x40, 0x1e))).toBe('avc1.4d401e');
    expect(codecStringFromSps(Uint8Array.from([...sps(0x64, 0, 0x2a), ...idr]))).toBe('avc1.64002a');
  });

  it('finds nothing in a delta frame or a truncated SPS', () => {
    expect(codecStringFromSps(Uint8Array.from([0, 0, 1, 0x41, 1, 2, 3, 4]))).toBeNull();
    expect(codecStringFromSps(Uint8Array.from([0, 0, 0, 1, 0x67, 0x64]))).toBeNull();
    expect(codecStringFromSps(new Uint8Array(0))).toBeNull();
  });

  it('starts as High 4.2 and follows the stream when the IDR says otherwise', () => {
    const { d } = harness();
    expect(configured).toEqual(['avc1.64002a']);
    // A High stream at the default: no reconfigure.
    d.decode(Uint8Array.from([...sps(0x64, 0, 0x2a), ...idr]), true, 1);
    expect(configured).toEqual(['avc1.64002a']);
    // A Main stream (the ffmpeg encoder): the decoder follows it once.
    d.decode(Uint8Array.from([...sps(0x4d, 0x40, 0x1f), ...idr]), true, 2);
    d.decode(Uint8Array.from([...sps(0x4d, 0x40, 0x1f), ...idr]), true, 3);
    expect(configured).toEqual(['avc1.64002a', 'avc1.4d401f']);
    // A delta frame never changes it.
    d.decode(Uint8Array.from([0, 0, 1, 0x41, 1, 2]), false, 4);
    expect(configured).toEqual(['avc1.64002a', 'avc1.4d401f']);
  });

  it('keeps the previous string when configure() refuses the derived one', () => {
    const { d } = harness();
    refuseCodec = 'avc1.4d401f';
    d.decode(Uint8Array.from([...sps(0x4d, 0x40, 0x1f), ...idr]), true, 1);
    // The refused string never took; the default is reconfigured and used.
    expect(configured).toEqual(['avc1.64002a', 'avc1.64002a']);
    d.decode(Uint8Array.from([...sps(0x4d, 0x40, 0x1f), ...idr]), true, 2);
    expect(configured.filter((c) => c === 'avc1.4d401f')).toEqual([]);
  });

  it('goes back once when the browser fails on the derived string before any picture', () => {
    const { d } = harness();
    d.decode(Uint8Array.from([...sps(0x4d, 0x40, 0x1f), ...idr]), true, 1);
    expect(configured).toEqual(['avc1.64002a', 'avc1.4d401f']);
    errors[0](new DOMException('unsupported', 'EncodingError'));
    d.decode(Uint8Array.from([...sps(0x4d, 0x40, 0x1f), ...idr]), true, 2);
    // The rebuild used the string before the derived one.
    expect(configured[configured.length - 1]).toBe('avc1.64002a');
  });
});
