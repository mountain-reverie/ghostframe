//! The band grab against a real X server (a private Xvfb): after every tick,
//! the frame delivered into a rotating pool slot equals what a fresh full
//! grab of the same screen returns (damage-driven encoding PR 5).
//!
//! The painter is deterministic (a seeded xorshift) and draws on its own
//! connection, so what is under test is the grab set: the union of damage
//! over each slot's age, the sweep slice, the cursor's old and new tiles, and
//! the full-grab fallbacks. The reference is a second `X11Frames` on the same
//! display that never receives a plan, so it always grabs the whole root.
//! Each `Mutation` removes one rule and must make the property fail.

use std::collections::VecDeque;
use std::io;

use ghostframe_lib::FrameBuf;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ChangeGCAux, ConnectionExt, CreateGCAux, Rectangle};
use x11rb::rust_connection::RustConnection;

use super::bands::Grab;
use super::frames::Mutation;
use super::test_support::PrivateXvfb;
use super::{damage, frames, X11Backend};
use crate::capture::safety_net::{NetConfig, SafetyNet, SWEEP_EVERY, SWEEP_SLICES};
use crate::capture::tick_stats::TickStats;
use crate::capture::{damage_plan_grab, Backend};

pub(crate) const W: u16 = 512;
pub(crate) const H: u16 = 640; // 16 x 20 tiles: room for bands below the 50 % fraction

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

pub(crate) struct Rig {
    _x: PrivateXvfb,
    pub backend: X11Backend,
    reference: frames::X11Frames,
    net: SafetyNet,
    conn: RustConnection,
    root: u32,
    gc: u32,
    ts: u32,
    /// Frames the "library" still holds: three, so the four slots rotate.
    held: VecDeque<FrameBuf>,
    /// How many frames `held` keeps; 0 returns each slot at once.
    pub hold: usize,
    size: (u16, u16),
}

/// What one tick grabbed and what it cost.
pub(crate) struct TickOut {
    pub grab: Grab,
    pub stats: TickStats,
    /// First differing byte against the fresh full grab, if any.
    pub mismatch: Option<usize>,
}

impl Rig {
    pub fn new(mutation: Mutation) -> Self {
        Self::with_size(mutation, W, H)
    }

    pub fn with_size(mutation: Mutation, w: u16, h: u16) -> Self {
        let x = PrivateXvfb::spawn(&["-screen", "0", &format!("{w}x{h}x24")]);
        let d = x.display().to_string();
        let mut backend = X11Backend {
            frames: frames::X11Frames::connect(Some(&d), None).unwrap(),
            damage: damage::XDamageMonitor::new(Some(&d), crate::config::DamageReport::NonEmpty),
            input: None,
            display: None,
            stats: Default::default(),
            hooks: Default::default(),
        };
        backend.frames.mutation = mutation;
        backend.arm_damage();
        assert!(backend.damage_armed());
        let reference = frames::X11Frames::connect(Some(&d), None).unwrap();
        let (conn, n) = x11rb::connect(Some(&d)).unwrap();
        let root = conn.setup().roots[n].root;
        let gc = conn.generate_id().unwrap();
        conn.create_gc(gc, root, &CreateGCAux::new().foreground(0xffffff))
            .unwrap();
        Self {
            _x: x,
            backend,
            reference,
            net: SafetyNet::new(NetConfig::for_fps(30)),
            conn,
            root,
            gc,
            ts: 1000,
            held: VecDeque::new(),
            hold: 3,
            size: (w, h),
        }
    }

    pub fn fill(&self, r: Rectangle, color: u32) {
        self.conn
            .change_gc(self.gc, &ChangeGCAux::new().foreground(color))
            .unwrap();
        self.conn
            .poly_fill_rectangle(self.root, self.gc, &[r])
            .unwrap();
        self.conn.get_input_focus().unwrap().reply().unwrap();
    }

