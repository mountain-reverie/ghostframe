import { describe, it, expect } from 'vitest';
import { PaletteShadow } from '../src/palette_shadow.js';

describe('PaletteShadow', () => {
  it('starts empty', () => {
    const s = new PaletteShadow();
    for (let i = 0; i < 256; i++) {
      expect(s.has(i)).toBe(false);
    }
  });

  it('records and reports presence', () => {
    const s = new PaletteShadow();
    s.put(7, 16);
    expect(s.has(7)).toBe(true);
    expect(s.count(7)).toBe(16);
  });

  it('overwrites existing entries', () => {
    const s = new PaletteShadow();
    s.put(7, 16);
    s.put(7, 4);
    expect(s.count(7)).toBe(4);
  });

  it('clear() removes everything', () => {
    const s = new PaletteShadow();
    s.put(7, 8);
    s.put(13, 16);
    s.clear();
    expect(s.has(7)).toBe(false);
    expect(s.has(13)).toBe(false);
  });
});
