//! Kernel-level network shaping for the browser e2e path.
//!
//! # Why this exists alongside `netsim`
//!
//! `netsim::NetProfile` shapes the **browserless** harness: it sits in-process
//! between a real `IoBridge` and a real `ClientNet` over a Unix socketpair,
//! and its token bucket is consumed inside `send_to_all_sessions` -- *below*
//! quinn's `datagram_send_buffer`. That is deliberate and documented, but it
//! means the browserless harness cannot apply backpressure to quinn, and it
//! has no browser and no GPU.
//!
//! The browser e2e has the opposite shape: real Chromium, real WebGPU, real
//! xdaemon in a container, real QUIC over a real (local) tailnet -- and no
//! shaping at all. Its RTT is ~0 and its bandwidth is a loopback bridge,
//! which is nothing like the tailnet path a production session runs over.
//!
//! This closes that gap with `tc netem` on the server container's egress
//! interface. It shapes the **real** path, below tsnet and below quinn, so
//! propagation delay and rate limiting produce genuine send-buffer
//! backpressure -- the mechanism that drives `Event::DatagramsUnblocked` and
//! therefore the scheduler's whole drain cadence.
//!
//! Deliberately not a port of `NetSim`: that would mean either depending on
//! `ghostframe-e2e` from `ghostframe-lib` (circular) or reimplementing a
//! token bucket in the send path. `netem` is in the kernel, already available
//! (the image installs `iproute2`, and the e2e containers run privileged),
//! and shapes the handshake too.
//!
//! # What it cannot do
//!
//! `netem` is not seeded, so a shaped browser e2e is not reproducible the way
//! a browserless scene is. Assert on convergence and on invariants, not on
//! byte counts.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// One-way path characteristics applied to the server container's egress.
///
/// Field names mirror `netsim::NetProfile` so a scene characterised in the
/// browserless harness can be described the same way here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NetShape {
    /// One-way propagation delay. `netem delay`.
    pub delay_us: u64,
    /// Delay jitter, applied around `delay_us`. `netem ... jitter`.
    pub jitter_us: u64,
    /// Link rate in **bits** per second. `netem rate`.
    ///
    /// Bits, not bytes -- unlike `netsim::CapTimeline`, whose `bps` parameter
    /// is actually bytes per second (see `docs/specs/bwe-googcc-review.md`'s
    /// measurement traps). Named to match what `tc` accepts so there is no
    /// second unit to get wrong.
    pub rate_bits_per_s: Option<u64>,
    /// Independent per-packet loss, as a percentage. `netem loss`.
    pub loss_pct: f64,
    /// Queue depth in packets before tail-drop. `netem limit`.
    ///
    /// This is the bufferbloat knob: a deep queue trades loss for latency,
    /// which is what a real wireless path does and what
    /// `netsim::Bottleneck::depth_ms` models in the browserless harness.
    pub limit_packets: Option<u32>,
}

impl NetShape {
    /// An unshaped link -- what the browser e2e has today.
    pub fn perfect() -> Self {
        Self {
            delay_us: 0,
            jitter_us: 0,
            rate_bits_per_s: None,
            loss_pct: 0.0,
            limit_packets: None,
        }
    }

    /// A path shaped like the production session that motivated this.
    ///
    /// Measured from that session's server counters: sustained ~580
    /// datagrams/s of ~1130 bytes ≈ 5 Mbps offered into a link whose
    /// `bytes_per_us=6.19` budget was never the binding constraint, with an
    /// `rto_ack_latency_p95_us` of 24,334 (≈24 ms round trip, so ~12 ms one
    /// way). The rate here is deliberately *below* what the first frame wants
    /// so the 2040-tile burst has to queue, which is the condition under
    /// which that session misbehaved.
    pub fn tailnet_like() -> Self {
        Self {
            delay_us: 12_000,
            jitter_us: 2_000,
            rate_bits_per_s: Some(8_000_000),
            loss_pct: 0.0,
            limit_packets: Some(1_000),
        }
    }

    /// `tailnet_like`, plus the loss rate at which the production session's
    /// retransmit path was active (`rto_fired=6739` against 9208 original
    /// passes).
    pub fn tailnet_like_lossy() -> Self {
        Self {
            loss_pct: 1.0,
            ..Self::tailnet_like()
        }
    }

