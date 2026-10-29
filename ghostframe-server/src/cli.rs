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
    /// Leave the tailnet and delete the node's state.
    Logout,
    /// Run the server (what the systemd unit starts).
    Capture,
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
        assert_eq!(parse(&["logout"]).unwrap(), Command::Logout);
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
    fn a_command_is_required_and_init_is_gone() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--init"]).is_err());
    }
}
