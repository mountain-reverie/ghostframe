//! Server-side e2e scenario setup: containers, tsnet, forwarders, Weston.
//!
//! Extracted from `tests/e2e.rs::setup_e2e_inner_with_url_extra` so that both
//! Chromium-driven and Firefox-driven tests can share the same container +
//! transport setup without duplicating ~80 lines.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use testcontainers::core::{IntoContainerPort, Mount, WaitFor};
use testcontainers::{runners::AsyncRunner, ContainerAsync, GenericImage, ImageExt};

use crate::harness::containers::{create_preauth_key, TestNode, NETWORK_NAME};
use crate::harness::e2e_certs::{generate as generate_cert, E2eCert};
use crate::harness::transport::{start_forwarder, start_tcp_forwarder};
use crate::harness::weston::{spawn_weston_headless, WestonGuard};

// Default container/port constants. Some are duplicated from tests/e2e.rs
// constants (HEADSCALE_HOST_PORT, DOCKER_HOST_IP) — kept here so the harness
// library is self-contained. If those constants drift, sync them in both
// places.
pub const HEADSCALE_HOST_PORT: u16 = 18080;
pub const DOCKER_HOST_IP: &str = "172.17.0.1";

/// Server-side state for one e2e scenario: containers, tsnet client, port
/// forwarders, optional Weston compositor, and a derived page URL the
/// browser should navigate to.
///
/// All fields are held until drop so containers + forwarders + weston stay
/// alive for the lifetime of the scenario.
/// Dump the server container's logs when a test fails.
///
/// Without this an e2e failure reports a wrong pixel and says nothing about
/// what the server did: the container's stdout never reached the test output,
/// so a CI failure could not distinguish "the server never emitted it" from
/// "the client never rendered it". The container already runs with
/// `RUST_LOG=ghostframe=trace,debug`, and `scene.rs` already reads its logs for
/// telemetry -- this path simply never used the capability.
///
/// On `E2eServerSetup` rather than the per-test wrapper so it covers every
/// harness built on it (Chromium and Firefox alike), and because adding `Drop`
/// to the wrapper would stop tests destructuring it.
///
/// Gated on `std::thread::panicking()` so a passing run stays silent, and
/// tail-only: a trace-level server produces far more than is readable, and the
/// end is where the failure is.
impl Drop for E2eServerSetup {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        // Two passes, because one is not enough at TRACE volume: a failing run
        // produces well over a thousand lines, so a tail alone covers barely a
        // second, and the line that explains the failure is usually a WARN
        // emitted much earlier.
        const TAIL_LINES: usize = 400;
        const MAX_NOTABLE: usize = 120;
        let logs = crate::harness::cleanup::read_server_logs_stripped(&self.server_container_name);
        let lines: Vec<&str> = logs.lines().collect();

        let notable: Vec<&&str> = lines
            .iter()
            .filter(|l| l.contains("WARN") || l.contains("ERROR"))
            .take(MAX_NOTABLE)
            .collect();
        eprintln!(
            "\n===== server container '{}': {} WARN/ERROR of {} lines =====",
            self.server_container_name,
            notable.len(),
            lines.len()
        );
        for l in &notable {
            eprintln!("{l}");
        }

        let start = lines.len().saturating_sub(TAIL_LINES);
        eprintln!(
            "----- tail ({} of {} lines) -----",
            lines.len() - start,
            lines.len()
        );
        for l in &lines[start..] {
            eprintln!("{l}");
        }
        eprintln!("===== end server container logs =====\n");
    }
}

pub struct E2eServerSetup {
    pub _headscale: ContainerAsync<GenericImage>,
    pub _server: ContainerAsync<GenericImage>,
    pub _test_node: Arc<TestNode>,
    /// UDP forwarder address (used for QUIC/WebTransport).
    pub _forwarder: SocketAddr,
    /// TCP forwarder address (used for TLS fallback).
    pub _tcp_forwarder: SocketAddr,
    /// Set only when `webgpu == true`. `None` for headless-mode setups.
    pub xvfb: Option<WestonGuard>,
    pub e2e_cert: E2eCert,
    pub page_url: String,
    pub server_container_name: String,
}

/// Parameters mirroring the existing `setup_e2e_inner_with_url_extra`
/// signature, hoisted into a struct so call sites stay readable.
pub struct E2eServerSpec<'a> {
    pub test_pattern_args: &'a str,
    pub extra_env: &'a [(&'a str, &'a str)],
    pub gpu: bool,
    pub webgpu: bool,
    pub url_query_extra: &'a str,
}

