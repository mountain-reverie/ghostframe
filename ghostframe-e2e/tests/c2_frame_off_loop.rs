//! WS-C2: does a frame's GPU and encode time stay off the transport event
//! loop? Real clock, a real QUIC session, no GPU.
//!
//! A [`FrameBackend`] that sleeps stands in for the Vulkan processor and the
//! hardware encoder: `analyse` sleeps 250 ms (the fence wait) and
//! `finish_tile`/`finish_h264` sleep 250 ms (readback, pass encode, H.264).
//! While a frame is in either sleep, a Version Negotiation probe is sent
//! every 7 ms; quinn answers it from the loop's inbound arm, so the time to
//! the reply is how long the loop took to turn.
//!
//! The property is that a probe sent well inside a busy window is answered
//! before the window ends. When the backend runs on the loop (the
//! synchronous seam before the frame worker) the reply cannot come until the
//! window is over, which is what fails there.
//!
//! No threshold on latency: the property is ordering against the backend's own
//! busy windows, which the backend records itself.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ghostframe_client_net::{ClientNet, ClientNetConfig, ClientNetEvent};
use ghostframe_e2e::netsim::SocketPairPump;
use ghostframe_lib::transport::io_bridge::{
    Analysed, Cdf53Encoded, Control, FrameBackend, H264Out, IoBridge, OwnedPalRle, PixelSource,
    TileOut, TileRequest, TileTimings,
};
use ghostframe_lib::transport::quic::QuicServer;
use ghostframe_lib::FrameSubmission;

const BUSY: Duration = Duration::from_millis(250);

