//! The H.264 reference gate (design 7.1) and the frame-ack window (6.2),
//! driven through the public API. The scenarios mirror the closed-loop
//! simulation in `ghostframe-lib`'s `encoder/h264/refs_tests.rs` (`Gate`),
//! whose rule this is the real version of.

use ghostframe_client_core::h264_gate::{
    FrameAckWindow, H264Gate, RefuseCause, ResetCause, Verdict, ACKED_REF_STUCK_US,
    FRAME_ACK_FALLBACK_US, FRAME_ACK_MIN_INTERVAL_US, RESET_REQUEST_MIN_INTERVAL_US,
};
use ghostframe_client_core::ResetReason;
use ghostframe_protocol::protocol::{
    FrameHeader, FRAME_FLAG_IDR, FRAME_FLAG_MARK_LT, FRAME_FLAG_MARK_TARGET, FRAME_FLAG_NON_REF,
    FRAME_FLAG_TARGET_IS_LT, NO_ACK, REF_FRAME_NONE,
};
use proptest::prelude::*;

fn hdr(seq: u32, flags: u8, slots: u8, reference: u32) -> FrameHeader {
    FrameHeader {
        frame_seq: seq,
        frag_idx: 0,
        frag_total: 1,
        wire_seq: NO_ACK,
        timestamp_us: 0,
        flags,
        slots,
        ref_frame_seq: reference,
    }
}

fn idr(seq: u32) -> FrameHeader {
    hdr(seq, FRAME_FLAG_IDR, 0, REF_FRAME_NONE)
}

/// A healthy P predicting from the short-term frame `reference`.
fn st(seq: u32, reference: u32) -> FrameHeader {
    hdr(seq, 0, 0, reference)
}

/// A healthy P that also marks itself into long-term slot `mark`.
fn st_mark(seq: u32, reference: u32, mark: u8) -> FrameHeader {
    hdr(seq, FRAME_FLAG_MARK_LT, mark, reference)
}

/// A healthy P whose marking (MMCO 3) makes the frame it predicts from,
/// `reference`, the long-term picture of slot `mark`.
fn st_mark_target(seq: u32, reference: u32, mark: u8) -> FrameHeader {
    hdr(seq, FRAME_FLAG_MARK_TARGET, mark, reference)
}

/// A recovery P (non-reference) predicting from long-term `slot`.
fn recovery(seq: u32, slot: u8, reference: u32) -> FrameHeader {
    hdr(
        seq,
        FRAME_FLAG_NON_REF | FRAME_FLAG_TARGET_IS_LT,
        slot << 2,
        reference,
    )
}

/// A re-root P: a reference frame predicting from long-term `slot`.
fn reroot(seq: u32, slot: u8, reference: u32) -> FrameHeader {
    hdr(seq, FRAME_FLAG_TARGET_IS_LT, slot << 2, reference)
}

fn fed(v: Verdict) -> bool {
    matches!(v, Verdict::Feed { .. })
}

#[test]
fn an_idr_is_fed_resets_the_decoder_and_seeds_slot_zero() {
    let mut g = H264Gate::new();
    assert_eq!(
        g.on_frame(&idr(5), 0),
        Verdict::Feed {
            reset_decoder: true
        }
    );
    assert_eq!(g.long_term(0), Some(5));
    assert_eq!(g.long_term(1), None);
    assert!(g.is_good(5));
    assert_eq!(
        g.take_request(),
        None,
        "an IDR needs nothing from the server"
    );
}

#[test]
fn a_long_term_target_is_fed_only_when_the_slot_holds_that_frame() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    assert!(fed(g.on_frame(&st(2, 1), 0)));
    // The slot holds 1.
    assert!(fed(g.on_frame(&recovery(3, 0, 1), 0)));
    // The server believes slot 0 holds 2: a belief error is a refusal.
    assert_eq!(
        g.on_frame(&recovery(4, 0, 2), 0),
        Verdict::Refuse(RefuseCause::NoReference)
    );
    // An empty slot holds nothing.
    assert_eq!(
        g.on_frame(&recovery(5, 1, 1), 0),
        Verdict::Refuse(RefuseCause::NoReference)
    );
    // A slot the stream does not have.
    assert_eq!(
        g.on_frame(&recovery(6, 2, 1), 0),
        Verdict::Refuse(RefuseCause::BadSlot)
    );
}

