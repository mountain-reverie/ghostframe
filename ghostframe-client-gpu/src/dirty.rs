//! Tile-granular dirty tracking across multiple frames of history.
//!
//! Each export buffer the client fills may be several frames stale: the
//! host holds a buffer until it has finished presenting it, so by the time
//! we get to write into it again, several generations of decode/render
//! work may have accumulated. Filling that buffer correctly means copying
//! everything that changed since *that buffer* was last written, not just
//! what changed this frame.
//!
//! Dirty state is recorded by the renderer at the point it *writes* a
//! tile, never derived from the protocol's `frame_seq`. A CDF 5/3
//! refinement pass changes the framebuffer without producing a new frame,
//! so deriving damage from `frame_seq` would silently drop those updates.

/// One bit per tile. At 1920x1080 the tile grid is 60x34 = 2040 bits =
/// 255 bytes, so a generation of history is cheap enough to keep many of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyGrid {
    cols: u32,
    rows: u32,
    words: Vec<u64>,
}

impl DirtyGrid {
    pub fn new(cols: u32, rows: u32) -> Self {
        let bits = cols as usize * rows as usize;
        let word_count = bits.div_ceil(64);
        Self {
            cols,
            rows,
            words: vec![0u64; word_count],
        }
    }

    /// Bit index for `(tx, ty)`, or `None` if out of range.
    fn bit_index(&self, tx: u32, ty: u32) -> Option<usize> {
        if tx >= self.cols || ty >= self.rows {
            None
        } else {
            Some(ty as usize * self.cols as usize + tx as usize)
        }
    }

    /// Mark tile `(tx, ty)` dirty.
    ///
    /// Out-of-range coordinates are ignored in release builds and trip a
    /// `debug_assert` in debug builds. They must never wrap into a
    /// neighbouring tile's bit -- silently corrupting a different tile's
    /// dirty state is worse than silently dropping an invalid call.
    pub fn set(&mut self, tx: u32, ty: u32) {
        match self.bit_index(tx, ty) {
            Some(bit) => self.words[bit / 64] |= 1u64 << (bit % 64),
            None => debug_assert!(
                false,
                "DirtyGrid::set out of range: ({tx}, {ty}) for a {}x{} grid",
                self.cols, self.rows
            ),
        }
    }

