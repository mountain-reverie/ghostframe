use ghostframe_client_core::loss_tracker::{
    encode_display_info, encode_display_mode, encode_hello, LossTracker,
};
use ghostframe_protocol::feedback::ReceiverFeedback;

#[test]
fn hello_indices_raw_only() {
    assert_eq!(encode_hello(true, false), vec![0x03, 0x01]);
}

#[test]
fn hello_nothing_set() {
    assert_eq!(encode_hello(false, false), vec![0x03, 0x00]);
}

#[test]
fn hello_h264_only() {
    assert_eq!(encode_hello(false, true), vec![0x03, 0x02]);
}

#[test]
fn hello_both_caps() {
    assert_eq!(encode_hello(true, true), vec![0x03, 0x03]);
}

#[test]
fn loss_tracker_round_trip() {
    let mut t = LossTracker::new();
    t.on_datagram_sent_at(1_000, 1_000);
    t.on_datagram_sent_at(2_000, 2_000);
    t.on_datagram_sent_at(3_000, 3_000);
    t.on_stale_tile(5, 3); // 2 lost
    t.on_fec_recovery();

    let buf = t.encode_feedback(5_000_000);
    assert_eq!(buf.len(), 22);

    let fb = ReceiverFeedback::decode(&buf).expect("decode failed");
    assert_eq!(fb.timestamp_ns, 5_000_000_000);
    assert_eq!(fb.datagrams_received, 3);
    assert_eq!(fb.datagrams_lost, 2);
    assert_eq!(fb.datagrams_recovered_fec, 1);
    assert!(!fb.suspension_detected);

    // Counters reset after encode.
    let buf2 = t.encode_feedback(6_000_000);
    let fb2 = ReceiverFeedback::decode(&buf2).expect("decode failed");
    assert_eq!(fb2.datagrams_received, 0);
    assert_eq!(fb2.datagrams_lost, 0);
    assert_eq!(fb2.datagrams_recovered_fec, 0);
    assert!(!fb2.suspension_detected);
}

fn suspension_after(t: &mut LossTracker, now_us: u64) -> bool {
    ReceiverFeedback::decode(&t.encode_feedback(now_us))
        .expect("decode failed")
        .suspension_detected
}

/// The path held a datagram: sent 10 ms after its predecessor, delivered
/// 150 ms after it.
#[test]
fn a_stall_in_the_path_is_a_suspension() {
    let mut t = LossTracker::new();
    t.on_datagram_sent_at(1_000, 500_000);
    t.on_datagram_sent_at(151_000, 510_000);
    assert!(suspension_after(&mut t, 200_000));
}

/// The defect: a sender with nothing to send is not a suspended path. A
/// static screen for 9.4 s, then one change, is two datagrams 9.4 s apart
/// at both ends.
#[test]
fn a_sender_that_was_idle_is_not_a_suspension() {
    let mut t = LossTracker::new();
    t.on_datagram_sent_at(1_000, 500_000);
    t.on_datagram_sent_at(9_411_000, 9_910_000);
    assert!(
        !suspension_after(&mut t, 9_500_000),
        "9.4 s of silence that the sender's own stamps account for was \
         reported as a suspension; the server forces H.264 on that flag"
    );
}

/// Tile-mode frames 85 ms apart, arriving with 20 ms of jitter. Over the
/// old 100 ms arrival-gap threshold; nowhere near a stall.
#[test]
fn a_slow_sender_with_ordinary_jitter_is_not_a_suspension() {
    let mut t = LossTracker::new();
    t.on_datagram_sent_at(0, 0);
    t.on_datagram_sent_at(105_000, 85_000);
    t.on_datagram_sent_at(190_000, 170_000);
    assert!(!suspension_after(&mut t, 200_000));
}

/// The threshold is on the excess, and exclusive.
#[test]
fn the_threshold_is_on_arrival_gap_minus_send_gap() {
    let mut t = LossTracker::new();
    t.on_datagram_sent_at(0, 0);
    t.on_datagram_sent_at(130_000, 30_000); // excess exactly 100 ms
    assert!(!suspension_after(&mut t, 140_000));
    t.on_datagram_sent_at(260_001, 60_000); // excess 100.001 ms
    assert!(suspension_after(&mut t, 270_000));
}

/// A retransmission or FEC replay carries an older stamp and arrives late
/// by construction. It is neither a stall nor a new baseline.
#[test]
fn an_older_stamp_neither_flags_nor_moves_the_baseline() {
    let mut t = LossTracker::new();
    t.on_datagram_sent_at(0, 1_000_000);
    t.on_datagram_sent_at(10_000, 1_010_000);
    // Sent before both, delivered 300 ms later.
    t.on_datagram_sent_at(310_000, 900_000);
    assert!(!suspension_after(&mut t, 320_000));
    // Judged against the 1_010_000 baseline, not the replay: sent 320 ms
    // after it, arrived 320 ms after it.
    t.on_datagram_sent_at(330_000, 1_330_000);
    assert!(!suspension_after(&mut t, 340_000));
}

/// The sender's stamp wraps every 71.6 minutes.
#[test]
fn the_sender_stamp_wrapping_is_a_small_step_forward() {
    let mut t = LossTracker::new();
    t.on_datagram_sent_at(0, u32::MAX - 4_999);
    t.on_datagram_sent_at(10_000, 5_000); // 10 ms later on both clocks
    assert!(!suspension_after(&mut t, 20_000));
    t.on_datagram_sent_at(170_000, 15_000); // sent +10 ms, arrived +160 ms
    assert!(suspension_after(&mut t, 180_000));
}

/// A datagram with no sender stamp is counted and says nothing else.
#[test]
fn an_untimed_datagram_is_counted_but_cannot_flag() {
    let mut t = LossTracker::new();
    t.on_datagram_sent_at(0, 0);
    t.on_datagram(5_000_000);
    let fb = ReceiverFeedback::decode(&t.encode_feedback(5_100_000)).expect("decode failed");
    assert_eq!(fb.datagrams_received, 2);
    assert!(!fb.suspension_detected);
    // Nor did it become the baseline: this is 10 ms after the first on both
    // clocks, whatever the sentinel in between did.
    t.on_datagram_sent_at(5_010_000, 5_010_000);
    assert!(!suspension_after(&mut t, 5_100_000));
}

// Byte-exact oracle tests for DisplayInfo/DisplayMode: client-core cannot
// depend on ghostframe-lib (the server crate), so its encoders are
// duplicated there. These pin the exact wire bytes so a change on one side
// without the other fails loudly, mirroring ghostframe-lib's
// `transport::display` decode tests.
#[test]
fn display_info_wire_bytes() {
    assert_eq!(
        encode_display_info(2560, 1440, 1500, 597, 336),
        vec![0x07, 0x0A, 0x00, 0x05, 0xA0, 0x05, 0xDC, 0x02, 0x55, 0x01, 0x50]
    );
}

#[test]
fn display_mode_wire_bytes() {
    assert_eq!(
        encode_display_mode(1280, 800),
        vec![0x08, 0x05, 0x00, 0x03, 0x20]
    );
}
