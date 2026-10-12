//! Small helpers shared by the capture backends and `main`.

/// Read CLOCK_MONOTONIC and return the value as nanoseconds.
///
/// Stamps capture completion for the bench telemetry; both capture backends
/// must use the same clock for the numbers to be comparable.
pub fn monotonic_now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime is async-signal-safe and writes only to the
    // caller-owned timespec. CLOCK_MONOTONIC is always available on Linux.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

/// Interpret an environment value as a boolean flag: set, non-empty and not
/// `"0"` means on. `None` (unset) is off.
pub fn env_flag(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_flag_truth_table() {
        assert!(!env_flag(None));
        assert!(!env_flag(Some("")));
        assert!(!env_flag(Some("0")));
        assert!(env_flag(Some("1")));
        assert!(env_flag(Some("true")));
        // Only the exact string "0" is off.
        assert!(env_flag(Some("00")));
    }

    #[test]
    fn monotonic_clock_is_nonzero_and_nondecreasing() {
        let a = monotonic_now_ns();
        let b = monotonic_now_ns();
        assert!(a > 0);
        assert!(b >= a);
    }
}