/// Launch headscale + ghostframe-server containers, join tsnet, start
/// UDP/TCP forwarders, and (when `webgpu == true`) spawn a Weston headless
/// compositor with XWayland. Returns the assembled `E2eServerSetup` plus
/// the chosen `page_url`. Does NOT launch a browser.
pub async fn setup_e2e_server(spec: E2eServerSpec<'_>) -> Result<E2eServerSetup> {
    crate::harness::cleanup::cleanup_stale_xvfb_sockets();
    let hs_server_url = format!("http://{DOCKER_HOST_IP}:{HEADSCALE_HOST_PORT}");

    let headscale: ContainerAsync<GenericImage> =
        GenericImage::new("ghostframe/test-headscale", "latest")
            .with_mapped_port(HEADSCALE_HOST_PORT, 8080.tcp())
            .with_container_name("headscale")
            .with_network(NETWORK_NAME)
            .with_env_var("HS_SERVER_URL", &hs_server_url)
            .with_ready_conditions(vec![WaitFor::message_on_stderr(
                "listening and serving HTTP",
            )])
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .await
            .context("start headscale container")?;

    let server_key = create_preauth_key("headscale", "ghostframe").await?;
    let client_key = create_preauth_key("headscale", "ghostframe").await?;

    let e2e_cert = generate_cert(&["localhost", "127.0.0.1"])?;

    let server_container_name = "ghostframe-server".to_string();
    let mut server_image = GenericImage::new("ghostframe/test-server", "latest")
        .with_container_name(&server_container_name)
        .with_network(NETWORK_NAME)
        .with_env_var("TS_AUTHKEY", &server_key)
        .with_env_var("TS_CONTROL_URL", "http://headscale:8080")
        .with_env_var("RUST_LOG", "ghostframe=trace,debug")
        .with_env_var("TEST_PATTERN", spec.test_pattern_args)
        .with_env_var("GHOSTFRAME_WEB_TLS_CERT_PEM", &e2e_cert.cert_pem)
        .with_env_var("GHOSTFRAME_WEB_TLS_KEY_PEM", &e2e_cert.key_pem);
    if spec.gpu {
        server_image = server_image.with_env_var("XORG_CONF", "/etc/X11/xorg-vkms.conf");
        server_image = server_image.with_mount(Mount::bind_mount("/dev/dri", "/dev/dri"));
        server_image = server_image.with_privileged(true);
    }
    // extra_env applied LAST so tests can override defaults like XORG_CONF.
    for (k, v) in spec.extra_env {
        server_image = server_image.with_env_var(*k, *v);
    }
    let server: ContainerAsync<GenericImage> = server_image
        .with_ready_conditions(vec![WaitFor::message_on_stdout("QUIC server ready")])
        .with_startup_timeout(Duration::from_secs(120))
        .start()
        .await
        .context("start ghostframe-server container")?;

    let client_control_url = format!("http://127.0.0.1:{HEADSCALE_HOST_PORT}");
    let test_node = Arc::new(TestNode::join(client_key, client_control_url).await?);
    let upstream = test_node.dial("ghostframe-server:443")?;
    let forwarder = start_forwarder("127.0.0.1:0", upstream).await?;

    let tcp_bind = format!("127.0.0.1:{}", forwarder.port());
    let tcp_forwarder = start_tcp_forwarder(
        &tcp_bind,
        test_node.clone(),
        "ghostframe-server:443".to_string(),
    )
    .await?;

    let xvfb = if spec.webgpu {
        Some(spawn_weston_headless()?)
    } else {
        None
    };

    let page_url = if spec.url_query_extra.is_empty() {
        format!("https://127.0.0.1:{}/", forwarder.port())
    } else {
        let suffix = spec
            .url_query_extra
            .strip_prefix('&')
            .unwrap_or(spec.url_query_extra);
        format!("https://127.0.0.1:{}/?{}", forwarder.port(), suffix)
    };

    Ok(E2eServerSetup {
        _headscale: headscale,
        _server: server,
        _test_node: test_node,
        _forwarder: forwarder,
        _tcp_forwarder: tcp_forwarder,
        xvfb,
        e2e_cert,
        page_url,
        server_container_name,
    })
}

/// Poll `document.getElementById('status').textContent` until it contains
/// "Receiving frames", or `timeout` elapses. Generic over any
/// `BrowserSession` impl so Chromium and Firefox tests share the readiness
/// gate.
///
/// IMPORTANT: this is the STRICT criterion the inline loop in
/// `setup_e2e_inner_with_url_extra` had on master. The page sets status to
/// "Connected!" as soon as the WebTransport CONNECT succeeds, *before* the
/// first datagram arrives — returning on "Connected" would let tests
/// proceed before any frame is actually rendered (every red-pixel scan would
/// then read a default 300x150 canvas and fail). scene.rs's bench loop
/// accepts both because under harsh shaping the first frame may take
/// >30 s; the bench is a different consumer with its own inline wait.
pub async fn wait_for_frames<B: crate::harness::browser::BrowserSession>(
    browser: &mut B,
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let status: String = browser
            .evaluate::<String>(
                "(document.getElementById('status') || {textContent: '<null>'}).textContent",
            )
            .await
            .unwrap_or_default();
        if status.contains("Receiving frames") {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow::anyhow!(
                "timed out waiting for frame rendering. last status: {status}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
