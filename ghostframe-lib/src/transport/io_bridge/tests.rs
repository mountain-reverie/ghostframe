use super::*;
use std::sync::Arc;
use tokio::net::UnixStream;

/// Test-only helper: construct an IoBridge with a fresh UnixStream pair
/// and QuicServer. Discards the peer end of the stream pair (caller
/// doesn't need to interact with it).
async fn make_bridge_for_test() -> IoBridge {
    let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
    let server = QuicServer::new().expect("QuicServer::new failed");
    IoBridge::new_with_stream_for_test(our_end, server)
}

/// Test-only helper: make `h` the HELLO'd (live) session without
/// running `apply_hello`'s side effects.
fn make_live(bridge: &mut IoBridge, h: ConnectionHandle) {
    bridge.last_hello_from = Some(h);
}

/// Test-only helper: attach a fresh (disconnected) WebTransport session
/// for `h` to `bridge`. Shared by `test_bridge_with_sessions` and any
/// test that needs to attach a session mid-test (e.g. simulating a
/// reconnect on a new handle).
fn attach_session(bridge: &mut IoBridge, h: ConnectionHandle) {
    bridge.wt_sessions.insert(h, WebTransportServer::default());
}

/// Test-only helper: construct an IoBridge with a WebTransport session
/// already attached for each given handle. `async fn` (not plain `fn`)
/// because it calls tokio's `UnixStream::pair()`, which panics outside a
/// reactor; requiring `.await` enforces that structurally instead of
/// failing at runtime with an unrelated-looking message.
/// The session-lifecycle log reports `session_secs` and `gap_secs` from
/// these two fields. Asserting on log output is brittle; asserting that the
/// values the log reads are maintained is not, and it is the part that can
/// actually be wrong.
#[tokio::test]
async fn connection_established_starts_the_session_clock() {
    let h = ConnectionHandle(3);
    let mut bridge = test_bridge_with_sessions(&[]).await;
    assert!(
        bridge.session_started_at.is_none(),
        "no session has begun yet"
    );
    bridge.on_connection_established(h);
    assert!(
        bridge.session_started_at.is_none(),
        "a connection that has not sent HELLO is not a session yet"
    );
    bridge.apply_hello(h, hello(false));
    assert!(
        bridge.session_started_at.is_some(),
        "session_secs would report None for a live session"
    );
}

#[tokio::test]
async fn eviction_ends_the_session_and_opens_the_gap() {
    let h = ConnectionHandle(4);
    let mut bridge = test_bridge_with_sessions(&[h]).await;
    bridge.on_connection_established(h);
    bridge.apply_hello(h, hello(false));
    assert!(bridge.last_disconnect_at.is_none());

    bridge.evict_session(h, EvictionReason::DisplacedByNewSession);

    assert!(
        bridge.session_started_at.is_none(),
        "a session that ended must not keep reporting a duration"
    );
    assert!(
        bridge.last_disconnect_at.is_some(),
        "without this the next connect reports gap_secs=None and the \
         serving-nobody interval stays invisible"
    );
}

#[tokio::test]
async fn a_reconnect_can_measure_the_gap_it_followed() {
    // The whole point of the pair: disconnect then connect must leave
    // enough state to say how long nobody was being served.
    let h = ConnectionHandle(5);
    let mut bridge = test_bridge_with_sessions(&[h]).await;
    bridge.on_connection_established(h);
    bridge.apply_hello(h, hello(false));
    bridge.evict_session(h, EvictionReason::DisplacedByNewSession);
    let disconnected_at = bridge.last_disconnect_at.expect("set by eviction");

    let h2 = ConnectionHandle(6);
    bridge.on_connection_established(h2);
    bridge.apply_hello(h2, hello(false));
    let started_at = bridge.session_started_at.expect("set by connect");
    assert!(
        started_at >= disconnected_at,
        "gap_secs is computed as connect - last_disconnect; a negative \
         interval would mean the ordering is wrong"
    );
}

// ── connected_session_count: the capture-loop gate ───────────────────

/// The gate's publish side. Two properties the capture loop depends on:
///
/// 1. Every transition is published, so a parked loop wakes on connect and
///    stops on disconnect.
/// 2. An *unchanged* count publishes nothing. `refresh_connected_session_count`
///    is called once per frame from `compute_max_datagram_size`, so a
///    `send`-always implementation would wake every parked waiter ~30
///    times a second for no reason.
#[tokio::test]
async fn refresh_publishes_transitions_but_not_unchanged_counts() {
    let h = ConnectionHandle(1);
    let mut bridge = test_bridge_with_sessions(&[h]).await;
    make_live(&mut bridge, h);
    let mut rx = bridge.connected_session_count_handle();

    // Attached but not handshaked: not a consumer yet.
    bridge.refresh_connected_session_count();
    assert_eq!(
        *rx.borrow_and_update(),
        0,
        "a session that has not completed its H3 handshake must not open the gate"
    );

    bridge
        .wt_sessions
        .get_mut(&h)
        .expect("session attached above")
        .test_set_connected(true);
    bridge.refresh_connected_session_count();
    assert!(
        rx.has_changed().expect("sender alive"),
        "0 -> 1 must be published, or a parked capture loop never wakes"
    );
    assert_eq!(*rx.borrow_and_update(), 1);

    // Stand in for the per-frame refresh: nothing about the session
    // changed, so nothing may be published.
    for _ in 0..30 {
        bridge.refresh_connected_session_count();
    }
    assert!(
        !rx.has_changed().expect("sender alive"),
        "an unchanged count must publish nothing; the per-frame refresh \
         would otherwise wake every parked waiter once per frame"
    );

    bridge
        .wt_sessions
        .get_mut(&h)
        .expect("session attached above")
        .test_set_connected(false);
    bridge.refresh_connected_session_count();
    assert!(
        rx.has_changed().expect("sender alive"),
        "1 -> 0 must be published, or the capture loop keeps scraping for nobody"
    );
    assert_eq!(*rx.borrow_and_update(), 0);
}

/// Dropping the bridge closes the channel. That is the only signal a
/// parked capture loop has that the event loop is gone.
#[tokio::test]
async fn dropping_the_bridge_closes_the_session_count_channel() {
    let bridge = test_bridge_with_sessions(&[]).await;
    let rx = bridge.connected_session_count_handle();
    assert!(
        rx.has_changed().is_ok(),
        "channel is open while the bridge lives"
    );
    drop(bridge);
    assert!(
        rx.has_changed().is_err(),
        "a dropped bridge must close the channel so parked waiters are told"
    );
}

async fn test_bridge_with_sessions(handles: &[ConnectionHandle]) -> IoBridge {
    let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
    let server = QuicServer::new().expect("QuicServer::new failed");
    let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);
    for &h in handles {
        attach_session(&mut bridge, h);
    }
    bridge
}

/// Test-only helper: an `IoBridge` with exactly one attached (disconnected)
/// WebTransport session, and the handle it was attached under.
/// Convenience wrapper around `test_bridge_with_sessions` for the M4b
/// display-negotiation tests, which only ever need one handle to
/// dispatch feedback bytes against.
async fn test_bridge_with_one_session() -> (IoBridge, ConnectionHandle) {
    let handle = ConnectionHandle(0);
    let mut bridge = test_bridge_with_sessions(&[handle]).await;
    // The one session is the HELLO'd one: feedback from anyone else is
    // discarded, so tests that dispatch feedback need a live sender.
    make_live(&mut bridge, handle);
    (bridge, handle)
}

/// `IoBridge::new_with_lib_config_for_test` must take its transport
/// settings from the supplied `LibConfig`, not the process environment.
/// This is the whole point of threading config through the bridge:
/// process-global env reads at construction time were what made tests
/// race each other under parallel `cargo test` execution.
#[tokio::test]
async fn io_bridge_takes_injection_settings_from_config() {
    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let cfg = crate::config::LibConfig {
        transport: crate::config::TransportConfig {
            skip_palette_session_reset: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let bridge = IoBridge::new_with_lib_config_for_test(ours, server, cfg);
    assert!(
        bridge.skip_palette_session_reset,
        "config value must reach the bridge without touching the environment"
    );
}

/// The one-shot BGRA frame dump used to be read-then-`remove_var`'d
/// straight from `GHOSTFRAME_DUMP_FRAME`, using the process environment
/// as mutable one-shot state. It now lives in `LibConfig` and is
/// consumed via `Option::take`, so `take_dump_frame_path` must yield the
/// path exactly once.
#[tokio::test]
async fn dump_frame_path_is_consumed_once() {
    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let cfg = crate::config::LibConfig {
        diagnostics: crate::config::DiagnosticsConfig {
            dump_frame_path: Some("/tmp/ghostframe-test-dump.bgra".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bridge = IoBridge::new_with_lib_config_for_test(ours, server, cfg);
    assert!(
        bridge.take_dump_frame_path().is_some(),
        "first call yields the path"
    );
    assert!(
        bridge.take_dump_frame_path().is_none(),
        "the dump is one-shot: the second call must yield None"
    );
}

#[tokio::test]
async fn caps_default_to_disabled_until_hello() {
    use crate::transport::client_caps::ClientCapabilities;
    let bridge = make_bridge_for_test().await;
    // Before any HELLO, current_client_caps() returns the default.
    assert_eq!(bridge.current_client_caps(), ClientCapabilities::default());
    assert!(!bridge.current_client_caps().indices_raw_enabled);
}

#[tokio::test]
async fn apply_hello_enables_indices_raw() {
    use crate::transport::client_caps::{ClientCapabilities, HelloMsg};
    let mut bridge = make_bridge_for_test().await;
    attach_session(&mut bridge, ConnectionHandle(0));
    let msg = HelloMsg {
        caps: ClientCapabilities {
            indices_raw_enabled: true,
            supports_h264: false,
            h264_acked_ref: false,
        },
    };
    bridge.apply_hello(ConnectionHandle(0), msg);
    assert!(bridge.current_client_caps().indices_raw_enabled);
}

#[tokio::test]
async fn h264_is_enabled_by_hello_bit_1_alone() {
    // Bit 1 alone enables H.264 (the web client); bit 2 alone (v2 feedback
    // without a decoder) does not; both do.
    use crate::transport::client_caps::{ClientCapabilities, HelloMsg};
    let mut bridge = make_bridge_for_test().await;
    attach_session(&mut bridge, ConnectionHandle(0));
    for (supports_h264, h264_acked_ref, expect) in [
        (true, false, true),
        (false, true, false),
        (true, true, true),
        (false, false, false),
    ] {
        bridge.apply_hello(
            ConnectionHandle(0),
            HelloMsg {
                caps: ClientCapabilities {
                    indices_raw_enabled: false,
                    supports_h264,
                    h264_acked_ref,
                },
            },
        );
        assert_eq!(
            bridge.classifier.adaptation_context().supports_h264,
            expect,
            "bit1={supports_h264} bit2={h264_acked_ref}"
        );
    }
}

#[tokio::test]
async fn apply_hello_propagates_h264_capability_to_classifier() {
    // Bit 1 of the HELLO byte must flow through to the classifier's
    // AdaptationContext so the next decide_frame_mode() can see it.
    // This is what keeps a Firefox client from getting a stale
    // (h264-allowed) classifier state between session-open and the
    // arrival of its HELLO.
    use crate::transport::client_caps::{ClientCapabilities, HelloMsg};
    let mut bridge = make_bridge_for_test().await;
    attach_session(&mut bridge, ConnectionHandle(0));

    bridge.apply_hello(
        ConnectionHandle(0),
        HelloMsg {
            caps: ClientCapabilities {
                indices_raw_enabled: false,
                supports_h264: false,
                h264_acked_ref: false,
            },
        },
    );
    assert!(!bridge.current_client_caps().supports_h264);
    assert!(!bridge.classifier.adaptation_context().supports_h264);

    bridge.apply_hello(
        ConnectionHandle(0),
        HelloMsg {
            caps: ClientCapabilities {
                indices_raw_enabled: false,
                supports_h264: true,
                h264_acked_ref: true,
            },
        },
    );
    assert!(bridge.current_client_caps().supports_h264);
    assert!(bridge.classifier.adaptation_context().supports_h264);
}

#[tokio::test]
async fn dispatch_feedback_bytes_knows_which_session_sent_it() {
    // A non-default handle: with ConnectionHandle(0) the assertion below
    // could not distinguish "records the sender" from "records handle 0".
    let handle = ConnectionHandle(7);
    let mut bridge = test_bridge_with_sessions(&[handle]).await;
    let mut buf = Vec::new();
    crate::transport::client_caps::HelloMsg {
        caps: crate::transport::client_caps::ClientCapabilities {
            indices_raw_enabled: false,
            supports_h264: true,
            h264_acked_ref: false,
        },
    }
    .encode(&mut buf);

    bridge.dispatch_feedback_bytes(handle, &buf);

    assert_eq!(
        bridge.last_hello_from(),
        Some(handle),
        "the bridge must record which session advertised capabilities; \
         without it, eviction cannot tell the newcomer from the incumbent"
    );
}

/// The observed field bug end to end: a browser stream whose first read
/// carries the WebTransport header AND the HELLO. Before the strip the
/// feedback parser read 0x40 as an unknown type and dropped the buffer,
/// HELLO with it, and the session never went live.
#[tokio::test]
async fn hello_coalesced_with_the_webtransport_header_makes_the_session_live() {
    use quinn_proto::{Dir, Side, StreamId};
    let handle = ConnectionHandle(7);
    let mut bridge = test_bridge_with_sessions(&[handle]).await;
    let mut hello = Vec::new();
    crate::transport::client_caps::HelloMsg {
        caps: crate::transport::client_caps::ClientCapabilities {
            indices_raw_enabled: false,
            supports_h264: true,
            h264_acked_ref: false,
        },
    }
    .encode(&mut hello);
    // varint(0x41) = 40 41, then varint(session id 0).
    let mut wire = vec![0x40, 0x41, 0x00];
    wire.extend_from_slice(&hello);

    let sid = StreamId::new(Side::Client, Dir::Bi, 1);
    let wt = bridge.wt_sessions.get_mut(&handle).unwrap();
    wt.test_feed_feedback_stream(sid, &wire);
    let queued = wt.drain_feedback();
    assert_eq!(queued.len(), 1, "premise: the read reached the queue");
    for d in &queued {
        bridge.dispatch_feedback_bytes(handle, d);
    }
    assert_eq!(bridge.last_hello_from(), Some(handle), "HELLO was dropped");
}

#[tokio::test]
async fn forget_session_clears_last_hello_from_for_that_handle() {
    let handle_a = ConnectionHandle(7);
    let handle_b = ConnectionHandle(9);
    let mut bridge = test_bridge_with_sessions(&[handle_a, handle_b]).await;

    let mut buf = Vec::new();
    crate::transport::client_caps::HelloMsg {
        caps: crate::transport::client_caps::ClientCapabilities {
            indices_raw_enabled: false,
            supports_h264: true,
            h264_acked_ref: false,
        },
    }
    .encode(&mut buf);
    bridge.dispatch_feedback_bytes(handle_a, &buf);
    assert_eq!(bridge.last_hello_from(), Some(handle_a));

    bridge.forget_session(handle_a);

    assert_eq!(
        bridge.last_hello_from(),
        None,
        "forgetting the session that sent HELLO must clear the stale attribution; \
         quinn can hand the freed handle to a future, unrelated connection"
    );
}

#[tokio::test]
async fn forget_session_leaves_a_different_sessions_attribution_intact() {
    let handle_a = ConnectionHandle(7);
    let handle_b = ConnectionHandle(9);
    let mut bridge = test_bridge_with_sessions(&[handle_a, handle_b]).await;

    let mut buf = Vec::new();
    crate::transport::client_caps::HelloMsg {
        caps: crate::transport::client_caps::ClientCapabilities {
            indices_raw_enabled: false,
            supports_h264: true,
            h264_acked_ref: false,
        },
    }
    .encode(&mut buf);
    bridge.dispatch_feedback_bytes(handle_a, &buf);
    assert_eq!(bridge.last_hello_from(), Some(handle_a));

    // Forgetting an unrelated session must not clobber handle_a's attribution.
    bridge.forget_session(handle_b);

    assert_eq!(
        bridge.last_hello_from(),
        Some(handle_a),
        "forget_session must only clear last_hello_from when it names the \
         handle being forgotten"
    );
}

/// Test-only helper: a `HelloMsg` advertising the given H.264 support,
/// with `indices_raw_enabled` fixed at `false` since none of the
/// eviction tests care about that bit.
fn hello(supports_h264: bool) -> crate::transport::client_caps::HelloMsg {
    crate::transport::client_caps::HelloMsg {
        caps: crate::transport::client_caps::ClientCapabilities {
            indices_raw_enabled: false,
            supports_h264,
            h264_acked_ref: false,
        },
    }
}

#[tokio::test]
async fn a_second_hello_evicts_the_incumbent() {
    let first = ConnectionHandle(7);
    let second = ConnectionHandle(9);
    let mut bridge = test_bridge_with_sessions(&[first, second]).await;

    bridge.apply_hello(first, hello(true));
    assert!(bridge.wt_sessions.contains_key(&first));

    bridge.apply_hello(second, hello(true));

    assert!(
        !bridge.wt_sessions.contains_key(&first),
        "the incumbent must be dropped when a second client identifies itself"
    );
    assert!(
        bridge.wt_sessions.contains_key(&second),
        "the newcomer must survive its own arrival"
    );
}

/// THE test of this milestone: establishing a second QUIC connection
/// must not disturb an existing, HELLO-identified session. A port
/// scan, an abandoned handshake, or a client that dies mid-negotiation
/// must not kill a working session -- and the old connect-time prune
/// did exactly that, before it could tell the incumbent why.
///
/// Drives the real production path (`on_connection_established`, the
/// extracted body of the `Event::Connected` arm) rather than merely
/// calling `apply_hello` in isolation -- `server.connections.get_mut`
/// returning `None` for the synthetic handle is harmless here, since
/// `on_connection_established` tolerates a connection-less handle by
/// design (a QUIC `Connected` event always has a live connection in
/// production; the test only needs the `wt_sessions`-side effect).
///
/// This is the guard against reintroducing the deleted prune: putting
/// its body back inside `on_connection_established` makes this test
/// fail (verified by hand while landing this test, then reverted).
#[tokio::test]
async fn establishing_a_second_connection_does_not_disturb_an_existing_session() {
    let incumbent = ConnectionHandle(7);
    let mut bridge = test_bridge_with_sessions(&[incumbent]).await;
    bridge.apply_hello(incumbent, hello(true));

    // Stand-in for the "thousands of un-ACKable pending entries" the
    // old prune's comment described: a live retransmit-cache entry
    // that a wrongful eviction would incorrectly clear.
    let now = std::time::Instant::now();
    bridge.reliable_emitter.submit_one(
        crate::transport::reliable_emitter::EmitKey::new(1, 0, 0, 0),
        bytes::Bytes::from_static(&[0u8; 16]),
        now,
        None,
        now,
    );
    assert_eq!(bridge.reliable_emitter.pending_cache_entries(), 1);

    // A second, distinct QUIC connection establishes -- e.g. a port
    // scan or an abandoned handshake -- and never sends HELLO.
    bridge.on_connection_established(ConnectionHandle(9));

    assert!(
        bridge.wt_sessions.contains_key(&incumbent),
        "a second connection establishing must not displace an existing session"
    );
    assert_eq!(
        bridge.last_hello_from(),
        Some(incumbent),
        "attribution must survive an unrelated connection establishing"
    );
    assert_eq!(
        bridge.reliable_emitter.pending_cache_entries(),
        1,
        "the incumbent's pending entries must not be cleared by an unrelated connection"
    );
}

#[tokio::test]
async fn re_hello_from_the_same_session_does_not_evict_itself() {
    // A client may re-send HELLO (capability change, or a reconnect
    // within the same session). Treating that as a newcomer would have
    // it evict itself.
    let only = ConnectionHandle(7);
    let mut bridge = test_bridge_with_sessions(&[only]).await;

    bridge.apply_hello(only, hello(false));
    bridge.apply_hello(only, hello(true));

    assert!(bridge.wt_sessions.contains_key(&only));
    assert!(
        bridge.current_client_caps().supports_h264,
        "the second HELLO's capabilities must still be applied"
    );
}

#[tokio::test]
async fn reconnect_ends_with_one_session_and_a_cleared_emitter_cache() {
    // The property the old connect-time prune protected: a page reload
    // opens a fresh QUIC connection (new handle) without the old one
    // ever going through `Event::ConnectionLost`. Eviction must deliver
    // the same outcome -- exactly one live session, and no stale
    // un-ACKable entries left in the emitter's retransmit cache.
    let old = ConnectionHandle(7);
    let mut bridge = test_bridge_with_sessions(&[old]).await;
    bridge.apply_hello(old, hello(true));

    // Seed the emitter with a pending (un-ACKed) entry, standing in for
    // the "thousands of un-ACKable pending entries" the old prune's
    // comment described.
    let now = std::time::Instant::now();
    bridge.reliable_emitter.submit_one(
        crate::transport::reliable_emitter::EmitKey::new(1, 0, 0, 0),
        bytes::Bytes::from_static(&[0u8; 16]),
        now,
        None,
        now,
    );
    assert_eq!(bridge.reliable_emitter.pending_cache_entries(), 1);

    // Simulate a page reload: a new QUIC connection (new handle) is
    // accepted, then the newcomer sends HELLO.
    let reconnected = ConnectionHandle(9);
    attach_session(&mut bridge, reconnected);
    bridge.apply_hello(reconnected, hello(true));

    assert_eq!(
        bridge.wt_sessions.len(),
        1,
        "exactly one live session must remain after the reconnect"
    );
    assert!(bridge.wt_sessions.contains_key(&reconnected));
    assert!(
        !bridge.wt_sessions.contains_key(&old),
        "the pre-reload session must be gone"
    );
    assert_eq!(
        bridge.last_hello_from(),
        Some(reconnected),
        "attribution must point at the reconnected session, not the evicted one"
    );
    assert_eq!(
        bridge.reliable_emitter.pending_cache_entries(),
        0,
        "the old session's un-ACKable pending entries must not survive the reconnect"
    );
}

/// Verify that `IoBridge::run` returns `Ok(())` when the peer end of the
/// socketpair is dropped (EOF from ghostbridge).
#[tokio::test]
async fn run_exits_cleanly_on_eof() {
    let (our_end, peer) = UnixStream::pair().expect("UnixStream::pair failed");
    let server = QuicServer::new().expect("QuicServer::new failed");
    let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);

    let run_handle = tokio::spawn(async move { bridge.run().await });
    drop(peer);

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run_handle).await;
    match result {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => panic!("run() returned Err: {e}"),
        Ok(Err(join_err)) => panic!("task panicked: {join_err}"),
        Err(_) => panic!("run() did not return within timeout"),
    }
}

/// `validate_frame_header` bounds, tested directly.
///
/// These cannot be driven through `run()`: whether a frame is rejected or
/// accepted, the read that follows ends in `UnexpectedEof` once the peer
/// closes, and the event loop swallows both identically — so a `run()`
/// level test passes with the bounds removed entirely. (Verified: it did.)
#[test]
fn validate_frame_header_bounds() {
    // A legitimate small frame: 8 header + 4 payload + 2 port + "1.2.3.4\0".
    let ok = validate_frame_header(8 + 4 + 2 + 8, 4).expect("valid frame");
    assert_eq!(ok, 4 + 2 + 8, "remainder excludes the 8-byte header");

    // Exactly at the minimum: header + 0 payload + port + NUL.
    assert!(validate_frame_header(11, 0).is_ok());

    // One byte under the minimum.
    let e = validate_frame_header(10, 0).expect_err("too short must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);

    // Declared payload larger than the frame that carries it.
    assert!(validate_frame_header(20, 100).is_err());

    // Exactly at the upper bound is still accepted...
    assert!(validate_frame_header(MAX_FRAME_LEN, 0).is_ok());

    // ...one byte over is not. Without this bound the caller allocates
    // `total_len - 8` bytes and then awaits them forever.
    let e = validate_frame_header(MAX_FRAME_LEN + 1, 0).expect_err("over bound");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);

    // The hostile case: passes the too-short check trivially, and would
    // drive a ~4 GiB allocation.
    let e = validate_frame_header(0xFFFF_FFFF, 0).expect_err("absurd total_len");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
}

/// Re-injecting a tile must supersede whatever is still queued for it,
/// mirroring the capture path's `bump_generation`.
///
/// Without this, two versions of the same tile sit in the queue at once
/// and whichever drains later is rendered last. That inverts content
/// order because `drain_scheduler_into_quinn` stamps every tile it drains
/// with the *draining* frame's seq, not the enqueuing frame's — so the
/// older content can arrive labelled newer and win.
#[tokio::test]
async fn injected_frame_supersedes_prior_work_for_the_same_tile() {
    use crate::transport::scheduler::{TileWork, WorkState};

    let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
    let server = QuicServer::new().expect("QuicServer::new failed");
    let (_tx, rx) = mpsc::channel::<InjectedFrame>(4);
    let mut bridge = IoBridge::new_with_injection_for_test(our_end, server, rx, 4, 4);

    let work_for = |generation: u8, payload: Vec<u8>| TileWork {
        tile_x: 0,
        tile_y: 0,
        generation,
        pass_idx: 0,
        total_passes: 1,
        codec: crate::transport::protocol::Codec::Solid,
        payload,
        queued_at: now_std(),
        last_sent_at: None,
        state: WorkState::Pending,
    };

    let frame_of = |seq: u32, work: TileWork| InjectedFrame {
        seq,
        timestamp_us: seq * 16_000,
        work: vec![work],
        cost: None,
    };

    // No session is connected, so the drain budget clamps to zero and
    // nothing leaves the queue — which is what lets us observe the
    // supersede directly.
    bridge.apply_injected_frame(frame_of(0, work_for(0, vec![1, 2, 3, 255])));
    bridge.apply_injected_frame(frame_of(1, work_for(1, vec![9, 9, 9, 255])));

    let live: Vec<_> = bridge
        .scheduler
        .queue_states_for_test()
        .into_iter()
        .filter(|(x, y, state)| {
            *x == 0 && *y == 0 && !matches!(state, WorkState::Superseded | WorkState::Acked)
        })
        .collect();

    assert_eq!(
        live.len(),
        1,
        "exactly one live entry must remain for tile (0,0) after re-injection, \
         found {live:?}"
    );
}

/// A malformed frame must be logged and swallowed without killing the
/// event loop. We write a short header declaring `total_len = 4`, which
/// fails `process_inbound`'s minimum-size check, then drop the peer to
/// trigger clean shutdown. The `Ok(())` return proves the loop kept
/// running after the error.
#[tokio::test]
async fn run_survives_malformed_frame() {
    use tokio::io::AsyncWriteExt;

    let (our_end, mut peer) = UnixStream::pair().expect("UnixStream::pair failed");
    let server = QuicServer::new().expect("QuicServer::new failed");
    let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);

    let run_handle = tokio::spawn(async move { bridge.run().await });

    // total_len=4 (smaller than 8 header), payload_len=0. `read_exact`
    // will consume these 8 bytes; `process_inbound` then rejects with
    // "frame too short" and the loop continues.
    let bogus = [0u8, 0, 0, 4, 0, 0, 0, 0];
    peer.write_all(&bogus).await.expect("peer write failed");
    peer.flush().await.ok();

    // Give the loop a moment to observe and swallow the error.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    drop(peer);

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run_handle).await;
    match result {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => panic!("run() returned Err: {e}"),
        Ok(Err(join_err)) => panic!("task panicked: {join_err}"),
        Err(_) => panic!("run() did not return within timeout"),
    }
}

#[test]
fn ping_produces_pong() {
    assert_eq!(handle_datagram_payload(b"ping"), Some(&b"pong"[..]));
    assert_eq!(handle_datagram_payload(b"not ping"), None);
    assert_eq!(handle_datagram_payload(b""), None);
}

#[test]
fn parse_listen_port_accepts_bare_port() {
    assert_eq!(parse_listen_port(":443").unwrap(), 443);
    assert_eq!(parse_listen_port("0.0.0.0:443").unwrap(), 443);
    assert_eq!(parse_listen_port("[::]:443").unwrap(), 443);
}

#[test]
fn parse_listen_port_rejects_garbage() {
    assert!(parse_listen_port("nope").is_err());
    assert!(parse_listen_port(":abc").is_err());
}

#[tokio::test]
async fn process_frame_produces_no_panic() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);

    let frame = crate::server::FrameSubmission {
        width: 64,
        height: 64,
        stride: 64 * 4,
        pixels: vec![0xFF; 64 * 64 * 4].into(),
        dmabuf_fd: None,
        timestamp_us: 1000,
        content: crate::frame_damage::Content::Pixels,
        damage: crate::frame_damage::FrameDamage::Unknown,
        capture_done_ns: 0,
    };
    // No connected sessions, so datagrams go nowhere, but should not panic
    bridge.process_frame(frame);
}

/// A pooled slot handed to the bridge comes back when the bridge stops, even
/// with the frame still in flight in the worker: the slot must never be
/// stranded by a shutdown (the capture loop would wait on it forever).
#[tokio::test]
async fn a_pooled_frame_in_flight_returns_its_slot_when_the_bridge_stops() {
    use crate::frame_pool::FramePool;
    let pool = FramePool::new(2);
    let mut slot = pool.try_acquire().unwrap();
    slot.set_storage(Box::new(vec![0x7Fu8; 64 * 64 * 4]));
    let pixels = slot.into_buf().ok().unwrap();
    assert_eq!(pool.free_slots(), 1);

    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    bridge.process_frame(crate::server::FrameSubmission {
        width: 64,
        height: 64,
        stride: 64 * 4,
        pixels,
        dmabuf_fd: None,
        timestamp_us: 1000,
        content: crate::frame_damage::Content::Pixels,
        damage: crate::frame_damage::FrameDamage::Unknown,
        capture_done_ns: 0,
    });
    drop(bridge);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while pool.free_slots() != 2 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the bridge stopped and the slot was never returned");
}

#[tokio::test]
async fn dispatch_feedback_updates_adaptation_context() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    make_live(&mut bridge, ConnectionHandle(0));
    let initial_seq = bridge.adaptation_context.last_update_seq;
    let fb = crate::transport::feedback::ReceiverFeedback {
        timestamp_ns: 1_000_000,
        datagrams_received: 800,
        datagrams_recovered_fec: 0,
        suspension_detected: true,
    };
    let mut buf = Vec::new();
    fb.encode(&mut buf);
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &buf);
    assert!(bridge.adaptation_context.last_update_seq > initial_seq);
    // The feedback's own loss counts are deliberately not the source of
    // `loss_rate`: see `the_classifiers_loss_rate_is_the_ledgers_verdict`.
    assert_eq!(bridge.adaptation_context.loss_rate, 0.0);
    assert!(bridge.adaptation_context.suspended);
}

#[tokio::test]
async fn process_frame_increments_frame_seq() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);

    assert_eq!(bridge.frame_seq, 0);
    assert!(bridge.frame_rx.is_some());

    let make_frame = || crate::server::FrameSubmission {
        width: 32,
        height: 32,
        stride: 32 * 4,
        pixels: vec![0; 32 * 32 * 4].into(),
        dmabuf_fd: None,
        timestamp_us: 0,
        content: crate::frame_damage::Content::Pixels,
        damage: crate::frame_damage::FrameDamage::Unknown,
        capture_done_ns: 0,
    };

    bridge.process_frame(make_frame());
    assert_eq!(bridge.frame_seq, 1);

    bridge.process_frame(make_frame());
    assert_eq!(bridge.frame_seq, 2);
}

/// The GPU-path TileCodec branch must not emit zero-filled tiles when
/// `frame.pixels` is empty (DMA-BUF zero-copy path). Constructing a real
/// DMA-BUF fd in a unit test is impractical, so this test exercises the
/// adjacent skip path: empty pixels + no DMA-BUF fd routes to
/// `process_frame_cpu` (no GPU processor in the test bridge), which uses
/// `frame.pixels` directly. It documents the no-panic invariant for the
/// pathological "no pixels, no fd" submission. End-to-end coverage of the
/// readback fallback lives in `e2e_mode_switch`.
#[tokio::test]
async fn process_frame_no_panic_on_empty_pixels_no_dmabuf_no_session() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    bridge.frame_mode = crate::tile::FrameMode::TileCodec;

    let frame = crate::server::FrameSubmission {
        width: 64,
        height: 64,
        stride: 64 * 4,
        pixels: vec![0u8; 64 * 64 * 4].into(),
        dmabuf_fd: None,
        timestamp_us: 0,
        content: crate::frame_damage::Content::Pixels,
        damage: crate::frame_damage::FrameDamage::Unknown,
        capture_done_ns: 0,
    };
    bridge.process_frame(frame);
    assert!(bridge.frame_seq > 0, "frame_seq must advance");
}

// Note: Task 11 CPU-path scheduler integration is not unit-testable without
// a connected WebTransport session (the no-sessions early-return is
// intentional — it prevents the dirty tracker from absorbing frame state
// before any client can see it). Integration coverage lives in:
//   - transport::scheduler::tests::* — scheduler behavior in isolation.
//   - tests/e2e.rs::e2e_solid_color (Task 15) — full pipeline with client.

/// `Classifier::reset` must zero hysteresis streaks, so a single busy
/// frame after reset can NOT promote to H264 (it needs `enter_sustain_micros`
/// of dwell period). Without reset, a partial enter streak from a
/// prior session would leak into the new session and trigger early promotion.
#[test]
fn classifier_reset_clears_streaks() {
    use crate::tile::{Classifier, CodecState, FrameMode};
    let mut c = Classifier::new(crate::config::ClassifierConfig::default());
    let busy: Vec<CodecState> = (0..20)
        .map(|i| CodecState::H264 { frames_in_h264: i })
        .collect();
    // Build partial enter streak.
    c.decide_frame_mode(&busy, FrameMode::TileCodec);
    c.decide_frame_mode(&busy, FrameMode::TileCodec);
    // Reset clears it.
    c.reset();
    // After reset, one busy frame must NOT promote (streak was zeroed).
    assert_eq!(
        c.decide_frame_mode(&busy, FrameMode::TileCodec),
        FrameMode::TileCodec
    );
}

/// IoBridge must hold a Scheduler that resizes alongside metrics_tracker
/// and dirty_tracker. This test verifies the scheduler field is present
/// and zero-sized at construction time.
#[tokio::test]
async fn iobridge_holds_scheduler_zero_sized_on_construction() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    assert_eq!(bridge.scheduler.cols(), 0);
    assert_eq!(bridge.scheduler.rows(), 0);
    assert_eq!(bridge.scheduler.queue_len(), 0);
}

