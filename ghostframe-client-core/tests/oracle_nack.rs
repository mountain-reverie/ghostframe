use ghostframe_client_core::nack_batcher::{NackBatcher, NackEntry};

#[test]
fn encodes_8_byte_entries_with_0x05_envelope() {
    let mut b = NackBatcher::new();
    b.add(
        NackEntry {
            frame_seq: 0x01020304,
            tile_x: 5,
            tile_y: 6,
            pass_idx: 7,
            frag_idx: 9,
        },
        0,
    );
    let d = b.flush().unwrap();
    assert_eq!(d[0], 0x05);
    assert_eq!(d[1], 1);
    assert_eq!(&d[2..6], &[0x04, 0x03, 0x02, 0x01]); // LE
    assert_eq!(d[6], 5);
    assert_eq!(d[7], 6);
    assert_eq!(d[8], 7);
    assert_eq!(d[9], 9);
}

#[test]
fn flushes_at_max_entries_without_timer() {
    let mut b = NackBatcher::new();
    let mut sent = None;
    for i in 0..64u32 {
        if let Some(d) = b.add(
            NackEntry {
                frame_seq: i,
                tile_x: 0,
                tile_y: 0,
                pass_idx: 0,
                frag_idx: 0,
            },
            0,
        ) {
            sent = Some(d);
        }
    }
    let d = sent.expect("64th add flushes");
    assert_eq!(d[1], 64);
    assert_eq!(d.len(), 2 + 64 * 8);
}

#[test]
fn timer_flush_after_5ms() {
    let mut b = NackBatcher::new();
    b.add(
        NackEntry {
            frame_seq: 1,
            tile_x: 0,
            tile_y: 0,
            pass_idx: 0,
            frag_idx: 0,
        },
        0,
    );
    assert_eq!(b.poll_timeout(), Some(5_000)); // 0µs + 5ms
    assert!(b.on_timeout(4_999).is_none()); // not yet due
    let d = b.on_timeout(5_000).unwrap();
    assert_eq!(d[1], 1);
    assert_eq!(b.poll_timeout(), None);
}

#[test]
fn does_not_flush_when_empty() {
    let mut b = NackBatcher::new();
    assert!(b.on_timeout(5_000).is_none());
    assert!(b.flush().is_none());
}
