// SPDX-License-Identifier: AGPL-3.0-or-later
//! The adaptive controller's frame notch (rev 6, R6-e2 S3): one block spans a
//! frame (T = the frame interval, `k_max` 223) when the loss is bursty and a
//! frame no longer fits a 255-symbol block, end to end over the PCM rate model
//! at 150 and 200 Mbit/s.
//!
//! The arms are the product (`adaptive`, the notch allowed) and the same
//! controller with the notch forbidden (`RunOptions::fec_no_frame_notch`: the
//! two-notch ladder, one block of at most 223 sources over 20 ms). Everything
//! else, the seeds and the link, is the same, so a difference is the notch.
//!
//! One cell (150 Mbit/s, mean burst 8, 3 seeds) runs in CI. The grid (150 and
//! 200 Mbit/s x mean burst 3 and 8, plus 200 Mbit/s burst 16; 5 seeds) runs
//! behind `GHOSTFRAME_FEC_SWEEP=full` (about 10 minutes locally), the same
//! switch the control-law sweep uses; `GHOSTFRAME_FEC_SPLIT_SECS=<n>` sets the
//! virtual seconds per run. A premise that does not hold fails the test rather
//! than passing it: a scene where the loss never reached the controller says
//! nothing about the notch. The interleave this notch once came with (m > 1
//! blocks per frame) measured as no gain over this notch and was removed
//! (FINDINGS section 13).

use std::time::Duration;

use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, H264Load, LinkOverrides,
    RunOptions, SceneLoad, DEFAULT_CADENCE_US,
};
use ghostframe_e2e::harness::h264_scene::{pcm_load, Pattern};
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, DropPlan, NetProfile};
use ghostframe_lib::encoder::h264_harness::{PcmMode, PcmRateModel};
use ghostframe_lib::transport::reliable_emitter::emission_queue::WireClass;

const SEEDS: [u64; 5] = [0xFB_0001, 0xFB_0002, 0xFB_0003, 0xFB_0004, 0xFB_0005];

/// The controller's entry threshold (`FEC_SPLIT_B_ENTER`), as `b_hat x 100`.
const ENTER_X100: u64 = 600;
/// The exit threshold (`FEC_SPLIT_B_EXIT`), as `b_hat x 100`.
const EXIT_X100: u64 = 400;
/// The control arm's tail buffer, as the control-law sweep's (ms).
const BUF_MS: u64 = 150;
/// A frame is late when it was decoded more than this long after the stream's
/// own floor (the fastest frame). 33 ms, one frame interval, is the unit of the
/// split simulation (FINDINGS section 12, "late33") and the one the arms are
/// judged on; the control-law sweep's, an RTT (100 ms), is printed beside it:
/// at these rates ARQ recovers most losses inside it, so it hardly moves.
const LATE_US: u64 = 33_000;
const LATE_RTT_US: u64 = 100_000;
/// The injected loss: Gilbert-Elliott, loss 1 in the bad state, 2 % on average.
const AVG_LOSS: f64 = 0.02;

#[derive(Clone, Copy, Debug)]
struct Cell {
    cap_mbit: u64,
    /// The injected mean burst (datagrams).
    run: u32,
}

impl Cell {
    fn name(self) -> String {
        format!("{}M/b{}", self.cap_mbit, self.run)
    }
}