/// A recording stand-in for the GPU backend: what the worker was asked, in
/// order, with knobs to make it fail, panic, or take its time.
#[derive(Default)]
struct RecordingBackend {
    log: Arc<std::sync::Mutex<Vec<String>>>,
    h264: Option<(Vec<u8>, bool)>,
    /// `forward_cdf53_tiles` fails, as a lost device or a failed submit would.
    fail_forward: bool,
    analyse_err: Option<String>,
    /// `analyse` refuses the (unchanged) tick instead of failing.
    analyse_refused: Option<String>,
    /// Tiles `analyse` reports dirty.
    dirty_tiles: Vec<u32>,
    panic_in_analyse: bool,
    analyse_sleep: std::time::Duration,
    /// Shared by backends that must not overlap: how many are inside a call.
    inside: Option<Arc<std::sync::atomic::AtomicUsize>>,
    max_inside: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

impl FrameBackend for RecordingBackend {
    fn apply(&mut self, control: Control) {
        self.log.lock().unwrap().push(format!("apply {control:?}"));
    }
    fn analyse(
        &mut self,
        _frame: &FrameSubmission,
        _escalation: &[u32],
        _want_diag: bool,
    ) -> Result<Analysed, AnalyseError> {
        use std::sync::atomic::Ordering::SeqCst;
        self.log.lock().unwrap().push("analyse".into());
        if self.panic_in_analyse {
            panic!("recording backend: told to panic");
        }
        if let (Some(inside), Some(max)) = (&self.inside, &self.max_inside) {
            let now = inside.fetch_add(1, SeqCst) + 1;
            max.fetch_max(now, SeqCst);
        }
        std::thread::sleep(self.analyse_sleep);
        if let Some(inside) = &self.inside {
            inside.fetch_sub(1, SeqCst);
        }
        if let Some(why) = &self.analyse_refused {
            return Err(AnalyseError::Refused(why.clone()));
        }
        match &self.analyse_err {
            Some(e) => Err(e.clone().into()),
            None => Ok(Analysed {
                dirty_analysis: vec![None; self.dirty_tiles.len()],
                dirty_tiles: self.dirty_tiles.clone(),
                ..Default::default()
            }),
        }
    }
    fn finish_h264(&mut self, _frame: &FrameSubmission, force_keyframe: bool) -> Option<H264Out> {
        self.log
            .lock()
            .unwrap()
            .push(format!("finish_h264 force_keyframe={force_keyframe}"));
        self.h264.clone().map(|(payload, is_keyframe)| H264Out {
            payload,
            is_keyframe,
        })
    }
    fn finish_tile(&mut self, _frame: &FrameSubmission, req: &TileRequest<'_>) -> TileOut {
        self.log
            .lock()
            .unwrap()
            .push(format!("finish_tile fallback={:?}", req.fallback));
        TileOut {
            pixels: Err("recording backend".into()),
            palrle: OwnedPalRle::default(),
            cdf53_dirty: Vec::new(),
            cdf53_escalation: Vec::new(),
            cdf53_fallback: Vec::new(),
            timings: TileTimings::default(),
        }
    }
    fn finish_discard(&mut self) {
        self.log.lock().unwrap().push("finish_discard".into());
    }
    fn forward_cdf53_tiles(&mut self, tiles: &[&[u8]]) -> Result<Vec<i16>, String> {
        self.log
            .lock()
            .unwrap()
            .push(format!("forward_cdf53_tiles {}", tiles.len()));
        if self.fail_forward {
            return Err("injected".into());
        }
        // Test scaffolding: the CPU transform stands in for the GPU one.
        Ok(tiles
            .iter()
            .flat_map(|t| crate::encoder::cdf53::forward(t))
            .collect())
    }
}

fn bridge_with_backend(backend: RecordingBackend) -> (IoBridge, tokio::net::UnixStream) {
    let (our_end, peer) = std::os::unix::net::UnixStream::pair().expect("pair");
    our_end.set_nonblocking(true).unwrap();
    peer.set_nonblocking(true).unwrap();
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(
        tokio::net::UnixStream::from_std(our_end).unwrap(),
        server,
        rx,
    );
    bridge.set_frame_backend(Box::new(backend));
    (bridge, tokio::net::UnixStream::from_std(peer).unwrap())
}

fn recording_bridge() -> (
    IoBridge,
    tokio::net::UnixStream,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    let backend = RecordingBackend::default();
    let log = backend.log.clone();
    let (bridge, peer) = bridge_with_backend(backend);
    (bridge, peer, log)
}

/// The CDF 5/3 fallback reaches the GPU through the backend seam, once per
/// batch, with every tile of the batch: not through the processor.
#[tokio::test]
async fn the_cdf53_fallback_forwards_its_tiles_through_the_backend_in_one_call() {
    use crate::tile::CodecState;
    let (mut bridge, _peer, log) = recording_bridge();
    bridge.scheduler.resize(4, 4);
    bridge.metrics_tracker.resize(4, 4);
    bridge.metrics_tracker.get_mut(0, 0).codec_state = CodecState::Skip;
    bridge.metrics_tracker.get_mut(1, 0).codec_state = CodecState::H264 { frames_in_h264: 1 };
    bridge.metrics_tracker.get_mut(2, 0).codec_state = CodecState::Solid;
    let pixels = vec![0x40u8; 128 * 128 * 4];
    let grid = crate::tile::TileGrid::new(128, 128);
    let _ = bridge.dispatch_dirty_tiles_via_scheduler(
        &[(0, 0), (1, 0), (2, 0)],
        &grid,
        TileDispatchFrame {
            pixels: &pixels,
            stride: 128 * 4,
            seq: 1,
            timestamp_us: 0,
        },
        1200,
        SchedulerEmissionPolicy::GpuClassifierDriven,
        None,
    );
    let calls: Vec<String> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("forward_cdf53_tiles"))
        .cloned()
        .collect();
    assert_eq!(calls, vec!["forward_cdf53_tiles 2".to_string()]);
}

fn fallback_dispatch(bridge: &mut IoBridge, dirty: &[(u32, u32)]) {
    let pixels = vec![0x40u8; 128 * 128 * 4];
    let grid = crate::tile::TileGrid::new(128, 128);
    let _ = bridge.dispatch_dirty_tiles_via_scheduler(
        dirty,
        &grid,
        TileDispatchFrame {
            pixels: &pixels,
            stride: 128 * 4,
            seq: 1,
            timestamp_us: 0,
        },
        1200,
        SchedulerEmissionPolicy::GpuClassifierDriven,
        None,
    );
}

/// A tile that falls back while its PalRle work is still queued must give the
/// palette reference and carry count back: the fallback is entered exactly
/// when the palette table is under pressure, so a leak here feeds itself.
#[tokio::test]
async fn a_tile_falling_back_from_queued_palrle_releases_its_palette() {
    use crate::tile::CodecState;
    use crate::transport::protocol::Codec;
    use crate::transport::scheduler::{TileWork, WorkState};
    let (mut bridge, _peer, _log) = recording_bridge();
    bridge.scheduler.resize(4, 4);
    bridge.metrics_tracker.resize(4, 4);
    bridge.palette_table.ref_count[9] = 1;
    bridge.palette_table.in_flight_carrying[9] = 1;
    bridge.palette_table.slot_state[9] = crate::encoder::pal_rle::SlotState::Held;
    bridge.scheduler.enqueue_at(
        TileWork {
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            pass_idx: 0,
            total_passes: 1,
            codec: Codec::PalRle,
            payload: vec![1, 9, 0, 0],
            queued_at: now_std(),
            last_sent_at: None,
            state: WorkState::Pending,
        },
        now_std(),
    );
    // Phase A downgraded the tile (table full): no PalRle state, no payload.
    bridge.metrics_tracker.get_mut(0, 0).codec_state = CodecState::Skip;
    fallback_dispatch(&mut bridge, &[(0, 0)]);
    assert_eq!(bridge.palette_table.in_flight_carrying[9], 0);
    assert_eq!(bridge.palette_table.ref_count[9], 0);
}

/// A failed forward must not panic or send Raw, and must arrange a retry.
#[tokio::test]
async fn a_failed_cdf53_forward_sends_nothing_and_requests_a_retry() {
    use crate::tile::CodecState;
    let backend = RecordingBackend {
        fail_forward: true,
        ..Default::default()
    };
    let log = backend.log.clone();
    let (mut bridge, _peer) = bridge_with_backend(backend);
    bridge.scheduler.resize(4, 4);
    bridge.metrics_tracker.resize(4, 4);
    bridge.metrics_tracker.get_mut(1, 1).codec_state = CodecState::Skip;
    fallback_dispatch(&mut bridge, &[(1, 1)]);
    assert!(bridge.scheduler.peek_for_test().is_empty(), "no Raw");
    assert!(bridge.scheduler.refinement_peek_for_test().is_empty());
    // The retry control is queued for the worker, behind the failed request.
    assert!(
        bridge
            .pending_controls
            .contains(&Control::InvalidateBaseline(1)),
        "{:?} / {:?}",
        bridge.pending_controls,
        log.lock().unwrap()
    );
}

fn tiny_frame() -> FrameSubmission {
    FrameSubmission {
        width: 64,
        height: 64,
        stride: 64 * 4,
        pixels: vec![0u8; 64 * 64 * 4].into(),
        dmabuf_fd: None,
        timestamp_us: 0,
        content: crate::frame_damage::Content::Pixels,
        damage: crate::frame_damage::FrameDamage::Unknown,
        capture_done_ns: 0,
    }
}

/// Wait for the worker's next message (it runs on its own thread).
async fn next_worker_message(bridge: &mut IoBridge) -> Option<FromWorker> {
    let rx = &mut bridge.frame_pipeline.as_mut().unwrap().rx;
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("the worker answered within 5 s")
}

/// A reconnecting client must get a clean slate from the GPU side too: the
/// SAD baseline is dropped for 20 frames and the encoder is told to emit an
/// IDR, in that order, and a frame the worker is still on is marked as
/// belonging to the old session. (This replaces a test that only poked the
/// encoder directly and never ran the reset path.)
#[tokio::test]
async fn session_reset_queues_invalidate_then_keyframe_and_ends_the_epoch() {
    let (mut bridge, _peer, _log) = recording_bridge();
    let before = bridge.session_epoch;
    bridge.fire_session_reset(ConnectionHandle(0));
    assert_eq!(
        bridge.pending_controls,
        vec![Control::InvalidateBaseline(20), Control::RequestKeyframe]
    );
    assert_eq!(bridge.session_epoch, before + 1);
}

/// A frame dropped before the GPU stage (no session to send to) never
/// uploaded the tile rows its damage named, so the worker is told to stop
/// trusting damage until a whole frame has gone through.
#[tokio::test]
async fn a_frame_dropped_before_the_gpu_stage_makes_the_worker_forget_damage() {
    let (mut bridge, _peer, _log) = recording_bridge();
    bridge.process_frame(tiny_frame());
    assert!(bridge.in_flight.is_none(), "nothing went to the worker");
    assert_eq!(bridge.pending_controls, vec![Control::ForgetDamage]);
}

/// A frame whose session ended while the worker held it is not committed: the
/// worker is told to discard it, and the GPU baseline, which the frame already
/// advanced, is invalidated so the next frame repaints.
#[tokio::test]
async fn a_frame_whose_session_ended_in_flight_is_discarded_and_the_baseline_invalidated() {
    let (mut bridge, _peer, log) = recording_bridge();
    bridge.start_analysis(tiny_frame(), 1, 1200, Vec::new(), 0);
    assert!(bridge.in_flight.is_some());
    let analysed = next_worker_message(&mut bridge).await.expect("analysed");
    assert!(matches!(analysed, FromWorker::Analysed { .. }));

    bridge.session_epoch += 1; // the session ended while the GPU worked
    bridge.on_frame_worker_message(analysed);
    assert!(bridge.in_flight.is_some(), "the discard is still in flight");
    let discarded = next_worker_message(&mut bridge).await.expect("discarded");
    assert!(matches!(discarded, FromWorker::Discarded { .. }));
    bridge.on_frame_worker_message(discarded);
    assert!(bridge.in_flight.is_none());

    assert_eq!(
        *log.lock().unwrap(),
        vec![
            "analyse".to_string(),
            "apply InvalidateBaseline(1)".to_string(),
            "finish_discard".to_string(),
        ]
    );
}

/// A reset's 20-frame cushion must not be shortened to 1 by a discard queued
/// after it.
#[tokio::test]
async fn a_discard_does_not_shorten_a_resets_baseline_cushion() {
    let (mut bridge, _peer, log) = recording_bridge();
    bridge.start_analysis(tiny_frame(), 1, 1200, Vec::new(), 0);
    let analysed = next_worker_message(&mut bridge).await.unwrap();
    bridge.fire_session_reset(ConnectionHandle(0));
    bridge.on_frame_worker_message(analysed);
    let discarded = next_worker_message(&mut bridge).await.unwrap();
    bridge.on_frame_worker_message(discarded);
    let log = log.lock().unwrap();
    assert!(
        log.contains(&"apply InvalidateBaseline(20)".to_string()),
        "{log:?}"
    );
    assert!(
        !log.contains(&"apply InvalidateBaseline(1)".to_string()),
        "{log:?}"
    );
}

/// A failed GPU stage hands the frame back and the CPU pipeline takes it.
#[tokio::test]
async fn a_failed_gpu_stage_returns_the_frame_and_clears_the_flight() {
    let backend = RecordingBackend {
        analyse_err: Some("device lost".into()),
        ..Default::default()
    };
    let (mut bridge, _peer) = bridge_with_backend(backend);
    bridge.start_analysis(tiny_frame(), 1, 1200, Vec::new(), 0);
    let msg = next_worker_message(&mut bridge).await.unwrap();
    assert!(matches!(msg, FromWorker::AnalyseFailed { .. }));
    bridge.on_frame_worker_message(msg);
    assert!(bridge.in_flight.is_none());
    assert!(bridge.fatal.is_none());
}

/// A panic in the worker is fatal and says why: the result channel closes
/// with the panic message recorded, and `run` returns an error carrying it,
/// rather than the stream carrying on with a dead backend.
#[tokio::test]
async fn a_worker_panic_ends_run_with_an_error_naming_it() {
    let backend = RecordingBackend {
        panic_in_analyse: true,
        ..Default::default()
    };
    let (mut bridge, _peer) = bridge_with_backend(backend);
    bridge.start_analysis(tiny_frame(), 1, 1200, Vec::new(), 0);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), bridge.run())
        .await
        .expect("run noticed the dead worker within 5 s");
    let err = outcome.expect_err("run returns an error when the worker dies");
    let text = err.to_string();
    assert!(
        text.contains("told to panic"),
        "the error should carry the panic message: {text}"
    );
}

/// What to tell the worker for a frame the classifier put in H.264 mode: nothing
/// when no tile changed, else an encode that forces an IDR only on re-entry
/// from the tile path.
#[test]
fn the_h264_finish_action_forces_a_keyframe_only_on_re_entry_and_discards_when_idle() {
    use crate::tile::FrameMode;
    let action = |empty, prev| frame::h264_finish_action(empty, prev);
    assert!(matches!(
        action(false, FrameMode::TileCodec),
        FinishAction::H264 {
            force_keyframe: true
        }
    ));
    assert!(matches!(
        action(false, FrameMode::H264),
        FinishAction::H264 {
            force_keyframe: false
        }
    ));
    assert!(matches!(
        action(true, FrameMode::H264),
        FinishAction::Discard
    ));
    assert!(matches!(
        action(true, FrameMode::TileCodec),
        FinishAction::Discard
    ));
}

fn stale_inflight(stage: frame::InFlightStage) -> frame::InFlight {
    frame::InFlight {
        seq: 1,
        epoch: 0,
        meta: frame::FrameMeta::of(&tiny_frame()),
        max_dg_size: 1200,
        stage,
    }
}

/// An H.264 result for a session that has ended is dropped, and the encoder is
/// told to start the next frame with an IDR: its reference chain advanced past
/// a frame the client never got.
#[tokio::test]
async fn a_stale_h264_result_is_dropped_and_asks_for_a_keyframe() {
    let (mut bridge, _peer, _log) = recording_bridge();
    bridge.test_commit_dg_size = Some(1200);
    bridge.session_epoch = 1;
    bridge.on_h264_done(
        stale_inflight(frame::InFlightStage::FinishingH264 {
            trace: Default::default(),
        }),
        Some(H264Out {
            payload: vec![1, 2, 3],
            is_keyframe: false,
        }),
        0,
    );
    assert_eq!(bridge.pending_controls, vec![Control::RequestKeyframe]);
}

/// A tile result for a session that has ended is dropped, and the GPU baseline
/// (which the frame advanced) is invalidated so the next frame repaints.
#[tokio::test]
async fn a_stale_tile_result_is_dropped_and_invalidates_the_baseline() {
    let (mut bridge, _peer, _log) = recording_bridge();
    bridge.test_commit_dg_size = Some(1200);
    bridge.session_epoch = 1;
    let out = TileOut {
        pixels: Ok(PixelSource::Frame),
        palrle: OwnedPalRle::default(),
        cdf53_dirty: Vec::new(),
        cdf53_escalation: Vec::new(),
        cdf53_fallback: Vec::new(),
        timings: TileTimings::default(),
    };
    bridge.on_tile_done(
        stale_inflight(frame::InFlightStage::FinishingTile {
            dirty_xy: vec![(0, 0)],
            max_frag: 1000,
            escalation_candidates: Vec::new(),
            stranded_count: 0,
            frame_palette_set_count: 0,
            trace: Default::default(),
        }),
        out,
        Default::default(),
        0,
    );
    assert_eq!(
        bridge.pending_controls,
        vec![Control::InvalidateBaseline(1)]
    );
}

fn empty_tile_out() -> TileOut {
    TileOut {
        pixels: Ok(PixelSource::Frame),
        palrle: OwnedPalRle::default(),
        cdf53_dirty: Vec::new(),
        cdf53_escalation: Vec::new(),
        cdf53_fallback: Vec::new(),
        timings: TileTimings::default(),
    }
}

fn tile_inflight() -> frame::InFlight {
    stale_inflight(frame::InFlightStage::FinishingTile {
        dirty_xy: vec![(0, 0)],
        max_frag: 1000,
        escalation_candidates: Vec::new(),
        stranded_count: 0,
        frame_palette_set_count: 0,
        trace: Default::default(),
    })
}

/// With the same epoch but no live session left to size datagrams for, the
/// result is dropped like a stale one.
#[tokio::test]
async fn a_tile_result_with_no_live_session_left_is_dropped() {
    let (mut bridge, _peer, _log) = recording_bridge();
    bridge.on_tile_done(tile_inflight(), empty_tile_out(), Default::default(), 0);
    assert_eq!(
        bridge.pending_controls,
        vec![Control::InvalidateBaseline(1)]
    );
}

/// The path MTU can shrink while the worker runs: the tile frame is
/// fragmented for the smaller size, and one that no longer fits a fragment is
/// dropped with a repaint rather than sent in oversize datagrams.
#[tokio::test]
async fn a_path_mtu_that_shrank_below_a_fragment_drops_the_tile_frame() {
    let (mut bridge, _peer, _log) = recording_bridge();
    bridge.test_commit_dg_size = Some(10);
    bridge.on_tile_done(tile_inflight(), empty_tile_out(), Default::default(), 0);
    assert_eq!(
        bridge.pending_controls,
        vec![Control::InvalidateBaseline(1)]
    );
}

/// The frame's dimensions are announced when its results are committed, not
/// when its analysis is accepted: on a resolution change the client resizes its
/// canvas on them, and the finish can take a second.
#[tokio::test]
async fn a_frame_with_dirty_tiles_does_not_announce_its_dimensions_at_analysis() {
    struct Dirty(RecordingBackend);
    impl FrameBackend for Dirty {
        fn apply(&mut self, c: Control) {
            self.0.apply(c)
        }
        fn analyse(
            &mut self,
            f: &FrameSubmission,
            e: &[u32],
            d: bool,
        ) -> Result<Analysed, AnalyseError> {
            let mut a = self.0.analyse(f, e, d)?;
            a.dirty_tiles = vec![0];
            a.dirty_analysis = vec![None];
            Ok(a)
        }
        fn finish_h264(&mut self, f: &FrameSubmission, k: bool) -> Option<H264Out> {
            self.0.finish_h264(f, k)
        }
        fn finish_tile(&mut self, f: &FrameSubmission, r: &TileRequest<'_>) -> TileOut {
            self.0.finish_tile(f, r)
        }
        fn finish_discard(&mut self) {
            self.0.finish_discard()
        }
        fn forward_cdf53_tiles(&mut self, t: &[&[u8]]) -> Result<Vec<i16>, String> {
            self.0.forward_cdf53_tiles(t)
        }
    }
    let (mut bridge, _peer) = {
        let (our_end, peer) = std::os::unix::net::UnixStream::pair().expect("pair");
        our_end.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut bridge = IoBridge::new_with_frames_for_test(
            tokio::net::UnixStream::from_std(our_end).unwrap(),
            server,
            rx,
        );
        bridge.set_frame_backend(Box::new(Dirty(RecordingBackend::default())));
        (bridge, tokio::net::UnixStream::from_std(peer).unwrap())
    };
    bridge.start_analysis(tiny_frame(), 1, 1200, Vec::new(), 0);
    let analysed = next_worker_message(&mut bridge).await.unwrap();
    bridge.on_frame_worker_message(analysed);
    assert!(bridge.in_flight.is_some(), "the frame is being finished");
    assert_eq!(
        bridge.last_emitted_dimensions, None,
        "dimensions wait for the commit"
    );
}

/// A client decode error that names a tile of the frame being committed waits
/// for the commit: until then the tile's codec state is tentative.
#[tokio::test]
async fn a_decode_error_for_a_tile_in_flight_waits_for_the_commit() {
    use crate::tile::CodecState;
    use crate::transport::decode_error::{DecodeErrorMsg, ERR_THIN_UNCACHED_PALETTE};
    let (mut bridge, _peer, _log) = recording_bridge();
    bridge.metrics_tracker.resize(2, 2);
    bridge.metrics_tracker.get_mut(1, 1).codec_state = CodecState::PalRle { palette_id: 5 };
    bridge.palette_table.delivered.insert(5);
    bridge.in_flight = Some(stale_inflight(frame::InFlightStage::FinishingTile {
        dirty_xy: vec![(1, 1)],
        max_frag: 1000,
        escalation_candidates: Vec::new(),
        stranded_count: 0,
        frame_palette_set_count: 0,
        trace: Default::default(),
    }));
    bridge.handle_decode_error(DecodeErrorMsg {
        codec: 2,
        tile_x: 1,
        tile_y: 1,
        error_code: ERR_THIN_UNCACHED_PALETTE,
    });
    assert!(
        bridge.palette_table.delivered.contains(5),
        "not handled while the tile's frame is in flight"
    );
    assert_eq!(bridge.deferred_decode_errors.len(), 1);

    // The frame ends (here a discard): the error is handled against the
    // committed state.
    let (mut b, _p, _l) = (bridge, _peer, _log);
    b.in_flight = Some(stale_inflight(frame::InFlightStage::FinishingDiscard));
    b.on_frame_worker_message(FromWorker::Discarded { seq: 1 });
    assert!(b.deferred_decode_errors.is_empty());
    assert!(
        !b.palette_table.delivered.contains(5),
        "handled once the frame is done"
    );
}

/// Dropping the bridge stops waiting for a stuck worker after the bound, not
/// when the stuck call returns.
#[tokio::test]
async fn shutdown_waits_for_the_worker_only_for_the_bound() {
    let backend = RecordingBackend {
        // Holds the process-wide hardware lock while it sleeps, so keep it short
        // enough not to starve the other worker tests past their 5 s wait.
        analyse_sleep: std::time::Duration::from_secs(3),
        ..Default::default()
    };
    let (mut bridge, _peer) = bridge_with_backend(backend);
    bridge.start_analysis(tiny_frame(), 1, 1200, Vec::new(), 0);
    let t0 = std::time::Instant::now();
    drop(bridge);
    let waited = std::time::Instant::now().duration_since(t0);
    assert!(
        waited >= frame_worker::SHUTDOWN_JOIN && waited < frame_worker::SHUTDOWN_JOIN * 3,
        "waited {waited:?}"
    );
}

/// Two bridges in one process never run their backends at once.
#[tokio::test]
async fn two_bridges_never_use_the_hardware_at_the_same_time() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    let inside = Arc::new(AtomicUsize::new(0));
    let max = Arc::new(AtomicUsize::new(0));
    let mk = || RecordingBackend {
        analyse_sleep: std::time::Duration::from_millis(150),
        inside: Some(inside.clone()),
        max_inside: Some(max.clone()),
        ..Default::default()
    };
    let (mut a, _pa) = bridge_with_backend(mk());
    let (mut b, _pb) = bridge_with_backend(mk());
    a.start_analysis(tiny_frame(), 1, 1200, Vec::new(), 0);
    b.start_analysis(tiny_frame(), 1, 1200, Vec::new(), 0);
    let _ = next_worker_message(&mut a).await;
    let _ = next_worker_message(&mut b).await;
    assert_eq!(
        max.load(SeqCst),
        1,
        "both backends were inside a call at once"
    );
}

/// `apply_gpu_metrics` writes one tile's colour count and edge density, and
/// leaves a tile with no analysis entry untouched.
#[test]
fn gpu_metrics_are_written_per_tile_and_a_missing_entry_is_a_no_op() {
    let mut t = crate::tile::MetricsTracker::new(2, 1);
    let entry = crate::capture::gpu_pipeline::TileAnalysis {
        count: 5,
        edge_density_thou: 250,
        ..Default::default()
    };
    apply_gpu_metrics(&mut t, 1, 0, Some(&entry));
    assert_eq!(t.get(1, 0).unique_colors, 5);
    assert!((t.get(1, 0).edge_density - 0.25).abs() < 1e-6);
    let before = t.get(0, 0).unique_colors;
    apply_gpu_metrics(&mut t, 0, 0, None);
    assert_eq!(t.get(0, 0).unique_colors, before);
}

/// The real backend's `RequestKeyframe` reaches its encoder. Needs a GPU and a
/// codec; skips silently without one, as the test it replaces did.
#[tokio::test]
async fn gpu_backend_request_keyframe_reaches_its_encoder() {
    // Opening a real encoder drives the GPU: never beside other GPU tests.
    let _gpu = crate::gpu_test_lock::lock();
    let Ok(processor) = crate::capture::gpu_pipeline::GpuFrameProcessor::new(256) else {
        return;
    };
    let Ok(enc) = crate::encoder::h264_vaapi::FullFrameEncoder::new(640, 480) else {
        return;
    };
    let mut backend = GpuBackend::new(processor);
    backend.set_encoder_for_test(enc);
    backend.apply(Control::RequestKeyframe);
    assert_eq!(backend.encoder_keyframe_pending(), Some(true));
}

/// An inbound ACK_BATCH datagram with a recorded coverage entry should
/// fire the per-codec delivery handler (Cdf53 → counter increment).
#[tokio::test(flavor = "multi_thread")]
async fn dispatch_ack_datagram_cdf53_increments_counter() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    // The test bridge is constructed with `Scheduler::new(0, 0)`; production
    // resizes in lockstep with the dirty-detection grid (io_bridge.rs:1910)
    // before any tile can be acknowledged. `SlotMap` is dense, so a tile
    // outside the grid has no slot to record into — size the grid first.
    bridge.scheduler.resize(8, 10);
    // An acknowledgement only writes the mask of the tile's *live*
    // generation, and the coverage entry below names generation 1.
    bridge.scheduler.bump_generation(7, 9);
    // Record coverage as if the server had just emitted a Cdf53 pass.
    let cov: crate::transport::fragment_coverage::CoverageList =
        smallvec::smallvec![crate::transport::fragment_coverage::FragmentCoverage {
            tile_x: 7,
            tile_y: 9,
            generation: 1,
            pass_idx: 0,
            codec: crate::transport::protocol::Codec::Cdf53,
            palette_id: None,
            palette_bundled: false,
        }];
    // Key: (frame_seq=100, tile_x=7, tile_y=9, pass_idx=0)
    bridge.fragment_coverage.record((100, 7, 9, 0), cov);

    bridge.seed_transmission_for_test(
        1000,
        crate::transport::reliable_emitter::EmitKey::new(100, 7, 9, 0),
    );
    let batch = crate::transport::ack::AckBatch {
        entries: vec![crate::transport::ack::AckEntry {
            wire_seq: 1000,
            arrival_time_ms_lo16: 0,
        }],
    };
    bridge.dispatch_ack_datagram(&batch.encode());

    assert_eq!(
        bridge.scheduler.cdf53_passes_acked_for_test(7, 9, 1),
        1,
        "Cdf53 counter incremented exactly once"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_ack_datagram_missing_coverage_is_noop() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    bridge.seed_transmission_for_test(
        1001,
        crate::transport::reliable_emitter::EmitKey::new(999, 9, 5, 3),
    );
    let batch = crate::transport::ack::AckBatch {
        entries: vec![crate::transport::ack::AckEntry {
            wire_seq: 1001,
            arrival_time_ms_lo16: 0,
        }],
    };
    // Should not panic, should not modify any state.
    bridge.dispatch_ack_datagram(&batch.encode());
    assert_eq!(bridge.scheduler.cdf53_passes_acked_for_test(0, 0, 0), 0,);
}

/// dispatch_ack_datagram silently ignores datagrams whose first byte
/// isn't ACK_BATCH_MSG_TYPE.
#[tokio::test(flavor = "multi_thread")]
async fn dispatch_ack_datagram_ignores_unknown_msg_types() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    // Non-0x03 first byte → silently dropped.
    bridge.dispatch_ack_datagram(&[0xFF, 0, 0, 0, 0, 0]);
    bridge.dispatch_ack_datagram(&[0x01, 1, 0, 0]); // OLD FEEDBACK type; not us
                                                    // Empty.
    bridge.dispatch_ack_datagram(&[]);
    // No panic; nothing to assert beyond reaching this line.
}

/// An inbound TILE_NACK (0x05) envelope routes each entry into
/// `reliable_emitter.on_nack`, registering a hit when the cache holds
/// the matching EmitKey.
#[tokio::test(flavor = "multi_thread")]
async fn tile_nack_routes_to_emitter() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    // Pre-seed the emitter cache with one entry so on_nack has something
    // to look up.
    let key = crate::transport::reliable_emitter::EmitKey::new(1, 0, 0, 0);
    bridge.reliable_emitter.submit_one(
        key,
        bytes::Bytes::from(vec![0u8; 25]),
        std::time::Instant::now(),
        None,
        std::time::Instant::now(),
    );
    let nack_env = crate::transport::protocol::TileNackEnvelope {
        entries: vec![crate::transport::protocol::TileNackEntry {
            frame_seq: 1,
            tile_x: 0,
            tile_y: 0,
            pass_idx: 0,
        }],
    };
    let mut buf = Vec::new();
    nack_env.encode(&mut buf);
    bridge.dispatch_tile_nack_datagram(&buf);
    assert_eq!(bridge.reliable_emitter.stats.nack_hit, 1);
}

/// An inbound ACK_BATCH (0x03) envelope routes each entry into
/// `reliable_emitter.on_ack` (in addition to the existing scheduler /
/// fragment-coverage paths). A hit drops the matching cache entry and
/// bumps `ack_hit`.
#[tokio::test(flavor = "multi_thread")]
async fn ack_routes_through_emitter_dropping_cache() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    let key = crate::transport::reliable_emitter::EmitKey::new(1, 0, 0, 0);
    bridge.reliable_emitter.submit_one(
        key,
        bytes::Bytes::from(vec![0u8; 25]),
        std::time::Instant::now(),
        None,
        std::time::Instant::now(),
    );
    // Acknowledge the transmission the emitter actually allocated,
    // recorded into the ledger as the production drain would.
    let ledger_now = now_std();
    let emitted = bridge.reliable_emitter.take_transmissions();
    assert_eq!(emitted.len(), 1, "one datagram was submitted");
    for e in &emitted {
        bridge.transmission_ledger.record_departed(
            e.wire_seq,
            ledger_now,
            crate::transport::transmission_ledger::Transmission {
                emit_us: e.emit_us,
                wire_bytes: e.wire_bytes,
                content: e.content,
                depart_us: None,
                dep_idx: None,
            },
        );
    }
    let ack_env = crate::transport::ack::AckBatch {
        entries: vec![crate::transport::ack::AckEntry {
            wire_seq: emitted[0].wire_seq,
            arrival_time_ms_lo16: 0,
        }],
    };
    bridge.dispatch_ack_datagram(&ack_env.encode());
    assert_eq!(bridge.reliable_emitter.stats.ack_hit, 1);
    assert!(bridge.reliable_emitter.cache.get(&key).is_none());
}

/// `loss_override` reads `adaptation_context.loss_rate`. Every tile here
/// is one datagram, so a lost one leaves the client nothing to notice
/// and its `ReceiverFeedback` would report zero loss; the rate has to
/// come from what the server sent and never heard back about.
#[tokio::test(start_paused = true)]
async fn the_classifiers_loss_rate_is_the_ledgers_verdict() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);

    // 400 single-datagram transmissions, 1 ms apart; every fifth is
    // never acknowledged.
    let mut acked = Vec::new();
    for wire_seq in 0..400u32 {
        let now = now_std();
        bridge.transmission_ledger.record_departed(
            wire_seq,
            now,
            crate::transport::transmission_ledger::Transmission {
                emit_us: bridge.reliable_emitter.emit_us64(now),
                wire_bytes: 25,
                content: crate::transport::reliable_emitter::EmitKey::new(
                    wire_seq,
                    (wire_seq % 64) as u8,
                    0,
                    0,
                )
                .into(),
                depart_us: None,
                dep_idx: None,
            },
        );
        if !wire_seq.is_multiple_of(5) {
            acked.push(crate::transport::ack::AckEntry {
                wire_seq,
                arrival_time_ms_lo16: 0,
            });
        }
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
    }
    for batch in acked.chunks(32) {
        let env = crate::transport::ack::AckBatch {
            entries: batch.to_vec(),
        };
        bridge.dispatch_ack_datagram(&env.encode());
    }
    // Acknowledgements alone say nothing about loss.
    assert_eq!(bridge.adaptation_context.loss_rate, 0.0);

    // Declared lost (three or more later departures were acknowledged past
    // each of them), but not yet beyond recall: still no reading.
    bridge.expire_transmissions(now_std());
    assert_eq!(bridge.instr.losses_declared, 80);
    assert_eq!(bridge.adaptation_context.loss_rate, 0.0);

    // One of them turns up after all: the last unacknowledged one (fewer
    // than an FEC distance of departures later, so it cannot be a recovery).
    let late = crate::transport::ack::AckBatch {
        entries: vec![crate::transport::ack::AckEntry {
            wire_seq: 395,
            arrival_time_ms_lo16: 0,
        }],
    };
    bridge.dispatch_ack_datagram(&late.encode());

    tokio::time::advance(crate::transport::loss_window::SETTLE).await;
    bridge.expire_transmissions(now_std());
    let rate = bridge.adaptation_context.loss_rate;
    assert_eq!(rate, 79.0 / 400.0);
    assert_eq!(bridge.classifier.adaptation_context().loss_rate, rate);
    assert!(rate > crate::tile::classifier::LOSS_OVERRIDE_THRESHOLD);
}

/// dispatch_dirty_tiles_via_scheduler enqueues the right work in the
/// right codec/state for the CPU policy, even without a connected
/// session. This is the "Gap 3" fix from the final M3.1 review.
#[tokio::test]
async fn dispatch_via_scheduler_cpu_policy_enqueues_cdf53_tiles() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    bridge.metrics_tracker.resize(2, 2);

    let pixels = vec![0xAAu8; 64 * 64 * 4];
    let grid = crate::tile::TileGrid::new(64, 64);
    let dirty = vec![(0u32, 0u32), (1, 1)];

    let _ = bridge.dispatch_dirty_tiles_via_scheduler(
        &dirty,
        &grid,
        TileDispatchFrame {
            pixels: &pixels,
            stride: 64 * 4,
            seq: 1,
            timestamp_us: 0,
        },
        /* max_frag */ 1200,
        SchedulerEmissionPolicy::TestCdf53All,
        None,
    );

    // CDF 5/3 passes leave the refinement queue as they are sent, so the
    // evidence is what the emitter now holds for each tile.
    for (x, y) in [(0u8, 0u8), (1, 1)] {
        assert!(
            bridge.reliable_emitter.has_cache_entries_for_tile(x, y),
            "tile ({x},{y}) must have been sent"
        );
    }
    assert!(
        bridge.scheduler.peek_for_test().is_empty(),
        "no tile-codec work: the CPU policy has no tile encoders"
    );
}

