//! Bandwidth-estimator wrapper backed by a real GoogCC controller.
//!
//! # Design note — why not `str0m::bwe::Bwe`?
//!
//! str0m does expose a standalone `bwe_::Bwe` struct (a full GoogCC port)
//! internally, but it is declared `pub(crate)` and is NOT accessible from
//! outside the crate (checked against 0.21 and 0.23.1). The only public
//! `str0m::bwe` items are:
//!   - `Bitrate` — the bitrate newtype (re-exported from `str0m-proto`)
//!   - `Bwe<'a>` — a borrowed wrapper around an `Rtc` session
//!   - `BweKind`  — an event enum
//!
//! Constructing a standalone GCC estimator without an `Rtc` session is not
//! possible with str0m's public API. The standalone `goog_cc` crate backs
//! this wrapper instead — see `googcc::GoogCcDriver`.
//!
//! See `docs/superpowers/specs/2026-06-27-protocol-redesign-design.md`.

use std::time::Instant;

mod googcc;
mod timeline;

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
    /// Server-side send time in microseconds since the bridge's BWE epoch,
    /// taken from the retransmit cache's `last_sent_at`, so a retransmitted
    /// pass reports when it actually went out rather than when it first did.
    /// Monotonic and full-precision — unlike the arrival series this never
    /// crosses the wire, so it needs no unwrapping.
    pub server_emit_us: u64,
    /// Low 16 bits of the client's arrival time in ms.
    pub client_arrival_ms_lo16: u16,
    /// Wire size of the acknowledged datagram in bytes, summed over its
    /// fragments. GoogCC derives delivery rate from bytes; without this the
    /// controller sees arrivals but no volume and cannot form an estimate.
    /// This is datagram payload only and excludes UDP/IP framing (~48 bytes
    /// per packet on IPv6), so the estimate runs a few percent low on
    /// full-size datagrams — no correction factor is applied here.
    pub size_bytes: u32,
}

/// Public observability snapshot returned by `BweWrapper::snapshot()`.
#[derive(Debug, Clone, Copy)]
pub struct BweSnapshot {
    /// Current bandwidth estimate, bits per second.
    pub bitrate_bps: u64,
    /// Total ACK-arrival samples fed into the estimator since startup.
    pub samples_seen: u64,
}

// ── BweWrapper ────────────────────────────────────────────────────────────────

/// Bandwidth estimator wrapper. Delegates to a real `GoogCcDriver`
/// (`googcc::GoogCcDriver`), a standalone port of libwebrtc's GoogCC
/// congestion controller. Kept as a thin seam so the two `io_bridge`
/// call sites (`AckArrival` in, `BweSnapshot` out) never need to change
/// regardless of what backs the estimate.
pub struct BweWrapper {
    driver: googcc::GoogCcDriver,
}

impl BweWrapper {
    /// Initial bitrate seed. Sized for a typical broadband first-paint
    /// burst (2 Mbps); the estimator will adapt within a few hundred ms.
    pub const INITIAL_BPS: u64 = 2_000_000;

    /// `now` seeds the controller's clock base. Callers should pass the
    /// same clock source they'll later pass to `update` (`now_std()` in
    /// production) — seeding from a bare `Instant::now()` instead would mix
    /// one wall-clock stamp into an estimator whose every other input is
    /// the caller's (possibly virtual, paused-tokio) clock. Benign in
    /// practice since the wall clock only ever runs ahead of a freshly-
    /// paused virtual clock (the first window just over-reports elapsed
    /// time rather than under-reporting it), but there's no reason to
    /// leave a real wall-clock read inside the one estimator this
    /// paused-clock harness exists to unblock.
    pub fn new(initial_bps: u64, now: Instant) -> Self {
        Self {
            driver: googcc::GoogCcDriver::new(initial_bps, now),
        }
    }

    /// Feed a batch of ACK arrival records and update the estimate.
    ///
    /// `now` is the caller's wall-clock at the time these records were
    /// collected (typically `Instant::now()` at ACK-parse time).
    ///
    /// Returns the updated `BweSnapshot`.
    pub fn update(&mut self, records: &[AckArrival], now: Instant) -> BweSnapshot {
        self.driver.update(records, now)
    }

