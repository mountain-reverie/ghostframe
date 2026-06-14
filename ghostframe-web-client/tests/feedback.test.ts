import { describe, it, expect } from 'vitest';
import {
  encodeHello,
  encodeDecodeError,
  HELLO_MSG_TYPE,
  DECODE_ERROR_MSG_TYPE,
  ERR_THIN_UNCACHED_PALETTE,
} from '../src/feedback.js';

describe('encodeHello', () => {
  it('encodes indices_raw=true to [HELLO_MSG_TYPE, 0x01]', () => {
    const bytes = encodeHello({ indicesRawEnabled: true, supportsH264: false });
    expect(bytes).toEqual(new Uint8Array([HELLO_MSG_TYPE, 0x01]));
  });

  it('encodes nothing-set to [HELLO_MSG_TYPE, 0x00]', () => {
    const bytes = encodeHello({ indicesRawEnabled: false, supportsH264: false });
    expect(bytes).toEqual(new Uint8Array([HELLO_MSG_TYPE, 0x00]));
  });

  it('encodes supportsH264=true to [HELLO_MSG_TYPE, 0x02]', () => {
    const bytes = encodeHello({ indicesRawEnabled: false, supportsH264: true });
    expect(bytes).toEqual(new Uint8Array([HELLO_MSG_TYPE, 0x02]));
  });

  it('encodes both caps set to [HELLO_MSG_TYPE, 0x03]', () => {
    const bytes = encodeHello({ indicesRawEnabled: true, supportsH264: true });
    expect(bytes).toEqual(new Uint8Array([HELLO_MSG_TYPE, 0x03]));
  });
});

describe('encodeDecodeError', () => {
  it('encodes a thin-uncached-palette error', () => {
    const bytes = encodeDecodeError({
      codec: 2,
      tileX: 7,
      tileY: 13,
      errorCode: ERR_THIN_UNCACHED_PALETTE,
    });
    expect(bytes).toEqual(new Uint8Array([
      DECODE_ERROR_MSG_TYPE, 2, 7, 13, ERR_THIN_UNCACHED_PALETTE,
    ]));
  });

  it('emits exactly 5 bytes', () => {
    const bytes = encodeDecodeError({
      codec: 2, tileX: 0, tileY: 0, errorCode: 5,
    });
    expect(bytes.length).toBe(5);
  });
});
