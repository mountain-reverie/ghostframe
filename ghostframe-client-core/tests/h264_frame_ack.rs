//! Frame acks (datagram 0x0A) out of `ClientCore`: what the gate refuses and
//! what the decoder reports reach the wire as the cumulative masks, riding the
//! ack batch (never a packet of their own), on the caller's clock (no
//! wall-clock reads: `now_us` only).

use ghostframe_client_core::h264_gate::FRAME_ACK_FALLBACK_US;
use ghostframe_client_core::{
    ClientConfig, ClientCore, Event, H264Outcome, PollOutput, ResetReason,
};
use ghostframe_protocol::feedback_stream::ResetRequest;
use ghostframe_protocol::protocol::{fragment_frame_ref, FrameAck, FrameRefInfo};

fn core(acked_ref: bool) -> ClientCore {
    let mut c = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: true,
            supports_h264: true,
            h264_acked_ref: acked_ref,
            ..Default::default()
        },
        0,
    );
    while c.poll_transmit(0).is_some() {}
    c
}

/// Deliver a one-fragment frame whose datagram is tracked by the ledger
/// (`wire_seq` = `seq`), so the core owes the server an ack batch for it.
/// Returns whether the core handed it out for decoding.
fn send(c: &mut ClientCore, seq: u32, idr: bool, reference: u32, now_us: u64) -> bool {
    let info = if idr {
        FrameRefInfo::IDR
    } else {
        FrameRefInfo::predicted(reference)
    };
    let mut fed = false;
    for mut dg in fragment_frame_ref(seq, 0, info, &[seq as u8; 600], 1200) {
        dg[8..12].copy_from_slice(&seq.to_be_bytes());
        fed |= c
            .handle_datagram(&dg, now_us)
            .iter()
            .any(|e| matches!(e, Event::NeedsH264 { frame_seq, .. } if *frame_seq == seq));
    }
    fed
}

/// Everything queued, in order: `(first byte, frame ack if it is one)`.
fn drain(c: &mut ClientCore) -> Vec<(u8, Option<FrameAck>)> {
    let mut out = Vec::new();
    while let Some(o) = c.poll_transmit(0) {
        if let PollOutput::Datagram(d) = o {
            out.push((d[0], (d[0] == 0x0a).then(|| FrameAck::decode(&d).unwrap())));
        }
    }
    out
}

fn acks(c: &mut ClientCore) -> Vec<FrameAck> {
    drain(c).into_iter().filter_map(|(_, a)| a).collect()
}

fn resets(c: &mut ClientCore) -> Vec<(ResetReason, u32)> {
    let mut out = Vec::new();
    while let Some(o) = c.poll_transmit(0) {
        if let PollOutput::Stream(b) = o {
            if let Some(r) = ResetRequest::decode(&b) {
                out.push((r.reason, r.frame_seq));
            }
        }
    }
    out
}

#[test]
fn a_frame_ack_leaves_right_behind_the_ack_batch_and_never_before_it() {
    let mut c = core(true);
    assert!(send(&mut c, 1, true, 0, 0));
    c.h264_result(1, H264Outcome::Decoded, 100);
    assert!(drain(&mut c).is_empty(), "no packet of its own");
    c.on_timeout(5_000);
    let out = drain(&mut c);
    assert_eq!(
        out.iter().map(|(b, _)| *b).collect::<Vec<_>>(),
        vec![0x06, 0x0a],
        "the batch, then the frame ack in the same drain"
    );
    let a = out[1].1.unwrap();
    assert_eq!((a.newest_seq, a.decoded_mask, a.refused_mask), (1, 1, 0));
}

