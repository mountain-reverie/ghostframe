//! `install` and `uninstall`: a per-user install with no root.
//!
//! Everything written lives under the user's home: the binary in
//! `~/.local/bin`, the unit in `~/.config/systemd/user`, the autostart entry
//! in `~/.config/autostart`. `install` also migrates away from the old
//! `packaging/install.sh` layout, removing only files whose names it knows,
//! and reports root-owned leftovers in `/etc` without touching them.
//!
//! Nothing here reads the environment or derives a path on its own: every
//! location comes in through [`InstallPaths`] and [`InstallContext`], and
//! every `systemctl` call through [`Systemctl`], so the tests can run it all
//! against a temporary home (a getent-derived path once escaped a test
//! sandbox in this repository and deleted real files).

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::cli::MaxResolution;
use crate::state;

pub const BINARY_NAME: &str = "ghostframe-server";
pub const UNIT_NAME: &str = "ghostframe-server.service";
pub const AUTOSTART_NAME: &str = "ghostframe-server.desktop";

const UNIT_TEMPLATE: &str = include_str!("../../templates/ghostframe-server.service.in");
const AUTOSTART_TEMPLATE: &str = include_str!("../../templates/ghostframe-server.desktop");

/// User units the old installer wrote, by exact name. `ghostframe-xorg*.service`
/// is matched by [`is_legacy_unit`] as well.
/// The old installer's daemon unit.
const LEGACY_XDAEMON_UNIT: &str = "ghostframe-xdaemon.service";
const LEGACY_UNITS: &[&str] = &[
    LEGACY_XDAEMON_UNIT,
    "ghostframe.target",
    "ghostframe-wm.service",
    "ghostframe-xorg.service",
];
/// Drop-in directories of legacy units (the 2026-10-29 stopgap lives in one).
const LEGACY_DROPIN_DIRS: &[&str] = &["ghostframe-xdaemon.service.d"];
/// The old attach-mode autostart entry.
/// The one file the 2026-10-29 stopgap put in the drop-in directory.
const STOPGAP_DROPIN: &str = "50-force-root-get-image.conf";
const LEGACY_AUTOSTART: &str = "ghostframe-attach.desktop";

/// Root-owned files the old installer wrote and nothing needs any more.
/// Listed with the command to remove them; never touched.
const ROOT_LEFTOVERS: &[&str] = &[
    "etc/systemd/system/getty@tty1.service.d/99-ghostframe-autologin.conf",
    "etc/X11/ghostframe-headless.conf",
];

fn is_legacy_unit(name: &str) -> bool {
    LEGACY_UNITS.contains(&name)
        || (name.starts_with("ghostframe-xorg") && name.ends_with(".service"))
}

/// Where `install` writes, derived from the home directory alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPaths {
    /// `~/.local/bin/ghostframe-server`; the unit's `%h/.local/bin/...`.
    pub bin: PathBuf,
    /// `~/.config/systemd/user`.
    pub unit_dir: PathBuf,
    /// `~/.config/autostart`.
    pub autostart_dir: PathBuf,
}

impl InstallPaths {
    pub fn new(home: &Path) -> Self {
        Self {
            bin: home.join(".local/bin").join(BINARY_NAME),
            unit_dir: home.join(".config/systemd/user"),
            autostart_dir: home.join(".config/autostart"),
        }
    }
}

/// Everything about the machine `install` needs, gathered by the caller.
pub struct InstallContext<'a> {
    pub paths: &'a InstallPaths,
    /// The running executable, copied to `paths.bin`.
    pub current_exe: &'a Path,
    /// The directory holding `etc/`: `/` in production.
    pub sysroot: &'a Path,
    /// The tsnet state directory, if one can be derived. Seeded means
    /// `.joined` exists there; a registered `tailscaled.state` without the
    /// marker (the old xdaemon's) is adopted once the old unit is stopped.
    pub state_dir: Option<&'a Path>,
    /// The user's login name, for the autologin check.
    pub username: &'a str,
    /// The effective uid; 0 is refused.
    pub euid: u32,
}

/// `systemctl --user` (and `systemd-run`), behind a trait so tests can
/// record the calls.
pub trait Systemctl {
    /// Run `systemctl --user <args>` and return its stdout; `Err` carries the
    /// reason.
    fn user_output(&mut self, args: &[&str]) -> Result<String, String>;
    /// Run `systemctl --user <args>`; `Err` carries the reason.
    fn user(&mut self, args: &[&str]) -> Result<(), String> {
        self.user_output(args).map(|_| ())
    }
    /// Whether the unit is active right now (false when unknown).
    fn is_active(&mut self, unit: &str) -> bool;
    /// Run `systemd-run <args>`.
    fn systemd_run(&mut self, args: &[String]) -> Result<(), String>;
}

/// Whether a failed `systemctl --user <args>` that exited with `code` counts
/// as success: `stop` of a unit the manager has not loaded exits 5 ("Unit …
/// not loaded"), and there is nothing to stop. Nothing else is forgiven.
pub fn stop_ok(args: &[&str], code: Option<i32>) -> bool {
    args.first() == Some(&"stop") && code == Some(5)
}

/// The real `systemctl`. `LC_ALL=C` keeps its messages stable.
pub struct RealSystemctl;

