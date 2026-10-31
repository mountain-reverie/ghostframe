//! The damage safety net: sweep, confirmation, trust (design 3.4).
//!
//! Damage hints are only ever a claim. This module is what keeps a consumer of
//! them honest, and in shadow mode what measures how honest they are before
//! anything consumes them.
//!
//! Each capture tick the net is told the tick's [`Damage`]; it schedules the
//! sweep slice for that tick (one slice of tile rows every `k` ticks, `S`
//! slices, so every row is verified within `S * k` ticks whatever the damage
//! says) and remembers both in a short ring. The library's full compare then
//! reports, per frame, the tiles that really changed. A changed tile that
//! neither the damage nor the sweep covered is a *suspect*; it becomes a
//! confirmed *correction* only if the next tick's damage does not name it
//! either. A draw that lands between the damage subtract and the grab is in
//! this tick's pixels but reported next tick, so the next tick's damage clears
//! it (B4). A correction demotes trust globally for `distrust_ticks`.
//!
//! The net is pure state driven by the capture loop and by tests; it does no
//! I/O and owns no clock. Everything is counted in ticks.

use std::collections::VecDeque;

use ghostframe_lib::{FeedbackKind, FrameFeedback};

use super::{Damage, TileBitmap};

/// Sweep slices: the screen's tile rows are divided into this many parts.
pub const SWEEP_SLICES: u32 = 12;
/// A slice is due every this many ticks: with [`SWEEP_SLICES`] a full sweep is
/// 60 ticks, 2 s at 30 fps.
pub const SWEEP_EVERY: u32 = 5;
/// Seconds of full-cost capture after a confirmed correction.
pub const DISTRUST_SECS: u64 = 10;
/// Ticks of damage and slice remembered: the frame pool (4) plus the
/// confirmation tick plus the pipeline's lag in feedback.
const HISTORY: usize = 16;

/// Whether damage hints may be believed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Trusted,
    Distrusted,
}

/// A run of whole tile rows, `lo..hi`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slice {
    pub lo: u32,
    pub hi: u32,
}

