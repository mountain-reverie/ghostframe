#[path = "e2e/helpers.rs"]
mod helpers;

use std::time::Duration;

use anyhow::Result;
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{runners::AsyncRunner, ContainerAsync, GenericImage, ImageExt};

#[tokio::test]
async fn e2e_quic_ping_pong_over_tailscale() -> Result<()> {
    // NOTE: GenericImage methods (with_exposed_port) must be called before
    // ImageExt methods (with_container_name, with_network, etc.) because
    // ImageExt converts GenericImage into ContainerRequest<GenericImage>.
    let _headscale: ContainerAsync<GenericImage> =
        GenericImage::new("ghostframe/test-headscale", "latest")
            .with_exposed_port(8080.tcp())
            .with_container_name("headscale")
            .with_network(helpers::NETWORK_NAME)
            // Headscale emits this line to stdout after binding all listeners.
            .with_ready_conditions(vec![WaitFor::message_on_stdout(
                "listening and serving HTTP",
            )])
            .start()
            .await?;

    let server_key = helpers::create_preauth_key("headscale", "server").await?;
    let client_key = helpers::create_preauth_key("headscale", "client").await?;

    let _server: ContainerAsync<GenericImage> =
        GenericImage::new("ghostframe/test-server", "latest")
            .with_container_name("ghostframe-server")
            .with_network(helpers::NETWORK_NAME)
            .with_env_var("TS_AUTHKEY", &server_key)
            .with_env_var("TS_CONTROL_URL", "http://headscale:8080")
            .with_ready_conditions(vec![WaitFor::message_on_stdout("CERT_HASH_SHA256=")])
            .start()
            .await?;

    let cert_hash = helpers::read_cert_hash_from_logs("ghostframe-server").await?;

    // headscale is reachable from the host via the mapped port.
    let headscale_host_port = _headscale.get_host_port_ipv4(8080).await?;
    let control_url = format!("http://127.0.0.1:{headscale_host_port}");

    let test_node = helpers::TestNode::join(client_key, control_url).await?;
    let upstream = test_node.dial("ghostframe-server:4443")?;
    let forwarder = helpers::start_forwarder("127.0.0.1:0", upstream).await?;

    // Serve ghostframe-web-client/dist over http://127.0.0.1:<port>.
    // Must be HTTP on a loopback address so Chromium treats it as a secure
    // context; WebTransport is not allowed from file:// origins.
    let static_addr = helpers::start_static_server("ghostframe-web-client/dist").await?;

    let (browser, mut handler) = Browser::launch(BrowserConfig::builder().build().unwrap()).await?;
    let _handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page_url = format!(
        "http://{}/index.html?host={}:{}&certHash={}",
        static_addr,
        forwarder.ip(),
        forwarder.port(),
        cert_hash,
    );

    let page = browser.new_page(&page_url).await?;

    // Poll for success, 30s timeout.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status: String = page
            .evaluate("document.getElementById('status').textContent")
            .await?
            .into_value()?;
        if status.contains("Ping/Pong successful") {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for pong. last status: {status}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
