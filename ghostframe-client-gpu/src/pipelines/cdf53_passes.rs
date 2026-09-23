//! The sparse-K pass-resolution rule shared by every Cdf53 inverse pipeline.
//!
//! Ported line-for-line from `computePassesProcessed` in
//! `ghostframe-web-client/src/webgpu/cdf53.ts`. The two must not drift.

/// Bit-plane passes a CDF 5/3 tile can carry.
pub const MAX_PASSES: u32 = 14;
/// Mask of all valid pass bits.
pub const FULL_PASS_MASK: u16 = (1 << MAX_PASSES) - 1;

/// How many leading passes are resolved for this tile.
///
/// A pass is resolved when its plane has arrived OR `present_passes` says it
/// was never coming, so it is known-zero. Counting a skipped trailing plane
/// as merely "not yet arrived" adds a midpoint correction for bits known to
/// be zero -- measured at ~16/255 per channel on flat content.
///
/// `present_passes == 0` means the bitmap is not yet known (pass 0 has not
/// arrived), not "nothing present"; the pass-index rule applies then.
///
/// Mirrors `computePassesProcessed` in
/// `ghostframe-web-client/src/webgpu/cdf53.ts`. The two must not drift.
pub fn passes_processed(
    received_mask: u16,
    present_passes: u16,
    prev_passes_processed: u32,
    pass_idx: u8,
) -> u32 {
    let present = present_passes & FULL_PASS_MASK;
    if present == 0 {
        return prev_passes_processed.max(u32::from(pass_idx) + 1);
    }
    let resolved = (received_mask | !present) & FULL_PASS_MASK;
    let mut k = 0u32;
    while k < MAX_PASSES && (resolved & (1 << k)) != 0 {
        k += 1;
    }
    k
}
