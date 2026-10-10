// DECODE_ERROR wire encoding for GPU-stage failures (the renderer reports
// these after the core's own pre-validation). Message type comes from Rust.
import { protocol } from './protocol_constants.js';

export interface DecodeErrorMsg {
  codec: number;
  tileX: number;
  tileY: number;
  errorCode: number;
}

export function encodeDecodeError(m: DecodeErrorMsg): Uint8Array {
  return new Uint8Array([
    protocol().decodeErrorMsgType,
    m.codec & 0xFF,
    m.tileX & 0xFF,
    m.tileY & 0xFF,
    m.errorCode & 0xFF,
  ]);
}