impl Systemctl for RealSystemctl {
    fn user_output(&mut self, args: &[&str]) -> Result<String, String> {
        let out = std::process::Command::new("systemctl")
            .env("LC_ALL", "C")
            .arg("--user")
            .args(args)
            .output()
            .map_err(|e| format!("cannot run systemctl: {e}"))?;
        if out.status.success() || stop_ok(args, out.status.code()) {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!(
                "systemctl --user {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn is_active(&mut self, unit: &str) -> bool {
        std::process::Command::new("systemctl")
            .env("LC_ALL", "C")
            .args(["--user", "is-active", "--quiet", unit])
            .status()
            .is_ok_and(|s| s.success())
    }

    fn systemd_run(&mut self, args: &[String]) -> Result<(), String> {
        let out = std::process::Command::new("systemd-run")
            .env("LC_ALL", "C")
            .args(args)
            .output()
            .map_err(|e| format!("cannot run systemd-run: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "systemd-run failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
}

/// True if a `/proc/self/cgroup` listing puts this process under the old
/// headless session's `ghostframe-wm.service` or `ghostframe-xorg*.service`:
/// stopping those kills it. Lines are `hierarchy:controllers:path` (cgroup v2
/// has one `0::path` line; v1 or hybrid has several, one of them
/// `name=systemd`).
pub fn in_legacy_headless_cgroup(cgroup: &str) -> bool {
    cgroup.lines().any(|line| {
        let mut parts = line.splitn(3, ':');
        let path = parts.nth(2).unwrap_or("");
        path.split('/').any(is_headless_session_unit)
    })
}

fn is_headless_session_unit(component: &str) -> bool {
    component == "ghostframe-wm.service"
        || (component.starts_with("ghostframe-xorg") && component.ends_with(".service"))
}

/// The `install` arguments to repeat in the transient unit.
pub fn install_args(force: bool, max: MaxResolution) -> Vec<String> {
    let mut a = vec!["install".to_string()];
    if force {
        a.push("--force".into());
    }
    a.push("--max-resolution".into());
    a.push(match max {
        MaxResolution::Limit(w, h) => format!("{w}x{h}"),
        MaxResolution::Unlimited => "none".into(),
    });
    a
}

/// If this process runs inside the old headless session, re-run `install` as
/// the transient user unit `ghostframe-install` (outside that cgroup, so
/// stopping the session cannot kill it) and return `true`; the caller then
/// exits. `exe` is the installed copy (`~/.local/bin/ghostframe-server`), so
/// the unit does not depend on a path that may vanish. `detached` is true in
/// the re-exec'd copy, which never re-execs again.
pub fn maybe_detach(
    sys: &mut dyn Systemctl,
    cgroup: &str,
    detached: bool,
    exe: &Path,
    install_args: &[String],
    out: &mut dyn Write,
) -> Result<bool, String> {
    if detached || !in_legacy_headless_cgroup(cgroup) {
        return Ok(false);
    }
    let mut args: Vec<String> = [
        "--user",
        "--collect",
        "--unit=ghostframe-install",
        "--setenv=GHOSTFRAME_INSTALL_DETACHED=1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(exe.display().to_string());
    args.extend(install_args.iter().cloned());
    sys.systemd_run(&args).map_err(|e| {
        if e.contains("already exists") {
            format!(
                "{e}\n  an install is already running: journalctl --user -u ghostframe-install -f"
            )
        } else {
            format!(
                "{e}\n  no unit was written and nothing was stopped (the binary itself may \
                 have been copied); re-run `ghostframe-server install`"
            )
        }
    })?;
    let _ = writeln!(
        out,
        "the old headless session will end; install continues as ghostframe-install.service \
         - follow it with `journalctl --user -u ghostframe-install -f` (e.g. over ssh), or \
         reconnect once ghostframe-server is up.\n  Exit status 0 here means the install was \
         handed off, not that it has finished."
    );
    Ok(true)
}

/// `TS_STATE_DIR` from the value of `systemctl --user show -p Environment
/// --value <unit>`: space-separated `KEY=value` words, a word containing
/// spaces wrapped in double quotes. Specifiers (`%S`) are already expanded.
/// Only an absolute path is accepted.
pub fn ts_state_dir_from_environment(env_line: &str) -> Option<PathBuf> {
    let mut words = Vec::new();
    let (mut cur, mut quoted, mut chars) = (String::new(), false, env_line.trim().chars());
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' if quoted => cur.extend(chars.next()),
            ' ' if !quoted => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    // systemd applies the last assignment (a drop-in overrides the unit).
    let v = words
        .iter()
        .rev()
        .find_map(|w| w.strip_prefix("TS_STATE_DIR="))?;
    Path::new(v).is_absolute().then(|| PathBuf::from(v))
}

/// Refuse `login` and `capture` while the old installer's xdaemon is still
/// installed or running: it shares the tailnet state directory and takes no
/// lock on it. `active` is asked only when the file check passes.
pub fn refuse_if_legacy_xdaemon(
    home: Option<&Path>,
    active: impl FnOnce() -> bool,
) -> Result<(), String> {
    if let Some(h) = home {
        if InstallPaths::new(h)
            .unit_dir
            .join(LEGACY_XDAEMON_UNIT)
            .symlink_metadata()
            .is_ok()
        {
            return Err(
                "the old ghostframe-xdaemon is installed or running; run `ghostframe-server \
                 install` first"
                    .into(),
            );
        }
    }
    if active() {
        return Err(format!(
            "the old ghostframe-xdaemon is installed or running; run `systemctl --user stop \
             {LEGACY_XDAEMON_UNIT}`, then `ghostframe-server install`"
        ));
    }
    Ok(())
}

/// The unit file, with `GHOSTFRAME_MAX_RESOLUTION` set or left out.
pub fn render_unit(max: MaxResolution) -> String {
    let mut out = String::with_capacity(UNIT_TEMPLATE.len());
    for line in UNIT_TEMPLATE.lines() {
        if line == "@MAX_RESOLUTION@" {
            if let MaxResolution::Limit(w, h) = max {
                out.push_str(&format!("Environment=GHOSTFRAME_MAX_RESOLUTION={w}x{h}\n"));
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The XDG autostart entry.
pub fn render_autostart() -> &'static str {
    AUTOSTART_TEMPLATE
}

/// A `systemctl` failure right after the files were written, before the old
/// units were touched: only a re-run is safe (`enable --now` would start the
/// new service next to a running old one).
fn retry_hint(e: String) -> String {
    format!("{e}\n  re-running `ghostframe-server install` is safe and finishes the job")
}

/// A `systemctl` failure after the files are written: install is idempotent,
/// so say how to finish.
fn finish_hint(e: String) -> String {
    format!(
        "{e}\n  re-running `ghostframe-server install` is safe; to finish by hand: \
         `systemctl --user daemon-reload && systemctl --user enable --now ghostframe-server.service`"
    )
}

/// Resolve the home directory `install` writes under. The unit's `%h`
/// resolves to the passwd entry's home, so `HOME` must be absolute and equal
/// to it (the same directory, symlinks resolved), or the files written would
/// not be the ones the unit points at. The passwd spelling is returned.
pub fn validated_home(
    env_home: Option<&str>,
    passwd_home: Option<&str>,
) -> Result<PathBuf, String> {
    let home = env_home.ok_or("HOME is not set; install writes under the user's home directory")?;
    if !Path::new(home).is_absolute() {
        return Err(format!("HOME={home:?} is not an absolute path"));
    }
    let pw = passwd_home.ok_or("this uid has no passwd entry, so its home cannot be checked")?;
    // Same directory through a symlink or a trailing slash is fine; when
    // either cannot be resolved, compare literally.
    let same = match (Path::new(home).canonicalize(), Path::new(pw).canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => Path::new(home) == Path::new(pw),
    };
    if !same {
        return Err(format!(
            "HOME={home} is not your passwd home {pw}, which is what the unit's %h resolves \
             to; run install as yourself with a login shell"
        ));
    }
    // The passwd home is what `%h` resolves to: write there.
    Ok(PathBuf::from(pw))
}

/// `install` and its detach step both refuse root first.
pub fn refuse_root(euid: u32) -> Result<(), String> {
    if euid == 0 {
        return Err(
            "refusing to install as root: run `ghostframe-server install` as the \
                    desktop user whose session it should serve"
                .into(),
        );
    }
    Ok(())
}

/// `install`: see the design, section 3.1, for the steps. Order, and why:
///
/// 1. Refuse root; require a reachable user manager (`show-environment`).
///    Nothing has been touched yet, so a failure here is free.
/// 2. Warn if a headless `ghostframe-xorg` session is active: stopping it
///    ends the session this may be running in (SIGHUP is ignored by the
///    caller, so install still finishes).
/// 3. Everything that does not depend on the old units being stopped:
///    binary, unit, autostart, `daemon-reload`, `enable`.
/// 4. Stop every legacy unit by name, file or no file. A failure is fatal,
///    and nothing below has happened: the legacy files are intact, so a
///    re-run retries the stop.
/// 5. Only now delete the legacy files and `daemon-reload` again.
/// 6. Only now, with the old xdaemon confirmed stopped, read its state and
///    adopt a registration that predates the marker.
/// 7. Last: start the new service, if the node has joined.
pub fn install(
    ctx: &InstallContext,
    force: bool,
    max: MaxResolution,
    sys: &mut dyn Systemctl,
    out: &mut dyn Write,
) -> Result<(), String> {
    let w = |out: &mut dyn Write, s: String| {
        let _ = writeln!(out, "{s}");
    };
    // 1.
    refuse_root(ctx.euid)?;
    sys.user(&["show-environment"]).map_err(|e| {
        format!(
            "run `ghostframe-server install` from your desktop session (no user systemd \
             manager reachable): {e}"
        )
    })?;

    // 2.
    let stop_names = legacy_stop_names(ctx.paths)?;
    for n in stop_names
        .iter()
        .filter(|n| n.starts_with("ghostframe-xorg"))
    {
        if sys.is_active(n) {
            w(
                out,
                format!("warning: {n} is active; stopping it ends that graphical session"),
            );
        }
    }

    // 3.
    match copy_binary(ctx.current_exe, &ctx.paths.bin, force)? {
        CopyOutcome::AlreadyRunningFromTarget => w(
            out,
            format!("binary: already running from {}", ctx.paths.bin.display()),
        ),
        CopyOutcome::Unchanged => w(
            out,
            format!("binary: {} is up to date", ctx.paths.bin.display()),
        ),
        CopyOutcome::Copied => w(
            out,
            format!("binary: installed {}", ctx.paths.bin.display()),
        ),
    }
    write_file(&ctx.paths.unit_dir.join(UNIT_NAME), &render_unit(max))?;
    w(
        out,
        format!(
            "unit: wrote {}",
            ctx.paths.unit_dir.join(UNIT_NAME).display()
        ),
    );
    // Enlightenment never activates graphical-session.target.
    write_file(
        &ctx.paths.autostart_dir.join(AUTOSTART_NAME),
        render_autostart(),
    )?;
    w(
        out,
        format!(
            "autostart: wrote {}",
            ctx.paths.autostart_dir.join(AUTOSTART_NAME).display()
        ),
    );
    sys.user(&["daemon-reload"]).map_err(retry_hint)?;
    sys.user(&["enable", UNIT_NAME]).map_err(retry_hint)?;

    // The state directory is the one the *unit* uses, as the user manager
    // expanded it, not whatever this shell derives.
    let unit_state = match sys
        .user_output(&["show", "-p", "Environment", "--value", UNIT_NAME])
        .ok()
        .and_then(|o| ts_state_dir_from_environment(&o))
    {
        Some(d) => Some(d),
        None => {
            w(
                out,
                "warning: could not read TS_STATE_DIR from the user manager; using the \
                 derived state directory"
                    .into(),
            );
            ctx.state_dir.map(Path::to_path_buf)
        }
    };
    let state_dir = unit_state.as_deref();

    // 4.
    for name in &stop_names {
        sys.user(&["stop", name]).map_err(|e| {
            format!(
                "{e}\n  the old unit may still be running, so nothing of it was removed and \
                 the tailnet state was not touched; fix that and re-run `ghostframe-server install`"
            )
        })?;
    }

    // 5.
    for removed in remove_legacy(ctx.paths, out)? {
        w(out, format!("migrate: removed {}", removed.display()));
    }
    for leftover in root_leftovers(ctx.sysroot) {
        w(
            out,
            format!(
                "leftover from the old root install: {leftover}\n  remove it with: sudo rm {leftover}"
            ),
        );
    }
    sys.user(&["daemon-reload"]).map_err(finish_hint)?;

    // 6. The old xdaemon registered the node without writing the marker.
    let mut seeded = state_dir.is_some_and(state::state_dir_seeded);
    if let Some(dir) = state_dir {
        if !seeded && state::state_is_registered(dir) {
            state::mark_joined(dir).map_err(|e| {
                format!(
                    "recording the tailnet registration in {}: {e}",
                    dir.display()
                )
            })?;
            seeded = true;
            w(
                out,
                "state: adopted the existing tailnet registration".into(),
            );
        }
    }

    // 7.
    if seeded {
        sys.user(&["restart", UNIT_NAME]).map_err(finish_hint)?;
        w(out, format!("service: enabled and (re)started {UNIT_NAME}"));
    } else {
        w(
            out,
            format!(
                "service: enabled {UNIT_NAME}, not started: this node has not joined a tailnet.\n  \
                 run `ghostframe-server login`, then `systemctl --user start ghostframe-server`.\n  \
                 Do not run `login` while the old ghostframe-xdaemon is still up: both would use \
                 one tailnet state directory."
            ),
        );
    }

    // Unattended boot needs the session to exist: report, never fail.
    if lightdm_autologin_user(ctx.sysroot).as_deref() == Some(ctx.username) {
        w(
            out,
            format!("autologin: LightDM logs {} in at boot", ctx.username),
        );
    } else {
        w(out, autologin_hint(ctx.username));
    }
    Ok(())
}

/// `uninstall`: remove what `install` wrote. The tailnet state stays; that is
/// `logout`'s to remove.
pub fn uninstall(
    paths: &InstallPaths,
    sys: &mut dyn Systemctl,
    out: &mut dyn Write,
) -> Result<(), String> {
    // Not installed, or the user manager is gone: removing the files below
    // is still right, so report and continue.
    let mut disable_failed = false;
    if let Err(e) = sys.user(&["disable", "--now", UNIT_NAME]) {
        disable_failed = true;
        let _ = writeln!(out, "service: {e}");
    }
    // `disable` also removes the enable link; when it could not run, remove
    // that link by name or a dangling one keeps the unit "wanted".
    let wants = paths
        .unit_dir
        .join("graphical-session.target.wants")
        .join(UNIT_NAME);
    for path in [
        paths.unit_dir.join(UNIT_NAME),
        wants,
        paths.autostart_dir.join(AUTOSTART_NAME),
        paths.bin.clone(),
    ] {
        if remove_if_present(&path, false)? {
            let _ = writeln!(out, "removed {}", path.display());
        }
    }
    if let Err(e) = sys.user(&["daemon-reload"]) {
        disable_failed = true;
        let _ = writeln!(out, "service: {e}");
    }
    if disable_failed {
        let _ = writeln!(
            out,
            "warning: the user manager could not be told; if the service is still running, \
             stop it with `systemctl --user stop ghostframe-server.service` from your desktop session"
        );
    }
    let _ = writeln!(
        out,
        "the tailnet state is kept; `ghostframe-server logout` removes it"
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    AlreadyRunningFromTarget,
    Unchanged,
    Copied,
}

/// Copy `exe` to `target`, refusing to replace a different binary without
/// `force`. Writes beside the target and renames over it, so a running copy
/// of the old binary is never written into.
pub fn copy_binary(exe: &Path, target: &Path, force: bool) -> Result<CopyOutcome, String> {
    if target.exists() {
        let same_file = match (exe.canonicalize(), target.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if same_file {
            return Ok(CopyOutcome::AlreadyRunningFromTarget);
        }
        let new = std::fs::read(exe).map_err(|e| format!("reading {}: {e}", exe.display()))?;
        let old =
            std::fs::read(target).map_err(|e| format!("reading {}: {e}", target.display()))?;
        if new == old {
            return Ok(CopyOutcome::Unchanged);
        }
        if !force {
            return Err(format!(
                "{} exists and differs from this binary; re-run with --force to replace it",
                target.display()
            ));
        }
    }
    let dir = target
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", target.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let staging = target.with_extension("new");
    // A leftover from an interrupted run (or a planted one) must not be
    // written through: unlink it, then create exclusively.
    match std::fs::remove_file(&staging) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("removing {}: {e}", staging.display())),
    }
    let stage = || -> Result<(), String> {
        let mut src =
            std::fs::File::open(exe).map_err(|e| format!("opening {}: {e}", exe.display()))?;
        let mut dst = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&staging)
            .map_err(|e| format!("creating {}: {e}", staging.display()))?;
        std::io::copy(&mut src, &mut dst)
            .map_err(|e| format!("copying to {}: {e}", staging.display()))?;
        dst.sync_all()
            .map_err(|e| format!("syncing {}: {e}", staging.display()))?;
        std::fs::rename(&staging, target)
            .map_err(|e| format!("renaming onto {}: {e}", target.display()))?;
        std::fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| format!("syncing {}: {e}", dir.display()))
    };
    if let Err(e) = stage() {
        let _ = std::fs::remove_file(&staging);
        return Err(e);
    }
    Ok(CopyOutcome::Copied)
}

fn write_file(path: &Path, contents: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    std::fs::write(path, contents).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Remove a file or symlink if it exists; never follows a symlink. A
/// directory is refused unless `allow_dir` (only the listed drop-in
/// directories). Returns whether anything was there.
fn remove_if_present(path: &Path, allow_dir: bool) -> Result<bool, String> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("stat {}: {e}", path.display())),
    };
    let res = if md.is_dir() {
        if !allow_dir {
            return Err(format!(
                "{} is a directory, not a file or symlink; refusing to remove it, \
                 move it away and re-run",
                path.display()
            ));
        }
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    res.map_err(|e| format!("removing {}: {e}", path.display()))?;
    Ok(true)
}

/// Every legacy unit to stop, by name: the fixed names whether or not their
/// file exists (a loaded unit can outlive its file), plus any
/// `ghostframe-xorg*.service` file present.
fn legacy_stop_names(paths: &InstallPaths) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = LEGACY_UNITS.iter().map(|s| s.to_string()).collect();
    match std::fs::read_dir(&paths.unit_dir) {
        Ok(rd) => {
            let mut extra: Vec<String> = rd
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| is_legacy_unit(n) && !LEGACY_UNITS.contains(&n.as_str()))
                .collect();
            extra.sort();
            names.extend(extra);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("reading {}: {e}", paths.unit_dir.display())),
    }
    Ok(names)
}

/// Delete the old installer's unit files, drop-ins, the `*.wants/` links
/// `enable` made for them, and the old autostart entry. Only names in the
/// legacy lists are touched; the units are already stopped.
fn remove_legacy(paths: &InstallPaths, out: &mut dyn Write) -> Result<Vec<PathBuf>, String> {
    let mut removed = Vec::new();
    let entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(&paths.unit_dir) {
        Ok(rd) => rd.filter_map(Result::ok).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("reading {}: {e}", paths.unit_dir.display())),
    };
    for entry in &entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_legacy_unit(&name) {
            if remove_if_present(&entry.path(), false)? {
                removed.push(entry.path());
            }
        } else if LEGACY_DROPIN_DIRS.contains(&name.as_str()) {
            let mut foreign = false;
            let mut files: Vec<String> = std::fs::read_dir(entry.path())
                .map(|rd| {
                    rd.filter_map(Result::ok)
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            files.sort();
            for f in &files {
                let _ = writeln!(out, "migrate: removing drop-in {name}/{f}");
                foreign |= f != STOPGAP_DROPIN;
            }
            if remove_if_present(&entry.path(), true)? {
                removed.push(entry.path());
            }
            if foreign {
                let _ = writeln!(
                    out,
                    "  {name}/ held a file other than {STOPGAP_DROPIN}: if it carried settings \
                     you want, recreate them with `systemctl --user edit ghostframe-server` \
                     (GHOSTFRAME_ATTACH_MAX_RESOLUTION is now GHOSTFRAME_MAX_RESOLUTION)"
                );
            }
        } else if name.ends_with(".wants") && entry.file_type().is_ok_and(|t| t.is_dir()) {
            for link in std::fs::read_dir(entry.path())
                .map_err(|e| format!("reading {}: {e}", entry.path().display()))?
                .filter_map(Result::ok)
            {
                let link_name = link.file_name().to_string_lossy().into_owned();
                if is_legacy_unit(&link_name) && remove_if_present(&link.path(), false)? {
                    removed.push(link.path());
                }
            }
            // `enable` of a legacy unit created this directory (e.g.
            // `ghostframe.target.wants`): remove it only if now empty.
            if name.strip_suffix(".wants").is_some_and(is_legacy_unit)
                && std::fs::remove_dir(entry.path()).is_ok()
            {
                removed.push(entry.path());
            }
        }
    }
    let autostart = paths.autostart_dir.join(LEGACY_AUTOSTART);
    if remove_if_present(&autostart, false)? {
        removed.push(autostart);
    }
    removed.sort();
    Ok(removed)
}

/// The root-owned leftovers present under `sysroot`, as absolute paths.
fn root_leftovers(sysroot: &Path) -> Vec<String> {
    ROOT_LEFTOVERS
        .iter()
        .filter(|rel| sysroot.join(rel).exists())
        .map(|rel| format!("/{rel}"))
        .collect()
}

/// The user LightDM logs in automatically, if any: the last uncommented
/// `autologin-user=` in `etc/lightdm/lightdm.conf.d/*.conf` (sorted), then
/// `etc/lightdm/lightdm.conf`, which LightDM reads last.
fn lightdm_autologin_user(sysroot: &Path) -> Option<String> {
    let dir = sysroot.join("etc/lightdm");
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.join("lightdm.conf.d"))
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "conf"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files.push(dir.join("lightdm.conf"));
    let mut user = None;
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == "autologin-user" {
                    let v = v.trim();
                    user = (!v.is_empty()).then(|| v.to_string());
                }
            }
        }
    }
    user
}

