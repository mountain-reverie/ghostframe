//! M4a acceptance: a second client displaces the first, and the first
//! learns *why*.
//!
//! Joins the tailnet as its own node, connects client A over tsnet, waits
//! for A to be actively served (a published frame proves HELLO landed),
//! then connects client B on the same node and asserts A receives
//! `ClientEvent::Disconnected { reason: "...DisplacedByNewSession...",
//! expected: true }` -- not merely that its connection dropped.
//!
//! # Why this test matters more than a normal e2e
//!
//! `evict_session`'s build-3x-send loop (`ghostframe-lib/src/transport/io_bridge.rs`)
//! is unverified at the unit level: with `EVICTION_REPEATS = 0` every one of
//! the 460 lib tests stays green, because the test bridge has no live
//! `quinn_proto::Connection` in `server.connections`, so `evict_session`'s
//! `if let Some(conn)` never enters and the send loop never runs. This test,
//! asserting on the *reason string* client A actually receives, is the only
//! coverage of that send path. Do not weaken it to "the connection dropped"
//! -- a dropped connection is also what a network failure looks like, and
//! telling the two apart is the entire purpose of the eviction notice.
//!
//! Requires Docker AND a GPU. Deliberately NOT named in any CI workflow --
//! see the exemption comment in `.github/workflows/e2e.yml`.
//!
//! # `TS_CONTROL_URL` must be in the PROCESS environment, not set from Rust
//!
//! Run this as:
//!
//! ```text
//! TS_CONTROL_URL=http://127.0.0.1:18080 \
//!   cargo test -p ghostframe-e2e --test eviction -- --nocapture --test-threads=1
//! ```
//!
//! ghostbridge's Go `init()` picks the DERP transport at **package-init
//! time, before `main`**, from `os.Getenv("TS_CONTROL_URL")`: non-empty
//! means headscale, so opt into `TS_DEBUG_USE_DERP_HTTP=1`; empty means the
//! public tailnet, which requires HTTPS DERP (`ghostbridge/main.go`). A
//! `std::env::set_var("TS_CONTROL_URL", ..)` in the test body is therefore
//! far too late -- the decision was made when the binary loaded, and the
//! symptom (a wall of `derp.Recv`/TLS handshake errors) reads like a
//! network fault rather than a missing env var. See `native_client.rs`'s
//! module doc for the full diagnosis; the same trap applies here.
//!
//! # Both clients share the harness's tsnet node
//!
//! A second `tsnet.Server` in one process does not converge a working peer
//! datapath -- `native_client.rs` documents the diagnosis at length. Client
//! A and client B both attach to `setup._test_node.bridge()` rather than
//! either one standing up its own node.

use std::time::{Duration, Instant};

use ghostframe_client_native::{Client, ClientEvent, Config};
use ghostframe_e2e::harness::{read_server_logs_stripped, setup_e2e_server, E2eServerSpec};

#[path = "common/client_wait.rs"]
mod client_wait;
use client_wait::wait_for_frame;

