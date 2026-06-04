//! M3.7a bias-sweep bench: re-runs one scene at 4 values of
//! REFINEMENT_BIAS_PER_TILE_US by setting GHOSTFRAME_TEST_REFINEMENT_BIAS_US
//! before each run and clearing it after. Default scene is text content
//! under subtle drift, at a moderate bandwidth point so the classifier
//! has refinement work to manage.

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

/// Default scene + content for the sweep. Test-pattern args set the
/// subtle-drift mode at 50 ms (drift every 50 ms = 20 dirty events/sec
/// on a 1-px shift, plenty of refinement pressure without overwhelming).
pub const DEFAULT_SCENE_NAME: &str = "text";
pub const DEFAULT_TEST_PATTERN_ARGS: &[&str] = &[
    "--drm-direct",
    "--tile-pattern", "text",
    "--subtle-drift", "50",
];
pub const DEFAULT_BANDWIDTH_BYTES_PER_SEC: u64 = 1_250_000; // 10 mbps_dsl
pub const DEFAULT_LOSS_PROBABILITY: f32 = 0.01; // 1% — mild realism
