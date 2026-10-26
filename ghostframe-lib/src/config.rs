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

/// Tuning knobs for the frame-mode classifier. All `None`/default means the
/// compiled-in value: every field is an override of a constant that stays the
/// default, and an unset or out-of-range variable changes nothing.
///
/// These used to compile only under `test-loss-injection` and carried
/// `GHOSTFRAME_TEST_*` names. They are real operator knobs (a bench-tuned
/// constant is wrong on other hardware), so they are now parsed in every
/// build under unprefixed names. The old `GHOSTFRAME_TEST_*` names are still
/// accepted as aliases, because the e2e and bench crates set them; the new
/// name wins when both are present.
///
/// All four fields are **wired**: `Classifier::new` consumes them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClassifierConfig {
    /// Pins the frame mode. Fed by `GHOSTFRAME_FORCE_FRAME_MODE`
    /// (`h264` | `tile`; alias `GHOSTFRAME_TEST_FORCE_FRAME_MODE`), or by
    /// `GHOSTFRAME_FORCE_TILECODEC=1|true` as a high-level alias for
    /// `TileCodec`.
    pub force_frame_mode: Option<FrameMode>,
    /// Overrides `REFINEMENT_BIAS_PER_TILE_US`. Fed by
    /// `GHOSTFRAME_REFINEMENT_BIAS_US` (alias
    /// `GHOSTFRAME_TEST_REFINEMENT_BIAS_US`); accepts any value `> 0.0`.
    pub refinement_bias_us: Option<f32>,
    /// Overrides `HEADROOM_MIN_BYTES_PER_US`. Fed by
    /// `GHOSTFRAME_HEADROOM_MIN_BPUS` (alias
    /// `GHOSTFRAME_TEST_HEADROOM_MIN_BPUS`); accepts any value `> 0.0`.
    pub headroom_min_bpus: Option<f32>,
    /// Overrides `LOSS_OVERRIDE_THRESHOLD`. Fed by
    /// `GHOSTFRAME_LOSS_OVERRIDE_THRESHOLD` (alias
    /// `GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD`); accepts `(0.0, 1.0]`.
    pub loss_override_threshold: Option<f32>,
    /// Per-codec encode-cost overrides. See `CostModelConfig`.
    pub cost: CostModelConfig,
}

/// Overrides for the classifier's encode-cost model (`tile::CostModel`).
///
/// The defaults were bench-tuned on one machine (see
/// `docs/specs/m3-codec-bench-results.md`); on other hardware the cost
/// comparison mis-classifies. Each field is `None` for the compiled-in value;
/// every accepted value must be finite and `> 0.0`, anything else is ignored.
///
/// All fields are **wired**: `Classifier::new` builds its `CostModel` from
/// them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostModelConfig {
    /// `GHOSTFRAME_COST_SOLID_US` (compiled-in 0.05).
    pub solid_us: Option<f32>,
    /// `GHOSTFRAME_COST_PALRLE_US` (compiled-in 8.0).
    pub palrle_us: Option<f32>,
    /// `GHOSTFRAME_COST_CDF53_US` (compiled-in 90.0).
    pub cdf53_us: Option<f32>,
    /// `GHOSTFRAME_COST_H264_FRAME_US` (compiled-in 3000.0).
    pub h264_frame_us: Option<f32>,
    /// `GHOSTFRAME_COST_H264_FRAME_BYTES` (compiled-in 12000); a `u32 > 0`.
    pub h264_frame_bytes: Option<u32>,
    /// `GHOSTFRAME_COST_BYTES_PER_US` (compiled-in 12.5).
    pub bytes_per_us: Option<f32>,
}

/// First variable of `names` that `get` can see. Used for env-name aliases:
/// the preferred name goes first, so it wins when both are set.
fn get_first(get: &impl Fn(&str) -> Option<String>, names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| get(n))
}

