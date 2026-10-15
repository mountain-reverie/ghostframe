// Per-tile paint ordering for one rAF tick.
//
// The renderer used to paint in a fixed codec order (Raw, PalRle, Cdf53,
// Solid, H.264) whatever the server frame each entry came from. When two
// server frames land in one tick, an older Solid or H.264 draw could then sit
// over a newer CDF53 tile, and an older PalRle over a newer Raw one. Every
// queue entry now carries the `frame_seq` of the frame it came from, and
// this module decides, per tile, which entries may still be painted.
//
// Rules (all comparisons are serial-number arithmetic, so a u32 wrap is safe):
//  - Stateless entries (Raw, Solid, PalRle) are dropped when any entry for the
//    same tile from a newer frame exists in the tick. They only overwrite
//    pixels, so dropping one loses nothing.
//  - CDF53 entries are never dropped: they integrate into per-tile coefficient
//    state that later passes build on. They are ordered instead: the lift is
//    encoded BEFORE every stateless draw, and a stateless entry older than a
//    CDF53 entry for its tile is dropped, so the newer one is what remains.
//  - An H.264 frame covers the whole screen. A stateless entry older than the
//    newest H.264 frame is dropped. The frame itself is drawn after the CDF53
//    lift (stale CDF53 under it) unless some CDF53 entry is newer than it, in
//    which case it is drawn before the lift.
//
// Known limits:
//  - An H.264 frame with CDF53 entries both older and newer than it in ONE tick
//    cannot be honoured: the lift is a single dispatch, so the older lift lands
//    over the H.264 frame. One mode switch plus a late CDF53 retransmit of
//    another tile is enough to hit it.
//  - Ordering is resolved within a tick only. H.264 output is asynchronous, so
//    a frame can arrive a tick after newer tiles were painted, and a late CDF53
//    retransmit keeps its old frame_seq. Closing that needs a last-painted seq
//    kept across ticks.

export interface SeqTile {
  tileX: number;
  tileY: number;
  frameSeq: number;
}

/** True when `a` is strictly newer than `b` (RFC 1982 serial arithmetic). */
export function seqNewer(a: number, b: number): boolean {
  return ((a - b) | 0) > 0;
}

function tileKey(t: { tileX: number; tileY: number }): number {
  // Tile coordinates are u16 on the wire.
  return t.tileY * 65536 + t.tileX;
}

function newestByTile(into: Map<number, number>, entries: readonly SeqTile[]): void {
  for (const e of entries) {
    const k = tileKey(e);
    const cur = into.get(k);
    if (cur === undefined || seqNewer(e.frameSeq, cur)) into.set(k, e.frameSeq);
  }
}

export interface PaintPlanInput<R extends SeqTile, S extends SeqTile, P extends SeqTile> {
  raw: readonly R[];
  solid: readonly S[];
  palRle: readonly P[];
  cdf53: readonly SeqTile[];
  /** frame_seq of each queued H.264 frame, in arrival order. */
  h264Seqs: readonly number[];
}

export interface PaintPlan<R, S, P> {
  raw: R[];
  solid: S[];
  palRle: P[];
  /** Draw the H.264 frames before the CDF53 lift instead of after it. */
  h264BeforeCdf53: boolean;
}

export function planPaint<R extends SeqTile, S extends SeqTile, P extends SeqTile>(
  input: PaintPlanInput<R, S, P>,
): PaintPlan<R, S, P> {
  const stateless = new Map<number, number>();
  newestByTile(stateless, input.raw);
  newestByTile(stateless, input.solid);
  newestByTile(stateless, input.palRle);
  const cdf53 = new Map<number, number>();
  newestByTile(cdf53, input.cdf53);

  let h264Newest: number | undefined;
  for (const s of input.h264Seqs) {
    if (h264Newest === undefined || seqNewer(s, h264Newest)) h264Newest = s;
  }

  const keep = <T extends SeqTile>(e: T): boolean => {
    const k = tileKey(e);
    const sl = stateless.get(k);
    if (sl !== undefined && seqNewer(sl, e.frameSeq)) return false;
    const c = cdf53.get(k);
    if (c !== undefined && seqNewer(c, e.frameSeq)) return false;
    if (h264Newest !== undefined && seqNewer(h264Newest, e.frameSeq)) return false;
    return true;
  };

  let h264BeforeCdf53 = false;
  if (h264Newest !== undefined) {
    for (const e of input.cdf53) {
      if (seqNewer(e.frameSeq, h264Newest)) {
        h264BeforeCdf53 = true;
        break;
      }
    }
  }

  return {
    raw: input.raw.filter(keep),
    solid: input.solid.filter(keep),
    palRle: input.palRle.filter(keep),
    h264BeforeCdf53,
  };
}
