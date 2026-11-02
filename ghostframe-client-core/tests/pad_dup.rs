//! A copy of a fragment the client already holds (server probe padding, or any
//! duplicate) must be acknowledged and must never decode a frame twice.

use ghostframe_client_core::{ClientConfig, ClientCore, Event, PollOutput};
use ghostframe_protocol::ack::AckBatch;
use ghostframe_protocol::protocol::{fragment_frame, FRAME_FLAG_PAD};

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

fn needs(evs: &[Event]) -> Vec<u32> {
    evs.iter()
        .filter_map(|e| match e {
            Event::NeedsH264 { frame_seq, .. } => Some(*frame_seq),
            _ => None,
        })
        .collect()
}

/// IDR 1, P 2 and P 3 delivered; a copy of P 2's only fragment follows.
fn delivered_three() -> (ClientCore, Vec<u8>) {
    let mut c = core();
    let idr = fragment_frame(1, 0, true, &[1u8; 900], 1200);
    let p2 = fragment_frame(2, 0, false, &[2u8; 900], 1200);
    let p3 = fragment_frame(3, 0, false, &[3u8; 900], 1200);
    assert_eq!(p2.len(), 1, "premise: a single-fragment P frame");
    let mut n = 0;
    for dg in idr.iter().chain(&p2).chain(&p3) {
        n += needs(&c.handle_datagram(dg, 0)).len();
    }
    assert_eq!(n, 3, "premise: all three delivered");
    (c, p2[0].clone())
}

/// The wire_seqs acknowledged so far, once the batch timer has run.
fn acked(core: &mut ClientCore, now_us: u64) -> Vec<u32> {
    core.on_timeout(now_us);
    let mut out = Vec::new();
    while let Some(o) = core.poll_transmit(now_us) {
        if let PollOutput::Datagram(d) = o {
            if d.first() == Some(&0x06) {
                out.extend(
                    AckBatch::decode(&d)
                        .unwrap()
                        .entries
                        .iter()
                        .map(|e| e.wire_seq),
                );
            }
        }
    }
    out
}

#[test]
fn an_unflagged_copy_of_a_delivered_frame_is_not_decoded_again() {
    // Side (b): any fragment at or before the newest delivered frame is moot.
    let (mut c, p2) = delivered_three();
    let mut copy = p2;
    copy[8..12].copy_from_slice(&777u32.to_be_bytes());
    let evs = c.handle_datagram(&copy, 1000);
    assert!(needs(&evs).is_empty(), "re-emitted {:?}", needs(&evs));
}

#[test]
fn a_flagged_pad_is_acknowledged_and_touches_nothing() {
    // Side (a): the pad flag. Use a frame that is NOT yet delivered: a pad copy
    // of a fragment of frame 4 must neither start nor complete it.
    let mut c = core();
    let idr = fragment_frame(1, 0, true, &[1u8; 900], 1200);
    for dg in &idr {
        c.handle_datagram(dg, 0);
    }
    let p4 = fragment_frame(4, 0, false, &[4u8; 900], 1200);
    let mut pad = p4[0].clone();
    pad[8..12].copy_from_slice(&778u32.to_be_bytes());
    pad[16] |= FRAME_FLAG_PAD;
    let evs = c.handle_datagram(&pad, 1000);
    assert!(needs(&evs).is_empty(), "a pad completed a frame");
    assert!(
        acked(&mut c, 1_000_000).contains(&778),
        "the pad is acknowledged"
    );
    // And frame 4 still arrives normally afterwards (the pad created no
    // assembly that would swallow it).
    let evs = c.handle_datagram(&p4[0], 2000);
    assert_eq!(needs(&evs), vec![4]);
}

#[test]
fn a_pad_is_acknowledged_even_when_it_is_discarded() {
    let (mut c, p2) = delivered_three();
    let _ = acked(&mut c, 0);
    let mut pad = p2;
    pad[8..12].copy_from_slice(&779u32.to_be_bytes());
    pad[16] |= FRAME_FLAG_PAD;
    c.handle_datagram(&pad, 10_000_000);
    assert!(
        acked(&mut c, 20_000_000).contains(&779),
        "the pad must be acknowledged: the acknowledgement is the measurement"
    );
}

#[test]
fn a_duplicate_of_the_newest_delivered_idr_is_not_decoded_again() {
    let mut c = core();
    let idr = fragment_frame(1, 0, true, &[1u8; 900], 1200);
    assert_eq!(needs(&c.handle_datagram(&idr[0], 0)), vec![1]);
    // The same IDR again (a resend that crossed its acknowledgement): the
    // decoder must not restart from it.
    let mut copy = idr[0].clone();
    copy[8..12].copy_from_slice(&900u32.to_be_bytes());
    assert!(needs(&c.handle_datagram(&copy, 1_000)).is_empty());
}
