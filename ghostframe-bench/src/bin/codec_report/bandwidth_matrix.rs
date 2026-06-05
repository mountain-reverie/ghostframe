//! M3.6c bandwidth-axis bench: re-runs each scene at 4 bandwidth caps
//! by setting `GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP` and
//! `GHOSTFRAME_INBOUND_LOSS_PROBABILITY` before each scene and clearing
//! them after. Realistic operating points spanning the override
//! thresholds so the M3.6b headroom_guard + loss_override paths
//! actually fire under bench traffic.

pub struct BandwidthPoint {
    pub label: &'static str,
    pub bytes_per_sec: u64,
    /// Inbound loss probability passed to the test-server container via
    /// GHOSTFRAME_INBOUND_LOSS_PROBABILITY. Picked to mirror realistic
    /// link conditions at each bandwidth point.
    pub loss_probability: f32,
}

pub const BANDWIDTH_POINTS: &[BandwidthPoint] = &[
    // 1 Mbps mobile/satellite edge — below HEADROOM_MIN_BYTES_PER_US
    // (0.25 B/µs ≈ 2 Mbps). Combined with 15% loss this is the
    // worst-realistic case where both headroom_guard and loss_override
    // are expected to fire.
    BandwidthPoint { label: "1mbps_edge",    bytes_per_sec:    125_000, loss_probability: 0.15 },
    // 10 Mbps DSL with mild loss — above headroom floor but tight enough
    // to stress refinement bandwidth allocation.
    BandwidthPoint { label: "10mbps_dsl",    bytes_per_sec:  1_250_000, loss_probability: 0.05 },
    // 30 Mbps cable with token loss — comfortable for TileCodec.
    BandwidthPoint { label: "30mbps_cable",  bytes_per_sec:  3_750_000, loss_probability: 0.01 },
    // 100 Mbps LAN clean — pure cost_comparison baseline.
    BandwidthPoint { label: "100mbps_lan",   bytes_per_sec: 12_500_000, loss_probability: 0.0  },
];
