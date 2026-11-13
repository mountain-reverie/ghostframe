// SPDX-License-Identifier: AGPL-3.0-or-later
//! H.264 as a member of the transport, in the stepped browserless harness.
//!
//! What runs: the real `IoBridge` event loop (the `RefManager` plans every
//! frame, the commit and send path, the ledger feedback), a real `ClientNet`
//! that advertises H.264 and acknowledges the fragments it receives and
//! decodes them with libavcodec, a seeded netsim between them. What is
//! scripted: the encoder, a PCM-patch writer executing the manager's plans or
//! the real openh264 library. Every picture carries markers, so the client
//! side can say which picture it displayed and compare it with the one the
//! encoder meant (see `ghostframe_e2e::harness::h264_scene`).
//!
//! This file holds the capacity-step scenes (rate control, the estimator and
//! the headroom guard in H.264 mode); the loss, reordering, MTU and recovery
//! scenes are `refs_scenes`, those with the real library `openh264_scenes`.
//!
//! Every scene states its premise as an assertion before the effect it is
//! about: an effect asserted over a drop that never fired proves nothing.

use std::time::Duration;

use ghostframe_client_core::Event as CoreEvent;
use ghostframe_client_net::ClientNetEvent;
use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, H264Load, LinkOverrides,
    RunOptions, SceneLoad, DEFAULT_CADENCE_US,
};
use ghostframe_e2e::netsim::{
    Bottleneck, CapTimeline, DropPlan, FragSel, FrameDropRule, NetProfile,
};
use ghostframe_lib::encoder::h264_harness::PcmRateModel;
use ghostframe_lib::transport::io_bridge::IdrCause;

const CADENCE_US: u64 = DEFAULT_CADENCE_US;

fn scene(
    seed: u64,
    secs: u64,
    net: NetProfile,
    load: H264Load,
    drops: DropPlan,
) -> BrowserlessScene {
    BrowserlessScene {
        seed,
        datagram_send_buffer_bytes: None,
        tick_budget_floor_bytes: None,
        load: SceneLoad::H264(load),
        cadence_us: CADENCE_US,
        net,
        drops,
        duration: Duration::from_secs(secs),
        grid_cols: 4,
        grid_rows: 4,
    }
}

async fn run(s: BrowserlessScene, opts: RunOptions) -> anyhow::Result<BrowserlessResult> {
    let links = LinkOverrides::from_burst_env(&s.net);
    run_browserless_with_options(s, links, opts).await
}

fn link_20ms() -> NetProfile {
    NetProfile {
        delay_us: 20_000,
        ..NetProfile::perfect()
    }
}

/// `(frame_seq, is_keyframe)` of every frame the client reassembled, in order.
fn delivered(r: &BrowserlessResult) -> Vec<(u32, bool)> {
    r.events
        .iter()
        .filter_map(|e| match e {
            ClientNetEvent::Core(CoreEvent::NeedsH264 {
                frame_seq,
                is_keyframe,
                ..
            }) => Some((*frame_seq, *is_keyframe)),
            _ => None,
        })
        .collect()
}

fn report(name: &str, r: &BrowserlessResult) {
    let s = &r.h264.stats;
    println!(
        "{name}: sent={} idr={} (session={} fault={}) acked={} suspect={} \
         tx rec={} live={} lost={} forgot={} late={} outstanding={} \
         verdicts idr={} p={} spurious={} skipped hold={} bp={} purged={} \
         bwe_samples={} bwe_bps={} pto_fires={} loss_by_pto={} idr_resends={} probe_resends={} \
         delivered={} s2c_drop={}/{}",
        s.frames_sent,
        s.frames_idr,
        s.idr_by_cause[IdrCause::Session.index()],
        s.idr_by_cause[IdrCause::EncoderFault.index()],
        s.frames_acked,
        s.frames_suspect,
        s.tx_recorded,
        s.tx_acked_live,
        s.tx_judged_lost,
        s.tx_forgotten,
        s.tx_acked_late,
        r.h264.ledger_records,
        s.verdicts_idr_frag,
        s.verdicts_p_frag,
        s.verdicts_already_acked,
        s.frames_skipped_hold,
        s.frames_skipped_backpressure,
        s.queue_purged,
        r.bwe_samples_seen,
        r.bwe_estimate_bps,
        r.emitter_stats.pto_fires,
        r.transport.instr.loss_by_pto,
        r.emitter_stats.h264_idr_resends,
        r.emitter_stats.h264_probe_resends,
        delivered(r).len(),
        r.s2c_datagrams_dropped,
        r.s2c_datagrams_offered,
    );
    println!(
        "  app_probes={}; padding windows={} datagrams={} bytes={}; skipped for padding={}",
        s.app_probes,
        s.probe_padding_windows,
        s.probe_padding_datagrams,
        s.probe_padding_bytes,
        s.frames_skipped_pad,
    );
}

/// The estimate a scene should end above: half of what it was before the
/// disturbance (the design's bound for the acknowledgement stall; it is not
/// tuned). What it is measured against: the stall's only loss events are
/// verdicts on datagrams whose acknowledgements were lost (3 to 25 of ~400-600
/// transmissions), which goog_cc's loss-based controller meets with a bounded
/// step per update. Across the six runs below the lowest end-of-scene to
/// pre-stall ratio is 0.70 (seed 0x3301, quiet screen), against 0.5 here. The
/// estimate of an application-limited sender wanders by itself (a stall-free
/// control of the quiet scene dips to 0.60 of its pre-stall value between
/// IDR bursts), so it is the end of the scene that is compared, as the design
/// has it, not the minimum.
fn estimate_floor(pre_stall_bps: u64) -> u64 {
    pre_stall_bps / 2
}

fn estimate_at(r: &BrowserlessResult, at_us: u64) -> u64 {
    r.bwe_estimate_samples
        .iter()
        .rfind(|(t, _)| *t <= at_us)
        .map(|x| x.1)
        .expect("a sample before the stall")
}

// ---- rate control (design PR A4) --------------------------------------------

/// Frames per second of the harness cadence (33.3 ms).
const FPS: u32 = 30;

fn bottleneck_net(
    cap_bytes_per_s: CapTimeline,
    delay_us: u64,
    jitter_us: u64,
    depth_ms: u64,
) -> NetProfile {
    NetProfile {
        delay_us,
        jitter_us,
        // Bytes per second despite the name.
        cap: cap_bytes_per_s,
        bottleneck: Some(Bottleneck {
            depth_ms,
            aqm: None,
        }),
        ..NetProfile::perfect()
    }
}

/// Scene P-B (design 4.3): a capacity step 2 -> 6 Mbit/s with the stream in
/// H.264 mode. The step is at `STEP_AT_US`; the scene runs long enough after
/// it to observe the whole `T_UP` bound.
///
/// The link has no jitter. With 1 ms of it, a probe burst of 100 to 180 kB at 6
/// to 12 Mbit/s reorders neighbouring datagrams and the loss path's time rule
/// judges some lost (9 to 12 spurious loss IDRs per run, all acknowledged late):
/// a finding about the loss path and bursts, separate from whether the
/// estimate finds the headroom, and it would make this scene measure both.
struct StepRun {
    r: BrowserlessResult,
    /// Median and minimum estimate over the four seconds before the step (the
    /// median is printed; the minimum is what the property compares with).
    pre: u64,
    pre_min: u64,
    /// `(t_us, estimate)` from the step on.
    post: Vec<(u64, u64)>,
}

const STEP_AT_US: u64 = 6_000_000;
const STEP_FROM_BPS: u64 = 2_000_000;
const STEP_TO_BPS: u64 = 6_000_000;
/// The client-to-server variants the populations run: as it comes, one extra
/// packet at once per delivery, and one 8 ms after it.
const CHATTER: [Option<Duration>; 3] = [None, Some(Duration::ZERO), Some(CHATTER_LATE)];
const CHATTER_LATE: Duration = Duration::from_millis(8);
/// The estimator's start rate, `BweWrapper::INITIAL_BPS`.
const BWE_START_BPS: u64 = 2_000_000;
/// `2 x MIN_TIME_BETWEEN_ALR_PROBES + 1 s`, the D1 P6b bound (design 4.3).
const T_UP_US: u64 = 11_000_000;
/// The seeds the P-B gates run: the first six of the 24 timelines
/// 0x6701..=0x6718 that `p_b_seed_population` measures (the harness is
/// deterministic, each seed is a fixed timeline). Not picked: they are simply
/// the first six, and the population says how the other 18 fare.
const P_B_SEEDS: [u64; 6] = [0x6701, 0x6702, 0x6703, 0x6704, 0x6705, 0x6706];

