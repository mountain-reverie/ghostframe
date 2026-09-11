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
