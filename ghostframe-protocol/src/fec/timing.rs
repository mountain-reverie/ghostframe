//! The recovery window as a function of what the server advertises.
//!
//! The server states two things in frames it already sends: the block's span
//! `T_adv` (a 3-bit code in SOURCE flags bits 4-6) and a hint at the round
//! trip (a 4-bit code in the high nibble of the REPAIR scheme byte). Client and
//! server both derive their deadlines from the functions below, so there is one
//! home for the arithmetic and nothing to mirror.
//!
//! No `pub const` lives here on purpose: every `pub const` in this crate lands
//! in the generated C header. The numbers are private; functions are exposed.

/// `T_adv` in microseconds by code. Code 0 means "nothing advertised": the span
/// the client assumed before the code existed.
const T_ADV_US: [u64; 8] = [
    33_000, 10_000, 15_000, 20_000, 25_000, 40_000, 50_000, 66_000,
];

/// RTT hint in microseconds by code 1..=15 (index 0 is "unknown"): sqrt 2 per
/// step from 4 ms, rounded up. Code 15 saturates at 512 ms.
const RTT_HINT_US: [u64; 16] = [
    0, 4_000, 5_657, 8_000, 11_314, 16_000, 22_627, 32_000, 45_255, 64_000, 90_510, 128_000,
    181_019, 256_000, 362_039, 512_000,
];

/// Time after the span that a late source may still arrive.
const ALLOWANCE_US: u64 = 10_000;
/// Added to the round-trip hint for the requested row to come back. Derived
/// from E11 section 10.4: the requested row returned up to 24 ms later than
/// the round trip under bursts; 30 ms is that plus 25 %.
const REQUEST_MARGIN_US: u64 = 30_000;
/// A frame older than this is superseded: two intervals at 30 fps.
const H264_LIFE_US: u64 = 66_000;

/// Code of the smallest span at least `span_us` (the largest code saturates).
/// Code 0 (33 ms) takes its place in the order of the spans, not in code order.
pub fn t_adv_code_for(span_us: u64) -> u8 {
    let mut best: Option<(u64, u8)> = None;
    for (code, &us) in T_ADV_US.iter().enumerate() {
        if us >= span_us && best.is_none_or(|(b, _)| us < b) {
            best = Some((us, code as u8));
        }
    }
    best.map_or(7, |(_, c)| c)
}

/// Span of a code; a code above 7 reads as 0 (nothing advertised).
pub fn t_adv_us(code: u8) -> u64 {
    T_ADV_US
        .get(usize::from(code))
        .copied()
        .unwrap_or(T_ADV_US[0])
}

/// Smallest hint code (1..=15) at least `rtt_us`; 15 saturates.
pub fn rtt_hint_code_for(rtt_us: u64) -> u8 {
    (1..16u8)
        .find(|&c| RTT_HINT_US[usize::from(c)] >= rtt_us)
        .unwrap_or(15)
}

/// Round-trip hint of a code; 0 is "unknown" and so is a code above 15.
pub fn rtt_hint_us(code: u8) -> u64 {
    RTT_HINT_US.get(usize::from(code)).copied().unwrap_or(0)
}

/// How long after a block's first arrival the client waits before it asks for
/// a row: the span plus the allowance.
pub fn first_window_us(t_adv_code: u8) -> u64 {
    t_adv_us(t_adv_code) + ALLOWANCE_US
}

/// How long after the request the client waits for the row. No hint: the
/// window of a client that was told nothing.
pub fn request_round_us(hint_code: u8) -> u64 {
    match rtt_hint_us(hint_code) {
        0 => first_window_us(0),
        h => h + REQUEST_MARGIN_US,
    }
}

/// An H.264 block is declined without a request when the round trip is longer
/// than the frame's life: the row would arrive after the frame is superseded.
pub fn h264_skips_request(hint_code: u8) -> bool {
    rtt_hint_us(hint_code) > H264_LIFE_US
}