/// Which stream the capacity-step scene runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepMode {
    /// An acked-reference session (`refs_cbr_load`), rate moved in place, the
    /// PCM encoder modelling the whole `RateTarget` (S2's VCE).
    Refs,
    /// The first B6 baseline: the bps-only rate model (a VBR bucket of
    /// `bps / 10`), the in-place policy.
    RefsBpsOnly,
    /// `Refs` with S2's 1.8x first frame, settling in four, given to every
    /// rate step, however small (the pessimistic reading of the model).
    RefsFullTransient,
    /// `Refs` with the first-frame excess scaled linearly with the size of the
    /// step (the optimistic reading: small steps are nearly free).
    RefsLinearStep,
    /// `Refs` on the earlier burst calibration of a re-send (a backend that
    /// re-sends the HRD at 75 % fullness): the first frame 1.4-1.8x.
    RefsBurst,
    /// The refs twin with the probe raise mutated off (`Refs` has it, as
    /// production does): padding alone fills the windows. A control.
    RefsNoRaise,
    /// `Refs` with a pessimistic model: a window's frames yield 0.86 of
    /// what the VCE's profile gives them, its minimum (mean yield 0.63).
    RefsPessRaise,
    /// The refs twin with a 1 s spike hold (still let go early on a fall).
    RefsHold1s,
    /// The refs twin with the 2 s hold that is never let go early (the hold
    /// before review 2).
    RefsHoldFixed,
}

impl StepMode {
    /// `GF_POP_MODE=refs|refs-no-raise|refs-pess-raise|refs-hold-1s|refs-hold-fixed|refs-bps-only|refs-full-transient|refs-linear-step|refs-burst` (default refs).
    fn from_env() -> Self {
        match std::env::var("GF_POP_MODE").as_deref() {
            Ok("refs") => Self::Refs,
            Ok("refs-bps-only") => Self::RefsBpsOnly,
            Ok("refs-full-transient") => Self::RefsFullTransient,
            Ok("refs-linear-step") => Self::RefsLinearStep,
            Ok("refs-burst") => Self::RefsBurst,
            Ok("refs-no-raise") => Self::RefsNoRaise,
            Ok("refs-pess-raise") => Self::RefsPessRaise,
            Ok("refs-hold-1s") => Self::RefsHold1s,
            Ok("refs-hold-fixed") => Self::RefsHoldFixed,
            _ => Self::Refs,
        }
    }

    fn load(self) -> H264Load {
        match self {
            Self::Refs => refs_cbr_load(),
            Self::RefsFullTransient => {
                let mut l = refs_cbr_load();
                l.pcm_rate_model = Some(PcmRateModel::s2(40_000, 120_000, FPS).with_s2_transient());
                l
            }
            Self::RefsLinearStep => {
                let mut l = refs_cbr_load();
                l.pcm_rate_model = Some(
                    PcmRateModel::s2(40_000, 120_000, FPS)
                        .with_burst_resend()
                        .with_linear_step_scaling(),
                );
                l
            }
            Self::RefsBurst => {
                let mut l = refs_cbr_load();
                l.pcm_rate_model = Some(PcmRateModel::s2(40_000, 120_000, FPS).with_burst_resend());
                l
            }
            Self::RefsNoRaise => {
                let mut l = refs_cbr_load();
                l.no_probe_raise = true;
                l
            }
            Self::RefsPessRaise => {
                let mut l = refs_cbr_load();
                l.pcm_rate_model =
                    Some(PcmRateModel::s2(40_000, 120_000, FPS).with_raise_yield(0.86));
                l
            }
            Self::RefsHold1s => {
                let mut l = refs_cbr_load();
                l.spike_tune = Some((1_000, true));
                l
            }
            Self::RefsHoldFixed => {
                let mut l = refs_cbr_load();
                l.spike_tune = Some((2_000, false));
                l
            }
            Self::RefsBpsOnly => {
                let mut l = refs_cbr_load();
                l.pcm_rate_model = Some(PcmRateModel::bps_only(40_000, 120_000, FPS));
                l
            }
        }
    }
}

/// An acked-reference stream under production's rate policy, and a PCM
/// encoder that sizes its frames from the rate it is told. The content
/// demands more than any rate the step scene reaches (40 kB a P frame is 9.6
/// Mbit/s at 30 fps), so the stream is held to the rate and rate control is
/// what the scene measures: `pcm_load`'s 3 kB frames are 0.72 Mbit/s, below
/// every rate here, and would show nothing.
fn refs_cbr_load() -> H264Load {
    let mut l = ghostframe_e2e::harness::h264_scene::pcm_load(
        ghostframe_lib::encoder::h264_harness::PcmMode::RefManager,
    );
    l.pcm_rate_policy = true;
    l.pcm_rate_model = Some(PcmRateModel::s2(40_000, 120_000, FPS));
    l
}

async fn capacity_step(seed: u64) -> StepRun {
    capacity_step_with_jitter(seed, 0).await
}

async fn capacity_step_with_jitter(seed: u64, jitter_us: u64) -> StepRun {
    capacity_step_with(seed, jitter_us, None).await
}

async fn capacity_step_with(seed: u64, jitter_us: u64, ack_chatter: Option<Duration>) -> StepRun {
    capacity_step_on(
        seed,
        jitter_us,
        ack_chatter,
        NORMAL_DELAY_US,
        StepMode::Refs,
    )
    .await
}

/// One-way delay of the scenes' links (a 40 ms round trip), and of the
/// low-latency variants (4 ms): shorter than the client's 5 ms acknowledgement
/// flush and than a frame interval, which is where a gap measured against the
/// round trip alone switched the ceiling off.
const NORMAL_DELAY_US: u64 = 20_000;
const LOW_DELAY_US: u64 = 1_000;

async fn capacity_step_on(
    seed: u64,
    jitter_us: u64,
    ack_chatter: Option<Duration>,
    delay_us: u64,
    mode: StepMode,
) -> StepRun {
    capacity_step_deep(seed, jitter_us, ack_chatter, delay_us, mode, LINK_BUFFER_MS).await
}

/// The scenes' link buffer, ms of the link's rate.
const LINK_BUFFER_MS: u64 = 600;

/// `capacity_step_on` on a link whose buffer holds `buffer_ms`.
async fn capacity_step_deep(
    seed: u64,
    jitter_us: u64,
    ack_chatter: Option<Duration>,
    delay_us: u64,
    mode: StepMode,
    buffer_ms: u64,
) -> StepRun {
    let net = bottleneck_net(
        CapTimeline::step(STEP_FROM_BPS / 8, STEP_AT_US, STEP_TO_BPS / 8),
        delay_us,
        jitter_us,
        buffer_ms,
    );
    let r = run(
        scene(seed, 30, net, mode.load(), DropPlan::default()),
        RunOptions {
            ack_chatter,
            ..RunOptions::default()
        },
    )
    .await
    .expect("scene");
    let mut pre: Vec<u64> = r
        .bwe_estimate_samples
        .iter()
        .filter(|(t, _)| (STEP_AT_US - 4_000_000..STEP_AT_US).contains(t))
        .map(|x| x.1)
        .collect();
    assert!(!pre.is_empty(), "premise: estimate samples before the step");
    pre.sort_unstable();
    let post = r
        .bwe_estimate_samples
        .iter()
        .filter(|(t, _)| *t >= STEP_AT_US)
        .copied()
        .collect();
    StepRun {
        pre_min: pre[0],
        pre: pre[pre.len() / 2],
        post,
        r,
    }
}

/// `(time to 0.8 x the new capacity, lowest estimate in T_UP / pre-step minimum)`.
fn step_outcome(s: &StepRun) -> (Option<u64>, f64) {
    let target = STEP_TO_BPS * 8 / 10;
    let reached = s
        .post
        .iter()
        .find(|(_, b)| *b >= target)
        .map(|(t, _)| t - STEP_AT_US);
    let min_after = s
        .post
        .iter()
        .filter(|(t, _)| *t < STEP_AT_US + T_UP_US)
        .map(|x| x.1)
        .min()
        .expect("samples after the step");
    (reached, min_after as f64 / s.pre_min as f64)
}

