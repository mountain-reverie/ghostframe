// The wire constants the web client needs, read from Rust.
//
// Every number here is defined once, in `ghostframe-protocol`, and reaches
// the browser through the getters in `ghostframe-client-wasm/src/constants.rs`.
// TypeScript must not retype them: a retyped literal stays green while the
// Rust value drifts, which is the failure the `GHOSTFRAME_PROTOCOL_STAMP`
// hash cannot see (it hashes Rust sources only).
// `tests/protocol_literals.test.ts` fails the build if one reappears.
//
// The module is initialised once, right after the wasm module is. Callers
// that run before that get a loud error rather than a silent `undefined`.

/** The slice of the wasm module this file reads. Both `pkg-web` (browser)
 * and `pkg-node` (vitest) satisfy it structurally. */
export interface ProtocolWasm {
  tileSize(): number;
  tileBytes(): number;
  cdf53PassCount(): number;
  datagramHeaderSize(): number;
  tileHeaderSize(): number;
  frameHeaderSize(): number;
  tileDatagramFlag(): number;
  frameDimensionsSentinelX(): number;
  frameDimensionsSentinelY(): number;
  evictionSentinelX(): number;
  evictionSentinelY(): number;
  evictionReasonDisplaced(): number;
  feedbackMsgType(): number;
  helloMsgType(): number;
  helloSize(): number;
  decodeErrorMsgType(): number;
  decodeErrorSize(): number;
  inputMsgType(): number;
  displayInfoMsgType(): number;
  displayModeMsgType(): number;
  helloCapIndicesRaw(): number;
  helloCapSupportsH264(): number;
  helloCapsReservedMask(): number;
  codecs(): unknown;
  paletteFingerprint(bgra: Uint8Array): number;
}

export interface Codecs {
  readonly Skip: number;
  readonly H264: number;
  readonly PalRle: number;
  readonly Solid: number;
  readonly Raw: number;
  readonly Cdf53: number;
}

export interface ProtocolConstants {
  readonly tileSize: number;
  readonly tileBytes: number;
  readonly cdf53PassCount: number;
  /** Bitmask with one bit per CDF53 pass: `(1 << cdf53PassCount) - 1`. */
  readonly cdf53FullPassMask: number;
  readonly datagramHeaderSize: number;
  readonly tileHeaderSize: number;
  readonly frameHeaderSize: number;
  readonly tileDatagramFlag: number;
  readonly frameDimensionsSentinelX: number;
  readonly frameDimensionsSentinelY: number;
  readonly evictionSentinelX: number;
  readonly evictionSentinelY: number;
  readonly evictionReasonDisplaced: number;
  readonly feedbackMsgType: number;
  readonly helloMsgType: number;
  readonly helloSize: number;
  readonly decodeErrorMsgType: number;
  readonly decodeErrorSize: number;
  readonly inputMsgType: number;
  readonly displayInfoMsgType: number;
  readonly displayModeMsgType: number;
  readonly helloCapIndicesRaw: number;
  readonly helloCapSupportsH264: number;
  readonly helloCapsReservedMask: number;
  readonly codec: Codecs;
  /** `ghostframe_protocol::codec::pal_rle::palette_fingerprint_bgra`. */
  paletteFingerprint(bgra: Uint8Array): number;
}

let current: ProtocolConstants | null = null;

/** Read every constant from the wasm module into one frozen object and make
 * it the process-wide value returned by [`protocol`]. Idempotent. */
export function initProtocol(w: ProtocolWasm): ProtocolConstants {
  const passCount = w.cdf53PassCount();
  current = Object.freeze({
    tileSize: w.tileSize(),
    tileBytes: w.tileBytes(),
    cdf53PassCount: passCount,
    cdf53FullPassMask: 2 ** passCount - 1,
    datagramHeaderSize: w.datagramHeaderSize(),
    tileHeaderSize: w.tileHeaderSize(),
    frameHeaderSize: w.frameHeaderSize(),
    tileDatagramFlag: w.tileDatagramFlag(),
    frameDimensionsSentinelX: w.frameDimensionsSentinelX(),
    frameDimensionsSentinelY: w.frameDimensionsSentinelY(),
    evictionSentinelX: w.evictionSentinelX(),
    evictionSentinelY: w.evictionSentinelY(),
    evictionReasonDisplaced: w.evictionReasonDisplaced(),
    feedbackMsgType: w.feedbackMsgType(),
    helloMsgType: w.helloMsgType(),
    helloSize: w.helloSize(),
    decodeErrorMsgType: w.decodeErrorMsgType(),
    decodeErrorSize: w.decodeErrorSize(),
    inputMsgType: w.inputMsgType(),
    displayInfoMsgType: w.displayInfoMsgType(),
    displayModeMsgType: w.displayModeMsgType(),
    helloCapIndicesRaw: w.helloCapIndicesRaw(),
    helloCapSupportsH264: w.helloCapSupportsH264(),
    helloCapsReservedMask: w.helloCapsReservedMask(),
    codec: Object.freeze(w.codecs() as Codecs),
    paletteFingerprint: (bgra: Uint8Array) => w.paletteFingerprint(bgra),
  });
  return current;
}

/** The initialised constants. Throws if the wasm module has not been
 * initialised yet: call it at use time, never at module scope. */
export function protocol(): ProtocolConstants {
  if (current === null) {
    throw new Error('protocol constants read before initProtocol(wasm) ran');
  }
  return current;
}