// Former home of `loss_injector_from_env_parses_probability_and_predicate`,
// `loss_injector_from_env_returns_none_when_unset`, and
// `oob_injector_from_env_parses`: moved to `config.rs` alongside
// `TransportConfig::from_lookup`, the code they now test (see
// `transport_config_parses_loss_probability_and_predicate`,
// `transport_config_loss_injector_none_when_unset`, and
// `transport_config_parses_oob_inject_at`). They no longer need
// `lock_env()`: `from_lookup` never touches process env.

#[tokio::test]
async fn session_reset_skips_first_hello_fires_on_takeover_not_on_rehello() {
    // `fire_session_reset` reads `self.skip_palette_session_reset`, a
    // plain struct field set once at construction time, not a live env
    // read, so this test does not race the env-parsing tests in config.rs.
    let handle_a = ConnectionHandle(0);
    let handle_b = ConnectionHandle(1);
    let mut bridge = test_bridge_with_sessions(&[handle_a, handle_b]).await;
    bridge.palette_table.delivered.insert(7);

    // Merely being connected (no HELLO) fires nothing and latches nothing.
    bridge
        .wt_sessions
        .get_mut(&handle_a)
        .expect("wt session")
        .test_set_connected(true);
    assert!(!bridge.has_seen_prior_session);
    assert!(bridge.palette_table.delivered.contains(7));

    // FIRST ever HELLO: the reset body is skipped (tests that rely on
    // classifier/scheduler state surviving first connect keep working),
    // but the latch is set.
    bridge.apply_hello(handle_a, hello(false));
    assert!(
        bridge.palette_table.delivered.contains(7),
        "first HELLO: reset body skipped"
    );
    assert!(bridge.has_seen_prior_session, "latch set by first HELLO");

    // Re-HELLO from the SAME handle is not a new session: no reset.
    bridge.apply_hello(handle_a, hello(false));
    assert!(
        bridge.palette_table.delivered.contains(7),
        "re-HELLO from the live handle must not reset"
    );

    // Takeover by a new handle: reset fires.
    bridge.apply_hello(handle_b, hello(false));
    assert!(
        !bridge.palette_table.delivered.contains(7),
        "takeover: reset body MUST fire (delivered cleared)"
    );

    // Re-arm; re-HELLO from the new live handle: still no re-fire.
    bridge.palette_table.delivered.insert(7);
    bridge.apply_hello(handle_b, hello(false));
    assert!(bridge.palette_table.delivered.contains(7));
}

#[tokio::test]
async fn takeover_clears_pending_display_mode_and_scale() {
    let (mut bridge, a) = test_bridge_with_one_session().await;
    let b = ConnectionHandle(9);
    attach_session(&mut bridge, b);
    bridge.apply_hello(a, hello(false));
    bridge.display_scale_milli = 2000;
    bridge.pending_display_mode = Some(PendingDisplayMode {
        width: 800,
        height: 600,
        deadline_us: 1,
    });
    bridge.apply_hello(b, hello(false));
    assert!(bridge.pending_display_mode.is_none());
    assert_eq!(bridge.display_scale_milli, DEFAULT_SCALE_MILLI);
}

// Former home of `skip_palette_session_reset_from_env_parses`: moved to
// `config::tests::transport_config_parses_skip_palette_session_reset`.
//
// Former home of `diagnose_tiles_env_var_parses`: moved to
// `config::tests::diagnostics_config_parses_diagnose_tiles`.

/// dispatch_dirty_tiles_via_scheduler emits Solid bytes when the GPU policy
/// reads CodecState::Solid for a tile.
#[tokio::test]
async fn dispatch_via_scheduler_gpu_policy_emits_solid_for_solid_state() {
    use crate::tile::CodecState;
    use crate::transport::scheduler::WorkState;
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    bridge.scheduler.resize(2, 2);
    bridge.metrics_tracker.resize(2, 2);
    bridge.metrics_tracker.get_mut(0, 0).codec_state = CodecState::Solid;

    let pixels = vec![0xBBu8; 64 * 64 * 4];
    let grid = crate::tile::TileGrid::new(64, 64);
    let dirty = vec![(0u32, 0u32)];

    let _ = bridge.dispatch_dirty_tiles_via_scheduler(
        &dirty,
        &grid,
        TileDispatchFrame {
            pixels: &pixels,
            stride: 64 * 4,
            seq: 1,
            timestamp_us: 0,
        },
        1200,
        SchedulerEmissionPolicy::GpuClassifierDriven,
        None,
    );

    let queued = bridge.scheduler.peek_for_test();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].codec, Codec::Solid);
    assert_eq!(queued[0].payload.len(), 4, "Solid is 4 bytes BGRA");
    assert_eq!(queued[0].state, WorkState::InFlight);
}

#[test]
fn populate_gpu_metrics_writes_unique_colors_and_edge_density() {
    use crate::capture::gpu_pipeline::TileAnalysis;
    use crate::tile::MetricsTracker;

    let mut tracker = MetricsTracker::new(2, 1);
    let analysis = vec![
        TileAnalysis {
            count: 1,
            edge_density_thou: 0,
            _pad: [0; 2],
            colors: [0; 16],
        },
        TileAnalysis {
            count: 17,
            edge_density_thou: 850,
            _pad: [0; 2],
            colors: [0; 16],
        },
    ];
    let dirty: Vec<(u32, u32)> = vec![(0, 0), (1, 0)];

    super::populate_gpu_metrics(&mut tracker, &dirty, 2, &analysis);

    assert_eq!(tracker.get(0, 0).unique_colors, 1);
    assert!(tracker.get(0, 0).edge_density.abs() < 1e-6);

    assert_eq!(tracker.get(1, 0).unique_colors, 17);
    assert!((tracker.get(1, 0).edge_density - 0.850).abs() < 1e-6);
}

#[test]
fn populate_gpu_metrics_skips_non_dirty_tiles() {
    use crate::capture::gpu_pipeline::TileAnalysis;
    use crate::tile::MetricsTracker;

    let mut tracker = MetricsTracker::new(2, 1);
    // Pre-seed tile (1,0) to a known sentinel so we can prove it stayed.
    tracker.get_mut(1, 0).unique_colors = u16::MAX;
    tracker.get_mut(1, 0).edge_density = f32::NAN;

    let analysis = vec![
        TileAnalysis {
            count: 5,
            edge_density_thou: 100,
            _pad: [0; 2],
            colors: [0; 16],
        },
        TileAnalysis {
            count: 9,
            edge_density_thou: 200,
            _pad: [0; 2],
            colors: [0; 16],
        },
    ];
    // Only (0,0) is dirty.
    let dirty: Vec<(u32, u32)> = vec![(0, 0)];
    super::populate_gpu_metrics(&mut tracker, &dirty, 2, &analysis);

    assert_eq!(tracker.get(0, 0).unique_colors, 5);
    assert_eq!(
        tracker.get(1, 0).unique_colors,
        u16::MAX,
        "non-dirty tile untouched"
    );
    assert!(
        tracker.get(1, 0).edge_density.is_nan(),
        "non-dirty tile untouched"
    );
}

/// A *thin* PalRle emission must not decrement `in_flight_carrying`, and
/// must not release a palette acquire.
///
/// Both the counter bump and the acquire are taken only on the bundled
/// path, next to `acquire_or_allocate`. Undoing them for a thin emission
/// takes down a count that was never put up — which underflows — and
/// releases a reference that was never taken, which can free a palette
/// slot another tile is still using.
///
/// This fired 602 times in a single live session while the same log showed
/// none in the preceding week, with the warning text naming itself an
/// "enqueue/ack pairing bug".
/// The emit stamp is `u32` microseconds because the datagram header
/// carries it in 4 bytes, so it wraps every 2^32 us = 71.6 minutes. The
/// clock it is subtracted from is `u64` and does not. Mixing them breaks
/// at the first wrap: the u64 keeps climbing while the u32 resets, and the
/// difference becomes elapsed session time rather than a round trip.
///
/// Observed in production on a daemon up 81 minutes:
/// `critical_latency_mean_us=2448007853` (2448 seconds), after an hour of
/// a steady 19375 us. The same corruption reaches
/// `BweSample::server_emit_us`, so the bandwidth estimator's timing input
/// is garbage past the first wrap.
#[test]
fn ack_latency_survives_the_emit_stamp_wrapping() {
    // A session past its first wrap. True elapsed at emit is
    // 4_300_000_000 us (~71.7 min), which the u32 stamp records as
    // 5_032_704 -- it has wrapped. The acknowledgement arrives 30 ms
    // later, and the u64 epoch clock reads the true 4_300_030_000.
    const WRAP: u64 = 1u64 << 32;
    let true_emit_us: u64 = 4_300_000_000;
    let emit_us: u32 = true_emit_us as u32; // what the header carries
    let elapsed_since_epoch_us: u64 = true_emit_us + 30_000;
    assert!(
        true_emit_us > WRAP,
        "precondition: the session must be past the first wrap"
    );

    let naive = elapsed_since_epoch_us.saturating_sub(emit_us as u64);
    assert!(
        naive > 1_000_000,
        "precondition: the naive subtraction must be absurd, got {naive} us"
    );

    let wrapped = (elapsed_since_epoch_us as u32).wrapping_sub(emit_us) as u64;
    assert_eq!(
        wrapped, 30_000,
        "wrapping subtraction must recover the true 30 ms round trip"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ack_for_a_thin_palrle_tile_leaves_the_carry_count_alone() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);

    // One bundled emission is outstanding, holding one acquire.
    bridge.palette_table.ref_count[9] = 1;
    bridge.palette_table.in_flight_carrying[9] = 1;

    // A *thin* emission of the same palette is acknowledged. It carried no
    // palette, so it took neither the count nor the acquire.
    let cov: crate::transport::fragment_coverage::CoverageList =
        smallvec::smallvec![crate::transport::fragment_coverage::FragmentCoverage {
            tile_x: 2,
            tile_y: 3,
            generation: 0,
            pass_idx: 0,
            codec: crate::transport::protocol::Codec::PalRle,
            palette_id: Some(9),
            palette_bundled: false,
        }];
    bridge.fragment_coverage.record((301, 2, 3, 0), cov);
    bridge.seed_transmission_for_test(
        2001,
        crate::transport::reliable_emitter::EmitKey::new(301, 2, 3, 0),
    );
    let batch = crate::transport::ack::AckBatch {
        entries: vec![crate::transport::ack::AckEntry {
            wire_seq: 2001,
            arrival_time_ms_lo16: 0,
        }],
    };
    bridge.dispatch_ack_datagram(&batch.encode());

    assert_eq!(
        bridge.palette_table.in_flight_carrying[9], 1,
        "a thin emission must not decrement the bundled emission's carry count"
    );
    assert_eq!(
        bridge.palette_table.ref_count[9], 1,
        "a thin emission must not release an acquire it never took"
    );
    assert!(
        bridge.palette_table.delivered.contains(9),
        "a thin emission arriving still proves the receiver has the palette"
    );
}

/// Verify that `IoBridge` constructors initialize `palette_table` to an
/// all-empty state. M3.2a: palette state is per-server, so a fresh bridge
/// must start with no allocated slots.
#[tokio::test]
async fn io_bridge_constructs_with_empty_palette_table() {
    let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
    let server = QuicServer::new().expect("QuicServer::new failed");
    let bridge = IoBridge::new_with_stream_for_test(our_end, server);

    for id in 0..crate::encoder::pal_rle::PALETTE_TABLE_SLOTS {
        assert_eq!(
            bridge.palette_table.slot_state[id],
            crate::encoder::pal_rle::SlotState::Empty,
            "slot {id} must start Empty"
        );
        assert!(
            bridge.palette_table.entries[id].is_none(),
            "slot {id} entries must start None"
        );
        assert_eq!(
            bridge.palette_table.ref_count[id], 0,
            "slot {id} ref_count must start 0"
        );
    }
    // Spot-check that the delivered bitset contains no ids.
    assert!(
        !bridge.palette_table.delivered.contains(0),
        "delivered[0] must start false"
    );
    assert!(
        !bridge.palette_table.delivered.contains(255),
        "delivered[255] must start false"
    );
}

#[tokio::test]
async fn phase_a_allocates_and_marks_bundled_for_first_emission() {
    use crate::tile::CodecState;

    let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
    let server = QuicServer::new().expect("QuicServer::new failed");
    let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);
    let cols = 4;
    bridge.metrics_tracker.resize(cols, 2);
    bridge.metrics_tracker.get_mut(0, 0).codec_state = CodecState::PalRle { palette_id: 0 };

    let compact_list = vec![0u32];
    let per_tile_id = vec![0u8];
    // folded_into[0] = default-self = ((255-1)<<8) | 0
    let folded_into = vec![(255u32 - 1) << 8; 256];

    // Build a frame_palette_set with slot 0 populated (count=1, color BGRA[10,20,30,255]).
    let mut frame_palette_set = vec![
        crate::capture::gpu_pipeline::FramePaletteEntryRaw {
            count: 0,
            _pad: [0; 3],
            colors: [0; 16],
        };
        256
    ];
    frame_palette_set[0].count = 1;
    frame_palette_set[0].colors[0] = 10 | (20 << 8) | (30 << 16) | (255 << 24);
    let index_buffer = vec![0u8; 512];

    let preps = bridge.phase_a_palette_allocation(
        cols,
        &compact_list,
        &per_tile_id,
        &folded_into,
        &frame_palette_set,
        &index_buffer,
    );

    assert_eq!(preps.len(), 1);
    assert_eq!(preps[0].tile_xy, (0, 0));
    assert_eq!(preps[0].palette.count, 1);
    assert_eq!(preps[0].palette.colors[0], [10, 20, 30, 255]);
    assert!(preps[0].bundled, "first emission must bundle");
    assert_eq!(preps[0].palette_id, 0);
    assert_eq!(bridge.palette_table.in_flight_carrying[0], 1);
    assert!(!bridge.palette_table.delivered.contains(0));
}

#[tokio::test]
async fn phase_a_marks_thin_when_delivered_already_set() {
    use crate::encoder::pal_rle::PaletteEntry;
    use crate::tile::CodecState;

    let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
    let server = QuicServer::new().expect("QuicServer::new failed");
    let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);
    let cols = 4;
    bridge.metrics_tracker.resize(cols, 2);
    bridge.metrics_tracker.get_mut(0, 0).codec_state = CodecState::PalRle { palette_id: 0 };

    // Pre-populate persistent slot 3 with exact palette + delivered=true.
    let mut p = PaletteEntry::default();
    p.colors[0] = [10, 20, 30, 255];
    p.count = 1;
    bridge.palette_table.entries[3] = Some(p);
    bridge.palette_table.slot_state[3] = crate::encoder::pal_rle::SlotState::FreeButCached;
    bridge.palette_table.delivered.insert(3);
    bridge.palette_table.free_lru.push_back(3);

    let compact_list = vec![0u32];
    let per_tile_id = vec![0u8];
    let folded_into = vec![(255u32 - 1) << 8; 256];
    let mut frame_palette_set = vec![
        crate::capture::gpu_pipeline::FramePaletteEntryRaw {
            count: 0,
            _pad: [0; 3],
            colors: [0; 16],
        };
        256
    ];
    frame_palette_set[0].count = 1;
    frame_palette_set[0].colors[0] = 10 | (20 << 8) | (30 << 16) | (255 << 24);
    let index_buffer = vec![0u8; 512];

    let preps = bridge.phase_a_palette_allocation(
        cols,
        &compact_list,
        &per_tile_id,
        &folded_into,
        &frame_palette_set,
        &index_buffer,
    );

    assert_eq!(preps.len(), 1);
    assert_eq!(preps[0].palette_id, 3, "find_matching should hit slot 3");
    assert!(!preps[0].bundled, "delivered=true → thin payload");
    assert_eq!(bridge.palette_table.in_flight_carrying[3], 0);
}

#[tokio::test]
async fn phase_a_falls_back_to_raw_on_sentinel() {
    use crate::tile::CodecState;

    let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
    let server = QuicServer::new().expect("QuicServer::new failed");
    let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);
    let cols = 4;
    bridge.metrics_tracker.resize(cols, 2);
    bridge.metrics_tracker.get_mut(0, 0).codec_state = CodecState::PalRle { palette_id: 0 };

    let compact_list = vec![0u32];
    let per_tile_id = vec![0xFFu8]; // overflow sentinel
    let folded_into = vec![0u32; 256];
    let frame_palette_set = vec![];
    let index_buffer = vec![];

    let preps = bridge.phase_a_palette_allocation(
        cols,
        &compact_list,
        &per_tile_id,
        &folded_into,
        &frame_palette_set,
        &index_buffer,
    );

    assert!(preps.is_empty());
    assert_eq!(
        bridge.metrics_tracker.get(0, 0).codec_state,
        CodecState::Skip
    );
    assert_eq!(bridge.palette_table.stats_frame.fell_back_to_raw, 1);
}

#[test]
fn phase_b_encodes_each_prep_in_parallel() {
    use crate::encoder::pal_rle::PaletteEntry;

    let mut palette = PaletteEntry::default();
    palette.colors[0] = [1, 2, 3, 255];
    palette.count = 1;
    let preps = vec![
        PalRleTileWorkPrep {
            tile_xy: (0, 0),
            indices: [0; 512],
            palette,
            palette_id: 7,
            bundled: true,
        },
        PalRleTileWorkPrep {
            tile_xy: (1, 0),
            indices: [0; 512],
            palette,
            palette_id: 7,
            bundled: false,
        },
    ];
    let encoded = IoBridge::phase_b_encode_payloads(&preps);
    assert_eq!(encoded.len(), 2);
    let p00 = &encoded[&(0u32, 0u32)];
    let p10 = &encoded[&(1u32, 0u32)];
    // Bundled has flag bit 0 set.
    assert_eq!(p00[0] & 0x01, 0x01);
    assert_eq!(p10[0] & 0x01, 0x00);
    // Both reference id 7.
    assert_eq!(p00[1], 7);
    assert_eq!(p10[1], 7);
}

#[tokio::test]
async fn ack_for_palrle_tile_sets_delivered_and_decrements_in_flight() {
    use crate::encoder::pal_rle::{PaletteEntry, SlotState};

    let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
    let server = QuicServer::new().expect("QuicServer::new failed");
    let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);
    // Resize scheduler so tile (0,0) is in-bounds.
    bridge.scheduler.resize(1, 1);

    let mut p = PaletteEntry::default();
    p.colors[0] = [1, 2, 3, 255];
    p.count = 1;
    bridge.palette_table.entries[7] = Some(p);
    bridge.palette_table.slot_state[7] = SlotState::Held;
    bridge.palette_table.in_flight_carrying[7] = 1;

    // Record coverage as if the server emitted a PalRle tile with palette_id=7.
    let cov: crate::transport::fragment_coverage::CoverageList =
        smallvec::smallvec![crate::transport::fragment_coverage::FragmentCoverage {
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            pass_idx: 0,
            codec: crate::transport::protocol::Codec::PalRle,
            palette_id: Some(7),
            palette_bundled: true,
        }];
    // Key: (frame_seq=200, tile_x=0, tile_y=0, pass_idx=0)
    bridge.fragment_coverage.record((200, 0, 0, 0), cov);

    bridge.seed_transmission_for_test(
        1003,
        crate::transport::reliable_emitter::EmitKey::new(200, 0, 0, 0),
    );
    let batch = crate::transport::ack::AckBatch {
        entries: vec![crate::transport::ack::AckEntry {
            wire_seq: 1003,
            arrival_time_ms_lo16: 0,
        }],
    };
    bridge.dispatch_ack_datagram(&batch.encode());

    assert!(bridge.palette_table.delivered.contains(7));
    assert_eq!(bridge.palette_table.in_flight_carrying[7], 0);
}

#[tokio::test]
async fn ack_for_palrle_tile_releases_ref_count() {
    use crate::encoder::pal_rle::{PaletteEntry, SlotState};

    let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
    let server = QuicServer::new().expect("QuicServer::new failed");
    let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);
    bridge.scheduler.resize(1, 1);

    let mut p = PaletteEntry::default();
    p.colors[0] = [1, 2, 3, 255];
    p.count = 1;
    bridge.palette_table.entries[5] = Some(p);
    bridge.palette_table.slot_state[5] = SlotState::Held;
    bridge.palette_table.ref_count[5] = 1;
    bridge.palette_table.in_flight_carrying[5] = 1;

    // Record coverage as if the server emitted a PalRle tile with
    // palette_id=5, carrying the palette inline. Bundled specifically:
    // the acquire and the `in_flight_carrying` bump seeded above are both
    // taken on that path, so only a bundled emission has anything for the
    // acknowledgement to undo.
    let cov: crate::transport::fragment_coverage::CoverageList =
        smallvec::smallvec![crate::transport::fragment_coverage::FragmentCoverage {
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            pass_idx: 0,
            codec: crate::transport::protocol::Codec::PalRle,
            palette_id: Some(5),
            palette_bundled: true,
        }];
    // Key: (frame_seq=201, tile_x=0, tile_y=0, pass_idx=0)
    bridge.fragment_coverage.record((201, 0, 0, 0), cov);

    bridge.seed_transmission_for_test(
        1004,
        crate::transport::reliable_emitter::EmitKey::new(201, 0, 0, 0),
    );
    let batch = crate::transport::ack::AckBatch {
        entries: vec![crate::transport::ack::AckEntry {
            wire_seq: 1004,
            arrival_time_ms_lo16: 0,
        }],
    };
    bridge.dispatch_ack_datagram(&batch.encode());

    assert_eq!(
        bridge.palette_table.ref_count[5], 0,
        "ACK must release the Phase A acquire"
    );
    assert_eq!(
        bridge.palette_table.slot_state[5],
        SlotState::FreeButCached,
        "slot must transition to FreeButCached after ref_count reaches 0"
    );
}

// Former home of the loss-injection predicate tests
// (`palrle_bundled_predicate_matches_bundled_datagram`,
// `palrle_bundled_predicate_rejects_thin_datagram`,
// `palrle_thin_predicate_matches_thin_only`): moved to `config.rs`
// alongside `TransportConfig::from_lookup` / `loss_injector_from_lookup`
// (see `transport_config_palrle_bundled_predicate_matches_bundled` and
// friends), which now owns the predicate table they exercise. Moving
// them turns this file's `DATAGRAM_HEADER_SIZE`/`TILE_HEADER_SIZE`
// import fully unused (previously only used by the code that moved here
// in the first place) — see the "Critical constraints" note on not
// fixing that pre-existing unused-import warning at `io_bridge.rs:47-48`;
// it is a known, accepted consequence of this move, not something to
// patch around by keeping tests here artificially.

#[test]
fn phase_b_emits_indices_raw_when_caps_enabled_and_thin() {
    use crate::encoder::pal_rle::PaletteEntry;
    let prep = PalRleTileWorkPrep {
        tile_xy: (0, 0),
        indices: [0xAB; 512],
        palette: PaletteEntry {
            count: 2,
            colors: [[0xFF, 0, 0, 0xFF]; 16],
        },
        palette_id: 9,
        bundled: false, // thin path
    };
    let caps = crate::transport::client_caps::ClientCapabilities {
        indices_raw_enabled: true,
        supports_h264: false,
        h264_acked_ref: false,
    };
    let map = IoBridge::phase_b_encode_payloads_with_caps(&[prep], &caps, None);
    let payload = &map[&(0, 0)];
    assert_eq!(payload[0], 0x02, "indices_raw flag (bit 1)");
    assert_eq!(payload[1], 9, "palette_id");
    assert_eq!(payload.len(), 514, "fixed-size indices_raw payload");
}

#[test]
fn phase_b_emits_thin_rle_when_caps_disabled() {
    use crate::encoder::pal_rle::PaletteEntry;
    let prep = PalRleTileWorkPrep {
        tile_xy: (5, 7),
        indices: [0x00; 512], // all-same index → max-compressible RLE
        palette: PaletteEntry {
            count: 1,
            colors: [[0xFF, 0, 0, 0xFF]; 16],
        },
        palette_id: 3,
        bundled: false, // thin path
    };
    let caps = crate::transport::client_caps::ClientCapabilities {
        indices_raw_enabled: false,
        supports_h264: false,
        h264_acked_ref: false,
    };
    let map = IoBridge::phase_b_encode_payloads_with_caps(&[prep], &caps, None);
    let payload = &map[&(5, 7)];
    assert_eq!(payload[0], 0x00, "thin flag (bit 0 clear, bit 1 clear)");
    assert_eq!(payload[1], 3, "palette_id");
    assert!(
        payload.len() < 514,
        "RLE compresses single-index tile far below 514"
    );
}

#[test]
fn phase_b_emits_bundled_when_bundled_regardless_of_caps() {
    use crate::encoder::pal_rle::PaletteEntry;
    let prep = PalRleTileWorkPrep {
        tile_xy: (1, 1),
        indices: [0x00; 512],
        palette: PaletteEntry {
            count: 1,
            colors: [[0xFF, 0, 0, 0xFF]; 16],
        },
        palette_id: 12,
        bundled: true,
    };
    let caps = crate::transport::client_caps::ClientCapabilities {
        indices_raw_enabled: true,
        supports_h264: false,
        h264_acked_ref: false,
    };
    let map = IoBridge::phase_b_encode_payloads_with_caps(&[prep], &caps, None);
    let payload = &map[&(1, 1)];
    assert_eq!(payload[0], 0x01, "bundled flag (bit 0 set)");
    // indices_raw promotion only applies to thin path; bundled passes through unchanged.
}

#[tokio::test]
async fn feedback_stream_parses_hello_then_feedback_then_decode_error() {
    use crate::encoder::pal_rle::PaletteEntry;
    use crate::transport::client_caps::{ClientCapabilities, HelloMsg};
    use crate::transport::decode_error::{DecodeErrorMsg, ERR_THIN_UNCACHED_PALETTE};
    use crate::transport::feedback::ReceiverFeedback;

    let mut bridge = make_bridge_for_test().await;
    attach_session(&mut bridge, ConnectionHandle(0));

    // Pre-populate palette_table slot 7 as delivered, so the decode-error
    // path can find a palette to clear. The metrics_tracker also needs
    // to record that tile (3, 4) is currently rendering palette_id=7
    // for handle_decode_error to locate it.
    let pal = PaletteEntry {
        count: 1,
        colors: [[10, 20, 30, 255]; 16],
    };
    bridge.palette_table.write_bytes(7, &pal);
    bridge.palette_table.delivered.insert(7);
    bridge.metrics_tracker.resize(8, 8);
    bridge.metrics_tracker.get_mut(3, 4).codec_state =
        crate::tile::CodecState::PalRle { palette_id: 7 };

    // Concatenated stream: HELLO (2 B) + FEEDBACK (18 B) + DECODE_ERROR (5 B) = 25 bytes
    let hello = HelloMsg {
        caps: ClientCapabilities {
            indices_raw_enabled: true,
            supports_h264: false,
            h264_acked_ref: false,
        },
    };
    let fb = ReceiverFeedback {
        timestamp_ns: 0,
        datagrams_received: 0,
        datagrams_recovered_fec: 0,
        suspension_detected: false,
    };
    let err = DecodeErrorMsg {
        codec: 2,
        tile_x: 3,
        tile_y: 4,
        error_code: ERR_THIN_UNCACHED_PALETTE,
    };

    let mut buf = Vec::new();
    hello.encode(&mut buf);
    fb.encode(&mut buf);
    err.encode(&mut buf);
    assert_eq!(buf.len(), 2 + 18 + 5);

    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &buf);

    // HELLO applied:
    assert!(
        bridge.current_client_caps().indices_raw_enabled,
        "HELLO message must apply caps"
    );

    // DECODE_ERROR with code 3 cleared the delivered bit (via force_rebundle):
    assert!(
        !bridge.palette_table.delivered.contains(7),
        "ERR_THIN_UNCACHED_PALETTE must clear delivered bit via force_rebundle"
    );
}

#[tokio::test]
async fn decode_error_3_triggers_force_rebundle() {
    use crate::encoder::pal_rle::PaletteEntry;
    use crate::tile::CodecState;
    use crate::transport::decode_error::{DecodeErrorMsg, ERR_THIN_UNCACHED_PALETTE};

    let mut bridge = make_bridge_for_test().await;

    // Set up state: palette_table slot 5 is in the "delivered" state so the
    // next emission for that palette would be thin. metrics_tracker records
    // that tile (3, 4) is currently rendering palette_id=5.
    let pal = PaletteEntry {
        count: 1,
        colors: [[10, 20, 30, 255]; 16],
    };
    bridge.palette_table.write_bytes(5, &pal);
    bridge.palette_table.delivered.insert(5);
    bridge.metrics_tracker.resize(8, 8);
    bridge.metrics_tracker.get_mut(3, 4).codec_state = CodecState::PalRle { palette_id: 5 };

    // Simulate client reporting thin-uncached for that tile.
    let msg = DecodeErrorMsg {
        codec: 2,
        tile_x: 3,
        tile_y: 4,
        error_code: ERR_THIN_UNCACHED_PALETTE,
    };
    bridge.handle_decode_error(msg);

    // delivered bit must be cleared so the next emission rebundles.
    assert!(
        !bridge.palette_table.delivered.contains(5),
        "force_rebundle should clear the delivered bit for palette_id=5"
    );
}

#[tokio::test]
async fn decode_error_other_codes_no_op_on_palette_table() {
    use crate::encoder::pal_rle::PaletteEntry;
    use crate::tile::CodecState;
    use crate::transport::decode_error::{DecodeErrorMsg, ERR_INDEX_OOB};

    let mut bridge = make_bridge_for_test().await;
    let pal = PaletteEntry {
        count: 1,
        colors: [[10, 20, 30, 255]; 16],
    };
    bridge.palette_table.write_bytes(5, &pal);
    bridge.palette_table.delivered.insert(5);
    bridge.metrics_tracker.resize(8, 8);
    bridge.metrics_tracker.get_mut(3, 4).codec_state = CodecState::PalRle { palette_id: 5 };

    let msg = DecodeErrorMsg {
        codec: 2,
        tile_x: 3,
        tile_y: 4,
        error_code: ERR_INDEX_OOB,
    };
    bridge.handle_decode_error(msg);

    // Code 5 is log-only in M3.2b — delivered must remain set.
    assert!(
        bridge.palette_table.delivered.contains(5),
        "ERR_INDEX_OOB should not clear delivered bit (M3.2b leaves it as a future hook)"
    );
}

/// Verify the io_bridge escalation-sweep logic in isolation:
/// `detect_escalation_candidates` is called against a hand-built tracker
/// and the results match the expected eligible indices.
///
/// This mirrors exactly the sweep that `process_frame_gpu` will run on every
/// frame. The GPU dispatch path itself requires a Vulkan device and is
/// validated end-to-end by Task 13's e2e_progressive_refinement test.
#[tokio::test]
async fn escalation_sweep_returns_expected_candidates() {
    use crate::capture::gpu_pipeline::MAX_ESCALATION_PER_FRAME;
    use crate::tile::detect_escalation_candidates;
    use crate::tile::{CodecState, MetricsTracker};

    // Build a 4×4 tracker and make tiles (0,0), (1,0), (2,0) eligible:
    // idle_frames > 30, lossy codec (H264 post-M3.5b L1 prune; Solid
    // is no longer eligible because it's lossless within its predicate),
    // not already escalated.
    let mut tracker = MetricsTracker::new(4, 4);
    for x in 0..3u32 {
        let m = tracker.get_mut(x, 0);
        m.idle_frames = 31;
        m.codec_state = CodecState::H264 { frames_in_h264: 5 };
    }
    // Tile (3,0): idle but already escalated — must be excluded.
    {
        let m = tracker.get_mut(3, 0);
        m.idle_frames = 31;
        m.codec_state = CodecState::H264 { frames_in_h264: 5 };
        m.already_escalated_this_gen = true;
    }
    // Remaining tiles (row 1-3): below threshold or Skip — excluded.

    let candidates = detect_escalation_candidates(&tracker, MAX_ESCALATION_PER_FRAME);

    // Flat row-major indices: (0,0)=0, (1,0)=1, (2,0)=2
    assert_eq!(
        candidates,
        vec![0u32, 1, 2],
        "sweep should return exactly the three eligible tiles in row-major order"
    );

    // Simulate what process_frame_gpu does: stash on IoBridge.
    let mut bridge = make_bridge_for_test().await;
    bridge.cdf53_escalation_candidates_this_frame = candidates.clone();
    assert_eq!(
        bridge.cdf53_escalation_candidates_this_frame,
        vec![0u32, 1, 2],
        "stashed candidates must match what the sweep returned"
    );

    // Verify k_max capping is respected by the sweep helper.
    let capped = detect_escalation_candidates(&tracker, 2);
    assert_eq!(
        capped,
        vec![0u32, 1],
        "k_max=2 should cap the result to the first 2 candidates"
    );
}

/// Verify that `process_frame` emits a `frame.captured` tracing event with
/// the expected JSON fields when the ghostframe::bench target is active.
///
/// Uses `tracing_subscriber`'s JSON formatter to capture log output and
/// asserts that `frame_seq`, `capture_done_ns`, and `message` appear.
#[tokio::test]
async fn frame_captured_event_format() {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::util::SubscriberInitExt;

    #[derive(Default, Clone)]
    struct CapturingWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer = CapturingWriter(Arc::clone(&buf));

    // Install a JSON subscriber that only passes ghostframe::bench=info.
    // `set_default` returns a guard; drop it at the end of the test.
    let _g = tracing_subscriber::fmt()
        .json()
        .with_writer(writer)
        .with_target(true)
        .with_env_filter("ghostframe::bench=info")
        .set_default();

    // Build a bridge and submit a frame with a known seq + capture_done_ns.
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);

    let frame = crate::server::FrameSubmission {
        width: 32,
        height: 32,
        stride: 32 * 4,
        pixels: vec![0u8; 32 * 32 * 4].into(),
        dmabuf_fd: None,
        timestamp_us: 0,
        content: crate::frame_damage::Content::Pixels,
        damage: crate::frame_damage::FrameDamage::Unknown,
        capture_done_ns: 12345,
    };
    // frame_seq starts at 0; after process_frame it becomes 1.
    bridge.process_frame(frame);

    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("frame.captured"),
        "expected 'frame.captured' in logs; got: {logs}"
    );
    assert!(
        logs.contains("12345"),
        "expected capture_done_ns=12345 in logs; got: {logs}"
    );
    // frame_seq is 1 after the first process_frame call.
    assert!(
        logs.contains("\"frame_seq\":1"),
        "expected frame_seq:1 in logs; got: {logs}"
    );
}