fn positive_f32(raw: Option<String>) -> Option<f32> {
    raw.and_then(|s| s.parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
}

impl CostModelConfig {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            solid_us: positive_f32(get("GHOSTFRAME_COST_SOLID_US")),
            palrle_us: positive_f32(get("GHOSTFRAME_COST_PALRLE_US")),
            cdf53_us: positive_f32(get("GHOSTFRAME_COST_CDF53_US")),
            h264_frame_us: positive_f32(get("GHOSTFRAME_COST_H264_FRAME_US")),
            h264_frame_bytes: get("GHOSTFRAME_COST_H264_FRAME_BYTES")
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|v| *v > 0),
            bytes_per_us: positive_f32(get("GHOSTFRAME_COST_BYTES_PER_US")),
        }
    }
}

impl ClassifierConfig {
    /// Parse from an arbitrary lookup. `from_env` is this with a real
    /// environment lookup; tests use a map so they never touch process-global
    /// state. Keeping the parsing here means the crate reads the process
    /// environment in exactly one place.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            refinement_bias_us: positive_f32(get_first(
                &get,
                &[
                    "GHOSTFRAME_REFINEMENT_BIAS_US",
                    "GHOSTFRAME_TEST_REFINEMENT_BIAS_US",
                ],
            )),
            loss_override_threshold: get_first(
                &get,
                &[
                    "GHOSTFRAME_LOSS_OVERRIDE_THRESHOLD",
                    "GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD",
                ],
            )
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|v| *v > 0.0 && *v <= 1.0),
            headroom_min_bpus: positive_f32(get_first(
                &get,
                &[
                    "GHOSTFRAME_HEADROOM_MIN_BPUS",
                    "GHOSTFRAME_TEST_HEADROOM_MIN_BPUS",
                ],
            )),
            // `GHOSTFRAME_FORCE_TILECODEC=1`/`true` is a high-level alias
            // for `GHOSTFRAME_FORCE_FRAME_MODE=tile`. Used by the
            // `e2e_lossless_golden_png` test to bypass the H.264 forced-
            // start at session entry so the cdf53 first-paint burst gets
            // exercised (H.264 has its own FEC + parity + NACK and renders
            // fully even under wire loss, masking tile-codec regressions).
            force_frame_mode: get("GHOSTFRAME_FORCE_TILECODEC")
                .filter(|s| s == "1" || s == "true")
                .map(|_| FrameMode::TileCodec)
                .or_else(|| {
                    get_first(
                        &get,
                        &[
                            "GHOSTFRAME_FORCE_FRAME_MODE",
                            "GHOSTFRAME_TEST_FORCE_FRAME_MODE",
                        ],
                    )
                    .and_then(|s| match s.as_str() {
                        "h264" | "H264" => Some(FrameMode::H264),
                        "tile" | "TileCodec" | "tilecodec" => Some(FrameMode::TileCodec),
                        _ => None,
                    })
                }),
            cost: CostModelConfig::from_lookup(&get),
        }
    }

    /// Parse environment variables into a classifier configuration.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
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
    /// Emit the tile XOR parity (k=10 groups, one parity datagram per group).
    /// Off by default: it recovered at most 0.8 % of datagrams and cost about
    /// 11 to 14 % of the bytes (13 % typical). From `GHOSTFRAME_TILE_FEC=1|true`. The client
    /// decodes parity whenever it arrives, whatever this says. H.264 parity is
    /// separate and unaffected.
    pub tile_fec: bool,
}

