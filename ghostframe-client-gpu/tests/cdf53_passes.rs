use ghostframe_client_gpu::pipelines::cdf53_passes::{
    passes_processed, FULL_PASS_MASK, MAX_PASSES,
};

#[test]
fn unknown_bitmap_falls_back_to_the_pass_index_rule() {
    // present_passes == 0 means pass 0 has not arrived yet.
    assert_eq!(passes_processed(0b1, 0, 0, 0), 1);
    assert_eq!(passes_processed(0b11, 0, 1, 1), 2);
    // Never goes backwards.
    assert_eq!(passes_processed(0b1, 0, 5, 0), 5);
}

#[test]
fn contiguous_arrivals_count_up() {
    let present = FULL_PASS_MASK;
    assert_eq!(passes_processed(0b0000_0001, present, 0, 0), 1);
    assert_eq!(passes_processed(0b0000_0011, present, 1, 1), 2);
    assert_eq!(passes_processed(0b0000_0111, present, 2, 2), 3);
}

#[test]
fn a_gap_in_arrivals_stops_the_prefix() {
    // Passes 0 and 2 arrived, 1 is present but missing -> prefix stops at 1.
    assert_eq!(passes_processed(0b0000_0101, FULL_PASS_MASK, 0, 2), 1);
}

#[test]
fn a_skipped_plane_counts_as_resolved_not_missing() {
    // THE RULE THIS FILE EXISTS FOR. Pass 1 is absent from present_passes,
    // so it is known-zero. With 0 and 2 arrived, the prefix must reach 3 --
    // NOT stop at 1 as it would if the skip were treated as "not yet here".
    let present = FULL_PASS_MASK & !(1 << 1);
    assert_eq!(passes_processed(0b0000_0101, present, 0, 2), 3);
}

#[test]
fn all_trailing_planes_skipped_reaches_full_depth() {
    // Only passes 0..=2 ever exist; all three arrived. The tile is lossless,
    // so K must be MAX_PASSES, not 3. Stopping at 3 applies a midpoint
    // correction for bits that are known to be zero.
    let present = 0b0000_0111;
    assert_eq!(passes_processed(0b0000_0111, present, 0, 2), MAX_PASSES);
}

#[test]
fn saturates_at_max_passes() {
    assert_eq!(
        passes_processed(FULL_PASS_MASK, FULL_PASS_MASK, 0, 13),
        MAX_PASSES
    );
}

#[test]
fn bits_above_the_valid_range_are_ignored() {
    // Nothing above bit 13 is meaningful; stray high bits must not extend K.
    assert_eq!(passes_processed(0xFFFF, 0xFFFF, 0, 13), MAX_PASSES);
}