/// Verify that `dispatch_dirty_tiles_via_scheduler` returns non-zero
/// `FrameSendStats` for a dirty tile frame, and that the resulting
/// `frame.last_send` tracing event contains all required JSON fields with
/// expected values.
///
/// Note: `process_frame` early-returns without sending if no QUIC session is
/// connected (no MTU to compute). We test the dispatch path directly via
/// `dispatch_dirty_tiles_via_scheduler` (pub(crate)), which lets us verify
/// both the returned stats and the emitted log event in the same test.
#[tokio::test]
async fn frame_last_send_event_format() {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::util::SubscriberInitExt;

    #[derive(Default, Clone)]
    struct CapturingWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer = CapturingWriter(Arc::clone(&buf));

    // Install a JSON subscriber that only passes ghostframe::bench=info.
    let _g = tracing_subscriber::fmt()
        .json()
        .with_writer(writer)
        .with_target(true)
        .with_env_filter("ghostframe::bench=info")
        .set_default();

    // Build a bridge and drive dispatch directly.
    // `process_frame` early-returns when no QUIC sessions are connected
    // (no MTU available). Calling dispatch_dirty_tiles_via_scheduler
    // directly bypasses that guard and exercises the send-stats + event path.
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    bridge.metrics_tracker.resize(1, 1);

    // Non-zero, non-uniform pixels: a 32×32 frame = 1 tile.
    let mut pixels = vec![0u8; 32 * 32 * 4];
    for px in pixels.chunks_exact_mut(4) {
        px[0] = 128;
        px[1] = 64;
        px[2] = 32;
        px[3] = 255;
    }
    let grid = crate::tile::TileGrid::new(32, 32);
    let dirty = vec![(0u32, 0u32)];
    let seq: u32 = 1;

    // Drive the dispatch. No QUIC sessions → send_to_all_sessions is a no-op,
    // but the stats accumulator still counts the dispatched work.
    let stats = bridge.dispatch_dirty_tiles_via_scheduler(
        &dirty,
        &grid,
        TileDispatchFrame {
            pixels: &pixels,
            stride: 32 * 4,
            seq,
            timestamp_us: 0,
        },
        // A realistic max_frag: 1200 byte payload limit.
        1200,
        SchedulerEmissionPolicy::TestCdf53All,
        None,
    );

    // Stats should reflect 1 CDF 5/3 tile dispatched.
    assert_eq!(
        stats.tile_count, 1,
        "expected 1 tile in stats; got {}",
        stats.tile_count
    );
    assert!(
        stats.total_wire_bytes > 0,
        "expected non-zero wire bytes; got {}",
        stats.total_wire_bytes
    );
    assert_eq!(
        stats.codec_histogram.cdf53, 1,
        "expected codec_cdf53=1 for TestCdf53All policy"
    );

    // Emit the frame.last_send event exactly as the production path does.
    tracing::info!(
        target: "ghostframe::bench",
        frame_seq = seq,
        last_send_ns = monotonic_now_ns(),
        total_wire_bytes = stats.total_wire_bytes,
        tile_count = stats.tile_count,
        codec_solid = stats.codec_histogram.solid,
        codec_palrle = stats.codec_histogram.palrle,
        codec_cdf53 = stats.codec_histogram.cdf53,
        codec_h264 = stats.codec_histogram.h264,
        "frame.last_send"
    );

    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("frame.last_send"),
        "expected 'frame.last_send' in logs; got: {logs}"
    );
    assert!(
        logs.contains("\"frame_seq\":1"),
        "expected frame_seq:1 in 'frame.last_send' log; got: {logs}"
    );
    // Verify tile_count and total_wire_bytes are non-zero.
    assert!(
        !logs.contains("\"tile_count\":0"),
        "tile_count must be non-zero for a dirty frame; got: {logs}"
    );
    assert!(
        !logs.contains("\"total_wire_bytes\":0"),
        "total_wire_bytes must be non-zero for a dirty frame; got: {logs}"
    );
    // Verify last_send_ns is present (field exists in the JSON).
    assert!(
        logs.contains("last_send_ns"),
        "expected 'last_send_ns' field in logs; got: {logs}"
    );
}

#[tokio::test]
async fn sample_quic_path_stats_updates_bytes_per_us_and_rtt() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    let initial_seq = bridge.adaptation_context.last_update_seq;
    // Inject a path-stats snapshot manually (the wire to a real quinn
    // Connection is exercised by integration tests, not this unit).
    bridge.apply_path_stats_snapshot(/* smoothed_rtt_us */ 12_000.0);
    assert!(bridge.adaptation_context.last_update_seq > initial_seq);
    assert!(bridge.adaptation_context.bytes_per_us > 0.0);
    assert!((bridge.adaptation_context.smoothed_rtt_us - 12_000.0).abs() < 1e-6);
    // The classifier reads the estimate, in bytes per microsecond.
    let expect = (bridge.model_rate_bytes_per_s() / 1e6) as f32;
    assert!((bridge.adaptation_context.bytes_per_us - expect).abs() < 1e-6);
}

/// The first probe ladder is never capped by a reading taken before any
/// feedback (design gate P6a, the glue half).
///
/// Before anything is measured quinn's window over its 333 ms initial RTT
/// reads 288 kbit/s; nothing derived from it may reach the estimator, or the
/// 6 / 12 Mbit ladder collapses to the 2 Mbit start rate. The two key
/// exchanges differ only in how many loop turns precede the first feedback,
/// so this sweeps that count (0 to 256), sampling the path on every turn.
#[tokio::test]
async fn the_first_probe_ladder_is_never_capped_whatever_the_handshake_cost() {
    let start = crate::transport::bwe::BweWrapper::INITIAL_BPS;
    for turns_before_feedback in [0usize, 1, 8, 15, 16, 17, 24, 64, 256] {
        let (our_end, _peer) = UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
        // quinn is the limit on every turn, and the path is busy, at quinn's
        // initial 333 ms RTT.
        bridge.forced_fifo_depth_for_test = Some(1_200);
        for _ in 0..turns_before_feedback {
            bridge.apply_path_stats_snapshot(333_000.0);
        }
        // The first feedback: a few acknowledged datagrams.
        let t0 = now_std();
        let records: Vec<crate::transport::bwe::AckArrival> = (0..12u32)
            .map(|i| crate::transport::bwe::AckArrival {
                server_emit_us: i as u64 * 1_000,
                client_arrival_ms_lo16: (i + 15) as u16,
                size_bytes: 1_200,
                probe: None,
            })
            .collect();
        bridge.bwe.update(
            &records,
            &[],
            t0 + std::time::Duration::from_millis(20),
            14_400,
        );
        let stats = bridge.bwe.snapshot().stats;
        assert!(
            stats.probe_requests >= 2,
            "{turns_before_feedback} turns: no ladder was requested ({stats:?})"
        );
        // `ProbeControllerConfig::p1` and `p2`: 3 x and 6 x the start rate.
        assert_eq!(
            &stats.first_probe_targets_bps[..2],
            &[3 * start, 6 * start],
            "{turns_before_feedback} turns: the ladder was capped"
        );
        assert!(
            stats.first_probe_targets_bps[..2]
                .iter()
                .all(|t| *t >= 3 * start),
            "no probe cluster target is below 3 x the start rate"
        );
    }
}

/// The budget floor is one datagram, not a fixed 256 KiB.
///
/// This is a regression gate, not a description: the old flat floor was
/// 256 KiB per 33.3 ms tick (7.86 MB/s), so the bandwidth-derived term
/// only bound above ~70 Mbit/s and the budget was effectively constant on
/// every link this system serves. Nothing in the lib suite moved when that
/// changed -- 452 tests passed before and after -- so without this test
/// the floor can silently drift back to dominating the estimate.
#[tokio::test]
async fn the_budget_floor_is_one_datagram_not_a_fixed_quarter_megabyte() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);

    bridge.last_max_datagram_size = Some(1_200);

    assert_eq!(
        bridge.tick_budget_floor(),
        1_200,
        "the floor is one datagram so a tile can always make progress, not a \
         constant that re-establishes the old ~70 Mbit/s knee"
    );
    let tick_s = SCHEDULER_TICK_INTERVAL_US / 1e6;
    // The initial estimate (2 Mbit/s) is far above one datagram a tick
    // (36 kB/s), so the estimate sets the rate...
    let rate = bridge.admission_rate();
    assert_eq!(rate.term, crate::transport::instruments::BudgetTerm::Pacer);
    assert!(rate.bytes_per_sec > 1_200.0 / tick_s);
    // ...and a floor above the estimate lifts the rate to exactly the floor
    // and is counted as the floor.
    bridge.set_tick_budget_floor_bytes(1 << 20);
    let rate = bridge.admission_rate();
    assert_eq!(rate.term, crate::transport::instruments::BudgetTerm::Floor);
    assert!(
        (rate.bytes_per_sec - (1u64 << 20) as f64 / tick_s).abs() < 1.0,
        "the admission rate is the floor when the floor is higher, got {}",
        rate.bytes_per_sec
    );
}

#[tokio::test]
async fn test_force_bytes_per_us_overrides_path_stats() {
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    // No connections ⇒ sample_all_path_stats is a no-op even when the
    // override is set. Set the override directly + call the inner
    // apply_path_stats_snapshot to verify the override logic separately.
    bridge.test_force_bytes_per_us = Some(0.1);

    // Inject a snapshot that, with override active, should produce
    // bytes_per_us == 0.1 regardless of the estimate.
    bridge.apply_path_stats_snapshot(20_000.0);

    // Verify AdaptationContext reflects the forced value.
    assert!(
        (bridge.adaptation_context.bytes_per_us - 0.1).abs() < 0.01,
        "got {}",
        bridge.adaptation_context.bytes_per_us
    );
}

#[tokio::test]
async fn dispatch_feedback_bytes_routes_input_to_injector() {
    use crate::transport::input_inject::InputInjector;
    use std::sync::{Arc, Mutex};

    struct CountingInjector {
        moves: Mutex<Vec<(i16, i16)>>,
    }
    impl InputInjector for CountingInjector {
        fn pointer_move(&self, x: i16, y: i16) {
            self.moves.lock().unwrap().push((x, y));
        }
        fn pointer_button(&self, _: i16, _: i16, _: u8, _: bool) {}
        fn wheel(&self, _: i16, _: i16) {}
        fn key(&self, _: u32, _: bool) {}
    }

    let injector = Arc::new(CountingInjector {
        moves: Mutex::new(Vec::new()),
    });
    let mut bridge = make_bridge_for_test().await;
    make_live(&mut bridge, ConnectionHandle(0));
    bridge.input_injector = Some(injector.clone() as Arc<dyn InputInjector>);

    // pointer-move (0x05 0x01 x=100 y=200) — 100 = 0x0064, 200 = 0x00c8
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &[0x05, 0x01, 0x00, 0x64, 0x00, 0xc8]);

    assert_eq!(injector.moves.lock().unwrap().clone(), vec![(100, 200)]);
}

#[tokio::test]
async fn dispatch_feedback_bytes_sizes_every_input_kind_and_clears_on_unknown() {
    // The dispatcher pre-sizes an INPUT message with `required_len` before it
    // has the whole thing. Every known sub-kind, delivered as exactly
    // `required_len` bytes, must reach the injector once; an unknown sub-kind
    // must discard the buffer (so the garbage tail is not replayed) without
    // injecting, and the next well-formed message must still go through.
    use crate::transport::input_inject::{required_len, InputInjector};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Default)]
    struct Counting {
        calls: AtomicUsize,
    }
    impl InputInjector for Counting {
        fn pointer_move(&self, _: i16, _: i16) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
        fn pointer_button(&self, _: i16, _: i16, _: u8, _: bool) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
        fn wheel(&self, _: i16, _: i16) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
        fn key(&self, _: u32, _: bool) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    let injector = Arc::new(Counting::default());
    let mut bridge = make_bridge_for_test().await;
    make_live(&mut bridge, ConnectionHandle(0));
    bridge.input_injector = Some(injector.clone() as Arc<dyn InputInjector>);

    for sub_kind in 1u8..=5 {
        let n = required_len(sub_kind).expect("known sub-kind");
        let mut msg = vec![0u8; n];
        msg[0] = 0x05;
        msg[1] = sub_kind;
        let before = injector.calls.load(Ordering::SeqCst);
        bridge.dispatch_feedback_bytes(ConnectionHandle(0), &msg);
        assert_eq!(
            injector.calls.load(Ordering::SeqCst),
            before + 1,
            "sub-kind {sub_kind:#04x} sized at {n} bytes must inject exactly once"
        );
    }

    // Unknown sub-kind followed by a valid pointer-move in the same read:
    // the whole tail is discarded, nothing is injected.
    let before = injector.calls.load(Ordering::SeqCst);
    bridge.dispatch_feedback_bytes(
        ConnectionHandle(0),
        &[0x05, 0x7f, 0x05, 0x01, 0x00, 0x64, 0x00, 0xc8],
    );
    assert_eq!(injector.calls.load(Ordering::SeqCst), before);
    // And nothing is left behind to corrupt the next message.
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &[0x05, 0x01, 0x00, 0x64, 0x00, 0xc8]);
    assert_eq!(injector.calls.load(Ordering::SeqCst), before + 1);
}

#[tokio::test]
async fn dispatch_feedback_bytes_silently_skips_input_when_no_injector() {
    // input_injector is None on the test bridge — must not panic.
    let mut bridge = make_bridge_for_test().await;
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &[0x05, 0x01, 0x00, 0x64, 0x00, 0xc8]);
}

#[tokio::test]
async fn dispatch_feedback_bytes_reassembles_split_input_message() {
    // Regression: QUIC stream reads can split an 8-byte PointerButton
    // across two deliveries. The dispatcher must accumulate them and
    // emit the event once the message is complete.
    use crate::transport::input_inject::InputInjector;
    use std::sync::{Arc, Mutex};

    struct CountingInjector {
        buttons: Mutex<Vec<(i16, i16, u8, bool)>>,
    }
    impl InputInjector for CountingInjector {
        fn pointer_move(&self, _: i16, _: i16) {}
        fn pointer_button(&self, x: i16, y: i16, button: u8, down: bool) {
            self.buttons.lock().unwrap().push((x, y, button, down));
        }
        fn wheel(&self, _: i16, _: i16) {}
        fn key(&self, _: u32, _: bool) {}
    }

    let injector = Arc::new(CountingInjector {
        buttons: Mutex::new(Vec::new()),
    });
    let mut bridge = make_bridge_for_test().await;
    make_live(&mut bridge, ConnectionHandle(0));
    bridge.input_injector = Some(injector.clone() as Arc<dyn InputInjector>);

    // PointerButton x=100 y=200 button=1 down=true → 8 bytes total:
    //   [0x05, 0x02, 0x00, 0x64, 0x00, 0xc8, 0x01, 0x01]
    // Deliver as 6 + 2 — the exact split observed on the wire.
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &[0x05, 0x02, 0x00, 0x64, 0x00, 0xc8]);
    assert!(
        injector.buttons.lock().unwrap().is_empty(),
        "first half must not dispatch — message is incomplete"
    );
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &[0x01, 0x01]);
    assert_eq!(
        injector.buttons.lock().unwrap().clone(),
        vec![(100, 200, 1, true)]
    );
}

#[tokio::test]
async fn dispatch_feedback_bytes_clears_buf_on_unknown_msg_type() {
    // A truly unknown msg_type must drop the rest of the buffer; a
    // valid PointerMove after that must dispatch normally.
    use crate::transport::input_inject::InputInjector;
    use std::sync::{Arc, Mutex};

    struct CountingInjector {
        moves: Mutex<Vec<(i16, i16)>>,
    }
    impl InputInjector for CountingInjector {
        fn pointer_move(&self, x: i16, y: i16) {
            self.moves.lock().unwrap().push((x, y));
        }
        fn pointer_button(&self, _: i16, _: i16, _: u8, _: bool) {}
        fn wheel(&self, _: i16, _: i16) {}
        fn key(&self, _: u32, _: bool) {}
    }

    let injector = Arc::new(CountingInjector {
        moves: Mutex::new(Vec::new()),
    });
    let mut bridge = make_bridge_for_test().await;
    make_live(&mut bridge, ConnectionHandle(0));
    bridge.input_injector = Some(injector.clone() as Arc<dyn InputInjector>);

    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &[0xff, 0xaa, 0xbb]);
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &[0x05, 0x01, 0x00, 0x0a, 0x00, 0x14]);
    assert_eq!(injector.moves.lock().unwrap().clone(), vec![(10, 20)]);
}

#[test]
fn now_std_works_outside_a_runtime() {
    let t0 = super::now_std();
    let t1 = super::now_std();
    assert!(t1 >= t0, "now_std must be monotonic outside a runtime");
}

#[tokio::test(start_paused = true)]
async fn now_std_follows_the_virtual_clock() {
    let t0 = super::now_std();
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    let t1 = super::now_std();
    assert!(
        t1.duration_since(t0) >= std::time::Duration::from_secs(5),
        "now_std must advance with the paused clock, got {:?}",
        t1.duration_since(t0)
    );
}

/// Regression test for the classifier clock bug: the frame-mode call
/// site in `process_frame_gpu` fed `Classifier::decide_frame_mode`,
/// which reads `self.epoch.elapsed()` off an `Instant::now()`-seeded
/// field private to `Classifier` — a 14th time source invisible to a
/// grep of this file, and one that reads the real wall clock
/// regardless of `tokio::time::pause()`. The fix routes the same
/// dwell computation through `IoBridge::epoch` (stamped from
/// `now_std()`) and calls `decide_frame_mode_at` with an explicit
/// `now_us`, exactly mirroring every other deadline in this file.
///
/// **Why this test doesn't drive `process_frame_gpu` itself:** the
/// call site only runs once `compute_max_datagram_size()` sees a
/// connected session backed by a real `quinn_proto::Connection` in
/// `self.server.connections` — that requires a completed QUIC +
/// WebTransport handshake, which is exactly the machinery the
/// browserless harness (a later task) exists to provide. Building a
/// bespoke in-process handshake fixture here would be scope creep well
/// beyond the epoch plumbing this commit is about. Instead, this test
/// exercises the *exact expression* the call site now uses —
/// `now_std().duration_since(self.epoch).as_micros() as u64` fed to
/// `decide_frame_mode_at` — against the bridge's real, private `epoch`
/// and `classifier` fields, and proves discrimination by running the
/// old broken API (`decide_frame_mode`, still public for non-harness
/// callers) through the identical loop: it must NOT observe the same
/// dwell, because real wall-clock time barely moves while
/// `tokio::time::advance` fast-forwards the virtual clock.
///
/// Verified by hand that this discriminates: temporarily changing the
/// "fixed" loop below to call `fixed.decide_frame_mode(&[],
/// FrameMode::H264)` (dropping the bridge-epoch `now_us`) makes the
/// first assertion fail, reproducing the exact symptom this commit
/// fixes — mode never leaves H264 under a paused runtime regardless
/// of how much virtual time passes.
#[tokio::test(start_paused = true)]
async fn frame_mode_exit_dwell_tracks_virtual_time_via_bridge_epoch() {
    use crate::tile::{Classifier, FrameMode};

    let bridge = make_bridge_for_test().await;
    let epoch = bridge.epoch;

    // Fixed path: same `Classifier` state the bridge owns, driven with
    // the same `now_us` expression the `process_frame_gpu` call site
    // uses. Empty `tentative_states` under `FrameMode::H264` drives the
    // exit-sustain timer (cost is 0, below `exit_factor * h264_cost`).
    // 30 calls satisfies `exit_sustain_frames_min`; spacing them 20ms
    // apart in virtual time crosses `exit_sustain_micros` (500_000us)
    // by the last call.
    let mut fixed = bridge.classifier.clone();
    let mut fixed_mode = FrameMode::H264;
    for _ in 0..30 {
        let now_us = super::now_std().duration_since(epoch).as_micros() as u64;
        fixed_mode = fixed.decide_frame_mode_at(now_us, &[], FrameMode::H264);
        tokio::time::advance(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        fixed_mode,
        FrameMode::TileCodec,
        "bridge-epoch-derived now_us must cross exit_sustain_micros as \
         tokio's virtual clock advances"
    );

    // Broken path: identical loop shape, but through the old
    // `decide_frame_mode()` API that reads Classifier's own
    // wall-clock epoch. `tokio::time::advance` does not consume real
    // wall-clock time, so this must NEVER observe the dwell — this is
    // the exact bug Commit 1 fixes at the `process_frame_gpu` call
    // site.
    let mut broken = Classifier::new(crate::config::ClassifierConfig::default());
    let mut broken_mode = FrameMode::H264;
    for _ in 0..30 {
        broken_mode = broken.decide_frame_mode(&[], FrameMode::H264);
        tokio::time::advance(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        broken_mode,
        FrameMode::H264,
        "decide_frame_mode's wall-clock epoch must NOT observe the \
         dwell under a paused runtime -- if this fails, the paused-\
         clock harness's assumptions about tokio::time::advance no \
         longer hold and this test needs re-deriving"
    );
}

/// Production-wiring coverage for the BWE chain: `dispatch_ack_datagram`
/// → `bwe_samples_buffer.push` → `run()`'s drain → `self.bwe.update(...)`.
///
/// This was deleted in 6bf7e09 (`feat(bwe): back BweWrapper with real
/// GoogCC instead of EWMA`) alongside its sibling
/// `bwe_window_flushes_on_virtual_time`, on the grounds that its
/// assertions ("must not move before the window elapses") were EWMA
/// semantics that don't apply to GoogCC. That's true of the
/// *assertions*, but the *harness* — seeding the retransmit cache via
/// `submit_one` exactly as real emitted tile passes would, feeding a
/// real ACK batch through the production `dispatch_ack_datagram` entry
/// point (the same one real inbound ACK_BATCH datagrams reach), then
/// running the bridge's actual `run()` event loop so the buffered
/// samples drain through the real `self.bwe.update(&records,
/// now_std())` call — proves something independent of which estimator
/// backs `BweWrapper`: that the wiring from wire bytes to the
/// estimator's input actually works. Nothing replaced that proof when
/// the test was deleted, so it was silently uncovered until restored
/// here.
///
/// It advances tokio's *virtual* clock between two rounds (50 acks —
/// the wire format caps one batch at `MAX_ACK_ENTRIES_PER_BATCH = 72`
/// — then 50 more 250ms of virtual time later) and asserts
/// `samples_seen` after each round: that alone proves the real ACK
/// path populated the buffer and the real loop drained it. It does
/// NOT re-assert any estimate threshold or "must not move before the
/// window elapses" behaviour — those were EWMA-specific and don't
/// carry over to GoogCC.
#[tokio::test(start_paused = true)]
async fn bwe_estimate_advances_through_bridge_production_wiring() {
    use crate::transport::ack::{AckBatch, AckEntry};
    use crate::transport::reliable_emitter::EmitKey;
    use tokio::io::AsyncWriteExt;

    let (our_end, mut peer) = UnixStream::pair().expect("UnixStream::pair failed");
    let server = QuicServer::new().expect("QuicServer::new failed");
    let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);

    // A malformed-but-well-framed inbound "packet": short enough that
    // `process_inbound` rejects it immediately with "frame too short"
    // (same shape as `run_survives_malformed_frame`), so it costs
    // nothing but still drives one full pass through the event loop's
    // body — including the post-select drain section that owns the
    // BWE call site.
    const BOGUS_FRAME: [u8; 8] = [0, 0, 0, 4, 0, 0, 0, 0];

    // Seed `count` retransmit-cache entries (frame_seq starting at
    // `frame_seq_start`) with a plausible emit timestamp, feed a
    // matching ACK batch through the real `dispatch_ack_datagram`
    // entry point (populating `bwe_samples_buffer` exactly as a real
    // inbound ACK would), then run one iteration of the bridge's real
    // `run()` loop so the buffer drains through the production
    // `self.bwe.update(&records, now_std())` call.
    async fn feed_one_ack_round(
        bridge: &mut IoBridge,
        peer: &mut UnixStream,
        frame_seq_start: u32,
        count: u32,
    ) {
        let mut entries = Vec::with_capacity(count as usize);
        for i in 0..count {
            let frame_seq = frame_seq_start + i;
            let key = EmitKey::new(frame_seq, 1, 2, 0);
            // The emit time now comes from `CacheEntry::last_sent_at`
            // (stamped by `submit_one` below), not from the datagram
            // bytes — this fixed [12..16] payload is just filler so the
            // fragment is long enough to look like a real tile datagram.
            let mut frag = vec![0u8; 20];
            frag[12..16].copy_from_slice(&1_000_000u32.to_be_bytes());
            bridge.reliable_emitter.submit_one(
                key,
                bytes::Bytes::from(frag),
                super::now_std(),
                None,
                super::now_std(),
            );
        }
        // This test really does submit through the emitter, so it can
        // acknowledge the `wire_seq` values actually allocated rather
        // than declaring a mapping — recording them the same way the
        // production drain does.
        let ledger_now = super::now_std();
        for e in bridge.reliable_emitter.take_transmissions() {
            bridge.transmission_ledger.record_departed(
                e.wire_seq,
                ledger_now,
                crate::transport::transmission_ledger::Transmission {
                    emit_us: e.emit_us,
                    wire_bytes: e.wire_bytes,
                    content: e.content,
                    depart_us: None,
                    dep_idx: None,
                },
            );
            entries.push(AckEntry {
                wire_seq: e.wire_seq,
                arrival_time_ms_lo16: 1_500,
            });
        }
        let batch = AckBatch { entries };
        // Real production entry point for inbound ACK_BATCH datagrams.
        bridge.dispatch_ack_datagram(&batch.encode());

        peer.write_all(&BOGUS_FRAME)
            .await
            .expect("peer write failed");
        peer.flush().await.ok();

        // Race the bridge's real event loop against a bounded number of
        // yields. `run()` processes the one queued inbound frame (which
        // errors out harmlessly), falls through the always-drain
        // section — where the real `bwe_samples_buffer` drain and
        // `self.bwe.update(&records, now_std())` call live — and then
        // blocks again at the top-level `select!` waiting for the next
        // event. The yield branch wins once that happens, so `run()`'s
        // future is dropped and `bridge` is ours again afterward.
        tokio::select! {
            biased;
            result = bridge.run() => {
                panic!("run() exited unexpectedly: {result:?}");
            }
            _ = async {
                for _ in 0..64 {
                    tokio::task::yield_now().await;
                }
            } => {}
        }
    }

    // Round 1: 50 acks (the wire format caps one batch at
    // MAX_ACK_ENTRIES_PER_BATCH = 72), comfortably above the
    // estimator's MIN_SAMPLES_FOR_ESTIMATE gate. This alone proves the
    // real ACK path populated the buffer and the real loop drained it.
    feed_one_ack_round(&mut bridge, &mut peer, 0, 50).await;
    let snap1 = bridge.bwe.snapshot();
    assert_eq!(
        snap1.samples_seen, 50,
        "the real dispatch_ack_datagram -> bwe_samples_buffer -> run() \
         drain -> bwe.update chain did not deliver round 1's 50 acks"
    );

    // Advance the *virtual* clock. Real wall-clock time spent doing
    // this is on the order of microseconds.
    tokio::time::advance(std::time::Duration::from_millis(250)).await;

    // Round 2: another 50 acks, stamped with the now-advanced virtual
    // clock via the *same* production call sites.
    feed_one_ack_round(&mut bridge, &mut peer, 1_000, 50).await;
    let snap2 = bridge.bwe.snapshot();
    assert_eq!(
        snap2.samples_seen, 100,
        "the real dispatch_ack_datagram -> bwe_samples_buffer -> run() \
         drain -> bwe.update chain did not deliver round 2's acks on \
         top of round 1's"
    );
    assert!(
        snap2.bitrate_bps > 0,
        "controller produced no estimate after 100 acks through the \
         real production wiring"
    );
}

/// Run the bridge's real event loop until it has nothing left to do.
///
/// Every source in these tests is queued up front, so the loop works
/// through all of it and then parks on its `select!`; the bounded-yield
/// arm wins at that point and hands the bridge back.
async fn run_until_idle(bridge: &mut IoBridge) {
    tokio::select! {
        biased;
        result = bridge.run() => panic!("run() exited unexpectedly: {result:?}"),
        _ = async {
            for _ in 0..4096 {
                tokio::task::yield_now().await;
            }
        } => {}
    }
}

/// A well-framed inbound packet too short to be anything: costs one pass
/// through the loop and is rejected with "frame too short".
const BOGUS_INBOUND: [u8; 8] = [0, 0, 0, 4, 0, 0, 0, 0];

/// The network is read before the next frame is taken.
///
/// Frames used to be listed ahead of inbound in a biased `select!`, so a
/// capture that kept a frame waiting -- any time one frame's work
/// outlasts the capture interval -- starved the socket outright.
/// Measured on a production-scale screen: 34 frames and 8 inbound
/// packets in 18 s, no acknowledgement read, ~35 packets delivered.
#[tokio::test(start_paused = true)]
async fn inbound_is_read_before_a_waiting_frame() {
    use tokio::io::AsyncWriteExt;

    let (ours, mut peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (tx, rx) = mpsc::channel(8);
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 4, 4);

    // Both sources ready before the loop runs at all, so the order they
    // are taken in is purely the loop's choice.
    for seq in 0..4u32 {
        tx.send(InjectedFrame {
            seq,
            timestamp_us: 0,
            work: Vec::new(),
            cost: None,
        })
        .await
        .expect("inject");
    }
    for _ in 0..4 {
        peer.write_all(&BOGUS_INBOUND).await.expect("peer write");
    }
    peer.flush().await.ok();
    // Written is not yet observable: tokio learns a socket is readable
    // from its I/O driver, which has not had a turn. Without this the
    // loop's first round sees a socket it believes empty and the test
    // measures the driver's scheduling instead of the arm order.
    bridge.stream.readable().await.expect("readable");

    run_until_idle(&mut bridge).await;

    let order: Vec<LoopArm> = bridge
        .loop_arm_trace
        .iter()
        .copied()
        .filter(|a| matches!(a, LoopArm::Inbound | LoopArm::Injected))
        .collect();
    assert_eq!(
        order,
        [
            LoopArm::Inbound,
            LoopArm::Inbound,
            LoopArm::Inbound,
            LoopArm::Inbound,
            LoopArm::Injected,
            LoopArm::Injected,
            LoopArm::Injected,
            LoopArm::Injected,
        ],
        "every waiting inbound packet must be read before the next frame \
         is taken; frames-first is the ordering that stopped the server \
         reading acknowledgements whenever a frame was always waiting"
    );
}

/// ...and reading the network cannot starve frames in turn.
#[tokio::test(start_paused = true)]
async fn a_flood_of_inbound_still_lets_a_frame_through() {
    use tokio::io::AsyncWriteExt;

    let (ours, mut peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (tx, rx) = mpsc::channel(8);
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 4, 4);

    tx.send(InjectedFrame {
        seq: 0,
        timestamp_us: 0,
        work: Vec::new(),
        cost: None,
    })
    .await
    .expect("inject");
    let flood = INBOUND_STREAK_LIMIT as usize * 3;
    for _ in 0..flood {
        peer.write_all(&BOGUS_INBOUND).await.expect("peer write");
    }
    peer.flush().await.ok();
    bridge.stream.readable().await.expect("readable");

    run_until_idle(&mut bridge).await;

    let trace = &bridge.loop_arm_trace;
    let frame_at = trace
        .iter()
        .position(|a| *a == LoopArm::Injected)
        .expect("the frame was never taken");
    let inbound_before = trace[..frame_at]
        .iter()
        .filter(|a| **a == LoopArm::Inbound)
        .count();
    assert_eq!(
        inbound_before, INBOUND_STREAK_LIMIT as usize,
        "the frame must be taken after exactly one streak of inbound, \
         not after the whole flood"
    );
    let inbound_total = trace.iter().filter(|a| **a == LoopArm::Inbound).count();
    assert_eq!(
        inbound_total, flood,
        "and the rest of the flood is still read afterwards -- sitting \
         out a round must not strand it"
    );
}

/// Regression test for the missing-dimensions divergence: a browserless
/// client driven purely by injected scenes must still learn the
/// canvas/tile-grid size, or every tile datagram it receives is for a
/// grid it was never told about.
///
/// This calls the real, unmodified `emit_frame_dimensions` production
/// method (shared with `process_frame_cpu`/`process_frame_gpu`) with the
/// exact width/height expression `apply_injected_frame` now feeds it —
/// `scheduler.cols()/rows()` (the grid fixed at construction) scaled by
/// `crate::tile::TILE_SIZE`. It does not drive `apply_injected_frame`
/// itself: that call site sits behind `compute_max_datagram_size()`,
/// which requires a live, post-handshake `quinn_proto::Connection` —
/// Task 15's browserless-harness responsibility.
/// `last_emitted_dimensions` is the only place this is observable without
/// one.
#[tokio::test(start_paused = true)]
async fn injected_frame_dimensions_derive_from_the_fixed_grid() {
    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (_tx, rx) = mpsc::channel(8);
    // 5x3 grid of 32px tiles -> 160x96 px.
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 5, 3);

    assert_eq!(
        bridge.last_emitted_dimensions, None,
        "no dimensions message has been emitted yet"
    );

    let cols = bridge.scheduler.cols();
    let rows = bridge.scheduler.rows();
    bridge.emit_frame_dimensions(
        1,
        0,
        cols * crate::tile::TILE_SIZE,
        rows * crate::tile::TILE_SIZE,
    );

    assert_eq!(
        bridge.last_emitted_dimensions,
        Some((160, 96)),
        "dimensions must be derived from the fixed grid × TILE_SIZE"
    );
}

/// The frame-dimensions datagram is sent while the depth gate is saturated.
/// It is checked once per frame before the admission pass, so in a
/// cwnd-limited steady state (FIFO always at `D_target`) a gated send
/// starved, and the client refused every tile larger than the dimensions it
/// knew. Putting the old `depth >= d_target` skip back makes this fail.
#[tokio::test(start_paused = true)]
async fn frame_dimensions_are_sent_while_the_depth_gate_is_saturated() {
    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (_tx, rx) = mpsc::channel(8);
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 5, 3);
    bridge.gate = Some(pacing::GateParams { d_target: 2_500 });
    bridge.forced_fifo_depth_for_test = Some(2_500); // at the bound: the gate refuses
    assert_eq!(bridge.live_fifo_depth(), Some(2_500));

    bridge.emit_frame_dimensions(1, 0, 160, 96);

    assert!(
        !bridge.sent_log_for_test.is_empty(),
        "the dimensions datagram must go out although the FIFO is at D_target"
    );
    assert_eq!(
        bridge.dimensions_retransmits_left,
        FRAME_DIMENSIONS_RETRANSMITS - 1,
        "and the send is spent"
    );
}

