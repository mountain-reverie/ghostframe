//! The command line: `ghostframe-server <command>`.

#[derive(Debug, clap::Parser)]
#[command(
    name = "ghostframe-server",
    version,
    about = "Serve this user's X session to ghostframe clients over a Tailscale tailnet"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, PartialEq, Eq, clap::Subcommand)]
pub enum Command {
    /// Install this binary, its systemd user unit and an autostart entry
    /// for the current user, and remove what the old root installer left.
    Install {
        /// Replace a different ~/.local/bin/ghostframe-server.
        #[arg(long)]
        force: bool,
        /// Largest resolution a client may set, `WIDTHxHEIGHT`, or `none`.
        #[arg(long, value_name = "WxH|none", default_value = "1920x1080", value_parser = parse_max_resolution)]
        max_resolution: MaxResolution,
    },
    /// Remove what `install` wrote. Keeps the tailnet state (see `logout`).
    Uninstall,
    /// Join the tailnet: print a sign-in URL and wait until the node is
    /// authorised. With TS_AUTHKEY set, join without a browser.
    Login {
        /// Node name to register (default: TS_HOSTNAME, else `<host>-ghostframe`).
        #[arg(long)]
        hostname: Option<String>,
        /// Control server URL, e.g. a headscale instance (default: TS_CONTROL_URL, else Tailscale).
        #[arg(long, value_name = "URL")]
        control_url: Option<String>,
    },
    /// Leave the tailnet and delete the node's state. Pass the same
    /// --hostname/--control-url that `login` was given.
    Logout {
        /// Node name it was registered under (default: TS_HOSTNAME, else `<host>-ghostframe`).
        #[arg(long)]
        hostname: Option<String>,
        /// Control server URL it was registered with (default: TS_CONTROL_URL, else Tailscale).
        #[arg(long, value_name = "URL")]
        control_url: Option<String>,
    },
    /// Run the server (what the systemd unit starts).
    Capture,
}

/// The ceiling `install` writes into the unit as `GHOSTFRAME_MAX_RESOLUTION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxResolution {
    Limit(u16, u16),
    Unlimited,
}

/// `WIDTHxHEIGHT` (both non-zero) or `none`. Unlike the runtime parser in
/// `capture::x11::display`, which ignores a bad value rather than stop a
/// running service, `install` rejects one before writing anything.
pub fn parse_max_resolution(s: &str) -> Result<MaxResolution, String> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("none") {
        return Ok(MaxResolution::Unlimited);
    }
    let bad = || format!("{s:?} is not WIDTHxHEIGHT or 'none'");
    let (w, h) = t.split_once(['x', 'X']).ok_or_else(bad)?;
    let w: u16 = w.trim().parse().map_err(|_| bad())?;
    let h: u16 = h.trim().parse().map_err(|_| bad())?;
    if w == 0 || h == 0 {
        return Err(bad());
    }
    Ok(MaxResolution::Limit(w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Command, clap::Error> {
        Cli::try_parse_from(std::iter::once("ghostframe-server").chain(args.iter().copied()))
            .map(|c| c.command)
    }

    #[test]
    fn parses_each_command() {
        assert_eq!(parse(&["capture"]).unwrap(), Command::Capture);
        assert_eq!(
            parse(&["logout"]).unwrap(),
            Command::Logout {
                hostname: None,
                control_url: None
            }
        );
        assert_eq!(
            parse(&["logout", "--hostname", "h", "--control-url", "http://hs"]).unwrap(),
            Command::Logout {
                hostname: Some("h".into()),
                control_url: Some("http://hs".into())
            }
        );
        assert_eq!(
            parse(&["login"]).unwrap(),
            Command::Login {
                hostname: None,
                control_url: None
            }
        );
        assert_eq!(
            parse(&["login", "--hostname", "h", "--control-url", "http://hs"]).unwrap(),
            Command::Login {
                hostname: Some("h".into()),
                control_url: Some("http://hs".into())
            }
        );
    }

    #[test]
    fn install_defaults_to_1080p_and_takes_none() {
        assert_eq!(
            parse(&["install"]).unwrap(),
            Command::Install {
                force: false,
                max_resolution: MaxResolution::Limit(1920, 1080)
            }
        );
        assert_eq!(
            parse(&["install", "--force", "--max-resolution", "none"]).unwrap(),
            Command::Install {
                force: true,
                max_resolution: MaxResolution::Unlimited
            }
        );
        assert_eq!(parse(&["uninstall"]).unwrap(), Command::Uninstall);
    }

    #[test]
    fn max_resolution_rejects_what_it_cannot_write() {
        assert_eq!(
            parse_max_resolution("2560X1440"),
            Ok(MaxResolution::Limit(2560, 1440))
        );
        for bad in [
            "",
            "1920",
            "1920x",
            "x1080",
            "0x1080",
            "1920x0",
            "abcxdef",
            "1920*1080",
        ] {
            assert!(parse_max_resolution(bad).is_err(), "{bad:?} accepted");
        }
        assert!(parse(&["install", "--max-resolution", "huge"]).is_err());
    }

    #[test]
    fn a_command_is_required_and_init_is_gone() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--init"]).is_err());
    }
}
