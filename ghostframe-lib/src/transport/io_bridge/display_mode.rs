//! Display-mode negotiation: debouncing and applying a client's `DisplayMode`.
//!
//! Owns the `DisplayMode` clamp/align policy (`clamp_and_align_mode` and its
//! width granularity and floors), the `PendingDisplayMode` request that
//! waits out the debounce, and `IoBridge::on_timeout`, which applies it once
//! due. The `pending_display_mode` field itself stays on `IoBridge`.
//!
//! Must not: touch the session table, the pacing/BWE state or the wire
//! format; it only decides when and to what size the output is changed.

use super::*;

/// How long to wait after the last `DisplayMode` before applying it.
///
/// **A guess, not a measurement.** Roughly the pause a person makes on
/// releasing a window edge, and far longer than the 2.6-4.9 ms a resolution
/// change costs (M3 spec §10.7), so being wrong means latency on the last
/// resize rather than thrash. If it proves wrong, measure a real resize
/// drag rather than guessing again.
pub(super) const DISPLAY_MODE_DEBOUNCE_US: u64 = 250_000;

/// CVT rounds h_active UP to a multiple of 8 (verified: `cvt 1283 817 60 -r`
/// yields a 1288-wide mode). Align requested widths DOWN to this instead:
/// a mode wider than the client's window crops the remote desktop
/// off-screen, while a narrower one leaves a harmless border.
const MODE_WIDTH_GRANULARITY: u16 = 8;

/// Floor so that aligning down, or a client dragging a window to nothing,
/// cannot ask X for a degenerate mode.
pub(super) const MIN_MODE_WIDTH: u16 = 320;

pub(super) const MIN_MODE_HEIGHT: u16 = 240;

/// A `DisplayMode` request awaiting the debounce deadline. Stores the
/// already-clamped/aligned/floored size (see `clamp_and_align_mode`) so
/// `on_timeout` only has to compare a deadline and call `set_output` --
/// the decision of *what* to apply was made at dispatch time, `when` is
/// decided here.
pub(super) struct PendingDisplayMode {
    pub(super) width: u16,
    pub(super) height: u16,
    /// Microseconds since `IoBridge::epoch` (same clock as the classifier's
    /// `now_us`, i.e. `now_std().duration_since(self.epoch)`, the same
    /// epoch-relative clock the classifier's dwell timers use) at which this
    /// becomes due.
    pub(super) deadline_us: u64,
}

/// Clamp a requested mode to the controller's ceiling, then align the width
/// down to `MODE_WIDTH_GRANULARITY`, then apply the floors. Order matters:
/// aligning down AFTER clamping guarantees the result never exceeds the
/// ceiling even when the ceiling itself isn't a multiple of the granularity;
/// aligning first and clamping second would not have that guarantee. Height
/// is only clamped and floored -- CVT's granularity constraint is
/// horizontal-only.
pub(super) fn clamp_and_align_mode(
    requested_w: u16,
    requested_h: u16,
    ceiling: (u16, u16),
) -> (u16, u16) {
    let w = requested_w.min(ceiling.0);
    let h = requested_h.min(ceiling.1);
    let w = (w / MODE_WIDTH_GRANULARITY) * MODE_WIDTH_GRANULARITY;
    (w.max(MIN_MODE_WIDTH), h.max(MIN_MODE_HEIGHT))
}

impl IoBridge {
    pub(crate) fn on_timeout(&mut self, now_us: u64) {
        let Some(pending) = self.pending_display_mode.as_ref() else {
            return;
        };
        // Diagnostic for the open M4b defect: the e2e shows a pending mode
        // stored and never applied, with no log from either arm below. This
        // line distinguishes "deadline never reached" from "loop never woke"
        // -- run the server with
        // `RUST_LOG=ghostframe_lib::transport::io_bridge=trace` to see it.
        // Left in (at trace) rather than deleted because reproducing the
        // failure needs a container rebuild, which is expensive.
        tracing::trace!(
            target: LOG_TARGET,
            now_us,
            deadline_us = pending.deadline_us,
            due = now_us >= pending.deadline_us,
            has_controller = self.display_controller.is_some(),
            "m4b-diag: on_timeout saw a pending display mode"
        );
        if now_us < pending.deadline_us {
            return;
        }
        let PendingDisplayMode { width, height, .. } = self
            .pending_display_mode
            .take()
            .expect("checked Some above");
        if let Some(ctl) = self.display_controller.as_ref() {
            match ctl.set_output(width, height, self.display_scale_milli) {
                Ok(()) => {
                    tracing::info!(
                        target: LOG_TARGET,
                        width,
                        height,
                        scale_milli = self.display_scale_milli,
                        "display mode applied"
                    );
                }
                Err(e) => {
                    tracing::warn!(target: LOG_TARGET, width, height, error = %e, "display mode apply failed");
                }
            }
        }
    }
}