#[test]
fn a_mark_moves_a_frame_into_its_slot_and_a_later_target_finds_it() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    assert!(fed(g.on_frame(&st_mark(2, 1, 1), 0)));
    assert_eq!(g.long_term(1), Some(2));
    assert!(fed(g.on_frame(&st(3, 2), 0)));
    assert!(fed(g.on_frame(&recovery(4, 1, 2), 0)));
    // Slot 0 still has the IDR.
    assert!(fed(g.on_frame(&recovery(5, 0, 1), 0)));
}

#[test]
fn a_target_mark_puts_the_predecessor_not_the_carrier_into_its_slot() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    assert!(fed(g.on_frame(&st(2, 1), 0)));
    assert_eq!(g.long_term(1), None, "premise: slot 1 is empty");
    // 3 carries the mark and predicts from 2: 2 becomes long-term.
    assert!(fed(g.on_frame(&st_mark_target(3, 2, 1), 0)));
    assert_eq!(g.long_term(1), Some(2));
    assert_eq!(g.long_term(0), Some(1), "the other slot is untouched");
    // The carrier is not the long-term picture.
    assert_eq!(
        g.on_frame(&reroot(4, 1, 3), 0),
        Verdict::Refuse(RefuseCause::NoReference)
    );
    assert!(fed(g.on_frame(&reroot(5, 1, 2), 0)));
}

#[test]
fn a_target_mark_needs_its_target_to_be_the_newest_reference() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    assert!(fed(g.on_frame(&st(2, 1), 0)));
    assert!(fed(g.on_frame(&st(3, 2), 0)));
    // The usual rule: 2 is good but not the newest reference.
    assert_eq!(
        g.on_frame(&st_mark_target(4, 2, 1), 0),
        Verdict::Refuse(RefuseCause::NoReference)
    );
    assert_eq!(g.long_term(1), None, "a refused frame marks nothing");
    assert_eq!(g.long_term(0), Some(1));
}

#[test]
fn a_target_mark_means_nothing_on_a_frame_that_cannot_carry_it() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    assert!(fed(g.on_frame(&st(2, 1), 0)));
    // Predicting from a long-term picture: no short-term target to mark.
    // Slot 0 holds the IDR (1); the mark bits name slot 1, which must stay empty.
    let lt_target = hdr(3, FRAME_FLAG_MARK_TARGET | FRAME_FLAG_TARGET_IS_LT, 1, 1);
    assert!(fed(g.on_frame(&lt_target, 0)));
    assert_eq!(g.long_term(1), None);
    // A non-reference carrier cannot hold an MMCO.
    let non_ref = hdr(4, FRAME_FLAG_MARK_TARGET | FRAME_FLAG_NON_REF, 1, 3);
    assert!(fed(g.on_frame(&non_ref, 0)));
    assert_eq!(g.long_term(1), None);
}

#[test]
fn a_target_mark_names_one_slot_and_excludes_the_carrier_mark() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    assert!(fed(g.on_frame(&st(2, 1), 0)));
    assert_eq!(
        g.on_frame(&st_mark_target(3, 2, 2), 0),
        Verdict::Refuse(RefuseCause::BadSlot)
    );
    let both = hdr(4, FRAME_FLAG_MARK_TARGET | FRAME_FLAG_MARK_LT, 1, 2);
    assert_eq!(g.on_frame(&both, 0), Verdict::Refuse(RefuseCause::BadSlot));
    assert_eq!((g.long_term(0), g.long_term(1)), (Some(1), None));
}

#[test]
fn a_target_mark_alone_makes_a_stream_an_acked_reference_stream() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    assert!(!g.is_acked_ref_stream(), "premise: an IDR does not");
    assert!(fed(g.on_frame(&st_mark_target(2, 1, 1), 0)));
    assert!(g.is_acked_ref_stream());
    // And so a refusal waits for the server's repair instead of asking for
    // an IDR at once, as it would on a legacy stream.
    assert_eq!(
        g.on_frame(&st_mark_target(4, 3, 1), 10),
        Verdict::Refuse(RefuseCause::NoReference)
    );
    assert_eq!(g.take_request(), None);
}

