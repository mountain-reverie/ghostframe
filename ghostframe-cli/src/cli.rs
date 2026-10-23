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
        /// Read a non-interactive auth key from stdin (a hidden prompt on a
        /// terminal, otherwise one line). Without this or `--authkey-file`,
        /// an auth URL is printed. There is deliberately no `--authkey
        /// <KEY>`: argv is world-readable through /proc.
        #[arg(long, conflicts_with = "authkey_file")]
        authkey_stdin: bool,
        /// Read a non-interactive auth key from this file (first line). The
        /// file must not be readable by group or others.
        #[arg(long, value_name = "PATH")]
        authkey_file: Option<std::path::PathBuf>,
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

/// First line of `r`, trimmed. An empty key is an error, never "no key":
/// silently falling back to the interactive flow would hang a script.
pub fn read_authkey_line(r: &mut impl std::io::BufRead) -> Result<String, String> {
    let mut line = String::new();
    r.read_line(&mut line)
        .map_err(|e| format!("reading auth key: {e}"))?;
    let key = line.trim();
    if key.is_empty() {
        return Err("auth key is empty".to_string());
    }
    Ok(key.to_string())
}

/// Read an auth key from `path`. Refuses a file that group or others can
/// read, for the same reason the key is not accepted on argv.
pub fn read_authkey_file(path: &std::path::Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{}: auth key file is accessible by other users; chmod 600 it",
            path.display()
        ));
    }
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    read_authkey_line(&mut std::io::BufReader::new(f))
}

/// Read the key from stdin. On a terminal, prompt on stderr with echo off.
pub fn read_authkey_stdin() -> Result<String, String> {
    use std::io::IsTerminal;
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return read_authkey_line(&mut stdin.lock());
    }
    eprint!("Auth key: ");
    // SAFETY: termios is plain data; fd 0 is stdin and a terminal here.
    let saved = unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) == 0 {
            let mut quiet = t;
            quiet.c_lflag &= !libc::ECHO;
            libc::tcsetattr(0, libc::TCSANOW, &quiet);
            Some(t)
        } else {
            None
        }
    };
    let r = read_authkey_line(&mut stdin.lock());
    if let Some(t) = saved {
        // SAFETY: restoring the attributes read above.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        eprintln!();
    }
    r
}

/// Resolve the login flags into a key, if one was asked for.
pub fn resolve_authkey(
    stdin_flag: bool,
    file: Option<&std::path::Path>,
) -> Result<Option<String>, String> {
    match (stdin_flag, file) {
        (_, Some(p)) => read_authkey_file(p).map(Some),
        (true, None) => read_authkey_stdin().map(Some),
        (false, None) => Ok(None),
    }
}
