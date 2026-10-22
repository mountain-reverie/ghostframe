//! Is the browserless harness a deterministic simulator?
//!
//! A simulator whose output depends on anything but its seed cannot be used
//! to bisect a protocol bug: a rerun that behaves differently is
//! indistinguishable from a fix. These tests run one scene twice in a single
//! process and compare what came out.
//!
//! # What is pinned
//!
//! Every counter, byte totals and the bandwidth-estimate trace included. The
//! bridge is stepped by the harness over an in-memory pipe (see the
//! `browserless` module docs), so two same-seed runs, in one process or in
//! two, produce identical results. The tests print
//! `DETERMINISM-DIGEST <name> <hash>`; the hash is the same in every process,
//! and the two whole-scene tests re-run themselves in a child process and
//! compare it. The digest covers every datagram (offer instant, direction,
//! length, verdict) and the final framebuffer as well as the counters.
//!
//! This file used to say the opposite -- behaviour reproducible, byte volume
//! not, about 16% apart on a busy lossy scene -- when the bridge ran as a
//! spawned task reading a kernel socket. Three things were behind it, and a
//! change to any of them is what these tests exist to catch:
//!
//! - the socketpair and the tokio reactor decided when the bridge saw bytes;
//! - peer lookups ran on a blocking thread;
//! - the client core sent NACKs in `HashMap` order, which differs per map and
//!   per process, so which NACKs shared a datagram varied.

use std::time::Duration;

use ghostframe_e2e::harness::browserless::{
    run_browserless, BrowserlessResult, BrowserlessScene, FrameScript, SceneLoad,
    DEFAULT_CADENCE_US,
};
use ghostframe_e2e::harness::load_profile::gradient_tile;
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::NetProfile;

fn scene() -> BrowserlessScene {
    BrowserlessScene {
        seed: 0xDE7E_0001,
        datagram_send_buffer_bytes: None,
        tick_budget_floor_bytes: None,
        load: SceneLoad::Script(vec![
            FrameScript {
                tiles: vec![
                    (
                        (0, 0),
                        TileSpec::Solid {
                            bgra: [10, 20, 30, 255],
                        },
                    ),
                    (
                        (1, 1),
                        TileSpec::Cdf53 {
                            bgra: gradient_tile(0, 1, 1),
                        },
                    ),
                ],
            },
            FrameScript {
                tiles: vec![(
                    (1, 1),
                    TileSpec::Cdf53 {
                        bgra: gradient_tile(37, 1, 1),
                    },
                )],
            },
        ]),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile::perfect(),
        drops: Default::default(),
        duration: Duration::from_secs(3),
        grid_cols: 4,
        grid_rows: 4,
    }
}

/// Every behavioural scalar worth comparing, as (name, value), so a
/// divergence names itself instead of showing up as a bare `assert_eq` on a
/// struct.
///
/// Byte totals are deliberately absent — see the module docs.
fn digest(r: &BrowserlessResult) -> Vec<(&'static str, u64)> {
    vec![
        ("events_len", r.events.len() as u64),
        ("bytes_dropped", r.bytes_dropped),
        ("stale_generation_tiles", r.stale_generation_tiles as u64),
        ("retransmit_attempts_total", r.retransmit_attempts_total),
        ("nack_hit", r.nack_hit),
        ("nack_miss", r.nack_miss),
        ("rto_fired", r.rto_fired),
        ("critical_latency_count", r.critical_latency_count),
        ("refinement_latency_count", r.refinement_latency_count),
        ("bwe_samples_seen", r.bwe_samples_seen),
        ("bwe_estimate_bps", r.bwe_estimate_bps),
        ("probes_completed", r.probes_completed),
        ("probes_abandoned", r.probes_abandoned),
    ]
}

