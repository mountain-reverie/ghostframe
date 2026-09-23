use clap::Parser;

use ghostframe_cli::cli::{Cli, Command};
use ghostframe_cli::commands::{self, CommandError};

fn main() -> std::process::ExitCode {
    // A CLI that answers "you are not logged in" with a backtrace is
    // user-hostile, and this is the first thing a person touches.
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ghostframe: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), CommandError> {
    // `RUST_LOG` drives this if set; normal user-facing output goes
    // through plain println!/eprintln! below, not through tracing, so
    // nobody needs RUST_LOG set just to see an auth URL.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Command::Login {
            authkey,
            login_server,
            hostname,
        } => commands::login(authkey, login_server, hostname),
        Command::Logout => commands::logout(),
        Command::Connect {
            host,
            port,
            chord_prefix,
        } => commands::connect(host, port, chord_prefix),
    }
}
