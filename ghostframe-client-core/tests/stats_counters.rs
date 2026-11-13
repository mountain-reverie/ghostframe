//! `ClientStats::samples_accepted` and `frames_received` are the signals the
//! web e2e polls (`window.__ghostframeStatsSamples` / `__ghostframeStatsFrames`).
//! `samples_accepted` counts exactly the samples applied to the rings: not
//! rejected ones, not schema frames, not partial reads. `frames_received`
//! counts every complete frame, before any check, so it moves even when the
//! sample is rejected and whatever the core's subscribed flag says.

use ghostframe_client_core::{ClientConfig, ClientCore};
use ghostframe_protocol::stats::{
    MetricDescriptor, MetricGroup, MetricKind, MetricValue, StatsSample, StatsSchema,
};

fn schema() -> StatsSchema {
    StatsSchema {
        schema_id: 1,
        server_version: "0.1.0 (aaaaaaaa)".to_string(),
        host_name: "host".to_string(),
        metrics: vec![MetricDescriptor {
            id: 1,
            name: "ghostframe.rtt".to_string(),
            unit: "ms".to_string(),
            kind: MetricKind::Gauge,
            group: MetricGroup::Connection,
            label: "RTT".to_string(),
        }],
    }
}

fn sample(schema_id: u16, rtt: f64) -> StatsSample {
    StatsSample {
        schema_id,
        timestamp_us: 1_000_000,
        values: vec![MetricValue::Num(rtt)],
    }
}

fn new_core() -> ClientCore {
    ClientCore::new(ClientConfig::default(), 0)
}

#[test]
fn counts_applied_samples_only() {
    let mut core = new_core();
    assert_eq!(core.stats().samples_accepted(), 0);

    // A sample before any schema is rejected, not accepted.
    core.handle_server_stream(&sample(1, 4.0).encode_frame(), 500_000);
    assert_eq!(core.stats().samples_accepted(), 0);

    core.handle_server_stream(&schema().encode_frame(), 600_000);
    assert_eq!(
        core.stats().samples_accepted(),
        0,
        "a schema is not a sample"
    );

    core.handle_server_stream(&sample(1, 4.0).encode_frame(), 1_000_000);
    core.handle_server_stream(&sample(1, 5.0).encode_frame(), 2_000_000);
    assert_eq!(core.stats().samples_accepted(), 2);

    // A sample for another schema id is rejected and not counted.
    core.handle_server_stream(&sample(9, 1.0).encode_frame(), 3_000_000);
    assert_eq!(core.stats().samples_accepted(), 2);
    assert_eq!(core.stats().view(3_000_000).samples_rejected, 2);
}

#[test]
fn frames_received_counts_every_frame_before_any_check() {
    let mut core = new_core();
    assert_eq!(core.stats().frames_received(), 0);

    // Rejected (no schema yet): still a received frame. This is the case the
    // e2e needs: a server that sends without a subscribe produces exactly
    // this, and `samples_accepted` never sees it.
    core.handle_server_stream(&sample(1, 4.0).encode_frame(), 500_000);
    assert_eq!(core.stats().frames_received(), 1);
    assert_eq!(core.stats().samples_accepted(), 0);

    core.handle_server_stream(&schema().encode_frame(), 600_000);
    assert_eq!(core.stats().frames_received(), 2, "a schema is a frame");

    core.handle_server_stream(&sample(9, 1.0).encode_frame(), 700_000);
    assert_eq!(
        core.stats().frames_received(),
        3,
        "a rejected sample is a frame"
    );
}

#[test]
fn counters_ignore_the_cores_own_subscribed_flag() {
    // The core has never been told the panel is open (`set_stats_subscribed`
    // not called), and is then told it is closed: samples are still applied
    // and frames still counted. The e2e's "server went quiet" assertion rests
    // on this: the client must not filter on its own belief.
    let mut core = new_core();
    core.set_stats_subscribed(false);
    core.handle_server_stream(&schema().encode_frame(), 0);
    core.handle_server_stream(&sample(1, 3.0).encode_frame(), 1_000_000);
    assert_eq!(core.stats().frames_received(), 2);
    assert_eq!(core.stats().samples_accepted(), 1);
}

#[test]
fn a_frame_split_across_reads_counts_once() {
    let mut core = new_core();
    core.handle_server_stream(&schema().encode_frame(), 0);

    let frame = sample(1, 7.0).encode_frame();
    let (head, tail) = frame.split_at(frame.len() / 2);
    core.handle_server_stream(head, 1_000_000);
    assert_eq!(
        core.stats().samples_accepted(),
        0,
        "half a frame is no sample"
    );
    assert_eq!(
        core.stats().frames_received(),
        1,
        "half a frame is no frame"
    );
    core.handle_server_stream(tail, 1_000_100);
    assert_eq!(core.stats().samples_accepted(), 1);
    assert_eq!(core.stats().frames_received(), 2);
}

#[test]
fn apply_sample_counts_only_when_its_own_checks_pass() {
    // `apply_sample` is public and has its own schema checks, which the
    // stream path (decoding against the schema first) never reaches.
    use ghostframe_client_core::stats::ClientStats;
    let mut stats = ClientStats::new(0);
    stats.apply_sample(&sample(1, 1.0), 1_000_000);
    assert_eq!(stats.samples_accepted(), 0, "no schema yet");
    stats.apply_schema(schema());
    stats.apply_sample(&sample(9, 1.0), 1_000_000);
    assert_eq!(stats.samples_accepted(), 0, "wrong schema id");
    stats.apply_sample(&sample(1, 1.0), 1_000_000);
    assert_eq!(stats.samples_accepted(), 1);
}
