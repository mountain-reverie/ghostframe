//! M3.7a bias-sweep bench: re-runs one scene at 4 values of
//! REFINEMENT_BIAS_PER_TILE_US by setting GHOSTFRAME_TEST_REFINEMENT_BIAS_US
//! before each run and clearing it after. Default scene is the proven
//! mode_switch alternating cycle — refinement converges during static
//! halves while motion halves test bias resistance to mode flips.

pub struct BiasPoint {
    pub label: &'static str,
    pub value_us: f32,
}

pub const BIAS_VALUES_US: &[BiasPoint] = &[
    BiasPoint { label: "bias_2us",  value_us:  2.0 },
    BiasPoint { label: "bias_5us",  value_us:  5.0 },  // current default
    BiasPoint { label: "bias_10us", value_us: 10.0 },
    BiasPoint { label: "bias_20us", value_us: 20.0 },
];

/// Default scene + content. Uses `--mode-switch-cycle 12` — same pattern
/// as e2e_progressive_refinement, the only known scene config that
/// reliably observes PixelPerfect transitions (720+ per 30 s scene in
/// the canonical test). 12-second halves give refinement room to complete
/// all 14 passes (~5-7 s at 30 fps with `refinement_bandwidth_fraction
/// = 0.2`) WITHIN a static half, so the PixelPerfect outcome metric can
/// actually fire. The 4-second cycle tried earlier (cycle = 4) ran the
/// motion half before refinement converged, producing zero PixelPerfect.
///
/// Bench scene_duration should be ≥ 4× cycle (≥ 48 s) so multiple
/// refinement-complete events accumulate per swept value.
///
/// The `--subtle-drift` mode added in Task 5 stays in the test-pattern
/// binary as ad-hoc tooling; this bench uses mode_switch_cycle because
/// it's the only proven content that reaches PixelPerfect under bench.
pub const DEFAULT_SCENE_NAME: &str = "mode_switch_12s";
pub const DEFAULT_TEST_PATTERN_ARGS: &[&str] = &[
    "--mode-switch-cycle", "12",
];
pub const DEFAULT_BANDWIDTH_BYTES_PER_SEC: u64 = 1_250_000; // 10 mbps_dsl
pub const DEFAULT_LOSS_PROBABILITY: f32 = 0.01; // 1% — mild realism
