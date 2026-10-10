//! The only module in this crate that reads environment variables.
//!
//! Mirrors the convention CI enforces on `ghostframe-lib`, where an
//! `env::var` call outside `config.rs` fails the build; the same guard now
//! covers every `ghostframe-client-*` crate.
//!
//! Holds the `GHOSTFRAME_CLIENT_DUMP_*` sampling knobs, which the framebuffer
//! dump (`Renderer`) and the export-buffer dump (`ghostframe-client-native`'s
//! render thread) both honour. They used to parse the three variables
//! independently, so the two dumps could drift apart on defaults.

/// Which frames of a session to dump to disk, and where.
///
/// `GHOSTFRAME_CLIENT_DUMP_FRAME` and `GHOSTFRAME_CLIENT_DUMP_EXPORT` name the
/// path prefix of their respective dumps (unset disables that dump); the two
/// variables below choose *which* publishes/flushes are written, and apply to
/// both:
///
/// - `GHOSTFRAME_CLIENT_DUMP_EVERY=n` (`n > 0`): every nth, overwriting
///   `<prefix>.latest.ppm` -- for comparing a settled session rather than the
///   opening paint.
/// - otherwise the first `GHOSTFRAME_CLIENT_DUMP_COUNT` (default 1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DumpConfig {
    /// `GHOSTFRAME_CLIENT_DUMP_FRAME`.
    pub frame_prefix: Option<String>,
    /// `GHOSTFRAME_CLIENT_DUMP_EXPORT`.
    pub export_prefix: Option<String>,
    /// `GHOSTFRAME_CLIENT_DUMP_EVERY`; `0` means "use `count`". Unparseable
    /// values read as `0`.
    pub every: u64,
    /// `GHOSTFRAME_CLIENT_DUMP_COUNT`; default 1. Unparseable values read as 1.
    pub count: u64,
}

impl DumpConfig {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let num = |name: &str, default: u64| {
            get(name)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(default)
        };
        Self {
            frame_prefix: get("GHOSTFRAME_CLIENT_DUMP_FRAME"),
            export_prefix: get("GHOSTFRAME_CLIENT_DUMP_EXPORT"),
            every: num("GHOSTFRAME_CLIENT_DUMP_EVERY", 0),
            count: num("GHOSTFRAME_CLIENT_DUMP_COUNT", 1),
        }
    }

    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Whether the `n`th (zero-based) opportunity should be written.
    pub fn should_dump(&self, n: u64) -> bool {
        if self.every > 0 {
            n.is_multiple_of(self.every)
        } else {
            n < self.count
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn defaults_dump_only_the_first_frame() {
        let c = DumpConfig::from_lookup(lookup(&[]));
        assert_eq!(c.frame_prefix, None);
        assert_eq!(c.export_prefix, None);
        assert_eq!((c.every, c.count), (0, 1));
        assert!(c.should_dump(0));
        assert!(!c.should_dump(1));
    }

    #[test]
    fn count_selects_a_prefix_series() {
        let c = DumpConfig::from_lookup(lookup(&[("GHOSTFRAME_CLIENT_DUMP_COUNT", "3")]));
        let hits: Vec<u64> = (0..6).filter(|n| c.should_dump(*n)).collect();
        assert_eq!(hits, vec![0, 1, 2]);
    }

    #[test]
    fn every_overrides_count_and_samples_forever() {
        let c = DumpConfig::from_lookup(lookup(&[
            ("GHOSTFRAME_CLIENT_DUMP_EVERY", "4"),
            ("GHOSTFRAME_CLIENT_DUMP_COUNT", "1"),
        ]));
        let hits: Vec<u64> = (0..13).filter(|n| c.should_dump(*n)).collect();
        assert_eq!(hits, vec![0, 4, 8, 12]);
    }

    #[test]
    fn prefixes_and_garbage_numbers() {
        let c = DumpConfig::from_lookup(lookup(&[
            ("GHOSTFRAME_CLIENT_DUMP_FRAME", "/tmp/f"),
            ("GHOSTFRAME_CLIENT_DUMP_EXPORT", "/tmp/e"),
            ("GHOSTFRAME_CLIENT_DUMP_EVERY", "x"),
            ("GHOSTFRAME_CLIENT_DUMP_COUNT", "-2"),
        ]));
        assert_eq!(c.frame_prefix.as_deref(), Some("/tmp/f"));
        assert_eq!(c.export_prefix.as_deref(), Some("/tmp/e"));
        assert_eq!((c.every, c.count), (0, 1));
    }
}
