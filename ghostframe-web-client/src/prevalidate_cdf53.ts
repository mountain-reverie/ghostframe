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
