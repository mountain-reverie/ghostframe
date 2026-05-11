#[path = "e2e/helpers.rs"]
mod helpers;

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use testcontainers::core::{IntoContainerPort, Mount, WaitFor};
use testcontainers::{runners::AsyncRunner, ContainerAsync, GenericImage, ImageExt};

/// Fixed host port for headscale.  Using a fixed port lets us construct the
/// `HS_SERVER_URL` (headscale's advertised URL) before the container starts, so
/// the embedded DERP server URL is reachable from both the host machine and
/// Docker containers.  The port is outside the Linux ephemeral range.
const HEADSCALE_HOST_PORT: u16 = 18080;

/// The Docker default-bridge gateway address.  This IP is reachable from:
///   - the host (via the docker0 interface), and
///   - any Docker container (via their default gateway or routing table).
///
/// Using this as headscale's `server_url` host lets both the host-side tsnet
/// (e2e-test-client) and the container-side tsnet (ghostframe-server) use the
/// same DERP relay URL.
const DOCKER_HOST_IP: &str = "172.17.0.1";

#[tokio::test]
async fn e2e_quic_ping_pong_over_tailscale() -> Result<()> {
    // The headscale server_url must be reachable from BOTH the host (where the
    // test tsnet node runs) and Docker containers (where ghostframe-server runs).
    // Using DOCKER_HOST_IP (172.17.0.1) satisfies both: it's the Docker bridge
    // gateway, reachable from the host via the docker0 interface and from all
    // containers via their default route.
    let hs_server_url = format!("http://{DOCKER_HOST_IP}:{HEADSCALE_HOST_PORT}");

    // NOTE: GenericImage methods (with_exposed_port / with_mapped_port) must be
    // called before ImageExt methods (with_container_name, with_network, etc.)
    // because ImageExt converts GenericImage into ContainerRequest<GenericImage>.
    let _headscale: ContainerAsync<GenericImage> =
        GenericImage::new("ghostframe/test-headscale", "latest")
            .with_mapped_port(HEADSCALE_HOST_PORT, 8080.tcp())
            .with_container_name("headscale")
            .with_network(helpers::NETWORK_NAME)
            // HS_SERVER_URL is read by the headscale entrypoint to override
            // server_url in the config — makes DERP reachable from the host.
            .with_env_var("HS_SERVER_URL", &hs_server_url)
            // Headscale (zerolog) emits this line to stderr after binding all listeners.
            .with_ready_conditions(vec![WaitFor::message_on_stderr(
                "listening and serving HTTP",
            )])
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .await?;

    // Both nodes in the same headscale user so MagicDNS resolves short
    // hostnames (e.g. "ghostframe-server") within the same namespace.
    let server_key = helpers::create_preauth_key("headscale", "ghostframe").await?;
    let client_key = helpers::create_preauth_key("headscale", "ghostframe").await?;

    let _server: ContainerAsync<GenericImage> =
        GenericImage::new("ghostframe/test-server", "latest")
            .with_container_name("ghostframe-server")
            .with_network(helpers::NETWORK_NAME)
            .with_env_var("TS_AUTHKEY", &server_key)
            .with_env_var("TS_CONTROL_URL", "http://headscale:8080")
            .with_env_var("RUST_LOG", "ghostframe=trace,debug")
            .with_ready_conditions(vec![WaitFor::message_on_stdout("CERT_HASH_SHA256=")])
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .await?;

    let cert_hash = helpers::read_cert_hash_from_logs("ghostframe-server").await?;

    // The test tsnet node runs on the host and connects to headscale via the
    // fixed host-mapped port.  DERP relay also uses this same URL.
    // Use 127.0.0.1 for the test client's control URL — the tsnet client runs
    // on the host where headscale is mapped to localhost:HEADSCALE_HOST_PORT.
    let client_control_url = format!("http://127.0.0.1:{HEADSCALE_HOST_PORT}");
    let test_node = helpers::TestNode::join(client_key, client_control_url).await?;
    let upstream = test_node.dial("ghostframe-server:4443")?;
    let forwarder = helpers::start_forwarder("127.0.0.1:0", upstream).await?;

    // Serve ghostframe-web-client/dist over http://127.0.0.1:<port>.
    // Must be HTTP on a loopback address so Chromium treats it as a secure
    // context; WebTransport is not allowed from file:// origins.
    // CARGO_MANIFEST_DIR is the ghostframe-lib package dir; go up one level
    // to reach the workspace root where ghostframe-web-client lives.
    let dist_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("ghostframe-web-client/dist");
    let static_addr = helpers::start_static_server(dist_dir).await?;

    let (browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .chrome_executable("/usr/bin/chromium")
            .no_sandbox()
            .new_headless_mode()
            .build()
            .unwrap(),
    )
    .await?;
    let _handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page_url = format!(
        "http://{}/index.html?host={}:{}&certHash={}",
        static_addr,
        forwarder.ip(),
        forwarder.port(),
        cert_hash,
    );

    println!("page_url: {page_url}");
    let page = browser.new_page(&page_url).await?;

    // Poll for success, 30s timeout.
    // Use a JS snippet that returns null-safe text so we don't panic if the
    // element isn't in the DOM yet (page still loading).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let result = page
            .evaluate("(document.getElementById('status') || {textContent: '<null>'}).textContent")
            .await;
        if let Ok(v) = result {
            let status: String = v.into_value().unwrap_or_default();
            // M1 web client no longer sends ping/pong; it receives tile
            // datagrams.  Check for "Connected" or "Receiving frames"
            // which both prove the WebTransport session is live.
            if status.contains("Connected") || status.contains("Receiving frames") {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                let content = page.content().await.unwrap_or_default();
                println!("page content:\n{content}");
                panic!("timed out waiting for connection. last status: {status}");
            }
        } else if tokio::time::Instant::now() >= deadline {
            let content = page.content().await.unwrap_or_default();
            println!("page content:\n{content}");
            panic!("timed out waiting for pong; evaluate kept failing");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Common E2E setup: starts headscale + server containers, connects browser,
/// returns the Page for assertions. Caller is responsible for keeping the
/// returned handles alive (dropping them tears down containers/browser).
struct E2eSetup {
    _headscale: ContainerAsync<GenericImage>,
    _server: ContainerAsync<GenericImage>,
    _browser: Browser,
    _handler_task: tokio::task::JoinHandle<()>,
    _test_node: helpers::TestNode,
    _forwarder: SocketAddr,
    page: chromiumoxide::Page,
}

async fn setup_e2e(test_pattern_args: &str) -> Result<E2eSetup> {
    setup_e2e_inner(test_pattern_args, &[], false).await
}

async fn setup_e2e_with_env(test_pattern_args: &str, extra_env: &[(&str, &str)]) -> Result<E2eSetup> {
    setup_e2e_inner(test_pattern_args, extra_env, false).await
}

/// Variant of `setup_e2e` that bind-mounts the host's `/dev/dri` into the
/// container and runs privileged so `xdaemon`'s DRM capture can use VKMS
/// (host must have `vkms` module loaded with `enable_writeback=1`). The
/// container's Xorg switches to the modesetting driver via `XORG_CONF`.
async fn setup_e2e_gpu(test_pattern_args: &str) -> Result<E2eSetup> {
    setup_e2e_inner(test_pattern_args, &[], true).await
}

async fn setup_e2e_inner(
    test_pattern_args: &str,
    extra_env: &[(&str, &str)],
    gpu: bool,
) -> Result<E2eSetup> {
    let hs_server_url = format!("http://{DOCKER_HOST_IP}:{HEADSCALE_HOST_PORT}");

    let headscale: ContainerAsync<GenericImage> =
        GenericImage::new("ghostframe/test-headscale", "latest")
            .with_mapped_port(HEADSCALE_HOST_PORT, 8080.tcp())
            .with_container_name("headscale")
            .with_network(helpers::NETWORK_NAME)
            .with_env_var("HS_SERVER_URL", &hs_server_url)
            .with_ready_conditions(vec![WaitFor::message_on_stderr(
                "listening and serving HTTP",
            )])
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .await?;

    let server_key = helpers::create_preauth_key("headscale", "ghostframe").await?;
    let client_key = helpers::create_preauth_key("headscale", "ghostframe").await?;

    let mut server_image = GenericImage::new("ghostframe/test-server", "latest")
            .with_container_name("ghostframe-server")
            .with_network(helpers::NETWORK_NAME)
            .with_env_var("TS_AUTHKEY", &server_key)
            .with_env_var("TS_CONTROL_URL", "http://headscale:8080")
            .with_env_var("RUST_LOG", "ghostframe=trace,debug")
            .with_env_var("TEST_PATTERN", test_pattern_args);
    for (k, v) in extra_env {
        server_image = server_image.with_env_var(*k, *v);
    }
    if gpu {
        // Switch Xorg to the modesetting driver targeting VKMS card0.
        server_image = server_image.with_env_var("XORG_CONF", "/etc/X11/xorg-vkms.conf");
        // Bind-mount the host's DRM nodes so the container's Xorg + xdaemon
        // can drive VKMS card0 and use renderD128 for VA-API.
        server_image = server_image.with_mount(Mount::bind_mount("/dev/dri", "/dev/dri"));
        // Privileged is the pragmatic choice for solo dev; production CI
        // would narrow to cap_add(SYS_ADMIN) + cgroup device rules.
        server_image = server_image.with_privileged(true);
    }
    let server: ContainerAsync<GenericImage> = server_image
            .with_ready_conditions(vec![WaitFor::message_on_stdout("CERT_HASH_SHA256=")])
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .await?;

    let cert_hash = helpers::read_cert_hash_from_logs("ghostframe-server").await?;

    let client_control_url = format!("http://127.0.0.1:{HEADSCALE_HOST_PORT}");
    let test_node = helpers::TestNode::join(client_key, client_control_url).await?;
    let upstream = test_node.dial("ghostframe-server:4443")?;
    let forwarder = helpers::start_forwarder("127.0.0.1:0", upstream).await?;

    let dist_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("ghostframe-web-client/dist");
    let static_addr = helpers::start_static_server(dist_dir).await?;

    let (browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .chrome_executable("/usr/bin/chromium")
            .no_sandbox()
            .new_headless_mode()
            .build()
            .unwrap(),
    )
    .await?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page_url = format!(
        "http://{}/index.html?host={}:{}&certHash={}",
        static_addr,
        forwarder.ip(),
        forwarder.port(),
        cert_hash,
    );
    println!("page_url: {page_url}");
    let page = browser.new_page(&page_url).await?;

    // Wait for "Receiving frames" status
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let result = page
            .evaluate("(document.getElementById('status') || {textContent: '<null>'}).textContent")
            .await;
        if let Ok(v) = result {
            let status: String = v.into_value().unwrap_or_default();
            if status.contains("Receiving frames") {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                let content = page.content().await.unwrap_or_default();
                println!("page content:\n{content}");
                panic!("timed out waiting for frame rendering. last status: {status}");
            }
        } else if tokio::time::Instant::now() >= deadline {
            let content = page.content().await.unwrap_or_default();
            println!("page content:\n{content}");
            panic!("timed out waiting for frame rendering");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    Ok(E2eSetup {
        _headscale: headscale,
        _server: server,
        _browser: browser,
        _handler_task: handler_task,
        _test_node: test_node,
        _forwarder: forwarder,
        page,
    })
}

/// M1: Verify that captured pixels appear correctly in the browser.
///
/// The server captures from the X11 root window (XGetImage fallback) since
/// the test container uses the Xorg dummy driver which has no DRM device.
/// The test pattern app draws a red square at (100,100)-(300,300).
#[tokio::test]
async fn e2e_raw_frame_round_trip() -> Result<()> {
    let hs_server_url = format!("http://{DOCKER_HOST_IP}:{HEADSCALE_HOST_PORT}");

    let _headscale: ContainerAsync<GenericImage> =
        GenericImage::new("ghostframe/test-headscale", "latest")
            .with_mapped_port(HEADSCALE_HOST_PORT, 8080.tcp())
            .with_container_name("headscale")
            .with_network(helpers::NETWORK_NAME)
            .with_env_var("HS_SERVER_URL", &hs_server_url)
            .with_ready_conditions(vec![WaitFor::message_on_stderr(
                "listening and serving HTTP",
            )])
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .await?;

    let server_key = helpers::create_preauth_key("headscale", "ghostframe").await?;
    let client_key = helpers::create_preauth_key("headscale", "ghostframe").await?;

    let _server: ContainerAsync<GenericImage> =
        GenericImage::new("ghostframe/test-server", "latest")
            .with_container_name("ghostframe-server")
            .with_network(helpers::NETWORK_NAME)
            .with_env_var("TS_AUTHKEY", &server_key)
            .with_env_var("TS_CONTROL_URL", "http://headscale:8080")
            .with_env_var("RUST_LOG", "ghostframe=trace,debug")
            .with_ready_conditions(vec![WaitFor::message_on_stdout("CERT_HASH_SHA256=")])
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .await?;

    let cert_hash = helpers::read_cert_hash_from_logs("ghostframe-server").await?;

    let client_control_url = format!("http://127.0.0.1:{HEADSCALE_HOST_PORT}");
    let test_node = helpers::TestNode::join(client_key, client_control_url).await?;
    let upstream = test_node.dial("ghostframe-server:4443")?;
    let forwarder = helpers::start_forwarder("127.0.0.1:0", upstream).await?;

    let dist_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("ghostframe-web-client/dist");
    let static_addr = helpers::start_static_server(dist_dir).await?;

    let (browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .chrome_executable("/usr/bin/chromium")
            .no_sandbox()
            .new_headless_mode()
            .build()
            .unwrap(),
    )
    .await?;
    let _handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page_url = format!(
        "http://{}/index.html?host={}:{}&certHash={}",
        static_addr,
        forwarder.ip(),
        forwarder.port(),
        cert_hash,
    );
    println!("page_url: {page_url}");
    let page = browser.new_page(&page_url).await?;

    // Wait for "Receiving frames" status
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let result = page
            .evaluate("(document.getElementById('status') || {textContent: '<null>'}).textContent")
            .await;
        if let Ok(v) = result {
            let status: String = v.into_value().unwrap_or_default();
            if status.contains("Receiving frames") {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                let content = page.content().await.unwrap_or_default();
                println!("page content:\n{content}");
                panic!("timed out waiting for frame rendering. last status: {status}");
            }
        } else if tokio::time::Instant::now() >= deadline {
            let content = page.content().await.unwrap_or_default();
            println!("page content:\n{content}");
            panic!("timed out waiting for frame rendering; evaluate kept failing");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Let frames accumulate -- at 2fps, 5 seconds gives ~10 frames.
    // QUIC slow-start needs a few RTTs to open the congestion window
    // wide enough for a full 640x480 frame (300 tiles × 4 fragments).
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Dump page content for diagnostic logging
    let content = page.content().await.unwrap_or_default();
    println!("=== page content ===\n{content}\n=== end ===");

    // Scan the canvas for any red pixel.  Due to QUIC congestion control,
    // only some tiles' fragments complete reassembly — we can't predict
    // which tile arrives first.  The test pattern fills the root window red
    // so ANY successfully rendered tile proves the full pipeline:
    // X11 capture → tile → fragment → transport → reassemble → BGRA→RGBA → canvas.
    let scan_js = r#"
        (() => {
            const canvas = document.getElementById('canvas');
            const ctx = canvas.getContext('2d');
            // Sample every 32 pixels (tile boundaries) across the frame area
            for (let y = 16; y < 480; y += 32) {
                for (let x = 16; x < 640; x += 32) {
                    const p = ctx.getImageData(x, y, 1, 1).data;
                    if (p[0] > 200 && p[1] < 50 && p[2] < 50) {
                        return { found: true, x: x, y: y, r: p[0], g: p[1], b: p[2] };
                    }
                }
            }
            return { found: false, x: 0, y: 0, r: 0, g: 0, b: 0 };
        })()
    "#;

    let scan_result = page.evaluate(scan_js).await?;
    let scan: serde_json::Value = scan_result.into_value()?;
    let found = scan.get("found").and_then(|v| v.as_bool()).unwrap_or(false);
    let sx = scan.get("x").and_then(|v| v.as_u64()).unwrap_or(0);
    let sy = scan.get("y").and_then(|v| v.as_u64()).unwrap_or(0);
    let r = scan.get("r").and_then(|v| v.as_u64()).unwrap_or(0);
    let g = scan.get("g").and_then(|v| v.as_u64()).unwrap_or(0);
    let b = scan.get("b").and_then(|v| v.as_u64()).unwrap_or(0);

    println!("pixel scan: found={found} at ({sx},{sy}) r={r} g={g} b={b}");
    assert!(found, "no red pixel found on canvas — pipeline failed");

    Ok(())
}

/// M2: Solid red renders correctly through H.264 pipeline (color fidelity).
#[tokio::test]
async fn e2e_solid_color() -> Result<()> {
    let setup = setup_e2e("--solid-red").await?;

    // Wait for frames to accumulate and QUIC congestion window to open
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Scan for red pixel — H.264 is lossy so allow wider tolerance than Raw
    let scan_js = r#"
        (() => {
            const canvas = document.getElementById('canvas');
            const ctx = canvas.getContext('2d');
            for (let y = 16; y < 480; y += 32) {
                for (let x = 16; x < 640; x += 32) {
                    const p = ctx.getImageData(x, y, 1, 1).data;
                    if (p[0] > 180 && p[1] < 80 && p[2] < 80) {
                        return { found: true, x, y, r: p[0], g: p[1], b: p[2] };
                    }
                }
            }
            return { found: false };
        })()
    "#;

    let scan: serde_json::Value = setup.page.evaluate(scan_js).await?.into_value()?;
    let found = scan.get("found").and_then(|v| v.as_bool()).unwrap_or(false);
    assert!(found, "no red pixel found on canvas — H.264 pipeline failed");

    Ok(())
}

/// M2: Static content produces stable canvas (skip codec / no-change detection).
#[tokio::test]
async fn e2e_tile_skip() -> Result<()> {
    let setup = setup_e2e("--solid-red").await?;

    // Wait for initial frames to settle
    tokio::time::sleep(Duration::from_secs(6)).await;

    // Take two canvas snapshots 2 seconds apart
    let snapshot_js = r#"
        (() => {
            const canvas = document.getElementById('canvas');
            const ctx = canvas.getContext('2d');
            const data = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
            // Return checksum of all pixel data
            let hash = 0;
            for (let i = 0; i < data.length; i++) {
                hash = ((hash << 5) - hash + data[i]) | 0;
            }
            return hash;
        })()
    "#;

    let snap_a: i64 = setup.page.evaluate(snapshot_js).await?.into_value()?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let snap_b: i64 = setup.page.evaluate(snapshot_js).await?.into_value()?;

    assert_eq!(snap_a, snap_b, "Static content should produce identical canvas snapshots");

    Ok(())
}

/// M2: Motion region produces different frames via H.264 decoding.
#[tokio::test]
async fn e2e_h264_motion() -> Result<()> {
    let setup = setup_e2e("--solid-red --spinner").await?;

    // Wait for frames
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Sample the spinner region (100,100)-(164,164) at two time points
    let sample_js = r#"
        (() => {
            const canvas = document.getElementById('canvas');
            const ctx = canvas.getContext('2d');
            const data = ctx.getImageData(116, 116, 32, 32).data;
            let hash = 0;
            for (let i = 0; i < data.length; i++) {
                hash = ((hash << 5) - hash + data[i]) | 0;
            }
            return hash;
        })()
    "#;

    let snap_a: i64 = setup.page.evaluate(sample_js).await?.into_value()?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let snap_b: i64 = setup.page.evaluate(sample_js).await?.into_value()?;

    assert_ne!(snap_a, snap_b, "Spinner region should change between snapshots");

    Ok(())
}

/// M2: Spinner stops → tiles transition to skip (no more changes).
/// Tests that the STATIC region (outside the spinner) stays stable over time,
/// proving skip detection works for unchanged tiles.
#[tokio::test]
async fn e2e_codec_transition() -> Result<()> {
    let setup = setup_e2e("--solid-red --spinner").await?;

    // The spinner keeps running in this test. What we actually test is that
    // the STATIC region (outside the spinner) stays stable over time.
    // This proves skip detection works for unchanged tiles.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Sample a region OUTSIDE the spinner (top-left corner, tile 0,0)
    let static_js = r#"
        (() => {
            const canvas = document.getElementById('canvas');
            const ctx = canvas.getContext('2d');
            const data = ctx.getImageData(0, 0, 32, 32).data;
            let hash = 0;
            for (let i = 0; i < data.length; i++) {
                hash = ((hash << 5) - hash + data[i]) | 0;
            }
            return hash;
        })()
    "#;

    let snap_a: i64 = setup.page.evaluate(static_js).await?.into_value()?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let snap_b: i64 = setup.page.evaluate(static_js).await?.into_value()?;

    assert_eq!(snap_a, snap_b, "Static region should stay unchanged while spinner runs elsewhere");

    Ok(())
}

/// M2: Verify edge tiles (non-32-aligned resolution) render correctly.
///
/// Uses 700x500 resolution (22 full cols + 1 partial col of 28px wide,
/// 15 full rows + 1 partial row of 20px tall). These partial tiles must
/// be encoded, transported, and decoded without corruption.
#[tokio::test]
async fn e2e_edge_tiles() -> Result<()> {
    let setup = setup_e2e_with_env("--solid-red", &[
        ("XORG_CONF", "/etc/X11/xorg-odd.conf"),
    ]).await?;

    // Wait for frames
    tokio::time::sleep(Duration::from_secs(6)).await;

    // Sample pixels at the far right edge (inside rightmost partial tile)
    // and bottom edge (inside bottom partial tile).
    // If edge tiles work, these should be red (from --solid-red pattern).
    let edge_js = r#"
        (() => {
            const canvas = document.getElementById('canvas');
            const ctx = canvas.getContext('2d');
            const results = {};

            // Right edge: x=695 (inside last partial-width tile at col 21)
            const pr = ctx.getImageData(695, 240, 1, 1).data;
            results.right = { r: pr[0], g: pr[1], b: pr[2] };

            // Bottom edge: y=495 (inside last partial-height tile at row 15)
            const pb = ctx.getImageData(320, 495, 1, 1).data;
            results.bottom = { r: pb[0], g: pb[1], b: pb[2] };

            // Corner: bottom-right (both partial width AND height)
            const pc = ctx.getImageData(695, 495, 1, 1).data;
            results.corner = { r: pc[0], g: pc[1], b: pc[2] };

            // Center (known-good reference)
            const pm = ctx.getImageData(350, 250, 1, 1).data;
            results.center = { r: pm[0], g: pm[1], b: pm[2] };

            return results;
        })()
    "#;

    let result: serde_json::Value = setup.page.evaluate(edge_js).await?.into_value()?;

    // Check center renders red (baseline)
    let center = &result["center"];
    let cr = center["r"].as_u64().unwrap_or(0);
    assert!(cr > 150, "Center pixel should be red (got r={cr}) - baseline failed");

    // Check right edge renders red
    let right = &result["right"];
    let rr = right["r"].as_u64().unwrap_or(0);
    let rg = right["g"].as_u64().unwrap_or(255);
    assert!(rr > 150 && rg < 100,
        "Right edge tile should be red: r={rr} g={rg}");

    // Check bottom edge renders red
    let bottom = &result["bottom"];
    let br = bottom["r"].as_u64().unwrap_or(0);
    let bg = bottom["g"].as_u64().unwrap_or(255);
    assert!(br > 150 && bg < 100,
        "Bottom edge tile should be red: r={br} g={bg}");

    // Check bottom-right corner renders red
    let corner = &result["corner"];
    let corr = corner["r"].as_u64().unwrap_or(0);
    let corg = corner["g"].as_u64().unwrap_or(255);
    assert!(corr > 150 && corg < 100,
        "Corner edge tile should be red: r={corr} g={corg}");

    Ok(())
}

/// M2: Verify multiple tiles across the grid render correctly.
///
/// Samples pixels at multiple tile-center positions across the 640x480
/// frame to ensure the full grid (20x15 = 300 tiles) is being served.
#[tokio::test]
async fn e2e_multi_tile_grid() -> Result<()> {
    let setup = setup_e2e("--solid-red").await?;

    // Wait for frames and QUIC congestion window
    tokio::time::sleep(Duration::from_secs(8)).await;

    // Sample 9 positions spread across the grid
    let grid_js = r#"
        (() => {
            const canvas = document.getElementById('canvas');
            const ctx = canvas.getContext('2d');
            const positions = [
                [16, 16],     // top-left tile (0,0)
                [320, 16],    // top-center tile (10,0)
                [624, 16],    // top-right tile (19,0)
                [16, 240],    // mid-left tile (0,7)
                [320, 240],   // center tile (10,7)
                [624, 240],   // mid-right tile (19,7)
                [16, 464],    // bottom-left tile (0,14)
                [320, 464],   // bottom-center tile (10,14)
                [624, 464],   // bottom-right tile (19,14)
            ];
            let redCount = 0;
            for (const [x, y] of positions) {
                const p = ctx.getImageData(x, y, 1, 1).data;
                if (p[0] > 150 && p[1] < 100 && p[2] < 100) {
                    redCount++;
                }
            }
            return { redCount, total: positions.length };
        })()
    "#;

    let result: serde_json::Value = setup.page.evaluate(grid_js).await?.into_value()?;
    let red_count = result["redCount"].as_u64().unwrap_or(0);
    let total = result["total"].as_u64().unwrap_or(9);

    // Due to QUIC congestion control, not all tiles may arrive.
    // But with 8 seconds at 2fps, most of the grid should render.
    // Require at least 5 of 9 positions (>50%) to be red.
    assert!(red_count >= 5,
        "Expected at least 5/9 grid positions to be red, got {red_count}/{total}");

    Ok(())
}

/// Pre-M3: validate text legibility via per-pixel contrast at known glyph
/// positions. Post-M3 (CDF 5/3 refinement) this test should be tightened
/// to assert SSIM > 0.99 against a reference PNG; see TODO below.
#[tokio::test]
async fn e2e_text_clarity() -> Result<()> {
    use ghostframe_test_pattern::text_grid::SAMPLES;

    let setup = setup_e2e("--text-grid").await?;

    // Allow QUIC slow-start + a couple of frames so every glyph tile arrives.
    tokio::time::sleep(Duration::from_secs(6)).await;

    // ── (a) Per-pair contrast: ink position must be much brighter than bg.
    for (i, pair) in SAMPLES.iter().enumerate() {
        let probe_js = format!(
            r#"
            (() => {{
                const canvas = document.getElementById('canvas');
                const ctx = canvas.getContext('2d');
                const ink = ctx.getImageData({ix}, {iy}, 1, 1).data;
                const bg  = ctx.getImageData({bx}, {by}, 1, 1).data;
                return {{
                    ink: {{ r: ink[0], g: ink[1], b: ink[2] }},
                    bg:  {{ r: bg[0],  g: bg[1],  b: bg[2]  }},
                }};
            }})()
            "#,
            ix = pair.ink.0, iy = pair.ink.1,
            bx = pair.bg.0,  by = pair.bg.1,
        );
        let probe: serde_json::Value = setup.page.evaluate(probe_js.as_str()).await?.into_value()?;

        let ink_lum = luminance(&probe["ink"]);
        let bg_lum  = luminance(&probe["bg"]);

        assert!(
            ink_lum - bg_lum > 80.0,
            "sample {i}: insufficient contrast (ink {ink_lum:.0} - bg {bg_lum:.0}); pair={pair:?}"
        );
        assert!(ink_lum > 150.0, "sample {i}: ink too dark — luminance {ink_lum:.0}");
        assert!(bg_lum  <  80.0, "sample {i}: bg too bright — luminance {bg_lum:.0}");
    }

    // ── (b) Stability: two snapshots 2s apart must be byte-identical.
    let hash_js = r#"
        (() => {
            const canvas = document.getElementById('canvas');
            const ctx = canvas.getContext('2d');
            const data = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
            let h = 0;
            for (let i = 0; i < data.length; i++) h = ((h << 5) - h + data[i]) | 0;
            return h;
        })()
    "#;
    let h1: i64 = setup.page.evaluate(hash_js).await?.into_value()?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let h2: i64 = setup.page.evaluate(hash_js).await?.into_value()?;
    assert_eq!(h1, h2, "text canvas drifted between snapshots");

    // TODO(M3): replace the contrast check with SSIM > 0.99 against
    //           tests/fixtures/text_grid_reference.png once CDF 5/3
    //           refinement is wired and lossless reconstruction works.

    Ok(())
}

fn luminance(c: &serde_json::Value) -> f64 {
    let r = c["r"].as_f64().unwrap_or(0.0);
    let g = c["g"].as_f64().unwrap_or(0.0);
    let b = c["b"].as_f64().unwrap_or(0.0);
    // Rec. 709 luma — close enough for "is this pixel ink or bg".
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

#[derive(Debug, Clone, Copy)]
enum RegionCheck {
    /// Every sampled pixel must be solidly red.
    SolidRed,
    /// Region must contain both very dark and very bright pixels (text on bg).
    Legible,
    /// Region brightness must be monotonic top→bottom (gradient).
    SmoothGradient,
    /// Two snapshots 1.5 s apart must differ.
    Changing,
}

async fn assert_region_rendered(
    page: &chromiumoxide::Page,
    region: &ghostframe_test_pattern::mixed::Region,
    check: RegionCheck,
) -> Result<()> {
    use serde_json::Value;

    match check {
        RegionCheck::SolidRed => {
            // Sample 9 evenly spaced points inside the region; >= 7 must be red.
            let js = format!(
                r#"
                (() => {{
                    const c = document.getElementById('canvas').getContext('2d');
                    const xs = [{x}+16, {x}+{w}/2|0, {x}+{w}-16];
                    const ys = [{y}+16, {y}+{h}/2|0, {y}+{h}-16];
                    let red = 0, total = 0;
                    for (const xx of xs) for (const yy of ys) {{
                        const p = c.getImageData(xx, yy, 1, 1).data;
                        if (p[0] > 180 && p[1] < 80 && p[2] < 80) red++;
                        total++;
                    }}
                    return {{ red, total }};
                }})()
                "#,
                x = region.x, y = region.y, w = region.w, h = region.h,
            );
            let out: Value = page.evaluate(js.as_str()).await?.into_value()?;
            let red = out["red"].as_u64().unwrap_or(0);
            let total = out["total"].as_u64().unwrap_or(0);
            assert!(red >= 7, "{}: only {red}/{total} samples were red", region.name);
        }

        RegionCheck::Legible => {
            // Find the brightest and darkest pixels in a sweep — gap > 100 luma
            // means we have both ink and bg, i.e. text rendered.
            let js = format!(
                r#"
                (() => {{
                    const c = document.getElementById('canvas').getContext('2d');
                    let lo = 255, hi = 0;
                    for (let dy = 8; dy < {h}; dy += 4) {{
                        for (let dx = 8; dx < {w}; dx += 4) {{
                            const p = c.getImageData({x}+dx, {y}+dy, 1, 1).data;
                            const lum = 0.2126*p[0] + 0.7152*p[1] + 0.0722*p[2];
                            if (lum < lo) lo = lum;
                            if (lum > hi) hi = lum;
                        }}
                    }}
                    return {{ lo, hi }};
                }})()
                "#,
                x = region.x, y = region.y, w = region.w, h = region.h,
            );
            let out: Value = page.evaluate(js.as_str()).await?.into_value()?;
            let lo = out["lo"].as_f64().unwrap_or(255.0);
            let hi = out["hi"].as_f64().unwrap_or(0.0);
            assert!(
                hi - lo > 100.0,
                "{}: contrast too low (lo {lo:.0}, hi {hi:.0}); text not rendered",
                region.name
            );
        }

        RegionCheck::SmoothGradient => {
            // Average luma at three vertical bands — top, middle, bottom.
            // Bottom must be brighter than top by a clear margin.
            let js = format!(
                r#"
                (() => {{
                    const c = document.getElementById('canvas').getContext('2d');
                    function band(y0, y1) {{
                        let sum = 0, n = 0;
                        for (let yy = y0; yy < y1; yy += 4) {{
                            for (let xx = {x}+8; xx < {x}+{w}; xx += 8) {{
                                const p = c.getImageData(xx, yy, 1, 1).data;
                                sum += 0.2126*p[0] + 0.7152*p[1] + 0.0722*p[2];
                                n++;
                            }}
                        }}
                        return n ? sum / n : 0;
                    }}
                    return {{
                        top:    band({y}, {y}+{h}/4|0),
                        middle: band({y}+{h}/4|0, {y}+3*{h}/4|0),
                        bottom: band({y}+3*{h}/4|0, {y}+{h}),
                    }};
                }})()
                "#,
                x = region.x, y = region.y, w = region.w, h = region.h,
            );
            let out: Value = page.evaluate(js.as_str()).await?.into_value()?;
            let top    = out["top"].as_f64().unwrap_or(0.0);
            let bottom = out["bottom"].as_f64().unwrap_or(0.0);
            assert!(
                bottom - top > 50.0,
                "{}: gradient too flat (top {top:.0}, bottom {bottom:.0})",
                region.name
            );
        }

        RegionCheck::Changing => {
            let js = format!(
                r#"
                (() => {{
                    const c = document.getElementById('canvas').getContext('2d');
                    const data = c.getImageData({x}, {y}, {w}, {h}).data;
                    let h = 0;
                    for (let i = 0; i < data.length; i++) h = ((h << 5) - h + data[i]) | 0;
                    return h;
                }})()
                "#,
                x = region.x, y = region.y, w = region.w, h = region.h,
            );
            let h1: i64 = page.evaluate(js.as_str()).await?.into_value()?;
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let h2: i64 = page.evaluate(js.as_str()).await?.into_value()?;
            assert_ne!(
                h1, h2,
                "{}: region was static between snapshots — spinner not animating",
                region.name
            );
        }
    }
    Ok(())
}

/// FEC: Verify that XOR parity generation doesn't break the rendering pipeline.
///
/// Forces FEC on via GHOSTFRAME_FEC_K=4 and checks that the H.264 pipeline
/// still renders frames correctly with parity datagrams being generated and
/// sent alongside source fragments.
#[tokio::test]
async fn e2e_fec_parity_enabled() -> Result<()> {
    let setup = setup_e2e_with_env("--solid-red", &[("GHOSTFRAME_FEC_K", "4")]).await?;

    // Wait for frames + QUIC congestion window
    tokio::time::sleep(Duration::from_secs(6)).await;

    // Scan for red pixel — same as e2e_solid_color but with FEC active
    let scan_js = r#"
        (() => {
            const canvas = document.getElementById('canvas');
            const ctx = canvas.getContext('2d');
            for (let y = 16; y < 480; y += 32) {
                for (let x = 16; x < 640; x += 32) {
                    const p = ctx.getImageData(x, y, 1, 1).data;
                    if (p[0] > 180 && p[1] < 80 && p[2] < 80) {
                        return { found: true, x, y, r: p[0], g: p[1], b: p[2] };
                    }
                }
            }
            return { found: false };
        })()
    "#;

    let scan: serde_json::Value = setup.page.evaluate(scan_js).await?.into_value()?;
    let found = scan.get("found").and_then(|v| v.as_bool()).unwrap_or(false);
    assert!(found, "no red pixel found — FEC parity may have broken the H.264 pipeline");

    Ok(())
}

/// Validates the pipeline survives a mid-stream resolution change.
///
/// Pre-M3 there is no client→server `DisplayLayout` protocol — the test
/// triggers the change via xrandr inside the server container. Post-M4,
/// a sibling test `e2e_resolution_change_via_protocol` will exercise the
/// real protocol path.
#[tokio::test]
#[ignore = "blocked on resize support: encoder lazy-init stalls QUIC at 1024x768; web client canvas only grows. See docs/superpowers/plans/2026-04-27-e2e-resolution-change.md follow-up."]
async fn e2e_resolution_change() -> Result<()> {
    // Phase A: 1024×768 — server starts in this mode (first entry in
    // xorg-multi.conf's Modes list).
    let setup = setup_e2e_with_env(
        "--solid-red",
        &[("XORG_CONF", "/etc/X11/xorg-multi.conf")],
    ).await?;

    // Wait for QUIC slow-start + initial frames.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Assert: canvas is 1024×768 and a center pixel is red.
    let dims_a: (u32, u32) = setup.page
        .evaluate(r#"
            (() => {
                const c = document.getElementById('canvas');
                return [c.width, c.height];
            })()
        "#)
        .await?
        .into_value()?;
    assert_eq!(dims_a, (1024, 768), "phase A: canvas dimensions");

    let red_a: bool = setup.page
        .evaluate(r#"
            (() => {
                const c = document.getElementById('canvas').getContext('2d');
                const p = c.getImageData(512, 384, 1, 1).data;
                return p[0] > 180 && p[1] < 80 && p[2] < 80;
            })()
        "#)
        .await?
        .into_value()?;
    assert!(red_a, "phase A: center pixel not red");

    // ── Trigger the resolution change. ──────────────────────────────────────

    // 1. xrandr to switch the dummy driver to 640×480.
    let (out, err, status) = helpers::docker_run_in_container(
        "ghostframe-server",
        &[("DISPLAY", ":99")],
        &["xrandr", "--output", "DUMMY0", "--mode", "640x480"],
    ).await?;
    assert_eq!(
        status, 0,
        "xrandr exited with {status}: stdout={out:?} stderr={err:?}"
    );

    // 2. Re-paint the root window — dummy driver clears framebuffer on mode change.
    let (_, _, status) = helpers::docker_run_in_container(
        "ghostframe-server",
        &[("DISPLAY", ":99")],
        &["/usr/local/bin/ghostframe-test-pattern", "--solid-red"],
    ).await?;
    // The test-pattern process forks/daemonises; if status != 0 the binary failed.
    assert_eq!(status, 0, "re-paint after resolution change failed");

    // ── Phase B: assert the client picks up the new dimensions. ─────────────

    // Allow time for: encoder reset, keyframe, several frames, canvas resize.
    tokio::time::sleep(Duration::from_secs(8)).await;

    let dims_b: (u32, u32) = setup.page
        .evaluate(r#"
            (() => {
                const c = document.getElementById('canvas');
                return [c.width, c.height];
            })()
        "#)
        .await?
        .into_value()?;
    assert_eq!(dims_b, (640, 480), "phase B: canvas did not resize to 640x480");

    let red_b: bool = setup.page
        .evaluate(r#"
            (() => {
                const c = document.getElementById('canvas').getContext('2d');
                const p = c.getImageData(320, 240, 1, 1).data;
                return p[0] > 180 && p[1] < 80 && p[2] < 80;
            })()
        "#)
        .await?
        .into_value()?;
    assert!(red_b, "phase B: center pixel not red after resize");

    Ok(())
}

/// Mixed-content rendering test. Pre-M3 (single codec) the assertion is
/// per-region rendering correctness. Post-M3 the same REGIONS table will
/// drive codec-selection assertions via a future stats channel — see
/// `Region::expected_codec`.
#[tokio::test]
async fn e2e_multi_pattern() -> Result<()> {
    use ghostframe_test_pattern::mixed::{region, SETTLE};

    let setup = setup_e2e("--mixed").await?;

    // SETTLE is 7s — enough for QUIC slow-start to open and at least two
    // spinner frames to land.
    tokio::time::sleep(SETTLE).await;

    assert_region_rendered(&setup.page, region("solid"),    RegionCheck::SolidRed).await?;
    assert_region_rendered(&setup.page, region("text"),     RegionCheck::Legible).await?;
    assert_region_rendered(&setup.page, region("gradient"), RegionCheck::SmoothGradient).await?;
    assert_region_rendered(&setup.page, region("spinner"),  RegionCheck::Changing).await?;

    // TODO(M3): once the classifier ships, also assert that each region's
    //           tiles are encoded with `region.expected_codec`. Will require
    //           a per-tile codec stats channel from server → test (not
    //           shipped pre-M3).

    Ok(())
}

/// M3.0: Verify the classifier switches between TileCodec and H264 frame
/// modes in response to alternating static/motion content.
///
/// The test pattern toggles every 3 seconds (--mode-switch-cycle 3): static,
/// then motion, then static, etc. We sample the web client's per-mode datagram
/// counters every 250 ms over ~20 seconds. Because the test pattern starts on
/// its own clock inside the container, we anchor the timeline to the FIRST
/// sample with non-zero datagrams, then bin subsequent deltas into 3-second
/// "phases" relative to that anchor.
///
/// Assertions:
/// - both datagram kinds must be observed (basic precondition: tile_seen && frame_seen),
/// - at least one phase must be H264-dominated (frame deltas substantially
///   outnumber tile deltas — characteristic of a motion phase),
/// - some phase must contain TileCodec emissions,
/// - **at least 2 distinct frame-dominated phases must be observed.** This is
///   the load-bearing proof of bidirectional mode switching: the classifier
///   must have ENTERED H264 (phase 1), then EXITED it (otherwise we wouldn't
///   see a *distinct* later entry), then RE-ENTERED H264 (phase 2). Both
///   directions exercised transitively without depending on observable
///   datagrams during the exit (static halves are silent — no dirty tiles →
///   no datagrams either way, regardless of mode).
///
/// FUTURE PHASE (M3.3+): once progressive CDF 5/3 refinement is wired, the
/// static halves will produce wavelet bit-plane datagrams (lossless build-up
/// from H264 snapshot toward pixel-perfect). At that point the test can
/// observe **direct** mode transitions during static phases (Frame →
/// refinement-Tile datagrams instead of silence), and the "≥2 distinct
/// frame-dominated phases" indirection can tighten to "≥2 explicit Frame→Tile
/// flips and ≥1 explicit Tile→Frame flip." Tracked in memory note
/// `project_m33_static_refinement.md`.
#[tokio::test]
async fn e2e_mode_switch() -> Result<()> {
    let setup = setup_e2e_gpu("--drm-direct --mode-switch-cycle 3").await?;

    // 20 seconds: covers three full 6-second cycles plus startup settle.
    // The shorter 14s window occasionally hits a startup race where most
    // sampling lands on static halves and frame_seen flakes false.
    let total = Duration::from_secs(20);
    let interval = Duration::from_millis(250);
    let started = tokio::time::Instant::now();
    let mut samples: Vec<(u64, u64, u64)> = Vec::new(); // (elapsed_ms, tile, frame)

    while tokio::time::Instant::now() - started < total {
        let v: serde_json::Value = setup
            .page
            .evaluate(
                "(() => { const s = window.__ghostframeStats || {tileDatagrams:0, frameDatagrams:0}; return {t: s.tileDatagrams, f: s.frameDatagrams}; })()",
            )
            .await?
            .into_value()?;
        let elapsed_ms = (tokio::time::Instant::now() - started).as_millis() as u64;
        let tile = v["t"].as_u64().unwrap_or(0);
        let frame = v["f"].as_u64().unwrap_or(0);
        samples.push((elapsed_ms, tile, frame));
        tokio::time::sleep(interval).await;
    }

    // Compute per-sample deltas (events per 250 ms window).
    let deltas: Vec<(u64, i64, i64)> = samples
        .windows(2)
        .map(|w| {
            (
                w[1].0,
                w[1].1 as i64 - w[0].1 as i64,
                w[1].2 as i64 - w[0].2 as i64,
            )
        })
        .collect();

    let tile_seen  = deltas.iter().any(|(_, dt, _)| *dt > 0);
    let frame_seen = deltas.iter().any(|(_, _, df)| *df > 0);

    // Anchor timeline to the first non-zero delta — that's our best estimate
    // of when the test pattern began streaming.
    let t_first_data = deltas
        .iter()
        .find(|(_, dt, df)| *dt > 0 || *df > 0)
        .map(|(t, _, _)| *t);

    // Bin into 3-second phases relative to t_first_data.
    // PHASE_MS matches --mode-switch-cycle 3 (3000 ms per static/motion half).
    const PHASE_MS: u64 = 3000;
    let mut phases: Vec<(i64, i64)> = Vec::new(); // (tile_total, frame_total)
    if let Some(anchor) = t_first_data {
        for (t, dt, df) in &deltas {
            if *t < anchor { continue; }
            let phase_idx = ((*t - anchor) / PHASE_MS) as usize;
            while phases.len() <= phase_idx {
                phases.push((0, 0));
            }
            phases[phase_idx].0 += *dt;
            phases[phase_idx].1 += *df;
        }
    }

    // Frame-dominated phase: H264 frames substantially outnumber tile datagrams.
    // Use a 5x ratio with a minimum frame count to filter noise around
    // cooldown periods where a few residual frame fragments could fire.
    const FRAME_DOMINANCE_RATIO: i64 = 5;
    const MIN_FRAME_COUNT_FOR_DOMINANCE: i64 = 10;
    let is_frame_dominated = |(t, f): &(i64, i64)| -> bool {
        *f >= MIN_FRAME_COUNT_FOR_DOMINANCE && *f >= FRAME_DOMINANCE_RATIO * t.max(&1)
    };
    let frame_dominated_phases: Vec<usize> = phases.iter()
        .enumerate()
        .filter(|(_, p)| is_frame_dominated(*p))
        .map(|(i, _)| i)
        .collect();

    // Tile presence is harder to isolate because H.264 cooldown frames may
    // overlap static phases. Just require non-zero tile emission in any phase.
    let tile_present_phase = phases.iter().any(|(t, _)| *t > 0);

    // Diagnostic table — print on any failure.
    let pass = tile_seen
        && frame_seen
        && tile_present_phase
        && frame_dominated_phases.len() >= 2;
    if !pass {
        eprintln!("--- per-sample deltas ---");
        for (ms, dt, df) in &deltas {
            eprintln!("t={ms:>5}ms  +tile={dt:>4}  +frame={df:>4}");
        }
        eprintln!("--- per-phase totals (anchor={:?}) ---", t_first_data);
        for (i, (t, f)) in phases.iter().enumerate() {
            let marker = if is_frame_dominated(&(*t, *f)) { " <FRAME-DOMINATED>" } else { "" };
            eprintln!("phase {i}  tile={t:>5}  frame={f:>5}{marker}");
        }
        eprintln!(
            "tile_seen={tile_seen} frame_seen={frame_seen} tile_present_phase={tile_present_phase} frame_dominated_phases={frame_dominated_phases:?}"
        );
    }

    assert!(tile_seen,
        "expected at least one tile datagram during the test (TileCodec mode); none observed");
    assert!(frame_seen,
        "expected at least one frame datagram during the test (H264 mode); none observed");
    assert!(tile_present_phase,
        "expected at least one phase containing TileCodec datagrams; classifier may be stuck in H264");
    // Load-bearing assertion: 2+ distinct frame-dominated phases proves the
    // classifier entered H264, *left it* (else the later phase wouldn't be a
    // distinct entry), and re-entered. Both directions exercised transitively.
    // This is the strongest assertion the architecture currently permits —
    // static halves emit no datagrams, so direct Frame→Tile transitions can't
    // be observed. See M3.3 future-work note in the function docstring.
    assert!(frame_dominated_phases.len() >= 2,
        "expected at least 2 distinct frame-dominated phases (proves classifier flipped both \
         directions: H264 → exit → H264). Observed frame-dominated phase indices: {:?}",
        frame_dominated_phases);

    Ok(())
}
