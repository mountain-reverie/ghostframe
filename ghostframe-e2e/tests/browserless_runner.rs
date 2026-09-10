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
