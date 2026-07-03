use ghostframe_client_core::decode_error_batcher::DecodeErrorBatcher;
use ghostframe_client_core::DecodeErrorCode;
use ghostframe_protocol::protocol::Codec;

#[test]
fn emits_the_first_error() {
    let mut b = DecodeErrorBatcher::new();
    let m = b.report(Codec::PalRle, 3, 4, DecodeErrorCode::ThinUncachedPalette, 0);
    assert!(m.is_some());
}

#[test]
fn drops_a_duplicate_within_1000ms() {
    let mut b = DecodeErrorBatcher::new();
    let mut emitted = 0;
    if b.report(Codec::PalRle, 3, 4, DecodeErrorCode::ThinUncachedPalette, 0)
        .is_some()
    {
        emitted += 1;
    }
    if b.report(
        Codec::PalRle,
        3,
        4,
        DecodeErrorCode::ThinUncachedPalette,
        500_000,
    )
    .is_some()
    {
        emitted += 1;
    }
    assert_eq!(emitted, 1);
}

#[test]
fn allows_same_key_after_1000ms_window() {
    let mut b = DecodeErrorBatcher::new();
    let mut emitted = 0;
    if b.report(Codec::PalRle, 3, 4, DecodeErrorCode::ThinUncachedPalette, 0)
        .is_some()
    {
        emitted += 1;
    }
    if b.report(
        Codec::PalRle,
        3,
        4,
        DecodeErrorCode::ThinUncachedPalette,
        1_001_000,
    )
    .is_some()
    {
        emitted += 1;
    }
    assert_eq!(emitted, 2);
}

#[test]
fn allows_distinct_keys_inside_window() {
    let mut b = DecodeErrorBatcher::new();
    let mut emitted = 0;
    for i in 0..10u8 {
        if b.report(Codec::PalRle, i, 4, DecodeErrorCode::ThinUncachedPalette, 0)
            .is_some()
        {
            emitted += 1;
        }
    }
    assert_eq!(emitted, 10);
}

#[test]
fn drops_above_global_cap_of_32() {
    let mut b = DecodeErrorBatcher::new();
    let mut emitted = 0;
    for i in 0..40u8 {
        if b.report(Codec::PalRle, i, 0, DecodeErrorCode::ThinUncachedPalette, 0)
            .is_some()
        {
            emitted += 1;
        }
    }
    assert_eq!(emitted, 32);
}

#[test]
fn replenishes_global_cap_after_1000ms_window() {
    let mut b = DecodeErrorBatcher::new();
    let mut emitted = 0;
    for i in 0..40u8 {
        if b.report(Codec::PalRle, i, 0, DecodeErrorCode::ThinUncachedPalette, 0)
            .is_some()
        {
            emitted += 1;
        }
    }
    assert_eq!(emitted, 32);
    if b.report(
        Codec::PalRle,
        100,
        0,
        DecodeErrorCode::ThinUncachedPalette,
        1_001_000,
    )
    .is_some()
    {
        emitted += 1;
    }
    assert_eq!(emitted, 33);
}

#[test]
fn decode_error_is_five_bytes() {
    let mut b = DecodeErrorBatcher::new();
    let m = b
        .report(
            Codec::PalRle,
            7,
            13,
            DecodeErrorCode::ThinUncachedPalette,
            0,
        )
        .unwrap();
    assert_eq!(m, vec![0x04, 2, 7, 13, 3]);
}
