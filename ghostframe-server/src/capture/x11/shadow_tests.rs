//! The safety net against a real X server (a private Xvfb).
//!
//! The pieces the net does not own are stood in for, at the seams they
//! really have:
//!
//! - the X11 backend is real, with a fault injected *inside* it
//!   ([`TestHooks`](super::TestHooks)): damage for a rectangle is lost after
//!   the drain and before `capture` decides anything, or an application
//!   paints between the damage subtract and the grab;
//! - the library's compare is a stand-in that holds a per-tile
//!   last-committed reference and reports a [`FrameFeedback`]. [`Compare::Full`]
//!   is shadow mode (every tile compared, as today); [`Compare::Candidates`]
//!   is what a consumer will do (only the hinted tiles and the sweep slice),
//!   so a miss is found only when the sweep reaches it;
//! - the consumer is a persistent buffer updated only for the tiles the
//!   would-be grab set names (hint plus slice, everything when `Unknown` or
//!   `Distrusted`). Its distance from the real frame is the staleness a user
//!   would see.

use std::cell::RefCell;
use std::rc::Rc;

use ghostframe_lib::{FeedbackKind, FrameFeedback};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ChangeGCAux, ConnectionExt, CreateGCAux, Rectangle};
use x11rb::rust_connection::RustConnection;

use super::test_support::PrivateXvfb;
use super::{damage, frames, X11Backend};
use crate::capture::safety_net::{NetConfig, SafetyNet, Trust};
use crate::capture::{damage_then_grab, Backend, Damage, TileBitmap};

const SIZE: u32 = 256; // 8 x 8 tiles
const TILE: u32 = 32;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Compare {
    Full,
    Candidates,
}

type Paints = Rc<RefCell<Vec<(Rectangle, u32)>>>;

fn paint(conn: &RustConnection, root: u32, gc: u32, r: Rectangle, color: u32) {
    conn.change_gc(gc, &ChangeGCAux::new().foreground(color))
        .unwrap();
    conn.poly_fill_rectangle(root, gc, &[r]).unwrap();
    conn.get_input_focus().unwrap().reply().unwrap();
}

struct Rig {
    _x: PrivateXvfb,
    backend: X11Backend,
    net: SafetyNet,
    cfg: NetConfig,
    conn: RustConnection,
    root: u32,
    gc: u32,
    raced: Paints,
    compare: Compare,
    tick: u64,
    ts: u32,
    pending: Vec<FrameFeedback>,
    /// The library's per-tile last-committed reference; `None` before the
    /// first frame.
    reference: Option<Vec<u8>>,
    /// The consumer's persistent buffer.
    model: Vec<u8>,
}

struct Tick {
    /// Tiles where the consumer's buffer differs from the frame just grabbed.
    stale: usize,
}

impl Rig {
    fn new(cfg: NetConfig, compare: Compare) -> Self {
        let x = PrivateXvfb::spawn(&["-screen", "0", &format!("{SIZE}x{SIZE}x24")]);
        let d = x.display().to_string();
        let mut backend = X11Backend {
            frames: frames::X11Frames::connect(Some(&d), None).unwrap(),
            damage: damage::XDamageMonitor::new(Some(&d), crate::config::DamageReport::NonEmpty),
            input: None,
            display: None,
            stats: Default::default(),
            hooks: Default::default(),
        };
        backend.arm_damage();
        assert!(backend.damage_armed());
        let (conn, n) = x11rb::connect(Some(&d)).unwrap();
        let root = conn.setup().roots[n].root;
        let gc = conn.generate_id().unwrap();
        conn.create_gc(gc, root, &CreateGCAux::new().foreground(0xffffff))
            .unwrap();
        // The racing painter has its own connection: it paints from inside
        // `capture`, between the drain and the grab.
        let (rconn, rn) = x11rb::connect(Some(&d)).unwrap();
        let rroot = rconn.setup().roots[rn].root;
        let rgc = rconn.generate_id().unwrap();
        rconn
            .create_gc(rgc, rroot, &CreateGCAux::new().foreground(0xffffff))
            .unwrap();
        let raced: Paints = Rc::default();
        let queue = raced.clone();
        backend.hooks.before_grab = Some(Box::new(move || {
            for (r, c) in queue.borrow_mut().drain(..) {
                paint(&rconn, rroot, rgc, r, c);
            }
        }));
        Self {
            _x: x,
            backend,
            net: SafetyNet::new(cfg),
            cfg,
            conn,
            root,
            gc,
            raced,
            compare,
            tick: 0,
            ts: 1000,
            pending: Vec::new(),
            reference: None,
            model: Vec::new(),
        }
    }

    /// Paint before the next tick's drain: X reports it with that tick.
    fn draw(&self, r: Rectangle, color: u32) {
        paint(&self.conn, self.root, self.gc, r, color);
    }

    /// Paint after the next tick's drain and before its grab: in that tick's
    /// pixels, reported by the following tick.
    fn race(&self, r: Rectangle, color: u32) {
        self.raced.borrow_mut().push((r, color));
    }

