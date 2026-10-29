//! The `ghostframe-server` binary's command-line contract, run as a
//! subprocess with an empty environment. Every case here fails before tsnet
//! or X is touched, so it needs neither a tailnet nor a display.
//!
//! A tests/*.rs target runs in CI only if a workflow names it: see the
//! `--test commands` step in .github/workflows/ci.yml.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ghostframe-server"));
    cmd.args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn ghostframe-server")
}

/// A 0700 state directory, as the server insists on.
fn private_dir() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

/// Hold the state lock the way a running service would.
fn hold_lock(dir: &Path) -> File {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(".lock"))
        .unwrap();
    // SAFETY: flock on a descriptor this test owns.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "test could not take the lock");
    f
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn capture_without_login_exits_2_and_says_to_log_in() {
    let state = private_dir();
    let out = run(&["capture"], &[("TS_STATE_DIR", state.path())]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("run `ghostframe-server login`"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn capture_refuses_a_wayland_only_session() {
    let state = private_dir();
    let out = run(
        &["capture"],
        &[
            ("TS_STATE_DIR", state.path()),
            ("WAYLAND_DISPLAY", Path::new("wayland-0")),
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("Wayland sessions are not supported yet"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn capture_refuses_a_world_accessible_state_dir() {
    let state = tempfile::tempdir().unwrap();
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = run(&["capture"], &[("TS_STATE_DIR", state.path())]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("chmod 700"), "{}", stderr(&out));
}

#[test]
fn login_logout_and_capture_refuse_while_the_service_holds_the_lock() {
    let state = private_dir();
    std::fs::write(state.path().join("tailscaled.state"), b"{}").unwrap();
    let _held = hold_lock(state.path());
    for cmd in ["login", "logout", "capture"] {
        let out = run(&[cmd], &[("TS_STATE_DIR", state.path())]);
        assert_eq!(out.status.code(), Some(1), "{cmd}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("the ghostframe-server service is running; stop it first"),
            "{cmd}: {}",
            stderr(&out)
        );
    }
    assert!(
        state.path().join("tailscaled.state").exists(),
        "a refused logout must not delete the state"
    );
}

#[test]
fn init_is_gone() {
    let state = private_dir();
    for args in [&["--init"][..], &["capture", "--init"]] {
        let out = run(args, &[("TS_STATE_DIR", state.path())]);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("unexpected argument '--init'"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn capture_does_not_treat_a_started_but_unauthorised_node_as_joined() {
    // What an interrupted `login` leaves: tsnet writes `{}` (then the machine
    // key) before any authorisation. Starting capture on it would make tsnet
    // log a claimable sign-in URL to the journal every few seconds.
    for content in [&b"{}"[..], br#"{"_machinekey":"AAAA"}"#] {
        let state = private_dir();
        std::fs::write(state.path().join("tailscaled.state"), content).unwrap();
        let out = run(&["capture"], &[("TS_STATE_DIR", state.path())]);
        assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
        assert!(
            stderr(&out).contains("run `ghostframe-server login`"),
            "{}",
            stderr(&out)
        );
    }
}

#[test]
fn logout_refuses_a_directory_it_did_not_create() {
    let state = private_dir();
    std::fs::write(state.path().join("holiday.jpg"), b"precious").unwrap();
    let out = run(&["logout"], &[("TS_STATE_DIR", state.path())]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("holiday.jpg"), "{}", stderr(&out));
    assert!(state.path().join("holiday.jpg").exists());
}

#[test]
fn help_lists_every_command() {
    let out = run(&["--help"], &[]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    for cmd in ["install", "uninstall", "login", "logout", "capture"] {
        assert!(text.contains(cmd), "{cmd} missing from:\n{text}");
    }
}
