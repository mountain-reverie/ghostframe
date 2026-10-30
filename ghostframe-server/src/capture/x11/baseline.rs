//! Baseline harness for damage-driven encoding (PR 0): a deterministic scene
//! driver over a private Xvfb, and the per-tick cost report it produces.
//!
//! Each tick paints the scene through a second X connection, waits for the
//! server to finish, then runs exactly what the capture loop runs between its
//! timestamps (`drain_damage` + `capture`), recording wall time, thread CPU
//! time and the counters of [`crate::capture::tick_stats`]. The library, GPU
//! and network are not involved: this prices the X11 capture stage on its own.
//! X server CPU is not included (it is a separate process).
//!
//! `cargo test -p ghostframe-server baseline_table -- --ignored --nocapture`
//! (each run spawns its own Xvfb) prints the markdown table recorded in
//! `docs/specs/damage-baseline.md`.

use std::time::Instant;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, CreateGCAux, Rectangle};

use super::test_support::PrivateXvfb;
use super::{damage, frames, X11Backend};
use crate::capture::tick_stats::thread_cpu_us;
use crate::capture::Backend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    Idle,
    /// A 30x16 px update at 10 Hz (every third tick of a 30 fps loop).
    Typing,
    /// A region of up to 1920x900 scrolling 32 px per tick.
    Scrolling,
    /// Full screen, new content every tick (30 Hz).
    Video,
}

impl Scene {
    pub const ALL: [Scene; 4] = [Scene::Idle, Scene::Typing, Scene::Scrolling, Scene::Video];

    pub fn name(self) -> &'static str {
        match self {
            Scene::Idle => "idle",
            Scene::Typing => "typing",
            Scene::Scrolling => "scrolling",
            Scene::Video => "video",
        }
    }
}

/// xorshift64*: deterministic content from the seed, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

struct Painter {
    conn: x11rb::rust_connection::RustConnection,
    root: u32,
    gc: u32,
    w: u16,
    h: u16,
    rng: Rng,
    typing_col: u16,
    typing_row: u16,
}

impl Painter {
    fn new(display: &str, w: u16, h: u16, seed: u64) -> Self {
        let (conn, n) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[n].root;
        let gc = conn.generate_id().unwrap();
        conn.create_gc(gc, root, &CreateGCAux::new().foreground(0))
            .unwrap();
        Self {
            conn,
            root,
            gc,
            w,
            h,
            rng: Rng(seed | 1),
            typing_col: 0,
            typing_row: 0,
        }
    }

    fn fill(&self, r: Rectangle) {
        self.conn
            .poly_fill_rectangle(self.root, self.gc, &[r])
            .unwrap();
    }

    fn color(&mut self) {
        let c = (self.rng.next() as u32) & 0x00ff_ffff;
        self.conn
            .change_gc(
                self.gc,
                &x11rb::protocol::xproto::ChangeGCAux::new().foreground(c),
            )
            .unwrap();
    }

    /// Paint tick `t` of `scene`, then wait until the X server has done it.
    fn paint(&mut self, scene: Scene, t: u32) {
        match scene {
            Scene::Idle => {}
            Scene::Typing => {
                if t.is_multiple_of(3) {
                    self.color();
                    let (x, y) = (self.typing_col * 30, 100 + self.typing_row * 16);
                    self.fill(Rectangle {
                        x: x as i16,
                        y: y as i16,
                        width: 30,
                        height: 16,
                    });
                    self.typing_col += 1;
                    if (self.typing_col + 1) * 30 > self.w {
                        self.typing_col = 0;
                        self.typing_row = (self.typing_row + 1) % 20;
                    }
                }
            }
            Scene::Scrolling => {
                let rw = self.w.min(1920);
                let rh = self.h.min(900);
                self.conn
                    .copy_area(self.root, self.root, self.gc, 0, 32, 0, 0, rw, rh - 32)
                    .unwrap();
                // New content enters at the bottom: bands of random colour.
                let bands = 8u16;
                for b in 0..bands {
                    self.color();
                    self.fill(Rectangle {
                        x: (b * (rw / bands)) as i16,
                        y: (rh - 32) as i16,
                        width: rw / bands,
                        height: 32,
                    });
                }
            }
            Scene::Video => {
                let (cols, rows) = (16u16, 9u16);
                for r in 0..rows {
                    for c in 0..cols {
                        self.color();
                        self.fill(Rectangle {
                            x: (c * (self.w / cols)) as i16,
                            y: (r * (self.h / rows)) as i16,
                            width: self.w / cols + 1,
                            height: self.h / rows + 1,
                        });
                    }
                }
            }
        }
        self.conn.get_input_focus().unwrap().reply().unwrap();
    }
}

