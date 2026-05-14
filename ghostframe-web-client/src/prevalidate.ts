import { PaletteShadow } from './palette_shadow.js';
import {
  ERR_PAYLOAD_TOO_SHORT,
  ERR_COUNT_OUT_OF_RANGE,
  ERR_THIN_UNCACHED_PALETTE,
  ERR_BUNDLED_TRUNCATED,
} from './feedback.js';

export enum PalRleVariant {
  Bundled = 0,
  Thin = 1,
  IndicesRaw = 2,
}

export interface PalRleEntry {
  variant: PalRleVariant;
  paletteId: number;
  count: number;                 // 1..16
  indices: Uint8Array;           // 512 bytes (1024 × 4-bit indices, low-nibble = even pixel)
  paletteUpsert?: Uint8Array;    // Bundled only: count × 4 BGRA bytes
}

export type PrevalidateResult =
  | { ok: true; entry: PalRleEntry }
  | { ok: false; errorCode: number };

/**
 * Validate + materialize a PalRle payload into a 512 B indices buffer that
 * the GPU compute shader can consume directly.
 *
 * - Bundled (flags bit 0 set): also surfaces a `paletteUpsert` blob so the
 *   caller can `queue.writeBuffer(palette_atlas, ...)` before dispatch.
 * - Thin (flags clear): requires `shadow.has(palette_id) === true`.
 * - IndicesRaw (flags bit 1 set): 512 B copied verbatim from payload.
 *
 * On any structural failure returns `{ ok: false, errorCode }` matching the
 * server's error_code taxonomy.
 */
export function prevalidatePalRle(
  payload: Uint8Array,
  shadow: PaletteShadow,
): PrevalidateResult {
  if (payload.length < 2) {
    return { ok: false, errorCode: ERR_PAYLOAD_TOO_SHORT };
  }
  const flags = payload[0];
  const paletteId = payload[1];
  const bundled = (flags & 0x01) !== 0;
  const indicesRaw = (flags & 0x02) !== 0;

  if (indicesRaw) {
    // [0x02][palette_id][512 B indices]
    if (payload.length < 2 + 512) {
      return { ok: false, errorCode: ERR_PAYLOAD_TOO_SHORT };
    }
    if (!shadow.has(paletteId)) {
      return { ok: false, errorCode: ERR_THIN_UNCACHED_PALETTE };
    }
    const indices = new Uint8Array(512);
    indices.set(payload.subarray(2, 2 + 512));
    return {
      ok: true,
      entry: {
        variant: PalRleVariant.IndicesRaw,
        paletteId,
        count: shadow.count(paletteId),
        indices,
      },
    };
  }

  let cursor = 2;
  let count: number;
  let paletteUpsert: Uint8Array | undefined;

  if (bundled) {
    if (cursor >= payload.length) {
      return { ok: false, errorCode: ERR_BUNDLED_TRUNCATED };
    }
    count = payload[cursor++];
    if (count < 1 || count > 16) {
      return { ok: false, errorCode: ERR_COUNT_OUT_OF_RANGE };
    }
    if (cursor + count * 4 > payload.length) {
      return { ok: false, errorCode: ERR_BUNDLED_TRUNCATED };
    }
    paletteUpsert = payload.subarray(cursor, cursor + count * 4);
    cursor += count * 4;
  } else {
    if (!shadow.has(paletteId)) {
      return { ok: false, errorCode: ERR_THIN_UNCACHED_PALETTE };
    }
    count = shadow.count(paletteId);
  }

  // Expand RLE bytes into 1024 4-bit indices.
  const indices = expandRleToIndices(payload.subarray(cursor));
  if (indices === null) {
    return { ok: false, errorCode: ERR_PAYLOAD_TOO_SHORT };
  }

  return {
    ok: true,
    entry: {
      variant: bundled ? PalRleVariant.Bundled : PalRleVariant.Thin,
      paletteId,
      count,
      indices,
      paletteUpsert: paletteUpsert ? new Uint8Array(paletteUpsert) : undefined,
    },
  };
}

/**
 * Expand nibble-packed RLE bytes `(idx << 4) | (run_len - 1)` into 512 bytes
 * of 4-bit-packed indices (low nibble of byte 0 = pixel 0, high nibble of
 * byte 0 = pixel 1, etc.). Returns null if expansion doesn't yield exactly
 * 1024 pixels.
 */
export function expandRleToIndices(rleBytes: Uint8Array): Uint8Array | null {
  const indices = new Uint8Array(512);
  let pixelIdx = 0;
  for (let i = 0; i < rleBytes.length; i++) {
    const b = rleBytes[i];
    const idx = (b >> 4) & 0x0F;
    const runLen = (b & 0x0F) + 1;
    for (let j = 0; j < runLen; j++) {
      if (pixelIdx >= 1024) return null;
      const byteIdx = pixelIdx >> 1;
      if ((pixelIdx & 1) === 0) {
        indices[byteIdx] = (indices[byteIdx] & 0xF0) | idx;
      } else {
        indices[byteIdx] = (indices[byteIdx] & 0x0F) | (idx << 4);
      }
      pixelIdx++;
    }
  }
  if (pixelIdx !== 1024) return null;
  return indices;
}