fn secs(default: u64) -> u64 {
    std::env::var("GHOSTFRAME_FEC_SPLIT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn scene(cell: Cell, seed: u64, ms: u64) -> (BrowserlessScene, LinkOverrides) {
    let base = NetProfile {
        delay_us: 25_000,
        cap: CapTimeline::constant(cell.cap_mbit * 125_000),
        bottleneck: Some(Bottleneck {
            depth_ms: BUF_MS,
            aqm: None,
        }),
        ..NetProfile::perfect()
    };
    let exit = 1.0 / f64::from(cell.run);
    let (net, links) = Pattern::Burst {
        enter: AVG_LOSS * exit / (1.0 - AVG_LOSS),
        exit,
        loss: 1.0,
    }
    .apply(base);
    let mut load: H264Load = pcm_load(PcmMode::RefManager);
    load.pcm_rate_policy = true;
    let frame = ((cell.cap_mbit as usize * 125_000 / 30) * 3 / 2) as f64;
    let frame = frame as usize;
    load.pcm_rate_model = Some(PcmRateModel::s2(frame, frame * 3, 30));
    (
        BrowserlessScene {
            seed,
            datagram_send_buffer_bytes: None,
            tick_budget_floor_bytes: None,
            load: SceneLoad::H264(load),
            cadence_us: DEFAULT_CADENCE_US,
            net,
            drops: DropPlan::default(),
            duration: Duration::from_millis(ms),
            grid_cols: 4,
            grid_rows: 4,
        },
        links,
    )
}

/// What one run reads.
#[derive(Clone, Debug, Default)]
struct Run {
    shown: usize,
    /// Frames decoded more than [`LATE_US`] / [`LATE_RTT_US`] after the floor.
    late: usize,
    late_rtt: usize,
    dropped: u64,
    reports: u64,
    engaged: bool,
    sized: u64,
    /// `b_hat x 100` at the end of the run, the controller's burst estimate.
    b_hat_x100: u64,
    p_hat_ppm: u64,
    in_frame_notch: bool,
    frame_entries: u64,
    notch_changes: u64,
    changes: u64,
    rho_bytes: f64,
    requests: u64,
    late_rows: u64,
    declined: u64,
    /// Server side: blocks handed to ARQ plus blocks closed declined.
    handoffs_declined: u64,
    declined_held_bytes: u64,
    declined_open_cap: u64,
    held_bytes_peak: u64,
    wait_ms: [u64; 9],
    digest: u64,
}

impl Run {
    fn late_pct(&self) -> f64 {
        100.0 * self.late as f64 / self.shown.max(1) as f64
    }

    /// Upper bound (ms) of the bucket holding the 95th percentile of the
    /// recovery wait; 0 when no block was rebuilt, 999 above the last bound.
    fn wait_p95_ms(&self) -> u64 {
        let n: u64 = self.wait_ms.iter().sum();
        let mut acc = 0;
        for (i, c) in self.wait_ms.iter().enumerate() {
            acc += c;
            if n > 0 && acc * 100 >= n * 95 {
                return ghostframe_client_core::fec_rx::RECOVERY_WAIT_BOUNDS_MS
                    .get(i)
                    .copied()
                    .unwrap_or(999);
            }
        }
        0
    }
}

fn frames_late(r: &BrowserlessResult) -> (usize, usize, usize) {
    let Some(run) = r.h264_run() else {
        return (0, 0, 0);
    };
    let ts: std::collections::HashMap<u32, u64> = run
        .truth
        .order
        .iter()
        .zip(&run.truth.au_log)
        .map(|(seq, au)| (*seq, u64::from(au.timestamp_us)))
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut lat: Vec<u64> = Vec::new();
    for rec in &run.display {
        if seen.insert(rec.frame_seq) {
            if let Some(t) = ts.get(&rec.frame_seq) {
                lat.push(rec.at_us.saturating_sub(*t));
            }
        }
    }
    let floor = lat.iter().copied().min().unwrap_or(0);
    let late = lat.iter().filter(|l| **l - floor > LATE_US).count();
    let late_rtt = lat.iter().filter(|l| **l - floor > LATE_RTT_US).count();
    (lat.len(), late, late_rtt)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    /// The frame notch forbidden: the ladder as before it existed.
    Single,
    /// The product.
    Frame,
}

async fn run(cell: Cell, seed: u64, secs: u64, arm: Arm) -> Run {
    let (scene, links) = scene(cell, seed, secs * 1000);
    let opts = RunOptions {
        fec_no_frame_notch: arm == Arm::Single,
        ..RunOptions::default()
    };
    let r = run_browserless_with_options(scene, links, opts)
        .await
        .expect("scene ran");
    let (a, i, c) = (&r.transport.adapt, &r.transport.instr, &r.client_blocks);
    let repair = r.transport.instr.offered.of(WireClass::Repair);
    let (shown, late, late_rtt) = frames_late(&r);
    let out = Run {
        shown,
        late,
        late_rtt,
        dropped: r.s2c_datagrams_dropped,
        reports: r.transport.loss_reports,
        engaged: a.engaged,
        sized: a.blocks_sized,
        b_hat_x100: a.b_hat_x100,
        p_hat_ppm: a.p_hat_ppm,
        in_frame_notch: a.frame,
        frame_entries: a.frame_entries,
        notch_changes: a.notch_changes,
        changes: a.changes,
        rho_bytes: repair as f64 / i.offered.total().saturating_sub(repair).max(1) as f64,
        requests: c.requests,
        late_rows: c.late_rows,
        declined: c.declined,
        handoffs_declined: i.block_handoffs + i.block_closed_declined,
        declined_held_bytes: c.declined_held_bytes,
        declined_open_cap: c.declined_open_cap,
        held_bytes_peak: c.held_bytes_peak,
        wait_ms: c.recovery_wait_ms,
        digest: r.wire_digest,
    };
    println!(
        "FECFRAME run cell={} seed={seed:#x} secs={secs} arm={} shown={} late33={} late_pct={:.2} late100={} \
         dropped={} reports={} engaged={} sized={} b_hat={:.2} p_hat_ppm={} in_frame={} \
         frame_entries={} notch_changes={} changes={} rho_bytes={:.4} \
         requests={} late_rows={} declined={} handoffs_declined={} declined_held_bytes={} declined_open_cap={} \
         held_bytes_peak={} wait_p95_ms={} wait_hist={:?} digest={:#x}",
        cell.name(),
        format!("{arm:?}").to_lowercase(),
        out.shown,
        out.late,
        out.late_pct(),
        out.late_rtt,
        out.dropped,
        out.reports,
        out.engaged,
        out.sized,
        out.b_hat_x100 as f64 / 100.0,
        out.p_hat_ppm,
        out.in_frame_notch,
        out.frame_entries,
        out.notch_changes,
        out.changes,
        out.rho_bytes,
        out.requests,
        out.late_rows,
        out.declined,
        out.handoffs_declined,
        out.declined_held_bytes,
        out.declined_open_cap,
        out.held_bytes_peak,
        out.wait_p95_ms(),
        out.wait_ms,
        out.digest,
    );
    out
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len().max(1) as f64
}

fn range(v: &[f64]) -> f64 {
    let (lo, hi) = v
        .iter()
        .fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(*x), h.max(*x)));
    if v.is_empty() {
        0.0
    } else {
        hi - lo
    }
}

