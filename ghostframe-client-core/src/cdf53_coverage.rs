//! Port of `ghostframe-web-client/src/cdf53_coverage.ts`.
//!
//! Per-(tile, generation) CDF53 pass-coverage bookkeeping. Holds the
//! client's view of which passes have been successfully received and
//! which have been NACKed.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CoverageEntry {
    pub generation: u8,
    pub frame_seq: u32,
    pub pass_mask: u16,
    pub nacked_mask: u16,
    /// The tile's `present_passes` bitmap (bit *i* set ⇒ pass *i* was
    /// actually sent for this generation), learned from pass 0's payload.
    /// `None` until pass 0 arrives -- a tile is not "complete" while this is
    /// `None`, and the only pass worth asking for in that state is pass 0
    /// itself (there is no way to know which of 1..13 to expect yet).
    /// Reset to `None` on every new generation, like the rest of the entry.
    pub present_passes: Option<u16>,
    pub last_change_us: u64,
    /// Tail sweeps spent on this entry since it last made progress.
    ///
    /// The sweep re-requests every missing pass that has gap evidence (see
    /// `highest_pass_seen`) and clears `nacked_mask` so
    /// the request can repeat, which has no terminating condition of its own:
    /// a pass that will never arrive is asked for every 500 ms for the life of
    /// the session. Measured in production on a *static* screen with nothing
    /// to send -- 106,847 NACKs from the client against 94,834 server
    /// retransmissions -- and reproduced as exactly linear growth in session
    /// duration (56 NACKs over 12 s, 120 over 24 s).
    ///
    /// Reset whenever a pass actually lands, so a tile still making progress
    /// keeps its full budget and only a stalled one gives up.
    pub sweep_attempts: u8,
    /// Highest `pass_idx` of this generation that has arrived off the wire,
    /// whether or not it validated. `None` until something has.
    ///
    /// This is the tail sweep's *gap evidence*. A pass in `present_passes`
    /// that has not arrived is only known to be lost if a higher pass of the
    /// same generation did: the server sends a tile's passes in order, so a
    /// later pass landing means the earlier one was dropped (or reordered
    /// past it). A pass above everything received may simply not have been
    /// sent yet -- refinement passes are queued behind bandwidth -- and
    /// asking for it measures the server's queue, not the network. See
    /// [`CoverageEntry::gap_evidence_mask`].
    pub highest_pass_seen: Option<u8>,
}

