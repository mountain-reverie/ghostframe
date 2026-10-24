//! F2: the departure tracker. Tells the rest of the transport when each
//! datagram left quinn's send FIFO for the wire.
//!
//! # What quinn guarantees (quinn-proto 0.11.14, the version in `Cargo.lock`)
//!
//! - The queue is strictly FIFO. `Datagrams::send` appends with `push_back`
//!   (`connection/datagrams.rs:56`); `DatagramState::write` takes the head
//!   with `pop_front` (`:167`) and, when it does not fit the packet being
//!   built, puts it back with `push_front` (`:175`) and stops, so a small
//!   datagram never overtakes a large one that did not fit ("Future work: we
//!   could be more clever about cramming small datagrams", `:172-174`).
//! - `outgoing_total` falls by a datagram's length in exactly one place that
//!   matters, in the same call as the `pop_front`, immediately before the
//!   frame is encoded into the packet (`:181`). That call sits in
//!   `populate_packet`'s DATAGRAM section (`connection/mod.rs:3334-3345`),
//!   which `poll_transmit` calls at `:881` and then finishes the packet
//!   (`finish_and_track`, `:927`); nothing abandons a packet between the two.
//!   So the datagrams a `poll_transmit` batch wrote are exactly the ones
//!   whose bytes left `outgoing_total` during it.
//! - It rises only in `send` (`datagrams.rs:55`), after the `Blocked` check.
//!
//! So with `cum_enqueued` the session's cumulative accepted byte count,
//! `cum_departed = cum_enqueued - outgoing_total` is the byte count that has
//! been written into packets, and a datagram whose `cum_end <= cum_departed`
//! has departed.
//!
//! Two places lower the count without a departure, and neither is reachable
//! by the server in normal operation:
//!
//! - `send(.., drop = true)` pops the front unsent to make room
//!   (`datagrams.rs:38-48`). This crate always passes `drop = false`
//!   (`WebTransportServer::send_datagram`), so a full buffer is `Blocked`,
//!   and a `Blocked` datagram is never mirrored because it was never queued.
//! - `drop_oversized` discards queued datagrams after an MTU black hole
//!   (`datagrams.rs:147-160`, `connection/mod.rs:1773`). The dropped
//!   datagrams read as departed, and so do any behind them up to the dropped
//!   byte count: a stamp can be early, by at most one FIFO's worth, and only
//!   on a black hole. A datagram stamped here that did not leave is then
//!   never acknowledged, which is the correct verdict for it.
//!
//! The unit test below runs against a real `Connection` and cross-checks the
//! mirror against quinn's own `frame_tx.datagram` count, which counts frames
//! written into packets and is independent of `outgoing_total`.
//!
//! # Granularity
//!
//! One poll: every datagram that left during one `drain_outbound` shares that
//! drain's stamp. quinn paces inside a turn, so this is at most about one
//! pacing interval coarse.
//!
//! # Cost
//!
//! Amortised O(1) per datagram. The deque is bounded by the FIFO's depth,
//! which admission bounds; [`TRACKER_CAP`] is a backstop only.

use crate::transport::instruments::LagHistogram;
use std::collections::VecDeque;
use std::time::Instant;

/// Most entries the tracker holds. The FIFO itself is bounded by quinn's send
/// buffer and, far below that, by admission; this is a backstop so a quinn
/// behaviour the module comment does not model cannot grow memory without
/// bound. An entry pushed out by it is handed back to the caller to be
/// treated as departed, never dropped silently.
pub(crate) const TRACKER_CAP: usize = 1 << 18;

#[derive(Debug, Clone, Copy)]
struct Entry {
    /// The session's cumulative accepted byte count including this datagram.
    cum_end: u64,
    enqueued_at: Instant,
    /// The transmission this datagram carries, if it is one the ledger
    /// tracks. Datagrams that are not (H.264, control, parity) are mirrored
    /// all the same so the FIFO order is complete, but name no transmission.
    wire_seq: Option<u32>,
}

