export const TILE_NACK_ENVELOPE = 0x05;
export const NACK_BATCH_FLUSH_MS = 5;
export const NACK_BATCH_MAX = 64;

export interface EmitKey { frameSeq: number; tileX: number; tileY: number; passIdx: number; }
export interface NackEntry { key: EmitKey; fragIdx: number; }

type Sender = (buf: Uint8Array) => void;

export class NackBatcher {
  private entries: NackEntry[] = [];
  private flushTimer: ReturnType<typeof setTimeout> | null = null;
  constructor(private send: Sender) {}

  add(key: EmitKey, fragIdx: number): void {
    this.entries.push({ key, fragIdx });
    if (this.entries.length >= NACK_BATCH_MAX) {
      this.flushNow();
    } else if (this.flushTimer === null) {
      this.flushTimer = setTimeout(() => this.flushNow(), NACK_BATCH_FLUSH_MS);
    }
  }

  flushNow(): void {
    if (this.flushTimer !== null) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
    if (this.entries.length === 0) return;
    const n = Math.min(this.entries.length, NACK_BATCH_MAX);
    const buf = new Uint8Array(2 + n * 8);
    buf[0] = TILE_NACK_ENVELOPE;
    buf[1] = n;
    const v = new DataView(buf.buffer);
    for (let i = 0; i < n; i++) {
      const e = this.entries[i];
      const off = 2 + i * 8;
      v.setUint32(off, e.key.frameSeq, true);  // LE
      buf[off + 4] = e.key.tileX;
      buf[off + 5] = e.key.tileY;
      buf[off + 6] = e.key.passIdx;
      buf[off + 7] = e.fragIdx;
    }
    this.send(buf);
    this.entries.splice(0, n);
    if (this.entries.length > 0) {
      this.flushTimer = setTimeout(() => this.flushNow(), NACK_BATCH_FLUSH_MS);
    }
  }
}

export function parseNackEnvelopeForTest(buf: Uint8Array): NackEntry[] {
  if (buf[0] !== TILE_NACK_ENVELOPE) throw new Error('not a NACK envelope');
  const n = buf[1];
  const v = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const out: NackEntry[] = [];
  for (let i = 0; i < n; i++) {
    const off = 2 + i * 8;
    out.push({
      key: {
        frameSeq: v.getUint32(off, true),
        tileX: buf[off + 4],
        tileY: buf[off + 5],
        passIdx: buf[off + 6],
      },
      fragIdx: buf[off + 7],
    });
  }
  return out;
}
