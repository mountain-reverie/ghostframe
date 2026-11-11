//! Repair coefficients (rev 6 §6.1).
//!
//! Every block is a submatrix of one 256-point Cauchy matrix over
//! GF(2^8)/0x11d: `c(i, j) = 1 / (x_i + y_j)` with `x_i = 255 - i` (repair
//! row) and `y_j = j` (source index). Columns are normalised by `c(0, j)` so
//! repair row 0 is all ones: a single loss is rebuilt by XOR. Scaling a
//! column keeps every square submatrix nonsingular, so any block with
//! `k + rows <= 256` is MDS.
//!
//! The coefficient depends on (row, source) only, never on k or on how many
//! rows the block planned. That is what lets the server fold each source into
//! the repair rows as it is sent, and lets rows sent later (on request, as
//! probe padding) belong to the same code. Do not add k or r to this function.
use rusty_erasure::gf;

/// Most symbols (sources + repair rows) one block can address.
pub const FEC_MAX_SYMBOLS: usize = 256;

/// Highest repair row a block of `k` sources may use (`k + row + 1 <= 256`),
/// or `None` when `k` is 0 or leaves no row.
pub fn max_row_for(k: usize) -> Option<u8> {
    if k == 0 || k >= FEC_MAX_SYMBOLS {
        return None;
    }
    u8::try_from(FEC_MAX_SYMBOLS - 1 - k).ok()
}

/// Coefficient of source `j` in repair row `i`.
///
/// # Panics
/// When `i + j > 254`: the pair is outside the matrix (`x_i == y_j`). Callers
/// validate rows with [`max_row_for`] first, so a panic here is a bug.
pub fn coef(i: u8, j: u8) -> u8 {
    assert!(
        u16::from(i) + u16::from(j) <= 254,
        "fec coef({i}, {j}) is outside the 256-point Cauchy matrix"
    );
    // c(i, j) / c(0, j) = (x_0 + y_j) / (x_i + y_j)   (+ is XOR in GF(2^8))
    gf::mul(255 ^ j, gf::inv((255 - i) ^ j))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_zero_is_all_ones() {
        for j in 0..=254u8 {
            assert_eq!(coef(0, j), 1, "coef(0, {j})");
        }
    }

    /// Values computed independently (Python, GF(2^8)/0x11d tables) when the
    /// construction was chosen, 2026-11-08. A change here changes the wire.
    #[test]
    fn known_answers() {
        assert_eq!(
            [coef(1, 0), coef(1, 1), coef(1, 2), coef(1, 3), coef(1, 4)],
            [127, 252, 126, 254, 233]
        );
        assert_eq!(coef(7, 200), 228);
    }

    #[test]
    fn max_row_bounds() {
        assert_eq!(max_row_for(0), None);
        assert_eq!(max_row_for(1), Some(254));
        assert_eq!(max_row_for(64), Some(191));
        assert_eq!(max_row_for(255), Some(0));
        assert_eq!(max_row_for(256), None);
    }

    #[test]
    #[should_panic(expected = "outside the 256-point Cauchy matrix")]
    fn out_of_matrix_panics() {
        coef(200, 55);
    }
}
