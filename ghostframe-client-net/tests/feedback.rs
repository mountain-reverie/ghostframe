mod common;

use common::{connected_session, pump_with_wt};

/// The Hello queued by `ClientCore::new` at construction must survive the
/// gap between "queued" and "a feedback stream exists to carry it" — that
/// gap is exactly what `pending_stream_out` exists to bridge. Once the
/// WebTransport session is ready, `ClientNet` opens a bidi stream and
/// flushes it there; the server routes data on any bidi stream other than
/// the session stream into its feedback queue.
#[test]
fn hello_reaches_the_server_on_the_feedback_stream() {
    let (mut client, mut server, mut wt, mut now_us, base) = connected_session();
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);

    let feedback = wt.drain_feedback();
    assert!(
        feedback.iter().any(|m| m.first() == Some(&0x03)),
        "server must receive the Hello message (type 0x03), got {feedback:?}"
    );
}

/// Input produced before the session is ready -- the pointer crossing the
/// window as it appears, the release of the Enter that launched the client --
/// must not reach the stream ahead of the Hello. The server reads the first
/// non-Hello message from a session that is not live yet as grounds to
/// discard the rest of that read, Hello included, and then closes the
/// connection at the HELLO deadline: a black window and no input, 20 s later
/// "no HELLO before deadline".
#[test]
fn input_sent_before_the_session_is_ready_does_not_precede_the_hello() {
    use common::{admitting_server, pinned_hash};
    use ghostframe_client_net::{ClientNet, ClientNetConfig};
    use ghostframe_lib::transport::webtransport::WebTransportServer;
    use std::net::{Ipv6Addr, SocketAddr};
    use std::time::Instant;

    let mut server = admitting_server();
    let mut wt = WebTransportServer::new();
    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256: pinned_hash(&server),
        indices_raw_enabled: true,
        supports_h264: false,
        display: None,
    };
    let base = Instant::now();
    let mut now_us = 0u64;
    let mut client = ClientNet::new(cfg, now_us).expect("ClientNet::new");
    client
        .connect(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443), now_us)
        .expect("connect");
    // A pointer move, as the CLI encodes it, before any handshake step.
    client.send_input(vec![0x05, 0x01, 0x00, 0x10, 0x00, 0x20], now_us);

    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 128);
    assert!(wt.is_connected(), "server must accept the CONNECT");

    let first: Vec<u8> = wt.drain_feedback().into_iter().flatten().collect();
    assert_eq!(
        first.first(),
        Some(&0x03),
        "the first byte on the feedback stream must be the Hello's type, got {first:02x?}"
    );
}

/// The control for the test above: input sent once the session is ready does
/// arrive, so dropping early input is not hiding input altogether.
#[test]
fn input_sent_after_the_session_is_ready_reaches_the_server() {
    let (mut client, mut server, mut wt, mut now_us, base) = connected_session();
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);
    let _ = wt.drain_feedback();

    let motion = vec![0x05, 0x01, 0x00, 0x10, 0x00, 0x20];
    client.send_input(motion.clone(), now_us);
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);

    let got: Vec<u8> = wt.drain_feedback().into_iter().flatten().collect();
    assert!(
        got.windows(motion.len()).any(|w| w == motion.as_slice()),
        "input after SessionReady must reach the server, got {got:02x?}"
    );
}
