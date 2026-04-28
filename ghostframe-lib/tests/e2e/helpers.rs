use std::net::SocketAddr;
use std::os::unix::io::FromRawFd;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use axum::Router;
use ghostframe_lib::framing::{encode_frame, parse_frame_rest};
use ghostframe_lib::transport::ghostbridge::{GhostbridgeConfig, GhostbridgeHandle, UdpPacketConn};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UdpSocket, UnixStream as TokioUnixStream};
use tower_http::services::ServeDir;

pub const NETWORK_NAME: &str = "ghostframe-e2e";

/// Create a headscale preauth key for the given user. Idempotent — if the
/// user already exists the `users create` error is ignored.
///
/// Headscale 0.28 requires `--user <numeric_id>` (not name) for preauthkeys.
/// So we create the user by name, then look up its numeric ID via `users list`,
/// and pass that ID to `preauthkeys create`.
pub async fn create_preauth_key(container_name: &str, user: &str) -> Result<String> {
    // Idempotent: ignore "user already exists" error.
    let _ = tokio::process::Command::new("docker")
        .args(["exec", container_name, "headscale", "users", "create", user])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;

    // Look up the numeric user ID — headscale 0.28 requires it for preauthkeys.
    let list_out = tokio::process::Command::new("docker")
        .args([
            "exec",
            container_name,
            "headscale",
            "users",
            "list",
            "--output",
            "json",
        ])
        .output()
        .await
        .context("running headscale users list")?;
    let users: serde_json::Value = serde_json::from_slice(&list_out.stdout)?;
    let user_id = users
        .as_array()
        .and_then(|arr| arr.iter().find(|u| u["name"].as_str() == Some(user)))
        .and_then(|u| u["id"].as_u64())
        .ok_or_else(|| anyhow!("user {user} not found in headscale users list"))?;

    let out = tokio::process::Command::new("docker")
        .args([
            "exec",
            container_name,
            "headscale",
            "preauthkeys",
            "create",
            "--user",
            &user_id.to_string(),
            "--reusable",
            "--expiration",
            "1h",
            "--output",
            "json",
        ])
        .output()
        .await
        .context("running headscale preauthkeys create")?;
    if !out.status.success() {
        return Err(anyhow!(
            "preauthkeys create failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    v.get("key")
        .and_then(|k| k.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            anyhow!(
                "no `key` field in preauthkey JSON: {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
}

/// Tail `docker logs -f <container>` until a line starting with
/// `CERT_HASH_SHA256=` is seen; returns the hash hex string. Timeout: 60s.
pub async fn read_cert_hash_from_logs(container_name: &str) -> Result<String> {
    let mut child = tokio::process::Command::new("docker")
        .args(["logs", "-f", container_name])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let line = tokio::time::timeout(remaining, lines.next_line()).await??;
        let Some(line) = line else { break };
        if let Some(rest) = line.strip_prefix("CERT_HASH_SHA256=") {
            let hash = rest.trim().to_string();
            child.kill().await.ok();
            return Ok(hash);
        }
    }
    Err(anyhow!("cert hash not seen in container logs within 60s"))
}

/// A tsnet node running inside the test process. Joins the test headscale
/// tailnet via ghostbridge so the test can dial the server container directly
/// over the tailnet.
pub struct TestNode {
    handle: GhostbridgeHandle,
}

impl TestNode {
    pub async fn join(authkey: String, control_url: String) -> Result<Self> {
        let handle = GhostbridgeHandle::connect(&GhostbridgeConfig {
            hostname: "e2e-test-client".into(),
            authkey,
            state_dir: format!("/tmp/ghostframe-e2e-client-{}", std::process::id()),
            control_url,
        })?;
        handle.up()?;
        Ok(Self { handle })
    }

    pub fn dial(&self, remote: &str) -> Result<UdpPacketConn> {
        Ok(self.handle.dial_udp(remote)?)
    }
}

/// Proxies between a local loopback UDP socket (what Chromium sees) and a
/// `UdpPacketConn` that forwards packets over tsnet to the server container.
///
/// Returns the bound `SocketAddr` that the browser should connect to.
pub async fn start_forwarder(local_bind: &str, upstream: UdpPacketConn) -> Result<SocketAddr> {
    let sock = UdpSocket::bind(local_bind).await?;
    let local = sock.local_addr()?;

    // Wrap the upstream UdpPacketConn fd in tokio for async I/O.
    let raw_fd = upstream.into_raw_fd();
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(raw_fd) };
    std_stream.set_nonblocking(true)?;
    let mut upstream = TokioUnixStream::from_std(std_stream)?;

    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        let mut header = [0u8; 8];
        let mut client: Option<SocketAddr> = None;

        loop {
            tokio::select! {
                // Browser → tsnet
                res = sock.recv_from(&mut buf) => {
                    let Ok((n, from)) = res else { return };
                    client = Some(from);
                    let frame = encode_frame(&buf[..n], &from);
                    if upstream.write_all(&frame).await.is_err() { return; }
                }
                // tsnet → browser
                res = upstream.read_exact(&mut header) => {
                    if res.is_err() { return }
                    let total_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
                    let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
                    let mut rest = vec![0u8; total_len - 8];
                    if upstream.read_exact(&mut rest).await.is_err() { return }
                    let Ok(pkt) = parse_frame_rest(&rest, payload_len) else { continue };
                    if let Some(dst) = client {
                        let _ = sock.send_to(&pkt.payload, dst).await;
                    }
                }
            }
        }
    });

    Ok(local)
}

/// Serve a directory over HTTP on 127.0.0.1:<random>. Returns the bound address.
/// Used so Chromium loads the page from a secure-context origin (http://127.0.0.1),
/// which is required for WebTransport. `file://` origins are not a secure context.
pub async fn start_static_server(dir: impl AsRef<Path>) -> Result<SocketAddr> {
    let app = Router::new().fallback_service(ServeDir::new(dir.as_ref()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(addr)
}

/// Run `docker exec [-e <env>...] <container> <args...>` against an
/// already-running container, and return `(stdout, stderr, exit_code)`.
///
/// Uses `tokio::process::Command` (no shell — args are passed directly to
/// `execve`). Pass environment variables as `("KEY", "value")` tuples —
/// useful for invoking X11 client tools that need `DISPLAY=:99`.
pub async fn docker_run_in_container(
    container: &str,
    env: &[(&str, &str)],
    args: &[&str],
) -> Result<(String, String, i32)> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("exec");
    for (k, v) in env {
        cmd.arg("-e").arg(format!("{k}={v}"));
    }
    cmd.arg(container).args(args);
    let out = cmd.output().await.context("running docker exec")?;
    Ok((
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    ))
}