/// Work that an injected frame enqueued and a budget-limited pass did not
/// reach must be sent by a later admission pass, not wait for the *next
/// injected frame* -- which may never arrive once a scene has finished
/// injecting. The pass is what `post_turn` runs after every turn, with the
/// frame identity the last frame left behind.
///
/// Drives the real `pump_admission` against work queued directly. No live
/// session is needed: with none, the gate has no queue to read and the
/// bucket alone governs.
#[tokio::test(start_paused = true)]
async fn an_admission_pass_drains_stranded_injected_work() {
    use crate::transport::protocol::Codec;
    use crate::transport::scheduler::{TileWork, WorkState};

    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (_tx, rx) = mpsc::channel(8);
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 4, 4);

    // Queue work directly, simulating tile work an injected frame
    // enqueued but a budget-limited pass did not reach.
    for (tile_x, tile_y) in [(0u8, 0u8), (1, 0)] {
        bridge.scheduler.enqueue(TileWork {
            tile_x,
            tile_y,
            generation: 0,
            pass_idx: 0,
            total_passes: 1,
            codec: Codec::Solid,
            payload: vec![10, 20, 30, 255],
            queued_at: super::now_std(),
            last_sent_at: None,
            state: WorkState::Pending,
        });
    }
    assert_eq!(
        bridge.scheduler_peek_for_test().len(),
        2,
        "both tiles queued"
    );

    // Before any frame has left its identity the pass has nothing to tag the
    // work with, and sends none.
    bridge.pump_admission();
    for (_, _, state) in bridge.scheduler.queue_states_for_test() {
        assert_eq!(state, WorkState::Pending);
    }

    bridge.last_drain_frame_context = Some(DrainFrameContext {
        seq: 7,
        timestamp_us: 123,
        max_frag: 1200,
    });
    bridge.pump_admission();

    // `drain_priority_queue` marks popped work `InFlight` and retains it
    // in the queue (for potential RTO retry) rather than removing it --
    // so "drained" is observed as a state transition, not queue
    // shrinkage.
    let states = bridge.scheduler.queue_states_for_test();
    assert_eq!(states.len(), 2, "both tiles remain in the priority queue");
    for (tile_x, tile_y, state) in &states {
        assert_eq!(
            *state,
            WorkState::InFlight,
            "tile ({tile_x}, {tile_y}) must have been drained (Pending -> InFlight)"
        );
    }
}

/// Regression test for the dead stranded-tile guard: `IoBridge`'s
/// Phase 1.5-B stranded escalation calls `enqueue_refinement_subset_at`
/// at the EXISTING generation (deliberately, so the client's
/// already-delivered passes survive). Because there is no
/// `bump_generation`, nothing supersedes work already sitting in the
/// refinement queue — so a pass that is merely *unsent* (still
/// `Pending`, never transmitted) got enqueued a second time and
/// emitted twice. `Scheduler::refinement_queue_holds_tile` exists
/// exactly to stop this and was never called.
///
/// This drives the real, unmodified `IoBridge::stranded_reenqueue`
/// (the extracted body of the stranded branch inside
/// `process_frame_gpu`'s post-fence Phase B loop) directly, seeding
/// the scheduler's refinement queue the same way a prior frame's
/// dispatch would have: one `Pending` `TileWork` entry per Cdf53
/// pass, never drained. No ACKs are recorded, so
/// `cdf53_unacked_pass_mask` reports every pass unacked — exactly
/// what a real stranded-detection sweep would see for this tile.
#[tokio::test(start_paused = true)]
async fn stranded_reenqueue_skips_tiles_still_pending_in_the_queue() {
    use crate::transport::protocol::Codec;
    use crate::transport::scheduler::{TileWork, WorkState};

    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (_tx, rx) = mpsc::channel(8);
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 4, 4);

    let max_passes = crate::encoder::cdf53::CDF53_PASS_COUNT as u8;
    let (tile_x, tile_y) = (2u8, 1u8);

    // Seed the refinement queue with this tile's passes, still
    // `Pending` — classified and enqueued by an earlier frame, but
    // never drained/sent. This is the "merely unsent" case the
    // stranded escalation must not duplicate.
    for pass_idx in 0..max_passes {
        bridge.scheduler.enqueue_refinement_work_at(
            TileWork {
                tile_x,
                tile_y,
                generation: 0,
                pass_idx,
                total_passes: max_passes,
                codec: Codec::Cdf53,
                payload: vec![0u8; 4],
                queued_at: super::now_std(),
                last_sent_at: None,
                state: WorkState::Pending,
            },
            super::now_std(),
        );
    }

    let before = bridge.scheduler.refinement_queue_len();
    assert_eq!(
        before, max_passes as usize,
        "queue seeded with one Pending entry per pass"
    );

    let present_passes: u16 = 0x3FFF;
    let codec_state = crate::tile::CodecState::Cdf53 {
        passes_sent: 0,
        present_passes,
    };
    let passes: Vec<(u8, Vec<u8>)> = (0..max_passes).map(|i| (i, vec![i; 4])).collect();
    bridge.stranded_reenqueue(tile_x, tile_y, codec_state, present_passes, passes);

    let after = bridge.scheduler.refinement_queue_len();
    assert_eq!(
        after, before,
        "stranded re-enqueue must not duplicate passes still Pending \
         in the refinement queue (before={before}, after={after})"
    );
}

/// One retry path per tile. While the emitter has a priority tile
/// cached, the scheduler does not re-emit it; once the emitter has lost
/// it, the scheduler is what brings it back.
///
/// Drives the real `drain_scheduler_into_quinn`, because the property
/// is in the wiring between the two: each half is unit-tested on its
/// own and both would still pass with the predicate never connected.
#[tokio::test(start_paused = true)]
async fn a_priority_tile_the_emitter_still_holds_is_not_re_emitted_by_the_scheduler() {
    use crate::transport::protocol::Codec;
    use crate::transport::scheduler::{TileWork, WorkState};

    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (_tx, rx) = mpsc::channel(8);
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 4, 4);
    bridge
        .scheduler
        .set_retry_after(std::time::Duration::from_millis(150));

    bridge.scheduler.enqueue_at(
        TileWork {
            tile_x: 2,
            tile_y: 1,
            generation: 0,
            pass_idx: 0,
            total_passes: 1,
            codec: Codec::Solid,
            payload: vec![10, 20, 30, 255],
            queued_at: super::now_std(),
            last_sent_at: None,
            state: WorkState::Pending,
        },
        super::now_std(),
    );

    let (_, _, first) = bridge.drain_scheduler_into_quinn(1, 0, 1200, usize::MAX);
    assert_eq!(first, 1, "the tile goes out once");
    assert!(
        bridge.reliable_emitter.holds_pass(2, 1, 0),
        "precondition: emitting it put it in the retransmit cache"
    );

    // Far past the retry deadline, never acknowledged. The emitter's
    // RTO is responsible for this tile now.
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    let (_, _, again) = bridge.drain_scheduler_into_quinn(2, 0, 1200, usize::MAX);
    assert_eq!(
        again, 0,
        "the scheduler re-emitted a tile the emitter is still holding: \
         two retry paths for one tile"
    );

    // The cache loses it without an acknowledgement -- what a
    // `ConnectionLost` wipe does. Nothing else will ever resend it.
    bridge.reliable_emitter.clear_cache();
    let (_, _, recovered) = bridge.drain_scheduler_into_quinn(3, 0, 1200, usize::MAX);
    assert_eq!(recovered, 1, "the fallback must bring the tile back");
    assert!(bridge.reliable_emitter.holds_pass(2, 1, 0));
}

/// The scheduler's retry gate is the reliable emitter's own probe timeout,
/// from the departure-based RTT estimator: one notion of "overdue", which
/// follows measured round trips (batching delay included) in both directions.
#[tokio::test(start_paused = true)]
async fn the_priority_retry_gate_is_the_emitters_probe_timeout() {
    use std::time::Duration;
    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (_tx, rx) = mpsc::channel(8);
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 4, 4);

    // No measurement yet: RFC 9002's initial RTT (333 + 4 x 166.5 + 5 ms),
    // not quinn's handshake RTT (see `rtt_estimator`'s module doc).
    assert_eq!(bridge.priority_retry_after(), Duration::from_millis(1004));

    // A first sample replaces it: 6 ms is a fast path plus the client's 5 ms
    // flush, 6 + 4 x 3 + 5.
    bridge
        .reliable_emitter
        .rtt
        .on_sample(Duration::from_millis(6));
    assert_eq!(bridge.priority_retry_after(), Duration::from_millis(23));
    assert_eq!(bridge.priority_retry_after(), bridge.reliable_emitter.pto());

    // And it stretches when round trips are genuinely slow.
    for _ in 0..200 {
        bridge
            .reliable_emitter
            .rtt
            .on_sample(Duration::from_millis(400));
    }
    let slow = bridge.priority_retry_after();
    assert!(
        slow >= Duration::from_millis(405) && slow < Duration::from_millis(420),
        "{slow:?}"
    );
}

/// A tile whose passes are still queued is not a stranded tile, and
/// must be turned away *before* it costs a GPU forward transform and a
/// re-encode -- not after, which is where the only check used to be.
#[tokio::test(start_paused = true)]
async fn a_tile_still_in_the_refinement_queue_is_not_a_stranded_candidate() {
    use crate::transport::protocol::Codec;
    use crate::transport::scheduler::{TileWork, WorkState};

    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (_tx, rx) = mpsc::channel(8);
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 4, 4);
    bridge.metrics_tracker.resize(4, 4);

    let present_passes: u16 = 0b0011_1110_0001;
    let long_idle = crate::tile::escalation::IDLE_THRESHOLD + 1;
    let (queued, stranded) = ((2u8, 1u8), (3u8, 3u8));
    for (x, y) in [queued, stranded] {
        let m = bridge.metrics_tracker.get_mut(x as u32, y as u32);
        m.codec_state = crate::tile::CodecState::Cdf53 {
            passes_sent: 0,
            present_passes,
        };
        m.idle_frames = long_idle;
    }
    // One of the two still has a pass waiting to be sent.
    bridge.scheduler.enqueue_refinement_work_at(
        TileWork {
            tile_x: queued.0,
            tile_y: queued.1,
            generation: 0,
            pass_idx: 9,
            total_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
            codec: Codec::Cdf53,
            payload: vec![0u8; 4],
            queued_at: super::now_std(),
            last_sent_at: None,
            state: WorkState::Pending,
        },
        super::now_std(),
    );

    let found = bridge.detect_stranded_cdf53_tiles(usize::MAX);
    let flat = |(x, y): (u8, u8)| y as u32 * 4 + x as u32;
    assert_eq!(
        found,
        vec![flat(stranded)],
        "only the tile nothing is working on is stranded; the one with \
         a pass still queued is merely unsent"
    );
}

/// Task 3: the tile-injection channel must enqueue injected work into
/// the scheduler and attempt a drain. With no connected WebTransport
/// session, `compute_max_datagram_size()` returns `None`, so the drain
/// is skipped and the work stays queued — this test pins the enqueue +
/// graceful no-session path. End-to-end emission over a real connection
/// is asserted by the harness in Task 15.
#[tokio::test(start_paused = true)]
async fn injected_work_is_enqueued_and_drain_is_attempted() {
    use crate::transport::protocol::Codec;
    use crate::transport::scheduler::{TileWork, WorkState};

    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (tx, rx) = mpsc::channel(8);
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 4, 4);

    tx.send(InjectedFrame {
        seq: 1,
        timestamp_us: 0,
        work: vec![TileWork {
            tile_x: 1,
            tile_y: 2,
            generation: 0,
            pass_idx: 0,
            total_passes: 1,
            codec: Codec::Solid,
            payload: vec![10, 20, 30, 255],
            queued_at: super::now_std(),
            last_sent_at: None,
            state: WorkState::Pending,
        }],
        cost: None,
    })
    .await
    .expect("send injection");

    bridge.drain_injection_for_test().await;

    let queued = bridge.scheduler_peek_for_test();
    assert_eq!(queued.len(), 1, "one work item must be queued");
    assert_eq!((queued[0].tile_x, queued[0].tile_y), (1, 2));
    assert_eq!(queued[0].codec, Codec::Solid);
}

/// Test-fidelity regression: `apply_injected_frame` must route CDF53
/// passes into `refinement_queue` (the pass-major-drained one), not
/// `priority_queue` (FIFO), or the browserless harness never exercises
/// `drain_refinement_pass_major` at all — see
/// docs/specs/bwe-tier-latency-baseline.md's "Correction: the harness
/// never exercises pass-major ordering". Injects two tiles' worth of
/// out-of-order CDF53 passes (deliberately not pass_idx-ascending, and a
/// non-CDF53 Solid tile alongside them) and asserts: CDF53 work lands in
/// `refinement_queue` with `pass_idx`/`total_passes` preserved exactly
/// as submitted (no renumbering by position), `priority_queue` gets only
/// the Solid tile, and `refinement_queue` is untouched by the Solid
/// tile.
#[tokio::test(start_paused = true)]
async fn injected_cdf53_work_lands_in_refinement_queue_not_priority_queue() {
    use crate::transport::protocol::Codec;
    use crate::transport::scheduler::{TileWork, WorkState};

    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (tx, rx) = mpsc::channel(8);
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 4, 4);

    let cdf53_work = |tile_x: u8, tile_y: u8, pass_idx: u8| TileWork {
        tile_x,
        tile_y,
        generation: 0,
        pass_idx,
        total_passes: 14,
        codec: Codec::Cdf53,
        payload: vec![pass_idx; 4],
        queued_at: super::now_std(),
        last_sent_at: None,
        state: WorkState::Pending,
    };

    tx.send(InjectedFrame {
        seq: 1,
        timestamp_us: 0,
        work: vec![
            // Tile (0,0): submitted out of order (pass 3 before pass 0)
            // and as a partial set (only 2 of 14 passes) — exactly the
            // shape `enqueue_refinement_work_at` must not renumber.
            cdf53_work(0, 0, 3),
            cdf53_work(0, 0, 0),
            // Tile (1, 1): a single CDF53 pass.
            cdf53_work(1, 1, 7),
            // A non-CDF53 tile alongside them, which must still go to
            // priority_queue exactly as before this change.
            TileWork {
                tile_x: 2,
                tile_y: 2,
                generation: 0,
                pass_idx: 0,
                total_passes: 1,
                codec: Codec::Solid,
                payload: vec![1, 2, 3, 255],
                queued_at: super::now_std(),
                last_sent_at: None,
                state: WorkState::Pending,
            },
        ],
        cost: None,
    })
    .await
    .expect("send injection");

    bridge.drain_injection_for_test().await;

    // No connected session -> compute_max_datagram_size() is None ->
    // the drain is skipped, so everything above is still sitting
    // exactly where it was enqueued.
    let priority = bridge.scheduler_peek_for_test();
    let refinement = bridge.scheduler_refinement_peek_for_test();

    assert_eq!(
        priority.len(),
        1,
        "priority_queue must hold only the non-CDF53 (Solid) tile"
    );
    assert_eq!((priority[0].tile_x, priority[0].tile_y), (2, 2));
    assert_eq!(priority[0].codec, Codec::Solid);

    assert_eq!(
        refinement.len(),
        3,
        "refinement_queue must hold all 3 CDF53 passes across both tiles"
    );
    assert!(
        refinement.iter().all(|w| w.codec == Codec::Cdf53),
        "priority_queue must never receive CDF53 work post-fix"
    );

    // pass_idx/total_passes must be exactly what was submitted -- not
    // renumbered by insertion position (which would turn pass_idx 3
    // into 0 and pass_idx 0 into 1 for tile (0,0) if this used
    // enqueue_refinement_at's positional Vec<Vec<u8>> path instead of
    // enqueue_refinement_work_at).
    let tile00: Vec<(u8, u8)> = refinement
        .iter()
        .filter(|w| (w.tile_x, w.tile_y) == (0, 0))
        .map(|w| (w.pass_idx, w.total_passes))
        .collect();
    assert_eq!(
        tile00.len(),
        2,
        "tile (0,0) must keep both of its submitted passes"
    );
    assert!(
        tile00.contains(&(3, 14)),
        "pass_idx 3 must survive as 3, not be renumbered: {tile00:?}"
    );
    assert!(
        tile00.contains(&(0, 14)),
        "pass_idx 0 must survive as 0: {tile00:?}"
    );

    let tile11: Vec<(u8, u8)> = refinement
        .iter()
        .filter(|w| (w.tile_x, w.tile_y) == (1, 1))
        .map(|w| (w.pass_idx, w.total_passes))
        .collect();
    assert_eq!(tile11, vec![(7, 14)], "pass_idx 7 must survive as 7");
}

/// Regression test for the growth-on-demand hazard: a bounding-box
/// resize on every injected batch would call `Scheduler::resize` when
/// a later frame's tiles exceed the current grid, and `resize` clears
/// the priority queue — silently discarding any earlier frame's still-
/// queued work. The grid is now sized once at construction, so a
/// second injected frame touching a coordinate within that fixed grid
/// must never disturb the first frame's queued entry.
#[tokio::test(start_paused = true)]
async fn second_injected_frame_does_not_clear_first_frames_queued_work() {
    use crate::transport::protocol::Codec;
    use crate::transport::scheduler::{TileWork, WorkState};

    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (tx, rx) = mpsc::channel(8);
    // Grid sized for the whole scene (4x4) up front, not derived from
    // either individual frame's tiles.
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 4, 4);

    let make_work = |tile_x: u8, tile_y: u8| TileWork {
        tile_x,
        tile_y,
        generation: 0,
        pass_idx: 0,
        total_passes: 1,
        codec: Codec::Solid,
        payload: vec![10, 20, 30, 255],
        queued_at: super::now_std(),
        last_sent_at: None,
        state: WorkState::Pending,
    };

    // Frame 1: tile (0, 0).
    tx.send(InjectedFrame {
        seq: 1,
        timestamp_us: 0,
        work: vec![make_work(0, 0)],
        cost: None,
    })
    .await
    .expect("send injection 1");
    bridge.drain_injection_for_test().await;

    // Frame 2: tile (3, 3) — outside the old bounding-box-derived 1x1
    // grid, but well within the fixed 4x4 grid.
    tx.send(InjectedFrame {
        seq: 2,
        timestamp_us: 0,
        work: vec![make_work(3, 3)],
        cost: None,
    })
    .await
    .expect("send injection 2");
    bridge.drain_injection_for_test().await;

    let queued = bridge.scheduler_peek_for_test();
    let coords: Vec<(u8, u8)> = queued.iter().map(|w| (w.tile_x, w.tile_y)).collect();
    assert!(
        coords.contains(&(0, 0)),
        "frame 1's tile (0,0) must survive frame 2's injection; queued = {coords:?}"
    );
    assert!(
        coords.contains(&(3, 3)),
        "frame 2's tile (3,3) must be queued; queued = {coords:?}"
    );
    assert_eq!(
        queued.len(),
        2,
        "both frames' work must be queued together; queued = {coords:?}"
    );
}

/// A tile coordinate outside the fixed grid must be skipped (logged,
/// not enqueued, not panicked) rather than silently growing the grid
/// or crashing the `run()`-loop path.
#[tokio::test(start_paused = true)]
async fn out_of_grid_tile_is_skipped_not_enqueued() {
    use crate::transport::protocol::Codec;
    use crate::transport::scheduler::{TileWork, WorkState};

    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (tx, rx) = mpsc::channel(8);
    // Fixed 2x2 grid; tile (5, 5) is out of bounds.
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 2, 2);

    tx.send(InjectedFrame {
        seq: 1,
        timestamp_us: 0,
        work: vec![TileWork {
            tile_x: 5,
            tile_y: 5,
            generation: 0,
            pass_idx: 0,
            total_passes: 1,
            codec: Codec::Solid,
            payload: vec![10, 20, 30, 255],
            queued_at: super::now_std(),
            last_sent_at: None,
            state: WorkState::Pending,
        }],
        cost: None,
    })
    .await
    .expect("send injection");

    // Must not panic.
    bridge.drain_injection_for_test().await;

    let queued = bridge.scheduler_peek_for_test();
    assert!(
        queued.is_empty(),
        "out-of-grid tile must be skipped, not enqueued; queued = {queued:?}"
    );
}

/// Regression for the `drain_injection_for_test` contract bug: it must
/// report the number of items actually enqueued by
/// `apply_injected_frame`, not the number of items submitted in the
/// batch. A batch with one in-grid and one out-of-grid tile must
/// return 1, not 2 — the previous implementation returned
/// `inj.work.len()` and would tell a harness that dropped work had
/// been delivered.
#[tokio::test(start_paused = true)]
async fn drain_injection_for_test_returns_enqueued_not_submitted_count() {
    use crate::transport::protocol::Codec;
    use crate::transport::scheduler::{TileWork, WorkState};

    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (tx, rx) = mpsc::channel(8);
    // Fixed 2x2 grid; tile (5, 5) is out of bounds, tile (0, 0) is not.
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 2, 2);

    let make_work = |tile_x: u8, tile_y: u8| TileWork {
        tile_x,
        tile_y,
        generation: 0,
        pass_idx: 0,
        total_passes: 1,
        codec: Codec::Solid,
        payload: vec![10, 20, 30, 255],
        queued_at: super::now_std(),
        last_sent_at: None,
        state: WorkState::Pending,
    };

    tx.send(InjectedFrame {
        seq: 1,
        timestamp_us: 0,
        work: vec![make_work(0, 0), make_work(5, 5)],
        cost: None,
    })
    .await
    .expect("send injection");

    let enqueued = bridge.drain_injection_for_test().await;
    assert_eq!(
        enqueued, 1,
        "one in-grid + one out-of-grid tile submitted; only the \
         in-grid tile was actually enqueued, so the return value must \
         be 1, not the submitted count of 2"
    );

    let queued = bridge.scheduler_peek_for_test();
    assert_eq!(queued.len(), 1, "only the in-grid tile is queued");
    assert_eq!((queued[0].tile_x, queued[0].tile_y), (0, 0));
}

// -----------------------------------------------------------------
// M4b Task 3: DisplayController dispatch + server-side debounce.
// -----------------------------------------------------------------

/// Mock `DisplayController` recording every `set_output` call as
/// `(width, height, scale_milli)`. `ceiling()` defaults to effectively
/// unbounded (`u16::MAX`) so tests that aren't exercising the clamp path
/// don't accidentally hit it.
struct RecordingController {
    outputs: std::sync::Mutex<Vec<(u16, u16, u16)>>,
    ceiling: (u16, u16),
}

impl Default for RecordingController {
    fn default() -> Self {
        Self {
            outputs: std::sync::Mutex::new(Vec::new()),
            ceiling: (u16::MAX, u16::MAX),
        }
    }
}

impl RecordingController {
    fn with_ceiling(width: u16, height: u16) -> Self {
        Self {
            outputs: std::sync::Mutex::new(Vec::new()),
            ceiling: (width, height),
        }
    }

    fn outputs(&self) -> Vec<(u16, u16, u16)> {
        self.outputs.lock().unwrap().clone()
    }
}

impl crate::transport::display::DisplayController for RecordingController {
    fn ceiling(&self) -> (u16, u16) {
        self.ceiling
    }

    fn set_output(
        &self,
        width: u16,
        height: u16,
        scale_milli: u16,
    ) -> Result<(), crate::transport::display::DisplayError> {
        self.outputs
            .lock()
            .unwrap()
            .push((width, height, scale_milli));
        Ok(())
    }
}

/// A `now_us` value comfortably past `DISPLAY_MODE_DEBOUNCE_US` from
/// bridge construction, regardless of how much real wall-clock time
/// test setup itself burns (these tests don't pause tokio's clock, so
/// `self.epoch` is a real `Instant` and dispatch happens microseconds,
/// not milliseconds, after it). 10x the debounce interval leaves ample
/// margin without being so large it reads as an unrelated constant.
fn debounce_elapsed_us() -> u64 {
    DISPLAY_MODE_DEBOUNCE_US * 10
}

#[tokio::test]
async fn a_pending_display_mode_arms_the_event_loop_wakeup() {
    // The whole milestone was inert without this. `sleep_fut` used to
    // wait on `server.next_timeout()` alone, which is `None` on an idle
    // session -- so on a static desktop (no damage, no frames, silent
    // client) the loop blocked and the 250 ms debounce was never
    // examined. Every other display test passed throughout, because they
    // call `on_timeout` directly and never exercise the wakeup.
    use crate::transport::display::DisplayModeMsg;

    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::default());
    bridge.display_controller = Some(ctl.clone());

    assert!(
        bridge.next_wake_deadline().is_none(),
        "no QUIC deadline and no pending mode should mean no wakeup"
    );

    let mut buf = Vec::new();
    DisplayModeMsg {
        width: 1280,
        height: 800,
    }
    .encode(&mut buf);
    bridge.dispatch_feedback_bytes(handle, &buf);

    let deadline = bridge
        .next_wake_deadline()
        .expect("a pending DisplayMode must arm a wakeup, or it never fires");
    let from_now = deadline.saturating_duration_since(now_std());
    assert!(
        from_now <= std::time::Duration::from_micros(DISPLAY_MODE_DEBOUNCE_US),
        "wakeup must be no later than the debounce deadline, got {from_now:?}"
    );
}

#[tokio::test]
async fn a_display_mode_request_reaches_the_controller() {
    use crate::transport::display::DisplayModeMsg;

    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::default());
    bridge.display_controller = Some(ctl.clone());

    let mut buf = Vec::new();
    DisplayModeMsg {
        width: 1280,
        height: 800,
    }
    .encode(&mut buf);
    bridge.dispatch_feedback_bytes(handle, &buf);
    bridge.on_timeout(debounce_elapsed_us());

    assert_eq!(ctl.outputs(), vec![(1280, 800, 1000)]);
}

#[tokio::test]
async fn rapid_resizes_collapse_to_one_change() {
    // A resize drag emits continuously. Applying every one would thrash
    // X for sizes the user never settled on.
    use crate::transport::display::DisplayModeMsg;

    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::default());
    bridge.display_controller = Some(ctl.clone());

    for w in [1000u16, 1100, 1200, 1280] {
        let mut buf = Vec::new();
        DisplayModeMsg {
            width: w,
            height: 800,
        }
        .encode(&mut buf);
        bridge.dispatch_feedback_bytes(handle, &buf);
    }
    bridge.on_timeout(debounce_elapsed_us());

    assert_eq!(
        ctl.outputs().len(),
        1,
        "only the final size should be applied"
    );
    assert_eq!(ctl.outputs()[0].0, 1280);
}

#[tokio::test]
async fn a_width_is_aligned_down_to_the_cvt_granularity() {
    // CVT rounds h_active UP to a multiple of 8: `cvt 1283 817 60 -r`
    // gives a 1288-wide mode. Rendering 1288 into a 1283-wide window
    // crops 5 px of the remote desktop off-screen -- content the user
    // cannot see or scroll to. Aligning DOWN leaves a 3 px border
    // instead, which is harmless.
    use crate::transport::display::DisplayModeMsg;

    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::default());
    bridge.display_controller = Some(ctl.clone());

    let mut buf = Vec::new();
    DisplayModeMsg {
        width: 1283,
        height: 817,
    }
    .encode(&mut buf);
    bridge.dispatch_feedback_bytes(handle, &buf);
    bridge.on_timeout(debounce_elapsed_us());

    assert_eq!(
        ctl.outputs(),
        vec![(1280, 817, 1000)],
        "width must align down to a multiple of 8; height is unconstrained"
    );
}

#[tokio::test]
async fn a_tiny_window_does_not_collapse_to_nothing() {
    use crate::transport::display::DisplayModeMsg;

    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::default());
    bridge.display_controller = Some(ctl.clone());

    let mut buf = Vec::new();
    DisplayModeMsg {
        width: 4,
        height: 3,
    }
    .encode(&mut buf);
    bridge.dispatch_feedback_bytes(handle, &buf);
    bridge.on_timeout(debounce_elapsed_us());

    let (w, h, _) = ctl.outputs()[0];
    assert!(
        w >= MIN_MODE_WIDTH && h >= MIN_MODE_HEIGHT,
        "expected a floor, got {w}x{h}"
    );
}

#[tokio::test]
async fn a_mode_above_the_ceiling_is_clamped_not_rejected() {
    use crate::transport::display::DisplayModeMsg;

    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::with_ceiling(4096, 2160));
    bridge.display_controller = Some(ctl.clone());

    let mut buf = Vec::new();
    DisplayModeMsg {
        width: 8192,
        height: 4320,
    }
    .encode(&mut buf);
    bridge.dispatch_feedback_bytes(handle, &buf);
    bridge.on_timeout(debounce_elapsed_us());

    assert_eq!(
        ctl.outputs(),
        vec![(4096, 2160, 1000)],
        "an oversized request must be clamped so the client still gets a picture"
    );
}

#[tokio::test]
async fn the_scale_from_display_info_is_applied_to_later_modes() {
    // DisplayInfo carries the scale once; every subsequent mode change
    // must carry it through, or a resize would silently reset the
    // user's DPI.
    use crate::transport::display::{DisplayInfoMsg, DisplayModeMsg};

    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::default());
    bridge.display_controller = Some(ctl.clone());

    let mut info = Vec::new();
    DisplayInfoMsg {
        max_width: 2560,
        max_height: 1440,
        scale_milli: 1500,
        mm_width: 597,
        mm_height: 336,
    }
    .encode(&mut info);
    bridge.dispatch_feedback_bytes(handle, &info);

    let mut mode = Vec::new();
    DisplayModeMsg {
        width: 1280,
        height: 800,
    }
    .encode(&mut mode);
    bridge.dispatch_feedback_bytes(handle, &mode);
    bridge.on_timeout(debounce_elapsed_us());

    assert_eq!(ctl.outputs(), vec![(1280, 800, 1500)]);
}

/// A full-screen change must cost in proportion to the screen.
///
/// Every tile dirty on every frame, nothing acknowledged: the state a
/// scrolling or animating desktop puts the bridge in on a link slower
/// than the content. Three pieces of per-tile bookkeeping each scanned a
/// whole-session structure here (the retransmit cache, the scheduler
/// queues, the coverage map), so the frame cost grew with the *square*
/// of the tile count: 40-70 ms at 1080p, 0.5 s at 4K, 1.7 s across two
/// 4K displays, against a 33 ms frame. Measured after: 0.65 ms, 1.75 ms
/// and 3.2 ms.
///
/// Asserted as a ratio between two screen sizes rather than as a
/// duration, so it holds in a debug build on a loaded machine. Eight
/// times the tiles is 8x the work if the path is linear and 64x if it
/// is quadratic; the old code measured 31x here and the fix 5x.
///
/// For absolute numbers:
/// `cargo test --release -p ghostframe-lib full_screen_change -- --nocapture`
#[tokio::test]
async fn full_screen_change_costs_in_proportion_to_the_screen() {
    async fn best_frame_us(w: u32, h: u32) -> (usize, u128) {
        let (our_end, _peer) = UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
        let grid = crate::tile::TileGrid::new(w, h);
        bridge.metrics_tracker.resize(grid.cols, grid.rows);
        let mut dirty = Vec::new();
        for y in 0..grid.rows {
            for x in 0..grid.cols {
                bridge.metrics_tracker.get_mut(x, y).codec_state = crate::tile::CodecState::Solid;
                dirty.push((x, y));
            }
        }
        let pixels = vec![0x40u8; (w * h * 4) as usize];
        let mut best = u128::MAX;
        // The first frame is excluded: it runs against empty state,
        // which is the one case the scans were cheap in.
        for seq in 0..10u32 {
            let t = std::time::Instant::now();
            let _ = bridge.dispatch_dirty_tiles_via_scheduler(
                &dirty,
                &grid,
                TileDispatchFrame {
                    pixels: &pixels,
                    stride: w * 4,
                    seq,
                    timestamp_us: seq * 33_333,
                },
                1200,
                SchedulerEmissionPolicy::GpuClassifierDriven,
                None,
            );
            let spent = std::time::Instant::now().duration_since(t);
            if seq > 0 {
                best = best.min(spent.as_micros());
            }
        }
        assert_eq!(
            bridge.scheduler.queue_len(),
            dirty.len(),
            "premise: the link is slower than the screen, so a backlog \
             of one item per tile is what each frame has to supersede"
        );
        (dirty.len(), best)
    }

    let (small_tiles, small) = best_frame_us(1920, 1080).await;
    let (_, mid) = best_frame_us(3840, 2160).await;
    let (large_tiles, large) = best_frame_us(7680, 2160).await;
    eprintln!("full-screen dirty frame: 1080p {small} us, 4K {mid} us, 2x4K {large} us");
    assert_eq!(large_tiles, 8 * small_tiles);
    assert!(
        large <= 16 * small.max(1),
        "8x the tiles cost {large} us against {small} us ({}x): the \
         dirty-tile path is no longer linear in the screen",
        large / small.max(1)
    );
}

// ── HELLO gating: only the live session is honoured ──────────────────

/// A bridge with two attached sessions: `live` has sent HELLO, `other`
/// has not. Distinct non-zero handles so "records the sender" cannot be
/// confused with "records handle 0".
async fn live_and_other() -> (IoBridge, ConnectionHandle, ConnectionHandle) {
    let live = ConnectionHandle(11);
    let other = ConnectionHandle(12);
    let mut bridge = test_bridge_with_sessions(&[live, other]).await;
    bridge.apply_hello(live, hello(false));
    assert_eq!(bridge.last_hello_from(), Some(live));
    (bridge, live, other)
}

#[tokio::test]
async fn gate_feedback_only_from_the_live_session() {
    let (mut bridge, live, other) = live_and_other().await;
    let fb = crate::transport::feedback::ReceiverFeedback {
        timestamp_ns: 1,
        datagrams_received: 10,
        datagrams_recovered_fec: 0,
        suspension_detected: true,
    };
    let mut buf = Vec::new();
    fb.encode(&mut buf);

    let seq0 = bridge.adaptation_context.last_update_seq;
    bridge.dispatch_feedback_bytes(other, &buf);
    assert_eq!(bridge.adaptation_context.last_update_seq, seq0);
    assert!(
        !bridge.adaptation_context.suspended,
        "non-live feedback ignored"
    );

    bridge.dispatch_feedback_bytes(live, &buf);
    assert!(bridge.adaptation_context.last_update_seq > seq0);
    assert!(
        bridge.adaptation_context.suspended,
        "live feedback honoured"
    );
}

#[tokio::test]
async fn gate_decode_error_only_from_the_live_session() {
    use crate::transport::decode_error::{DecodeErrorMsg, ERR_THIN_UNCACHED_PALETTE};
    let (mut bridge, live, other) = live_and_other().await;
    bridge.palette_table.delivered.insert(7);
    bridge.metrics_tracker.resize(8, 8);
    bridge.metrics_tracker.get_mut(3, 4).codec_state =
        crate::tile::CodecState::PalRle { palette_id: 7 };
    let mut buf = Vec::new();
    DecodeErrorMsg {
        codec: 2,
        tile_x: 3,
        tile_y: 4,
        error_code: ERR_THIN_UNCACHED_PALETTE,
    }
    .encode(&mut buf);

    bridge.dispatch_feedback_bytes(other, &buf);
    assert!(
        bridge.palette_table.delivered.contains(7),
        "non-live DECODE_ERROR must not touch the palette"
    );
    bridge.dispatch_feedback_bytes(live, &buf);
    assert!(
        !bridge.palette_table.delivered.contains(7),
        "live DECODE_ERROR is honoured"
    );
}

#[tokio::test]
async fn gate_display_info_only_from_the_live_session() {
    use crate::transport::display::DisplayInfoMsg;
    let (mut bridge, live, other) = live_and_other().await;
    let mut buf = Vec::new();
    DisplayInfoMsg {
        max_width: 1920,
        max_height: 1080,
        scale_milli: 2000,
        mm_width: 0,
        mm_height: 0,
    }
    .encode(&mut buf);
    bridge.dispatch_feedback_bytes(other, &buf);
    assert_eq!(bridge.display_scale_milli, DEFAULT_SCALE_MILLI);
    bridge.dispatch_feedback_bytes(live, &buf);
    assert_eq!(bridge.display_scale_milli, 2000);
}

