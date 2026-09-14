use ghostframe_client_core::ack_batcher::AckBatcher;
use ghostframe_protocol::ack::{
    AckBatch, AckEntry, ACK_FRESH_ENTRY_SIZE, ACK_HEADER_SIZE, ACK_OVERLAP_COUNT,
};

fn e(frame_seq: u32, tile_x: u8, tile_y: u8, pass_idx: u8, ts: u32) -> AckEntry {
    AckEntry {
        frame_seq,
        tile_x,
        tile_y,
        pass_idx,
        arrival_us: ts,
    }
}

#[test]
fn encodes_msg_type_and_fields() {
    let mut b = AckBatcher::new();
    assert!(b.add(e(0x12345678, 3, 7, 13, 0), 0).is_none());
    let d = b.flush().unwrap();
    assert_eq!(d[0], 0x06);
    assert_eq!(d[1], 1);
    assert_eq!(&d[7..11], &[0x78, 0x56, 0x34, 0x12]); // frame_seq LE
    assert_eq!((d[11], d[12], d[13]), (3, 7, 13)); // tile_x, tile_y, pass_idx
    assert_eq!(&d[14..16], &[0, 0]); // arrival_delta_us
}

#[test]
fn flushes_at_max_entries_without_timer() {
    let mut b = AckBatcher::new();
    let mut sent = None;
    for i in 0..64u32 {
        if let Some(d) = b.add(e(i, 0, 0, 0, 0), 0) {
            sent = Some(d);
        }
    }
    let d = sent.expect("64th add flushes");
    assert_eq!(d[1], 64);
    assert_eq!(d.len(), ACK_HEADER_SIZE + 64 * ACK_FRESH_ENTRY_SIZE);
}

#[test]
fn overlap_appends_up_to_eight_prior_entries() {
    let mut b = AckBatcher::new();
    for i in 0..5u32 {
        b.add(e(i, 0, 0, 0, 0), 0);
    }
    b.flush().unwrap();
    for i in 0..3u32 {
        b.add(e(100 + i, 0, 0, 0, 0), 0);
    }
    let d = b.flush().unwrap();
    let count_fresh = d[1] as usize;
    let count_overlap = d[2] as usize;
    assert_eq!(count_fresh, 3);
    assert_eq!(count_overlap, 5.min(ACK_OVERLAP_COUNT)); // 3 fresh + up to 5 overlap
    let batch = AckBatch::decode(&d).unwrap();
    assert_eq!(batch.entries()[0].frame_seq, 100); // fresh first
}

#[test]
fn timer_flush_after_5ms() {
    let mut b = AckBatcher::new();
    b.add(e(1, 2, 3, 4, 0), 1_000);
    assert_eq!(b.poll_timeout(), Some(6_000)); // 1000µs + 5ms
    assert!(b.on_timeout(5_999).is_none()); // not yet due
    let d = b.on_timeout(6_000).unwrap();
    assert_eq!(d[1], 1);
    assert_eq!(b.poll_timeout(), None);
}

#[test]
fn round_trips_arrival_us() {
    let mut b = AckBatcher::new();
    b.add(e(0x12345678, 7, 9, 3, 0xABCD), 0);
    let d = b.flush().unwrap();
    let batch = AckBatch::decode(&d).unwrap();
    assert_eq!(batch.entries().len(), 1);
    assert_eq!(batch.entries()[0].arrival_us, 0xABCD);
    assert_eq!(batch.entries()[0].frame_seq, 0x12345678);
    assert_eq!(batch.entries()[0].pass_idx, 3);
}

#[test]
fn empty_flush_returns_none() {
    let mut b = AckBatcher::new();
    assert!(b.flush().is_none());
}

#[test]
fn caps_overlap_at_eight_after_twenty_single_entry_flushes() {
    let mut b = AckBatcher::new();
    for i in 0..20u32 {
        b.add(e(i, 0, 0, 0, 0), 0);
        b.flush();
    }
    b.add(e(100, 1, 2, 3, 0), 0);
    let d = b.flush().unwrap();
    let batch = AckBatch::decode(&d).unwrap();
    assert_eq!(batch.entries().len(), 1 + ACK_OVERLAP_COUNT);
    assert_eq!(batch.entries()[0], e(100, 1, 2, 3, 0));
    for k in 0..ACK_OVERLAP_COUNT {
        let expected_frame_seq = 20 - ACK_OVERLAP_COUNT as u32 + k as u32;
        assert_eq!(batch.entries()[1 + k].frame_seq, expected_frame_seq);
    }
}

#[test]
fn no_overlap_on_very_first_batch() {
    let mut b = AckBatcher::new();
    b.add(e(42, 1, 2, 3, 0), 0);
    let d = b.flush().unwrap();
    let batch = AckBatch::decode(&d).unwrap();
    assert_eq!(batch.entries().len(), 1);
}

#[test]
fn produces_no_datagram_on_flush_with_no_entries_after_being_used() {
    let mut b = AckBatcher::new();
    b.add(e(1, 0, 0, 0, 0), 0);
    b.flush().unwrap();
    // second flush with nothing queued should be None
    assert!(b.flush().is_none());
}
