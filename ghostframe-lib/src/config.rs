//! Library configuration, parsed once at the executable boundary.
//!
//! `ghostframe-lib` must never read `std::env` outside this file. Environment
//! variables are process-global while `cargo test` runs tests as threads in one
//! process, so call-time reads made tests race each other (~2 failures per 6
//! full-suite runs before this refactor). Threading configuration through
//! constructors makes that class of bug structurally impossible.
//!
//! Variable names and parsing are frozen: the e2e suite drives the
//! containerised daemon through these exact names.
//!
//! ## Adding a variable
//!
//! 1. Add a field to the relevant `*Config` struct (or a new one), with a
//!    doc comment naming its environment variable and accepted range.
//! 2. Parse it in that config's `from_lookup` (or `from_env`, for configs
//!    that have not yet been converted).
//! 3. Thread it through the constructor of whatever it configures.
//! 4. Never read `std::env` at the consumption site — that reintroduces the
//!    process-global race this module exists to remove.

use crate::tile::FrameMode;

/// Test-only tuning knobs for the frame-mode classifier. All `None`/default
/// means production behaviour: a production-relevant tunable would need
/// wiring in `Classifier::new` and other codepaths that does not yet exist.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClassifierConfig {
    /// Pins the frame mode. Fed by `GHOSTFRAME_TEST_FORCE_FRAME_MODE`, or by
    /// `GHOSTFRAME_FORCE_TILECODEC=1|true` as a high-level alias for
    /// `TileCodec`.
    pub force_frame_mode: Option<FrameMode>,
    /// Overrides `REFINEMENT_BIAS_PER_TILE_US`. Fed by
    /// `GHOSTFRAME_TEST_REFINEMENT_BIAS_US`; accepts any value `> 0.0`.
    pub refinement_bias_us: Option<f32>,
    /// Overrides `HEADROOM_MIN_BYTES_PER_US`. Fed by
    /// `GHOSTFRAME_TEST_HEADROOM_MIN_BPUS`; accepts any value `> 0.0`.
    pub headroom_min_bpus: Option<f32>,
    /// Overrides `LOSS_OVERRIDE_THRESHOLD`. Fed by
    /// `GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD`; accepts `(0.0, 1.0]`.
    pub loss_override_threshold: Option<f32>,
}

impl ClassifierConfig {
    /// Parse from an arbitrary lookup. `from_env` is this with a real
    /// environment lookup; tests use a map so they never touch process-global
    /// state. Keeping the parsing here means the crate reads the process
    /// environment in exactly one place.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            refinement_bias_us: get("GHOSTFRAME_TEST_REFINEMENT_BIAS_US")
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|v| *v > 0.0),
            loss_override_threshold: get("GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD")
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|v| *v > 0.0 && *v <= 1.0),
            headroom_min_bpus: get("GHOSTFRAME_TEST_HEADROOM_MIN_BPUS")
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|v| *v > 0.0),
            // `GHOSTFRAME_FORCE_TILECODEC=1`/`true` is a high-level alias
            // for `GHOSTFRAME_TEST_FORCE_FRAME_MODE=tile`. Used by the
            // `e2e_lossless_golden_png` test to bypass the H.264 forced-
            // start at session entry so the cdf53 first-paint burst gets
            // exercised (H.264 has its own FEC + parity + NACK and renders
            // fully even under wire loss, masking tile-codec regressions).
            force_frame_mode: get("GHOSTFRAME_FORCE_TILECODEC")
                .filter(|s| s == "1" || s == "true")
                .map(|_| FrameMode::TileCodec)
                .or_else(|| {
                    get("GHOSTFRAME_TEST_FORCE_FRAME_MODE").and_then(|s| match s.as_str() {
                        "h264" | "H264" => Some(FrameMode::H264),
                        "tile" | "TileCodec" | "tilecodec" => Some(FrameMode::TileCodec),
                        _ => None,
                    })
                }),
        }
    }

    /// Parse environment variables into a classifier configuration.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Production builds without `test-loss-injection` ignore the environment
    /// entirely, exactly as today: the reads are not compiled.
    #[cfg(not(any(test, feature = "test-loss-injection")))]
    pub fn from_env() -> Self {
        Self::default()
    }
}

