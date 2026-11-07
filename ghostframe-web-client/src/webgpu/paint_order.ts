// Per-tile paint ordering for one rAF tick.
//
// The renderer used to paint in a fixed codec order (PalRle, Cdf53,
// Solid, H.264) whatever the server frame each entry came from. When two
// server frames land in one tick, an older Solid or H.264 draw could then sit
// over a newer CDF53 tile, and an older PalRle over a newer Solid one. Every
// queue entry now carries the `frame_seq` of the frame it came from, and
// this module decides, per tile, which entries may still be painted.
//
// Rules (all comparisons are serial-number arithmetic, so a u32 wrap is safe):
//  - Stateless entries (Solid, PalRle) are dropped when any entry for the
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
// Across ticks (the renderer keeps what it painted, see `PriorPaint`):
//  - Any entry (stateless or CDF53) older than the newest frame_seq already
//    painted on its tile, or than the last H.264 frame drawn, is dropped. The
//    server has superseded that content; a CDF53 pass is dropped too because
//    what it would integrate into is no longer on screen. Equal never drops
//    (refinement passes share their generation's frame_seq).
//  - An H.264 frame is dropped when it is older than a previous H.264 frame
//    or than any tile painted in an earlier tick: it covers every tile, so
//    drawing it would undo the newer paint. Decoder output is asynchronous,
//    which is how it can arrive that late.
//
// Known limits:
//  - An H.264 frame with CDF53 entries both older and newer than it in ONE tick
//    cannot be honoured: the lift is a single dispatch, so the older lift lands
//    over the H.264 frame. One mode switch plus a late CDF53 retransmit of
//    another tile is enough to hit it.

export interface SeqTile {
  tileX: number;
  tileY: number;
  frameSeq: number;
}

/** The wire's `frame_seq` is 31 bits: bit 31 is the tile-datagram flag. */
const SEQ_MASK = 0x7fffffff;

/**
 * True when `a` is strictly newer than `b`: RFC 1982 serial arithmetic on the
 * 31-bit wire `frame_seq`, as the core compares it (`frame_seq_is_newer`).
 * A tile left unpainted for 2^30 frames (about 207 days at 60 fps) would
 * compare wrongly; that is accepted.
 */
export function seqNewer(a: number, b: number): boolean {
  const forward = (a - b) & SEQ_MASK;
  return forward !== 0 && forward < 0x40000000;
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

/** What earlier ticks painted. Lookups are O(1): flat typed arrays. */
export interface PriorPaint {
  /** Newest frame_seq painted on the tile, or undefined if nothing was. */
  tile(tileX: number, tileY: number): number | undefined;
  /** frame_seq of the newest H.264 frame drawn. */
  h264Floor: number | undefined;
  /** Newest frame_seq painted on any tile. */
  maxTile: number | undefined;
}

export interface PaintPlanInput<S extends SeqTile, P extends SeqTile, C extends SeqTile = SeqTile> {
  solid: readonly S[];
  palRle: readonly P[];
  cdf53: readonly C[];
  /** frame_seq of each queued H.264 frame, in arrival order. */
  h264Seqs: readonly number[];
  /** Omit to order within the tick only. */
  prior?: PriorPaint;
}

export interface PaintPlan<S, P, C = SeqTile> {
  solid: S[];
  palRle: P[];
  /** CDF53 entries to integrate; fewer than queued only via `prior`. */
  cdf53: C[];
  /** Index-aligned with `h264Seqs`: whether that frame is drawn. */
  h264Keep: boolean[];
  /** Draw the H.264 frames before the CDF53 lift instead of after it. */
  h264BeforeCdf53: boolean;
}

export function planPaint<S extends SeqTile, P extends SeqTile, C extends SeqTile = SeqTile>(
  input: PaintPlanInput<S, P, C>,
): PaintPlan<S, P, C> {
  const prior = input.prior;
  const olderThanPrior = (e: SeqTile): boolean => {
    if (!prior) return false;
    const t = prior.tile(e.tileX, e.tileY);
    if (t !== undefined && seqNewer(t, e.frameSeq)) return true;
    return prior.h264Floor !== undefined && seqNewer(prior.h264Floor, e.frameSeq);
  };
  const cdf53Entries = prior ? input.cdf53.filter((e) => !olderThanPrior(e)) : input.cdf53;
  const solidIn = prior ? input.solid.filter((e) => !olderThanPrior(e)) : input.solid;
  const palRleIn = prior ? input.palRle.filter((e) => !olderThanPrior(e)) : input.palRle;

  // An H.264 frame covers every tile: it is stale when a previous frame or any
  // earlier tile paint is newer, or when a newer frame is queued with it.
  let queuedNewest: number | undefined;
  for (const s of input.h264Seqs) {
    if (queuedNewest === undefined || seqNewer(s, queuedNewest)) queuedNewest = s;
  }
  const h264Keep = input.h264Seqs.map((s) => {
    if (queuedNewest !== undefined && seqNewer(queuedNewest, s)) return false;
    if (!prior) return true;
    if (prior.h264Floor !== undefined && seqNewer(prior.h264Floor, s)) return false;
    return !(prior.maxTile !== undefined && seqNewer(prior.maxTile, s));
  });

  const stateless = new Map<number, number>();
  newestByTile(stateless, solidIn);
  newestByTile(stateless, palRleIn);
  const cdf53 = new Map<number, number>();
  newestByTile(cdf53, cdf53Entries);

  let h264Newest: number | undefined;
  input.h264Seqs.forEach((s, i) => {
    if (!h264Keep[i]) return;
    if (h264Newest === undefined || seqNewer(s, h264Newest)) h264Newest = s;
  });

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
    for (const e of cdf53Entries) {
      if (seqNewer(e.frameSeq, h264Newest)) {
        h264BeforeCdf53 = true;
        break;
      }
    }
  }

  return {
    solid: solidIn.filter(keep),
    palRle: palRleIn.filter(keep),
    cdf53: [...cdf53Entries],
    h264Keep,
    h264BeforeCdf53,
  };
}
