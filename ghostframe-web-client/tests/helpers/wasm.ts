// Adapts the wasm batchers' poll-and-injected-time API to the shape the
// pre-cutover suites were written against: a callback that appends to a
// `sent` array, and a fake-timer advance.
//
// The TS batchers took a callback and used setTimeout; the Rust ones return
// Option<Vec<u8>> and take an explicit `now_us`. Making time explicit is an
// improvement — these harnesses are where that improvement is absorbed so
// the assertions do not have to change.
import * as wasm from '../../pkg-node/ghostframe_client_wasm.js';

const TILE_DATAGRAM_FLAG = 0x80000000;

function u32be(n: number): number[] {
  return [(n >>> 24) & 0xff, (n >>> 16) & 0xff, (n >>> 8) & 0xff, n & 0xff];
}
function u16be(n: number): number[] {
  return [(n >>> 8) & 0xff, n & 0xff];
}

/**
 * Builds tile-datagram fragments matching `ghostframe_protocol::protocol`'s
 * `fragment_tile` wire layout exactly:
 *   [DatagramHeader 16B: frame_seq|frag_idx|frag_total|wire_seq|timestamp_us]
 *   [TileHeader 8B: tile_x|tile_y|(codec<<1|lz4)|(generation<<4|pass)|payload_len]
 *   [payload chunk]
 * `wire_seq` is UNSTAMPED_WIRE_SEQ (0); `timestamp_us` is 0 — neither is
 * read by the reassembly path these tests exercise.
 *
 * Shared by `tests/tile_assembly.test.ts` and `tests/cdf53_globals.test.ts`
 * so both drive the wasm core with the exact same wire bytes rather than
 * each hand-rolling its own tile-datagram construction.
 */
export function fragmentTile(
  frameSeq: number,
  tileX: number,
  tileY: number,
  codec: number,
  generation: number,
  pass: number,
  payload: Uint8Array,
  maxFragmentPayload: number,
): Uint8Array[] {
  const chunks: Uint8Array[] = [];
  if (payload.length === 0) {
    chunks.push(new Uint8Array(0));
  } else {
    for (let i = 0; i < payload.length; i += maxFragmentPayload) {
      chunks.push(payload.slice(i, i + maxFragmentPayload));
    }
  }
  const fragTotal = chunks.length;
  return chunks.map((chunk, idx) => {
    const header = new Uint8Array([
      ...u32be(frameSeq | TILE_DATAGRAM_FLAG),
      ...u16be(idx),
      ...u16be(fragTotal),
      ...u32be(0), // wire_seq (UNSTAMPED_WIRE_SEQ)
      ...u32be(0), // timestamp_us
      tileX,
      tileY,
      (codec << 1) | 0, // lz4 = false
      ((generation & 0x0f) << 4) | (pass & 0x0f),
      ...u32be(payload.length),
    ]);
    const out = new Uint8Array(header.length + chunk.length);
    out.set(header, 0);
    out.set(chunk, header.length);
    return out;
  });
}

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

  /** Replaces the TS `flushNow()` — immediate, bypassing the deadline. */
  flush(): void { this.collect(this.inner.flush()); }

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