/// The one root step an unattended boot needs.
fn autologin_hint(username: &str) -> String {
    format!(
        "autologin: none found for {username}. After a reboot the service starts only once \
         {username} logs in.\n  For an unattended boot, as root:\n    \
         sudo mkdir -p /etc/lightdm/lightdm.conf.d\n    \
         printf '[Seat:*]\\nautologin-user={username}\\nautologin-user-timeout=0\\n' | \
         sudo tee /etc/lightdm/lightdm.conf.d/99-ghostframe-autologin.conf"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Records `systemctl --user` calls instead of making them.
    #[derive(Default)]
    struct Recorder {
        calls: Vec<String>,
        /// Calls (joined args) that fail.
        fail_on: Vec<String>,
        /// Units reported active.
        active: Vec<String>,
        /// When this call is made, write a registered `tailscaled.state`
        /// into the directory: the old xdaemon registering "while it runs",
        /// visible only if the state is read after the stop.
        write_state_on: Option<(String, PathBuf)>,
        /// Answer to `show -p Environment`.
        env_line: Option<String>,
        /// `enable` must find this unit file already written.
        unit_path: Option<PathBuf>,
        /// `systemd-run` argument lists.
        runs: Vec<Vec<String>>,
        /// `systemd-run` fails with this.
        run_err: Option<String>,
    }

    impl Systemctl for Recorder {
        fn user_output(&mut self, args: &[&str]) -> Result<String, String> {
            let call = args.join(" ");
            self.calls.push(call.clone());
            if call.starts_with("enable ") {
                if let Some(u) = &self.unit_path {
                    assert!(u.exists(), "enable before the unit file was written");
                }
            }
            if let Some((on, dir)) = &self.write_state_on {
                if *on == call {
                    write_state(dir, true);
                }
            }
            if self.fail_on.contains(&call) {
                return Err(format!("systemctl --user {call} failed: injected"));
            }
            if call.starts_with("show -p Environment") {
                return Ok(self.env_line.clone().unwrap_or_default());
            }
            Ok(String::new())
        }

        fn systemd_run(&mut self, args: &[String]) -> Result<(), String> {
            self.runs.push(args.to_vec());
            match &self.run_err {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }

        fn is_active(&mut self, unit: &str) -> bool {
            self.active.iter().any(|u| u == unit)
        }
    }

    /// A sandbox: `root/home` is HOME, `root/sysroot` holds `etc/`, and a
    /// canary that carries a legacy unit's *name* sits outside both. Every
    /// test ends by checking the canary survived.
    struct Sandbox {
        root: tempfile::TempDir,
        paths: InstallPaths,
        exe: PathBuf,
        state: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let paths = InstallPaths::new(&root.path().join("home"));
            let exe = root.path().join("build/ghostframe-server");
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::write(&exe, b"binary v2").unwrap();
            let canary = root.path().join("canary/ghostframe-xdaemon.service");
            std::fs::create_dir_all(canary.parent().unwrap()).unwrap();
            std::fs::write(&canary, b"must survive").unwrap();
            let state = root.path().join("home/.local/state/ghostframe");
            Self {
                root,
                paths,
                exe,
                state,
            }
        }

        fn sysroot(&self) -> PathBuf {
            self.root.path().join("sysroot")
        }

        fn ctx<'a>(&'a self, sysroot: &'a Path, seeded: bool) -> InstallContext<'a> {
            if seeded {
                self.touch(&self.state.join(state::JOINED_FILE));
            }
            InstallContext {
                paths: &self.paths,
                current_exe: &self.exe,
                sysroot,
                state_dir: Some(&self.state),
                username: "alice",
                euid: 1000,
            }
        }

        fn touch(&self, path: &Path) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"x").unwrap();
        }

        fn run_install(&self, seeded: bool, force: bool) -> (Result<(), String>, Recorder, String) {
            self.run_install_with(seeded, force, Recorder::default())
        }

        fn run_install_with(
            &self,
            seeded: bool,
            force: bool,
            mut sys: Recorder,
        ) -> (Result<(), String>, Recorder, String) {
            sys.unit_path = Some(self.paths.unit_dir.join(UNIT_NAME));
            let sysroot = self.sysroot();
            let mut out = Vec::new();
            let r = install(
                &self.ctx(&sysroot, seeded),
                force,
                MaxResolution::Limit(1920, 1080),
                &mut sys,
                &mut out,
            );
            (r, sys, String::from_utf8(out).unwrap())
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let canary = self.root.path().join("canary/ghostframe-xdaemon.service");
            if !std::thread::panicking() {
                assert_eq!(std::fs::read(&canary).unwrap(), b"must survive");
            }
        }
    }

    #[test]
    fn unit_template_renders_the_limit_or_leaves_it_out() {
        let limited = render_unit(MaxResolution::Limit(2560, 1440));
        assert!(limited.contains("\nEnvironment=GHOSTFRAME_MAX_RESOLUTION=2560x1440\n"));
        assert!(limited.contains("\nExecStart=%h/.local/bin/ghostframe-server capture\n"));
        assert!(limited.contains("\nEnvironment=TS_STATE_DIR=%S/ghostframe\n"));
        assert!(limited.contains("\nEnvironment=DISPLAY=:0\n"));
        assert!(limited.contains("\nWantedBy=graphical-session.target\n"));
        assert!(!limited.contains('@'), "placeholder left in:\n{limited}");
        let open = render_unit(MaxResolution::Unlimited);
        assert!(!open.contains("GHOSTFRAME_MAX_RESOLUTION="));
        assert!(!open.contains('@'));
    }

    #[test]
    fn autostart_starts_the_service() {
        assert!(render_autostart()
            .contains("\nExec=systemctl --user start ghostframe-server.service\n"));
    }

    #[test]
    fn install_writes_binary_unit_and_autostart_and_does_not_start_unseeded() {
        let sb = Sandbox::new();
        let (r, sys, out) = sb.run_install(false, false);
        r.unwrap();
        assert_eq!(std::fs::read(&sb.paths.bin).unwrap(), b"binary v2");
        let mode = std::fs::metadata(&sb.paths.bin)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            std::fs::read_to_string(sb.paths.unit_dir.join(UNIT_NAME)).unwrap(),
            render_unit(MaxResolution::Limit(1920, 1080))
        );
        assert!(sb.paths.autostart_dir.join(AUTOSTART_NAME).exists());
        assert_eq!(
            sys.calls,
            [
                "show-environment",
                "daemon-reload",
                "enable ghostframe-server.service",
                "show -p Environment --value ghostframe-server.service",
                "stop ghostframe-xdaemon.service",
                "stop ghostframe.target",
                "stop ghostframe-wm.service",
                "stop ghostframe-xorg.service",
                "daemon-reload"
            ]
        );
        assert!(out.contains("run `ghostframe-server login`"), "{out}");
    }

    #[test]
    fn install_restarts_the_service_when_seeded() {
        let sb = Sandbox::new();
        let (r, sys, _) = sb.run_install(true, false);
        r.unwrap();
        assert_eq!(
            sys.calls,
            [
                "show-environment",
                "daemon-reload",
                "enable ghostframe-server.service",
                "show -p Environment --value ghostframe-server.service",
                "stop ghostframe-xdaemon.service",
                "stop ghostframe.target",
                "stop ghostframe-wm.service",
                "stop ghostframe-xorg.service",
                "daemon-reload",
                "restart ghostframe-server.service"
            ]
        );
    }

    #[test]
    fn install_refuses_root() {
        let sb = Sandbox::new();
        let sysroot = sb.sysroot();
        let mut ctx = sb.ctx(&sysroot, false);
        ctx.euid = 0;
        let mut sys = Recorder::default();
        let e = install(
            &ctx,
            false,
            MaxResolution::Unlimited,
            &mut sys,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(e.contains("refusing to install as root"), "{e}");
        assert!(!sb.paths.bin.exists() && sys.calls.is_empty());
    }

    #[test]
    fn a_different_installed_binary_needs_force() {
        let sb = Sandbox::new();
        sb.touch(&sb.paths.bin);
        let (r, _, _) = sb.run_install(false, false);
        assert!(r.unwrap_err().contains("--force"));
        assert_eq!(std::fs::read(&sb.paths.bin).unwrap(), b"x");
        let (r, _, _) = sb.run_install(false, true);
        r.unwrap();
        assert_eq!(std::fs::read(&sb.paths.bin).unwrap(), b"binary v2");
    }

    #[test]
    fn running_from_the_target_skips_the_copy() {
        let sb = Sandbox::new();
        std::fs::create_dir_all(sb.paths.bin.parent().unwrap()).unwrap();
        std::fs::copy(&sb.exe, &sb.paths.bin).unwrap();
        assert_eq!(
            copy_binary(&sb.paths.bin, &sb.paths.bin, false).unwrap(),
            CopyOutcome::AlreadyRunningFromTarget
        );
        assert_eq!(
            copy_binary(&sb.exe, &sb.paths.bin, false).unwrap(),
            CopyOutcome::Unchanged
        );
    }

    #[test]
    fn migration_removes_exactly_the_legacy_names() {
        let sb = Sandbox::new();
        let units = &sb.paths.unit_dir;
        let legacy = [
            units.join("ghostframe-xdaemon.service"),
            units.join("ghostframe-xdaemon.service.d/force-root.conf"),
            units.join("ghostframe.target"),
            units.join("ghostframe-wm.service"),
            units.join("ghostframe-xorg.service"),
            units.join("ghostframe-xorg-vt7.service"),
            units.join("default.target.wants/ghostframe.target"),
            units.join("graphical-session.target.wants/ghostframe-xdaemon.service"),
            sb.paths.autostart_dir.join("ghostframe-attach.desktop"),
        ];
        let decoys = [
            units.join("ghostframe-xdaemon.service.bak"),
            units.join("ghostframe-xorg.timer"),
            units.join("my-ghostframe.target"),
            units.join("default.target.wants/pipewire.service"),
            units.join("other.service"),
            sb.paths
                .autostart_dir
                .join("ghostframe-attach.desktop.orig"),
        ];
        for p in legacy.iter().chain(&decoys) {
            sb.touch(p);
        }
        let (r, sys, _) = sb.run_install(false, false);
        r.unwrap();
        for p in &legacy {
            assert!(!p.exists(), "{} survived", p.display());
        }
        assert!(!units.join("ghostframe-xdaemon.service.d").exists());
        for p in &decoys {
            assert!(p.exists(), "{} was removed", p.display());
        }
        for unit in [
            "ghostframe-xdaemon.service",
            "ghostframe.target",
            "ghostframe-wm.service",
            "ghostframe-xorg.service",
        ] {
            assert!(
                sys.calls.contains(&format!("stop {unit}")),
                "{unit} not stopped: {:?}",
                sys.calls
            );
        }
    }

    #[test]
    fn root_leftovers_are_listed_and_left_alone() {
        let sb = Sandbox::new();
        let conf = sb.sysroot().join("etc/X11/ghostframe-headless.conf");
        sb.touch(&conf);
        let (r, _, out) = sb.run_install(false, false);
        r.unwrap();
        assert!(
            out.contains("sudo rm /etc/X11/ghostframe-headless.conf"),
            "{out}"
        );
        assert!(!out.contains("getty@tty1"), "{out}");
        assert!(conf.exists());
    }

    #[test]
    fn lightdm_autologin_is_detected_with_lightdm_conf_read_last() {
        let sb = Sandbox::new();
        let etc = sb.sysroot().join("etc/lightdm");
        assert_eq!(lightdm_autologin_user(&sb.sysroot()), None);
        std::fs::create_dir_all(etc.join("lightdm.conf.d")).unwrap();
        std::fs::write(
            etc.join("lightdm.conf.d/99-ghostframe-autologin.conf"),
            "[Seat:*]\nautologin-user=alice\n",
        )
        .unwrap();
        assert_eq!(
            lightdm_autologin_user(&sb.sysroot()).as_deref(),
            Some("alice")
        );
        // A commented line does not count; an uncommented empty value in the
        // main file, read last, wins and means nobody.
        std::fs::write(etc.join("lightdm.conf"), "#autologin-user=bob\n").unwrap();
        assert_eq!(
            lightdm_autologin_user(&sb.sysroot()).as_deref(),
            Some("alice")
        );
        std::fs::write(etc.join("lightdm.conf"), "[Seat:*]\nautologin-user=\n").unwrap();
        assert_eq!(lightdm_autologin_user(&sb.sysroot()), None);
    }

    #[test]
    fn install_prints_the_autologin_step_only_when_missing() {
        let sb = Sandbox::new();
        let (_, _, out) = sb.run_install(false, false);
        assert!(out.contains("autologin-user=alice"), "{out}");
        let etc = sb.sysroot().join("etc/lightdm/lightdm.conf.d");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join("50-x.conf"), "[Seat:*]\nautologin-user=alice\n").unwrap();
        let (_, _, out) = sb.run_install(false, false);
        assert!(out.contains("LightDM logs alice in at boot"), "{out}");
        assert!(!out.contains("sudo tee"), "{out}");
    }

    /// A `tailscaled.state` of the real schema (see
    /// `state::state_is_registered`), with made-up values and no keys.
    fn write_state(dir: &Path, registered: bool) {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;
        let (node, login) = if registered {
            ("nSYNTHETIC1", "synthetic@example.invalid")
        } else {
            ("", "")
        };
        let prefs = serde_json::json!({
            "Config": {"NodeID": node, "UserProfile": {"LoginName": login}},
            "LoggedOut": false
        });
        let map = serde_json::json!({
            "_current-profile": b64.encode("profile-synthetic"),
            "profile-synthetic": b64.encode(prefs.to_string()),
        });
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("tailscaled.state"), map.to_string()).unwrap();
    }

    /// The legacy layout the owner's machine has: the old unit, the stopgap
    /// drop-in, the headless units, the old autostart entry, a registered
    /// state with no marker.
    fn legacy_layout(sb: &Sandbox) -> Vec<PathBuf> {
        let units = &sb.paths.unit_dir;
        let legacy = vec![
            units.join("ghostframe-xdaemon.service"),
            units.join("ghostframe-xdaemon.service.d/50-force-root-get-image.conf"),
            units.join("ghostframe.target"),
            units.join("ghostframe-wm.service"),
            units.join("graphical-session.target.wants/ghostframe-xdaemon.service"),
            sb.paths.autostart_dir.join("ghostframe-attach.desktop"),
        ];
        for p in &legacy {
            sb.touch(p);
        }
        legacy
    }

    #[test]
    fn a_legacy_home_ends_up_migrated_adopted_and_started_in_order() {
        let sb = Sandbox::new();
        let legacy = legacy_layout(&sb);
        // The registration appears only when the old xdaemon is stopped, so
        // adopting before that stop finds nothing and the marker stays out.
        let rec = Recorder {
            write_state_on: Some(("stop ghostframe-xdaemon.service".into(), sb.state.clone())),
            ..Recorder::default()
        };
        assert!(!state::state_dir_seeded(&sb.state));

        let (r, sys, out) = sb.run_install_with(false, false, rec);
        r.unwrap();

        for p in &legacy {
            assert!(!p.exists(), "{} survived", p.display());
        }
        assert!(!sb
            .paths
            .unit_dir
            .join("ghostframe-xdaemon.service.d")
            .exists());
        assert!(sb.paths.unit_dir.join(UNIT_NAME).exists());
        assert!(sb.paths.autostart_dir.join(AUTOSTART_NAME).exists());
        assert!(state::state_dir_seeded(&sb.state), "marker not adopted");
        assert!(
            out.contains("adopted the existing tailnet registration"),
            "{out}"
        );
        // Everything independent of the stop first, then the stops, the
        // deletes' reload, and the start last.
        assert_eq!(
            sys.calls[..4],
            [
                "show-environment",
                "daemon-reload",
                "enable ghostframe-server.service",
                "show -p Environment --value ghostframe-server.service"
            ]
        );
        let n = sys.calls.len();
        assert!(sys.calls[4..n - 2]
            .iter()
            .all(|c| c.starts_with("stop ghostframe")));
        assert!(sys
            .calls
            .contains(&"stop ghostframe-xdaemon.service".to_string()));
        assert_eq!(
            sys.calls[n - 2..],
            ["daemon-reload", "restart ghostframe-server.service"]
        );
    }

    #[test]
    fn an_unregistered_state_is_not_adopted_and_the_service_is_not_started() {
        let sb = Sandbox::new();
        legacy_layout(&sb);
        write_state(&sb.state, false);
        let (r, sys, out) = sb.run_install(false, false);
        r.unwrap();
        assert!(!state::state_dir_seeded(&sb.state));
        assert!(!out.contains("adopted"), "{out}");
        assert!(out.contains("run `ghostframe-server login`"), "{out}");
        assert!(
            out.contains("while the old ghostframe-xdaemon is still up"),
            "{out}"
        );
        assert!(!sys.calls.iter().any(|c| c.starts_with("restart")));
        // And with no state at all.
        std::fs::remove_dir_all(&sb.state).unwrap();
        let (r, sys, _) = sb.run_install(false, false);
        r.unwrap();
        assert!(!sys.calls.iter().any(|c| c.starts_with("restart")));
        assert!(!sb.state.join(state::JOINED_FILE).exists());
    }

    #[test]
    fn the_unit_does_not_restart_on_exit_2_and_carries_no_removed_knobs() {
        let unit = render_unit(MaxResolution::Limit(1920, 1080));
        assert!(unit.contains("\nRestart=on-failure\n"));
        assert!(unit.contains("\nRestartPreventExitStatus=2\n"), "{unit}");
        for gone in [
            "GHOSTFRAME_FORCE_ROOT_GET_IMAGE",
            "GHOSTFRAME_X11_CAPTURE_ONLY",
        ] {
            assert!(!unit.contains(gone), "{gone} in unit");
        }
    }

    #[test]
    fn uninstall_removes_the_install_and_keeps_the_state() {
        let sb = Sandbox::new();
        sb.run_install(false, false).0.unwrap();
        let state = sb
            .root
            .path()
            .join("home/.local/state/ghostframe/tailscaled.state");
        sb.touch(&state);
        let mut sys = Recorder::default();
        uninstall(&sb.paths, &mut sys, &mut Vec::new()).unwrap();
        assert!(!sb.paths.bin.exists());
        assert!(!sb.paths.unit_dir.join(UNIT_NAME).exists());
        assert!(!sb.paths.autostart_dir.join(AUTOSTART_NAME).exists());
        assert!(state.exists());
        assert_eq!(
            sys.calls,
            ["disable --now ghostframe-server.service", "daemon-reload"]
        );
    }

    #[test]
    fn no_user_manager_fails_before_anything_is_touched() {
        let sb = Sandbox::new();
        let legacy = legacy_layout(&sb);
        let rec = Recorder {
            fail_on: vec!["show-environment".into()],
            ..Recorder::default()
        };
        let (r, sys, _) = sb.run_install_with(false, false, rec);
        let e = r.unwrap_err();
        assert!(e.contains("from your desktop session"), "{e}");
        assert_eq!(sys.calls, ["show-environment"]);
        assert!(!sb.paths.bin.exists() && !sb.paths.unit_dir.join(UNIT_NAME).exists());
        for p in &legacy {
            assert!(p.exists());
        }
    }

    #[test]
    fn a_failed_stop_is_fatal_before_any_delete_or_adopt_and_a_rerun_retries() {
        let sb = Sandbox::new();
        let legacy = legacy_layout(&sb);
        write_state(&sb.state, true);
        let rec = Recorder {
            fail_on: vec!["stop ghostframe-xdaemon.service".into()],
            ..Recorder::default()
        };
        let (r, sys, _) = sb.run_install_with(false, false, rec);
        let e = r.unwrap_err();
        assert!(e.contains("re-run"), "{e}");
        for p in &legacy {
            assert!(
                p.exists(),
                "{} deleted despite the failed stop",
                p.display()
            );
        }
        assert!(
            !state::state_dir_seeded(&sb.state),
            "adopted despite failed stop"
        );
        assert!(!sys.calls.iter().any(|c| c.starts_with("restart")));
        // The files are still there, so the re-run stops again and finishes.
        let (r, sys, _) = sb.run_install(false, false);
        r.unwrap();
        assert!(sys
            .calls
            .contains(&"stop ghostframe-xdaemon.service".to_string()));
        assert!(state::state_dir_seeded(&sb.state));
    }

    #[test]
    fn units_are_stopped_by_name_even_when_their_files_are_gone() {
        let sb = Sandbox::new();
        let (r, sys, _) = sb.run_install(false, false);
        r.unwrap();
        assert!(sys
            .calls
            .contains(&"stop ghostframe-xdaemon.service".to_string()));
    }

    #[test]
    fn a_failing_systemctl_after_the_files_says_rerun_is_safe() {
        let sb = Sandbox::new();
        let rec = Recorder {
            fail_on: vec!["restart ghostframe-server.service".into()],
            ..Recorder::default()
        };
        let e = sb.run_install_with(true, false, rec).0.unwrap_err();
        assert!(e.contains("re-running"), "{e}");
        assert!(e.contains("systemctl --user daemon-reload && systemctl --user enable --now ghostframe-server.service"), "{e}");
    }

    #[test]
    fn an_active_headless_session_is_warned_about() {
        let sb = Sandbox::new();
        sb.touch(&sb.paths.unit_dir.join("ghostframe-xorg.service"));
        let rec = Recorder {
            active: vec!["ghostframe-xorg.service".into()],
            ..Recorder::default()
        };
        let (r, _, out) = sb.run_install_with(false, false, rec);
        r.unwrap();
        assert!(out.contains("ends that graphical session"), "{out}");
        let (_, _, out) = sb.run_install(false, false);
        assert!(!out.contains("ends that graphical session"), "{out}");
    }

    #[test]
    fn uninstall_continues_past_a_failed_disable_and_removes_the_wants_link() {
        let sb = Sandbox::new();
        sb.run_install(false, false).0.unwrap();
        let link = sb
            .paths
            .unit_dir
            .join("graphical-session.target.wants")
            .join(UNIT_NAME);
        sb.touch(&link);
        let mut sys = Recorder {
            fail_on: vec!["disable --now ghostframe-server.service".into()],
            ..Recorder::default()
        };
        let mut out = Vec::new();
        uninstall(&sb.paths, &mut sys, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("injected"), "{out}");
        assert!(!link.exists() && !sb.paths.bin.exists());
        assert!(!sb.paths.unit_dir.join(UNIT_NAME).exists());
        assert!(sys.calls.contains(&"daemon-reload".to_string()));
    }

    #[test]
    fn dropin_files_are_listed_and_foreign_ones_get_a_hint() {
        let sb = Sandbox::new();
        let d = sb.paths.unit_dir.join("ghostframe-xdaemon.service.d");
        sb.touch(&d.join("50-force-root-get-image.conf"));
        let (_, _, out) = sb.run_install(false, false);
        assert!(
            out.contains(
                "removing drop-in ghostframe-xdaemon.service.d/50-force-root-get-image.conf"
            ),
            "{out}"
        );
        assert!(!out.contains("systemctl --user edit"), "{out}");
        sb.touch(&d.join("local.conf"));
        let (_, _, out) = sb.run_install(false, false);
        assert!(
            out.contains("systemctl --user edit ghostframe-server"),
            "{out}"
        );
        assert!(out.contains("GHOSTFRAME_ATTACH_MAX_RESOLUTION"), "{out}");
    }

    #[test]
    fn a_directory_named_like_a_legacy_unit_is_refused_not_deleted() {
        let sb = Sandbox::new();
        let dir = sb.paths.unit_dir.join("ghostframe-wm.service");
        sb.touch(&dir.join("precious"));
        let e = sb.run_install(false, false).0.unwrap_err();
        assert!(e.contains("refusing to remove"), "{e}");
        assert!(dir.join("precious").exists());
    }

    #[test]
    fn a_symlinked_wants_dir_is_not_followed() {
        let sb = Sandbox::new();
        let outside = sb.root.path().join("outside.wants");
        sb.touch(&outside.join("ghostframe-xdaemon.service"));
        std::fs::create_dir_all(&sb.paths.unit_dir).unwrap();
        std::os::unix::fs::symlink(&outside, sb.paths.unit_dir.join("evil.wants")).unwrap();
        sb.run_install(false, false).0.unwrap();
        assert!(outside.join("ghostframe-xdaemon.service").exists());
    }

    #[test]
    fn a_stale_staging_file_is_not_written_through() {
        let sb = Sandbox::new();
        let victim = sb.root.path().join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        std::fs::create_dir_all(sb.paths.bin.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&victim, sb.paths.bin.with_extension("new")).unwrap();
        sb.run_install(false, false).0.unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert_eq!(std::fs::read(&sb.paths.bin).unwrap(), b"binary v2");
        assert!(!sb.paths.bin.with_extension("new").exists());
    }

    #[test]
    fn home_must_be_absolute_and_the_passwd_home() {
        assert_eq!(
            validated_home(Some("/srv/alice"), Some("/srv/alice")).unwrap(),
            PathBuf::from("/srv/alice")
        );
        let e = validated_home(Some("srv/alice"), Some("/srv/alice")).unwrap_err();
        assert!(e.contains("not an absolute path"), "{e}");
        let e = validated_home(Some("/tmp/fake"), Some("/srv/alice")).unwrap_err();
        assert!(e.contains("not your passwd home /srv/alice"), "{e}");
        assert!(validated_home(None, Some("/srv/alice")).is_err());
        assert!(validated_home(Some("/srv/alice"), None).is_err());
    }

    #[test]
    fn the_cgroup_parse_finds_the_headless_session_in_v2_and_v1() {
        let v2 =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/ghostframe-wm.service\n";
        assert!(in_legacy_headless_cgroup(v2));
        let xorg = "0::/user.slice/user@1000.service/ghostframe-xorg-vt7.service\n";
        assert!(in_legacy_headless_cgroup(xorg));
        let hybrid = "12:cpu:/user.slice\n1:name=systemd:/user.slice/user@1000.service/ghostframe-wm.service\n0::/user.slice\n";
        assert!(in_legacy_headless_cgroup(hybrid));
        // Not the headless session: a desktop terminal, ssh, the new service,
        // and look-alike names.
        for no in [
            "0::/user.slice/user-1000.slice/session-3.scope\n",
            "0::/user.slice/user@1000.service/app.slice/ghostframe-server.service\n",
            "0::/user.slice/user@1000.service/ghostframe-install.service\n",
            "0::/user.slice/my-ghostframe-wm.service\n",
            "0::/user.slice/ghostframe-xorg.timer\n",
            "",
        ] {
            assert!(!in_legacy_headless_cgroup(no), "{no:?}");
        }
    }

    #[test]
    fn inside_the_headless_session_install_reexecs_as_a_transient_unit_once() {
        let cg = "0::/user.slice/user@1000.service/ghostframe-wm.service\n";
        let exe = Path::new("/proc/self/exe");
        let args = install_args(true, MaxResolution::Limit(1920, 1080));
        let mut sys = Recorder::default();
        let mut out = Vec::new();
        let done = maybe_detach(&mut sys, cg, false, exe, &args, &mut out).unwrap();
        assert!(done);
        assert_eq!(sys.runs.len(), 1);
        let run = &sys.runs[0];
        assert_eq!(
            run[..3],
            ["--user", "--collect", "--unit=ghostframe-install"]
        );
        assert!(run.contains(&"--setenv=GHOSTFRAME_INSTALL_DETACHED=1".to_string()));
        assert!(!run.iter().any(|a| a.contains("XDG_STATE_HOME")));
        assert_eq!(
            run[run.len() - 5..],
            [
                "/proc/self/exe",
                "install",
                "--force",
                "--max-resolution",
                "1920x1080"
            ]
        );
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("journalctl --user -u ghostframe-install -f"),
            "{out}"
        );
        // The re-exec'd copy, and a session that is not the headless one, run in place.
        let mut sys = Recorder::default();
        assert!(!maybe_detach(&mut sys, cg, true, exe, &args, &mut Vec::new()).unwrap());
        assert!(!maybe_detach(
            &mut sys,
            "0::/user.slice/session-1.scope\n",
            false,
            exe,
            &args,
            &mut Vec::new()
        )
        .unwrap());
        assert!(sys.runs.is_empty());
        assert_eq!(
            install_args(false, MaxResolution::Unlimited),
            ["install", "--max-resolution", "none"]
        );
        assert!(
            out.contains("handed off, not that it has finished"),
            "{out}"
        );
    }

    #[test]
    fn install_args_round_trip_through_the_clap_parser() {
        use clap::Parser;
        for force in [false, true] {
            for max in [
                MaxResolution::Unlimited,
                MaxResolution::Limit(1920, 1080),
                MaxResolution::Limit(3840, 2160),
            ] {
                let argv = std::iter::once("ghostframe-server".to_string())
                    .chain(install_args(force, max));
                let cli = crate::cli::Cli::try_parse_from(argv).unwrap();
                assert_eq!(
                    cli.command,
                    crate::cli::Command::Install {
                        force,
                        max_resolution: max
                    }
                );
            }
        }
    }

    #[test]
    fn a_failing_systemd_run_says_nothing_was_changed_or_that_one_is_running() {
        let cg = "0::/user.slice/ghostframe-wm.service\n";
        let run = |err: &str| {
            let mut sys = Recorder {
                run_err: Some(err.into()),
                ..Recorder::default()
            };
            maybe_detach(&mut sys, cg, false, Path::new("/x"), &[], &mut Vec::new()).unwrap_err()
        };
        let e = run("systemd-run failed: Failed to connect to bus");
        assert!(
            e.contains("no unit was written and nothing was stopped"),
            "{e}"
        );
        let e = run("systemd-run failed: Unit ghostframe-install.service already exists.");
        assert!(
            e.contains("an install is already running: journalctl --user -u ghostframe-install -f"),
            "{e}"
        );
    }

    #[test]
    fn the_last_ts_state_dir_wins_and_escapes_inside_quotes_work() {
        let p = ts_state_dir_from_environment;
        // A drop-in's assignment comes later in the list and overrides.
        assert_eq!(
            p("TS_STATE_DIR=/first A=1 TS_STATE_DIR=/second"),
            Some(PathBuf::from("/second"))
        );
        // The last one counts even when it is unusable.
        assert_eq!(p("TS_STATE_DIR=/first TS_STATE_DIR=rel"), None);
        assert_eq!(
            p(r#""TS_STATE_DIR=/a\"b c" X=1"#),
            Some(PathBuf::from("/a\"b c"))
        );
        assert_eq!(p(r#""TS_STATE_DIR=/a\\b""#), Some(PathBuf::from("/a\\b")));
    }

    #[test]
    fn root_is_refused_before_anything() {
        assert!(refuse_root(0)
            .unwrap_err()
            .contains("refusing to install as root"));
        assert!(refuse_root(1000).is_ok());
    }

    #[test]
    fn only_exit_5_of_stop_is_forgiven() {
        assert!(stop_ok(&["stop", "x.service"], Some(5)));
        assert!(!stop_ok(&["stop", "x.service"], Some(1)));
        assert!(!stop_ok(&["stop", "x.service"], None));
        assert!(!stop_ok(&["enable", "x.service"], Some(5)));
    }

    #[test]
    fn ts_state_dir_is_read_from_the_manager_environment() {
        let p = |s: &str| ts_state_dir_from_environment(s);
        assert_eq!(
            p("DISPLAY=:0 TS_STATE_DIR=/srv/a/.local/state/ghostframe GHOSTFRAME_MAX_RESOLUTION=1920x1080"),
            Some(PathBuf::from("/srv/a/.local/state/ghostframe"))
        );
        assert_eq!(
            p("DISPLAY=:0 \"TS_STATE_DIR=/srv/a b/state\" X=1"),
            Some(PathBuf::from("/srv/a b/state"))
        );
        assert_eq!(p("DISPLAY=:0"), None);
        assert_eq!(p("TS_STATE_DIR=relative"), None);
        assert_eq!(p(""), None);
    }

    #[test]
    fn adoption_uses_the_state_dir_the_manager_reports() {
        let sb = Sandbox::new();
        let other = sb.root.path().join("elsewhere/ghostframe");
        let rec = Recorder {
            env_line: Some(format!("DISPLAY=:0 TS_STATE_DIR={}", other.display())),
            write_state_on: Some(("stop ghostframe-xdaemon.service".into(), other.clone())),
            ..Recorder::default()
        };
        let (r, sys, out) = sb.run_install_with(false, false, rec);
        r.unwrap();
        assert!(state::state_dir_seeded(&other), "{out}");
        assert!(!state::state_dir_seeded(&sb.state));
        assert!(sys
            .calls
            .contains(&"restart ghostframe-server.service".to_string()));
        assert!(!out.contains("could not read TS_STATE_DIR"), "{out}");
    }

    #[test]
    fn an_unreadable_manager_environment_falls_back_with_a_warning() {
        let sb = Sandbox::new();
        let (r, _, out) = sb.run_install(false, false);
        r.unwrap();
        assert!(
            out.contains("could not read TS_STATE_DIR from the user manager"),
            "{out}"
        );
    }

    #[test]
    fn a_symlinked_home_matches_its_target_and_the_passwd_spelling_is_returned() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (real_s, link_s) = (real.to_str().unwrap(), link.to_str().unwrap());
        assert_eq!(validated_home(Some(link_s), Some(real_s)).unwrap(), real);
        assert_eq!(validated_home(Some(real_s), Some(link_s)).unwrap(), link);
        let other = root.path().join("other");
        std::fs::create_dir(&other).unwrap();
        assert!(validated_home(Some(other.to_str().unwrap()), Some(real_s)).is_err());
        // Trailing slash, unresolvable paths compared literally.
        assert!(validated_home(Some("/srv/alice/"), Some("/srv/alice")).is_ok());
        assert!(validated_home(Some("/srv/alice"), Some("/srv/alice/")).is_ok());
    }

    #[test]
    fn the_empty_target_wants_dir_goes_but_a_foreign_one_stays() {
        let sb = Sandbox::new();
        let units = &sb.paths.unit_dir;
        sb.touch(&units.join("ghostframe.target.wants/ghostframe-xdaemon.service"));
        sb.touch(&units.join("ghostframe-wm.service.wants/mine.service"));
        sb.run_install(false, false).0.unwrap();
        assert!(!units.join("ghostframe.target.wants").exists());
        assert!(units
            .join("ghostframe-wm.service.wants/mine.service")
            .exists());
    }

    #[test]
    fn a_failure_before_the_old_units_are_stopped_says_to_rerun_not_enable_now() {
        let sb = Sandbox::new();
        let rec = Recorder {
            fail_on: vec!["enable ghostframe-server.service".into()],
            ..Recorder::default()
        };
        let e = sb.run_install_with(false, false, rec).0.unwrap_err();
        assert!(
            e.contains("re-running `ghostframe-server install` is safe"),
            "{e}"
        );
        assert!(!e.contains("enable --now"), "{e}");
    }

    #[test]
    fn login_and_capture_refuse_while_the_old_xdaemon_is_around() {
        let sb = Sandbox::new();
        let home = sb.root.path().join("home");
        assert!(refuse_if_legacy_xdaemon(Some(&home), || false).is_ok());
        assert!(refuse_if_legacy_xdaemon(None, || false).is_ok());
        let e = refuse_if_legacy_xdaemon(Some(&home), || true).unwrap_err();
        assert!(e.contains("installed or running"), "{e}");
        assert!(
            e.contains("systemctl --user stop ghostframe-xdaemon.service"),
            "{e}"
        );
        sb.touch(&sb.paths.unit_dir.join("ghostframe-xdaemon.service"));
        let e =
            refuse_if_legacy_xdaemon(Some(&home), || panic!("file check comes first")).unwrap_err();
        assert!(e.contains("run `ghostframe-server install` first"), "{e}");
    }
}
