//! Thin bandwidth-estimator wrapper for Phase 1 observability.
//!
//! # Design note — why not `str0m::bwe::Bwe`?
//!
//! str0m 0.21 does expose a standalone `bwe_::Bwe` struct (a full GoogCC port)
//! internally, but it is declared `pub(crate)` and is NOT accessible from
//! outside the crate. The only public `str0m::bwe` items are:
//!   - `Bitrate` — the bitrate newtype (re-exported from `str0m-proto`)
//!   - `Bwe<'a>` — a borrowed wrapper around an `Rtc` session
//!   - `BweKind`  — an event enum
//!
//! Constructing a standalone GCC estimator without an `Rtc` session is not
//! possible with the public API of str0m 0.21.
//!
//! For Phase 1 (observability only) we implement a lightweight EWMA delivery-
//! rate estimator ourselves. It has the same external interface (`AckArrival`
//! in, `BweSnapshot` out) that the rest of the protocol-redesign plan depends
//! on. When Phase 2 wires the estimate into the pacer we can swap the internals
//! for a richer estimator (e.g. str0m-proto's forthcoming standalone API, or
//! a vendored GCC) without changing any call-sites.
//!
//! str0m is still listed as a Cargo dependency so that `str0m::bwe::Bitrate`
//! is available for future use and to keep the upgrade path obvious.
//!
//! See `docs/superpowers/specs/2026-06-27-protocol-redesign-design.md`.

use std::time::{Duration, Instant};

// `str0m::bwe::Bitrate` is the public bitrate newtype from str0m-proto.
// We import it so callers can convert between our u64 bps and str0m's type.
#[allow(unused_imports)]
pub use str0m::bwe::Bitrate as Str0mBitrate;

// ── EWMA parameters ─────────────────────────────────────────────────────────

/// The half-life of the EWMA: after this many seconds without new samples the
/// weight of old observations has decayed to ~50 %.
const EWMA_HALF_LIFE_SECS: f64 = 0.5;

/// Minimum number of samples in a window before we emit an estimate. Below
/// this threshold we return the initial seed.
const MIN_SAMPLES_FOR_ESTIMATE: u64 = 3;

/// Window length. We bucket arriving samples into this window and compute a
/// delivery rate from the bytes and time span observed. The EWMA then
/// smooths successive window estimates.
const WINDOW: Duration = Duration::from_millis(200);

// ── Public types ─────────────────────────────────────────────────────────────

/// Tier-tagged ACK arrival record fed into the estimator. Built from the
/// `BweSample`s the io_bridge accumulates at ACK-receive time.
#[derive(Debug, Clone, Copy)]
pub struct AckArrival {
    /// Per-session monotonic seq we feed the estimator as packet identity.
    /// In Phase 1 this is `(emit_lo16 << 16) | arrival_lo16` — a stable
    /// per-sample identifier. Phase 2 will replace it with the actual
    /// wire_seq from the cache entry once plumbed through `BweSample`.
    pub wire_seq: u32,
    /// Low 16 bits of the server's emit time in ms.
    pub server_emit_ms_lo16: u16,
    /// Low 16 bits of the client's arrival time in ms.
    pub client_arrival_ms_lo16: u16,
}

/// Public observability snapshot returned by `BweWrapper::snapshot()`.
#[derive(Debug, Clone, Copy)]
pub struct BweSnapshot {
    /// Current bandwidth estimate, bits per second.
    pub bitrate_bps: u64,
    /// Total ACK-arrival samples fed into the estimator since startup.
    pub samples_seen: u64,
}

// ── Internal window accumulator ───────────────────────────────────────────────

/// State for the current accumulation window.
struct Window {
    /// Wall-clock start of this window.
    start: Instant,
    /// Number of ACK samples received in this window.
    count: u32,
}

// ── BweWrapper ────────────────────────────────────────────────────────────────

/// Bandwidth estimator wrapper.
///
/// Implements a simple EWMA over per-window delivery-rate observations
/// derived from the one-way-delay (OWD) gradient in ACK records.
///
/// The `AckArrival` records carry 16-bit wrapped millisecond timestamps from
/// both sides. We use the *count* of ACKs in each 200 ms window together with
/// the initial seed to produce a conservative delivery-rate estimate. For
/// Phase 1 (logging/observability) this is sufficient; Phase 2 will replace
/// this with a proper GCC or SCReAM implementation.
pub struct BweWrapper {
    /// Current smoothed estimate, bits per second.
    estimate_bps: f64,
    /// EWMA decay coefficient per `WINDOW` period:
    ///   α = exp(−ln(2) / (HALF_LIFE / WINDOW))
    alpha: f64,
    /// Current accumulation window.
    window: Window,
    /// Total samples fed in since construction.
    samples_seen: u64,
}

impl BweWrapper {
    /// Initial bitrate seed. Sized for a typical broadband first-paint
    /// burst (2 Mbps); the estimator will adapt within a few hundred ms.
    pub const INITIAL_BPS: u64 = 2_000_000;

    pub fn new(initial_bps: u64) -> Self {
        // Compute the EWMA decay factor for one WINDOW period.
        let window_secs = WINDOW.as_secs_f64();
        let alpha = (-std::f64::consts::LN_2 / (EWMA_HALF_LIFE_SECS / window_secs)).exp();

        Self {
            estimate_bps: initial_bps as f64,
            alpha,
            window: Window {
                start: Instant::now(),
                count: 0,
            },
            samples_seen: 0,
        }
    }

