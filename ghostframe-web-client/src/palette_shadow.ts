/**
 * CPU-side mirror of which palette slots have been delivered (i.e., the
 * client has uploaded their colors to the GPU `palette_atlas` buffer).
 *
 * Replaces the M3.2a `PaletteCache` JS Map. The actual palette bytes live
 * in the GPU atlas; we only need to know "is slot N populated?" to validate
 * thin PalRLE payloads on the CPU before dispatching to the GPU.
 */
export class PaletteShadow {
  // counts[i] === 0 means slot i is not populated; > 0 means populated with that count.
  // Using a sentinel 0 (vs separate boolean array) keeps the data structure flat.
  private counts = new Uint8Array(256);

  has(paletteId: number): boolean {
    return this.counts[paletteId] !== 0;
  }

  count(paletteId: number): number {
    return this.counts[paletteId];
  }

  put(paletteId: number, count: number): void {
    if (count < 1 || count > 16) {
      throw new Error(`PaletteShadow.put: count ${count} out of range 1..16`);
    }
    this.counts[paletteId] = count;
  }

  clear(): void {
    this.counts.fill(0);
  }
}