#[tokio::test(start_paused = true)]
async fn the_same_seed_produces_the_same_behaviour() {
    let a = run_browserless(scene()).await.expect("run a");
    let b = run_browserless(scene()).await.expect("run b");

    // Premise: a digest of all zeros compares equal for the uninteresting
    // reason. Two tests in this file's own history passed vacuously before
    // someone checked, so pin down that the scene actually ran.
    assert!(
        a.events.len() > 1 && a.critical_latency_count > 0,
        "scene produced nothing to compare (events={}, critical_latency_count={}); \
         equality below would hold whether or not the harness is deterministic",
        a.events.len(),
        a.critical_latency_count
    );

    let (da, db) = (digest(&a), digest(&b));
    let diffs: Vec<String> = da
        .iter()
        .zip(db.iter())
        .filter(|((_, x), (_, y))| x != y)
        .map(|((n, x), (_, y))| format!("  {n}: {x} != {y}"))
        .collect();

    assert!(
        diffs.is_empty(),
        "the same seed produced two behaviourally different scenes:\n{}",
        diffs.join("\n")
    );
}

/// Narrows the divergence: a scene with no tiles at all exercises only the
/// QUIC/TLS handshake and the idle path. If the byte counts differ here,
/// the source is the handshake, not tile delivery.
#[tokio::test(start_paused = true)]
async fn an_empty_scene_is_byte_identical() {
    let empty = || BrowserlessScene {
        seed: 0xDE7E_0002,
        datagram_send_buffer_bytes: None,
        tick_budget_floor_bytes: None,
        load: SceneLoad::Script(vec![]),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile::perfect(),
        drops: Default::default(),
        duration: Duration::from_millis(800),
        grid_cols: 4,
        grid_rows: 4,
    };
    let a = run_browserless(empty()).await.expect("run a");
    let b = run_browserless(empty()).await.expect("run b");
    // Premise: a scene that delivered nothing is byte-identical for the
    // uninteresting reason. Without this the comparison below passes
    // whether or not a handshake ever happened.
    // A scene that delivered nothing is byte-identical for the uninteresting
    // reason. Measured: an empty scene leaves one direction at zero, so this
    // checks the combined counter -- the same premise
    // `bytes_actually_cross_the_socketpair` asserts.
    assert!(
        a.bytes_delivered > 0,
        "empty scene delivered nothing, so byte equality proves nothing \
         about handshake determinism"
    );
    // Both directions. The client's randomly skipped packet number was the
    // root cause of an earlier rare 3050-vs-3052 flake (now pinned by seeding
    // the endpoint RNGs). `c2s` used to move too (2872 / 2874 / 2905) because
    // ACK batching followed when the client's timer fired relative to
    // arrivals on a real socket; with the bridge stepped over an in-memory
    // pipe it is a function of the seed like everything else.
    assert_eq!(
        (a.bytes_delivered_s2c, a.bytes_delivered_c2s),
        (b.bytes_delivered_s2c, b.bytes_delivered_c2s),
        "the byte counts are not reproducible across two identical handshake-only scenes"
    );
    assert!(
        a.bytes_delivered_c2s > 0 && b.bytes_delivered_c2s > 0,
        "no client->server bytes at all, so the handshake did not complete \
         and the s2c comparison above proves nothing"
    );
}

/// Many runs of the handshake-only scene, not two. quinn-proto skips one
/// application-space packet number, drawn from `0..64` out of the endpoint
/// RNG; when the client's skip lands among its first few 1-RTT packets the
/// server's ACK carries an extra gap range, two bytes. With an entropy-seeded
/// client that moved `bytes_delivered_s2c` in roughly 1 run in 60, so a
/// pairwise comparison passes ~97% of the time while the cause is still
/// there; 400 runs all agree by chance under that bug about 0.2% of the
/// time. The harness pins the client endpoint's RNG to the scene seed, which
/// fixes the skipped number (it does not remove the dependence: a different
/// seed whose skip hits the trigger value would settle on 3052, stably).
#[tokio::test(start_paused = true)]
async fn the_empty_scene_s2c_bytes_are_stable_across_many_runs() {
    let mut counts = std::collections::BTreeMap::<u64, u32>::new();
    for _ in 0..400 {
        let r = run_browserless(BrowserlessScene {
            seed: 0xDE7E_0003,
            datagram_send_buffer_bytes: None,
            tick_budget_floor_bytes: None,
            load: SceneLoad::Script(vec![]),
            cadence_us: DEFAULT_CADENCE_US,
            net: NetProfile::perfect(),
            drops: Default::default(),
            duration: Duration::from_millis(800),
            grid_cols: 4,
            grid_rows: 4,
        })
        .await
        .expect("run");
        assert!(
            r.bytes_delivered_s2c > 0,
            "scene delivered nothing to the client"
        );
        *counts.entry(r.bytes_delivered_s2c).or_default() += 1;
    }
    assert_eq!(
        counts.len(),
        1,
        "server->client byte count varied across identical handshake-only scenes: {counts:?}"
    );
}