/// A mirror of quinn's datagram FIFO that stamps each datagram when it
/// leaves it.
#[derive(Debug, Default)]
pub struct DepartureTracker {
    q: VecDeque<Entry>,
    /// Enqueue-to-departure lag of everything stamped.
    pub lag: LagHistogram,
    /// Entries pushed out unstamped because the tracker hit [`TRACKER_CAP`].
    pub overflowed: u64,
    /// The byte count that had departed at the last poll.
    cum_departed: u64,
    /// Datagrams stamped so far.
    stamped_total: u64,
    #[cfg(test)]
    pub(crate) stamped: Vec<(Instant, Instant)>,
}

impl DepartureTracker {
    pub fn len(&self) -> usize {
        self.q.len()
    }

    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    /// Datagrams stamped so far.
    pub fn stamped_total(&self) -> u64 {
        self.stamped_total
    }

    /// The byte count that had departed at the last [`Self::on_poll`].
    pub fn cum_departed(&self) -> u64 {
        self.cum_departed
    }

    /// A datagram was accepted by quinn; `cum_enqueued` is the session's
    /// cumulative accepted byte count including it.
    ///
    /// Returns the `wire_seq` of an entry the cap pushed out, if it named one.
    /// The caller must treat that transmission as departed (it left, or
    /// quinn's queue has been deeper than the cap for a reason this module
    /// does not model); leaving it unstamped would exempt it from the loss
    /// clock forever.
    #[must_use]
    pub fn on_enqueue(
        &mut self,
        wire_seq: Option<u32>,
        cum_enqueued: u64,
        now: Instant,
    ) -> Option<u32> {
        let mut evicted = None;
        if self.q.len() >= TRACKER_CAP {
            evicted = self.q.pop_front().and_then(|e| e.wire_seq);
            self.overflowed += 1;
        }
        self.q.push_back(Entry {
            cum_end: cum_enqueued,
            enqueued_at: now,
            wire_seq,
        });
        evicted
    }

    /// After a `poll_transmit` batch: stamp everything that has left.
    /// `outgoing_total` is quinn's queued byte count
    /// (`datagram_send_buffer_size - send_buffer_space()`). `stamp` is called
    /// once per departed datagram, oldest first, with its `wire_seq` (if it
    /// names one) and the instant it was enqueued.
    ///
    /// Returns `cum_departed`, `cum_enqueued - outgoing_total`.
    pub fn on_poll(
        &mut self,
        cum_enqueued: u64,
        outgoing_total: u64,
        now: Instant,
        mut stamp: impl FnMut(Option<u32>, Instant),
    ) -> u64 {
        let departed = cum_enqueued.saturating_sub(outgoing_total);
        self.cum_departed = departed;
        while let Some(&e) = self.q.front() {
            if e.cum_end > departed {
                break;
            }
            self.q.pop_front();
            self.stamped_total += 1;
            self.lag
                .record(now.saturating_duration_since(e.enqueued_at));
            #[cfg(test)]
            if self.stamped.len() < 4096 {
                self.stamped.push((e.enqueued_at, now));
            }
            stamp(e.wire_seq, e.enqueued_at);
        }
        departed
    }

