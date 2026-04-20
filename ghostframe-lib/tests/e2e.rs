#[path = "e2e/helpers.rs"]
mod helpers;

use std::time::Duration;

use anyhow::Result;
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{runners::AsyncRunner, ContainerAsync, GenericImage, ImageExt};

/// Fixed host port for headscale.  Using a fixed port lets us construct the
/// `HS_SERVER_URL` (headscale's advertised URL) before the container starts, so
/// the embedded DERP server URL is reachable from both the host machine and
/// Docker containers.  The port is outside the Linux ephemeral range.
const HEADSCALE_HOST_PORT: u16 = 18080;

/// The Docker default-bridge gateway address.  This IP is reachable from:
///   - the host (via the docker0 interface), and
///   - any Docker container (via their default gateway or routing table).
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
            if status.contains("Ping/Pong successful") {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                let content = page.content().await.unwrap_or_default();
                println!("page content:\n{content}");
                let srv = tokio::process::Command::new("docker")
                    .args(["logs", "ghostframe-server"])
                    .output().await.ok();
                if let Some(o) = srv {
                    println!("=== server stdout ===\n{}", String::from_utf8_lossy(&o.stdout));
                    println!("=== server stderr ===\n{}", String::from_utf8_lossy(&o.stderr));
                }
                panic!("timed out waiting for pong. last status: {status}");
            }
        } else if tokio::time::Instant::now() >= deadline {
            let content = page.content().await.unwrap_or_default();
            println!("page content:\n{content}");
            panic!("timed out waiting for pong; evaluate kept failing");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