// ---------------------------------------------------------------------------
// Whole-scene reproducibility (D1 PR-H)
// ---------------------------------------------------------------------------

use std::hash::{Hash, Hasher};

/// A busy, impaired scene: sustained region churn over a 10 ms link with
/// i.i.d. loss, so retransmits, NACKs and the loss detector all run and every
/// netsim draw lands on a datagram whose identity depends on the order the
/// bridge emitted things in.
fn rich_scene() -> BrowserlessScene {
    use ghostframe_e2e::harness::load_profile::{Churn, LoadProfile, PRODUCTION_CADENCE_US};
    BrowserlessScene {
        seed: 0xDE7E_0010,
        datagram_send_buffer_bytes: None,
        tick_budget_floor_bytes: None,
        load: SceneLoad::Profile(LoadProfile {
            cadence_us: PRODUCTION_CADENCE_US,
            churn: Churn::Region { tiles_per_tick: 24 },
        }),
        cadence_us: PRODUCTION_CADENCE_US,
        net: NetProfile {
            delay_us: 10_000,
            loss: 0.03,
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(5),
        grid_cols: 16,
        grid_rows: 16,
    }
}

fn hash_of(x: &impl Hash) -> u64 {
    // `DefaultHasher::new()` uses fixed keys, so this is stable across processes.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    x.hash(&mut h);
    h.finish()
}

/// Everything a gate could read, exactly, byte totals included.
fn full_digest(r: &BrowserlessResult) -> Vec<(&'static str, u64)> {
    let mut d = digest(r);
    d.extend([
        ("bytes_delivered", r.bytes_delivered),
        ("bytes_delivered_s2c", r.bytes_delivered_s2c),
        ("bytes_delivered_c2s", r.bytes_delivered_c2s),
        ("datagrams_delivered_s2c", r.datagrams_delivered_s2c),
        ("s2c_datagrams_offered", r.s2c_datagrams_offered),
        ("s2c_datagrams_dropped", r.s2c_datagrams_dropped),
        ("s2c_loss_runs", r.s2c_loss_runs),
        ("retransmit_by_nack", r.retransmit_by_nack),
        ("rto_spurious", r.rto_spurious),
        ("rto_useful", r.rto_useful),
        ("send_datagram_errs", r.send_datagram_errs),
        ("emission_queue_peak", r.emission_queue_peak as u64),
        ("critical_latency_mean_us", r.critical_latency_mean_us),
        ("critical_latency_max_us", r.critical_latency_max_us),
        (
            "critical_latency_buckets",
            hash_of(&r.critical_latency_buckets),
        ),
        (
            "refinement_latency_buckets",
            hash_of(&r.refinement_latency_buckets),
        ),
        (
            "queued_critical_latency_buckets",
            hash_of(&r.queued_critical_latency_buckets),
        ),
        ("pacer_rate_bps", r.pacer_rate_bps.unwrap_or(0)),
        ("implausible_rtt_samples", r.implausible_rtt_samples),
        ("estimate_trace_hash", hash_of(&r.bwe_estimate_samples)),
        ("estimate_trace_len", r.bwe_estimate_samples.len() as u64),
        (
            "inbound_service_delays_hash",
            hash_of(&r.inbound_service_delays),
        ),
        ("transport_cwnd", r.transport.cwnd),
        ("transport_lost_packets", r.transport.lost_packets),
        ("transport_sent_packets", r.transport.sent_packets),
        ("transport_congestion_events", r.transport.congestion_events),
        (
            "transport_max_queue_bytes",
            r.transport.max_queue_bytes as u64,
        ),
        ("transport_samples", r.transport.samples),
        ("transport_client_received", r.transport.client_received),
        ("transport_client_recovered", r.transport.client_recovered),
        ("client_lost_pre_fec", r.client_wire_loss.lost_pre_fec),
        ("events_hash", hash_of(&format!("{:?}", r.events))),
        // Every datagram, both directions: offer instant, length, and what
        // the netsim made of it. Counters can agree while a datagram went
        // out at another instant or at another size; this cannot. Lengths,
        // not bytes: the ciphertext depends on OS-drawn TLS keys.
        ("wire_digest", r.wire_digest),
        ("wire_datagrams", r.wire_datagrams),
        // The picture the client ended up with.
        ("framebuffer_digest", r.framebuffer.digest()),
    ]);
    d
}

/// Run this same test again in a child process and require the same digest.
///
/// Two runs in one process share the allocator, the `HashMap` key source
/// (`RandomState` keys are per-thread, advancing per map) and the thread
/// state; a source of nondeterminism that is constant within a process, such
/// as a hash seed or an address-dependent order, passes the in-process pair
/// and is only caught by a fresh process. The alternative, a digest constant
/// pinned in the source, would catch it too but breaks on every legitimate
/// change to the codec, the scheduler or the wire format and so teaches people
/// to regenerate it blindly, and `DefaultHasher`'s algorithm is not promised
/// stable across Rust releases. The child needs no maintenance and cannot rot.
///
/// The child is this binary re-run with the test's own name and
/// `GF_DETERMINISM_CHILD` set, which turns off the re-exec.
fn assert_same_digest_in_a_fresh_process(
    test: &str,
    name: &str,
    digest: u64,
    trace: &ghostframe_e2e::harness::browserless::WireTrace,
) {
    const CHILD: &str = "GF_DETERMINISM_CHILD";
    // The child leaves its wire records here so the parent can name the first
    // divergent datagram instead of just saying the digests differ.
    let trace_path = std::env::temp_dir().join(format!(
        "gf-determinism-{name}-{}.trace",
        std::process::id()
    ));
    if let Some(p) = std::env::var_os("GF_DETERMINISM_TRACE_FILE") {
        std::fs::write(p, trace.to_text()).expect("write the wire trace");
        return;
    }
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().expect("test binary path"))
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("GF_DETERMINISM_TRACE_FILE", &trace_path)
        .output()
        .expect("spawn the test binary again");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "the child process failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let prefix = format!("DETERMINISM-DIGEST {name} ");
    let theirs = stdout
        .lines()
        .find_map(|l| l.find(&prefix).map(|i| &l[i + prefix.len()..]))
        .unwrap_or_else(|| panic!("the child printed no `{prefix}` line:\n{stdout}"));
    let theirs_trace = std::fs::read_to_string(&trace_path)
        .map(|t| ghostframe_e2e::harness::browserless::WireTrace::from_text(&t));
    let _ = std::fs::remove_file(&trace_path);
    if theirs.trim() != format!("{digest:016x}") {
        let report = theirs_trace
            .ok()
            .and_then(|t| trace.first_divergence(&t))
            .unwrap_or_else(|| "wire traces agree: the divergence is not in the wire".into());
        panic!("a fresh process produced a different scene from the same seed\n{report}");
    }
}

