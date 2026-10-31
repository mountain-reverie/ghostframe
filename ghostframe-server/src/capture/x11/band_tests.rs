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

use ghostframe_lib::{FrameBuf, FrameDamage};
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
    /// The previous delivered frame, for the named-rows check.
    prev: Option<Vec<u8>>,
    size: (u16, u16),
}

/// What one tick grabbed and what it cost.
pub(crate) struct TickOut {
    pub grab: Grab,
    pub stats: TickStats,
    /// First differing byte against the fresh full grab, if any.
    pub mismatch: Option<usize>,
    /// What the library was told, and the first tile that differs from the
    /// previous delivered frame although no tile in its row was named.
    pub damage: FrameDamage,
    pub unnamed_change: Option<u32>,
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
            prev: None,
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

    /// One tick in `on` mode with `Unchanged` allowed, and the frame as it
    /// came: no property check.
    pub fn tick_raw(&mut self) -> ghostframe_lib::FrameSubmission {
        self.ts += 33_333;
        self.backend.frames.last_grab = None;
        let (_, frame) = damage_plan_grab(&mut self.backend, &mut self.net, self.ts);
        frame.expect("grab")
    }

    /// A fresh whole-root grab (cursor blended), by the reference connection.
    pub fn fresh(&mut self) -> Vec<u8> {
        self.reference
            .capture(self.ts)
            .expect("reference grab")
            .pixels
            .to_vec()
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
        let unnamed_change = self.unnamed_change(&frame.damage, frame.pixels.as_ref());
        self.prev = Some(frame.pixels.to_vec());
        self.held.push_back(frame.pixels);
        while self.held.len() > self.hold {
            self.held.pop_front();
        }
        Ok(TickOut {
            grab,
            stats,
            mismatch,
            damage: frame.damage,
            unnamed_change,
        })
    }

    /// The library may skip every tile row `damage` does not name, so a tile
    /// that differs from the frame it last saw must be in a named row.
    fn unnamed_change(&self, damage: &FrameDamage, now: &[u8]) -> Option<u32> {
        let (FrameDamage::Tiles { hint, verify }, Some(prev)) = (damage, &self.prev) else {
            return None;
        };
        let (w, h) = (usize::from(self.size.0), usize::from(self.size.1));
        let cols = w.div_ceil(32);
        let named =
            |ty: usize| (0..cols).any(|tx| hint.get(ty * cols + tx) || verify.get(ty * cols + tx));
        for y in 0..h {
            let row = y * w * 4..(y + 1) * w * 4;
            if prev[row.clone()] != now[row.clone()] {
                // Which tile columns changed on this pixel row.
                for tx in 0..cols {
                    let x0 = row.start + tx * 32 * 4;
                    let x1 = (x0 + 32 * 4).min(row.end);
                    if prev[x0..x1] != now[x0..x1] && !named(y / 32) {
                        return Some(((y / 32) * cols + tx) as u32);
                    }
                }
            }
        }
        None
    }

