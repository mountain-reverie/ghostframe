//! The interleaved split (rev 6, R6-e2 S3): the adaptive controller's split
//! notch, end to end over the PCM rate model at 150 and 200 Mbit/s.
//!
//! The arms are the product (`adaptive`, the split allowed), the same
//! controller with the split forbidden (`RunOptions::fec_no_split`: the
//! two-notch ladder, one block of at most 223 sources over 20 ms), and the
//! split notch with `m` pinned to 1 (`RunOptions::fec_split_m1`: its span and
//! per-block rule, no interleave). Everything else, the seeds and the link, is
//! the same, so the first difference is the split as a whole and the second is
//! the interleave alone.
//!
//! One cell (150 Mbit/s, mean burst 8, 3 seeds) runs in CI. The whole grid
//! (150 and 200 Mbit/s x mean burst 3 and 8, 5 seeds) runs behind
//! `GHOSTFRAME_FEC_SWEEP=full` (about ten minutes locally), the same switch the
//! control-law sweep uses; `GHOSTFRAME_FEC_SPLIT_SECS=<n>` sets the virtual
//! seconds per run. A premise that does not hold fails the test rather than
//! passing it: a scene where the loss never reached the controller says
//! nothing about the split.

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
    m: u8,
    split: bool,
    split_entries: u64,
    split_groups: u64,
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
    /// The split forbidden: the ladder as before it existed.
    Single,
    /// The split notch, one block per group.
    M1,
    /// The product.
    Split,
}

