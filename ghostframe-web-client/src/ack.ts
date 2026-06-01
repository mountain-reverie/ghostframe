// Batched-ACK datagram sender. Buffers per-tile-pass ACK entries; flushes
// on either ≥64 entries OR ≥5 ms since the last flush. One unreliable
// WebTransport datagram per flush.
//
// Wire format mirrors transport/ack.rs (M3.3d rev2):
//   [0]      message_type = 0x03
//   [1]      count: u8 (1..=64)
//   [2..]    count × 7 bytes:
//              [0..4]  frame_seq: u32 little-endian
//              [4]     tile_x: u8
//              [5]     tile_y: u8
//              [6]     pass_idx: u8
//
// Key change from M3.3d initial: use (frame_seq, tile_x, tile_y, pass_idx)
// instead of (frame_seq, frag_idx) to avoid collisions when multiple
// single-fragment work items per frame all share frag_idx=0.

export const ACK_BATCH_MSG_TYPE = 0x03;
export const MAX_ACK_ENTRIES = 64;
export const ACK_ENTRY_SIZE = 7;
export const FLUSH_INTERVAL_MS = 5;

export interface AckEntry {
  frameSeq: number;
  tileX: number;
  tileY: number;
  passIdx: number;
}

export class AckBatcher {
  private entries: AckEntry[] = [];
  private flushTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(private readonly send: (datagram: Uint8Array) => void) {}

  add(entry: AckEntry): void {
    this.entries.push(entry);
    if (this.entries.length >= MAX_ACK_ENTRIES) {
      this.flush();
      return;
    }
    if (this.flushTimer === null) {
      this.flushTimer = setTimeout(() => this.flush(), FLUSH_INTERVAL_MS);
    }
  }

  flush(): void {
    if (this.flushTimer !== null) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
    if (this.entries.length === 0) return;

    const count = Math.min(this.entries.length, MAX_ACK_ENTRIES);
    const buf = new Uint8Array(2 + count * ACK_ENTRY_SIZE);
    buf[0] = ACK_BATCH_MSG_TYPE;
    buf[1] = count;
    const view = new DataView(buf.buffer);
    for (let i = 0; i < count; i++) {
      const e = this.entries[i];
      const off = 2 + i * ACK_ENTRY_SIZE;
      view.setUint32(off, e.frameSeq >>> 0, /* littleEndian */ true);
      buf[off + 4] = e.tileX & 0xFF;
      buf[off + 5] = e.tileY & 0xFF;
      buf[off + 6] = e.passIdx & 0xFF;
    }
    this.entries.splice(0, count);
    this.send(buf);

    // Carry-over: if more entries arrived during the flush (shouldn't in
    // single-threaded JS, but defensively), re-arm the timer.
    if (this.entries.length > 0) {
      this.flushTimer = setTimeout(() => this.flush(), FLUSH_INTERVAL_MS);
    }
  }
}
