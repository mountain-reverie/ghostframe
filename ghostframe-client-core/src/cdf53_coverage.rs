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
    pub last_change_us: u64,
    /// Tail sweeps spent on this entry since it last made progress.
    ///
    /// The sweep re-requests every missing pass and clears `nacked_mask` so
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrivalOutcome {
    pub entry: CoverageEntry,
    pub nack_passes: Vec<u8>,
}

/// Apply one CDF53 pass arrival to the coverage entry.
///
/// - If `prev` is `None` OR its generation differs from `generation`, a
///   fresh entry is created with `pass_mask = 0`, `nacked_mask = 0`.
/// - On prevalidation FAILURE: `pass_mask` stays unset for that bit. The
///   failed pass is NACKed once, dedup'd via `nacked_mask`.
/// - On prevalidation SUCCESS: `pass_mask` gets the bit set. If the
///   bitmap grew, `last_change_us` advances and gap-detection scans for
///   lower-indexed missing passes (only on existing-generation arrivals).
pub fn apply_cdf53_arrival(
    prev: Option<CoverageEntry>,
    generation: u8,
    pass_idx: u8,
    frame_seq: u32,
    now_us: u64,
    prevalidation_ok: bool,
) -> ArrivalOutcome {
    let mut e: CoverageEntry;
    let is_new_generation: bool;
    match prev {
        Some(entry) if entry.generation == generation => {
            e = entry;
            e.frame_seq = frame_seq;
            is_new_generation = false;
        }
        _ => {
            e = CoverageEntry {
                generation,
                frame_seq,
                pass_mask: 0,
                nacked_mask: 0,
                last_change_us: now_us,
                sweep_attempts: 0,
            };
            is_new_generation = true;
        }
    }

    let mut nack_passes = Vec::new();

    if !prevalidation_ok {
        let pass_bit = 1u16 << pass_idx;
        if e.nacked_mask & pass_bit == 0 {
            nack_passes.push(pass_idx);
            e.nacked_mask |= pass_bit;
        }
        return ArrivalOutcome {
            entry: e,
            nack_passes,
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
            let sentinel: u16 = (1u16 << pass_idx) - 1;
            let missing_below = sentinel & !e.pass_mask & !e.nacked_mask;
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
    }
}
