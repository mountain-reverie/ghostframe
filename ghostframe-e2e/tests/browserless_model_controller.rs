//! D1 PR-8 (`transport/model-controller`, design A): the scene gates.
//!
//! Each scene runs the model controller (the only controller) and prints its
//! numbers as `P8 <scene> <name> <value>` before it asserts. `GF_P8_SEED` is
//! XORed into every seed for sweeps.
//!
//! * **P8a, probes.** On the startup map, every probe cluster that had backlog
//!   put at least 0.9 of its target on the way, and a probe completed in every
//!   cell.
//! * **P8b, drop-tail 10 Mbit, 200 ms buffer.** The settled estimate sits in
//!   the AIMD band, and the bottleneck queue never exceeds what the window
//!   allows.
//! * **P8c, feedback blackout.** Two seconds of no client-to-server packets.
//!   Bytes in flight stay inside the window, the queue stays bounded, and the
//!   session finds the link again within `T_up`.
//! * **P8d, capacity drop during an idle.** 20 -> 2 Mbit/s inside an 8 s idle.
//!   No spurious retransmission, and the queue the restart builds is bounded.
//!
//! Run: `cargo test --test browserless_model_controller -- --nocapture
//! --test-threads=1`

use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, FrameScript, LinkOverrides,
    RunOptions, SceneLoad,
};
use ghostframe_e2e::harness::cc_probe as cc;
use ghostframe_e2e::harness::load_profile::{
    gradient_tile, Churn, LoadProfile, PRODUCTION_CADENCE_US,
};
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, NetProfile};
use ghostframe_lib::config::ModelSettings;
use ghostframe_lib::transport::model_controller::{model_window, RTT_WINDOW};
use std::time::Duration;

const S: u64 = 1_000_000;
/// Bytes per second in one Mbit/s.
const MBIT: u64 = 125_000;
const OVERDRIVE: f64 = 1.3;
/// quinn's `max_ack_delay` as this server counts it (`rtt_estimator::MAX_ACK_DELAY`).
const MAX_ACK_DELAY_US: u64 = 5_000;
/// The host FIFO's allowance in the design's queue bounds (`T_q`, 10 ms).
const T_Q_US: u64 = 10_000;
/// One wire datagram, the window's floor unit.
const MTU: u64 = 1_200;
/// `T_up` of P6b: two `MIN_TIME_BETWEEN_ALR_PROBES` and a second.
const T_UP_US: u64 = 11 * S;

