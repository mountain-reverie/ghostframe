import { describe, expect, it } from 'vitest';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';
import { AckHarness } from './helpers/wasm.js';

const ACK_BATCH_MSG_TYPE = Number(wasm.ackBatchMsgType());
const ACK_ENTRY_SIZE = Number(wasm.ackEntrySize());
const ACK_OVERLAP_COUNT = Number(wasm.ackOverlapCount());
const MAX_ACK_ENTRIES = Number(wasm.maxAckEntries());

/** Decodes an ACK envelope produced by `AckHarness`. Wraps the wasm export,
 * which returns `undefined` for a malformed envelope — never hit here. */
function parseAckEnvelopeForTest(buf: Uint8Array) {
  const entries = wasm.parseAckEnvelope(buf);
  if (entries === undefined) throw new Error('not an ACK envelope');
  return entries;
}

describe('AckBatcher wire format', () => {
  it('encodes msg type 0x04', () => {
    const b = new AckHarness();
    b.add({ frameSeq: 1, tileX: 0, tileY: 0, passIdx: 0, arrivalTimeMsLo16: 0 });
    b.flush();
    expect(b.sent).toHaveLength(1);
    expect(b.sent[0][0]).toBe(0x04);
    expect(b.sent[0][0]).toBe(ACK_BATCH_MSG_TYPE);
  });

  it('encodes frame_seq, tile_x, tile_y, pass_idx', () => {
    const b = new AckHarness();
    b.add({ frameSeq: 0x12345678, tileX: 3, tileY: 7, passIdx: 13, arrivalTimeMsLo16: 0 });
    b.flush();
    expect(b.sent).toHaveLength(1);
    const dg = b.sent[0];
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
    const batcher = new AckHarness();
    batcher.add({
      frameSeq: 0x12345678,
      tileX: 7,
      tileY: 9,
      passIdx: 3,
      arrivalTimeMsLo16: 0xABCD,
    });
    batcher.flush();
    expect(batcher.sent).toHaveLength(1);
    const sentBytes = batcher.sent[0];
    expect(sentBytes[0]).toBe(0x04);
    const entries = parseAckEnvelopeForTest(sentBytes);
    expect(entries.length).toBe(1);
    expect(entries[0].arrival_time_ms_lo16).toBe(0xABCD);
    expect(entries[0].frame_seq).toBe(0x12345678);
    expect(entries[0].pass_idx).toBe(3);
  });

  it('flushes at max entries', () => {
    const b = new AckHarness();
    for (let i = 0; i < MAX_ACK_ENTRIES; i++) {
      b.add({ frameSeq: i, tileX: 0, tileY: 0, passIdx: 0, arrivalTimeMsLo16: 0 });
    }
    // 64th add triggers immediate flush (no timer wait).
    expect(b.sent).toHaveLength(1);
    expect(b.sent[0][1]).toBe(MAX_ACK_ENTRIES);
    expect(b.sent[0].length).toBe(2 + MAX_ACK_ENTRIES * ACK_ENTRY_SIZE);
  });

  it('flushes after the 5ms interval', () => {
    const b = new AckHarness();
    b.add({ frameSeq: 100, tileX: 5, tileY: 2, passIdx: 3, arrivalTimeMsLo16: 0 });
    expect(b.sent).toHaveLength(0); // not yet
    b.advanceMs(10);
    expect(b.sent).toHaveLength(1);
    expect(b.sent[0][1]).toBe(1);
  });

  it('produces empty output when flushed with no entries', () => {
    const b = new AckHarness();
    b.flush();
    expect(b.sent).toHaveLength(0);
  });
});

describe('AckBatcher overlap', () => {
  it('appends up to ACK_OVERLAP_COUNT prior entries to each batch', () => {
    const batcher = new AckHarness();
    // First batch: 5 fresh entries.
    for (let i = 0; i < 5; i++) {
      batcher.add({ frameSeq: i, tileX: 0, tileY: 0, passIdx: 0, arrivalTimeMsLo16: 0 });
    }
    batcher.flush();
    expect(batcher.sent).toHaveLength(1);
    expect(parseAckEnvelopeForTest(batcher.sent[0]).length).toBe(5);
    // Second batch: 3 fresh entries, expect 3 + overlap from previous.
    for (let i = 5; i < 8; i++) {
      batcher.add({ frameSeq: i, tileX: 0, tileY: 0, passIdx: 0, arrivalTimeMsLo16: 0 });
    }
    batcher.flush();
    expect(batcher.sent).toHaveLength(2);
    const expectedOverlap = Math.min(5, ACK_OVERLAP_COUNT);
    expect(parseAckEnvelopeForTest(batcher.sent[1]).length).toBe(3 + expectedOverlap);
  });

  it('caps overlap at ACK_OVERLAP_COUNT after many batches', () => {
    const batcher = new AckHarness();
    // Push 20 entries across several flushes.
    for (let i = 0; i < 20; i++) {
      batcher.add({ frameSeq: i, tileX: 0, tileY: 0, passIdx: 0, arrivalTimeMsLo16: 0 });
      batcher.flush();
    }
    // Final fresh batch.
    batcher.add({ frameSeq: 100, tileX: 1, tileY: 2, passIdx: 3, arrivalTimeMsLo16: 0 });
    batcher.flush();
    const last = parseAckEnvelopeForTest(batcher.sent[batcher.sent.length - 1]);
    // 1 fresh + ACK_OVERLAP_COUNT overlap.
    expect(last.length).toBe(1 + ACK_OVERLAP_COUNT);
    // First entry is the fresh one.
    expect(last[0]).toEqual({ frame_seq: 100, tile_x: 1, tile_y: 2, pass_idx: 3, arrival_time_ms_lo16: 0 });
    // Trailing overlap is the LAST ACK_OVERLAP_COUNT fresh entries from prior batches.
    for (let k = 0; k < ACK_OVERLAP_COUNT; k++) {
      const expectedFrameSeq = 20 - ACK_OVERLAP_COUNT + k;
      expect(last[1 + k].frame_seq).toBe(expectedFrameSeq);
    }
  });

  it('emits no overlap on the very first batch', () => {
    const batcher = new AckHarness();
    batcher.add({ frameSeq: 42, tileX: 1, tileY: 2, passIdx: 3, arrivalTimeMsLo16: 0 });
    batcher.flush();
    expect(parseAckEnvelopeForTest(batcher.sent[0]).length).toBe(1);
  });
});
