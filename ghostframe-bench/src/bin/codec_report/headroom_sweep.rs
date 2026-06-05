//! M3.7b headroom sweep: re-runs 4 HEADROOM_MIN_BPUS threshold values
//! across 3 shaping points (1/2/5 Mbps) — the bandwidth range where
//! the threshold matters. At 10+ Mbps the override never fires; below
//! 1 Mbps the link is unusable for either codec.
//!
//! Default scene matches the M3.7a bias_sweep / loss_axis conventions:
//! `--mode-switch-cycle 12` (proven content that produces PixelPerfect
//! reliably; see M3.7a final bench results).

pub struct HeadroomPoint {
    pub label: &'static str,
    pub value_bpus: f32,
}

pub const HEADROOM_VALUES_BPUS: &[HeadroomPoint] = &[
    HeadroomPoint { label: "headroom_0.10", value_bpus: 0.10 },
    HeadroomPoint { label: "headroom_0.25", value_bpus: 0.25 }, // current default
    HeadroomPoint { label: "headroom_0.50", value_bpus: 0.50 },
    HeadroomPoint { label: "headroom_1.00", value_bpus: 1.00 },
];

pub struct HeadroomShapingPoint {
    pub label: &'static str,
    pub bandwidth_kbps: u32,
    pub delay_ms: u32,
    pub loss_pct: u32,
}

pub const HEADROOM_SHAPING_POINTS: &[HeadroomShapingPoint] = &[
    HeadroomShapingPoint { label: "1mbps", bandwidth_kbps: 1_000, delay_ms: 100, loss_pct: 5 },
    HeadroomShapingPoint { label: "2mbps", bandwidth_kbps: 2_000, delay_ms:  80, loss_pct: 3 },
    HeadroomShapingPoint { label: "5mbps", bandwidth_kbps: 5_000, delay_ms:  40, loss_pct: 1 },
];

pub const DEFAULT_SCENE_NAME: &str = "mode_switch_12s";
pub const DEFAULT_TEST_PATTERN_ARGS: &[&str] = &["--mode-switch-cycle", "12"];