    /// Return the current observability snapshot without advancing the estimator.
    pub fn snapshot(&self) -> BweSnapshot {
        self.driver.snapshot()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_starts_near_initial_bps() {
        let bwe = BweWrapper::new(BweWrapper::INITIAL_BPS, Instant::now());
        let snap = bwe.snapshot();
        // The initial seed should be reflected as the estimate before any
        // update. Loose tolerance in case the controller clamps the seed.
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
        let mut bwe = BweWrapper::new(BweWrapper::INITIAL_BPS, Instant::now());
        let records = vec![
            AckArrival {
                wire_seq: 1,
                server_emit_us: 100,
                client_arrival_ms_lo16: 110,
                size_bytes: 1200,
            },
            AckArrival {
                wire_seq: 2,
                server_emit_us: 120,
                client_arrival_ms_lo16: 135,
                size_bytes: 1200,
            },
        ];
        let snap = bwe.update(&records, Instant::now());
        assert_eq!(snap.samples_seen, 2);
    }

    /// Guards the import paths. goog_cc's published docs reference
    /// `goog_cc::api::*`, which is a private module; the real re-exports are
    /// at the crate root. This test fails to compile if that changes.
    #[test]
    fn googcc_constructs_standalone() {
        use goog_cc::network_control::NetworkControllerConfig;
        use goog_cc::transport::TargetRateConstraints;
        use goog_cc::units::{DataRate, Timestamp};
        use goog_cc::{GoogCcConfig, GoogCcNetworkController};

        let t0 = Timestamp::from_millis(1_000);
        let cfg = NetworkControllerConfig {
            constraints: TargetRateConstraints {
                at_time: t0,
                min_data_rate: Some(DataRate::from_kilobits_per_sec(100)),
                max_data_rate: Some(DataRate::from_kilobits_per_sec(50_000)),
                starting_rate: Some(DataRate::from_kilobits_per_sec(1_000)),
            },
            ..Default::default()
        };
        let _ctl = GoogCcNetworkController::new(
            cfg,
            GoogCcConfig {
                feedback_only: false,
            },
        );
    }

    /// GoogCC derives rate from bytes, so a sample without a size cannot
    /// contribute to an estimate. This pins the field onto the seam.
    #[test]
    fn ack_arrival_carries_packet_size() {
        let a = AckArrival {
            wire_seq: 1,
            server_emit_us: 10,
            client_arrival_ms_lo16: 25,
            size_bytes: 1200,
        };
        assert_eq!(a.size_bytes, 1200);
    }

    /// The emit time must be server-side monotonic microseconds, not a
    /// 16-bit wrapped wire value. A retransmitted datagram keeps its original
    /// stamp in the cache, so reading bytes would report the first send and
    /// bury the RTO backoff inside the measured one-way delay.
    #[test]
    fn ack_arrival_emit_time_is_monotonic_micros() {
        let a = AckArrival {
            wire_seq: 1,
            server_emit_us: 5_000_000,
            client_arrival_ms_lo16: 25,
            size_bytes: 1200,
        };
        assert_eq!(a.server_emit_us, 5_000_000);
    }

    /// The seam must be backed by GoogCC, not the EWMA. Rising one-way delay
    /// at constant volume is the signal only a delay-gradient controller
    /// reacts to: a delivery-rate estimator sees steady bytes and holds.
    #[test]
    fn wrapper_backs_off_on_rising_delay() {
        let t0 = std::time::Instant::now();
        let mut w = BweWrapper::new(4_000_000, t0);

        let mut last = w.snapshot().bitrate_bps;
        for step in 0..400u32 {
            let batch: Vec<AckArrival> = (0..12u32)
                .map(|i| {
                    let emit_us = ((step * 20 + i) as u64) * 1_000;
                    // One-way delay grows 10 ms per step. Volume is constant.
                    let arrival_ms = emit_us / 1_000 + 15 + (step as u64) * 10;
                    AckArrival {
                        wire_seq: step * 12 + i,
                        server_emit_us: emit_us,
                        client_arrival_ms_lo16: (arrival_ms & 0xFFFF) as u16,
                        size_bytes: 1200,
                    }
                })
                .collect();
            last = w
                .update(
                    &batch,
                    t0 + std::time::Duration::from_millis(20 * step as u64),
                )
                .bitrate_bps;
        }

        assert!(
            last < 4_000_000,
            "estimate {last} never fell despite steadily rising one-way delay"
        );
    }

    /// Control for `wrapper_backs_off_on_rising_delay`. Same shape, flat
    /// delay: the estimate must RISE above the seed. Without this, a driver
    /// that simply decays toward zero would satisfy the back-off test.
    #[test]
    fn wrapper_rises_on_a_clean_link() {
        let t0 = std::time::Instant::now();
        let mut w = BweWrapper::new(4_000_000, t0);

        let mut last = w.snapshot().bitrate_bps;
        for step in 0..400u32 {
            let batch: Vec<AckArrival> = (0..12u32)
                .map(|i| {
                    let emit_us = ((step * 20 + i) as u64) * 1_000;
                    let arrival_ms = emit_us / 1_000 + 15;
                    AckArrival {
                        wire_seq: step * 12 + i,
                        server_emit_us: emit_us,
                        client_arrival_ms_lo16: (arrival_ms & 0xFFFF) as u16,
                        size_bytes: 1200,
                    }
                })
                .collect();
            last = w
                .update(
                    &batch,
                    t0 + std::time::Duration::from_millis(20 * step as u64),
                )
                .bitrate_bps;
        }

        assert!(
            last > 4_000_000,
            "estimate {last} did not rise on a clean link with constant delay"
        );
    }
}
