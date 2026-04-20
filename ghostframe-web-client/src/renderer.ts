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
}
