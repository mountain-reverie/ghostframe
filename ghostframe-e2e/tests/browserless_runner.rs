//! Task 15a/15b: the browserless netsim scene runner.
//!
//! Task 15a proved a real `IoBridge` and a real `ClientNet` can establish a
//! WebTransport session across a Unix socketpair, routed through the
//! netsim, under tokio's virtual clock (tests 1-3 below). Task 15b adds
//! tile injection and framebuffer assembly (tests 4-5): frames declared on
//! `BrowserlessScene::frames` are encoded and sent to a real `IoBridge`,
//! decoded by a real `ClientNet`, and the resulting `Event::TileReady`
//! pixels land in `BrowserlessResult.framebuffer`.

use std::time::Duration;

use ghostframe_client_net::ClientNetEvent;
use ghostframe_e2e::harness::browserless::{run_browserless, BrowserlessScene, FrameScript};
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::NetProfile;

#[tokio::test(start_paused = true)]
async fn the_session_establishes_over_the_socketpair() {
    let scene = BrowserlessScene {
        seed: 1,
        frames: vec![],
        net: NetProfile::perfect(),
        duration: Duration::from_millis(500),
        grid_cols: 4,
        grid_rows: 4,
    };
    let result = run_browserless(scene).await.expect("scene ran");
    assert!(
        result.events.contains(&ClientNetEvent::SessionReady),
        "seed 1: the WebTransport session must establish over the socketpair"
    );
    assert_eq!(result.seed, 1);
}

/// Proves datagrams genuinely crossed the socketpair (and therefore
/// `IoBridge`) rather than the handshake being short-circuited: every byte
/// counted here passed through `SocketPairPump::send`/`recv` — the exact
/// wire framing `IoBridge` speaks on the other end of the socket. A
/// short-circuited handshake (e.g. wiring `ClientNet` directly to a
/// `QuicServer` in-memory, as `ghostframe-client-net`'s own test helpers
/// do) would never touch this counter at all.
#[tokio::test(start_paused = true)]
async fn bytes_actually_cross_the_socketpair() {
    let scene = BrowserlessScene {
        seed: 2,
        frames: vec![],
        net: NetProfile::perfect(),
        duration: Duration::from_millis(500),
        grid_cols: 4,
        grid_rows: 4,
    };
    let result = run_browserless(scene).await.expect("scene ran");
    assert!(
        result.bytes_delivered > 0,
        "seed 2: at least the QUIC handshake bytes must have crossed the socketpair"
    );
    assert_eq!(
        result.bytes_dropped, 0,
        "seed 2: NetProfile::perfect() drops nothing"
    );
}

/// Proves the netsim is genuinely in the datagram path.
///
/// The two tests above run on `NetProfile::perfect()`, where every verdict
/// is `Deliver` — so removing the netsim from the loop entirely would not
/// change their result. Only a profile that actually drops packets can
/// distinguish "routed through the simulator" from "handed straight over".
///
/// QUIC retransmits, so the session must still establish despite the loss;
/// asserting both halves means neither a bypassed simulator (no drops) nor
/// a broken retransmit path (no session) can pass.
#[tokio::test(start_paused = true)]
async fn a_lossy_link_still_establishes_and_records_drops() {
    let scene = BrowserlessScene {
        seed: 7,
        frames: vec![],
        net: NetProfile {
            loss: 0.10,
            ..NetProfile::perfect()
        },
        duration: Duration::from_secs(5),
        grid_cols: 4,
        grid_rows: 4,
    };

    let result = run_browserless(scene).await.expect("scene ran");

    assert!(
        result.bytes_dropped > 0,
        "seed 7: a 10% loss profile must drop something; 0 dropped bytes \
         means the netsim is not in the datagram path at all"
    );
    assert!(
        result.events.contains(&ClientNetEvent::SessionReady),
        "seed 7: the session must still establish across a lossy link — \
         QUIC retransmits (dropped {} bytes, delivered {})",
        result.bytes_dropped,
        result.bytes_delivered
    );
}

/// The plan's acceptance test: a single Solid tile, injected once the
/// session is ready, arrives at the client and decodes to the right BGRA
/// -> RGBA swizzle.
#[tokio::test(start_paused = true)]
async fn a_single_solid_tile_arrives_on_a_perfect_link() {
    let scene = BrowserlessScene {
        seed: 1,
        frames: vec![FrameScript {
            tiles: vec![(
                (0, 0),
                TileSpec::Solid {
                    bgra: [10, 20, 30, 255],
                },
            )],
        }],
        net: NetProfile::perfect(),
        duration: Duration::from_millis(500),
        grid_cols: 4,
        grid_rows: 4,
    };

    let result = run_browserless(scene).await.expect("scene ran");

    let px = result
        .framebuffer
        .tile_rgba(0, 0)
        .expect("tile (0,0) decoded");
    assert_eq!(&px[0..4], &[30, 20, 10, 255], "BGRA -> RGBA swizzle");
    assert_eq!(result.stale_generation_tiles, 0);
}

/// Proves frames after the first are actually injected: a two-frame scene
/// rewrites tile (0,0) with a different color in frame 2, and only frame 2's
/// color must survive. `a_single_solid_tile_arrives_on_a_perfect_link` alone
/// would pass even if the injection loop only ever sent frame 0 — this is
/// the test that catches that.
///
/// Note what this does NOT prove. Pinning the per-tile generation counter to
/// a constant leaves every test in this file green, so nothing here is
/// load-bearing on the generation advancing. Solid tiles are self-contained
/// single payloads, so a stale generation cannot change their pixels.
/// Generation only becomes observable with a multi-pass codec, where
/// `Cdf53TileState` must discard the previous generation's accumulated
/// planes instead of blending them — a Cdf53 multi-frame scene (task 16) is
/// what will pin that down.
#[tokio::test(start_paused = true)]
async fn a_second_frame_overwrites_the_first_frames_tile() {
    let scene = BrowserlessScene {
        seed: 3,
        frames: vec![
            FrameScript {
                tiles: vec![(
                    (0, 0),
                    TileSpec::Solid {
                        bgra: [10, 20, 30, 255],
                    },
                )],
            },
            FrameScript {
                tiles: vec![(
                    (0, 0),
                    TileSpec::Solid {
                        bgra: [200, 150, 100, 255],
                    },
                )],
            },
        ],
        net: NetProfile::perfect(),
        duration: Duration::from_millis(500),
        grid_cols: 4,
        grid_rows: 4,
    };

    let result = run_browserless(scene).await.expect("scene ran");

    let px = result
        .framebuffer
        .tile_rgba(0, 0)
        .expect("tile (0,0) decoded");
    assert_eq!(
        &px[0..4],
        &[100, 150, 200, 255],
        "frame 2's color (BGRA [200,150,100,255] -> RGBA) must be what the \
         client ends up rendering, proving frame 2 was actually injected"
    );
    assert_eq!(result.stale_generation_tiles, 0);
}
