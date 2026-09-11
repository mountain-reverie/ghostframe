//! wasm-bindgen boundary for `ghostframe-client-core`.
//!
//! Two layers live here:
//!
//! * [`core::WasmClientCore`] — the real session boundary. `main.ts` drives
//!   this at cutover: datagrams in, events out, bytes back to the wire.
//! * [`units`] — thin per-unit shims. These exist so the *existing* vitest
//!   suites can be retargeted at Rust without being rewritten as integration
//!   tests; the suites encode the old implementation's real behaviour, which
//!   is what makes them able to detect divergence.
//!
//! A Rust panic in wasm aborts the module: every later call traps, so one
//! malformed datagram would end the session rather than drop a frame. No
//! export here may `unwrap` on wire-derived data.

use wasm_bindgen::prelude::*;

pub mod boundary;
pub mod core;
pub mod input;
pub mod units;

/// Hash of the `client-core` + `protocol` sources this module was built
/// from. See `build.rs`; asserted against the workspace by
/// `tests/wasm_smoke.test.ts` so a stale `dist/` fails instead of silently
/// serving yesterday's protocol.
pub const PROTOCOL_STAMP: &str = env!("GHOSTFRAME_PROTOCOL_STAMP");

/// Installs the panic hook. Idempotent; call once at module load.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

#[wasm_bindgen]
pub fn protocol_stamp() -> String {
    PROTOCOL_STAMP.to_string()
}
