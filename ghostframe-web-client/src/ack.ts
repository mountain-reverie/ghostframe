// Batched-ACK datagram sender. Buffers per-tile ACK entries; flushes on
// either ≥64 entries OR ≥5 ms since the last flush. One unreliable
// WebTransport datagram per flush.
//
// Wire format mirrors transport/ack.rs:
//   [0]      message_type = 0x02
//   [1]      count: u8 (1..=64)
//   [2..]    count × 4 bytes: tile_x, tile_y, (gen<<4)|pass, reserved=0

export const ACK_BATCH_MSG_TYPE = 0x02;
export const MAX_ACK_ENTRIES = 64;
export const FLUSH_INTERVAL_MS = 5;

export interface AckEntry {
  tileX: number;
  tileY: number;
  generation: number;
  pass: number;
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
    const buf = new Uint8Array(2 + count * 4);
    buf[0] = ACK_BATCH_MSG_TYPE;
    buf[1] = count;
    for (let i = 0; i < count; i++) {
      const e = this.entries[i];
      buf[2 + i * 4 + 0] = e.tileX & 0xFF;
      buf[2 + i * 4 + 1] = e.tileY & 0xFF;
      buf[2 + i * 4 + 2] = ((e.generation & 0x0F) << 4) | (e.pass & 0x0F);
      buf[2 + i * 4 + 3] = 0;
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
