// SPDX-License-Identifier: MIT OR Apache-2.0
//! Connection statistics the client keeps for its stats panel.
//!
//! Two sources feed one view. **Server metrics** arrive as `STATS_SAMPLE`
//! frames on the feedback stream's return half, described by the latest
//! `STATS_SCHEMA`. **Local metrics** are measured here, once a second, from
//! counters the core already keeps; they run whether or not the panel is
//! open so their sparklines are full when it opens.
//!
//! Plain data, no `Rc`/`RefCell`: the native client may move the core between
//! threads. Time is injected (`now_us`) like everywhere else in this crate.

use std::collections::VecDeque;

use ghostframe_protocol::stats::{
    MetricGroup, MetricKind, MetricValue, ServerFrameReader, StatsSample, StatsSchema,
    SERVER_MSG_STATS_SAMPLE, SERVER_MSG_STATS_SCHEMA,
};

use crate::stats_local::LocalStats;
use crate::wire_loss::WireLossStats;

/// Samples kept per metric: one minute at the 1 Hz cadence.
pub const RING_LEN: usize = 60;

/// No server sample for this long and the server rows are stale.
pub const STALE_AFTER_US: u64 = 3_000_000;

/// A fixed-capacity history, oldest first.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ring {
    buf: VecDeque<f64>,
}

impl Ring {
    pub(crate) fn push(&mut self, v: f64) {
        if self.buf.len() == RING_LEN {
            self.buf.pop_front();
        }
        self.buf.push_back(v);
    }

    pub(crate) fn to_vec(&self) -> Vec<f64> {
        self.buf.iter().copied().collect()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buf.len()
    }
}

/// One metric's state: its history, what to show now, and the counter state
/// a rate is derived from.
#[derive(Debug, Clone, Default)]
pub(crate) struct Series {
    ring: Ring,
    current: Option<f64>,
    text: Option<String>,
    /// Counters only: the previous `(value, time_us)`.
    prev: Option<(f64, u64)>,
}

impl Series {
    pub(crate) fn current(&self) -> Option<f64> {
        self.current
    }

    pub(crate) fn history(&self) -> Vec<f64> {
        self.ring.to_vec()
    }

    pub(crate) fn push_gauge(&mut self, v: f64) {
        if v.is_finite() {
            self.ring.push(v);
            self.current = Some(v);
        }
    }

    /// Feed a running total taken at `at_us`. A total that went backwards (a
    /// server restart, a session reset) or a clock that did not advance
    /// restarts the rate: nothing is pushed and the current value is cleared,
    /// so the panel shows a dash for a second rather than a negative rate.
    pub(crate) fn push_counter(&mut self, total: f64, at_us: u64) {
        if !total.is_finite() {
            return;
        }
        match self.prev {
            Some((prev, prev_at)) if total >= prev && at_us > prev_at => {
                let rate = (total - prev) / ((at_us - prev_at) as f64 / 1e6);
                self.ring.push(rate);
                self.current = Some(rate);
            }
            Some(_) => self.current = None,
            None => {}
        }
        self.prev = Some((total, at_us));
    }
}

