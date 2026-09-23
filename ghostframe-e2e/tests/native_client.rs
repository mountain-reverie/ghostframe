//! M1 acceptance: the native client against a live ghostframe server.
//!
//! Joins the tailnet as its own node, connects over tsnet, decodes on the
//! GPU, and asserts exact pixels in the exported dmabuf. There is no
//! direct-socket path here and none may be added -- staying inside the
//! tailnet is a security property of this system, not a test convenience.
//!
//! Requires Docker AND a GPU. Deliberately NOT named in any CI workflow
//! (see `ghostframe-client-gpu/README.md`'s test-target table).

use std::time::{Duration, Instant};

use ghostframe_client_native::{Client, ClientEvent, Config, PublishedFrame};
use ghostframe_e2e::harness::e2e_setup::HEADSCALE_HOST_PORT;
use ghostframe_e2e::harness::{
    create_preauth_key, read_server_logs_stripped, setup_e2e_server, E2eServerSpec,
};

/// Drain queued events (logging each) and return the first published frame,
/// or `None` if `timeout` elapses first. Panics eagerly on a
/// `ClientEvent::Error` rather than waiting out the full timeout, since that
/// event means the library has already given up.
fn wait_for_frame(client: &mut Client, timeout: Duration) -> Option<PublishedFrame> {
    let deadline = Instant::now() + timeout;
    loop {
        while let Some(ev) = client.next_event() {
            tracing::info!(?ev, "client event");
            if let ClientEvent::Error { message } = &ev {
                panic!("client reported an error while waiting for a frame: {message}");
            }
        }
        if let Some(frame) = client.acquire_frame() {
            return Some(frame);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn native_client_renders_the_test_pattern_into_an_exported_dmabuf() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--solid-red",
        extra_env: &[],
        gpu: false,
        webgpu: false,
        url_query_extra: "",
    })
    .await
    .expect("bring up headscale + ghostframe server");

    // Join the tailnet as our own node, exactly as a real client would --
    // no forwarder, no direct socket, just tsnet.
    let authkey = create_preauth_key("headscale", "ghostframe")
        .await
        .expect("preauth key");

    // `Client::connect` reads `TS_CONTROL_URL` the same way
    // `ghostframe-xdaemon` does (unset = the real Tailscale network, set =
    // this custom control plane); point it at the same headscale instance
    // `setup_e2e_server`'s own tsnet node joined, reachable from the host at
    // the port `e2e_setup::HEADSCALE_HOST_PORT` maps.
    std::env::set_var(
        "TS_CONTROL_URL",
        format!("http://127.0.0.1:{HEADSCALE_HOST_PORT}"),
    );

    let state_dir = tempfile::tempdir().expect("tempdir");
    let mut client = Client::new(Config {
        hostname: "native-client-test".into(),
        authkey,
        state_dir: state_dir.path().to_path_buf(),
        supports_h264: false,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers: vec![],
    })
    .expect("create client");

    let connect_result = client.connect(&setup.server_container_name, 443);
    if let Err(e) = &connect_result {
        eprintln!(
            "--- server logs ({}) ---\n{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
        panic!("connect over the tailnet failed: {e}");
    }

    let frame = match wait_for_frame(&mut client, Duration::from_secs(60)) {
        Some(f) => f,
        None => {
            eprintln!(
                "--- server logs ({}) ---\n{}",
                setup.server_container_name,
                read_server_logs_stripped(&setup.server_container_name)
            );
            panic!("no frame published within 60s");
        }
    };

    eprintln!(
        "frame_id={} buffer_id={} {}x{} damage={:?}",
        frame.frame_id, frame.buffer_id, frame.width, frame.height, frame.damage
    );

    // The export buffer has never been filled before this publish, so
    // `ExportRing::publish` must take the full-surface blit path: the
    // damage on the first frame should cover the whole surface.
    let full_surface_covered = frame
        .damage
        .iter()
        .any(|r| r.x == 0 && r.y == 0 && r.w == frame.width && r.h == frame.height);
    assert!(
        full_surface_covered,
        "expected the first frame's damage to cover the full {}x{} surface \
         (never-filled export buffer forces a full blit), got {:?}",
        frame.width, frame.height, frame.damage
    );

    let f = client.debug_map_frame(&frame).expect("map exported dmabuf");

    // The scene is solid red. Sample well away from the origin: an
    // origin-only check passes even when the stride is wrong.
    let (x, y) = (100u64, 100u64);
    assert!(
        u64::from(frame.width) > x && u64::from(frame.height) > y,
        "sample point ({x},{y}) is outside the {}x{} surface",
        frame.width,
        frame.height
    );
    let o = (f.offset + y * f.stride + x * 4) as usize;
    let px = &f.bytes[o..o + 4];
    eprintln!(
        "pixel at ({x},{y}): {px:?} (stride={}, offset={})",
        f.stride, f.offset
    );
    assert_eq!(
        (px[0], px[1], px[2]),
        (0xFF, 0x00, 0x00),
        "expected solid red (R=0xFF,G=0x00,B=0x00) at ({x},{y}), got {px:?}"
    );

    client.release_frame(frame.frame_id);
    client.disconnect().expect("disconnect");
}
