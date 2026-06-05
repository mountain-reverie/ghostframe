//! Token-bucket bandwidth cap for tests + bench scenarios.
//!
//! Mirrors the `loss_injection` env-var pattern: `GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP`
//! sets bytes-per-second; absent or zero = no cap (defaults to `u64::MAX`).
//! Production code path is unaffected when the env var is unset.

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

    pub fn from_env() -> Option<Self> {
        let cap: u64 = std::env::var("GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if cap == 0 {
            None
        } else {
            Some(Self::new(cap))
        }
    }

    /// True iff `bytes` worth of tokens were consumed. False = drop / defer.
    /// Refills tokens at `bytes_per_sec` * elapsed since last call.
    pub fn try_consume(&mut self, bytes: usize) -> bool {
        let now = Instant::now();
        let elapsed_s = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens_bytes =
            (self.tokens_bytes + elapsed_s * self.bytes_per_sec as f64)
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

    #[test]
    fn from_env_returns_none_when_unset() {
        std::env::remove_var("GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP");
        assert!(BandwidthCap::from_env().is_none());
    }

    #[test]
    fn from_env_parses_value() {
        std::env::set_var("GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP", "1250000");
        let cap = BandwidthCap::from_env().expect("set");
        assert_eq!(cap.bytes_per_sec, 1_250_000);
        std::env::remove_var("GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP");
    }
}
