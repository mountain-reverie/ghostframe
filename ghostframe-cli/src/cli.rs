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

        /// Run without opening a window: connect, render, dump, exit.
        ///
        /// For debugging and for machines with no display. The render path is
        /// unchanged -- frames are still decoded, composited and exported as
        /// dmabufs -- only the present step is skipped, so a dump taken here
        /// is the same image a window would have shown.
        #[arg(long)]
        headless: bool,

        /// Write every frame into this directory as binary PPM.
        ///
        /// `frame-NNNNNN.ppm` is what the client rendered (the framebuffer,
        /// read back through wgpu) and `export-NNNNNN.ppm` is the dmabuf a
        /// display server would import. Both, because a difference between
        /// them localises a fault that looks identical from outside: matching
        /// but wrong means the fault is upstream in decode; differing means it
        /// is in the export path.
        #[arg(long, value_name = "DIR")]
        dump_dir: Option<std::path::PathBuf>,

        /// Exit this many seconds after the session is established.
        ///
        /// Timed from "session ready", not from process start, so it measures
        /// the session rather than however long the tailnet took to come up.
        #[arg(long, value_name = "SECONDS")]
        duration: Option<u64>,
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
