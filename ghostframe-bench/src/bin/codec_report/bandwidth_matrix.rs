//! M3.6c bandwidth-axis bench: re-runs each scene at 3 bandwidth caps
//! by setting `GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP` and
//! `GHOSTFRAME_INBOUND_LOSS_PROBABILITY` before each scene and clearing
//! them after. Realistic operating points so the headroom_guard and
//! loss_override paths in M3.6b actually fire under bench traffic.

pub struct BandwidthPoint {
    pub label: &'static str,
    pub bytes_per_sec: u64,
    /// Inbound loss probability passed to the test-server container via
    /// GHOSTFRAME_INBOUND_LOSS_PROBABILITY. Picked to mirror realistic
    /// link conditions at each bandwidth point.
    pub loss_probability: f32,
}

pub const BANDWIDTH_POINTS: &[BandwidthPoint] = &[
    BandwidthPoint { label: "10mbps_dsl",    bytes_per_sec:  1_250_000, loss_probability: 0.05 },
    BandwidthPoint { label: "30mbps_cable",  bytes_per_sec:  3_750_000, loss_probability: 0.01 },
    BandwidthPoint { label: "100mbps_lan",   bytes_per_sec: 12_500_000, loss_probability: 0.0  },
];
