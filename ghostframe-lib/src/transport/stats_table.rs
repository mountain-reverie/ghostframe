// SPDX-License-Identifier: AGPL-3.0-or-later
//! The server's stats metric table and the numbers behind it.
//!
//! Internal on purpose: nothing here is `pub`, so none of it reaches the
//! cbindgen header. It is the one list a later OpenTelemetry exporter will
//! read, so the panel and the exporter cannot drift apart.
//!
//! A metric is a [`Stat`]. [`Stat::ALL`] is the order of a schema and of
//! every sample against it; [`Stat::descriptor`] and [`StatsInputs::value`]
//! are exhaustive matches, so adding a variant that forgets either does not
//! compile. Ids are stable: never renumber, never reuse.

use ghostframe_protocol::stats::{
    MetricDescriptor, MetricGroup, MetricKind, MetricValue, StatsSchema,
};

/// `0.1.0 (aab2fa2d)`, `0.1.0 (aab2fa2d, dirty)`, or `0.1.0 (unknown)` outside a
/// git checkout. Captured by `build.rs`; see its doc for what triggers a rerun.
pub(crate) const SERVER_VERSION: &str = env!("GHOSTFRAME_BUILD_VERSION");

/// One server metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stat {
    RttSmoothed,
    SessionUptime,
    BweEstimate,
    RateTarget,
    SentBytes,
    FrameMode,
    ModeReason,
    H264Backend,
    LossPathResends,
    LossVerdicts,
    NacksReceived,
}

impl Stat {
    /// Schema and sample order.
    pub(crate) const ALL: [Stat; 11] = [
        Stat::RttSmoothed,
        Stat::SessionUptime,
        Stat::BweEstimate,
        Stat::RateTarget,
        Stat::SentBytes,
        Stat::FrameMode,
        Stat::ModeReason,
        Stat::H264Backend,
        Stat::LossPathResends,
        Stat::LossVerdicts,
        Stat::NacksReceived,
    ];

    /// The wire id. Stable.
    pub(crate) fn id(self) -> u16 {
        match self {
            Stat::RttSmoothed => 1,
            Stat::SessionUptime => 2,
            Stat::BweEstimate => 3,
            Stat::RateTarget => 4,
            Stat::SentBytes => 5,
            Stat::FrameMode => 6,
            Stat::ModeReason => 7,
            Stat::H264Backend => 8,
            Stat::LossPathResends => 9,
            Stat::LossVerdicts => 10,
            Stat::NacksReceived => 11,
        }
    }

    /// `(name, unit, kind, group, label)`.
    fn def(
        self,
    ) -> (
        &'static str,
        &'static str,
        MetricKind,
        MetricGroup,
        &'static str,
    ) {
        use MetricGroup::{Bandwidth, Connection, Loss, Video};
        use MetricKind::{Counter, Gauge, Text};
        match self {
            Stat::RttSmoothed => (
                "ghostframe.rtt.smoothed",
                "ms",
                Gauge,
                Connection,
                "Round-trip time",
            ),
            Stat::SessionUptime => (
                "ghostframe.session.uptime",
                "s",
                Gauge,
                Connection,
                "Session uptime",
            ),
            Stat::BweEstimate => (
                "ghostframe.bwe.estimate",
                "bit/s",
                Gauge,
                Bandwidth,
                "Bandwidth estimate",
            ),
            Stat::RateTarget => (
                "ghostframe.rate.target",
                "bit/s",
                Gauge,
                Bandwidth,
                "Rate target",
            ),
            Stat::SentBytes => ("ghostframe.sent.bytes", "By", Counter, Bandwidth, "Sent"),
            Stat::FrameMode => ("ghostframe.frame.mode", "1", Text, Video, "Frame mode"),
            Stat::ModeReason => (
                "ghostframe.frame.mode_reason",
                "1",
                Text,
                Video,
                "Mode reason",
            ),
            Stat::H264Backend => ("ghostframe.h264.backend", "1", Text, Video, "H.264 encoder"),
            Stat::LossPathResends => (
                "ghostframe.loss.path_resends",
                "1",
                Counter,
                Loss,
                "Loss-path resends",
            ),
            Stat::LossVerdicts => (
                "ghostframe.loss.verdicts",
                "1",
                Counter,
                Loss,
                "Loss verdicts",
            ),
            Stat::NacksReceived => (
                "ghostframe.loss.nacks_received",
                "1",
                Counter,
                Loss,
                "NACKs received",
            ),
        }
    }

