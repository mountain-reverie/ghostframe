import { encodeDecodeError, type DecodeErrorMsg } from './feedback.js';

/**
 * Rate-limited DECODE_ERROR emitter.
 *   - per-(codec, tile_x, tile_y) cap: ≤ 1 emission per WINDOW_MS
 *   - global cap: ≤ GLOBAL_CAP emissions per WINDOW_MS
 *   - Emits immediately via the writeBytes callback; entries above either
 *     cap are dropped silently.
 */
const WINDOW_MS = 1000;
const GLOBAL_CAP = 32;

export class DecodeErrorBatcher {
  private perKey = new Map<string, number>();   // key → last-emit timestamp
  private recentEmits: number[] = [];           // sliding window of timestamps

  constructor(
    private writeBytes: (bytes: Uint8Array) => void,
    private now: () => number = () => performance.now(),
  ) {}

  report(msg: DecodeErrorMsg): void {
    const t = this.now();

    // Trim sliding global window.
    while (this.recentEmits.length > 0 && this.recentEmits[0] <= t - WINDOW_MS) {
      this.recentEmits.shift();
    }
    if (this.recentEmits.length >= GLOBAL_CAP) {
      return; // global cap exceeded — drop
    }

    const key = `${msg.codec},${msg.tileX},${msg.tileY}`;
    const last = this.perKey.get(key);
    if (last !== undefined && t - last < WINDOW_MS) {
      return; // per-key cap exceeded — drop
    }

    this.perKey.set(key, t);
    this.recentEmits.push(t);
    this.writeBytes(encodeDecodeError(msg));
  }
}
