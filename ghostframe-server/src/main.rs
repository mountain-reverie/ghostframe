//! `ghostframe-server`: command dispatch only. Each command lives in
//! `commands/`.

mod capture;
mod cli;
mod commands;
mod config;
mod state;
mod util;

use clap::Parser;

use cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();
    let cfg = config::ServerConfig::from_env();
    // Default to INFO, not DEBUG. `ghostframe=debug` as a *default* means a
    // deployed daemon writes every DEBUG line forever: measured on a live
    // session at ~207 lines/second on a screen with nothing to send, six per
    // captured frame at ~29 fps (two INFO dispatch lines, two bench lines,
    // plus DEBUG scheduler.tick and dirty-tile-detection). That is formatting
    // and a journald write per line, and it is also what makes journald's
    // RateLimitBurst reachable -- a first-frame burst on top of this steady
    // rate blew the 10000/30s budget and silently discarded 23232 messages,
    // leaving a 20-second hole in the journal.
    //
    // DEBUG stays one `RUST_LOG=ghostframe=debug` away for anyone who wants
    // it; e2e sets `ghostframe=trace,debug` explicitly and is unaffected.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "ghostframe=info".into());

    if cfg.log_json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    if let Err(e) = refuse_root_for(&cli.command, util::euid()) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    // The old xdaemon shares the tailnet state and takes no lock on it.
    if matches!(
        cli.command,
        Command::Capture | Command::Login { .. } | Command::Logout { .. }
    ) {
        if let Err(e) = refuse_if_legacy_xdaemon_installed(&cfg) {
            eprintln!("error: {e}");
            // 2: the unit does not restart-loop on it.
            std::process::exit(if matches!(cli.command, Command::Capture) {
                2
            } else {
                1
            });
        }
    }

    let result = match cli.command {
        Command::Capture => return commands::capture::run(cfg).await,
        Command::Login {
            hostname,
            control_url,
        } => {
            warn_if_login_state_dir_differs(&cfg);
            commands::login::login(&cfg, hostname, control_url)
        }
        Command::Logout {
            hostname,
            control_url,
        } => commands::login::logout(&cfg, hostname, control_url),
        Command::Install {
            force,
            max_resolution,
        } => install(&cfg, force, max_resolution),
        Command::Uninstall => uninstall(&cfg),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    Ok(())
}

/// The validated home, and the passwd entry it was checked against.
fn checked_home(
    cfg: &config::ServerConfig,
) -> Result<(std::path::PathBuf, Option<util::Passwd>), String> {
    let pw = util::current_passwd();
    let home = commands::install::validated_home(cfg.home(), pw.as_ref().map(|p| p.home.as_str()))?;
    Ok((home, pw))
}

fn install(
    cfg: &config::ServerConfig,
    force: bool,
    max_resolution: Option<cli::MaxResolution>,
) -> Result<(), String> {
    util::ignore_sighup();
    let (home, pw) = checked_home(cfg)?;
    let paths = commands::install::InstallPaths::new(&home);
    commands::install::refuse_root(util::euid(), "install")?;
    let exe = std::env::current_exe().map_err(|e| format!("locating this binary: {e}"))?;
    let username = pw.map(|p| p.name).ok_or("this uid has no passwd entry")?;
    // Inside the old headless session, stopping it would kill this process:
    // continue as a transient unit outside its cgroup, running the installed
    // copy (copied first, so the unit does not depend on this path).
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    if !cfg.install_detached() && commands::install::in_legacy_headless_cgroup(&cgroup) {
        commands::install::copy_binary(&exe, &paths.bin, force)?;
    }
    // The detached copy's output goes to a journal nobody is watching: decide
    // the cap here, print it where the terminal is, and pass it on explicitly
    // so the copy writes what was printed.
    let detaching =
        !cfg.install_detached() && commands::install::in_legacy_headless_cgroup(&cgroup);
    let (args, cap_line) = if detaching {
        let (args, line) = commands::install::detach_install_args(
            force,
            max_resolution,
            &paths.unit_dir.join(commands::install::UNIT_NAME),
        );
        (args, Some(line))
    } else {
        (commands::install::install_args(force, max_resolution), None)
    };
    if let Some(line) = cap_line {
        println!("{line}");
    }
    if commands::install::maybe_detach(
        &mut commands::install::RealSystemctl,
        &cgroup,
        cfg.install_detached(),
        &paths.bin,
        &args,
        &mut std::io::stdout(),
    )? {
        return Ok(());
    }
    let state_dir = cfg.unit_state_dir(&home)?;
    if let Some(ts) = cfg.ts_state_dir() {
        if std::path::Path::new(ts) != state_dir {
            eprintln!(
                "warning: TS_STATE_DIR={ts} in this shell is ignored: the installed service \
                 uses {}",
                state_dir.display()
            );
        }
    }
    let ctx = commands::install::InstallContext {
        paths: &paths,
        current_exe: &exe,
        sysroot: std::path::Path::new("/"),
        state_dir: Some(&state_dir),
        username: &username,
        euid: util::euid(),
        shell_wayland: cfg.wayland_display.as_deref(),
    };
    commands::install::install(
        &ctx,
        force,
        max_resolution,
        &mut commands::install::RealSystemctl,
        &mut std::io::stdout(),
    )
}

