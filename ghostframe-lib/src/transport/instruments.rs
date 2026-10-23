//! Transport instruments for the `cumulative emit` log line and the
//! browserless harness. MEASUREMENT ONLY: nothing here is read by a
//! scheduling, pacing, loss or congestion decision, and nothing changes a
//! byte on the wire. These are the readings the D1 redesign is judged
//! against (offered load by class against quinn's departures, FIFO depth,
//! enqueue-to-departure lag, which term bound the budget).
//!
//! Everything is bounded and O(1) per datagram or per turn.

use crate::transport::reliable_emitter::emission_queue::WireClass;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Offered bytes by class
// ---------------------------------------------------------------------------

/// Wire bytes handed to quinn, by what they were for.
///
/// Billed at the moment a datagram is accepted by quinn's send queue (not
/// when the scheduler or emitter produced it), and billed at wire size: the
/// datagram as given to the transport, plus the WebTransport quarter-stream
/// id varint in `overhead`. So `total()` equals the session's
/// `WebTransportServer::datagram_bytes_enqueued` exactly, and a class that
/// goes unbilled makes that identity fail.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OfferedBytes {
    pub bytes: [u64; WireClass::COUNT],
    pub datagrams: [u64; WireClass::COUNT],
    /// WebTransport framing added in front of every datagram.
    pub overhead: u64,
}

impl OfferedBytes {
    pub fn bill(&mut self, class: WireClass, datagram_len: usize, framing_len: usize) {
        let i = class.index();
        self.bytes[i] += datagram_len as u64;
        self.datagrams[i] += 1;
        self.overhead += framing_len as u64;
    }

    pub fn of(&self, class: WireClass) -> u64 {
        self.bytes[class.index()]
    }

    /// Everything billed: every class plus framing overhead.
    pub fn total(&self) -> u64 {
        self.bytes.iter().sum::<u64>() + self.overhead
    }
}

// ---------------------------------------------------------------------------
// Enqueue -> departure lag
// ---------------------------------------------------------------------------

/// Most entries the departure mirror holds. The FIFO itself is bounded by
/// quinn's send buffer; this is a backstop so a quinn behaviour this mirror
/// does not model (see [`DepartureMirror`]) cannot grow memory without bound.
const MIRROR_CAP: usize = 1 << 18;

/// Log-linear histogram of durations in microseconds: exact below 4 us, then
/// four sub-buckets per octave (a reading is within 25 % of the truth).
/// Fixed size, O(1) record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LagHistogram {
    buckets: [u64; Self::BUCKETS],
    count: u64,
    max_us: u64,
}

impl Default for LagHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; Self::BUCKETS],
            count: 0,
            max_us: 0,
        }
    }
}

impl LagHistogram {
    const BUCKETS: usize = 160;

    fn index(us: u64) -> usize {
        if us < 4 {
            return us as usize;
        }
        let o = 63 - us.leading_zeros() as usize; // floor(log2), >= 2
        let sub = ((us >> (o - 2)) & 3) as usize;
        (4 + (o - 2) * 4 + sub).min(Self::BUCKETS - 1)
    }

    /// Upper edge (exclusive) of bucket `i`, in microseconds.
    fn upper_us(i: usize) -> u64 {
        if i < 4 {
            return i as u64 + 1;
        }
        let o = (i - 4) / 4 + 2;
        let sub = ((i - 4) % 4) as u64;
        (5 + sub) << (o - 2)
    }