#[test]
fn a_short_term_target_must_be_good_and_the_newest_reference() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    assert!(fed(g.on_frame(&st(2, 1), 0)));
    // 1 is good but 2 is the newest reference now.
    assert_eq!(
        g.on_frame(&st(3, 1), 0),
        Verdict::Refuse(RefuseCause::NoReference)
    );
    // A frame never fed is not good.
    assert_eq!(
        g.on_frame(&st(4, 3), 0),
        Verdict::Refuse(RefuseCause::NoReference)
    );
    assert!(fed(g.on_frame(&st(5, 2), 0)));
}

#[test]
fn a_non_reference_frame_never_becomes_the_newest_reference() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    assert!(fed(g.on_frame(&st(2, 1), 0)));
    assert!(fed(g.on_frame(&recovery(3, 0, 1), 0)));
    assert!(g.is_good(3), "decoded, so counted as good ...");
    // ... but a short-term target on it is refused: it is not in the DPB.
    assert_eq!(
        g.on_frame(&st(4, 3), 0),
        Verdict::Refuse(RefuseCause::NoReference)
    );
    assert!(
        fed(g.on_frame(&st(5, 2), 0)),
        "2 is still the newest reference"
    );
    // A re-root reference frame takes the place of 2.
    assert!(fed(g.on_frame(&reroot(6, 0, 1), 0)));
    assert!(fed(g.on_frame(&st(7, 6), 0)));
}

#[test]
fn a_frame_not_newer_than_the_last_fed_is_refused() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    assert!(fed(g.on_frame(&st(2, 1), 0)));
    assert!(fed(g.on_frame(&st(3, 2), 0)));
    // 2 again (a copy) and 3 again: both stale, neither asks for an IDR.
    assert_eq!(
        g.on_frame(&st(2, 1), 0),
        Verdict::Refuse(RefuseCause::Stale)
    );
    assert_eq!(
        g.on_frame(&st(3, 2), 0),
        Verdict::Refuse(RefuseCause::Stale)
    );
    assert_eq!(g.take_request(), None);
    // Gaps are fine: tile frames share the sequence space.
    assert!(fed(g.on_frame(&st(40, 3), 0)));
}

#[test]
fn an_idr_resets_what_was_held() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    assert!(fed(g.on_frame(&st_mark(2, 1, 1), 0)));
    assert!(fed(g.on_frame(&st(3, 2), 0)));
    assert_eq!(
        g.on_frame(&idr(10), 0),
        Verdict::Feed {
            reset_decoder: true
        }
    );
    assert_eq!(g.long_term(0), Some(10));
    assert_eq!(g.long_term(1), None, "the old mark is gone");
    assert!(!g.is_good(3));
    assert_eq!(
        g.on_frame(&st(11, 3), 0),
        Verdict::Refuse(RefuseCause::NoReference)
    );
    assert!(fed(g.on_frame(&st(11, 10), 0)));
}

#[test]
fn a_decode_error_resets_requests_an_idr_paced_and_marks_the_next_feed() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    g.reset(ResetCause::DecoderFailure, 2, 1_000);
    assert_eq!(g.take_request(), Some((ResetReason::DecoderFailure, 2)));
    assert_eq!(
        g.on_frame(&st(3, 1), 1_100),
        Verdict::Refuse(RefuseCause::NoReference)
    );
    // The refusal behind the failure is the same event to the server: it
    // shares the pacing slot and asks for nothing more.
    assert_eq!(g.take_request(), None);
    // Paced: a second failure inside the interval asks for nothing more.
    g.reset(
        ResetCause::DecoderFailure,
        3,
        1_000 + RESET_REQUEST_MIN_INTERVAL_US - 1,
    );
    assert_eq!(g.take_request(), None);
    g.reset(
        ResetCause::DecoderFailure,
        4,
        1_000 + RESET_REQUEST_MIN_INTERVAL_US,
    );
    assert_eq!(g.take_request(), Some((ResetReason::DecoderFailure, 4)));
    assert_eq!(
        g.on_frame(&idr(9), 5_000_000),
        Verdict::Feed {
            reset_decoder: true
        }
    );
}