    pub fn warp(&self, x: i16, y: i16) {
        self.conn
            .warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, x, y)
            .unwrap();
        self.conn.get_input_focus().unwrap().reply().unwrap();
    }

    /// One tick of the capture loop in `on` mode, then the property check.
    pub fn tick(&mut self) -> TickOut {
        self.try_tick().expect("grab")
    }

    pub fn try_tick(&mut self) -> io::Result<TickOut> {
        self.ts += 33_333;
        let (_, frame) = damage_plan_grab(&mut self.backend, &mut self.net, self.ts);
        let frame = frame?;
        let stats = self.backend.take_tick_stats();
        let grab = self.backend.frames.last_grab.expect("a band-capable tick");
        let fresh = self.reference.capture(self.ts).expect("reference grab");
        let mismatch = frame
            .pixels
            .iter()
            .zip(fresh.pixels.iter())
            .position(|(a, b)| a != b);
        self.held.push_back(frame.pixels);
        while self.held.len() > self.hold {
            self.held.pop_front();
        }
        Ok(TickOut {
            grab,
            stats,
            mismatch,
        })
    }

    /// Resize the root the way `xrandr --fb` does.
    pub fn resize(&mut self, w: u16, h: u16) {
        use x11rb::protocol::randr::{self, ConnectionExt as _};
        let res = self
            .conn
            .randr_get_screen_resources(self.root)
            .unwrap()
            .reply()
            .unwrap();
        for crtc in &res.crtcs {
            self.conn
                .randr_set_crtc_config(
                    *crtc,
                    x11rb::CURRENT_TIME,
                    res.config_timestamp,
                    0,
                    0,
                    0,
                    randr::Rotation::ROTATE0,
                    &[],
                )
                .unwrap()
                .reply()
                .unwrap();
        }
        self.conn
            .randr_set_screen_size(self.root, w, h, 10, 10)
            .unwrap()
            .check()
            .unwrap();
        self.conn.get_input_focus().unwrap().reply().unwrap();
        self.size = (w, h);
    }

    /// Install a 16x16 cursor with translucent pixels (premultiplied ARGB,
    /// alpha 0x80 on the left half, opaque on the right), so blending it twice
    /// differs from blending it once.
    pub fn use_translucent_cursor(&self) {
        use x11rb::protocol::render::{self, ConnectionExt as _, PictType};
        use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ImageFormat};
        let formats = self
            .conn
            .render_query_pict_formats()
            .unwrap()
            .reply()
            .unwrap();
        let fmt = formats
            .formats
            .iter()
            .find(|f| {
                f.depth == 32
                    && f.type_ == PictType::DIRECT
                    && f.direct.alpha_mask == 0xff
                    && f.direct.red_mask == 0xff
            })
            .expect("an ARGB32 picture format")
            .id;
        let pix = self.conn.generate_id().unwrap();
        self.conn.create_pixmap(32, pix, self.root, 16, 16).unwrap();
        let mut data = Vec::new();
        for _y in 0..16 {
            for x in 0..16 {
                let (a, c) = if x < 8 {
                    (0x80u8, 0x80u8)
                } else {
                    (0xff, 0xff)
                };
                data.extend_from_slice(&[0, 0, c, a]); // B G R A, premultiplied red
            }
        }
        let gc = self.conn.generate_id().unwrap();
        self.conn.create_gc(gc, pix, &CreateGCAux::new()).unwrap();
        self.conn
            .put_image(ImageFormat::Z_PIXMAP, pix, gc, 16, 16, 0, 0, 0, 32, &data)
            .unwrap();
        let pic = self.conn.generate_id().unwrap();
        self.conn
            .render_create_picture(pic, pix, fmt, &render::CreatePictureAux::new())
            .unwrap();
        let cur = self.conn.generate_id().unwrap();
        self.conn.render_create_cursor(cur, pic, 8, 8).unwrap();
        self.conn
            .change_window_attributes(self.root, &ChangeWindowAttributesAux::new().cursor(cur))
            .unwrap();
        self.conn.get_input_focus().unwrap().reply().unwrap();
    }
}

fn band_area_bytes(g: &Grab) -> u64 {
    let stride = u64::from(W) * 4;
    match g {
        Grab::Full => u64::from(H) * stride,
        Grab::Bands(b) => u64::from(b.rows()) * stride,
    }
}

/// Random local painting and pointer moves for `ticks` ticks; the first
/// failure as a message, or the tallies of what was exercised.
fn run_property(mutation: Mutation, ticks: u32, seed: u64) -> Result<(u32, u32), String> {
    run_property_with(mutation, ticks, seed, false)
}

