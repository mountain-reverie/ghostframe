//! M3.7a loss-axis bench: re-runs one scene at 6 inbound-loss rates
//! by setting GHOSTFRAME_INBOUND_LOSS_PROBABILITY before each run.
//! Default scene is the existing mode_switch alternating cycle at a
//! comfortable bandwidth point — the classifier has clear flips to
//! make, and the loss axis varies how often loss_override fires vs
//! cost_comparison.

pub struct LossPoint {
    pub label: &'static str,
    pub probability: f32,
}

pub const LOSS_VALUES: &[LossPoint] = &[
    LossPoint {
        label: "loss_2pct",
        probability: 0.02,
    },
    LossPoint {
        label: "loss_5pct",
        probability: 0.05,
    },
    LossPoint {
        label: "loss_8pct",
        probability: 0.08,
    },
    LossPoint {
        label: "loss_10pct",
        probability: 0.10,
    }, // current threshold
    LossPoint {
        label: "loss_12pct",
        probability: 0.12,
    },
    LossPoint {
        label: "loss_15pct",
        probability: 0.15,
    },
];

/// 12-second halves match the bias_sweep default for consistency. The
/// loss axis doesn't strictly need refinement convergence to fire (the
/// loss_override path triggers independently), but longer scenes give
/// more opportunities for the smoothed loss_rate to cross the threshold
/// across different content phases.
pub const DEFAULT_SCENE_NAME: &str = "mode_switch_12s";
pub const DEFAULT_TEST_PATTERN_ARGS: &[&str] = &["--mode-switch-cycle", "12"];
pub const DEFAULT_BANDWIDTH_BYTES_PER_SEC: u64 = 3_750_000; // 30 mbps_cable
