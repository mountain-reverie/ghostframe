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

    // The old xdaemon shares the tailnet state and takes no lock on it.
    if matches!(cli.command, Command::Capture | Command::Login { .. }) {
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
        } => commands::login::login(&cfg, hostname, control_url),
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
    max_resolution: cli::MaxResolution,
) -> Result<(), String> {
    util::ignore_sighup();
    let (home, pw) = checked_home(cfg)?;
    let paths = commands::install::InstallPaths::new(&home);
    commands::install::refuse_root(util::euid())?;
    let exe = std::env::current_exe().map_err(|e| format!("locating this binary: {e}"))?;
    let username = pw.map(|p| p.name).ok_or("this uid has no passwd entry")?;
    // Inside the old headless session, stopping it would kill this process:
    // continue as a transient unit outside its cgroup, running the installed
    // copy (copied first, so the unit does not depend on this path).
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    if !cfg.install_detached() && commands::install::in_legacy_headless_cgroup(&cgroup) {
        commands::install::copy_binary(&exe, &paths.bin, force)?;
    }
    if commands::install::maybe_detach(
        &mut commands::install::RealSystemctl,
        &cgroup,
        cfg.install_detached(),
        &paths.bin,
        &commands::install::install_args(force, max_resolution),
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

/// `login` and `capture` refuse while the old xdaemon is installed or active.
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