#[test]
fn resolution_and_idr_causes_ask_for_nothing() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    g.reset(ResetCause::Resolution, 1, 0);
    g.reset(ResetCause::IdrReceived, 1, 0);
    assert_eq!(g.take_request(), None);
    assert!(!g.is_good(1));
}

// --- legacy (removed Phase A) streams ------------------------------------------------

#[test]
fn a_legacy_stream_is_fed_in_order_and_a_missing_reference_asks_for_an_idr_at_once() {
    let mut g = H264Gate::new();
    assert!(fed(g.on_frame(&idr(1), 0)));
    assert!(fed(g.on_frame(&st(2, 1), 1)));
    // 3 was lost; 4 names it.
    assert_eq!(
        g.on_frame(&st(4, 3), 2_000),
        Verdict::Refuse(RefuseCause::NoReference)
    );
    assert_eq!(g.take_request(), Some((ResetReason::GateReset, 4)));
    assert!(!g.is_acked_ref_stream());
    // Everything chained from it is held back, with no further request until
    // the interval passes.
    assert!(!fed(g.on_frame(&st(5, 4), 3_000)));
    assert_eq!(g.take_request(), None);
    assert!(!fed(
        g.on_frame(&st(6, 5), 2_000 + RESET_REQUEST_MIN_INTERVAL_US)
    ));
    assert_eq!(g.take_request(), Some((ResetReason::GateReset, 6)));
    assert!(fed(g.on_frame(&idr(7), 3_000_000)));
    assert!(fed(g.on_frame(&st(8, 7), 3_001_000)));
}

#[test]
fn a_legacy_p_frame_naming_no_reference_is_fed_until_the_gate_waits_for_an_idr() {
    let mut g = H264Gate::new();
    assert!(fed(g.on_frame(&st(1, REF_FRAME_NONE), 0)));
    assert!(fed(g.on_frame(&st(2, REF_FRAME_NONE), 0)));
    assert!(!fed(g.on_frame(&st(4, 3), 0)));
    assert!(
        !fed(g.on_frame(&st(5, REF_FRAME_NONE), 0)),
        "waiting for an IDR"
    );
    assert!(fed(g.on_frame(&idr(6), 0)));
    assert!(fed(g.on_frame(&st(7, REF_FRAME_NONE), 0)));
}

#[test]
fn an_acked_reference_stream_gives_the_server_time_to_repair_before_asking_for_an_idr() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    assert!(fed(g.on_frame(&st_mark(2, 1, 1), 0)));
    assert!(g.is_acked_ref_stream());
    // 3 lost, 4 names it: refused, but the refused bit is the server's cue.
    assert!(!fed(g.on_frame(&st(4, 3), 10_000)));
    assert_eq!(g.take_request(), None);
    assert!(!fed(g.on_frame(&st(5, 4), 20_000)));
    assert_eq!(g.take_request(), None);
    // The repair arrives from the anchor: fed, and the stuck clock restarts.
    assert!(fed(g.on_frame(&recovery(6, 1, 2), 30_000)));
    assert!(!fed(g.on_frame(&st(7, 6), 40_000)));
    assert_eq!(g.take_request(), None);
    // Refused for too long with nothing fed: fall back to an IDR.
    assert!(!fed(g.on_frame(&st(8, 7), 40_000 + ACKED_REF_STUCK_US)));
    assert_eq!(g.take_request(), Some((ResetReason::GateReset, 8)));
}

// --- the frame-ack window -------------------------------------------------------

#[test]
fn the_window_encodes_one_bit_per_frame_back_from_the_newest_handled() {
    let mut w = FrameAckWindow::new();
    w.record(10, true, 0);
    w.record(12, false, 0);
    w.record(11, true, 0); // out of order, still inside the window
    let ack = w.take_due(0).unwrap();
    assert_eq!(ack.newest_seq, 12);
    assert_eq!(ack.decoded_mask, 0b110);
    assert_eq!(ack.refused_mask, 0b001);
    assert!(ack.is_decoded(10) && ack.is_decoded(11) && ack.is_refused(12));
}

#[test]
fn a_run_of_refusals_advances_newest_seq_so_none_fall_off_unreported() {
    let mut w = FrameAckWindow::new();
    w.record(100, true, 0);
    for s in 101..=110 {
        w.record(s, false, 0);
    }
    let ack = w.take_due(0).unwrap();
    assert_eq!(ack.newest_seq, 110);
    assert_eq!(ack.refused_mask, (1 << 10) - 1);
    assert_eq!(ack.decoded_mask, 1 << 10, "frame 100 is ten behind");
}