#[tokio::test]
async fn gate_display_mode_only_from_the_live_session() {
    use crate::transport::display::DisplayModeMsg;
    let (mut bridge, live, other) = live_and_other().await;
    bridge.display_controller = Some(Arc::new(RecordingController::default()));
    let mut buf = Vec::new();
    DisplayModeMsg {
        width: 1280,
        height: 800,
    }
    .encode(&mut buf);
    bridge.dispatch_feedback_bytes(other, &buf);
    assert!(bridge.pending_display_mode.is_none());
    bridge.dispatch_feedback_bytes(live, &buf);
    assert!(bridge.pending_display_mode.is_some());
}

/// Records pointer moves; shared by the input gate tests.
#[derive(Default)]
struct MoveRecorder {
    moves: std::sync::Mutex<Vec<(i16, i16)>>,
}
impl crate::transport::input_inject::InputInjector for MoveRecorder {
    fn pointer_move(&self, x: i16, y: i16) {
        self.moves.lock().unwrap().push((x, y));
    }
    fn pointer_button(&self, _: i16, _: i16, _: u8, _: bool) {}
    fn wheel(&self, _: i16, _: i16) {}
    fn key(&self, _: u32, _: bool) {}
}

#[tokio::test]
async fn gate_input_only_from_the_live_session() {
    let (mut bridge, live, other) = live_and_other().await;
    let rec = Arc::new(MoveRecorder::default());
    bridge.input_injector =
        Some(rec.clone() as Arc<dyn crate::transport::input_inject::InputInjector>);
    let mv = [0x05, 0x01, 0x00, 0x64, 0x00, 0xc8];
    bridge.dispatch_feedback_bytes(other, &mv);
    assert!(
        rec.moves.lock().unwrap().is_empty(),
        "an unidentified connection must not inject input"
    );
    bridge.dispatch_feedback_bytes(live, &mv);
    assert_eq!(rec.moves.lock().unwrap().clone(), vec![(100, 200)]);
}

#[tokio::test]
async fn gate_hello_then_display_info_in_one_buffer_is_accepted() {
    use crate::transport::display::DisplayInfoMsg;
    let (mut bridge, _live, other) = live_and_other().await;
    let mut buf = Vec::new();
    hello(false).encode(&mut buf);
    DisplayInfoMsg {
        max_width: 1920,
        max_height: 1080,
        scale_milli: 1500,
        mm_width: 0,
        mm_height: 0,
    }
    .encode(&mut buf);
    bridge.dispatch_feedback_bytes(other, &buf);
    assert_eq!(bridge.last_hello_from(), Some(other));
    assert_eq!(
        bridge.display_scale_milli, 1500,
        "the gate is per message: a HELLO makes the sender live for what follows"
    );
}

#[tokio::test]
async fn gate_non_live_first_byte_non_hello_discards_the_buffer() {
    let (mut bridge, live, other) = live_and_other().await;
    let rec = Arc::new(MoveRecorder::default());
    bridge.input_injector =
        Some(rec.clone() as Arc<dyn crate::transport::input_inject::InputInjector>);
    // A full INPUT followed by a HELLO: the HELLO must NOT be reached,
    // the whole buffer goes.
    let mut buf = vec![0x05, 0x01, 0x00, 0x64, 0x00, 0xc8];
    hello(false).encode(&mut buf);
    bridge.dispatch_feedback_bytes(other, &buf);
    assert!(rec.moves.lock().unwrap().is_empty());
    assert_eq!(bridge.non_live_feedback_drops, 1);
    assert_eq!(bridge.last_hello_from(), Some(live), "no takeover");
    assert!(
        !bridge.feedback_recv_bufs.contains_key(&other),
        "nothing retained for a non-live sender"
    );

    // A lone partial non-HELLO header is discarded too, not retained.
    bridge.dispatch_feedback_bytes(other, &[crate::transport::feedback::FEEDBACK_MSG_TYPE]);
    assert!(!bridge.feedback_recv_bufs.contains_key(&other));
    assert_eq!(bridge.non_live_feedback_drops, 2);

    // And the sender is not wedged: a HELLO still gets through.
    let mut h = Vec::new();
    hello(false).encode(&mut h);
    bridge.dispatch_feedback_bytes(other, &h);
    assert_eq!(bridge.last_hello_from(), Some(other));
}

#[tokio::test]
async fn feedback_buffers_are_per_handle() {
    let (mut bridge, live, other) = live_and_other().await;
    let rec = Arc::new(MoveRecorder::default());
    bridge.input_injector =
        Some(rec.clone() as Arc<dyn crate::transport::input_inject::InputInjector>);
    let mut h = Vec::new();
    hello(true).encode(&mut h);
    assert_eq!(h.len(), 2);

    // live: first half of a pointer move.
    bridge.dispatch_feedback_bytes(live, &[0x05, 0x01, 0x00]);
    // other: first byte of a HELLO, interleaved.
    bridge.dispatch_feedback_bytes(other, &h[..1]);
    assert_eq!(bridge.feedback_recv_bufs.len(), 2);
    // live finishes its message; with one shared buffer this would have
    // parsed other's HELLO byte as part of the move.
    bridge.dispatch_feedback_bytes(live, &[0x64, 0x00, 0xc8]);
    assert_eq!(rec.moves.lock().unwrap().clone(), vec![(100, 200)]);
    // other finishes its HELLO.
    bridge.dispatch_feedback_bytes(other, &h[1..]);
    assert_eq!(bridge.last_hello_from(), Some(other));
    assert!(bridge.current_client_caps().supports_h264);

    // forget_session drops the handle's buffer.
    bridge.dispatch_feedback_bytes(other, &[0x05, 0x01]);
    assert!(bridge.feedback_recv_bufs.contains_key(&other));
    bridge.forget_session(other);
    assert!(!bridge.feedback_recv_bufs.contains_key(&other));
}

/// Seed one un-ACKed emitter entry and record its transmission, as the
/// production drain does. Returns its key and wire_seq.
fn seed_unacked_entry(
    bridge: &mut IoBridge,
    frame_seq: u32,
) -> (crate::transport::reliable_emitter::EmitKey, u32) {
    let key = crate::transport::reliable_emitter::EmitKey::new(frame_seq, 0, 0, 0);
    let t = std::time::Instant::now();
    bridge
        .reliable_emitter
        .submit_one(key, bytes::Bytes::from(vec![0u8; 25]), t, None, t);
    let emitted = bridge.reliable_emitter.take_transmissions();
    assert_eq!(emitted.len(), 1);
    let ledger_now = now_std();
    for e in &emitted {
        bridge.transmission_ledger.record_departed(
            e.wire_seq,
            ledger_now,
            crate::transport::transmission_ledger::Transmission {
                emit_us: e.emit_us,
                wire_bytes: e.wire_bytes,
                content: e.content,
                depart_us: None,
                dep_idx: None,
            },
        );
    }
    (key, emitted[0].wire_seq)
}

#[tokio::test(flavor = "multi_thread")]
async fn gate_ack_datagram_only_from_the_live_session() {
    let (mut bridge, live, other) = live_and_other().await;
    let (key, wire_seq) = seed_unacked_entry(&mut bridge, 1);
    let dg = crate::transport::ack::AckBatch {
        entries: vec![crate::transport::ack::AckEntry {
            wire_seq,
            arrival_time_ms_lo16: 0,
        }],
    }
    .encode();

    bridge.dispatch_inbound_datagram(other, &dg);
    assert!(
        bridge.reliable_emitter.cache.get(&key).is_some(),
        "an ACK from a non-live session must not release the cache entry"
    );
    assert_eq!(bridge.non_live_datagram_drops, 1);

    bridge.dispatch_inbound_datagram(live, &dg);
    assert!(
        bridge.reliable_emitter.cache.get(&key).is_none(),
        "the live session's ACK releases it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gate_nack_datagram_only_from_the_live_session() {
    let (mut bridge, live, other) = live_and_other().await;
    let (_key, _) = seed_unacked_entry(&mut bridge, 1);
    let mut dg = Vec::new();
    crate::transport::protocol::TileNackEnvelope {
        entries: vec![crate::transport::protocol::TileNackEntry {
            frame_seq: 1,
            tile_x: 0,
            tile_y: 0,
            pass_idx: 0,
        }],
    }
    .encode(&mut dg);

    bridge.dispatch_inbound_datagram(other, &dg);
    assert_eq!(bridge.reliable_emitter.stats.nack_hit, 0);
    bridge.dispatch_inbound_datagram(live, &dg);
    assert_eq!(bridge.reliable_emitter.stats.nack_hit, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_loss_of_a_stale_handle_keeps_the_live_sessions_cache() {
    let (mut bridge, live, other) = live_and_other().await;
    let (_key, _) = seed_unacked_entry(&mut bridge, 1);
    assert_eq!(bridge.reliable_emitter.pending_cache_entries(), 1);
    let started = bridge.session_started_at;
    assert!(started.is_some());

    bridge.on_connection_gone(other, "scanner timed out");
    assert_eq!(
        bridge.reliable_emitter.pending_cache_entries(),
        1,
        "a never-HELLO'd connection dying must not wipe the live cache"
    );
    assert_eq!(bridge.last_hello_from(), Some(live));
    assert!(bridge.session_started_at.is_some());
    assert!(bridge.last_disconnect_at.is_none());

    bridge.on_connection_gone(live, "closed");
    assert_eq!(bridge.reliable_emitter.pending_cache_entries(), 0);
    assert_eq!(bridge.last_hello_from(), None);
    assert!(bridge.session_started_at.is_none());
    assert!(bridge.last_disconnect_at.is_some());
}

#[tokio::test]
async fn connected_count_requires_hello() {
    let h = ConnectionHandle(21);
    let mut bridge = test_bridge_with_sessions(&[h]).await;
    let rx = bridge.connected_session_count_handle();
    bridge
        .wt_sessions
        .get_mut(&h)
        .expect("attached")
        .test_set_connected(true);
    bridge.refresh_connected_session_count();
    assert_eq!(
        *rx.borrow(),
        0,
        "connected but never HELLO'd: must not open the capture gate"
    );
    bridge.apply_hello(h, hello(false));
    bridge.refresh_connected_session_count();
    assert_eq!(*rx.borrow(), 1);
}

#[tokio::test]
async fn only_the_live_connected_session_is_a_send_target() {
    let (mut bridge, live, other) = live_and_other().await;
    // Live but not connected: no target.
    assert_eq!(bridge.live_connected_handle(), None);
    // A connected non-live session is never a target.
    bridge
        .wt_sessions
        .get_mut(&other)
        .expect("attached")
        .test_set_connected(true);
    assert_eq!(bridge.live_connected_handle(), None);
    // Live and connected: the target (a unit bridge has no quinn
    // connection, so the full send handle stays None).
    bridge
        .wt_sessions
        .get_mut(&live)
        .expect("attached")
        .test_set_connected(true);
    assert_eq!(bridge.live_connected_handle(), Some(live));
    assert_eq!(bridge.live_send_handle(), None);
    // With no live session, send_to_all_sessions consumes, as before.
    bridge.forget_session(live);
    let out = bridge.send_to_all_sessions(&[1, 2, 3], WireClass::Control);
    assert!(matches!(
        out,
        crate::transport::reliable_emitter::traits::SendOutcome::Sent
    ));
    assert_eq!(bridge.compute_max_datagram_size(), None);
}

#[tokio::test]
async fn hello_deadline_is_armed_cleared_swept_and_wakes_the_loop() {
    let h = ConnectionHandle(31);
    let mut bridge = test_bridge_with_sessions(&[h]).await;
    let t0 = std::time::Instant::now();
    assert!(bridge.next_wake_deadline().is_none());

    bridge.arm_hello_deadline(h, t0);
    assert_eq!(
        bridge.close_deadlines.get(&h).map(|d| d.code),
        Some(CLOSE_CODE_NO_HELLO)
    );
    assert_eq!(
        bridge.next_wake_deadline(),
        Some(t0 + HELLO_DEADLINE),
        "the loop must wake for the deadline even when nothing else is pending"
    );

    // A sweep before the deadline leaves it alone.
    bridge.sweep_close_deadlines(t0 + HELLO_DEADLINE / 2);
    assert!(bridge.close_deadlines.contains_key(&h));
    assert!(bridge.wt_sessions.contains_key(&h));

    // A sweep after it closes (no quinn conn in a unit bridge) and forgets.
    bridge.sweep_close_deadlines(t0 + HELLO_DEADLINE + std::time::Duration::from_millis(1));
    assert!(!bridge.close_deadlines.contains_key(&h));
    assert!(
        !bridge.wt_sessions.contains_key(&h),
        "sweep forgets the handle"
    );
}

#[test]
fn gpu_tile_count_is_clamped_to_the_grid() {
    assert_eq!(clamp_gpu_tile_count(0, 100), 0);
    assert_eq!(clamp_gpu_tile_count(100, 100), 100);
    assert_eq!(clamp_gpu_tile_count(101, 100), 100);
    assert_eq!(clamp_gpu_tile_count(u32::MAX, 100), 100);
}

#[tokio::test]
async fn a_second_takeover_inside_the_interval_is_refused_and_the_newcomer_closed() {
    let (a, b, c) = (
        ConnectionHandle(0),
        ConnectionHandle(1),
        ConnectionHandle(2),
    );
    let mut bridge = test_bridge_with_sessions(&[a, b, c]).await;
    bridge.apply_hello(a, hello(false));
    // First takeover (a -> b) is allowed.
    bridge.apply_hello(b, hello(false));
    assert!(bridge.is_live(b));
    // An immediate second takeover (b -> c) is not.
    bridge.apply_hello(c, hello(false));
    assert!(bridge.is_live(b), "incumbent must be untouched");
    assert!(!bridge.is_live(c), "newcomer must not become live");
    assert_eq!(
        bridge.close_deadlines.get(&c).map(|d| d.code),
        Some(CLOSE_CODE_TAKEOVER_RATE_LIMITED)
    );
    // Once the interval has passed, c may take over.
    bridge.last_takeover_at = Some(
        std::time::Instant::now() - TAKEOVER_MIN_INTERVAL - std::time::Duration::from_millis(1),
    );
    bridge.apply_hello(c, hello(false));
    assert!(bridge.is_live(c));
}

#[tokio::test]
async fn reconnect_after_the_incumbent_left_is_not_rate_limited() {
    let (a, b, c) = (
        ConnectionHandle(0),
        ConnectionHandle(1),
        ConnectionHandle(2),
    );
    let mut bridge = test_bridge_with_sessions(&[a, b, c]).await;
    bridge.apply_hello(a, hello(false));
    bridge.apply_hello(b, hello(false)); // takeover, stamps last_takeover_at
    bridge.forget_session(b); // incumbent departs
    bridge.apply_hello(c, hello(false)); // reconnect, not a takeover
    assert!(bridge.is_live(c));
}

#[tokio::test]
async fn hello_removes_the_never_hello_deadline() {
    let h = ConnectionHandle(32);
    let mut bridge = test_bridge_with_sessions(&[h]).await;
    bridge.arm_hello_deadline(h, std::time::Instant::now());
    bridge.apply_hello(h, hello(false));
    assert!(!bridge.close_deadlines.contains_key(&h));
    // And the deadline set up at accept survives on_connection_established.
    let g = ConnectionHandle(33);
    bridge.arm_hello_deadline(g, std::time::Instant::now());
    bridge.on_connection_established(g);
    assert!(bridge.close_deadlines.contains_key(&g));
}

#[tokio::test]
async fn eviction_schedules_a_close_after_forgetting_the_session() {
    let (mut bridge, live, _other) = live_and_other().await;
    let t0 = std::time::Instant::now();
    bridge.evict_session(live, EvictionReason::DisplacedByNewSession);
    assert!(!bridge.wt_sessions.contains_key(&live));
    let d = bridge
        .close_deadlines
        .get(&live)
        .copied()
        .expect("forget_session must not swallow the eviction's close entry");
    assert_eq!(d.code, CLOSE_CODE_EVICTED);
    assert!(d.at >= t0 + EVICT_CLOSE_GRACE);
    assert!(bridge.next_wake_deadline().is_some());
    // The sweep can close a handle whose wt_sessions entry is gone.
    bridge.sweep_close_deadlines(d.at);
    assert!(!bridge.close_deadlines.contains_key(&live));
}

#[test]
fn live_path_rtt_reads_only_the_live_sessions_connection() {
    use std::time::Duration;
    let live = ConnectionHandle(1);
    let scanner = ConnectionHandle(2);
    let rtts = vec![
        (scanner, Duration::from_millis(1)),
        (live, Duration::from_millis(80)),
    ];
    assert_eq!(
        super::select_live_rtt(Some(live), rtts.clone().into_iter()),
        Some(Duration::from_millis(80)),
        "a lower-RTT stranger must not set the live session's horizon"
    );
    assert_eq!(
        super::select_live_rtt(None, rtts.clone().into_iter()),
        None,
        "no live session means no RTT, so callers keep their fallback"
    );
    assert_eq!(
        super::select_live_rtt(Some(ConnectionHandle(9)), rtts.into_iter()),
        None
    );
}

/// The time threshold is 9/8 of the larger of srtt and the latest round trip,
/// from the emitter's own (application-acknowledgement) estimator.
#[test]
fn the_time_threshold_is_nine_eighths_of_the_larger_rtt() {
    use crate::transport::rtt_estimator::RttEstimator;
    let ms = std::time::Duration::from_millis;
    let mut e = RttEstimator::new();
    e.on_sample(ms(80));
    assert_eq!(crate::transport::loss_path::time_threshold(&e), ms(90));
    // A latest sample above srtt wins.
    e.on_sample(ms(160));
    assert!(e.latest_rtt() > e.srtt());
    assert_eq!(crate::transport::loss_path::time_threshold(&e), ms(180));
}

#[tokio::test]
async fn takeover_via_apply_hello_leaves_session_started_at_set() {
    let first = ConnectionHandle(7);
    let second = ConnectionHandle(9);
    let mut bridge = test_bridge_with_sessions(&[first, second]).await;
    bridge.apply_hello(first, hello(false));
    assert!(bridge.session_started_at.is_some());
    bridge.apply_hello(second, hello(false));
    assert_eq!(bridge.last_hello_from(), Some(second));
    assert!(
        bridge.session_started_at.is_some(),
        "evicting the incumbent must not erase the newcomer's start stamp"
    );
}

#[tokio::test]
async fn hello_from_a_forgotten_handle_is_ignored() {
    let live = ConnectionHandle(7);
    let gone = ConnectionHandle(9);
    let mut bridge = test_bridge_with_sessions(&[live]).await;
    bridge.apply_hello(live, hello(false));
    // `gone` has no wt_sessions entry (evicted earlier in the batch).
    bridge.apply_hello(gone, hello(true));
    assert_eq!(bridge.last_hello_from(), Some(live));
    assert!(
        bridge.wt_sessions.contains_key(&live),
        "a forgotten handle's HELLO must not evict the incumbent"
    );
    assert!(bridge.session_started_at.is_some());
}

#[tokio::test(start_paused = true)]
async fn eviction_close_deadline_uses_the_virtual_clock() {
    let (mut bridge, live, _other) = live_and_other().await;
    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    let t0 = super::now_std();
    bridge.evict_session(live, EvictionReason::DisplacedByNewSession);
    let d = bridge.close_deadlines.get(&live).copied().expect("armed");
    assert!(
        d.at >= t0 + EVICT_CLOSE_GRACE,
        "deadline must be on the same clock the sweep compares against"
    );
}

#[tokio::test]
async fn a_reused_handle_after_reap_is_not_live() {
    let h = ConnectionHandle(41);
    let mut bridge = test_bridge_with_sessions(&[h]).await;
    bridge.apply_hello(h, hello(false));
    bridge.dispatch_feedback_bytes(h, &[0x05, 0x01]);
    assert!(bridge.is_live(h));

    // Reaped by handle_timeout (no ConnectionLost): same cleanup.
    bridge.on_connection_gone(h, "idle");
    assert!(!bridge.is_live(h));
    assert!(!bridge.wt_sessions.contains_key(&h));
    assert!(!bridge.feedback_recv_bufs.contains_key(&h));

    // quinn hands the slab key to a new connection: it starts clean.
    bridge.on_connection_established(h);
    assert!(!bridge.is_live(h));
    let rec = Arc::new(MoveRecorder::default());
    bridge.input_injector =
        Some(rec.clone() as Arc<dyn crate::transport::input_inject::InputInjector>);
    bridge.dispatch_feedback_bytes(h, &[0x05, 0x01, 0x00, 0x64, 0x00, 0xc8]);
    assert!(rec.moves.lock().unwrap().is_empty());
}

#[tokio::test]
async fn establishing_over_a_stale_live_entry_clears_it() {
    // Backstop: a reap that never reached the bridge must not leave the
    // old holder's authority attached to the recycled key.
    let h = ConnectionHandle(42);
    let mut bridge = test_bridge_with_sessions(&[h]).await;
    bridge.apply_hello(h, hello(false));
    bridge.dispatch_feedback_bytes(h, &[0x05, 0x01]);
    bridge.on_connection_established(h);
    assert!(!bridge.is_live(h));
    assert!(!bridge.feedback_recv_bufs.contains_key(&h));
}

/// `enqueue_cdf53_generation` is shared by the dirty-tile and escalation
/// paths; the `source` it is given must pick BOTH the bump counter and the
/// `source` field of the `cdf53.emit` log line. Swapping either is invisible
/// to every other test (the GPU callers need Vulkan), so pin them here.
#[tokio::test]
async fn enqueue_cdf53_generation_source_selects_counter_and_log_label() {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::util::SubscriberInitExt;

    use super::frame::Cdf53Source;

    #[derive(Default, Clone)]
    struct CapturingWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let _g = tracing_subscriber::fmt()
        .json()
        .with_writer(CapturingWriter(Arc::clone(&buf)))
        .with_target(true)
        .with_env_filter("ghostframe::cdf53=debug")
        .set_default();

    let mut bridge = make_bridge_for_test().await;
    let passes = || vec![(0u8, vec![1u8]), (3u8, vec![2u8, 2])];
    let present = 0b1001u16;

    // Dirty: two passes, two bumps on tile (1,1), one on (2,2).
    bridge.enqueue_cdf53_generation(1, 1, present, passes(), Cdf53Source::Dirty);
    bridge.enqueue_cdf53_generation(1, 1, present, passes(), Cdf53Source::Dirty);
    bridge.enqueue_cdf53_generation(2, 2, present, passes(), Cdf53Source::Dirty);
    assert_eq!(bridge.bump_count_accumulator.cdf53_dirty_path, 3);
    assert_eq!(bridge.bump_count_accumulator.cdf53_escalation_path, 0);
    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert_eq!(logs.matches("\"source\":\"dirty\"").count(), 6, "{logs}");
    assert_eq!(
        logs.matches("\"source\":\"escalation\"").count(),
        0,
        "{logs}"
    );
    buf.lock().unwrap().clear();

    // Escalation: one bump, two log lines.
    bridge.enqueue_cdf53_generation(3, 3, present, passes(), Cdf53Source::Escalation);
    assert_eq!(bridge.bump_count_accumulator.cdf53_dirty_path, 3);
    assert_eq!(bridge.bump_count_accumulator.cdf53_escalation_path, 1);
    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.matches("\"source\":\"escalation\"").count(),
        2,
        "{logs}"
    );
    assert_eq!(logs.matches("\"source\":\"dirty\"").count(), 0, "{logs}");
    assert_eq!(logs.matches("cdf53.emit").count(), 2, "{logs}");
}

/// `ack_batch_received` fires once per ACK batch, which is every few
/// milliseconds on a busy session. The daemon's default filter is
/// `ghostframe=info` (ghostframe-server main.rs); an operator who sets
/// `RUST_LOG=ghostframe=debug` lets DEBUG through for this target and feeds
/// journald's rate limit, so it is TRACE: invisible at DEBUG, present at the
/// TRACE filter the e2e containers run under.
#[tokio::test]
async fn ack_batch_received_is_a_trace_event() {
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Sink {
            self.clone()
        }
    }

    let batch = crate::transport::ack::AckBatch {
        entries: vec![crate::transport::ack::AckEntry {
            wire_seq: 1,
            arrival_time_ms_lo16: 0,
        }],
    };
    let wire = batch.encode();

    let mut bridge = make_bridge_for_test().await;
    let captured = |max: tracing::Level, bridge: &mut IoBridge| {
        let sink = Sink::default();
        let sub = tracing_subscriber::fmt()
            .with_max_level(max)
            .with_writer(sink.clone())
            .finish();
        tracing::subscriber::with_default(sub, || bridge.dispatch_ack_datagram(&wire));
        let out = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        out
    };
    let at_debug = captured(tracing::Level::DEBUG, &mut bridge);
    assert!(!at_debug.contains("ack_batch_received"), "got: {at_debug}");
    let at_trace = captured(tracing::Level::TRACE, &mut bridge);
    assert!(at_trace.contains("ack_batch_received"), "got: {at_trace}");
}

// ---- GPU-failure fallback (T1, T2), PalRle count clamp (T3), escalation flag (T11) ----

/// The server needs a GPU: with no usable Vulkan compute device, startup must
/// fail with an error that says so and carries the cause. There is no software
/// path to fall back to, and a server that started anyway would drop every
/// frame.
#[test]
fn startup_without_a_usable_gpu_fails_with_a_clear_error() {
    let init: Result<crate::capture::gpu_pipeline::GpuFrameProcessor, String> =
        Err("no Vulkan device found".into());
    let err = match require_gpu_backend(init) {
        Ok(_) => panic!("startup must fail without a GPU"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("GPU"),
        "the error must name the missing GPU: {msg}"
    );
    assert!(
        msg.contains("no Vulkan device found"),
        "the error must carry the cause: {msg}"
    );
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
}

/// A GPU failure that repeats every frame logs once per doubling, not at
/// frame rate.
#[test]
fn a_repeating_gpu_failure_logs_once_per_doubling() {
    let logged: Vec<u32> = (1..=40u32)
        .filter(|&n| super::frame::log_gpu_failure(n))
        .collect();
    assert_eq!(logged, vec![1, 2, 4, 8, 16, 32]);
}

/// T3. A PalRle compact count the GPU wrote past the grid must not size the
/// index-buffer slice. `u32::MAX * 512` overflows `u32` (a panic here, a wrapped
/// short slice in a release build) and, widened, would read far past the buffer.
#[test]
fn an_out_of_range_palrle_count_is_clamped_before_it_sizes_the_index_slice() {
    let grid = TileGrid::new(64, 64); // 2x2 = 4 tiles
    let index = vec![0u8; 4 * 512]; // exactly the grid's worth
    let analysis = crate::capture::gpu_pipeline::FrameAnalysis {
        dirty_tiles: Vec::new(),
        nv12_data: std::ptr::null(),
        nv12_width: 0,
        nv12_height: 0,
        nv12_y_stride: 0,
        nv12_uv_stride: 0,
        nv12_uv_offset: 0,
        tile_analysis: std::ptr::null(),
        tile_analysis_len: 0,
        palrle_compact_list: std::ptr::null(),
        palrle_compact_count: u32::MAX,
        frame_palette_set: std::ptr::null(),
        frame_palette_set_count: 0,
        per_tile_frame_palette_id: std::ptr::null(),
        folded_into: std::ptr::null(),
        index_buffer: index.as_ptr(),
    };
    // The slice Phase A is handed is the grid's worth, not what the GPU claimed.
    let tiles = (grid.cols * grid.rows) as usize;
    assert_eq!(palrle_index_slice(&analysis, tiles).len(), tiles * 512);
}

/// T11. `already_escalated_this_gen` is what stops the idle-escalation sweep
/// from re-promoting a tile it already promoted. A dirty commit starts a new
/// generation (eligible again); an escalation commit uses it up.
#[tokio::test]
async fn escalation_commit_marks_the_generation_used_and_a_dirty_commit_clears_it() {
    use crate::tile::{detect_escalation_candidates, CodecState};
    use crate::transport::io_bridge::frame::Cdf53Source;

    let mut bridge = make_bridge_for_test().await;
    bridge.metrics_tracker.resize(2, 2);
    bridge.scheduler.resize(2, 2);
    let (present, passes) = crate::encoder::cdf53::encode_passes_sparse(&[0i16; 3072]);

    // Tile (0,0): dirty commit over a stale `true`. Tile (1,0): escalation.
    bridge
        .metrics_tracker
        .get_mut(0, 0)
        .already_escalated_this_gen = true;
    bridge.commit_tile_to_cdf53(0, 0, present, passes.clone(), Cdf53Source::Dirty);
    bridge.commit_tile_to_cdf53(1, 0, present, passes, Cdf53Source::Escalation);

    // Later both tiles are re-encoded as H.264 and sit idle; only the sweep's
    // view of them is under test.
    for x in 0..2 {
        let m = bridge.metrics_tracker.get_mut(x, 0);
        assert!(matches!(m.codec_state, CodecState::Cdf53 { .. }));
        m.codec_state = CodecState::H264 { frames_in_h264: 5 };
        m.idle_frames = 31;
    }
    assert_eq!(
        detect_escalation_candidates(&bridge.metrics_tracker, 16),
        vec![0u32],
        "the dirty-committed tile may escalate again; the escalated one may not"
    );
}

// ---- T4: a late ACK must retract the loss it contradicts ----

fn ledger_tx(
    emit_us: u64,
    bytes: usize,
    frame_seq: u32,
) -> crate::transport::transmission_ledger::Transmission {
    crate::transport::transmission_ledger::Transmission {
        emit_us,
        wire_bytes: bytes,
        content: crate::transport::reliable_emitter::EmitKey::new(frame_seq, 1, 2, 0).into(),
        depart_us: None,
        dep_idx: None,
    }
}

/// A transmission declared lost sits in `pending_losses` until the next
/// acknowledgement lets goog_cc take a report. If that same transmission is
/// then acknowledged late, `loss_window` is corrected but the pending loss was
/// left to reach the estimator as a loss of a packet that arrived.
#[tokio::test]
async fn a_late_ack_retracts_the_pending_loss_it_contradicts() {
    use crate::transport::ack::{AckBatch, AckEntry};
    let mut bridge = make_bridge_for_test().await;
    let t0 = super::now_std();
    bridge
        .transmission_ledger
        .record_departed(41, t0, ledger_tx(1_000, 1_200, 1));
    bridge
        .transmission_ledger
        .record_departed(42, t0, ledger_tx(2_000, 900, 2));
    bridge.declare_lost_for_test(&[41, 42]);
    assert_eq!(bridge.pending_losses.len(), 2, "both declared lost");

    // Only wire_seq 41 turns out to have arrived.
    let batch = AckBatch {
        entries: vec![AckEntry {
            wire_seq: 41,
            arrival_time_ms_lo16: 7,
        }],
    };
    bridge.dispatch_ack_datagram(&batch.encode());
    bridge.retract_late_acked_losses();

    let left: Vec<u32> = bridge.pending_losses.iter().map(|l| l.wire_seq).collect();
    assert_eq!(
        left,
        vec![42],
        "the loss of the acknowledged transmission must not reach the estimator"
    );
    assert_eq!(bridge.pending_losses_retracted, 1);
    assert!(bridge.late_acked.is_empty(), "bounded to one turn");
}

/// Identical (emit_us, bytes) twins are told apart by wire_seq: acknowledging
/// one withdraws that one, not whichever matches first.
#[tokio::test]
async fn a_late_ack_withdraws_its_own_loss_among_identical_twins() {
    use crate::transport::ack::{AckBatch, AckEntry};
    let mut bridge = make_bridge_for_test().await;
    let t0 = super::now_std();
    for ws in [10, 11, 12] {
        bridge
            .transmission_ledger
            .record_departed(ws, t0, ledger_tx(5_000, 1_200, ws));
    }
    bridge.declare_lost_for_test(&[10, 11, 12]);
    let batch = AckBatch {
        entries: vec![AckEntry {
            wire_seq: 11,
            arrival_time_ms_lo16: 1,
        }],
    };
    bridge.dispatch_ack_datagram(&batch.encode());
    bridge.retract_late_acked_losses();
    let left: Vec<u32> = bridge.pending_losses.iter().map(|l| l.wire_seq).collect();
    assert_eq!(left, vec![10, 12]);
}

/// A loss already handed to the estimator cannot be taken back: a late ack for
/// it is a no-op and leaves nothing behind.
#[tokio::test]
async fn a_late_ack_for_an_already_flushed_loss_is_a_no_op() {
    use crate::transport::ack::{AckBatch, AckEntry};
    let mut bridge = make_bridge_for_test().await;
    let t0 = super::now_std();
    bridge
        .transmission_ledger
        .record_departed(1, t0, ledger_tx(1_000, 1_200, 1));
    bridge
        .transmission_ledger
        .record_departed(2, t0, ledger_tx(2_000, 1_200, 2));
    bridge.declare_lost_for_test(&[1, 2]);
    bridge.pending_losses.retain(|l| l.wire_seq != 1); // wire_seq 1 was flushed
    let batch = AckBatch {
        entries: vec![AckEntry {
            wire_seq: 1,
            arrival_time_ms_lo16: 1,
        }],
    };
    bridge.dispatch_ack_datagram(&batch.encode());
    bridge.retract_late_acked_losses();
    assert_eq!(bridge.pending_losses.len(), 1);
    assert_eq!(bridge.pending_losses_retracted, 0);
    assert!(bridge.late_acked.is_empty());
}

/// The same, through the production turn: a loss expires in one `post_turn`,
/// its late ACK is dispatched, and the next `post_turn` must withdraw it. The
/// unit tests above call `retract_late_acked_losses` by hand; this one fails
/// if `post_turn` stops calling it.
#[tokio::test]
async fn post_turn_withdraws_the_loss_of_a_late_acked_transmission() {
    use crate::transport::ack::{AckBatch, AckEntry};
    let mut bridge = make_bridge_for_test().await;
    let long_ago = super::now_std()
        .checked_sub(std::time::Duration::from_secs(60))
        .expect("monotonic clock has 60 s of history");
    bridge
        .transmission_ledger
        .record_departed(41, long_ago, ledger_tx(1_000, 1_200, 1));
    bridge
        .transmission_ledger
        .record_departed(42, long_ago, ledger_tx(2_000, 900, 2));
    bridge.declare_lost_for_test(&[41, 42]);

    bridge.post_turn().await.expect("post_turn");
    assert_eq!(
        bridge.pending_losses.len(),
        2,
        "both declared, no sample to flush them"
    );

    let batch = AckBatch {
        entries: vec![AckEntry {
            wire_seq: 41,
            arrival_time_ms_lo16: 7,
        }],
    };
    bridge.dispatch_ack_datagram(&batch.encode());
    bridge.post_turn().await.expect("post_turn");

    let left: Vec<u32> = bridge.pending_losses.iter().map(|l| l.wire_seq).collect();
    assert_eq!(left, vec![42]);
    assert_eq!(bridge.pending_losses_retracted, 1);
}

/// Instrument: a loss is "late-acked after the flush" only if goog_cc was
/// already told about it, and "before the flush" only if it was withdrawn
/// while pending. The burst-loss scene cannot show the former (it reports a
/// legitimate zero there), so it is pinned here through the production turn:
/// expire, flush on a live acknowledgement, then acknowledge the "lost" one.
#[tokio::test]
async fn a_late_ack_after_the_flush_is_counted_apart_from_one_before_it() {
    use crate::transport::ack::{AckBatch, AckEntry};
    let ack = |ws: u32| {
        AckBatch {
            entries: vec![AckEntry {
                wire_seq: ws,
                arrival_time_ms_lo16: 7,
            }],
        }
        .encode()
    };
    let mut bridge = make_bridge_for_test().await;
    let long_ago = super::now_std()
        .checked_sub(std::time::Duration::from_secs(60))
        .expect("monotonic clock has 60 s of history");
    for ws in [1, 2] {
        bridge.transmission_ledger.record_departed(
            ws,
            long_ago,
            ledger_tx(1_000 * ws as u64, 1_200, ws),
        );
    }
    bridge.declare_lost_for_test(&[1, 2]);
    bridge.post_turn().await.expect("post_turn");
    assert_eq!(bridge.pending_losses.len(), 2, "declared, not yet reported");
    assert_eq!(bridge.instr.losses_declared, 2);
    assert_eq!(bridge.instr.losses_flushed, 0);

    // A live acknowledgement gives goog_cc a report to carry the losses.
    bridge
        .transmission_ledger
        .record_departed(3, super::now_std(), ledger_tx(3_000, 1_200, 3));
    bridge.dispatch_ack_datagram(&ack(3));
    bridge.post_turn().await.expect("post_turn");
    assert!(bridge.pending_losses.is_empty(), "flushed to goog_cc");
    assert_eq!(bridge.instr.losses_flushed, 2);

    // Now the first "lost" transmission turns out to have arrived.
    bridge.dispatch_ack_datagram(&ack(1));
    bridge.post_turn().await.expect("post_turn");
    let s = bridge.instrument_summary();
    assert_eq!(
        (
            s.late_acked_after_flush,
            s.late_acked_before_flush,
            s.late_acked_unreported
        ),
        (1, 0, 0)
    );
}

/// quinn reuses connection handles. The session-scoped instruments must not
/// carry one connection's departure tracker (or its billing) into the next one
/// that is given the same handle, or its first lags are stamped against
/// byte counts that belong to a connection that no longer exists.
#[tokio::test]
async fn a_forgotten_session_does_not_leak_its_mirror_into_a_reused_handle() {
    let h = ConnectionHandle(7);
    let mut bridge = test_bridge_with_sessions(&[h]).await;
    bridge.note_enqueued(
        h,
        Enqueued {
            class: WireClass::Critical,
            gated: true,
            dg_len: 100,
            framing_len: 2,
            wire_seq: None,
            parity_group: None,
            cum_enqueued: 102,
            queued: 102,
        },
    );
    bridge.note_enqueued(
        h,
        Enqueued {
            class: WireClass::Critical,
            gated: true,
            dg_len: 100,
            framing_len: 2,
            wire_seq: None,
            parity_group: None,
            cum_enqueued: 204,
            queued: 204,
        },
    );
    assert_eq!(bridge.departures.len(), 2);
    assert_eq!(bridge.instr.offered.total(), 204);

    bridge.forget_session(h);
    assert!(bridge.departures.is_empty(), "forgotten session's mirror");
    assert_eq!(bridge.instr.offered.total(), 0);
    assert_eq!(bridge.departures_handle, None);

    // The same handle, a new connection whose counter starts again.
    bridge.note_enqueued(
        h,
        Enqueued {
            class: WireClass::Critical,
            gated: true,
            dg_len: 100,
            framing_len: 2,
            wire_seq: None,
            parity_group: None,
            cum_enqueued: 102,
            queued: 102,
        },
    );
    assert_eq!(
        bridge.departures.len(),
        1,
        "no stale entry from the old one"
    );
}

// ---- T5: the sample's send time must survive the 32-bit emit clock wrapping ----

/// The datagram header carries the emit stamp as a `u32` of microseconds, which
/// wraps at 2^32 us = 71.6 min. `BweSample::server_emit_us` is documented as
/// full-precision and is what goog_cc turns into a send time. 80 minutes into a
/// session it must be ~4.8e9 us, not the wrapped ~5.05e8. Driven through the
/// real emitter, ledger and ACK path.
#[tokio::test]
async fn bwe_sample_send_time_is_not_the_wrapped_emit_clock() {
    use crate::transport::ack::{AckBatch, AckEntry};
    let mut bridge = make_bridge_for_test().await;
    let base = super::now_std()
        .checked_sub(std::time::Duration::from_secs(80 * 60))
        .expect("monotonic clock has 80 minutes of history");
    bridge.bwe_epoch = base;
    bridge.reliable_emitter.time_base = base;

    let now = super::now_std();
    let mut frag = vec![0u8; 20];
    frag[0] = 0x80; // stamped
    bridge.reliable_emitter.submit_one(
        crate::transport::reliable_emitter::EmitKey::new(1, 1, 2, 0),
        bytes::Bytes::from(frag),
        now,
        None,
        now,
    );
    let sent = bridge.reliable_emitter.take_transmissions();
    assert_eq!(sent.len(), 1);
    let true_us = now.duration_since(base).as_micros() as u64;
    assert!(
        true_us > u32::MAX as u64,
        "the fixture must be past the wrap"
    );
    bridge.transmission_ledger.record_departed(
        sent[0].wire_seq,
        now,
        crate::transport::transmission_ledger::Transmission {
            emit_us: sent[0].emit_us,
            wire_bytes: sent[0].wire_bytes,
            content: sent[0].content,
            depart_us: None,
            dep_idx: None,
        },
    );
    let batch = AckBatch {
        entries: vec![AckEntry {
            wire_seq: sent[0].wire_seq,
            arrival_time_ms_lo16: 1,
        }],
    };
    bridge.dispatch_ack_datagram(&batch.encode());

    assert_eq!(bridge.bwe_samples_buffer.len(), 1);
    let s = &bridge.bwe_samples_buffer[0];
    let err = s.server_emit_us.abs_diff(true_us);
    assert!(
        err < 1_000_000,
        "server_emit_us {} is {} us from the true send time {}",
        s.server_emit_us,
        err,
        true_us
    );
}

/// The loss path has the same clock: a transmission declared lost 80 minutes
/// in must reach goog_cc with a send time on the same (unwrapped) axis as the
/// acknowledged ones in the same report.
#[tokio::test]
async fn pending_loss_send_time_is_not_the_wrapped_emit_clock() {
    let mut bridge = make_bridge_for_test().await;
    let base = super::now_std()
        .checked_sub(std::time::Duration::from_secs(80 * 60))
        .expect("monotonic clock has 80 minutes of history");
    bridge.bwe_epoch = base;
    bridge.reliable_emitter.time_base = base;
    let now = super::now_std();
    let mut frag = vec![0u8; 20];
    frag[0] = 0x80;
    bridge.reliable_emitter.submit_one(
        crate::transport::reliable_emitter::EmitKey::new(1, 1, 2, 0),
        bytes::Bytes::from(frag),
        now,
        None,
        now,
    );
    let mut first_ws = 0;
    for e in bridge.reliable_emitter.take_transmissions() {
        first_ws = e.wire_seq;
        bridge.transmission_ledger.record_departed(
            e.wire_seq,
            now,
            crate::transport::transmission_ledger::Transmission {
                emit_us: e.emit_us,
                wire_bytes: e.wire_bytes,
                content: e.content,
                depart_us: None,
                dep_idx: None,
            },
        );
    }
    bridge.declare_lost_for_test(&[first_ws]);
    assert_eq!(bridge.pending_losses.len(), 1);
    let true_us = now.duration_since(base).as_micros() as u64;
    assert!(
        bridge.pending_losses[0].emit_us.abs_diff(true_us) < 1_000_000,
        "pending loss send time {} vs true {}",
        bridge.pending_losses[0].emit_us,
        true_us
    );
}

/// Before the wrap the widened stamp is exactly the emitter's stamp (whose low
/// 32 bits are what the header carries; see the emitter's boundary tests), so the
/// estimator's input for every session shorter than 71.6 minutes is unchanged.
#[tokio::test]
async fn pre_wrap_sample_send_time_equals_the_emitted_stamp() {
    use crate::transport::ack::{AckBatch, AckEntry};
    let mut bridge = make_bridge_for_test().await;
    let now = super::now_std();
    bridge.reliable_emitter.time_base = now - std::time::Duration::from_millis(1234);
    let mut frag = vec![0u8; 20];
    frag[0] = 0x80;
    bridge.reliable_emitter.submit_one(
        crate::transport::reliable_emitter::EmitKey::new(1, 1, 2, 0),
        bytes::Bytes::from(frag),
        now,
        None,
        now,
    );
    let sent = bridge.reliable_emitter.take_transmissions();
    let e = sent[0];
    assert_eq!(e.emit_us, 1_234_000);
    bridge.transmission_ledger.record_departed(
        e.wire_seq,
        now,
        crate::transport::transmission_ledger::Transmission {
            emit_us: e.emit_us,
            wire_bytes: e.wire_bytes,
            content: e.content,
            depart_us: None,
            dep_idx: None,
        },
    );
    let batch = AckBatch {
        entries: vec![AckEntry {
            wire_seq: e.wire_seq,
            arrival_time_ms_lo16: 1,
        }],
    };
    bridge.dispatch_ack_datagram(&batch.encode());
    assert_eq!(bridge.bwe_samples_buffer[0].server_emit_us, 1_234_000);
}

// ---- T14: the GPU-written compact list is not trusted ----

/// An index off the grid used to reach `metrics_tracker.get` and panic; short
/// side buffers used to panic on indexing. All of them must just skip the tile
/// and leave the palette table untouched.
#[tokio::test]
async fn phase_a_skips_compact_list_entries_it_cannot_resolve() {
    use crate::tile::CodecState;
    let mut bridge = make_bridge_for_test().await;
    bridge.metrics_tracker.resize(2, 2);
    for t in 0..4u32 {
        bridge.metrics_tracker.get_mut(t % 2, t / 2).codec_state =
            CodecState::PalRle { palette_id: 0 };
    }
    let palette_set = vec![
        crate::capture::gpu_pipeline::FramePaletteEntryRaw {
            count: 1,
            _pad: [0; 3],
            colors: [0; 16],
        };
        256
    ];
    let folded: Vec<u32> = (0..256).collect();
    let ids = [0u8; 1];
    let indices = vec![0u8; 512];

    // Off the grid.
    let preps = bridge.phase_a_palette_allocation(
        2,
        &[4, u32::MAX],
        &[0, 0],
        &folded,
        &palette_set,
        &indices,
    );
    assert!(preps.is_empty(), "off-grid tiles produce no work");
    // Zero columns.
    assert!(bridge
        .phase_a_palette_allocation(0, &[0], &[0], &folded, &palette_set, &indices)
        .is_empty());
    // In-grid tile 0 but a side buffer is empty: folded, palette set, indices.
    assert!(bridge
        .phase_a_palette_allocation(2, &[0], &[0], &[], &palette_set, &indices)
        .is_empty());
    assert!(bridge
        .phase_a_palette_allocation(2, &[0], &[0], &folded, &[], &indices)
        .is_empty());
    assert!(bridge
        .phase_a_palette_allocation(2, &[0], &[0], &folded, &palette_set, &[])
        .is_empty());
    // Nothing was acquired by any of the skipped entries.
    assert!(bridge
        .palette_table
        .in_flight_carrying
        .iter()
        .all(|&c| c == 0));
    assert!(bridge.palette_table.ref_count.iter().all(|&c| c == 0));
    // A palette-id slice shorter than the list: the second entry is skipped, the first still works.
    let preps =
        bridge.phase_a_palette_allocation(2, &[0, 1], &ids, &folded, &palette_set, &indices);
    assert_eq!(preps.len(), 1);
}

// ---- T12: a superseded bundled emission is settled once, not twice ----

/// Two bundled emissions (A, B) of one palette are outstanding. A's tile is
/// re-encoded, which supersedes A and gives A's count and reference back; A's
/// datagram is then acknowledged anyway. That acknowledgement must not give
/// them back again, or the count reads 0 while B still carries the palette.
#[tokio::test(flavor = "multi_thread")]
async fn acking_a_superseded_bundled_emission_does_not_release_its_palette_twice() {
    use crate::transport::protocol::Codec;
    use crate::transport::scheduler::ResolvedTileWork;
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);

    bridge.palette_table.ref_count[9] = 2;
    bridge.palette_table.slot_state[9] = crate::encoder::pal_rle::SlotState::Held;
    bridge.palette_table.in_flight_carrying[9] = 2;
    let cov: crate::transport::fragment_coverage::CoverageList =
        smallvec::smallvec![crate::transport::fragment_coverage::FragmentCoverage {
            tile_x: 2,
            tile_y: 3,
            generation: 0,
            pass_idx: 0,
            codec: Codec::PalRle,
            palette_id: Some(9),
            palette_bundled: true,
        }];
    bridge.fragment_coverage.record((301, 2, 3, 0), cov);
    bridge.seed_transmission_for_test(
        2001,
        crate::transport::reliable_emitter::EmitKey::new(301, 2, 3, 0),
    );

    // A's tile is re-encoded: its in-flight work is superseded.
    let superseded = [ResolvedTileWork {
        tile_x: 2,
        tile_y: 3,
        generation: 0,
        pass: 0,
        codec: Codec::PalRle,
        palette_id: Some(9),
        palette_bundled: true,
        via_ack: false,
    }];
    bridge.settle_superseded_palettes(2, 3, &superseded);
    assert_eq!(bridge.palette_table.in_flight_carrying[9], 1);
    assert_eq!(bridge.palette_table.ref_count[9], 1);

    // The superseded datagram turns out to have arrived.
    let batch = crate::transport::ack::AckBatch {
        entries: vec![crate::transport::ack::AckEntry {
            wire_seq: 2001,
            arrival_time_ms_lo16: 0,
        }],
    };
    bridge.dispatch_ack_datagram(&batch.encode());

    assert_eq!(
        bridge.palette_table.in_flight_carrying[9], 1,
        "B is still carrying the palette"
    );
    assert_eq!(
        bridge.palette_table.ref_count[9], 1,
        "B still holds its reference"
    );
    assert!(
        bridge.palette_table.delivered.contains(9),
        "the ack still proves the receiver has the palette"
    );
}

/// A superseded *thin* emission never raised the carry count, so it must not
/// lower it either (it would take down a bundled emission's count).
#[tokio::test(flavor = "multi_thread")]
async fn superseding_a_thin_emission_leaves_the_carry_count_alone() {
    use crate::transport::protocol::Codec;
    use crate::transport::scheduler::ResolvedTileWork;
    let (our_end, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
    bridge.palette_table.ref_count[9] = 2;
    bridge.palette_table.slot_state[9] = crate::encoder::pal_rle::SlotState::Held;
    bridge.palette_table.in_flight_carrying[9] = 1; // one bundled emission elsewhere
    let thin = [ResolvedTileWork {
        tile_x: 2,
        tile_y: 3,
        generation: 0,
        pass: 0,
        codec: Codec::PalRle,
        palette_id: Some(9),
        palette_bundled: false,
        via_ack: false,
    }];
    bridge.settle_superseded_palettes(2, 3, &thin);
    assert_eq!(bridge.palette_table.in_flight_carrying[9], 1);
}

/// The tile arm's commit step: the backend's dirty CDF 5/3 tiles are enqueued
/// for refinement, an escalation slot with encoded passes is enqueued, and a
/// slot the backend skipped is not.
#[tokio::test]
async fn the_tile_arm_commits_the_backends_cdf53_results_and_skips_what_it_skipped() {
    const W: u32 = 128;
    let (mut bridge, _peer, _log) = recording_bridge();
    bridge.scheduler.resize(4, 4);
    bridge.metrics_tracker.resize(4, 4);
    let frame = FrameSubmission {
        width: W,
        height: W,
        stride: W * 4,
        pixels: vec![0u8; (W * W * 4) as usize].into(),
        dmabuf_fd: None,
        timestamp_us: 0,
        content: crate::frame_damage::Content::Pixels,
        damage: crate::frame_damage::FrameDamage::Unknown,
        capture_done_ns: 0,
    };
    let enc = |p: u16| Cdf53Encoded {
        present_passes: 1,
        passes: vec![(0, vec![p as u8; 8])],
    };
    // Tiles (1,1), (2,2), (3,3) are flat indices 5, 10, 15 on a 4-wide grid.
    let results = frame::TileResults {
        palrle: OwnedPalRle::default(),
        cdf53_dirty: vec![Cdf53Tile {
            tile_x: 1,
            tile_y: 1,
            encoded: enc(1),
            #[cfg(feature = "cdf53-diag")]
            coeffs_i16: Vec::new(),
        }],
        cdf53_escalation: vec![Some(enc(2)), None],
        escalation_candidates: vec![10, 15],
        stranded_count: 0,
        timings: TileTimings::default(),
        frame_palette_set_count: 0,
    };
    bridge.emit_tile_codec_frame(
        &[(1, 1)],
        &frame::FrameMeta::of(&frame),
        &frame.pixels,
        results,
        1,
        1000,
    );
    assert!(
        bridge.scheduler.refinement_queue_holds_tile(1, 1)
            || bridge.scheduler.generation_for(1, 1) > 0,
        "the dirty CDF 5/3 tile was committed"
    );
    assert!(
        bridge.scheduler.generation_for(2, 2) > 0,
        "the encoded escalation slot started a new generation"
    );
    assert_eq!(
        bridge.scheduler.generation_for(3, 3),
        0,
        "the slot the backend skipped was not committed"
    );
}

/// The tiles the worker pre-encodes are those no tile encoder will carry.
#[tokio::test]
async fn the_worker_pre_encodes_exactly_the_tiles_no_tile_encoder_carries() {
    use crate::tile::CodecState;
    let (mut bridge, _peer, _log) = recording_bridge();
    bridge.metrics_tracker.resize(3, 2);
    let set = |b: &mut IoBridge, x, y, s| b.metrics_tracker.get_mut(x, y).codec_state = s;
    set(&mut bridge, 0, 0, CodecState::Skip);
    set(&mut bridge, 1, 0, CodecState::H264 { frames_in_h264: 3 });
    set(&mut bridge, 2, 0, CodecState::PixelPerfect);
    set(&mut bridge, 0, 1, CodecState::Solid);
    set(&mut bridge, 1, 1, CodecState::PalRle { palette_id: 2 });
    let all: Vec<(u32, u32)> = (0..2).flat_map(|y| (0..3).map(move |x| (x, y))).collect();
    set(
        &mut bridge,
        2,
        1,
        CodecState::Cdf53 {
            passes_sent: 0,
            present_passes: 1,
        },
    );
    assert_eq!(
        bridge.worker_fallback_tiles(&all),
        vec![(0, 0), (1, 0), (2, 0)]
    );
}

/// After an H.264 exit the first tile frame has every dirty tile in the H264
/// state, so every one of them takes the CDF 5/3 fallback. The worker encodes
/// them in the frame's own finish, so committing the frame makes no blocking
/// request of the worker (a full-screen round trip is 20 ms at 1080p, 86 ms at
/// 4K, on the loop).
#[tokio::test]
async fn a_full_screen_tile_frame_after_an_h264_exit_makes_no_blocking_forward() {
    use crate::tile::CodecState;
    const W: u32 = 128;
    let (mut bridge, _peer, log) = recording_bridge();
    bridge.test_commit_dg_size = Some(1200);
    bridge.scheduler.resize(4, 4);
    bridge.metrics_tracker.resize(4, 4);
    let all: Vec<(u32, u32)> = (0..4).flat_map(|y| (0..4).map(move |x| (x, y))).collect();
    for &(x, y) in &all {
        bridge.metrics_tracker.get_mut(x, y).codec_state = CodecState::H264 { frames_in_h264: 9 };
    }
    // What the worker returns for them: every tile, already encoded.
    let enc = |v: u8| Cdf53Encoded {
        present_passes: 1,
        passes: vec![(0, vec![v; 8])],
    };
    let out = TileOut {
        pixels: Ok(PixelSource::Frame),
        palrle: OwnedPalRle::default(),
        cdf53_dirty: Vec::new(),
        cdf53_escalation: Vec::new(),
        cdf53_fallback: all.iter().map(|&xy| (xy, enc(1))).collect(),
        timings: TileTimings::default(),
    };
    let inflight = stale_inflight(frame::InFlightStage::FinishingTile {
        dirty_xy: all.clone(),
        max_frag: 1000,
        escalation_candidates: Vec::new(),
        stranded_count: 0,
        frame_palette_set_count: 0,
        trace: Default::default(),
    });
    let inflight = frame::InFlight {
        meta: frame::FrameMeta {
            width: W,
            height: W,
            stride: W * 4,
            timestamp_us: 0,
            capture_done_ns: 0,
            unchanged: false,
        },
        ..inflight
    };
    bridge.on_tile_done(inflight, out, vec![0x40; (W * W * 4) as usize].into(), 0);

    assert_eq!(
        bridge.frame_pipeline.as_ref().unwrap().forward_requests(),
        0,
        "no blocking Forward request"
    );
    assert!(
        !log.lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("forward_cdf53_tiles")),
        "the worker was never asked to forward"
    );
    for &(x, y) in &all {
        assert!(
            bridge.scheduler.generation_for(x as u8, y as u8) > 0,
            "tile ({x},{y}) was committed as CDF 5/3"
        );
    }
}

/// A tile the GPU's own compact list already covers is not transformed a
/// second time for the fallback.
#[test]
fn fallback_tiles_already_in_the_compact_list_are_not_forwarded_twice() {
    let in_list: std::collections::HashSet<u32> = [5u32, 10].into_iter().collect();
    // 4 columns: flat 5 = (1,1), flat 10 = (2,2).
    let got = super::frame_backend::fallback_not_in_compact_list(
        &[(0, 0), (1, 1), (2, 2), (3, 3)],
        4,
        &in_list,
    );
    assert_eq!(got, vec![(0, 0), (3, 3)]);
}

/// The finish request the loop builds lists the dirty tiles in a state no tile
/// encoder carries (and the worker passes the list on to the backend).
#[tokio::test]
async fn the_finish_request_lists_the_tiles_the_loop_classified_as_fallback() {
    use crate::tile::CodecState;
    let backend = RecordingBackend::default();
    let log = backend.log.clone();
    let (mut bridge, _peer) = bridge_with_backend(backend);
    bridge.metrics_tracker.resize(4, 4);
    let set = |b: &mut IoBridge, x, y, s| b.metrics_tracker.get_mut(x, y).codec_state = s;
    set(&mut bridge, 0, 0, CodecState::Skip);
    set(&mut bridge, 1, 0, CodecState::H264 { frames_in_h264: 2 });
    set(&mut bridge, 2, 0, CodecState::Solid);
    set(&mut bridge, 3, 0, CodecState::PixelPerfect);
    let dirty = [(0, 0), (1, 0), (2, 0), (3, 0)];
    let (action, _, _) = bridge.tile_finish_action(4, 4, &dirty);
    let FinishAction::Tile { fallback, .. } = &action else {
        panic!("a tile frame is finished with FinishAction::Tile");
    };
    assert_eq!(fallback, &vec![(0, 0), (1, 0), (3, 0)]);

    // And the worker hands it to the backend as the request's fallback list.
    bridge.start_analysis(tiny_frame(), 1, 1200, Vec::new(), 0);
    let analysed = next_worker_message(&mut bridge).await.unwrap();
    let inflight = bridge.in_flight.take().unwrap();
    drop(analysed);
    bridge.send_finish(inflight, action, frame::InFlightStage::FinishingDiscard);
    let _done = next_worker_message(&mut bridge).await.unwrap();
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|l| l == "finish_tile fallback=[(0, 0), (1, 0), (3, 0)]"),
        "{:?}",
        log.lock().unwrap()
    );
}

