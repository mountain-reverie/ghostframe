//! Gilbert-Elliott burst-loss parameters, solved from quantities a test
//! cares about (long-run loss rate, mean loss-run length) instead of the raw
//! transition probabilities.
//!
//! # Why this exists
//!
//! `NetSim::decide` has always carried a two-state loss machine
//! (`NetProfile::{burst_enter, burst_exit, burst_loss, loss}`), but the only
//! way to configure it was to guess transition probabilities, and the only
//! test was qualitative ("drops cluster"). Real links lose packets in
//! bursts; a protocol tuned against i.i.d. loss at the same average rate can
//! behave very differently. This module makes the two knobs that matter --
//! average rate and mean burst length -- the inputs.
//!
//! # Derivation, for `decide`'s exact order
//!
//! `decide` advances the state first and rolls loss with the *new* state.
//! States: Good (loss `lg`), Bad (loss `lb`). `p = P(G->B)`, `r = P(B->G)`.
//!
//! * Stationary `pi_B = p / (p + r)`. Because the roll uses the post-
//!   transition state, the probability that a given datagram is rolled in
//!   Bad is still `pi_B` (the chain is stationary at every step).
//! * Long-run loss `avg = pi_B * lb + (1 - pi_B) * lg`.
//! * A Bad sojourn begins at the datagram on which the G->B transition
//!   happened (that datagram is rolled in Bad) and lasts a geometric number
//!   of datagrams with mean `1 / r`. So **mean Bad sojourn = `1 / r`**.
//!   With `lb = 1`, `lg = 0` every Bad datagram is lost and every Good one
//!   delivered, so the loss run is exactly the sojourn: mean run = `1 / r`.
//!
//! Solving for a target `(avg, L)` with `lg = 0`:
//!
//! ```text
//! r    = 1 / L
//! pi_B = avg / lb
//! p    = r * pi_B / (1 - pi_B)        (needs pi_B < 1 and p <= 1)
//! ```
//!
//! Note `L = 1` gives `r = 1`: Bad never lasts two datagrams, so losses are
//! *never adjacent* (mean run exactly 1). That is "anti-clustered", not
//! i.i.d.; true i.i.d. at rate `a` has mean run `1 / (1 - a)`. For an i.i.d.
//! control use `NetProfile::loss` directly.
//!
//! # Time, not datagrams (the default)
//!
//! The derivation above is for a chain that steps once per datagram. That is
//! wrong for a link whose sender backs off: a quinn connection that has lost
//! a burst sends one probe every few seconds, and each probe stepped the
//! chain once, so a "burst of 8 datagrams" lasted eight probes -- tens of
//! seconds -- and every probe died inside it (seed 0x22 stranded 147 tiles
//! for that reason; PR #237). A real burst is an interval of time.
//!
//! [`GilbertElliott::from_rate_and_burst`] therefore builds a chain that
//! steps once per [`BURST_QUANTUM_US`] of virtual time, whatever was sent:
//! `NetSim::decide` advances it by the number of quanta elapsed since the
//! previous datagram, in closed form (one rng draw per call, as before), so
//! a gap of any length costs O(1) and the draw order is unchanged.
//!
//! **Conversion from datagrams.** The `len` the callers and the env protocol
//! give is "mean burst in datagrams *at the reference send rate*". The
//! reference is [`REFERENCE_DATAGRAM_INTERVAL_US`] = 1000 us: the browserless
//! burst scenes offer ~1030 s2c datagrams per virtual second on a clean link
//! (12363 in 12 s, `iid_versus_burst_loss_at_equal_average_rate`). So
//! `mean burst = len * 1000 us`, i.e. 8 datagrams is an 8 ms burst, and a
//! scene sending at the reference rate sees the same mean loss rate and (to
//! within +-0.5 datagram) the same loss-run length as before. A sender
//! slower than the reference now sees shorter runs, which is the point.
//! Per quantum: `r = 1 / (len * REF / QUANTUM)`, `pi_B` and `p` as above.
//!
//! The per-datagram chain remains available as
//! [`GilbertElliott::from_rate_and_burst_per_datagram`] (and as the raw
//! `NetProfile` fields with `burst_quantum_us == 0`) for analyses that
//! reason about runs of consecutive datagrams by index
//! (`fec_burst_analysis`) and for the netsim mechanics tests.
//!
//! # Env protocol (harness layer: QUIC packets, browserless only)
//!
//! Per direction `<DIR>` in `OUTBOUND` (server -> client) / `INBOUND`
//! (client -> server). Deliberately **different names** from ghostframe-lib's
//! i.i.d. `GHOSTFRAME_<DIR>_LOSS_*` so the two can never double-inject:
//!
//! * `GHOSTFRAME_<DIR>_BURST_LOSS_PROBABILITY` -- long-run average, in (0, 1).
//!   Unset => no burst model for that direction.
//! * `GHOSTFRAME_<DIR>_BURST_MEAN_LEN` -- mean burst length in datagrams at
//!   the reference send rate (see above), `>= 1`. Required when the
//!   probability is set.
//! * `GHOSTFRAME_<DIR>_BURST_LOSS_BAD` -- loss probability in the bad state,
//!   default `1.0`.
//! * `GHOSTFRAME_<DIR>_BURST_SEED` -- u64, default `0`; XORed into the
//!   direction's `NetSim` seed.