/// Transport-layer knobs: fault injection, pacing overrides, FEC.
///
/// Deliberately not `Clone`: `outbound_loss`/`inbound_loss` carry a
/// `LossInjector`, whose PRNG state a clone would duplicate rather than
/// fork, producing bit-identical "independent" loss sequences. See
/// `LossInjector`'s doc comment.
#[derive(Debug, Default)]
pub struct TransportConfig {
    // `loss_injection` is itself a `#[cfg(any(test, feature =
    // "test-loss-injection"))]` module (`transport/mod.rs:17`), so these two
    // fields carry the same gate — the type does not exist otherwise. This
    // mirrors `IoBridge`'s own fields at `io_bridge.rs:388,397`. Unit tests are
    // unaffected: `any(test, ..)` means the fields exist under `cargo test`.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub outbound_loss: Option<crate::transport::loss_injection::LossInjector>,
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub inbound_loss: Option<crate::transport::loss_injection::LossInjector>,
    /// Rate in bytes/sec. Deliberately a plain `u64` rather than a
    /// `BandwidthCap` (also a gated module): the config carries data, and
    /// `IoBridge` constructs the gated type from it. That keeps this field
    /// ungated.
    pub outbound_bandwidth_cap_bps: Option<u64>,
    /// `(frame_seq, tile_index)` at which to inject an out-of-range PalRLE
    /// index, from `GHOSTFRAME_INJECT_OOB_PALRLE`.
    pub oob_inject_at: Option<(u32, u32)>,
    /// From `GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1|true`. See
    /// `IoBridge::maybe_fire_session_reset`'s doc comment.
    pub skip_palette_session_reset: bool,
    /// Overrides `sample_all_path_stats`'s `bytes_per_us`. From
    /// `GHOSTFRAME_TEST_FORCE_BYTES_PER_US`; accepts any value `> 0.0`.
    pub test_force_bytes_per_us: Option<f32>,
    /// FEC parity group size. From `GHOSTFRAME_FEC_K`; `None` ⇒ `IoBridge`
    /// defaults to `0` (disabled) at the consumption site, exactly as
    /// today. Forces FEC on for e2e testing — production toggles it
    /// automatically from receiver feedback loss rate instead.
    pub fec_k: Option<usize>,
}

impl TransportConfig {
    /// Parse from an arbitrary lookup. `from_env` is this with a real
    /// environment lookup; tests use a map so they never touch process-global
    /// state. Keeping the parsing here means the crate reads the process
    /// environment in exactly one place.
    ///
    /// Unlike `ClassifierConfig::from_lookup`, this function itself is
    /// **not** gated behind `cfg(any(test, feature = "test-loss-injection"))`:
    /// `fec_k` (`GHOSTFRAME_FEC_K`) has never been gated at its former
    /// `io_bridge` read site, so a genuine production build honours it
    /// today. Gating the whole function the way `ClassifierConfig` does
    /// would silently stop that. Instead, the `#[cfg]` sits on the
    /// individual field initialisers below — the two `LossInjector` fields
    /// (whose type only exists under the feature) and the four others that
    /// were gated at their old `io_bridge` sites, mirroring the shape of
    /// `IoBridge`'s own struct literal. `fec_k` is the only field that
    /// parses unconditionally.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            #[cfg(any(test, feature = "test-loss-injection"))]
            outbound_loss: loss_injector_from_lookup("OUTBOUND", &get),
            #[cfg(any(test, feature = "test-loss-injection"))]
            inbound_loss: loss_injector_from_lookup("INBOUND", &get),
            #[cfg(any(test, feature = "test-loss-injection"))]
            outbound_bandwidth_cap_bps: get("GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP")
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|v| *v != 0),
            #[cfg(not(any(test, feature = "test-loss-injection")))]
            outbound_bandwidth_cap_bps: None,
            #[cfg(any(test, feature = "test-loss-injection"))]
            oob_inject_at: oob_inject_at_from_lookup(&get),
            #[cfg(not(any(test, feature = "test-loss-injection")))]
            oob_inject_at: None,
            #[cfg(any(test, feature = "test-loss-injection"))]
            skip_palette_session_reset: matches!(
                get("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET").as_deref(),
                Some("1") | Some("true")
            ),
            #[cfg(not(any(test, feature = "test-loss-injection")))]
            skip_palette_session_reset: false,
            #[cfg(any(test, feature = "test-loss-injection"))]
            test_force_bytes_per_us: get("GHOSTFRAME_TEST_FORCE_BYTES_PER_US")
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|v| *v > 0.0),
            #[cfg(not(any(test, feature = "test-loss-injection")))]
            test_force_bytes_per_us: None,
            fec_k: get("GHOSTFRAME_FEC_K").and_then(|v| v.parse::<usize>().ok()),
        }
    }

    /// Parse environment variables into a transport configuration. Reads the
    /// real environment in every build — see `from_lookup`'s doc comment for
    /// why this (unlike `ClassifierConfig::from_env`) is not gated.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }
}

