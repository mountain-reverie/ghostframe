//! Token-bucket bandwidth cap for tests + bench scenarios.
//!
//! `GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP` sets bytes-per-second; absent or zero
//! = no cap. Parsing lives in `crate::config::TransportConfig` (see its
//! `outbound_bandwidth_cap_bps` field) — this type only carries the rate.

use std::time::Instant;

#[derive(Debug)]
pub struct BandwidthCap {
    bytes_per_sec: u64,
    tokens_bytes: f64,
    last_refill: Instant,
}

impl BandwidthCap {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            bytes_per_sec,
            tokens_bytes: bytes_per_sec as f64,
            last_refill: Instant::now(),
        }
    }

    /// True iff `bytes` worth of tokens were consumed. False = drop / defer.
    /// Refills tokens at `bytes_per_sec` * elapsed since last call.
    pub fn try_consume(&mut self, bytes: usize) -> bool {
        let now = Instant::now();
        let elapsed_s = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens_bytes = (self.tokens_bytes + elapsed_s * self.bytes_per_sec as f64)
            .min(self.bytes_per_sec as f64);
        if self.tokens_bytes >= bytes as f64 {
            self.tokens_bytes -= bytes as f64;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn consumes_until_empty_then_refills() {
        // 10_000 bytes/sec. First 10_000 bytes immediate; next 10_000 takes 1s.
        let mut cap = BandwidthCap::new(10_000);
        assert!(cap.try_consume(10_000));
        assert!(!cap.try_consume(10_000)); // empty
        sleep(Duration::from_millis(600));
        assert!(cap.try_consume(5_000)); // ~6_000 tokens refilled
    }

    // Env-var parsing (`GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP`) moved to
    // `crate::config::TransportConfig::from_lookup`; its tests live there.
    // `new` no longer touches the environment, so no shared-environment-lock
    // guard is needed here.
    #[test]
    fn new_stores_the_configured_rate() {
        let cap = BandwidthCap::new(1_250_000);
        assert_eq!(cap.bytes_per_sec, 1_250_000);
        assert_eq!(cap.tokens_bytes, 1_250_000.0, "bucket starts full");
    }
}
