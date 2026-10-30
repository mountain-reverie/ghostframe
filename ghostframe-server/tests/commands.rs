//! The `ghostframe-server` binary's command-line contract, run as a
//! subprocess with an empty environment. Every case here fails before tsnet
//! is touched, so none needs a tailnet. None needs a display either, except
//! the 16 bpp case, which starts its own Xvfb.
//!
//! A tests/*.rs target runs in CI only if a workflow names it: see the
//! `--test commands` step in .github/workflows/ci.yml.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Run the binary with an environment that cannot reach a real control
/// server: `TS_CONTROL_URL` points at a closed local port and `TS_AUTHKEY` is
/// empty, unless a test's own pair overrides them. A wall-clock limit kills a
/// run that hangs, so a regressed guard fails the test instead of waiting on
/// the network.
fn run(args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ghostframe-server"));
    cmd.args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("TS_CONTROL_URL", "http://127.0.0.1:1")
        .env("TS_AUTHKEY", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn ghostframe-server");
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().expect("poll child").is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let out = child.wait_with_output().expect("collect output");
            panic!(
                "ghostframe-server {args:?} did not exit within 20 s; stderr:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().expect("collect output")
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
fn capture_refuses_a_wayland_session_with_or_without_display() {
    // The second case is what the installed unit produces: it hard-codes
    // DISPLAY=:0, so DISPLAY is set on a Wayland session too.
    for display in [None, Some(Path::new(":0"))] {
        let state = private_dir();
        let mut env = vec![
            ("TS_STATE_DIR", state.path()),
            ("WAYLAND_DISPLAY", Path::new("wayland-0")),
        ];
        if let Some(d) = display {
            env.push(("DISPLAY", d));
        }
        let out = run(&["capture"], &env);
        assert_eq!(out.status.code(), Some(2), "{display:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(
            err.contains("Wayland sessions are not supported yet")
                && err.contains("unset-environment"),
            "{display:?}: {err}"
        );
    }
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

#[test]
fn login_logout_and_capture_refuse_while_the_old_xdaemon_unit_exists() {
    // A temporary HOME holding the old unit file; the file check needs no
    // systemctl (XDG_RUNTIME_DIR is unset, so none is asked).
    let home = tempfile::tempdir().unwrap();
    let unit = home
        .path()
        .join(".config/systemd/user/ghostframe-xdaemon.service");
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, b"[Service]\n").unwrap();
    let state = private_dir();
    for cmd in ["login", "logout", "capture"] {
        let out = run(
            &[cmd],
            &[("HOME", home.path()), ("TS_STATE_DIR", state.path())],
        );
        let want = if cmd == "capture" { 2 } else { 1 };
        assert_eq!(out.status.code(), Some(want), "{cmd}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("run `ghostframe-server install` first"),
            "{cmd}: {}",
            stderr(&out)
        );
    }
    // Without the unit file the same command gets past the guard.
    std::fs::remove_file(&unit).unwrap();
    let out = run(
        &["capture"],
        &[("HOME", home.path()), ("TS_STATE_DIR", state.path())],
    );
    assert!(!stderr(&out).contains("install` first"), "{}", stderr(&out));
}

#[test]
fn login_warns_when_the_shell_would_write_a_state_dir_the_service_does_not_read() {
    // A held lock stops login right after the warning, before any network.
    let home = tempfile::tempdir().unwrap();
    let state = private_dir();
    let _held = hold_lock(state.path());
    let out = run(
        &["login"],
        &[("HOME", home.path()), ("TS_STATE_DIR", state.path())],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    let unit_dir = home.path().join(".local/state/ghostframe");
    // No unit is installed here, so only TS_STATE_DIR was compared.
    assert!(!err.contains("XDG_STATE_HOME"), "{err}");
    assert!(
        err.contains("the service will read") && err.contains(&unit_dir.display().to_string()),
        "{err}"
    );
}

/// An Xvfb whose root is 16 bpp: a session that can never be captured.
struct Xvfb16 {
    child: std::process::Child,
    display: String,
}

impl Xvfb16 {
    fn spawn() -> Self {
        use std::io::{BufRead, BufReader};
        let mut child = Command::new("Xvfb")
            .args(["-displayfd", "1", "-nolisten", "tcp", "-noreset"])
            .args(["-screen", "0", "64x48x16"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn Xvfb");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let n: u32 = line.trim().parse().expect("Xvfb display number");
        Self {
            child,
            display: format!(":{n}"),
        }
    }
}

impl Drop for Xvfb16 {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn capture_on_a_16_bit_root_exits_2_so_the_unit_does_not_restart_loop() {
    let x = Xvfb16::spawn();
    let state = private_dir();
    // A non-empty key skips the "not logged in" exit; the backend is
    // connected before anything touches the network.
    let out = run(
        &["capture"],
        &[
            ("TS_AUTHKEY", Path::new("tskey-unused")),
            ("TS_STATE_DIR", state.path()),
            ("DISPLAY", Path::new(&x.display)),
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("root depth is 16"),
        "{}",
        stderr(&out)
    );
}