    fn netem_args(&self) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();
        // `delay` must come first; netem rejects `jitter` without it.
        if self.delay_us > 0 {
            a.push("delay".into());
            a.push(format!("{}us", self.delay_us));
            if self.jitter_us > 0 {
                a.push(format!("{}us", self.jitter_us));
            }
        }
        if self.loss_pct > 0.0 {
            a.push("loss".into());
            a.push(format!("{}%", self.loss_pct));
        }
        if let Some(r) = self.rate_bits_per_s {
            a.push("rate".into());
            a.push(format!("{r}bit"));
        }
        if let Some(l) = self.limit_packets {
            a.push("limit".into());
            a.push(l.to_string());
        }
        a
    }

    /// Apply this shape to `container`'s default-route interface.
    ///
    /// Uses `qdisc replace`, so calling it twice re-shapes rather than
    /// failing on an existing qdisc.
    pub fn apply(&self, container: &str) -> Result<()> {
        let args = self.netem_args();
        if args.is_empty() {
            return Self::clear(container);
        }
        let iface = default_iface(container)?;
        let mut cmd = vec![
            "exec".to_string(),
            container.to_string(),
            "tc".to_string(),
            "qdisc".to_string(),
            "replace".to_string(),
            "dev".to_string(),
            iface.clone(),
            "root".to_string(),
            "netem".to_string(),
        ];
        cmd.extend(args.iter().cloned());
        run_docker(&cmd).with_context(|| {
            format!("apply netem to {container}:{iface} with args {args:?}")
        })?;
        Ok(())
    }

    /// Remove any shaping. Safe to call when none is applied.
    pub fn clear(container: &str) -> Result<()> {
        let iface = default_iface(container)?;
        // `del` errors when no qdisc is present; that is not a failure here.
        let _ = run_docker(&[
            "exec", container, "tc", "qdisc", "del", "dev", &iface, "root",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>());
        Ok(())
    }

    /// Read back the qdisc, so a test can prove shaping is actually in place
    /// rather than assuming `apply` took effect. A shaped scene that silently
    /// ran unshaped would report the *absence* of a problem it never created.
    pub fn verify(container: &str) -> Result<String> {
        let iface = default_iface(container)?;
        run_docker(
            &["exec", container, "tc", "qdisc", "show", "dev", &iface]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
    }
}

/// The container's default-route interface. Not hardcoded to `eth0`: the
/// compose network could name it otherwise, and shaping the wrong device
/// fails silently by doing nothing.
fn default_iface(container: &str) -> Result<String> {
    let out = run_docker(
        &["exec", container, "sh", "-c", "ip route show default"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    )?;
    out.split_whitespace()
        .skip_while(|t| *t != "dev")
        .nth(1)
        .map(str::to_string)
        .with_context(|| format!("no default route in {container}: {out:?}"))
}

fn run_docker(args: &[String]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .output()
        .context("spawn docker")?;
    if !out.status.success() {
        bail!(
            "docker {args:?} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_produces_no_netem_args() {
        assert!(NetShape::perfect().netem_args().is_empty());
    }

    #[test]
    fn delay_precedes_jitter_and_jitter_needs_delay() {
        let s = NetShape {
            delay_us: 12_000,
            jitter_us: 2_000,
            ..NetShape::perfect()
        };
        assert_eq!(s.netem_args(), vec!["delay", "12000us", "2000us"]);

        // Jitter without delay must not emit a bare jitter token -- netem
        // rejects it, and the failure would surface as an unshaped link.
        let j = NetShape {
            delay_us: 0,
            jitter_us: 2_000,
            ..NetShape::perfect()
        };
        assert!(j.netem_args().is_empty());
    }

    #[test]
    fn rate_is_bits_matching_tc_not_netsim_bytes() {
        let s = NetShape {
            rate_bits_per_s: Some(8_000_000),
            ..NetShape::perfect()
        };
        assert_eq!(s.netem_args(), vec!["rate", "8000000bit"]);
    }

    #[test]
    fn the_production_shape_is_delayed_and_rate_limited() {
        let a = NetShape::tailnet_like().netem_args();
        assert!(a.contains(&"delay".to_string()), "{a:?}");
        assert!(a.contains(&"rate".to_string()), "{a:?}");
        assert!(a.contains(&"limit".to_string()), "{a:?}");
        // No loss in the base shape: the lossy variant is separate so a test
        // can tell a queueing problem from a retransmission one.
        assert!(!a.contains(&"loss".to_string()), "{a:?}");
        assert!(NetShape::tailnet_like_lossy()
            .netem_args()
            .contains(&"loss".to_string()));
    }
}
