// SPDX-License-Identifier: MIT OR Apache-2.0
//! Number and row formatting for the stats panel. Plain Rust: no DOM, so it
//! is tested with `cargo test` on the host.

use ghostframe_client_core::{MetricKind, RowView};

/// Shown where there is no value: stale, not yet measured, or not a number.
pub const DASH: &str = "\u{2014}";

const PREFIXES: [&str; 5] = ["", "k", "M", "G", "T"];

/// Units that are not scaled with SI prefixes (`ms` has its own rule, `s` is
/// a duration).
const UNPREFIXED: [&str; 5] = ["ms", "s", "%", "fps", "Hz"];

/// `v` to three significant digits, trailing zeros dropped: `42.1`, `18.4`,
/// `1.5`, `12`, `123`, `0.0123`. A value that rounds up a decade (`9.996`)
/// prints `10`: the extra zero the rounding adds is one of the trailing zeros
/// that are dropped.
pub fn sig3(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        return "0".to_string();
    }
    let a = v.abs();
    let sign = if v < 0.0 { "-" } else { "" };
    let mag = a.log10().floor() as i32;
    let decimals = (2 - mag).max(0) as usize;
    let text = format!("{:.*}", decimals, a);
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        &text
    };
    format!("{sign}{text}")
}

/// What `sig3` would print, as a number: used to see a carry coming.
fn rounded_sig3(v: f64) -> f64 {
    sig3(v).parse().unwrap_or(v)
}

/// Scale `v` into `[1, 1000)` and name the SI prefix: `42_100_000` is
/// `(42.1, "M")`. Stops at tera.
fn si_scale(v: f64) -> (f64, &'static str) {
    let mut scaled = v;
    let mut e = 0;
    while e + 1 < PREFIXES.len() && (scaled.abs() >= 1000.0 || rounded_sig3(scaled).abs() >= 1000.0)
    {
        scaled /= 1000.0;
        e += 1;
    }
    (scaled, PREFIXES[e])
}

/// `1h 02m`, `3m 07s`, `12 s`: an uptime, not a measurement.
pub fn format_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return DASH.to_string();
    }
    let s = secs as u64;
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        3600..=86_399 => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d {:02}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// `v` in `unit` for display. A counter's `v` is already a per-second rate
/// (`ClientStats::view` computed it), so it gets a `/s`.
pub fn format_value(v: f64, unit: &str, kind: &MetricKind) -> String {
    if !v.is_finite() {
        return DASH.to_string();
    }
    let per_second = matches!(kind, MetricKind::Counter);
    if unit == "s" && !per_second {
        return format_duration(v);
    }
    let (number, suffix) = if unit == "ms" {
        if rounded_sig3(v).abs() >= 1000.0 {
            (sig3(v / 1000.0), "s".to_string())
        } else {
            (sig3(v), "ms".to_string())
        }
    } else if UNPREFIXED.contains(&unit) {
        (sig3(v), unit.to_string())
    } else {
        let (scaled, prefix) = si_scale(v);
        // UCUM spells bytes `By`; people write `B`. `1` is a bare count.
        let base = match unit {
            "1" => "",
            "By" => "B",
            other => other,
        };
        (sig3(scaled), format!("{prefix}{base}"))
    };
    let suffix = if per_second {
        format!("{suffix}/s")
    } else {
        suffix
    };
    if suffix.is_empty() {
        number
    } else {
        format!("{number} {suffix}")
    }
}

