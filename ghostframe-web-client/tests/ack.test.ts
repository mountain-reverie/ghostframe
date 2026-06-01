import { describe, expect, it, vi } from 'vitest';
import {
  ACK_BATCH_MSG_TYPE,
  ACK_ENTRY_SIZE,
  AckBatcher,
  MAX_ACK_ENTRIES,
} from '../src/ack';

describe('AckBatcher wire format', () => {
  it('encodes msg type 0x03', () => {
    const sent: Uint8Array[] = [];
    const b = new AckBatcher((dg) => sent.push(dg));
    b.add({ frameSeq: 1, fragIdx: 0 });
    b.flush();
    expect(sent).toHaveLength(1);
    expect(sent[0][0]).toBe(0x03);
    expect(sent[0][0]).toBe(ACK_BATCH_MSG_TYPE);
  });

  it('encodes frame_seq and frag_idx little-endian', () => {
    const sent: Uint8Array[] = [];
    const b = new AckBatcher((dg) => sent.push(dg));
    b.add({ frameSeq: 0x12345678, fragIdx: 0x9ABC });
    b.flush();
    expect(sent).toHaveLength(1);
    const dg = sent[0];
    expect(dg[1]).toBe(1); // count
    // frame_seq LE
    expect(dg[2]).toBe(0x78);
    expect(dg[3]).toBe(0x56);
    expect(dg[4]).toBe(0x34);
    expect(dg[5]).toBe(0x12);
    // frag_idx LE
    expect(dg[6]).toBe(0xBC);
    expect(dg[7]).toBe(0x9A);
  });

  it('flushes at max entries', () => {
    const sent: Uint8Array[] = [];
    const b = new AckBatcher((dg) => sent.push(dg));
    for (let i = 0; i < MAX_ACK_ENTRIES; i++) {
      b.add({ frameSeq: i, fragIdx: 0 });
    }
    // 64th add triggers immediate flush (no timer wait).
    expect(sent).toHaveLength(1);
    expect(sent[0][1]).toBe(MAX_ACK_ENTRIES);
    expect(sent[0].length).toBe(2 + MAX_ACK_ENTRIES * ACK_ENTRY_SIZE);
  });

  it('flushes after the 5ms interval', async () => {
    vi.useFakeTimers();
    try {
      const sent: Uint8Array[] = [];
      const b = new AckBatcher((dg) => sent.push(dg));
      b.add({ frameSeq: 100, fragIdx: 5 });
      expect(sent).toHaveLength(0); // not yet
      vi.advanceTimersByTime(10);
      expect(sent).toHaveLength(1);
      expect(sent[0][1]).toBe(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it('produces empty output when flushed with no entries', () => {
    const sent: Uint8Array[] = [];
    const b = new AckBatcher((dg) => sent.push(dg));
    b.flush();
    expect(sent).toHaveLength(0);
  });
});
