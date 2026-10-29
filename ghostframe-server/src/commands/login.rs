//! `login` and `logout`: join the tailnet once, interactively or with
//! `TS_AUTHKEY`, and leave it again.
//!
//! Both hold the state-directory lock, so neither can run while the service
//! (`capture`) is using the same `tailscaled.state`.

use ghostframe_lib::transport::ghostbridge::GhostbridgeHandle;
use ghostframe_lib::GhostbridgeConfig;

use crate::config::ServerConfig;
use crate::state::{self, StateLock};

/// Bring the node up, print the sign-in URL (unless `TS_AUTHKEY` makes that
/// unnecessary) and return once the node is authorised.
pub fn login(
    cfg: &ServerConfig,
    hostname: Option<String>,
    control_url: Option<String>,
) -> Result<(), String> {
    let dir = cfg.state_dir()?;
    crate::util::ensure_private_dir(&dir)?;
    let _lock = StateLock::acquire(&dir).map_err(|e| e.to_string())?;

    let config = GhostbridgeConfig {
        hostname: hostname.clone().unwrap_or_else(|| cfg.ts_hostname.clone()),
        authkey: cfg.ts_authkey.clone().unwrap_or_default(),
        state_dir: dir.to_string_lossy().into_owned(),
        control_url: control_url
            .clone()
            .unwrap_or_else(|| cfg.ts_control_url.clone()),
    };
    let bridge = GhostbridgeHandle::connect(&config).map_err(|e| format!("tailnet: {e}"))?;

    if !config.authkey.is_empty() {
        // Non-interactive: the auth key authorises the node; Up returns once
        // it is Running.
        bridge.up().map_err(|e| format!("tailnet: {e}"))?;
        println!("logged in as {}", config.hostname);
    } else {
        // `login_url` returns once control publishes a URL, or `None` once
        // the node is Running (already authorised).
        match bridge.login_url().map_err(|e| format!("tailnet: {e}"))? {
            Some(url) => {
                println!(
                    "To authorise this node, open:\n\n    {url}\n\nWaiting for authorisation..."
                );
                bridge.up().map_err(|e| format!("tailnet: {e}"))?;
                println!("authorised as {}", config.hostname);
            }
            None => println!("already authorised as {}", config.hostname),
        }
    }

    // The service reads the same two settings from its environment, not from
    // anything `login` stores. Say so when they differ, or the next start
    // renames the node or talks to another control server.
    if hostname.is_some_and(|h| h != cfg.ts_hostname) {
        println!(
            "note: the service registers as {:?} unless its unit sets TS_HOSTNAME; \
             add `Environment=TS_HOSTNAME={}` with `systemctl --user edit ghostframe-server`",
            cfg.ts_hostname, config.hostname
        );
    }
    if control_url.is_some_and(|u| u != cfg.ts_control_url) {
        println!(
            "note: the service uses the default control server unless its unit sets \
             TS_CONTROL_URL; add `Environment=TS_CONTROL_URL={}` with \
             `systemctl --user edit ghostframe-server`",
            config.control_url
        );
    }
    Ok(())
}

/// Deregister the node from the control server, then delete the state
/// directory.
pub fn logout(cfg: &ServerConfig) -> Result<(), String> {
    let dir = cfg.state_dir()?;
    if !dir.exists() {
        println!(
            "not logged in ({} does not exist); nothing to do",
            dir.display()
        );
        return Ok(());
    }
    crate::util::ensure_private_dir(&dir)?;
    let lock = StateLock::acquire(&dir).map_err(|e| e.to_string())?;

    if state::state_dir_seeded(&dir) {
        let config = GhostbridgeConfig {
            hostname: cfg.ts_hostname.clone(),
            authkey: String::new(),
            state_dir: dir.to_string_lossy().into_owned(),
            control_url: cfg.ts_control_url.clone(),
        };
        let bridge = GhostbridgeHandle::connect(&config).map_err(|e| format!("tailnet: {e}"))?;
        // Log out BEFORE deleting state: the credentials logout needs live
        // there. If it fails, keep the state so the user can retry; deleting
        // it would strand the node in the tailnet's device list.
        bridge
            .logout()
            .map_err(|e| format!("tailnet logout failed, state kept for a retry: {e}"))?;
        // Dropping the handle closes the tsnet server.
        drop(bridge);
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("removing {}: {e}", dir.display()))?;
    drop(lock);
    println!("logged out; removed {}", dir.display());
    Ok(())
}
