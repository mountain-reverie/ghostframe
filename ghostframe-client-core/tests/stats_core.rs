//! `ClientCore`'s side of the stats panel: the subscribe message, the server
//! stream, and the local measurements.

use ghostframe_client_core::{ClientConfig, ClientCore, PollOutput, RowView, StatsView};
use ghostframe_protocol::stats::{
    MetricDescriptor, MetricGroup, MetricKind, MetricValue, StatsSample, StatsSchema,
};

fn test_core() -> ClientCore {
    let mut core = ClientCore::new(ClientConfig::default(), 0);
    while core.poll_transmit(0).is_some() {} // the Hello
    core
}

fn row(view: &StatsView, label: &str) -> RowView {
    view.groups
        .iter()
        .flat_map(|g| g.rows.iter())
        .find(|r| r.label == label)
        .unwrap_or_else(|| panic!("no row {label}: {view:?}"))
        .clone()
}

#[test]
fn subscribe_and_unsubscribe_are_queued_on_the_feedback_stream() {
    let mut core = test_core();
    core.set_stats_subscribed(true);
    core.set_stats_subscribed(false);
    assert_eq!(
        core.poll_transmit(0),
        Some(PollOutput::Stream(vec![0x0B, 1]))
    );
    assert_eq!(
        core.poll_transmit(0),
        Some(PollOutput::Stream(vec![0x0B, 0]))
    );
    assert_eq!(core.poll_transmit(0), None);
}

#[test]
fn the_server_stream_reaches_the_view() {
    let mut core = test_core();
    core.set_client_version("0.1.0 (aab2fa2d)");
    let schema = StatsSchema {
        schema_id: 7,
        server_version: "0.1.0 (aab2fa2d)".to_string(),
        host_name: "evangeline".to_string(),
        metrics: vec![MetricDescriptor {
            id: 1,
            name: "ghostframe.rtt".to_string(),
            unit: "ms".to_string(),
            kind: MetricKind::Gauge,
            group: MetricGroup::Connection,
            label: "Round trip".to_string(),
        }],
    };
    let sample = StatsSample {
        schema_id: 7,
        timestamp_us: 1_000_000,
        values: vec![MetricValue::Num(18.4)],
    };
    let mut bytes = schema.encode_frame();
    bytes.extend(sample.encode_frame());
    // Delivered in two reads, split mid-frame.
    let (a, b) = bytes.split_at(bytes.len() / 2);
    core.handle_server_stream(a, 2_000_000);
    core.handle_server_stream(b, 2_000_000);
    let view = core.stats().view(2_000_000);
    assert_eq!(row(&view, "Round trip").current, Some(18.4));
    assert_eq!(view.server_host.as_deref(), Some("evangeline"));
    assert_eq!(view.client_version, "0.1.0 (aab2fa2d)");
    assert!(view.versions_match);
    assert_eq!(core.stats().samples_rejected(), 0);
}
