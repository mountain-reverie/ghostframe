//! Browserless e2e: real IoBridge + netsim + headless client, no browser,
//! no containers, no GPU. See
//! docs/superpowers/specs/2026-09-07-headless-client-netsim-design.md

use ghostframe_lib::transport::io_bridge::IoBridge;
use ghostframe_lib::transport::quic::QuicServer;
use tokio::net::UnixStream;

#[tokio::test]
async fn io_bridge_constructs_from_a_socketpair() {
    let (ours, _peer) = UnixStream::pair().expect("UnixStream::pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let bridge = IoBridge::new_with_stream_for_test(ours, server);

    // The assertion that matters is structural: this crate can link and call
    // the constructor at all under `browserless-harness`. Real scene coverage
    // arrives with the harness in a later task.
    let hash = bridge.cert_hash_sha256();
    assert_eq!(hash.len(), 64, "cert hash must be 64 hex chars");
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit()),
        "cert hash must be hex, got {hash:?}"
    );
}

// ── Access re-check wiring ───────────────────────────────────────────────
//
// The unit tests cover the re-check rule; this one covers the wiring that
// makes it run on an otherwise idle server: the loop must be woken for the
// due time (`next_wake_deadline`), and the lookup the re-check queued must be
// started with no inbound packet to start it (`post_turn`'s
// `spawn_peer_lookups`). Dropping either leaves a revoked session connected
// until some unrelated packet arrives.

mod recheck_wiring {
    use super::*;
    use ghostframe_client_net::{ClientNet, ClientNetConfig, ClientNetEvent};
    use ghostframe_e2e::netsim::SocketPairPump;
    use ghostframe_lib::config::{AccessConfig, LibConfig};
    use ghostframe_lib::transport::peer_auth::{
        AccessPolicy, DenialLog, PeerGate, PeerIdentity, PeerIdentitySource, PeerLookup,
    };
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::time::Instant;

    fn who(id: i64, login: &str) -> PeerIdentity {
        PeerIdentity {
            user_id: id,
            login_name: login.into(),
            node_name: "peer".into(),
            tags: vec![],
        }
    }

    /// A background (non-immediate) source whose answer the scene switches.
    struct Switch(Mutex<PeerLookup>);

    impl PeerIdentitySource for Switch {
        fn lookup(&self, _: std::net::IpAddr) -> PeerLookup {
            self.0.lock().unwrap().clone()
        }
        fn is_immediate(&self) -> bool {
            false
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_revoked_session_is_closed_on_an_idle_server() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let owner = who(1, "owner@test");
                let source = Arc::new(Switch(Mutex::new(Ok(owner.clone()))));
                let gate = PeerGate::new(
                    Arc::new(AccessPolicy::new(&owner, &AccessConfig::default())),
                    source.clone(),
                    Arc::new(DenialLog::new()),
                );

                let (ours, peer) = tokio::net::UnixStream::pair().unwrap();
                let server = QuicServer::new_with_rng_seed(
                    ghostframe_lib::config::datagram_send_buffer_bytes(),
                    [7; 32],
                )
                .unwrap();
                let mut hash = [0u8; 32];
                hex::decode_to_slice(&server.cert_info().sha256_hex, &mut hash).unwrap();
                let mut cfg = LibConfig::default();
                cfg.access.recheck_interval = Duration::from_secs(1);
                let mut bridge = IoBridge::new_with_lib_config_for_test(ours, server, cfg);
                bridge.set_peer_gate_for_test(gate);
                let live = bridge.connected_session_count_handle();
                tokio::task::spawn_local(async move {
                    let _ = bridge.run().await;
                });

                let base = Instant::now();
                let us = || Instant::now().duration_since(base).as_micros() as u64;
                let mut client = ClientNet::with_rng_seed(
                    ClientNetConfig {
                        server_name: "localhost".into(),
                        server_cert_sha256: hash,
                        indices_raw_enabled: true,
                        supports_h264: false,
                        display: None,
                    },
                    us(),
                    [9; 32],
                )
                .unwrap();
                let srv: SocketAddr = "[::1]:443".parse().unwrap();
                let cli: SocketAddr = "[::1]:5000".parse().unwrap();
                client.connect(srv, us()).unwrap();

                // `SocketPairPump::recv` is not cancel-safe: a reader task
                // owns one dup of the socket and forwards what arrives.
                let std_peer = peer.into_std().unwrap();
                let std_rd = std_peer.try_clone().unwrap();
                let mut pump =
                    SocketPairPump::new(tokio::net::UnixStream::from_std(std_peer).unwrap());
                let mut rd = SocketPairPump::new(tokio::net::UnixStream::from_std(std_rd).unwrap());
                let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel();
                tokio::task::spawn_local(async move {
                    while let Ok(pk) = rd.recv().await {
                        if ptx.send(pk).is_err() {
                            break;
                        }
                    }
                });

                // Phase 1: connect and HELLO, the client driving normally.
                // The identity source says "owner", so the lookup admits it.
                let mut ready = false;
                let deadline = Instant::now() + Duration::from_millis(900);
                while Instant::now() < deadline {
                    while let Some(o) = client.poll_transmit() {
                        pump.send(&o.payload, &cli).await.unwrap();
                    }
                    for e in client.take_events() {
                        ready |= matches!(e, ClientNetEvent::SessionReady);
                    }
                    let wake = client
                        .poll_timeout()
                        .map(|t| base + Duration::from_micros(t))
                        .unwrap_or(Instant::now() + Duration::from_millis(2))
                        .min(Instant::now() + Duration::from_millis(5));
                    tokio::select! {
                        Some(pk) = prx.recv() => client.handle_udp(&pk.payload, srv, us()),
                        _ = tokio::time::sleep_until(wake) => client.on_timeout(us()),
                    }
                }
                assert!(ready, "premise: the session came up");
                assert_eq!(*live.borrow(), 1, "premise: HELLO completed");

                // Access is revoked. The client now goes silent: it sends
                // nothing and runs no timers, so no inbound packet can wake
                // the server's loop. Only the re-check's own wakeup can.
                *source.0.lock().unwrap() = Ok(who(7, "stranger@test"));
                let silent_until = Instant::now() + Duration::from_secs(2);
                let mut lost: Option<String> = None;
                while Instant::now() < silent_until && lost.is_none() {
                    tokio::select! {
                        Some(pk) = prx.recv() => {
                            client.handle_udp(&pk.payload, srv, us());
                            for e in client.take_events() {
                                if let ClientNetEvent::ConnectionLost { reason } = e {
                                    lost = Some(reason);
                                }
                            }
                        }
                        _ = tokio::time::sleep_until(silent_until) => {}
                    }
                }
                let reason = lost.expect("the revoked session was closed within the interval");
                assert!(reason.contains("access revoked"), "close reason: {reason}");
                assert!(
                    reason.contains("26372") || reason.contains("0x6704"),
                    "close code 0x6704 (26372): {reason}"
                );
            })
            .await;
    }
}
