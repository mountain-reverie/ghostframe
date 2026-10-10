import { protocol } from './protocol_constants.js';

// Belt-and-braces bound on the framebuffer size the server may ask for.
//
// A FrameDimensions event drives `renderer.resize`, which allocates GPU
// buffers proportional to width*height (several per-tile buffers and the
// framebuffer texture). The authoritative limit belongs in the protocol
// core -- WS-A2 is adding a frame-dimension limit constant to
// ghostframe-client-core, and this value should be replaced by that
// constant's value (exported through wasm) once it lands. Until then 8192
// matches what the tile coordinate space can address: tile x/y are u8 with
// 0xFE/0xFF reserved as sentinels, so at most 254 tiles * 32 px = 8128 px.
export const MAX_FRAME_DIMENSION = 8192;

/** The L1 inverse launches 4 workgroups per tile and WebGPU's default
 * dispatch limit is 65535, so more than 16383 tiles invalidates the frame. */
export const MAX_TILES = 16383;

export type DimensionVerdict = { ok: true } | { ok: false; reason: string };

/** Whether a server-announced framebuffer size is safe to allocate. */
export function checkFrameDimensions(width: number, height: number): DimensionVerdict {
  if (!Number.isInteger(width) || !Number.isInteger(height)) {
    return { ok: false, reason: `non-integer size ${width}x${height}` };
  }
  if (width <= 0 || height <= 0) {
    return { ok: false, reason: `empty size ${width}x${height}` };
  }
  if (width > MAX_FRAME_DIMENSION || height > MAX_FRAME_DIMENSION) {
    return {
      ok: false,
      reason: `size ${width}x${height} exceeds the ${MAX_FRAME_DIMENSION}px limit`,
    };
  }
  const ts = protocol().tileSize;
  if (Math.ceil(width / ts) * Math.ceil(height / ts) > MAX_TILES) {
    return { ok: false, reason: `size ${width}x${height} needs more than ${MAX_TILES} tiles` };
  }
  return { ok: true };
}