/// One tick's measurements.
#[derive(Debug, Clone, Default)]
pub struct Tick {
    pub capture_us: u64,
    pub cpu_us: u64,
    pub bytes_copied: u64,
    pub allocs: u32,
    /// Tiles in the damage hint; 0 when the hint was `None`.
    pub hint_tiles: usize,
    /// The hint itself, sorted, for determinism checks.
    pub hint: Vec<(u32, u32)>,
    /// FNV-1a of the captured pixels.
    pub pixel_hash: u64,
}

#[derive(Debug)]
pub struct RunReport {
    pub scene: Scene,
    /// Root size in pixels (W*H).
    pub pixels: u64,
    pub ticks: Vec<Tick>,
}

impl RunReport {
    fn mean(&self, f: impl Fn(&Tick) -> u64) -> f64 {
        self.ticks.iter().map(f).sum::<u64>() as f64 / self.ticks.len().max(1) as f64
    }
    pub fn capture_us(&self) -> f64 {
        self.mean(|t| t.capture_us)
    }
    pub fn cpu_us(&self) -> f64 {
        self.mean(|t| t.cpu_us)
    }
    pub fn bytes_copied(&self) -> f64 {
        self.mean(|t| t.bytes_copied)
    }
    pub fn allocs(&self) -> f64 {
        self.mean(|t| u64::from(t.allocs))
    }
    pub fn hint_tiles(&self) -> f64 {
        self.mean(|t| t.hint_tiles as u64)
    }

    /// Reject a run whose measurement cannot be real. Every scene, idle
    /// included, grabs the whole root each tick, so wall time, CPU time,
    /// bytes and allocations must all be non-zero; a counter stuck at zero
    /// reads as success otherwise.
    pub fn validate(&self) -> Result<(), String> {
        if self.ticks.is_empty() {
            return Err("no ticks measured".into());
        }
        // The grab copies the whole root every tick, so every tick (not just
        // the mean) carries at least W*H*4 bytes.
        let floor = self.pixels * 4;
        if let Some((i, t)) = self
            .ticks
            .iter()
            .enumerate()
            .find(|(_, t)| t.bytes_copied < floor)
        {
            return Err(format!(
                "{} scene: tick {i} copied {} bytes, below the W*H*4 = {floor} floor",
                self.scene.name(),
                t.bytes_copied
            ));
        }
        for (name, v) in [
            ("capture_us", self.capture_us()),
            ("cpu_us", self.cpu_us()),
            ("bytes_copied", self.bytes_copied()),
            ("allocs", self.allocs()),
        ] {
            if v <= 0.0 {
                return Err(format!(
                    "{} scene: counter {name} is zero over {} ticks; the measurement is not live",
                    self.scene.name(),
                    self.ticks.len()
                ));
            }
        }
        Ok(())
    }
}

/// Run `ticks` ticks of `scene` on a fresh Xvfb of `w`x`h`, after two
/// unmeasured warm-up ticks (the first grab allocates the SHM segment).
pub fn run_scene(scene: Scene, w: u16, h: u16, ticks: u32, seed: u64) -> RunReport {
    let x = PrivateXvfb::spawn(&["-screen", "0", &format!("{w}x{h}x24")]);
    let d = x.display();
    let mut backend = X11Backend {
        frames: frames::X11Frames::connect(Some(d), None).unwrap(),
        damage: damage::XDamageMonitor::new(Some(d)),
        input: None,
        display: None,
        stats: Default::default(),
    };
    backend.arm_damage();
    let mut painter = Painter::new(d, w, h, seed);
    let mut out = Vec::with_capacity(ticks as usize);
    for t in 0..ticks + 2 {
        painter.paint(scene, t);
        let cpu0 = thread_cpu_us();
        let t0 = Instant::now();
        let hint = backend.drain_damage();
        let frame = backend.capture(t * 33_333).expect("capture");
        #[allow(
            clippy::disallowed_methods,
            reason = "synchronous harness on a real Xvfb, never under a paused tokio clock; this is real wall-clock capture cost"
        )]
        let capture_us = t0.elapsed().as_micros() as u64;
        let cpu_us = thread_cpu_us().saturating_sub(cpu0);
        let pixel_hash = frame.pixels.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        assert_eq!((frame.width, frame.height), (u32::from(w), u32::from(h)));
        let s = backend.take_tick_stats();
        if t >= 2 {
            out.push(Tick {
                capture_us,
                cpu_us,
                bytes_copied: s.bytes_copied,
                allocs: s.allocs,
                hint_tiles: hint.as_ref().map_or(0, |h| h.len()),
                hint: {
                    let mut h = hint.unwrap_or_default();
                    h.sort_unstable();
                    h
                },
                pixel_hash,
            });
        }
    }
    RunReport {
        scene,
        pixels: u64::from(w) * u64::from(h),
        ticks: out,
    }
}

mod tests {
    use super::*;