fn diff(a: &[(&'static str, u64)], b: &[(&'static str, u64)]) -> Vec<String> {
    a.iter()
        .zip(b.iter())
        .filter(|((_, x), (_, y))| x != y)
        .map(|((n, x), (_, y))| format!("  {n}: {x} != {y}"))
        .collect()
}

/// A second shape: the other netsim impairments (jitter, reorder, duplicate,
/// corrupt) and a server-side frame cost, so the held-frame deadline and the
/// netsim's own draws are in play as well.
fn impaired_scene() -> BrowserlessScene {
    let mut s = rich_scene();
    s.seed = 0xDE7E_0011;
    s.net = NetProfile {
        delay_us: 20_000,
        jitter_us: 4_000,
        reorder_us: 3_000,
        loss: 0.02,
        duplicate: 0.01,
        corrupt: 0.005,
        ..NetProfile::perfect()
    };
    s.duration = Duration::from_secs(4);
    s
}

fn check_premises(a: &BrowserlessResult) {
    // Premise: equality of zeros proves nothing. Each of these has to be
    // non-zero for the comparison to exercise the machinery it names.
    for (name, v) in [
        ("bytes_delivered_s2c", a.bytes_delivered_s2c),
        ("s2c_datagrams_dropped", a.s2c_datagrams_dropped),
        ("retransmit_attempts_total", a.retransmit_attempts_total),
        ("critical_latency_count", a.critical_latency_count),
        ("bwe_samples_seen", a.bwe_samples_seen),
        ("transport_sent_packets", a.transport.sent_packets),
    ] {
        assert!(
            v > 0,
            "premise: {name} is zero, the scene did not exercise it"
        );
    }
    assert!(
        a.nack_hit + a.nack_miss + a.rto_fired > 0,
        "premise: no NACK and no RTO, so loss recovery never ran"
    );
}

