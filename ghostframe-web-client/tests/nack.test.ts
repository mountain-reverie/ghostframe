import { describe, it, expect } from 'vitest';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';
import { NackHarness, parseNackEnvelopeNested } from './helpers/wasm.js';

const NACK_BATCH_FLUSH_MS = Number(wasm.nackBatchFlushMs());

describe('NackBatcher', () => {
  it('flushes when reaching 64 entries', () => {
    const batcher = new NackHarness();
    for (let i = 0; i < 64; i++) {
      batcher.add({ frameSeq: i, tileX: 0, tileY: 0, passIdx: 0, fragIdx: 0 });
    }
    expect(batcher.sent).toHaveLength(1);
    const parsed = parseNackEnvelopeNested(batcher.sent[0]);
    expect(parsed!.length).toBe(64);
  });

  it('flushes after timeout if entries pending', () => {
    const batcher = new NackHarness();
    batcher.add({ frameSeq: 1, tileX: 0, tileY: 0, passIdx: 0, fragIdx: 0 });
    expect(batcher.sent).toHaveLength(0);
    batcher.advanceMs(NACK_BATCH_FLUSH_MS + 1);
    expect(batcher.sent).toHaveLength(1);
    expect(parseNackEnvelopeNested(batcher.sent[0])!.length).toBe(1);
  });

  it('does not flush when empty', () => {
    const batcher = new NackHarness();
    batcher.advanceMs(NACK_BATCH_FLUSH_MS + 1);
    expect(batcher.sent).toHaveLength(0);
  });

  it('encodes 8 bytes per entry with envelope 0x05', () => {
    const batcher = new NackHarness();
    batcher.add({ frameSeq: 0x01020304, tileX: 5, tileY: 6, passIdx: 7, fragIdx: 9 });
    batcher.flush();
    expect(batcher.sent[0][0]).toBe(0x05);
    expect(batcher.sent[0][1]).toBe(1);
    expect(batcher.sent[0].slice(2, 6)).toEqual(new Uint8Array([0x04, 0x03, 0x02, 0x01]));
    expect(batcher.sent[0][6]).toBe(5);
    expect(batcher.sent[0][7]).toBe(6);
    expect(batcher.sent[0][8]).toBe(7);
    expect(batcher.sent[0][9]).toBe(9);
  });
});
