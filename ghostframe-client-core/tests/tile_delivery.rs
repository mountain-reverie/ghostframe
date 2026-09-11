//! Tests for the tile-delivery mode switch and the payload events.

use ghostframe_client_core::{ClientConfig, ClientCore, TileDelivery};
use ghostframe_protocol::protocol::{fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG};

fn core_with(delivery: TileDelivery) -> ClientCore {
    let mut core = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: true,
            supports_h264: true,
            tile_delivery: delivery,
        },
        0,
    );
    while core.poll_transmit(0).is_some() {}
    core
}

fn tile_datagrams(
    frame_seq: u32,
    x: u8,
    y: u8,
    codec: Codec,
    pass: u8,
    payload: &[u8],
    mtu_payload: usize,
) -> Vec<Vec<u8>> {
    fragment_tile(
        &TileFragmentInputs {
            frame_seq: frame_seq | TILE_DATAGRAM_FLAG,
            tile_x: x,
            tile_y: y,
            codec,
            generation: 1,
            pass,
            timestamp_us: 0,
        },
        payload,
        mtu_payload,
    )
}

/// Feed every fragment of one tile, in order, and collect the events
/// produced across all of them. Single-fragment tiles just run this loop
/// once; a payload large enough to fragment at `mtu_payload` is fed
/// fragment-by-fragment, which is the realistic multi-datagram case.
fn drive_one_tile(delivery: TileDelivery, codec: Codec, payload: &[u8]) -> Vec<Event> {
    let mut core = core_with(delivery);
    let dgs = tile_datagrams(1, 0, 0, codec, 0, payload, 1200);
    let mut events = Vec::new();
    for dg in &dgs {
        events.extend(core.handle_datagram(dg, 0));
    }
    events
}

/// The default must be the behaviour every current consumer already relies
/// on. A default of `Payload` would silently stop `ghostframe-e2e`'s
/// FrameBuffer and the native client receiving pixels at all.
#[test]
fn tile_delivery_defaults_to_decoded() {
    let cfg = ClientConfig {
        indices_raw_enabled: true,
        supports_h264: false,
        ..Default::default()
    };
    assert_eq!(cfg.tile_delivery, TileDelivery::Decoded);
}

use ghostframe_client_core::Event;