    /// Whether tile `(tx, ty)` is dirty. Out-of-range coordinates report
    /// `false` rather than panicking, so callers can probe near an edge
    /// without bounds-checking first.
    pub fn get(&self, tx: u32, ty: u32) -> bool {
        match self.bit_index(tx, ty) {
            Some(bit) => (self.words[bit / 64] >> (bit % 64)) & 1 != 0,
            None => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    pub fn clear(&mut self) {
        for w in &mut self.words {
            *w = 0;
        }
    }

    /// In-place union: mark dirty every tile that is dirty in `other`.
    ///
    /// Both grids must share the same shape -- mismatched shapes trip a
    /// `debug_assert` and, in release builds, union only the overlapping
    /// word range rather than panicking or indexing out of bounds.
    pub fn union_with(&mut self, other: &DirtyGrid) {
        debug_assert_eq!(
            self.cols, other.cols,
            "DirtyGrid::union_with shape mismatch"
        );
        debug_assert_eq!(
            self.rows, other.rows,
            "DirtyGrid::union_with shape mismatch"
        );
        for (a, b) in self.words.iter_mut().zip(other.words.iter()) {
            *a |= *b;
        }
    }

    /// Iterate dirty tile coordinates in row-major order.
    pub fn iter_set(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        let cols = self.cols.max(1);
        self.words.iter().enumerate().flat_map(move |(wi, &w)| {
            (0..64u32).filter_map(move |bit| {
                if (w >> bit) & 1 != 0 {
                    let idx = wi as u32 * 64 + bit;
                    Some((idx % cols, idx / cols))
                } else {
                    None
                }
            })
        })
    }

    pub fn cols(&self) -> u32 {
        self.cols
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }
}

/// A bounded ring of per-generation dirty maps.
///
/// Every call to [`DirtyHistory::advance`] seals whatever tiles were
/// marked dirty in the current generation and starts a fresh one. Callers
/// that fill an export buffer record the generation returned by `advance`
/// (or [`DirtyHistory::current_gen`]) as that buffer's watermark, then
/// later ask [`DirtyHistory::union_since`] for everything that changed
/// after that watermark.
pub struct DirtyHistory {
    cols: u32,
    rows: u32,
    capacity: usize,
    current: DirtyGrid,
    /// The generation number `current` will be sealed as on the next
    /// `advance()`. Also doubles as "one past the newest sealed
    /// generation" once at least one `advance()` has happened.
    next_gen: u64,
    /// Sealed generations, oldest first, bounded to `capacity` entries.
    sealed: std::collections::VecDeque<(u64, DirtyGrid)>,
}

impl DirtyHistory {
    pub fn new(cols: u32, rows: u32, capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            cols,
            rows,
            capacity,
            current: DirtyGrid::new(cols, rows),
            next_gen: 0,
            sealed: std::collections::VecDeque::with_capacity(capacity),
        }
    }

    /// The map for the generation being accumulated now.
    pub fn current_mut(&mut self) -> &mut DirtyGrid {
        &mut self.current
    }

    /// Seal the current generation and start a new one. Returns the
    /// generation just sealed.
    pub fn advance(&mut self) -> u64 {
        let gen = self.next_gen;
        let sealed_grid =
            std::mem::replace(&mut self.current, DirtyGrid::new(self.cols, self.rows));
        self.sealed.push_back((gen, sealed_grid));
        if self.sealed.len() > self.capacity {
            self.sealed.pop_front();
        }
        self.next_gen += 1;
        gen
    }

    /// Union of every sealed generation strictly after `since`.
    ///
    /// Returns `None` when the caller must do a full blit instead:
    ///
    /// - `since` is `None`: the buffer has never been filled, so there is
    ///   no watermark to union from -- everything is potentially stale.
    /// - `since` names a generation older than everything left in the
    ///   ring: some of the generations we would need to union (those in
    ///   `since+1 ..= newest`) have already been evicted, so the true
    ///   union cannot be reconstructed. Returning a *partial* union here
    ///   would under-copy and leave stale pixels in the buffer that look
    ///   almost right -- far harder to notice than a visibly broken
    ///   frame, so we refuse to guess and force a full blit instead.
    ///
    /// `since` need not itself still be present in the ring: only the
    /// generations *after* it are read, so a watermark exactly at the
    /// oldest retained generation minus one is still fully reconstructable.
    pub fn union_since(&self, since: Option<u64>) -> Option<DirtyGrid> {
        let since = since?;

        // `next_gen == 0` means advance() has never been called: nothing
        // has been sealed, so nothing can have changed yet.
        let newest_sealed = self.next_gen.checked_sub(1)?;

        if since >= newest_sealed {
            // The watermark already covers everything sealed so far.
            return Some(DirtyGrid::new(self.cols, self.rows));
        }

        // Invariant: whenever `sealed` is non-empty (guaranteed here,
        // since newest_sealed exists and capacity >= 1), its front holds
        // the oldest retained generation.
        let oldest_retained = self
            .sealed
            .front()
            .map(|(g, _)| *g)
            .unwrap_or(newest_sealed);

        if since + 1 < oldest_retained {
            // A generation we need (since+1) has already been evicted.
            return None;
        }

        let mut result = DirtyGrid::new(self.cols, self.rows);
        for (gen, grid) in &self.sealed {
            if *gen > since {
                result.union_with(grid);
            }
        }
        Some(result)
    }

    /// The generation number currently being accumulated (i.e. what the
    /// next `advance()` call will seal and return).
    pub fn current_gen(&self) -> u64 {
        self.next_gen
    }

    /// Drop all history. Called on resize, when the grid shape changes.
    pub fn reset(&mut self, cols: u32, rows: u32) {
        self.cols = cols;
        self.rows = rows;
        self.current = DirtyGrid::new(cols, rows);
        self.next_gen = 0;
        self.sealed.clear();
    }
}