/// P-B (design 4.3): after a capacity step 2 -> 6 Mbit/s, a CBR stream in H.264
/// mode finds the headroom. The estimate reaches `0.8 x 6` Mbit/s within
/// `T_UP = 11 s` of the step and never falls under `0.8 x` its pre-step value
/// meanwhile (the CBR loop is a feedback loop: a rate cut the estimator reads
/// as a smaller path would ratchet down).
///
/// What finds the headroom, and what each piece is for (each was removed in
/// turn; the A4 report lists what then happened):
///  * the application probes (`h264_start_app_probe`): a stream held at
///    `0.8 x` its estimate is not application-limited by goog_cc's ALR
///    detector, so goog_cc never asks for a periodic probe. The bridge starts a
///    cluster of its own at twice the estimate every 5 s (and again straight
///    away while the estimate reaches 0.7 x the last target), with an id goog_cc
///    never issues, and goog_cc's probe estimator measures it from the
///    acknowledged packets and applies the result;
///  * a probe window no IDR is due to fill is filled with padding: flagged
///    copies of an acknowledged P-frame fragment, measured at the probe rate;
///  * the estimate ceiling is held off around the padding: a burst spends the
///    whole grant before any of it is acknowledged, and the ceiling reads
///    that as a saturated silent path and clamps the estimate to its floor;
///  * a rise of more than 30 % reopens the encoder at once, once the raw
///    estimate has stayed over 1.3 x the smoothed one for two evaluations:
///    goog_cc pulls a probe's result back to `1.5 x` the acknowledged rate
///    within a few hundred milliseconds.
///
/// "Its pre-step value" is the lowest estimate of the four seconds before the
/// step, not the median: the estimate of a stream that fills 0.8 of a 2 Mbit/s
/// link wanders between 1.0 and 2.1 Mbit/s by itself, and the step only adds
/// headroom, so an estimate under 0.8 x the quietest it was before is the loop
/// collapsing.
///
/// Premises: the application probed, padding windows were filled, probes
/// completed after the start-up ladder, and the pre-step estimate is the 2
/// Mbit/s link's.
///
/// The lower bound holds on every seed. The headroom is found within `T_UP` on
/// all but one of the six: the harness is chaotic (one timing tweak moves a
/// seed's probes by a round), and `p_b_seed_population` measures 96 % of
/// timelines reaching it, so a bound that asks all of any six seeds fails one
/// code change in five for no reason. The population gate beside it
/// (`p_b_population_never_collapses_and_finds_the_headroom_whenever_the_client_acks`)
/// asks the rate of a larger set. Mutation: with the application's probing
/// disabled none of the six reaches it.
#[tokio::test(start_paused = true)]
async fn a_capacity_step_up_is_found_within_t_up_in_h264_mode() {
    let mut found = 0;
    for seed in P_B_SEEDS {
        let st = capacity_step(seed).await;
        report(&format!("P-B {seed:#x}"), &st.r);
        let (reached, low) = step_outcome(&st);
        let (r, s) = (&st.r, &st.r.h264.stats);
        println!(
            "  reached {reached:?} us, lowest/pre-min {low:.2}, pre={} pre_min={}; probes completed={} \
             abandoned={} padding windows={} bytes={} app_probes={}",
            st.pre,
            st.pre_min,
            r.probes_completed,
            r.probes_abandoned,
            s.probe_padding_windows,
            s.probe_padding_bytes,
            s.app_probes
        );
        assert!(
            s.app_probes > 0,
            "seed {seed:#x}: premise: the application probed"
        );
        assert!(
            s.probe_padding_windows > 0,
            "seed {seed:#x}: premise: padding was used"
        );
        assert!(
            r.probes_completed > 2,
            "seed {seed:#x}: premise: probes beyond the ladder"
        );
        assert!(
            st.pre > STEP_FROM_BPS / 2 && st.pre <= STEP_FROM_BPS * 11 / 10,
            "seed {seed:#x}: pre-step estimate {} is not the 2 Mbit/s link's",
            st.pre
        );
        found += reached.is_some_and(|t| t <= T_UP_US) as usize;
        assert!(
            low >= 0.8,
            "seed {seed:#x}: the estimate fell to {low:.2} x its pre-step minimum"
        );
    }
    assert!(
        found + 1 >= P_B_SEEDS.len(),
        "the estimate reached 0.8 x {STEP_TO_BPS} within {T_UP_US} us on only {found} of {} seeds",
        P_B_SEEDS.len()
    );
}

/// P-B on a link with 1 ms of jitter: a probe burst reorders neighbouring
/// datagrams, and the loss path must not read that as loss. No P-frame loss
/// verdict may fall in a probe burst or with padding in flight
/// (`p_verdicts_in_probe`), the estimate keeps the lower bound, and the
/// headroom is found within `T_UP` on all but one of eight seeds (the stream
/// loses nothing: `s2c_drop` 0). Not all of them: the harness is chaotic, and
/// over 20 timelines (0x6701..0x6714, 1 ms of jitter) this finds the headroom
/// within `T_UP` on 19 (0x6702 takes 11.6 s) with the lower bound kept on all
/// 20; master finds it on 18 (one at 15.1 s, one never) and keeps the bound on
/// 19. A bound that asks all of any three seeds fails one code change in
/// three for no reason.
///
/// Three measures: a pad's acknowledgement does not move the reference real
/// datagrams are judged against (`TransmissionLedger::resolve_at`), padding is
/// only queued behind an empty H.264 queue, and a datagram that departed in a
/// probe burst (an IDR's or a pad's) is judged by the time rule after three
/// times the usual threshold (`TransmissionLedger::mark_probe_burst`; the
/// tile path shares the ledger but has no such burst in H.264 mode). The same
/// scene made 5, 6 and 0 loss IDRs on these seeds before them (the reviewer's
/// figures); now 2, 0, 2, none of them near a probe.
#[tokio::test(start_paused = true)]
async fn a_capacity_step_up_with_jitter_costs_no_extra_idr() {
    let (mut loss_total, mut in_probe, mut found) = (0, 0, 0usize);
    let seeds: Vec<u64> = (0x6701u64..=0x6708).collect();
    for &seed in &seeds {
        let st = capacity_step_with_jitter(seed, 1_000).await;
        report(&format!("P-B jitter {seed:#x}"), &st.r);
        let (reached, low) = step_outcome(&st);
        let s = &st.r.h264.stats;
        println!("  reached {reached:?} us, lowest/pre-min {low:.2}");
        println!(
            "  extra idrs={} verdicts p={} time={} pkt={}",
            s.frames_idr.saturating_sub(1),
            s.verdicts_p_frag,
            st.r.transport.instr.loss_by_time_threshold,
            st.r.transport.instr.loss_by_packet_threshold
        );

        assert!(
            s.probe_padding_windows > 0,
            "seed {seed:#x}: premise: padding was used"
        );
        assert_eq!(
            st.r.s2c_datagrams_dropped, 0,
            "seed {seed:#x}: premise: nothing dropped"
        );
        loss_total += s.frames_idr.saturating_sub(1);
        in_probe += s.p_verdicts_in_probe;
        assert!(low >= 0.8, "seed {seed:#x}: lowest/pre-min {low:.2}");
        found += reached.is_some_and(|t| t <= T_UP_US) as usize;
    }
    assert!(
        found + 1 >= seeds.len(),
        "the headroom was found within {T_UP_US} us on only {found} of {} seeds",
        seeds.len()
    );
    println!(
        "loss IDRs over the {} seeds: {loss_total}, of them in a probe: {in_probe}",
        seeds.len()
    );
    assert_eq!(
        in_probe, 0,
        "a P-frame loss verdict fell in a probe burst, with padding in flight or a window open"
    );
    // The loss verdicts left (2 on seed 0x6701, 2 on 0x6703, none near a probe
    // or a pad) are the loss path's own jitter sensitivity with large frames:
    // the same scene with padding switched off makes 3 on seed 0x6701. They are
    // bounded, not zero.
    let _ = loss_total;
}

/// At 2 ms of jitter the loss path judges many datagrams lost for reasons of
/// its own (30 to 40 loss IDRs over these seeds, with or without a probe), so
/// that is not asserted. What is: no verdict falls on a datagram that departed
/// inside a probe burst (`p_verdicts_in_probe`, classified by departure). This
/// is the scene that tells the burst patience is doing something: with the
/// factor at 1 (no patience) the counter reads 2 here and 0 with it.
#[tokio::test(start_paused = true)]
async fn probe_bursts_cause_no_verdicts_on_a_jittery_link() {
    let mut in_probe = 0;
    let mut padded = 0;
    for seed in [0x6701u64, 0x6702, 0x6703] {
        let st = capacity_step_with_jitter(seed, 2_000).await;
        let s = &st.r.h264.stats;
        println!(
            "jitter 2 ms {seed:#x}: extra idrs={} verdicts in a probe burst={} padding windows={}",
            s.frames_idr.saturating_sub(1),
            s.p_verdicts_in_probe,
            s.probe_padding_windows
        );
        in_probe += s.p_verdicts_in_probe;
        padded += s.probe_padding_windows;
    }
    assert!(padded > 0, "premise: padding was used");
    assert_eq!(in_probe, 0, "a verdict fell on a datagram of a probe burst");
}

/// One run of the capacity-step scene, reduced to what the population gates
/// read.
struct StepSummary {
    seed: u64,
    /// Time from the step to the first estimate sample at `0.8 x` the new
    /// capacity, if reached. Mostly a transient spike on an in-place stream:
    /// the gates read `sus_use` there.
    reached: Option<u64>,
    /// The sustained metric (see [`sustained`]); `sus_use` is `None` on a
    /// stream with no access-unit log (the legacy scripted encoder), which is
    /// judged on `reached`.
    sus_est: Option<u64>,
    sus_use: Option<u64>,
    has_au_log: bool,
    /// Lowest estimate within `T_UP` of the step over the pre-step minimum.
    low: f64,
    /// Lowest estimate from the step to the end of the run over the pre-step
    /// minimum: the link is lossless and only ever faster after the step, so
    /// anything under half is the loop collapsing.
    min_all: f64,
    /// Share of the run's ceiling readings that found the sender saturated.
    sat_share: f64,
    /// The headroom guard let go in time (see
    /// `the_headroom_guard_releases_in_h264_mode_on_a_capacity_step`).
    guard_ok: bool,
    /// Time from the step to the guard letting go, if it did.
    guard_us: Option<u64>,
    /// Probe-padding windows the run filled: the premise that the stream is
    /// not application-limited and the probes had something to fill.
    padding_windows: u64,
    /// Times the policy moved the rate up in place.
    applies_up: u64,
    /// Frames the rate model cut below the content's demand (the cap or the
    /// VBV decided): the premise that the stream was held to the rate.
    model_limited: u64,
    /// Bytes the model gave the frames over the bytes the rates told allow.
    bytes_vs_target: f64,
    /// Padding bytes sent, probe windows the raise used, bytes of the frames
    /// coded at a window's rate, raised frames a verdict named, and the
    /// longest a raised frame took to be fully acknowledged.
    padding_bytes: u64,
    raise_windows: u64,
    raise_bytes: u64,
    raised_lost: u64,
    raised_delay_max_us: u64,
    /// Probe pads the loss path judged lost (each pauses the raise).
    pads_lost: u64,
}

