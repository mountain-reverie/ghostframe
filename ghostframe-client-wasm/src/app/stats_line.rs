// SPDX-License-Identifier: MIT OR Apache-2.0
//! The page's ~2 s log summary (port of `stats_log.ts`): per-tick drain
//! deltas, framebuffer size, CDF 5/3 coverage, NACKs sent. Identical
//! snapshots are dropped: silence means nothing changed. Pure; the browser
//! side gathers `StatsInputs` and logs what `on_tick` returns.

use ghostframe_protocol::codec::cdf53::CDF53_PASS_COUNT;

use crate::app::session::Drained;

/// The line fires only when more than this has passed since the last one.
pub const STATS_INTERVAL_MS: f64 = 2000.0;

/// What one `AfterPaint` tells the stats line, plus what it reads elsewhere.
pub struct StatsInputs<'a> {
    pub now_ms: f64,
    pub drained: Drained,
    pub raf_ticks: u32,
    pub submit_count: u32,
    pub fb: (u32, u32),
    pub cdf53_fails: u32,
    pub cdf53_last: Option<u8>,
    pub nack_sent: u32,
    /// `core.cdf53_coverage_summary()`'s preformatted line and the two counts.
    pub coverage_line: &'a str,
    pub partial: u64,
    pub gave_up: u64,
    /// The worst incomplete tiles (only read when `partial` or `gave_up`).
    pub incomplete: &'a [(u8, u8, u16, u32)], // (tile_x, tile_y, missing, sweep_attempts)
}

/// The ~2 s page-log summary. `on_tick` returns the lines to log, or `None`
/// when the interval has not passed or nothing a reader cares about changed.
#[derive(Debug, Default)]
pub struct StatsLine {
    totals: Drained,
    last: Drained,
    last_ms: f64,
    last_key: String,
}

fn add(a: u32, b: u32) -> u32 {
    a.wrapping_add(b)
}

