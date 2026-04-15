use std::env;

use ghostframe_lib::{GhostbridgeConfig, IoBridge};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ghostframe=debug,info".into()),
        )
        .init();

    let authkey = env::var("TS_AUTHKEY").expect("TS_AUTHKEY must be set");
    let hostname = env::var("TS_HOSTNAME").unwrap_or_else(|_| "ghostframe-server".into());
    let state_dir = env::var("TS_STATE_DIR").unwrap_or_else(|_| "/tmp/ghostframe-ts".into());
    let control_url = env::var("TS_CONTROL_URL").unwrap_or_default();

    let config = GhostbridgeConfig {
        hostname,
        authkey,
        state_dir,
        control_url,
    };

    tracing::info!("Connecting to Tailscale...");
    let mut bridge = IoBridge::new(&config, ":4443").await?;

    // Machine-parseable line for the E2E test harness. Use println! (stdout)
    // rather than tracing so the format stays stable regardless of log config.
    println!("CERT_HASH_SHA256={}", bridge.cert_hash_sha256());

    tracing::info!("I/O bridge ready, entering event loop");
    bridge.run().await?;
    Ok(())
}