fn uninstall(cfg: &config::ServerConfig) -> Result<(), String> {
    commands::install::uninstall(
        &commands::install::InstallPaths::new(&checked_home(cfg)?.0),
        &mut commands::install::RealSystemctl,
        &mut std::io::stdout(),
    )
}

/// `login`, `logout` and `uninstall` act on one user's files or tailnet
/// state, so they refuse root like `install` does (which checks for itself,
/// after its own prerequisites). `capture` stays allowed: the e2e container
/// runs it as root.
fn refuse_root_for(command: &Command, euid: u32) -> Result<(), String> {
    let name = match command {
        Command::Login { .. } => "login",
        Command::Logout { .. } => "logout",
        Command::Uninstall => "uninstall",
        Command::Install { .. } | Command::Capture => return Ok(()),
    };
    commands::install::refuse_root(euid, name)
}

/// `login` writes the state the *service* must read: warn when this shell
/// would put it somewhere else. With the unit installed (and a user manager
/// to ask) the comparison is against the manager's own view, as `install`
/// reads it; otherwise against the directory the unit would use, which
/// differs from this shell's only through `TS_STATE_DIR`.
fn warn_if_login_state_dir_differs(cfg: &config::ServerConfig) {
    let (Some(home), Ok(login_dir)) = (cfg.home(), cfg.state_dir()) else {
        return;
    };
    let home = std::path::Path::new(home);
    let installed = commands::install::InstallPaths::new(home)
        .unit_dir
        .join(commands::install::UNIT_NAME)
        .exists();
    let from_manager = if installed && cfg.user_manager_possible() {
        commands::install::manager_unit_state_dir(&mut commands::install::RealSystemctl)
    } else {
        None
    };
    let (unit_dir, from_manager) = match from_manager {
        Some(d) => (d, true),
        None => match cfg.unit_state_dir(home) {
            Ok(d) => (d, false),
            Err(_) => return,
        },
    };
    if let Some(w) =
        commands::install::state_dir_mismatch_warning(&login_dir, &unit_dir, from_manager)
    {
        eprintln!("{w}");
    }
}

/// `login`, `logout` and `capture` refuse while the old xdaemon is installed or active.
fn refuse_if_legacy_xdaemon_installed(cfg: &config::ServerConfig) -> Result<(), String> {
    let home = cfg.home().map(std::path::Path::new);
    commands::install::refuse_if_legacy_xdaemon(home, || {
        // Only ask when a user manager is plausibly there (XDG_RUNTIME_DIR).
        cfg.user_manager_possible()
            && commands::install::Systemctl::is_active(
                &mut commands::install::RealSystemctl,
                "ghostframe-xdaemon.service",
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_refused_by_login_logout_and_uninstall_but_not_capture() {
        let login = Command::Login {
            hostname: None,
            control_url: None,
        };
        let logout = Command::Logout {
            hostname: None,
            control_url: None,
        };
        for (cmd, name) in [
            (login, "login"),
            (logout, "logout"),
            (Command::Uninstall, "uninstall"),
        ] {
            let e = refuse_root_for(&cmd, 0).unwrap_err();
            assert!(e.contains(&format!("refusing to {name} as root")), "{e}");
            assert!(refuse_root_for(&cmd, 1000).is_ok());
        }
        // The e2e container runs capture as root.
        assert!(refuse_root_for(&Command::Capture, 0).is_ok());
    }
}
