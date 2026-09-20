//! Port of `ghostframe-web-client/tests/cdf53_coverage.test.ts` (11 cases).
//!
//! `now_us` here is treated as an opaque scaled timestamp matching the
//! vitest `nowMs` values directly (no unit conversion) — the tests only
//! ever compare relative ordering/equality of the stamped values, so the
//! literal numbers are ported byte-for-byte from the TS source.

use ghostframe_client_core::cdf53_coverage::apply_cdf53_arrival;
use ghostframe_client_core::CoverageEntry;

#[test]
fn creates_a_fresh_entry_for_a_brand_new_tile_generation() {
    let out = apply_cdf53_arrival(None, 3, 0, 7, 100, true, None);
    assert_eq!(out.entry.generation, 3);
    assert_eq!(out.entry.frame_seq, 7);
    assert_eq!(out.entry.pass_mask, 1);
    assert_eq!(out.entry.nacked_mask, 0);
    assert_eq!(out.entry.last_change_us, 100);
    assert_eq!(out.nack_passes, Vec::<u8>::new());
}

#[test]
fn replaces_the_entry_when_generation_differs() {
    let stale = CoverageEntry {
        generation: 2,
        frame_seq: 1,
        pass_mask: 0x3FFF,
        nacked_mask: 0x0F,
        present_passes: None,
        last_change_us: 50,
        sweep_attempts: 0,
    };
    let out = apply_cdf53_arrival(Some(stale), 3, 5, 7, 100, true, None);
    assert_eq!(out.entry.generation, 3);
    assert_eq!(out.entry.frame_seq, 7);
    assert_eq!(out.entry.pass_mask, 1 << 5);
    assert_eq!(out.entry.nacked_mask, 0);
    assert_eq!(out.entry.last_change_us, 100);
    assert_eq!(out.nack_passes, Vec::<u8>::new());
}

#[test]
fn refreshes_frame_seq_on_existing_generation_arrival() {
    let e = CoverageEntry {
        generation: 1,
        frame_seq: 5,
        pass_mask: 1,
        nacked_mask: 0,
        present_passes: None,
        last_change_us: 0,
        sweep_attempts: 0,
    };
    let out = apply_cdf53_arrival(Some(e), 1, 1, 9, 10, true, None);
    assert_eq!(out.entry.frame_seq, 9);
}

#[test]
fn runs_gap_detection_on_existing_gen_success_and_nacks_missing_lower_passes() {
    let e = CoverageEntry {
        generation: 1,
        frame_seq: 0,
        pass_mask: 0b0000001,
        nacked_mask: 0,
        // Pass 0 is set in `pass_mask` below, and pass 0 is what carries
        // the bitmap -- so `None` here is not a state that can occur.
        // A dense tile (all 14 passes present) is the case this test
        // was written for.
        present_passes: Some(0x3FFF),
        last_change_us: 0,
        sweep_attempts: 0,
    };
    let out = apply_cdf53_arrival(Some(e), 1, 4, 0, 10, true, None);
    assert_eq!(out.nack_passes, vec![1, 2, 3]);
    assert_eq!(out.entry.pass_mask, 0b0010001);
    assert_eq!(out.entry.nacked_mask, 0b0001110);
}

#[test]
fn does_not_run_gap_detection_on_a_fresh_generation_entry() {
    let out = apply_cdf53_arrival(None, 0, 5, 0, 0, true, None);
    assert_eq!(out.nack_passes, Vec::<u8>::new());
    assert_eq!(out.entry.pass_mask, 1 << 5);
    assert_eq!(out.entry.nacked_mask, 0);
}

#[test]
fn dedups_gap_detection_nacks_via_nacked_mask() {
    let e = CoverageEntry {
        generation: 1,
        frame_seq: 0,
        pass_mask: 1,
        nacked_mask: 0b0000010,
        // Pass 0 is set in `pass_mask` below, and pass 0 is what carries
        // the bitmap -- so `None` here is not a state that can occur.
        // A dense tile (all 14 passes present) is the case this test
        // was written for.
        present_passes: Some(0x3FFF),
        last_change_us: 0,
        sweep_attempts: 0,
    };
    let out = apply_cdf53_arrival(Some(e), 1, 3, 0, 10, true, None);
    assert_eq!(out.nack_passes, vec![2]);
    assert_eq!(out.entry.nacked_mask, 0b0000110);
}

#[test]
fn phase_1_5a_nacks_the_failing_pass_on_prevalidation_failure() {
    let out = apply_cdf53_arrival(None, 2, 7, 0, 50, false, None);
    assert_eq!(out.nack_passes, vec![7]);
    assert_eq!(out.entry.pass_mask, 0);
    assert_eq!(out.entry.nacked_mask, 1 << 7);
}