use super::NetProfile;

/// Which way the injected loss acts, using the same names as the lib's
/// i.i.d. injection: outbound = server -> client, inbound = client -> server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BurstDirection {
    Outbound,
    Inbound,
}

impl BurstDirection {
    fn tag(self) -> &'static str {
        match self {
            BurstDirection::Outbound => "OUTBOUND",
            BurstDirection::Inbound => "INBOUND",
        }
    }
}

/// Virtual-time step of the time-based chain, in microseconds.
pub const BURST_QUANTUM_US: u64 = 100;

/// The send interval that converts "datagrams" to time: see the module docs.
pub const REFERENCE_DATAGRAM_INTERVAL_US: u64 = 1_000;

/// Gilbert-Elliott parameters. See the module docs for semantics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GilbertElliott {
    /// Good -> Bad transition probability.
    pub p_gb: f64,
    /// Bad -> Good transition probability.
    pub p_bg: f64,
    pub loss_good: f64,
    pub loss_bad: f64,
    /// Virtual microseconds per chain step; 0 steps once per datagram.
    /// `p_gb`/`p_bg` are probabilities per step.
    pub quantum_us: u64,
}

impl GilbertElliott {
    /// Time-based model: stationary loss rate `avg_loss`, mean Bad sojourn
    /// `mean_burst_len` datagrams at the reference send rate
    /// ([`REFERENCE_DATAGRAM_INTERVAL_US`]), `loss_good` fixed at 0.
    pub fn from_rate_and_burst(
        avg_loss: f64,
        mean_burst_len: f64,
        loss_bad: f64,
    ) -> Result<Self, String> {
        if !(mean_burst_len.is_finite() && mean_burst_len >= 1.0) {
            return Err(format!("mean_burst_len {mean_burst_len} must be >= 1"));
        }
        Self::from_rate_and_burst_us(
            avg_loss,
            mean_burst_len * REFERENCE_DATAGRAM_INTERVAL_US as f64,
            loss_bad,
        )
    }

    /// Time-based model with the burst given directly as a mean duration.
    pub fn from_rate_and_burst_us(
        avg_loss: f64,
        mean_burst_us: f64,
        loss_bad: f64,
    ) -> Result<Self, String> {
        if !(mean_burst_us.is_finite() && mean_burst_us >= BURST_QUANTUM_US as f64) {
            return Err(format!(
                "mean_burst_us {mean_burst_us} must be >= the {BURST_QUANTUM_US} us quantum"
            ));
        }
        let steps = mean_burst_us / BURST_QUANTUM_US as f64;
        let mut m = Self::solve(avg_loss, steps, loss_bad)?;
        m.quantum_us = BURST_QUANTUM_US;
        Ok(m)
    }

    /// The old model: the chain steps once per datagram, so `mean_burst_len`
    /// is a count of datagrams whatever the send rate. Only for analyses
    /// that reason about runs by datagram index.
    pub fn from_rate_and_burst_per_datagram(
        avg_loss: f64,
        mean_burst_len: f64,
        loss_bad: f64,
    ) -> Result<Self, String> {
        if !(mean_burst_len.is_finite() && mean_burst_len >= 1.0) {
            return Err(format!("mean_burst_len {mean_burst_len} must be >= 1"));
        }
        Self::solve(avg_loss, mean_burst_len, loss_bad)
    }

