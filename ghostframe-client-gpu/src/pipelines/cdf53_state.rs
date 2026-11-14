// SPDX-License-Identifier: MIT OR Apache-2.0
//! Host-side bookkeeping for [`crate::pipelines::cdf53_grid::Cdf53Grid`]: what
//! each tile has received, what is on screen, and which tiles to reconstruct in
//! the next flush.
//!
//! The grid itself is stateless: it draws whatever tiles it is told to, from
//! the planes in its store. Everything that makes a draw *correct* lives here:
//!
//! - **Which prefix.** A tile is drawn from the contiguous prefix of resolved
//!   passes, by the rule the CPU decoder uses
//!   ([`ghostframe_client_core::cdf53_tile_state::resolved_prefix`]): nothing
//!   until pass 0 has arrived, then every pass up to the first one that is
//!   marked present and has not arrived. The `differential` tests hold the two
//!   to each other.
//! - **Generations.** A pass of a new generation resets the tile: older
//!   planes, not yet uploaded, are dropped with it, and the mask handed to the
//!   GPU names only the new generation's planes, so the stale ones left in the
//!   store are never read.
//! - **Not drawing what is not worth drawing.** A tile is listed at most once
//!   per flush, at its newest prefix; not at all if that prefix is the one on
//!   screen, or if it has no magnitude plane (pass 0 holds only signs, and a
//!   sign-only prefix reconstructs as black, which the CPU decoder also
//!   refuses to draw).
//! - **A time budget.** One flush reconstructs about [`FLUSH_BUDGET_US`] worth
//!   of tiles (and never more than [`MAX_TILES_PER_FLUSH`], so no single submit
//!   nears panfrost's ~500 ms job watchdog); the rest stay pending, are listed
//!   by a later flush at whatever prefix they have reached by then, and nothing
//!   is lost. Complete tiles go first, then the oldest.
//! - **Throttled refinement.** A tile that was drawn at a partial prefix last
//!   flush and is still receiving passes waits one flush rather than being
//!   redrawn at every pass: at most one partial draw per tile per two flushes.
//! - **Paint order.** A Solid / PalRle / RGBA tile newer than a tile's CDF 5/3
//!   state supersedes it ([`Cdf53GridState::stateless`]): the pending draw is
//!   dropped, and later passes older than the stateless tile are refused.
//!
//! - **Fairness.** Complete tiles go first, but a tile that has waited
//!   [`MAX_WAIT_FLUSHES`] outranks everything, and a pending tile hit by a new
//!   generation keeps its place in the queue.
//! - **H.264.** [`Cdf53GridState::supersede_all`]: a decoded H.264 frame covers
//!   the screen, so every pending draw (older, and about to land on top of it)
//!   is cancelled and older passes are refused.
//!
//! # What the host cannot know
//!
//! The host decides to list a tile from which planes arrived, not from their
//! contents; the shader decides, from the coefficients, whether a partial
//! prefix has anything to draw (`any_magnitude`). They agree because the
//! server encodes sparsely: a plane it sends has a set bit, and a magnitude
//! plane that is all zero is simply not sent. A dense-encoded tile whose
//! early magnitude planes are all zero is listed (so it counts towards the
//! paint-order ledger and may hide an older Solid in its batch) and then not
//! drawn by the shader. That is a latent mismatch, left as it is; the cure
//! would be a non-zero scan of each plane as it arrives.
//!
//! Planes are held here, as the `Vec` the event brought, until their tile is
//! listed, and are uploaded then: a plane for a tile that is deferred, or never
//! worth drawing, costs no upload, and no plane is uploaded twice.

use ghostframe_client_core::cdf53_tile_state::resolved_prefix;

/// How many microseconds of GPU reconstruction one flush may be asked for. A
/// frame is 16 ms; this leaves room for the rest of the flush and the publish.
pub const FLUSH_BUDGET_US: u32 = 7000;
/// Hard cap on tiles in one submit, whatever the cost model says.
pub const MAX_TILES_PER_FLUSH: usize = 1000;

/// Estimated GPU time to reconstruct one tile of prefix `k`, in microseconds:
/// a line through the measurements on a Mali-T860 at 2040 tiles per dispatch
/// (`grid_cost`: k = 1 17, k = 6 38, k = 14 50), rounded up. This is the
/// *shape* of the cost; the level comes from
/// [`Cdf53GridState::set_cost_per_tile_k14`], which a start-up measurement
/// feeds (a desktop GPU is an order of magnitude faster than this Mali).
pub fn tile_cost_us(k: u32) -> u32 {
    20 + (k * 5).div_ceil(2)
}

use crate::paint_order::seq_newer;

/// A pending tile that has waited this many flushes (about 100 ms at 60 Hz) is
/// listed ahead of everything that has not, whatever its prefix: "complete
/// tiles first" is a preference, not a licence to starve a partial tile.
pub const MAX_WAIT_FLUSHES: u64 = 6;

#[derive(Default)]
struct TileState {
    /// Anything received for the tile since it was last reset.
    live: bool,
    generation: u8,
    /// Bit `p` set when pass `p` of this generation has arrived.
    received: u16,
    /// Pass 0's `present_passes`.
    present: Option<u16>,
    /// The prefix length last handed to the GPU for this generation.
    drawn_k: Option<u8>,
    /// Newest `frame_seq` of any pass of this generation.
    newest_seq: u32,
    /// Newest `frame_seq` of a Solid / PalRle / RGBA tile for this tile.
    stateless_seq: Option<u32>,
    pending: bool,
    /// Whether the tile's index is in the state's `dirty` list, so that a
    /// cancelled and re-armed tile is not in it twice (and so listed twice).
    queued: bool,
    /// Arrival order of the pending state, for oldest-first.
    pending_since: u64,
    /// The `floor_epoch` at which this tile accepted a pass; see the state's field.
    floor_waived: u64,
    /// `flushes` when the tile became pending, for the age cap.
    pending_flush: u64,
    /// The flush that last drew this tile at a partial prefix.
    partial_drawn_at: Option<u64>,
    /// Planes that arrived and have not been uploaded.
    planes: [Option<Vec<u8>>; 14],
}