    pub fn record(&mut self, d: Duration) {
        let us = d.as_micros().min(u64::MAX as u128) as u64;
        self.buckets[Self::index(us)] += 1;
        self.count += 1;
        self.max_us = self.max_us.max(us);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn max_us(&self) -> u64 {
        self.max_us
    }

    /// The `q`-quantile (0..=1) as the upper edge of the bucket holding it,
    /// capped at the exact maximum. Zero with no samples.
    pub fn quantile_us(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = ((self.count as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= target {
                return Self::upper_us(i).min(self.max_us.max(1));
            }
        }
        self.max_us
    }
}

/// A mirror of quinn's datagram FIFO that stamps each datagram when it
/// leaves it. OBSERVABILITY ONLY: nothing may read it for behaviour.
///
/// quinn's queue is strictly FIFO and `outgoing_total` falls by a datagram's
/// length at exactly one point that matters here: `DatagramState::write`,
/// which pops the head and decrements in the same call, immediately before
/// encoding the frame into the packet being built
/// (quinn-proto 0.11.14 `connection/datagrams.rs:162-185`: `pop_front` at
/// :167, put back untouched with `push_front` at :175 when it does not fit,
/// `outgoing_total -= len` at :181). It is called from the DATAGRAM section of
/// `populate_packet` (`connection/mod.rs:3337`). The count rises only in
/// `send` (`datagrams.rs:55`, after the `Blocked` check), so
/// `cum_departed = cum_enqueued - outgoing_total` is the byte count that has
/// been written into packets.
///
/// Two other places also lower the count without sending: `drop_oversized`
/// (`datagrams.rs:143-160`, on an MTU black hole, `mod.rs:1773`) and the
/// `drop = true` branch of `send` (`datagrams.rs:38-48`; this crate always
/// passes `drop = false`). A datagram discarded by `drop_oversized` is read
/// here as departed, so a lag can be stamped early in that case. It is rare
/// and bounded, and only the lag histogram sees it.
///
/// Granularity is one poll: every datagram that left during one
/// `drain_outbound` gets that drain's stamp.
#[derive(Debug, Default)]
pub struct DepartureMirror {
    /// `(cum_end, enqueued_at)`, oldest first. `cum_end` is the session's
    /// cumulative enqueued byte count after this datagram was accepted.
    q: VecDeque<(u64, Instant)>,
    pub lag: LagHistogram,
    /// Entries dropped unstamped because the mirror hit [`MIRROR_CAP`].
    pub overflowed: u64,
    #[cfg(test)]
    pub(crate) stamped: Vec<(Instant, Instant)>,
}

impl DepartureMirror {
    pub fn len(&self) -> usize {
        self.q.len()
    }

    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    pub fn clear(&mut self) {
        self.q.clear();
    }

    /// A datagram was accepted by quinn; `cum_enqueued` is the session's
    /// cumulative accepted byte count including it.
    pub fn on_enqueue(&mut self, cum_enqueued: u64, now: Instant) {
        if self.q.len() >= MIRROR_CAP {
            self.q.pop_front();
            self.overflowed += 1;
        }
        self.q.push_back((cum_enqueued, now));
    }

    /// After a `poll_transmit` batch: stamp everything that has left.
    /// `outgoing_total` is quinn's queued byte count
    /// (`datagram_send_buffer_size - send_buffer_space()`).
    pub fn on_poll(&mut self, cum_enqueued: u64, outgoing_total: u64, now: Instant) {
        let departed = cum_enqueued.saturating_sub(outgoing_total);
        while let Some(&(cum_end, enq)) = self.q.front() {
            if cum_end > departed {
                break;
            }
            self.q.pop_front();
            self.lag.record(now.saturating_duration_since(enq));
            #[cfg(test)]
            if self.stamped.len() < 4096 {
                self.stamped.push((enq, now));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// FIFO depth series
// ---------------------------------------------------------------------------

const SLICE_CAP: usize = 64;

#[derive(Debug, Clone, Copy)]
struct Slice {
    /// Slice index: covers `[idx * width, (idx + 1) * width)` microseconds
    /// since the first sample.
    idx: u64,
    n: f64,
    st: f64,
    sd: f64,
    std: f64,
    stt: f64,
}

/// FIFO depth readings, one per loop turn after the drain: max, a
/// time-weighted mean (each reading is held until the next) and a least
/// squares slope over the last quarter of the observed time.
///
/// Bounded: the slope is kept as regression sums in at most [`SLICE_CAP`]
/// time slices whose width doubles (merging neighbours) when they run out, so
/// memory is constant however long the session runs, and a record is O(1)
/// amortised.
#[derive(Debug, Clone)]
pub struct FifoSeries {
    t0: Option<Instant>,
    last: Option<(Instant, u64)>,
    samples: u64,
    nonempty: u64,
    /// Microseconds the FIFO stood non-empty, each reading held until the next.
    nonempty_us: u128,
    max: u64,
    weighted_byte_us: u128,
    width_us: u64,
    slices: Vec<Slice>,
}

impl Default for FifoSeries {
    fn default() -> Self {
        Self {
            t0: None,
            last: None,
            samples: 0,
            nonempty: 0,
            nonempty_us: 0,
            max: 0,
            weighted_byte_us: 0,
            width_us: 1_000,
            slices: Vec::with_capacity(SLICE_CAP + 1),
        }
    }
}

impl FifoSeries {
    pub fn record(&mut self, now: Instant, depth: u64) {
        let t0 = *self.t0.get_or_insert(now);
        if let Some((tp, dp)) = self.last {
            let dt = now.saturating_duration_since(tp).as_micros();
            self.weighted_byte_us += dp as u128 * dt;
            if dp > 0 {
                self.nonempty_us += dt;
            }
        }
        self.last = Some((now, depth));
        self.samples += 1;
        if depth > 0 {
            self.nonempty += 1;
        }
        self.max = self.max.max(depth);

        let t_us = now.saturating_duration_since(t0).as_micros() as u64;
        let idx = t_us / self.width_us;
        let t = t_us as f64 / 1e6;
        let d = depth as f64;
        match self.slices.last_mut() {
            Some(s) if s.idx == idx => {
                s.n += 1.0;
                s.st += t;
                s.sd += d;
                s.std += t * d;
                s.stt += t * t;
            }
            _ => {
                self.slices.push(Slice {
                    idx,
                    n: 1.0,
                    st: t,
                    sd: d,
                    std: t * d,
                    stt: t * t,
                });
                if self.slices.len() > SLICE_CAP {
                    self.coarsen();
                }
            }
        }
    }

    /// Double the slice width and merge neighbours that now share an index.
    fn coarsen(&mut self) {
        self.width_us *= 2;
        let mut out: Vec<Slice> = Vec::with_capacity(SLICE_CAP + 1);
        for s in self.slices.drain(..) {
            let idx = s.idx / 2;
            match out.last_mut() {
                Some(o) if o.idx == idx => {
                    o.n += s.n;
                    o.st += s.st;
                    o.sd += s.sd;
                    o.std += s.std;
                    o.stt += s.stt;
                }
                _ => out.push(Slice { idx, ..s }),
            }
        }
        self.slices = out;
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Readings with a non-empty FIFO: turns that ended quinn-limited. That
    /// is cwnd-limited OR held back by quinn's own pacer: both leave datagrams
    /// queued (`connection/mod.rs:608` and `:617-625` both set
    /// `congestion_blocked`), and this cannot tell them apart.
    pub fn nonempty(&self) -> u64 {
        self.nonempty
    }

    /// Microseconds the FIFO stood non-empty (the time-weighted counterpart
    /// of [`Self::nonempty`], with the same reading-held-until-next rule as
    /// the mean).
    pub fn nonempty_us(&self) -> u64 {
        self.nonempty_us.min(u64::MAX as u128) as u64
    }

    pub fn max(&self) -> u64 {
        self.max
    }

    /// Microseconds between the first and last reading.
    pub fn window_us(&self) -> u64 {
        match (self.t0, self.last) {
            (Some(t0), Some((tl, _))) => tl.saturating_duration_since(t0).as_micros() as u64,
            _ => 0,
        }
    }

    pub fn time_weighted_mean(&self) -> f64 {
        let w = self.window_us();
        if w == 0 {
            return 0.0;
        }
        self.weighted_byte_us as f64 / w as f64
    }

    /// Bytes per second, least squares over readings in the last 25 % of the
    /// observed time (to slice granularity: at least eight slices once the
    /// slice table has filled). Zero when fewer than two readings or no
    /// spread in time.
    pub fn slope_last_quarter(&self) -> f64 {
        let w = self.window_us();
        let from_us = w - w / 4;
        let (mut n, mut st, mut sd, mut std, mut stt) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for s in self
            .slices
            .iter()
            .filter(|s| s.idx * self.width_us >= from_us)
        {
            n += s.n;
            st += s.st;
            sd += s.sd;
            std += s.std;
            stt += s.stt;
        }
        let denom = n * stt - st * st;
        if n < 2.0 || denom.abs() < 1e-12 {
            return 0.0;
        }
        (n * std - st * sd) / denom
    }
}

// ---------------------------------------------------------------------------
// Losses already reported to goog_cc
// ---------------------------------------------------------------------------

/// Most flushed-loss `wire_seq`s remembered: twice the ledger's tombstone
/// capacity (`TRANSMISSION_LEDGER_CAPACITY`, 16 384), so a late
/// acknowledgement the ledger can still classify as late finds its loss here
/// if it was reported.
const FLUSHED_CAP: usize = 32_768;

/// The `wire_seq`s of losses already handed to goog_cc, newest
/// [`FLUSHED_CAP`] only. Lets a late acknowledgement be classified as
/// "arrived after the estimator was told" in O(1). Observability only.
#[derive(Debug, Default)]
pub struct FlushedLosses {
    set: std::collections::HashSet<u32>,
    order: VecDeque<u32>,
}

impl FlushedLosses {
    pub fn insert(&mut self, wire_seq: u32) {
        if self.set.insert(wire_seq) {
            self.order.push_back(wire_seq);
            while self.order.len() > FLUSHED_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.set.remove(&old);
                }
            }
        }
    }

    /// True (and forgets it) when `wire_seq` was reported as lost.
    pub fn take(&mut self, wire_seq: u32) -> bool {
        // `order` keeps a stale entry; it is trimmed by the cap, and removing
        // an absent key from the set on trim is a no-op.
        self.set.remove(&wire_seq)
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Budget-binding term
// ---------------------------------------------------------------------------

/// Which input set the scheduler's per-tick budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetTerm {
    /// The `cwnd / srtt` bandwidth term.
    Cwnd,
    /// goog_cc's pacer rate was the smaller of the two.
    Pacer,
    /// The AIMD multiplier (`tick_budget_multiplier` < 1, backed off by send
    /// errors) cut the budget below the unscaled base.
    Aimd,
    /// The one-datagram floor beat the bandwidth term.
    Floor,
    /// A probe window's target exceeded the paced budget.
    Probe,
}

/// Ticks counted by binding term, plus how many were then cut by quinn's
/// send-buffer clamp (a separate, later stage; the clamp can bind on top of
/// any of the four).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BudgetBinding {
    pub cwnd: u64,
    pub pacer: u64,
    pub floor: u64,
    pub probe: u64,
    pub aimd: u64,
    pub sendbuf_clamped: u64,
    /// Ticks where retransmission replayed since the last tick was
    /// subtracted from the budget (`RETRANSMIT_BUDGET_SHARE` give-back), and
    /// the bytes subtracted.
    pub giveback_ticks: u64,
    pub giveback_bytes: u64,
}

impl BudgetBinding {
    pub fn note(&mut self, term: BudgetTerm, clamped: bool, give_back: usize) {
        if give_back > 0 {
            self.giveback_ticks += 1;
            self.giveback_bytes += give_back as u64;
        }
        match term {
            BudgetTerm::Aimd => self.aimd += 1,
            BudgetTerm::Cwnd => self.cwnd += 1,
            BudgetTerm::Pacer => self.pacer += 1,
            BudgetTerm::Floor => self.floor += 1,
            BudgetTerm::Probe => self.probe += 1,
        }
        if clamped {
            self.sendbuf_clamped += 1;
        }
    }

    pub fn ticks(&self) -> u64 {
        self.cwnd + self.pacer + self.floor + self.probe + self.aimd
    }
}

/// Classify a tick. Pure; the arguments are values the caller already
/// computed to make its decision.
///
/// - `probe_binds`: a probe window was open and its budget beat the paced one.
/// - `pacer_budget`: the pacer's per-tick bytes when the pacer is trusted.
/// - `aimd_budget`: the budget before the pacer min, i.e. the unscaled base
///   times the AIMD multiplier.
/// - `unscaled_base`: `base_budget_bytes()` before the multiplier.
/// - `cwnd_term` / `floor`: the two operands of `base_budget_bytes`'s `max`.
pub fn classify_budget_term(
    probe_binds: bool,
    pacer_budget: Option<usize>,
    aimd_budget: usize,
    unscaled_base: usize,
    cwnd_term: usize,
    floor: usize,
) -> BudgetTerm {
    if probe_binds {
        BudgetTerm::Probe
    } else if pacer_budget.is_some_and(|p| p < aimd_budget) {
        BudgetTerm::Pacer
    } else if aimd_budget < unscaled_base {
        BudgetTerm::Aimd
    } else if cwnd_term <= floor {
        BudgetTerm::Floor
    } else {
        BudgetTerm::Cwnd
    }
}

// ---------------------------------------------------------------------------
// ECN
// ---------------------------------------------------------------------------

/// What quinn put on its transmits, across ALL connections (the count is taken
/// in `drain_outbound`, which polls every connection; production runs one).
/// quinn abandoning ECN is fixed by construction here, not a finding about the
/// path: inbound ECN is hard-wired to `None` (`handle_datagram(.., None, ..)`)
/// and `encode_frame_append` carries no ECN codepoint either way, so the peer
/// can never echo counts and the first ACK without them disables ECN.
/// quinn-proto 0.11.14 keeps its ECN
/// validation state in `PathData::sending_ecn` (`connection/paths.rs:20`),
/// which is `pub(super)`; the only accessor, `Connection::using_ecn`
/// (`connection/mod.rs:3632`), is `#[cfg(test)] pub(crate)`. Nothing public
/// exposes the state, and `ConnectionStats` carries no ECN counters. What is
/// observable is `Transmit::ecn`: quinn marks transmits ECT(0) while it
/// believes ECN works (it starts true, `paths.rs:73`) and sends them unmarked
/// once validation has failed or been abandoned.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EcnObservation {
    /// Transmits quinn asked to be sent as ECT(0).
    pub ect0_tx: u64,
    /// Transmits with no ECN codepoint.
    pub unmarked_tx: u64,
    /// Whether the most recent transmit was marked: `true` means quinn still
    /// considered ECN usable at the last poll, `false` that it had stopped
    /// (validation failed, or the peer's ACKs carried no ECN counts) or had
    /// not sent anything yet.
    pub last_marked: bool,
}

impl EcnObservation {
    pub fn note(&mut self, marked: bool) {
        if marked {
            self.ect0_tx += 1;
        } else {
            self.unmarked_tx += 1;
        }
        self.last_marked = marked;
    }
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// A plain-data snapshot of every instrument, for the log line and the
/// harness result. `Copy` so it can ride in `TransportProbe`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InstrumentSummary {
    /// Microseconds between the first and last FIFO reading: the divisor for
    /// every "per second" figure.
    pub window_us: u64,
    pub fifo_samples: u64,
    /// Largest reading taken after a drain (one per loop turn).
    pub fifo_depth_max: u64,
    /// Largest depth seen at the moment a datagram was accepted.
    pub fifo_depth_peak_at_enqueue: u64,
    /// Time-weighted mean depth, bytes.
    pub fifo_depth_mean: u64,
    /// Least-squares slope over the last quarter of the window, bytes/s.
    pub fifo_depth_slope_last_quarter_bps: i64,
    /// Turns that ended with a non-empty FIFO: quinn-limited, meaning
    /// cwnd-limited OR held by quinn's pacer (not distinguishable).
    pub quinn_limited_turns: u64,
    /// Microseconds the FIFO stood non-empty (time-weighted counterpart).
    pub quinn_limited_us: u64,
    pub offered: OfferedBytes,
    /// Bytes quinn accepted into its datagram queue since the live session
    /// began (the session counter minus its value at HELLO).
    pub quinn_bytes_enqueued: u64,
    /// `quinn_bytes_enqueued` minus what was still queued at the last reading.
    pub quinn_bytes_departed: u64,
    pub lag_samples: u64,
    pub lag_p50_us: u64,
    pub lag_p95_us: u64,
    pub lag_max_us: u64,
    /// Entries the departure mirror still held at the last reading.
    pub mirror_len: u64,
    pub mirror_overflowed: u64,
    pub budget: BudgetBinding,
    /// Ledger losses (declared lost, reported to goog_cc or retracted).
    pub ledger_losses_declared: u64,
    /// Of those, handed to goog_cc.
    pub ledger_losses_flushed: u64,
    /// Declared lost, then dropped unreported because the pending list hit
    /// its cap (oldest first).
    pub ledger_losses_overflowed: u64,
    /// Declared lost, then acknowledged while still waiting to be reported:
    /// withdrawn, goog_cc never heard of them.
    pub late_acked_before_flush: u64,
    /// Declared lost, already reported to goog_cc, then acknowledged: the
    /// estimator was told about a loss that did not happen.
    pub late_acked_after_flush: u64,
    /// Late acknowledgements of losses that were neither retracted nor found
    /// among the reported ones (dropped by the pending cap, or reported so
    /// long ago the record aged out). goog_cc never heard of the former.
    /// A LOWER BOUND: the ledger keeps declared-lost entries in a ring of
    /// 16 384 tombstones, and a late acknowledgement for an evicted one
    /// resolves to nothing, so it appears only in `ledger_unknown_acks`.
    pub late_acked_unreported: u64,
    /// Acknowledgements naming a `wire_seq` the ledger knows nothing of
    /// (duplicates, or declared lost and since evicted from the tombstones).
    pub ledger_unknown_acks: u64,
    /// Tombstones evicted by newer ones; each is a loss whose late
    /// acknowledgement can no longer be classified.
    pub ledger_tombstone_evictions: u64,
    pub ecn: EcnObservation,
}

impl InstrumentSummary {
    /// A byte total as bytes per second over the observation window.
    pub fn per_second(&self, bytes: u64) -> f64 {
        if self.window_us == 0 {
            0.0
        } else {
            bytes as f64 * 1e6 / self.window_us as f64
        }
    }

    /// Fraction of turns that ended with queued datagrams (quinn-limited).
    pub fn quinn_limited_turn_fraction(&self) -> f64 {
        if self.fifo_samples == 0 {
            0.0
        } else {
            self.quinn_limited_turns as f64 / self.fifo_samples as f64
        }
    }

    /// Fraction of the window the FIFO stood non-empty.
    pub fn quinn_limited_time_fraction(&self) -> f64 {
        if self.window_us == 0 {
            0.0
        } else {
            self.quinn_limited_us as f64 / self.window_us as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(base: Instant, us: u64) -> Instant {
        base + Duration::from_micros(us)
    }

    #[test]
    fn histogram_quantiles_bracket_the_truth() {
        let mut h = LagHistogram::default();
        for us in 1..=1000u64 {
            h.record(Duration::from_micros(us));
        }
        let p50 = h.quantile_us(0.5);
        let p95 = h.quantile_us(0.95);
        assert!((500..=640).contains(&p50), "p50 {p50}");
        assert!((950..=1000).contains(&p95), "p95 {p95}");
        assert_eq!(h.max_us(), 1000);
        assert_eq!(LagHistogram::default().quantile_us(0.5), 0);
    }

    #[test]
    fn departure_stamps_are_monotone_and_the_mirror_drains() {
        let base = Instant::now();
        let mut m = DepartureMirror::default();
        // Ten 100-byte datagrams accepted 1 ms apart.
        let mut cum = 0u64;
        for i in 0..10u64 {
            cum += 100;
            m.on_enqueue(cum, t(base, i * 1_000));
        }
        assert_eq!(m.len(), 10);
        // Nothing has left: queue still holds all 1000 bytes.
        m.on_poll(cum, 1000, t(base, 10_000));
        assert_eq!(m.len(), 10);
        // Three leave (queued 700), then a partial 50 bytes more (no whole
        // datagram), then everything.
        m.on_poll(cum, 700, t(base, 11_000));
        assert_eq!(m.len(), 7);
        m.on_poll(cum, 650, t(base, 12_000));
        assert_eq!(m.len(), 7, "a datagram leaves only when all of it has");
        m.on_poll(cum, 0, t(base, 13_000));
        assert!(m.is_empty(), "the mirror drains when quinn's queue does");
        assert_eq!(m.lag.count(), 10);
        // Stamps in FIFO order never go backwards, and each datagram departs
        // no earlier than it was enqueued.
        let departures: Vec<Instant> = m.stamped.iter().map(|&(_, d)| d).collect();
        assert!(departures.windows(2).all(|w| w[0] <= w[1]));
        assert!(m.stamped.iter().all(|&(e, d)| e <= d));
        let enqueues: Vec<Instant> = m.stamped.iter().map(|&(e, _)| e).collect();
        assert!(enqueues.windows(2).all(|w| w[0] <= w[1]));
        // First datagram: enqueued at 0, left at 11 ms.
        assert_eq!(
            m.stamped[0].1 - m.stamped[0].0,
            Duration::from_micros(11_000)
        );
    }

    #[test]
    fn mirror_is_bounded() {
        let base = Instant::now();
        let mut m = DepartureMirror::default();
        for i in 0..(MIRROR_CAP as u64 + 10) {
            m.on_enqueue(i + 1, base);
        }
        assert_eq!(m.len(), MIRROR_CAP);
        assert_eq!(m.overflowed, 10);
    }

    #[test]
    fn fifo_series_mean_max_and_slope() {
        let base = Instant::now();
        let mut s = FifoSeries::default();
        // 0 for 1 s, then a ramp of 1000 B/s for 1 s sampled every 10 ms.
        for i in 0..=200u64 {
            let us = i * 10_000;
            let depth = if us <= 1_000_000 {
                0
            } else {
                (us - 1_000_000) / 1_000
            };
            s.record(t(base, us), depth);
        }
        assert_eq!(s.max(), 1000);
        assert_eq!(s.samples(), 201);
        let slope = s.slope_last_quarter();
        assert!((900.0..=1100.0).contains(&slope), "slope {slope}");
        let mean = s.time_weighted_mean();
        assert!((200.0..=300.0).contains(&mean), "mean {mean}");
        assert!(s.nonempty() > 0 && s.nonempty() < s.samples());
    }

    #[test]
    fn fifo_series_memory_is_bounded_over_a_long_session() {
        let base = Instant::now();
        let mut s = FifoSeries::default();
        for i in 0..100_000u64 {
            s.record(t(base, i * 5_000), i % 7);
        }
        assert!(s.slices.len() <= SLICE_CAP);
        assert!(s.slices.len() >= SLICE_CAP / 2 - 1, "{}", s.slices.len());
    }

    #[test]
    fn classify_prefers_probe_then_pacer_then_floor() {
        use BudgetTerm::*;
        assert_eq!(
            classify_budget_term(true, Some(1), 100, 100, 100, 10),
            Probe
        );
        assert_eq!(
            classify_budget_term(false, Some(50), 100, 100, 100, 10),
            Pacer
        );
        assert_eq!(
            classify_budget_term(false, Some(150), 100, 100, 100, 10),
            Cwnd
        );
        // The multiplier cut a base that the bandwidth term had set.
        assert_eq!(classify_budget_term(false, None, 50, 100, 100, 10), Aimd);
        // Floor wins when the unscaled base is the floor.
        assert_eq!(classify_budget_term(false, None, 10, 10, 5, 10), Floor);
        assert_eq!(classify_budget_term(false, None, 100, 100, 100, 10), Cwnd);
    }

    #[test]
    fn flushed_losses_are_taken_once_and_bounded() {
        let mut f = FlushedLosses::default();
        f.insert(7);
        f.insert(7);
        assert!(f.take(7));
        assert!(!f.take(7));
        assert!(!f.take(8));
        for i in 0..(FLUSHED_CAP as u32 + 100) {
            f.insert(i);
        }
        assert_eq!(f.len(), FLUSHED_CAP);
        assert!(!f.take(0), "the oldest aged out");
        assert!(f.take(FLUSHED_CAP as u32 + 99));
    }

    #[test]
    fn offered_total_includes_every_class_and_the_overhead() {
        let mut o = OfferedBytes::default();
        for (i, c) in WireClass::ALL.iter().enumerate() {
            o.bill(*c, 100 + i, 2);
        }
        assert_eq!(
            o.total(),
            (0..WireClass::COUNT as u64).map(|i| 100 + i).sum::<u64>()
                + 2 * WireClass::COUNT as u64
        );
    }
}