impl StatsLine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_tick(&mut self, i: &StatsInputs<'_>) -> Option<Vec<String>> {
        self.totals = Drained {
            solid: add(self.totals.solid, i.drained.solid),
            palrle: add(self.totals.palrle, i.drained.palrle),
            cdf53: add(self.totals.cdf53, i.drained.cdf53),
            h264: add(self.totals.h264, i.drained.h264),
        };
        if i.now_ms - self.last_ms <= STATS_INTERVAL_MS {
            return None;
        }
        self.last_ms = i.now_ms;
        let d = Drained {
            solid: self.totals.solid.wrapping_sub(self.last.solid),
            palrle: self.totals.palrle.wrapping_sub(self.last.palrle),
            cdf53: self.totals.cdf53.wrapping_sub(self.last.cdf53),
            h264: self.totals.h264.wrapping_sub(self.last.h264),
        };
        self.last = self.totals;

        // The key excludes `raf` (always changes).
        let key = format!(
            "drain:{}/{}/{}/{}|fb:{}x{}|fails:{}|nack:{}",
            d.solid, d.palrle, d.cdf53, d.h264, i.fb.0, i.fb.1, i.cdf53_fails, i.nack_sent
        );
        if key == self.last_key {
            return None;
        }
        self.last_key = key;

        let last = i
            .cdf53_last
            .map_or_else(|| "-".to_string(), |c| c.to_string());
        let mut lines = vec![
            format!(
                "stats: \u{394}drain{{s:{} p:{} c:{} h:{}}} fb:{}x{} cdf53fails:{}(last={}) raf:{} submit:{}",
                d.solid,
                d.palrle,
                d.cdf53,
                d.h264,
                i.fb.0,
                i.fb.1,
                i.cdf53_fails,
                last,
                i.raf_ticks,
                i.submit_count
            ),
            format!("cdf53-coverage: {}", i.coverage_line),
        ];
        if i.partial > 0 || i.gave_up > 0 {
            let tiles: Vec<String> = i
                .incomplete
                .iter()
                .map(|(x, y, missing, sweeps)| {
                    format!(
                        "({x},{y}) missing=0b{missing:0>width$b} sweeps={sweeps}",
                        width = CDF53_PASS_COUNT
                    )
                })
                .collect();
            lines.push(format!("cdf53-incomplete: {}", tiles.join(" ")));
        }
        lines.push(format!("nack_sent={}", i.nack_sent));
        Some(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(now_ms: f64) -> StatsInputs<'static> {
        StatsInputs {
            now_ms,
            drained: Drained::default(),
            raf_ticks: 7,
            submit_count: 3,
            fb: (1920, 1080),
            cdf53_fails: 0,
            cdf53_last: None,
            nack_sent: 0,
            coverage_line: "tiles=0",
            partial: 0,
            gave_up: 0,
            incomplete: &[],
        }
    }

    #[test]
    fn nothing_before_two_seconds() {
        let mut s = StatsLine::new();
        assert_eq!(s.on_tick(&inputs(1999.0)), None);
        assert_eq!(s.on_tick(&inputs(2000.0)), None, "exactly 2 s is not more");
    }

    #[test]
    fn the_first_line_after_two_seconds_names_the_drain_since_the_last() {
        let mut s = StatsLine::new();
        let mut i = inputs(500.0);
        i.drained = Drained {
            solid: 1,
            palrle: 2,
            cdf53: 3,
            h264: 4,
        };
        assert_eq!(s.on_tick(&i), None);
        let mut i = inputs(2500.0);
        i.drained = Drained {
            solid: 10,
            palrle: 0,
            cdf53: 0,
            h264: 1,
        };
        let lines = s.on_tick(&i).expect("a line");
        assert_eq!(
            lines[0],
            "stats: \u{394}drain{s:11 p:2 c:3 h:5} fb:1920x1080 cdf53fails:0(last=-) raf:7 submit:3"
        );
        assert_eq!(lines[1], "cdf53-coverage: tiles=0");
        assert_eq!(lines.last().unwrap(), "nack_sent=0");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn an_unchanged_line_is_suppressed() {
        let mut s = StatsLine::new();
        assert!(s.on_tick(&inputs(2500.0)).is_some());
        // Only raf and time moved.
        let mut i = inputs(5000.0);
        i.raf_ticks = 400;
        assert_eq!(s.on_tick(&i), None);
        // A NACK is a change.
        let mut i = inputs(7600.0);
        i.nack_sent = 1;
        assert!(s.on_tick(&i).is_some());
    }

    #[test]
    fn incomplete_tiles_are_listed_with_their_missing_mask() {
        let mut s = StatsLine::new();
        let tiles = [(3u8, 4u8, 0b101u16, 2u32), (0, 1, 1, 0)];
        let mut i = inputs(2500.0);
        i.partial = 2;
        i.incomplete = &tiles;
        let lines = s.on_tick(&i).expect("a line");
        let w = CDF53_PASS_COUNT;
        let want = format!(
            "cdf53-incomplete: (3,4) missing=0b{:0>w$} sweeps=2 (0,1) missing=0b{:0>w$} sweeps=0",
            "101", "1"
        );
        assert_eq!(lines[2], want);
        assert_eq!(lines.len(), 4);
        // With a five-pass count the text would be `0b00101`.
        assert!(format!("{:0>5b}", 0b101u16) == "00101");
        // Not listed when nothing is partial.
        let mut s = StatsLine::new();
        let mut i = inputs(2500.0);
        i.incomplete = &tiles;
        assert_eq!(s.on_tick(&i).unwrap().len(), 3);
    }

    #[test]
    fn a_missing_last_fail_code_prints_a_dash() {
        let mut s = StatsLine::new();
        let mut i = inputs(2500.0);
        i.cdf53_fails = 2;
        i.cdf53_last = Some(9);
        assert!(s.on_tick(&i).unwrap()[0].contains("cdf53fails:2(last=9)"));
        let mut s = StatsLine::new();
        i.cdf53_last = None;
        assert!(s.on_tick(&i).unwrap()[0].contains("cdf53fails:2(last=-)"));
    }
}
