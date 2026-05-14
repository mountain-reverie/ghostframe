import { describe, it, expect } from 'vitest';
import {
  prevalidatePalRle,
  PalRleVariant,
  type PalRleEntry,
} from '../src/prevalidate.js';
import { PaletteShadow } from '../src/palette_shadow.js';
import {
  ERR_PAYLOAD_TOO_SHORT,
  ERR_COUNT_OUT_OF_RANGE,
  ERR_THIN_UNCACHED_PALETTE,
  ERR_BUNDLED_TRUNCATED,
} from '../src/feedback.js';

function bundledPayload(paletteId: number, count: number, rleBytes: number[]): Uint8Array {
  const palette: number[] = [];
  for (let i = 0; i < count; i++) {
    palette.push(0xFF, 0x00, 0x00, 0xFF); // bgra red
  }
  return new Uint8Array([0x01, paletteId, count, ...palette, ...rleBytes]);
}

function thinPayload(paletteId: number, rleBytes: number[]): Uint8Array {
  return new Uint8Array([0x00, paletteId, ...rleBytes]);
}

function indicesRawPayload(paletteId: number, indices512: Uint8Array): Uint8Array {
  const out = new Uint8Array(2 + indices512.length);
  out[0] = 0x02;
  out[1] = paletteId;
  out.set(indices512, 2);
  return out;
}

describe('prevalidatePalRle: error paths', () => {
  it('reports payload too short', () => {
    const shadow = new PaletteShadow();
    const r = prevalidatePalRle(new Uint8Array([0x00]), shadow);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.errorCode).toBe(ERR_PAYLOAD_TOO_SHORT);
  });

  it('reports bundled count=0', () => {
    const shadow = new PaletteShadow();
    const r = prevalidatePalRle(bundledPayload(5, 0, [0x00]), shadow);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.errorCode).toBe(ERR_COUNT_OUT_OF_RANGE);
  });

  it('reports bundled count=17', () => {
    const shadow = new PaletteShadow();
    // can't actually build a 17-color payload via the helper, hand-roll:
    const payload = new Uint8Array([0x01, 5, 17, /* palette short on purpose */]);
    const r = prevalidatePalRle(payload, shadow);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.errorCode).toBe(ERR_COUNT_OUT_OF_RANGE);
  });

  it('reports bundled truncated palette', () => {
    const shadow = new PaletteShadow();
    // count=2 means 8 bytes of palette expected; supply only 4.
    const payload = new Uint8Array([0x01, 5, 2, 0xFF, 0, 0, 0xFF]);
    const r = prevalidatePalRle(payload, shadow);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.errorCode).toBe(ERR_BUNDLED_TRUNCATED);
  });

  it('reports thin uncached palette', () => {
    const shadow = new PaletteShadow(); // shadow empty
    const r = prevalidatePalRle(thinPayload(99, [0x0F]), shadow);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.errorCode).toBe(ERR_THIN_UNCACHED_PALETTE);
  });

  it('reports indices_raw too short', () => {
    const shadow = new PaletteShadow();
    shadow.put(5, 1);
    const payload = new Uint8Array([0x02, 5, ...new Array(500).fill(0)]); // 502 bytes, not 514
    const r = prevalidatePalRle(payload, shadow);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.errorCode).toBe(ERR_PAYLOAD_TOO_SHORT);
  });
});

describe('prevalidatePalRle: success paths', () => {
  it('bundled with 1-color tile expands all-same indices', () => {
    const shadow = new PaletteShadow();
    // Single run of 16, repeated 64 times = 1024 pixels of index 0.
    const rle = new Array(64).fill(0x0F); // (0 << 4) | (16-1) = 0x0F repeated
    const r = prevalidatePalRle(bundledPayload(5, 1, rle), shadow);
    expect(r.ok).toBe(true);
    if (r.ok) {
      expect(r.entry.variant).toBe(PalRleVariant.Bundled);
      expect(r.entry.paletteId).toBe(5);
      expect(r.entry.count).toBe(1);
      expect(r.entry.indices.length).toBe(512);
      // All nibbles should be 0 (index 0).
      for (let i = 0; i < 512; i++) expect(r.entry.indices[i]).toBe(0);
    }
  });

  it('thin against pre-populated shadow', () => {
    const shadow = new PaletteShadow();
    shadow.put(7, 4);
    const rle = new Array(64).fill(0x0F);
    const r = prevalidatePalRle(thinPayload(7, rle), shadow);
    expect(r.ok).toBe(true);
    if (r.ok) {
      expect(r.entry.variant).toBe(PalRleVariant.Thin);
      expect(r.entry.paletteId).toBe(7);
      expect(r.entry.count).toBe(4);
    }
  });

  it('indices_raw passes 512 bytes verbatim', () => {
    const shadow = new PaletteShadow();
    shadow.put(9, 8);
    const indices = new Uint8Array(512);
    for (let i = 0; i < 512; i++) indices[i] = (i & 0x77); // any pattern <= 7 nibbles
    const r = prevalidatePalRle(indicesRawPayload(9, indices), shadow);
    expect(r.ok).toBe(true);
    if (r.ok) {
      expect(r.entry.variant).toBe(PalRleVariant.IndicesRaw);
      expect(r.entry.paletteId).toBe(9);
      expect(r.entry.indices).toEqual(indices);
    }
  });

  it('bundled returns paletteUpsert with the colors', () => {
    const shadow = new PaletteShadow();
    const rle = new Array(64).fill(0x0F);
    const r = prevalidatePalRle(bundledPayload(11, 2, rle), shadow);
    expect(r.ok).toBe(true);
    if (r.ok && r.entry.variant === PalRleVariant.Bundled) {
      expect(r.entry.paletteUpsert!.length).toBe(8); // 2 × 4 bytes
      expect(r.entry.paletteUpsert![0]).toBe(0xFF); // B
    }
  });
});

describe('expandRleToIndices: adversarial patterns', () => {
  it('alternating-index sequence packs correctly', () => {
    const shadow = new PaletteShadow();
    // index 0 run-1, index 1 run-1, index 0 run-1, ... 1024 times → 1024 RLE bytes
    const rle: number[] = [];
    for (let i = 0; i < 1024; i++) {
      const idx = i & 1;
      rle.push((idx << 4) | 0); // run length 1
    }
    const r = prevalidatePalRle(bundledPayload(3, 2, rle), shadow);
    expect(r.ok).toBe(true);
    if (r.ok) {
      // pixel 0 = 0, pixel 1 = 1, pixel 2 = 0, ...
      // Stored as: byte 0 = (pixel 1 << 4) | pixel 0 = 0x10
      //            byte 1 = (pixel 3 << 4) | pixel 2 = 0x10
      // Every byte should be 0x10.
      for (let i = 0; i < 512; i++) expect(r.entry.indices[i]).toBe(0x10);
    }
  });
});
