//! Tests for the tile-delivery mode switch and the payload events.

use ghostframe_client_core::{ClientConfig, TileDelivery};

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
use ghostframe_protocol::protocol::Codec;

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
