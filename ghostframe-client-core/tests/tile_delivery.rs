//! Tests for the tile-delivery mode switch and the payload events.

use ghostframe_client_core::{ClientConfig, ClientCore, PollOutput, TileDelivery};
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

/// Like `drive_one_tile`, but also drains everything the core queued to send.
/// The Cdf53 deferred ACK is a `PollOutput`, not an `Event`, so a test that
/// only inspects events cannot see whether it fired.
///
/// A single ACK entry does not flush `AckBatcher` immediately — it only
/// flushes at `MAX_FRESH_ENTRIES_PER_BATCH` entries or 5ms after the first
/// queued entry (`FLUSH_INTERVAL_US`), same as every other codec's
/// on-receipt ACK. So this advances time past that debounce window with
/// `on_timeout` before draining `poll_transmit`, well short of the 100ms
/// periodic-feedback interval so no unrelated output sneaks in.
fn drive_one_tile_with_outputs(
    delivery: TileDelivery,
    codec: Codec,
    payload: &[u8],
) -> (Vec<Event>, Vec<PollOutput>) {
    let mut core = core_with(delivery);
    let mut events = Vec::new();
    for dg in tile_datagrams(1, 0, 0, codec, 0, payload, 1200) {
        events.extend(core.handle_datagram(&dg, 0));
    }
    core.on_timeout(10_000);
    let mut outputs = Vec::new();
    while let Some(o) = core.poll_transmit(0) {
        outputs.push(o);
    }
    (events, outputs)
}