impl TransportConfig {
    /// Parse from an arbitrary lookup. `from_env` is this with a real
    /// environment lookup; tests use a map so they never touch process-global
    /// state. Keeping the parsing here means the crate reads the process
    /// environment in exactly one place.
    ///
    /// The `#[cfg(any(test, feature = "test-loss-injection"))]` sits on the
    /// individual field initialisers below rather than on the function: the
    /// two `LossInjector` fields (whose type only exists under the feature)
    /// and the others that were gated at their old `io_bridge` sites, mirroring
    /// the shape of `IoBridge`'s own struct literal.
    // Every read of `get` below is behind the test-loss-injection cfg, so a
    // build without it (the client crates) leaves the parameter unused.
    #[cfg_attr(
        not(any(test, feature = "test-loss-injection")),
        allow(unused_variables)
    )]
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
            tile_fec: matches!(
                get("GHOSTFRAME_TILE_FEC").as_deref(),
                Some("1") | Some("true")
            ),
        }
    }

    /// Parse environment variables into a transport configuration. Reads the
    /// real environment in every build — see `from_lookup`'s doc comment.
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
    /// `dump_frame_path` (`GHOSTFRAME_DUMP_FRAME`) parses only under
    /// `cfg(any(test, feature = "test-loss-injection"))`: it writes raw screen
    /// contents to a path the environment names, which a release daemon must
    /// never do. `cdf53_diff_tile` /
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
            // Writes screen contents to a caller-chosen path: test builds only.
            #[cfg(any(test, feature = "test-loss-injection"))]
            dump_frame_path: get("GHOSTFRAME_DUMP_FRAME"),
            #[cfg(not(any(test, feature = "test-loss-injection")))]
            dump_frame_path: None,
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

/// Who, besides this node's owner, may connect. From
/// `GHOSTFRAME_ALLOWED_USERS`, a comma-separated list of tailnet login names
/// (`alice@example.com`) and ACL tags (`tag:ops`).
///
/// The list only ever ADDS: the user who owns this node is always admitted
/// (see `transport::peer_auth`), and an empty or unset list means "the owner
/// only". There is deliberately no value that admits everyone.
///
/// **Wired.** `IoBridge::new` builds the `AccessPolicy` from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessConfig {
    /// Login names, matched exactly (case-sensitive) against the peer
    /// node's owning user. Never consulted for a tagged peer.
    pub allowed_logins: Vec<String>,
    /// ACL tags, each including its `tag:` prefix, matched exactly.
    pub allowed_tags: Vec<String>,
    /// Entries that were not valid and were dropped. Kept so startup can
    /// name them: a typo here silently locks a user out otherwise.
    pub rejected: Vec<String>,
    /// `GHOSTFRAME_WEB_ORIGINS`: extra browser origins whose pages may open a
    /// session, beyond this node's own `https://<FQDN>` (see
    /// `transport::origin`). Validated where the policy is built, which
    /// reports the malformed ones.
    pub web_origins: Vec<String>,
    /// `GHOSTFRAME_ACCESS_RECHECK_SECS` (default 60): how often a connected
    /// session's peer is checked against the access rule again, and closed
    /// if it no longer passes. Whole seconds in
    /// `MIN_RECHECK_SECS..=MAX_RECHECK_SECS`. Zero, non-numeric or
    /// out-of-range values are a startup error ([`ConfigError`]), never a
    /// silent fallback: the operator asked for something, and quietly running
    /// with another interval is not what they asked for.
    pub recheck_interval: std::time::Duration,
}

impl Default for AccessConfig {
    fn default() -> Self {
        Self {
            allowed_logins: Vec::new(),
            allowed_tags: Vec::new(),
            rejected: Vec::new(),
            web_origins: Vec::new(),
            recheck_interval: crate::transport::peer_auth::DEFAULT_RECHECK_INTERVAL,
        }
    }
}

/// A setting that is present but unusable. Startup is refused with it: the
/// process must not run with a configuration other than the one requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub variable: &'static str,
    pub value: String,
    pub expected: &'static str,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid {}={:?}: expected {}",
            self.variable, self.value, self.expected
        )
    }
}

impl std::error::Error for ConfigError {}

