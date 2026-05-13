// PaletteCache — server-driven palette storage for PalRle decode.
//
// The server's persistent PaletteTable carries lifecycle (ref counting, LRU,
// delivered tracking). The client side just memorizes whatever bytes the
// server has told us to associate with each palette_id. On transport
// disconnect/reconnect we clear; server's on_session_reset ensures next
// emissions bundle.

export type PaletteId = number; // 0..255
export type Bgra = readonly [number, number, number, number];

export class PaletteCache {
    private entries = new Map<PaletteId, Bgra[]>();

    upsert(id: PaletteId, colors: Bgra[]): void {
        this.entries.set(id, colors);
    }

    get(id: PaletteId): Bgra[] | undefined {
        return this.entries.get(id);
    }

    clear(): void {
        this.entries.clear();
    }

    /** Test-only — number of cached palettes. */
    size(): number {
        return this.entries.size;
    }
}