impl TileState {
    fn reset(&mut self) {
        self.live = false;
        self.received = 0;
        self.present = None;
        self.drawn_k = None;
        self.pending = false;
        self.partial_drawn_at = None;
        self.planes = Default::default();
        // `queued` is the list's business, not the tile's.
    }
}

/// One tile for the GPU: what to upload and how to draw it.
#[derive(Debug, PartialEq, Eq)]
pub struct ListedTile {
    pub tile_x: u8,
    pub tile_y: u8,
    /// Row-major index in the grid.
    pub index: u32,
    /// Planes of this generation, bit `p` = pass `p`; only passes below `k`.
    pub mask: u32,
    pub k: u32,
    /// The tile's newest `frame_seq`, for the paint-order ledger.
    pub frame_seq: u32,
    /// Planes to upload now, `(pass, 384 bytes)`.
    pub planes: Vec<(u8, Vec<u8>)>,
}

/// What [`Cdf53GridState::plan`] decided, counted since the last
/// [`Cdf53GridState::take_stats`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cdf53Stats {
    /// Passes fed in.
    pub passes: u64,
    /// Tiles handed to the GPU.
    pub listed: u64,
    /// Tile-flushes a ready tile waited because the budget was spent.
    pub deferred: u64,
    /// Tile-flushes a partial tile waited out its throttle.
    pub throttled: u64,
    /// Pending tiles with no magnitude plane yet.
    pub skipped_no_magnitude: u64,
    /// Pending tiles whose prefix is the one already drawn.
    pub skipped_same_k: u64,
    /// Pending tiles still missing pass 0.
    pub skipped_no_pass0: u64,
    /// Passes refused as older than a newer Solid / PalRle / RGBA tile.
    pub dropped_superseded: u64,
    /// Passes for a tile outside the grid.
    pub dropped_off_grid: u64,
    /// Pending draws cancelled by a newer stateless tile.
    pub cancelled_superseded: u64,
}

pub struct Cdf53GridState {
    cols: u32,
    rows: u32,
    tiles: Vec<TileState>,
    /// Indices that may be pending; entries for tiles no longer pending are
    /// skipped when read.
    dirty: Vec<u32>,
    arrivals: u64,
    flushes: u64,
    /// Passes older than this are refused (an H.264 frame covered the screen)...
    floor_seq: Option<u32>,
    /// ...except on a tile that has since accepted a pass (`floor_waived`
    /// equal to this), so a restarted `frame_seq` cannot lock a tile out.
    floor_epoch: u64,
    /// Multiplies [`tile_cost_us`]; 1.0 is the Mali-T860 the model was fitted on.
    cost_scale: f64,
    budget_us: u32,
    max_tiles: usize,
    stats: Cdf53Stats,
}

impl Default for Cdf53GridState {
    fn default() -> Self {
        Self::new()
    }
}

impl Cdf53GridState {
    pub fn new() -> Self {
        Self::with_budget(FLUSH_BUDGET_US, MAX_TILES_PER_FLUSH)
    }

    pub fn with_budget(budget_us: u32, max_tiles: usize) -> Self {
        Cdf53GridState {
            cols: 0,
            rows: 0,
            tiles: Vec::new(),
            dirty: Vec::new(),
            arrivals: 0,
            flushes: 0,
            floor_seq: None,
            floor_epoch: 1,
            cost_scale: 1.0,
            budget_us,
            max_tiles: max_tiles.max(1),
            stats: Cdf53Stats::default(),
        }
    }

    /// Set the cost level from a measurement: GPU microseconds for one complete
    /// (k = 14) tile on this device. Without it the model is the Mali-T860's.
    pub fn set_cost_per_tile_k14(&mut self, us: f64) {
        self.cost_scale = (us / tile_cost_us(14) as f64).clamp(0.01, 100.0);
    }

    fn cost_us(&self, k: u32) -> u32 {
        (tile_cost_us(k) as f64 * self.cost_scale).ceil().max(1.0) as u32
    }