#[test]
fn phase_1_5a_does_not_re_nack_an_already_nacked_failed_pass() {
    let e = CoverageEntry {
        generation: 1,
        frame_seq: 0,
        pass_mask: 0,
        nacked_mask: 1 << 7,
        present_passes: None,
        last_change_us: 0,
        sweep_attempts: 0,
    };
    let out = apply_cdf53_arrival(Some(e), 1, 7, 0, 100, false, None);
    assert_eq!(out.nack_passes, Vec::<u8>::new());
    assert_eq!(out.entry.pass_mask, 0);
    assert_eq!(out.entry.nacked_mask, 1 << 7);
}

#[test]
fn phase_1_5a_failure_does_not_advance_last_change_us() {
    let e = CoverageEntry {
        generation: 1,
        frame_seq: 0,
        pass_mask: 0,
        nacked_mask: 0,
        present_passes: None,
        last_change_us: 42,
        sweep_attempts: 0,
    };
    let out = apply_cdf53_arrival(Some(e), 1, 3, 0, 999, false, None);
    assert_eq!(out.entry.last_change_us, 42);
}

#[test]
fn phase_1_5a_success_on_a_previously_failed_pass_sets_bit_and_retains_nacked_mask_bit() {
    let e = CoverageEntry {
        generation: 1,
        frame_seq: 0,
        pass_mask: 0,
        nacked_mask: 1 << 5,
        present_passes: None,
        last_change_us: 0,
        sweep_attempts: 0,
    };
    let out = apply_cdf53_arrival(Some(e), 1, 5, 0, 200, true, None);
    assert_eq!(out.entry.pass_mask, 1 << 5);
    // Pass 0 has not arrived (`pass_mask: 0`), so the tile's bitmap is still
    // unknown and pass 0 is the only pass known to exist. Gap detection
    // therefore asks for pass 0 alone rather than every lower index: under
    // sparse encoding, 1..4 are usually planes the server never sent, and
    // NACKing them produces nothing but misses.
    assert_eq!(out.entry.nacked_mask, (1 << 5) | 1);
    assert_eq!(out.entry.last_change_us, 200);
    assert_eq!(out.nack_passes, vec![0]);
}

#[test]
fn duplicate_success_arrivals_do_not_advance_last_change_us() {
    let e = CoverageEntry {
        generation: 1,
        frame_seq: 0,
        pass_mask: 1 << 3,
        nacked_mask: 0,
        present_passes: None,
        last_change_us: 42,
        sweep_attempts: 0,
    };
    let out = apply_cdf53_arrival(Some(e), 1, 3, 0, 999, true, None);
    assert_eq!(out.entry.pass_mask, 1 << 3);
    assert_eq!(out.entry.last_change_us, 42);
    assert_eq!(out.nack_passes, Vec::<u8>::new());
}

// Representative case from the task brief (duplicate of the gap-detection
// test above but kept verbatim per brief instructions).
#[test]
fn gap_detection_nacks_missing_lower_passes() {
    let prev = CoverageEntry {
        generation: 3,
        frame_seq: 5,
        pass_mask: 0b1,
        nacked_mask: 0,
        // Pass 0 is set in `pass_mask` below, and pass 0 is what carries
        // the bitmap -- so `None` here is not a state that can occur.
        // A dense tile (all 14 passes present) is the case this test
        // was written for.
        present_passes: Some(0x3FFF),
        last_change_us: 42,
        sweep_attempts: 0,
    };
    let out = apply_cdf53_arrival(Some(prev), 3, 4, 9, 100, true, None);
    assert_eq!(out.nack_passes, vec![1, 2, 3]);
    assert_eq!(out.entry.pass_mask, 0b0010001);
    assert_eq!(out.entry.nacked_mask, 0b0001110);
}

// ---------------------------------------------------------------------------
// Stale-generation handling. Not part of the TS port: the behaviour below did
// not exist there, and its absence is what left a tile rendering wrong
// forever. See `refinement_completes_when_a_second_frame_supersedes_the_first`
// in ghostframe-e2e for the end-to-end reproduction.
// ---------------------------------------------------------------------------