/// The allowance after a block's span.
pub const fn allowance_us() -> u64 {
    ALLOWANCE_US
}

/// The span assumed when nothing is advertised (code 0).
pub const fn default_t_adv_us() -> u64 {
    T_ADV_US[0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Spans in increasing order, with the code that carries each.
    fn spans_in_order() -> Vec<(u64, u8)> {
        let mut v: Vec<(u64, u8)> = (0..8u8).map(|c| (t_adv_us(c), c)).collect();
        v.sort();
        v
    }

    #[test]
    fn the_span_table_is_strictly_increasing_apart_from_code_zero() {
        let v: Vec<u64> = (1..8u8).map(t_adv_us).collect();
        assert!(v.windows(2).all(|w| w[0] < w[1]), "{v:?}");
        // Code 0 sits between codes 4 and 5, which is the documented place.
        assert!(t_adv_us(4) < t_adv_us(0) && t_adv_us(0) < t_adv_us(5));
        let s = spans_in_order();
        assert!(s.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn the_hint_table_is_strictly_increasing() {
        let v: Vec<u64> = (1..16u8).map(rtt_hint_us).collect();
        assert!(v.windows(2).all(|w| w[0] < w[1]), "{v:?}");
        assert_eq!(rtt_hint_us(0), 0);
    }

    proptest! {
        /// The code names the smallest span that is at least what was asked.
        #[test]
        fn t_adv_code_is_the_smallest_entry_at_least_the_span(x in 0u64..80_000) {
            let c = t_adv_code_for(x);
            prop_assert!(c < 8);
            if x <= t_adv_us(7) {
                prop_assert!(t_adv_us(c) >= x, "rounded down: {x} -> {}", t_adv_us(c));
                for (us, _) in spans_in_order() {
                    if us >= x {
                        prop_assert!(t_adv_us(c) <= us, "{x}: code {c} is not the smallest");
                    }
                }
            } else {
                prop_assert_eq!(c, 7);
            }
        }

        /// Never under-reports (up to the top code); over-reports by at most
        /// one step of x sqrt 2.
        #[test]
        fn rtt_hint_never_under_reports_and_over_reports_boundedly(x in 1u64..600_000) {
            let c = rtt_hint_code_for(x);
            prop_assert!((1..16).contains(&c));
            let h = rtt_hint_us(c);
            if x <= rtt_hint_us(15) {
                prop_assert!(h >= x, "under-reported {x} as {h}");
                // sqrt 2 per step, plus a step of slack for the smallest entry.
                let bound = (x as f64 * std::f64::consts::SQRT_2).ceil() as u64 + 1;
                prop_assert!(h <= bound.max(rtt_hint_us(1)), "{x} reported as {h}");
            } else {
                prop_assert_eq!(c, 15);
            }
        }
    }

    #[test]
    fn the_window_functions_at_code_zero_are_the_unadvertised_values() {
        assert_eq!(first_window_us(0), 43_000);
        assert_eq!(request_round_us(0), 43_000);
        assert_eq!(default_t_adv_us(), 33_000);
        assert_eq!(allowance_us(), 10_000);
    }

    #[test]
    fn the_round_grows_with_the_hint_and_the_window_with_the_span() {
        let r: Vec<u64> = (1..16u8).map(request_round_us).collect();
        assert!(r.windows(2).all(|w| w[0] < w[1]));
        let hint_us = rtt_hint_us(5);
        assert_eq!(request_round_us(5), hint_us + 30_000);
        assert!(first_window_us(1) < first_window_us(0) && first_window_us(0) < first_window_us(7));
    }

    #[test]
    fn the_h264_skip_starts_above_the_frame_life() {
        assert!(!h264_skips_request(0));
        assert!(!h264_skips_request(9));
        assert!(h264_skips_request(10));
        assert!(h264_skips_request(15));
    }
}