    /// Size for a `cols` x `rows` grid. The same shape keeps everything (a
    /// repeated `FrameDimensions` must not lose a tile's progress); a new shape
    /// forgets every tile, because the GPU's plane store is re-made with it and
    /// its planes are gone.
    pub fn resize(&mut self, cols: u32, rows: u32) {
        if self.cols == cols && self.rows == rows && !self.tiles.is_empty() {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.floor_seq = None;
        self.tiles = std::iter::repeat_with(TileState::default)
            .take(cols as usize * rows as usize)
            .collect();
        self.dirty.clear();
    }

    fn index(&self, tile_x: u8, tile_y: u8) -> Option<usize> {
        ((tile_x as u32) < self.cols && (tile_y as u32) < self.rows)
            .then(|| tile_y as usize * self.cols as usize + tile_x as usize)
    }

    /// Feed one pass of a tile, as `TileData::Cdf53` carries it. `new_generation`
    /// is the core's word that this pass starts a new generation even if the
    /// 4-bit number is unchanged.
    #[allow(clippy::too_many_arguments)]
    pub fn pass(
        &mut self,
        tile_x: u8,
        tile_y: u8,
        frame_seq: u32,
        generation: u8,
        new_generation: bool,
        pass_idx: u8,
        present_passes: Option<u16>,
        bit_planes: Vec<u8>,
    ) {
        self.stats.passes += 1;
        let Some(i) = self.index(tile_x, tile_y) else {
            self.stats.dropped_off_grid += 1;
            return;
        };
        if pass_idx >= 14 || bit_planes.len() != 384 {
            return;
        }
        let t = &mut self.tiles[i];
        // A pass the core says starts a new generation is never stale: the
        // core has already judged it against the tile's coverage, and a server
        // that restarted its `frame_seq` must not be locked out by an old
        // stateless tile or floor.
        if !new_generation
            && (t.stateless_seq.is_some_and(|s| seq_newer(s, frame_seq))
                || (t.floor_waived != self.floor_epoch
                    && self.floor_seq.is_some_and(|s| seq_newer(s, frame_seq))))
        {
            // Older than a Solid / PalRle / RGBA tile (or an H.264 frame)
            // already drawn over it.
            self.stats.dropped_superseded += 1;
            return;
        }
        // Accepted: whatever this tile's last stateless draw was, this pass is
        // not older than it, so the record has done its job.
        t.stateless_seq = None;
        t.floor_waived = self.floor_epoch;
        if !t.live || new_generation || t.generation != generation {
            // Drops this tile's older pending planes with it -- but not its
            // place in the queue: a tile hit again while waiting for the GPU
            // keeps its age, or sustained churn would send it to the back
            // every time and the bottom of the screen would never update.
            let (was_pending, since, flush) = (t.pending, t.pending_since, t.pending_flush);
            t.reset();
            if was_pending {
                t.pending = true;
                t.pending_since = since;
                t.pending_flush = flush;
            }
            t.live = true;
            t.generation = generation;
            t.newest_seq = frame_seq;
        } else if seq_newer(frame_seq, t.newest_seq) {
            t.newest_seq = frame_seq;
        }
        // Newest wins: a pass that is still waiting for upload is replaced.
        t.planes[pass_idx as usize] = Some(bit_planes);
        t.received |= 1 << pass_idx;
        if pass_idx == 0 {
            if let Some(p) = present_passes {
                t.present = Some(p);
            }
        }
        if !t.pending {
            t.pending = true;
            t.pending_since = self.arrivals;
            t.pending_flush = self.flushes;
            self.arrivals += 1;
        }
        if !t.queued {
            t.queued = true;
            self.dirty.push(i as u32);
        }
    }

    /// A Solid / PalRle / RGBA tile for `(tile_x, tile_y)` from `frame_seq`
    /// was applied. If it is newer than the tile's CDF 5/3 state, a draw still
    /// waiting would land on top of it with older content: cancel it, and
    /// refuse later passes older than it. (A pass of the same generation that is
    /// newer again re-arms the tile and draws its whole prefix, as the CPU
    /// decoder would.)
    pub fn stateless(&mut self, tile_x: u8, tile_y: u8, frame_seq: u32) {
        let Some(i) = self.index(tile_x, tile_y) else {
            return;
        };
        let t = &mut self.tiles[i];
        if t.stateless_seq.is_none_or(|s| seq_newer(frame_seq, s)) {
            t.stateless_seq = Some(frame_seq);
        }
        if t.live && t.pending && seq_newer(frame_seq, t.newest_seq) {
            t.pending = false;
            self.stats.cancelled_superseded += 1;
        }
    }

    /// An H.264 frame from `frame_seq` covers the screen: every pending draw
    /// not newer than it would land on top of it, so cancel those, and refuse
    /// passes older than it. A pending tile whose newest pass is newer than
    /// the frame stays: a late picture (the decoder delays its output) must not
    /// cancel the tiles of a later frame. (Tiles the grid already drew are under the
    /// frame; the flush that preceded the blit saw to that.)
    pub fn supersede_all(&mut self, frame_seq: u32) {
        for &i in &self.dirty {
            let t = &mut self.tiles[i as usize];
            if t.pending && !seq_newer(t.newest_seq, frame_seq) {
                t.pending = false;
                self.stats.cancelled_superseded += 1;
            }
        }
        if self.floor_seq.is_none_or(|f| seq_newer(frame_seq, f)) {
            self.floor_seq = Some(frame_seq);
        }
        self.floor_epoch += 1;
    }

    /// Whether tiles are still waiting for a flush that has not listed them.
    /// Only meaningful after [`Self::plan`].
    pub fn has_pending(&self) -> bool {
        self.dirty.iter().any(|&i| self.tiles[i as usize].pending)
    }

    /// Decide this flush's tiles. Each pending tile is listed at most once,
    /// at its newest prefix; a tile that is not listed is either dropped from
    /// the pending set (nothing to draw) or stays in it (deferred).
    pub fn plan(&mut self) -> Vec<ListedTile> {
        self.flushes += 1;
        let now = self.flushes;
        // (index, k, pending_since, waited too long)
        let mut ready: Vec<(u32, u32, u64, bool)> = Vec::new();
        let mut carry: Vec<u32> = Vec::new();
        let mut seen_stale = std::mem::take(&mut self.dirty);
        for &i in &seen_stale {
            let t = &mut self.tiles[i as usize];
            t.queued = false;
            if !t.pending {
                continue;
            }
            let k = resolved_prefix(t.received, t.present) as u32;
            if k == 0 {
                t.pending = false;
                self.stats.skipped_no_pass0 += 1;
                continue;
            }
            if t.drawn_k == Some(k as u8) {
                t.pending = false;
                self.stats.skipped_same_k += 1;
                continue;
            }
            let prefix_mask = (1u32 << k) - 1;
            if k < 14 && (t.received as u32 & prefix_mask & !1) == 0 {
                t.pending = false;
                self.stats.skipped_no_magnitude += 1;
                continue;
            }
            if k < 14 && t.partial_drawn_at.is_some_and(|f| f + 1 == now) {
                self.stats.throttled += 1;
                carry.push(i);
                continue;
            }
            let aged = now.saturating_sub(t.pending_flush) >= MAX_WAIT_FLUSHES;
            ready.push((i, k, t.pending_since, aged));
        }
        seen_stale.clear();
        self.dirty = seen_stale;

        // Tiles that have waited too long, then complete tiles, then the oldest.
        ready.sort_by_key(|&(_, k, since, aged)| (!aged, k < 14, since));
        let mut spent = 0u32;
        let mut listed = Vec::new();
        for (n, &(i, k, ..)) in ready.iter().enumerate() {
            let cost = self.cost_us(k);
            let over = spent.saturating_add(cost) > self.budget_us || n >= self.max_tiles;
            // Always make progress: at least one tile per flush.
            if over && n > 0 {
                self.stats.deferred += (ready.len() - n) as u64;
                carry.extend(ready[n..].iter().map(|&(i, ..)| i));
                break;
            }
            spent += cost;
            listed.push(self.list(i, k, now));
        }
        for &i in &carry {
            self.tiles[i as usize].queued = true;
        }
        self.dirty.extend(carry);
        self.stats.listed += listed.len() as u64;
        listed
    }

    fn list(&mut self, i: u32, k: u32, now: u64) -> ListedTile {
        let (tile_x, tile_y) = ((i % self.cols) as u8, (i / self.cols) as u8);
        let t = &mut self.tiles[i as usize];
        let prefix_mask = (1u32 << k) - 1;
        let mask = t.received as u32 & prefix_mask;
        // Only planes inside the prefix go up; a later one waits for the tile
        // to be listed at a longer prefix.
        let planes: Vec<(u8, Vec<u8>)> = (0..k as usize)
            .filter_map(|p| t.planes[p].take().map(|v| (p as u8, v)))
            .collect();
        t.drawn_k = Some(k as u8);
        t.pending = false;
        if k < 14 {
            t.partial_drawn_at = Some(now);
        } else {
            t.partial_drawn_at = None;
        }
        ListedTile {
            tile_x,
            tile_y,
            index: i,
            mask,
            k,
            frame_seq: t.newest_seq,
            planes,
        }
    }

    /// Counters since the last call.
    pub fn take_stats(&mut self) -> Cdf53Stats {
        std::mem::take(&mut self.stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostframe_client_core::cdf53_prevalidate::PrevalidatedCdf53;
    use ghostframe_client_core::cdf53_tile_state::Cdf53TileState;
    use ghostframe_protocol::codec::cdf53 as codec;

    fn plane(tag: u8) -> Vec<u8> {
        vec![tag; 384]
    }

    /// Feed pass `p` with a recognisable plane; pass 0 carries `present`.
    fn feed(s: &mut Cdf53GridState, tx: u8, p: u8, present: u16, seq: u32, gen: u8) {
        s.pass(
            tx,
            0,
            seq,
            gen,
            false,
            p,
            (p == 0).then_some(present),
            plane(p + 1),
        );
    }

    fn grid() -> Cdf53GridState {
        let mut s = Cdf53GridState::new();
        s.resize(8, 4);
        s
    }

    #[test]
    fn nothing_is_listed_until_pass_zero_arrives() {
        let mut s = grid();
        // Passes 1..=13 of a tile that will say it has all 14: signs missing.
        for p in 1..14 {
            feed(&mut s, 0, p, 0x3FFF, 1, 0);
        }
        assert!(s.plan().is_empty());
        assert_eq!(s.take_stats().skipped_no_pass0, 1);
        assert!(!s.has_pending());
        // Pass 0 arrives: now the whole prefix resolves at once.
        feed(&mut s, 0, 0, 0x3FFF, 1, 0);
        let l = s.plan();
        assert_eq!(l.len(), 1);
        assert_eq!((l[0].k, l[0].mask), (14, 0x3FFF));
        assert_eq!(l[0].planes.len(), 14);
    }

    #[test]
    fn the_spike_s_max_prev_pass_rule_would_draw_here_and_this_does_not() {
        // Reordered arrival: pass 3 first. `max(prev, pass + 1)` says k = 4;
        // pass 0 is not here, so the tile has no signs and must not be drawn.
        let mut s = grid();
        feed(&mut s, 0, 3, 0x3FFF, 1, 0);
        assert!(s.plan().is_empty());
    }

    #[test]
    fn absent_passes_resolve_from_the_bitmap() {
        let mut s = grid();
        // Passes 0, 2 present; 1 and 3.. are not coming.
        feed(&mut s, 0, 0, 0b101, 1, 0);
        let l = s.plan();
        // Pass 0 only: k = 2 (pass 1 absent, pass 2 missing). No magnitude yet.
        assert!(l.is_empty());
        assert_eq!(s.take_stats().skipped_no_magnitude, 1);
        feed(&mut s, 0, 2, 0b101, 1, 0);
        let l = s.plan();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].k, 14, "everything after pass 2 is known-zero");
        assert_eq!(l[0].mask, 0b101);
    }

    #[test]
    fn a_pass_delivered_twice_before_a_flush_uploads_once_and_the_newest_wins() {
        let mut s = grid();
        s.pass(0, 0, 1, 0, false, 0, Some(0x3FFF), plane(7));
        s.pass(0, 0, 1, 0, false, 0, Some(0x3FFF), plane(9));
        feed(&mut s, 0, 1, 0x3FFF, 1, 0);
        let l = s.plan();
        assert_eq!(l[0].planes.len(), 2);
        assert_eq!(l[0].planes[0], (0, plane(9)));
    }

    #[test]
    fn a_tile_is_listed_once_per_flush_at_its_newest_prefix() {
        let mut s = grid();
        for p in 0..5 {
            feed(&mut s, 0, p, 0x3FFF, 1, 0);
        }
        let l = s.plan();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].k, 5);
    }

    #[test]
    fn the_prefix_on_screen_is_not_drawn_again() {
        let mut s = grid();
        for p in 0..14 {
            feed(&mut s, 0, p, 0x3FFF, 1, 0);
        }
        assert_eq!(s.plan().len(), 1);
        // A retransmitted pass changes nothing on screen.
        feed(&mut s, 0, 6, 0x3FFF, 1, 0);
        assert!(s.plan().is_empty());
        assert_eq!(s.take_stats().skipped_same_k, 1);
    }

    #[test]
    fn a_pass_behind_a_gap_does_not_redraw() {
        let mut s = grid();
        feed(&mut s, 0, 0, 0x3FFF, 1, 0);
        feed(&mut s, 0, 1, 0x3FFF, 1, 0);
        assert_eq!(s.plan().len(), 1); // k = 2
        feed(&mut s, 0, 5, 0x3FFF, 1, 0); // gap at 2..4
        assert!(s.plan().is_empty());
    }

    #[test]
    fn a_new_generation_drops_older_pending_planes_and_the_mask() {
        let mut s = grid();
        for p in 0..4 {
            feed(&mut s, 0, p, 0x3FFF, 1, 0);
        }
        // Not flushed yet: generation 1 starts. Gen 0's four planes must go.
        feed(&mut s, 0, 0, 0x3, 2, 1);
        feed(&mut s, 0, 1, 0x3, 2, 1);
        let l = s.plan();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].k, 14);
        assert_eq!(l[0].mask, 0b11, "only the new generation's planes");
        assert_eq!(l[0].planes.len(), 2);
        assert_eq!(l[0].planes[0].1, plane(1));
        assert_eq!(l[0].frame_seq, 2);
    }

    #[test]
    fn a_new_generation_with_the_same_number_resets_too() {
        let mut s = grid();
        for p in 0..14 {
            feed(&mut s, 0, p, 0x3FFF, 1, 3);
        }
        assert_eq!(s.plan().len(), 1);
        // Same 4-bit number, but the core says it is new.
        s.pass(0, 0, 9, 3, true, 0, Some(0x3), plane(1));
        s.pass(0, 0, 9, 3, false, 1, None, plane(2));
        let l = s.plan();
        assert_eq!(
            l.len(),
            1,
            "drawn_k of the old generation must not suppress"
        );
        assert_eq!(l[0].mask, 0b11);
    }

    fn complete_tiles(s: &mut Cdf53GridState, n: u8) {
        for x in 0..n {
            for p in 0..14 {
                feed(s, x, p, 0x3FFF, 1, 0);
            }
        }
    }

    #[test]
    fn the_budget_defers_complete_tiles_oldest_first_and_loses_none() {
        let per = tile_cost_us(14);
        let mut s = Cdf53GridState::with_budget(per * 3, 1000);
        s.resize(8, 4);
        complete_tiles(&mut s, 8);
        let mut order = Vec::new();
        let mut flushes = 0;
        while s.has_pending() || flushes == 0 {
            let l = s.plan();
            assert!(l.len() <= 3, "{} tiles over a budget of 3", l.len());
            assert!(!l.is_empty(), "a flush with pending tiles must progress");
            order.extend(l.iter().map(|t| t.tile_x));
            flushes += 1;
        }
        assert_eq!(flushes, 3);
        assert_eq!(order, (0..8).collect::<Vec<_>>(), "oldest first, none lost");
        assert_eq!(s.take_stats().deferred, 5 + 2);
    }

    #[test]
    fn complete_tiles_go_before_older_partial_ones() {
        let per = tile_cost_us(14);
        let mut s = Cdf53GridState::with_budget(per, 1000);
        s.resize(8, 4);
        // Tile 0: older, partial (k = 3). Tile 1: newer, complete.
        for p in 0..3 {
            feed(&mut s, 0, p, 0x3FFF, 1, 0);
        }
        for p in 0..14 {
            feed(&mut s, 1, p, 0x3FFF, 1, 0);
        }
        let l = s.plan();
        assert_eq!((l.len(), l[0].tile_x), (1, 1));
        let l = s.plan();
        assert_eq!((l.len(), l[0].tile_x, l[0].k), (1, 0, 3));
    }

    #[test]
    fn a_deferred_tile_is_listed_later_at_its_newest_prefix() {
        let mut s = Cdf53GridState::with_budget(tile_cost_us(14), 1000);
        s.resize(8, 4);
        // Tile 0 is complete (and goes first), tile 1 starts at k = 2.
        for p in 0..14 {
            feed(&mut s, 0, p, 0x3FFF, 1, 0);
        }
        feed(&mut s, 1, 0, 0x3FFF, 1, 0);
        feed(&mut s, 1, 1, 0x3FFF, 1, 0);
        assert_eq!(s.plan()[0].tile_x, 0);
        // While it waits, three more passes arrive; its planes were held, not
        // uploaded, and are all uploaded now, once.
        for p in 2..5 {
            feed(&mut s, 1, p, 0x3FFF, 1, 0);
        }
        let l = s.plan();
        assert_eq!((l[0].tile_x, l[0].k), (1, 5));
        assert_eq!(l[0].planes.len(), 5);
    }

    #[test]
    fn the_hard_cap_binds_whatever_the_budget() {
        let mut s = Cdf53GridState::with_budget(u32::MAX, 3);
        s.resize(8, 4);
        complete_tiles(&mut s, 8);
        assert_eq!(s.plan().len(), 3);
        assert!(s.has_pending());
    }

    #[test]
    fn a_partial_tile_is_redrawn_at_most_every_other_flush() {
        let mut s = grid();
        feed(&mut s, 0, 0, 0x3FFF, 1, 0);
        feed(&mut s, 0, 1, 0x3FFF, 1, 0);
        assert_eq!(s.plan().len(), 1); // k = 2
        feed(&mut s, 0, 2, 0x3FFF, 1, 0);
        assert!(s.plan().is_empty(), "drawn last flush, still receiving");
        assert!(s.has_pending(), "waits, is not dropped");
        let l = s.plan();
        assert_eq!((l.len(), l[0].k), (1, 3));
        assert!(s.take_stats().throttled >= 1);
    }

    #[test]
    fn a_complete_tile_is_never_throttled() {
        let mut s = grid();
        feed(&mut s, 0, 0, 0x3FFF, 1, 0);
        feed(&mut s, 0, 1, 0x3FFF, 1, 0);
        assert_eq!(s.plan().len(), 1);
        for p in 2..14 {
            feed(&mut s, 0, p, 0x3FFF, 1, 0);
        }
        assert_eq!(s.plan().len(), 1);
    }

    #[test]
    fn a_newer_stateless_tile_cancels_a_pending_draw_and_older_passes() {
        let mut s = grid();
        for p in 0..14 {
            feed(&mut s, 0, p, 0x3FFF, 5, 0);
        }
        s.stateless(0, 0, 6);
        assert!(s.plan().is_empty(), "the Solid must not be drawn over");
        assert_eq!(s.take_stats().cancelled_superseded, 1);
        // A late pass of the older frame is refused...
        feed(&mut s, 0, 3, 0x3FFF, 5, 0);
        assert!(s.plan().is_empty());
        assert_eq!(s.take_stats().dropped_superseded, 1);
        // ...a newer one re-arms the tile.
        feed(&mut s, 0, 3, 0x3FFF, 7, 0);
        assert_eq!(s.plan().len(), 1);
    }

    #[test]
    fn an_older_or_equal_stateless_tile_does_not_cancel() {
        let mut s = grid();
        for p in 0..14 {
            feed(&mut s, 0, p, 0x3FFF, 5, 0);
        }
        s.stateless(0, 0, 5); // equal: never supersedes
        s.stateless(0, 0, 4);
        assert_eq!(s.plan().len(), 1);
    }

    #[test]
    fn a_resize_to_the_same_shape_keeps_progress_and_a_new_shape_forgets() {
        let mut s = grid();
        feed(&mut s, 0, 0, 0x3FFF, 1, 0);
        feed(&mut s, 0, 1, 0x3FFF, 1, 0);
        s.resize(8, 4);
        assert_eq!(s.plan().len(), 1);
        feed(&mut s, 0, 2, 0x3FFF, 1, 0);
        s.resize(9, 4);
        assert!(s.plan().is_empty());
        assert!(!s.has_pending());
    }

    #[test]
    fn a_pass_for_a_tile_off_the_grid_is_counted_and_dropped() {
        let mut s = grid();
        s.pass(200, 0, 1, 0, false, 0, Some(1), plane(1));
        s.pass(0, 99, 1, 0, false, 0, Some(1), plane(1));
        assert_eq!(s.take_stats().dropped_off_grid, 2);
        assert!(s.plan().is_empty());
    }

    // ---- fairness, supersede, calibration ----------------------------------

    #[test]
    fn a_pending_tile_hit_by_a_new_generation_keeps_its_place_in_the_queue() {
        let per = tile_cost_us(14);
        let mut s = Cdf53GridState::with_budget(per, 1000);
        s.resize(8, 4);
        // Tile 0 is the oldest pending; tiles 1..3 follow.
        for x in 0..4 {
            for p in 0..14 {
                feed(&mut s, x, p, 0x3FFF, 1, 0);
            }
        }
        // Tile 0 is hit by a new generation while it waits (not yet complete).
        s.pass(0, 0, 2, 1, true, 0, Some(0x3FFF), plane(1));
        for p in 1..14 {
            s.pass(0, 0, 2, 1, false, p, None, plane(p + 1));
        }
        let first = s.plan();
        assert_eq!(first[0].tile_x, 0, "it was first in line and must stay so");
        assert_eq!(first[0].mask, 0x3FFF, "and carries the new generation");
        assert_eq!(first[0].frame_seq, 2);
    }

    #[test]
    fn churn_over_budget_does_not_starve_the_bottom_rows() {
        // Raster-order churn: every flush re-dirties the first tiles with new
        // generations, more than the budget covers. The last tile must still
        // be drawn within a bounded number of flushes.
        let per = tile_cost_us(14);
        let mut s = Cdf53GridState::with_budget(per * 2, 1000);
        s.resize(8, 4);
        for p in 0..14 {
            feed(&mut s, 7, p, 0x3FFF, 1, 0);
        }
        let mut drawn_at = None;
        for flush in 0..40u32 {
            // Fresh complete pictures for tiles 0..=5 every flush.
            for x in 0..6u8 {
                for p in 0..14 {
                    s.pass(
                        x,
                        0,
                        10 + flush,
                        (flush % 15) as u8,
                        p == 0,
                        p,
                        (p == 0).then_some(0x3FFF),
                        plane(p + 1),
                    );
                }
            }
            if s.plan().iter().any(|t| t.tile_x == 7) && drawn_at.is_none() {
                drawn_at = Some(flush);
            }
        }
        let at = drawn_at.expect("the last tile was never drawn");
        assert!(
            at <= MAX_WAIT_FLUSHES as u32 + 2,
            "drawn only at flush {at}"
        );
    }

    #[test]
    fn a_partial_tile_is_not_starved_by_a_stream_of_complete_ones() {
        let per = tile_cost_us(14);
        let mut s = Cdf53GridState::with_budget(per, 1000);
        s.resize(8, 4);
        // Tile 1 is partial (k = 3); every flush brings a new complete tile.
        for p in 0..3 {
            feed(&mut s, 1, p, 0x3FFF, 1, 0);
        }
        let mut at = None;
        for flush in 0..30u32 {
            for p in 0..14 {
                s.pass(
                    2,
                    0,
                    5 + flush,
                    (flush % 15) as u8,
                    p == 0,
                    p,
                    (p == 0).then_some(0x3FFF),
                    plane(p + 1),
                );
            }
            if s.plan().iter().any(|t| t.tile_x == 1) && at.is_none() {
                at = Some(flush);
            }
        }
        let at = at.expect("the partial tile was never drawn");
        assert!(
            at <= MAX_WAIT_FLUSHES as u32 + 2,
            "drawn only at flush {at}"
        );
    }

    #[test]
    fn supersede_all_cancels_everything_pending_and_refuses_older_passes() {
        let per = tile_cost_us(14);
        let mut s = Cdf53GridState::with_budget(per * 2, 1000);
        s.resize(8, 4);
        complete_tiles(&mut s, 8);
        assert_eq!(s.plan().len(), 2); // 6 left over
        assert!(s.has_pending());
        s.supersede_all(50);
        assert!(!s.has_pending());
        assert!(s.plan().is_empty());
        assert_eq!(s.take_stats().cancelled_superseded, 6);
        // Older than the H.264 frame: refused, on any tile.
        feed(&mut s, 3, 0, 0x3FFF, 49, 0);
        assert!(s.plan().is_empty());
        // At or after it: drawn as usual.
        for p in 0..14 {
            feed(&mut s, 3, p, 0x3FFF, 50, 0);
        }
        assert_eq!(s.plan().len(), 1);
    }

    #[test]
    fn a_late_picture_does_not_cancel_the_pending_tiles_of_a_later_frame() {
        let mut s = Cdf53GridState::with_budget(tile_cost_us(14), 1000);
        s.resize(8, 4);
        // Tile 0 from frame 301 (after the picture), tile 1 from frame 299.
        for p in 0..14 {
            feed(&mut s, 0, p, 0x3FFF, 301, 0);
            feed(&mut s, 1, p, 0x3FFF, 299, 0);
        }
        s.supersede_all(300);
        let listed = s.plan();
        assert_eq!(listed.len(), 1, "only the later frame's tile survives");
        assert_eq!(listed[0].frame_seq, 301);
        assert_eq!(s.take_stats().cancelled_superseded, 1);
    }

    #[test]
    fn a_new_generation_is_never_refused_as_stale() {
        let mut s = grid();
        s.supersede_all(1000);
        s.stateless(0, 0, 1000);
        // A server that restarted its frame_seq: the core says new generation.
        s.pass(0, 0, 3, 0, true, 0, Some(0x3), plane(1));
        s.pass(0, 0, 3, 0, false, 1, None, plane(2));
        assert_eq!(s.plan().len(), 1);
    }

    #[test]
    fn a_stateless_record_ends_once_a_newer_pass_is_accepted() {
        let mut s = grid();
        s.stateless(0, 0, 10);
        feed(&mut s, 0, 0, 0x3FFF, 5, 0);
        assert_eq!(s.take_stats().dropped_superseded, 1, "older: refused");
        feed(&mut s, 0, 0, 0x3FFF, 11, 0);
        assert_eq!(s.take_stats().dropped_superseded, 0);
        // The record is spent: an old-looking seq (a wrap, a restart) is not
        // refused for ever.
        feed(&mut s, 0, 1, 0x3FFF, 5, 0);
        assert_eq!(s.take_stats().dropped_superseded, 0);
    }

    #[test]
    fn the_budget_follows_the_calibrated_cost() {
        let tiles_in_first_flush = |us_per_tile: Option<f64>| {
            let mut s = Cdf53GridState::with_budget(5000, 100_000);
            s.resize(8, 4);
            if let Some(us) = us_per_tile {
                s.set_cost_per_tile_k14(us);
            }
            complete_tiles(&mut s, 8);
            s.plan().len()
        };
        let slow = tiles_in_first_flush(Some(900.0));
        let model = tiles_in_first_flush(None);
        let fast = tiles_in_first_flush(Some(1.0));
        assert_eq!(slow, 5);
        assert_eq!(model, 8, "the Mali model fits 8 tiles in 5 ms");
        assert_eq!(fast, 8);
        // A big enough burst separates model from fast.
        let mut s = Cdf53GridState::with_budget(5000, 100_000);
        s.resize(60, 34);
        s.set_cost_per_tile_k14(2.0);
        for y in 0..34u8 {
            for x in 0..60u8 {
                for p in 0..14 {
                    s.pass(
                        x,
                        y,
                        1,
                        0,
                        p == 0,
                        p,
                        (p == 0).then_some(0x3FFF),
                        plane(p + 1),
                    );
                }
            }
        }
        assert_eq!(
            s.plan().len(),
            2040,
            "a fast GPU takes the whole screen in one"
        );
    }

    // ---- differential against the CPU decoder -----------------------------

    fn lcg(state: &mut u64) -> u32 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*state >> 33) as u32
    }

    /// Rebuild the picture a listed tile asks for, the way the shader does:
    /// planes by mask, zero elsewhere, `decode_planes` + `inverse`.
    fn picture(planes: &[Option<Vec<u8>>; 14], mask: u32, k: u32) -> Option<Vec<u8>> {
        let zero = vec![0u8; 384];
        let refs: Vec<&[u8]> = (0..k as usize)
            .map(|p| {
                if mask >> p & 1 == 1 {
                    planes[p].as_deref().unwrap()
                } else {
                    &zero[..]
                }
            })
            .collect();
        let coeffs = codec::decode_planes(&refs);
        if k < 14 && coeffs.iter().all(|&c| c == 0) {
            return None;
        }
        Some(codec::inverse(&coeffs))
    }

    /// One generation of a tile: its number, `present_passes`, and the planes.
    type Wire = (u8, u16, Vec<(u8, Vec<u8>)>);

    /// The wire passes of a random tile: `(present_passes, [(pass, plane)])`.
    fn random_wire(rng: &mut u64) -> (u16, Vec<(u8, Vec<u8>)>) {
        let kind = lcg(rng) % 3;
        let coeffs: Vec<i16> = (0..3072)
            .map(|i| match kind {
                0 => ((i * 37 % 255) as i16) - 127,
                1 if i % 11 == 0 => ((lcg(rng) % 2001) as i16) - 1000,
                1 => 0,
                _ => {
                    if i < 40 {
                        (lcg(rng) % 9) as i16 - 4
                    } else {
                        0
                    }
                }
            })
            .collect();
        let (present, sparse) = codec::encode_passes_sparse(&coeffs);
        let wire = sparse
            .into_iter()
            .map(|(p, payload)| {
                let mut off = if p == 0 { 2 } else { 0 };
                let mut plane = Vec::new();
                for _ in 0..3 {
                    let len = u16::from_be_bytes([payload[off], payload[off + 1]]) as usize;
                    plane.extend(codec::rle_decode(&payload[off + 2..off + 2 + len]));
                    off += 2 + len;
                }
                (p, plane)
            })
            .collect();
        (present, wire)
    }

    /// Random arrival orders, duplicates, gaps, two generations, and Solid
    /// tiles interleaved: after every flush, what the scheduler would put on
    /// screen for the tile is what the CPU decoder (plus a Solid overwriting
    /// the screen) has on screen. The reference sees exactly the passes the
    /// scheduler should accept -- a pass older than the last Solid is refused
    /// by design -- so a disagreement is a bug in the prefix, mask, generation
    /// or supersede logic, not in the model.
    #[test]
    fn differential_against_the_cpu_decoder_on_random_arrivals() {
        const SOLID: u8 = 0xEE;
        let mut rng = 12345u64;
        let (mut drawn_total, mut solids_seen, mut refused, mut gens_drawn) = (0, 0, 0, 0);
        for round in 0..80 {
            // Two generations of the tile, A then B.
            let gens: Vec<Wire> = (1..=2u8)
                .map(|g| {
                    let (present, wire) = random_wire(&mut rng);
                    (g, present, wire)
                })
                .collect();
            let mut script: Vec<(u8, u16, u8, Vec<u8>, bool)> = Vec::new(); // gen, present, pass, plane, first-of-gen
            for (g, present, wire) in &gens {
                let mut order: Vec<usize> = (0..wire.len()).collect();
                for i in (1..order.len()).rev() {
                    order.swap(i, (lcg(&mut rng) as usize) % (i + 1));
                }
                if round % 3 == 0 {
                    order.sort();
                }
                if round % 4 == 1 && order.len() > 2 {
                    order.remove(1);
                }
                let dup = order.first().copied();
                order.extend(dup);
                // The core marks the first pass it hands over as new.
                for (n, &w) in order.iter().enumerate() {
                    script.push((*g, *present, wire[w].0, wire[w].1.clone(), n == 0));
                }
            }

            let mut cpu = Cdf53TileState::new();
            let mut s = grid();
            let mut cpu_screen: Option<Vec<u8>> = None;
            let mut gpu_screen: Option<Vec<u8>> = None;
            let mut kept: [Option<Vec<u8>>; 14] = Default::default();
            let mut seq = 10u32;
            let mut last_solid: Option<u32> = None;
            let mut gen_started = [false; 2];
            for (g, present, p, plane, first) in script {
                seq += 1;
                // Now and then a Solid lands (newer than everything so far).
                if lcg(&mut rng).is_multiple_of(5) {
                    seq += 1;
                    s.stateless(0, 0, seq);
                    last_solid = Some(seq);
                    cpu_screen = Some(vec![SOLID]);
                    gpu_screen = Some(vec![SOLID]);
                    solids_seen += 1;
                    // The Solid is the newest thing; nothing pending draws.
                    for _ in 0..2 {
                        assert!(s.plan().is_empty(), "round {round}: drew over a Solid");
                    }
                }
                // Now and then a pass arrives from before the last Solid.
                let this_seq = match last_solid {
                    Some(ls) if lcg(&mut rng).is_multiple_of(4) => ls - 1,
                    _ => seq,
                };
                let new_gen = first && !gen_started[g as usize - 1];
                if first {
                    gen_started[g as usize - 1] = true;
                }
                let refuse = !new_gen && last_solid.is_some_and(|ls| this_seq < ls);
                let pass_present = (p == 0).then_some(present);
                s.pass(0, 0, this_seq, g, new_gen, p, pass_present, plane.clone());
                if refuse {
                    refused += 1;
                } else {
                    // An accepted pass is not older than the Solid: the record
                    // has done its job.
                    last_solid = None;
                    let pre = PrevalidatedCdf53 {
                        generation: g,
                        pass_idx: p,
                        bit_planes: plane.clone(),
                        present_passes: pass_present,
                    };
                    if let Some(bgr) = cpu.integrate_bgr(0, 0, &pre) {
                        cpu_screen = Some(bgr);
                    }
                    if g == 2 {
                        gens_drawn += 1;
                    }
                }
                // Two flushes: the throttle only ever delays by one.
                for _ in 0..2 {
                    for l in s.plan() {
                        for (pp, v) in l.planes {
                            kept[pp as usize] = Some(v);
                        }
                        if let Some(pic) = picture(&kept, l.mask, l.k) {
                            gpu_screen = Some(pic);
                            drawn_total += 1;
                        }
                    }
                }
                assert!(
                    gpu_screen == cpu_screen,
                    "round {round}: screens differ after gen {g} pass {p} (seq {this_seq})"
                );
            }
        }
        assert!(drawn_total > 150, "only {drawn_total} draws compared");
        assert!(
            solids_seen > 20 && refused > 5,
            "{solids_seen} solids, {refused} refused"
        );
        assert!(
            gens_drawn > 50,
            "only {gens_drawn} second-generation passes"
        );
    }
}