impl AccessConfig {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        use crate::transport::peer_auth::{MAX_RECHECK_SECS, MIN_RECHECK_SECS};
        let mut cfg = Self::default();
        if let Some(raw) = get("GHOSTFRAME_WEB_ORIGINS") {
            cfg.web_origins = raw
                .split(',')
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(str::to_owned)
                .collect();
        }
        if let Some(raw) = get("GHOSTFRAME_ACCESS_RECHECK_SECS") {
            match raw.trim().parse::<u64>() {
                Ok(n) if (MIN_RECHECK_SECS..=MAX_RECHECK_SECS).contains(&n) => {
                    cfg.recheck_interval = std::time::Duration::from_secs(n);
                }
                _ => {
                    return Err(ConfigError {
                        variable: "GHOSTFRAME_ACCESS_RECHECK_SECS",
                        value: raw,
                        expected: "whole seconds from 1 to 86400",
                    })
                }
            }
        }
        let Some(raw) = get("GHOSTFRAME_ALLOWED_USERS") else {
            return Ok(cfg);
        };
        for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let well_formed = !entry.chars().any(|c| c.is_whitespace() || c.is_control());
            match entry.strip_prefix("tag:") {
                Some(name) if well_formed && !name.is_empty() => {
                    cfg.allowed_tags.push(entry.to_owned());
                }
                None if well_formed => cfg.allowed_logins.push(entry.to_owned()),
                _ => cfg.rejected.push(entry.to_owned()),
            }
        }
        Ok(cfg)
    }

    pub fn from_env() -> Result<Self, ConfigError> {
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
    pub access: AccessConfig,
}

impl LibConfig {
    /// Parse all three sub-configs from the real environment. Intended for
    /// use at the executable boundary only (e.g. `GhostframeServer::new`'s
    /// callers) — everything downstream of that should take a `LibConfig`
    /// by value rather than calling this again.
    ///
    /// Fails on a setting that is present but unusable (see [`ConfigError`]);
    /// the caller refuses to start.
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self {
            classifier: ClassifierConfig::from_env(),
            transport: TransportConfig::from_env(),
            diagnostics: DiagnosticsConfig::from_env(),
            access: AccessConfig::from_env()?,
        })
    }
}

// ── Transport knobs read here, not at the consumption site ───────────────
//
// These four live here for the reason this module exists: `cargo test` runs
// tests as threads in one process, so a call-time `env::var` is a
// process-global read that races other tests. CI enforces it -- grep for
// `env::var` outside this file fails the build.

/// Reliable-emitter retransmission-timer knob: the backoff ceiling.
///
/// The deadline itself is measured, not configured: an RFC 9002 probe timeout
/// from the departure-based RTT estimator (`rto::pto_for_attempt`). The
/// `ACK_DEADLINE_FLOOR/COLD_START/CEILING` constants and their environment
/// overrides went with the p95 x 2 formula they bounded. The default is the
/// compiled-in constant in `transport/reliable_emitter/rto.rs`, so an unset
/// environment changes nothing.
///
/// `pto_for_attempt` reads the process-wide copy returned by
/// `emitter_config()` (parsed once, like `no_rto()`); `pto_for_attempt_with`
/// takes an explicit config and is what the tests drive.
///
/// There is deliberately no cache-capacity field: `RetransmitCache` is
/// unbounded by design (an un-ACKed tile-pass is never dropped) and has no
/// capacity at all, so a knob for one would read from the environment and
/// change nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmitterConfig {
    /// `GHOSTFRAME_RTO_BACKOFF_MAX_MS` (default 5000). Any value `> 0`.
    pub rto_backoff_max: std::time::Duration,
}

impl Default for EmitterConfig {
    fn default() -> Self {
        use crate::transport::reliable_emitter::rto;
        Self {
            rto_backoff_max: rto::RTO_BACKOFF_MAX,
        }
    }
}

impl EmitterConfig {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let ms = |name: &str, default: std::time::Duration| {
            get(name)
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .map(std::time::Duration::from_millis)
                .unwrap_or(default)
        };
        let d = Self::default();
        Self {
            rto_backoff_max: ms("GHOSTFRAME_RTO_BACKOFF_MAX_MS", d.rto_backoff_max),
        }
    }

    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }
}

/// Process-wide `EmitterConfig`, parsed once. Cached for the same reason as
/// `no_rto()`: `pto_for_attempt` runs on the scheduler tick.
pub(crate) fn emitter_config() -> &'static EmitterConfig {
    static CFG: std::sync::OnceLock<EmitterConfig> = std::sync::OnceLock::new();
    CFG.get_or_init(EmitterConfig::from_env)
}