/// A pass from an older generation must leave the entry untouched.
///
/// The catch-all that used to handle "generation differs" reset the entry to
/// whatever generation arrived -- including a stale one -- discarding
/// everything accumulated for the current generation. Guarded here as well as
/// at the reassembly call site because this function is public API: the wasm
/// boundary calls it directly, without that call site's protection.
#[test]
fn a_pass_from_an_older_generation_is_ignored() {
    let current = CoverageEntry {
        generation: 1,
        frame_seq: 10,
        pass_mask: 0b11, // gen 1 passes 0 and 1 already landed
        nacked_mask: 0,
        present_passes: None,
        last_change_us: 500,
        sweep_attempts: 0,
    };
    let out = apply_cdf53_arrival(Some(current), 0, 12, 11, 900, true, None);
    assert_eq!(
        out.entry, current,
        "a stale generation-0 pass must not touch a generation-1 entry"
    );
    assert!(out.nack_passes.is_empty(), "and must not provoke a NACK");
}

#[test]
fn a_pass_from_a_newer_generation_starts_over() {
    let current = CoverageEntry {
        generation: 1,
        frame_seq: 10,
        pass_mask: 0b11,
        nacked_mask: 0,
        present_passes: None,
        last_change_us: 500,
        sweep_attempts: 0,
    };
    let out = apply_cdf53_arrival(Some(current), 2, 0, 11, 900, true, None);
    assert_eq!(out.entry.generation, 2);
    assert_eq!(
        out.entry.pass_mask, 0b1,
        "a newer generation starts from scratch with only its own pass"
    );
}

// ---------------------------------------------------------------------------
// `present_passes` bitmap plumbing (sparse CDF53 encoding).
// ---------------------------------------------------------------------------

#[test]
fn present_passes_is_none_until_pass_zero_arrives() {
    let out = apply_cdf53_arrival(None, 0, 6, 0, 0, true, None);
    assert_eq!(
        out.entry.present_passes, None,
        "a non-pass-0 arrival carries no bitmap of its own"
    );
}

#[test]
fn present_passes_is_stored_from_pass_zero() {
    let out = apply_cdf53_arrival(None, 0, 0, 0, 0, true, Some(0x0041));
    assert_eq!(out.entry.present_passes, Some(0x0041));
}

#[test]
fn present_passes_survives_later_passes_that_carry_no_bitmap() {
    let out = apply_cdf53_arrival(None, 0, 0, 0, 0, true, Some(0x0041));
    let out = apply_cdf53_arrival(Some(out.entry), 0, 6, 0, 10, true, None);
    assert_eq!(
        out.entry.present_passes,
        Some(0x0041),
        "a later pass's None must not clobber what pass 0 already taught the tile"
    );
}

#[test]
fn present_passes_resets_to_none_on_a_new_generation() {
    let out = apply_cdf53_arrival(None, 0, 0, 0, 0, true, Some(0x0041));
    let out = apply_cdf53_arrival(Some(out.entry), 1, 0, 0, 10, true, None);
    assert_eq!(
        out.entry.present_passes, None,
        "a new generation starts fresh until its own pass 0 arrives"
    );
}

/// Generations are 4 bits and wrap at 16, so "newer" cannot be a plain `>`.
#[test]
fn generation_ordering_handles_the_four_bit_wrap() {
    use ghostframe_client_core::cdf53_coverage::generation_is_newer;
    assert!(generation_is_newer(1, 0));
    assert!(generation_is_newer(0, 15), "0 follows 15");
    assert!(generation_is_newer(2, 15), "and 2 is newer still");
    assert!(
        !generation_is_newer(15, 0),
        "15 is stale once 0 has arrived"
    );
    assert!(!generation_is_newer(0, 1));
    assert!(!generation_is_newer(3, 3), "equal is not newer");
}


/// Gap detection must respect the tile's own pass set.
///
/// Regression guard for the defect that a static 16-tile lossless scene
/// surfaced as 80 NACKs, every one a miss: gap detection treated every lower
/// index as expected, but sparse encoding skips bit-planes that are entirely
/// zero, and passes 1-5 are empty for essentially all content. The server
/// holds no cache entry for a pass it never sent, so each request could only
/// ever miss.
#[test]
fn gap_detection_skips_passes_the_tile_never_had() {
    // Bitmap says: passes 0, 6 and 7 exist. Passes 1-5 were never sent.
    let present = 0b1100_0001u16;
    let e = CoverageEntry {
        generation: 1,
        frame_seq: 0,
        pass_mask: 0b0000_0001, // pass 0 landed
        nacked_mask: 0,
        present_passes: Some(present),
        last_change_us: 0,
        sweep_attempts: 0,
    };
    // Pass 7 arrives; pass 6 is a real gap, 1-5 are not gaps at all.
    let out = apply_cdf53_arrival(Some(e), 1, 7, 0, 10, true, Some(present));
    assert_eq!(
        out.nack_passes,
        vec![6],
        "only pass 6 is genuinely missing; 1-5 were never sent"
    );
    assert_eq!(out.entry.nacked_mask, 1 << 6);
}
