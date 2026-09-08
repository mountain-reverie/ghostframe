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
///
/// All fields carry test-only tuning knobs. A production-relevant tunable
/// would need wiring in `Classifier::new` and other codepaths that does
/// not yet exist.
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

impl ClassifierConfig {
    /// Parse from an arbitrary lookup. `from_env` is this with a real
    /// environment lookup; tests use a map so they never touch process-global
    /// state. Keeping the parsing here means the crate reads the process
    /// environment in exactly one place.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            refinement_bias_us: get("GHOSTFRAME_TEST_REFINEMENT_BIAS_US")
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|v| *v > 0.0),
            loss_override_threshold: get("GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD")
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|v| *v > 0.0 && *v <= 1.0),
            headroom_min_bpus: get("GHOSTFRAME_TEST_HEADROOM_MIN_BPUS")
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|v| *v > 0.0),
            // `GHOSTFRAME_FORCE_TILECODEC=1`/`true` is a high-level alias
            // for `GHOSTFRAME_TEST_FORCE_FRAME_MODE=tile`. Used by the
            // `e2e_lossless_golden_png` test to bypass the H.264 forced-
            // start at session entry so the cdf53 first-paint burst gets
            // exercised (H.264 has its own FEC + parity + NACK and renders
            // fully even under wire loss, masking tile-codec regressions).
            force_frame_mode: get("GHOSTFRAME_FORCE_TILECODEC")
                .filter(|s| s == "1" || s == "true")
                .map(|_| FrameMode::TileCodec)
                .or_else(|| {
                    get("GHOSTFRAME_TEST_FORCE_FRAME_MODE").and_then(|s| match s.as_str() {
                        "h264" | "H264" => Some(FrameMode::H264),
                        "tile" | "TileCodec" | "tilecodec" => Some(FrameMode::TileCodec),
                        _ => None,
                    })
                }),
        }
    }

    /// Parse environment variables into a classifier configuration.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Production builds without `test-loss-injection` ignore the environment
    /// entirely, exactly as today: the reads are not compiled.
    #[cfg(not(any(test, feature = "test-loss-injection")))]
    pub fn from_env() -> Self {
        Self::default()
    }
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
    pub cdf53_diff_tile: Option<(u32, u32)>,
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

    /// Build a lookup closure over a small fixture, the way `from_env` builds
    /// one over the real environment. Tests use this instead of mutating
    /// process env, so they need no shared-environment-lock guard and cannot
    /// race other tests' `std::env::set_var` calls.
    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn classifier_config_parses_force_frame_mode() {
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[(
                "GHOSTFRAME_TEST_FORCE_FRAME_MODE",
                "h264"
            )]))
            .force_frame_mode,
            Some(FrameMode::H264)
        );
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[(
                "GHOSTFRAME_TEST_FORCE_FRAME_MODE",
                "tile"
            )]))
            .force_frame_mode,
            Some(FrameMode::TileCodec)
        );
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[])).force_frame_mode,
            None
        );
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[(
                "GHOSTFRAME_TEST_FORCE_FRAME_MODE",
                "bogus"
            )]))
            .force_frame_mode,
            None,
            "an unrecognised value must be ignored, not guessed at"
        );
    }

    #[test]
    fn force_tilecodec_is_an_alias_for_tile_mode() {
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[("GHOSTFRAME_FORCE_TILECODEC", "1")]))
                .force_frame_mode,
            Some(FrameMode::TileCodec)
        );
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[("GHOSTFRAME_FORCE_TILECODEC", "true")]))
                .force_frame_mode,
            Some(FrameMode::TileCodec)
        );
        // Any other value is ignored, not an error.
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[("GHOSTFRAME_FORCE_TILECODEC", "0")]))
                .force_frame_mode,
            None
        );
    }

    #[test]
    fn classifier_config_filters_out_of_range_values() {
        // loss_override_threshold accepts (0.0, 1.0]; bias and headroom accept > 0.0
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[(
                "GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD",
                "1.5"
            )]))
            .loss_override_threshold,
            None
        );
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[(
                "GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD",
                "0.5"
            )]))
            .loss_override_threshold,
            Some(0.5)
        );
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[("GHOSTFRAME_TEST_HEADROOM_MIN_BPUS", "-1")]))
                .headroom_min_bpus,
            None
        );
    }

    /// `from_env` is a thin wrapper around `from_lookup` over the real
    /// environment; this is its only coverage, so it is the one remaining
    /// test in this module that touches process-global state.
    #[test]
    fn from_env_reads_the_real_environment() {
        let _env = crate::test_env::lock_env();
        std::env::set_var("GHOSTFRAME_TEST_FORCE_FRAME_MODE", "h264");
        assert_eq!(
            ClassifierConfig::from_env().force_frame_mode,
            Some(FrameMode::H264)
        );
        std::env::remove_var("GHOSTFRAME_TEST_FORCE_FRAME_MODE");
    }
}
