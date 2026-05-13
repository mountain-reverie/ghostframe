import { TILE_SIZE } from './decoder';

export class TileRenderer {
  private ctx: CanvasRenderingContext2D;
  private canvas: HTMLCanvasElement;

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
    this.ctx = canvas.getContext('2d')!;
  }

  resize(width: number, height: number) {
    if (this.canvas.width !== width || this.canvas.height !== height) {
      this.canvas.width = width;
      this.canvas.height = height;
    }
  }

  drawRawTile(tileX: number, tileY: number, bgraData: Uint8Array) {
    const px = tileX * TILE_SIZE;
    const py = tileY * TILE_SIZE;
    // Convert BGRA to RGBA for ImageData
    const rgba = new Uint8ClampedArray(TILE_SIZE * TILE_SIZE * 4);
    for (let i = 0; i < TILE_SIZE * TILE_SIZE; i++) {
      const s = i * 4;
      rgba[s]     = bgraData[s + 2]; // R <- B channel
      rgba[s + 1] = bgraData[s + 1]; // G
      rgba[s + 2] = bgraData[s];     // B <- R channel
      rgba[s + 3] = 255;              // A — force opaque (X11/DRM use BGRX, not BGRA)
    }
    const imageData = new ImageData(rgba, TILE_SIZE, TILE_SIZE);
    this.ctx.putImageData(imageData, px, py);
  }

  /** Draw a full-frame VideoFrame covering the entire canvas. */
  drawFullFrame(frame: VideoFrame) {
    const w = frame.displayWidth;
    const h = frame.displayHeight;
    if (this.canvas.width !== w || this.canvas.height !== h) {
      this.resize(w, h);
    }
    this.ctx.drawImage(frame, 0, 0);
  }

  drawVideoFrame(tileX: number, tileY: number, frame: VideoFrame) {
    const px = tileX * TILE_SIZE;
    const py = tileY * TILE_SIZE;
    // The encoded frame may be larger than TILE_SIZE (e.g. 128x128 when
    // VA-API pads for hardware minimum constraints).  Crop the top-left
    // TILE_SIZE x TILE_SIZE region which contains the actual tile content.
    this.ctx.drawImage(
      frame,
      0, 0, TILE_SIZE, TILE_SIZE,         // source rect (crop)
      px, py, TILE_SIZE, TILE_SIZE,        // destination rect
    );
  }

  /**
   * Draw a Solid-codec tile: fill a TILE_SIZE × TILE_SIZE region with one
   * BGRA color. The payload is 4 bytes [B, G, R, A] matching the server's
   * `encode_solid` output.
   */
  drawSolidTile(tileX: number, tileY: number, bgra: Uint8Array) {
    const px = tileX * TILE_SIZE;
    const py = tileY * TILE_SIZE;
    const b = bgra[0];
    const g = bgra[1];
    const r = bgra[2];
    // Force opaque per project convention — X11/DRM use BGRX, alpha
    // can be 0 which destroys color in putImageData; same applies here.
    this.ctx.fillStyle = `rgb(${r}, ${g}, ${b})`;
    this.ctx.fillRect(px, py, TILE_SIZE, TILE_SIZE);
  }

  /**
   * Draw a PalRle-codec tile. The decoded pixel data is BGRA; swap to RGBA
   * for ImageData per project convention.
   */
  drawPalRleTile(tileX: number, tileY: number, bgra: Uint8ClampedArray) {
    const px = tileX * TILE_SIZE;
    const py = tileY * TILE_SIZE;
    // ImageData expects RGBA in browsers; decoder emits BGRA. Swap per pixel.
    const rgba = new Uint8ClampedArray(bgra.length);
    for (let i = 0; i < bgra.length; i += 4) {
      rgba[i]     = bgra[i + 2]; // R from B-slot
      rgba[i + 1] = bgra[i + 1]; // G
      rgba[i + 2] = bgra[i];     // B from R-slot
      rgba[i + 3] = bgra[i + 3]; // A
    }
    const imageData = new ImageData(rgba, TILE_SIZE, TILE_SIZE);
    this.ctx.putImageData(imageData, px, py);
  }
}
