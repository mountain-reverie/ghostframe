//! `login` and `logout`: join the tailnet once, interactively or with
//! `TS_AUTHKEY`, and leave it again.
//!
//! Both hold the state-directory lock, so neither can run while the service
//! (`capture`) is using the same `tailscaled.state`.
//!
//! The tsnet handle sits behind [`Tailnet`] so the ordering guarantees that
//! matter (the lock is held while joining, state survives a failed logout,
//! nothing is marked joined unless `up()` returned) are unit-tested against
//! a fake.

use std::io::Write;

use ghostframe_lib::transport::ghostbridge::{
    GhostbridgeError, GhostbridgeHandle, LOGIN_URL_MACHINE_AUTH_RC,
};
use ghostframe_lib::GhostbridgeConfig;

use crate::commands::install::{InstallPaths, RealSystemctl, Systemctl, UNIT_NAME};
use crate::config::ServerConfig;
use crate::state::{self, RemoveError, StateLock};

/// The slice of `GhostbridgeHandle` these commands use.
pub trait Tailnet {
    fn up(&self) -> Result<(), GhostbridgeError>;
    fn login_url(&self) -> Result<Option<String>, GhostbridgeError>;
    fn logout(&self) -> Result<(), GhostbridgeError>;
}

impl Tailnet for GhostbridgeHandle {
    fn up(&self) -> Result<(), GhostbridgeError> {
        GhostbridgeHandle::up(self)
    }
    fn login_url(&self) -> Result<Option<String>, GhostbridgeError> {
        GhostbridgeHandle::login_url(self)
    }
    fn logout(&self) -> Result<(), GhostbridgeError> {
        GhostbridgeHandle::logout(self)
    }
}

/// `gbridge_login_url` gives up after 60 s without a URL or a Running node.
const LOGIN_URL_TIMEOUT_RC: i32 = -6;

fn tailnet_err(e: GhostbridgeError) -> String {
    match e {
        GhostbridgeError::Ffi("gbridge_login_url", LOGIN_URL_TIMEOUT_RC) => "no sign-in URL from \
             the control server within 60 s; check the network or --control-url"
            .to_string(),
        GhostbridgeError::Ffi("gbridge_login_url", LOGIN_URL_MACHINE_AUTH_RC) => {
            "this node signed in but is waiting for device approval: approve it in the \
             admin console, then run `ghostframe-server login` again"
                .to_string()
        }
        e => format!("tailnet: {e}"),
    }
}

pub fn login(
    cfg: &ServerConfig,
    hostname: Option<String>,
    control_url: Option<String>,
    state_dir_differs: bool,
) -> Result<(), String> {
    login_with(
        cfg,
        hostname,
        control_url,
        state_dir_differs,
        GhostbridgeHandle::connect,
        &mut RealSystemctl,
        &mut std::io::stdout(),
    )
}

pub fn logout(
    cfg: &ServerConfig,
    hostname: Option<String>,
    control_url: Option<String>,
) -> Result<(), String> {
    logout_with(
        cfg,
        hostname,
        control_url,
        GhostbridgeHandle::connect,
        &mut std::io::stdout(),
    )
}

fn bridge_config(
    cfg: &ServerConfig,
    dir: &std::path::Path,
    hostname: &Option<String>,
    control_url: &Option<String>,
) -> GhostbridgeConfig {
    GhostbridgeConfig {
        hostname: hostname.clone().unwrap_or_else(|| cfg.ts_hostname.clone()),
        authkey: cfg.ts_authkey.clone().unwrap_or_default(),
        state_dir: dir.to_string_lossy().into_owned(),
        control_url: control_url
            .clone()
            .unwrap_or_else(|| cfg.ts_control_url.clone()),
        // This command owns the terminal: the sign-in URL is the one line the
        // person must see, not tsnet's backend log.
        quiet: true,
        non_interactive: false,
        redact_auth_urls: false,
    }
}

