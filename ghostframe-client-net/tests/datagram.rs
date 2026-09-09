mod common;

use common::{connected_session, pump_with_wt};
use ghostframe_client_net::ClientNetEvent;

/// A Solid tile the server sends must surface as a decoded RGBA tile.
#[test]
fn solid_tile_datagram_becomes_tile_ready() {
    let (mut client, mut server, mut wt, mut now_us, base) = connected_session();

    // Encode exactly as the server does: a Solid payload is the tile's BGRA,
    // and `fragment_tile` is the same function `io_bridge` uses on the wire.
    let bgra = [10u8, 20, 30, 255];
    let payload = ghostframe_protocol::codec::solid::encode_solid(&bgra.repeat(32 * 32));
    let inputs = ghostframe_protocol::protocol::TileFragmentInputs {
        // NOTE: the server ORs in TILE_DATAGRAM_FLAG; without it the client
        // does not treat this as a tile datagram at all.
        frame_seq: 1 | ghostframe_protocol::protocol::TILE_DATAGRAM_FLAG,
        tile_x: 0,
        tile_y: 0,
        codec: ghostframe_protocol::protocol::Codec::Solid,
        generation: 0,
        pass: 0,
        timestamp_us: 0,
    };
    let datagrams = ghostframe_protocol::protocol::fragment_tile(&inputs, &payload, 1000);

    {
        let conn = server.connections.values_mut().next().unwrap();
        for dg in &datagrams {
            wt.send_datagram(conn, dg).expect("send_datagram");
        }
    }
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);

    let tiles: Vec<_> = client
        .take_events()
        .into_iter()
        .filter_map(|e| match e {
            ClientNetEvent::Core(ghostframe_client_core::Event::TileReady {
                tile_x,
                tile_y,
                rgba,
                ..
            }) => Some((tile_x, tile_y, rgba)),
            _ => None,
        })
        .collect();

    assert_eq!(tiles.len(), 1, "exactly one tile must decode");
    assert_eq!(
        &tiles[0].2[0..4],
        &[30, 20, 10, 255],
        "BGRA -> RGBA swizzle"
    );
}
