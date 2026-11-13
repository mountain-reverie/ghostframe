//! The client's own measurements, taken once a second whether or not the
//! stats panel is open, so a sparkline is already full when it opens.
//!
//! The core bumps [`LocalCounters`] as it works (one add per event, no
//! allocation); [`LocalStats::sample`] turns them into per-second rates.

use ghostframe_protocol::stats::{MetricGroup, MetricKind};

use crate::stats::{RowView, Series};
use crate::wire_loss::WireLossStats;

/// Local metrics are sampled this often.
pub const LOCAL_SAMPLE_INTERVAL_US: u64 = 1_000_000;

/// How a local metric is derived from the raw numbers taken each second.
#[derive(Debug, Clone, Copy)]
enum LocalSource {
    /// Per-second rate of a running total, times a scale (8 turns bytes into bits).
    Rate(f64),
    /// The raw number as is.
    Gauge,
}

struct LocalDef {
    key: &'static str,
    label: &'static str,
    unit: &'static str,
    kind: MetricKind,
    group: MetricGroup,
    source: LocalSource,
}

/// Indices into the raw numbers one sample takes; also `LOCAL_DEFS` order.
const RX_BYTES: usize = 0;
const FRAME_SIZE: usize = 1;
const PAINTS: usize = 2;
const TILES_SOLID: usize = 3;
const TILES_PALRLE: usize = 4;
const PASSES_CDF53: usize = 5;
const FRAMES_H264: usize = 6;
const WIRE_LOST: usize = 7;
const FEC_RECOVERED: usize = 8;
const LONGEST_RUN: usize = 9;
const NACKS_SENT: usize = 10;
const DECODE_ERRORS: usize = 11;
const LOCAL_COUNT: usize = 12;

static LOCAL_DEFS: [LocalDef; LOCAL_COUNT] = [
    LocalDef {
        key: "rx_throughput",
        label: "Received throughput",
        unit: "bit/s",
        kind: MetricKind::Gauge,
        group: MetricGroup::Bandwidth,
        source: LocalSource::Rate(8.0),
    },
    LocalDef {
        key: "frame_size",
        label: "H.264 frame size",
        unit: "By",
        kind: MetricKind::Gauge,
        group: MetricGroup::Video,
        source: LocalSource::Gauge,
    },
    LocalDef {
        key: "paints",
        label: "Painted frames",
        unit: "fps",
        kind: MetricKind::Gauge,
        group: MetricGroup::Video,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "tiles_solid",
        label: "Solid tiles",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Video,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "tiles_palrle",
        label: "PalRLE tiles",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Video,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "passes_cdf53",
        label: "CDF 5/3 passes",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Video,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "frames_h264",
        label: "H.264 frames",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Video,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "wire_lost",
        label: "Wire loss before FEC",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Loss,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "fec_recovered",
        label: "FEC recoveries",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Loss,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "longest_run",
        label: "Longest loss run (session)",
        unit: "1",
        kind: MetricKind::Gauge,
        group: MetricGroup::Loss,
        source: LocalSource::Gauge,
    },
    LocalDef {
        key: "nacks_sent",
        label: "NACKs sent",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Loss,
        source: LocalSource::Rate(1.0),
    },
    LocalDef {
        key: "decode_errors",
        label: "Decode errors",
        unit: "1",
        kind: MetricKind::Counter,
        group: MetricGroup::Loss,
        source: LocalSource::Rate(1.0),
    },
];

/// Running totals the core bumps as it works. Cheap: one add per event.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LocalCounters {
    pub rx_bytes: u64,
    pub tiles_solid: u64,
    pub tiles_palrle: u64,
    pub passes_cdf53: u64,
    pub frames_h264: u64,
    pub h264_bytes: u64,
    pub nacks_sent: u64,
    /// Decode errors the payload step reports (`Event::DecodeError`). This
    /// undercounts: errors the pixel decoder raises later (native client,
    /// `Decoded` delivery) never pass through `tally_events`, and the web
    /// renderer's own decode failures are not fed back here either.
    pub decode_errors: u64,
    pub paints: u64,
}

/// The local metrics' state.
pub(crate) struct LocalStats {
    series: Vec<Series>,
    pub(crate) counters: LocalCounters,
    next_sample_us: u64,
    last_us: Option<u64>,
    prev_h264: (u64, u64),
}

impl LocalStats {
    pub(crate) fn new(now_us: u64) -> Self {
        LocalStats {
            series: vec![Series::default(); LOCAL_COUNT],
            counters: LocalCounters::default(),
            next_sample_us: now_us + LOCAL_SAMPLE_INTERVAL_US,
            last_us: None,
            prev_h264: (0, 0),
        }
    }

    /// The deadline of the next sample, for `poll_timeout`.
    pub(crate) fn next_sample_us(&self) -> u64 {
        self.next_sample_us
    }

    /// Take the measurements. The first call only sets the baseline the
    /// rates are measured from.
    pub(crate) fn sample(&mut self, now_us: u64, loss: WireLossStats) {
        self.next_sample_us = now_us + LOCAL_SAMPLE_INTERVAL_US;
        let c = self.counters;
        let (frames, bytes) = (c.frames_h264, c.h264_bytes);
        let frame_size = if frames > self.prev_h264.0 {
            (bytes - self.prev_h264.1) as f64 / (frames - self.prev_h264.0) as f64
        } else {
            0.0
        };
        self.prev_h264 = (frames, bytes);
        let mut raw = [0.0f64; LOCAL_COUNT];
        raw[RX_BYTES] = c.rx_bytes as f64;
        raw[FRAME_SIZE] = frame_size;
        raw[PAINTS] = c.paints as f64;
        raw[TILES_SOLID] = c.tiles_solid as f64;
        raw[TILES_PALRLE] = c.tiles_palrle as f64;
        raw[PASSES_CDF53] = c.passes_cdf53 as f64;
        raw[FRAMES_H264] = c.frames_h264 as f64;
        raw[WIRE_LOST] = loss.lost_pre_fec as f64;
        raw[FEC_RECOVERED] = loss.recovered as f64;
        raw[LONGEST_RUN] = loss.max_run as f64;
        raw[NACKS_SENT] = c.nacks_sent as f64;
        raw[DECODE_ERRORS] = c.decode_errors as f64;

        let first = self.last_us.is_none();
        self.last_us = Some(now_us);
        for ((def, series), value) in LOCAL_DEFS.iter().zip(self.series.iter_mut()).zip(raw) {
            match def.source {
                LocalSource::Gauge => {
                    if !first {
                        series.push_gauge(value)
                    }
                }
                LocalSource::Rate(scale) => series.push_counter(value * scale, now_us),
            }
        }
    }

    /// Every local metric as a row, with the group it belongs to.
    pub(crate) fn rows(&self) -> Vec<(&'static MetricGroup, RowView)> {
        LOCAL_DEFS
            .iter()
            .zip(&self.series)
            .map(|(def, series)| {
                (
                    &def.group,
                    RowView {
                        id: format!("c.{}", def.key),
                        label: def.label.to_string(),
                        unit: def.unit.to_string(),
                        kind: def.kind,
                        local: true,
                        current: series.current(),
                        text: None,
                        history: series.history(),
                        stale: false,
                    },
                )
            })
            .collect()
    }
}