/// The PR-H gate. Two runs of one busy, lossy scene from one seed, in one
/// process, must agree on every counter -- byte totals and the estimate trace
/// included. Before the bridge was single-stepped the same pair diverged by
/// about 16% in delivered bytes.
///
/// The digest is printed so that a caller can run the binary in several
/// processes and compare: `DETERMINISM-DIGEST <name> <hash>` must be the same
/// line every time.
#[tokio::test(start_paused = true)]
async fn a_rich_scene_is_reproducible_from_its_seed() {
    let a = run_browserless(rich_scene()).await.expect("run a");
    let b = run_browserless(rich_scene()).await.expect("run b");
    check_premises(&a);

    let (da, db) = (full_digest(&a), full_digest(&b));
    println!("DETERMINISM-DIGEST rich {:016x}", hash_of(&da));
    let diffs = diff(&da, &db);
    assert!(
        diffs.is_empty(),
        "the same seed produced two different scenes:\n{}\n{}",
        diffs.join("\n"),
        a.wire_trace
            .first_divergence(&b.wire_trace)
            .unwrap_or_else(|| "wire traces agree".into())
    );
    assert!(
        a.wire_datagrams > 1000 && a.framebuffer.digest() != 0,
        "premise: the wire digest covers a real run ({} datagrams)",
        a.wire_datagrams
    );
    assert_same_digest_in_a_fresh_process(
        "a_rich_scene_is_reproducible_from_its_seed",
        "rich",
        hash_of(&da),
        &a.wire_trace,
    );
}

#[tokio::test(start_paused = true)]
async fn an_impaired_scene_with_frame_cost_is_reproducible_from_its_seed() {
    use ghostframe_e2e::harness::browserless::{
        run_browserless_with_options, LinkOverrides, RunOptions,
    };
    let opts = RunOptions {
        frame_cost: Some(Duration::from_millis(7)),
        ..Default::default()
    };
    let go = || async {
        run_browserless_with_options(impaired_scene(), LinkOverrides::default(), opts)
            .await
            .expect("run")
    };
    let a = go().await;
    let b = go().await;
    check_premises(&a);
    assert!(
        !a.frame_cost_windows.is_empty(),
        "premise: no frame was held, so the held-frame path did not run"
    );

    let (da, db) = (full_digest(&a), full_digest(&b));
    println!("DETERMINISM-DIGEST impaired {:016x}", hash_of(&da));
    let diffs = diff(&da, &db);
    assert!(
        diffs.is_empty(),
        "the same seed produced two different scenes:\n{}\n{}",
        diffs.join("\n"),
        a.wire_trace
            .first_divergence(&b.wire_trace)
            .unwrap_or_else(|| "wire traces agree".into())
    );
    assert_same_digest_in_a_fresh_process(
        "an_impaired_scene_with_frame_cost_is_reproducible_from_its_seed",
        "impaired",
        hash_of(&da),
        &a.wire_trace,
    );
}