#[test]
fn the_window_slides_out_old_frames_and_forgets_a_jump_of_64_or_more() {
    let mut w = FrameAckWindow::new();
    w.record(0, true, 0);
    w.record(63, true, 0);
    let ack = w.take_due(0).unwrap();
    assert_eq!(ack.decoded_mask, 1 | (1 << 63));
    w.record(64, true, 0);
    let ack = w.take_due(FRAME_ACK_MIN_INTERVAL_US).unwrap();
    assert_eq!(ack.decoded_mask, 1 | (1 << 1), "frame 0 fell off");
    w.record(1000, false, 0);
    let ack = w.take_due(2 * FRAME_ACK_MIN_INTERVAL_US).unwrap();
    assert_eq!((ack.decoded_mask, ack.refused_mask), (0, 1));
    // A frame already older than the window is not reported at all.
    w.record(5, true, 0);
    assert_eq!(w.poll_timeout(), None);
}

#[test]
fn a_decode_outranks_a_later_refusal_of_the_same_frame() {
    let mut w = FrameAckWindow::new();
    w.record(7, true, 0);
    w.record(7, false, 0); // a duplicate copy refused as stale
    let ack = w.take_due(0).unwrap();
    assert_eq!((ack.decoded_mask, ack.refused_mask), (1, 0));
}

#[test]
fn acks_are_throttled_to_one_per_interval_and_the_fallback_has_the_only_deadline() {
    let mut w = FrameAckWindow::new();
    assert_eq!(w.poll_timeout(), None);
    w.record(1, true, 1_000);
    assert_eq!(w.poll_timeout(), Some(1_000 + FRAME_ACK_FALLBACK_US));
    assert!(w.take_due(1_000).is_some(), "the first goes at once");
    assert_eq!(w.poll_timeout(), None);
    w.record(2, true, 1_500);
    assert!(w.take_due(1_000 + FRAME_ACK_MIN_INTERVAL_US - 1).is_none());
    let ack = w.take_due(1_000 + FRAME_ACK_MIN_INTERVAL_US).unwrap();
    assert_eq!(ack.decoded_mask, 0b11, "cumulative, not just the new frame");
    assert!(w.take_due(1_000_000).is_none(), "nothing new, nothing sent");
}

#[test]
fn an_unreported_frame_goes_out_alone_only_after_the_fallback_delay() {
    let mut w = FrameAckWindow::new();
    w.record(5, true, 10_000);
    w.record(6, true, 20_000);
    assert!(w.take_overdue(10_000 + FRAME_ACK_FALLBACK_US - 1).is_none());
    let ack = w.take_overdue(10_000 + FRAME_ACK_FALLBACK_US).unwrap();
    assert_eq!(ack.newest_seq, 6, "the delay counts from the oldest");
    assert!(w.take_overdue(1_000_000_000).is_none());
}

#[test]
fn the_window_orders_frames_across_the_31_bit_wrap_as_the_server_reads_it() {
    let mut w = FrameAckWindow::new();
    w.record(0x7FFF_FFFE, true, 0);
    w.record(0x7FFF_FFFF, false, 0);
    w.record(1, true, 0); // two on the far side of the wrap
    let ack = w.take_due(0).unwrap();
    assert_eq!(ack.newest_seq, 1);
    // bit i is newest - i in 31-bit order: 1, 0, 0x7FFF_FFFF, 0x7FFF_FFFE
    assert_eq!(ack.decoded_mask, 0b1001);
    assert_eq!(ack.refused_mask, 0b0100);
    assert!(ack.is_decoded(0x7FFF_FFFE) && ack.is_refused(0x7FFF_FFFF));
    // A straggler from before the wrap is older, not newer.
    w.record(0x7FFF_FFFD, true, 0);
    let ack = w.take_due(1_000_000).unwrap();
    assert_eq!(ack.newest_seq, 1);
    assert_eq!(ack.decoded_mask, 0b11001);
}

// --- no decoder, and a single pacing slot ---------------------------------------

