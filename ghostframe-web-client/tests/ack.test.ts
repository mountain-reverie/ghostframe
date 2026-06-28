import { describe, expect, it, vi } from 'vitest';
import {
  ACK_BATCH_MSG_TYPE,
  ACK_ENTRY_SIZE,
  ACK_OVERLAP_COUNT,
  AckBatcher,
  MAX_ACK_ENTRIES,
  parseAckEnvelopeForTest,
} from '../src/ack';

describe('AckBatcher wire format', () => {
  it('encodes msg type 0x04', () => {
    const sent: Uint8Array[] = [];
    const b = new AckBatcher((dg) => sent.push(dg));
    b.add({ frameSeq: 1, tileX: 0, tileY: 0, passIdx: 0, arrivalTimeMsLo16: 0 });
    b.flush();
    expect(sent).toHaveLength(1);
    expect(sent[0][0]).toBe(0x04);
    expect(sent[0][0]).toBe(ACK_BATCH_MSG_TYPE);
  });

  it('encodes frame_seq, tile_x, tile_y, pass_idx', () => {
    const sent: Uint8Array[] = [];
    const b = new AckBatcher((dg) => sent.push(dg));
    b.add({ frameSeq: 0x12345678, tileX: 3, tileY: 7, passIdx: 13, arrivalTimeMsLo16: 0 });
    b.flush();
    expect(sent).toHaveLength(1);
    const dg = sent[0];
    expect(dg[1]).toBe(1); // count
    // frame_seq LE
    expect(dg[2]).toBe(0x78);
    expect(dg[3]).toBe(0x56);
    expect(dg[4]).toBe(0x34);
    expect(dg[5]).toBe(0x12);
    // tile_x, tile_y, pass_idx
    expect(dg[6]).toBe(3);
    expect(dg[7]).toBe(7);
    expect(dg[8]).toBe(13);
    // arrivalTimeMsLo16 = 0 (placeholder) at bytes [9..11]
    expect(dg[9]).toBe(0);
    expect(dg[10]).toBe(0);
  });

  it('round-trips arrivalTimeMsLo16 in the v0x04 envelope', () => {
    let sentBytes: Uint8Array | null = null;
    const batcher = new AckBatcher((dg) => { sentBytes = dg; });
    batcher.add({
      frameSeq: 0x12345678,
      tileX: 7,
      tileY: 9,
      passIdx: 3,
      arrivalTimeMsLo16: 0xABCD,
    });
    batcher.flush();
    expect(sentBytes).not.toBeNull();
    expect(sentBytes![0]).toBe(0x04);
    const entries = parseAckEnvelopeForTest(sentBytes!);
    expect(entries.length).toBe(1);
    expect(entries[0].arrivalTimeMsLo16).toBe(0xABCD);
    expect(entries[0].frameSeq).toBe(0x12345678);
    expect(entries[0].passIdx).toBe(3);
  });

  it('flushes at max entries', () => {
    const sent: Uint8Array[] = [];
    const b = new AckBatcher((dg) => sent.push(dg));
    for (let i = 0; i < MAX_ACK_ENTRIES; i++) {
      b.add({ frameSeq: i, tileX: 0, tileY: 0, passIdx: 0, arrivalTimeMsLo16: 0 });
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
      b.add({ frameSeq: 100, tileX: 5, tileY: 2, passIdx: 3, arrivalTimeMsLo16: 0 });
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

describe('AckBatcher overlap', () => {
  it('appends up to ACK_OVERLAP_COUNT prior entries to each batch', () => {
    const sent: Uint8Array[] = [];
    const batcher = new AckBatcher((buf) => sent.push(buf));
    // First batch: 5 fresh entries.
    for (let i = 0; i < 5; i++) {
      batcher.add({ frameSeq: i, tileX: 0, tileY: 0, passIdx: 0, arrivalTimeMsLo16: 0 });
    }
    batcher.flush();
    expect(sent).toHaveLength(1);
    expect(parseAckEnvelopeForTest(sent[0]).length).toBe(5);
    // Second batch: 3 fresh entries, expect 3 + overlap from previous.
    for (let i = 5; i < 8; i++) {
      batcher.add({ frameSeq: i, tileX: 0, tileY: 0, passIdx: 0, arrivalTimeMsLo16: 0 });
    }
    batcher.flush();
    expect(sent).toHaveLength(2);
    const expectedOverlap = Math.min(5, ACK_OVERLAP_COUNT);
    expect(parseAckEnvelopeForTest(sent[1]).length).toBe(3 + expectedOverlap);
  });

  it('caps overlap at ACK_OVERLAP_COUNT after many batches', () => {
    const sent: Uint8Array[] = [];
    const batcher = new AckBatcher((buf) => sent.push(buf));
    // Push 20 entries across several flushes.
    for (let i = 0; i < 20; i++) {
      batcher.add({ frameSeq: i, tileX: 0, tileY: 0, passIdx: 0, arrivalTimeMsLo16: 0 });
      batcher.flush();
    }
    // Final fresh batch.
    batcher.add({ frameSeq: 100, tileX: 1, tileY: 2, passIdx: 3, arrivalTimeMsLo16: 0 });
    batcher.flush();
    const last = parseAckEnvelopeForTest(sent[sent.length - 1]);
    // 1 fresh + ACK_OVERLAP_COUNT overlap.
    expect(last.length).toBe(1 + ACK_OVERLAP_COUNT);
    // First entry is the fresh one.
    expect(last[0]).toEqual({ frameSeq: 100, tileX: 1, tileY: 2, passIdx: 3, arrivalTimeMsLo16: 0 });
    // Trailing overlap is the LAST ACK_OVERLAP_COUNT fresh entries from prior batches.
    for (let k = 0; k < ACK_OVERLAP_COUNT; k++) {
      const expectedFrameSeq = 20 - ACK_OVERLAP_COUNT + k;
      expect(last[1 + k].frameSeq).toBe(expectedFrameSeq);
    }
  });

  it('emits no overlap on the very first batch', () => {
    const sent: Uint8Array[] = [];
    const batcher = new AckBatcher((buf) => sent.push(buf));
    batcher.add({ frameSeq: 42, tileX: 1, tileY: 2, passIdx: 3, arrivalTimeMsLo16: 0 });
    batcher.flush();
    expect(parseAckEnvelopeForTest(sent[0]).length).toBe(1);
  });
});
