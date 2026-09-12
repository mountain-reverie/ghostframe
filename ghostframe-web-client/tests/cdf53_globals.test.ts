// Proves the five protocol-derived `window.__*` globals
// (docs/specs/wasm-cutover-main-ts-map.md, Part 3) that main.ts's
// `handleEvent` re-derives via `recordProtocolEvent` actually move.
//
// "A global wired to nothing is indistinguishable from a global reporting
// zero" — none of the five are asserted on by the browser e2e suite today,
// so this is the only automated coverage that would catch a version wired
// to nothing (or, for `__cdf53DispatchSeen`, a version that silently
// under-counts by only summing successes). No browser is available in this
// environment, so this drives the exact same `recordProtocolEvent` main.ts
// calls, against `pkg-node`, rather than duplicating its logic.
import { describe, it, expect } from 'vitest';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';
import { fragmentTile } from './helpers/wasm.js';
import { recordProtocolEvent, type ProtocolGlobals, type Cdf53ErrorCodes } from '../src/cdf53_globals.js';

const CDF53_CODEC = 5; // ghostframe_protocol::protocol::Codec::Cdf53

/**
 * One all-zero CDF53 pass payload: 3 channels, each RLE-encoded as a
 * single 128-byte zero run (`rle_decode`'s `0xFF` token decodes to 128
 * zero bytes — see cdf53.rs's `rle_decode_is_public_and_decodes_zero_run`).
 * `prevalidate_cdf53` only checks structure (a length header plus exactly
 * one 128-byte decode per channel), not pixel content, so this is enough
 * to prevalidate successfully without running the real forward transform.
 */
function validCdf53PassPayload(): Uint8Array {
  const channel = new Uint8Array([0x00, 0x01, 0xff]); // len=1, rle=[0xFF] -> 128 zero bytes
  const out = new Uint8Array(channel.length * 3);
  out.set(channel, 0);
  out.set(channel, channel.length);
  out.set(channel, channel.length * 2);
  return out;
}

/** Mirrors main.ts's real construction (`new WasmClientCore(true,
 * renderer.h264Supported, true, nowUs())`) — `indices_raw_enabled` and
 * `supports_h264` don't affect the Cdf53 path; `tile_delivery_payload =
 * true` is what makes arrivals surface as `TilePayload`/`DecodeError`
 * instead of `TileReady`. */
function newPayloadCore(): InstanceType<typeof wasm.WasmClientCore> {
  return new wasm.WasmClientCore(true, true, true, 0n);
}

describe('protocol-derived test globals move (docs/specs/wasm-cutover-main-ts-map.md Part 3)', () => {
  it('each of the five globals takes more than one value across a valid + a malformed CDF53 tile', () => {
    const core = newPayloadCore();
    const codes = wasm.errorCodes() as unknown as Cdf53ErrorCodes;
    const globals: ProtocolGlobals = {};

    const snapshot = () => ({
      dispatchSeen: globals.__cdf53DispatchSeen,
      pushedToQueue: globals.__cdf53PushedToQueue,
      prevalidateFails: globals.__cdf53PrevalidateFails,
      lastFailCode: globals.__cdf53LastFailCode,
      pushLogLen: globals.__h5_tilePushLog?.length ?? 0,
    });

    const before = snapshot();
    expect(before).toEqual({
      dispatchSeen: undefined,
      pushedToQueue: undefined,
      prevalidateFails: undefined,
      lastFailCode: undefined,
      pushLogLen: 0,
    });

    // 1. A valid CDF53 tile -> TilePayload{codec: Cdf53}. Single fragment:
    // the 9-byte payload is well under the 1200-byte MTU budget.
    const validFrags = fragmentTile(1, 3, 4, CDF53_CODEC, /*gen*/ 1, /*pass*/ 0, validCdf53PassPayload(), 1200);
    expect(validFrags.length).toBe(1);
    const validEvents = core.handleDatagram(validFrags[0], 0n) as any[];
    expect(validEvents.some((e) => e.kind === 'TilePayload' && e.codec === CDF53_CODEC)).toBe(true);
    for (const ev of validEvents) recordProtocolEvent(globals, ev, CDF53_CODEC, codes, 1);

    const afterValid = snapshot();
    expect(afterValid.dispatchSeen).toBe(1);
    expect(afterValid.pushedToQueue).toBe(1);
    expect(afterValid.prevalidateFails).toBeUndefined();
    expect(afterValid.lastFailCode).toBeUndefined();
    expect(afterValid.pushLogLen).toBe(1);

    // 2. A malformed CDF53 tile: pass_idx 14 >= CDF53_PASS_COUNT (14) is
    // rejected by prevalidate_cdf53 before it even looks at the payload
    // bytes (cdf53_prevalidate.rs: `if pass_idx >= 14 { return
    // Err(Cdf53BadPass) }`) -> DecodeError{codec: Cdf53, code:
    // ERR_CDF53_BAD_PASS}.
    const badFrags = fragmentTile(2, 3, 4, CDF53_CODEC, /*gen*/ 1, /*pass*/ 14, validCdf53PassPayload(), 1200);
    const badEvents = core.handleDatagram(badFrags[0], 0n) as any[];
    expect(badEvents.some((e) => e.kind === 'DecodeError' && e.codec === CDF53_CODEC)).toBe(true);
    for (const ev of badEvents) recordProtocolEvent(globals, ev, CDF53_CODEC, codes, 2);

    const afterBad = snapshot();
    // __cdf53DispatchSeen sums TilePayload + DecodeError for codec Cdf53 —
    // this is the assertion that would fail if a "count only successes"
    // version had landed instead (it would still read 1, not 2).
    expect(afterBad.dispatchSeen).toBe(2);
    expect(afterBad.pushedToQueue).toBe(1); // unchanged: only TilePayload feeds this
    expect(afterBad.prevalidateFails).toBe(1);
    expect(afterBad.lastFailCode).toBe(codes.ERR_CDF53_BAD_PASS);
    expect(afterBad.pushLogLen).toBe(2);

    // The headline property this whole suite exists to prove: every one of
    // the five took at least two distinct values across the sequence.
    // (`toEqual`/`toBe` above already pin exact numbers; this restates the
    // same evidence as the general property the task asked for.)
    const seriesFor = (key: keyof ReturnType<typeof snapshot>) =>
      new Set([before[key], afterValid[key], afterBad[key]]).size;
    expect(seriesFor('dispatchSeen')).toBeGreaterThan(1);
    expect(seriesFor('pushedToQueue')).toBeGreaterThan(1);
    expect(seriesFor('prevalidateFails')).toBeGreaterThan(1);
    expect(seriesFor('lastFailCode')).toBeGreaterThan(1);
    expect(seriesFor('pushLogLen')).toBeGreaterThan(1);
  });
});
