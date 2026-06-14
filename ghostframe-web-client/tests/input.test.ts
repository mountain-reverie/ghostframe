import { describe, it, expect } from 'vitest';
import { keyboardEventToKeysym } from '../src/input/keymap.js';

function ev(key: string): KeyboardEvent {
  return { key } as unknown as KeyboardEvent;
}

describe('keyboardEventToKeysym', () => {
  it('returns null for dead keys', () => {
    expect(keyboardEventToKeysym(ev('Dead'))).toBeNull();
    expect(keyboardEventToKeysym(ev('Unidentified'))).toBeNull();
  });

  it('maps Enter to XK_Return (0xff0d)', () => {
    expect(keyboardEventToKeysym(ev('Enter'))).toBe(0xff0d);
  });

  it('maps ArrowUp to XK_Up (0xff52)', () => {
    expect(keyboardEventToKeysym(ev('ArrowUp'))).toBe(0xff52);
  });

  it('maps F1 to XK_F1 (0xffbe)', () => {
    expect(keyboardEventToKeysym(ev('F1'))).toBe(0xffbe);
  });

  it("maps printable 'a' to 0x61", () => {
    expect(keyboardEventToKeysym(ev('a'))).toBe(0x61);
  });

  it("maps Latin-1 'ñ' to 0xf1", () => {
    expect(keyboardEventToKeysym(ev('ñ'))).toBe(0xf1);
  });

  it("maps higher BMP '中' to 0x01000000 | codepoint", () => {
    expect(keyboardEventToKeysym(ev('中'))).toBe(0x01000000 | 0x4e2d);
  });

  it('returns null for unknown multi-char names', () => {
    expect(keyboardEventToKeysym(ev('NotARealKey'))).toBeNull();
  });

  it('handles space (KeyboardEvent.key = " ")', () => {
    expect(keyboardEventToKeysym(ev(' '))).toBe(0x20);
  });
});

import {
  encodePointerMove,
  encodePointerButton,
  encodeWheel,
  encodeKeyDown,
  encodeKeyUp,
  INPUT_MSG_TYPE,
} from '../src/input/encode.js';

describe('encodeInput*', () => {
  it('encodePointerMove writes [0x05, 0x01, x:i16, y:i16] (6 bytes)', () => {
    const buf = encodePointerMove(400, -2);
    expect(buf).toEqual(new Uint8Array([0x05, 0x01, 0x01, 0x90, 0xff, 0xfe]));
  });

  it('encodePointerButton writes [0x05, 0x02, x:i16, y:i16, btn, down]', () => {
    const buf = encodePointerButton(5, 10, 1, true);
    expect(buf).toEqual(new Uint8Array([0x05, 0x02, 0, 5, 0, 10, 1, 1]));
  });

  it('encodePointerButton encodes down=false correctly', () => {
    const buf = encodePointerButton(5, 10, 3, false);
    expect(buf).toEqual(new Uint8Array([0x05, 0x02, 0, 5, 0, 10, 3, 0]));
  });

  it('encodeWheel writes [0x05, 0x03, dx:i16, dy:i16]', () => {
    const buf = encodeWheel(0, 1);
    expect(buf).toEqual(new Uint8Array([0x05, 0x03, 0, 0, 0, 1]));
  });

  it('encodeKeyDown writes [0x05, 0x04, keysym:u32 BE]', () => {
    const buf = encodeKeyDown(0xff0d);
    expect(buf).toEqual(new Uint8Array([0x05, 0x04, 0, 0, 0xff, 0x0d]));
  });

  it('encodeKeyUp writes [0x05, 0x05, keysym:u32 BE]', () => {
    const buf = encodeKeyUp(0x61);
    expect(buf).toEqual(new Uint8Array([0x05, 0x05, 0, 0, 0, 0x61]));
  });

  it('encodePointerMove encodes negative y correctly', () => {
    const buf = encodePointerMove(-1, -1);
    expect(buf).toEqual(new Uint8Array([0x05, 0x01, 0xff, 0xff, 0xff, 0xff]));
  });

  it('INPUT_MSG_TYPE is 0x05', () => {
    expect(INPUT_MSG_TYPE).toBe(0x05);
  });
});