/// With `still_translucent`, a translucent cursor that rarely moves: the
/// pointer stands still under painting elsewhere, so only the rule that
/// re-grabs the rows under the cursor keeps it from being blended twice.
fn run_property_with(
    mutation: Mutation,
    ticks: u32,
    seed: u64,
    still_translucent: bool,
) -> Result<(u32, u32), String> {
    let mut rig = Rig::new(mutation);
    if still_translucent {
        rig.use_translucent_cursor();
        rig.warp(200, 300);
    }
    let mut rng = Rng(seed | 1);
    let (mut bands_ticks, mut full_ticks) = (0, 0);
    for t in 0..ticks {
        // Mostly small local updates; now and then a wide one that crosses
        // the full-grab fraction; the pointer wanders.
        for _ in 0..rng.below(3) {
            let big = rng.below(12) == 0;
            let (w, h) = if big {
                (u64::from(W), 300 + rng.below(100))
            } else {
                (8 + rng.below(120), 4 + rng.below(60))
            };
            let r = Rectangle {
                x: rng.below(u64::from(W) - 8) as i16,
                y: rng.below(u64::from(H) - 4) as i16,
                width: w as u16,
                height: h as u16,
            };
            rig.fill(r, (rng.next() as u32) & 0xff_ffff);
        }
        if rng.below(if still_translucent { 40 } else { 2 }) == 0 {
            rig.warp(
                rng.below(u64::from(W)) as i16,
                rng.below(u64::from(H)) as i16,
            );
        }
        let out = rig.tick();
        if let Some(i) = out.mismatch {
            return Err(format!(
                "tick {t}: the delivered slot differs from a fresh full grab at byte {i} \
                 (pixel {},{}), grab was {:?}",
                (i / 4) % usize::from(W),
                (i / 4) / usize::from(W),
                out.grab
            ));
        }
        // The counter is the band area, whatever was grabbed.
        let expect = band_area_bytes(&out.grab);
        if out.stats.bytes_grabbed != expect {
            return Err(format!(
                "tick {t}: bytes_grabbed {} != band area {expect} for {:?}",
                out.stats.bytes_grabbed, out.grab
            ));
        }
        match out.grab {
            Grab::Full => full_ticks += 1,
            Grab::Bands(b) => {
                bands_ticks += 1;
                assert_eq!(out.stats.grab_requests as usize, b.as_slice().len());
            }
        }
    }
    Ok((bands_ticks, full_ticks))
}

#[test]
fn x11_every_delivered_slot_equals_a_fresh_full_grab() {
    let (bands, full) = run_property(Mutation::None, 160, 0x5eed).unwrap();
    // Not vacuous: both kinds of grab happened, and many bands ones.
    assert!(bands >= 40, "only {bands} band ticks");
    assert!(full >= 5, "only {full} full ticks");
}

fn mutant_fails(m: Mutation) -> String {
    let e = run_property(m, 160, 0x5eed).expect_err("the mutation must break the property");
    println!("mutation {m:?}: {e}");
    e
}

#[test]
fn x11_mutation_dropping_the_age_union_fails_the_property() {
    mutant_fails(Mutation::DropAgeUnion);
}

#[test]
fn x11_mutation_dropping_the_old_cursor_tiles_fails_the_property() {
    mutant_fails(Mutation::DropOldCursorTiles);
}

#[test]
fn x11_mutation_skipping_a_band_fails_the_property() {
    mutant_fails(Mutation::SkipBand);
}

/// A miss (damage lost before the grab decides anything) is repaired by the
/// sweep in `on` mode: the slot shows stale pixels until the slice reaches
/// the rows, then every later frame is right again.
#[test]
fn x11_a_lost_damage_report_is_repaired_by_the_sweep() {
    let mut rig = Rig::new(Mutation::None);
    // Warm up: every slot known.
    for _ in 0..8 {
        assert_eq!(rig.tick().mismatch, None);
    }
    let r = Rectangle {
        x: 40,
        y: 7 * 32 + 3,
        width: 50,
        height: 20,
    };
    rig.backend
        .hooks
        .lose_damage
        .push((r.x, r.y, r.width, r.height));
    rig.fill(r, 0x00ab_cdef);
    let period = u64::from(SWEEP_EVERY * SWEEP_SLICES);
    let mut stale_ticks = 0;
    let mut clean_run = 0;
    for _ in 0..period + 8 {
        match rig.tick().mismatch {
            Some(_) => {
                stale_ticks += 1;
                clean_run = 0;
            }
            None => clean_run += 1,
        }
    }
    assert!(
        stale_ticks > 0,
        "the miss never showed: the test is vacuous"
    );
    assert!(
        stale_ticks <= period as u32 + 4,
        "stale for {stale_ticks} ticks, longer than the sweep period plus the pool"
    );
    assert!(
        clean_run >= 4,
        "still stale at the end of a sweep period ({clean_run} clean ticks)"
    );
}

