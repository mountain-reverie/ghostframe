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

/// Default scene + content. Uses `--solid-red --spinner` — a large
/// static red background plus a small rotating spinner, same pattern
/// as e2e_h264_motion. Borderline by design: mostly-static content
/// with localized motion is exactly the regime where the bias term
/// matters (the policy's cost comparison is close to the deadband,
/// and bias decides whether the spinner's dirty events flip to H264
/// or stay in TileCodec). Clear-cut content (full motion or fully
/// static) wouldn't differentiate bias values.
///
/// The `--subtle-drift` mode added in Task 5 stays in the test-pattern
/// binary as ad-hoc tooling; this bench uses `--solid-red --spinner`
/// because (a) it's a proven working pattern on the WebGPU client and
/// (b) it produces the borderline content bias tuning needs.
pub const DEFAULT_SCENE_NAME: &str = "spinner";
pub const DEFAULT_TEST_PATTERN_ARGS: &[&str] = &[
    "--solid-red",
    "--spinner",
];
pub const DEFAULT_BANDWIDTH_BYTES_PER_SEC: u64 = 1_250_000; // 10 mbps_dsl
pub const DEFAULT_LOSS_PROBABILITY: f32 = 0.01; // 1% — mild realism