#[test]
fn a_gate_reset_never_delays_a_decoder_failure_but_a_failure_holds_back_gate_resets() {
    // The server acts only on DecoderFailure and ignores GateReset.
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    assert!(!fed(g.on_frame(&st(3, 2), 1_000)));
    assert_eq!(g.take_request(), Some((ResetReason::GateReset, 3)));
    g.reset(ResetCause::DecoderFailure, 4, 2_000);
    assert_eq!(
        g.take_request(),
        Some((ResetReason::DecoderFailure, 4)),
        "an earlier GateReset must not swallow a DecoderFailure"
    );
    // Now the other direction: a GateReset right after the failure is held back.
    assert!(!fed(g.on_frame(&st(6, 5), 3_000)));
    assert_eq!(g.take_request(), None);
    // And it comes back once the failure's window has passed.
    assert!(!fed(
        g.on_frame(&st(7, 6), 2_000 + RESET_REQUEST_MIN_INTERVAL_US)
    ));
    assert_eq!(g.take_request(), Some((ResetReason::GateReset, 7)));
}

#[test]
fn no_decoder_is_asked_for_once_and_silences_gate_resets() {
    let mut g = H264Gate::new();
    g.on_frame(&idr(1), 0);
    g.reset(ResetCause::NoDecoder, 1, 0);
    assert_eq!(g.take_request(), Some((ResetReason::NoDecoder, 1)));
    // Every later AU: another reset, another refusal -- and no request, however
    // long it goes on.
    for i in 0..10u64 {
        let now = i * RESET_REQUEST_MIN_INTERVAL_US;
        g.reset(ResetCause::NoDecoder, 2 + i as u32, now);
        assert!(!fed(g.on_frame(&st(100 + i as u32, 50), now)));
        assert_eq!(g.take_request(), None, "round {i}");
    }
}

// --- property: the gate never feeds a frame whose reference it does not hold ----

/// A stand-in for the lib crate's `RefManager`, which client-core cannot
/// depend on: it emits the header shapes that planner emits (IDR, healthy P
/// with an optional mark, recovery P, re-root P), with the named reference
/// drawn from the sender's belief, and sometimes from a wrong one. Whether
/// the client got the frame is a separate coin flip.
#[derive(Debug, Clone)]
enum Step {
    Idr,
    Healthy {
        mark: Option<u8>,
    },
    Recovery {
        slot: u8,
    },
    Reroot {
        slot: u8,
        mark: Option<u8>,
    },
    /// A header whose reference the sender got wrong.
    Wrong {
        slot: Option<u8>,
        back: u32,
    },
}

fn step_strategy() -> impl Strategy<Value = Step> {
    prop_oneof![
        1 => Just(Step::Idr),
        6 => proptest::option::of(0u8..2).prop_map(|mark| Step::Healthy { mark }),
        2 => (0u8..2).prop_map(|slot| Step::Recovery { slot }),
        1 => (0u8..2, proptest::option::of(0u8..2))
            .prop_map(|(slot, mark)| Step::Reroot { slot, mark }),
        1 => (proptest::option::of(0u8..3), 1u32..6)
            .prop_map(|(slot, back)| Step::Wrong { slot, back }),
    ]
}