impl StepSummary {
    /// Found the headroom within `T_UP`: sustained and used on an access-unit
    /// logged stream, the first sample otherwise.
    fn hit(&self) -> bool {
        let t = if self.has_au_log {
            self.sus_use
        } else {
            self.reached
        };
        t.is_some_and(|t| t <= T_UP_US)
    }
    /// The time `hit` reads.
    fn step_up_us(&self) -> Option<u64> {
        if self.has_au_log {
            self.sus_use
        } else {
            self.reached
        }
    }
    fn collapsed(&self) -> bool {
        self.min_all < 0.5
    }
}

/// `ack_chatter` adds one client-to-server packet per delivery (see
/// `RunOptions::ack_chatter`): the same scene with the client-to-server timing
/// earlier, which must not change what the estimate finds.
async fn step_summary(
    seed: u64,
    ack_chatter: Option<Duration>,
    delay_us: u64,
    mode: StepMode,
) -> StepSummary {
    step_summary_deep(seed, ack_chatter, delay_us, mode, LINK_BUFFER_MS).await
}

async fn step_summary_deep(
    seed: u64,
    ack_chatter: Option<Duration>,
    delay_us: u64,
    mode: StepMode,
    buffer_ms: u64,
) -> StepSummary {
    let st = capacity_step_deep(seed, 0, ack_chatter, delay_us, mode, buffer_ms).await;
    let (reached, low) = step_outcome(&st);
    let (sus_est, sus_use, has_au_log) = sustained(&st);
    let min_all = st
        .post
        .iter()
        .map(|x| x.1)
        .min()
        .expect("samples after the step");
    let b = &st.r.transport.instr.bwe;
    let (engaged, released) = guard_release(&st);
    StepSummary {
        seed,
        reached,
        sus_est,
        sus_use,
        has_au_log,
        low,
        min_all: min_all as f64 / st.pre_min as f64,
        sat_share: b.ceiling_saturated_reads as f64 / b.ceiling_reads.max(1) as f64,
        guard_ok: engaged && released.is_some_and(|t| t <= T_UP_US + 2_000_000),
        guard_us: released,
        padding_windows: st.r.h264.stats.probe_padding_windows,
        applies_up: st
            .r
            .h264_run()
            .map_or(0, |x| x.analyse().refs.rate_applies_up),
        model_limited: st.r.h264_run().map_or(0, |x| {
            x.truth.model.frames_capped + x.truth.model.frames_bucket_empty
        }),
        bytes_vs_target: st.r.h264_run().map_or(0.0, |x| {
            x.truth.model.bytes as f64 / x.truth.model.allowed_bytes.max(1) as f64
        }),
        padding_bytes: st.r.h264.stats.probe_padding_bytes,
        raise_windows: st.r.h264.refs.probe_raise_windows,
        raise_bytes: st.r.h264.refs.probe_raise_bytes,
        raised_lost: st.r.h264.refs.raised_frame_lost,
        raised_delay_max_us: st.r.h264.refs.raised_delivery_us_max,
        pads_lost: st.r.h264.stats.pads_lost,
    }
}

/// The sustained step-up metric (design 4.3 as reviewed): `(sus_est, sus_use)`,
/// microseconds from the step.
///
///  * `sus_est`: the first `t` on a 100 ms grid such that every estimate sample
///    in `[t, t + 1 s]` is at least `0.8 x` the new capacity;
///  * `sus_use`: the same `t`, and the access units captured in `[t, t + 1 s)`
///    total at least `0.8 x` that rate (the stream *uses* what the estimate
///    says, so a spike nobody sent into is not a step-up).
///
/// "Reached" (the first sample over the line) counts a transient spike: it
/// read 96/95/95 where `sus_use` reads 24/21/18 of 32 (review 2, 2026-11-06).
/// `None` for `sus_use` without an access-unit log.
fn sustained(st: &StepRun) -> (Option<u64>, Option<u64>, bool) {
    let thr = STEP_TO_BPS * 8 / 10;
    let aus: Vec<(u64, usize)> =
        st.r.h264_run()
            .map(|h| {
                h.truth
                    .au_log
                    .iter()
                    .map(|a| (u64::from(a.timestamp_us), a.bytes))
                    .collect()
            })
            .unwrap_or_default();
    let has_log = !aus.is_empty();
    let enc_bps = |from: u64| -> f64 {
        aus.iter()
            .filter(|(t, _)| (from..from + 1_000_000).contains(t))
            .map(|a| a.1)
            .sum::<usize>() as f64
            * 8.0
    };
    let end = st.post.last().map_or(STEP_AT_US, |x| x.0);
    let (mut sus_est, mut sus_use) = (None, None);
    let mut t = STEP_AT_US;
    while t + 1_000_000 <= end {
        let mut win = st
            .post
            .iter()
            .filter(|(s, _)| (t..=t + 1_000_000).contains(s))
            .map(|x| x.1)
            .peekable();
        let est_ok = win.peek().is_some() && win.all(|b| b >= thr);
        if est_ok && sus_est.is_none() {
            sus_est = Some(t - STEP_AT_US);
        }
        if est_ok && has_log && enc_bps(t) >= 0.8 * thr as f64 {
            sus_use = Some(t - STEP_AT_US);
            break;
        }
        t += 100_000;
    }
    (sus_est, sus_use, has_log)
}

/// Median and 90th percentile of the times that exist, and how many seeds had
/// none (never reached), as `p50=.. p90=.. none=..` in milliseconds.
fn percentiles_ms(times: impl Iterator<Item = Option<u64>>) -> String {
    let mut t: Vec<u64> = Vec::new();
    let mut none = 0;
    for x in times {
        match x {
            Some(v) => t.push(v),
            None => none += 1,
        }
    }
    t.sort_unstable();
    if t.is_empty() {
        return format!("p50=- p90=- none={none}");
    }
    let at = |q: usize| t[(t.len() * q / 100).min(t.len() - 1)] / 1000;
    format!("p50={} p90={} none={none}", at(50), at(90))
}