/// Build a `LossInjector` for one direction (`"OUTBOUND"` or `"INBOUND"`)
/// from a lookup. Returns `None` if the relevant probability is `0` or the
/// env var isn't set. Recognized env vars (`<DIR>` is `OUTBOUND`/`INBOUND`):
/// - `GHOSTFRAME_<DIR>_LOSS_PROBABILITY` — f32 in `[0.0, 1.0]`, default `0.0`
/// - `GHOSTFRAME_<DIR>_LOSS_PREDICATE` — one of `all` / `tile` / `ack` /
///   `palrle_bundled` / `palrle_thin`, default `all`.
/// - `GHOSTFRAME_<DIR>_LOSS_SEED` — u64, default `0`.
#[cfg(any(test, feature = "test-loss-injection"))]
fn loss_injector_from_lookup(
    direction: &str,
    get: &impl Fn(&str) -> Option<String>,
) -> Option<crate::transport::loss_injection::LossInjector> {
    let prob_var = format!("GHOSTFRAME_{direction}_LOSS_PROBABILITY");
    let pred_var = format!("GHOSTFRAME_{direction}_LOSS_PREDICATE");
    let seed_var = format!("GHOSTFRAME_{direction}_LOSS_SEED");

    let prob: f32 = get(&prob_var).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    if prob <= 0.0 {
        return None;
    }

    // Predicate: function pointer that classifies an outbound/inbound
    // datagram by its first byte. Selected by the *_LOSS_PREDICATE env var.
    fn predicate_all(_: &[u8]) -> bool {
        true
    }
    // Tile datagrams set bit 31 of frame_seq (TILE_DATAGRAM_FLAG = 0x80000000),
    // which is the high bit of byte [0] in big-endian wire order.
    fn predicate_tile(dg: &[u8]) -> bool {
        !dg.is_empty() && (dg[0] & 0x80) != 0
    }
    // ACK_BATCH_MSG_TYPE = 0x02 (see transport/ack.rs).
    fn predicate_ack(dg: &[u8]) -> bool {
        dg.first().copied() == Some(crate::transport::ack::ACK_BATCH_MSG_TYPE)
    }
    // PalRle tile datagrams: tile datagram flag set, codec field = PalRle (2),
    // payload byte 0 has bundle flag set (0x01).
    // Wire layout: [DatagramHeader DATAGRAM_HEADER_SIZE][TileHeader TILE_HEADER_SIZE][payload].
    // TileHeader byte [2] (wire index DATAGRAM_HEADER_SIZE + 2) = (codec << 1) | lz4.
    // First payload byte is at DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE.
    const CODEC_BYTE: usize = crate::transport::protocol::DATAGRAM_HEADER_SIZE + 2;
    const PAYLOAD_START: usize = crate::transport::protocol::DATAGRAM_HEADER_SIZE
        + crate::transport::protocol::TILE_HEADER_SIZE;
    const MIN_BUNDLE_LEN: usize = PAYLOAD_START + 1;
    fn predicate_palrle_bundled(dg: &[u8]) -> bool {
        dg.len() >= MIN_BUNDLE_LEN
            && (dg[0] & 0x80) != 0
            && (dg[CODEC_BYTE] >> 1) == (crate::transport::protocol::Codec::PalRle as u8)
            && (dg[PAYLOAD_START] & 0x01) != 0
    }
    // Inverse: PalRle tile datagrams without the bundle flag.
    fn predicate_palrle_thin(dg: &[u8]) -> bool {
        dg.len() >= MIN_BUNDLE_LEN
            && (dg[0] & 0x80) != 0
            && (dg[CODEC_BYTE] >> 1) == (crate::transport::protocol::Codec::PalRle as u8)
            && (dg[PAYLOAD_START] & 0x01) == 0
    }

    let predicate: crate::transport::loss_injection::DropPredicate = match get(&pred_var).as_deref()
    {
        Some("tile") => predicate_tile,
        Some("ack") => predicate_ack,
        Some("palrle_bundled") => predicate_palrle_bundled,
        Some("palrle_thin") => predicate_palrle_thin,
        _ => predicate_all,
    };
    let seed: u64 = get(&seed_var).and_then(|s| s.parse().ok()).unwrap_or(0);

    tracing::info!(
        direction,
        prob,
        "test-loss-injection: installed LossInjector"
    );
    Some(crate::transport::loss_injection::LossInjector::new(
        prob, predicate, seed,
    ))
}

/// Parse `GHOSTFRAME_INJECT_OOB_PALRLE` as `"x,y"` (two u32 separated by a
/// comma). Returns `None` when the env var is unset or unparseable. The
/// resulting coordinate is stored on `IoBridge` and consumed (set to `None`)
/// the first time the matching tile is encoded.
#[cfg(any(test, feature = "test-loss-injection"))]
fn oob_inject_at_from_lookup(get: &impl Fn(&str) -> Option<String>) -> Option<(u32, u32)> {
    let raw = get("GHOSTFRAME_INJECT_OOB_PALRLE")?;
    let mut parts = raw.split(',');
    let x = parts.next()?.parse::<u32>().ok()?;
    let y = parts.next()?.parse::<u32>().ok()?;
    Some((x, y))
}

/// Parse `GHOSTFRAME_CDF53_DIFF_TILE` as `"x,y"` (two u32 separated by a
/// comma, whitespace around each number trimmed). Returns `None` when the
/// env var is unset or unparseable. Mirrors the former inline parsing at
/// `io_bridge.rs:3215-3218`.
#[cfg(feature = "cdf53-diag")]
fn cdf53_diff_tile_from_lookup(get: &impl Fn(&str) -> Option<String>) -> Option<(u32, u32)> {
    let spec = get("GHOSTFRAME_CDF53_DIFF_TILE")?;
    let (sx, sy) = spec.split_once(',')?;
    let x = sx.trim().parse::<u32>().ok()?;
    let y = sy.trim().parse::<u32>().ok()?;
    Some((x, y))
}

