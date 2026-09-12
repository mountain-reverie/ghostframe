// Re-derives the five protocol-derived `window.__*` test globals the
// browser e2e suite reads (docs/specs/wasm-cutover-main-ts-map.md, Part 3)
// from the `WasmClientCore` event stream.
//
// This used to be inline bookkeeping inside `finishAssembly` in main.ts,
// fed by the TS protocol layer's own dispatch/prevalidate calls. That
// dispatch/prevalidate moment doesn't exist as a discrete step any more —
// `WasmClientCore` prevalidates internally and only ever hands back a
// `TilePayload` (success) or a `DecodeError` (failure). This module is the
// single place that re-derives the old counters from those two event
// kinds, so it can be called identically from `main.ts` (against
// `window`) and from a Node driver against `pkg-node` (against a plain
// object) — never duplicated.
//
// See `main.ts`'s `handleEvent` for the call site and
// `ghostframe-web-client/tests/cdf53_globals.test.ts` for the proof that
// each of the five actually moves.

/** The two event kinds this module cares about. Structurally matches the
 * corresponding arms of `main.ts`'s `WasmEvent` (itself mirroring
 * `ghostframe-client-wasm/src/boundary.rs`), but declared independently so
 * a Node caller driving `pkg-node`'s untyped (`any`) event objects doesn't
 * need to import main.ts's private type.
 *
 * Deliberately only these two variants, not a catch-all third member for
 * the other `WasmEvent` kinds: callers (main.ts, the Node test) always
 * cross an `any`-typed wasm event into this parameter, so the other kinds
 * still reach `recordProtocolEvent` at runtime — the `if` guard below
 * handles them — without this type needing to model their shape too. */
export type ProtocolEvent =
  | {
      kind: 'TilePayload';
      frame_seq: number;
      tile_x: number;
      tile_y: number;
      codec: number;
      payload: Uint8Array;
    }
  | { kind: 'DecodeError'; codec: number; tile_x: number; tile_y: number; code: number };

export interface Cdf53TilePushEntry {
  seq: number;
  tx: number;
  ty: number;
  codec: number;
  c0: string;
  ts: number;
}

/** The subset of globals this module reads/writes. Structurally compatible
 * with both `window as any` in the browser and a plain `{}` in Node —
 * nothing here is browser-specific. */
export interface ProtocolGlobals {
  __cdf53DispatchSeen?: number;
  __cdf53PushedToQueue?: number;
  __cdf53PrevalidateFails?: number;
  __cdf53LastFailCode?: number;
  __h5_tilePushLog?: Cdf53TilePushEntry[];
}

/** The three CDF53 `DecodeErrorCode` discriminants, shaped like the wasm
 * `errorCodes()` export. Callers must read these from that export rather
 * than hardcoding 8/9/10 — a drifting discriminant should break the count,
 * not silently stop matching. */
export interface Cdf53ErrorCodes {
  ERR_CDF53_BAD_PASS: number;
  ERR_CDF53_TRUNCATED: number;
  ERR_CDF53_RLE_LENGTH: number;
}

/** Matches the pre-cutover FIFO cap on `__h5_tilePushLog` (former
 * main.ts:836). */
const MAX_H5_LOG = 32768;

/** First up to `n` bytes of `bytes`, as lowercase hex — mirrors the
 * pre-cutover `h5c0` computation (former main.ts:820-827) so the e2e
 * diagnostic dumps that parse this field see the same shape. */
function hexPrefix(bytes: Uint8Array, n = 8): string {
  const len = bytes.byteLength;
  if (len <= 0) return `LEN=${len}`;
  const count = Math.min(len, n);
  let s = '';
  for (let i = 0; i < count; i++) s += bytes[i].toString(16).padStart(2, '0');
  return s;
}

/**
 * Updates `globals` in place for one event out of
 * `core.handleDatagram`/`core.onTimeout`. Call once per event, in arrival
 * order — the five globals are cumulative counters plus a FIFO log, so
 * order and completeness both matter.
 *
 * Events other than `TilePayload`/`DecodeError` are no-ops here (`TileReady`
 * cannot occur under Payload delivery; `PaletteUpdated`/`FrameDimensions`/
 * `NeedsH264` aren't per-tile-codec arrivals the old `__h5_tilePushLog`
 * ever recorded).
 */
export function recordProtocolEvent(
  globals: ProtocolGlobals,
  ev: ProtocolEvent,
  cdf53Codec: number,
  cdf53ErrorCodes: Cdf53ErrorCodes,
  nowMs: number,
): void {
  if (ev.kind !== 'TilePayload' && ev.kind !== 'DecodeError') return;

  const log = (globals.__h5_tilePushLog ??= []);

  if (ev.kind === 'TilePayload') {
    log.push({
      seq: ev.frame_seq,
      tx: ev.tile_x,
      ty: ev.tile_y,
      codec: ev.codec,
      c0: hexPrefix(ev.payload),
      ts: nowMs,
    });
    if (ev.codec === cdf53Codec) {
      // __cdf53DispatchSeen sums TilePayload (this, the success half) and
      // DecodeError (the failure half, below) for codec Cdf53. The old
      // counter incremented on every Cdf53 arrival *before* prevalidation
      // ran; that moment isn't a discrete event any more — prevalidation
      // now happens inside WasmClientCore, and only its outcome (success
      // or failure) crosses the boundary. See the map doc's Part 4 point 2.
      globals.__cdf53DispatchSeen = (globals.__cdf53DispatchSeen ?? 0) + 1;
      globals.__cdf53PushedToQueue = (globals.__cdf53PushedToQueue ?? 0) + 1;
    }
  } else {
    // DecodeError. No frame_seq crosses the boundary for this event
    // (boundary.rs) — sentinel it so a diagnostic dump can tell this entry
    // apart from a real tile arrival rather than reading a bogus frame_seq.
    log.push({
      seq: -1,
      tx: ev.tile_x,
      ty: ev.tile_y,
      codec: ev.codec,
      c0: `ERR:${ev.code}`,
      ts: nowMs,
    });
    if (ev.codec === cdf53Codec) {
      globals.__cdf53DispatchSeen = (globals.__cdf53DispatchSeen ?? 0) + 1;

      const isCdf53Code =
        ev.code === cdf53ErrorCodes.ERR_CDF53_BAD_PASS ||
        ev.code === cdf53ErrorCodes.ERR_CDF53_TRUNCATED ||
        ev.code === cdf53ErrorCodes.ERR_CDF53_RLE_LENGTH;
      if (isCdf53Code) {
        globals.__cdf53PrevalidateFails = (globals.__cdf53PrevalidateFails ?? 0) + 1;
        globals.__cdf53LastFailCode = ev.code;
      }
    }
  }

  if (log.length > MAX_H5_LOG) log.shift();
}