/// Bring the node up, print the sign-in URL (unless `TS_AUTHKEY` makes that
/// unnecessary) and return once the node is authorised. Then, with the state
/// lock released, enable and start the installed service: `install` leaves
/// it stopped for a node that had not joined, and nothing else would start
/// it on a desktop that never activates `graphical-session.target`.
pub fn login_with<T: Tailnet>(
    cfg: &ServerConfig,
    hostname: Option<String>,
    control_url: Option<String>,
    state_dir_differs: bool,
    connect: impl FnOnce(&GhostbridgeConfig) -> Result<T, GhostbridgeError>,
    sys: &mut dyn Systemctl,
    out: &mut dyn Write,
) -> Result<(), String> {
    // The lock and the tsnet handle live (and die) inside `join`: the
    // service needs both free to start.
    let env_notes = join(cfg, hostname, control_url, connect, out)?;
    let hold = if state_dir_differs {
        Some("the service will not see this login (it reads a different state directory, see the warning above)")
    } else if env_notes {
        Some("the service would register differently from this login")
    } else {
        None
    };
    start_service(cfg, hold, env_notes, sys, out)
}

/// Enable and start the installed unit after a successful join. Never fails
/// the login: the join worked, so a systemd problem is a warning.
fn start_service(
    cfg: &ServerConfig,
    hold: Option<&str>,
    env_notes: bool,
    sys: &mut dyn Systemctl,
    out: &mut dyn Write,
) -> Result<(), String> {
    let w = |e: std::io::Error| format!("writing output: {e}");
    let start_by_hand = format!("systemctl --user start {UNIT_NAME}");
    let Some(home) = cfg.home() else {
        writeln!(
            out,
            "HOME is unset, cannot locate the unit; if it is installed, run: \
             systemctl --user enable {UNIT_NAME} && {start_by_hand}"
        )
        .map_err(w)?;
        return Ok(());
    };
    let paths = InstallPaths::new(std::path::Path::new(home));
    if !paths.unit_dir.join(UNIT_NAME).exists() {
        writeln!(
            out,
            "{UNIT_NAME} is not installed; run `ghostframe-server install` to set it up"
        )
        .map_err(w)?;
        return Ok(());
    }
    let by_hand =
        format!("systemctl --user enable {UNIT_NAME} && systemctl --user start {UNIT_NAME}");
    if !cfg.user_manager_possible() {
        writeln!(
            out,
            "note: no user systemd manager reachable from this shell (XDG_RUNTIME_DIR unset), \
             so {UNIT_NAME} was not started; from your desktop session run: {by_hand}"
        )
        .map_err(w)?;
        return Ok(());
    }
    if let Some(why) = hold {
        // Enable is harmless; starting would run a service that ignores this
        // login. (A differing state dir: enable still follows the unit.)
        let enabled = sys.user(&["enable", UNIT_NAME]);
        let first = match (&enabled, env_notes) {
            (Err(e), _) => format!("{e}\n  "),
            (Ok(()), true) => "after adding the Environment= lines (`systemctl --user edit \
                 ghostframe-server`), "
                .to_string(),
            (Ok(()), false) => String::new(),
        };
        writeln!(
            out,
            "not starting {UNIT_NAME}: {why}.\n  {first}run: {start_by_hand}"
        )
        .map_err(w)?;
        return Ok(());
    }
    // No daemon-reload: `install` wrote the unit and reloaded; login changes
    // no unit file.
    match sys
        .user(&["enable", UNIT_NAME])
        .and_then(|()| sys.user(&["start", UNIT_NAME]))
    {
        Ok(()) => writeln!(
            out,
            "enabled and started {UNIT_NAME} (check: systemctl --user status ghostframe-server)"
        )
        .map_err(w)?,
        Err(e) => writeln!(
            out,
            "warning: logged in, but {UNIT_NAME} could not be enabled and started: {e}\n  \
             run by hand: {by_hand}"
        )
        .map_err(w)?,
    }
    Ok(())
}