/// The text a metric row shows for its value.
pub fn format_row(row: &RowView) -> String {
    if row.stale {
        return DASH.to_string();
    }
    match row.kind {
        MetricKind::Text => row.text.clone().unwrap_or_else(|| DASH.to_string()),
        MetricKind::Gauge | MetricKind::Counter => row
            .current
            .map(|v| format_value(v, &row.unit, &row.kind))
            .unwrap_or_else(|| DASH.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(v: f64, unit: &str) -> String {
        format_value(v, unit, &MetricKind::Gauge)
    }
    fn c(v: f64, unit: &str) -> String {
        format_value(v, unit, &MetricKind::Counter)
    }

    #[test]
    fn three_significant_digits() {
        assert_eq!(sig3(42.1234), "42.1");
        assert_eq!(sig3(18.4), "18.4");
        assert_eq!(sig3(1.5), "1.5");
        assert_eq!(sig3(123.456), "123");
        assert_eq!(sig3(0.01234), "0.0123");
        assert_eq!(sig3(0.0), "0");
        assert_eq!(sig3(-5.0), "-5");
    }

    #[test]
    fn rounding_up_a_decade_keeps_three_digits() {
        assert_eq!(sig3(9.996), "10");
        assert_eq!(sig3(99.96), "100");
    }

    #[test]
    fn si_prefixes_on_rates_and_sizes() {
        assert_eq!(g(42_100_000.0, "bit/s"), "42.1 Mbit/s");
        assert_eq!(g(950.0, "bit/s"), "950 bit/s");
        assert_eq!(g(1_234_567.0, "By"), "1.23 MB");
        assert_eq!(g(2_500_000_000.0, "bit/s"), "2.5 Gbit/s");
    }

    #[test]
    fn a_value_that_rounds_up_to_a_thousand_takes_the_next_prefix() {
        assert_eq!(g(999_960.0, "bit/s"), "1 Mbit/s");
    }

    #[test]
    fn milliseconds_switch_to_seconds_at_a_thousand() {
        assert_eq!(g(18.4, "ms"), "18.4 ms");
        assert_eq!(g(0.0, "ms"), "0 ms");
        assert_eq!(g(1500.0, "ms"), "1.5 s");
        assert_eq!(g(999.96, "ms"), "1 s");
    }

    #[test]
    fn counters_are_per_second_rates() {
        assert_eq!(c(12.0, "1"), "12 /s");
        assert_eq!(c(1500.0, "1"), "1.5 k/s");
        assert_eq!(c(0.0, "1"), "0 /s");
        assert_eq!(c(2048.0, "By"), "2.05 kB/s");
    }

    #[test]
    fn plain_counts_have_no_unit() {
        assert_eq!(g(3.0, "1"), "3");
        assert_eq!(g(1500.0, "1"), "1.5 k");
    }

    #[test]
    fn durations_read_as_uptime() {
        assert_eq!(g(7.0, "s"), "7 s");
        assert_eq!(g(187.0, "s"), "3m 07s");
        assert_eq!(g(3723.0, "s"), "1h 02m");
        assert_eq!(g(90_000.0, "s"), "1d 01h");
    }

    #[test]
    fn non_finite_values_show_a_dash() {
        assert_eq!(g(f64::NAN, "ms"), DASH);
        assert_eq!(c(f64::INFINITY, "1"), DASH);
    }

    fn row(kind: MetricKind, current: Option<f64>, text: Option<&str>, stale: bool) -> RowView {
        RowView {
            id: "s1.1".to_string(),
            label: "x".to_string(),
            unit: "ms".to_string(),
            kind,
            local: false,
            current,
            text: text.map(str::to_string),
            history: vec![],
            stale,
        }
    }

    #[test]
    fn rows_show_a_dash_when_stale_or_empty() {
        assert_eq!(
            format_row(&row(MetricKind::Gauge, Some(18.4), None, false)),
            "18.4 ms"
        );
        assert_eq!(
            format_row(&row(MetricKind::Gauge, Some(18.4), None, true)),
            DASH
        );
        assert_eq!(format_row(&row(MetricKind::Gauge, None, None, false)), DASH);
        assert_eq!(
            format_row(&row(MetricKind::Text, None, Some("h264"), false)),
            "h264"
        );
        assert_eq!(format_row(&row(MetricKind::Text, None, None, false)), DASH);
        assert_eq!(
            format_row(&row(MetricKind::Text, None, Some("h264"), true)),
            DASH
        );
    }
}