/// Pump events until the client reports a disconnect, returning
/// `(reason, expected)`.
///
/// Unlike `wait_for_frame`, this must NOT panic on `ClientEvent::Error`: a
/// session torn down underneath the client may surface an error alongside
/// the disconnect, and panicking there would hide the very event under
/// test.
fn wait_for_disconnect(client: &mut Client, timeout: Duration) -> Option<(String, bool)> {
    let deadline = Instant::now() + timeout;
    loop {
        while let Some(ev) = client.next_event() {
            tracing::info!(?ev, "client event");
            if let ClientEvent::Disconnected { reason, expected } = ev {
                return Some((reason, expected));
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn make_client(hostname: &str, state_dir: &std::path::Path) -> Client {
    Client::new(Config {
        hostname: hostname.into(),
        authkey: String::new(), // unused: the bridge below is already up
        state_dir: state_dir.to_path_buf(),
        supports_h264: false,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers: vec![],
        debug_map_frames: false,
    })
    .expect("create client")
}

#[tokio::test(flavor = "multi_thread")]
async fn second_client_displaces_the_first_and_names_the_reason() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    eprintln!("[phase] starting headscale + ghostframe-server containers");
    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--solid-red",
        extra_env: &[],
        gpu: false,
        webgpu: false,
        url_query_extra: "",
    })
    .await
    .expect("bring up headscale + ghostframe server");

    eprintln!("[phase] containers up; building client A");
    let state_dir_a = tempfile::tempdir().expect("tempdir a");
    let mut a = make_client("eviction-test-a", state_dir_a.path());
    a.attach_bridge(setup._test_node.bridge());

    eprintln!(
        "[phase] connecting client A to {}:443 over tsnet",
        setup.server_container_name
    );
    if let Err(e) = a.connect(&setup.server_container_name, 443) {
        eprintln!(
            "--- server logs ({}) ---\n{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
        panic!("client A connect over the tailnet failed: {e}");
    }

    // Wait for a frame on A *before* connecting B. A published frame proves
    // A has sent HELLO and is actively being served -- without this the
    // test can race and evict a client that had not yet identified itself,
    // passing for the wrong reason.
    eprintln!("[phase] waiting for the first published frame on A");
    if wait_for_frame(&mut a, Duration::from_secs(60)).is_none() {
        eprintln!(
            "--- server logs ({}) ---\n{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
        panic!("no frame published to client A within 60s");
    }

    eprintln!("[phase] building + connecting client B (should displace A)");
    let state_dir_b = tempfile::tempdir().expect("tempdir b");
    let mut b = make_client("eviction-test-b", state_dir_b.path());
    b.attach_bridge(setup._test_node.bridge());
    if let Err(e) = b.connect(&setup.server_container_name, 443) {
        eprintln!(
            "--- server logs ({}) ---\n{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
        panic!("client B connect over the tailnet failed: {e}");
    }

    eprintln!("[phase] waiting for A's disconnect notice");
    let disconnect = wait_for_disconnect(&mut a, Duration::from_secs(30));
    let (reason, expected) = match disconnect {
        Some(pair) => pair,
        None => {
            // The server logs the `evicting session` line with a
            // `notices_sent` count on eviction. Absent entirely means HELLO
            // attribution from B never reached `apply_hello` (A was never
            // marked evictable); present with `notices_sent=0` means all
            // three eviction datagrams hit QUIC backpressure and were
            // never actually sent; present with a nonzero count means the
            // datagrams left the server and the fault is on the client
            // side (net_thread not surfacing `Event::Evicted`, or the
            // datagram lost in transit despite 3x resend). Check which of
            // these it is before assuming the send path itself is broken.
            eprintln!(
                "--- server logs ({}) ---\n{}",
                setup.server_container_name,
                read_server_logs_stripped(&setup.server_container_name)
            );
            panic!("client A never reported a disconnect within 30s of B connecting");
        }
    };

    eprintln!("[result] A disconnected: reason={reason:?} expected={expected}");

    assert!(
        reason.contains("DisplacedByNewSession"),
        "expected A's disconnect reason to name DisplacedByNewSession, got {reason:?}. \
         A silent drop is indistinguishable from a network failure -- this is the \
         one assertion that tells them apart."
    );
    // This is the flag the CLI's exit code keys on: a regression here would
    // make a deliberate displacement look like a crash to any embedder.
    assert!(
        expected,
        "displacement must be reported as `expected: true`; got false, which would make \
         the CLI (and any other embedder) treat a deliberate hand-off as a crash"
    );

    // B must survive its own arrival: it should not itself be evicted by
    // the notices meant for A, and it should go on to be served normally.
    eprintln!("[phase] waiting for a frame on B (must survive)");
    if wait_for_frame(&mut b, Duration::from_secs(60)).is_none() {
        eprintln!(
            "--- server logs ({}) ---\n{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
        panic!("client B (the new incumbent) never received a frame within 60s");
    }

    eprintln!("[phase] done");
    let _ = b.disconnect();
}
