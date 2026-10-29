//! Integration tests for ghostframe-server's --init flag and TS_AUTHKEY
//! optionality. These run the binary as a subprocess with empty/seeded state
//! directories and verify the pre-tsnet error paths. A real --init that joins
//! a tsnet control server is exercised by the e2e harness.

use std::path::PathBuf;
use std::process::Command;

fn binary_path() -> PathBuf {
    // `cargo test` sets CARGO_BIN_EXE_<name> to the integration-test binary.
    PathBuf::from(env!("CARGO_BIN_EXE_ghostframe-server"))
}

fn unique_tmp(label: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "ghostframe-server-test-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    // The daemon refuses a state dir that is group- or world-accessible.
    std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    p
}

#[test]
fn errors_when_no_authkey_and_empty_state_dir() {
    let state = unique_tmp("empty-state");
    let out = Command::new(binary_path())
        .env_clear()
        .env("TS_STATE_DIR", &state)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("spawn ghostframe-server");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr was: {stderr}");
    assert!(
        stderr.contains("TS_AUTHKEY not set") && stderr.contains("--init"),
        "stderr should explain --init: {stderr}"
    );
    std::fs::remove_dir_all(&state).ok();
}

#[test]
fn errors_when_init_flag_set_but_no_authkey() {
    let state = unique_tmp("init-no-key");
    let out = Command::new(binary_path())
        .arg("--init")
        .env_clear()
        .env("TS_STATE_DIR", &state)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("spawn ghostframe-server");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr was: {stderr}");
    assert!(
        stderr.contains("--init requires TS_AUTHKEY"),
        "stderr should mention --init + TS_AUTHKEY: {stderr}"
    );
    std::fs::remove_dir_all(&state).ok();
}

#[test]
fn refuses_world_accessible_state_dir() {
    let state = unique_tmp("loose-state");
    std::fs::set_permissions(&state, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let out = Command::new(binary_path())
        .env_clear()
        .env("TS_STATE_DIR", &state)
        .env("TS_AUTHKEY", "tskey-auth-not-used")
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("spawn ghostframe-server");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr was: {stderr}");
    assert!(stderr.contains("chmod 700"), "stderr was: {stderr}");
    std::fs::remove_dir_all(&state).ok();
}