#[test]
fn x11_a_standing_translucent_cursor_is_not_blended_twice() {
    run_property_with(Mutation::None, 120, 0xb1e4d, true).unwrap();
}

#[test]
fn x11_mutation_dropping_the_blend_cursor_tiles_fails_the_property() {
    let e = run_property_with(Mutation::DropBlendCursorTiles, 120, 0xb1e4d, true)
        .expect_err("the mutation must break the property");
    println!("mutation DropBlendCursorTiles: {e}");
}

/// A resize to the same byte size (640x512 -> 512x640) keeps every segment, and a
/// slot of age 1 would be refreshed with cursor rows of the old geometry.
#[test]
fn x11_a_resize_to_the_same_byte_size_delivers_the_new_geometry() {
    // Xvfb's RandR maximum is its initial size: start 640x640, shrink to 640x512.
    let mut rig = Rig::with_size(Mutation::None, 640, 640);
    rig.resize(640, 512);
    rig.hold = 0; // each slot returns at once: the next tick reuses it, age 1
    for _ in 0..12 {
        assert_eq!(rig.tick().mismatch, None);
    }
    rig.resize(512, 640);
    rig.fill(
        Rectangle {
            x: 3,
            y: 600,
            width: 20,
            height: 8,
        },
        0x0033_66aa,
    );
    for i in 0..12 {
        let out = rig.tick();
        assert_eq!(
            out.mismatch, None,
            "tick {i} after the resize: {:?}",
            out.grab
        );
    }
}

/// A band request failing after the first band was written leaves a
/// half-refreshed slot; no later delivered slot may show it.
#[test]
fn x11_a_band_failing_mid_grab_leaves_no_stale_slot() {
    let mut rig = Rig::new(Mutation::None);
    for _ in 0..10 {
        assert_eq!(rig.tick().mismatch, None);
    }
    // Two far-apart damaged rows: two bands.
    for y in [2 * 32, 14 * 32] {
        rig.fill(
            Rectangle {
                x: 10,
                y,
                width: 40,
                height: 10,
            },
            0x00ff_0000 + y as u32,
        );
    }
    rig.backend.frames.fail_second_band_once = true;
    assert!(rig.try_tick().is_err(), "the injected failure did not fire");
    rig.fill(
        Rectangle {
            x: 100,
            y: 5 * 32,
            width: 40,
            height: 10,
        },
        0x0000_ff00,
    );
    let mut fulls = 0;
    for i in 0..12 {
        let out = rig.tick();
        assert_eq!(
            out.mismatch, None,
            "tick {i} after the failure: {:?}",
            out.grab
        );
        if out.grab == Grab::Full {
            fulls += 1;
        }
    }
    assert!(fulls >= 1, "the failure should force a full grab");
}

/// A slot left unused for more than AGE_RING ticks has unknown age and must
/// come back with a full grab, not a union of ticks the ring has forgotten.
#[test]
fn x11_a_slot_idle_longer_than_the_age_ring_is_regrabbed_in_full() {
    let mut rig = Rig::new(Mutation::None);
    let mut rng = Rng(0xa9e);
    let mut paint = |rig: &Rig| {
        let r = Rectangle {
            x: rng.below(400) as i16,
            y: rng.below(600) as i16,
            width: 40,
            height: 12,
        };
        rig.fill(r, (rng.next() as u32) & 0xff_ffff);
    };
    for _ in 0..8 {
        paint(&rig);
        assert_eq!(rig.tick().mismatch, None); // all four slots known
    }
    rig.hold = 0; // one slot is reused for 14 ticks; the others go idle
    for _ in 0..14 {
        paint(&rig);
        assert_eq!(rig.tick().mismatch, None);
    }
    rig.hold = 3;
    let mut fulls = 0;
    for _ in 0..8 {
        paint(&rig);
        let out = rig.tick();
        assert_eq!(out.mismatch, None);
        if out.grab == Grab::Full {
            fulls += 1;
        }
    }
    assert!(
        fulls >= 1,
        "no slot came back old enough to need a full grab"
    );
}
