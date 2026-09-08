//! Library configuration, parsed once at the executable boundary.
//!
//! `ghostframe-lib` must never read `std::env` outside this file. Environment
//! variables are process-global while `cargo test` runs tests as threads in one
//! process, so call-time reads made tests race each other (~2 failures per 6
//! full-suite runs before this refactor). Threading configuration through
//! constructors makes that class of bug structurally impossible.
//!
//! Variable names and parsing are frozen: the e2e suite drives the
//! containerised daemon through these exact names.

use crate::tile::FrameMode;

/// Knobs the frame-mode classifier reads. All `None`/default means production
/// behaviour.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClassifierConfig {
    /// Pins the frame mode. Fed by `GHOSTFRAME_TEST_FORCE_FRAME_MODE`, or by
    /// `GHOSTFRAME_FORCE_TILECODEC=1|true` as a high-level alias for
    /// `TileCodec`.
    pub force_frame_mode: Option<FrameMode>,
    pub refinement_bias_us: Option<f32>,
    pub headroom_min_bpus: Option<f32>,
    pub loss_override_threshold: Option<f32>,
}

/// Transport-layer knobs: fault injection, pacing overrides, FEC.
#[derive(Debug, Clone, Default)]
pub struct TransportConfig {
    // `loss_injection` is itself a `#[cfg(any(test, feature =
    // "test-loss-injection"))]` module (`transport/mod.rs:17`), so these two
    // fields carry the same gate — the type does not exist otherwise. This
    // mirrors `IoBridge`'s own fields at `io_bridge.rs:388,397`. Unit tests are
    // unaffected: `any(test, ..)` means the fields exist under `cargo test`.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub outbound_loss: Option<crate::transport::loss_injection::LossInjector>,
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub inbound_loss: Option<crate::transport::loss_injection::LossInjector>,
    /// Rate in bytes/sec. Deliberately a plain `u64` rather than a
    /// `BandwidthCap` (also a gated module): the config carries data, and
    /// `IoBridge` constructs the gated type from it. That keeps this field
    /// ungated.
    pub outbound_bandwidth_cap_bps: Option<u64>,
    /// `(frame_seq, tile_index)` at which to inject an out-of-range PalRLE
    /// index, from `GHOSTFRAME_INJECT_OOB_PALRLE`.
    pub oob_inject_at: Option<(u32, u32)>,
    pub skip_palette_session_reset: bool,
    pub test_force_bytes_per_us: Option<f32>,
    pub fec_k: Option<usize>,
}

/// Diagnostic logging and one-shot dumps.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsConfig {
    pub diagnose_tiles: bool,
    pub diagnose_gpu_pipeline: bool,
    pub diagnose_color_hist: bool,
    /// Path for the one-shot raw-BGRA frame dump. Consumed once, then cleared
    /// by the bridge — this replaces the current read-then-`remove_var`.
    pub dump_frame_path: Option<String>,
    pub cdf53_diff_tile: Option<(u8, u8)>,
    pub cdf53_dump_pending: bool,
    // Deliberately no `cdf53_skip_l2_l3` / `cdf53_skip_l3`: those two reads
    // live in the Vulkan dispatch path and are deferred. Adding unused fields
    // now would imply a wiring that does not exist.
}

#[derive(Debug, Clone, Default)]
pub struct LibConfig {
    pub classifier: ClassifierConfig,
    pub transport: TransportConfig,
    pub diagnostics: DiagnosticsConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_production_inert() {
        let cfg = LibConfig::default();
        assert!(cfg.classifier.force_frame_mode.is_none());
        assert!(cfg.classifier.refinement_bias_us.is_none());
        assert!(cfg.classifier.headroom_min_bpus.is_none());
        assert!(cfg.classifier.loss_override_threshold.is_none());
        assert!(cfg.transport.outbound_loss.is_none());
        assert!(cfg.transport.inbound_loss.is_none());
        assert!(cfg.transport.outbound_bandwidth_cap_bps.is_none());
        assert!(cfg.transport.oob_inject_at.is_none());
        assert!(!cfg.transport.skip_palette_session_reset);
        assert!(cfg.transport.test_force_bytes_per_us.is_none());
        assert!(cfg.transport.fec_k.is_none());
        assert!(!cfg.diagnostics.diagnose_tiles);
        assert!(!cfg.diagnostics.diagnose_gpu_pipeline);
        assert!(!cfg.diagnostics.diagnose_color_hist);
        assert!(cfg.diagnostics.dump_frame_path.is_none());
    }
}