impl Slice {
    pub fn covers_row(&self, ty: u32) -> bool {
        (self.lo..self.hi).contains(&ty)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NetConfig {
    pub every: u32,
    pub slices: u32,
    pub distrust_ticks: u64,
    /// Test-only mutations: prove the tests that guard each mechanism fail
    /// without it.
    #[cfg(test)]
    pub sweep: bool,
    #[cfg(test)]
    pub confirm: bool,
}

impl NetConfig {
    /// The defaults for a capture rate of `fps`.
    pub fn for_fps(fps: u64) -> Self {
        Self {
            every: SWEEP_EVERY,
            slices: SWEEP_SLICES,
            distrust_ticks: DISTRUST_SECS * fps.max(1),
            #[cfg(test)]
            sweep: true,
            #[cfg(test)]
            confirm: true,
        }
    }

    fn sweep_on(&self) -> bool {
        #[cfg(test)]
        {
            self.sweep
        }
        #[cfg(not(test))]
        {
            true
        }
    }

    fn confirm_on(&self) -> bool {
        #[cfg(test)]
        {
            self.confirm
        }
        #[cfg(not(test))]
        {
            true
        }
    }
}

/// What the net did, cumulatively. All tile counts are tiles, not frames.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Counters {
    pub ticks: u64,
    /// Ticks whose damage was `Unknown` (or for another screen size).
    pub unknown_ticks: u64,
    /// Ticks spent `Distrusted`.
    pub distrusted_ticks: u64,
    /// Sum over ticks of tiles a consumer would have grabbed: the hint plus
    /// the sweep slice (an upper bound; they may overlap).
    pub would_grab_tiles: u64,
    /// Sum over ticks of all tiles.
    pub total_tiles: u64,
    /// Feedback records judged against a tick.
    pub judged: u64,
    /// Feedback that said nothing about single tiles (dropped, saturated, or
    /// for another screen size).
    pub skipped: u64,
    /// Feedback for a tick no longer (or never) in the ring.
    pub stale: u64,
    /// Tiles the library found changed.
    pub dirty_tiles: u64,
    /// ...of which the damage hint named.
    pub by_hint: u64,
    /// ...which an uncompared (dropped) earlier tick had covered: the
    /// reference lagged, the change was reported.
    pub drift: u64,
    /// ...which the damage did not name (and no drift explains): a suspected
    /// miss.
    pub suspects: u64,
    /// ...of the suspects, those the sweep slice of that tick also covered. A
    /// consumer that compares only the hint and the slice finds a miss
    /// exactly this way.
    pub suspects_swept: u64,
    /// Suspects the next tick's damage explained: a draw racing the grab, or
    /// a one-tick miss the next damage happens to name. Neither hides a
    /// persistent miss.
    pub false_suspects: u64,
    /// Suspects the next tick's damage did not explain.
    pub corrections: u64,
    /// ...of the corrections, those no sweep slice covered that tick: the
    /// would-have-been misses, stale in a consumer until the sweep reached
    /// them. The miss count only under a full compare (shadow); a consumer
    /// needs tile-ticks of staleness.
    pub corrections_unswept: u64,
    /// `Trusted` -> `Distrusted` transitions.
    pub distrust_entries: u64,
}

#[derive(Debug, Clone)]
struct TickRec {
    /// `FrameSubmission::timestamp_us`, the tick's identity in feedback.
    ts: u32,
    tick: u64,
    /// The damage was `Unknown`: everything counts as covered.
    unknown: bool,
    damage: TileBitmap,
    slice: Option<Slice>,
}

#[derive(Debug)]
pub struct SafetyNet {
    cfg: NetConfig,
    /// The tick number the next `begin_tick` assigns.
    tick: u64,
    ring: Vec<Option<TickRec>>,
    /// Tiles covered by damage or the sweep on ticks the library never
    /// compared (it dropped the frame): the reference did not move over those
    /// ticks, so a later compare may find such a tile dirty for change this
    /// net was told about then, not for new change.
    touched: TileBitmap,
    /// The verdict being judged, as a bitmap; reused every frame.
    scratch: TileBitmap,
    /// The newest tick whose feedback has been judged.
    judged_through: Option<u64>,
    unjudged: VecDeque<FrameFeedback>,
    trust: Trust,
    distrusted_until: u64,
    counters: Counters,
}

impl SafetyNet {
    pub fn new(cfg: NetConfig) -> Self {
        Self {
            cfg,
            tick: 0,
            ring: vec![None; HISTORY],
            touched: TileBitmap::empty(),
            scratch: TileBitmap::empty(),
            judged_through: None,
            unjudged: VecDeque::new(),
            trust: Trust::Trusted,
            distrusted_until: 0,
            counters: Counters::default(),
        }
    }

    /// The slice due at `tick` for a screen of `rows` tile rows: slice number
    /// `(tick / every) % slices` on every `every`-th tick. Pure arithmetic;
    /// its cost does not depend on the tick, the screen or the damage.
    pub fn slice_for(cfg: &NetConfig, tick: u64, rows: u32) -> Option<Slice> {
        if !cfg.sweep_on() || !tick.is_multiple_of(u64::from(cfg.every)) {
            return None;
        }
        let n = u64::from(cfg.slices);
        let idx = (tick / u64::from(cfg.every)) % n;
        let lo = (idx * u64::from(rows) / n) as u32;
        let hi = ((idx + 1) * u64::from(rows) / n) as u32;
        (lo < hi).then_some(Slice { lo, hi })
    }

    /// Whether hints may be believed this tick. An unarmed damage monitor is
    /// `Distrusted` whatever the history says.
    pub fn trust(&self, armed: bool) -> Trust {
        if armed {
            self.trust
        } else {
            Trust::Distrusted
        }
    }

    #[cfg(test)]
    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// Forget the stream (a client left): history, unjudged feedback and the
    /// drift set. Trust and counters stay.
    pub fn reset_stream(&mut self) {
        self.ring.iter_mut().for_each(|r| *r = None);
        self.unjudged.clear();
        self.touched = TileBitmap::empty();
        self.judged_through = None;
    }

    /// The library's verdict on an earlier frame.
    pub fn ingest(&mut self, fb: FrameFeedback) {
        self.unjudged.push_back(fb);
    }

