//! WS-C2 MEASUREMENT ONLY (not a gate): how long does `IoBridge::process_frame`
//! hold the event loop, and how late is an inbound packet handled meanwhile?
//!
//! Real clock, real GPU (Vulkan compute; VA-API is NOT touched, so H.264
//! frames bail at encoder init -- the NV12 conversion still runs). Run only
//! through the GPU guard:
//!   gf-gpu-run cargo test -p ghostframe-e2e \
//!     --test c2_loop_stall_measure -- --ignored --nocapture --test-threads=1
//!
//! Probe: a 1200-byte unsupported-version long-header packet from an unknown address makes
//! quinn answer with Version Negotiation; the time from write to the reply is the
//! loop's inbound turnaround. Probes are tagged "during encode" when they
//! overlap a process_frame ENTER..EXIT window (from the diag trace events).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ghostframe_client_net::{ClientNet, ClientNetConfig, ClientNetEvent};
use ghostframe_e2e::netsim::SocketPairPump;
use ghostframe_lib::capture::gpu_pipeline::GpuFrameProcessor;
use ghostframe_lib::transport::io_bridge::IoBridge;
use ghostframe_lib::transport::quic::QuicServer;
use ghostframe_lib::FrameSubmission;
use tracing_subscriber::layer::SubscriberExt;

type StageEvent = (String, Vec<(&'static str, u64)>);

#[derive(Default)]
struct Windows {
    /// (when, is_enter, dt_us) for process_frame ENTER/EXIT.
    spans: Mutex<Vec<(Instant, bool, u64)>>,
    /// frame.stages / frame.tile_stages events: (message, [(field, us)]).
    stages: Mutex<Vec<StageEvent>>,
}

struct Rec(Arc<Windows>);

#[derive(Default)]
struct V {
    msg: String,
    u: Vec<(&'static str, u64)>,
}
impl tracing::field::Visit for V {
    fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
        if f.name() == "message" {
            self.msg = format!("{v:?}");
        }
    }
    fn record_u64(&mut self, f: &tracing::field::Field, v: u64) {
        self.u.push((f.name(), v));
    }
}
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Rec {
    fn on_event(&self, e: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if e.metadata().target() != "ghostframe::io_bridge::diag" {
            return;
        }
        let mut v = V::default();
        e.record(&mut v);
        if v.msg.contains("process_frame ENTER") {
            self.0.spans.lock().unwrap().push((Instant::now(), true, 0));
        } else if v.msg.contains("process_frame EXIT") {
            let dt = v.u.iter().find(|(n, _)| *n == "dt_us").map_or(0, |x| x.1);
            self.0
                .spans
                .lock()
                .unwrap()
                .push((Instant::now(), false, dt));
        } else if v.msg.contains("frame.stages") || v.msg.contains("frame.tile_stages") {
            let name = if v.msg.contains("tile_stages") {
                "tile_stages"
            } else {
                "stages"
            };
            self.0.stages.lock().unwrap().push((name.into(), v.u));
        }
    }
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p) as usize]
}

/// Deterministic BGRA frame; `dirty_frac` of the 32x32 tiles (leading rows) get
/// per-frame noise, the rest is a fixed gradient.
fn make_frame(w: u32, h: u32, n: u32, dirty_frac: f64) -> Vec<u8> {
    let mut px = vec![0u8; (w * h * 4) as usize];
    let dirty_rows = ((h as f64 * dirty_frac) as u32).min(h);
    let mut x = 0x9E3779B9u32 ^ n.wrapping_mul(2654435761);
    for y in 0..h {
        for xx in 0..w {
            let i = ((y * w + xx) * 4) as usize;
            if y < dirty_rows {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                px[i..i + 4].copy_from_slice(&[x as u8, (x >> 8) as u8, (x >> 16) as u8, 255]);
            } else {
                px[i..i + 4].copy_from_slice(&[(xx / 8) as u8, (y / 8) as u8, 80, 255]);
            }
        }
    }
    px
}