/// Both arms over the seeds of one cell.
struct Pair {
    frame: Vec<Run>,
    single: Vec<Run>,
}

impl Pair {
    fn late(runs: &[Run]) -> Vec<f64> {
        runs.iter().map(Run::late_pct).collect()
    }

    /// The same seed draws the same demand and loss in both arms, so the
    /// difference per seed (single - frame, points of frames late) is the
    /// measurement the seed spread buries: `(mean, standard error, seeds where
    /// the notch is no later)`.
    fn paired(&self) -> (f64, f64, usize) {
        let d: Vec<f64> = self
            .single
            .iter()
            .zip(&self.frame)
            .map(|(c, s)| c.late_pct() - s.late_pct())
            .collect();
        let m = mean(&d);
        let var = d.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (d.len().max(2) - 1) as f64;
        (
            m,
            (var / d.len().max(1) as f64).sqrt(),
            d.iter().filter(|x| **x >= 0.0).count(),
        )
    }

    /// The notch's gain, judged on the pairs: no seed later, and the mean gain
    /// `sigmas` standard errors above zero. `Err` names the miss.
    fn paired_gain(&self, sigmas: f64) -> Result<(), String> {
        let (m, sem, ok) = self.paired();
        if ok == self.frame.len() && m > sigmas * sem && m > 0.0 {
            Ok(())
        } else {
            Err(format!(
                "paired gain {m:+.2} +- {sem:.2} points, notch no later in {ok}/{} seeds",
                self.frame.len()
            ))
        }
    }