/// The client's decoder DPB, built only from the frames actually fed to it.
#[derive(Default)]
struct Dpb {
    newest_ref: Option<u32>,
    lt: [Option<u32>; 2],
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn the_gate_never_feeds_a_frame_whose_reference_the_decoder_does_not_hold(
        steps in proptest::collection::vec((step_strategy(), 0u32..4, any::<bool>()), 1..120),
    ) {
        let mut gate = H264Gate::new();
        let mut dpb = Dpb::default();
        // The sender's belief: its newest reference and its slots.
        let mut sent_newest: Option<u32> = None;
        let mut sent_lt: [Option<u32>; 2] = [None; 2];
        let mut seq = 0u32;
        for (i, (step, gap, lost)) in steps.into_iter().enumerate() {
            seq += 1 + gap; // tile frames share the sequence space
            let slot_ref = |slot: u8, lt: &[Option<u32>; 2]| {
                lt.get(usize::from(slot)).copied().flatten().unwrap_or(REF_FRAME_NONE)
            };
            let h = match step {
                Step::Idr => idr(seq),
                Step::Healthy { mark } => {
                    let r = sent_newest.unwrap_or(REF_FRAME_NONE);
                    match mark {
                        Some(k) => st_mark(seq, r, k),
                        None => st(seq, r),
                    }
                }
                Step::Recovery { slot } => recovery(seq, slot, slot_ref(slot, &sent_lt)),
                Step::Reroot { slot, mark } => {
                    let mut h = reroot(seq, slot, slot_ref(slot, &sent_lt));
                    if let Some(k) = mark {
                        h.flags |= FRAME_FLAG_MARK_LT;
                        h.slots |= k;
                    }
                    h
                }
                Step::Wrong { slot, back } => match slot {
                    Some(s) => recovery(seq, s, seq.saturating_sub(back)),
                    None => st(seq, seq.saturating_sub(back)),
                },
            };
            // The sender's belief advances as if the frame arrived.
            if h.flags & FRAME_FLAG_IDR != 0 {
                sent_newest = Some(seq);
                sent_lt = [Some(seq), None];
            } else if h.flags & FRAME_FLAG_NON_REF == 0 && !matches!(step, Step::Wrong { .. }) {
                sent_newest = Some(seq);
                if h.flags & FRAME_FLAG_MARK_LT != 0 {
                    sent_lt[usize::from(h.slots & 1)] = Some(seq);
                }
            }
            if lost && i > 0 {
                continue; // never reached the client
            }
            if let Verdict::Feed { .. } = gate.on_frame(&h, i as u64 * 16_000) {
                if h.flags & FRAME_FLAG_IDR == 0 && h.ref_frame_seq != REF_FRAME_NONE {
                    // The decoder must hold exactly the picture the header names.
                    if h.flags & FRAME_FLAG_TARGET_IS_LT != 0 {
                        let slot = usize::from((h.slots >> 2) & 3);
                        prop_assert!(
                            slot < 2 && dpb.lt[slot] == Some(h.ref_frame_seq),
                            "fed {h:?} but the slot holds {:?}",
                            dpb.lt.get(slot)
                        );
                    } else {
                        prop_assert_eq!(
                            dpb.newest_ref,
                            Some(h.ref_frame_seq),
                            "fed {:?} but the newest reference is {:?}",
                            h,
                            dpb.newest_ref
                        );
                    }
                }
                // The decoder takes the frame.
                if h.flags & FRAME_FLAG_IDR != 0 {
                    dpb = Dpb { newest_ref: Some(seq), lt: [Some(seq), None] };
                } else if h.flags & FRAME_FLAG_NON_REF == 0 {
                    dpb.newest_ref = Some(seq);
                    if h.flags & FRAME_FLAG_MARK_LT != 0 {
                        dpb.lt[usize::from(h.slots & 3)] = Some(seq);
                    }
                }
            }
            let _ = gate.take_request();
        }
    }

    /// Liveness: a stream whose every frame arrives, with a correct sender
    /// belief, is never refused.
    #[test]
    fn an_intact_stream_is_never_refused(
        steps in proptest::collection::vec(
            (0u8..4, proptest::option::of(0u8..2), 0u32..4), 1..100),
    ) {
        let mut gate = H264Gate::new();
        let mut newest: u32 = 1;
        let mut lt = [Some(1u32), None];
        prop_assert!(fed(gate.on_frame(&idr(1), 0)));
        let mut seq = 1u32;
        for (kind, mark, gap) in steps {
            seq += 1 + gap;
            let h = match kind {
                0 | 1 => match mark {
                    Some(k) => st_mark(seq, newest, k),
                    None => st(seq, newest),
                },
                2 => match lt[0] {
                    Some(r) => recovery(seq, 0, r),
                    None => continue,
                },
                _ => match lt[0] {
                    Some(r) => reroot(seq, 0, r),
                    None => continue,
                },
            };
            prop_assert!(fed(gate.on_frame(&h, 0)), "refused {h:?}");
            if h.flags & FRAME_FLAG_NON_REF == 0 {
                newest = seq;
                if h.flags & FRAME_FLAG_MARK_LT != 0 {
                    lt[usize::from(h.slots & 3)] = Some(seq);
                }
            }
        }
    }
}