    /// Start a tick: record its `damage` and sweep slice, judge whatever
    /// feedback is now judgeable, and say what a consumer would grab.
    /// `width` x `height` is the frame this tick grabbed.
    pub fn begin_tick(
        &mut self,
        ts: u32,
        damage: &Damage,
        width: u32,
        height: u32,
        armed: bool,
    ) -> Plan {
        let tick = self.tick;
        self.tick += 1;
        if !self.touched.matches(width, height) {
            // First tick, or a resize: nothing carried over describes this grid.
            self.touched = TileBitmap::new(width, height);
            self.unjudged.clear();
            self.judged_through = None;
            self.ring.iter_mut().for_each(|r| *r = None);
        }
        let (cols, rows) = self.touched.grid();

        let (unknown, hint_tiles) = match damage {
            Damage::Tiles(b) if b.matches(width, height) => (false, b.count() as u64),
            Damage::None => (false, 0),
            Damage::Unknown | Damage::Tiles(_) => (true, 0),
        };
        let slice = Self::slice_for(&self.cfg, tick, rows);
        let slot = (tick % HISTORY as u64) as usize;
        let rec = self.ring[slot].get_or_insert_with(|| TickRec {
            ts,
            tick,
            unknown,
            damage: TileBitmap::new(width, height),
            slice,
        });
        rec.ts = ts;
        rec.tick = tick;
        rec.unknown = unknown;
        rec.slice = slice;
        match damage {
            Damage::Tiles(b) if !unknown => rec.damage.copy_from(b),
            _ => {
                if !rec.damage.matches(width, height) {
                    rec.damage = TileBitmap::new(width, height);
                } else {
                    rec.damage.clear();
                }
            }
        }

        let c = &mut self.counters;
        c.ticks += 1;
        c.total_tiles += u64::from(cols) * u64::from(rows);
        if unknown {
            c.unknown_ticks += 1;
        }
        if self.trust == Trust::Distrusted && tick >= self.distrusted_until {
            self.trust = Trust::Trusted;
            tracing::info!(
                target: "ghostframe::server::damage",
                tick,
                "damage trust restored after a clean period"
            );
        }
        let distrusted = self.trust(armed) == Trust::Distrusted;
        if distrusted {
            self.counters.distrusted_ticks += 1;
        }
        // What a consumer would grab: everything when it cannot believe the
        // hint, else the hint plus the slice (an upper bound; they may overlap).
        let all = u64::from(cols) * u64::from(rows);
        self.counters.would_grab_tiles += if unknown || distrusted {
            all
        } else {
            hint_tiles + slice.map_or(0, |s| u64::from(s.hi - s.lo) * u64::from(cols))
        };

        self.judge(tick);
        Plan {
            tick,
            slice,
            trust: self.trust(armed),
        }
    }

    /// One `damage safety net` line of cumulative counters, for the journal.
    ///
    /// Reading it: `dirty_tiles` is what the library's full compare found
    /// changed; `by_hint` is the part the damage named. `suspects` is the part
    /// it did not: `false_suspects` were explained by the next tick's damage
    /// (a draw racing the grab), `corrections` are the confirmed remainder,
    /// and `corrections_unswept` the corrections no sweep slice covered that
    /// tick, i.e. the would-have-been misses a consumer would have shown
    /// stale until the sweep came round. `would_grab_tiles` against
    /// `total_tiles` is what a consumer would have saved.
    pub fn log_counters(&self, mode: crate::config::DamageMode, armed: bool) {
        let c = &self.counters;
        tracing::info!(
            target: "ghostframe::server::damage",
            mode = ?mode,
            trust = ?self.trust(armed),
            ticks = c.ticks,
            unknown_ticks = c.unknown_ticks,
            distrusted_ticks = c.distrusted_ticks,
            dirty_tiles = c.dirty_tiles,
            by_hint = c.by_hint,
            drift = c.drift,
            suspects = c.suspects,
            suspects_swept = c.suspects_swept,
            false_suspects = c.false_suspects,
            corrections = c.corrections,
            corrections_unswept = c.corrections_unswept,
            distrust_entries = c.distrust_entries,
            would_grab_tiles = c.would_grab_tiles,
            total_tiles = c.total_tiles,
            judged = c.judged,
            skipped = c.skipped,
            stale = c.stale,
            "damage safety net"
        );
    }