    /// What goes in the schema for this metric.
    pub(crate) fn descriptor(self) -> MetricDescriptor {
        let (name, unit, kind, group, label) = self.def();
        MetricDescriptor {
            id: self.id(),
            name: name.to_string(),
            unit: unit.to_string(),
            kind,
            group,
            label: label.to_string(),
        }
    }
}

/// The numbers one sample is built from, read off the bridge in one place so
/// the table above and the bridge cannot disagree about a source.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct StatsInputs {
    /// Smoothed round-trip time, milliseconds.
    pub(crate) rtt_ms: f64,
    /// Seconds since the live session began.
    pub(crate) uptime_s: f64,
    /// The bandwidth estimate, bit/s.
    pub(crate) bwe_bps: f64,
    /// The rate the H.264 encoder was last told, bit/s (0 off H.264).
    pub(crate) rate_target_bps: f64,
    /// Cumulative bytes handed to quinn for this session's datagrams.
    pub(crate) sent_bytes: f64,
    /// `tile` or `h264`.
    pub(crate) frame_mode: &'static str,
    /// The last `mode.decision` reason, `none` before the first.
    pub(crate) mode_reason: &'static str,
    /// `libva-direct`, `openh264`, `pcm`, `none` or `unavailable`.
    pub(crate) h264_backend: &'static str,
    pub(crate) loss_path_resends: f64,
    pub(crate) loss_verdicts: f64,
    pub(crate) nacks_received: f64,
}

impl StatsInputs {
    /// The value of one metric.
    pub(crate) fn value(&self, stat: Stat) -> MetricValue {
        match stat {
            Stat::RttSmoothed => MetricValue::Num(self.rtt_ms),
            Stat::SessionUptime => MetricValue::Num(self.uptime_s),
            Stat::BweEstimate => MetricValue::Num(self.bwe_bps),
            Stat::RateTarget => MetricValue::Num(self.rate_target_bps),
            Stat::SentBytes => MetricValue::Num(self.sent_bytes),
            Stat::FrameMode => MetricValue::Text(self.frame_mode.to_string()),
            Stat::ModeReason => MetricValue::Text(self.mode_reason.to_string()),
            Stat::H264Backend => MetricValue::Text(self.h264_backend.to_string()),
            Stat::LossPathResends => MetricValue::Num(self.loss_path_resends),
            Stat::LossVerdicts => MetricValue::Num(self.loss_verdicts),
            Stat::NacksReceived => MetricValue::Num(self.nacks_received),
        }
    }

    /// Every value, in [`Stat::ALL`] order: a sample's `values`.
    pub(crate) fn values(&self) -> Vec<MetricValue> {
        Stat::ALL.iter().map(|s| self.value(*s)).collect()
    }
}

/// This machine's host name, `unknown` when the kernel will not say.
///
/// `ghostframe-server` has the same helper in `util.rs`, but the library
/// cannot depend on the server crate.
pub(crate) fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into `buf`.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return "unknown".to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    if name.is_empty() {
        "unknown".to_string()
    } else {
        name
    }
}