    fn report(&self, cell: Cell) {
        let (pm, psem, pok) = self.paired();
        let (s, c) = (Self::late(&self.frame), Self::late(&self.single));
        let spread = range(&s).max(range(&c));
        let b: Vec<f64> = self
            .frame
            .iter()
            .map(|r| r.b_hat_x100 as f64 / 100.0)
            .collect();
        let rho_f: Vec<f64> = self.frame.iter().map(|r| r.rho_bytes).collect();
        let rho_c: Vec<f64> = self.single.iter().map(|r| r.rho_bytes).collect();
        let held = self
            .frame
            .iter()
            .map(|r| r.held_bytes_peak)
            .max()
            .unwrap_or(0);
        let held_dec: u64 = self.frame.iter().map(|r| r.declined_held_bytes).sum();
        let wait: Vec<u64> = self.frame.iter().map(Run::wait_p95_ms).collect();
        println!(
            "FECFRAME cell={} seeds={} late33_single={:.2} late33_frame={:.2} gain={:+.2} \
             spread={spread:.2} outside_spread={} paired_gain={pm:+.2}+-{psem:.2} \
             frame_seeds_no_later={pok}/{} b_hat_mean={:.2} overhead_single={:.3} \
             overhead_frame={:.3} held_peak_max={held} declined_held_bytes_sum={held_dec} \
             wait_p95_ms_per_seed={wait:?} frame_entries={:?} notch_changes_frame_arm={:?}",
            cell.name(),
            self.frame.len(),
            mean(&c),
            mean(&s),
            mean(&c) - mean(&s),
            mean(&c) - mean(&s) > spread,
            self.frame.len(),
            mean(&b),
            mean(&rho_c),
            mean(&rho_f),
            self.frame
                .iter()
                .map(|r| r.frame_entries)
                .collect::<Vec<_>>(),
            self.frame
                .iter()
                .map(|r| r.notch_changes)
                .collect::<Vec<_>>(),
        );
    }
}

async fn pair(cell: Cell, seeds: &[u64], secs: u64) -> Pair {
    let mut p = Pair {
        frame: vec![],
        single: vec![],
    };
    for &seed in seeds {
        p.single.push(run(cell, seed, secs, Arm::Single).await);
        p.frame.push(run(cell, seed, secs, Arm::Frame).await);
    }
    p
}

/// Premises of every run: the loss happened, the reports reached the
/// controller, it engaged and sized blocks, and the client counted what it
/// should. A scene that fails one of these measured nothing.
fn premises(p: &Pair, cell: Cell) {
    for (arm, runs) in [("frame", &p.frame), ("single", &p.single)] {
        for (i, r) in runs.iter().enumerate() {
            let ctx = format!("{} {arm} seed #{i}", cell.name());
            assert!(r.dropped > 100, "{ctx}: the link dropped {}", r.dropped);
            assert!(r.reports > 20, "{ctx}: {} reports", r.reports);
            assert!(
                r.engaged && r.sized > 50,
                "{ctx}: engaged={} sized={}",
                r.engaged,
                r.sized
            );
            assert!(r.shown > 100, "{ctx}: {} frames shown", r.shown);
            assert!(
                r.wait_ms.iter().sum::<u64>() > 0,
                "{ctx}: no block was rebuilt from repair rows"
            );
        }
    }
    for r in &p.single {
        assert_eq!(
            r.frame_entries,
            0,
            "{}: the control arm entered the frame notch",
            cell.name()
        );
    }
}

/// Where the notch is expected: it engaged on every seed, and the burst
/// estimate (read at the end of the run, a 10 s average that wanders by a unit
/// or two) was above the exit: the notch holds between the exit and the entry,
/// so the entry itself is not asserted on a final reading.
fn frame_premises(p: &Pair, cell: Cell) {
    for (i, r) in p.frame.iter().enumerate() {
        let ctx = format!("{} seed #{i}", cell.name());
        assert!(r.frame_entries >= 1, "{ctx}: the frame notch never engaged");
        assert!(r.in_frame_notch, "{ctx}: not in the frame notch at the end");
        assert!(r.b_hat_x100 > EXIT_X100, "{ctx}: b_hat {}", r.b_hat_x100);
    }
}