    fn rec(&self, tick: u64) -> Option<&TickRec> {
        ring_rec(&self.ring, tick)
    }

    /// Judge, in order, every unjudged record whose tick and the tick after it
    /// are both in the ring. A record whose next tick has not begun waits.
    fn judge(&mut self, now: u64) {
        while let Some(fb) = self.unjudged.front() {
            let Some(t) = (0..=now.min(HISTORY as u64 - 1))
                .map(|back| now - back)
                .find(|&t| self.rec(t).is_some_and(|r| r.ts == fb.timestamp_us))
            else {
                self.counters.stale += 1;
                self.unjudged.pop_front();
                continue;
            };
            if t == now {
                return; // the confirming tick has not begun
            }
            let fb = self.unjudged.pop_front().expect("front was Some");
            self.judge_one(&fb, t);
        }
    }

    fn judge_one(&mut self, fb: &FrameFeedback, t: u64) {
        // Disjoint field borrows throughout: the ring is read while the
        // drift set, scratch and counters are written, with no per-frame copy.
        let ring = &self.ring;
        // Ticks between the last judged one and this one never got a verdict
        // (the library dropped the frame, or the record was lost). The
        // reference did not move over them, so what they damaged is pending
        // drift. (A sweep slice is not a damage claim and adds nothing.)
        let first_gap = self.judged_through.map_or(t, |j| j + 1);
        for g in first_gap..t {
            match ring_rec(ring, g) {
                Some(r) if !r.unknown => self.touched.or_with(&r.damage),
                Some(_) | None => self.touched.set_all(),
            }
        }
        self.judged_through = Some(t);

        let rec_t = ring_rec(ring, t).expect("t is in the ring");
        let confirming = ring_rec(ring, t + 1).expect("t + 1 is in the ring");
        let (w, h) = rec_t.damage.px();

        let have_dirty = match &fb.kind {
            FeedbackKind::Dirty(words) if (fb.width, fb.height) == (w, h) => {
                self.scratch.load_words(w, h, words)
            }
            _ => false,
        };
        if !have_dirty {
            self.counters.skipped += 1;
            match fb.kind {
                // A reference reseeded from this frame: nothing is pending.
                FeedbackKind::Saturated => self.touched.clear(),
                // Not compared: the reference did not move.
                FeedbackKind::Dropped | FeedbackKind::Dirty(_) => {
                    if rec_t.unknown {
                        self.touched.set_all();
                    } else {
                        self.touched.or_with(&rec_t.damage);
                    }
                }
            }
            return;
        }
        self.counters.judged += 1;

        let confirm = self.cfg.confirm_on();
        let mut first_correction = None;
        for (tx, ty) in self.scratch.iter() {
            self.counters.dirty_tiles += 1;
            if rec_t.unknown || rec_t.damage.get(tx, ty) {
                self.counters.by_hint += 1;
                continue;
            }
            if self.touched.get(tx, ty) {
                self.counters.drift += 1;
                continue;
            }
            self.counters.suspects += 1;
            let swept = rec_t.slice.is_some_and(|s| s.covers_row(ty));
            if swept {
                self.counters.suspects_swept += 1;
            }
            let explained = confirming.unknown || confirming.damage.get(tx, ty);
            if explained && confirm {
                self.counters.false_suspects += 1;
            } else {
                self.counters.corrections += 1;
                if !swept {
                    self.counters.corrections_unswept += 1;
                }
                first_correction.get_or_insert((tx, ty));
            }
        }
        // The compare covered every tile (shadow mode): each dirty tile was
        // committed and each clean one has nothing pending, so no drift
        // survives it. A consumer that compares only candidates must clear
        // just the tiles it compared.
        self.touched.clear();
        if let Some(tile) = first_correction {
            self.correct(confirming.tick, tile);
        }
    }

