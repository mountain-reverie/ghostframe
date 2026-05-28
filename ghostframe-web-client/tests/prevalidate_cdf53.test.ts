import { describe, it, expect } from 'vitest';
import { rleDecode } from '../src/prevalidate_cdf53.js';
import {
  prevalidateCdf53,
  type PrevalidatedCdf53,
} from '../src/prevalidate_cdf53.js';
import {
  ERR_CDF53_BAD_PASS,
  ERR_CDF53_TRUNCATED,
  ERR_CDF53_RLE_LENGTH,
} from '../src/feedback.js';
import fixture from '../../ghostframe-e2e/tests/fixtures/cdf53_fixture.json';

describe('rleDecode', () => {
  it('all-zero token decodes to 128 zero bytes', () => {
    // Single zero-run token: 0x80 | 127 = 0xFF means "128 zeros".
    const out = rleDecode(new Uint8Array([0xFF]));
    expect(out.length).toBe(128);
    expect(out.every((b) => b === 0)).toBe(true);
  });

  it('two zero-runs concatenate', () => {
    // 0x80 = 1 zero, 0x81 = 2 zeros → total 3 zeros.
    const out = rleDecode(new Uint8Array([0x80, 0x81]));
    expect(Array.from(out)).toEqual([0, 0, 0]);
  });

  it('literal bytes pass through', () => {
    // 0x05, 0x42, 0x10 are all < 0x7F → literal.
    const out = rleDecode(new Uint8Array([0x05, 0x42, 0x10]));
    expect(Array.from(out)).toEqual([0x05, 0x42, 0x10]);
  });

  it('0x7F escape emits the next byte literally', () => {
    // 0x7F escape, then 0x80 → emits 0x80 (not a zero-run).
    const out = rleDecode(new Uint8Array([0x7F, 0x80, 0x05]));
    expect(Array.from(out)).toEqual([0x80, 0x05]);
  });

  // Fixture-driven exhaustive roundtrip across all 14 passes × 3 channels.
  it('matches Rust-encoded fixture for all 14 passes × 3 channels', () => {
    // For each pass, the encoded payload is [u16 BE len_B][rle_B][u16 BE len_G][rle_G][u16 BE len_R][rle_R].
    // Walk each channel and assert rleDecode matches fixture.bit_planes_per_pass[pass][ch].
    for (let pass = 0; pass < fixture.pass_count; pass++) {
      const payload = new Uint8Array(fixture.encoded_passes[pass]);
      let offset = 0;
      for (let ch = 0; ch < fixture.channels; ch++) {
        const len = (payload[offset] << 8) | payload[offset + 1];
        offset += 2;
        const rle = payload.subarray(offset, offset + len);
        offset += len;
        const decoded = rleDecode(rle);
        expect(decoded.length).toBe(128);
        const expected = fixture.bit_planes_per_pass[pass][ch];
        for (let i = 0; i < 128; i++) {
          expect(decoded[i]).toBe(expected[i]);
        }
      }
    }
  });
});

describe('prevalidateCdf53', () => {
  it('happy-path roundtrip vs fixture', () => {
    // For each of the 14 passes in the fixture, prevalidate and assert the
    // packed [B][G][R] bit-planes match bit_planes_per_pass[pass].
    for (let passIdx = 0; passIdx < fixture.pass_count; passIdx++) {
      const payload = new Uint8Array(fixture.encoded_passes[passIdx]);
      const r = prevalidateCdf53(payload, /*gen*/ 1, passIdx);
      expect(r.ok).toBe(true);
      if (!r.ok) return; // narrowing for TS
      expect(r.entry.tileX).toBe(0); // caller-supplied; we don't pass it here, default 0
      expect(r.entry.gen).toBe(1);
      expect(r.entry.passIdx).toBe(passIdx);
      expect(r.entry.bitPlanes.length).toBe(384);
      for (let ch = 0; ch < 3; ch++) {
        const expected = fixture.bit_planes_per_pass[passIdx][ch];
        for (let i = 0; i < 128; i++) {
          expect(r.entry.bitPlanes[ch * 128 + i]).toBe(expected[i]);
        }
      }
    }
  });

  it('rejects pass_idx >= 14 with ERR_CDF53_BAD_PASS', () => {
    const r = prevalidateCdf53(new Uint8Array([0, 1, 0xFF]), 0, 14);
    expect(r.ok).toBe(false);
    if (r.ok) return;
    expect(r.errorCode).toBe(ERR_CDF53_BAD_PASS);
  });

  it('rejects truncated section header with ERR_CDF53_TRUNCATED', () => {
    // Only 1 byte — can't even read the first u16 length.
    const r = prevalidateCdf53(new Uint8Array([0]), 0, 0);
    expect(r.ok).toBe(false);
    if (r.ok) return;
    expect(r.errorCode).toBe(ERR_CDF53_TRUNCATED);
  });

  it('rejects RLE that decodes to wrong byte count with ERR_CDF53_RLE_LENGTH', () => {
    // One literal byte ⇒ decodes to 1 byte, not 128. Channel B only, then truncated.
    const r = prevalidateCdf53(new Uint8Array([0, 1, 0x05]), 0, 0);
    expect(r.ok).toBe(false);
    if (r.ok) return;
    expect(r.errorCode).toBe(ERR_CDF53_RLE_LENGTH);
  });
});
