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
