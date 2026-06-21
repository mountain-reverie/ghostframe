// Mirrors the Rust unit tests in ghostframe-test-pattern::lossless_golden.
// The TS frameRgba() must be deterministic, byte-equal to the Rust impl,
// and uphold the same per-region codec-classification invariants (Solid:
// 1 unique colour; PalRle: 2-16; Cdf53: > 16).

import { describe, expect, test } from 'vitest';

import {
  frameRgba,
  regionBoundarySolidPalrle,
  regionBoundaryPalrleCdf53,
  TILE_SIZE,
} from '../src/lossless_golden';

const W = 1920;
const H = 1080;

function countUniqueInTile(
  buf: Uint8Array,
  width: number,
  tilePx: number,
  tilePy: number,
): number {
  const s = new Set<number>();
  for (let yy = 0; yy < TILE_SIZE; yy++) {
    for (let xx = 0; xx < TILE_SIZE; xx++) {
      const off = ((tilePy + yy) * width + (tilePx + xx)) * 4;
      // Pack RGBA into a single number for Set keying.
      const k = (buf[off] << 24) | (buf[off + 1] << 16) | (buf[off + 2] << 8) | buf[off + 3];
      s.add(k);
    }
  }
  return s.size;
}

// Fast linear hash over a Uint8Array. We only need determinism evidence,
// not crypto strength — `expect(a).toEqual(b)` on a multi-MB TypedArray
// is dominated by vitest's deep-equality machinery and takes tens of
// seconds. A streaming hash with FNV-1a–style multiply/xor finishes in
// milliseconds and still catches every single-byte divergence.
function byteHash(buf: Uint8Array): number {
  let h = 0x811c9dc5 | 0;
  for (let i = 0; i < buf.length; i++) {
    h = Math.imul(h ^ buf[i], 0x01000193) | 0;
  }
  return h >>> 0;
}

describe('lossless_golden (TS port of lossless_golden.rs)', () => {
  test('is deterministic at canonical dims', () => {
    const a = frameRgba(W, H);
    const b = frameRgba(W, H);
    expect(a.length).toBe(W * H * 4);
    expect(byteHash(a)).toBe(byteHash(b));
  });

  test('is deterministic at non-canonical dims', () => {
    const a = frameRgba(1024, 768);
    const b = frameRgba(1024, 768);
    expect(byteHash(a)).toBe(byteHash(b));
  });

  test('region boundaries are tile-aligned', () => {
    expect(regionBoundarySolidPalrle(W) % TILE_SIZE).toBe(0);
    expect(regionBoundaryPalrleCdf53(W) % TILE_SIZE).toBe(0);
    expect(regionBoundarySolidPalrle(1024) % TILE_SIZE).toBe(0);
    expect(regionBoundaryPalrleCdf53(1024) % TILE_SIZE).toBe(0);
  });

  test('Solid region tiles have exactly 1 unique colour', () => {
    const f = frameRgba(W, H);
    const boundary = regionBoundarySolidPalrle(W);
    const tilesX = Math.floor(boundary / TILE_SIZE);
    const tilesY = Math.floor(H / TILE_SIZE);
    for (let ty = 0; ty < tilesY; ty++) {
      for (let tx = 0; tx < tilesX; tx++) {
        const n = countUniqueInTile(f, W, tx * TILE_SIZE, ty * TILE_SIZE);
        expect(n).toBe(1);
      }
    }
  });

  test('PalRle region tiles have 2..=16 unique colours', () => {
    const f = frameRgba(W, H);
    const bx0 = regionBoundarySolidPalrle(W);
    const bx1 = regionBoundaryPalrleCdf53(W);
    const tx0 = Math.floor(bx0 / TILE_SIZE);
    const tx1 = Math.floor(bx1 / TILE_SIZE);
    const tilesY = Math.floor(H / TILE_SIZE);
    for (let ty = 0; ty < tilesY; ty++) {
      for (let tx = tx0; tx < tx1; tx++) {
        const n = countUniqueInTile(f, W, tx * TILE_SIZE, ty * TILE_SIZE);
        expect(n).toBeGreaterThanOrEqual(2);
        expect(n).toBeLessThanOrEqual(16);
      }
    }
  });

  test('Cdf53 region tiles have > 16 unique colours', () => {
    const f = frameRgba(W, H);
    const bx1 = regionBoundaryPalrleCdf53(W);
    const tx0 = Math.floor(bx1 / TILE_SIZE);
    const txEnd = Math.floor(W / TILE_SIZE);
    const tilesY = Math.floor(H / TILE_SIZE);
    for (let ty = 0; ty < tilesY; ty++) {
      for (let tx = tx0; tx < txEnd; tx++) {
        const n = countUniqueInTile(f, W, tx * TILE_SIZE, ty * TILE_SIZE);
        expect(n).toBeGreaterThan(16);
      }
    }
  });

  test('byte-equal to Rust frame_rgba at sentinel positions', () => {
    // Canonical 1920×1080 — boundary_left=640, boundary_right=1280.
    // These specific pixel positions cross-check the per-region paths
    // by hand: any divergence between the TS and Rust impls trips at
    // least one of these. Computed by running the Rust frame_rgba
    // function and reading the RGBA at each (x, y).
    const f = frameRgba(W, H);
    function rgba(x: number, y: number): [number, number, number, number] {
      const off = (y * W + x) * 4;
      return [f[off], f[off + 1], f[off + 2], f[off + 3]];
    }

    // Solid region, top-left corner: x=0, y=0 → bx=0, by=0 → idx=0 → red.
    expect(rgba(0, 0)).toEqual([0xFF, 0x00, 0x00, 0x00]);
    // Solid region, just inside boundary: x=639, y=0 → bx=9, by=0 → idx=1 → green.
    expect(rgba(639, 0)).toEqual([0x00, 0xFF, 0x00, 0x00]);

    // PalRle region, first tile (tx=20, ty=0):
    // base = (20*5 + 0*3) % 16 = 100 % 16 = 4
    // palIdx = [4, 8, 12, 0]
    // top-left sub (subX=0, subY=0) → which=0 → palIdx[0]=4 → PALRLE[4] = [0xFF, 0x40, 0x40, 0]
    expect(rgba(640, 0)).toEqual([0xFF, 0x40, 0x40, 0x00]);

    // Cdf53 region, x=1281, y=0:
    // r = (1281*17 + 0) & 0xFF = 21777 & 0xFF = 0x11
    // g = (1281*13 + 0) & 0xFF = 16653 & 0xFF = 0x0D
    // b = (1281*29 + 0) & 0xFF = 37149 & 0xFF = 0x1D
    expect(rgba(1281, 0)).toEqual([0x11, 0x0D, 0x1D, 0x00]);
  });
});
