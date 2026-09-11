import { describe, it, expect } from 'vitest';
import {
  encodeHello,
  encodeDecodeError,
  helloMsgType,
  decodeErrorMsgType,
  errorCodes,
} from '../pkg-node/ghostframe_client_wasm.js';

const HELLO_MSG_TYPE = Number(helloMsgType());
const DECODE_ERROR_MSG_TYPE = Number(decodeErrorMsgType());
const ERR_THIN_UNCACHED_PALETTE = Number(errorCodes().ERR_THIN_UNCACHED_PALETTE);

describe('encodeHello', () => {
  it('encodes indices_raw=true to [HELLO_MSG_TYPE, 0x01]', () => {
    const bytes = encodeHello(true, false);
    expect(bytes).toEqual(new Uint8Array([HELLO_MSG_TYPE, 0x01]));
  });

  it('encodes nothing-set to [HELLO_MSG_TYPE, 0x00]', () => {
    const bytes = encodeHello(false, false);
    expect(bytes).toEqual(new Uint8Array([HELLO_MSG_TYPE, 0x00]));
  });

  it('encodes supportsH264=true to [HELLO_MSG_TYPE, 0x02]', () => {
    const bytes = encodeHello(false, true);
    expect(bytes).toEqual(new Uint8Array([HELLO_MSG_TYPE, 0x02]));
  });

  it('encodes both caps set to [HELLO_MSG_TYPE, 0x03]', () => {
    const bytes = encodeHello(true, true);
    expect(bytes).toEqual(new Uint8Array([HELLO_MSG_TYPE, 0x03]));
  });
});

describe('encodeDecodeError', () => {
  it('encodes a thin-uncached-palette error', () => {
    const bytes = encodeDecodeError(2, 7, 13, ERR_THIN_UNCACHED_PALETTE)!;
    expect(bytes).toEqual(new Uint8Array([
      DECODE_ERROR_MSG_TYPE, 2, 7, 13, ERR_THIN_UNCACHED_PALETTE,
    ]));
  });

  it('emits exactly 5 bytes', () => {
    const bytes = encodeDecodeError(2, 0, 0, 5)!;
    expect(bytes.length).toBe(5);
  });
});