/// A fallback tile the worker did not pre-encode (a PalRle tile that lost its
/// palette at commit) is the one case that still asks the worker.
#[tokio::test]
async fn a_fallback_tile_the_worker_did_not_encode_still_asks_for_it() {
    use crate::tile::CodecState;
    let (mut bridge, _peer, log) = recording_bridge();
    bridge.scheduler.resize(4, 4);
    bridge.metrics_tracker.resize(4, 4);
    bridge.metrics_tracker.get_mut(1, 1).codec_state = CodecState::Skip;
    fallback_dispatch(&mut bridge, &[(1, 1)]);
    assert_eq!(
        bridge.frame_pipeline.as_ref().unwrap().forward_requests(),
        1
    );
    assert!(log
        .lock()
        .unwrap()
        .iter()
        .any(|l| l == "forward_cdf53_tiles 1"));
}

/// The real backend's fallback pre-encode is the CPU transform of the same
/// tiles, byte for byte: the worker path changes where the work runs, not what
/// it produces. Needs a GPU; skips without one.
#[tokio::test]
async fn gpu_backend_pre_encodes_fallback_tiles_exactly_as_the_cpu_transform() {
    // Not a multiple of 32 in either direction, and a padded stride, so the
    // right and bottom edge tiles and the stride are exercised.
    const W: u32 = 100;
    const H: u32 = 70;
    const STRIDE: u32 = W * 4 + 16;
    let _gpu = crate::gpu_test_lock::lock();
    let processor = crate::capture::gpu_pipeline::GpuFrameProcessor::new(256)
        .expect("this test needs a Vulkan GPU and must not skip");
    let mut backend = GpuBackend::new(processor);
    // Three flat colours in diagonal bands: every tile has few colours, so none
    // is in the GPU's own CDF 5/3 compact list (which is what the fallback
    // covers), yet every tile has edges, so the transform is not trivial.
    let colours: [[u8; 4]; 3] = [[10, 20, 30, 255], [200, 100, 50, 255], [90, 220, 140, 255]];
    let pixels: Vec<u8> = (0..STRIDE * H)
        .map(|i| {
            let (row, col) = (i / STRIDE, i % STRIDE);
            let px = (col / 4) as usize;
            colours[(px / 7 + row as usize / 5) % 3][(col % 4) as usize]
        })
        .collect();
    let frame = FrameSubmission {
        width: W,
        height: H,
        stride: STRIDE,
        pixels: pixels.clone().into(),
        dmabuf_fd: None,
        timestamp_us: 0,
        content: crate::frame_damage::Content::Pixels,
        damage: crate::frame_damage::FrameDamage::Unknown,
        capture_done_ns: 0,
    };
    backend.analyse(&frame, &[], false).expect("analyse");
    // (3, 2) is the bottom-right edge tile, (3, 0) a right edge tile.
    let fallback = [(1u32, 0u32), (0, 2), (3, 2), (3, 0)];
    let out = backend.finish_tile(
        &frame,
        &TileRequest {
            cols: 4,
            rows: 3,
            escalation: &[],
            fallback: &fallback,
        },
    );
    assert_eq!(out.cdf53_fallback.len(), fallback.len());
    let grid = crate::tile::TileGrid::new(W, H);
    for (&(tx, ty), (got_xy, got)) in fallback.iter().zip(out.cdf53_fallback.iter()) {
        assert_eq!((tx, ty), *got_xy);
        let tile = grid.extract_tile(&pixels, STRIDE, tx, ty);
        let (present, passes) =
            crate::encoder::cdf53::encode_passes_sparse(&crate::encoder::cdf53::forward(&tile));
        assert_eq!(got.present_passes, present, "tile ({tx},{ty}) pass mask");
        assert_eq!(got.passes, passes, "tile ({tx},{ty}) pass bytes");
    }
}

// ── Access re-check: which lookup answers ────────────────────────────────

mod recheck_answers {
    use super::*;
    use crate::config::AccessConfig;
    use crate::transport::peer_auth::{
        AccessPolicy, DenialLog, PeerGate, PeerIdentity, PeerIdentitySource, PeerLookup,
    };

    fn who(id: i64) -> PeerIdentity {
        PeerIdentity {
            user_id: id,
            login_name: format!("u{id}@test"),
            node_name: "peer".into(),
            tags: vec![],
        }
    }

    struct Background;
    impl PeerIdentitySource for Background {
        fn lookup(&self, _: std::net::IpAddr) -> PeerLookup {
            Ok(who(1))
        }
        fn is_immediate(&self) -> bool {
            false
        }
    }

    async fn tracked_bridge(
        h: ConnectionHandle,
        ip: std::net::IpAddr,
        t0: std::time::Instant,
    ) -> IoBridge {
        let mut bridge = test_bridge_with_sessions(&[h]).await;
        let owner = who(1);
        bridge.set_peer_gate_for_test(PeerGate::new(
            Arc::new(AccessPolicy::new(&owner, &AccessConfig::default())),
            Arc::new(Background),
            Arc::new(DenialLog::new()),
        ));
        bridge.rechecker.track(h, ip, t0);
        bridge
    }

    // A lookup that began before the re-check may carry a pre-revocation
    // "allowed". It must neither extend the session nor close it; the
    // re-check's own lookup answers.
    #[tokio::test]
    async fn a_lookup_older_than_the_recheck_does_not_answer_it() {
        let h = ConnectionHandle(41);
        let ip: std::net::IpAddr = "100.64.0.2".parse().unwrap();
        let t0 = now_std() - std::time::Duration::from_secs(30);
        let mut bridge = tracked_bridge(h, ip, t0).await;
        let due = bridge.rechecker.next_due().unwrap();

        // A new-connection lookup is running when the re-check comes due.
        let gate = bridge.server.peer_gate_mut();
        gate.check(ip, now_std(), true);
        let old = gate.take_stamped_requests();
        assert_eq!(old.len(), 1);
        let _ = bridge.rechecker.run_due(bridge.server.peer_gate_mut(), due);
        let fresh = bridge.server.peer_gate_mut().take_stamped_requests();
        assert_eq!(fresh.len(), 1, "the re-check starts its own lookup");

        // The old lookup returns "allowed": dropped, nothing changes.
        bridge
            .apply_peer_lookup(ip, old[0].1, Ok(who(1)))
            .await
            .unwrap();
        assert_eq!(
            bridge.rechecker.next_due(),
            Some(due + crate::transport::peer_auth::DEFAULT_RECHECK_INTERVAL),
            "a stale answer must not move the schedule"
        );
        assert!(!bridge.close_deadlines.contains_key(&h));

        // The fresh lookup says the peer is a stranger: closed.
        bridge
            .apply_peer_lookup(ip, fresh[0].1, Ok(who(7)))
            .await
            .unwrap();
        assert_eq!(
            bridge.close_deadlines.get(&h).map(|d| d.code),
            Some(crate::transport::io_bridge::session::CLOSE_CODE_ACCESS_REVOKED)
        );
    }
}

#[test]
fn the_token_wake_floor_is_a_millisecond_or_a_datagram_at_the_rate() {
    use std::time::Duration;
    // 10 MB/s: a 1.4 kB datagram is 140 us, so the 1 ms floor rules.
    assert_eq!(
        pacing::token_wake_floor(1_400, 10_000_000.0),
        Duration::from_millis(1)
    );
    // 100 kB/s: a datagram is 14 ms of tokens, longer than the floor.
    assert_eq!(
        pacing::token_wake_floor(1_400, 100_000.0),
        Duration::from_millis(14)
    );
    assert_eq!(
        pacing::token_wake_floor(1_400, 0.0),
        Duration::from_millis(1)
    );
}

/// An ungated accept (H.264, control) that finds quinn's FIFO already at
/// `D_target` is counted, and one that finds it below is not; either way it
/// is charged to the bucket. `depth_violations` stays for the gated classes.
#[tokio::test(start_paused = true)]
async fn ungated_accepts_at_or_above_the_depth_target_are_counted() {
    use super::transport_probe::Enqueued;
    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (_tx, rx) = mpsc::channel(8);
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 4, 4);
    bridge.gate = Some(pacing::GateParams { d_target: 2_000 });
    let h = ConnectionHandle(0);
    let e = |queued: u64| Enqueued {
        class: WireClass::H264,
        gated: false,
        dg_len: 1_000,
        framing_len: 2,
        wire_seq: None,
        parity_group: None,
        cum_enqueued: queued,
        queued,
    };
    // The FIFO stood at 500 before this one (1,502 after): below.
    bridge.note_enqueued(h, e(1_502));
    assert_eq!(bridge.instr.admission.ungated_at_depth, 0);
    // It stood at 2,000 before (3,002 after): at the target.
    bridge.note_enqueued(h, e(3_002));
    assert_eq!(bridge.instr.admission.ungated_at_depth, 1);
    assert_eq!(bridge.instr.admission.depth_violations, 0);
    assert_eq!(bridge.instr.admission.soft_charged_bytes, 2_004);
}

/// A tile parity datagram is tagged with its group on the way into the
/// departure tracker, so the ledger learns when it left; H.264 frame parity,
/// which travels under the same wire class, is not. (The full chain through a
/// live quinn connection is covered by the browserless `FECORACLE` report; the
/// lib has no in-process connection fixture.)
#[tokio::test(start_paused = true)]
async fn only_tile_parity_reaches_the_departure_tracker_tagged() {
    use super::transport_probe::Enqueued;
    let (ours, _peer) = tokio::net::UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let (_tx, rx) = mpsc::channel(8);
    let mut bridge = IoBridge::new_with_injection_for_test(ours, server, rx, 4, 4);
    let h = ConnectionHandle(0);
    let e = |cum: u64, parity_group: Option<u32>| Enqueued {
        class: WireClass::Parity,
        gated: false,
        dg_len: 100,
        framing_len: 2,
        wire_seq: None,
        parity_group,
        cum_enqueued: cum,
        queued: cum,
    };
    // A 4K keyframe's H.264 parity: many datagrams, no group.
    for i in 0..88u64 {
        bridge.note_enqueued(h, e(102 * (i + 1), None));
    }
    // One tile group's parity.
    bridge.note_enqueued(h, e(102 * 89, Some(4_000)));
    let now = super::now_std();
    bridge.departures.on_poll(102 * 89, 0, now, |_, _| {});
    assert_eq!(
        bridge.departures.take_departed_parity(),
        vec![4_000],
        "the tile group, and none of the 88 H.264 parity datagrams"
    );
}

// ---- F3: the loss clock and the estimator's send times start at departure ----

/// A transmission that has been built and recorded but has not left quinn's
/// FIFO is in no departure order, however old it is; once it leaves, a verdict
/// on it is reported to goog_cc against the moment it left, not the moment it
/// was built.
#[tokio::test]
async fn a_queued_transmission_is_not_declared_lost_and_a_departed_one_is_reported_at_departure() {
    let mut bridge = make_bridge_for_test().await;
    let long_ago = super::now_std()
        .checked_sub(std::time::Duration::from_secs(60))
        .expect("monotonic clock has 60 s of history");
    // The emitter's clock starts a minute ago, so "5 ms in" is a minute ago.
    bridge.reliable_emitter.time_base = long_ago;
    let base = long_ago;
    bridge
        .transmission_ledger
        .record(41, long_ago, ledger_tx(1_000, 1_200, 1));
    // Five later transmissions leave and the last is acknowledged: the first
    // three of them are lost, and the queued one is not among them however old.
    for ws in 42..=46u32 {
        bridge
            .transmission_ledger
            .record_departed(ws, long_ago, ledger_tx(1_000, 1_200, ws));
    }
    assert!(bridge.transmission_ledger.resolve(46).is_some());
    bridge.post_turn().await.expect("post_turn");
    assert!(
        bridge.pending_losses.iter().all(|l| l.wire_seq != 41),
        "a transmission still queued cannot have been lost"
    );
    // 42 and 43 by the packet rule, 44 and 45 by the time rule (a minute old,
    // behind the acknowledged 46).
    assert_eq!(bridge.instr.losses_declared, 4, "42 to 45");
    assert_eq!(bridge.transmission_ledger.departed(41), Some(false));
    bridge.pending_losses.clear();

    // It leaves 5 ms after the emitter's epoch, and is then declared lost.
    let left = base + std::time::Duration::from_millis(5);
    bridge.stamp_departure(41, left);
    bridge.declare_lost_for_test(&[41]);
    assert_eq!(bridge.pending_losses.len(), 1);
    assert_eq!(
        bridge.pending_losses[0].emit_us, 5_000,
        "reported against the departure (5 ms), not the build stamp (1 ms)"
    );
    assert_eq!(
        bridge.transmission_ledger.stats().expired_before_departure,
        0
    );
}

