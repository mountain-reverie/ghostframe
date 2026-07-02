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
    let out = apply_cdf53_arrival(None, 3, 0, 7, 100, true);
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
        last_change_us: 50,
    };
    let out = apply_cdf53_arrival(Some(stale), 3, 5, 7, 100, true);
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
        last_change_us: 0,
    };
    let out = apply_cdf53_arrival(Some(e), 1, 1, 9, 10, true);
    assert_eq!(out.entry.frame_seq, 9);
}

#[test]
fn runs_gap_detection_on_existing_gen_success_and_nacks_missing_lower_passes() {
    let e = CoverageEntry {
        generation: 1,
        frame_seq: 0,
        pass_mask: 0b0000001,
        nacked_mask: 0,
        last_change_us: 0,
    };
    let out = apply_cdf53_arrival(Some(e), 1, 4, 0, 10, true);
    assert_eq!(out.nack_passes, vec![1, 2, 3]);
    assert_eq!(out.entry.pass_mask, 0b0010001);
    assert_eq!(out.entry.nacked_mask, 0b0001110);
}

#[test]
fn does_not_run_gap_detection_on_a_fresh_generation_entry() {
    let out = apply_cdf53_arrival(None, 0, 5, 0, 0, true);
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
        last_change_us: 0,
    };
    let out = apply_cdf53_arrival(Some(e), 1, 3, 0, 10, true);
    assert_eq!(out.nack_passes, vec![2]);
    assert_eq!(out.entry.nacked_mask, 0b0000110);
}

#[test]
fn phase_1_5a_nacks_the_failing_pass_on_prevalidation_failure() {
    let out = apply_cdf53_arrival(None, 2, 7, 0, 50, false);
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
        last_change_us: 0,
    };
    let out = apply_cdf53_arrival(Some(e), 1, 7, 0, 100, false);
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
        last_change_us: 42,
    };
    let out = apply_cdf53_arrival(Some(e), 1, 3, 0, 999, false);
    assert_eq!(out.entry.last_change_us, 42);
}

#[test]
fn phase_1_5a_success_on_a_previously_failed_pass_sets_bit_and_retains_nacked_mask_bit() {
    let e = CoverageEntry {
        generation: 1,
        frame_seq: 0,
        pass_mask: 0,
        nacked_mask: 1 << 5,
        last_change_us: 0,
    };
    let out = apply_cdf53_arrival(Some(e), 1, 5, 0, 200, true);
    assert_eq!(out.entry.pass_mask, 1 << 5);
    assert_eq!(out.entry.nacked_mask, 0b0111111);
    assert_eq!(out.entry.last_change_us, 200);
    assert_eq!(out.nack_passes, vec![0, 1, 2, 3, 4]);
}

#[test]
fn duplicate_success_arrivals_do_not_advance_last_change_us() {
    let e = CoverageEntry {
        generation: 1,
        frame_seq: 0,
        pass_mask: 1 << 3,
        nacked_mask: 0,
        last_change_us: 42,
    };
    let out = apply_cdf53_arrival(Some(e), 1, 3, 0, 999, true);
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
        last_change_us: 42,
    };
    let out = apply_cdf53_arrival(Some(prev), 3, 4, 9, 100, true);
    assert_eq!(out.nack_passes, vec![1, 2, 3]);
    assert_eq!(out.entry.pass_mask, 0b0010001);
    assert_eq!(out.entry.nacked_mask, 0b0001110);
}
