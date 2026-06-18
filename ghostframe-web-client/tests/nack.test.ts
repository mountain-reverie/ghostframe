import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { NackBatcher, parseNackEnvelopeForTest, NACK_BATCH_FLUSH_MS } from '../src/nack.js';

describe('NackBatcher', () => {
  beforeEach(() => { vi.useFakeTimers(); });
  afterEach(() => { vi.useRealTimers(); });

  it('flushes when reaching 64 entries', () => {
    const sent: Uint8Array[] = [];
    const batcher = new NackBatcher(buf => sent.push(buf));
    for (let i = 0; i < 64; i++) {
      batcher.add({ frameSeq: i, tileX: 0, tileY: 0, passIdx: 0 }, 0);
    }
    expect(sent).toHaveLength(1);
    const parsed = parseNackEnvelopeForTest(sent[0]);
    expect(parsed.length).toBe(64);
  });

  it('flushes after timeout if entries pending', () => {
    const sent: Uint8Array[] = [];
    const batcher = new NackBatcher(buf => sent.push(buf));
    batcher.add({ frameSeq: 1, tileX: 0, tileY: 0, passIdx: 0 }, 0);
    expect(sent).toHaveLength(0);
    vi.advanceTimersByTime(NACK_BATCH_FLUSH_MS + 1);
    expect(sent).toHaveLength(1);
    expect(parseNackEnvelopeForTest(sent[0]).length).toBe(1);
  });

  it('does not flush when empty', () => {
    const sent: Uint8Array[] = [];
    const batcher = new NackBatcher(buf => sent.push(buf));
    vi.advanceTimersByTime(NACK_BATCH_FLUSH_MS + 1);
    expect(sent).toHaveLength(0);
  });

  it('encodes 8 bytes per entry with envelope 0x05', () => {
    const sent: Uint8Array[] = [];
    const batcher = new NackBatcher(buf => sent.push(buf));
    batcher.add({ frameSeq: 0x01020304, tileX: 5, tileY: 6, passIdx: 7 }, 9);
    batcher.flushNow();
    expect(sent[0][0]).toBe(0x05);
    expect(sent[0][1]).toBe(1);
    expect(sent[0].slice(2, 6)).toEqual(new Uint8Array([0x04, 0x03, 0x02, 0x01]));
    expect(sent[0][6]).toBe(5);
    expect(sent[0][7]).toBe(6);
    expect(sent[0][8]).toBe(7);
    expect(sent[0][9]).toBe(9);
  });
});