#[test]
fn with_no_batch_to_ride_the_ack_goes_alone_after_the_fallback_delay() {
    let mut c = core(true);
    assert!(send(&mut c, 1, true, 0, 0));
    c.on_timeout(5_000);
    drain(&mut c); // the batch for frame 1's datagram
    c.h264_result(1, H264Outcome::Decoded, 6_000);
    assert!(
        c.poll_timeout().unwrap() <= 6_000 + FRAME_ACK_FALLBACK_US,
        "the core asks to be woken for the fallback"
    );
    c.on_timeout(6_000 + FRAME_ACK_FALLBACK_US - 1);
    assert!(acks(&mut c).is_empty());
    c.on_timeout(6_000 + FRAME_ACK_FALLBACK_US);
    let a = acks(&mut c);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].decoded_mask, 1);
}

#[test]
fn a_frame_the_gate_refuses_is_acknowledged_as_refused_without_a_decoder() {
    let mut c = core(true);
    assert!(send(&mut c, 1, true, 0, 0));
    c.h264_result(1, H264Outcome::Decoded, 100);
    // 2 never arrives; 3 names it.
    assert!(!send(&mut c, 3, false, 2, 10_000));
    c.on_timeout(15_000);
    let a = acks(&mut c);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].newest_seq, 3);
    assert!(a[0].is_refused(3) && a[0].is_decoded(1) && !a[0].is_decoded(3));
}

#[test]
fn acks_are_throttled_to_one_per_4_ms() {
    let mut c = core(true);
    assert!(send(&mut c, 1, true, 0, 1_000));
    c.h264_result(1, H264Outcome::Decoded, 1_000);
    c.on_timeout(6_000);
    assert_eq!(acks(&mut c).len(), 1);
    assert!(send(&mut c, 2, false, 1, 6_500));
    c.h264_result(2, H264Outcome::Decoded, 6_500);
    // A batch inside 4 ms of the last frame ack does not carry one.
    c.on_timeout(9_999);
    assert!(acks(&mut c).is_empty());
    c.on_timeout(11_500);
    let a = acks(&mut c);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].decoded_mask, 0b11, "cumulative");
}

#[test]
fn a_client_that_did_not_say_it_sends_acks_sends_none() {
    let mut c = core(false);
    assert!(send(&mut c, 1, true, 0, 0));
    c.h264_result(1, H264Outcome::Decoded, 0);
    c.on_timeout(5_000);
    c.on_timeout(100_000);
    assert!(acks(&mut c).is_empty());
}

#[test]
fn a_decoder_with_one_frame_of_output_delay_keeps_the_legacy_stream_flowing() {
    // Every result is NotOutput (the call returned the previous frame's
    // picture). The decoder still holds every frame, so nothing is evicted:
    // each P names the frame before it and is fed.
    let mut c = core(true);
    assert!(send(&mut c, 1, true, 0, 0));
    c.h264_result(1, H264Outcome::NotOutput, 0);
    for seq in 2..=40u32 {
        let t = u64::from(seq) * 16_000;
        assert!(
            send(&mut c, seq, false, seq - 1, t),
            "frame {seq} was held back"
        );
        c.h264_result(seq, H264Outcome::NotOutput, t);
    }
    assert!(resets(&mut c).is_empty(), "and no IDR was asked for");
    assert_eq!(c.h264_not_output_stats(), (40, 40));
    c.h264_result(41, H264Outcome::Decoded, 700_000);
    assert_eq!(c.h264_not_output_stats(), (40, 0), "the run ends");
}

#[test]
fn after_a_decode_error_frames_still_in_the_pipeline_are_refused_until_the_reset() {
    let mut c = core(true);
    assert!(send(&mut c, 1, true, 0, 0));
    c.h264_result(1, H264Outcome::Decoded, 0);
    assert!(send(&mut c, 2, false, 1, 10_000));
    assert!(send(&mut c, 3, false, 2, 20_000));
    c.h264_result(2, H264Outcome::DecodeError, 20_000);
    assert_eq!(resets(&mut c), vec![(ResetReason::DecoderFailure, 2)]);
    // Frame 3 was queued behind the error and decoded on the broken decoder.
    c.h264_result(3, H264Outcome::Decoded, 21_000);
    c.on_timeout(30_000);
    let a = acks(&mut c);
    assert!(a[0].is_refused(3) && !a[0].is_decoded(3), "{a:?}");
    assert!(!send(&mut c, 4, false, 3, 40_000), "the gate holds nothing");
    // The IDR that answers carries the reset, and what it decodes counts.
    assert!(send(&mut c, 5, true, 0, 50_000));
    c.h264_result(5, H264Outcome::Decoded, 50_000);
    // A straggler from before that reset is still refused.
    c.h264_result(4, H264Outcome::Decoded, 50_500);
    c.on_timeout(60_000);
    let a = acks(&mut c);
    let last = a.last().unwrap();
    assert!(
        last.is_decoded(5) && !last.is_decoded(4) && last.is_refused(4),
        "{a:?}"
    );
}

