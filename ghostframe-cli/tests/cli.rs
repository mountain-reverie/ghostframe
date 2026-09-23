use clap::Parser;
use ghostframe_cli::chord::Prefix;
use ghostframe_cli::cli::{parse_prefix, Cli, Command};

#[test]
fn connect_defaults_to_443_and_the_safe_prefix() {
    let c = Cli::parse_from(["ghostframe", "connect", "host"]);
    match c.cmd {
        Command::Connect {
            port, chord_prefix, ..
        } => {
            assert_eq!(port, 443);
            assert_eq!(chord_prefix, "ctrl-alt-b");
        }
        _ => panic!("wrong subcommand"),
    }
}

#[test]
fn both_prefixes_parse() {
    assert_eq!(parse_prefix("ctrl-alt-b").unwrap(), Prefix::CtrlAltB);
    assert_eq!(parse_prefix("super-b").unwrap(), Prefix::SuperB);
}

#[test]
fn an_unknown_chord_prefix_is_rejected_at_parse_time() {
    // Better a clear error than a window whose chord silently never fires.
    let err = parse_prefix("meta-q").expect_err("must reject");
    assert!(
        err.contains("meta-q"),
        "error should name the bad value: {err}"
    );
}

#[test]
fn login_accepts_an_authkey_and_a_custom_control_plane() {
    let c = Cli::parse_from([
        "ghostframe",
        "login",
        "--authkey",
        "tskey-abc",
        "--login-server",
        "http://headscale:8080",
    ]);
    match c.cmd {
        Command::Login {
            authkey,
            login_server,
            ..
        } => {
            assert_eq!(authkey.as_deref(), Some("tskey-abc"));
            assert_eq!(login_server.as_deref(), Some("http://headscale:8080"));
        }
        _ => panic!("wrong subcommand"),
    }
}

#[test]
fn connect_requires_a_host() {
    // A bare `connect` must fail at parse time, not dial an empty host.
    assert!(Cli::try_parse_from(["ghostframe", "connect"]).is_err());
}
