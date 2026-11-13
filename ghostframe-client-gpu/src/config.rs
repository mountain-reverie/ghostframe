// SPDX-License-Identifier: MIT OR Apache-2.0
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

/// Whether the native client uses upload slabs. On by default; the knob exists
/// to A/B the slab path against the `Vec` path in one binary, and as an escape
/// hatch should a driver mishandle the linear texture.
///
/// `GHOSTFRAME_NO_UPLOAD_SLABS=1` (any value but `0` or empty) turns them off.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadConfig {
    pub slabs_disabled: bool,
}

impl UploadConfig {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            slabs_disabled: get("GHOSTFRAME_NO_UPLOAD_SLABS")
                .is_some_and(|v| !matches!(v.as_str(), "" | "0")),
        }
    }

    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }
}

/// How a publish makes its buffer safe to read. See [`crate::ring::ExportRing`].
///
/// - `GHOSTFRAME_BLOCKING_PUBLISH=1`: wait for the GPU after every publish, the
///   way the ring did before it stopped. For ruling the non-blocking publish in
///   or out when something looks torn.
/// - `GHOSTFRAME_NO_FENCE_DEVICE=1` (Vulkan): open the device the plain wgpu
///   way instead of adding `VK_KHR_external_semaphore_fd`. Without the
///   extension the ring has no fence and falls back to the blocking wait.
///
/// - `GHOSTFRAME_NO_COMPUTE_EXPORT=1`: copy damage into the export buffer with
///   the render-pass blit instead of the compute kernel that writes only the
///   damaged tiles. The blit is also the fallback where the export cannot be
///   bound as a storage image; this forces it where it could be. For A/B and
///   for ruling the kernel in or out when a frame looks wrong.
///
/// Any of them takes any value but `0` or empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublishConfig {
    pub blocking: bool,
    pub plain_device_open: bool,
    pub render_pass_export: bool,
}

impl PublishConfig {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let on = |name: &str| get(name).is_some_and(|v| !matches!(v.as_str(), "" | "0"));
        Self {
            blocking: on("GHOSTFRAME_BLOCKING_PUBLISH"),
            plain_device_open: on("GHOSTFRAME_NO_FENCE_DEVICE"),
            render_pass_export: on("GHOSTFRAME_NO_COMPUTE_EXPORT"),
        }
    }

    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }
}

/// Where the native client reconstructs CDF 5/3 tiles.
///
/// `GHOSTFRAME_CDF53_DECODE`:
/// - unset, empty, `gpu` or `auto`: on the GPU
///   ([`crate::pipelines::cdf53_grid`]), if a start-up self-test of the shaders
///   passes on this device and the frame's plane store fits; otherwise on the
///   decode thread's CPU, as before.
/// - `cpu`: always the CPU. For A/B in one binary, and the escape hatch when a
///   driver miscompiles the shader in a way the self-test does not see.
///
/// Anything else reads as the default; [`Cdf53Decode::from_env`] says so in the
/// log, once per call (the native client calls it once).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Cdf53Decode {
    #[default]
    Auto,
    Cpu,
}

impl Cdf53Decode {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        match get("GHOSTFRAME_CDF53_DECODE").as_deref() {
            Some(v) if v.eq_ignore_ascii_case("cpu") => Cdf53Decode::Cpu,
            _ => Cdf53Decode::Auto,
        }
    }

    /// Whether `get` holds a value this knows nothing about.
    pub fn is_unrecognised(get: impl Fn(&str) -> Option<String>) -> Option<String> {
        get("GHOSTFRAME_CDF53_DECODE").filter(|v| {
            !["", "auto", "gpu", "cpu"]
                .iter()
                .any(|ok| v.eq_ignore_ascii_case(ok))
        })
    }

    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok();
        if let Some(v) = Self::is_unrecognised(get) {
            tracing::warn!(
                value = %v,
                "GHOSTFRAME_CDF53_DECODE: unrecognised value, expected `cpu`, `gpu` or `auto`; using auto"
            );
        }
        Self::from_lookup(get)
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
    fn slabs_are_on_unless_switched_off() {
        let off = |v: &'static str| UploadConfig::from_lookup(lookup_one(v)).slabs_disabled;
        assert!(!UploadConfig::from_lookup(lookup(&[])).slabs_disabled);
        assert!(!off("0") && !off(""));
        assert!(off("1") && off("yes"));
    }

    fn lookup_one(v: &'static str) -> impl Fn(&str) -> Option<String> {
        move |k| (k == "GHOSTFRAME_NO_UPLOAD_SLABS").then(|| v.to_string())
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

    #[test]
    fn cdf53_decode_is_auto_unless_cpu_is_asked_for() {
        let mode = |pairs| Cdf53Decode::from_lookup(lookup(pairs));
        assert_eq!(mode(&[]), Cdf53Decode::Auto);
        assert_eq!(
            mode(&[("GHOSTFRAME_CDF53_DECODE", "gpu")]),
            Cdf53Decode::Auto
        );
        assert_eq!(
            mode(&[("GHOSTFRAME_CDF53_DECODE", "cpu")]),
            Cdf53Decode::Cpu
        );
        assert_eq!(
            mode(&[("GHOSTFRAME_CDF53_DECODE", "CPU")]),
            Cdf53Decode::Cpu
        );
        assert_eq!(mode(&[("GHOSTFRAME_CDF53_DECODE", "")]), Cdf53Decode::Auto);
        let odd = |v: &str| {
            let v = v.to_string();
            Cdf53Decode::is_unrecognised(move |k| {
                (k == "GHOSTFRAME_CDF53_DECODE").then(|| v.clone())
            })
        };
        assert_eq!(odd("cpuu").as_deref(), Some("cpuu"));
        assert_eq!(odd("1").as_deref(), Some("1"));
        assert_eq!(odd("GPU"), None);
        assert_eq!(odd("Cpu"), None);
        assert_eq!(odd(""), None);
    }

    #[test]
    fn publish_waits_only_when_asked() {
        let cfg = |pairs| PublishConfig::from_lookup(lookup(pairs));
        assert_eq!(cfg(&[]), PublishConfig::default());
        assert!(cfg(&[("GHOSTFRAME_BLOCKING_PUBLISH", "1")]).blocking);
        assert!(!cfg(&[("GHOSTFRAME_BLOCKING_PUBLISH", "0")]).blocking);
        assert!(cfg(&[("GHOSTFRAME_NO_FENCE_DEVICE", "1")]).plain_device_open);
        assert!(cfg(&[("GHOSTFRAME_NO_COMPUTE_EXPORT", "1")]).render_pass_export);
        assert!(!cfg(&[("GHOSTFRAME_NO_COMPUTE_EXPORT", "")]).render_pass_export);
    }
}
