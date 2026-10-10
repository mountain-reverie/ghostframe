import { describe, it, expect } from 'vitest';
import { MAX_FRAME_DIMENSION, checkFrameDimensions } from '../src/frame_limits';
import { protocol } from '../src/protocol_constants';

describe('checkFrameDimensions', () => {
  it('accepts ordinary and boundary sizes', () => {
    for (const [w, h] of [[640, 480], [1920, 1080], [3840, 2160], [MAX_FRAME_DIMENSION, 1], [1, MAX_FRAME_DIMENSION]]) {
      expect(checkFrameDimensions(w, h).ok, `${w}x${h}`).toBe(true);
    }
  });

  it('rejects oversize, empty and non-integer sizes', () => {
    for (const [w, h] of [
      [MAX_FRAME_DIMENSION + 1, 480],
      [640, MAX_FRAME_DIMENSION + 1],
      [0xffffffff, 0xffffffff],
      [0, 480],
      [640, 0],
      [-1, 480],
      [NaN, 480],
      [640.5, 480],
      [MAX_FRAME_DIMENSION, 4320], // fits per-axis but needs > 16383 tiles
    ]) {
      expect(checkFrameDimensions(w, h).ok, `${w}x${h}`).toBe(false);
    }
  });

  it('the limit covers what the u8 tile grid can address, and not much more', () => {
    // Tile coordinates are u8 with two values reserved as sentinels.
    const addressable = (254 + 1) * protocol().tileSize; // grid of 255 columns at most
    expect(MAX_FRAME_DIMENSION).toBeGreaterThanOrEqual(addressable - protocol().tileSize);
    expect(MAX_FRAME_DIMENSION).toBeLessThanOrEqual(addressable + protocol().tileSize * 8);
  });
});
