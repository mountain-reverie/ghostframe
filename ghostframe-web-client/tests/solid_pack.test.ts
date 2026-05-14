import { describe, it, expect } from 'vitest';

// We can't import the real SolidPipeline (it needs GPUDevice). Test the
// pack-to-Uint32Array math by replicating the formula here. If this test
// drifts from the real implementation, the formula in solid.ts changed —
// update both.

function packBgraColor(bgra: Uint8Array): number {
  return (bgra[0] | (bgra[1] << 8) | (bgra[2] << 16) | (bgra[3] << 24)) >>> 0;
}

describe('solid color packing', () => {
  it('packs BGRA = [B, G, R, A] into u32 with B at LSB', () => {
    expect(packBgraColor(new Uint8Array([0xFF, 0x00, 0x00, 0xFF]))).toBe(0xFF0000FF);
    expect(packBgraColor(new Uint8Array([0x00, 0xFF, 0x00, 0xFF]))).toBe(0xFF00FF00);
    expect(packBgraColor(new Uint8Array([0x00, 0x00, 0xFF, 0xFF]))).toBe(0xFFFF0000);
  });
});
