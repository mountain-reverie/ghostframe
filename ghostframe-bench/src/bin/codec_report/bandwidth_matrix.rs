//! M3.6c bandwidth-axis bench: re-runs each scene at 3 bandwidth caps
//! by setting `GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP` before each scene
//! and clearing it after. Mirrors the existing loss-injection env-var
//! protocol.

pub struct BandwidthPoint {
    pub label: &'static str,
    pub bytes_per_sec: u64,
}

pub const BANDWIDTH_POINTS: &[BandwidthPoint] = &[
    BandwidthPoint { label: "10mbps_dsl",    bytes_per_sec:  1_250_000 },
    BandwidthPoint { label: "30mbps_cable",  bytes_per_sec:  3_750_000 },
    BandwidthPoint { label: "100mbps_lan",   bytes_per_sec: 12_500_000 },
];
