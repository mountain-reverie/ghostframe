//! Shared polling helper for the native-client e2e acceptance tests
//! (`tests/native_client.rs`, `tests/h264.rs`, `tests/eviction.rs`). Lives
//! in `tests/common/` rather than being copy-pasted per test file -- every
//! test that uses it needs it byte-identical, and a fix in one copy
//! silently not reaching the others is exactly the kind of drift this
//! crate's harness/ exists to avoid (see `framebuffer.rs` and
//! `browserless.rs` for the same move, made for the same reason).
//!
//! `ghostframe-client-native` is a **dev-dependency** of `ghostframe-e2e`
//! (`Cargo.toml`), deliberately kept that way: an earlier attempt to
//! promote it to a normal dependency pulled `client-gpu` into the
//! server image's build graph, where `shaders/` are not copied, and broke
//! CI. It was reverted. This file lives under `tests/` specifically
//! *because* the dependency stayed dev-only -- a `src/` module here could
//! not use it.

use std::time::{Duration, Instant};

use ghostframe_client_native::{Client, ClientEvent, PublishedFrame};

/// Pump events until a frame is published, or `timeout` elapses.
///
/// `Client` exposes no blocking frame call, and an `acquire_frame` poll
/// that never drains `next_event` would swallow the very error events
/// that explain a stall -- so this drains events first on every iteration,
/// and panics eagerly on `ClientEvent::Error` rather than waiting out the
/// full timeout, since that event means the library has already given up.
pub fn wait_for_frame(client: &mut Client, timeout: Duration) -> Option<PublishedFrame> {
    let deadline = Instant::now() + timeout;
    loop {
        while let Some(ev) = client.next_event() {
            tracing::info!(?ev, "client event");
            if let ClientEvent::Error { message } = &ev {
                panic!("client reported an error while waiting for a frame: {message}");
            }
        }
        if let Some(frame) = client.acquire_frame() {
            return Some(frame);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