impl CoverageEntry {
    /// The passes the tail sweep may ask for: the missing ones that a
    /// higher-numbered arrival of the same generation vouches were sent.
    ///
    /// `missing` is the caller's judgement of what is still owed (the tile's
    /// own `present_passes` minus `pass_mask`, or pass 0 alone while the
    /// bitmap is unknown). The result is `missing` restricted to
    /// `pass_idx <= highest_pass_seen`; the highest pass itself is included
    /// because it can be in `missing` only if it arrived but failed
    /// prevalidation, which is a request already owed.
    ///
    /// No arrival, no evidence: silence alone never produces a request.
    pub fn gap_evidence_mask(&self, missing: u16) -> u16 {
        match self.highest_pass_seen {
            None => 0,
            Some(h) => missing & ((2u16 << h.min(14)) - 1),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrivalOutcome {
    pub entry: CoverageEntry,
    pub nack_passes: Vec<u8>,
    /// The arrival began a new generation of the tile, so anything held for
    /// the old one (the coverage entry, and the CPU integrator's planes,
    /// which key on the 4-bit generation alone) belongs to different
    /// content. False for a stale arrival and for one that merely adds to
    /// the generation already held.
    pub new_generation: bool,
    /// Whether `entry` should replace the tile's stored coverage. False for
    /// a stale arrival, and for a would-be new generation whose payload
    /// failed prevalidation: that one has not shown it is real, so it must
    /// not displace the generation the tile already holds. `entry` is then
    /// the held entry (or, with none held, the entry the arrival would have
    /// created, for its NACK bookkeeping), and is not to be stored.
    pub committed: bool,
}

/// Is `candidate` a newer 4-bit generation than `current`?
///
/// Generations wrap at 16, so this cannot be a plain `>`: after 15 comes 0,
/// and 0 is newer. A forward distance under 8 is an advance, anything else a
/// step back.
///
/// **This cannot order generations that are far apart, and must not be used
/// to.** A pass that is 9..=15 generations late has a forward distance of
/// 9..=15, which this reads as 1..=7 generations *ahead*. A tile that changes
/// every frame reaches that in 9 frames (150 ms at 60 fps) of queueing or
/// reordering, and acting on the answer is worse than ignoring the pass: the
/// entry is rewound to the old generation and every pass of the real current
/// one is then dropped as stale. Order arrivals with [`arrival_is_stale`],
/// which uses the datagram's `frame_seq`; this is only its tie-break for two
/// generations that share a label.
pub fn generation_is_newer(candidate: u8, current: u8) -> bool {
    let forward = candidate.wrapping_sub(current) & 0x0F;
    forward != 0 && forward < 8
}

/// `frame_seq` is 31 bits on the wire (bit 31 is `TILE_DATAGRAM_FLAG`).
const FRAME_SEQ_MASK: u32 = 0x7FFF_FFFF;

/// Is label `a` strictly newer than label `b`? RFC 1982 serial arithmetic on
/// the 31-bit `frame_seq`, so a wrap is safe.
pub fn frame_seq_is_newer(a: u32, b: u32) -> bool {
    let forward = a.wrapping_sub(b) & FRAME_SEQ_MASK;
    forward != 0 && forward < (1 << 30)
}

/// How an arrival relates to the generation a tile currently holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrivalOrder {
    /// Another datagram of the generation held: accumulate.
    Current,
    /// Content newer than what is held: start over from this arrival.
    NewGeneration,
    /// Content the tile has already superseded: ignore.
    Stale,
}

/// How many frames ahead of the newest label received off the wire a label
/// may be and still be believed.
///
/// A datagram rebuilt from parity is the one arrival whose label is not
/// vouched for by QUIC: if the group was mis-assembled, the bytes (label
/// included) are arbitrary. A real rebuilt datagram was sent no later than
/// the group-mates that let it be rebuilt, and a label is the `seq` of a
/// frame at or before the datagram's own, so it can be ahead of the newest
/// received label by at most a parity group's worth of frames. 2^12 frames
/// (about 68 s at 60 fps) is hundreds of times that, yet 2^18 times smaller
/// than the 2^30 serial half-range a poisoned label needs to be accepted as
/// "newer". See [`label_runs_ahead`].
pub const LABEL_WINDOW: u32 = 1 << 12;

/// A held label this far behind the newest label received is treated as
/// expired. Serial comparison inverts at 2^30 frames (about 207 days at
/// 60 fps); a tile unchanged for that long would otherwise reject its next
/// generation. Expiring at 2^29 leaves a full 2^29 of margin before the
/// inversion (provided some datagram is received in the interval, which a
/// live session does), and no live session's current content is that old.
pub const LABEL_EXPIRY: u32 = 1 << 29;

fn forward(a: u32, b: u32) -> u32 {
    (a & FRAME_SEQ_MASK).wrapping_sub(b & FRAME_SEQ_MASK) & FRAME_SEQ_MASK
}

/// Is `label` implausibly far ahead of `newest_received`, the newest label
/// seen on a datagram that came off the wire?
///
/// With nothing received yet (`None`) there is no evidence to judge by, and
/// the label is believed: the first tile datagram of a session has to be
/// accepted, and a parity rebuild cannot precede every received datagram
/// because it is built from them.
pub fn label_runs_ahead(label: u32, newest_received: Option<u32>) -> bool {
    match newest_received {
        None => false,
        Some(newest) => {
            let ahead = forward(label, newest);
            ahead > LABEL_WINDOW && ahead < (1 << 30)
        }
    }
}

/// Has a held label aged out relative to the newest label received? See
/// [`LABEL_EXPIRY`].
pub fn label_expired(held_label: u32, newest_received: Option<u32>) -> bool {
    match newest_received {
        None => false,
        Some(newest) => {
            // Modulo 2^31, "behind by 2^30 or more" is what a serial
            // comparison reads as *ahead*; that is exactly the inversion to
            // catch, so it counts. Only a label ahead of the newest by up to
            // `LABEL_WINDOW` (a rebuilt datagram, legitimately) does not.
            let behind = forward(newest, held_label);
            behind > LABEL_EXPIRY && behind < (1 << 31) - LABEL_WINDOW
        }
    }
}

/// Order an arrival of generation `generation`, labelled `frame_seq`, against
/// the generation the tile holds.
///
/// The server stamps every datagram of one tile generation -- first send, NACK
/// and timer repairs, scheduler retries, refinement passes sent many frames
/// later -- with the `frame_seq` of that generation's *first* datagram, and
/// parity rebuilds restore the original bytes, label included. So the label
/// is a per-generation identity that also grows with every generation, and
/// unlike the 4-bit generation it does not wrap within any realistic session.
/// Two datagrams with different labels are therefore different content, even
/// when their 4-bit generations are equal.
///
/// - Different label: the label decides. Newer is a new generation, older is
///   superseded. This includes an *equal* 4-bit generation: a tile that
///   spends 16 generations in PalRle/Solid (which do not touch the CDF 5/3
///   entry) returns on an aliased number, a scheduler resize resets
///   generations to 0 while the client keeps its state, and the fallback
///   path bumps twice per change. Merging those into the held generation
///   painted old coefficients into new content and hid the new generation's
///   gaps from NACK.
/// - Same label, same generation: more of the generation held.
/// - Same label, different generation: the server's drain can label two
///   generations of a tile with one frame when both are sent within it;
///   they are at most a handful of generations apart, which is what the
///   4-bit comparison can order.
pub fn classify_arrival(
    held_generation: u8,
    held_frame_seq: u32,
    generation: u8,
    frame_seq: u32,
) -> ArrivalOrder {
    let (new, old) = (frame_seq & FRAME_SEQ_MASK, held_frame_seq & FRAME_SEQ_MASK);
    if new != old {
        return if frame_seq_is_newer(new, old) {
            ArrivalOrder::NewGeneration
        } else {
            ArrivalOrder::Stale
        };
    }
    if generation == held_generation {
        ArrivalOrder::Current
    } else if generation_is_newer(generation, held_generation) {
        ArrivalOrder::NewGeneration
    } else {
        ArrivalOrder::Stale
    }
}

/// Should this arrival be discarded because the tile already holds newer
/// content? See [`classify_arrival`].
pub fn arrival_is_stale(
    held_generation: u8,
    held_frame_seq: u32,
    generation: u8,
    frame_seq: u32,
) -> bool {
    classify_arrival(held_generation, held_frame_seq, generation, frame_seq) == ArrivalOrder::Stale
}

/// Apply one CDF53 pass arrival to the coverage entry.
///
/// - If `prev` is `None`, or the arrival is a newer generation by
///   [`classify_arrival`] (a newer label, including one with the same 4-bit
///   generation), a fresh entry is created with `pass_mask = 0`,
///   `nacked_mask = 0`.
/// - If the arrival is stale, it is ignored entirely: the entry is returned
///   untouched and no NACK is raised.
/// - On prevalidation FAILURE: `pass_mask` stays unset for that bit. The
///   failed pass is NACKed once, dedup'd via `nacked_mask`.
/// - On prevalidation SUCCESS: `pass_mask` gets the bit set. If the
///   bitmap grew, `last_change_us` advances and gap-detection scans for
///   lower-indexed missing passes (only on existing-generation arrivals).
///
/// `present_passes` is the freshly-parsed bitmap from `PrevalidatedCdf53`
/// (`Some` only when `pass_idx == 0` and prevalidation succeeded; `None`
/// otherwise). When `Some`, it's stored on the entry; when `None`, the
/// entry's existing value (if any) is left untouched -- passes 1..13 carry
/// no bitmap of their own and must not clobber what pass 0 already taught
/// the tile.
pub fn apply_cdf53_arrival(
    prev: Option<CoverageEntry>,
    generation: u8,
    pass_idx: u8,
    frame_seq: u32,
    now_us: u64,
    prevalidation_ok: bool,
    present_passes: Option<u16>,
) -> ArrivalOutcome {
    // A pass from a generation *older* than the entry's is stale: the server
    // has already superseded that content. Accepting it used to fall into the
    // catch-all below and reset the entry back to the old generation, which
    // discarded everything accumulated for the current one.
    //
    // Reproduced as a tile that never converges. Its arrival sequence was
    // `gen0 p0..p13, gen1 p0, gen0 p12, gen1 p1..p13`: the late `gen0 p12`
    // rewound the entry to generation 0, `gen1 p1` reset it forward again,
    // and generation 1's pass 0 was lost from the mask for good. The tile
    // could never reach a complete pass set and never got its base layer, so
    // it rendered wrong no matter how long it waited.
    let order = match prev {
        None => ArrivalOrder::NewGeneration,
        Some(entry) => classify_arrival(entry.generation, entry.frame_seq, generation, frame_seq),
    };
    if order == ArrivalOrder::Stale {
        return ArrivalOutcome {
            entry: prev.expect("only a held entry can make an arrival stale"),
            nack_passes: Vec::new(),
            new_generation: false,
            committed: false,
        };
    }

    let is_new_generation = order == ArrivalOrder::NewGeneration;
    let mut e: CoverageEntry = match prev {
        Some(entry) if !is_new_generation => entry,
        _ => CoverageEntry {
            generation,
            frame_seq,
            pass_mask: 0,
            nacked_mask: 0,
            present_passes: None,
            last_change_us: now_us,
            sweep_attempts: 0,
            highest_pass_seen: None,
        },
    };

    // Pass 0's bitmap, once known, is stuck to the entry regardless of
    // whether this particular arrival is the one that set it (passes 1..13
    // carry `None` and must not clobber it).
    if let Some(p) = present_passes {
        e.present_passes = Some(p);
    }

    let mut nack_passes = Vec::new();

    // Gap evidence for the tail sweep: this pass arrived, so every lower
    // pass of the generation was sent before it. A failed validation counts
    // -- the datagram was on the wire; its payload is what is wrong.
    //
    // Only a real pass index counts: a corrupt 14 or 15 would otherwise set
    // the evidence to 15 and unlock every pass of the tile.
    if pass_idx < 14 {
        e.highest_pass_seen = Some(e.highest_pass_seen.map_or(pass_idx, |h| h.max(pass_idx)));
    }

    if !prevalidation_ok {
        let pass_bit = 1u16 << pass_idx;
        if e.nacked_mask & pass_bit == 0 {
            nack_passes.push(pass_idx);
            e.nacked_mask |= pass_bit;
        }
        // A new generation is only believed once a payload of it has been
        // shown to be real. One that fails prevalidation (a datagram rebuilt
        // from the wrong parity group, a server bug) must not displace the
        // generation held, least of all by moving its label: a label far
        // ahead would make every later, genuine generation read as stale.
        let committed = !is_new_generation;
        return ArrivalOutcome {
            entry: if committed { e } else { prev.unwrap_or(e) },
            nack_passes,
            new_generation: false,
            committed,
        };
    }

    let before = e.pass_mask;
    e.pass_mask |= 1u16 << pass_idx;
    // A pass landed: the tile is making progress, so the sweep budget
    // is refreshed. Only a stalled tile exhausts it.
    e.sweep_attempts = 0;
    if e.pass_mask != before {
        e.last_change_us = now_us;
        if !is_new_generation {
            // Gap detection is judged against the tile's OWN pass set, the
            // same rule `tail_sweep` uses. Sparse encoding skips bit-planes
            // that are entirely zero, so a lower index is not evidence of a
            // gap -- it is usually a pass the server never sent. Measured on
            // a static 16-tile lossless scene: treating every lower index as
            // expected produced 80 NACKs, all misses, because passes 1-5 are
            // empty for essentially all content and no cache entry exists for
            // them.
            //
            // Until pass 0 arrives its bitmap is unknown, and pass 0 is the
            // only pass guaranteed to exist -- there is no way to know which
            // of 1..13 to expect without it, so assume nothing else.
            let expected: u16 = e.present_passes.unwrap_or(1);
            let sentinel: u16 = (1u16 << pass_idx) - 1;
            let missing_below = sentinel & expected & !e.pass_mask & !e.nacked_mask;
            if missing_below != 0 {
                for p in 0..pass_idx {
                    if missing_below & (1u16 << p) != 0 {
                        nack_passes.push(p);
                    }
                }
                e.nacked_mask |= missing_below;
            }
        }
    }

    ArrivalOutcome {
        entry: e,
        nack_passes,
        new_generation: is_new_generation,
        committed: true,
    }
}

/// A snapshot of per-tile Cdf53 coverage, for diagnostics.
///
/// The browser used to compute this itself from a `window.__cdf53Coverage`
/// map maintained by the TypeScript decode path. That path was deleted at
/// the wasm cutover and the map was never repopulated, so the client's
/// `cdf53-coverage` log line reported `tiles=0 refined=0` on sessions that
/// had in fact decoded well over a thousand tiles. This puts the same view
/// back, sourced from the state that actually exists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cdf53CoverageSummary {
    /// Tiles with any coverage state at all.
    pub tiles: u32,
    /// Tiles whose received passes equal their `present_passes` bitmap.
    pub complete: u32,
    /// Tiles still missing at least one present pass.
    pub partial: u32,
    /// Tiles whose bitmap is still unknown because pass 0 has not arrived.
    /// These cannot be judged complete at all, and can only ask for pass 0.
    pub bitmap_unknown: u32,
    /// Tiles that have exhausted their tail-sweep budget and stopped asking:
    /// a gap that was known (a higher pass arrived) and never repaired.
    /// This is NOT the stranding signal. A tile whose last pass was lost
    /// has no gap evidence, never asks, and never counts here; it shows in
    /// `partial`.
    pub gave_up: u32,
    /// Histogram over the number of received passes (index = popcount of
    /// `pass_mask`, 0..=14).
    pub pass_hist: [u32; 15],
}

impl Cdf53CoverageSummary {
    /// Compact one-line form for a log.
    pub fn to_log_line(&self) -> String {
        let hist: Vec<String> = self
            .pass_hist
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .map(|(i, n)| format!("{i}:{n}"))
            .collect();
        format!(
            "tiles={} complete={} partial={} bitmap_unknown={} gave_up={} pass-hist{{{}}}",
            self.tiles,
            self.complete,
            self.partial,
            self.bitmap_unknown,
            self.gave_up,
            hist.join(" ")
        )
    }
}

/// Summarise a coverage map. `max_sweeps` is the tail-sweep give-up
/// threshold, passed in so this stays a pure function over the map.
pub fn summarize<'a, I>(entries: I, max_sweeps: u8) -> Cdf53CoverageSummary
where
    I: IntoIterator<Item = &'a CoverageEntry>,
{
    let mut s = Cdf53CoverageSummary::default();
    for e in entries {
        s.tiles += 1;
        let received = e.pass_mask.count_ones() as usize;
        s.pass_hist[received.min(14)] += 1;
        match e.present_passes {
            Some(present) => {
                if e.pass_mask & present == present {
                    s.complete += 1;
                } else {
                    s.partial += 1;
                }
            }
            None => s.bitmap_unknown += 1,
        }
        if e.sweep_attempts >= max_sweeps {
            s.gave_up += 1;
        }
    }
    s
}