    /// Feed a batch of ACK arrival records and update the estimate.
    ///
    /// `now` is the caller's wall-clock at the time these records were
    /// collected (typically `Instant::now()` at ACK-parse time).
    ///
    /// Returns the updated `BweSnapshot`.
    pub fn update(&mut self, records: &[AckArrival], now: Instant) -> BweSnapshot {
        if records.is_empty() {
            return self.snapshot();
        }

        self.samples_seen = self.samples_seen.saturating_add(records.len() as u64);
        self.window.count = self.window.count.saturating_add(records.len() as u32);

        // Flush the window if it has expired.
        let elapsed = now.saturating_duration_since(self.window.start);
        if elapsed >= WINDOW {
            self.flush_window(elapsed);
            self.window = Window { start: now, count: 0 };
        }

        self.snapshot()
    }

    /// Return the current observability snapshot without advancing the estimator.
    pub fn snapshot(&self) -> BweSnapshot {
        BweSnapshot {
            bitrate_bps: self.estimate_bps.max(0.0).round() as u64,
            samples_seen: self.samples_seen,
        }
    }

    // ── Private ───────────────────────────────────────────────────────────────

    /// Flush the accumulated window into the EWMA.
    ///
    /// We derive a "delivery rate" for the window as:
    ///   rate_bps = (count * AVG_ACK_PAYLOAD_BITS) / elapsed_secs
    ///
    /// where `AVG_ACK_PAYLOAD_BITS` is a conservative per-tile payload
    /// estimate. This is intentionally rough for Phase 1 — the estimate
    /// serves logging only, not pacing.
    fn flush_window(&mut self, elapsed: Duration) {
        if self.window.count == 0 || self.samples_seen < MIN_SAMPLES_FOR_ESTIMATE {
            return;
        }

        // Each ACK record corresponds to one tile pass datagram. A tile pass
        // datagram for our typical 64 × 64 tile at PalRLE is ~1–4 kB; CDF53
        // passes range 256 B – 8 kB. Use 4 kB = 32 768 bits as a mid estimate.
        const AVG_ACK_PAYLOAD_BITS: f64 = 4096.0 * 8.0;

        let elapsed_secs = elapsed.as_secs_f64().max(1e-6);
        let window_rate_bps = (self.window.count as f64 * AVG_ACK_PAYLOAD_BITS) / elapsed_secs;

        // EWMA update: new = α * old + (1 − α) * observation
        self.estimate_bps = self.alpha * self.estimate_bps + (1.0 - self.alpha) * window_rate_bps;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_starts_near_initial_bps() {
        let bwe = BweWrapper::new(BweWrapper::INITIAL_BPS);
        let snap = bwe.snapshot();
        // The initial seed should be reflected as the estimate. Use a
        // loose tolerance because the EWMA may adjust slightly.
        assert!(
            snap.bitrate_bps >= BweWrapper::INITIAL_BPS / 2
                && snap.bitrate_bps <= BweWrapper::INITIAL_BPS * 2,
            "initial estimate {} not within 2× of seed {}",
            snap.bitrate_bps,
            BweWrapper::INITIAL_BPS,
        );
        assert_eq!(snap.samples_seen, 0);
    }

    #[test]
    fn update_counts_records_seen() {
        let mut bwe = BweWrapper::new(BweWrapper::INITIAL_BPS);
        let records = vec![
            AckArrival { wire_seq: 1, server_emit_ms_lo16: 100, client_arrival_ms_lo16: 110 },
            AckArrival { wire_seq: 2, server_emit_ms_lo16: 120, client_arrival_ms_lo16: 135 },
        ];
        let snap = bwe.update(&records, Instant::now());
        assert_eq!(snap.samples_seen, 2);
    }

    #[test]
    fn estimate_adapts_upward_with_high_arrival_rate() {
        let mut bwe = BweWrapper::new(BweWrapper::INITIAL_BPS);

        // Simulate 100 records arriving in the first window.
        let records: Vec<AckArrival> = (0u32..100)
            .map(|i| AckArrival {
                wire_seq: i,
                server_emit_ms_lo16: (i * 5) as u16,
                client_arrival_ms_lo16: (i * 5 + 10) as u16,
            })
            .collect();

        // First call — samples accumulate in window but elapsed < WINDOW,
        // so estimate stays at seed.
        let now = Instant::now();
        let snap1 = bwe.update(&records, now);
        assert_eq!(snap1.samples_seen, 100);

        // Second call after WINDOW has elapsed — window flushes and EWMA
        // updates. With 100 packets × 32 768 bits / 0.2 s ≈ 16.4 Mbps
        // observation the estimate should rise above the 2 Mbps seed.
        let snap2 = bwe.update(&records, now + WINDOW + Duration::from_millis(1));
        assert!(
            snap2.bitrate_bps > BweWrapper::INITIAL_BPS,
            "estimate {} should be above seed {} after high arrival rate",
            snap2.bitrate_bps,
            BweWrapper::INITIAL_BPS,
        );
    }

    #[test]
    fn str0m_bitrate_type_accessible() {
        // Smoke-test that we can construct a str0m Bitrate from our bps value.
        let br = Str0mBitrate::bps(BweWrapper::INITIAL_BPS);
        assert_eq!(br.as_u64(), BweWrapper::INITIAL_BPS);
    }
}
