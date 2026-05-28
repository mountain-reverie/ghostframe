import {
  ERR_CDF53_BAD_PASS,
  ERR_CDF53_TRUNCATED,
  ERR_CDF53_RLE_LENGTH,
} from './feedback.js';

export interface PrevalidatedCdf53 {
  tileX: number;
  tileY: number;
  gen: number;     // 0..15
  passIdx: number; // 0..13
  bitPlanes: Uint8Array; // 384 = 128 × 3 (B, G, R)
}

export type Cdf53Result =
  | { ok: true; entry: PrevalidatedCdf53 }
  | { ok: false; errorCode: number };

/**
 * Parse one Cdf53 pass payload into 3 × 128-byte bit-planes ready for GPU
 * upload. `tileX` and `tileY` default to 0 here; caller fills them from the
 * tile header before queueing. `gen` and `passIdx` come from the tile
 * header's `(gen << 4) | pass_idx` byte and are validated/stamped here.
 *
 * Payload layout (M3.3a wire format):
 *   [u16 BE len_B][rle_B][u16 BE len_G][rle_G][u16 BE len_R][rle_R]
 */
export function prevalidateCdf53(
  payload: Uint8Array,
  gen: number,
  passIdx: number,
): Cdf53Result {
  if (passIdx >= 14) return { ok: false, errorCode: ERR_CDF53_BAD_PASS };

  const bitPlanes = new Uint8Array(384);
  let offset = 0;
  for (let ch = 0; ch < 3; ch++) {
    if (offset + 2 > payload.length) {
      return { ok: false, errorCode: ERR_CDF53_TRUNCATED };
    }
    const len = (payload[offset] << 8) | payload[offset + 1];
    offset += 2;
    if (offset + len > payload.length) {
      return { ok: false, errorCode: ERR_CDF53_TRUNCATED };
    }
    const decoded = rleDecode(payload.subarray(offset, offset + len));
    offset += len;
    if (decoded.length !== 128) {
      return { ok: false, errorCode: ERR_CDF53_RLE_LENGTH };
    }
    bitPlanes.set(decoded, ch * 128);
  }
  return {
    ok: true,
    entry: { tileX: 0, tileY: 0, gen, passIdx, bitPlanes },
  };
}

/**
 * Run-length decoder mirroring `cdf53.rs:rle_decode` exactly. Token layout:
 *   - 0x00..=0x7E: literal byte (data byte itself).
 *   - 0x7F: two-byte literal escape — next byte is the actual literal value.
 *   - 0x80..=0xFF: zero run of `(token & 0x7F) + 1` bytes.
 *
 * Output length is unbounded — callers that expect a fixed size MUST verify.
 */
export function rleDecode(rle: Uint8Array): Uint8Array {
  // Worst case: every byte is a literal (no compression). Allocate that
  // upper bound to skip resizing; trim at the end.
  const out = new Uint8Array(rle.length * 128);
  let outLen = 0;
  let i = 0;
  while (i < rle.length) {
    const token = rle[i];
    if (token === 0x7F) {
      if (i + 1 >= rle.length) break;
      out[outLen++] = rle[i + 1];
      i += 2;
    } else if ((token & 0x80) !== 0) {
      const runLen = (token & 0x7F) + 1;
      for (let k = 0; k < runLen; k++) out[outLen++] = 0;
      i += 1;
    } else {
      out[outLen++] = token;
      i += 1;
    }
  }
  return out.subarray(0, outLen);
}