async fn run(cell: Cell, seed: u64, secs: u64, arm: Arm) -> Run {
    let (scene, links) = scene(cell, seed, secs * 1000);
    let opts = RunOptions {
        fec_adaptive: true,
        fec_no_split: arm == Arm::Single,
        fec_split_m1: arm == Arm::M1,
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
        m: a.m,
        split: a.split,
        split_entries: a.split_entries,
        split_groups: a.split_groups,
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
        "FECSPLIT run cell={} seed={seed:#x} secs={secs} arm={} shown={} late33={} late_pct={:.2} late100={} \
         dropped={} reports={} engaged={} sized={} b_hat={:.2} p_hat_ppm={} m={} in_split={} \
         split_entries={} split_groups={} notch_changes={} changes={} rho_bytes={:.4} \
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
        out.m,
        out.split,
        out.split_entries,
        out.split_groups,
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
    split: Vec<Run>,
    single: Vec<Run>,
    m1: Vec<Run>,
}

impl Pair {
    fn late(runs: &[Run]) -> Vec<f64> {
        runs.iter().map(Run::late_pct).collect()
    }

    /// `(single mean, split mean, gap, spread)` of late frames, percent; the
    /// spread is the larger seed range of the two arms (max - min), as the
    /// split simulation (FINDINGS section 12) judged it.
    fn gap(&self) -> (f64, f64, f64, f64) {
        let (s, c) = (Self::late(&self.split), Self::late(&self.single));
        let spread = range(&s).max(range(&c));
        (mean(&c), mean(&s), mean(&c) - mean(&s), spread)
    }

    /// The same seed draws the same demand and loss in both arms, so the
    /// difference per seed (single - split, points of frames late) is the
    /// measurement the seed spread buries: `(mean, standard error, seeds
    /// where the split is no later)`.
    fn paired(&self) -> (f64, f64, usize) {
        Self::paired_of(&self.single, &self.split)
    }

    /// [`Self::paired`] of any two arms: `(mean, s.e.m., seeds where `b` is
    /// no later than `a`)` of `a - b`.
    fn paired_of(a: &[Run], b: &[Run]) -> (f64, f64, usize) {
        let d: Vec<f64> = a
            .iter()
            .zip(b)
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

    /// The split's gain, judged on the pairs: no seed later, and the mean
    /// gain `sigmas` standard errors above zero. `Err` names the miss.
    fn paired_gain(&self, sigmas: f64) -> Result<(), String> {
        let (m, sem, ok) = self.paired();
        if ok == self.split.len() && m > sigmas * sem && m > 0.0 {
            Ok(())
        } else {
            Err(format!(
                "paired gain {m:+.2} +- {sem:.2} points, split no later in {ok}/{} seeds",
                self.split.len()
            ))
        }
    }

    fn report(&self, cell: Cell) {
        let (pm, psem, pok) = self.paired();
        let (im, isem, iok) = Self::paired_of(&self.m1, &self.split);
        let late_m1 = mean(&Self::late(&self.m1));
        let (single, split, gap, spread) = self.gap();
        let b: Vec<f64> = self
            .split
            .iter()
            .map(|r| r.b_hat_x100 as f64 / 100.0)
            .collect();
        let rho_s: Vec<f64> = self.split.iter().map(|r| r.rho_bytes).collect();
        let rho_c: Vec<f64> = self.single.iter().map(|r| r.rho_bytes).collect();
        let held: u64 = self
            .split
            .iter()
            .map(|r| r.held_bytes_peak)
            .max()
            .unwrap_or(0);
        let held_dec: u64 = self.split.iter().map(|r| r.declined_held_bytes).sum();
        let wait: Vec<u64> = self.split.iter().map(Run::wait_p95_ms).collect();
        println!(
            "FECSPLIT cell={} seeds={} late33_single={single:.2} late33_split={split:.2} late33_m1={late_m1:.2} gap={gap:+.2} \
             spread={spread:.2} outside_spread={} paired_gain={pm:+.2}+-{psem:.2} split_seeds_no_later={pok}/{} interleave_alone_gain={im:+.2}+-{isem:.2} no_later={iok} b_hat_mean={:.2} \
             overhead_single={:.3} overhead_split={:.3} held_peak_max={held} \
             declined_held_bytes_sum={held_dec} wait_p95_ms_per_seed={wait:?} \
             split_entries={:?} notch_changes_split_arm={:?}",
            cell.name(),
            self.split.len(),
            gap > spread,
            self.split.len(),
            mean(&b),
            mean(&rho_c),
            mean(&rho_s),
            self.split
                .iter()
                .map(|r| r.split_entries)
                .collect::<Vec<_>>(),
            self.split
                .iter()
                .map(|r| r.notch_changes)
                .collect::<Vec<_>>(),
        );
    }
}

async fn pair(cell: Cell, seeds: &[u64], secs: u64) -> Pair {
    let mut p = Pair {
        split: vec![],
        single: vec![],
        m1: vec![],
    };
    for &seed in seeds {
        p.single.push(run(cell, seed, secs, Arm::Single).await);
        p.m1.push(run(cell, seed, secs, Arm::M1).await);
        p.split.push(run(cell, seed, secs, Arm::Split).await);
    }
    p
}

/// Premises of every run: the loss happened, the reports reached the
/// controller, it engaged and sized blocks, and the client counted what it
/// should. A scene that fails one of these measured nothing.
fn premises(p: &Pair, cell: Cell) {
    for (arm, runs) in [("split", &p.split), ("single", &p.single), ("m1", &p.m1)] {
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
            (r.split_entries, r.split_groups),
            (0, 0),
            "{}: the control arm entered the split",
            cell.name()
        );
    }
}

/// Where the split is expected: it engaged on every seed, its groups were
/// really interleaved, and the burst estimate (read at the end of the run, a
/// 10 s average that wanders by a unit or two) was above the exit: the notch
/// holds between the exit and the entry, so the entry itself is not asserted
/// on a final reading.
fn split_premises(p: &Pair, cell: Cell) {
    for (i, r) in p.split.iter().enumerate() {
        let ctx = format!("{} seed #{i}", cell.name());
        assert!(r.split_entries >= 1, "{ctx}: the split never engaged");
        assert!(
            r.split_groups > 20,
            "{ctx}: {} interleaved groups",
            r.split_groups
        );
        assert!(r.b_hat_x100 > EXIT_X100, "{ctx}: b_hat {}", r.b_hat_x100);
    }
    for (i, r) in p.m1.iter().enumerate() {
        let ctx = format!("{} m1 seed #{i}", cell.name());
        assert!(r.split_entries >= 1, "{ctx}: the split notch never engaged");
        assert_eq!(r.split_groups, 0, "{ctx}: the control interleaved");
    }
}

/// No flapping. A stationary burst of 8 has an estimate that wanders by about
/// a unit (30 s runs: mean 8.08, s.d. 1.01, lowest 6.4 over 20 runs), nowhere
/// near the exit at 4, so the notch is entered once and held: the split arm
/// changes notch twice (Wide, then Split; at most three with a stray Narrow
/// at start-up) and enters the split exactly once. Without the hysteresis
/// (exit at the entry) the estimate's dips below 6 leave and re-enter it, and
/// the entries reach 2.
fn bounded_changes(p: &Pair, cell: Cell, _secs: u64) {
    for (i, r) in p.split.iter().enumerate() {
        assert!(
            r.notch_changes <= 3,
            "{} seed #{i}: {} notch changes",
            cell.name(),
            r.notch_changes
        );
        assert!(
            r.split_entries <= 1,
            "{} seed #{i}: {} entries into the split",
            cell.name(),
            r.split_entries
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_split_helps_at_150_mbit_burst_8() {
    let cell = Cell {
        cap_mbit: 150,
        run: 8,
    };
    let secs = secs(20);
    let p = pair(cell, &SEEDS[..3], secs).await;
    p.report(cell);
    premises(&p, cell);
    split_premises(&p, cell);
    bounded_changes(&p, cell, secs);
    // Late frames no higher than the one-block control, seed by seed. The
    // gain is inside the seed range (the simulation found this cell so too),
    // so it is judged on the pairs: three seeds, so only the sign.
    // What the cell shows is the notch's span (T = the frame interval), not the
    // interleave: both the product and the one-block `m1` control gain over the
    // single block (the interleave alone, split - m1, is not asserted: it is
    // zero here and at bursts 16 and 32, FINDINGS section 13).
    if let Err(e) = p.paired_gain(0.0) {
        panic!("{}: split - single: {e}", cell.name());
    }
    let (m, sem, ok) = Pair::paired_of(&p.single, &p.m1);
    assert!(
        ok == p.m1.len() && m > 0.0,
        "{}: m1 - single: paired gain {m:+.2} +- {sem:.2}, m1 no later in {ok} seeds",
        cell.name()
    );
}

/// Below two blocks the split is never entered, whatever the burst: the run
/// with the split allowed is the run without it, datagram for datagram. (The
/// loss is as bursty as at 150 Mbit/s b8, so the burst estimate is over the
/// entry and only the rate keeps the notch out.)
#[tokio::test(start_paused = true)]
async fn a_frame_that_fits_one_block_never_splits() {
    let cell = Cell {
        cap_mbit: 10,
        run: 8,
    };
    let a = run(cell, SEEDS[0], 8, Arm::Split).await;
    let b = run(cell, SEEDS[0], 8, Arm::Single).await;
    assert!(
        a.engaged && a.dropped > 20 && a.sized > 10,
        "premise: engaged and lossy"
    );
    assert!(
        a.b_hat_x100 > ENTER_X100,
        "premise: b_hat {} is over the entry",
        a.b_hat_x100
    );
    assert_eq!((a.split_entries, a.split_groups), (0, 0));
    assert_eq!(
        a.digest, b.digest,
        "the split arm is the control, byte for byte"
    );
}

/// The whole grid, behind `GHOSTFRAME_FEC_SWEEP=full`.
#[tokio::test(start_paused = true)]
async fn the_split_grid() {
    if !std::env::var("GHOSTFRAME_FEC_SWEEP").is_ok_and(|v| v == "full") {
        eprintln!("skipped: set GHOSTFRAME_FEC_SWEEP=full (about ten minutes)");
        return;
    }
    let secs = secs(30);
    // Outcomes are collected and asserted after every cell has printed, so one
    // failing cell does not hide the numbers of the others.
    let mut failures: Vec<String> = Vec::new();
    for (cap_mbit, run) in [(150, 3), (150, 8), (200, 3), (200, 8)] {
        let cell = Cell { cap_mbit, run };
        let p = pair(cell, &SEEDS, secs).await;
        p.report(cell);
        premises(&p, cell);
        bounded_changes(&p, cell, secs);
        if cell.run == 3 {
            // Burst 3: b_hat is under the entry and the split stays out.
            for (i, r) in p.split.iter().enumerate() {
                if r.b_hat_x100 >= ENTER_X100 || r.split_entries != 0 {
                    failures.push(format!(
                        "{} seed #{i}: b_hat {} entries {}",
                        cell.name(),
                        r.b_hat_x100,
                        r.split_entries
                    ));
                }
            }
        } else {
            split_premises(&p, cell);
            // The gain is inside the seed range at both rates (printed as
            // `outside_spread`; FINDINGS section 13), so it is judged on the
            // pairs: no seed later and the mean 3 standard errors above 0.
            if let Err(e) = p.paired_gain(3.0) {
                failures.push(format!("{}: split - single: {e}", cell.name()));
            }
            let (m, sem, ok) = Pair::paired_of(&p.single, &p.m1);
            if ok != p.m1.len() || m <= 3.0 * sem {
                failures.push(format!(
                    "{}: m1 - single: {m:+.2} +- {sem:.2}, {ok} seeds",
                    cell.name()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