type Windows = Arc<Mutex<Vec<(&'static str, Instant, Instant)>>>;

struct SleepyBackend {
    windows: Windows,
}

impl SleepyBackend {
    fn busy(&self, what: &'static str) {
        let start = Instant::now();
        std::thread::sleep(BUSY);
        self.windows
            .lock()
            .unwrap()
            .push((what, start, Instant::now()));
    }
}

impl FrameBackend for SleepyBackend {
    fn apply(&mut self, _control: Control) {}

    fn analyse(
        &mut self,
        _frame: &FrameSubmission,
        _escalation: &[u32],
        _want_diag: bool,
    ) -> Result<Analysed, String> {
        self.busy("analyse");
        // Every tile of the 64x64 test frame (a 2x2 grid) is dirty.
        Ok(Analysed {
            dirty_tiles: vec![0, 1, 2, 3],
            dirty_analysis: vec![None; 4],
            ..Default::default()
        })
    }

    fn finish_h264(&mut self, _frame: &FrameSubmission, _force_keyframe: bool) -> Option<H264Out> {
        self.busy("finish");
        Some(H264Out {
            payload: vec![0u8; 800],
            is_keyframe: true,
        })
    }

    fn finish_tile(&mut self, _frame: &FrameSubmission, _req: &TileRequest<'_>) -> TileOut {
        self.busy("finish");
        TileOut {
            pixels: Ok(PixelSource::Frame),
            palrle: OwnedPalRle::default(),
            cdf53_dirty: Vec::new(),
            cdf53_escalation: Vec::<Option<Cdf53Encoded>>::new(),
            cdf53_fallback: Vec::new(),
            timings: TileTimings::default(),
        }
    }

    fn finish_discard(&mut self) {}

    fn forward_cdf53_tiles(&mut self, tiles: &[&[u8]]) -> Result<Vec<i16>, String> {
        Ok(vec![
            0;
            tiles.len()
                * ghostframe_lib::encoder::cdf53::CDF53_TOTAL_COEFFS
        ])
    }
}

fn frame() -> FrameSubmission {
    FrameSubmission {
        width: 64,
        height: 64,
        stride: 64 * 4,
        pixels: vec![0x40; 64 * 64 * 4].into(),
        dmabuf_fd: None,
        timestamp_us: 0,
        damage: ghostframe_lib::FrameDamage::Unknown,
        capture_done_ns: 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_frame_in_the_backend_does_not_hold_the_event_loop() {
    // A hung call must not hang the test run.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(90));
        eprintln!("c2_frame_off_loop watchdog: aborting");
        std::process::abort();
    });

    let windows: Windows = Default::default();
    let (ours, peer) = tokio::net::UnixStream::pair().unwrap();
    let server = QuicServer::new().unwrap();
    let mut hash = [0u8; 32];
    hex::decode_to_slice(&server.cert_info().sha256_hex, &mut hash).unwrap();
    let (ftx, frx) = tokio::sync::mpsc::channel::<FrameSubmission>(2);
    let mut bridge = IoBridge::new_with_frames_for_test(ours, server, frx);
    bridge.set_frame_backend(Box::new(SleepyBackend {
        windows: windows.clone(),
    }));
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
    // `SocketPairPump::recv` is not cancel-safe, so a reader task owns one dup
    // of the socket and forwards packets with their arrival time.
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

    let end = Instant::now() + Duration::from_secs(8);
    let mut next_frame = Instant::now();
    let mut next_probe = Instant::now();
    let mut ready = false;
    let mut probe_id = 0u64;
    let mut sent: std::collections::HashMap<u64, Instant> = Default::default();
    let mut replies: Vec<(Instant, Instant)> = Vec::new();
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
        // One frame every 700 ms: 250 ms analysing, 250 ms finishing, idle after.
        if ready && server_live && now >= next_frame && ftx.capacity() > 0 {
            let _ = ftx.try_send(frame());
            next_frame = now + Duration::from_millis(700);
        }
        if ready && server_live && now >= next_probe {
            let mut p = [0u8; 1200];
            p[0] = 0xC0;
            p[1..5].copy_from_slice(&0xbabababau32.to_be_bytes());
            p[5] = 8;
            probe_id += 1;
            p[6..14].copy_from_slice(&probe_id.to_be_bytes());
            sent.insert(probe_id, Instant::now());
            pump.send(&p, &probe_src).await.unwrap();
            next_probe = now + Duration::from_millis(7);
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
                    let d = &pk.payload;
                    assert!(d.len() >= 15 && d[1..5] == [0; 4], "not a Version Negotiation reply");
                    let id = u64::from_be_bytes(d[7..15].try_into().unwrap());
                    if let Some(s) = sent.remove(&id) {
                        replies.push((s, at));
                    }
                } else {
                    client.handle_udp(&pk.payload, srv, us(()));
                }
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => {
                client.on_timeout(us(()));
            }
        }
    }

    let windows = windows.lock().unwrap().clone();
    // Probes sent at least 40 ms into a window and at least 40 ms before its
    // end must be answered before it ends: the loop was free to turn.
    let margin = Duration::from_millis(40);
    for kind in ["analyse", "finish"] {
        let (mut inside, mut answered_in_time) = (0u32, 0u32);
        for &(k, start, finish) in windows.iter().filter(|w| w.0 == kind) {
            let _ = k;
            for &(s, r) in &replies {
                if s >= start + margin && s + margin <= finish {
                    inside += 1;
                    if r <= finish {
                        answered_in_time += 1;
                    }
                }
            }
            // A probe that was sent inside the window and never answered at
            // all also counts against it.
            for &s in sent.values() {
                if s >= start + margin && s + margin <= finish {
                    inside += 1;
                }
            }
        }
        println!("METRIC off_loop {kind} probes_inside={inside} answered_before_window_end={answered_in_time}");
        assert!(
            windows.iter().filter(|w| w.0 == kind).count() >= 3,
            "premise: the backend ran {kind} at least 3 times ({windows:?})"
        );
        assert!(
            inside >= 30,
            "premise: probes sent inside {kind} windows ({inside})"
        );
        assert!(
            answered_in_time * 100 >= inside * 95,
            "{kind}: {answered_in_time} of {inside} probes sent inside a busy window were \
             answered before it ended; the loop was held"
        );
    }
}