    /// The session ended or changed: hand back every entry still queued, so
    /// the caller can release what hangs off them. They did not necessarily
    /// leave, but nothing will ever stamp them, and a transmission that is
    /// never stamped is never subject to the loss clock.
    pub fn drain_pending(&mut self, mut f: impl FnMut(Option<u32>)) {
        for e in self.q.drain(..) {
            f(e.wire_seq);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::{Bytes, BytesMut};
    use proptest::prelude::*;
    use quinn_proto::{
        ClientConfig, Connection, ConnectionHandle, DatagramEvent, Endpoint, EndpointConfig,
    };
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::transport::quic::QuicServer;

    fn t(base: Instant, us: u64) -> Instant {
        base + Duration::from_micros(us)
    }

    #[test]
    fn stamps_are_monotone_and_the_tracker_drains() {
        let base = Instant::now();
        let mut m = DepartureTracker::default();
        // Ten 100-byte datagrams accepted 1 ms apart.
        let mut cum = 0u64;
        for i in 0..10u64 {
            cum += 100;
            assert_eq!(m.on_enqueue(Some(i as u32), cum, t(base, i * 1_000)), None);
        }
        assert_eq!(m.len(), 10);
        // Nothing has left: queue still holds all 1000 bytes.
        m.on_poll(cum, 1000, t(base, 10_000), |_, _| panic!("nothing left"));
        assert_eq!(m.len(), 10);
        // Three leave (queued 700), then a partial 50 bytes more (no whole
        // datagram), then everything.
        let mut seen = Vec::new();
        m.on_poll(cum, 700, t(base, 11_000), |ws, _| seen.push(ws));
        assert_eq!(seen, vec![Some(0), Some(1), Some(2)]);
        assert_eq!(m.len(), 7);
        m.on_poll(cum, 650, t(base, 12_000), |_, _| panic!("partial"));
        assert_eq!(m.len(), 7, "a datagram leaves only when all of it has");
        m.on_poll(cum, 0, t(base, 13_000), |ws, _| seen.push(ws));
        assert!(m.is_empty(), "the tracker drains when quinn's queue does");
        assert_eq!(seen.len(), 10);
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
    fn the_cap_hands_back_what_it_pushes_out() {
        let base = Instant::now();
        let mut m = DepartureTracker::default();
        let mut pushed = Vec::new();
        for i in 0..(TRACKER_CAP as u64 + 10) {
            if let Some(ws) = m.on_enqueue(Some(i as u32), i + 1, base) {
                pushed.push(ws);
            }
        }
        assert_eq!(m.len(), TRACKER_CAP);
        assert_eq!(m.overflowed, 10);
        assert_eq!(pushed, (0..10).collect::<Vec<u32>>());
    }

    #[test]
    fn drain_pending_returns_every_queued_entry_once() {
        let base = Instant::now();
        let mut m = DepartureTracker::default();
        for i in 0..5u64 {
            let _ = m.on_enqueue((i % 2 == 0).then_some(i as u32), (i + 1) * 10, base);
        }
        let mut got = Vec::new();
        m.drain_pending(|ws| got.push(ws));
        assert_eq!(got, vec![Some(0), None, Some(2), None, Some(4)]);
        assert!(m.is_empty());
    }

    // -----------------------------------------------------------------------
    // Against a real quinn Connection
    // -----------------------------------------------------------------------

    /// A server (the production `QuicServer`) and a client endpoint joined
    /// back to back with no socket, no harness and a clock the test moves.
    struct Pair {
        server: QuicServer,
        server_handle: ConnectionHandle,
        client_ep: Endpoint,
        client: Connection,
        now: Instant,
        saddr: SocketAddr,
        caddr: SocketAddr,
        cap: usize,
    }

    impl Pair {
        fn new(cap: usize) -> Self {
            let mut server = QuicServer::new_with_datagram_send_buffer(cap).expect("server");
            server.set_peer_gate(crate::transport::peer_auth::PeerGate::single_owner_for_tests());
            let mut roots = rustls::RootCertStore::empty();
            roots
                .add(rustls::pki_types::CertificateDer::from(
                    server.cert_info().schedule.serving_cert_der(),
                ))
                .expect("trust the server's own certificate");
            let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
                crate::transport::quic::pq_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
            tls.alpn_protocols = vec![b"h3".to_vec()];
            let quic = quinn_proto::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
            let client_config = ClientConfig::new(Arc::new(quic));
            let mut client_ep = Endpoint::new(
                Arc::new(EndpointConfig::default()),
                None,
                true,
                Some([7u8; 32]),
            );
            let saddr: SocketAddr = "127.0.0.1:4433".parse().unwrap();
            let caddr: SocketAddr = "127.0.0.1:50000".parse().unwrap();
            let now = Instant::now();
            let (_, client) = client_ep
                .connect(now, client_config, saddr, "localhost")
                .expect("connect");
            let mut p = Self {
                server,
                server_handle: ConnectionHandle(0),
                client_ep,
                client,
                now,
                saddr,
                caddr,
                cap,
            };
            for _ in 0..200 {
                p.step_all(Duration::from_millis(2));
                if let Some(h) = p.server.take_accepted().first() {
                    p.server_handle = *h;
                }
                if p.server
                    .connections
                    .get_mut(&p.server_handle)
                    .is_some_and(|c| !c.is_handshaking() && c.datagrams().max_size().is_some())
                    && !p.client.is_handshaking()
                {
                    // Let the handshake's tail (ACKs, HANDSHAKE_DONE) settle.
                    for _ in 0..10 {
                        p.step_all(Duration::from_millis(2));
                    }
                    return p;
                }
            }
            panic!("handshake did not complete");
        }

        fn server_conn(&mut self) -> &mut Connection {
            self.server
                .connections
                .get_mut(&self.server_handle)
                .unwrap()
        }

        /// Advance the clock, run timers, and move every packet both ways
        /// until quiet. Drains the server's FIFO as far as quinn lets it.
        fn step_all(&mut self, dt: Duration) {
            self.now += dt;
            let now = self.now;
            self.server.handle_timeout(now);
            if self.client.poll_timeout().is_some_and(|x| x <= now) {
                self.client.handle_timeout(now);
            }
            for _ in 0..64 {
                let mut moved = false;
                let mut buf = Vec::new();
                while let Some(tx) = self.client.poll_transmit(now, 1, &mut buf) {
                    moved = true;
                    let mut resp = Vec::new();
                    let pkt = BytesMut::from(&buf[..tx.size]);
                    if let Some(r) = self
                        .server
                        .handle_datagram(now, self.caddr, None, tx.ecn, pkt, &mut resp)
                    {
                        let mut out = BytesMut::from(&resp[..r.size]);
                        self.client_receive(now, &mut out);
                    }
                    buf.clear();
                }
                self.server.drain_endpoint_events();
                let mut buf = Vec::new();
                while let Some(tx) = self.server.poll_transmit(now, 1, &mut buf) {
                    moved = true;
                    let mut pkt = BytesMut::from(&buf[..tx.size]);
                    self.client_receive(now, &mut pkt);
                    buf.clear();
                }
                self.drain_client_endpoint();
                if !moved {
                    break;
                }
            }
        }

        fn client_receive(&mut self, now: Instant, pkt: &mut BytesMut) {
            let mut resp = Vec::new();
            if let Some(DatagramEvent::ConnectionEvent(_, ev)) =
                self.client_ep
                    .handle(now, self.saddr, None, None, std::mem::take(pkt), &mut resp)
            {
                self.client.handle_event(ev);
            }
        }

        fn drain_client_endpoint(&mut self) {
            while let Some(ev) = self.client.poll_endpoint_events() {
                // The handle is irrelevant to a one-connection endpoint.
                if let Some(ce) = self.client_ep.handle_event(ConnectionHandle(0), ev) {
                    self.client.handle_event(ce);
                }
            }
            while self.client.poll().is_some() {}
        }

        /// One `drain_outbound`: poll the server until quinn has nothing more
        /// to send now. Returns the raw packets so the caller can choose to
        /// deliver them.
        fn drain_server(&mut self) -> Vec<Vec<u8>> {
            let now = self.now;
            let mut out = Vec::new();
            let mut buf = Vec::new();
            while let Some(tx) = self.server_conn().poll_transmit(now, 1, &mut buf) {
                out.push(buf[..tx.size].to_vec());
                buf.clear();
            }
            out
        }

        fn deliver_to_client(&mut self, pkts: Vec<Vec<u8>>) {
            let now = self.now;
            for p in pkts {
                let mut b = BytesMut::from(&p[..]);
                self.client_receive(now, &mut b);
            }
            self.drain_client_endpoint();
        }

        fn queued(&mut self) -> u64 {
            let cap = self.cap;
            (cap - self.server_conn().datagrams().send_buffer_space()) as u64
        }
    }

    #[derive(Debug, Clone)]
    enum Op {
        /// Offer a datagram of this many bytes to quinn.
        Send(usize),
        /// One `drain_outbound`; the packets are lost.
        DrainLose,
        /// One `drain_outbound`; the packets arrive and are acknowledged
        /// after the clock moves by this many ms.
        DrainDeliver(u8),
        /// Time passes with no I/O.
        Idle(u8),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            6 => (1usize..=1000).prop_map(Op::Send),
            2 => Just(Op::DrainLose),
            3 => (1u8..=40).prop_map(Op::DrainDeliver),
            1 => (1u8..=30).prop_map(Op::Idle),
        ]
    }

    /// The state the property is checked against: only what the test itself
    /// observed, never the tracker's own bookkeeping.
    #[derive(Default)]
    struct Oracle {
        /// Datagram ids accepted by quinn, in order, with their length.
        accepted: Vec<usize>,
        cum_enqueued: u64,
        /// How many times each id was stamped, and when.
        stamps: Vec<Vec<Instant>>,
        /// Ids in the order they were stamped.
        order: Vec<usize>,
    }

    fn poll_and_check(pair: &mut Pair, tr: &mut DepartureTracker, o: &mut Oracle) {
        let now = pair.now;
        let queued = pair.queued();
        let before = tr.stamped_total();
        let cd = tr.on_poll(o.cum_enqueued, queued, now, |ws, _| {
            let id = ws.expect("every mirrored datagram here names an id") as usize;
            o.stamps[id].push(now);
            o.order.push(id);
        });
        assert_eq!(
            cd,
            o.cum_enqueued - queued,
            "cum_departed must equal cum_enqueued - outgoing_total"
        );
        assert!(tr.stamped_total() >= before);
        // Independent oracle 1: quinn counts every DATAGRAM frame it writes
        // into a packet.
        let frames = pair.server_conn().stats().frame_tx.datagram;
        assert_eq!(
            tr.stamped_total(),
            frames,
            "stamped datagrams vs DATAGRAM frames quinn wrote"
        );
        // Independent oracle 2: the stamped prefix, in bytes, is exactly what
        // left quinn's byte counter.
        let stamped_bytes: u64 = o.order.iter().map(|&i| o.accepted[i] as u64).sum();
        assert_eq!(stamped_bytes, cd, "stamped bytes vs bytes that left");
        // The stamped ids are exactly the oldest ones, in order.
        assert!(
            o.order.iter().copied().eq(0..o.order.len()),
            "FIFO order broken: {:?}",
            &o.order[..o.order.len().min(20)]
        );
        assert_eq!(tr.len(), o.accepted.len() - o.order.len());
    }

    fn run_ops(cap: usize, ops: Vec<Op>) {
        let mut pair = Pair::new(cap);
        let mut tr = DepartureTracker::default();
        let mut o = Oracle::default();
        let mut blocked = 0u32;
        for op in ops {
            match op {
                Op::Send(len) => {
                    let id = o.accepted.len();
                    let mut v = vec![0u8; len];
                    v[..8.min(len)].fill(0xAB);
                    let r = pair.server_conn().datagrams().send(Bytes::from(v), false);
                    match r {
                        Ok(()) => {
                            o.cum_enqueued += len as u64;
                            o.accepted.push(len);
                            o.stamps.push(Vec::new());
                            let ev = tr.on_enqueue(Some(id as u32), o.cum_enqueued, pair.now);
                            assert_eq!(ev, None);
                        }
                        Err(_) => blocked += 1,
                    }
                }
                Op::DrainLose => {
                    let _ = pair.drain_server();
                    poll_and_check(&mut pair, &mut tr, &mut o);
                }
                Op::DrainDeliver(ms) => {
                    let pkts = pair.drain_server();
                    poll_and_check(&mut pair, &mut tr, &mut o);
                    pair.now += Duration::from_millis(ms as u64);
                    pair.deliver_to_client(pkts);
                    pair.step_all(Duration::from_millis(1));
                }
                Op::Idle(ms) => {
                    pair.step_all(Duration::from_millis(ms as u64));
                }
            }
        }
        // Let the connection finish sending whatever is left, delivering
        // everything, then require that every accepted datagram was stamped
        // exactly once.
        for _ in 0..4000 {
            if pair.queued() == 0 {
                break;
            }
            let pkts = pair.drain_server();
            poll_and_check(&mut pair, &mut tr, &mut o);
            pair.now += Duration::from_millis(20);
            pair.deliver_to_client(pkts);
            pair.step_all(Duration::from_millis(5));
        }
        poll_and_check(&mut pair, &mut tr, &mut o);
        assert_eq!(
            pair.queued(),
            0,
            "the FIFO should empty with a healthy path"
        );
        assert!(tr.is_empty(), "tracker holds {} entries", tr.len());
        for (id, s) in o.stamps.iter().enumerate() {
            assert_eq!(s.len(), 1, "datagram {id} stamped {} times", s.len());
        }
        // Stamps never go backwards in enqueue order.
        let times: Vec<Instant> = o.stamps.iter().map(|s| s[0]).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]));
        let _ = blocked;
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 40, ..ProptestConfig::default() })]

        /// Random interleavings of `send(len)` and `poll_transmit`-driven
        /// drains against a real quinn `Connection` pair: every enqueued
        /// datagram is stamped exactly once, stamps are non-decreasing in
        /// enqueue order, and `cum_departed == cum_enqueued - outgoing_total`
        /// at every step. A small buffer makes `Blocked` part of the mix.
        #[test]
        fn every_datagram_is_stamped_exactly_once_and_in_order(
            cap in 3_000usize..=40_000,
            ops in proptest::collection::vec(op(), 1..120),
        ) {
            run_ops(cap, ops);
        }
    }

    /// The premise the property rests on: the generated mix really does
    /// leave datagrams in the FIFO across a drain (quinn's window or pacer
    /// held some back), and really does fill the buffer. A test that never
    /// reached these would pass on any tracker.
    #[test]
    fn the_scenario_does_hold_datagrams_back_and_does_block() {
        let mut pair = Pair::new(8_000);
        let mut tr = DepartureTracker::default();
        let mut o = Oracle::default();
        let mut blocked = 0;
        let mut left_behind = false;
        for round in 0..30 {
            for _ in 0..12 {
                let id = o.accepted.len();
                match pair
                    .server_conn()
                    .datagrams()
                    .send(Bytes::from(vec![1u8; 1000]), false)
                {
                    Ok(()) => {
                        o.cum_enqueued += 1000;
                        o.accepted.push(1000);
                        o.stamps.push(Vec::new());
                        let _ = tr.on_enqueue(Some(id as u32), o.cum_enqueued, pair.now);
                    }
                    Err(_) => blocked += 1,
                }
            }
            let _ = pair.drain_server();
            poll_and_check(&mut pair, &mut tr, &mut o);
            left_behind |= pair.queued() > 0;
            pair.now += Duration::from_millis(round);
        }
        assert!(blocked > 0, "the buffer never filled");
        assert!(left_behind, "quinn never left a datagram in the FIFO");
        assert!(tr.stamped_total() > 0, "nothing ever departed");
    }
}
