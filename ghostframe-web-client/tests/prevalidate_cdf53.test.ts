import { describe, it, expect } from 'vitest';
import { rleDecode } from '../src/prevalidate_cdf53.js';
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
