import { describe, it, expect } from 'vitest';
import {
  WasmPaletteShadow,
  prevalidatePalRle,
  palRleVariants,
  errorCodes,
} from '../pkg-node/ghostframe_client_wasm.js';

const PalRleVariant = palRleVariants();
const {
  ERR_PAYLOAD_TOO_SHORT,
  ERR_COUNT_OUT_OF_RANGE,
  ERR_THIN_UNCACHED_PALETTE,
  ERR_BUNDLED_TRUNCATED,
} = errorCodes();

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
    const shadow = new WasmPaletteShadow();
    const r = prevalidatePalRle(new Uint8Array([0x00]), shadow);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.code).toBe(ERR_PAYLOAD_TOO_SHORT);
  });

  it('reports bundled count=0', () => {
    const shadow = new WasmPaletteShadow();
    const r = prevalidatePalRle(bundledPayload(5, 0, [0x00]), shadow);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.code).toBe(ERR_COUNT_OUT_OF_RANGE);
  });

  it('reports bundled count=17', () => {
    const shadow = new WasmPaletteShadow();
    // can't actually build a 17-color payload via the helper, hand-roll:
    const payload = new Uint8Array([0x01, 5, 17, /* palette short on purpose */]);
    const r = prevalidatePalRle(payload, shadow);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.code).toBe(ERR_COUNT_OUT_OF_RANGE);
  });

  it('reports bundled truncated palette', () => {
    const shadow = new WasmPaletteShadow();
    // count=2 means 8 bytes of palette expected; supply only 4.
    const payload = new Uint8Array([0x01, 5, 2, 0xFF, 0, 0, 0xFF]);
    const r = prevalidatePalRle(payload, shadow);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.code).toBe(ERR_BUNDLED_TRUNCATED);
  });

  it('reports thin uncached palette', () => {
    const shadow = new WasmPaletteShadow(); // shadow empty
    const r = prevalidatePalRle(thinPayload(99, [0x0F]), shadow);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.code).toBe(ERR_THIN_UNCACHED_PALETTE);
  });

  it('reports indices_raw too short', () => {
    const shadow = new WasmPaletteShadow();
    shadow.put(5, 1);
    const payload = new Uint8Array([0x02, 5, ...new Array(500).fill(0)]); // 502 bytes, not 514
    const r = prevalidatePalRle(payload, shadow);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.code).toBe(ERR_PAYLOAD_TOO_SHORT);
  });
});

describe('prevalidatePalRle: success paths', () => {
  it('bundled with 1-color tile expands all-same indices', () => {
    const shadow = new WasmPaletteShadow();
    // Single run of 16, repeated 64 times = 1024 pixels of index 0.
    const rle = new Array(64).fill(0x0F); // (0 << 4) | (16-1) = 0x0F repeated
    const r = prevalidatePalRle(bundledPayload(5, 1, rle), shadow);
    expect(r.ok).toBe(true);
    if (r.ok) {
      expect(r.variant).toBe(PalRleVariant.Bundled);
      expect(r.palette_id).toBe(5);
      expect(r.count).toBe(1);
      expect(r.indices.length).toBe(512);
      // All nibbles should be 0 (index 0).
      for (let i = 0; i < 512; i++) expect(r.indices[i]).toBe(0);
    }
  });

  it('thin against pre-populated shadow', () => {
    const shadow = new WasmPaletteShadow();
    shadow.put(7, 4);
    const rle = new Array(64).fill(0x0F);
    const r = prevalidatePalRle(thinPayload(7, rle), shadow);
    expect(r.ok).toBe(true);
    if (r.ok) {
      expect(r.variant).toBe(PalRleVariant.Thin);
      expect(r.palette_id).toBe(7);
      expect(r.count).toBe(4);
    }
  });

  it('indices_raw passes 512 bytes verbatim', () => {
    const shadow = new WasmPaletteShadow();
    shadow.put(9, 8);
    const indices = new Uint8Array(512);
    for (let i = 0; i < 512; i++) indices[i] = (i & 0x77); // any pattern <= 7 nibbles
    const r = prevalidatePalRle(indicesRawPayload(9, indices), shadow);
    expect(r.ok).toBe(true);
    if (r.ok) {
      expect(r.variant).toBe(PalRleVariant.IndicesRaw);
      expect(r.palette_id).toBe(9);
      // r.indices crosses the wasm boundary as a plain Array (serde_wasm_bindgen
      // has no serde_bytes annotation on WasmPrevalidatedPalRle::indices), not a
      // Uint8Array, so a raw toEqual against the Uint8Array fixture fails on
      // constructor identity even though every byte matches. Array.from(...)
      // on both sides compares values only — representation, not behaviour.
      expect(Array.from(r.indices)).toEqual(Array.from(indices));
    }
  });

  it('bundled returns paletteUpsert with the colors', () => {
    const shadow = new WasmPaletteShadow();
    const rle = new Array(64).fill(0x0F);
    const r = prevalidatePalRle(bundledPayload(11, 2, rle), shadow);
    expect(r.ok).toBe(true);
    if (r.ok && r.variant === PalRleVariant.Bundled) {
      expect(r.palette_upsert.length).toBe(8); // 2 × 4 bytes
      expect(r.palette_upsert[0]).toBe(0xFF); // B
    }
  });
});

describe('expandRleToIndices: adversarial patterns', () => {
  it('alternating-index sequence packs correctly', () => {
    const shadow = new WasmPaletteShadow();
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
      for (let i = 0; i < 512; i++) expect(r.indices[i]).toBe(0x10);
    }
  });
});