#[test]
fn an_idr_on_a_healthy_decoder_does_not_refuse_answers_still_in_flight() {
    // Web no longer rebuilds the decoder on an IDR, so a P fed just before one
    // can be answered after it, and its answer is still good.
    let mut c = core(true);
    assert!(send(&mut c, 1, true, 0, 0));
    c.h264_result(1, H264Outcome::Decoded, 0);
    assert!(send(&mut c, 2, false, 1, 10_000));
    assert!(
        send(&mut c, 3, true, 0, 20_000),
        "a GOP IDR, reset_decoder set"
    );
    c.h264_result(2, H264Outcome::Decoded, 25_000);
    c.h264_result(3, H264Outcome::Decoded, 26_000);
    c.on_timeout(40_000);
    let a = acks(&mut c);
    let last = a.last().unwrap();
    assert!(
        last.is_decoded(2) && last.is_decoded(3) && !last.is_refused(2),
        "{a:?}"
    );
}

#[test]
fn a_copy_of_a_frame_already_fed_is_not_a_refusal() {
    let mut c = core(true);
    assert!(send(&mut c, 1, true, 0, 0));
    // The same IDR completes a second time before the decoder has answered.
    assert!(!send(&mut c, 1, true, 0, 1_000));
    c.on_timeout(1_000 + FRAME_ACK_FALLBACK_US);
    assert!(
        drain(&mut c).iter().all(|(b, _)| *b != 0x0a),
        "no refused bit"
    );
    let t = 1_000 + FRAME_ACK_FALLBACK_US;
    c.h264_result(1, H264Outcome::Decoded, t + 10_000);
    c.on_timeout(t + 10_000 + FRAME_ACK_FALLBACK_US);
    let a = acks(&mut c);
    assert_eq!((a[0].decoded_mask, a[0].refused_mask), (1, 0));
}

#[test]
fn no_decoder_is_sent_once_and_the_refused_stream_asks_for_nothing_more() {
    let mut c = core(true);
    let mut sent = Vec::new();
    for seq in 1..=20u32 {
        let t = u64::from(seq) * 1_000_000;
        // The server sends an IDR now and then, P frames in between.
        let fed = send(&mut c, seq, seq % 5 == 1, seq - 1, t);
        if fed {
            c.h264_result(seq, H264Outcome::NoDecoder, t);
        }
        sent.extend(resets(&mut c));
    }
    assert_eq!(sent, vec![(ResetReason::NoDecoder, 1)], "{sent:?}");
}

#[test]
fn an_idr_event_tells_the_decoder_to_reset_and_a_p_frame_does_not() {
    let mut c = core(true);
    let mut flags = Vec::new();
    for (seq, idr, r) in [(1, true, 0), (2, false, 1)] {
        let info = if idr {
            FrameRefInfo::IDR
        } else {
            FrameRefInfo::predicted(r)
        };
        for dg in fragment_frame_ref(seq, 0, info, &[1u8; 50], 1200) {
            for e in c.handle_datagram(&dg, 0) {
                if let Event::NeedsH264 { reset_decoder, .. } = e {
                    flags.push(reset_decoder);
                }
            }
        }
    }
    assert_eq!(flags, vec![true, false]);
}