/// `GHOSTFRAME_VAAPI_DEVICE`: VA-API render node the H.264 encoders open,
/// overriding the default `/dev/dri/renderD128` (wrong on multi-GPU hosts).
/// `None` when unset or blank. Cached: a device that changed mid-process would
/// make one session's contexts disagree with each other.
///
/// **Wired:** `encoder::vaapi_device::vaapi_device_path()` applies it at the
/// three `h264_vaapi` context-creation sites.
pub(crate) fn vaapi_device_override() -> Option<&'static str> {
    static DEV: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    DEV.get_or_init(|| vaapi_device_from_lookup(|k| std::env::var(k).ok()))
        .as_deref()
}

fn vaapi_device_from_lookup(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    get("GHOSTFRAME_VAAPI_DEVICE").filter(|s| !s.trim().is_empty())
}

/// Size of quinn's datagram send buffer, in bytes.
///
/// 16 MiB by default. `GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES` overrides it so
/// the buffer-size sweeps in `browserless_blocked_path` can be repeated
/// without a rebuild. A zero or unparseable value falls back to the default:
/// a zero-byte buffer rejects every datagram, which presents as total packet
/// loss rather than as a bad config value.
///
/// Deliberately **not** cached: the tests below set the variable and expect
/// the next call to observe it.
///
/// The override exists only under `cfg(any(test, feature =
/// "test-loss-injection"))`; a release daemon always uses the default.
pub fn datagram_send_buffer_bytes() -> usize {
    const DEFAULT: usize = 16 * 1024 * 1024;
    #[cfg(any(test, feature = "test-loss-injection"))]
    fn override_bytes() -> Option<usize> {
        std::env::var("GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
    }
    #[cfg(not(any(test, feature = "test-loss-injection")))]
    fn override_bytes() -> Option<usize> {
        None
    }
    override_bytes().unwrap_or(DEFAULT)
}

/// `GHOSTFRAME_ACK_ORDER_PROBE=1` — emit one line per ACK batch for offline
/// reorder histogramming. Cached: a probe flag that changed mid-process would
/// make its own output uninterpretable.
pub(crate) fn ack_order_probe_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("GHOSTFRAME_ACK_ORDER_PROBE").is_ok_and(|v| v == "1"))
}

/// `GHOSTFRAME_RTO_PROBE=1` — one line per RTO fire and per acknowledgement.
pub(crate) fn rto_probe_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("GHOSTFRAME_RTO_PROBE").is_ok_and(|v| v == "1"))
}

/// `GHOSTFRAME_NO_RTO=1` — disable the RTO timer, leaving the cache and NACK
/// path untouched, so a test can attribute a repair to one mechanism.
///
/// Cached, unlike the buffer size above. This gates `RtoHeap::pop_due`, which
/// runs on every scheduler tick; it previously read the environment on each
/// call, which is both a guard violation and a syscall-shaped cost in a hot
/// path.
///
/// Test builds only (`cfg(any(test, feature = "test-loss-injection"))`): a
/// release daemon must not let its environment turn off loss recovery.
#[cfg(any(test, feature = "test-loss-injection"))]
pub(crate) fn no_rto() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("GHOSTFRAME_NO_RTO").is_ok_and(|v| v == "1"))
}

#[cfg(not(any(test, feature = "test-loss-injection")))]
pub(crate) fn no_rto() -> bool {
    false
}