/// The schema a session sends: the version, the host and the whole table.
pub(crate) fn schema(schema_id: u16) -> StatsSchema {
    StatsSchema {
        schema_id,
        server_version: SERVER_VERSION.to_string(),
        host_name: host_name(),
        metrics: Stat::ALL.iter().map(|s| s.descriptor()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_version_has_the_documented_shape() {
        let rest = SERVER_VERSION
            .strip_prefix(env!("CARGO_PKG_VERSION"))
            .unwrap_or_else(|| panic!("{SERVER_VERSION:?} must start with the crate version"));
        let inner = rest
            .strip_prefix(" (")
            .and_then(|r| r.strip_suffix(')'))
            .unwrap_or_else(|| panic!("{SERVER_VERSION:?} must end in ` (<commit>)`"));
        let commit = inner.strip_suffix(", dirty").unwrap_or(inner);
        assert!(
            commit == "unknown"
                || (commit.len() >= 8 && commit.chars().all(|c| c.is_ascii_hexdigit())),
            "commit part is `unknown` or 8+ hex digits: {SERVER_VERSION:?}"
        );
    }

    #[test]
    fn server_version_names_the_checked_out_commit() {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--short=8", "HEAD"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output();
        let Ok(out) = out else { return }; // no git on this machine
        if !out.status.success() {
            return; // not a checkout (a source tarball): `unknown` is right
        }
        let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(
            SERVER_VERSION.contains(&head),
            "build.rs captured a stale commit: version {SERVER_VERSION:?}, HEAD {head}"
        );
    }

    use ghostframe_protocol::stats::{
        MetricGroup, MetricKind, MetricValue, StatsSchema, SERVER_FRAME_MAX_LEN,
        SERVER_MSG_STATS_SCHEMA,
    };

    #[test]
    fn ids_and_names_are_unique_and_ids_are_nonzero() {
        let mut ids = std::collections::HashSet::new();
        let mut names = std::collections::HashSet::new();
        for s in Stat::ALL {
            let d = s.descriptor();
            assert_ne!(d.id, 0, "id 0 is reserved");
            assert!(ids.insert(d.id), "id {} used twice", d.id);
            assert!(names.insert(d.name.clone()), "name {} used twice", d.name);
            assert!(!d.label.is_empty() && !d.unit.is_empty());
        }
    }

    #[test]
    fn every_stat_is_in_all_exactly_once() {
        // `ALL` is written by hand and the compiler cannot compare it with the
        // enum: the literal 11 below is the guard. Adding a variant means
        // updating `ALL` and this number together.
        let n = Stat::ALL.len();
        let distinct: std::collections::HashSet<u16> = Stat::ALL.iter().map(|s| s.id()).collect();
        assert_eq!(distinct.len(), n);
        assert_eq!(
            n, 11,
            "the v1 table; update the spec table when this changes"
        );
    }

    #[test]
    fn a_value_has_the_variant_its_descriptor_kind_says() {
        let inputs = StatsInputs::default();
        for s in Stat::ALL {
            let d = s.descriptor();
            match (d.kind, inputs.value(s)) {
                (MetricKind::Text, MetricValue::Text(_)) => {}
                (MetricKind::Gauge | MetricKind::Counter, MetricValue::Num(_)) => {}
                (kind, v) => panic!("{}: kind {kind:?} but value {v:?}", d.name),
            }
        }
    }

    #[test]
    fn values_follow_the_schema_order_and_carry_the_inputs() {
        let inputs = StatsInputs {
            rtt_ms: 18.5,
            uptime_s: 61.0,
            bwe_bps: 42_100_000.0,
            rate_target_bps: 33_000_000.0,
            sent_bytes: 9_000_000.0,
            frame_mode: "h264",
            mode_reason: "bandwidth_short",
            h264_backend: "libva-direct",
            loss_path_resends: 4.0,
            loss_verdicts: 5.0,
            nacks_received: 6.0,
        };
        let schema = schema(3);
        let values = inputs.values();
        assert_eq!(values.len(), schema.metrics.len());
        use MetricValue::{Num, Text};
        let expect = [
            ("ghostframe.rtt.smoothed", Num(18.5)),
            ("ghostframe.session.uptime", Num(61.0)),
            ("ghostframe.bwe.estimate", Num(42_100_000.0)),
            ("ghostframe.rate.target", Num(33_000_000.0)),
            ("ghostframe.sent.bytes", Num(9_000_000.0)),
            ("ghostframe.frame.mode", Text("h264".into())),
            (
                "ghostframe.frame.mode_reason",
                Text("bandwidth_short".into()),
            ),
            ("ghostframe.h264.backend", Text("libva-direct".into())),
            ("ghostframe.loss.path_resends", Num(4.0)),
            ("ghostframe.loss.verdicts", Num(5.0)),
            ("ghostframe.loss.nacks_received", Num(6.0)),
        ];
        assert_eq!(expect.len(), schema.metrics.len());
        for (n, want) in expect {
            let i = schema.metrics.iter().position(|m| m.name == n).expect(n);
            assert_eq!(values[i], want, "{n}");
        }
    }

    #[test]
    fn the_schema_fits_the_wire_and_round_trips() {
        let s = schema(9);
        assert_eq!(s.schema_id, 9);
        assert_eq!(s.server_version, SERVER_VERSION);
        assert!(
            !s.host_name.is_empty(),
            "falls back to `unknown`, never empty"
        );
        let frame = s
            .try_encode_frame()
            .expect("the table fits the 16 KiB frame");
        assert!(frame.len() - 3 <= SERVER_FRAME_MAX_LEN);
        assert_eq!(frame[0], SERVER_MSG_STATS_SCHEMA);
        let back = StatsSchema::decode_payload(&frame[3..]).expect("decodes");
        assert_eq!(back, s);
    }

    #[test]
    fn groups_cover_the_four_panel_sections() {
        let groups: std::collections::HashSet<u8> = Stat::ALL
            .iter()
            .map(|s| s.descriptor().group.as_u8())
            .collect();
        for g in [
            MetricGroup::Connection,
            MetricGroup::Bandwidth,
            MetricGroup::Video,
            MetricGroup::Loss,
        ] {
            assert!(groups.contains(&g.as_u8()), "no metric in {g:?}");
        }
    }
}
