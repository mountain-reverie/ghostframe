//! `login`, `logout`, and `connect` — the CLI's actual behaviour.

use std::path::PathBuf;

use ghostframe_client_native::{Client, Config};
use ghostframe_tsnet::{GhostbridgeConfig, GhostbridgeHandle};

use crate::chord::Chord;
use crate::cli::parse_prefix;

/// Errors surfaced by the CLI's subcommands.
///
/// Every variant's `Display` is written for a terminal, not a log: `main`
/// prints it verbatim (`ghostframe: {e}`) and exits non-zero, with no
/// backtrace.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("tailnet: {0}")]
    Bridge(#[from] ghostframe_tsnet::GhostbridgeError),
    #[error("client: {0}")]
    Client(#[from] ghostframe_client_native::ClientError),
    #[error("{0}")]
    Message(String),
}

/// `$XDG_STATE_HOME/ghostframe/tsnet`, falling back to
/// `$HOME/.local/state/ghostframe/tsnet`.
pub fn state_dir() -> Result<PathBuf, CommandError> {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("ghostframe").join("tsnet"));
        }
    }
    let home = std::env::var("HOME")
        .map_err(|_| CommandError::Message("neither XDG_STATE_HOME nor HOME is set".to_string()))?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("ghostframe")
        .join("tsnet"))
}

/// A sensible default node name: `ghostframe-<machine hostname>`, or a
/// fixed fallback if the machine's hostname can't be determined.
fn default_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| format!("ghostframe-{s}"))
        .unwrap_or_else(|| "ghostframe-client".to_string())
}

fn print_ips(bridge: &GhostbridgeHandle) -> Result<(), CommandError> {
    let ips = bridge.get_ips()?;
    println!("tailnet IPs:");
    for ip in ips {
        println!("  {ip}");
    }
    Ok(())
}

pub fn login(
    authkey: Option<String>,
    login_server: Option<String>,
    hostname: Option<String>,
) -> Result<(), CommandError> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| CommandError::Message(format!("creating state dir {}: {e}", dir.display())))?;

    let config = GhostbridgeConfig {
        hostname: hostname.unwrap_or_else(default_hostname),
        authkey: authkey.clone().unwrap_or_default(),
        state_dir: dir.to_string_lossy().into_owned(),
        control_url: login_server.unwrap_or_default(),
    };

    let bridge = GhostbridgeHandle::connect(&config)?;

    if authkey.is_some() {
        bridge.up()?;
        println!("logged in.");
    } else {
        match bridge.login_url()? {
            Some(url) => {
                println!(
                    "To authorise this node, open:\n\n    {url}\n\nWaiting for authorisation..."
                );
                bridge.up()?;
                println!("authorised.");
            }
            None => {
                // `login_url` blocks until the node either gets a URL or
                // reaches Running -- `None` means the latter already
                // happened, so there is nothing to wait for.
                println!("already authorised.");
                print_ips(&bridge)?;
                return Ok(());
            }
        }
    }

    print_ips(&bridge)?;
    Ok(())
}

pub fn logout() -> Result<(), CommandError> {
    let dir = state_dir()?;
    if !dir.join("tailscaled.state").exists() {
        println!("not logged in; nothing to do.");
        return Ok(());
    }

    let config = GhostbridgeConfig {
        hostname: default_hostname(),
        authkey: String::new(),
        state_dir: dir.to_string_lossy().into_owned(),
        control_url: String::new(),
    };
    let bridge = GhostbridgeHandle::connect(&config)?;

    // logout() BEFORE removing state: it needs the credentials in the
    // state dir to tell the control plane this node is leaving. Deleting
    // state first would strand the node in the tailnet's device list with
    // no way to reach it. And if logout() itself fails, leave the state
    // dir alone -- silently deleting it after a failed logout is the worst
    // outcome, since it discards the user's only means to retry.
    bridge.logout()?;

    std::fs::remove_dir_all(&dir)
        .map_err(|e| CommandError::Message(format!("removing state dir {}: {e}", dir.display())))?;
    println!("logged out.");
    Ok(())
}

pub fn connect(host: String, port: u16, chord_prefix: String) -> Result<(), CommandError> {
    // Argument validation before any I/O or state check: a typo'd
    // --chord-prefix should fail the same way whether or not the caller
    // happens to be logged in.
    let prefix = parse_prefix(&chord_prefix).map_err(CommandError::Message)?;
    // Proves the chord state machine wires up end-to-end; the window loop
    // that will actually feed it key events is Tasks 6-9.
    let _chord = Chord::new(prefix);

    let dir = state_dir()?;
    if !dir.join("tailscaled.state").exists() {
        return Err(CommandError::Message(
            "not logged in: run 'ghostframe login' first".to_string(),
        ));
    }

    let config = Config {
        hostname: default_hostname(),
        authkey: String::new(),
        state_dir: dir,
        supports_h264: false,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers: vec![],
        debug_map_frames: false,
    };

    let mut client = Client::new(config)?;
    client.connect(&host, port)?;

    // --- SEAM: window backend goes here (M2 Tasks 6-9) ---
    //
    // `client` is connected and ready to feed a render/event loop: see
    // `Client::event_fd`, `Client::next_event`, `Client::acquire_frame`,
    // and the `push_*` input methods. Wiring those to an actual Wayland
    // or X11 window is out of scope for this task.
    println!("connected to {host}:{port}.");
    println!("window backend not yet implemented (M2 task 6-9).");

    // `client`'s `Drop` tears down the net/render threads and the tsnet
    // session cleanly; there is no window loop yet to hand it off to.
    Ok(())
}