    fn correct(&mut self, at_tick: u64, tile: (u32, u32)) {
        if self.trust == Trust::Trusted {
            self.counters.distrust_entries += 1;
            tracing::warn!(
                target: "ghostframe::server::damage",
                tick = at_tick,
                tile_x = tile.0,
                tile_y = tile.1,
                for_ticks = self.cfg.distrust_ticks,
                "a screen change the damage did not report was confirmed; \
                 ignoring damage hints"
            );
        }
        self.trust = Trust::Distrusted;
        self.distrusted_until = self.distrusted_until.max(at_tick + self.cfg.distrust_ticks);
    }
}

fn ring_rec(ring: &[Option<TickRec>], tick: u64) -> Option<&TickRec> {
    ring[(tick % HISTORY as u64) as usize]
        .as_ref()
        .filter(|r| r.tick == tick)
}

/// What a consumer of the net would do this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub tick: u64,
    /// The sweep slice due, to add to the grab set and the compare mask.
    pub slice: Option<Slice>,
    /// Whether to believe the hint at all this tick.
    pub trust: Trust,
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 320; // 10 x 8 tiles
    const H: u32 = 256;

    fn cfg() -> NetConfig {
        NetConfig::for_fps(30)
    }

    fn bitmap(tiles: &[(u32, u32)]) -> TileBitmap {
        let mut b = TileBitmap::new(W, H);
        for &(x, y) in tiles {
            b.set(x, y);
        }
        b
    }

    fn feedback(ts: u32, tiles: &[(u32, u32)]) -> FrameFeedback {
        let b = bitmap(tiles);
        let mut words = vec![0u64; (10 * 8usize).div_ceil(64)];
        for (x, y) in b.iter() {
            let i = (y * 10 + x) as usize;
            words[i / 64] |= 1 << (i % 64);
        }
        FrameFeedback {
            timestamp_us: ts,
            width: W,
            height: H,
            kind: FeedbackKind::Dirty(words),
        }
    }

    /// Drives a net the way the capture loop does: one `begin_tick` per frame,
    /// the library's verdict for a frame arriving `lag` ticks later.
    struct Sim {
        net: SafetyNet,
        ts: u32,
        lag: usize,
        due: Vec<(usize, FrameFeedback)>,
        t: usize,
    }

    impl Sim {
        fn new(cfg: NetConfig, lag: usize) -> Self {
            Self {
                net: SafetyNet::new(cfg),
                ts: 1000,
                lag,
                due: Vec::new(),
                t: 0,
            }
        }

        /// One tick. `damage` is what X reported for it; `changed` is what
        /// the library's compare finds different in the frame it grabbed.
        fn step(&mut self, damage: Damage, changed: &[(u32, u32)]) -> Plan {
            self.ts += 33_333;
            let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.due)
                .into_iter()
                .partition(|(at, _)| *at <= self.t);
            self.due = rest;
            for (_, fb) in due {
                self.net.ingest(fb);
            }
            let plan = self.net.begin_tick(self.ts, &damage, W, H, true);
            self.due
                .push((self.t + self.lag, feedback(self.ts, changed)));
            self.t += 1;
            plan
        }

