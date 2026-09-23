//! Command-line surface: `ghostframe login|logout|connect`.

use crate::chord::Prefix;

#[derive(clap::Parser)]
#[command(name = "ghostframe", about = "ghostframe native client")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(clap::Subcommand)]
pub enum Command {
    /// Join a tailnet.
    Login {
        /// Non-interactive auth key. Without it, an auth URL is printed.
        #[arg(long)]
        authkey: Option<String>,
        /// Custom control plane (e.g. a headscale instance).
        #[arg(long)]
        login_server: Option<String>,
        /// Node name to present to the tailnet.
        #[arg(long)]
        hostname: Option<String>,
    },
    /// Leave the tailnet and remove local state.
    Logout,
    /// Connect to a server and open a window.
    Connect {
        host: String,
        #[arg(long, default_value_t = 443)]
        port: u16,
        /// `ctrl-alt-b` (default) or `super-b`.
        #[arg(long, default_value = "ctrl-alt-b")]
        chord_prefix: String,
    },
}

/// `"ctrl-alt-b"` / `"super-b"` -> `chord::Prefix`.
///
/// Rejects anything else rather than falling back to a default: a typo'd
/// prefix that quietly becomes Ctrl+Alt+b leaves the user with a chord that
/// never fires and no indication why.
pub fn parse_prefix(s: &str) -> Result<Prefix, String> {
    match s {
        "ctrl-alt-b" => Ok(Prefix::CtrlAltB),
        "super-b" => Ok(Prefix::SuperB),
        other => Err(format!(
            "unknown --chord-prefix {other:?}: expected \"ctrl-alt-b\" or \"super-b\""
        )),
    }
}