/// Diagnostic logging and one-shot dumps.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsConfig {
    /// From `GHOSTFRAME_DIAGNOSE_TILES=1|true`.
    pub diagnose_tiles: bool,
    /// From `GHOSTFRAME_DIAGNOSE_GPU_PIPELINE=1|true`.
    pub diagnose_gpu_pipeline: bool,
    /// From `GHOSTFRAME_DIAGNOSE_COLOR_HIST=1|true`.
    pub diagnose_color_hist: bool,
    /// Path for the one-shot raw-BGRA frame dump, from `GHOSTFRAME_DUMP_FRAME`.
    /// Consumed once via `IoBridge::take_dump_frame_path`, which `Option::take`s
    /// it — this replaces the former read-then-`remove_var` one-shot pattern.
    pub dump_frame_path: Option<String>,
    /// Tile coordinates for the M3.3b GPU-vs-CPU CDF53 diff diagnostic, from
    /// `GHOSTFRAME_CDF53_DIFF_TILE="x,y"`. Only consumed under the
    /// `cdf53-diag` feature.
    pub cdf53_diff_tile: Option<(u32, u32)>,
    /// From `GHOSTFRAME_CDF53_DUMP_PENDING`; presence, not value, is the
    /// signal (`.is_ok()` semantics, as with the two fields below). Only
    /// consumed under the `cdf53-diag` feature.
    pub cdf53_dump_pending: bool,
    /// From `GHOSTFRAME_CDF53_SKIP_L2_L3`. Presence, not value, is the
    /// signal: the former `io_bridge.rs` read site used `.is_ok()`, so any
    /// set value (including empty) means `true`. Only consumed under the
    /// `cdf53-diag` feature.
    pub cdf53_skip_l2_l3: bool,
    /// True if `GHOSTFRAME_CDF53_SKIP_L3` **or** `GHOSTFRAME_CDF53_SKIP_L2_L3`
    /// is set: the former call site treated skip-L2-and-L3 as implying
    /// skip-L3 too (`io_bridge.rs:3232-3233`). The OR is resolved here in
    /// `from_lookup` so the call site just reads the field, keeping
    /// `from_lookup` the only place that reasons about the raw env vars.
    pub cdf53_skip_l3: bool,
}

impl DiagnosticsConfig {
    /// Parse from an arbitrary lookup. Unlike `ClassifierConfig` /
    /// `TransportConfig`, this is **not** gated behind `cfg(any(test,
    /// feature = "test-loss-injection"))`: the three `io_bridge.rs` helpers
    /// this replaces (`diagnose_tiles_from_env`, `diagnose_gpu_pipeline_from_env`,
    /// `diagnose_color_histogram_from_env`) were themselves compiled and read
    /// unconditionally in every build, because these are live production
    /// diagnostics toggles (an operator can flip them on a running
    /// deployment), not test-only knobs. Gating this function the same way
    /// as the other two configs would silently stop honouring these
    /// variables in production — so it doesn't.
    ///
    /// `dump_frame_path` parses unconditionally: the former `io_bridge.rs`
    /// site (`GHOSTFRAME_DUMP_FRAME`) was ungated, a live production
    /// one-shot dump, not a test-only knob. `cdf53_diff_tile` /
    /// `cdf53_dump_pending` / `cdf53_skip_l2_l3` / `cdf53_skip_l3` only
    /// parse under the `cdf53-diag` feature, mirroring the `#[cfg]` that
    /// gates their `io_bridge.rs` call sites; a build without the feature
    /// leaves them at `Default`.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            diagnose_tiles: matches!(
                get("GHOSTFRAME_DIAGNOSE_TILES").as_deref(),
                Some("1") | Some("true")
            ),
            diagnose_gpu_pipeline: matches!(
                get("GHOSTFRAME_DIAGNOSE_GPU_PIPELINE").as_deref(),
                Some("1") | Some("true")
            ),
            diagnose_color_hist: matches!(
                get("GHOSTFRAME_DIAGNOSE_COLOR_HIST").as_deref(),
                Some("1") | Some("true")
            ),
            dump_frame_path: get("GHOSTFRAME_DUMP_FRAME"),
            #[cfg(feature = "cdf53-diag")]
            cdf53_diff_tile: cdf53_diff_tile_from_lookup(&get),
            #[cfg(not(feature = "cdf53-diag"))]
            cdf53_diff_tile: None,
            #[cfg(feature = "cdf53-diag")]
            cdf53_dump_pending: get("GHOSTFRAME_CDF53_DUMP_PENDING").is_some(),
            #[cfg(not(feature = "cdf53-diag"))]
            cdf53_dump_pending: false,
            // `.is_some()`, not a value comparison: the former
            // `io_bridge.rs` sites used `std::env::var(..).is_ok()`, so mere
            // presence (even `""`) means true. `cdf53_skip_l3` is true if
            // *either* SKIP_L3 or SKIP_L2_L3 is set — the former call site
            // treated skip-L2-and-L3 as implying skip-L3
            // (`io_bridge.rs:3232-3233`). Both quirks are preserved as-is.
            #[cfg(feature = "cdf53-diag")]
            cdf53_skip_l2_l3: get("GHOSTFRAME_CDF53_SKIP_L2_L3").is_some(),
            #[cfg(not(feature = "cdf53-diag"))]
            cdf53_skip_l2_l3: false,
            #[cfg(feature = "cdf53-diag")]
            cdf53_skip_l3: get("GHOSTFRAME_CDF53_SKIP_L3").is_some()
                || get("GHOSTFRAME_CDF53_SKIP_L2_L3").is_some(),
            #[cfg(not(feature = "cdf53-diag"))]
            cdf53_skip_l3: false,
        }
    }

    /// Parse environment variables into a diagnostics configuration. Reads
    /// the real environment in every build — see `from_lookup`'s doc comment.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }
}

