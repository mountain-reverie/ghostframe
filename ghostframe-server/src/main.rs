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
        Command::Logout => commands::login::logout(&cfg),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    Ok(())
}
