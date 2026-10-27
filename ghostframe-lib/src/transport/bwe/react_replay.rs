//! D1 design A, section 6.6: how fast does goog_cc cut a stale estimate when
//! the sender resumes after an idle on a link that lost capacity meanwhile,
//! with the model controller's window bounding what is in flight?
//!
//! goog_cc is driven as a pure state machine (`GoogCcDriver`, the same driver
//! production uses) with feedback from a fluid model of the bottleneck: a FIFO
//! served at `C`, a fixed propagation delay, a 5 ms acknowledgement flush. The
//! sender is the admission bucket at the estimate, held to the window
//! `model_window(R_w, min_rtt + max_ack_delay, G)`. Nothing is random and no
//! clock is read, so the result is exact.
//!
//! The design's `T_react` bounds (section 6.6) are hand arithmetic on the
//! trendline's constants; this test is what settles them, and in particular
//! whether the detector still sees a queue the window has flattened.

use super::googcc::GoogCcDriver;
use super::AckArrival;
use crate::transport::model_controller::model_window;
use crate::transport::rtt_estimator::MAX_ACK_DELAY;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MTU: u64 = 1200;
/// The link after the idle: 2 Mbit/s.
const C_BPS: f64 = 250_000.0;
const PROP_ONE_WAY_US: u64 = 15_000;
const BASE_RTT: Duration = Duration::from_millis(30);
const FLUSH_US: u64 = 5_000;
/// How long the pre-idle flow runs at `k x C` before the idle.
const WARM_UP_US: u64 = 6_000_000;

/// One datagram in the fluid model.
struct Flight {
    emit_us: u64,
    arrive_us: u64,
    ack_at_us: u64,
}

struct Outcome {
    /// Virtual time from resume to the first estimate under 0.9 x the held
    /// rate, if there was one before the run ended.
    t_react_us: Option<u64>,
    /// Largest standing queue at the bottleneck after the resume, in us of drain.
    peak_queue_us: u64,
    /// The highest estimate published after the resume, bits per second.
    peak_rate_bps: u64,
    /// The estimate at the resume, bits per second: the held rate.
    held_bps: u64,
    /// Estimate at the end, bits per second.
    final_bps: u64,
    /// Datagrams the sender sent after the resume (premise: it did send).
    sent_after_resume: u64,
}

/// Run the pre-idle flow on a `k x C` link until the estimate has settled
/// there, idle for `idle_us`, then resume on a `C` link at the held estimate
/// with window gain `gain` until the first feedback and `2` after it.
fn run(k: f64, idle_us: u64, restart_gain: f64) -> Outcome {
    let t0 = Instant::now();
    let held_bps_target = (k * C_BPS * 8.0) as u64;
    let mut d = GoogCcDriver::new(held_bps_target, t0);
    d.note_path_rtt(BASE_RTT);

    let mut flights: VecDeque<Flight> = VecDeque::new();
    let (mut granted, mut spent, mut offered) = (0.0f64, 0.0f64, 0.0f64);
    let mut tokens = 0.0f64;
    let mut in_flight: u64 = 0;
    let mut last_dep_us: u64 = 0;
    let mut next_sample_us = 0u64;
    let mut cap = k * C_BPS;

    let mut now_us = 0u64;
    let end_warm = WARM_UP_US;
    let resume_us = end_warm + idle_us;
    let run_until = resume_us + 6_000_000;
    let mut held_bps = 0u64;
    let mut resumed = false;
    let mut feedback_since_resume = false;
    let mut t_react_us = None;
    let mut peak_queue_us = 0u64;
    let mut sent_after_resume = 0u64;
    let mut srtt = BASE_RTT;
    let mut peak_rate = 0u64;

    while now_us < run_until {
        now_us += 1_000;
        let now = t0 + Duration::from_micros(now_us);

        if !resumed && now_us >= resume_us {
            // The idle is over: capacity has dropped to C, nothing is in
            // flight, the held estimate is what goog_cc last published.
            resumed = true;
            held_bps = d.snapshot().bitrate_bps;
            cap = C_BPS;
            last_dep_us = now_us;
            tokens = 0.0;
        }
        let idle = now_us >= end_warm && now_us < resume_us;

        // Acknowledgements reaching the sender this millisecond, in one batch
        // per flush instant (the client flushes every 5 ms). The idle stops
        // the sending, not the feedback of what is already in flight.
        {
            let mut batch: Vec<AckArrival> = Vec::new();
            while flights.front().is_some_and(|f| f.ack_at_us <= now_us) {
                let f = flights.pop_front().expect("checked");
                in_flight = in_flight.saturating_sub(MTU);
                batch.push(AckArrival {
                    server_emit_us: f.emit_us,
                    client_arrival_ms_lo16: ((f.arrive_us / 1_000) & 0xFFFF) as u16,
                    size_bytes: MTU as u32,
                    probe: None,
                });
                srtt = Duration::from_micros(f.ack_at_us - f.emit_us);
            }
            if !batch.is_empty() {
                d.note_path_rtt(srtt);
                d.note_offered(now, offered);
                d.note_admission(now, granted, spent);
                d.update(&batch, &[], now, in_flight as usize);
                if resumed {
                    feedback_since_resume = true;
                }
            }
        }

        let snap = d.snapshot();
        if resumed && t_react_us.is_none() && (snap.bitrate_bps as f64) < 0.9 * held_bps as f64 {
            t_react_us = Some(now_us - resume_us);
        }
        if resumed {
            let queue_us = last_dep_us.saturating_sub(now_us);
            peak_queue_us = peak_queue_us.max(queue_us);
        }
        if now_us >= next_sample_us {
            next_sample_us = now_us + 1_000;
        }
        if idle {
            continue;
        }

        // The sender: admission at the estimate, then the window.
        let rate_bytes = snap.bitrate_bps as f64 / 8.0;
        tokens = (tokens + rate_bytes * 0.001).min(rate_bytes * 0.010 + MTU as f64);
        granted += rate_bytes * 0.001;
        let gain = if resumed && !feedback_since_resume {
            restart_gain
        } else {
            2.0
        };
        let w = model_window(rate_bytes as u64, BASE_RTT + MAX_ACK_DELAY, gain, MTU);
        if resumed {
            peak_rate = peak_rate.max(snap.bitrate_bps);
        }
        while tokens >= MTU as f64 && in_flight + MTU <= w {
            tokens -= MTU as f64;
            in_flight += MTU;
            spent += MTU as f64;
            offered += MTU as f64;
            if resumed {
                sent_after_resume += 1;
            }
            let dep = last_dep_us.max(now_us) + (MTU as f64 / cap * 1e6) as u64;
            last_dep_us = dep;
            let arrive = dep + PROP_ONE_WAY_US;
            let ack_at = (arrive.div_ceil(FLUSH_US)) * FLUSH_US + PROP_ONE_WAY_US;
            flights.push_back(Flight {
                emit_us: now_us,
                arrive_us: arrive,
                ack_at_us: ack_at,
            });
        }
    }

    Outcome {
        t_react_us,
        peak_queue_us,
        peak_rate_bps: peak_rate,
        held_bps,
        final_bps: d.snapshot().bitrate_bps,
        sent_after_resume,
    }
}