/// `TilePayload` carries everything the GPU decoder needs and should not have
/// to re-derive: which tile, which pass, which generation, which codec.
#[test]
fn tile_payload_carries_the_gpu_decoders_inputs() {
    let e = Event::TilePayload {
        frame_seq: 7,
        tile_x: 1,
        tile_y: 2,
        pass_idx: 3,
        generation: 4,
        codec: Codec::PalRle,
        payload: vec![0xAA, 0xBB],
    };
    match e {
        Event::TilePayload {
            frame_seq,
            tile_x,
            tile_y,
            pass_idx,
            generation,
            codec,
            payload,
        } => {
            assert_eq!(
                (frame_seq, tile_x, tile_y, pass_idx, generation),
                (7, 1, 2, 3, 4)
            );
            assert_eq!(codec, Codec::PalRle);
            assert_eq!(payload, vec![0xAA, 0xBB]);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

/// The palette shadow is protocol state and lives in the core, but
/// `palrle_decode.wgsl` needs the table to decode against. Without this event
/// the core would own the palette and the shader could not see it — which
/// shows up as wrong colours, not an error.
#[test]
fn palette_updated_carries_the_slot_and_colours() {
    let e = Event::PaletteUpdated {
        palette_id: 5,
        colors: vec![[1, 2, 3, 255], [4, 5, 6, 255]],
    };
    match e {
        Event::PaletteUpdated { palette_id, colors } => {
            assert_eq!(palette_id, 5);
            assert_eq!(colors.len(), 2);
            assert_eq!(colors[1], [4, 5, 6, 255]);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

/// Under Payload mode a Solid tile must arrive as its 4 wire bytes, not as
/// 4096 bytes of expanded pixels — the GPU's solid.wgsl does the expansion.
#[test]
fn solid_in_payload_mode_emits_wire_bytes() {
    let events = drive_one_tile(TileDelivery::Payload, Codec::Solid, &[10, 20, 30, 255]);
    let payloads: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::TilePayload { payload, codec, .. } => Some((payload.clone(), *codec)),
            _ => None,
        })
        .collect();
    assert_eq!(
        payloads.len(),
        1,
        "expected exactly one TilePayload, got {events:?}"
    );
    assert_eq!(payloads[0].0, vec![10, 20, 30, 255]);
    assert_eq!(payloads[0].1, Codec::Solid);
    assert!(
        !events.iter().any(|e| matches!(e, Event::TileReady { .. })),
        "Payload mode must not emit TileReady"
    );
}

/// The default path must be untouched: the same input under Decoded mode
/// still produces 4096 bytes of expanded RGBA.
#[test]
fn solid_in_decoded_mode_is_unchanged() {
    let events = drive_one_tile(TileDelivery::Decoded, Codec::Solid, &[10, 20, 30, 255]);
    let ready: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::TileReady { rgba, .. } => Some(rgba.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ready.len(), 1, "got {events:?}");
    assert_eq!(ready[0].len(), 4096);
    // BGRA 10,20,30 -> RGBA 30,20,10,255
    assert_eq!(&ready[0][0..4], &[30, 20, 10, 255]);
}

/// Raw is a straight BGRA->RGBA swizzle under Decoded; under Payload the
/// swizzle is the GPU's job and the bytes pass through untouched.
#[test]
fn raw_in_payload_mode_passes_bytes_through() {
    let bgra: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let events = drive_one_tile(TileDelivery::Payload, Codec::Raw, &bgra);
    let got: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::TilePayload { payload, .. } => Some(payload.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(got.len(), 1, "got {events:?}");
    assert_eq!(got[0], bgra, "payload must be the wire bytes, unswizzled");
}

/// Payload mode must still apply the bundled palette upsert — protocol state,
/// not decoding — and must surface it so the GPU can upload the table.
/// Dropping the upsert leaves the shader decoding against a stale palette,
/// which shows as wrong colours rather than an error.
#[test]
fn palrle_in_payload_mode_applies_and_reports_the_palette() {
    use ghostframe_protocol::codec::pal_rle::{encode_pal_rle_payload, PaletteEntry};

    let mut colors = [[0u8; 4]; 16];
    colors[0] = [10, 20, 30, 255];
    colors[1] = [40, 50, 60, 255];
    let entry = PaletteEntry { colors, count: 2 };
    let packed = [0u8; 512];
    let bundled = encode_pal_rle_payload(&packed, &entry, 5, true);

    let events = drive_one_tile(TileDelivery::Payload, Codec::PalRle, &bundled);

    let updated: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::PaletteUpdated { palette_id, colors } => Some((*palette_id, colors.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        updated.len(),
        1,
        "expected one PaletteUpdated, got {events:?}"
    );
    assert_eq!(updated[0].0, 5);
    assert_eq!(updated[0].1[0], [10, 20, 30, 255]);
    assert_eq!(updated[0].1[1], [40, 50, 60, 255]);

    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TilePayload {
                codec: Codec::PalRle,
                ..
            }
        )),
        "the tile payload itself must still be emitted, got {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::TileReady { .. })),
        "Payload mode must not emit TileReady"
    );
}

/// The Decoded path must be untouched. Same input, pixels out, and the first
/// pixel resolves through palette entry 0.
#[test]
fn palrle_in_decoded_mode_is_unchanged() {
    use ghostframe_protocol::codec::pal_rle::{encode_pal_rle_payload, PaletteEntry};

    let mut colors = [[0u8; 4]; 16];
    colors[0] = [10, 20, 30, 255];
    colors[1] = [40, 50, 60, 255];
    let entry = PaletteEntry { colors, count: 2 };
    let packed = [0u8; 512];
    let bundled = encode_pal_rle_payload(&packed, &entry, 5, true);

    let events = drive_one_tile(TileDelivery::Decoded, Codec::PalRle, &bundled);
    let ready: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::TileReady { rgba, .. } => Some(rgba.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ready.len(), 1, "got {events:?}");
    assert_eq!(ready[0].len(), 4096);
    // All indices are 0 -> palette entry 0, BGRA 10,20,30 -> RGBA 30,20,10.
    assert_eq!(&ready[0][0..4], &[30, 20, 10, 255]);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::PaletteUpdated { .. })),
        "Decoded mode applies the palette itself; the consumer never needs to see it"
    );
}