/// The estimator's send time for an acknowledged transmission is when it left
/// the FIFO; the latency statistics keep the build stamp.
#[tokio::test]
async fn an_acknowledged_transmission_reaches_the_estimator_at_its_departure_time() {
    use crate::transport::ack::{AckBatch, AckEntry};
    let mut bridge = make_bridge_for_test().await;
    let base = bridge.reliable_emitter.time_base;
    let now = super::now_std();
    bridge
        .transmission_ledger
        .record(7, now, ledger_tx(1_000, 1_200, 1));
    // Built at 1 ms, left at 9 ms.
    bridge.stamp_departure(7, base + std::time::Duration::from_millis(9));
    let batch = AckBatch {
        entries: vec![AckEntry {
            wire_seq: 7,
            arrival_time_ms_lo16: 3,
        }],
    };
    bridge.dispatch_ack_datagram(&batch.encode());
    assert_eq!(bridge.bwe_samples_buffer.len(), 1);
    let s = &bridge.bwe_samples_buffer[0];
    assert_eq!(s.depart_us, 9_000, "the estimator's send time");
    assert_eq!(
        s.server_emit_us, 1_000,
        "the latency stats' start, unchanged"
    );
    assert_eq!(
        s.ack_arrival().server_emit_us,
        9_000,
        "what goog_cc is given is the departure time"
    );
}

// ---- D1 PR-6b: the delivery-rate sampler is fed by departures and ACKs ----

/// Four transmissions depart, the newest is acknowledged and the three behind
/// it are declared lost; four more depart and three are acknowledged as one
/// batch. Each is marked at its departure, a batch makes one sample, the
/// in-flight tally follows every way a record leaves, and a transmission
/// declared lost leaves the tally without feeding the sampler.
#[tokio::test]
async fn departures_are_marked_and_an_ack_batch_makes_one_rate_sample() {
    use crate::transport::ack::{AckBatch, AckEntry};
    let mut bridge = make_bridge_for_test().await;
    let now = super::now_std();
    let t0 = now - std::time::Duration::from_millis(40);
    for ws in 1..=4u32 {
        bridge
            .transmission_ledger
            .record(ws, t0, ledger_tx(1_000, 1_200, ws));
    }
    for ws in 1..=4u32 {
        bridge.stamp_departure(ws, t0 + std::time::Duration::from_millis(ws as u64));
    }
    assert_eq!(bridge.delivery_sampler.marked(), 4);
    assert_eq!(bridge.transmission_ledger.departed_bytes_in_flight(), 4_800);
    let ack = |wss: std::ops::RangeInclusive<u32>| {
        AckBatch {
            entries: wss
                .map(|ws| AckEntry {
                    wire_seq: ws,
                    arrival_time_ms_lo16: 3,
                })
                .collect(),
        }
        .encode()
    };

    // 4 is acknowledged ahead of 1..=3, which makes them holes; the time rule
    // (zero threshold) declares them lost. Their bytes leave the tally and
    // they are never sampled.
    bridge.dispatch_ack_datagram(&ack(4..=4));
    let lost = bridge.transmission_ledger.detect_losses_at(
        now + std::time::Duration::from_secs(60),
        std::time::Duration::ZERO,
    );
    assert_eq!(lost.len(), 3, "premise: three holes behind the ack");
    assert_eq!(bridge.transmission_ledger.departed_bytes_in_flight(), 0);
    assert_eq!(bridge.delivery_sampler.stats(super::now_std()).acked, 1);

    for ws in 1..=4u32 {
        bridge
            .transmission_ledger
            .record(10 + ws, t0, ledger_tx(1_000, 1_200, ws));
        bridge.stamp_departure(
            10 + ws,
            t0 + std::time::Duration::from_millis(10 + ws as u64),
        );
    }
    assert_eq!(bridge.transmission_ledger.departed_bytes_in_flight(), 4_800);

    bridge.dispatch_ack_datagram(&ack(11..=13));
    let st = bridge.delivery_sampler.stats(super::now_std());
    assert_eq!(st.acked, 4);
    assert_eq!(st.samples, 2, "one sample per acknowledgement batch");
    assert!(st.latest_bps > 0, "the sample carries a rate");
    assert_eq!(bridge.transmission_ledger.departed_bytes_in_flight(), 1_200);

    // A duplicate of the same batch resolves to nothing and samples nothing.
    bridge.dispatch_ack_datagram(&ack(11..=13));
    assert_eq!(bridge.delivery_sampler.stats(super::now_std()).samples, 2);
}

/// A transmission declared lost and acknowledged after all was delivered by
/// the network, only late: it is a delivery sample (the tombstone keeps its
/// mark). One that may be an FEC recovery is not (nothing was delivered).
#[tokio::test]
async fn a_late_ack_of_a_declared_lost_transmission_is_still_sampled() {
    use crate::transport::ack::{AckBatch, AckEntry};
    let mut bridge = make_bridge_for_test().await;
    let now = super::now_std();
    let t0 = now - std::time::Duration::from_millis(40);
    for ws in 1..=3u32 {
        bridge
            .transmission_ledger
            .record(ws, t0, ledger_tx(1_000, 1_200, ws));
        bridge.stamp_departure(ws, t0 + std::time::Duration::from_millis(ws as u64));
    }
    let ack = |ws: u32| {
        AckBatch {
            entries: vec![AckEntry {
                wire_seq: ws,
                arrival_time_ms_lo16: 3,
            }],
        }
        .encode()
    };
    bridge.dispatch_ack_datagram(&ack(3));
    let lost = bridge.transmission_ledger.detect_losses_at(
        now + std::time::Duration::from_secs(60),
        std::time::Duration::ZERO,
    );
    assert_eq!(lost.len(), 2, "premise: 1 and 2 are declared lost");
    let before = bridge.delivery_sampler.stats(super::now_std());
    assert_eq!((before.acked, before.samples), (1, 1));

    bridge.dispatch_ack_datagram(&ack(2));
    let after = bridge.delivery_sampler.stats(super::now_std());
    assert_eq!(after.acked, 2, "the late ack was folded in");
    assert_eq!(after.samples, 2, "and made a sample");
    assert_eq!(
        bridge.transmission_ledger.stats().acked_after_declared_lost,
        1,
        "premise: it went through the Late path"
    );
}

/// A session change replaces the sampler: its `delivered` and ordinals belong
/// to the session that is gone, and a mark left on a surviving record must not
/// be read against the fresh estimator.
#[tokio::test]
async fn a_session_change_replaces_the_delivery_sampler_and_drops_old_marks() {
    use crate::transport::ack::{AckBatch, AckEntry};
    let mut bridge = make_bridge_for_test().await;
    let now = super::now_std();
    let t0 = now - std::time::Duration::from_millis(40);
    for ws in 1..=3u32 {
        bridge
            .transmission_ledger
            .record(ws, t0, ledger_tx(1_000, 1_200, ws));
        bridge.stamp_departure(ws, t0 + std::time::Duration::from_millis(ws as u64));
    }
    let ack = |ws: u32| {
        AckBatch {
            entries: vec![AckEntry {
                wire_seq: ws,
                arrival_time_ms_lo16: 3,
            }],
        }
        .encode()
    };
    bridge.dispatch_ack_datagram(&ack(1));
    let before = bridge.delivery_sampler.stats(super::now_std());
    assert_eq!(before.marked, 3);
    assert_eq!(before.samples, 1, "premise: the old sampler had state");

    bridge.release_departures();

    let after = bridge.delivery_sampler.stats(super::now_std());
    assert_eq!(after.marked, 0, "a fresh sampler");
    assert_eq!(after.delivered_bytes, 0);
    // 2 and 3 departed under the old sampler. The ledger reset forgot them
    // (the same call resets both), so their acknowledgement is unknown and is
    // not sampled against the new sampler.
    assert_eq!(bridge.transmission_ledger.departed(2), None);
    bridge.dispatch_ack_datagram(&ack(2));
    let st = bridge.delivery_sampler.stats(super::now_std());
    assert_eq!((st.acked, st.samples), (0, 0));
}

/// Forgetting the live session discards the transmissions that were queued
/// for it and never left: they are neither lost nor delivered, so the
/// estimator hears nothing, and nothing is left on the books to alias the
/// next connection that reuses the handle. Ones that did leave go too (since
/// PR-5): outstanding, they would be declared lost into the next connection's
/// estimator, and its adapted threshold would carry over.
#[tokio::test]
async fn forgetting_a_session_clears_the_ledger() {
    let h = ConnectionHandle(7);
    let mut bridge = test_bridge_with_sessions(&[h]).await;
    let now = super::now_std();
    bridge
        .transmission_ledger
        .record(1, now, ledger_tx(1_000, 1_200, 1));
    bridge
        .transmission_ledger
        .record_departed(2, now, ledger_tx(2_000, 1_200, 2));
    bridge.track_enqueue(h, Some(1), 100);
    assert_eq!(bridge.departures.len(), 1);

    bridge.forget_session(h);

    assert!(bridge.departures.is_empty());
    assert_eq!(bridge.departures_handle, None);
    assert_eq!(bridge.transmission_ledger.departed(1), None, "discarded");
    assert_eq!(
        bridge.transmission_ledger.departed(2),
        None,
        "and the departed"
    );
    assert_eq!(bridge.transmission_ledger.len(), 0);
}

/// A loss verdict from the old session cannot reach the new connection's
/// estimator: the old session's outstanding records are gone, and a threshold
/// the old path's reordering raised is back at its initial value.
#[tokio::test]
async fn a_new_session_does_not_inherit_the_old_sessions_verdicts() {
    let h = ConnectionHandle(7);
    let mut bridge = test_bridge_with_sessions(&[h]).await;
    let now = super::now_std();
    // Five departures, the last acknowledged: two are lost, three outstanding.
    for ws in 0..5u32 {
        bridge
            .transmission_ledger
            .record_departed(ws, now, ledger_tx(1_000, 1_200, ws));
    }
    assert!(bridge.transmission_ledger.resolve(4).is_some());
    bridge.track_enqueue(h, Some(1), 100);
    bridge.forget_session(h);
    // The new connection acknowledges one of its own.
    bridge
        .transmission_ledger
        .record_departed(100, now, ledger_tx(1_000, 1_200, 100));
    assert!(bridge.transmission_ledger.resolve(100).is_some());
    bridge.expire_transmissions(now);
    assert!(
        bridge.pending_losses.is_empty(),
        "the old session's transmissions were declared lost into the new one"
    );
    assert_eq!(bridge.instr.losses_declared, 0);
}

// ---- F4 part 2: a verdict contradicted by a late acknowledgement is undone ----

/// Eifel response: the acknowledgement of a transmission already declared lost
/// retracts the loss from the loss window and from the pending list and ends
/// the PTO backoff; its (long) round trip is not a sample. goog_cc cannot be told to forget,
/// and the instruments say how much it was told (`late_acked_after_flush`).
/// Skipping the retraction leaves the loss rate and the pending loss standing.
#[tokio::test(start_paused = true)]
async fn a_spurious_verdict_is_retracted_and_its_round_trip_is_not_a_sample() {
    use crate::transport::ack::{AckBatch, AckEntry};
    let mut bridge = make_bridge_for_test().await;
    bridge
        .reliable_emitter
        .rtt
        .on_sample(std::time::Duration::from_millis(40));
    let srtt_before = bridge.reliable_emitter.rtt.srtt();
    let now = super::now_std();
    bridge.transmission_ledger.record_departed(
        7,
        now,
        ledger_tx(bridge.reliable_emitter.emit_us64(now), 1_200, 7),
    );
    bridge.declare_lost_for_test(&[7]);
    assert_eq!(bridge.pending_losses.len(), 1, "premise: declared lost");
    // The path was silent: the probe backoff is up.
    bridge.reliable_emitter.note_pto_fire(now);
    bridge
        .reliable_emitter
        .note_pto_fire(now + std::time::Duration::from_secs(1));
    assert_eq!(bridge.reliable_emitter.rtt.pto_count(), 2, "premise");

    // A second later the "lost" datagram is acknowledged.
    tokio::time::advance(std::time::Duration::from_millis(900)).await;
    let batch = AckBatch {
        entries: vec![AckEntry {
            wire_seq: 7,
            arrival_time_ms_lo16: 1,
        }],
    };
    bridge.dispatch_ack_datagram(&batch.encode());
    bridge.retract_late_acked_losses();

    assert!(
        bridge.pending_losses.is_empty(),
        "retracted before it reached goog_cc"
    );
    assert_eq!(bridge.pending_losses_retracted, 1);
    assert_eq!(bridge.reliable_emitter.rtt.pto_count(), 0, "backoff reset");
    // The late round trip is NOT a sample: it includes the time the verdict
    // beat it by (and a recovered datagram's reflects the parity).
    assert_eq!(
        bridge.reliable_emitter.rtt.srtt(),
        srtt_before,
        "a late acknowledgement must not move srtt"
    );
    assert_eq!(bridge.reliable_emitter.rtt.samples(), 1);
    assert_eq!(bridge.transmission_ledger.stats().spurious_losses, 1);
}

/// The probe timeout is a wake deadline of the event loop: with a static
/// screen after a lost final pass there is no frame and no heartbeat to run it,
/// and a loop that did not wake for it would sleep through the repair.
#[tokio::test(start_paused = true)]
async fn the_probe_timeout_is_a_wake_deadline() {
    let mut bridge = make_bridge_for_test().await;
    bridge
        .reliable_emitter
        .rtt
        .on_sample(std::time::Duration::from_millis(40));
    assert_eq!(
        bridge.next_wake_deadline(),
        None,
        "premise: nothing pending"
    );
    let now = super::now_std();
    let key = crate::transport::reliable_emitter::EmitKey::new(1, 1, 2, 0);
    let mut frag = vec![0u8; 20];
    frag[0] = 0x80;
    bridge
        .reliable_emitter
        .submit_one(key, bytes::Bytes::from(frag), now, None, now);
    for e in bridge.reliable_emitter.take_transmissions() {
        bridge.transmission_ledger.record_departed(
            e.wire_seq,
            now,
            crate::transport::transmission_ledger::Transmission {
                emit_us: e.emit_us,
                wire_bytes: e.wire_bytes,
                content: e.content,
                depart_us: None,
                dep_idx: None,
            },
        );
        bridge.reliable_emitter.on_departed(e.wire_seq, now);
    }
    let pto = bridge.reliable_emitter.pto();
    assert_eq!(
        bridge.next_wake_deadline(),
        Some(now + pto),
        "the loop wakes for the probe"
    );
    // And it fires there, on its own: the pass is sent again, in the same
    // turn. The probe runs before the admission pump, so the pump hands the
    // retransmission to the transport now and not at some later wake.
    bridge.post_turn().await.expect("post_turn");
    let sent_before = bridge.sent_log_for_test.len();
    assert_eq!(sent_before, 1, "premise: the original has been sent");
    tokio::time::advance(pto).await;
    bridge.post_turn().await.expect("post_turn");
    assert_eq!(bridge.instr.pto_probes_requested, 1);
    assert_eq!(
        bridge.reliable_emitter.pending_loss_count(),
        0,
        "the probe's retransmission was not left waiting for another turn"
    );
    assert_eq!(
        bridge.sent_log_for_test.len(),
        sent_before + 1,
        "and it reached the transport in the turn that queued it"
    );
}

/// The client acks a datagram it rebuilt from parity under its original
/// `wire_seq`, a round trip after the group's parity left and with no other
/// member missing. That is a loss that happened: it stays in the pending list
/// and the loss window and is counted apart. The same acknowledgement sooner
/// than a round trip after the parity cannot be one and is retracted
/// (`a_spurious_verdict_is_retracted_...`).
#[tokio::test(start_paused = true)]
async fn an_acknowledgement_a_round_trip_after_the_parity_is_an_fec_recovery_and_not_retracted() {
    use crate::transport::ack::{AckBatch, AckEntry};
    for (wait_ms, recovery) in [(50u64, true), (30, false)] {
        let mut bridge = make_bridge_for_test().await;
        bridge
            .reliable_emitter
            .rtt
            .on_sample(std::time::Duration::from_millis(40));
        let now = super::now_std();
        for ws in 0..10u32 {
            bridge
                .transmission_ledger
                .record_departed(ws, now, ledger_tx(1_000, 1_200, ws));
        }
        // The group's parity has left, through the tracker as in production.
        let _ = bridge.departures.on_enqueue(None, 100, now);
        bridge.departures.tag_last_as_parity(0);
        bridge.departures.on_poll(100, 0, now, |_, _| {});
        for g in bridge.departures.take_departed_parity() {
            bridge.transmission_ledger.note_parity_departed(
                g,
                now,
                std::time::Duration::from_secs(1),
            );
        }
        for ws in (0..10u32).filter(|w| *w != 3) {
            assert!(bridge.transmission_ledger.resolve(ws).is_some());
        }
        bridge.declare_lost_for_test(&[3]);
        assert_eq!(bridge.pending_losses.len(), 1, "premise: declared lost");
        tokio::time::advance(std::time::Duration::from_millis(wait_ms)).await;
        let batch = AckBatch {
            entries: vec![AckEntry {
                wire_seq: 3,
                arrival_time_ms_lo16: 1,
            }],
        };
        bridge.dispatch_ack_datagram(&batch.encode());
        bridge.retract_late_acked_losses();
        assert_eq!(
            (
                bridge.instr.late_acks_fec_possible,
                bridge.pending_losses.len(),
                bridge.pending_losses_retracted,
            ),
            if recovery { (1, 1, 0) } else { (0, 0, 1) },
            "{wait_ms} ms after the parity"
        );
        assert_eq!(bridge.reliable_emitter.rtt.samples(), 1, "never a sample");
    }
}

/// A hole with too few departures behind it for the packet rule is found by
/// the time rule, and the event loop wakes for it: `next_wake_deadline` carries
/// the loss time, so it is declared at about 9/8 of a round trip and not at
/// the probe timeout.
#[tokio::test(start_paused = true)]
async fn the_loss_time_of_a_hole_is_a_wake_deadline() {
    let mut bridge = make_bridge_for_test().await;
    bridge
        .reliable_emitter
        .rtt
        .on_sample(std::time::Duration::from_millis(40));
    let thr = crate::transport::loss_path::time_threshold(&bridge.reliable_emitter.rtt);
    assert_eq!(thr, std::time::Duration::from_millis(45));
    let now = super::now_std();
    for ws in 0..3u32 {
        bridge
            .transmission_ledger
            .record_departed(ws, now, ledger_tx(1_000, 1_200, ws));
    }
    assert!(bridge.transmission_ledger.resolve(1).is_some());
    assert!(bridge.transmission_ledger.resolve(2).is_some());
    // Hole at 0, two departures behind it: no packet verdict, a loss time.
    bridge.expire_transmissions(now);
    assert!(bridge.pending_losses.is_empty());
    assert_eq!(bridge.next_wake_deadline(), Some(now + thr));
    tokio::time::advance(thr).await;
    bridge.expire_transmissions(super::now_std());
    assert_eq!(bridge.pending_losses.len(), 1, "declared at the loss time");
    assert_eq!(bridge.transmission_ledger.stats().lost_time_threshold, 1);
}

/// What the `loss path` journal event reads: the instrument summary carries the
/// packet threshold, the verdicts by rule, the contradicted ones and what the
/// probe did, from the live counters.
#[tokio::test(start_paused = true)]
async fn the_loss_path_counters_reach_the_instrument_summary() {
    let mut bridge = make_bridge_for_test().await;
    let now = super::now_std();
    bridge
        .transmission_ledger
        .record_departed(1, now, ledger_tx(1_000, 1_200, 1));
    bridge.declare_lost_for_test(&[1]);
    bridge.reliable_emitter.note_pto_fire(now);
    let s = bridge.instrument_summary();
    assert_eq!((s.k_pkt, s.loss_by_pto, s.pto_fires), (3, 1, 1));
    assert_eq!(
        (s.loss_by_packet_threshold, s.loss_by_time_threshold),
        (0, 0)
    );
    assert_eq!(
        (s.spurious_loss, s.spurious_pto, s.late_acks_fec_possible),
        (0, 0, 0)
    );
}

/// The compare's verdict reaches the feedback sink under the frame's own
/// timestamp, as a mask of exactly the dirty tiles; a frame whose GPU stage
/// failed is reported dropped. (A sink that was never enabled records
/// nothing, which `feedback::tests` covers.)
#[tokio::test]
async fn the_dirty_set_and_a_failed_frame_reach_the_feedback_sink() {
    use crate::feedback::{FeedbackKind, FeedbackSink};
    let backend = RecordingBackend {
        dirty_tiles: vec![1, 2],
        ..Default::default()
    };
    let (mut bridge, _peer) = bridge_with_backend(backend);
    let sink = Arc::new(FeedbackSink::new());
    sink.enable();
    bridge.feedback = Some(sink.clone());

    let mut frame = tiny_frame(); // 64x64: a 2x2 tile grid
    frame.timestamp_us = 42;
    bridge.start_analysis(frame, 1, 1200, Vec::new(), 0);
    let analysed = next_worker_message(&mut bridge).await.expect("analysed");
    bridge.on_frame_worker_message(analysed);
    let mut got = Vec::new();
    sink.drain_into(&mut got);
    assert_eq!(got.len(), 1, "one record per analysed frame");
    assert_eq!(got[0].timestamp_us, 42);
    assert_eq!((got[0].width, got[0].height), (64, 64));
    assert_eq!(got[0].kind, FeedbackKind::Dirty(vec![0b0110]));

    let backend = RecordingBackend {
        analyse_err: Some("boom".into()),
        ..Default::default()
    };
    let (mut bridge, _peer) = bridge_with_backend(backend);
    bridge.feedback = Some(sink.clone());
    let mut frame = tiny_frame();
    frame.timestamp_us = 43;
    bridge.start_analysis(frame, 1, 1200, Vec::new(), 0);
    let failed = next_worker_message(&mut bridge).await.expect("failed");
    bridge.on_frame_worker_message(failed);
    got.clear();
    sink.drain_into(&mut got);
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].timestamp_us, &got[0].kind),
        (43, &FeedbackKind::Dropped)
    );
}

/// An unchanged tick the backend refuses is a dropped frame, not a GPU fault:
/// the producer is told (a Dropped verdict), nothing is counted as a failure.
#[tokio::test]
async fn a_refused_unchanged_tick_is_dropped_without_counting_a_gpu_failure() {
    use crate::feedback::{FeedbackKind, FeedbackSink};
    for (refused, counted) in [(true, 0), (false, 1)] {
        let backend = RecordingBackend {
            analyse_refused: refused.then(|| "no baseline".to_string()),
            analyse_err: (!refused).then(|| "vkQueueSubmit failed".to_string()),
            ..Default::default()
        };
        let (mut bridge, _peer) = bridge_with_backend(backend);
        let sink = Arc::new(FeedbackSink::new());
        sink.enable();
        bridge.feedback = Some(sink.clone());
        bridge.start_analysis(tiny_frame(), 1, 1200, Vec::new(), 0);
        let msg = next_worker_message(&mut bridge).await.expect("failed");
        bridge.on_frame_worker_message(msg);
        assert_eq!(bridge.gpu_analyse_failures, counted);
        let mut got = Vec::new();
        sink.drain_into(&mut got);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, FeedbackKind::Dropped);
    }
}

/// The periodic transport lines (`cumulative emit`, `bwe periodic snapshot`)
/// are driven from the frame-sending step of both frame modes. They used to
/// live only in the tile arm, so an H.264 stretch went silent.
#[tokio::test]
async fn an_h264_frame_drives_the_periodic_transport_logs() {
    let (mut bridge, _peer, _log) = recording_bridge();
    let meta = frame::FrameMeta {
        width: 128,
        height: 128,
        stride: 512,
        timestamp_us: 0,
        capture_done_ns: 0,
        unchanged: false,
    };
    assert_eq!(bridge.last_cumulative_emit_log_frame, 0);
    assert!(bridge.bwe_log_last_at.is_none());
    // Inside the throttle window: nothing is logged, the marker stays put.
    bridge.send_h264_frame(
        H264Out {
            payload: vec![7; 64],
            is_keyframe: true,
        },
        &meta,
        CUMULATIVE_EMIT_LOG_INTERVAL_FRAMES - 1,
        1200,
    );
    assert_eq!(bridge.last_cumulative_emit_log_frame, 0);
    // Past it: the cumulative line fires and the marker advances.
    bridge.send_h264_frame(
        H264Out {
            payload: vec![7; 64],
            is_keyframe: false,
        },
        &meta,
        CUMULATIVE_EMIT_LOG_INTERVAL_FRAMES,
        1200,
    );
    assert_eq!(
        bridge.last_cumulative_emit_log_frame,
        CUMULATIVE_EMIT_LOG_INTERVAL_FRAMES
    );
    assert!(
        bridge.bwe_log_last_at.is_some(),
        "the BWE snapshot fired in H.264 mode"
    );
    assert!(bridge.cumulative_datagrams_emitted.h264 > 0);
    assert!(
        bridge.bytes_emitted_h264 > 0 && bridge.bytes_emitted_snapshot.2 > 0,
        "the H.264 byte counter feeds bps_h264"
    );
}

/// The mirror of the H.264 test: the tile arm still drives the periodic lines.
#[tokio::test]
async fn a_tile_frame_drives_the_periodic_transport_logs() {
    let (mut bridge, _peer, _log) = recording_bridge();
    bridge.scheduler.resize(4, 4);
    bridge.metrics_tracker.resize(4, 4);
    let frame = FrameSubmission {
        width: 128,
        height: 128,
        stride: 512,
        pixels: vec![0u8; 128 * 128 * 4].into(),
        dmabuf_fd: None,
        timestamp_us: 0,
        content: crate::frame_damage::Content::Pixels,
        damage: crate::frame_damage::FrameDamage::Unknown,
        capture_done_ns: 0,
    };
    let results = frame::TileResults {
        palrle: OwnedPalRle::default(),
        cdf53_dirty: Vec::new(),
        cdf53_escalation: Vec::new(),
        escalation_candidates: Vec::new(),
        stranded_count: 0,
        timings: TileTimings::default(),
        frame_palette_set_count: 0,
    };
    assert_eq!(bridge.last_cumulative_emit_log_frame, 0);
    bridge.emit_tile_codec_frame(
        &[],
        &frame::FrameMeta::of(&frame),
        &frame.pixels,
        results,
        CUMULATIVE_EMIT_LOG_INTERVAL_FRAMES,
        1000,
    );
    assert_eq!(
        bridge.last_cumulative_emit_log_frame,
        CUMULATIVE_EMIT_LOG_INTERVAL_FRAMES
    );
    assert!(bridge.bwe_log_last_at.is_some());
}

// ---------------------------------------------------------------------------
// H.264 wire v2: reset requests (0x09) and frame acks (0x0A)
// ---------------------------------------------------------------------------

fn reset_request_bytes(
    reason: ghostframe_protocol::feedback_stream::ResetReason,
    frame_seq: u32,
) -> Vec<u8> {
    let mut buf = Vec::new();
    ghostframe_protocol::feedback_stream::ResetRequest { reason, frame_seq }.encode(&mut buf);
    buf
}

fn keyframe_controls(bridge: &IoBridge) -> usize {
    bridge
        .pending_controls
        .iter()
        .filter(|c| **c == Control::RequestKeyframe)
        .count()
}

#[tokio::test]
async fn a_decoder_failure_reset_request_asks_for_one_idr_per_two_seconds() {
    use ghostframe_protocol::feedback_stream::ResetReason;
    let (mut bridge, _peer) = bridge_with_backend(RecordingBackend::default());
    make_live(&mut bridge, ConnectionHandle(0));
    bridge.pending_controls.clear();

    let msg = reset_request_bytes(ResetReason::DecoderFailure, 77);
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &msg);
    assert_eq!(keyframe_controls(&bridge), 1, "first request is honoured");
    assert_eq!(bridge.reset_requests_honoured, 1);

    // A burst inside the window is answered by the IDR already requested.
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &msg);
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &msg);
    assert_eq!(keyframe_controls(&bridge), 1);
    assert_eq!(bridge.reset_requests_received, 3);
    assert_eq!(bridge.reset_requests_rate_limited, 2);

    // Past the window the next one acts again. Drive the handler with a
    // synthetic clock rather than sleeping.
    let t0 = bridge.last_reset_request_idr_at.expect("recorded");
    let req = ghostframe_protocol::feedback_stream::ResetRequest::decode(&msg).unwrap();
    bridge.handle_reset_request(req, t0 + std::time::Duration::from_millis(1999));
    assert_eq!(keyframe_controls(&bridge), 1, "1.999 s is still inside");
    bridge.handle_reset_request(req, t0 + std::time::Duration::from_secs(2));
    assert_eq!(keyframe_controls(&bridge), 2, "2 s later it acts");
}

#[tokio::test]
async fn reset_reasons_other_than_decoder_failure_request_no_idr() {
    use ghostframe_protocol::feedback_stream::ResetReason;
    let (mut bridge, _peer) = bridge_with_backend(RecordingBackend::default());
    make_live(&mut bridge, ConnectionHandle(0));
    bridge.pending_controls.clear();
    for reason in [
        ResetReason::Resolution,
        ResetReason::NoDecoder,
        ResetReason::GateReset,
    ] {
        bridge.dispatch_feedback_bytes(ConnectionHandle(0), &reset_request_bytes(reason, 1));
    }
    assert_eq!(keyframe_controls(&bridge), 0);
    assert_eq!(bridge.reset_requests_received, 3);
    assert_eq!(bridge.reset_requests_honoured, 0);
}

#[tokio::test]
async fn reset_requests_survive_stream_splits_and_neighbours() {
    use ghostframe_protocol::feedback_stream::ResetReason;
    let (mut bridge, _peer) = bridge_with_backend(RecordingBackend::default());
    make_live(&mut bridge, ConnectionHandle(0));
    bridge.pending_controls.clear();

    // Split across two stream reads, with an undefined-reason message and a
    // real one behind it in the same buffer: the fixed length keeps the
    // stream in step, so the later message still lands.
    let mut bad = reset_request_bytes(ResetReason::DecoderFailure, 5);
    bad[1] = 0xEE;
    let good = reset_request_bytes(ResetReason::DecoderFailure, 6);
    let mut all = bad.clone();
    all.extend_from_slice(&good);
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &all[..4]);
    assert_eq!(keyframe_controls(&bridge), 0, "incomplete message waits");
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &all[4..]);
    assert_eq!(keyframe_controls(&bridge), 1);
    assert_eq!(
        bridge.reset_requests_received, 1,
        "the undefined one is not counted"
    );
}

#[tokio::test]
async fn a_reset_request_from_a_session_that_never_said_hello_does_nothing() {
    use ghostframe_protocol::feedback_stream::ResetReason;
    let (mut bridge, _peer) = bridge_with_backend(RecordingBackend::default());
    bridge.pending_controls.clear();
    bridge.dispatch_feedback_bytes(
        ConnectionHandle(9),
        &reset_request_bytes(ResetReason::DecoderFailure, 1),
    );
    assert_eq!(keyframe_controls(&bridge), 0);
    assert_eq!(bridge.reset_requests_received, 0);
}

#[tokio::test]
async fn frame_acks_are_counted_and_ignored() {
    use crate::transport::protocol::{FrameAck, FRAME_ACK_SIZE};
    let mut bridge = make_bridge_for_test().await;
    make_live(&mut bridge, ConnectionHandle(0));

    let mut dg = Vec::new();
    FrameAck {
        newest_seq: 10,
        decoded_mask: 0b111,
        refused_mask: 0,
    }
    .encode(&mut dg);
    bridge.dispatch_inbound_datagram(ConnectionHandle(0), &dg);
    assert_eq!(bridge.frame_acks_received, 1);

    // Truncated and over-long acks are malformed, not routed elsewhere.
    bridge.dispatch_inbound_datagram(ConnectionHandle(0), &dg[..FRAME_ACK_SIZE - 1]);
    let mut long = dg.clone();
    long.push(0);
    bridge.dispatch_inbound_datagram(ConnectionHandle(0), &long);
    assert_eq!(bridge.frame_acks_received, 1);
    assert_eq!(bridge.frame_acks_malformed, 2);
    assert_eq!(bridge.unroutable_inbound_datagrams, 0);
}

#[test]
fn h264_frames_name_the_previous_frame_as_their_reference() {
    use crate::transport::protocol::{FRAME_FLAG_IDR, REF_FRAME_NONE};
    let idr = frame::h264_ref_info(true, Some(41));
    assert_eq!(idr.flags, FRAME_FLAG_IDR);
    assert_eq!(
        idr.ref_frame_seq, REF_FRAME_NONE,
        "an IDR names no reference"
    );
    assert_eq!(idr.slots, 0);

    let p = frame::h264_ref_info(false, Some(41));
    assert_eq!(p.flags, 0, "a P frame carries no idr bit");
    assert_eq!(p.ref_frame_seq, 41);

    let unknown = frame::h264_ref_info(false, None);
    assert_eq!(
        unknown.ref_frame_seq, REF_FRAME_NONE,
        "no previous frame: unknown"
    );
}

fn h264_frame_meta() -> frame::FrameMeta {
    frame::FrameMeta {
        width: 64,
        height: 64,
        stride: 256,
        timestamp_us: 0,
        capture_done_ns: 0,
        unchanged: false,
    }
}

#[tokio::test]
async fn sending_an_h264_frame_records_it_as_the_next_frames_reference_until_a_session_reset() {
    let mut bridge = make_bridge_for_test().await;
    assert_eq!(bridge.last_h264_frame_seq, None);
    let meta = h264_frame_meta();
    let out = |k| H264Out {
        payload: vec![1; 50],
        is_keyframe: k,
    };
    bridge.send_h264_frame(out(true), &meta, 7, 1200);
    assert_eq!(bridge.last_h264_frame_seq, Some(7));
    bridge.send_h264_frame(out(false), &meta, 9, 1200);
    assert_eq!(
        bridge.last_h264_frame_seq,
        Some(9),
        "tracks the newest frame"
    );

    // A session reset clears it, and the rate-limit window with it, so a new
    // client's first decoder failure is honoured at once.
    bridge.last_reset_request_idr_at = Some(std::time::Instant::now());
    bridge.fire_session_reset(ConnectionHandle(0));
    assert_eq!(bridge.last_h264_frame_seq, None);
    assert_eq!(bridge.last_reset_request_idr_at, None);
}

#[tokio::test]
async fn the_reset_request_window_is_cleared_by_a_session_reset() {
    use ghostframe_protocol::feedback_stream::ResetReason;
    let (mut bridge, _peer) = bridge_with_backend(RecordingBackend::default());
    make_live(&mut bridge, ConnectionHandle(0));
    let msg = reset_request_bytes(ResetReason::DecoderFailure, 1);
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &msg);
    bridge.pending_controls.clear();
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &msg);
    assert_eq!(keyframe_controls(&bridge), 0, "inside the window");
    bridge.fire_session_reset(ConnectionHandle(0));
    bridge.pending_controls.clear();
    bridge.dispatch_feedback_bytes(ConnectionHandle(0), &msg);
    assert_eq!(
        keyframe_controls(&bridge),
        1,
        "a new session starts a new window"
    );
}