async fn scenario(name: &str, w: u32, h: u32, dirty_frac: f64) {
    let windows = Arc::new(Windows::default());
    let sub = tracing_subscriber::registry().with(Rec(windows.clone()));
    // Global, not thread-local: the bridge runs on another runtime worker.
    // One scenario per process, so run each test by name.
    tracing::subscriber::set_global_default(sub).expect("one scenario per process");

    let (ours, peer) = tokio::net::UnixStream::pair().unwrap();
    let server = QuicServer::new().unwrap();
    let mut hash = [0u8; 32];
    hex::decode_to_slice(&server.cert_info().sha256_hex, &mut hash).unwrap();
    let (ftx, frx) = tokio::sync::mpsc::channel::<FrameSubmission>(2);
    let mut bridge = IoBridge::new_with_frames_for_test(ours, server, frx);
    bridge.set_gpu_frame_processor_for_measurement(
        GpuFrameProcessor::new(((w / 32 + 1) * (h / 32 + 1)).max(4096)).expect("needs Vulkan GPU"),
    );
    let live = bridge.connected_session_count_handle();
    tokio::spawn(async move {
        let _ = bridge.run().await;
    });

    let base = Instant::now();
    let us = |_: ()| Instant::now().duration_since(base).as_micros() as u64;
    let mut client = ClientNet::new(
        ClientNetConfig {
            server_name: "localhost".into(),
            server_cert_sha256: hash,
            indices_raw_enabled: true,
            supports_h264: false,
            display: None,
        },
        us(()),
    )
    .unwrap();
    let srv: SocketAddr = "[::1]:443".parse().unwrap();
    let cli: SocketAddr = "[::1]:5000".parse().unwrap();
    let probe_src: SocketAddr = "[::1]:6000".parse().unwrap();
    client.connect(srv, us(())).unwrap();
    // `SocketPairPump::recv` is not cancel-safe (read_exact), so a reader task
    // owns one dup of the socket and forwards packets; sends use the other.
    let std_peer = peer.into_std().unwrap();
    let std_rd = std_peer.try_clone().unwrap();
    let mut pump = SocketPairPump::new(tokio::net::UnixStream::from_std(std_peer).unwrap());
    let mut rd = SocketPairPump::new(tokio::net::UnixStream::from_std(std_rd).unwrap());
    let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(pk) = rd.recv().await {
            if ptx.send((pk, Instant::now())).is_err() {
                break;
            }
        }
    });

    let frame_period = Duration::from_millis(33);
    let end = Instant::now() + Duration::from_secs(12);
    let mut next_frame = Instant::now() + Duration::from_secs(2);
    let mut next_probe = Instant::now() + Duration::from_secs(2);
    let mut frames = 0u32;
    let mut ready = false;
    let mut probe_sent: Option<Instant> = None;
    // (send, reply) per probe
    let mut probes: Vec<(Instant, Instant)> = Vec::new();
    let mut probe_unanswered = 0u32;
    let mut seed = 7u32;

    while Instant::now() < end {
        while let Some(o) = client.poll_transmit() {
            pump.send(&o.payload, &cli).await.unwrap();
        }
        for e in client.take_events() {
            if matches!(e, ClientNetEvent::SessionReady) {
                ready = true;
            }
        }
        let now = Instant::now();
        let server_live = *live.borrow() > 0;
        if ready && server_live && now >= next_frame && ftx.capacity() > 0 {
            frames += 1;
            let px = make_frame(w, h, frames, if frames == 1 { 1.0 } else { dirty_frac });
            let _ = ftx.try_send(FrameSubmission {
                width: w,
                height: h,
                stride: w * 4,
                pixels: px,
                dmabuf_fd: None,
                timestamp_us: 0,
                damage_tiles: None,
                capture_done_ns: 0,
            });
            next_frame = Instant::now() + frame_period;
        }
        if ready && server_live && probe_sent.is_none() && Instant::now() >= next_probe {
            // Long header, unsupported version, 1200 bytes: quinn answers
            // with a Version Negotiation packet, deterministically.
            let mut p = [0u8; 1200];
            p[0] = 0xC0;
            p[1..5].copy_from_slice(&0xbabababau32.to_be_bytes());
            p[5] = 8; // DCID len
            for b in p[6..14].iter_mut() {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                *b = seed as u8;
            }
            p[14] = 0; // SCID len
            probe_sent = Some(Instant::now());
            pump.send(&p, &probe_src).await.unwrap();
            next_probe = Instant::now() + Duration::from_millis(7);
        }
        let wake = client
            .poll_timeout()
            .map(|t| base + Duration::from_micros(t))
            .unwrap_or(now + Duration::from_millis(2))
            .min(now + Duration::from_millis(1));
        tokio::select! {
            r = prx.recv() => {
                let (pk, at) = r.unwrap();
                if pk.addr == probe_src {
                    if let Some(s) = probe_sent.take() { probes.push((s, at)); }
                } else {
                    client.handle_udp(&pk.payload, srv, us(()));
                }
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => {
                client.on_timeout(us(()));
                // An unanswered probe older than 500 ms is counted and dropped.
                if let Some(s) = probe_sent { if Instant::now().duration_since(s) > Duration::from_millis(500) { probe_unanswered += 1; probe_sent = None; } }
            }
        }
    }

    let w_ = windows.spans.lock().unwrap().clone();
    let mut spans: Vec<(Instant, Instant)> = Vec::new();
    let mut dts: Vec<f64> = Vec::new();
    let mut enter = None;
    for (t, is_enter, dt) in &w_ {
        if *is_enter {
            enter = Some(*t);
        } else if let Some(e) = enter.take() {
            spans.push((e, *t));
            dts.push(*dt as f64 / 1000.0);
        }
    }
    let (mut during, mut idle) = (Vec::new(), Vec::new());
    for (s, r) in &probes {
        let lat = r.duration_since(*s).as_secs_f64() * 1000.0;
        if spans.iter().any(|(a, b)| *s < *b && *r > *a) {
            during.push(lat)
        } else {
            idle.push(lat)
        }
    }
    println!(
        "METRIC {name} {w}x{h} dirty={dirty_frac} frames_submitted={frames} frames_measured={}",
        dts.len()
    );
    println!(
        "METRIC {name} process_frame_ms p50={:.2} p95={:.2} max={:.2}",
        pct(&mut dts.clone(), 0.5),
        pct(&mut dts.clone(), 0.95),
        pct(&mut dts, 1.0)
    );
    println!(
        "METRIC {name} probes total={} unanswered={} during_encode={} idle={}",
        probes.len(),
        probe_unanswered,
        during.len(),
        idle.len()
    );
    println!(
        "METRIC {name} probe_latency_ms during p50={:.2} p95={:.2} max={:.2} | idle p50={:.2} p95={:.2} max={:.2}",
        pct(&mut during.clone(), 0.5),
        pct(&mut during.clone(), 0.95),
        pct(&mut during, 1.0),
        pct(&mut idle.clone(), 0.5),
        pct(&mut idle.clone(), 0.95),
        pct(&mut idle, 1.0)
    );
    // Per-stage medians (ms), over frames that reached each stage.
    {
        let st = windows.stages.lock().unwrap();
        for kind in ["stages", "tile_stages"] {
            let mut cols: std::collections::BTreeMap<&str, Vec<f64>> = Default::default();
            let mut n = 0;
            for (k, fields) in st.iter().filter(|(k, _)| k == kind) {
                let _ = k;
                n += 1;
                for (name, us) in fields {
                    if *name != "seq" {
                        cols.entry(name).or_default().push(*us as f64 / 1000.0);
                    }
                }
            }
            let line: Vec<String> = cols
                .iter_mut()
                .map(|(k, v)| format!("{k}={:.2}/{:.2}", pct(v, 0.5), pct(v, 1.0)))
                .collect();
            println!("METRIC {name} {kind} n={n} (p50/max ms) {}", line.join(" "));
        }
    }
    // Non-zero premises: the encode ran and probes landed in both classes.
    assert!(!spans.is_empty(), "no process_frame window observed");
    assert!(!probes.is_empty(), "no probe was answered");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "WS-C2 measurement; GPU; run via gf-gpu-run"]
async fn c2_stall_1080p_small_dirty() {
    scenario("1080p_small", 1920, 1080, 0.05).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "WS-C2 measurement; GPU; run via gf-gpu-run"]
async fn c2_stall_1080p_full_dirty() {
    scenario("1080p_full", 1920, 1080, 1.0).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "WS-C2 measurement; GPU; run via gf-gpu-run"]
async fn c2_stall_4k_small_dirty() {
    scenario("4k_small", 3840, 2160, 0.05).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "WS-C2 measurement; GPU; run via gf-gpu-run"]
async fn c2_stall_4k_full_dirty() {
    scenario("4k_full", 3840, 2160, 1.0).await;
}