    /// The sweep slice due at the tick the next `tick()` runs covers `row`.
    fn sweep_covers_next(&self, row: u32) -> bool {
        SafetyNet::slice_for(&self.cfg, self.tick, SIZE / TILE).is_some_and(|s| s.covers_row(row))
    }

    fn tick(&mut self) -> Tick {
        for fb in self.pending.drain(..) {
            self.net.ingest(fb);
        }
        self.ts += 33_333;
        let (damage, frame) = damage_then_grab(&mut self.backend, self.ts);
        let px: Vec<u8> = frame.expect("grab").pixels.to_vec();
        let plan = self
            .net
            .begin_tick(self.ts, &damage, SIZE, SIZE, self.backend.damage_armed());
        self.tick += 1;

        // The would-be grab set.
        let mut grab = TileBitmap::new(SIZE, SIZE);
        let everything = plan.trust == Trust::Distrusted
            || !matches!(&damage, Damage::None | Damage::Tiles(_))
            || self.model.is_empty();
        if everything {
            grab.set_all();
        } else {
            if let Damage::Tiles(b) = &damage {
                grab.or_with(b);
            }
            if let Some(s) = plan.slice {
                grab.set_rows(s.lo, s.hi);
            }
        }
        if self.model.is_empty() {
            self.model = vec![0; px.len()];
        }
        for (tx, ty) in grab.iter() {
            copy_tile(&mut self.model, &px, tx, ty);
        }

        // The library's compare.
        let fb_kind = match self.reference.as_mut() {
            None => {
                self.reference = Some(px.clone());
                FeedbackKind::Saturated
            }
            Some(reference) => {
                let mut dirty = TileBitmap::new(SIZE, SIZE);
                let compared = match self.compare {
                    Compare::Full => None,
                    Compare::Candidates => Some(&grab),
                };
                for ty in 0..SIZE / TILE {
                    for tx in 0..SIZE / TILE {
                        if compared.is_some_and(|c| !c.get(tx, ty)) {
                            continue;
                        }
                        if !tile_eq(reference, &px, tx, ty) {
                            dirty.set(tx, ty);
                            copy_tile(reference, &px, tx, ty);
                        }
                    }
                }
                let mut words = vec![0u64; ((SIZE / TILE) * (SIZE / TILE)).div_ceil(64) as usize];
                for (tx, ty) in dirty.iter() {
                    let i = (ty * (SIZE / TILE) + tx) as usize;
                    words[i / 64] |= 1 << (i % 64);
                }
                FeedbackKind::Dirty(words)
            }
        };
        self.pending.push(FrameFeedback {
            timestamp_us: self.ts,
            width: SIZE,
            height: SIZE,
            kind: fb_kind,
        });

        let mut stale = 0;
        for ty in 0..SIZE / TILE {
            for tx in 0..SIZE / TILE {
                if !tile_eq(&self.model, &px, tx, ty) {
                    stale += 1;
                }
            }
        }
        Tick { stale }
    }

    fn ticks(&mut self, n: usize) -> Tick {
        let mut last = self.tick();
        for _ in 1..n {
            last = self.tick();
        }
        last
    }
}

fn rows(tx: u32, ty: u32) -> impl Iterator<Item = std::ops::Range<usize>> {
    (ty * TILE..(ty + 1) * TILE).map(move |y| {
        let start = (y * SIZE + tx * TILE) as usize * 4;
        start..start + TILE as usize * 4
    })
}

fn tile_eq(a: &[u8], b: &[u8], tx: u32, ty: u32) -> bool {
    rows(tx, ty).all(|r| a[r.clone()] == b[r])
}

fn copy_tile(dst: &mut [u8], src: &[u8], tx: u32, ty: u32) {
    for r in rows(tx, ty) {
        dst[r.clone()].copy_from_slice(&src[r]);
    }
}

fn rect(x: i16, y: i16, width: u16, height: u16) -> Rectangle {
    Rectangle {
        x,
        y,
        width,
        height,
    }
}

/// Steady typing: a 30x16 glyph per tick, half painted before the drain
/// (reported at once), half between the drain and the grab (in the pixels one
/// tick before the damage names them). The race produces suspects; none may
/// become a correction. Run against both library models.
fn typing_run(compare: Compare, cfg: NetConfig) -> (crate::capture::safety_net::Counters, Trust) {
    let mut rig = Rig::new(cfg, compare);
    rig.ticks(3); // the first frame is Unknown
    for i in 0..180u32 {
        // Tiles away from (0,0), where the pointer sits.
        let r = rect(
            40 + ((i * 37) % 180) as i16,
            40 + ((i * 53) % 170) as i16,
            30,
            16,
        );
        let color = 0x0100_0000 + i * 0x0001_0407;
        if i % 2 == 0 {
            rig.draw(r, color);
        } else {
            rig.race(r, color);
        }
        rig.tick();
    }
    let end = rig.ticks(4);
    assert_eq!(end.stale, 0, "the consumer ended stale");
    (rig.net.counters().clone(), rig.net.trust(true))
}