/// What `ClientStats` knows, shaped for a renderer to draw without further
/// logic. Built by [`ClientStats::view`].
#[derive(Debug, Clone, PartialEq)]
pub struct StatsView {
    /// Non-empty groups in display order: Connection, Bandwidth, Video,
    /// Loss and repair, then Other.
    pub groups: Vec<GroupView>,
    pub client_version: String,
    pub server_version: Option<String>,
    pub server_host: Option<String>,
    /// `false` only when both versions are known and differ.
    pub versions_match: bool,
    /// Server frames and samples refused so far (decode errors, count or
    /// `schema_id` mismatch, a sample before any schema).
    pub samples_rejected: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupView {
    /// Stable ordering key (the group's wire byte).
    pub key: u8,
    pub title: &'static str,
    pub rows: Vec<RowView>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowView {
    /// Unique and stable while the schema lasts: `s<schema_id>.<id>` for a
    /// server metric, `c.<key>` for a local one.
    pub id: String,
    pub label: String,
    pub unit: String,
    pub kind: MetricKind,
    /// Measured by this client rather than reported by the server.
    pub local: bool,
    /// The value to show: a gauge as is, a counter as a per-second rate.
    pub current: Option<f64>,
    /// A `Text` metric's value.
    pub text: Option<String>,
    /// Oldest first, at most [`RING_LEN`] samples. Empty for text.
    pub history: Vec<f64>,
    /// Server rows only: no sample for [`STALE_AFTER_US`].
    pub stale: bool,
}

impl Default for StatsView {
    /// Nothing known yet. Versions count as matching: there is nothing to
    /// compare, and an alert before the first sample would be noise.
    fn default() -> Self {
        StatsView {
            groups: Vec::new(),
            client_version: String::new(),
            server_version: None,
            server_host: None,
            versions_match: true,
            samples_rejected: 0,
        }
    }
}

impl StatsView {
    /// The row with this id, if any group holds it.
    pub fn row(&self, id: &str) -> Option<&RowView> {
        self.groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .find(|r| r.id == id)
    }

    /// The group with this key, if it has rows.
    pub fn group(&self, key: u8) -> Option<&GroupView> {
        self.groups.iter().find(|g| g.key == key)
    }
}

/// `0.1.0 (aab2fa2d, dirty)` as `("0.1.0", "aab2fa2d")`; `None` when the
/// string is not of the form `<pkg> (<commit>[, dirty])`.
fn parse_version(s: &str) -> Option<(&str, &str)> {
    let (pkg, rest) = s.split_once(" (")?;
    let inner = rest.strip_suffix(')')?;
    let commit = inner.strip_suffix(", dirty").unwrap_or(inner);
    Some((pkg, commit))
}

/// Whether two version strings (`0.1.0 (aab2fa2d)`, `0.1.0 (aab2fa2d, dirty)`)
/// describe the same build, by the rule PR 1 fixed: the crate version and the
/// commit are compared, `dirty` never matters (the two sides compute it at
/// different build-script runs, so it can legitimately differ on current
/// code), and anything that cannot be judged (no server version yet, an
/// `unknown` commit, an unparsable string) matches.
pub fn versions_match(client: &str, server: Option<&str>) -> bool {
    let Some(server) = server else {
        return true;
    };
    match (parse_version(client), parse_version(server)) {
        (Some((client_pkg, client_commit)), Some((server_pkg, server_commit))) => {
            client_pkg == server_pkg
                && (client_commit == "unknown"
                    || server_commit == "unknown"
                    || client_commit == server_commit)
        }
        _ => true,
    }
}

fn group_order(g: &MetricGroup) -> (u8, &'static str) {
    match g {
        MetricGroup::Connection => (0, "Connection"),
        MetricGroup::Bandwidth => (1, "Bandwidth"),
        MetricGroup::Video => (2, "Video"),
        MetricGroup::Loss => (3, "Loss and repair"),
        MetricGroup::Other => (255, "Other"),
    }
}

pub struct ClientStats {
    reader: ServerFrameReader,
    schema: Option<StatsSchema>,
    server: Vec<Series>,
    last_sample_us: Option<u64>,
    rejected: u64,
    client_version: String,
    pub(crate) local: LocalStats,
}

impl ClientStats {
    pub fn new(now_us: u64) -> Self {
        ClientStats {
            reader: ServerFrameReader::default(),
            schema: None,
            server: Vec::new(),
            last_sample_us: None,
            rejected: 0,
            client_version: "unknown".to_string(),
            local: LocalStats::new(now_us),
        }
    }

    pub fn set_client_version(&mut self, version: impl Into<String>) {
        self.client_version = version.into();
    }

    /// Server frames and samples refused so far.
    pub fn samples_rejected(&self) -> u64 {
        self.rejected
    }

    /// The deadline of the next local sample, for `poll_timeout`.
    pub(crate) fn next_local_sample_us(&self) -> u64 {
        self.local.next_sample_us()
    }

    /// Feed bytes read from the feedback stream's return half, in any
    /// chunking. Complete frames are applied; an unknown frame type is
    /// skipped by its length; anything malformed is counted and dropped.
    pub fn handle_stream_bytes(&mut self, bytes: &[u8], now_us: u64) {
        self.reader.push(bytes);
        while let Some(item) = self.reader.next_frame() {
            match item {
                Err(_) => self.rejected = self.rejected.saturating_add(1),
                Ok((SERVER_MSG_STATS_SCHEMA, payload)) => {
                    match StatsSchema::decode_payload(&payload) {
                        Ok(schema) => self.apply_schema(schema),
                        Err(_) => self.rejected = self.rejected.saturating_add(1),
                    }
                }
                Ok((SERVER_MSG_STATS_SAMPLE, payload)) => {
                    let decoded = self
                        .schema
                        .as_ref()
                        .map(|schema| StatsSample::decode_payload(&payload, schema));
                    match decoded {
                        Some(Ok(sample)) => self.apply_sample(&sample, now_us),
                        Some(Err(_)) | None => self.rejected = self.rejected.saturating_add(1),
                    }
                }
                Ok(_unknown_type) => {}
            }
        }
    }

    /// Adopt a schema. A new `schema_id` clears every server ring: ids may now
    /// mean different metrics. The same id with the same metric count keeps
    /// them.
    pub fn apply_schema(&mut self, schema: StatsSchema) {
        let same = self
            .schema
            .as_ref()
            .is_some_and(|old| old.schema_id == schema.schema_id)
            && self.server.len() == schema.metrics.len();
        if !same {
            self.server = vec![Series::default(); schema.metrics.len()];
            self.last_sample_us = None;
        }
        self.schema = Some(schema);
    }

    /// Apply one sample that already decoded against the current schema.
    /// `now_us` is when it arrived, on the client's clock; it drives
    /// staleness. Counter rates use the server's own timestamps.
    pub fn apply_sample(&mut self, sample: &StatsSample, now_us: u64) {
        let Some(schema) = self.schema.as_ref() else {
            self.rejected = self.rejected.saturating_add(1);
            return;
        };
        if sample.schema_id != schema.schema_id || sample.values.len() != schema.metrics.len() {
            self.rejected = self.rejected.saturating_add(1);
            return;
        }
        for ((desc, value), series) in schema
            .metrics
            .iter()
            .zip(&sample.values)
            .zip(self.server.iter_mut())
        {
            match (&desc.kind, value) {
                (MetricKind::Gauge, MetricValue::Num(v)) => series.push_gauge(*v),
                (MetricKind::Counter, MetricValue::Num(v)) => {
                    series.push_counter(*v, sample.timestamp_us)
                }
                (MetricKind::Text, MetricValue::Text(s)) => series.text = Some(s.clone()),
                (MetricKind::Gauge | MetricKind::Counter, MetricValue::Text(_))
                | (MetricKind::Text, MetricValue::Num(_)) => {}
            }
        }
        self.last_sample_us = Some(now_us);
    }

    /// Take the once-a-second local measurements. The first call only sets
    /// the baseline the rates are measured from.
    pub(crate) fn sample_local(&mut self, now_us: u64, loss: WireLossStats) {
        self.local.sample(now_us, loss);
    }

    fn is_stale(&self, now_us: u64) -> bool {
        match self.last_sample_us {
            None => true,
            Some(t) => now_us.saturating_sub(t) > STALE_AFTER_US,
        }
    }

    /// Everything a renderer needs, as of `now_us`.
    pub fn view(&self, now_us: u64) -> StatsView {
        let stale = self.is_stale(now_us);
        let mut groups: Vec<GroupView> = Vec::new();
        let mut add = |key_title: (u8, &'static str), row: RowView| match groups
            .iter_mut()
            .find(|g| g.key == key_title.0)
        {
            Some(g) => g.rows.push(row),
            None => groups.push(GroupView {
                key: key_title.0,
                title: key_title.1,
                rows: vec![row],
            }),
        };
        if let Some(schema) = &self.schema {
            for (desc, series) in schema.metrics.iter().zip(&self.server) {
                add(
                    group_order(&desc.group),
                    RowView {
                        id: format!("s{}.{}", schema.schema_id, desc.id),
                        label: desc.label.clone(),
                        unit: desc.unit.clone(),
                        kind: desc.kind,
                        local: false,
                        current: series.current,
                        text: series.text.clone(),
                        history: series.ring.to_vec(),
                        stale,
                    },
                );
            }
        }
        for (group, row) in self.local.rows() {
            add(group_order(group), row);
        }
        groups.sort_by_key(|g| g.key);
        let server_version = self.schema.as_ref().map(|s| s.server_version.clone());
        StatsView {
            groups,
            versions_match: versions_match(&self.client_version, server_version.as_deref()),
            client_version: self.client_version.clone(),
            server_version,
            server_host: self.schema.as_ref().map(|s| s.host_name.clone()),
            samples_rejected: self.rejected,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use ghostframe_protocol::stats::MetricDescriptor;

    fn desc(id: u16, kind: MetricKind, group: MetricGroup, unit: &str) -> MetricDescriptor {
        MetricDescriptor {
            id,
            name: format!("test.m{id}"),
            unit: unit.to_string(),
            kind,
            group,
            label: format!("Metric {id}"),
        }
    }

    fn schema(schema_id: u16, metrics: Vec<MetricDescriptor>) -> StatsSchema {
        StatsSchema {
            schema_id,
            server_version: "0.1.0 (aab2fa2d)".to_string(),
            host_name: "evangeline".to_string(),
            metrics,
        }
    }

    /// Gauge, counter, text: one of each kind, in one group each.
    fn three_kinds(schema_id: u16) -> StatsSchema {
        schema(
            schema_id,
            vec![
                desc(1, MetricKind::Gauge, MetricGroup::Connection, "ms"),
                desc(2, MetricKind::Counter, MetricGroup::Loss, "1"),
                desc(3, MetricKind::Text, MetricGroup::Video, "1"),
            ],
        )
    }

    fn sample(schema_id: u16, at_us: u64, gauge: f64, counter: f64, text: &str) -> StatsSample {
        StatsSample {
            schema_id,
            timestamp_us: at_us,
            values: vec![
                MetricValue::Num(gauge),
                MetricValue::Num(counter),
                MetricValue::Text(text.to_string()),
            ],
        }
    }

    fn row(view: &StatsView, label: &str) -> RowView {
        view.groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .find(|r| r.label == label)
            .unwrap_or_else(|| panic!("no row {label}: {view:?}"))
            .clone()
    }

    fn fed(now: u64) -> ClientStats {
        let mut s = ClientStats::new(0);
        s.apply_schema(three_kinds(1));
        s.apply_sample(&sample(1, 1_000_000, 10.0, 100.0, "h264"), now);
        s
    }

    #[test]
    fn ring_keeps_the_newest_sixty_in_order() {
        let mut r = Ring::default();
        for i in 0..(RING_LEN + 7) {
            r.push(i as f64);
        }
        let v = r.to_vec();
        assert_eq!(r.len(), RING_LEN, "bounded");
        assert_eq!(v.first(), Some(&7.0), "oldest 7 were dropped");
        assert_eq!(v.last(), Some(&((RING_LEN + 6) as f64)), "newest is last");
        assert!(v.windows(2).all(|w| w[0] < w[1]), "order preserved");
    }

    #[test]
    fn gauge_and_text_samples_reach_the_view() {
        let s = fed(1_000_000);
        let v = s.view(1_000_000);
        let g = row(&v, "Metric 1");
        assert_eq!(g.current, Some(10.0));
        assert_eq!(g.history, vec![10.0]);
        assert!(!g.stale);
        assert_eq!(row(&v, "Metric 3").text.as_deref(), Some("h264"));
        assert_eq!(v.server_host.as_deref(), Some("evangeline"));
        assert_eq!(v.server_version.as_deref(), Some("0.1.0 (aab2fa2d)"));
    }

    #[test]
    fn counter_is_shown_as_a_per_second_rate() {
        let mut s = fed(1_000_000);
        // 250 more in half a second of server time = 500/s.
        s.apply_sample(&sample(1, 1_500_000, 11.0, 350.0, "h264"), 1_500_000);
        let c = row(&s.view(1_500_000), "Metric 2");
        assert_eq!(c.current, Some(500.0));
        assert_eq!(
            c.history,
            vec![500.0],
            "the first total only set the baseline"
        );
    }

    #[test]
    fn counter_going_backwards_restarts_the_rate_and_never_goes_negative() {
        let mut s = fed(1_000_000);
        s.apply_sample(&sample(1, 2_000_000, 1.0, 200.0, "x"), 2_000_000);
        // Server restarted: the total fell from 200 to 5.
        s.apply_sample(&sample(1, 3_000_000, 1.0, 5.0, "x"), 3_000_000);
        let c = row(&s.view(3_000_000), "Metric 2");
        assert_eq!(
            c.current, None,
            "restart shows no value, not a negative one"
        );
        assert_eq!(c.history, vec![100.0], "nothing pushed for the restart");
        // And it recovers from the new baseline.
        s.apply_sample(&sample(1, 4_000_000, 1.0, 25.0, "x"), 4_000_000);
        let c = row(&s.view(4_000_000), "Metric 2");
        assert_eq!(c.current, Some(20.0));
        assert!(c.history.iter().all(|r| *r >= 0.0));
    }

    #[test]
    fn server_rows_go_stale_after_three_seconds_and_keep_their_history() {
        let s = fed(10_000_000);
        let at_limit = s.view(10_000_000 + STALE_AFTER_US);
        assert!(
            !row(&at_limit, "Metric 1").stale,
            "exactly 3 s is still fresh"
        );
        let past = s.view(10_000_000 + STALE_AFTER_US + 1);
        let g = row(&past, "Metric 1");
        assert!(g.stale);
        assert_eq!(g.history, vec![10.0], "history is kept");
        assert!(
            !row(&past, "Received throughput").stale,
            "local rows are never stale"
        );
    }

    #[test]
    fn rows_are_stale_before_the_first_sample() {
        let mut s = ClientStats::new(0);
        s.apply_schema(three_kinds(1));
        assert!(row(&s.view(0), "Metric 1").stale);
    }

    #[test]
    fn a_new_schema_id_clears_the_server_rings_but_a_repeat_keeps_them() {
        let mut s = fed(1_000_000);
        s.apply_schema(three_kinds(1));
        assert_eq!(row(&s.view(1_000_000), "Metric 1").history, vec![10.0]);
        s.apply_schema(three_kinds(2));
        let v = s.view(1_000_000);
        assert!(row(&v, "Metric 1").history.is_empty());
        assert_eq!(row(&v, "Metric 1").current, None);
        // A sample of the old schema is now refused.
        s.apply_sample(&sample(1, 2_000_000, 5.0, 1.0, "x"), 2_000_000);
        assert_eq!(s.samples_rejected(), 1);
    }

    #[test]
    fn row_ids_change_with_the_schema_so_a_keyed_list_rebuilds() {
        let mut s = fed(1_000_000);
        let before = row(&s.view(1_000_000), "Metric 1").id;
        s.apply_schema(three_kinds(2));
        let after = row(&s.view(1_000_000), "Metric 1").id;
        assert_ne!(before, after);
    }

    #[test]
    fn mismatched_samples_are_dropped_and_counted() {
        let mut s = fed(1_000_000);
        // Wrong schema id.
        s.apply_sample(&sample(9, 2_000_000, 99.0, 1.0, "x"), 2_000_000);
        // Wrong value count.
        let mut short = sample(1, 2_000_000, 99.0, 1.0, "x");
        short.values.pop();
        s.apply_sample(&short, 2_000_000);
        assert_eq!(s.samples_rejected(), 2);
        let g = row(&s.view(1_000_000), "Metric 1");
        assert_eq!(g.history, vec![10.0], "a rejected sample changes nothing");
    }

    #[test]
    fn a_sample_before_any_schema_is_rejected() {
        let mut s = ClientStats::new(0);
        s.handle_stream_bytes(&sample(1, 1, 1.0, 1.0, "x").encode_frame(), 0);
        assert_eq!(s.samples_rejected(), 1);
    }

    #[test]
    fn non_finite_numbers_are_ignored() {
        let mut s = fed(1_000_000);
        s.apply_sample(
            &sample(1, 2_000_000, f64::NAN, f64::INFINITY, "x"),
            2_000_000,
        );
        let v = s.view(2_000_000);
        assert_eq!(row(&v, "Metric 1").history, vec![10.0]);
        assert!(row(&v, "Metric 2").history.is_empty());
    }

    #[test]
    fn frames_split_across_reads_are_reassembled_and_unknown_types_skipped() {
        let mut s = ClientStats::new(0);
        let mut bytes = three_kinds(1).encode_frame();
        // An unknown frame type, skipped by its length.
        bytes.extend_from_slice(&[0x7F, 2, 0, 0xAA, 0xBB]);
        bytes.extend_from_slice(&sample(1, 1_000_000, 10.0, 100.0, "h264").encode_frame());
        for chunk in bytes.chunks(3) {
            s.handle_stream_bytes(chunk, 1_000_000);
        }
        assert_eq!(s.samples_rejected(), 0);
        assert_eq!(row(&s.view(1_000_000), "Metric 1").current, Some(10.0));
    }

    #[test]
    fn an_oversized_frame_is_counted_and_the_reader_recovers() {
        let mut s = ClientStats::new(0);
        s.handle_stream_bytes(&[SERVER_MSG_STATS_SAMPLE, 0xFF, 0xFF], 0);
        assert_eq!(s.samples_rejected(), 1);
        s.handle_stream_bytes(&three_kinds(1).encode_frame(), 0);
        assert!(
            s.view(0).server_version.is_some(),
            "the next frame still parses"
        );
    }

    #[test]
    fn groups_come_in_display_order_with_other_last() {
        let mut s = ClientStats::new(0);
        s.apply_schema(schema(
            1,
            vec![
                desc(1, MetricKind::Gauge, MetricGroup::Other, "1"),
                desc(2, MetricKind::Gauge, MetricGroup::Loss, "1"),
                desc(3, MetricKind::Gauge, MetricGroup::Connection, "ms"),
                desc(4, MetricKind::Gauge, MetricGroup::Video, "1"),
                desc(5, MetricKind::Gauge, MetricGroup::Bandwidth, "bit/s"),
            ],
        ));
        let titles: Vec<&str> = s.view(0).groups.iter().map(|g| g.title).collect();
        assert_eq!(
            titles,
            vec![
                "Connection",
                "Bandwidth",
                "Video",
                "Loss and repair",
                "Other"
            ]
        );
    }

    #[test]
    fn local_metrics_are_measured_from_the_counters_and_are_non_zero() {
        let mut s = ClientStats::new(0);
        s.sample_local(1_000_000, WireLossStats::default()); // baseline
        s.local.counters.rx_bytes += 1_250_000;
        s.local.counters.nacks_sent += 30;
        s.local.counters.paints += 60;
        let loss = WireLossStats {
            lost_pre_fec: 4,
            recovered: 2,
            max_run: 3,
            ..WireLossStats::default()
        };
        s.sample_local(2_000_000, loss);
        let v = s.view(2_000_000);
        assert_eq!(row(&v, "Received throughput").current, Some(10_000_000.0));
        assert_eq!(row(&v, "NACKs sent").current, Some(30.0));
        assert_eq!(row(&v, "Painted frames").current, Some(60.0));
        assert_eq!(row(&v, "Wire loss before FEC").current, Some(4.0));
        assert_eq!(row(&v, "FEC recoveries").current, Some(2.0));
        assert_eq!(row(&v, "Longest loss run (session)").current, Some(3.0));
        assert_eq!(
            row(&v, "Longest loss run (session)").history,
            vec![3.0],
            "the baseline call pushes nothing"
        );
        assert!(row(&v, "Received throughput").local);
    }

    #[test]
    fn h264_frame_size_is_the_mean_of_the_second() {
        let mut s = ClientStats::new(0);
        s.sample_local(1_000_000, WireLossStats::default());
        s.local.counters.frames_h264 += 4;
        s.local.counters.h264_bytes += 4_000;
        s.sample_local(2_000_000, WireLossStats::default());
        assert_eq!(
            row(&s.view(2_000_000), "H.264 frame size").current,
            Some(1_000.0)
        );
        // A second with no H.264 frames reads 0, not the previous second's size.
        s.sample_local(3_000_000, WireLossStats::default());
        assert_eq!(
            row(&s.view(3_000_000), "H.264 frame size").current,
            Some(0.0)
        );
    }

    #[test]
    fn the_local_sample_deadline_advances() {
        let mut s = ClientStats::new(5);
        assert_eq!(
            s.next_local_sample_us(),
            5 + crate::stats_local::LOCAL_SAMPLE_INTERVAL_US
        );
        s.sample_local(2_500_000, WireLossStats::default());
        assert_eq!(s.next_local_sample_us(), 3_500_000);
    }

    #[test]
    fn version_comparison_ignores_dirty_and_unknown_commits() {
        let clean = "0.1.0 (aab2fa2d)";
        let dirty = "0.1.0 (aab2fa2d, dirty)";
        assert!(versions_match(clean, Some(clean)));
        assert!(versions_match(dirty, Some(dirty)));
        assert!(
            versions_match(clean, Some(dirty)),
            "dirty is shown, never a mismatch"
        );
        assert!(versions_match(dirty, Some(clean)));
        assert!(!versions_match(clean, Some("0.1.0 (0bad1dea)")));
        assert!(
            !versions_match(dirty, Some("0.1.0 (0bad1dea, dirty)")),
            "a different commit differs whatever the dirty flags say"
        );
        assert!(
            !versions_match(clean, Some("0.2.0 (aab2fa2d)")),
            "a different crate version differs"
        );
        assert!(versions_match("0.1.0 (unknown)", Some(clean)));
        assert!(versions_match(clean, Some("0.1.0 (unknown)")));
        assert!(
            versions_match("unknown", Some(clean)),
            "an unparsable string cannot be judged (the client's value before set_client_version)"
        );
        assert!(versions_match(clean, Some("")));
        assert!(
            versions_match(clean, None),
            "no server version yet: cannot judge"
        );
    }
}