/// One valid Cdf53 pass-0 payload over a gradient tile.
fn cdf53_pass0_payload() -> Vec<u8> {
    let mut bgra = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32u32 {
        for x in 0..32u32 {
            bgra.extend_from_slice(&[(x * 8) as u8, (y * 8) as u8, ((x + y) * 4) as u8, 255]);
        }
    }
    let coeffs = ghostframe_protocol::codec::cdf53::forward(&bgra);
    let passes = ghostframe_protocol::codec::cdf53::encode_passes(&coeffs);
    passes.into_iter().next().expect("at least one pass")
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

use ghostframe_client_core::{Event, TileData};

/// `TilePayload` carries everything the GPU decoder needs and should not have
/// to re-derive: which tile, which generation, and (via `TileData`) which
/// codec plus its codec-specific fields. `pass_idx` used to live directly on
/// the event; it now lives inside `TileData::Cdf53` only — it is meaningless
/// for the other three codecs — so this test exercises the Cdf53 variant to
/// keep `pass_idx` covered.
#[test]
fn tile_payload_carries_the_gpu_decoders_inputs() {
    let e = Event::TilePayload {
        frame_seq: 7,
        tile_x: 1,
        tile_y: 2,
        generation: 4,
        data: TileData::Cdf53 {
            pass_idx: 3,
            bit_planes: vec![0xAA, 0xBB],
        },
    };
    match e {
        Event::TilePayload {
            frame_seq,
            tile_x,
            tile_y,
            generation,
            data,
        } => {
            assert_eq!((frame_seq, tile_x, tile_y, generation), (7, 1, 2, 4));
            match data {
                TileData::Cdf53 {
                    pass_idx,
                    bit_planes,
                } => {
                    assert_eq!(pass_idx, 3);
                    assert_eq!(bit_planes, vec![0xAA, 0xBB]);
                }
                other => panic!("wrong TileData variant: {other:?}"),
            }
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
            Event::TilePayload { data, .. } => Some(data.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        payloads.len(),
        1,
        "expected exactly one TilePayload, got {events:?}"
    );
    // Was `assert_eq!(codec, Codec::Solid)` against a codec tag; the codec is
    // now the variant itself, so the variant check takes its place.
    assert!(
        matches!(payloads[0], TileData::Solid(_)),
        "expected TileData::Solid, got {:?}",
        payloads[0]
    );
    match &payloads[0] {
        TileData::Solid(quad) => assert_eq!(*quad, [10, 20, 30, 255]),
        other => panic!("wrong TileData variant: {other:?}"),
    }
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
            Event::TilePayload {
                data: TileData::Raw(bytes),
                ..
            } => Some(bytes.clone()),
            Event::TilePayload { data, .. } => panic!("expected TileData::Raw, got {data:?}"),
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
    // A non-trivial index pattern, not all zeros: two runs the RLE has to
    // encode and the prevalidator has to expand back. With count = 2 the only
    // legal indices are 0 and 1, so 0x10 is the pixel pair (0, 1) — low nibble
    // first — and 0x01 is (1, 0). An implementation that returned a correctly
    // sized zero buffer would pass against an all-zero input; it cannot pass
    // against this one.
    let mut packed = [0u8; 512];
    packed[..256].fill(0x10);
    packed[256..].fill(0x01);
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

    // Previously this only checked that a PalRle-coded TilePayload existed —
    // the test never re-decoded the wire RLE to check its content. Now the
    // prevalidated fields sit directly on the event, so assert them: this is
    // the exact product palrle_decode.wgsl consumes, which is a strengthened
    // check, not a relocated one. The input pattern is
    // non-uniform, so the expansion is actually checked.
    let payload: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::TilePayload {
                data:
                    TileData::PalRle {
                        palette_id,
                        count,
                        indices,
                    },
                ..
            } => Some((*palette_id, *count, indices.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        payload.len(),
        1,
        "the tile payload itself must still be emitted, got {events:?}"
    );
    assert_eq!(
        payload[0].0, 5,
        "TilePayload must reference the same palette slot"
    );
    assert_eq!(payload[0].1, 2);
    let mut expected_indices = vec![0u8; 512];
    expected_indices[..256].fill(0x10);
    expected_indices[256..].fill(0x01);
    assert_eq!(
        payload[0].2, expected_indices,
        "indices must be the expanded 512-byte buffer the shader reads, \
         round-tripped through the wire RLE"
    );

    assert!(
        !events.iter().any(|e| matches!(e, Event::TileReady { .. })),
        "Payload mode must not emit TileReady"
    );
}

/// Proves the palette is really *stored*, not merely announced.
///
/// `palrle_in_payload_mode_applies_and_reports_the_palette` checks that
/// `PaletteUpdated` is emitted, and a mutation check showed it still passes
/// when the state writes are deleted — an emitted event with no stored
/// palette would leave `palrle_decode.wgsl` decoding against a stale table,
/// which shows as wrong colours rather than an error.
///
/// A thin (non-bundled) payload carries no palette and prevalidates only if
/// `shadow.has(palette_id)`, so feeding one after the bundled tile is what
/// distinguishes "stored" from "announced".
#[test]
fn payload_mode_really_stores_the_palette_not_just_reports_it() {
    use ghostframe_protocol::codec::pal_rle::{encode_pal_rle_payload, PaletteEntry};

    let mut colors = [[0u8; 4]; 16];
    colors[0] = [10, 20, 30, 255];
    colors[1] = [40, 50, 60, 255];
    let entry = PaletteEntry { colors, count: 2 };
    let packed = [0u8; 512];

    let mut core = core_with(TileDelivery::Payload);

    // Tile 1: bundled, installs slot 5.
    let bundled = encode_pal_rle_payload(&packed, &entry, 5, true);
    let mut events = Vec::new();
    for dg in tile_datagrams(1, 0, 0, Codec::PalRle, 0, &bundled, 1200) {
        events.extend(core.handle_datagram(&dg, 0));
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::PaletteUpdated { palette_id: 5, .. })),
        "tile 1 should have installed slot 5, got {events:?}"
    );

    // Tile 2: thin, carries no palette — prevalidates only against the stored
    // shadow.
    let thin = encode_pal_rle_payload(&packed, &entry, 5, false);
    let mut events2 = Vec::new();
    for dg in tile_datagrams(2, 1, 0, Codec::PalRle, 0, &thin, 1200) {
        events2.extend(core.handle_datagram(&dg, 0));
    }

    assert!(
        !events2.iter().any(|e| matches!(
            e,
            Event::DecodeError {
                code: ghostframe_client_core::DecodeErrorCode::ThinUncachedPalette,
                ..
            }
        )),
        "thin tile was rejected as uncached — the bundled tile announced the \
         palette but did not store it, got {events2:?}"
    );
    // Was `codec: Codec::PalRle` against a codec tag; matching on
    // `TileData::PalRle { palette_id: 5, .. }` reaches the same check (a
    // payload was produced) and additionally confirms it references the
    // slot the bundled tile installed — a small strengthening, not just a
    // relocation.
    assert!(
        events2.iter().any(|e| matches!(
            e,
            Event::TilePayload {
                data: TileData::PalRle { palette_id: 5, .. },
                ..
            }
        )),
        "thin tile should have produced a payload, got {events2:?}"
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

/// Payload mode must keep every protocol side effect of a Cdf53 pass —
/// prevalidation, coverage, and the deferred ACK — and drop only the CPU
/// accumulation, which the GPU does instead in cdf53_integrate.wgsl.
///
/// The ACK is the one worth guarding hardest: without it the server
/// retransmits this pass indefinitely and nothing client-side looks wrong.
#[test]
fn cdf53_in_payload_mode_keeps_the_ack_and_skips_integrate() {
    let pass0 = cdf53_pass0_payload();
    let (events, outputs) =
        drive_one_tile_with_outputs(TileDelivery::Payload, Codec::Cdf53, &pass0);

    // Previously this only checked that a Cdf53-coded TilePayload existed.
    // The prevalidated bit_planes are now directly on the event, so assert
    // their shape too (384 bytes: 3 channels x 128) — the exact product
    // cdf53_integrate.wgsl consumes. Strengthened, not relocated.
    let cdf53_payloads: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::TilePayload {
                data:
                    TileData::Cdf53 {
                        pass_idx,
                        bit_planes,
                    },
                ..
            } => Some((*pass_idx, bit_planes.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        cdf53_payloads.len(),
        1,
        "expected a Cdf53 TilePayload, got {events:?}"
    );
    assert_eq!(
        cdf53_payloads[0].0, 0,
        "pass0 payload should carry pass_idx 0"
    );
    assert_eq!(
        cdf53_payloads[0].1.len(),
        384,
        "bit_planes must be 3 channels x 128 bytes"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::TileReady { .. })),
        "Payload mode must not emit TileReady, got {events:?}"
    );
    assert!(
        !outputs.is_empty(),
        "the deferred ACK must still be produced — without it the server \
         retransmits this pass indefinitely"
    );
}

/// The Decoded path is unchanged: pixels out, and the ACK still fires.
#[test]
fn cdf53_in_decoded_mode_is_unchanged() {
    let pass0 = cdf53_pass0_payload();
    let (events, outputs) =
        drive_one_tile_with_outputs(TileDelivery::Decoded, Codec::Cdf53, &pass0);

    let ready: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::TileReady { rgba, .. } => Some(rgba.len()),
            _ => None,
        })
        .collect();
    assert_eq!(ready, vec![4096], "got {events:?}");
    assert!(
        !outputs.is_empty(),
        "the deferred ACK must fire in Decoded mode too"
    );
}