/// Not `Clone`: `transport` carries `TransportConfig`, which is not `Clone`
/// (see its doc comment).
#[derive(Debug, Default)]
pub struct LibConfig {
    pub classifier: ClassifierConfig,
    pub transport: TransportConfig,
    pub diagnostics: DiagnosticsConfig,
}

impl LibConfig {
    /// Parse all three sub-configs from the real environment. Intended for
    /// use at the executable boundary only (e.g. `GhostframeServer::new`'s
    /// callers) — everything downstream of that should take a `LibConfig`
    /// by value rather than calling this again.
    pub fn from_env() -> Self {
        Self {
            classifier: ClassifierConfig::from_env(),
            transport: TransportConfig::from_env(),
            diagnostics: DiagnosticsConfig::from_env(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the three tests below that read the *real* process
    /// environment.
    ///
    /// `cargo test` runs tests as threads in one process, so `set_var` /
    /// `remove_var` mutate state shared by all of them. Every other test in
    /// this file parses via `from_lookup` with a fixture and needs no lock;
    /// only the thin `from_env` wrappers touch the environment, and they would
    /// clobber each other without this.
    ///
    /// This used to live in `crate::test_env` and guard twenty tests across
    /// three modules. Threading `LibConfig` through constructors removed the
    /// need for all but these three, so it moved here with them.
    /// Poisoning is ignored: a panic in one of these must not cascade into
    /// spurious failures in the others and obscure the real cause.
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn defaults_are_production_inert() {
        let cfg = LibConfig::default();
        assert!(cfg.classifier.force_frame_mode.is_none());
        assert!(cfg.classifier.refinement_bias_us.is_none());
        assert!(cfg.classifier.headroom_min_bpus.is_none());
        assert!(cfg.classifier.loss_override_threshold.is_none());
        assert!(cfg.transport.outbound_loss.is_none());
        assert!(cfg.transport.inbound_loss.is_none());
        assert!(cfg.transport.outbound_bandwidth_cap_bps.is_none());
        assert!(cfg.transport.oob_inject_at.is_none());
        assert!(!cfg.transport.skip_palette_session_reset);
        assert!(cfg.transport.test_force_bytes_per_us.is_none());
        assert!(cfg.transport.fec_k.is_none());
        assert!(!cfg.diagnostics.diagnose_tiles);
        assert!(!cfg.diagnostics.diagnose_gpu_pipeline);
        assert!(!cfg.diagnostics.diagnose_color_hist);
        assert!(cfg.diagnostics.dump_frame_path.is_none());
        assert!(cfg.diagnostics.cdf53_diff_tile.is_none());
        assert!(!cfg.diagnostics.cdf53_dump_pending);
        assert!(!cfg.diagnostics.cdf53_skip_l2_l3);
        assert!(!cfg.diagnostics.cdf53_skip_l3);
    }

    /// Build a lookup closure over a small fixture, the way `from_env` builds
    /// one over the real environment. Tests use this instead of mutating
    /// process env, so they need no shared-environment-lock guard and cannot
    /// race other tests' `std::env::set_var` calls.
    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn classifier_config_parses_force_frame_mode() {
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[("GHOSTFRAME_TEST_FORCE_FRAME_MODE", "h264")]))
                .force_frame_mode,
            Some(FrameMode::H264)
        );
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[("GHOSTFRAME_TEST_FORCE_FRAME_MODE", "tile")]))
                .force_frame_mode,
            Some(FrameMode::TileCodec)
        );
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[])).force_frame_mode,
            None
        );
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[("GHOSTFRAME_TEST_FORCE_FRAME_MODE", "bogus")]))
                .force_frame_mode,
            None,
            "an unrecognised value must be ignored, not guessed at"
        );
    }

    #[test]
    fn force_tilecodec_is_an_alias_for_tile_mode() {
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[("GHOSTFRAME_FORCE_TILECODEC", "1")]))
                .force_frame_mode,
            Some(FrameMode::TileCodec)
        );
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[("GHOSTFRAME_FORCE_TILECODEC", "true")]))
                .force_frame_mode,
            Some(FrameMode::TileCodec)
        );
        // Any other value is ignored, not an error.
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[("GHOSTFRAME_FORCE_TILECODEC", "0")]))
                .force_frame_mode,
            None
        );
    }

    #[test]
    fn classifier_config_filters_out_of_range_values() {
        // loss_override_threshold accepts (0.0, 1.0]; bias and headroom accept > 0.0
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[(
                "GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD",
                "1.5"
            )]))
            .loss_override_threshold,
            None
        );
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[(
                "GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD",
                "0.5"
            )]))
            .loss_override_threshold,
            Some(0.5)
        );
        assert_eq!(
            ClassifierConfig::from_lookup(lookup(&[("GHOSTFRAME_TEST_HEADROOM_MIN_BPUS", "-1")]))
                .headroom_min_bpus,
            None
        );
    }

    /// `from_env` is a thin wrapper around `from_lookup` over the real
    /// environment; this is its only coverage for `ClassifierConfig`. It and
    /// its `TransportConfig` / `DiagnosticsConfig` counterparts further down
    /// are the only tests in this module that touch process-global state.
    #[test]
    fn from_env_reads_the_real_environment() {
        let _env = lock_env();
        std::env::set_var("GHOSTFRAME_TEST_FORCE_FRAME_MODE", "h264");
        assert_eq!(
            ClassifierConfig::from_env().force_frame_mode,
            Some(FrameMode::H264)
        );
        std::env::remove_var("GHOSTFRAME_TEST_FORCE_FRAME_MODE");
    }

    // -----------------------------------------------------------------
    // TransportConfig
    // -----------------------------------------------------------------
    //
    // Formerly `IoBridge::loss_injector_from_env` / `oob_injector_from_env` /
    // `skip_palette_session_reset_from_env` and their tests (including the
    // three PalRLE bundled/thin predicate-selection tests) in
    // `transport::io_bridge`'s test module (moved here with the code they
    // test; `lock_env()` guards dropped since `from_lookup` never touches
    // process env).
    //
    // Moving the predicate tests leaves `io_bridge.rs`'s
    // `DATAGRAM_HEADER_SIZE`/`TILE_HEADER_SIZE` import with no remaining
    // user at all (that import already went unused in a plain production
    // build before this move — see the "Critical constraints" note not to
    // fix the pre-existing warning at `io_bridge.rs:47-48` — and this move
    // is what generalizes that to every build config). That's an accepted,
    // known consequence of relocating this logic, not something to route
    // around by leaving otherwise-misplaced tests behind.

    const CODEC_BYTE_OFFSET: usize = crate::transport::protocol::DATAGRAM_HEADER_SIZE + 2;
    const PAYLOAD_START_OFFSET: usize = crate::transport::protocol::DATAGRAM_HEADER_SIZE
        + crate::transport::protocol::TILE_HEADER_SIZE;
    const PALRLE_MIN_WIRE_LEN: usize = PAYLOAD_START_OFFSET + 1;

    #[test]
    fn transport_config_parses_loss_probability_and_predicate() {
        let cfg = TransportConfig::from_lookup(lookup(&[
            ("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY", "0.5"),
            ("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE", "tile"),
            ("GHOSTFRAME_OUTBOUND_LOSS_SEED", "42"),
        ]));
        let mut inj = cfg.outbound_loss.expect("probability > 0 must yield Some");
        // Tile-datagram first byte (high bit set) → predicate matches → may drop.
        let tile_dg = [0x80u8, 0, 0, 1];
        // ACK datagram first byte (0x02) → predicate doesn't match → never drops.
        let ack_dg = [0x02u8, 0, 0, 0];
        assert!(!inj.should_drop(&ack_dg), "tile predicate filters ack out");
        // Tile path may or may not drop on a given call; just exercise it.
        let _ = inj.should_drop(&tile_dg);
        // inbound_loss must be untouched by OUTBOUND-only fixture entries.
        assert!(cfg.inbound_loss.is_none());
    }

    #[test]
    fn transport_config_loss_injector_none_when_unset() {
        assert!(TransportConfig::from_lookup(lookup(&[]))
            .inbound_loss
            .is_none());
    }

    #[test]
    fn transport_config_parses_oob_inject_at() {
        assert_eq!(
            TransportConfig::from_lookup(lookup(&[("GHOSTFRAME_INJECT_OOB_PALRLE", "5,7")]))
                .oob_inject_at,
            Some((5u32, 7u32))
        );
        assert_eq!(
            TransportConfig::from_lookup(lookup(&[])).oob_inject_at,
            None
        );
    }

    #[test]
    fn transport_config_parses_skip_palette_session_reset() {
        assert!(
            TransportConfig::from_lookup(lookup(&[("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET", "1")]))
                .skip_palette_session_reset,
            "env=1 must yield true"
        );
        assert!(
            !TransportConfig::from_lookup(lookup(&[])).skip_palette_session_reset,
            "unset must yield false"
        );
    }

    #[test]
    fn transport_config_parses_test_force_bytes_per_us() {
        assert_eq!(
            TransportConfig::from_lookup(lookup(&[("GHOSTFRAME_TEST_FORCE_BYTES_PER_US", "12.5")]))
                .test_force_bytes_per_us,
            Some(12.5)
        );
        // Non-positive values are filtered, not passed through.
        assert_eq!(
            TransportConfig::from_lookup(lookup(&[("GHOSTFRAME_TEST_FORCE_BYTES_PER_US", "0")]))
                .test_force_bytes_per_us,
            None
        );
    }

    #[test]
    fn transport_config_parses_fec_k() {
        assert_eq!(
            TransportConfig::from_lookup(lookup(&[("GHOSTFRAME_FEC_K", "4")])).fec_k,
            Some(4)
        );
        assert_eq!(TransportConfig::from_lookup(lookup(&[])).fec_k, None);
    }

    #[test]
    fn transport_config_parses_outbound_bandwidth_cap_bps() {
        assert_eq!(
            TransportConfig::from_lookup(lookup(&[(
                "GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP",
                "1250000"
            )]))
            .outbound_bandwidth_cap_bps,
            Some(1_250_000)
        );
        // Unset or zero both mean "no cap".
        assert_eq!(
            TransportConfig::from_lookup(lookup(&[])).outbound_bandwidth_cap_bps,
            None
        );
        assert_eq!(
            TransportConfig::from_lookup(lookup(&[("GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP", "0")]))
                .outbound_bandwidth_cap_bps,
            None
        );
    }

    /// Formerly `palrle_bundled_predicate_matches_bundled_datagram` in
    /// `transport::io_bridge`.
    #[test]
    fn transport_config_palrle_bundled_predicate_matches_bundled() {
        let mut wire = vec![0u8; PALRLE_MIN_WIRE_LEN];
        wire[0] = 0x80; // tile datagram flag
        wire[CODEC_BYTE_OFFSET] = (crate::transport::protocol::Codec::PalRle as u8) << 1;
        wire[PAYLOAD_START_OFFSET] = 0x01; // bundled
        let mut inj = TransportConfig::from_lookup(lookup(&[
            ("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY", "1.0"),
            ("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE", "palrle_bundled"),
            ("GHOSTFRAME_OUTBOUND_LOSS_SEED", "1"),
        ]))
        .outbound_loss
        .unwrap();
        assert!(inj.should_drop(&wire));
    }

    /// Formerly `palrle_bundled_predicate_rejects_thin_datagram` in
    /// `transport::io_bridge`.
    #[test]
    fn transport_config_palrle_bundled_predicate_rejects_thin() {
        let mut wire = vec![0u8; PALRLE_MIN_WIRE_LEN];
        wire[0] = 0x80;
        wire[CODEC_BYTE_OFFSET] = (crate::transport::protocol::Codec::PalRle as u8) << 1;
        wire[PAYLOAD_START_OFFSET] = 0x00; // thin
        let mut inj = TransportConfig::from_lookup(lookup(&[
            ("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY", "1.0"),
            ("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE", "palrle_bundled"),
            ("GHOSTFRAME_OUTBOUND_LOSS_SEED", "1"),
        ]))
        .outbound_loss
        .unwrap();
        assert!(!inj.should_drop(&wire));
    }

    /// Formerly `palrle_thin_predicate_matches_thin_only` in
    /// `transport::io_bridge`.
    #[test]
    fn transport_config_palrle_thin_predicate_matches_thin_only() {
        let mut wire = vec![0u8; PALRLE_MIN_WIRE_LEN];
        wire[0] = 0x80;
        wire[CODEC_BYTE_OFFSET] = (crate::transport::protocol::Codec::PalRle as u8) << 1;
        wire[PAYLOAD_START_OFFSET] = 0x00;
        let fixture = lookup(&[
            ("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY", "1.0"),
            ("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE", "palrle_thin"),
            ("GHOSTFRAME_OUTBOUND_LOSS_SEED", "1"),
        ]);
        let mut inj = TransportConfig::from_lookup(fixture).outbound_loss.unwrap();
        assert!(inj.should_drop(&wire));

        wire[PAYLOAD_START_OFFSET] = 0x01;
        // Re-create inj since it consumed RNG state; the predicate is the
        // only filter at proba=1.0, so should_drop is purely predicate-driven.
        let fixture2 = lookup(&[
            ("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY", "1.0"),
            ("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE", "palrle_thin"),
            ("GHOSTFRAME_OUTBOUND_LOSS_SEED", "1"),
        ]);
        let mut inj2 = TransportConfig::from_lookup(fixture2)
            .outbound_loss
            .unwrap();
        assert!(!inj2.should_drop(&wire));
    }

    /// `from_env` is a thin wrapper around `from_lookup` over the real
    /// environment.
    #[test]
    fn transport_config_from_env_reads_the_real_environment() {
        let _env = lock_env();
        std::env::set_var("GHOSTFRAME_FEC_K", "7");
        assert_eq!(TransportConfig::from_env().fec_k, Some(7));
        std::env::remove_var("GHOSTFRAME_FEC_K");
    }

    // -----------------------------------------------------------------
    // DiagnosticsConfig
    // -----------------------------------------------------------------
    //
    // Formerly `IoBridge::diagnose_tiles_from_env` and its test in
    // `transport::io_bridge`'s test module.

    #[test]
    fn diagnostics_config_parses_diagnose_tiles() {
        assert!(
            DiagnosticsConfig::from_lookup(lookup(&[("GHOSTFRAME_DIAGNOSE_TILES", "1")]))
                .diagnose_tiles
        );
        assert!(!DiagnosticsConfig::from_lookup(lookup(&[])).diagnose_tiles);
    }

    #[test]
    fn diagnostics_config_parses_gpu_pipeline_and_color_hist() {
        assert!(
            DiagnosticsConfig::from_lookup(lookup(&[("GHOSTFRAME_DIAGNOSE_GPU_PIPELINE", "true")]))
                .diagnose_gpu_pipeline
        );
        assert!(
            DiagnosticsConfig::from_lookup(lookup(&[("GHOSTFRAME_DIAGNOSE_COLOR_HIST", "1")]))
                .diagnose_color_hist
        );
        let empty = DiagnosticsConfig::from_lookup(lookup(&[]));
        assert!(!empty.diagnose_gpu_pipeline);
        assert!(!empty.diagnose_color_hist);
    }

    /// `from_env` is a thin wrapper around `from_lookup` over the real
    /// environment, and (unlike `ClassifierConfig`/`TransportConfig`) always
    /// reads it, even in production builds — see `from_lookup`'s doc comment.
    #[test]
    fn diagnostics_config_from_env_reads_the_real_environment() {
        let _env = lock_env();
        std::env::set_var("GHOSTFRAME_DIAGNOSE_TILES", "1");
        assert!(DiagnosticsConfig::from_env().diagnose_tiles);
        std::env::remove_var("GHOSTFRAME_DIAGNOSE_TILES");
    }

    /// `dump_frame_path` parses unconditionally (no `cdf53-diag` gate) —
    /// see `from_lookup`'s doc comment.
    #[test]
    fn diagnostics_config_parses_dump_frame_path() {
        assert_eq!(
            DiagnosticsConfig::from_lookup(lookup(&[("GHOSTFRAME_DUMP_FRAME", "/tmp/frame.bgra")]))
                .dump_frame_path,
            Some("/tmp/frame.bgra".to_string())
        );
        assert_eq!(
            DiagnosticsConfig::from_lookup(lookup(&[])).dump_frame_path,
            None
        );
    }

    #[cfg(feature = "cdf53-diag")]
    #[test]
    fn diagnostics_config_parses_cdf53_diff_tile() {
        assert_eq!(
            DiagnosticsConfig::from_lookup(lookup(&[("GHOSTFRAME_CDF53_DIFF_TILE", "5,7")]))
                .cdf53_diff_tile,
            Some((5u32, 7u32))
        );
        // Whitespace around each number is trimmed.
        assert_eq!(
            DiagnosticsConfig::from_lookup(lookup(&[("GHOSTFRAME_CDF53_DIFF_TILE", " 5 , 7 ")]))
                .cdf53_diff_tile,
            Some((5u32, 7u32))
        );
        assert_eq!(
            DiagnosticsConfig::from_lookup(lookup(&[])).cdf53_diff_tile,
            None
        );
        assert_eq!(
            DiagnosticsConfig::from_lookup(lookup(&[("GHOSTFRAME_CDF53_DIFF_TILE", "bogus")]))
                .cdf53_diff_tile,
            None
        );
    }

    #[cfg(feature = "cdf53-diag")]
    #[test]
    fn diagnostics_config_parses_cdf53_dump_pending_and_skip_gates() {
        // Presence, not value, is the signal — `.is_ok()` semantics.
        assert!(
            DiagnosticsConfig::from_lookup(lookup(&[("GHOSTFRAME_CDF53_DUMP_PENDING", "")]))
                .cdf53_dump_pending
        );
        assert!(!DiagnosticsConfig::from_lookup(lookup(&[])).cdf53_dump_pending);

        assert!(
            DiagnosticsConfig::from_lookup(lookup(&[("GHOSTFRAME_CDF53_SKIP_L2_L3", "")]))
                .cdf53_skip_l2_l3
        );
        assert!(!DiagnosticsConfig::from_lookup(lookup(&[])).cdf53_skip_l2_l3);

        // cdf53_skip_l3 is true if EITHER GHOSTFRAME_CDF53_SKIP_L3 or
        // GHOSTFRAME_CDF53_SKIP_L2_L3 is set — preserved quirk from the
        // former io_bridge.rs call site.
        assert!(
            DiagnosticsConfig::from_lookup(lookup(&[("GHOSTFRAME_CDF53_SKIP_L3", "")]))
                .cdf53_skip_l3
        );
        assert!(
            DiagnosticsConfig::from_lookup(lookup(&[("GHOSTFRAME_CDF53_SKIP_L2_L3", "")]))
                .cdf53_skip_l3,
            "SKIP_L2_L3 alone must also imply skip_l3"
        );
        assert!(!DiagnosticsConfig::from_lookup(lookup(&[])).cdf53_skip_l3);
    }
}
