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
let needKey: boolean;

beforeEach(() => {
  needKey = true;
  outputs = [];
  (globalThis as any).EncodedVideoChunk = FakeChunk;
  (globalThis as any).VideoDecoder = class {
    state = 'configured';
    constructor(init: { output: (f: any) => void }) { outputs.push(init.output); }
    configure() {}
    decode(c: FakeChunk) {
      if (needKey && c.type !== 'key') throw new DOMException('key frame required', 'DataError');
      needKey = false;
      outputs[0]({ timestamp: c.timestamp, close() {} });
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
});