    /// Resize the root the way `xrandr --fb` does.
    pub fn resize(&mut self, w: u16, h: u16) {
        self.prev = None;
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
    let mut told_ticks = 0u32;
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
        if let Some(tile) = out.unnamed_change {
            return Err(format!(
                "tick {t}: tile {tile} changed but no tile in its row is named to the library"
            ));
        }
        if matches!(out.damage, FrameDamage::Tiles { .. }) {
            told_ticks += 1;
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
    // The rows check is vacuous unless the library was told something.
    if told_ticks < ticks / 2 {
        return Err(format!("only {told_ticks} of {ticks} ticks carried Tiles"));
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
        // Nothing may be left unnamed to the library. This holds without the
        // `age_floor` rule too, because the backend's distrust already makes the
        // tick after a failed capture Unknown; the rule itself is pinned by
        // `x11_the_tick_after_a_failed_grab_goes_to_the_library_as_unknown`.
        assert_eq!(out.unnamed_change, None, "tick {i} after the failure");
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
    rig.hold = 0; // one slot is reused for 40 ticks; the others go idle
    for _ in 0..40 {
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

/// The library is told damage as `Tiles` on a believed tick, `Unknown` on the
/// first and on a distrusted one; the hint is exactly the planned mask.
#[test]
fn x11_the_library_is_told_tiles_only_when_damage_is_believed() {
    let mut rig = Rig::new(Mutation::None);
    let first = rig.tick();
    assert_eq!(first.damage, FrameDamage::Unknown, "no history yet");
    for _ in 0..3 {
        rig.tick();
    }
    rig.fill(
        Rectangle {
            x: 40,
            y: 70,
            width: 10,
            height: 10,
        },
        0x00ff_0000,
    );
    let out = rig.tick();
    let FrameDamage::Tiles { hint, verify } = out.damage else {
        panic!("a believed tick must carry Tiles");
    };
    let cols = usize::from(W).div_ceil(32);
    assert!(hint.get(2 * cols + 1), "the painted tile (1, 2) is named");
    let named = (0..cols * usize::from(H).div_ceil(32))
        .filter(|&i| hint.get(i))
        .count();
    assert!(named < 8, "a 10x10 fill names a few tiles, not {named}");
    // A capture nobody planned (`off`, `shadow`) tells the library nothing.
    let unplanned = rig.backend.capture(1).expect("unplanned grab");
    assert_eq!(unplanned.damage, FrameDamage::Unknown);
    // The slice is a whole run of tile rows.
    let sliced = (0..cols * usize::from(H).div_ceil(32))
        .filter(|&i| verify.get(i))
        .count();
    assert_eq!(sliced % cols, 0, "verify is whole tile rows");
}

/// A change in the delivered pixels is always in a row the library is told
/// to compare: through painting, pointer moves and full-grab fallbacks.
#[test]
fn x11_every_changed_row_is_named_to_the_library() {
    run_property(Mutation::None, 160, 0x7a11).unwrap();
}

#[test]
fn x11_mutation_telling_the_library_no_hint_fails_the_rows_check() {
    let e = run_property(Mutation::EmptyLibHint, 160, 0x7a11)
        .expect_err("an empty hint must be caught");
    println!("mutation EmptyLibHint: {e}");
    assert!(e.contains("no tile in its row is named"), "{e}");
}

/// A lost damage report shows in the pixels only when the sweep slice
/// refreshes the rows, and that tick must name them in `verify`.
#[test]
fn x11_a_swept_repair_is_named_to_the_library() {
    for (m, must_fail) in [(Mutation::None, false), (Mutation::EmptyLibVerify, true)] {
        let mut rig = Rig::new(m);
        for _ in 0..8 {
            assert_eq!(rig.tick().unnamed_change, None);
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
        let mut unnamed = 0;
        let mut changed_ticks = 0;
        for _ in 0..period + 8 {
            let out = rig.tick();
            unnamed += u32::from(out.unnamed_change.is_some());
            changed_ticks += u32::from(out.mismatch.is_none());
        }
        assert!(changed_ticks > 0);
        assert_eq!(unnamed > 0, must_fail, "{m:?}: {unnamed} unnamed changes");
    }
}

/// The library is told `Unknown` for the tick after a failed grab, even if the
/// caller (bypassing the backend's distrust) plans it as believed: the failed
/// tick's rows never reached the library, so the new tick's damage names only
/// part of what it lacks.
#[test]
fn x11_the_tick_after_a_failed_grab_goes_to_the_library_as_unknown() {
    use crate::capture::{Damage, TileBitmap};
    let mut rig = Rig::new(Mutation::None);
    for _ in 0..10 {
        assert_eq!(rig.tick().mismatch, None);
    }
    let tiles = |ys: &[u32]| {
        let mut b = TileBitmap::new(u32::from(W), u32::from(H));
        for &y in ys {
            b.set(1, y);
        }
        Damage::Tiles(b)
    };
    let f = &mut rig.backend.frames;
    f.plan_grab(&tiles(&[2, 14]), None, true);
    f.fail_second_band_once = true;
    assert!(f.capture(1).is_err(), "the injected failure did not fire");
    f.plan_grab(&tiles(&[5]), None, true);
    let frame = f.capture(2).unwrap();
    assert_eq!(frame.damage, FrameDamage::Unknown);
    drop(frame);
    // And the one after is believed again.
    f.plan_grab(&tiles(&[5]), None, true);
    assert!(matches!(
        f.capture(3).unwrap().damage,
        FrameDamage::Tiles { .. }
    ));
}

/// A capture that fails before it consumes its plan (every slot in flight)
/// loses that tick's damage: the next plan must not be believed, or the
/// library is told only the new tick's rows.
#[test]
fn x11_the_tick_after_a_would_block_goes_to_the_library_as_unknown() {
    use crate::capture::{Damage, TileBitmap};
    let mut rig = Rig::new(Mutation::None);
    for _ in 0..10 {
        assert_eq!(rig.tick().mismatch, None);
    }
    let tiles = |y: u32| {
        let mut b = TileBitmap::new(u32::from(W), u32::from(H));
        b.set(1, y);
        Damage::Tiles(b)
    };
    let f = &mut rig.backend.frames;
    let mut held = Vec::new();
    while let Ok(fr) = f.capture(1) {
        held.push(fr);
    }
    f.plan_grab(&tiles(2), None, true);
    assert!(f.capture(2).is_err(), "every slot is in flight");
    held.pop();
    f.plan_grab(&tiles(5), None, true);
    let frame = f.capture(3).unwrap();
    assert_eq!(frame.damage, FrameDamage::Unknown);
}

/// What a library that trusts `Unchanged` believes the screen shows after
/// each tick: a pixels frame replaces the rows its damage names (all rows for
/// `Unknown`), an unchanged tick leaves it. It must equal a fresh full grab
/// after every tick, which fails the moment the server says "unchanged" about
/// a tick that changed something.
fn run_unchanged_property(mutation: Mutation, ticks: u32, seed: u64) -> Result<(u32, u32), String> {
    use ghostframe_lib::Content;
    let mut rig = Rig::new(mutation);
    rig.backend.set_allow_unchanged(true);
    let mut rng = Rng(seed | 1);
    let row = usize::from(W) * 4;
    let mut model: Vec<u8> = Vec::new();
    let (mut unchanged, mut pixels, mut refreshed) = (0u32, 0u32, 0u32);
    // The pipeline holds a frame for three ticks, so the slots rotate.
    let mut held: std::collections::VecDeque<(u32, ghostframe_lib::FrameBuf)> = Default::default();
    for t in 0..ticks {
        // Bursts of painting between long idle runs.
        let busy = (t / 12) % 3 == 0;
        if busy && rng.below(2) == 0 {
            let r = Rectangle {
                x: rng.below(u64::from(W) - 8) as i16,
                y: rng.below(u64::from(H) - 4) as i16,
                width: (8 + rng.below(60)) as u16,
                height: (4 + rng.below(40)) as u16,
            };
            rig.fill(r, (rng.next() as u32) & 0xff_ffff);
        }
        if busy && rng.below(8) == 0 {
            rig.warp(
                rng.below(u64::from(W)) as i16,
                rng.below(u64::from(H)) as i16,
            );
        }
        let frame = rig.tick_raw();
        let fresh = rig.fresh();
        match frame.content {
            Content::Unchanged => {
                unchanged += 1;
                // A content-free tick that grabbed kept a free slot current.
                refreshed += u32::from(rig.backend.frames.last_grab.is_some());
                if !frame.pixels.is_empty() {
                    return Err(format!("tick {t}: an unchanged tick carried pixels"));
                }
            }
            Content::Pixels => {
                pixels += 1;
                if let Some(i) = frame.pixels.iter().zip(&fresh).position(|(a, b)| a != b) {
                    return Err(format!(
                        "tick {t}: delivered slot differs from a full grab at byte {i}"
                    ));
                }
                match &frame.damage {
                    FrameDamage::Tiles { hint, verify } if !model.is_empty() => {
                        let cols = usize::from(W).div_ceil(32);
                        for ty in 0..usize::from(H).div_ceil(32) {
                            if (0..cols)
                                .any(|tx| hint.get(ty * cols + tx) || verify.get(ty * cols + tx))
                            {
                                let (y0, y1) = (ty * 32, ((ty + 1) * 32).min(usize::from(H)));
                                model[y0 * row..y1 * row]
                                    .copy_from_slice(&frame.pixels[y0 * row..y1 * row]);
                            }
                        }
                    }
                    _ => model = frame.pixels.to_vec(),
                }
            }
        }
        if model != fresh {
            let i = model
                .iter()
                .zip(&fresh)
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            return Err(format!(
                "tick {t}: the library's screen differs from the real one at pixel {},{} after a {:?} tick",
                (i / 4) % usize::from(W),
                (i / 4) / usize::from(W),
                frame.content
            ));
        }
        held.push_back((t, frame.pixels));
        while held.front().is_some_and(|(at, _)| at + 3 <= t) {
            held.pop_front();
        }
    }
    if refreshed == 0 {
        return Err("no content-free tick ever refreshed a free slot".into());
    }
    Ok((unchanged, pixels))
}

#[test]
fn x11_the_library_screen_stays_exact_across_unchanged_ticks() {
    let (unchanged, pixels) = run_unchanged_property(Mutation::None, 240, 0xfeed).unwrap();
    // Not vacuous: idle ticks went out without pixels, busy ones with.
    assert!(unchanged >= 60, "only {unchanged} unchanged ticks");
    assert!(pixels >= 60, "only {pixels} pixels ticks");
}

#[test]
fn x11_mutation_sending_unchanged_over_damage_fails_the_property() {
    let e = run_unchanged_property(Mutation::UnchangedIgnoresDamage, 240, 0xfeed)
        .expect_err("the mutation must break the property");
    println!("mutation UnchangedIgnoresDamage: {e}");
}
