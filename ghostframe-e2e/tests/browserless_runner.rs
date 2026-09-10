//! Task 15a: transport bring-up for the browserless netsim scene runner.
//!
//! Proves a real `IoBridge` and a real `ClientNet` can establish a
//! WebTransport session across a Unix socketpair, routed through the
//! netsim, under tokio's virtual clock. No tile injection, no frame
//! scripts, no framebuffer assembly — that is task 15b.

use std::time::Duration;

use ghostframe_client_net::ClientNetEvent;
use ghostframe_e2e::harness::browserless::{run_browserless, BrowserlessScene};
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
