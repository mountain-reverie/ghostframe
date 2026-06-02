//! Public harness library for ghostframe e2e tests and the Layer B
//! bench binary (`ghostframe-bench`'s `codec_report`).
//!
//! Sub-modules group helpers by concern; everything is re-exported
//! from `ghostframe_e2e::harness` for convenience.
//!
//! See `docs/superpowers/specs/2026-06-01-m3.5-bench-publication-design.md`
//! for the API contract.

pub mod chromium;
pub mod cleanup;
pub mod containers;
pub mod fixtures;
pub mod scene;
pub mod transport;
pub mod weston;

// Flat re-exports so callers can `use ghostframe_e2e::harness::*;` and
// get the legacy `tests/e2e/helpers.rs` flat API.
pub use chromium::{assert_ssim_against_golden, screenshot_canvas};
pub use cleanup::{cleanup_stale_xvfb_sockets, read_server_logs_stripped};
pub use containers::{
    create_preauth_key, docker_run_in_container, read_cert_hash_from_logs, TestNode, NETWORK_NAME,
};
pub use scene::{
    run_scene, ClientDiagnosticRecord, CodecHistogram, FrameMode, ProcSample, SceneResult,
    SceneSpec, ServerTelemetryRecord,
};
pub use transport::{start_forwarder, start_static_server};
pub use weston::{parse_weston_xwayland_display, spawn_weston_headless, WestonGuard};