/// The part of `login` that holds the state lock.
fn join<T: Tailnet>(
    cfg: &ServerConfig,
    hostname: Option<String>,
    control_url: Option<String>,
    connect: impl FnOnce(&GhostbridgeConfig) -> Result<T, GhostbridgeError>,
    out: &mut dyn Write,
) -> Result<bool, String> {
    let dir = cfg.state_dir()?;
    crate::util::ensure_private_dir(&dir)?;
    let _lock = StateLock::acquire(&dir).map_err(|e| e.to_string())?;

    let config = bridge_config(cfg, &dir, &hostname, &control_url);
    let bridge = connect(&config).map_err(tailnet_err)?;
    let w = |e: std::io::Error| format!("writing output: {e}");

    if !config.authkey.is_empty() {
        // Non-interactive: the auth key authorises the node; Up returns once
        // it is Running.
        bridge.up().map_err(tailnet_err)?;
        state::mark_joined(&dir).map_err(|e| format!("recording the join: {e}"))?;
        writeln!(out, "logged in as {}", config.hostname).map_err(w)?;
    } else {
        // `login_url` returns once control publishes a URL, or `None` once
        // the node is Running (already authorised).
        match bridge.login_url().map_err(tailnet_err)? {
            Some(url) => {
                writeln!(
                    out,
                    "To authorise this node, open:\n\n    {url}\n\n\
                     If your tailnet requires device approval, approve it in the admin console.\n\
                     Waiting for authorisation..."
                )
                .map_err(w)?;
                bridge.up().map_err(tailnet_err)?;
                state::mark_joined(&dir).map_err(|e| format!("recording the join: {e}"))?;
                writeln!(out, "authorised as {}", config.hostname).map_err(w)?;
            }
            // None = Running with an empty AuthURL. Caveat: a node resumed from a
            // cached netmap can report Running before control re-validated its
            // key, so this is "authorised as far as we can tell offline".
            None => {
                state::mark_joined(&dir).map_err(|e| format!("recording the join: {e}"))?;
                writeln!(out, "already authorised as {}", config.hostname).map_err(w)?;
            }
        }
    }

    // The service reads the same two settings from its environment, not from
    // anything `login` stores. Say so when they differ, or the next start
    // renames the node or talks to another control server. `logout` needs
    // the same values to reach the right control server.
    let mut env_notes = false;
    if hostname.is_some_and(|h| h != cfg.ts_hostname) {
        env_notes = true;
        writeln!(
            out,
            "note: the service registers as {:?} unless its unit sets TS_HOSTNAME; \
             add `Environment=TS_HOSTNAME={}` with `systemctl --user edit ghostframe-server`. \
             `ghostframe-server logout` needs `--hostname {}` too",
            cfg.ts_hostname, config.hostname, config.hostname
        )
        .map_err(w)?;
    }
    if control_url.is_some_and(|u| u != cfg.ts_control_url) {
        env_notes = true;
        writeln!(
            out,
            "note: the service uses the default control server unless its unit sets \
             TS_CONTROL_URL; add `Environment=TS_CONTROL_URL={}` with \
             `systemctl --user edit ghostframe-server`. \
             `ghostframe-server logout` needs `--control-url {}` too",
            config.control_url, config.control_url
        )
        .map_err(w)?;
    }
    Ok(env_notes)
}