/// Whether this build of `ghostframe-lib` was compiled with its test knobs
/// (`test-loss-injection`). Lets `ghostframe-xdaemon`, which has no features
/// of its own, gate its dump env vars on the same switch. A function rather
/// than a `pub const` so it stays out of the generated C header.
pub fn test_knobs_compiled() -> bool {
    cfg!(feature = "test-loss-injection")
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

    // -----------------------------------------------------------------
    // AccessConfig (GHOSTFRAME_ALLOWED_USERS)
    // -----------------------------------------------------------------

    #[test]
    fn web_origins_split_and_trim() {
        assert!(AccessConfig::from_lookup(lookup(&[]))
            .unwrap()
            .web_origins
            .is_empty());
        let c = AccessConfig::from_lookup(lookup(&[(
            "GHOSTFRAME_WEB_ORIGINS",
            " https://a.example , ,https://127.0.0.1:* ",
        )]))
        .unwrap();
        assert_eq!(c.web_origins, ["https://a.example", "https://127.0.0.1:*"]);
        assert!(c.allowed_logins.is_empty());
    }

    #[test]
    fn allowed_users_unset_means_owner_only() {
        assert_eq!(
            AccessConfig::from_lookup(lookup(&[])).unwrap(),
            AccessConfig::default()
        );
        assert_eq!(
            AccessConfig::from_lookup(lookup(&[("GHOSTFRAME_ALLOWED_USERS", " , ,")])).unwrap(),
            AccessConfig::default()
        );
    }

    #[test]
    fn recheck_interval_defaults_to_sixty_seconds() {
        let c = AccessConfig::from_lookup(lookup(&[])).unwrap();
        assert_eq!(c.recheck_interval, std::time::Duration::from_secs(60));
    }

    #[test]
    fn recheck_interval_is_configurable() {
        let c = AccessConfig::from_lookup(lookup(&[("GHOSTFRAME_ACCESS_RECHECK_SECS", " 15 ")]))
            .unwrap();
        assert_eq!(c.recheck_interval, std::time::Duration::from_secs(15));
        assert!(c.rejected.is_empty());
    }

    // A bad value refuses startup, naming the variable and the value; it
    // neither switches re-checking off (0 would mean "never" or "spin") nor
    // quietly runs with a default the operator did not ask for.
    #[test]
    fn recheck_interval_rejects_nonsense_at_startup() {
        for bad in [
            "0",
            "-5",
            "abc",
            "",
            "1.5",
            "5m",
            "60s",
            "86401",
            "99999999999999999999",
        ] {
            let e = AccessConfig::from_lookup(lookup(&[("GHOSTFRAME_ACCESS_RECHECK_SECS", bad)]))
                .expect_err(bad);
            assert_eq!(e.variable, "GHOSTFRAME_ACCESS_RECHECK_SECS");
            assert_eq!(e.value, bad);
            let shown = e.to_string();
            assert!(shown.contains("GHOSTFRAME_ACCESS_RECHECK_SECS"), "{shown}");
            assert!(shown.contains(&format!("{bad:?}")), "{shown}");
        }
    }

    #[test]
    fn recheck_interval_bounds_are_accepted() {
        for (v, secs) in [("1", 1), ("86400", 86400)] {
            let c = AccessConfig::from_lookup(lookup(&[("GHOSTFRAME_ACCESS_RECHECK_SECS", v)]))
                .unwrap();
            assert_eq!(c.recheck_interval, std::time::Duration::from_secs(secs));
        }
    }

    #[test]
    fn allowed_users_splits_logins_from_tags() {
        let c = AccessConfig::from_lookup(lookup(&[(
            "GHOSTFRAME_ALLOWED_USERS",
            "alice@example.com, tag:ops ,bob@github",
        )]))
        .unwrap();
        assert_eq!(c.allowed_logins, vec!["alice@example.com", "bob@github"]);
        assert_eq!(c.allowed_tags, vec!["tag:ops"]);
        assert!(c.rejected.is_empty());
    }

    #[test]
    fn allowed_users_rejects_malformed_entries_by_name() {
        let c = AccessConfig::from_lookup(lookup(&[(
            "GHOSTFRAME_ALLOWED_USERS",
            "tag:,al ice@example.com,ok@example.com,tag:a b",
        )]))
        .unwrap();
        assert_eq!(c.allowed_logins, vec!["ok@example.com"]);
        assert!(c.allowed_tags.is_empty());
        assert_eq!(c.rejected, vec!["tag:", "al ice@example.com", "tag:a b"]);
    }

    // There is no spelling that admits everyone: `*` is just a login name
    // no user has.
    #[test]
    fn allowed_users_has_no_wildcard() {
        let c =
            AccessConfig::from_lookup(lookup(&[("GHOSTFRAME_ALLOWED_USERS", "*,tag:*")])).unwrap();
        assert_eq!(c.allowed_logins, vec!["*"]);
        assert_eq!(c.allowed_tags, vec!["tag:*"]);
    }

    // -----------------------------------------------------------------
    // Un-gated classifier overrides, CostModelConfig, EmitterConfig
    // -----------------------------------------------------------------

    #[test]
    fn classifier_config_accepts_new_names_and_old_aliases() {
        let c = ClassifierConfig::from_lookup(lookup(&[
            ("GHOSTFRAME_REFINEMENT_BIAS_US", "7.5"),
            ("GHOSTFRAME_HEADROOM_MIN_BPUS", "0.5"),
            ("GHOSTFRAME_LOSS_OVERRIDE_THRESHOLD", "0.2"),
            ("GHOSTFRAME_FORCE_FRAME_MODE", "h264"),
        ]));
        assert_eq!(c.refinement_bias_us, Some(7.5));
        assert_eq!(c.headroom_min_bpus, Some(0.5));
        assert_eq!(c.loss_override_threshold, Some(0.2));
        assert_eq!(c.force_frame_mode, Some(FrameMode::H264));

        // The names the e2e and bench crates still set.
        let c = ClassifierConfig::from_lookup(lookup(&[
            ("GHOSTFRAME_TEST_REFINEMENT_BIAS_US", "1000"),
            ("GHOSTFRAME_TEST_HEADROOM_MIN_BPUS", "0.05"),
            ("GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD", "0.5"),
            ("GHOSTFRAME_TEST_FORCE_FRAME_MODE", "tile"),
        ]));
        assert_eq!(c.refinement_bias_us, Some(1000.0));
        assert_eq!(c.headroom_min_bpus, Some(0.05));
        assert_eq!(c.loss_override_threshold, Some(0.5));
        assert_eq!(c.force_frame_mode, Some(FrameMode::TileCodec));

        // New name wins when both are set.
        let c = ClassifierConfig::from_lookup(lookup(&[
            ("GHOSTFRAME_TEST_REFINEMENT_BIAS_US", "1"),
            ("GHOSTFRAME_REFINEMENT_BIAS_US", "2"),
        ]));
        assert_eq!(c.refinement_bias_us, Some(2.0));
    }

    #[test]
    fn cost_model_config_default_is_the_compiled_in_model() {
        use crate::tile::classifier::CostModel;
        // The literals are the values the model had before it was
        // configurable. A retune must change this test deliberately.
        let m = CostModel::from_config(&CostModelConfig::default());
        assert_eq!(m.solid_us, 0.05);
        assert_eq!(m.palrle_us, 8.0);
        assert_eq!(m.cdf53_us, 90.0);
        assert_eq!(m.h264_frame_us, 3000.0);
        assert_eq!(m.h264_frame_bytes, 12_000);
        assert_eq!(m.bytes_per_us(), 12.5);
        assert_eq!(
            CostModelConfig::from_lookup(lookup(&[])),
            CostModelConfig::default()
        );
    }

    #[test]
    fn cost_model_config_parses_and_filters() {
        let c = CostModelConfig::from_lookup(lookup(&[
            ("GHOSTFRAME_COST_SOLID_US", "0.1"),
            ("GHOSTFRAME_COST_PALRLE_US", "-3"),
            ("GHOSTFRAME_COST_CDF53_US", "nan"),
            ("GHOSTFRAME_COST_H264_FRAME_US", "4000"),
            ("GHOSTFRAME_COST_H264_FRAME_BYTES", "0"),
            ("GHOSTFRAME_COST_BYTES_PER_US", "25"),
        ]));
        assert_eq!(c.solid_us, Some(0.1));
        assert_eq!(c.palrle_us, None, "non-positive is ignored");
        assert_eq!(c.cdf53_us, None, "NaN is ignored");
        assert_eq!(c.h264_frame_us, Some(4000.0));
        assert_eq!(c.h264_frame_bytes, None, "zero bytes would divide by it");
        assert_eq!(c.bytes_per_us, Some(25.0));
        let m = crate::tile::classifier::CostModel::from_config(&c);
        assert_eq!(m.solid_us, 0.1);
        assert_eq!(m.palrle_us, 8.0, "an ignored override keeps the default");
        assert_eq!(m.bytes_per_us(), 25.0);
    }

    #[test]
    fn emitter_config_default_is_the_old_constants() {
        use std::time::Duration;
        let d = EmitterConfig::default();
        assert_eq!(d.rto_backoff_max, Duration::from_secs(5));
        assert_eq!(EmitterConfig::from_lookup(lookup(&[])), d);
    }

    #[test]
    fn emitter_config_parses_ms_and_rejects_nonsense() {
        use std::time::Duration;
        let c = EmitterConfig::from_lookup(lookup(&[("GHOSTFRAME_RTO_BACKOFF_MAX_MS", "10000")]));
        assert_eq!(c.rto_backoff_max, Duration::from_secs(10));
        for bad in ["0", "abc", "-5"] {
            let c = EmitterConfig::from_lookup(lookup(&[("GHOSTFRAME_RTO_BACKOFF_MAX_MS", bad)]));
            assert_eq!(c, EmitterConfig::default(), "{bad:?} ignored");
        }
    }

    #[test]
    fn vaapi_device_override_ignores_blank() {
        assert_eq!(vaapi_device_from_lookup(lookup(&[])), None);
        assert_eq!(
            vaapi_device_from_lookup(lookup(&[("GHOSTFRAME_VAAPI_DEVICE", "  ")])),
            None
        );
        assert_eq!(
            vaapi_device_from_lookup(lookup(&[(
                "GHOSTFRAME_VAAPI_DEVICE",
                "/dev/dri/renderD129"
            )])),
            Some("/dev/dri/renderD129".to_string())
        );
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
    fn tile_fec_is_off_unless_the_env_asks() {
        assert!(!TransportConfig::from_lookup(lookup(&[])).tile_fec);
        assert!(!TransportConfig::from_lookup(lookup(&[("GHOSTFRAME_TILE_FEC", "0")])).tile_fec);
        assert!(TransportConfig::from_lookup(lookup(&[("GHOSTFRAME_TILE_FEC", "1")])).tile_fec);
        assert!(TransportConfig::from_lookup(lookup(&[("GHOSTFRAME_TILE_FEC", "true")])).tile_fec);
        assert!(!TransportConfig::default().tile_fec);
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
        std::env::set_var("GHOSTFRAME_TEST_FORCE_BYTES_PER_US", "7.5");
        assert_eq!(
            TransportConfig::from_env().test_force_bytes_per_us,
            Some(7.5)
        );
        std::env::remove_var("GHOSTFRAME_TEST_FORCE_BYTES_PER_US");
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

    // Moved here from `transport::quic`, which had its own `lock_env` over a
    // *different* mutex -- so the two sets of env tests did not serialise
    // against each other and could still race. One lock, one place.

    #[test]
    fn datagram_send_buffer_defaults_to_16_mib() {
        let _g = lock_env();
        std::env::remove_var("GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES");
        assert_eq!(datagram_send_buffer_bytes(), 16 * 1024 * 1024);
    }

    #[test]
    fn datagram_send_buffer_env_override_is_honoured() {
        let _g = lock_env();
        std::env::set_var("GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES", "1048576");
        assert_eq!(datagram_send_buffer_bytes(), 1024 * 1024);
        std::env::remove_var("GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES");
    }

    #[test]
    fn datagram_send_buffer_nonsense_values_fall_back() {
        // A zero-byte send buffer rejects every datagram, which would look
        // like total packet loss rather than a bad config value.
        let _g = lock_env();
        for bad in ["0", "-1", "lots", ""] {
            std::env::set_var("GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES", bad);
            assert_eq!(
                datagram_send_buffer_bytes(),
                16 * 1024 * 1024,
                "{bad:?} should fall back to the default"
            );
        }
        std::env::remove_var("GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES");
    }
}