/// Section 6.6's `T_react` upper bounds for resuming at the held rate, with
/// the window gain `G = 1` of a restart: warm 0.29 s (`k = 10`) and 0.14 s
/// (`k = 4`); cold 0.41 s and 0.20 s. The test asserts them doubled.
fn bound_us(k: f64, cold: bool) -> u64 {
    let (warm, cold_b) = if k > 6.0 {
        (290_000, 410_000)
    } else {
        (140_000, 200_000)
    };
    2 * if cold { cold_b } else { warm }
}

fn check(k: f64, idle_us: u64) {
    let cold = idle_us >= 2_000_000;
    let o = run(k, idle_us, 1.0);
    eprintln!(
        "REPLAY k={k} idle={}s held={} bps t_react={:?} ms peak_queue={} ms final={} bps sent={}",
        idle_us / 1_000_000,
        o.held_bps,
        o.t_react_us.map(|t| t / 1000),
        o.peak_queue_us / 1000,
        o.final_bps,
        o.sent_after_resume
    );
    assert!(
        o.sent_after_resume > 100,
        "premise: the sender resumed and sent ({} datagrams)",
        o.sent_after_resume
    );
    // The window bounds the queue: with `W = G x R x RTT_eff` in flight and a
    // bandwidth-delay product of `C x RTT_eff` (the flush and ack path
    // included), the queue is `(G x R / C - 1) x RTT_eff`, plus the two flush
    // intervals the batching can hide. `R` is the highest estimate published,
    // not the held one: the estimate rises before it is cut (6.8 -> 9.1 Mbit
    // at k = 4), and the window follows it.
    let rtt_eff_s = (BASE_RTT + MAX_ACK_DELAY).as_secs_f64();
    let r_peak = o.peak_rate_bps as f64 / 8.0;
    let queue_bound_us = ((2.0 * r_peak / C_BPS - 1.0) * rtt_eff_s * 1e6) as u64 + 2 * FLUSH_US;
    assert!(
        o.peak_queue_us <= queue_bound_us,
        "k={k}: queue {} ms exceeds the window's bound {} ms",
        o.peak_queue_us / 1000,
        queue_bound_us / 1000
    );
    assert!(
        o.peak_queue_us > 50_000,
        "premise: the stale estimate built a queue ({} ms)",
        o.peak_queue_us / 1000
    );
    let t = o.t_react_us.unwrap_or_else(|| {
        panic!(
            "k={k} idle={idle_us}: the estimate was never cut: {o:?}",
            o = o.final_bps
        )
    });
    assert!(
        t <= bound_us(k, cold),
        "k={k} cold={cold}: T_react {} ms exceeds the doubled section 6.6 bound {} ms",
        t / 1000,
        bound_us(k, cold) / 1000
    );
}

#[test]
fn a_warm_idle_at_k10_is_cut_within_the_bound() {
    check(10.0, 1_000_000);
}

#[test]
fn a_warm_idle_at_k4_is_cut_within_the_bound() {
    check(4.0, 1_000_000);
}

#[test]
fn a_cold_idle_at_k10_is_cut_within_the_bound() {
    check(10.0, 3_000_000);
}

#[test]
fn a_cold_idle_at_k4_is_cut_within_the_bound() {
    check(4.0, 3_000_000);
}
