use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use ghostframe_lib::transport::ghostbridge::{GhostbridgeConfig, GhostbridgeHandle, UdpPacketConn};
use tokio::io::{AsyncBufReadExt, BufReader};

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
