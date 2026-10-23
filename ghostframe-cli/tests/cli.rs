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
fn login_takes_a_custom_control_plane_and_key_file() {
    let c = Cli::parse_from([
        "ghostframe",
        "login",
        "--authkey-file",
        "/run/key",
        "--login-server",
        "http://headscale:8080",
    ]);
    match c.cmd {
        Command::Login {
            authkey_stdin,
            authkey_file,
            login_server,
            ..
        } => {
            assert!(!authkey_stdin);
            assert_eq!(
                authkey_file.as_deref(),
                Some(std::path::Path::new("/run/key"))
            );
            assert_eq!(login_server.as_deref(), Some("http://headscale:8080"));
        }
        _ => panic!("wrong subcommand"),
    }
}

/// The secret must never be accepted on argv, where /proc exposes it.
#[test]
fn login_rejects_an_authkey_on_argv() {
    assert!(Cli::try_parse_from(["ghostframe", "login", "--authkey", "tskey-abc"]).is_err());
    assert!(Cli::try_parse_from(["ghostframe", "login", "--authkey=tskey-abc"]).is_err());
}

#[test]
fn stdin_and_file_are_mutually_exclusive() {
    assert!(Cli::try_parse_from([
        "ghostframe",
        "login",
        "--authkey-stdin",
        "--authkey-file",
        "/k"
    ])
    .is_err());
}

#[test]
fn authkey_line_is_trimmed_and_empty_is_an_error() {
    use ghostframe_cli::cli::read_authkey_line;
    assert_eq!(
        read_authkey_line(&mut "  tskey-abc \nsecond\n".as_bytes()).unwrap(),
        "tskey-abc"
    );
    assert!(read_authkey_line(&mut "".as_bytes()).is_err());
    assert!(read_authkey_line(&mut "  \n".as_bytes()).is_err());
}

#[test]
fn authkey_file_must_be_private() {
    use ghostframe_cli::cli::read_authkey_file;
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("gf-authkey-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("key");
    std::fs::write(&p, "tskey-file\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_authkey_file(&p).unwrap_err().contains("chmod 600"));
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(read_authkey_file(&p).unwrap(), "tskey-file");
    assert!(read_authkey_file(&dir.join("missing")).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn connect_requires_a_host() {
    // A bare `connect` must fail at parse time, not dial an empty host.
    assert!(Cli::try_parse_from(["ghostframe", "connect"]).is_err());
}
