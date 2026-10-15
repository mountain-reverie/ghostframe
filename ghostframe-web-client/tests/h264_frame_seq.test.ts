// The frame_seq of an H.264 frame must come back with the decoded frame, even
// when an earlier chunk was rejected. A side FIFO of seqs pushed before
// decode() went out of step on exactly that: VideoDecoder.decode() throws
// synchronously for a delta chunk that arrives before any key chunk, so the
// seq was queued and no frame ever came back for it.
import { beforeEach, describe, expect, it } from 'vitest';
import { FullFrameDecoder } from '../src/decoder';

class FakeChunk {
  type: string; timestamp: number; data: Uint8Array;
  constructor(i: { type: string; timestamp: number; data: Uint8Array }) {
    this.type = i.type; this.timestamp = i.timestamp; this.data = i.data;
  }
}

let outputs: Array<(f: { timestamp: number; close(): void }) => void>;
let errors: Array<(e: DOMException) => void>;
let decoders: any[];

beforeEach(() => {
  outputs = [];
  errors = [];
  decoders = [];
  (globalThis as any).EncodedVideoChunk = FakeChunk;
  (globalThis as any).VideoDecoder = class {
    state = 'configured';
    needKey = true;
    out: (f: any) => void;
    constructor(init: { output: (f: any) => void; error: (e: DOMException) => void }) {
      this.out = init.output; outputs.push(init.output); errors.push(init.error); decoders.push(this);
    }
    configure() {}
    decode(c: FakeChunk) {
      if (this.state === 'closed') throw new DOMException('closed', 'InvalidStateError');
      if (this.needKey && c.type !== 'key') throw new DOMException('key frame required', 'DataError');
      this.needKey = false;
      this.out({ timestamp: c.timestamp, close() {} });
    }
    close() { this.state = 'closed'; }
  };
});

describe('FullFrameDecoder frame_seq', () => {
  it('tags each output with the seq of its own chunk after a rejected chunk', () => {
    const got: Array<[number, number]> = [];
    const d = new FullFrameDecoder((f, seq) => got.push([f.timestamp, seq]), 64, 64);
    expect(() => d.decode(new Uint8Array(1), false, 10)).toThrow();
    d.decode(new Uint8Array(1), true, 11);
    d.decode(new Uint8Array(1), false, 12);
    expect(got).toEqual([[11, 11], [12, 12]]);
  });

  it('rebuilds after an async decode error, on the next key chunk', () => {
    const got: number[] = [];
    const d = new FullFrameDecoder((_f, seq) => got.push(seq), 64, 64);
    d.decode(new Uint8Array(1), true, 1);
    // WebCodecs closes the decoder before invoking the error callback.
    decoders[0].state = 'closed';
    errors[0](new DOMException('boom', 'EncodingError'));
    // Deltas have no reference now: dropped, not thrown, no new decoder yet.
    expect(() => d.decode(new Uint8Array(1), false, 2)).not.toThrow();
    expect(decoders).toHaveLength(1);
    d.decode(new Uint8Array(1), true, 3);
    expect(decoders).toHaveLength(2);
    d.decode(new Uint8Array(1), false, 4);
    expect(got).toEqual([1, 3, 4]);
  });
  it('does not rebuild after close()', () => {
    const d = new FullFrameDecoder(() => {}, 64, 64);
    d.close();
    d.decode(new Uint8Array(1), true, 1);
    expect(decoders).toHaveLength(1);
  });
  it('a late error from a replaced decoder does not poison the new one', () => {
    const got: number[] = [];
    const d = new FullFrameDecoder((_f, seq) => got.push(seq), 64, 64);
    decoders[0].state = 'closed';
    errors[0](new DOMException('boom', 'EncodingError'));
    d.decode(new Uint8Array(1), true, 5);
    errors[0](new DOMException('late', 'EncodingError'));
    d.decode(new Uint8Array(1), false, 6);
    expect(got).toEqual([5, 6]);
  });
  it('after a rejected delta-before-key, later deltas are dropped without throwing', () => {
    const got: number[] = [];
    const d = new FullFrameDecoder((_f, seq) => got.push(seq), 64, 64);
    expect(() => d.decode(new Uint8Array(1), false, 1)).toThrow();
    expect(() => d.decode(new Uint8Array(1), false, 2)).not.toThrow();
    d.decode(new Uint8Array(1), true, 3);
    d.decode(new Uint8Array(1), false, 4);
    expect(got).toEqual([3, 4]);
  });
});
