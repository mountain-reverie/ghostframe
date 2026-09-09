//! `Instant` that matches quinn-proto's own conditional type.
//!
//! quinn-proto picks its internal `Instant` with
//! `#[cfg(not(all(target_family = "wasm", target_os = "unknown")))] use
//! std::time::Instant` and `web_time::Instant` on wasm — but that switch is
//! `pub(crate)` inside quinn-proto, so it isn't visible to us. Every
//! quinn-proto method we call that takes or returns an `Instant`
//! (`Endpoint::handle`, `Endpoint::connect`, `Connection::poll_transmit`,
//! `Connection::handle_timeout`, `Connection::poll_timeout`, ...) is
//! monomorphized against whichever type won that `cfg`, so we have to mirror
//! the same switch exactly or the types fail to unify at the call site.
//!
//! `Duration` needs no such mirror: `web_time::Duration` is a bare
//! `pub use std::time::Duration` re-export on every target, wasm included,
//! so `std::time::Duration` always matches.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) use std::time::Instant;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) use web_time::Instant;
