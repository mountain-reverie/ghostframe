// SPDX-License-Identifier: AGPL-3.0-or-later
//! The server half of the stats panel, end to end and with no UI: a real
//! `ClientNet` subscribes on the feedback stream, the real `IoBridge` answers
//! on its return half, and the numbers are decoded with the protocol crate.
//!
//! Run: `cargo test --test browserless_stats -- --test-threads=1`

use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, LinkOverrides, RunOptions,
    SceneLoad,
};
use ghostframe_e2e::harness::load_profile::{Churn, LoadProfile, PRODUCTION_CADENCE_US};
use ghostframe_e2e::harness::stats_tap::StatsPlan;
use ghostframe_e2e::netsim::NetProfile;
use ghostframe_protocol::stats::{MetricValue, StatsSample, StatsSchema};
use std::time::Duration;

const S: u64 = 1_000_000;

async fn run(plan: StatsPlan) -> BrowserlessResult {
    let scene = BrowserlessScene {
        seed: 7,
        tick_budget_floor_bytes: None,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Profile(LoadProfile {
            cadence_us: PRODUCTION_CADENCE_US,
            churn: Churn::Region { tiles_per_tick: 8 },
        }),
        cadence_us: PRODUCTION_CADENCE_US,
        net: NetProfile {
            delay_us: 10_000,
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(12),
        grid_cols: 16,
        grid_rows: 16,
    };
    run_browserless_with_options(
        scene,
        LinkOverrides::default(),
        RunOptions {
            stats: Some(plan),
            ..Default::default()
        },
    )
    .await
    .expect("scene ran")
}

fn index(schema: &StatsSchema, name: &str) -> usize {
    schema
        .metrics
        .iter()
        .position(|m| m.name == name)
        .unwrap_or_else(|| panic!("schema has no {name}"))
}

fn num(schema: &StatsSchema, s: &StatsSample, name: &str) -> f64 {
    match &s.values[index(schema, name)] {
        MetricValue::Num(x) => *x,
        MetricValue::Text(t) => panic!("{name} is text {t:?}"),
    }
}

fn subscribe_at_1s() -> StatsPlan {
    StatsPlan {
        subscribe_at: Some(Duration::from_secs(1)),
        ..Default::default()
    }
}

/// The positive control and the main gate: a subscribed session gets a schema,
/// then about one sample a second, whose numbers are live.
#[tokio::test(start_paused = true)]
async fn a_subscribed_session_gets_a_schema_then_live_samples() {
    let r = run(subscribe_at_1s()).await;
    let st = &r.client_stats;
    println!(
        "STATS schemas={} samples={} rejected={} frame_errors={} bytes_read={}",
        st.schemas.len(),
        st.samples.len(),
        st.rejected,
        st.frame_errors,
        st.bytes_read
    );

    assert_eq!(st.subscribes_sent, 1);
    assert_eq!(st.schemas.len(), 1, "one subscribe, one schema");
    assert_eq!(st.frame_errors, 0);
    assert_eq!(st.rejected, 0, "every sample matched its schema");
    assert!(
        st.samples.len() >= 8,
        "11 s of subscription at 1 Hz, first sample with the schema: {}",
        st.samples.len()
    );

    let schema = &st.schemas[0].1;
    assert_ne!(schema.schema_id, 0);
    assert!(!schema.server_version.is_empty() && !schema.host_name.is_empty());

    // Timestamps: strictly increasing, about one second apart.
    for pair in st.samples.windows(2) {
        let gap = pair[1].1.timestamp_us - pair[0].1.timestamp_us;
        assert!(
            (900_000..=1_500_000).contains(&gap),
            "samples are ~1 s apart on the server's clock, got {gap} us"
        );
    }

    // The measurements are non-zero: a probe that never fires reads 0.
    let last = &st.samples.last().expect("samples").1;
    let bwe = num(schema, last, "ghostframe.bwe.estimate");
    assert!(bwe > 0.0, "bandwidth estimate: {bwe}");
    let rtt = num(schema, last, "ghostframe.rtt.smoothed");
    assert!(
        (20.0..1000.0).contains(&rtt),
        "RTT over a 2 x 10 ms link is at least 20 ms and well under a second: {rtt}"
    );
    let first_sent = num(schema, &st.samples[0].1, "ghostframe.sent.bytes");
    let last_sent = num(schema, last, "ghostframe.sent.bytes");
    assert!(
        last_sent > first_sent && last_sent > 0.0,
        "bytes handed to quinn grow while tiles are sent: {first_sent} -> {last_sent}"
    );
}

/// The negative control: the same scene and the same tap, never subscribing.
/// Vacuous without the positive test above, which proves the tap reads the
/// return half in this very scene.
#[tokio::test(start_paused = true)]
async fn nothing_arrives_without_a_subscribe() {
    let r = run(StatsPlan::default()).await;
    let st = &r.client_stats;
    assert_eq!(
        st.subscribes_sent, 0,
        "premise: this client never subscribed"
    );
    assert_eq!(st.bytes_read, 0, "not one byte on the return half");
    assert!(st.schemas.is_empty() && st.samples.is_empty());
}

/// After an unsubscribe the stream stops: no sample arrives later than the
/// message's own transit plus one in-flight frame.
#[tokio::test(start_paused = true)]
async fn an_unsubscribe_stops_the_stream() {
    let r = run(StatsPlan {
        subscribe_at: Some(Duration::from_secs(1)),
        unsubscribe_at: Some(Duration::from_secs(6)),
        ..Default::default()
    })
    .await;
    let st = &r.client_stats;
    assert_eq!(st.unsubscribes_sent, 1);
    assert!(
        st.samples.len() >= 3,
        "premise: samples flowed before the unsubscribe: {}",
        st.samples.len()
    );
    let latest = st.samples.iter().map(|(t, _)| *t).max().expect("samples");
    assert!(
        latest < 6 * S + 300_000,
        "a sample arrived at {latest} us, long after the unsubscribe at 6 s"
    );
}

/// A repeated subscribe re-sends the schema under a new id, and samples then
/// follow the new one.
#[tokio::test(start_paused = true)]
async fn a_repeated_subscribe_resends_the_schema_with_a_new_id() {
    let r = run(StatsPlan {
        subscribe_at: Some(Duration::from_secs(1)),
        resubscribe_at: Some(Duration::from_secs(5)),
        ..Default::default()
    })
    .await;
    let st = &r.client_stats;
    assert_eq!(st.subscribes_sent, 2);
    assert_eq!(st.schemas.len(), 2, "every on is answered with a schema");
    assert_ne!(st.schemas[0].1.schema_id, st.schemas[1].1.schema_id);
    assert_eq!(st.rejected, 0, "no sample straddled the change unmatched");
    let after = st
        .samples
        .iter()
        .filter(|(t, _)| *t > st.schemas[1].0)
        .all(|(_, s)| s.schema_id == st.schemas[1].1.schema_id);
    assert!(after, "samples after the second schema carry its id");
}
