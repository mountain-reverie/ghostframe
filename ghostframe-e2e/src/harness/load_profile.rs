//! Generated, sustained scene load for the browserless harness.
//!
//! A scripted `BrowserlessScene` lists its frames explicitly, which is right
//! for small deterministic scenes and unworkable for the hundreds a
//! production-cadence scene needs. It also makes every such scene a short
//! burst followed by silence: the harness injects the script, runs out, and
//! falls back to empty heartbeats for the rest of the duration. Production's
//! capture loop free-runs instead, producing a frame every tick for as long
//! as a client stays connected.
//!
//! That difference is not cosmetic. At production cadence the 8x8 CDF53 probe
//! scene completes zero probe clusters, where the same scene at the harness's
//! old 16 ms cadence completed ten of ten — see
//! `docs/specs/bwe-probe-emission-timing.md`.

use std::time::Duration;

use crate::harness::browserless::FrameScript;
use crate::harness::scene_tiles::TileSpec;

/// Production's capture-to-dispatch interval: `IoBridge`'s
/// `SCHEDULER_TICK_INTERVAL_US`. A scene injecting faster than this hands
/// probe windows more emission opportunities than production ever would.
pub const PRODUCTION_CADENCE_US: u64 = 33_333;

/// How much of the screen changes on each tick.
#[derive(Clone, Debug)]
pub enum Churn {
    /// Every tile rewritten each tick. The heaviest offered load available,
    /// and the shape `busy_frames_grid` already produces.
    FullGrid,
    /// A damage window of `tiles_per_tick` tiles walking the grid in
    /// row-major order, wrapping. Closer to what a desktop actually dirties —
    /// a dragged window or a scrolling pane touches a region, not the whole
    /// screen — which changes both byte volume and which tiles reach the
    /// refinement queue.
    Region { tiles_per_tick: usize },
}

// Generation cost, measured 2026-09-13 over a 10 s scene on an 8x8 grid:
// `FullGrid` produces 19,200 tiles in ~1.38 s, `Region { tiles_per_tick: 4 }`
// produces 1,200 in ~83 ms. That is generation alone, before CDF53 encoding
// and the protocol loop, so prefer `Region` unless a scene genuinely needs
// to offer more load than its link cap can carry.

/// Sustained load for a scene's whole duration.
#[derive(Clone, Debug)]
pub struct LoadProfile {
    pub cadence_us: u64,
    pub churn: Churn,
}

impl LoadProfile {
    /// Generate one `FrameScript` per tick for `duration`.
    ///
    /// Tile content varies with the tick index so every frame carries real,
    /// distinct CDF53 bit-planes. A generator emitting identical bytes each
    /// tick would produce no backlog and quietly defeat every scene built on
    /// it, while still looking like load.
    pub fn frames_for(&self, duration: Duration, cols: u8, rows: u8) -> Vec<FrameScript> {
        let ticks = (duration.as_micros() as u64 / self.cadence_us) as usize;
        let total = cols as usize * rows as usize;
        (0..ticks)
            .map(|i| {
                let coords: Vec<(u8, u8)> = match &self.churn {
                    Churn::FullGrid => (0..cols)
                        .flat_map(|x| (0..rows).map(move |y| (x, y)))
                        .collect(),
                    Churn::Region { tiles_per_tick } => {
                        let n = (*tiles_per_tick).min(total);
                        (0..n)
                            .map(|k| {
                                let idx = (i * n + k) % total;
                                ((idx % cols as usize) as u8, (idx / cols as usize) as u8)
                            })
                            .collect()
                    }
                };
                FrameScript {
                    tiles: coords
                        .into_iter()
                        .map(|(x, y)| {
                            (
                                (x, y),
                                TileSpec::Cdf53 {
                                    bgra: gradient_tile(i as u32, x, y),
                                },
                            )
                        })
                        .collect(),
                }
            })
            .collect()
    }
}

/// A 32x32 BGRA gradient whose content shifts with `shift`, so consecutive
/// ticks carry genuinely different bit-planes.
pub fn gradient_tile(shift: u32, tile_x: u8, tile_y: u8) -> Vec<u8> {
    let off = shift
        .wrapping_add(tile_x as u32 * 17)
        .wrapping_add(tile_y as u32 * 31);
    let mut bgra = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32u32 {
        for x in 0..32u32 {
            let b = (((x * 8) + off) % 256) as u8;
            let g = (((y * 8) + off * 3) % 256) as u8;
            let r = ((((x + y) * 4) + off * 5) % 256) as u8;
            bgra.extend_from_slice(&[b, g, r, 255]);
        }
    }
    bgra
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn full_grid_covers_the_whole_duration_at_the_given_cadence() {
        let p = LoadProfile {
            cadence_us: 33_333,
            churn: Churn::FullGrid,
        };
        let frames = p.frames_for(Duration::from_secs(10), 4, 4);
        assert_eq!(frames.len(), 300);
        assert!(frames.iter().all(|f| f.tiles.len() == 16));
    }

    #[test]
    fn region_churn_dirties_only_its_window_and_moves_it() {
        let p = LoadProfile {
            cadence_us: 33_333,
            churn: Churn::Region { tiles_per_tick: 3 },
        };
        let frames = p.frames_for(Duration::from_secs(1), 4, 4);
        assert!(frames.iter().all(|f| f.tiles.len() == 3));
        let coords = |f: &FrameScript| f.tiles.iter().map(|(c, _)| *c).collect::<Vec<_>>();
        assert_ne!(coords(&frames[0]), coords(&frames[1]));
    }

    #[test]
    fn region_window_wraps_and_stays_in_bounds() {
        let p = LoadProfile {
            cadence_us: 1_000,
            churn: Churn::Region { tiles_per_tick: 3 },
        };
        let frames = p.frames_for(Duration::from_millis(200), 2, 2);
        assert!(frames
            .iter()
            .flat_map(|f| f.tiles.iter())
            .all(|((x, y), _)| *x < 2 && *y < 2));
    }

    #[test]
    fn content_differs_between_ticks() {
        let p = LoadProfile {
            cadence_us: 33_333,
            churn: Churn::FullGrid,
        };
        let frames = p.frames_for(Duration::from_millis(100), 1, 1);
        let bytes = |f: &FrameScript| match &f.tiles[0].1 {
            TileSpec::Cdf53 { bgra } => bgra.clone(),
            other => panic!("expected Cdf53, got {other:?}"),
        };
        assert_ne!(bytes(&frames[0]), bytes(&frames[1]));
    }
}