    /// Per-step probabilities for a mean Bad sojourn of `mean_burst_len` steps.
    fn solve(avg_loss: f64, mean_burst_len: f64, loss_bad: f64) -> Result<Self, String> {
        if !(avg_loss.is_finite() && avg_loss > 0.0 && avg_loss < 1.0) {
            return Err(format!("avg_loss {avg_loss} must be in (0, 1)"));
        }
        if !(loss_bad.is_finite() && loss_bad > 0.0 && loss_bad <= 1.0) {
            return Err(format!("loss_bad {loss_bad} must be in (0, 1]"));
        }
        if avg_loss >= loss_bad {
            return Err(format!(
                "avg_loss {avg_loss} must be < loss_bad {loss_bad}: the link cannot lose \
                 more on average than it does while in the bad state"
            ));
        }
        let p_bg = 1.0 / mean_burst_len;
        let pi_bad = avg_loss / loss_bad;
        let p_gb = p_bg * pi_bad / (1.0 - pi_bad);
        if p_gb > 1.0 {
            return Err(format!(
                "infeasible: avg_loss {avg_loss} with mean_burst_len {mean_burst_len} \
                 and loss_bad {loss_bad} needs p_gb {p_gb:.3} > 1 (bursts too long for \
                 that much loss)"
            ));
        }
        Ok(Self {
            p_gb,
            p_bg,
            loss_good: 0.0,
            loss_bad,
            quantum_us: 0,
        })
    }

    /// Mean Bad sojourn in microseconds (`None` for the per-datagram model,
    /// which has no time scale).
    pub fn mean_burst_us(&self) -> Option<f64> {
        (self.quantum_us > 0).then(|| self.quantum_us as f64 / self.p_bg)
    }

    /// Stationary probability of being in the Bad state.
    pub fn stationary_bad(&self) -> f64 {
        self.p_gb / (self.p_gb + self.p_bg)
    }

    /// Analytic long-run loss rate.
    pub fn expected_loss(&self) -> f64 {
        let b = self.stationary_bad();
        b * self.loss_bad + (1.0 - b) * self.loss_good
    }

    /// Write the model into a profile's Gilbert-Elliott fields.
    pub fn apply_to(&self, profile: &mut NetProfile) {
        profile.burst_enter = self.p_gb;
        profile.burst_exit = self.p_bg;
        profile.burst_loss = self.loss_bad;
        profile.loss = self.loss_good;
        profile.burst_quantum_us = self.quantum_us;
    }
}

/// A model plus the seed salt from the env protocol.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BurstConfig {
    pub model: GilbertElliott,
    pub seed: u64,
}

impl BurstConfig {
    /// Parse one direction from an arbitrary lookup. `Ok(None)` when the
    /// probability variable is unset; `Err` on any malformed or infeasible
    /// value (a typo must not silently turn the injection off).
    pub fn from_lookup(
        dir: BurstDirection,
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, String> {
        let name = |suffix: &str| format!("GHOSTFRAME_{}_BURST_{suffix}", dir.tag());
        let parse_f64 = |n: &str, raw: String| {
            raw.trim()
                .parse::<f64>()
                .map_err(|e| format!("{n}={raw:?}: {e}"))
        };
        let pn = name("LOSS_PROBABILITY");
        let Some(p_raw) = get(&pn) else {
            for s in ["MEAN_LEN", "LOSS_BAD", "SEED"] {
                if get(&name(s)).is_some() {
                    return Err(format!("{} is set but {pn} is not", name(s)));
                }
            }
            return Ok(None);
        };
        let avg = parse_f64(&pn, p_raw)?;
        let ln = name("MEAN_LEN");
        let len_raw = get(&ln).ok_or_else(|| format!("{pn} is set but {ln} is not"))?;
        let len = parse_f64(&ln, len_raw)?;
        let bn = name("LOSS_BAD");
        let bad = match get(&bn) {
            Some(raw) => parse_f64(&bn, raw)?,
            None => 1.0,
        };
        let sn = name("SEED");
        let seed = match get(&sn) {
            Some(raw) => raw
                .trim()
                .parse::<u64>()
                .map_err(|e| format!("{sn}={raw:?}: {e}"))?,
            None => 0,
        };
        let model = GilbertElliott::from_rate_and_burst(avg, len, bad)
            .map_err(|e| format!("{pn}/{ln}/{bn}: {e}"))?;
        Ok(Some(Self { model, seed }))
    }

    /// [`Self::from_lookup`] over the real process environment.
    pub fn from_env(dir: BurstDirection) -> Result<Option<Self>, String> {
        Self::from_lookup(dir, |k| std::env::var(k).ok())
    }
}