#[test]
fn x11_steady_typing_with_the_sweep_on_confirms_no_corrections() {
    for compare in [Compare::Full, Compare::Candidates] {
        let (c, trust) = typing_run(compare, NetConfig::for_fps(30));
        assert_eq!(c.corrections, 0, "{c:?}");
        assert_eq!(c.distrust_entries, 0, "{c:?}");
        assert_eq!(trust, Trust::Trusted);
        assert!(c.by_hint > 50, "damage never reached the net: {c:?}");
        if compare == Compare::Full {
            assert!(
                c.false_suspects > 30,
                "the draw-between-subtract-and-grab race never produced a suspect, \
                 so the confirmation was not exercised: {c:?}"
            );
        }
    }
}

/// The mutation that makes the test above load-bearing: with the next-tick
/// confirmation off, the same typing is read as missed damage.
#[test]
fn x11_without_confirmation_the_same_typing_demotes_trust() {
    let mut cfg = NetConfig::for_fps(30);
    cfg.confirm = false;
    let (c, trust) = typing_run(Compare::Full, cfg);
    assert!(c.corrections > 30, "{c:?}");
    assert_eq!(trust, Trust::Distrusted);
}

/// Shadow mode (full compare): damage for tile (3,3) is lost inside the
/// backend; the draw is found by the compare at once, is not explained by the
/// next tick, and is counted a would-have-been miss.
#[test]
fn x11_shadow_mode_counts_a_draw_whose_damage_was_lost_and_distrusts() {
    let mut rig = Rig::new(NetConfig::for_fps(30), Compare::Full);
    rig.backend.hooks.lose_damage =
        vec![(3 * TILE as i16, 3 * TILE as i16, TILE as u16, TILE as u16)];
    rig.ticks(3);
    // Control: damage elsewhere is reported and is not a suspect.
    rig.draw(rect(40, 170, 20, 10), 0xff0000);
    rig.ticks(3);
    assert_eq!(rig.net.counters().suspects, 0, "{:?}", rig.net.counters());
    assert!(rig.net.counters().by_hint > 0);
    // The lost one, on a tick no sweep slice covers row 3.
    while rig.sweep_covers_next(3) {
        rig.tick();
    }
    rig.draw(rect(100, 100, 12, 12), 0x00ff00);
    rig.ticks(4);
    let c = rig.net.counters();
    assert_eq!(
        (c.suspects, c.corrections, c.corrections_unswept),
        (1, 1, 1),
        "{c:?}"
    );
    assert_eq!(rig.net.trust(true), Trust::Distrusted);
}

/// The positive case: a consumer compares only hint and slice. The lost
/// damage leaves the tile stale; the sweep slice that reaches row 3 repairs
/// it within `S * k` ticks, the compare then finds a change nothing named,
/// the next tick does not explain it, and trust drops.
#[test]
fn x11_an_injected_miss_is_repaired_within_the_sweep_and_distrusts() {
    let cfg = NetConfig::for_fps(30);
    let mut rig = Rig::new(cfg, Compare::Candidates);
    rig.backend.hooks.lose_damage =
        vec![(3 * TILE as i16, 3 * TILE as i16, TILE as u16, TILE as u16)];
    rig.ticks(3);
    while rig.sweep_covers_next(3) {
        rig.tick();
    }
    rig.draw(rect(100, 100, 12, 12), 0x00ff00);
    let first = rig.tick();
    assert_eq!(
        first.stale, 1,
        "the injected miss did not go stale: the test proves nothing"
    );
    let mut repaired_after = None;
    for n in 1..=(cfg.slices * cfg.every) as usize {
        if rig.tick().stale == 0 {
            repaired_after = Some(n);
            break;
        }
    }
    let n = repaired_after.expect("the sweep never repaired the tile");
    assert!(
        n <= (cfg.slices * cfg.every) as usize,
        "repaired after {n} ticks"
    );
    rig.ticks(3);
    let c = rig.net.counters();
    assert_eq!(c.corrections, 1, "{c:?}");
    assert_eq!(c.suspects_swept, 1, "{c:?}");
    assert_eq!(rig.net.trust(true), Trust::Distrusted);
    // And the demoted state takes everything: nothing is stale after.
    assert_eq!(rig.ticks(2).stale, 0);
}

/// The mutation: with the sweep off the same miss is never repaired, never
/// detected, and trust never drops.
#[test]
fn x11_without_the_sweep_the_injected_miss_persists() {
    let mut cfg = NetConfig::for_fps(30);
    cfg.sweep = false;
    let mut rig = Rig::new(cfg, Compare::Candidates);
    rig.backend.hooks.lose_damage =
        vec![(3 * TILE as i16, 3 * TILE as i16, TILE as u16, TILE as u16)];
    rig.ticks(3);
    rig.draw(rect(100, 100, 12, 12), 0x00ff00);
    rig.tick();
    let after = rig.ticks(3 * (cfg.slices * cfg.every) as usize);
    assert_eq!(
        after.stale, 1,
        "something repaired the tile without the sweep"
    );
    assert_eq!(rig.net.counters().corrections, 0);
    assert_eq!(rig.net.trust(true), Trust::Trusted);
}