/// Deregister the node from the control server, then delete the state
/// directory (known entries only: see [`state::remove_state_dir`]).
pub fn logout_with<T: Tailnet>(
    cfg: &ServerConfig,
    hostname: Option<String>,
    control_url: Option<String>,
    connect: impl FnOnce(&GhostbridgeConfig) -> Result<T, GhostbridgeError>,
    out: &mut dyn Write,
) -> Result<(), String> {
    let dir = cfg.state_dir()?;
    if !dir.exists() {
        writeln!(
            out,
            "not logged in ({} does not exist); nothing to do",
            dir.display()
        )
        .map_err(|e| e.to_string())?;
        return Ok(());
    }
    crate::util::ensure_private_dir(&dir)?;
    let lock = StateLock::acquire(&dir).map_err(|e| e.to_string())?;

    // Scan BEFORE deregistering: a directory holding anything ghostframe did
    // not create is refused while the node is still registered and still
    // marked joined. Refusing afterwards would leave a deregistered node
    // whose marker survives, and the next `capture` would start an
    // interactive tsnet.
    match state::plan_removal(&dir) {
        Ok(_) => {}
        Err(RemoveError::Unknown(names)) => {
            return Err(format!(
                "{} holds entries ghostframe did not create: {}. No change was made. \
                 Is TS_STATE_DIR right?",
                dir.display(),
                names.join(", ")
            ))
        }
        Err(RemoveError::Io(e)) => {
            return Err(format!(
                "cannot inspect {}: {e}. No change was made.",
                dir.display()
            ))
        }
    }

    // Nothing to deregister for a node that never got past tsnet's own start.
    let registered = dir.join("tailscaled.state").exists() && !state::never_joined(&dir);
    if registered {
        let mut config = bridge_config(cfg, &dir, &hostname, &control_url);
        config.authkey = String::new();
        let bridge = connect(&config).map_err(tailnet_err)?;
        // Log out BEFORE deleting state: the credentials logout needs live
        // there. If it fails, keep the state so the user can retry; deleting
        // it would strand the node in the tailnet's device list.
        bridge.logout().map_err(|e| {
            format!(
                "tailnet logout failed, state kept for a retry: {e}\n\
                 If you are offline or the node is already gone, remove it in the \
                 admin console, then delete the local state by hand: rm -r {:?}",
                dir
            )
        })?;
        // Dropping the handle closes the tsnet server.
        drop(bridge);
        // The node is deregistered: the directory must stop claiming a join
        // before anything else can go wrong.
        match state::remove_marker(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "logged out, but could not remove {}: {e}; delete the state directory \
                     by hand before starting the service: rm -r {:?}",
                    state::JOINED_FILE,
                    dir
                ))
            }
        }
    }
    if dir.join("files").is_dir() {
        writeln!(
            out,
            "removing {}/files/ (files received over Taildrop)",
            dir.display()
        )
        .map_err(|e| e.to_string())?;
    }
    state::remove_state_dir(&dir).map_err(|e| {
        let why = match e {
            RemoveError::Unknown(names) => {
                format!(
                    "entries ghostframe did not create appeared: {}",
                    names.join(", ")
                )
            }
            RemoveError::Io(e) => e.to_string(),
        };
        format!(
            "logged out of the tailnet, but the local state in {} was not fully removed: {why}",
            dir.display()
        )
    })?;
    drop(lock);
    writeln!(out, "logged out; removed {}", dir.display()).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    #[test]
    fn login_url_failures_read_as_instructions() {
        let t = tailnet_err(GhostbridgeError::Ffi("gbridge_login_url", -6));
        assert!(t.contains("within 60 s"), "{t}");
        let m = tailnet_err(GhostbridgeError::Ffi("gbridge_login_url", -8));
        assert!(m.contains("device approval"), "{m}");
        let o = tailnet_err(GhostbridgeError::Ffi("gbridge_login_url", -2));
        assert!(o.starts_with("tailnet: "), "{o}");
    }

    #[derive(Default)]
    struct Fake {
        dir: PathBuf,
        up_result_err: bool,
        url: Option<String>,
        url_err: bool,
        logout_err: bool,
        up_saw_lock_busy: Cell<bool>,
        logout_saw_state: Cell<bool>,
        url_calls: Cell<u32>,
        logout_calls: Cell<u32>,
        foreign_during_logout: bool,
    }

    impl Tailnet for &Fake {
        fn up(&self) -> Result<(), GhostbridgeError> {
            self.up_saw_lock_busy.set(matches!(
                StateLock::acquire(&self.dir),
                Err(state::LockError::Busy { .. })
            ));
            if self.up_result_err {
                return Err(GhostbridgeError::Ffi("gbridge_up", -3));
            }
            Ok(())
        }
        fn login_url(&self) -> Result<Option<String>, GhostbridgeError> {
            self.url_calls.set(self.url_calls.get() + 1);
            if self.url_err {
                return Err(GhostbridgeError::Ffi("gbridge_login_url", -6));
            }
            Ok(self.url.clone())
        }
        fn logout(&self) -> Result<(), GhostbridgeError> {
            self.logout_calls.set(self.logout_calls.get() + 1);
            if self.foreign_during_logout {
                std::fs::write(self.dir.join("holiday.jpg"), b"precious").unwrap();
            }
            self.logout_saw_state
                .set(self.dir.join("tailscaled.state").exists());
            if self.logout_err {
                return Err(GhostbridgeError::Ffi("gbridge_logout", -2));
            }
            Ok(())
        }
    }

    fn setup(env: &[(&str, &str)]) -> (tempfile::TempDir, ServerConfig, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let dir_s = dir.to_string_lossy().into_owned();
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .chain([("TS_STATE_DIR".into(), dir_s)])
            .collect();
        let cfg = ServerConfig::from_lookup(
            |k| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()),
            Some("host".into()),
        );
        (root, cfg, dir)
    }

    fn fake(dir: &std::path::Path) -> Fake {
        Fake {
            dir: dir.to_path_buf(),
            ..Fake::default()
        }
    }

    fn run_login(
        cfg: &ServerConfig,
        f: &Fake,
        hostname: Option<&str>,
        control: Option<&str>,
    ) -> (Result<(), String>, String, Option<GhostbridgeConfig>) {
        let seen = RefCell::new(None);
        let mut out = Vec::new();
        let r = login_with(
            cfg,
            hostname.map(Into::into),
            control.map(Into::into),
            false,
            |c: &GhostbridgeConfig| {
                *seen.borrow_mut() = Some(GhostbridgeConfig {
                    hostname: c.hostname.clone(),
                    authkey: c.authkey.clone(),
                    state_dir: c.state_dir.clone(),
                    control_url: c.control_url.clone(),
                    quiet: c.quiet,
                    non_interactive: c.non_interactive,
                    redact_auth_urls: c.redact_auth_urls,
                });
                Ok(f)
            },
            &mut Rec::new(dir_of(cfg)),
            &mut out,
        );
        (r, String::from_utf8(out).unwrap(), seen.into_inner())
    }

    fn dir_of(cfg: &ServerConfig) -> PathBuf {
        cfg.state_dir().unwrap()
    }

    /// Records `systemctl --user` calls and whether the state lock was free
    /// when each was made.
    struct Rec {
        dir: PathBuf,
        calls: Vec<String>,
        lock_free: Vec<bool>,
        fail_on: Option<String>,
    }

    impl Rec {
        fn new(dir: PathBuf) -> Self {
            Self {
                dir,
                calls: Vec::new(),
                lock_free: Vec::new(),
                fail_on: None,
            }
        }
    }

    impl Systemctl for Rec {
        fn user_output(&mut self, args: &[&str]) -> Result<String, String> {
            let call = args.join(" ");
            self.lock_free.push(StateLock::acquire(&self.dir).is_ok());
            self.calls.push(call.clone());
            if self.fail_on.as_deref() == Some(call.as_str()) {
                return Err(format!("systemctl --user {call} failed: injected"));
            }
            Ok(String::new())
        }
        fn is_active(&mut self, _: &str) -> bool {
            false
        }
        fn systemd_run(&mut self, _: &[String]) -> Result<(), String> {
            Err("not expected".into())
        }
    }

    /// A joined login with `HOME` (and optionally a user manager) in a
    /// sandbox; the unit file is written when `installed`.
    fn run_service(
        installed: bool,
        manager: bool,
        fail_on: Option<&str>,
    ) -> (Result<(), String>, String, Rec) {
        run_full(installed, manager, fail_on, (None, None), false, true)
    }

    /// `run_service` with the `--hostname`/`--control-url` flags, the
    /// state-dir mismatch flag, and whether `HOME` is set.
    fn run_full(
        installed: bool,
        manager: bool,
        fail_on: Option<&str>,
        flags: (Option<&str>, Option<&str>),
        differs: bool,
        home_set: bool,
    ) -> (Result<(), String>, String, Rec) {
        let home = tempfile::tempdir().unwrap();
        let h = home.path().to_string_lossy().into_owned();
        let mut env = Vec::new();
        if home_set {
            env.push(("HOME", h.as_str()));
        }
        if manager {
            env.push(("XDG_RUNTIME_DIR", h.as_str()));
        }
        let (_r, cfg, dir) = setup(&env);
        if installed {
            let unit = InstallPaths::new(home.path()).unit_dir.join(UNIT_NAME);
            std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
            std::fs::write(unit, b"[Service]\n").unwrap();
        }
        let f = Fake {
            url: None,
            ..fake(&dir)
        };
        let mut rec = Rec::new(dir);
        rec.fail_on = fail_on.map(Into::into);
        let mut out = Vec::new();
        let r = login_with(
            &cfg,
            flags.0.map(Into::into),
            flags.1.map(Into::into),
            differs,
            |_: &GhostbridgeConfig| Ok(&f),
            &mut rec,
            &mut out,
        );
        (r, String::from_utf8(out).unwrap(), rec)
    }

    #[test]
    fn a_state_dir_mismatch_enables_but_never_starts() {
        let (r, out, rec) = run_full(true, true, None, (None, None), true, true);
        r.unwrap();
        assert_eq!(rec.calls, ["enable ghostframe-server.service"]);
        assert!(out.contains("not starting"), "{out}");
        assert!(out.contains("will not see this login"), "{out}");
        assert!(
            out.contains("run: systemctl --user start ghostframe-server.service"),
            "{out}"
        );
    }

    #[test]
    fn a_hostname_note_enables_but_never_starts() {
        let (r, out, rec) = run_full(true, true, None, (Some("other"), None), false, true);
        r.unwrap();
        assert_eq!(rec.calls, ["enable ghostframe-server.service"]);
        assert!(out.contains("after adding the Environment= lines"), "{out}");
        assert!(
            out.contains("systemctl --user edit ghostframe-server"),
            "{out}"
        );
        assert!(
            out.contains("run: systemctl --user start ghostframe-server.service"),
            "{out}"
        );
    }

    #[test]
    fn a_control_url_note_enables_but_never_starts() {
        let (r, out, rec) = run_full(true, true, None, (None, Some("http://other")), false, true);
        r.unwrap();
        assert_eq!(rec.calls, ["enable ghostframe-server.service"]);
        assert!(out.contains("after adding the Environment= lines"), "{out}");
    }

    #[test]
    fn an_unset_home_prints_the_by_hand_line() {
        let (r, out, rec) = run_full(true, true, None, (None, None), false, false);
        r.unwrap();
        assert!(rec.calls.is_empty());
        assert!(
            out.contains("HOME is unset, cannot locate the unit"),
            "{out}"
        );
        assert!(
            out.contains("systemctl --user start ghostframe-server.service"),
            "{out}"
        );
    }

    #[test]
    fn a_joined_login_enables_then_starts_the_unit_with_the_lock_released() {
        let (r, out, rec) = run_service(true, true, None);
        r.unwrap();
        assert_eq!(
            rec.calls,
            [
                "enable ghostframe-server.service",
                "start ghostframe-server.service"
            ]
        );
        assert_eq!(rec.lock_free, [true, true], "lock still held");
        assert!(
            out.contains("enabled and started ghostframe-server.service (check: systemctl --user status ghostframe-server)"),
            "{out}"
        );
    }

    #[test]
    fn a_missing_unit_touches_no_systemd_and_hints_at_install() {
        let (r, out, rec) = run_service(false, true, None);
        r.unwrap();
        assert!(rec.calls.is_empty(), "{:?}", rec.calls);
        assert!(out.contains("ghostframe-server install"), "{out}");
    }

    #[test]
    fn no_user_manager_means_no_systemctl_and_a_by_hand_command() {
        let (r, out, rec) = run_service(true, false, None);
        r.unwrap();
        assert!(rec.calls.is_empty(), "{:?}", rec.calls);
        assert!(
            out.contains("systemctl --user start ghostframe-server.service"),
            "{out}"
        );
    }

    #[test]
    fn a_failing_start_warns_and_the_login_still_succeeds() {
        let (r, out, rec) = run_service(true, true, Some("start ghostframe-server.service"));
        r.unwrap();
        assert_eq!(rec.calls.len(), 2);
        assert!(out.contains("warning: logged in"), "{out}");
        assert!(out.contains("injected"), "{out}");
        assert!(out.contains("run by hand"), "{out}");
        assert!(
            !out.lines().any(|l| l.starts_with("enabled and started")),
            "{out}"
        );
    }

    #[test]
    fn a_failing_enable_skips_start() {
        let (r, out, rec) = run_service(true, true, Some("enable ghostframe-server.service"));
        r.unwrap();
        assert_eq!(rec.calls, ["enable ghostframe-server.service"]);
        assert!(out.contains("warning: logged in"), "{out}");
    }

    #[test]
    fn a_failed_join_starts_nothing() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path().to_string_lossy().into_owned();
        let (_r, cfg, dir) = setup(&[("HOME", &h), ("XDG_RUNTIME_DIR", &h)]);
        let unit = InstallPaths::new(home.path()).unit_dir.join(UNIT_NAME);
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(unit, b"[Service]\n").unwrap();
        let f = Fake {
            up_result_err: true,
            url: Some("http://u".into()),
            ..fake(&dir)
        };
        let mut rec = Rec::new(dir);
        let r = login_with(
            &cfg,
            None,
            None,
            false,
            |_: &GhostbridgeConfig| Ok(&f),
            &mut rec,
            &mut Vec::new(),
        );
        assert!(r.is_err());
        assert!(rec.calls.is_empty());
    }

    fn run_logout(cfg: &ServerConfig, f: &Fake) -> (Result<(), String>, String) {
        let mut out = Vec::new();
        let r = logout_with(cfg, None, None, |_: &GhostbridgeConfig| Ok(f), &mut out);
        (r, String::from_utf8(out).unwrap())
    }

    fn registered_state(dir: &std::path::Path) {
        std::fs::write(
            dir.join("tailscaled.state"),
            br#"{"_machinekey":"A","profile-1":"B"}"#,
        )
        .unwrap();
        state::mark_joined(dir).unwrap();
    }

    #[test]
    fn a_failing_logout_keeps_the_state_and_says_so() {
        let (_r, cfg, dir) = setup(&[]);
        registered_state(&dir);
        let f = Fake {
            logout_err: true,
            ..fake(&dir)
        };
        let (r, _) = run_logout(&cfg, &f);
        let e = r.unwrap_err();
        assert!(e.contains("state kept"), "{e}");
        assert!(e.contains("rm -r"), "the hand-removal hint: {e}");
        assert!(dir.join("tailscaled.state").exists());
        assert!(dir.join(".joined").exists());
    }

    #[test]
    fn the_state_still_exists_while_logout_runs_and_is_gone_after() {
        let (_r, cfg, dir) = setup(&[]);
        registered_state(&dir);
        let f = fake(&dir);
        let (r, out) = run_logout(&cfg, &f);
        r.unwrap();
        assert!(f.logout_saw_state.get(), "state deleted before logout()");
        assert!(!dir.exists());
        assert!(out.contains("logged out"), "{out}");
    }

    #[test]
    fn a_node_that_never_joined_is_not_deregistered() {
        let (_r, cfg, dir) = setup(&[]);
        std::fs::write(dir.join("tailscaled.state"), br#"{"_machinekey":"A"}"#).unwrap();
        let f = Fake {
            logout_err: true,
            ..fake(&dir)
        };
        let (r, _) = run_logout(&cfg, &f);
        r.unwrap();
        assert!(!f.logout_saw_state.get(), "logout() must not be called");
        assert!(!dir.exists());
    }

    #[test]
    fn logout_refuses_a_directory_with_foreign_files_and_deletes_nothing() {
        let (root, cfg, dir) = setup(&[]);
        registered_state(&dir);
        std::fs::write(dir.join("holiday.jpg"), b"precious").unwrap();
        let canary = root.path().join("canary");
        std::fs::write(&canary, b"x").unwrap();
        let f = fake(&dir);
        let (r, _) = run_logout(&cfg, &f);
        let e = r.unwrap_err();
        assert_eq!(
            f.logout_calls.get(),
            0,
            "refused up front, before deregistering"
        );
        assert!(
            state::state_dir_seeded(&dir),
            "still registered, still marked"
        );
        assert!(e.contains("holiday.jpg"), "{e}");
        assert!(e.contains("No change was made"), "{e}");
        assert!(dir.join("holiday.jpg").exists());
        assert!(dir.join("tailscaled.state").exists());
        assert!(canary.exists());
    }

    #[test]
    fn the_lock_is_held_while_up_runs() {
        let (_r, cfg, dir) = setup(&[]);
        let f = Fake {
            url: Some("https://login.example/a".into()),
            ..fake(&dir)
        };
        let (r, ..) = run_login(&cfg, &f, None, None);
        r.unwrap();
        assert!(f.up_saw_lock_busy.get());
    }

    #[test]
    fn an_auth_key_never_asks_for_a_url() {
        let (_r, cfg, dir) = setup(&[("TS_AUTHKEY", "tskey-x")]);
        let f = fake(&dir);
        let (r, out, seen) = run_login(&cfg, &f, None, None);
        r.unwrap();
        assert_eq!(f.url_calls.get(), 0);
        assert_eq!(seen.unwrap().authkey, "tskey-x");
        assert!(out.contains("logged in as"), "{out}");
        assert!(state::state_dir_seeded(&dir));
    }

    #[test]
    fn a_url_is_printed_exactly_once_and_none_means_already_authorised() {
        let url = "https://login.example/a/1234";
        let (_r, cfg, dir) = setup(&[]);
        let f = Fake {
            url: Some(url.into()),
            ..fake(&dir)
        };
        let (r, out, _) = run_login(&cfg, &f, None, None);
        r.unwrap();
        assert_eq!(out.matches(url).count(), 1, "{out}");
        assert!(out.contains("device approval"), "{out}");

        let (_r2, cfg2, dir2) = setup(&[]);
        let (r, out, _) = run_login(&cfg2, &fake(&dir2), None, None);
        r.unwrap();
        assert!(out.contains("already authorised"), "{out}");
        assert!(!out.contains("To authorise"), "{out}");
        assert!(state::state_dir_seeded(&dir2));
    }

    #[test]
    fn hostname_and_control_url_reach_the_bridge_and_notes_only_when_they_differ() {
        let (_r, cfg, dir) = setup(&[("TS_HOSTNAME", "same"), ("TS_CONTROL_URL", "http://hs")]);
        let mk = || Fake {
            url: None,
            ..fake(&dir)
        };
        let (r, out, seen) = run_login(&cfg, &mk(), Some("other"), Some("http://other"));
        r.unwrap();
        let seen = seen.unwrap();
        assert_eq!(seen.hostname, "other");
        assert_eq!(seen.control_url, "http://other");
        assert!(seen.quiet, "login must silence tsnet's log");
        assert!(out.contains("--hostname other"), "{out}");
        assert!(out.contains("--control-url http://other"), "{out}");

        let (r, out, seen) = run_login(&cfg, &mk(), Some("same"), Some("http://hs"));
        r.unwrap();
        assert_eq!(seen.unwrap().hostname, "same");
        assert!(!out.contains("note:"), "{out}");
        let (r, out, seen) = run_login(&cfg, &mk(), None, None);
        r.unwrap();
        assert_eq!(seen.unwrap().control_url, "http://hs");
        assert!(!out.contains("note:"), "{out}");
    }

    #[test]
    fn logout_passes_its_hostname_and_control_url_and_is_quiet() {
        let (_r, cfg, dir) = setup(&[]);
        registered_state(&dir);
        let f = fake(&dir);
        let seen = RefCell::new(None);
        logout_with(
            &cfg,
            Some("h".into()),
            Some("http://hs".into()),
            |c: &GhostbridgeConfig| {
                *seen.borrow_mut() = Some((c.hostname.clone(), c.control_url.clone(), c.quiet));
                Ok(&f)
            },
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            seen.into_inner().unwrap(),
            ("h".into(), "http://hs".into(), true)
        );
    }

    #[test]
    fn a_failed_or_interrupted_login_leaves_nothing_marked_joined() {
        // up() failing stands in for Ctrl-C, a timeout or a bad control URL:
        // every one of them leaves tsnet's own `{}` state file behind.
        let (_r, cfg, dir) = setup(&[]);
        std::fs::write(dir.join("tailscaled.state"), b"{}").unwrap();
        let f = Fake {
            up_result_err: true,
            url: Some("https://login.example/a".into()),
            ..fake(&dir)
        };
        let (r, ..) = run_login(&cfg, &f, None, None);
        assert!(r.is_err());
        assert!(!state::state_dir_seeded(&dir));

        let f = Fake {
            url_err: true,
            ..fake(&dir)
        };
        let (r, ..) = run_login(&cfg, &f, None, None);
        let e = r.unwrap_err();
        assert!(e.contains("within 60 s"), "{e}");
        assert!(!state::state_dir_seeded(&dir));
    }

    #[test]
    fn a_node_logged_out_is_never_left_marked_joined() {
        // A foreign file appears after the up-front scan; the removal then
        // fails. The node IS deregistered by then, so the marker must be gone
        // or the next `capture` starts an interactive tsnet.
        let (_r, cfg, dir) = setup(&[]);
        registered_state(&dir);
        let f = Fake {
            foreign_during_logout: true,
            ..fake(&dir)
        };
        let (r, _) = run_logout(&cfg, &f);
        let e = r.unwrap_err();
        assert_eq!(f.logout_calls.get(), 1);
        assert!(!state::state_dir_seeded(&dir), "marker survived a logout");
        assert!(dir.join("holiday.jpg").exists());
        assert!(e.contains("logged out"), "says what did happen: {e}");
        assert!(!e.contains("nothing was removed"), "{e}");
    }

    #[test]
    fn logout_says_when_it_removes_taildrop_files() {
        let (_r, cfg, dir) = setup(&[]);
        registered_state(&dir);
        std::fs::create_dir(dir.join("files")).unwrap();
        std::fs::write(dir.join("files/received.txt"), b"x").unwrap();
        let (r, out) = run_logout(&cfg, &fake(&dir));
        r.unwrap();
        assert!(out.contains("files/"), "{out}");
    }
}
