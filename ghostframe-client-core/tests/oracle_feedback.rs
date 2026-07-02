use ghostframe_client_core::loss_tracker::{encode_hello, LossTracker};
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
    t.on_datagram(1_000);
    t.on_datagram(2_000);
    t.on_datagram(3_000);
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

#[test]
fn loss_tracker_suspension_detected_after_gap_over_100ms() {
    let mut t = LossTracker::new();
    t.on_datagram(1_000);
    // Gap of 150ms > 100ms threshold.
    t.on_datagram(151_000);
    let buf = t.encode_feedback(200_000);
    let fb = ReceiverFeedback::decode(&buf).expect("decode failed");
    assert!(fb.suspension_detected);
}

#[test]
fn loss_tracker_no_suspension_within_100ms_gap() {
    let mut t = LossTracker::new();
    t.on_datagram(0);
    t.on_datagram(50_000);
    let buf = t.encode_feedback(100_000);
    let fb = ReceiverFeedback::decode(&buf).expect("decode failed");
    assert!(!fb.suspension_detected);
}