    /// The gate: a live run of the idle scene has non-zero cost on every
    /// counter, and the grab accounts for the whole root every tick.
    #[test]
    fn idle_scene_measures_nonzero_cost() {
        let r = run_scene(Scene::Idle, 640, 360, 12, 7);
        r.validate().unwrap();
        // Property, not a magic number: every tick copies the root and then
        // the cursor image on top, so strictly more than W*H*4.
        for (i, t) in r.ticks.iter().enumerate() {
            assert!(t.bytes_copied > 640 * 360 * 4, "tick {i}: {t:?}");
        }
        // Idle means no damage hint.
        assert_eq!(r.hint_tiles(), 0.0);
    }

    #[test]
    fn busy_scenes_report_damage() {
        for s in [Scene::Typing, Scene::Scrolling, Scene::Video] {
            let r = run_scene(s, 640, 360, 9, 7);
            r.validate().unwrap();
            assert!(r.hint_tiles() > 0.0, "{} reported no damage", s.name());
            for (i, t) in r.ticks.iter().enumerate() {
                assert!(
                    t.bytes_copied > 640 * 360 * 4,
                    "{} tick {i}: {t:?}",
                    s.name()
                );
                assert!(t.allocs > 0, "{} tick {i}: {t:?}", s.name());
            }
        }
    }

    #[test]
    fn the_harness_rejects_a_zeroed_counter() {
        for zero in 0..4 {
            let ticks = (0..5)
                .map(|_| Tick {
                    capture_us: if zero == 0 { 0 } else { 100 },
                    cpu_us: if zero == 1 { 0 } else { 90 },
                    bytes_copied: if zero == 2 { 0 } else { 4096 },
                    allocs: if zero == 3 { 0 } else { 1 },
                    hint_tiles: 0,
                    hint: vec![],
                    pixel_hash: 0,
                })
                .collect();
            let r = RunReport {
                scene: Scene::Idle,
                pixels: 1024,
                ticks,
            };
            assert!(r.validate().is_err(), "zeroed counter {zero} accepted");
        }
    }

    #[test]
    fn scenes_are_deterministic_for_a_seed() {
        for scene in [Scene::Typing, Scene::Scrolling, Scene::Video] {
            let a = run_scene(scene, 320, 180, 9, 99);
            let b = run_scene(scene, 320, 180, 9, 99);
            for (i, (x, y)) in a.ticks.iter().zip(&b.ticks).enumerate() {
                assert_eq!(x.hint, y.hint, "{} tick {i} hint", scene.name());
                assert_eq!(
                    x.pixel_hash,
                    y.pixel_hash,
                    "{} tick {i} pixels",
                    scene.name()
                );
            }
            // The seed is load-bearing: another seed paints other pixels.
            let c = run_scene(scene, 320, 180, 9, 100);
            assert!(
                a.ticks
                    .iter()
                    .zip(&c.ticks)
                    .any(|(x, y)| x.pixel_hash != y.pixel_hash),
                "{}: seed does not change the content",
                scene.name()
            );
        }
    }

    fn median(mut v: Vec<f64>) -> f64 {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    }

    /// Prints the baseline table. Slow (4K Xvfb x 4 scenes x 3 runs), so
    /// ignored; run it under the bench lock, see the module docs.
    #[test]
    #[ignore = "baseline measurement, prints a table; run by hand"]
    fn baseline_table() {
        const RUNS: u64 = 3;
        const TICKS: u32 = 90;
        let layouts: [(&str, u16, u16); 3] = [
            ("1080p", 1920, 1080),
            ("4K", 3840, 2160),
            ("dual-1080p", 3840, 1080),
        ];
        println!(
            "| layout | scene | capture_us (median, min-max) | cpu_us | bytes/tick | allocs/tick | hint tiles/tick |"
        );
        println!("|---|---|---|---|---|---|---|");
        for (name, w, h) in layouts {
            for scene in Scene::ALL {
                let runs: Vec<RunReport> = (0..RUNS)
                    .map(|i| {
                        let r = run_scene(scene, w, h, TICKS, 1000 + i);
                        r.validate().unwrap();
                        r
                    })
                    .collect();
                let col = |f: fn(&RunReport) -> f64| runs.iter().map(f).collect::<Vec<_>>();
                let cap = col(RunReport::capture_us);
                let (lo, hi) = (
                    cap.iter().cloned().fold(f64::MAX, f64::min),
                    cap.iter().cloned().fold(0.0, f64::max),
                );
                println!(
                    "| {name} | {} | {:.0} ({lo:.0}-{hi:.0}) | {:.0} | {:.0} | {:.1} | {:.0} |",
                    scene.name(),
                    median(cap.clone()),
                    median(col(RunReport::cpu_us)),
                    median(col(RunReport::bytes_copied)),
                    median(col(RunReport::allocs)),
                    median(col(RunReport::hint_tiles)),
                );
            }
        }
    }
}