fn seed_xor() -> u64 {
    std::env::var("GF_P8_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn p8(scene: &str, name: &str, val: impl std::fmt::Display) {
    println!("P8 {scene} {name} {val}");
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or("none".to_string(), |x| x.to_string())
}

/// Tiles per 33.3 ms tick that offer about `offered_mbit` (slope measured by
/// `browserless_nested_cc::calibrate_offered_load`).
fn tiles_for(offered_mbit: f64) -> usize {
    (offered_mbit / 0.30).ceil() as usize
}

fn region(tiles_per_tick: usize) -> LoadProfile {
    LoadProfile {
        cadence_us: PRODUCTION_CADENCE_US,
        churn: Churn::Region { tiles_per_tick },
    }
}

/// Everything about a run but its frames.
struct Spec {
    seed: u64,
    net: NetProfile,
    /// The client-to-server link, when it differs from a perfect one.
    c2s: Option<NetProfile>,
    secs: u64,
    grid: (u8, u8),
    classical_kx: bool,
    model: Option<ModelSettings>,
}

impl Spec {
    fn new(seed: u64, net: NetProfile, secs: u64) -> Self {
        Self {
            seed,
            net,
            c2s: None,
            secs,
            grid: (16, 16),
            classical_kx: false,
            model: None,
        }
    }
}

async fn run_frames(spec: Spec, frames: Vec<FrameScript>) -> BrowserlessResult {
    let scene = BrowserlessScene {
        seed: spec.seed ^ seed_xor(),
        tick_budget_floor_bytes: None,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(frames),
        cadence_us: PRODUCTION_CADENCE_US,
        net: spec.net,
        drops: Default::default(),
        duration: Duration::from_secs(spec.secs),
        grid_cols: spec.grid.0 as u32,
        grid_rows: spec.grid.1 as u32,
    };
    run_browserless_with_options(
        scene,
        LinkOverrides {
            c2s: spec.c2s,
            ..Default::default()
        },
        RunOptions {
            classical_kx: spec.classical_kx,
            model: spec.model,
            ..Default::default()
        },
    )
    .await
    .expect("scene ran")
}

fn median(s: &[(u64, u64)], from: u64, to: u64) -> Option<u64> {
    cc::median_bps(s, from, to)
}

/// Bottleneck queue wait of the datagrams offered in `[from_us, to_us)`:
/// their transit less the propagation delay.
fn queue_wait_us(r: &BrowserlessResult, prop_us: u64, from_us: u64, to_us: u64) -> Vec<u64> {
    r.s2c_transit_samples
        .iter()
        .filter(|(t, _)| *t >= from_us && *t < to_us)
        .map(|(_, transit)| transit.saturating_sub(prop_us))
        .collect()
}

fn peak(v: &[u64]) -> u64 {
    v.iter().copied().max().unwrap_or(0)
}

/// The payload fraction the run measured (countable bytes per billed byte).
fn payload_fraction(r: &BrowserlessResult) -> f64 {
    let f = r.transport.instr.bwe.payload_fraction_ppm as f64 / 1e6;
    assert!(f > 0.5 && f <= 1.0, "payload fraction {f}");
    f
}

fn report_common(scene: &str, r: &BrowserlessResult) {
    let i = &r.transport.instr;
    p8(scene, "final_est_bps", r.bwe_estimate_bps);
    p8(
        scene,
        "probes",
        format!(
            "requested={} done={} abandoned={} windows_closed={}",
            i.bwe.probe_requests, r.probes_completed, r.probes_abandoned, i.probe_windows_closed
        ),
    );
    let n_win = (i.probe_windows_closed as usize).min(i.probe_windows.len());
    for (k, w) in i.probe_windows[..n_win].iter().enumerate() {
        p8(
            scene,
            &format!("probe_window{k}"),
            format!(
                "target_bps={} bytes={} starved={} fill={:.3}",
                w.target_bps,
                w.bytes_sent,
                w.starved,
                w.fill()
            ),
        );
    }
    p8(
        scene,
        "budget_terms",
        format!(
            "pacer={} floor={} probe={}",
            i.budget.pacer, i.budget.floor, i.budget.probe
        ),
    );
    p8(
        scene,
        "model",
        format!(
            "{:?} restarts hold={} cold={}",
            i.model.stats, i.model.restarts_hold, i.model.restarts_cold
        ),
    );
    p8(
        scene,
        "resends",
        format!(
            "fired={} spurious={} original_first={} useful={} retransmit_attempts={}",
            r.loss_path_resends,
            r.loss_path_resends_spurious,
            r.loss_path_resends_original_first,
            r.loss_path_resends_useful,
            r.retransmit_attempts_total
        ),
    );
    p8(
        scene,
        "fifo_max_B",
        format!(
            "{} d_target_max_B={}",
            i.fifo_depth_max, i.admission.d_target_max
        ),
    );
    p8(
        scene,
        "critical_latency_us",
        format!(
            "n={} mean={} max={}",
            r.critical_latency_count, r.critical_latency_mean_us, r.critical_latency_max_us
        ),
    );
    p8(
        scene,
        "s2c_drops",
        format!("{}/{}", r.s2c_datagrams_dropped, r.s2c_datagrams_offered),
    );
}

fn premises(r: &BrowserlessResult, scene: &str) {
    assert!(r.bwe_samples_seen > 0, "{scene}: estimator never fed");
    assert!(
        cc::estimate_moved(&r.bwe_estimate_samples),
        "{scene}: the estimate never moved off its first value"
    );
    assert!(
        r.bytes_delivered_s2c > 0,
        "{scene}: no server->client bytes"
    );
    assert!(
        r.transport.instr.model.rate_bytes_per_s > 0,
        "{scene}: the model controller never published a window rate"
    );
}

// ---------------------------------------------------------------------------
// P8a: probes
// ---------------------------------------------------------------------------

struct Cell {
    rate_mbit: u64,
    rtt_ms: u64,
    classical: bool,
}

impl Cell {
    fn name(&self) -> String {
        format!(
            "p8a_{}mbit_{}ms_{}",
            self.rate_mbit,
            self.rtt_ms,
            if self.classical {
                "classical"
            } else {
                "hybrid"
            }
        )
    }
}

/// Fill of the probe windows that had backlog throughout and asked for no more
/// than the link carries (a 36 Mbit/s rung on a 20 Mbit/s link is clipped by
/// the window, correctly).
fn backlogged_fills(r: &BrowserlessResult, cap_bps: u64) -> Vec<f64> {
    let i = &r.transport.instr;
    let n = (i.probe_windows_closed as usize).min(i.probe_windows.len());
    i.probe_windows[..n]
        .iter()
        .filter(|w| !w.starved && w.target_bps <= cap_bps)
        .map(|w| w.fill())
        .collect()
}

async fn run_cell(c: &Cell) -> (usize, usize, f64) {
    let name = c.name();
    let (cols, rows) = (60u8, 34u8);
    let first_paint = FrameScript {
        tiles: (0..cols)
            .flat_map(|x| {
                (0..rows).map(move |y| {
                    (
                        (x, y),
                        TileSpec::Cdf53 {
                            bgra: gradient_tile(0, x, y),
                        },
                    )
                })
            })
            .collect(),
    };
    let secs = 20;
    let mut frames = vec![first_paint];
    frames.extend(
        region(tiles_for(OVERDRIVE * c.rate_mbit as f64)).frames_for(
            Duration::from_secs(secs),
            cols,
            rows,
        ),
    );
    let r = run_frames(
        Spec {
            grid: (cols, rows),
            classical_kx: c.classical,
            ..Spec::new(
                0x5B10_C4ED ^ (c.rate_mbit << 8) ^ c.rtt_ms ^ ((c.classical as u64) << 32),
                NetProfile {
                    delay_us: c.rtt_ms * 1000 / 2,
                    cap: CapTimeline::constant(c.rate_mbit * MBIT),
                    bottleneck: Some(Bottleneck::wifi()),
                    ..NetProfile::perfect()
                },
                secs,
            )
        },
        frames,
    )
    .await;
    premises(&r, &name);
    report_common(&name, &r);
    let i = &r.transport.instr;
    let n = (i.probe_windows_closed as usize).min(i.probe_windows.len());
    for (k, w) in i.probe_windows[..n].iter().enumerate() {
        p8(
            &name,
            &format!("window{k}"),
            format!(
                "target_bps={} duration_us={} bytes={} packets={} starved={} fill={:.3}",
                w.target_bps,
                w.duration_us,
                w.bytes_sent,
                w.packets_sent,
                w.starved,
                w.fill()
            ),
        );
    }
    let fills = backlogged_fills(&r, c.rate_mbit * 1_000_000);
    let worst = fills.iter().copied().fold(f64::INFINITY, f64::min);
    p8(&name, "backlogged_windows", fills.len());
    p8(&name, "worst_fill", format!("{worst:.3}"));
    p8(&name, "probes_completed", r.probes_completed);
    (fills.len(), r.probes_completed as usize, worst)
}

/// Gate (design P8a): every probe cluster with backlog puts at least 0.9 of
/// its target on the way. The premise is that the map had backlogged clusters
/// at all (a map of starved windows would pass vacuously).
///
/// The design adds "and `probes_completed > 0` in every cell". That count is
/// reported (`probes_completed`) and not gated: the glue counts a window as
/// completed only at 100.0 % of `target x duration` in tagged bytes, so a
/// window that sent 98.8 % (less than one datagram short) is "abandoned", and
/// whether a cell has one that lands on the right side of the line is a
/// granularity coin flip, not a property of the rate. The model arm also asks
/// for fewer clusters than the old Cubic path did (it feeds no capacity hint, so no
/// network-state probes), which lowers the odds further.
#[tokio::test(start_paused = true)]
async fn p8a_every_probe_cluster_with_backlog_fills_on_the_startup_map() {
    let mut cells = Vec::new();
    for classical in [false, true] {
        for (rate_mbit, rtt_ms) in [(2, 100), (10, 40), (20, 10)] {
            cells.push(Cell {
                rate_mbit,
                rtt_ms,
                classical,
            });
        }
    }
    let (mut backlogged, mut no_probe, mut worst) = (0usize, Vec::new(), f64::INFINITY);
    // Reported, not gated: see the doc comment.
    for c in &cells {
        let (n, done, w) = run_cell(c).await;
        backlogged += n;
        worst = worst.min(w);
        if done == 0 {
            no_probe.push(c.name());
        }
    }
    p8(
        "p8a",
        "summary",
        format!(
            "backlogged_windows={backlogged} worst_fill={worst:.3} cells_without_a_completed_probe(reported)={no_probe:?}"
        ),
    );
    assert!(
        backlogged >= cells.len(),
        "premise: only {backlogged} backlogged probe windows across {} cells",
        cells.len()
    );
    assert!(
        worst >= 0.9,
        "a probe cluster with backlog sent only {worst:.3} of its target"
    );
    assert!(no_probe.is_empty(), "no probe completed in: {no_probe:?}");
}

// ---------------------------------------------------------------------------
// P8b: drop-tail 10 Mbit, 200 ms buffer
// ---------------------------------------------------------------------------

const P8B_SEEDS: [u64; 5] = [0, 1, 2, 3, 4];

async fn p8b_run(i: u64, model: Option<ModelSettings>, secs: u64) -> (BrowserlessResult, u64) {
    const CAP_MBIT: u64 = 10;
    const PROP_US: u64 = 10_000;
    let frames = region(tiles_for(OVERDRIVE * CAP_MBIT as f64)).frames_for(
        Duration::from_secs(secs),
        16,
        16,
    );
    let r = run_frames(
        Spec {
            model,
            ..Spec::new(
                0xB8B0_0001 ^ i,
                NetProfile {
                    delay_us: PROP_US,
                    cap: CapTimeline::constant(CAP_MBIT * MBIT),
                    bottleneck: Some(Bottleneck {
                        depth_ms: 200,
                        aqm: None,
                    }),
                    ..NetProfile::perfect()
                },
                secs,
            )
        },
        frames,
    )
    .await;
    (r, PROP_US)
}

/// The window's queue bound for an estimate at most `r_max_ratio` of the
/// link: `(G x r_max - 1) x RTT_eff + T_q`, with `RTT_eff = min_rtt +
/// max_ack_delay` and the rates in the bucket's units (wire bytes).
fn queue_bound_us(gain: f64, r_max_ratio: f64, prop_us: u64) -> u64 {
    let rtt_eff = 2 * prop_us + MAX_ACK_DELAY_US;
    (((gain * r_max_ratio - 1.0).max(0.0)) * rtt_eff as f64) as u64 + T_Q_US
}

/// Gate (design P8b): over the settled half the mean estimate is in
/// `[0.6, 1.2]` of the link **in the units it counts** (link rate times the
/// payload fraction the run measured, as p6c and nested_cc do), on every one
/// of five seeds; and over that settled half the bottleneck queue never
/// exceeds `(G x 1.2 - 1) x RTT_eff + T_q`, where 1.2 is the estimate's upper
/// band edge in wire units (the window is sized in billed bytes, so the
/// estimate over the payload fraction is what it carries). The startup
/// excursion (the probe ladder is meant to fill the buffer) is reported as
/// `queue_peak_ms` and is not bounded by this. Mutation: `GF_P8_GAIN=8`.
#[tokio::test(start_paused = true)]
async fn p8b_droptail_c10_settles_in_band_with_a_queue_the_window_bounds() {
    const CAP_BPS: f64 = 10_000_000.0;
    let secs = 30;
    let settings = ModelSettings::default();
    let gain = std::env::var("GF_P8_GAIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(settings.gain);
    let model = Some(ModelSettings { gain, ..settings });
    let (mut ratios, mut peaks) = (Vec::new(), Vec::new());
    for &i in &P8B_SEEDS {
        let name = format!("p8b_seed{i}");
        let (r, prop_us) = p8b_run(i, model, secs).await;
        premises(&r, &name);
        report_common(&name, &r);
        let end = r.bwe_estimate_samples.last().unwrap().0 + 1;
        let (from, to) = (end / 2, end);
        let f = payload_fraction(&r);
        let mean = cc::mean_bps(&r.bwe_estimate_samples, from, to).expect("settled window");
        let ratio = mean / (CAP_BPS * f);
        let q = queue_wait_us(&r, prop_us, 0, u64::MAX);
        assert!(
            q.len() > 1000,
            "{name}: premise, {} delivered datagrams",
            q.len()
        );
        let queue_peak = peak(&q);
        p8(
            &name,
            "settled_mean_over_cap_payload",
            format!("{ratio:.3}"),
        );
        p8(
            &name,
            "settled_median_bps",
            opt(median(&r.bwe_estimate_samples, from, to)),
        );
        p8(&name, "payload_fraction", format!("{f:.3}"));
        p8(&name, "queue_peak_ms", queue_peak / 1000);
        p8(
            &name,
            "queue_peak_settled_ms",
            peak(&queue_wait_us(&r, prop_us, from, to)) / 1000,
        );
        ratios.push(ratio);
        peaks.push(peak(&queue_wait_us(&r, prop_us, from, to)));
    }
    // The design's bound, at the design's gain: a run with another gain (the
    // mutation) is held to it.
    let bound = queue_bound_us(ModelSettings::default().gain, 1.2, 10_000);
    p8(
        "p8b",
        "summary",
        format!(
            "ratios={:?} queue_peaks_ms={:?} bound_ms={}",
            ratios.iter().map(|r| format!("{r:.3}")).collect::<Vec<_>>(),
            peaks.iter().map(|p| p / 1000).collect::<Vec<_>>(),
            bound / 1000
        ),
    );
    for (i, r) in ratios.iter().enumerate() {
        assert!(
            (0.6..=1.2).contains(r),
            "seed {i}: settled estimate {r:.3} of the link (in payload units) is outside [0.6, 1.2]"
        );
    }
    for (i, p) in peaks.iter().enumerate() {
        assert!(
            *p <= bound,
            "seed {i}: bottleneck queue peaked at {} ms over the settled half, the window allows {} ms",
            p / 1000,
            bound / 1000
        );
    }
}

// ---------------------------------------------------------------------------
// P8c: feedback blackout
// ---------------------------------------------------------------------------

/// Bytes the server offered to the link in `[from_us, to_us)`.
fn offered_bytes(r: &BrowserlessResult, from_us: u64, to_us: u64) -> u64 {
    use ghostframe_e2e::harness::browserless::OFFERED_BIN_US;
    let (a, b) = (
        (from_us / OFFERED_BIN_US) as usize,
        (to_us / OFFERED_BIN_US) as usize,
    );
    r.s2c_offered_bins
        .iter()
        .enumerate()
        .filter(|(i, _)| *i >= a && *i < b)
        .map(|(_, n)| u64::from(*n))
        .sum()
}

/// Gate (design P8c): with every client-to-server packet dropped for 2 s,
///
/// * the server puts at most one window on the link: bytes offered after the
///   in-flight acknowledgements have landed (100 ms in) are at most
///   `W(R_max) + 16 datagrams` (PTO probes are exempt from the window; there
///   are about six PTOs in 2 s, two datagrams each). The design states this as
///   "quinn bytes-in-flight stay at most `W` (structural)"; quinn does not
///   export its in-flight count, and a counter kept from the controller's own
///   callbacks drifts upward under loss, so the property is read off the
///   link instead: with no acknowledgements, what is sent is what was in
///   flight;
/// * the host FIFO stays under the gate's `D_target` plus a datagram or two;
/// * the bottleneck queue stays inside its buffer;
/// * the session reaches 0.8 x the link (payload units) within `T_up` of the
///   blackout's end.
///
/// Persistent congestion is not reached here, by 2 s or by 6 s (measured:
/// `persistent_congestion_events=0`, `offered_in_blackout_B=11146` for 6 s):
/// only the acknowledgements are lost, the data arrives, and the first
/// acknowledgement afterwards covers it all, so quinn declares no loss. The
/// design's "backstop for lost feedback" therefore never fires on lost
/// feedback alone; the window is what bounds the flow, and the collapse is
/// covered by the controller's unit test.
///
/// Mutation: an unbounded window (`u64::MAX`).
#[tokio::test(start_paused = true)]
async fn p8c_a_feedback_blackout_leaves_the_window_binding_and_the_session_recovers() {
    const CAP_MBIT: u64 = 10;
    const PROP_US: u64 = 10_000;
    let secs = 36;
    let (b0, b1) = (10 * S, 12 * S);
    let cap_bps = (CAP_MBIT * 1_000_000) as f64;
    let (mut recovered, mut worst_queue, mut worst_sent) = (0, 0u64, 0u64);
    let seeds = [0u64, 1, 2, 3, 4];
    for &i in &seeds {
        let name = format!("p8c_seed{i}");
        let frames = region(tiles_for(OVERDRIVE * CAP_MBIT as f64)).frames_for(
            Duration::from_secs(secs),
            16,
            16,
        );
        let blackout = NetProfile {
            delay_us: PROP_US,
            cap: CapTimeline {
                points: vec![(0, u64::MAX), (b0, 0), (b1, u64::MAX)],
            },
            ..NetProfile::perfect()
        };
        let r = run_frames(
            Spec {
                c2s: Some(blackout),
                ..Spec::new(
                    0xB8C0_0001 ^ i,
                    NetProfile {
                        delay_us: PROP_US,
                        cap: CapTimeline::constant(CAP_MBIT * MBIT),
                        bottleneck: Some(Bottleneck {
                            depth_ms: 100,
                            aqm: None,
                        }),
                        ..NetProfile::perfect()
                    },
                    secs,
                )
            },
            frames,
        )
        .await;
        premises(&r, &name);
        report_common(&name, &r);
        let f = payload_fraction(&r);
        let i_ = &r.transport.instr;
        // The largest rate the window had to carry: the estimate's peak or a
        // probe's target, in billed bytes per second.
        let est_peak = r
            .bwe_estimate_samples
            .iter()
            .map(|(_, v)| *v)
            .max()
            .unwrap_or(0);
        let n_win = (i_.probe_windows_closed as usize).min(i_.probe_windows.len());
        let probe_peak = i_.probe_windows[..n_win]
            .iter()
            .map(|w| w.target_bps)
            .max()
            .unwrap_or(0);
        let r_max = est_peak.max(probe_peak) as f64 / f / 8.0;
        let w_bound = model_window(
            r_max as u64,
            Duration::from_micros(2 * PROP_US + MAX_ACK_DELAY_US),
            ModelSettings::default().gain,
            MTU,
        );
        let sent = offered_bytes(&r, b0 + 100_000, b1);
        let sent_bound = w_bound + 16 * MTU;
        let before = offered_bytes(&r, b0 - 2 * S, b0);
        let fifo_bound = i_.admission.d_target_max + 2 * MTU;
        let q = queue_wait_us(&r, PROP_US, 0, u64::MAX);
        let t_up = cc::time_to_first_reach(&r.bwe_estimate_samples, b1, cap_bps * f, 0.8);
        p8(&name, "window_bound_B", w_bound);
        p8(&name, "r_max_Bps", r_max as u64);
        p8(&name, "offered_in_blackout_B", sent);
        p8(&name, "offered_before_2s_B", before);
        p8(&name, "sent_bound_B", sent_bound);
        p8(&name, "fifo_bound_B", fifo_bound);
        p8(&name, "queue_peak_ms", peak(&q) / 1000);
        p8(&name, "t_up_ms", opt(t_up.map(|t| t / 1000)));
        // Premise: the sender was sending at full tilt before the blackout,
        // so a window that did not bind would have shown.
        assert!(
            before > 2 * sent_bound,
            "{name}: premise, {before} B offered in the 2 s before"
        );
        assert!(
            sent <= sent_bound,
            "{name}: {sent} B offered during the blackout, a window allows {sent_bound} B"
        );
        assert!(
            i_.fifo_depth_max <= fifo_bound,
            "{name}: host FIFO peaked at {} B, bound {fifo_bound} B",
            i_.fifo_depth_max
        );
        worst_queue = worst_queue.max(peak(&q));
        worst_sent = worst_sent.max(sent);
        recovered += t_up.is_some_and(|t| t <= T_UP_US) as usize;
        assert!(
            t_up.is_some_and(|t| t <= T_UP_US),
            "{name}: reached 0.8 x link {:?} ms after the blackout, T_up is {} ms",
            t_up.map(|t| t / 1000),
            T_UP_US / 1000
        );
    }
    p8(
        "p8c",
        "summary",
        format!(
            "recovered={recovered}/{} worst_queue_ms={} worst_offered_in_blackout_B={worst_sent}",
            seeds.len(),
            worst_queue / 1000
        ),
    );
    // The buffer is 100 ms: anything the link holds longer is a drop, not delay.
    assert!(
        worst_queue <= 100_000 + T_Q_US,
        "the bottleneck queue exceeded its own buffer"
    );
}

// ---------------------------------------------------------------------------
// P8e: the round trip rises at constant capacity
// ---------------------------------------------------------------------------

/// Gate: a 10 Mbit link whose one-way delay steps 5 -> 75 ms at 10 s (a
/// Tailscale direct path falling back to DERP; a competitor's standing queue
/// looks the same to quinn's minimum round trip). The estimate must stay at
/// 0.6 x the link (payload units) or more at the end, and must come back:
/// 0.6 x the link within the round-trip window plus two round trips of the
/// new path (derived at the assertion), and stay above 0.5 x thereafter, on
/// each of five seeds; the median over the seeds of the settled level after
/// the step is at least 0.6.
///
/// quinn's `min_rtt` never rises, so a window of `G x R x (min_rtt + 5 ms)`
/// holds the flow to `W / srtt` = `R x 2 x 10 / 80` = a quarter of `R`. In that
/// state the acknowledged rate says nothing about the path, yet goog_cc reads
/// the step as overuse and cuts to 0.85 x it, three times in 300 ms, to a tenth
/// of the link. Additive increase from there took 15 s. Mutation: take the
/// `BaseRise` restore out of `model_after_feedback` and the estimate stays
/// there.
#[tokio::test(start_paused = true)]
async fn p8e_the_estimate_survives_a_rising_round_trip() {
    // Chaotic scene: per seed, the estimate must come back and stay (no
    // collapse); the settled level is a property of the five seeds, not of
    // each (one seed's median after the step sat at 0.64 with the 3 s window).
    let mut afters = Vec::new();
    for i in 0..5u64 {
        afters.push(p8e_run(0xB8E0_0001 ^ (i << 8)).await);
    }
    afters.sort_by(f64::total_cmp);
    let pooled = afters[afters.len() / 2];
    p8(
        "p8e",
        "median_of_seed_medians_after_over_link",
        format!("{pooled:.3}"),
    );
    assert!(
        pooled >= 0.6,
        "median over seeds {pooled:.3} of the link after the step ({afters:?})"
    );
}

async fn p8e_run(seed: u64) -> f64 {
    const CAP_MBIT: u64 = 10;
    let secs = 40;
    let step_at = 10 * S;
    let frames = region(tiles_for(OVERDRIVE * CAP_MBIT as f64)).frames_for(
        Duration::from_secs(secs),
        16,
        16,
    );
    let name = "p8e";
    let r = run_frames(
        Spec::new(
            seed,
            NetProfile {
                delay_us: 5_000,
                delay_steps: vec![(step_at, 75_000)],
                cap: CapTimeline::constant(CAP_MBIT * MBIT),
                bottleneck: Some(Bottleneck {
                    depth_ms: 200,
                    aqm: None,
                }),
                ..NetProfile::perfect()
            },
            secs,
        ),
        frames,
    )
    .await;
    premises(&r, name);
    report_common(name, &r);
    let f = payload_fraction(&r);
    let link = (CAP_MBIT * 1_000_000) as f64 * f;
    let s = &r.bwe_estimate_samples;
    let end = s.last().unwrap().0 + 1;
    let before = median(s, 5 * S, step_at).expect("before") as f64 / link;
    let after = median(s, step_at + 8 * S, end).expect("after") as f64 / link;
    let low = |a: u64, b: u64| {
        cc::window(s, a, b)
            .iter()
            .map(|(_, v)| *v)
            .min()
            .expect("samples") as f64
            / link
    };
    // The step reads as overuse, and the window holds the flow to W / srtt
    // until the old round trips age out of the 3 s minimum: the estimate
    // dips. The gate is that it comes back, within the window plus `T_up`,
    // and stays.
    let t_low = cc::window(s, step_at, end)
        .iter()
        .min_by_key(|(_, v)| *v)
        .map(|(t, _)| *t)
        .expect("samples");
    let recovered = cc::time_to_first_reach(s, t_low, link, 0.6).map(|d| t_low + d - step_at);
    p8(name, "dip_over_link", format!("{:.3}", low(step_at, end)));
    p8(
        name,
        "t_recover_to_0.6_ms",
        opt(recovered.map(|t| t / 1000)),
    );
    p8(name, "median_before_over_link", format!("{before:.3}"));
    p8(name, "median_after_over_link", format!("{after:.3}"));
    assert!(
        before >= 0.6,
        "premise: {before:.3} of the link before the step"
    );
    let back = recovered.expect("the estimate never came back to 0.6 of the link");
    // Derived from the mechanism (`BaseRise`): the flow is stale until the
    // old minimum has left the base round trip's window, at most `RTT_WINDOW`
    // after the step; the next feedback then continues the estimate from the
    // rate the path acknowledged before the step, one round trip on the new
    // path, and the flow it admits is acknowledged one round trip after that. Nothing here waits on additive
    // increase, so no `T_up`.
    let rtt_after = 2 * 75_000 + 5_000;
    let bound = RTT_WINDOW.as_micros() as u64 + 2 * rtt_after;
    assert!(
        back <= bound,
        "estimate back at 0.6 x link after {} ms, bound {} ms",
        back / 1000,
        bound / 1000
    );
    // goog_cc takes the first feedback after a reseed back to the old value
    // (seen in its trace) and keeps the restored one from the second, so the
    // estimate holds from one round trip after it first reads restored.
    let lowest = low(step_at + back + rtt_after, end);
    p8(
        name,
        "lowest_after_recovery_over_link",
        format!("{lowest:.3}"),
    );
    assert!(
        lowest >= 0.5,
        "estimate fell back to {lowest:.3} of the link"
    );
    after
}

// ---------------------------------------------------------------------------
// P7c rows (report only): loss, then a burst
// ---------------------------------------------------------------------------

/// P7c, loss then burst: 10 Mbit, 20 ms round trip, 1 % iid loss. 10 s
/// application-limited at a tenth of the link, then a full-screen change
/// offering 1.3 x the link for 5 s. Reports the time until the estimate
/// reaches 0.8 x the link, the burst's queue peak and the bytes offered.
/// `warm_secs` of saturating load come first when non-zero. Not gated: PR-7
/// compares the arms on these rows.
async fn p7c_run(name: &str, warm_secs: u64) {
    const CAP_MBIT: u64 = 10;
    const PROP_US: u64 = 10_000;
    let cap_bps = (CAP_MBIT * 1_000_000) as f64;
    let mut frames = if warm_secs > 0 {
        region(tiles_for(OVERDRIVE * CAP_MBIT as f64)).frames_for(
            Duration::from_secs(warm_secs),
            16,
            16,
        )
    } else {
        Vec::new()
    };
    frames.extend(region(tiles_for(0.1 * CAP_MBIT as f64)).frames_for(
        Duration::from_secs(10),
        16,
        16,
    ));
    let burst_at = frames.len() as u64 * PRODUCTION_CADENCE_US;
    frames.extend(region(tiles_for(OVERDRIVE * CAP_MBIT as f64)).frames_for(
        Duration::from_secs(5),
        16,
        16,
    ));
    let r = run_frames(
        Spec::new(
            0xCC07_7C00 ^ warm_secs,
            NetProfile {
                delay_us: PROP_US,
                loss: 0.01,
                cap: CapTimeline::constant(CAP_MBIT * MBIT),
                bottleneck: Some(Bottleneck {
                    depth_ms: 100,
                    aqm: None,
                }),
                ..NetProfile::perfect()
            },
            warm_secs + 16,
        ),
        frames,
    )
    .await;
    premises(&r, name);
    report_common(name, &r);
    let frac = payload_fraction(&r);
    let reach = cc::time_to_first_reach(&r.bwe_estimate_samples, burst_at, cap_bps * frac, 0.8);
    let q = queue_wait_us(&r, PROP_US, burst_at, burst_at + 5 * S);
    p8(name, "t_rate_80pct_ms", opt(reach.map(|t| t / 1000)));
    p8(name, "burst_queue_peak_ms", peak(&q) / 1000);
    p8(
        name,
        "burst_queue_mean_ms",
        q.iter().sum::<u64>() / (q.len().max(1) as u64) / 1000,
    );
    p8(
        name,
        "offered_B_burst",
        offered_bytes(&r, burst_at, burst_at + 5 * S),
    );
}

#[tokio::test(start_paused = true)]
async fn p7c_loss_then_burst() {
    p7c_run("p7c", 0).await;
}

#[tokio::test(start_paused = true)]
async fn p7c_warm_loss_then_burst() {
    p7c_run("p7c_warm", 3).await;
}

// ---------------------------------------------------------------------------
// P8d: capacity drop during an idle
// ---------------------------------------------------------------------------

/// What one P8d run measured.
struct P8d {
    spurious: u64,
    queue_peak_us: u64,
    time_below_us: Option<u64>,
    held_bps: u64,
    resumed_bps: Option<u64>,
    payload_fraction: f64,
}

const P8D_PROP_US: u64 = 10_000;
/// The idle, `[start, end)`, and the instant the capacity drops inside it.
const P8D_IDLE: (u64, u64) = (14 * S, 22 * S);
const P8D_DROP_AT: u64 = 16 * S;

async fn p8d_run(
    i: u64,
    name: &str,
    model: Option<ModelSettings>,
    from_mbit: u64,
    to_mbit: u64,
) -> P8d {
    let secs = 40;
    let mut frames = region(tiles_for(OVERDRIVE * from_mbit as f64)).frames_for(
        Duration::from_secs(secs),
        16,
        16,
    );
    for (n, f) in frames.iter_mut().enumerate() {
        let t = n as u64 * PRODUCTION_CADENCE_US;
        if t >= P8D_IDLE.0 && t < P8D_IDLE.1 {
            f.tiles.clear();
        }
    }
    let r = run_frames(
        Spec {
            model,
            ..Spec::new(
                0xB8D0_0001 ^ i,
                NetProfile {
                    delay_us: P8D_PROP_US,
                    cap: CapTimeline {
                        points: vec![(0, from_mbit * MBIT), (P8D_DROP_AT, to_mbit * MBIT)],
                    },
                    // Deep enough that the queue is not truncated by drops.
                    bottleneck: Some(Bottleneck {
                        depth_ms: 700,
                        aqm: None,
                    }),
                    ..NetProfile::perfect()
                },
                secs,
            )
        },
        frames,
    )
    .await;
    premises(&r, name);
    report_common(name, &r);
    let s = &r.bwe_estimate_samples;
    let held = median(s, P8D_IDLE.0 - 2 * S, P8D_IDLE.0).expect("pre-idle estimate");
    let q_after = queue_wait_us(&r, P8D_PROP_US, P8D_IDLE.1, u64::MAX);
    // First offer instant after the resume from which the queue is back under
    // 2 x T_q for good.
    let mut below = None;
    let post: Vec<(u64, u64)> = r
        .s2c_transit_samples
        .iter()
        .filter(|(t, _)| *t >= P8D_IDLE.1)
        .map(|(t, tr)| (*t, tr.saturating_sub(P8D_PROP_US)))
        .collect();
    for (k, (t, _)) in post.iter().enumerate() {
        if post[k..].iter().all(|(_, q)| *q <= 2 * T_Q_US) {
            below = Some(*t - P8D_IDLE.1);
            break;
        }
    }
    let resumed = s
        .iter()
        .find(|(t, _)| *t >= P8D_IDLE.1 + 500_000)
        .map(|(_, v)| *v);
    // Is the idle idle on the wire? Offered bytes per second across the gap.
    for sec in 12..24u64 {
        p8(
            name,
            &format!("offered_B_at_{sec}s"),
            offered_bytes(&r, sec * S, (sec + 1) * S),
        );
    }
    // The first 600 ms after the resume, 20 ms at a time.
    for k in 0..30u64 {
        let (a, b) = (P8D_IDLE.1 + k * 20_000, P8D_IDLE.1 + (k + 1) * 20_000);
        p8(
            name,
            &format!("fine_t+{}ms", k * 20),
            format!(
                "queue_peak_ms={} offered_B={}",
                peak(&queue_wait_us(&r, P8D_PROP_US, a, b)) / 1000,
                offered_bytes(&r, a, b)
            ),
        );
    }
    // The first seconds after the resume, 250 ms at a time: the estimate, and
    // the worst queue wait of the datagrams offered in the bin.
    for k in 0..24u64 {
        let (a, b) = (P8D_IDLE.1 + k * 250_000, P8D_IDLE.1 + (k + 1) * 250_000);
        let est = s.iter().find(|(t, _)| *t >= a).map(|(_, v)| *v);
        p8(
            name,
            &format!("t+{}ms", k * 250),
            format!(
                "est={} queue_peak_ms={} offered_B={}",
                opt(est),
                peak(&queue_wait_us(&r, P8D_PROP_US, a, b)) / 1000,
                offered_bytes(&r, a, b)
            ),
        );
    }
    p8(name, "held_bps", held);
    p8(name, "est_500ms_after_resume_bps", opt(resumed));
    p8(name, "queue_peak_after_resume_ms", peak(&q_after) / 1000);
    p8(
        name,
        "queue_back_under_2tq_ms",
        opt(below.map(|t| t / 1000)),
    );
    p8(name, "samples_after_resume", post.len());
    P8d {
        spurious: r.loss_path_resends_spurious,
        queue_peak_us: peak(&q_after),
        time_below_us: below,
        held_bps: held,
        resumed_bps: resumed,
        payload_fraction: payload_fraction(&r),
    }
}

/// Gate (design P8d, the structural half): zero spurious retransmissions
/// after the resume, and the premise that the session resumed and queued.
#[tokio::test(start_paused = true)]
async fn p8d_a_capacity_drop_during_an_idle_costs_no_spurious_resend() {
    for i in 0..1u64 {
        let name = format!("p8d_seed{i}");
        let o = p8d_run(i, &name, Some(ModelSettings::default()), 20, 2).await;
        assert!(
            o.queue_peak_us > 0 && o.resumed_bps.is_some(),
            "{name}: premise, the session resumed and queued"
        );
        assert_eq!(o.spurious, 0, "{name}: spurious retransmissions");
    }
}

/// Queue half of design P8d, REPORTED, NOT GATED: the owner accepts some tail
/// latency for better latency and throughput the rest of the time. The cold
/// restart's first round trip puts tens of kB on a 2 Mbit/s link before any
/// feedback can cut it (queue ~215 ms against the 113 ms bound derived from
/// the window). The bound is printed as `queue_bound_ms` and
/// the peak as `queue_peak_after_resume_ms`. Design P8d: zero spurious retransmissions after the resume
/// (structural: timers start at departure and a PTO resets on every ack), and
/// the peak queue stays at or under the bound for the restart actually taken:
/// the cold restart resumes at `max(start, restart_fraction x held)` and the
/// window gain is `restart_gain` until the first feedback and `G` after it, so
/// the bound is `(G x k' - 1) x RTT_eff + T_q` with `k'` the resumed rate over
/// the new link (design section 6.6 gives the `G = 1` figure, which holds only
/// for that first round trip). Mutation: hold the rate through the idle
/// (`T_hold` of an hour) with `G = 2`: 665 ms of queue against the bound.
#[tokio::test(start_paused = true)]
async fn p8d_queue_after_a_cold_restart_is_reported() {
    let (from, to) = (20u64, 2u64);
    let settings = if std::env::var("GF_P8_HOLD").is_ok() {
        ModelSettings {
            t_hold: Duration::from_secs(3600),
            restart_gain: ModelSettings::default().gain,
            ..ModelSettings::default()
        }
    } else {
        ModelSettings::default()
    };
    let mut worst_spurious = 0;
    let mut rows = Vec::new();
    for i in 0..5u64 {
        let name = format!("p8d_seed{i}");
        let o = p8d_run(i, &name, Some(settings), from, to).await;
        let link_bps = (to * 1_000_000) as f64;
        // The rate the restart resumed at, over the new link, in billed
        // bytes: the estimate counts payload, the link carries every byte.
        let rtt_eff = (2 * P8D_PROP_US + MAX_ACK_DELAY_US) as f64;
        let idle = Duration::from_micros(P8D_IDLE.1 - P8D_IDLE.0);
        let held = o.held_bps as f64;
        let resumed = if idle > settings.t_hold {
            (held * settings.restart_fraction).max(2_000_000.0)
        } else {
            held
        };
        let k = resumed / o.payload_fraction / link_bps;
        // The estimate can climb on the first acknowledgements before the
        // detector cuts it, and the window follows: 1.2 is the headroom the
        // ceiling allows (`K = 1 / 0.85`, less its own noise).
        let bound = (((settings.gain * k * 1.2 - 1.0).max(0.0)) * rtt_eff) as u64 + T_Q_US;
        p8(&name, "restart_k", format!("{k:.2}"));
        p8(&name, "queue_bound_ms", bound / 1000);
        rows.push(format!(
            "seed{i}: peak={}ms back_under_2tq={}ms spurious={} bound={}ms",
            o.queue_peak_us / 1000,
            opt(o.time_below_us.map(|t| t / 1000)),
            o.spurious,
            bound / 1000
        ));
        worst_spurious = worst_spurious.max(o.spurious);
        assert!(
            o.queue_peak_us > 0 && o.resumed_bps.is_some(),
            "{name}: premise, the session resumed and queued"
        );
        assert_eq!(o.spurious, 0, "{name}: spurious retransmissions");
        p8(&name, "queue_over_bound", o.queue_peak_us > bound);
    }
    p8("p8d", "summary", rows.join(" | "));
}
