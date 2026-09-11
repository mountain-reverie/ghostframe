// Adapts the wasm batchers' poll-and-injected-time API to the shape the
// pre-cutover suites were written against: a callback that appends to a
// `sent` array, and a fake-timer advance.
//
// The TS batchers took a callback and used setTimeout; the Rust ones return
// Option<Vec<u8>> and take an explicit `now_us`. Making time explicit is an
// improvement — these harnesses are where that improvement is absorbed so
// the assertions do not have to change.
import * as wasm from '../../pkg-node/ghostframe_client_wasm.js';

/** Monotonic microsecond clock. `u64` crosses as BigInt, so every time
 * value handed to wasm must be a bigint — passing a number throws. */
export class Clock {
  private us = 0n;
  now(): bigint { return this.us; }
  advanceMs(ms: number): bigint {
    this.us += BigInt(Math.round(ms * 1000));
    return this.us;
  }
}

export interface AckEntryLike {
  frameSeq: number; tileX: number; tileY: number;
  passIdx: number; arrivalTimeMsLo16: number;
}

/** Stands in for `new AckBatcher(dg => sent.push(dg))`. */
export class AckHarness {
  readonly sent: Uint8Array[] = [];
  private readonly inner = new wasm.WasmAckBatcher();
  private readonly clock = new Clock();

  add(e: AckEntryLike): void {
    this.collect(this.inner.add(
      e.frameSeq, e.tileX, e.tileY, e.passIdx, e.arrivalTimeMsLo16, this.clock.now(),
    ));
  }

  flush(): void { this.collect(this.inner.flush()); }

  /** Replaces `vi.advanceTimersByTime(ms)`. */
  advanceMs(ms: number): void { this.collect(this.inner.onTimeout(this.clock.advanceMs(ms))); }

  private collect(out: Uint8Array | undefined): void {
    if (out !== undefined) this.sent.push(out);
  }
}

export interface NackEntryLike {
  frameSeq: number; tileX: number; tileY: number;
  passIdx: number; fragIdx: number;
}

/** Stands in for `new NackBatcher(dg => sent.push(dg))`. */
export class NackHarness {
  readonly sent: Uint8Array[] = [];
  private readonly inner = new wasm.WasmNackBatcher();
  private readonly clock = new Clock();

  add(e: NackEntryLike): void {
    this.collect(this.inner.add(
      e.frameSeq, e.tileX, e.tileY, e.passIdx, e.fragIdx, this.clock.now(),
    ));
  }

  /** Replaces `vi.advanceTimersByTime(ms)`. */
  advanceMs(ms: number): void { this.collect(this.inner.onTimeout(this.clock.advanceMs(ms))); }

  private collect(out: Uint8Array | undefined): void {
    if (out !== undefined) this.sent.push(out);
  }
}

/** Decodes a NACK envelope into the nested shape the TS
 * `parseNackEnvelopeForTest` returned — `{ key: { frameSeq, tileX, tileY,
 * passIdx }, fragIdx }` — from wasm's flat, snake_case entries. */
export function parseNackEnvelopeNested(bytes: Uint8Array):
  { key: { frameSeq: number; tileX: number; tileY: number; passIdx: number }; fragIdx: number }[] | undefined {
  const flat = wasm.parseNackEnvelope(bytes);
  if (flat === undefined) return undefined;
  return flat.map((e: any) => ({
    key: { frameSeq: e.frame_seq, tileX: e.tile_x, tileY: e.tile_y, passIdx: e.pass_idx },
    fragIdx: e.frag_idx,
  }));
}