/// No flapping. A stationary burst of 8 has an estimate that wanders by about
/// a unit (30 s runs: mean 8.08, s.d. 1.01, lowest 6.4 over 20 runs), nowhere
/// near the exit at 4, so the notch is entered once and held: the notch arm
/// changes notch twice (Wide, then Frame; at most three with a stray Narrow
/// at start-up) and enters the frame notch exactly once. Without the
/// hysteresis (exit at the entry) the estimate's dips below 6 leave and
/// re-enter it, and the entries reach 2.
fn bounded_changes(p: &Pair, cell: Cell) {
    for (i, r) in p.frame.iter().enumerate() {
        assert!(
            r.notch_changes <= 3,
            "{} seed #{i}: {} notch changes",
            cell.name(),
            r.notch_changes
        );
        assert!(
            r.frame_entries <= 1,
            "{} seed #{i}: {} entries into the frame notch",
            cell.name(),
            r.frame_entries
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_frame_notch_helps_at_150_mbit_burst_8() {
    let cell = Cell {
        cap_mbit: 150,
        run: 8,
    };
    let secs = secs(20);
    let p = pair(cell, &SEEDS[..3], secs).await;
    p.report(cell);
    premises(&p, cell);
    frame_premises(&p, cell);
    bounded_changes(&p, cell);
    // Late frames no higher than the one-block control, seed by seed. The gain
    // is inside the seed range, so it is judged on the pairs: three seeds, so
    // only the sign.
    if let Err(e) = p.paired_gain(0.0) {
        panic!("{}: frame notch - single: {e}", cell.name());
    }
}

/// While a frame fits one block the notch is never entered, whatever the
/// burst: the run with the notch allowed is the run without it, datagram for
/// datagram. (The loss is as bursty as at 150 Mbit/s b8, so the burst estimate
/// is over the entry and only the rate keeps the notch out.)
///
/// 50 Mbit/s with a mean burst of 10, not 10 Mbit/s with 8: the estimator
/// leaves out the runs FEC could not repair (`2 x r_cap(k)` at most), and at
/// 10 Mbit/s that limit is 12, under the 17-32 runs a mean burst of 8 has in
/// the tail. The burst estimate then reads 2.9, not over the entry, and the
/// premise is lost to the owner's rule (do not count what FEC cannot repair).
/// At 50 Mbit/s the limit is 52 (a block of 104), the tail is kept, and a
/// frame (fill 0.82) still fits one 255-symbol block with the frame notch's
/// repair share. Margins, not edges: the burst estimate is 7.0 to 9.7 against
/// the entry 6.0 on the three seeds (a burst of 8 read 5.1 on the first), and
/// the fill is 18 % under one block (60 Mbit/s was 2 % under). The unit walk
/// in `adapt_size.rs` proves the rule for a burst estimate up to 12 at any
/// rate.
#[tokio::test(start_paused = true)]
async fn a_frame_that_fits_one_block_never_enters_the_frame_notch() {
    let cell = Cell {
        cap_mbit: 50,
        run: 10,
    };
    for &seed in &SEEDS[..3] {
        let a = run(cell, seed, 8, Arm::Frame).await;
        let b = run(cell, seed, 8, Arm::Single).await;
        assert!(
            a.engaged && a.dropped > 20 && a.sized > 10,
            "premise: engaged and lossy"
        );
        assert!(
            a.b_hat_x100 > ENTER_X100,
            "premise: b_hat {} is over the entry (seed {seed:#x})",
            a.b_hat_x100
        );
        assert_eq!(a.frame_entries, 0, "seed {seed:#x}");
        assert_eq!(
            a.digest, b.digest,
            "the notch arm is the control, byte for byte (seed {seed:#x})"
        );
    }
}

/// The grid, behind `GHOSTFRAME_FEC_SWEEP=full`: 150 and 200 Mbit/s at bursts
/// 3 and 8, and 200 Mbit/s at burst 16.
#[tokio::test(start_paused = true)]
async fn the_frame_notch_grid() {
    if !std::env::var("GHOSTFRAME_FEC_SWEEP").is_ok_and(|v| v == "full") {
        eprintln!("skipped: set GHOSTFRAME_FEC_SWEEP=full (about ten minutes)");
        return;
    }
    let secs = secs(30);
    // Outcomes are collected and asserted after every cell has printed, so one
    // failing cell does not hide the numbers of the others.
    let mut failures: Vec<String> = Vec::new();
    for (cap_mbit, run) in [(150, 3), (150, 8), (200, 3), (200, 8), (200, 16)] {
        let cell = Cell { cap_mbit, run };
        let p = pair(cell, &SEEDS, secs).await;
        p.report(cell);
        premises(&p, cell);
        bounded_changes(&p, cell);
        if cell.run == 3 {
            // Burst 3: b_hat is under the entry and the notch stays out.
            for (i, r) in p.frame.iter().enumerate() {
                if r.b_hat_x100 >= ENTER_X100 || r.frame_entries != 0 {
                    failures.push(format!(
                        "{} seed #{i}: b_hat {} entries {}",
                        cell.name(),
                        r.b_hat_x100,
                        r.frame_entries
                    ));
                }
            }
        } else {
            frame_premises(&p, cell);
            // The gain is inside the seed range (printed as `outside_spread`;
            // FINDINGS section 13), so it is judged on the pairs: no seed later
            // and the mean 3 standard errors above 0.
            if let Err(e) = p.paired_gain(3.0) {
                failures.push(format!("{}: frame notch - single: {e}", cell.name()));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