/// The population behind `P_B_SEEDS`: `GF_POP_N` timelines (24 by default)
/// from `0x6701 + GF_POP_START`, each run as it comes and with one extra
/// client packet per delivery, at once and 8 ms later, and how many meet each bound. Ignored (it asserts
/// nothing and takes a minute per hundred seeds): run it to see the numbers,
/// which are the honest account of how reliably the bounds are met.
///
/// Measured over the 192 timelines 0x6701..=0x6760 (three times, see the rows), before and after the saturation signal was
/// made to measure the path (a window that binds only counts with a queue on
/// the path; a window across an acknowledgement gap is no new bound):
///
/// | | reach 0.8 x 6 in 11 s | lower bound | collapses (< 0.5) |
/// | before, as sent | 178/192 | 183/192 | 15 |
/// | before, a packet at once | 181/192 | 172/192 | 27 |
/// | before, a packet 8 ms later | 151/192 | 185/192 | 10 |
/// | after, as sent | 185/192 | 189/192 | 0 |
/// | after, a packet at once | 185/192 | 192/192 | 0 |
/// | after, a packet 8 ms later | 187/192 | 190/192 | 0 |
///
/// `GF_POP_MODE=refs` runs the acked-reference twin of the scene (a PCM
/// encoder that sizes frames from the rate, the in-place rate policy; the
/// default).
#[tokio::test(start_paused = true)]
#[ignore = "characterisation: prints the P-B outcome of GF_POP_N seeds, three times"]
async fn p_b_seed_population() {
    let n: u64 = std::env::var("GF_POP_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    let start: u64 = std::env::var("GF_POP_START")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // `GF_POP_DELAY_US`: the one-way delay of the link (default 20 ms).
    let delay_us: u64 = std::env::var("GF_POP_DELAY_US")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(NORMAL_DELAY_US);
    // `GF_POP_BUFFER_MS`: the link buffer (default 600 ms of the link's rate).
    let buffer_ms: u64 = std::env::var("GF_POP_BUFFER_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(LINK_BUFFER_MS);
    let mode = StepMode::from_env();
    let only_row: Option<usize> = std::env::var("GF_POP_ROW")
        .ok()
        .and_then(|v| v.parse().ok());
    for (row, chatter) in CHATTER.into_iter().enumerate() {
        if only_row.is_some_and(|r| r != row) {
            continue;
        }
        let mut all = Vec::new();
        for i in start..start + n {
            let s = step_summary_deep(0x6701 + i, chatter, delay_us, mode, buffer_ms).await;
            println!(
                "SEED {:#x} {mode:?} chatter={chatter:?} reached={:?} sus_est={:?} sus_use={:?} guard={:?} lowest/pre-min={:.2} min/pre-min={:.2} saturated={:.2} guard_ok={} padding_windows={} applies_up={} bytes_vs_target={:.3} padding_bytes={} raise_windows={} raise_bytes={} raised_lost={} raised_delay_max_ms={} pads_lost={}",
                s.seed, s.reached, s.sus_est, s.sus_use, s.guard_us, s.low, s.min_all, s.sat_share, s.guard_ok,
                s.padding_windows, s.applies_up, s.bytes_vs_target, s.padding_bytes,
                s.raise_windows, s.raise_bytes, s.raised_lost, s.raised_delay_max_us / 1000, s.pads_lost
            );
            all.push(s);
        }
        let count = |f: &dyn Fn(&StepSummary) -> bool| all.iter().filter(|s| f(s)).count();
        println!(
            "SUMMARY {mode:?} chatter={chatter:?} n={n}: reached {}, sus_est {}, sus_use {} (hits), lower bound kept {}, collapses {}, guard released {}, mean saturated share {:.2}; mean bytes/target {:.3}; step-up ms {} (sus_use); reached-step-up ms {}; guard ms {}",
            count(&|s| s.reached.is_some_and(|t| t <= T_UP_US)),
            count(&|s| s.sus_est.is_some_and(|t| t <= T_UP_US)),
            count(&|s| s.hit()),
            count(&|s| s.low >= 0.8),
            count(&|s| s.collapsed()),
            count(&|s| s.guard_ok),
            all.iter().map(|s| s.sat_share).sum::<f64>() / all.len().max(1) as f64,
            all.iter().map(|s| s.bytes_vs_target).sum::<f64>() / all.len().max(1) as f64,
            percentiles_ms(all.iter().map(|s| s.step_up_us())),
            percentiles_ms(all.iter().map(|s| s.reached)),
            percentiles_ms(all.iter().map(|s| s.guard_us)),
        );
    }
}

/// What a capacity drop 6 -> 2 Mbit/s costs a CBR stream.
struct DropSummary {
    /// Time from the drop to the first estimate within 1.2 x the new capacity.
    reacted_us: Option<u64>,
    /// Frames skipped for backpressure over the run (the queue the stale
    /// estimate built).
    skips: u64,
}

async fn step_down_summary(seed: u64, delay_us: u64) -> DropSummary {
    const DOWN_AT_US: u64 = 8_000_000;
    let net = bottleneck_net(
        CapTimeline::step(STEP_TO_BPS / 8, DOWN_AT_US, STEP_FROM_BPS / 8),
        delay_us,
        0,
        600,
    );
    let r = run(
        scene(seed, 24, net, refs_cbr_load(), DropPlan::default()),
        RunOptions::default(),
    )
    .await
    .expect("scene");
    let reacted_us = r
        .bwe_estimate_samples
        .iter()
        .find(|(t, b)| *t >= DOWN_AT_US && *b <= STEP_FROM_BPS * 12 / 10)
        .map(|(t, _)| t - DOWN_AT_US);
    DropSummary {
        reacted_us,
        skips: r.h264.stats.frames_skipped_backpressure,
    }
}

/// The quiet screen of the stall scene on a link with the given one-way delay:
/// `(lowest estimate from the end of the stall, estimate at the end)`.
async fn quiet_screen(seed: u64, chatter: Option<Duration>, delay_us: u64) -> (u64, u64) {
    let mut l = refs_cbr_load();
    l.quiet = Some((90, 130));
    let opts = RunOptions {
        ack_blackout: Some((Duration::from_millis(3_100), Duration::from_millis(3_900))),
        ack_chatter: chatter,
        ..RunOptions::default()
    };
    let mut net = link_20ms();
    net.delay_us = delay_us;
    net.jitter_us = 2_000.min(delay_us);
    let r = run(scene(seed, 7, net, l, DropPlan::default()), opts)
        .await
        .expect("scene");
    let lowest_after = r
        .bwe_estimate_samples
        .iter()
        .filter(|(t, _)| *t >= 3_900_000)
        .map(|x| x.1)
        .min()
        .expect("samples after the stall");
    (lowest_after, r.bwe_estimate_bps)
}

/// The acked-reference twin of the gate above: the same rows and seeds on an
/// acked-reference session (`refs_cbr_load`: a PCM encoder that sizes its
/// frames from the rate it is told, the in-place rate policy). Tightened per
/// row, as allowed misses (the rows are n = 24/8/8/15/6/4, where a percentage
/// says little): no collapse, the headroom found, the guard released.
///
/// Measured at `GF_POP_N=96` (`p_b_seed_population`, `GF_POP_MODE`), rows as
/// sent / a packet at once / a packet 8 ms later, on the 20 ms path. Reached
/// 0.8 x 6 Mbit/s within 11 s, guard released, step-up p50/p90 (ms):
///
/// | mode | reached | guard | step-up |
/// | legacy (scripted CBR) | 94/95/95 | 92/96/89 | 2100/7100, 2400/5600, 4200/7000 |
/// | refs, bps-only model, in place | 86/95/93 | 96/96/96 | 1900/12500, 2000/7600, 2000/7200 |
/// | refs, bps-only model, Phase A | 89/91/86 | 96/96/96 | 1900/7300, 2000/7700, 5300/10500 |
/// | refs, S2-calibrated full model (superseded), in place | 84/87/86 | 96/96/96 | 2000/11700, 2200/9700, 2300/10800 |
/// | refs, S2-calibrated full model (superseded), Phase A | 91/95/90 | 96/96/96 | 6300/9100, 2200/6700, 2200/7600 |
/// | refs, S2-calibrated, every step 1.8x (`refs-full-transient`) | 96/95/96 | 96/96/96 | 2000/5200, 2400/6300, 2300/6000 |
/// | refs, first VCE calibration (1.47x, two frames), in place | 90/90/92 | 96/96/96 | 5500/7900, 2200/10500, 5900/8400 |
/// | refs, first VCE calibration, Phase A | 95/93/90 | 96/96/96 | 3900/6600, 5200/7400, 5700/8000 |
/// | refs, re-send-burst model (B6), in place, 15 % rise band, no refresh | 90/90/92 | 96/96/96 | 1900/9200, 2100/8100, 2000/7800 |
/// | refs, re-send-burst model (B6), Phase A | 96/94/93 | 96/96/96 | 4200/6500, 2200/7100, 4000/7100 |
/// | refs, 40 %-fullness re-send model (T4 sweep), in place, 10 % rise band | 87/87/77 | 96/96/96 | 1900/10300, 2000/10600, 2100/12900 |
/// | refs, 40 %-fullness re-send model, in place, 15 % rise band (shipped) | 88/89/85 | 96/96/96 | 1900/10600, 1900/8600, 1900/12500 |
/// | refs, 40 %-fullness re-send model, Phase A | 90/86/86 | 96/96/96 | 5500/7300, 5900/11800, 5700/10900 |
/// | refs, 1 s spike hold, probe raise on | 79/80/80 | 96/96/96 | 1200/12600, 1300/12400, 1300/12300 |
/// | refs, 2 s spike hold, raise off | 94/96/90 | 96/96/96 | 1900/6600, 1900/6000, 1900/7300 |
/// | refs, 2 s spike hold, raise on (shipped) | 96/95/95 | 96/96/96 | 1200/5500, 1300/5400, 1300/5700 |
/// | refs, 2 s spike hold, raise on, model yield 0.7 | 94/93/92 | 96/96/96 | 4600/6100, 4500/6600, 5500/6700 |
/// | refs, Phase A policy, raise off (the pre-B6 policy) | 90/86/86 | 96/96/96 | 5500/7300, 5900/11800, 5700/10900 |
///
/// Every row above counts the *first estimate sample* over the line, which a
/// probe's transient spike gives; review 2 (2026-11-06) replaced it with the
/// sustained metric (`sustained`: `sus_est`, `sus_use`), which the gates read.
/// Re-measured on it, at `GF_POP_N=96` (`reached/sus_est/sus_use` hits per ack
/// timing; `sus_use` step-up p50/p90 in ms):
///
/// | mode | reached/sus_est/sus_use | sus_use step-up |
/// | refs, 2 s releasable hold, raise on (shipped) | 96/85/70, 96/89/67, 95/85/71 | 4500/11000, 5200/9900, 4500/5500 |
/// | the same, raise mutated off (`refs-no-raise`, was the shipped row) | 90/66/51, 95/66/58, 91/63/55 | 4100/16800, 4200/11000, 4300/6600 |
/// | the same, raise at the pessimistic yield 0.86 (`refs-pess-raise`) | 96/79/57, 94/53/36, 92/37/21 | 4800/10700, 4400/18500, 6200/17800 |
/// | refs, 2 s hold never released (`refs-hold-fixed`) | 90/66/52, 95/67/58, 91/64/56 | 4700/17000, 4700/20400, 4800/14000 |
/// | refs, 1 s hold, raise off (before the raise was shipped) | 92/67/28, 84/52/16, 81/52/18 | 11100/17300, 11300/17900, 8300/22300 |
/// | refs, 1 s hold, raise on (`refs-hold-1s`, the should_panic gate) | 66/23/13, 45/11/3, 67/27/21 | 14400/21400, 14900/21700, 12600/16000 |
/// | refs, Phase A policy (`refs-phase-a`) | 90/74/60, 81/61/48, 85/55/43 | 10400/20500, 10600/17300, 10800/18100 |
/// | refs, raise on with the "padding judged" guard (removed) | 95/74/55, 92/63/28, 93/64/44 | 5300/16900, 6600/20000, 6000/11800 |
/// | refs before review 2 (stale spike, 2 s hold, raise on) | 96/78/69, 95/71/65, 95/72/62 | 4400/8300, 4400/10900, 4400/11300 |
///
/// The gate's model is the 40 %-fullness re-send (`pcm_rate.rs`, provisional,
/// from T4 `h264_direct_resend_fullness_sweep` on the VCE): a P re-send of the
/// rate buffers gives frames of 1.00, 0.56 and 0.87 times the average, the
/// same for a rise, a fall and a pure re-send, and a raised IDR reaches 0.6 of
/// its ceiling. The burst model it replaces (1.8x / 1.7x / 1.4x on the first
/// frame, the backend re-sending the HRD at 75 %) is `GF_POP_MODE=refs-burst`.
/// Mean bytes over the bytes the rates told allow: 0.993 in place (burst
/// model: 1.019), 0.996 Phase A (1.013). `refs-linear-step` is the optimistic reading of the step-size dependence.
///
/// With the 2 s spike hold of the in-place policy and the probe raise (the
/// shipped rows) the twin reaches the headroom (first sample) on 96/96/95 of 96
/// timelines, no collapse and the guard released on all, and uses it
/// (`sus_use`) on 70/67/71; the raise cuts the padding per probe window to
/// 66 % of its no-raise figure (scene 9). On links that buffer less than a
/// window's burst it costs headroom found (scene 9, shallow).
///
/// No collapse in any. The twin never misses the guard (the 89/96 of the
/// legacy gate is the legacy 8 ms row only). The full model is the gate's;
/// its in-place policy now reaches the headroom as often as Phase A's (88/89/85
/// against 90/86/86) and in a median 1.9 s against 5.5-5.9 s; the tail (p90
/// 8.6-12.5 s against 7.3-11.8 s) is no better. Premise: probes were
/// padded, the policy moved the rate up in place and the model held frames to
/// the rate on every seed.
#[tokio::test(start_paused = true)]
async fn p_b_population_refs_twin_never_collapses_and_finds_the_headroom() {
    population_gate(StepMode::Refs, Some(REFS_TWIN_ALLOWED_MISSES)).await;
}

/// The twin with a pessimistic raise: the model gives a raise 0.86 of the bytes
/// the VCE's profile does (`PcmRateModel::raise_yield`), the VCE's minimum
/// (mean yield 0.63, T4 lever B). Same bound as the
/// raise at the measured yield: the stream does not collapse and does not lose
/// the headroom for a backend that yields less.
#[tokio::test(start_paused = true)]
async fn p_b_population_refs_twin_with_a_pessimistic_raise_yield() {
    population_gate(
        StepMode::RefsPessRaise,
        Some(REFS_TWIN_PESSIMISTIC_ALLOWED_MISSES),
    )
    .await;
}

/// The gate has power: the policy with a 1 s spike hold (which breaks the
/// probe chain: every give-back reads as a cut, 28/16/18 of 96 reach and use
/// the headroom against 52/58/55) misses it. A mutation test that stays: it
/// fails if the gate's bounds are loosened until they pass the broken policy.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "missed the headroom")]
async fn p_b_population_refs_twin_gate_fails_a_one_second_spike_hold() {
    population_gate(StepMode::RefsHold1s, Some(REFS_TWIN_ALLOWED_MISSES)).await;
}

/// Misses allowed per gate row: `(finding the headroom, releasing the guard)`,
/// where finding it is `sus_use`: the estimate held at `0.8 x` the new
/// capacity for a second *and* the stream sending at least `0.8 x` that in the
/// same second, within `T_UP` (the first sample over the line, which a
/// transient spike gives, read 96/95/95 of 96 for a stream that used 70/67/71).
/// Rows are n = 24/8/8/15/6/4.
///
/// Measured (2026-11-06) on the shipped twin (`StepMode::Refs`, the 2 s
/// releasable hold, the probe raise on): 9, 3, 3, 7, 0, 0 misses; the guard is
/// always released. The allowance is that plus a margin of 2 timelines (the
/// rows of 6 and 4 timelines get 2 on a measured 0), because the stepped
/// harness is chaotic (one timing tweak moves a timeline across the line); it is
/// not equal to the measurement. The margin is bounded by what the gate must
/// still catch: the 1 s hold misses 20 of the first row's 24
/// (`p_b_population_refs_twin_gate_fails_a_one_second_spike_hold`), against the
/// 11 allowed.
const REFS_TWIN_ALLOWED_MISSES: [(usize, usize); 6] =
    [(11, 0), (5, 0), (5, 0), (9, 0), (2, 0), (2, 0)];

/// The same rows with the raise at the pessimistic yield (0.86, the VCE's
/// minimum): 12, 6, 5, 2, 0, 1 measured. The raise at that yield finds the
/// headroom on fewer timelines than the raise off does (96-seed population,
/// `sus_use` 57/36/21 against 51/58/55): a bound that it does not collapse and
/// does not lose more than this, not a claim that it gains.
const REFS_TWIN_PESSIMISTIC_ALLOWED_MISSES: [(usize, usize); 6] =
    [(14, 0), (7, 0), (7, 0), (4, 0), (2, 0), (3, 0)];

/// The gate's rows: seeds, ack chatter and one-way delay.
type PopulationRow = (Option<Duration>, &'static [u64], u64);

/// The gate over the rows of `p_b_population_never_collapses_and_finds_the_headroom_whenever_the_client_acks`
/// for `mode`. `allowed_misses` is `None` for the legacy percentages, or, per
/// row, the misses tolerated for reaching the headroom and for the guard (rows
/// are n = 24/8/8/15/6/4, so a percentage means little on the small ones).
async fn population_gate(mode: StepMode, allowed_misses: Option<[(usize, usize); 6]>) {
    const AS_SENT: [u64; 24] = [
        0x6701, 0x6702, 0x6703, 0x6704, 0x6705, 0x6706, 0x6707, 0x6708, 0x6709, 0x670a, 0x670b,
        0x670c, 0x670d, 0x670e, 0x670f, 0x6710, 0x6715, 0x6720, 0x6724, 0x6727, 0x673a, 0x6746,
        0x6753, 0x675a,
    ];
    const ACKED_AT_ONCE: [u64; 8] = [
        0x6704, 0x670e, 0x6715, 0x671f, 0x6722, 0x6725, 0x672f, 0x6733,
    ];
    // The same scene on a 4 ms round trip, where the client's 5 ms flush and
    // the frame interval are longer than the round trip: the timelines that
    // collapsed on master there (23, 19 and about 20 of 96 under the three
    // timings), and the first eight as they come.
    const LOW_AS_SENT: [u64; 15] = [
        0x6701, 0x6702, 0x6703, 0x6704, 0x6705, 0x6706, 0x6707, 0x6708, 0x670c, 0x6714, 0x6717,
        0x6719, 0x6727, 0x6728, 0x672f,
    ];
    const LOW_AT_ONCE: [u64; 6] = [0x6703, 0x6704, 0x6708, 0x670d, 0x670e, 0x6716];
    const LOW_LATE: [u64; 4] = [0x6701, 0x6703, 0x6715, 0x671a];
    let rows: [PopulationRow; 6] = [
        (None, &AS_SENT[..], NORMAL_DELAY_US),
        (Some(Duration::ZERO), &ACKED_AT_ONCE[..], NORMAL_DELAY_US),
        (Some(CHATTER_LATE), &ACKED_AT_ONCE[..], NORMAL_DELAY_US),
        (None, &LOW_AS_SENT[..], LOW_DELAY_US),
        (Some(Duration::ZERO), &LOW_AT_ONCE[..], LOW_DELAY_US),
        (Some(CHATTER_LATE), &LOW_LATE[..], LOW_DELAY_US),
    ];
    for (row, (chatter, seeds, delay_us)) in rows.into_iter().enumerate() {
        let mut all = Vec::new();
        for &seed in seeds {
            all.push(step_summary(seed, chatter, delay_us, mode).await);
        }
        let n = all.len();
        let hits = all.iter().filter(|s| s.hit()).count();
        let released = all.iter().filter(|s| s.guard_ok).count();
        let share = all.iter().map(|s| s.sat_share).sum::<f64>() / n as f64;
        println!(
            "population {mode:?} chatter={chatter:?} delay {delay_us} us: n={n} hits={hits} (reached {}) guard={released} saturated share {share:.2} min/pre-min {:.2}; step-up ms {}; guard ms {}",
            all.iter().filter(|s| s.reached.is_some_and(|t| t <= T_UP_US)).count(),
            all.iter().map(|s| s.min_all).fold(f64::MAX, f64::min),
            percentiles_ms(all.iter().map(|s| s.step_up_us())),
            percentiles_ms(all.iter().map(|s| s.guard_us)),
        );
        {
            for s in &all {
                assert!(
                    s.padding_windows > 0 && s.applies_up > 0 && s.model_limited > 0,
                    "seed {:#x} {mode:?}: premise: probes were padded ({}), the rate moved up in place ({}) and the model held frames to it ({})",
                    s.seed, s.padding_windows, s.applies_up, s.model_limited
                );
                if mode != StepMode::RefsNoRaise {
                    assert!(
                        s.raise_windows > 0 && s.raise_bytes > 0,
                        "seed {:#x} {mode:?}: premise: frames were coded at a probe window's rate ({} windows, {} bytes)",
                        s.seed, s.raise_windows, s.raise_bytes
                    );
                }
            }
        }
        for s in &all {
            assert!(
                !s.collapsed(),
                "seed {:#x} chatter={chatter:?}: the estimate fell to {:.2} x its pre-step minimum on a lossless link",
                s.seed, s.min_all
            );
        }
        match allowed_misses {
            None => assert!(
                hits * 100 >= n * 80,
                "chatter={chatter:?}: only {hits} of {n} timelines reached the headroom"
            ),
            Some(a) => assert!(
                n - hits <= a[row].0,
                "{mode:?} row {row}: {} of {n} timelines missed the headroom (allowed {})",
                n - hits,
                a[row].0
            ),
        }
        // 6 of 8, not 7: the H.264 step-up ramp is slow and non-monotonic
        // (pre-existing; the saturation fix does not worsen it), and some
        // timelines release the guard just past T_UP + 2 s (0x670e and 0x6725
        // at +13.5 s against 13 s, on the 8 ms row). Measured at N=96 on that
        // row: 89/96 released on the fix, 89/96 before the rebase, 85/96 on
        // master without the fix (with 2 collapses). Follow-up: the slow,
        // erratic H.264 step-up ramp (B6 rate control). The collapse and
        // reached-headroom criteria stay strict.
        match allowed_misses {
            None => assert!(
                released * 100 >= n * 75,
                "chatter={chatter:?}: the headroom guard released on only {released} of {n}"
            ),
            Some(a) => assert!(
                n - released <= a[row].1,
                "{mode:?} row {row}: the guard missed on {} of {n} (allowed {})",
                n - released,
                a[row].1
            ),
        }
        // On the low-latency path the stream is saturated more often whatever
        // counts (0.74 - 0.80 here, 0.80 - 0.89 on master): no bound there.
        // The twin's stream is held to the rate it is told (the model's
        // bucket), so on the 20 ms path it is rarely saturated: 0.02 - 0.04
        // measured, 0.33 when the model ignores the rate.
        assert!(
            delay_us == LOW_DELAY_US || share < 0.15,
            "{mode:?} row {row}: {share:.2} of the readings found the stream saturated: the rate is not what sizes the frames"
        );
        assert!(
            share < 0.6 || delay_us == LOW_DELAY_US,
            "chatter={chatter:?}: {share:.2} of the readings of a CBR stream found it saturated"
        );
    }
}

/// A capacity drop 6 -> 2 Mbit/s on a CBR stream, at 4 ms and 40 ms round
/// trips: the estimate must come down to 1.2 x the new capacity within 4 s on
/// every seed, and the queue it built while it was stale must not cost more
/// than a fifth of the frames (the mean backpressure skips of 24 s at 30 fps:
/// 99 - 133 at 4 ms and 53 - 70 at 40 ms measured, 105 and 54 on master). The
/// ceiling's cuts depend on what counts as saturated, so a change to that is
/// held to the downward path here.
#[tokio::test(start_paused = true)]
async fn the_estimate_follows_a_capacity_drop_on_fast_and_slow_paths() {
    for delay_us in [LOW_DELAY_US, NORMAL_DELAY_US] {
        let mut skips = 0;
        for seed in 0x6A01u64..0x6A09 {
            let d = step_down_summary(seed, delay_us).await;
            assert!(
                d.reacted_us.is_some_and(|t| t <= 4_000_000),
                "seed {seed:#x} delay {delay_us} us: reacted after {:?}",
                d.reacted_us
            );
            skips += d.skips;
        }
        assert!(
            skips <= 8 * 144,
            "delay {delay_us} us: {skips} backpressure skips over 8 seeds"
        );
    }
}

/// The quiet screen of `an_acknowledgement_stall_on_a_quiet_screen_is_probed_not_declared_lost`,
/// over sixteen seeds with and without the client acknowledging at once:
/// the estimate must stay near the 2 Mbit/s start rate on a link that has no
/// limit. Before the fix the seeds fell in two modes: a few stayed at about
/// 2.1 Mbit/s, most were reseeded to 0.5 - 0.9 Mbit/s by a ceiling reading
/// across the acknowledgement stall and stayed there with backpressure skips.
/// Measured now: every seed ends at 2.2 - 2.4 Mbit/s.
#[tokio::test(start_paused = true)]
async fn the_quiet_screen_estimate_is_not_locked_below_the_start_rate() {
    for delay_us in [NORMAL_DELAY_US, LOW_DELAY_US] {
        for chatter in CHATTER {
            for seed in 0x3301u64..0x3311 {
                let (lowest_after, end) = quiet_screen(seed, chatter, delay_us).await;
                assert!(
                    lowest_after >= BWE_START_BPS * 4 / 5 && end >= BWE_START_BPS * 4 / 5,
                    "seed {seed:#x} chatter={chatter:?} delay {delay_us} us: the estimate fell to {lowest_after} and ended at {end} on an uncapped link"
                );
            }
        }
    }
}

/// Replay the run's estimate through the classifier's headroom guard: whether
/// it held H.264 before the step, and how long after the step it let go.
fn guard_release(st: &StepRun) -> (bool, Option<u64>) {
    use ghostframe_lib::config::ClassifierConfig;
    use ghostframe_lib::tile::classifier::{AdaptationContext, Classifier};
    use ghostframe_lib::tile::{CodecState, FrameMode};
    let mut c = Classifier::new(ClassifierConfig::default());
    let tentative = [CodecState::Solid];
    let mut mode = FrameMode::TileCodec;
    let mut engaged_before_step = false;
    let mut released_at = None;
    for (t_us, bps) in &st.r.bwe_estimate_samples {
        c.set_adaptation_context(AdaptationContext {
            supports_h264: true,
            bwe_estimate_bps: *bps,
            smoothed_rtt_us: 40_000.0,
            loss_rate: 0.0,
            suspended: false,
            last_update_seq: 1,
            estimate_known: true,
            ..Default::default()
        });
        let was_h264 = mode == FrameMode::H264;
        mode = c.decide_frame_mode_at(*t_us, &tentative, mode);
        if *t_us < STEP_AT_US && mode == FrameMode::H264 {
            engaged_before_step = true;
        }
        if *t_us >= STEP_AT_US && was_h264 && mode == FrameMode::TileCodec {
            released_at.get_or_insert(*t_us - STEP_AT_US);
        }
    }
    (engaged_before_step, released_at)
}

/// The classifier's headroom guard keeps the session in H.264 while the
/// estimate is under 2 Mbit/s and lets go after it has stayed over 3 Mbit/s for
/// a second. A CBR stream at 0.8 x its estimate cannot raise the estimate by
/// itself (see `a_capacity_step_up_is_found_within_t_up_in_h264_mode`), so
/// without the application's probes the guard would hold forever after a
/// capacity step. This replays the estimate of that scene's run, sample by
/// sample, through a real `Classifier` (the guard's exit is the classifier's
/// own decision, on the estimate the bridge hands it) and asserts: the guard
/// engages before the step, and releases within `T_UP` of it.
///
/// Mutation: with the application's probing disabled the estimate never
/// reaches the exit line and this fails (the report lists it).
#[tokio::test(start_paused = true)]
async fn the_headroom_guard_releases_in_h264_mode_on_a_capacity_step() {
    for seed in [0x6701u64, 0x6702, 0x6703] {
        let st = capacity_step(seed).await;
        let (engaged_before_step, released_at) = guard_release(&st);
        println!("guard seed {seed:#x}: engaged before step: {engaged_before_step}, released {released_at:?} us after it");
        assert!(
            engaged_before_step,
            "seed {seed:#x}: premise: the guard held H.264 on the 2 Mbit/s link"
        );
        assert!(
            released_at.is_some_and(|t| t <= T_UP_US + 2_000_000),
            "seed {seed:#x}: the guard released {released_at:?} us after the step (bound: T_UP plus its 1 s hold and a margin)"
        );
    }
}

#[path = "browserless_h264/openh264_scenes.rs"]
mod openh264_scenes;
#[path = "browserless_h264/refs_scenes.rs"]
mod refs_scenes;

/// What scene 9 adds up over its seeds for one mode.
#[derive(Debug, Default)]
struct RaiseTotals {
    padding_bytes: u64,
    windows: u64,
    raise_bytes: u64,
    raise_windows: u64,
    raised_lost: u64,
    raised_frames: u64,
    delivery_max_us: u64,
    abandoned: u64,
    min_raise_bytes_per_run: u64,
}

impl RaiseTotals {
    fn padding_per_window(&self) -> f64 {
        self.padding_bytes as f64 / self.windows.max(1) as f64
    }
}

async fn raise_totals(mode: StepMode, seeds: u64) -> RaiseTotals {
    let mut t = RaiseTotals {
        min_raise_bytes_per_run: u64::MAX,
        ..RaiseTotals::default()
    };
    for i in 0..seeds {
        let st = capacity_step_on(0x6701 + i, 0, None, NORMAL_DELAY_US, mode).await;
        let (h, refs) = (&st.r.h264, &st.r.h264.refs);
        t.padding_bytes += h.stats.probe_padding_bytes;
        t.windows += st.r.transport.instr.probe_windows_closed;
        t.raise_bytes += refs.probe_raise_bytes;
        t.raise_windows += refs.probe_raise_windows;
        t.raised_lost += refs.raised_frame_lost;
        t.raised_frames += refs.probe_raise_frames;
        t.delivery_max_us = t.delivery_max_us.max(refs.raised_delivery_us_max);
        t.abandoned += st.r.probes_abandoned;
        t.min_raise_bytes_per_run = t.min_raise_bytes_per_run.min(refs.probe_raise_bytes);
    }
    t
}

/// Scene 9, probe step-up 2 -> 6 Mbit/s (design T3): the probe raise carries
/// part of each probe window, so padding fills less of it.
///
/// A property gate over 12 timelines of the P-B capacity step (`refs_cbr_load`):
/// the padding bytes per probe window with the raise (`Refs`, as shipped) are
/// at most 75 % of those without it (`RefsNoRaise`), and at most 80 % with a
/// pessimistic model (a raise yields 0.86 of what the VCE's profile gives it,
/// its minimum). Measured over these 12 timelines, 2026-11-06, as shipped (no
/// "padding judged" wait): 134 kB a window without the raise, 89 kB with it
/// (66 %), 93 kB at yield 0.86 (70 %); the bounds leave 9 and 10 points. The share is not a design number:
/// padding is sized for the shortfall after what the frames still to come are
/// expected to carry (half an average frame each, `RAISE_EXPECTED_YIELD`), so a
/// 100 ms window of three frames leaves about 60 to 70 % for padding, and the
/// bound has the room that timeline-to-timeline variation needs.
///
/// Also bounded: the raised frames' cost to the stream on a lossless link: no
/// raised frame is named by a loss verdict, the longest a raised frame took to
/// be fully acknowledged is under the link's buffer (600 ms) and a second
/// (measured 583 ms), and the windows complete (abandoned under 2 %).
///
/// Premises: the raise ran in every timeline (`probe_raise_bytes > 0`), and
/// without it never did, and padding was sent in both.
///
/// Mutation: with the raise off in the "with" run the share is 100 % and the
/// premise fails.
#[tokio::test(start_paused = true)]
async fn the_probe_raise_cuts_padding_per_window_without_costing_the_raised_frames() {
    const SEEDS: u64 = 12;
    let off = raise_totals(StepMode::RefsNoRaise, SEEDS).await;
    let on = raise_totals(StepMode::Refs, SEEDS).await;
    let pess = raise_totals(StepMode::RefsPessRaise, SEEDS).await;
    println!("scene 9: without {off:?}\n  with {on:?}\n  pessimistic {pess:?}");
    assert!(
        off.padding_bytes > 0 && off.raise_bytes == 0,
        "premise: padding without the raise, and no raise: {off:?}"
    );
    for (name, t, share) in [("measured yield", &on, 0.75), ("yield 0.86", &pess, 0.80)] {
        assert!(
            t.min_raise_bytes_per_run > 0 && t.raise_windows > 0 && t.padding_bytes > 0,
            "{name}: premise: the raise ran in every timeline: {t:?}"
        );
        let got = t.padding_per_window() / off.padding_per_window();
        println!(
            "scene 9 {name}: padding per window {:.0} B against {:.0} B without = {:.0} %",
            t.padding_per_window(),
            off.padding_per_window(),
            got * 100.0
        );
        assert!(
            got <= share,
            "{name}: padding per window is {:.0} % of the no-raise figure (bound {:.0} %)",
            got * 100.0,
            share * 100.0
        );
        assert_eq!(
            t.raised_lost, 0,
            "{name}: a raised frame was named by a verdict: {t:?}"
        );
        assert!(
            t.raised_frames > 0 && t.delivery_max_us <= 600_000 + 400_000,
            "{name}: a raised frame took {} us to be acknowledged",
            t.delivery_max_us
        );
        assert!(
            t.abandoned * 50 <= t.windows,
            "{name}: {} of {} windows were abandoned",
            t.abandoned,
            t.windows
        );
    }
}

/// Scene 9, shallow buffers: the probe raise puts real frames where padding was,
/// so a buffer shallower than the window's burst tail-drops them. The scene
/// above runs on a lossless 600 ms buffer, where nothing can be lost; this one
/// runs the P-B step on links that buffer 30 and 60 ms. This is the trade the
/// raise is shipped with (it finds the headroom on 70/67/71 of 96 timelines at
/// 600 ms against 52/58/55 without it), so the bounds are those of a cost, not
/// of parity: against the raise off (`RefsNoRaise`), on the same 16 seeds,
///
///  * `sus_use` hits at least half the raise-off figure (it is lower: the raise
///    loses frames there; measured 0.60 and 0.64 of it);
///  * the raised frames lost average at most `LOST_PER_RUN` a run (bounded, not
///    zero: a burst on a buffer smaller than itself loses the frames behind its
///    front).
///
/// Premises: the raise engaged (`windows > 0`) and did lose frames at 30 ms
/// (the burst does overrun that buffer), and the raise-off runs lose none.
///
/// Measured (2026-11-06, shipped, these 16 seeds, as sent): 30 ms `sus_use`
/// 6 against 10 raise off, 60 raised frames lost (3.75 a run) in 30 raise
/// windows; 60 ms 7 against 11, 37 lost (2.3 a run) in 102 windows. On 48
/// timelines (16 seeds, three ack timings): 26 against 30 at 30 ms, 23 against 31
/// at 60 ms.
///
/// Mutations: no pause after a lost raised frame or pad (`RAISE_LOSS_PAUSE`
/// zero) brings the losses above `LOST_PER_RUN` (see the numbers printed); the
/// guards are also tested per plan in the lib
/// (`the_raise_checks_its_guards_for_every_raised_plan_and_pauses_after_a_loss`).
#[tokio::test(start_paused = true)]
async fn the_probe_raise_on_a_shallow_buffer_costs_a_bounded_share_of_the_raise_off() {
    const SEEDS: u64 = 16;
    const LOST_PER_RUN: f64 = 5.5;
    for buffer_ms in [30u64, 60] {
        let (mut on_hits, mut off_hits, mut lost, mut off_lost, mut windows) = (0, 0, 0, 0, 0);
        for i in 0..SEEDS {
            let seed = 0x6701 + i;
            let on =
                step_summary_deep(seed, None, NORMAL_DELAY_US, StepMode::Refs, buffer_ms).await;
            let off = step_summary_deep(
                seed,
                None,
                NORMAL_DELAY_US,
                StepMode::RefsNoRaise,
                buffer_ms,
            )
            .await;
            on_hits += usize::from(on.hit());
            off_hits += usize::from(off.hit());
            lost += on.raised_lost;
            off_lost += off.raised_lost;
            windows += on.raise_windows;
        }
        println!(
            "scene 9 shallow {buffer_ms} ms: sus_use hits {on_hits} of {SEEDS} (raise off {off_hits}); raised frames lost {lost} ({off_lost} off); raise windows {windows}"
        );
        assert!(
            windows > 0,
            "{buffer_ms} ms: premise: the raise engaged on no run"
        );
        if buffer_ms == 30 {
            assert!(
                lost > 0,
                "{buffer_ms} ms: premise: the burst overruns the buffer and loses a raised frame"
            );
        }
        assert_eq!(off_lost, 0, "{buffer_ms} ms: premise: nothing is raised");
        assert!(
            on_hits * 2 >= off_hits,
            "{buffer_ms} ms: the raise found the headroom on {on_hits} of {SEEDS}, the raise off on {off_hits}"
        );
        assert!(
            lost as f64 <= LOST_PER_RUN * SEEDS as f64,
            "{buffer_ms} ms: {lost} raised frames lost over {SEEDS} runs"
        );
    }
}