        fn quiet(&mut self, n: usize) {
            for _ in 0..n {
                self.step(Damage::None, &[]);
            }
        }
    }

    /// Every row is covered by a slice within `S * k` ticks wherever the
    /// window starts, with the damage not consulted at all; and a tick's slice
    /// is bounded by `ceil(rows / S)` rows however far the run goes (so the
    /// per-tick work does not grow with the tick count or the damage).
    #[test]
    fn every_row_is_swept_within_s_times_k_ticks_and_a_slice_is_bounded() {
        let c = cfg();
        for rows in [1u32, 5, 11, 12, 13, 34, 68, 135] {
            let window = u64::from(c.slices * c.every);
            let bound = rows.div_ceil(c.slices);
            for start in 0..(3 * window) {
                let mut seen = vec![false; rows as usize];
                for t in start..start + window {
                    if let Some(s) = SafetyNet::slice_for(&c, t, rows) {
                        assert!(s.hi - s.lo <= bound, "rows={rows} t={t}: {s:?}");
                        for y in s.lo..s.hi {
                            seen[y as usize] = true;
                        }
                    }
                }
                assert!(
                    seen.iter().all(|&v| v),
                    "rows={rows}: a row went unswept in the window starting at {start}"
                );
            }
        }
        // And far into a run: same bound, same arithmetic.
        let t = 10u64.pow(12);
        let s = SafetyNet::slice_for(&c, t - t % u64::from(c.every), 68).unwrap();
        assert!(s.hi - s.lo <= 68u32.div_ceil(c.slices));
    }

    /// Steady typing: each keystroke's pixels land in the frame one tick
    /// before X reports them (a draw between subtract and grab, B4), at a
    /// tile that moves along a line. With the sweep on this yields suspects
    /// (the measurement fires) and zero corrections.
    fn typing(lag: usize, cfg: NetConfig) -> Counters {
        let mut sim = Sim::new(cfg, lag);
        sim.step(Damage::Unknown, &[]); // the first frame
        for i in 0..900usize {
            let tile = ((i / 3) as u32 % 10, 2);
            match i % 3 {
                // Landed after the subtract: in this frame, not in this damage.
                0 => {
                    sim.step(Damage::None, &[tile]);
                }
                // ...reported now. Nothing new in the pixels.
                1 => {
                    sim.step(Damage::Tiles(bitmap(&[tile])), &[]);
                }
                // A keystroke reported in the tick it lands.
                _ => {
                    let t2 = (tile.0, 3);
                    sim.step(Damage::Tiles(bitmap(&[t2])), &[t2]);
                }
            }
        }
        sim.quiet(40);
        sim.net.counters().clone()
    }

    #[test]
    fn steady_typing_with_the_sweep_on_confirms_no_corrections() {
        for lag in [0usize, 1, 3] {
            let c = typing(lag, cfg());
            assert!(
                c.false_suspects > 100,
                "lag {lag}: the race never produced a suspect, the test proves nothing: {c:?}"
            );
            assert_eq!(c.corrections, 0, "lag {lag}: {c:?}");
            assert_eq!(c.distrust_entries, 0, "lag {lag}: {c:?}");
            assert_eq!(c.stale, 0, "lag {lag}: {c:?}");
        }
    }

    /// The mutation: without the next-tick confirmation every racing draw is
    /// a correction. This is what makes the test above load-bearing.
    #[test]
    fn without_confirmation_typing_demotes_trust() {
        let mut c = cfg();
        c.confirm = false;
        let r = typing(1, c);
        assert!(r.corrections > 100 && r.distrust_entries >= 1, "{r:?}");
    }

    #[test]
    fn a_change_no_damage_ever_named_is_a_confirmed_correction_then_trust_returns() {
        let mut sim = Sim::new(cfg(), 1);
        sim.step(Damage::Unknown, &[]);
        sim.quiet(5);
        assert_eq!(sim.net.trust(true), Trust::Trusted);
        sim.step(Damage::None, &[(4, 4)]); // changed, unreported
        sim.quiet(3);
        let c = sim.net.counters().clone();
        assert_eq!((c.suspects, c.corrections, c.distrust_entries), (1, 1, 1));
        assert_eq!(sim.net.trust(true), Trust::Distrusted);
        // Distrusted for T ticks, then Trusted after a clean period.
        let t = cfg().distrust_ticks as usize;
        sim.quiet(t - 10);
        assert_eq!(sim.net.trust(true), Trust::Distrusted);
        sim.quiet(20);
        assert_eq!(sim.net.trust(true), Trust::Trusted);
    }

    #[test]
    fn a_second_correction_while_distrusted_extends_the_period() {
        let mut sim = Sim::new(cfg(), 0);
        sim.step(Damage::Unknown, &[]);
        sim.step(Damage::None, &[(4, 4)]);
        sim.quiet(2);
        let t = cfg().distrust_ticks as usize;
        sim.quiet(t - 20);
        sim.step(Damage::None, &[(6, 6)]);
        sim.quiet(2);
        sim.quiet(t - 10);
        assert_eq!(sim.net.trust(true), Trust::Distrusted, "not extended");
        assert_eq!(sim.net.counters().distrust_entries, 1);
    }

    #[test]
    fn an_unarmed_monitor_is_distrusted_whatever_the_history() {
        let net = SafetyNet::new(cfg());
        assert_eq!(net.trust(false), Trust::Distrusted);
        assert_eq!(net.trust(true), Trust::Trusted);
    }

    /// A change the damage never named is a suspect whether or not the sweep
    /// slice covers it that tick; the slice only says whether a consumer would
    /// have found it at once (`suspects_swept`) or left it stale
    /// (`corrections_unswept`).
    #[test]
    fn a_missed_change_is_a_correction_and_the_slice_decides_whether_it_was_stale() {
        let run = |swept: bool| {
            let mut sim = Sim::new(cfg(), 0);
            sim.step(Damage::Unknown, &[]);
            for _ in 0..80 {
                let next = sim.net.tick;
                let covers = SafetyNet::slice_for(&cfg(), next, 8).is_some_and(|s| s.covers_row(4));
                if covers == swept {
                    sim.step(Damage::None, &[(4, 4)]);
                    break;
                }
                sim.step(Damage::None, &[]);
            }
            sim.quiet(3);
            sim.net.counters().clone()
        };
        let c = run(true);
        assert_eq!(
            (c.suspects_swept, c.corrections, c.corrections_unswept),
            (1, 1, 0),
            "{c:?}"
        );
        let c = run(false);
        assert_eq!(
            (c.suspects_swept, c.corrections, c.corrections_unswept),
            (0, 1, 1),
            "{c:?}"
        );
    }

    /// A frame the library dropped leaves the reference where it was: what
    /// that tick covered is pending, so the next compare finding it is not a
    /// miss.
    #[test]
    fn a_dropped_frame_does_not_turn_covered_change_into_a_miss() {
        let mut sim = Sim::new(cfg(), 0);
        sim.step(Damage::Unknown, &[]);
        sim.step(Damage::Tiles(bitmap(&[(5, 5)])), &[]);
        // Replace that tick's verdict with "dropped".
        sim.due.clear();
        sim.net.ingest(FrameFeedback {
            timestamp_us: sim.ts,
            width: W,
            height: H,
            kind: FeedbackKind::Dropped,
        });
        sim.step(Damage::None, &[(5, 5)]);
        sim.quiet(3);
        let c = sim.net.counters();
        assert_eq!((c.corrections, c.suspects), (0, 0), "{c:?}");
        assert_eq!(c.drift, 1);
    }

    /// After a gap the drift set holds the dropped ticks' damage; a verdict
    /// from a full compare settles it, so the next unreported change is a
    /// suspect, not drift.
    #[test]
    fn a_full_compare_settles_the_drift_set() {
        let mut sim = Sim::new(cfg(), 0);
        sim.step(Damage::Unknown, &[]);
        sim.step(Damage::Unknown, &[]); // judged as Dirty: settles
        sim.due.clear();
        sim.net.ingest(FrameFeedback {
            timestamp_us: sim.ts,
            width: W,
            height: H,
            kind: FeedbackKind::Dropped, // an Unknown tick, not compared: set_all
        });
        sim.step(Damage::None, &[]); // a clean Dirty verdict
        sim.step(Damage::None, &[(6, 6)]); // unreported change
        sim.quiet(3);
        let c = sim.net.counters();
        assert_eq!((c.suspects, c.drift), (1, 0), "{c:?}");
    }

    #[test]
    fn an_unknown_tick_covers_everything() {
        let mut sim = Sim::new(cfg(), 0);
        sim.step(Damage::Unknown, &[(1, 1), (9, 7)]);
        sim.step(Damage::Unknown, &[(3, 3)]);
        sim.quiet(3);
        let c = sim.net.counters();
        assert_eq!((c.by_hint, c.suspects), (3, 0), "{c:?}");
    }

    #[test]
    fn feedback_for_a_tick_that_left_the_ring_is_counted_stale_not_judged() {
        let mut sim = Sim::new(cfg(), 0);
        for _ in 0..(HISTORY + 4) {
            sim.step(Damage::None, &[]);
        }
        sim.net.ingest(feedback(1234, &[(0, 0)]));
        sim.quiet(2);
        let c = sim.net.counters();
        assert_eq!((c.stale, c.suspects), (1, 0));
    }

    #[test]
    fn a_resize_discards_what_described_the_old_screen() {
        let mut net = SafetyNet::new(cfg());
        net.begin_tick(1, &Damage::Unknown, W, H, true);
        net.ingest(feedback(1, &[(0, 0)]));
        // Next frame is another size: the old verdict is unjudgeable.
        net.begin_tick(2, &Damage::Unknown, 64, 64, true);
        net.begin_tick(3, &Damage::None, 64, 64, true);
        assert_eq!(net.counters().suspects, 0);
        assert_eq!(net.counters().judged, 0);
    }
}
