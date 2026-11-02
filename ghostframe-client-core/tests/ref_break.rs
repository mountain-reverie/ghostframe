//! A P frame whose reference was never delivered is not handed to the decoder:
//! with a 3 s GOP a silent reference break leaves the picture wrong for
//! seconds. The client holds P frames back, asks for an IDR (paced), and
//! resumes at the next one.

use ghostframe_client_core::{ClientConfig, ClientCore, Event, PollOutput, ResetReason};
use ghostframe_protocol::feedback_stream::ResetRequest;
use ghostframe_protocol::protocol::{fragment_frame_ref, FrameRefInfo};

fn core() -> ClientCore {
    let mut c = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: true,
            supports_h264: true,
            ..Default::default()
        },
        0,
    );
    while c.poll_transmit(0).is_some() {}
    c
}

fn send(c: &mut ClientCore, seq: u32, idr: bool, reference: u32, now_us: u64) -> bool {
    let info = if idr {
        FrameRefInfo::IDR
    } else {
        FrameRefInfo::predicted(reference)
    };
    let mut delivered = false;
    for dg in fragment_frame_ref(seq, 0, info, &[seq as u8; 600], 1200) {
        delivered |= c
            .handle_datagram(&dg, now_us)
            .iter()
            .any(|e| matches!(e, Event::NeedsH264 { frame_seq, .. } if *frame_seq == seq));
    }
    delivered
}

/// The reset requests queued so far, as `(reason, frame_seq)`.
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
fn a_chain_that_is_intact_is_delivered_and_asks_for_nothing() {
    let mut c = core();
    assert!(send(&mut c, 1, true, 0, 0));
    assert!(send(&mut c, 2, false, 1, 1_000));
    assert!(send(&mut c, 3, false, 2, 2_000));
    assert!(resets(&mut c).is_empty());
}

#[test]
fn a_p_frame_whose_reference_was_never_delivered_is_held_back_and_an_idr_is_asked_for() {
    let mut c = core();
    assert!(send(&mut c, 1, true, 0, 0));
    // Frame 2 never arrives; 3 names it.
    assert!(
        !send(&mut c, 3, false, 2, 1_000),
        "not handed to the decoder"
    );
    assert_eq!(resets(&mut c), vec![(ResetReason::GateReset, 3)]);
    // The frames chained from it are held back too, without more requests.
    assert!(!send(&mut c, 4, false, 3, 2_000));
    assert!(!send(&mut c, 5, false, 4, 3_000));
    assert!(resets(&mut c).is_empty(), "paced");
    // An IDR ends it, and the chain from the IDR is delivered.
    assert!(send(&mut c, 6, true, 0, 4_000));
    assert!(send(&mut c, 7, false, 6, 5_000));
    assert!(resets(&mut c).is_empty());
}

#[test]
fn the_request_is_repeated_only_after_the_pacing_interval() {
    let mut c = core();
    assert!(send(&mut c, 1, true, 0, 0));
    assert!(!send(&mut c, 3, false, 2, 1_000));
    assert_eq!(resets(&mut c).len(), 1);
    assert!(!send(&mut c, 4, false, 3, 2_199_000));
    assert!(resets(&mut c).is_empty(), "inside 2.2 s of the first");
    assert!(!send(&mut c, 5, false, 4, 2_201_001));
    assert_eq!(
        resets(&mut c),
        vec![(ResetReason::GateReset, 5)],
        "after it"
    );
}
