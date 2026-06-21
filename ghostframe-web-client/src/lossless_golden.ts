// TypeScript port of `ghostframe-test-pattern/src/lossless_golden.rs`.
//
// Used by the e2e_lossless_golden_png test: the web client computes the
// expected RGBA frame in-browser rather than receiving multi-MB of bytes
// over CDP. Both implementations are pure functions of (width, height)
// and must produce identical output for any given dimensions.
//
// Why a JS port? Shipping ~3-8 MB of RGBA from Rust to Chromium via
// CDP Runtime.evaluate stalled CDP under the live datagram-paint load:
// the JS main thread is too busy decoding tiles to drain CDP requests
// promptly, so even tiny evaluate() calls (e.g. window.__reset...) hit
// chromiumoxide's 30 s request timeout. Computing the expected buffer
// in the browser sidesteps the transport entirely.

// Tile size — matches ghostframe-lib::tile::TILE_SIZE. Region boundaries
// are snapped to multiples of this so every tile lies entirely within
// one codec zone.
export const TILE_SIZE = 32;

// Side length of each Solid-region block. Must be a multiple of
// TILE_SIZE so each block covers a whole number of tiles.
const SOLID_BLOCK = 64;

// Four Solid-region colours (RGBA), arranged checkerboard-style on a
// SOLID_BLOCK grid. Alpha forced to 0x00 to match what the WebGPU
// renderer actually writes (canvas alphaMode: 'opaque').
const SOLID_PALETTE: ReadonlyArray<readonly [number, number, number, number]> = [
  [0xFF, 0x00, 0x00, 0x00],
  [0x00, 0xFF, 0x00, 0x00],
  [0x00, 0x00, 0xFF, 0x00],
  [0xFF, 0xFF, 0x00, 0x00],
];

// 16-colour global palette for the PalRle middle region. Each tile uses
// 4 colours sub-selected by (tile_x, tile_y).
const PALRLE_PALETTE: ReadonlyArray<readonly [number, number, number, number]> = [
  [0x00, 0x00, 0x00, 0x00],
  [0x10, 0x10, 0x10, 0x00],
  [0x20, 0x20, 0x20, 0x00],
  [0x30, 0x30, 0x30, 0x00],
  [0xFF, 0x40, 0x40, 0x00],
  [0x40, 0xFF, 0x40, 0x00],
  [0x40, 0x40, 0xFF, 0x00],
  [0xFF, 0xFF, 0x40, 0x00],
  [0xFF, 0x40, 0xFF, 0x00],
  [0x40, 0xFF, 0xFF, 0x00],
  [0x80, 0x80, 0x80, 0x00],
  [0xC0, 0xC0, 0xC0, 0x00],
  [0xFF, 0x80, 0x00, 0x00],
  [0x00, 0x80, 0xFF, 0x00],
  [0x80, 0x00, 0xFF, 0x00],
  [0xFF, 0xFF, 0xFF, 0x00],
];

// Snap v down to a multiple of TILE_SIZE.
function snapTile(v: number): number {
  return v & ~(TILE_SIZE - 1);
}

// X-coordinate where the Solid region ends / PalRle region begins.
export function regionBoundarySolidPalrle(width: number): number {
  return snapTile(Math.floor(width / 3));
}

// X-coordinate where the PalRle region ends / Cdf53 region begins.
export function regionBoundaryPalrleCdf53(width: number): number {
  return snapTile(Math.floor((width * 2) / 3));
}

// Generate the full deterministic codec-mixed test frame as
// width * height * 4 bytes of RGBA8 in scanline order.
//
// Pure function of (width, height): same inputs → identical bytes.
// MUST match the Rust frame_rgba in lossless_golden.rs byte-for-byte.
export function frameRgba(width: number, height: number): Uint8Array {
  const buf = new Uint8Array(width * height * 4);
  const boundaryLeft = regionBoundarySolidPalrle(width);
  const boundaryRight = regionBoundaryPalrleCdf53(width);

  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const off = (y * width + x) * 4;
      let r: number, g: number, b: number, a: number;
      if (x < boundaryLeft) {
        // Solid region.
        const bx = Math.floor(x / SOLID_BLOCK);
        const by = Math.floor(y / SOLID_BLOCK);
        const idx = (bx & 1) | ((by & 1) << 1);
        [r, g, b, a] = SOLID_PALETTE[idx];
      } else if (x < boundaryRight) {
        // PalRle region.
        const tx = Math.floor(x / TILE_SIZE);
        const ty = Math.floor(y / TILE_SIZE);
        // u32 wrapping math: keep operands in u32 range with `>>> 0`.
        const base = ((Math.imul(tx, 5) + Math.imul(ty, 3)) >>> 0) % 16;
        const palIdx = [
          base % 16,
          (base + 4) % 16,
          (base + 8) % 16,
          (base + 12) % 16,
        ];
        const subX = Math.floor((x % TILE_SIZE) / (TILE_SIZE / 2));
        const subY = Math.floor((y % TILE_SIZE) / (TILE_SIZE / 2));
        const which = subX | (subY << 1);
        [r, g, b, a] = PALRLE_PALETTE[palIdx[which]];
      } else {
        // Cdf53 region — high-entropy gradient.
        // u8 truncation matches `as u8` cast in Rust.
        r = (Math.imul(x, 17) + Math.imul(y, 31)) & 0xFF;
        g = (Math.imul(x, 13) + Math.imul(y, 7)) & 0xFF;
        b = (Math.imul(x, 29) + Math.imul(y, 11)) & 0xFF;
        a = 0x00;
      }
      buf[off] = r;
      buf[off + 1] = g;
      buf[off + 2] = b;
      buf[off + 3] = a;
    }
  }
  return buf;
}
