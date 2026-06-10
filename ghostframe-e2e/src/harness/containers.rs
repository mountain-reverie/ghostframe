use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use ghostframe_lib::transport::ghostbridge::{GhostbridgeConfig, GhostbridgeHandle, UdpPacketConn};

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

    pub fn dial_tcp(&self, remote: &str) -> Result<std::os::unix::net::UnixStream> {
        Ok(self.handle.dial_tcp(remote)?)
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
