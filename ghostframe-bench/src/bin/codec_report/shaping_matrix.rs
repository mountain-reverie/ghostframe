//! M3.7b shaping matrix: re-runs each scene at 4 realistic operating
//! points driven by tc qdisc inside the test-server container. Unlike
//! M3.6c's `BANDWIDTH_POINTS` (which set GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP
//! at the WebTransport send layer above QUIC), these points use SHAPE_*
//! env vars that drive kernel-level netem + tbf, so QUIC's cwnd reacts
//! realistically. The harness forwards SHAPE_* to the container; the
//! container entrypoint (M3.7b Task 10) applies the qdisc on eth0.

pub struct ShapingPoint {
    pub label: &'static str,
    pub bandwidth_kbps: u32,
    pub delay_ms: u32,
    pub loss_pct: u32,
}

pub const SHAPING_POINTS: &[ShapingPoint] = &[
    // 1 Mbps satellite / heavily-congested. 300 ms RTT + 15% loss is
    // worst-realistic; QUIC cwnd should settle below the headroom floor.
    ShapingPoint {
        label: "1mbps_sat",
        bandwidth_kbps: 1_000,
        delay_ms: 300,
        loss_pct: 15,
    },
    // 10 Mbps DSL with cable-grade latency.
    ShapingPoint {
        label: "10mbps_dsl",
        bandwidth_kbps: 10_000,
        delay_ms: 50,
        loss_pct: 5,
    },
    // 30 Mbps standard cable.
    ShapingPoint {
        label: "30mbps_cable",
        bandwidth_kbps: 30_000,
        delay_ms: 20,
        loss_pct: 1,
    },
    // 100 Mbps LAN — clean baseline.
    ShapingPoint {
        label: "100mbps_lan",
        bandwidth_kbps: 100_000,
        delay_ms: 5,
        loss_pct: 0,
    },
];
