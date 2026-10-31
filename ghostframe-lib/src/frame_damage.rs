//! What a producer knows about the pixels that changed since its previous
//! submission (damage-driven encoding, design note section 3.2).
//!
//! The library compares pixels, and that comparison stays the authority over
//! what reaches the encoder. Damage only narrows which rows are *uploaded and
//! compared*: [`FrameDamage::Tiles`] names the tiles the producer believes
//! changed (`hint`) and the tiles it refreshed to check on its belief
//! (`verify`, the sweep slice). Every tile in a tile row that holds either is a
//! SAD candidate, so a changed tile nobody hinted is still sent when it shares
//! a band with one that was. Tiles in no such row are not compared this tick.

use crate::tile::TILE_SIZE;

/// One bit per tile, row-major, `ceil(w/32) * ceil(h/32)` bits in `u64` words
/// (the layout [`crate::FeedbackKind::Dirty`] uses).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TileBits {
    cols: u32,
    rows: u32,
    words: Vec<u64>,
}

impl TileBits {
    /// An empty set for a `width` x `height` pixel frame.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            cols: width.div_ceil(TILE_SIZE),
            rows: height.div_ceil(TILE_SIZE),
            words: vec![0; tile_count(width, height).div_ceil(64)],
        }
    }

    /// Adopt `words` as the bits of a `width` x `height` frame's tile grid.
    /// `None` if they are not exactly `ceil(tiles / 64)` words.
    pub fn from_words(width: u32, height: u32, words: Vec<u64>) -> Option<Self> {
        (words.len() == tile_count(width, height).div_ceil(64)).then_some(Self {
            cols: width.div_ceil(TILE_SIZE),
            rows: height.div_ceil(TILE_SIZE),
            words,
        })
    }

    /// Add flat tile `index`. Out of range is ignored.
    pub fn set(&mut self, index: usize) {
        if let Some(w) = self.words.get_mut(index / 64) {
            *w |= 1 << (index % 64);
        }
    }

    /// Whether flat tile `index` is in the set.
    pub fn get(&self, index: usize) -> bool {
        self.words
            .get(index / 64)
            .is_some_and(|w| w & (1 << (index % 64)) != 0)
    }

    /// The bits as words.
    pub fn words(&self) -> &[u64] {
        &self.words
    }
}

/// Damage for one submission.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FrameDamage {
    /// No knowledge: upload and compare the whole frame. Always safe, and
    /// the only value the C ABI and every non-X11 producer sends.
    #[default]
    Unknown,
    /// The pixels outside the tile rows of `hint | verify` are unchanged since
    /// the previous submission, and the pixels in those rows are current.
    Tiles {
        /// Tiles damage reports name (the cursor's old and new tiles included).
        hint: TileBits,
        /// Tiles refreshed only to check the hint (the sweep slice).
        verify: TileBits,
    },
}

/// What a submission's `pixels` hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Content {
    /// A complete, current buffer for the whole frame (what `damage` narrows).
    #[default]
    Pixels,
    /// No new pixels: `pixels` is empty. The screen is what the previous
    /// submission left in the library's upload image, and the tick is only
    /// the clock the refinement, PixelPerfect and escalation state machine
    /// runs on (design note section 3.3). The producer sends it only when it
    /// knows nothing changed; a library that cannot honour it (no consistent
    /// upload image of that size) drops the tick.
    Unchanged,
}

/// A run of tile rows `[first, first + count)`.
pub(crate) type RowRun = (u32, u32);

impl FrameDamage {
    /// The tile rows of a `width` x `height` frame this damage asks to be
    /// uploaded and compared, as maximal runs. `None` for [`Unknown`], and
    /// for a `Tiles` built for another frame size (its bits say nothing about
    /// this frame).
    ///
    /// [`Unknown`]: FrameDamage::Unknown
    pub(crate) fn upload_runs(&self, width: u32, height: u32) -> Option<Vec<RowRun>> {
        let Self::Tiles { hint, verify } = self else {
            return None;
        };
        let (cols, rows) = (width.div_ceil(TILE_SIZE), height.div_ceil(TILE_SIZE));
        // The same grid, not just as many bits: 64x64 and 160x128 both fit in
        // one word.
        if [hint, verify]
            .iter()
            .any(|b| b.cols != cols || b.rows != rows)
        {
            return None;
        }
        let (cols, rows) = (cols as usize, rows as usize);
        let mut runs: Vec<RowRun> = Vec::new();
        for r in 0..rows {
            let (lo, hi) = (r * cols, (r + 1) * cols);
            if any_bit(&hint.words, lo, hi) || any_bit(&verify.words, lo, hi) {
                match runs.last_mut() {
                    Some(last) if last.0 + last.1 == r as u32 => last.1 += 1,
                    _ => runs.push((r as u32, 1)),
                }
            }
        }
        Some(runs)
    }
}

/// Whether any bit in `[lo, hi)` is set.
fn any_bit(words: &[u64], lo: usize, hi: usize) -> bool {
    let (wlo, whi) = (lo / 64, (hi - 1) / 64);
    (wlo..=whi).any(|i| {
        let mut m = u64::MAX;
        if i == wlo {
            m &= u64::MAX << (lo % 64);
        }
        if i == whi {
            let top = (hi - 1) % 64;
            m &= u64::MAX >> (63 - top);
        }
        words[i] & m != 0
    })
}

fn tile_count(width: u32, height: u32) -> usize {
    (width.div_ceil(TILE_SIZE) as usize) * (height.div_ceil(TILE_SIZE) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiles(w: u32, h: u32, hint: &[usize], verify: &[usize]) -> FrameDamage {
        let mk = |v: &[usize]| {
            let mut b = TileBits::new(w, h);
            for &i in v {
                b.set(i);
            }
            b
        };
        FrameDamage::Tiles {
            hint: mk(hint),
            verify: mk(verify),
        }
    }

    #[test]
    fn unknown_and_wrong_sized_damage_have_no_runs() {
        assert_eq!(FrameDamage::Unknown.upload_runs(64, 64), None);
        let d = tiles(64, 64, &[0], &[]);
        assert_eq!(d.upload_runs(64, 64 * 40), None, "other grid");
    }

    #[test]
    fn runs_are_the_tile_rows_of_hint_or_verify() {
        // 3 columns x 8 rows (96 x 256 px).
        let d = tiles(96, 256, &[0, 4], &[3 * 6 + 2, 3 * 7]);
        // hint rows 0 and 1; verify rows 6 and 7.
        assert_eq!(d.upload_runs(96, 256), Some(vec![(0, 2), (6, 2)]));
        assert_eq!(tiles(96, 256, &[], &[]).upload_runs(96, 256), Some(vec![]));
    }

    #[test]
    fn any_bit_checks_exactly_the_range() {
        // Rows wider than a word: 70 columns.
        let cols = 70usize;
        let mut b = TileBits::new(70 * 32, 3 * 32);
        b.set(cols + 69); // last tile of row 1
        let w = b.words();
        assert!(!any_bit(w, 0, cols));
        assert!(any_bit(w, cols, 2 * cols));
        assert!(!any_bit(w, 2 * cols, 3 * cols));
    }
}
