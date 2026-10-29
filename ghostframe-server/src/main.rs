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

fn install_paths(cfg: &config::ServerConfig) -> Result<commands::install::InstallPaths, String> {
    let home = cfg
        .home()
        .ok_or("HOME is not set; install writes under the user's home directory")?;
    Ok(commands::install::InstallPaths::new(std::path::Path::new(
        home,
    )))
}

fn install(
    cfg: &config::ServerConfig,
    force: bool,
    max_resolution: cli::MaxResolution,
) -> Result<(), String> {
    let paths = install_paths(cfg)?;
    let exe = std::env::current_exe().map_err(|e| format!("locating this binary: {e}"))?;
    let username = util::current_username().ok_or("this uid has no passwd entry")?;
    let state_dir = cfg.state_dir().ok();
    let ctx = commands::install::InstallContext {
        paths: &paths,
        current_exe: &exe,
        sysroot: std::path::Path::new("/"),
        state_dir: state_dir.as_deref(),
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
        &install_paths(cfg)?,
        &mut commands::install::RealSystemctl,
        &mut std::io::stdout(),
    )
}
