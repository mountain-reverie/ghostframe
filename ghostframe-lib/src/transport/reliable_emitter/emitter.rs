//! Central facade that ties cache, emission queue, allocator, group
//! builder, the loss-path queue and the probe timer into one struct.
//! Per-session.

use crate::transport::protocol::TileParityEnvelope;
use crate::transport::reliable_emitter::cache::{CacheEntry, ProbeTag, RetransmitCache};
use crate::transport::reliable_emitter::emission_queue::{Emission, EmissionQueue, WireClass};
use crate::transport::reliable_emitter::h264::{H264Cache, H264Submit, H264_CACHE_FRAMES};
use crate::transport::reliable_emitter::parity::GroupBuilder;
use crate::transport::reliable_emitter::pto::pto_for_attempt;
use crate::transport::reliable_emitter::traits::{DatagramSender, SendOutcome};
use crate::transport::reliable_emitter::wire_seq::WireSeqAllocator;
use crate::transport::reliable_emitter::{
    Content, EmitKey, FEC_GROUP_SIZE_K, PARITY_INTERLEAVE_OFFSET,
};
use crate::transport::rtt_estimator::RttEstimator;
use bytes::{Bytes, BytesMut};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// Generation stamped in the `TileHeader` of a tile datagram's first
/// fragment, or `None` for anything else (control datagrams, later fragments,
/// short buffers). Byte 0's top bit marks a tile datagram; the tile header
/// starts after the 16-byte datagram header and carries
/// `generation << 4 | pass` in its fourth byte.
fn tile_datagram_generation(bytes: &[u8]) -> Option<u8> {
    const GEN_PASS: usize = crate::transport::protocol::DATAGRAM_HEADER_SIZE + 3;
    if bytes.len() <= GEN_PASS || bytes[0] & 0x80 == 0 || bytes[4..6] != [0, 0] {
        return None;
    }
    Some(bytes[GEN_PASS] >> 4)
}

pub struct ReliableTileEmitter {
    pub(crate) cache: RetransmitCache,
    /// Fragments of H.264 frames not fully acknowledged: the probe cache.
    pub(crate) h264: H264Cache,
    pub(crate) alloc: WireSeqAllocator,
    pub(crate) queue: EmissionQueue,
    pub(crate) group: GroupBuilder,
    /// Whether source datagrams feed the XOR parity group builder. Off by
    /// default; see `TransportConfig::tile_fec`.
    tile_fec: bool,
    /// Per pass the loss path resent and that is not acknowledged yet: the
    /// first retransmission's `wire_seq`, how many times it was resent and by
    /// whom (a verdict or the probe). Resolved by
    /// [`Self::note_ack_origin`]. Measurement only.
    resend_origin: HashMap<EmitKey, ResendOrigin>,
    /// Per tile: the generation of the last first-fragment tile datagram
    /// emitted for it. Only read when the loss path resends, to tell a resend for the
    /// tile's current content from one for content it has moved past
    /// (`loss_path_resends_superseded`). Measurement only.
    latest_tile_generation: HashMap<(u8, u8), u8>,
    /// RFC 9002 RTT estimator, fed by `IoBridge` with one departure-based
    /// sample per acknowledged `wire_seq`. [`pto_for_attempt`] derives every
    /// retransmission deadline from it (D1 F4).
    pub(crate) rtt: RttEstimator,
    /// When the newest acknowledgement of any tile transmission was received.
    /// The probe timeout runs from the later of the newest departure and this:
    /// "reset on any ACK" (RFC 9002 section 6.2.1). See [`Self::note_ack`].
    last_ack_at: Option<Instant>,
    /// When the newest tile transmission left quinn's FIFO.
    latest_departure_at: Option<Instant>,
    /// When the probe timeout last fired. A probe's retransmissions wait in
    /// the queue before they depart, so the departure alone does not move the
    /// timer; this does.
    last_pto_fire_at: Option<Instant>,
    /// Reference instant this emitter was constructed with. Tile-datagram
    /// emit stamps (`DatagramHeader.timestamp_us`) are measured as
    /// `now.duration_since(time_base)` rather than the wall clock, so they
    /// track whatever clock the caller's `now: Instant` comes from —
    /// `now_std()` in production (which tracks tokio's clock, paused or
    /// not), a bare `Instant::now()` in unit tests. See `emit_us` below.
    pub(crate) time_base: Instant,
    /// Everything actually put on the wire since the last collection, for the
    /// transmission ledger. Recorded here because this is where `wire_seq` is
    /// allocated; the caller cannot otherwise know which value a datagram
    /// carried.
    transmissions: Vec<EmittedDatagram>,
    /// Passes the client has asked for again, waiting for retransmit
    /// budget. Served oldest first by `tick_budgeted`, ahead of the loss path.
    ///
    /// A queue rather than an immediate send because a NACK arrives when the
    /// client decides to send it, which has nothing to do with whether the
    /// link has room. See `on_nack`.
    pending_nacks: VecDeque<EmitKey>,
    /// Membership of `pending_nacks`, so a request repeated while the first
    /// is still waiting does not become a second retransmission.
    pending_nack_set: HashSet<EmitKey>,
    /// Passes a loss verdict or the probe timeout asked to be sent again,
    /// waiting for budget, oldest first, with who asked and which transmission
    /// of the pass was judged. Served after NACK repair; an entry whose
    /// transmission is no longer the newest of its pass when it comes up (a NACK
    /// repair went first) is dropped, so a loss is repaired once. A key waits
    /// here at most once.
    pending_loss: VecDeque<(Content, RetxCause, u32)>,
    pending_loss_set: HashSet<Content>,
    /// Passes a loss verdict declined to resend ([`RetxRequest::Deferred`]),
    /// each with the transmission judged and the instant it is due to be sent
    /// again. Their latest transmission has been declared lost, so the ledger no
    /// longer shows them as outstanding and the probe's scan of the newest
    /// departures cannot find them. Each has its own backed-off deadline
    /// (`last_sent_at` plus the probe timeout at its loss-path attempt count),
    /// checked by the loss path on every turn ([`Self::take_due_deferred`]),
    /// so they are served while acknowledgements flow, which the single
    /// per-connection probe timer is not. A min-heap on the deadline, validated
    /// lazily when it surfaces (an entry whose pass is gone or has a newer
    /// transmission is dropped there). Not capped: an entry exists only for a
    /// pass in the cache, which is bounded, and each lives at most one backoff
    /// interval past that pass.
    deferred: BinaryHeap<Reverse<DeferredPass>>,
    /// Wire bytes each datagram costs beyond its own length (the
    /// WebTransport framing), added to what `tick_budgeted` bills. Zero
    /// unless the transport sets it.
    wire_overhead: usize,
    pub stats: EmitterStats,
}

/// One datagram as it went on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmittedDatagram {
    /// Unique per transmission, retransmissions included.
    pub wire_seq: u32,
    /// The stamp written into this datagram's header, in the emitter's
    /// clock-relative microseconds, kept at full width: the header carries
    /// only the low 32 bits (a wrap every 71.6 minutes), and the estimator
    /// must not inherit that wrap. 0 for a datagram that was not stamped.
    pub emit_us: u64,
    pub wire_bytes: usize,
    /// The content this transmission was carrying, so an acknowledgement can
    /// be mapped back to the tile-pass (or H.264 fragment) every delivery
    /// consumer expects.
    pub content: Content,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct EmitterStats {
    pub source_emitted: u64,
    pub parity_emitted: u64,
    /// Retransmissions made by the loss path: `loss_retransmits` plus
    /// `pto_probe_retransmits`. (Formerly `rto_fired`, from the per-pass timer, which
    /// is gone.)
    pub loss_path_resends: u64,
    pub ack_hit: u64,
    pub ack_miss: u64,
    pub nack_hit: u64,
    pub nack_miss: u64,
    /// NACKs whose exact `EmitKey` missed but whose tile-pass was found in
    /// the cache's content index.
    ///
    /// Worth its own counter rather than folding into `nack_hit`: on a
    /// refinement-heavy session this should be most of them, because a
    /// coverage NACK cannot name a transmission. If it ever reads zero while
    /// `nack_miss` is high, the index is not being maintained.
    pub nack_resolved_by_content: u64,
    /// Entries removed by `retire_stuck` because they had been retransmitting
    /// for longer than any plausible delivery. Each one was costing a
    /// retransmit every `RTO_BACKOFF_MAX` forever. Non-zero means the link or
    /// the ACK path is failing to retire content the normal way; sustained
    /// growth is a defect, not a tuning matter.
    pub stuck_entries_retired: u64,
    /// Datagrams the transport refused, each of which was re-queued rather
    /// than dropped. Non-zero means quinn's buffer filled despite the
    /// admission gate: a bug signal, since the gate holds the queue far
    /// below it.
    pub send_rejected: u64,
    /// High-water mark of the emission queue's depth.
    ///
    /// Answers whether backpressure is needed between the scheduler and the
    /// emitter, or whether the existing quinn-capacity clamp already bounds
    /// the backlog. A peak that scales with queued work means the bloat moved
    /// here; a bounded peak means it did not.
    pub emission_queue_peak: usize,
    pub retransmit_attempts_total: u64,
    /// NACKs for a fragment that already had a request waiting for budget.
    /// The client repeats itself every tail sweep; these cost nothing.
    pub nack_deduped: u64,
    /// Of `retransmit_attempts_total`, those a NACK asked for (the rest are the
    /// loss path's: `loss_path_resends`). Measurement only.
    pub retransmit_by_nack: u64,
    /// Loss-path resends (`loss_path_resends`) whose pass was then acknowledged by a
    /// transmission sent BEFORE the first resend: the data was still on its way.
    /// Takes its verdict back when the resend is acknowledged too (nearly always;
    /// every duplicate is acknowledged), so it reads far too low; see
    /// `loss_path_resends_original_first`. Measurement only.
    pub loss_path_resends_spurious: u64,
    /// Loss-path resends whose pass was first acknowledged through a
    /// transmission sent before the first resend: the original arrived, so the
    /// verdict or probe was premature (the Eifel test, RFC 3522). Unlike
    /// `loss_path_resends_spurious` this verdict is final: the acknowledgement of the
    /// retransmission that follows an acknowledged original (every duplicate
    /// is acknowledged) does not turn it into a useful fire. Measurement only.
    ///
    /// Not disjoint from `loss_path_resends_useful`: a fire is both when the original is
    /// acknowledged first and the retransmission after it, so
    /// `loss_path_resends_original_first + loss_path_resends_useful` can exceed `loss_path_resends`. Read it as a
    /// share of `loss_path_resends`, not as a partition.
    pub loss_path_resends_original_first: u64,
    /// Loss-path resends whose pass was acknowledged by a retransmission: the
    /// resend earned its keep. `loss_path_resends - loss_path_resends_spurious - loss_path_resends_useful` were
    /// for passes not acknowledged yet. Measurement only.
    pub loss_path_resends_useful: u64,
    /// Retransmissions made because a loss verdict (packet or time threshold)
    /// named a transmission whose content was still current.
    pub loss_retransmits: u64,
    /// Retransmissions made by the probe timeout: the newest one or two
    /// outstanding transmissions, when it expired.
    pub pto_probe_retransmits: u64,
    /// Times the probe timeout expired.
    pub pto_fires: u64,
    /// Verdicts with no retransmission, by why: the pass was no longer cached
    /// (acknowledged or superseded), or a newer transmission of it was already
    /// in flight (a NACK repair, say), or one was already waiting.
    /// Passes still cached (sent, not acknowledged, not cancelled) when the
    /// stats were last published. Only set by the publish; zero in `stats`.
    pub cache_entries: usize,
    pub verdict_content_gone: u64,
    /// H.264 fragments submitted (first transmissions).
    pub h264_fragments_submitted: u64,
    /// H.264 fragments sent again because a verdict named an IDR fragment.
    pub h264_idr_resends: u64,
    /// H.264 fragments sent again by the probe timeout (any frame).
    pub h264_probe_resends: u64,
    /// Verdicts on a P-frame fragment, which are not sent again.
    pub h264_p_verdicts_not_resent: u64,
    /// Fragments released by an acknowledgement, and acknowledgements that
    /// found no entry (a duplicate, or a frame since superseded or trimmed).
    pub h264_ack_hits: u64,
    pub h264_ack_misses: u64,
    /// Cache entries dropped without an acknowledgement: superseded by an
    /// IDR, or trimmed to the newest frames.
    pub h264_cache_dropped: u64,
    /// Queued emissions removed before they left (superseded by an IDR, or a
    /// suspect frame's), fragments and parity.
    pub h264_queue_purged: u64,
    pub verdict_newer_in_flight: u64,
    pub verdict_already_queued: u64,
    /// Verdicts left to the probe because the pass had been resent
    /// [`LOSS_RETX_MAX_ATTEMPTS`] times by the loss path.
    pub verdict_deferred_to_pto: u64,
    /// Loss retransmissions whose original turned out to have arrived (the
    /// original was the first transmission of the pass to be acknowledged):
    /// spurious loss verdicts, Eifel style. Counted per retransmission.
    pub spurious_loss: u64,
    /// The same for probe retransmissions: spurious probes.
    pub spurious_pto: u64,
    /// Times the resend bookkeeping hit its cap and was dropped; resends made
    /// before a reset and not yet acknowledged are never attributed.
    pub resend_origin_resets: u64,
    /// First-fragment tile datagrams emitted at a different generation from
    /// the tile's previous one: tile content replaced while the emitter was
    /// watching. The premise of `loss_path_resends_superseded`, and deliberately
    /// independent of `cancel_for_tile` (a count of cancellations would read
    /// zero exactly when the cancel is missing, hiding that bug behind a failed
    /// premise). Zero means no tile changed and a zero there proves nothing.
    /// Measurement only.
    pub generation_replacements: u64,
    /// Loss-path resends for a pass whose tile has since been emitted at a
    /// different generation: the path re-sent content the client will discard
    /// unacknowledged, so the fire can never be settled. Zero by construction
    /// while every supersession cancels the cache; non-zero means a path
    /// replaces tile content without cancelling. Measurement only.
    pub loss_path_resends_superseded: u64,
}

/// What is known about a pass the loss path resent: the first retransmission's
/// `wire_seq`, how many times it fired, and whether those fires are already
/// counted as spurious (an older transmission was acknowledged) so a later
/// acknowledgement of a retransmission can move them to useful.
#[derive(Clone, Copy)]
struct ResendOrigin {
    first_retx_ws: u32,
    fires: u32,
    counted_spurious: bool,
    /// The first acknowledgement seen for the pass named a transmission sent
    /// before the first retransmission. Sticky: unlike `counted_spurious` it
    /// is not taken back when the retransmission is acknowledged as well.
    original_first: bool,
    /// The probe timeout made the first retransmission (rather than a loss
    /// verdict).
    by_pto: bool,
}

/// Who asked for a retransmission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetxCause {
    /// A loss verdict from the threshold detector.
    Loss,
    /// The probe timeout.
    Probe,
}

/// Retransmissions of one pass after which a loss verdict stops resending it
/// and leaves the pass to the probe timeout. Three: the pass has then been lost
/// four times running, which at 5 % independent loss happens once in 160,000
/// passes and at 50 % in 1 of 16, and the verdict-driven resends so far cost
/// one round trip each; past that the path or the pass is not behaving, and a
/// resend per round trip (each one another reported loss) is the storm the
/// per-pass RTO backoff used to prevent. The probe's own backoff (doubling to 5
/// s) takes over.
pub(crate) const LOSS_RETX_MAX_ATTEMPTS: u8 = 3;

/// A pass a verdict declined to resend, waiting for its own deadline. Ordered
/// by `(due, judged_ws)`; the key rides along.
#[derive(Debug, Clone, Copy)]
struct DeferredPass {
    key: Content,
    /// The transmission judged lost. If the pass's newest transmission is no
    /// longer this one, something else has resent it and the entry is moot.
    judged_ws: u32,
    due: Instant,
}

impl PartialEq for DeferredPass {
    fn eq(&self, o: &Self) -> bool {
        (self.due, self.judged_ws) == (o.due, o.judged_ws)
    }
}
impl Eq for DeferredPass {}
impl PartialOrd for DeferredPass {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for DeferredPass {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (self.due, self.judged_ws).cmp(&(o.due, o.judged_ws))
    }
}

/// What became of a request to retransmit a transmission's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetxRequest {
    /// Queued: the content is current and nothing else is repairing it.
    Queued,
    /// The pass is not cached: it was acknowledged, or superseded.
    ContentGone,
    /// A newer transmission of the pass exists (a repair already in flight).
    NewerInFlight,
    /// The pass already waits for budget.
    AlreadyQueued,
    /// `GHOSTFRAME_NO_RTO=1`: the loss path and the probe are off; the cache
    /// and the NACK path are not.
    Disabled,
    /// A verdict on a fragment of an H.264 P frame: not sent again. The
    /// client would decode it 1.5 round trips late behind newer frames that
    /// chain from it; the caller answers with a recovery IDR instead. (A
    /// probe does send a P fragment again, to draw an acknowledgement.)
    NotRetransmitted,
    /// The pass has already been resent [`LOSS_RETX_MAX_ATTEMPTS`] times: a
    /// verdict no longer resends it (and so does not report another loss every
    /// round trip for a pass the client never acknowledges); only the probe
    /// does, and it backs off.
    Deferred,
}

/// Most passes whose resend history is tracked at once. Measurement only: a
/// pass that fires and is never acknowledged (cancelled, retired, or every
/// copy lost) would otherwise sit here forever. At the cap the whole table is
/// dropped and `resend_origin_resets` counts it, so the newest fires stay
/// attributed and a reader can see that older ones were forgotten.
const RESEND_ORIGIN_CAP: usize = 16_384;

impl ReliableTileEmitter {
    /// `now` becomes `time_base` — the reference instant every subsequent
    /// emit stamp is measured against (see `emit_us`). Callers should pass
    /// the same clock source they'll later pass to `submit_one` / `tick` /
    /// etc. (`now_std()` in production) so the stamps stay self-consistent
    /// under a paused tokio clock.
    pub fn new(now: Instant) -> Self {
        Self {
            cache: RetransmitCache::new(),
            h264: H264Cache::new(),
            alloc: WireSeqAllocator::new(),
            queue: EmissionQueue::new(),
            group: GroupBuilder::new(FEC_GROUP_SIZE_K),
            tile_fec: false,
            resend_origin: HashMap::new(),
            latest_tile_generation: HashMap::new(),
            rtt: RttEstimator::new(),
            last_ack_at: None,
            latest_departure_at: None,
            last_pto_fire_at: None,
            time_base: now,
            transmissions: Vec::new(),
            pending_nacks: VecDeque::new(),
            pending_nack_set: HashSet::new(),
            pending_loss: VecDeque::new(),
            pending_loss_set: HashSet::new(),
            deferred: BinaryHeap::new(),
            wire_overhead: 0,
            stats: EmitterStats::default(),
        }
    }

    /// Turn tile XOR parity on or off. Off at construction. Turn it on before
    /// the first submit: a group needs contiguous `wire_seq`s, so enabling it
    /// mid-stream makes the first group short and it never completes.
    pub fn set_tile_fec(&mut self, on: bool) {
        self.tile_fec = on;
    }

    /// Bill `bytes` more than each retransmitted datagram's length against
    /// `tick_budgeted`'s byte bound: the framing the transport adds.
    pub fn set_wire_overhead(&mut self, bytes: usize) {
        self.wire_overhead = bytes;
    }

    /// Datagrams built (wire_seq allocated, cached; the timer is not running
    /// yet, it starts at departure) and not yet handed to the transport.
    pub fn emission_queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Retransmissions the client asked for that are waiting for budget.
    pub fn pending_nack_count(&self) -> usize {
        self.pending_nacks.len()
    }

    /// Retransmissions a loss verdict or a probe asked for that are waiting
    /// for budget.
    pub fn pending_loss_count(&self) -> usize {
        self.pending_loss.len()
    }

    /// When the probe timeout expires, or `None` while it is not running
    /// (nothing is cached, or nothing has departed yet). Runs from the later
    /// of the newest departure, the newest acknowledgement and the last
    /// expiry, for one PTO backed off by the consecutive expiries since the
    /// last acknowledgement (RFC 9002 section 6.2).
    pub fn pto_deadline(&self) -> Option<Instant> {
        if self.pending_cache_entries() == 0 && self.h264.is_empty() {
            return None;
        }
        let departed = self.latest_departure_at?;
        let base = [self.last_ack_at, self.last_pto_fire_at]
            .into_iter()
            .flatten()
            .fold(departed, Instant::max);
        let backoff = self.rtt.pto_count().min(8) as u8;
        Some(base + pto_for_attempt(&self.rtt, backoff))
    }

    pub fn pto_due(&self, now: Instant) -> bool {
        self.pto_deadline().is_some_and(|d| d <= now)
    }

    /// The probe timeout expired at `now`: count it and restart it. The caller
    /// then asks for the newest outstanding transmissions to be probed.
    pub fn note_pto_fire(&mut self, now: Instant) {
        self.rtt.on_pto_fire(now);
        self.last_pto_fire_at = Some(now);
        self.stats.pto_fires += 1;
    }

    /// The probe timeout of a first transmission: what the scheduler's
    /// priority retry waits for, so the system has one notion of "overdue".
    pub fn pto(&self) -> Duration {
        pto_for_attempt(&self.rtt, 0)
    }

    /// Submit one tile-pass with a single payload buffer. The buffer is
    /// the body of the source datagram *minus* the DatagramHeader bytes
    /// (the emitter prepends the header internally with the allocated
    /// wire_seq).
    ///
    /// `queued_at` is when the underlying `TileWork` became available to
    /// the scheduler (`TileWork::queued_at`), distinct from `now` (when
    /// it's actually being handed to the wire). Carried onto `CacheEntry`
    /// so the ACK path can measure `queued_at -> ACK` (BWE Stage 2.1) in
    /// addition to `last_sent_at -> ACK` (Stage 2.0) — the former captures
    /// scheduler queueing delay, which the latter cannot see.
    ///
    /// `probe` mirrors `queued_at`'s treatment: `Some` iff this pass is
    /// being sent inside an active BWE Stage 2.4 probe window, and it is
    /// carried onto `CacheEntry` unchanged so the ACK path can build the
    /// `PacedPacketInfo` goog_cc's probe estimator requires. `None` for
    /// the overwhelming majority of ordinary traffic.
    pub fn submit_one(
        &mut self,
        key: EmitKey,
        source_datagram_bytes: Bytes,
        queued_at: Instant,
        probe: Option<ProbeTag>,
        now: Instant,
    ) {
        self.submit_one_as(key, source_datagram_bytes, queued_at, probe, now, false);
    }

    /// [`Self::submit_one`], saying whether this is the scheduler re-sending a
    /// pass rather than a first send. Only the offered-load label differs.
    pub fn submit_one_as(
        &mut self,
        key: EmitKey,
        source_datagram_bytes: Bytes,
        queued_at: Instant,
        probe: Option<ProbeTag>,
        now: Instant,
        scheduler_retry: bool,
    ) {
        let wire_seq = self.alloc.allocate();
        // Stamp in place when this handle is the only owner (the production
        // path hands over a freshly built `Vec`); copy only when it is shared.
        let mut bytes = match source_datagram_bytes.try_into_mut() {
            Ok(m) => m,
            Err(shared) => BytesMut::from(&shared[..]),
        };
        if bytes.len() >= 12 {
            bytes[8..12].copy_from_slice(&wire_seq.to_be_bytes());
        }
        // Stamp server emit time (clock-relative μs, u32 wrap) into the
        // DatagramHeader.timestamp_us field at [12..16] for tile datagrams
        // (which set TILE_DATAGRAM_FLAG = 0x80 in byte 0). Used by the
        // client's per-tier latency tracking and echoed back via the ACK
        // envelope for the BWE delay-gradient input.
        let mut stamped_emit_us = 0u64;
        if bytes.len() >= 16 && (bytes[0] & 0x80) != 0 {
            let emit_us = self.emit_us64(now);
            stamped_emit_us = emit_us;
            bytes[12..16].copy_from_slice(&(emit_us as u32).to_be_bytes());
        }
        if let Some(g) = tile_datagram_generation(&bytes) {
            if self
                .latest_tile_generation
                .insert((key.tile_x, key.tile_y), g)
                .is_some_and(|prev| prev != g)
            {
                self.stats.generation_replacements += 1;
            }
        }
        // One allocation from here on: the cache, the FEC group and the
        // emission queue share it by reference count.
        let bytes: Bytes = bytes.freeze();
        self.transmissions.push(EmittedDatagram {
            wire_seq,
            emit_us: stamped_emit_us,
            wire_bytes: bytes.len(),
            content: Content::Tile(key),
        });
        let entry = CacheEntry {
            datagram: bytes.clone(),
            queued_at,
            first_sent_at: now,
            last_sent_at: now,
            attempts: 0,
            loss_attempts: 0,
            latest_ws: wire_seq,
            probe,
        };
        self.cache.insert(key, entry);
        // Feed group builder; on K-th source build & schedule parity envelope.
        self.feed_group(wire_seq, bytes.clone());
        self.note_emission_queue_depth();
        // Enqueue the source itself.
        let class = if scheduler_retry {
            WireClass::SchedulerRetry
        } else {
            WireClass::for_first_send(key.pass_idx)
        };
        self.queue.push_source(bytes, class);
        self.stats.source_emitted += 1;
    }

    /// Submit one fragment of an H.264 access unit: allocate its `wire_seq`,
    /// stamp it into the datagram (bytes 8..12 of the frame header, where
    /// `fragment_frame_ref` left `NO_ACK`), report it for the ledger, keep it
    /// in the probe cache and queue it. `bytes` is the whole frame datagram.
    ///
    /// The queue and the ledger see it exactly as they see a tile pass: a
    /// real `wire_seq` is never on the wire without a ledger record, because
    /// the record is made from the transmission reported here.
    pub fn submit_h264_fragment(&mut self, frag: H264Submit, now: Instant) {
        let H264Submit {
            frame_seq,
            frag_idx,
            is_idr,
            bytes,
            queued_at,
            probe,
        } = frag;
        let wire_seq = self.alloc.allocate();
        let mut buf = match bytes.try_into_mut() {
            Ok(m) => m,
            Err(shared) => BytesMut::from(&shared[..]),
        };
        debug_assert!(buf.len() >= 12, "a frame datagram has a 22-byte header");
        if buf.len() >= 12 {
            buf[8..12].copy_from_slice(&wire_seq.to_be_bytes());
        }
        let bytes: Bytes = buf.freeze();
        self.transmissions.push(EmittedDatagram {
            wire_seq,
            emit_us: self.emit_us64(now),
            wire_bytes: bytes.len(),
            content: Content::H264Frag {
                frame_seq,
                frag_idx,
            },
        });
        self.h264.insert(
            frame_seq,
            frag_idx,
            is_idr,
            CacheEntry {
                datagram: bytes.clone(),
                queued_at,
                first_sent_at: now,
                last_sent_at: now,
                attempts: 0,
                loss_attempts: 0,
                latest_ws: wire_seq,
                probe,
            },
        );
        // Not fed to the tile XOR group builder: a fragment takes a wire_seq,
        // which breaks the run of a tile group in progress, and the builder
        // abandons a group at a gap rather than emit a parity whose
        // membership is a lie.
        self.note_emission_queue_depth();
        self.queue.push_source(bytes, WireClass::H264);
        self.stats.h264_fragments_submitted += 1;
    }

    /// Send a cached H.264 fragment again, with a `wire_seq` of its own.
    /// Returns the wire bytes queued, or `None` when there was nothing to send
    /// (the fragment was acknowledged or dropped while the request waited, or
    /// another transmission of it went first).
    fn resend_h264_fragment(
        &mut self,
        frame_seq: u32,
        frag_idx: u16,
        cause: RetxCause,
        judged_ws: u32,
        now: Instant,
    ) -> Option<usize> {
        let emit_us = self.emit_us64(now);
        let ws = self.alloc.peek();
        let entry = self.h264.get_mut(frame_seq, frag_idx)?;
        if entry.latest_ws != judged_ws {
            self.stats.verdict_newer_in_flight += 1;
            return None;
        }
        self.alloc.allocate();
        entry.attempts = entry.attempts.saturating_add(1);
        entry.loss_attempts = entry.loss_attempts.saturating_add(1);
        entry.last_sent_at = now;
        // A resend leaves any probe cluster it was tagged for, as a tile does.
        entry.probe = None;
        entry.latest_ws = ws;
        let mut bytes = entry.datagram.to_vec();
        bytes[8..12].copy_from_slice(&ws.to_be_bytes());
        let bytes = Bytes::from(bytes);
        self.transmissions.push(EmittedDatagram {
            wire_seq: ws,
            emit_us,
            wire_bytes: bytes.len(),
            content: Content::H264Frag {
                frame_seq,
                frag_idx,
            },
        });
        let sent = bytes.len() + self.wire_overhead;
        self.queue.push_source(bytes, WireClass::LossRetransmit);
        self.stats.loss_path_resends += 1;
        self.stats.retransmit_attempts_total += 1;
        match cause {
            RetxCause::Loss => {
                self.stats.loss_retransmits += 1;
                self.stats.h264_idr_resends += 1;
            }
            RetxCause::Probe => {
                self.stats.pto_probe_retransmits += 1;
                self.stats.h264_probe_resends += 1;
            }
        }
        Some(sent)
    }

    /// An acknowledgement named fragment `frag_idx` of `frame_seq` (through
    /// any of its transmissions): release it. Returns whether an entry was held.
    pub fn on_h264_ack(&mut self, frame_seq: u32, frag_idx: u16) -> bool {
        if self.h264.remove(frame_seq, frag_idx).is_some() {
            self.stats.h264_ack_hits += 1;
            true
        } else {
            self.stats.h264_ack_misses += 1;
            false
        }
    }

    /// An IDR at `frame_seq` supersedes every older frame: forget their cached
    /// fragments and take their emissions that have not left the queue out of
    /// it. Returns the `wire_seq`s of the queued fragments removed, whose
    /// ledger records the caller must forget (they never departed, so they can
    /// be neither acknowledged nor lost).
    pub fn supersede_h264_before(&mut self, frame_seq: u32) -> Vec<u32> {
        self.stats.h264_cache_dropped += self.h264.drop_before(frame_seq) as u64;
        self.purge_queued_h264(|f| f < frame_seq)
    }

    /// Remove the queued frame datagrams (fragments, resends and parity) of
    /// the frames `doomed` names. Returns the `wire_seq`s of the tracked ones.
    pub fn purge_queued_h264(&mut self, doomed: impl Fn(u32) -> bool) -> Vec<u32> {
        let (removed, wire_seqs) = self.queue.purge_frames(doomed);
        self.stats.h264_queue_purged += removed as u64;
        wire_seqs
    }

    /// A frame was found undecodable: forget the cached fragments of it and of
    /// every later frame (they chain from it), so the probe timer and the
    /// bytes-in-flight figure stop counting bytes nothing will deliver.
    pub fn drop_h264_from(&mut self, frame_seq: u32) {
        self.stats.h264_cache_dropped += self.h264.drop_from(frame_seq) as u64;
    }

    /// Keep the probe cache to its newest [`H264_CACHE_FRAMES`] frames.
    ///
    /// The bound is a fixed 64 frames for now. It should follow the design's
    /// in-flight bound, `max(32, ceil(2 * srtt / frame_interval) + 8)` (design
    /// section 5), once the frame interval is known here: on a long path a
    /// fixed bound trims fragments that are still legitimately in flight.
    pub fn trim_h264_cache(&mut self) {
        self.stats.h264_cache_dropped += self.h264.trim_to(H264_CACHE_FRAMES) as u64;
    }

    /// Queue a frame datagram the ledger does not track (XOR parity, whose
    /// `wire_seq` is `NO_ACK`), to go through admission like any other.
    pub fn queue_untracked(&mut self, bytes: Bytes, class: WireClass) {
        self.queue.push_source(bytes, class);
    }

    /// Wire bytes of H.264 frame datagrams waiting in the emission queue.
    pub fn queued_h264_bytes(&self) -> usize {
        self.queue.frame_bytes()
    }

    /// Feed one freshly on-the-wire source datagram into the FEC group
    /// builder and, when it completes a group, build the parity envelope
    /// and hand it to the emission queue.
    ///
    /// Every site that allocates a `wire_seq` for a source transmission —
    /// `submit_one`, `tick`'s loss-path retransmit, and `on_nack` — must route
    /// through here rather than each building its own envelope. The
    /// decoder reconstructs a group's membership as a contiguous run of
    /// `k` wire_seqs starting at `group_first_wire_seq`, so if any
    /// allocated `wire_seq` skipped the group builder (which is exactly
    /// the bug this helper fixes: `tick` and `on_nack` used to allocate
    /// `wire_seq`s that never joined a group), the decoder would treat a
    /// `wire_seq` that was never a source as a missing one and fabricate
    /// a datagram that was never sent.
    fn feed_group(&mut self, wire_seq: u32, bytes: Bytes) {
        if !self.tile_fec {
            return;
        }
        if let Some(result) = self.group.add(wire_seq, bytes) {
            let envelope = TileParityEnvelope {
                group_first_wire_seq: result.group_first_wire_seq,
                k: result.k,
                parity_idx: 0,
                source_lens: result.source_lens,
                parity_payload: result.parity,
            };
            let mut env_bytes = Vec::new();
            envelope.encode(&mut env_bytes);
            let emit_after = result
                .group_first_wire_seq
                .wrapping_add(PARITY_INTERLEAVE_OFFSET);
            self.queue.schedule_parity(emit_after, env_bytes);
        }
    }

    /// [`Self::submit_batch`], with `retry[i]` true where item `i` is a
    /// scheduler re-send (see [`Self::submit_one_as`]).
    pub fn submit_batch_marking_retries(
        &mut self,
        items: Vec<(EmitKey, Bytes, Instant, Option<ProbeTag>)>,
        retry: &[bool],
        now: Instant,
    ) {
        for (i, (key, bytes, queued_at, probe)) in items.into_iter().enumerate() {
            let r = retry.get(i).copied().unwrap_or(false);
            self.submit_one_as(key, bytes, queued_at, probe, now, r);
        }
    }

    pub fn submit_batch(
        &mut self,
        items: Vec<(EmitKey, Bytes, Instant, Option<ProbeTag>)>,
        now: Instant,
    ) {
        for (key, bytes, queued_at, probe) in items {
            self.submit_one(key, bytes, queued_at, probe, now);
        }
    }

    fn note_loss_path_resend(&mut self, key: EmitKey, first_ws: Option<u32>, by_pto: bool) {
        let Some(ws) = first_ws else { return };
        if let Some(e) = self.resend_origin.get_mut(&key) {
            e.fires += 1;
            if e.counted_spurious {
                self.stats.loss_path_resends_spurious += 1;
            }
            if e.original_first {
                self.stats.loss_path_resends_original_first += 1;
                if e.by_pto {
                    self.stats.spurious_pto += 1;
                } else {
                    self.stats.spurious_loss += 1;
                }
            }
            return;
        }
        if self.resend_origin.len() >= RESEND_ORIGIN_CAP {
            self.resend_origin.clear();
            self.stats.resend_origin_resets += 1;
        }
        self.resend_origin.insert(
            key,
            ResendOrigin {
                first_retx_ws: ws,
                fires: 1,
                counted_spurious: false,
                original_first: false,
                by_pto,
            },
        );
    }

    /// An acknowledgement named `wire_seq` for pass `key`. If the loss path had
    /// resent that pass: an acknowledgement for a transmission sent BEFORE the
    /// first resend counts those resends as spurious (the data was still on its
    /// way); one for the retransmission or later counts them as
    /// useful and overrides an earlier spurious verdict, because a
    /// multi-fragment pass is acknowledged fragment by fragment and the
    /// first entry may be a fragment that was never lost. Measurement only:
    /// changes no state the emitter acts on. A NACK retransmission is
    /// indistinguishable from the loss path's here, so a pass both resent and
    /// NACKed is attributed by whichever transmission was acknowledged.
    pub fn note_ack_origin(&mut self, key: EmitKey, wire_seq: u32) {
        let Some(e) = self.resend_origin.get_mut(&key) else {
            return;
        };
        let fires = u64::from(e.fires);
        if (wire_seq.wrapping_sub(e.first_retx_ws) as i32) < 0 {
            if !e.counted_spurious {
                e.counted_spurious = true;
                self.stats.loss_path_resends_spurious += fires;
            }
            if !e.original_first {
                e.original_first = true;
                self.stats.loss_path_resends_original_first += fires;
                if e.by_pto {
                    self.stats.spurious_pto += fires;
                } else {
                    self.stats.spurious_loss += fires;
                }
            }
        } else {
            if e.counted_spurious {
                self.stats.loss_path_resends_spurious -= fires;
            }
            self.stats.loss_path_resends_useful += fires;
            self.resend_origin.remove(&key);
        }
    }

    /// Ingest a batch of ACKs from the client. For each key, remove the
    /// corresponding cache entry. A hit (entry was live) bumps `ack_hit`; a
    /// miss (already evicted/cancelled) bumps `ack_miss`. Idempotent: late
    /// ACKs are silent.
    pub fn on_ack(&mut self, keys: &[EmitKey]) {
        for key in keys {
            if self.cache.remove(key).is_some() {
                self.stats.ack_hit += 1;
            } else {
                self.stats.ack_miss += 1;
            }
        }
    }

    /// Record that an acknowledgement of a tile transmission was received at
    /// `received_at`. The caller also feeds the RTT estimator; this is the
    /// other half of the PTO: the timer restarts from the newest acknowledgement.
    /// O(1).
    pub fn note_ack(&mut self, received_at: Instant) {
        self.rtt.on_ack();
        self.last_ack_at = Some(self.last_ack_at.map_or(received_at, |t| t.max(received_at)));
    }

    /// Send what is owed: the client's NACKs, then the loss path's requests
    /// (loss verdicts and probes), each bounded by `max_retransmits` and, in
    /// `tick_budgeted`, by bytes. Nothing here finds losses: the transmission
    /// ledger's threshold detector and the probe timeout do, and ask through
    /// [`Self::request_retransmit`]. This only builds the datagrams, one per
    /// request, with a fresh `wire_seq` (so client dedup keys stay stable and
    /// acknowledgements name transmissions). The cache entry stays until ACKed,
    /// cancelled via `cancel_for_tile`, retired by `retire_stuck` or the session
    /// ends via `clear_cache`.
    ///
    /// Rate-limit rationale: when many requests are waiting (sustained loss),
    /// handing them all to quinn in one call exceeds its datagram buffer and
    /// turns repair into a storm. A small per-call count and a byte bound
    /// convert that into an orderly drip-feed: requests not served this call
    /// stay queued for the next, and the admission pass calls again after
    /// every drain, as quinn's FIFO empties.
    pub fn tick(&mut self, now: Instant, max_retransmits: usize) {
        self.tick_budgeted(now, max_retransmits, usize::MAX);
    }

    /// `tick`, bounded in bytes as well as in count. Returns the wire bytes
    /// queued for retransmission, so the caller can bill them.
    ///
    /// Retransmission used to be bounded only by `max_retransmits` per call,
    /// and this was called on every frame *and* on every event that freed
    /// quinn's buffer (since removed) -- so the bound was per event, not per
    /// unit of time, and said nothing about what the link could carry.
    /// Measured on a 2040-tile screen behind 8 Mbit, in the nine seconds
    /// after a full-screen change: 29,889 retransmissions (14,081 RTO,
    /// 15,808 NACK), ~1.7 MB/s offered to a link moving 1 MB/s, while the
    /// scheduler -- which *is* budgeted -- was offering 0.6 MB/s of passes
    /// the client did not have yet. The excess sat in quinn's 16 MB datagram
    /// buffer until it was full, at which point the quinn-capacity clamp
    /// (since replaced by the admission gate) gave the scheduler nothing and
    /// emission stopped for good, with every acknowledgement arriving 16
    /// seconds stale behind its own retransmits.
    ///
    /// Requests from the client go first: a NACK is the receiver saying a
    /// datagram did not arrive, where the RTO is a timer guessing, and under
    /// queueing delay the timer guesses wrong for everything in flight.
    ///
    /// The byte bound is soft, like `Scheduler::drain_priority_queue`'s: the
    /// retransmission that crosses it is still sent and the loop stops
    /// before the next. A hard bound would strand a pass larger than the
    /// remaining budget behind it forever.
    pub fn tick_budgeted(
        &mut self,
        now: Instant,
        max_retransmits: usize,
        max_bytes: usize,
    ) -> usize {
        let mut count = 0;
        let nacks = self.tick_nacks_budgeted(now, max_retransmits, max_bytes, &mut count);
        let loss = self.tick_loss_budgeted(
            now,
            max_retransmits,
            max_bytes.saturating_sub(nacks),
            &mut count,
        );
        nacks + loss
    }

    /// The first half of [`Self::tick_budgeted`]: the retransmissions the
    /// client asked for, oldest request first, until `max_bytes` is spent.
    /// Returns the wire bytes queued. `count` carries the number of
    /// retransmissions made so far against `max_retransmits`.
    ///
    /// Its own method so the admission pass can serve NACK repair ahead of
    /// every other class and the timer's retransmissions after it.
    pub fn tick_nacks_budgeted(
        &mut self,
        now: Instant,
        max_retransmits: usize,
        max_bytes: usize,
        count: &mut usize,
    ) -> usize {
        let mut spent = 0usize;
        let emit_us_now = self.emit_us64(now);
        while *count < max_retransmits && spent < max_bytes {
            let Some(key) = self.pending_nacks.pop_front() else {
                break;
            };
            self.pending_nack_set.remove(&key);
            // Acknowledged or superseded while it waited: nothing to resend.
            let Some(entry) = self.cache.get_mut(&key) else {
                continue;
            };
            let mut bytes = entry.datagram.to_vec();
            // A NACK is the client stating outright that this fragment did
            // not arrive — ground truth, unlike RTO, which fires on delay as
            // readily as on loss and never fires at all for a tile that gets
            // superseded first. On a churning screen supersession is the
            // common case, so NACKs carry most of the loss signal the
            // estimator ever sees.
            //
            // Re-stamped for the same reason as the RTO path: the consumer
            // wants the on-wire moment of *this* transmission, and the stamp
            // being overwritten identifies the one that was lost.
            // Fresh `wire_seq` for the same reason as the RTO path below.
            let mut ws = 0u32;
            if bytes.len() >= 12 {
                ws = self.alloc.allocate();
                bytes[8..12].copy_from_slice(&ws.to_be_bytes());
            }
            if bytes.len() >= 16 && (bytes[0] & 0x80) != 0 {
                bytes[12..16].copy_from_slice(&(emit_us_now as u32).to_be_bytes());
            }
            let bytes = Bytes::from(bytes);
            entry.attempts = entry.attempts.saturating_add(1);
            entry.latest_ws = ws;
            entry.last_sent_at = now;
            entry.probe = None;
            self.transmissions.push(EmittedDatagram {
                wire_seq: ws,
                emit_us: emit_us_now,
                wire_bytes: bytes.len(),
                content: Content::Tile(key),
            });
            // A NACK retransmit allocates its own `wire_seq` above; it must
            // join the FEC group exactly as a first transmission does (see
            // `feed_group`'s doc comment).
            if bytes.len() >= 12 {
                self.feed_group(ws, bytes.clone());
            }
            spent += bytes.len() + self.wire_overhead;
            self.queue.push_source(bytes, WireClass::NackRepair);
            self.stats.retransmit_attempts_total += 1;
            self.stats.retransmit_by_nack += 1;
            *count += 1;
        }
        spent
    }

    /// The second half of [`Self::tick_budgeted`]: the retransmissions the
    /// loss path asked for (loss verdicts and probes), oldest request first,
    /// until `max_bytes` is spent. Returns the wire bytes queued. See
    /// [`Self::tick_nacks_budgeted`] for `count`.
    pub fn tick_loss_budgeted(
        &mut self,
        now: Instant,
        max_retransmits: usize,
        max_bytes: usize,
        count: &mut usize,
    ) -> usize {
        let mut spent = 0usize;
        let emit_us_now = self.emit_us64(now);
        while *count < max_retransmits && spent < max_bytes {
            let Some((content, cause, judged_ws)) = self.pending_loss.pop_front() else {
                break;
            };
            self.pending_loss_set.remove(&content);
            let key = match content {
                Content::Tile(key) => key,
                Content::H264Frag {
                    frame_seq,
                    frag_idx,
                } => {
                    if let Some(sent) =
                        self.resend_h264_fragment(frame_seq, frag_idx, cause, judged_ws, now)
                    {
                        spent += sent;
                        *count += 1;
                    }
                    continue;
                }
            };
            let Some(entry) = self.cache.get_mut(&key) else {
                // Acknowledged or superseded while it waited: nothing to send.
                continue;
            };
            if entry.latest_ws != judged_ws {
                // A NACK repair (or a resubmission) went first: the pass has a
                // newer transmission and its own verdict will come.
                self.stats.verdict_newer_in_flight += 1;
                continue;
            }
            // [RTO-PROBE] temporary instrumentation.
            if crate::transport::reliable_emitter::rto_probe_enabled() {
                let probe_codec = entry
                    .datagram
                    .get(18)
                    .copied()
                    .map(|b| b >> 1)
                    .unwrap_or(255);
                eprintln!(
                    "RTOPROBE retx cause={:?} codec={} pass={} age_since_first_us={} age_since_last_us={} attempts={}",
                    cause,
                    probe_codec,
                    key.pass_idx,
                    now.saturating_duration_since(entry.first_sent_at).as_micros(),
                    now.saturating_duration_since(entry.last_sent_at).as_micros(),
                    entry.attempts,
                );
            }
            entry.attempts = entry.attempts.saturating_add(1);
            entry.loss_attempts = entry.loss_attempts.saturating_add(1);
            entry.last_sent_at = now;
            // A probe cluster measures one burst: goog_cc divides the
            // cluster's bytes by the span between its first and last send
            // to get a send rate. This entry is about to be re-sent with a
            // fresh `emit_us` (below) while still carrying the cluster id
            // from its original transmission, which stretches that span to
            // cover the retransmit and makes the rate meaningless.
            // Measured before this: clusters reported `send: 8970 bytes /
            // 167 ms` — five frame ticks — against a 20 ms receive span, so
            // every one was rejected as `receive/send ratio too high`.
            //
            // Dropping only the cluster membership is deliberately narrower
            // than Karn's algorithm, which discards the whole timing sample
            // from an ambiguous retransmission. That was tried and reverted
            // (see the revert of 033b138): on a congested link it threw away
            // 99% of samples and the delay-based estimator went blind. The
            // delay gradient tolerates the ambiguity; the probe-rate
            // division does not, so only the latter opts out.
            entry.probe = None;
            let mut bytes = entry.datagram.to_vec();
            // [H3-DIAG] log every actual retransmit. Captures the EmitKey
            // routing tuple + a fingerprint of the first fragment so a
            // stale-bundle replay (e.g. a PalRle bundled payload whose slot
            // id has since been reassigned to a different palette) shows
            // up as: same (tile_x, tile_y) replayed across many frame_seqs
            // with the same fingerprint, while the live palette has moved on.
            let attempts = entry.attempts;
            if tracing::enabled!(target: "emitter.h3", tracing::Level::TRACE) {
                let (fp, codec_byte, palette_id_byte, flags_byte) = datagram_fp(&bytes);
                tracing::trace!(
                    target: "emitter.h3",
                    frame_seq = key.frame_seq,
                    tile_x = key.tile_x,
                    tile_y = key.tile_y,
                    pass_idx = key.pass_idx,
                    attempts = attempts,
                    codec = codec_byte,
                    palrle_flags = flags_byte,
                    palette_id = palette_id_byte,
                    fp = format_args!("{:08x}", fp),
                    "[H3-SRV] retransmit"
                );
            }
            // Re-stamp emit time on retransmit so the BWE consumer sees
            // the actual on-wire moment, not the original send. Tile
            // datagrams only (top bit of byte 0 set).
            // A retransmission is a *new transmission* and gets its own
            // `wire_seq`. Reusing the original's — which both retransmit
            // paths did — is what made an acknowledgement ambiguous
            // about which send it referred to, and that ambiguity is the
            // reason a Karn-style filter had to discard 99% of timing
            // samples on a congested link before being reverted.
            let mut ws = 0u32;
            let mut first_ws: Option<u32> = None;
            if bytes.len() >= 12 {
                ws = self.alloc.allocate();
                first_ws = Some(ws);
                bytes[8..12].copy_from_slice(&ws.to_be_bytes());
            }
            let mut stamped = 0u64;
            if bytes.len() >= 16 && (bytes[0] & 0x80) != 0 {
                stamped = emit_us_now;
                bytes[12..16].copy_from_slice(&(stamped as u32).to_be_bytes());
            }
            if let Some(g) = tile_datagram_generation(&bytes) {
                if self
                    .latest_tile_generation
                    .get(&(key.tile_x, key.tile_y))
                    .is_some_and(|&latest| latest != g)
                {
                    self.stats.loss_path_resends_superseded += 1;
                }
            }
            let bytes = Bytes::from(bytes);
            entry.latest_ws = ws;
            self.transmissions.push(EmittedDatagram {
                wire_seq: ws,
                emit_us: stamped,
                wire_bytes: bytes.len(),
                content: Content::Tile(key),
            });
            // A retransmit allocates its own `wire_seq` above; it must
            // join the FEC group exactly as a first transmission does
            // (see `feed_group`'s doc comment), or the group builder's
            // membership range no longer matches what was actually put
            // on the wire.
            if bytes.len() >= 12 {
                self.feed_group(ws, bytes.clone());
            }
            spent += bytes.len() + self.wire_overhead;
            self.queue.push_source(bytes, WireClass::LossRetransmit);
            self.stats.loss_path_resends += 1;
            match cause {
                RetxCause::Loss => self.stats.loss_retransmits += 1,
                RetxCause::Probe => self.stats.pto_probe_retransmits += 1,
            }
            self.note_loss_path_resend(key, first_ws, cause == RetxCause::Probe);
            self.stats.retransmit_attempts_total += 1;
            *count += 1;
        }
        spent
    }

    /// Number of cache entries currently held — i.e. unACKed tile-passes
    /// awaiting either an ACK, a `cancel_for_tile`, or session end.
    /// Diagnostic accessor for the cumulative-emit log.
    /// Retire entries that have been pending longer than `older_than`, and
    /// return their keys so the caller can arrange for the content to be sent
    /// again fresh.
    ///
    /// The returned keys are NOT "dropped frames" — the caller is expected to
    /// force the affected tiles dirty, so the content is re-encoded at a
    /// current `frame_seq` and re-sent. That is strictly better than what it
    /// replaces: the retired payload was a snapshot from `older_than` ago.
    pub fn retire_stuck(&mut self, now: Instant, older_than: Duration) -> Vec<EmitKey> {
        let retired = self.cache.retire_older_than(now, older_than);
        self.stats.stuck_entries_retired += retired.len() as u64;
        for k in &retired {
            self.resend_origin.remove(k);
        }
        retired
    }

    pub fn pending_cache_entries(&self) -> usize {
        self.cache.len()
    }

    /// Wire bytes currently in flight: everything cached and not yet ACKed.
    /// Fed to goog_cc as `TransportPacketsFeedback::data_in_flight`.
    pub fn bytes_in_flight(&self) -> usize {
        self.cache.bytes_outstanding() + self.h264.bytes_outstanding()
    }

    /// Ingest a batch of NACKs from the client. For each pass key:
    /// - Cache miss → bump `nack_miss` and continue.
    /// - Already waiting for budget → bump `nack_deduped` and continue.
    /// - Otherwise queue the pass's datagram for `tick_budgeted` and bump
    ///   `nack_hit`.
    ///
    /// Nothing is sent from here. This used to push the fragment straight
    /// onto the emission queue, which made the client's request rate the
    /// server's send rate: the tail sweep re-asks for every missing pass of
    /// every stalled tile each 500 ms, and nothing between that and the wire
    /// knew what the link could carry. See `tick_budgeted`.
    ///
    /// A loss verdict or probe request already waiting for this pass becomes
    /// stale when the NACK repair goes out (it is the pass's newest transmission
    /// then) and is dropped at send time; `tick_budgeted` bumps `attempts` when
    /// the datagram actually goes out.
    ///
    /// `_now` is unused now that the send is deferred, and kept so callers
    /// continue to pass the clock they use for `submit_one` / `tick`.
    pub fn on_nack(&mut self, entries: &[EmitKey], _now: Instant) {
        for &nacked_key in entries {
            // Exact first: a NACK whose `frame_seq` happens to be the one the
            // pass was cached under hits directly.
            //
            // Content second, and the usual case: the client never received
            // the datagram, so it has no `frame_seq` for it and sends the
            // tile's last-known one, while refinement passes for one tile
            // are emitted across different frames. Looked up by exact key
            // alone that missed every time, which is why `nack_hit` read
            // exactly 0 in the field rather than merely low -- the pass was
            // never resent, and the client gave up on it after
            // MAX_TAIL_SWEEP_ATTEMPTS, stranding the tile short of its
            // finest bit-planes for the rest of the session.
            let key = if self.cache.get(&nacked_key).is_some() {
                nacked_key
            } else {
                match self.cache.lookup_content(
                    nacked_key.tile_x,
                    nacked_key.tile_y,
                    nacked_key.pass_idx,
                ) {
                    Some(found) => {
                        self.stats.nack_resolved_by_content += 1;
                        found
                    }
                    None => nacked_key,
                }
            };
            if self.cache.get(&key).is_none() {
                self.stats.nack_miss += 1;
                if crate::transport::reliable_emitter::rto_probe_enabled() {
                    eprintln!("RTOPROBE nack_miss");
                }
                continue;
            }
            if crate::transport::reliable_emitter::rto_probe_enabled() {
                eprintln!("RTOPROBE nack_hit");
            }
            if !self.pending_nack_set.insert(key) {
                self.stats.nack_deduped += 1;
                continue;
            }
            self.pending_nacks.push_back(key);
            self.stats.nack_hit += 1;
        }
    }

    pub fn cancel_for_tile(&mut self, tile_x: u8, tile_y: u8) {
        self.cache.cancel_for_tile(tile_x, tile_y);
        // Loss-path requests still queued for the tile are dropped when they
        // come up and the cache lookup misses.
    }

    /// Drop every cached entry. Called from `IoBridge::Event::ConnectionLost`
    /// — see `RetransmitCache::clear` doc.
    pub fn clear_cache(&mut self) {
        self.cache.clear();
        self.h264.clear();
        self.resend_origin.clear();
        self.latest_tile_generation.clear();
        self.pending_loss.clear();
        self.pending_loss_set.clear();
        self.deferred.clear();
        // The estimator and the acknowledgement progress describe the
        // connection that is gone. Its PTO backoff, srtt and "newest ACK"
        // must not hold back or delay the next connection's first passes.
        self.reset_connection_state();
        // Queued loss-path requests are dropped when they come up and the
        // cache lookup misses.
    }

    /// Forget everything measured on the current connection: the RTT
    /// estimator (back to RFC 9002's initial RTT, backoff zero) and the
    /// newest-acknowledgement time and departure the probe timer runs from.
    pub fn reset_connection_state(&mut self) {
        self.rtt = RttEstimator::new();
        self.last_ack_at = None;
        self.latest_departure_at = None;
        self.last_pto_fire_at = None;
    }

    pub fn has_cache_entries_for_tile(&self, tile_x: u8, tile_y: u8) -> bool {
        self.cache.has_entries_for_tile(tile_x, tile_y)
    }

    /// Is this tile-pass still in the retransmit cache, i.e. will `tick`
    /// re-send it if no acknowledgement arrives?
    ///
    /// O(1), through the content index -- this is asked once per in-flight
    /// priority tile per scheduler tick.
    ///
    /// May answer `false` for a pass that is in fact cached under an older
    /// emission whose newer sibling has since been removed (the index
    /// points at the newest and is cleared with it). The caller treats
    /// `false` as "nobody is holding this, send it again", so that
    /// direction costs a redundant resend and never a stranded tile.
    pub fn holds_pass(&self, tile_x: u8, tile_y: u8, pass_idx: u8) -> bool {
        self.cache
            .lookup_content(tile_x, tile_y, pass_idx)
            .is_some()
    }

    /// Returns `now` measured against `time_base` (the instant this
    /// emitter was constructed with), truncated to a u32 microsecond
    /// counter (≈71 minute wrap). Used to stamp
    /// `DatagramHeader.timestamp_us` on tile datagrams.
    ///
    /// Deliberately *not* UNIX-epoch-based: clock skew between server and
    /// client is irrelevant here — the BWE consumer (`io_bridge.rs`'s OWD
    /// computation) only looks at deltas between packets in the same
    /// batch, computed as `arrival_lo16.wrapping_sub(emit_lo16)` on the
    /// client's own independently-stamped arrival clock. So the stamp only
    /// needs to advance consistently with whatever clock `now` comes from
    /// — `now_std()` (tokio's clock, paused-aware) in production, a bare
    /// `Instant::now()` in unit tests that don't care about virtual time.
    /// `duration_since` saturates to zero rather than panicking if `now`
    /// somehow precedes `time_base`.
    pub(crate) fn emit_us(&self, now: Instant) -> u32 {
        self.emit_us64(now) as u32
    }

    /// [`Self::emit_us`] before the truncation to the 32 bits the datagram
    /// header carries.
    pub(crate) fn emit_us64(&self, now: Instant) -> u64 {
        now.duration_since(self.time_base).as_micros() as u64
    }

    /// A tile transmission left quinn's FIFO at `at`: the probe timeout runs
    /// from the newest departure. O(1). (Which transmission it was does not
    /// matter here; the ledger orders them.)
    pub fn on_departed(&mut self, _wire_seq: u32, at: Instant) {
        self.latest_departure_at = Some(self.latest_departure_at.map_or(at, |t| t.max(at)));
    }

    /// What a request to retransmit `content`, because transmission
    /// `wire_seq` of it is lost or overdue, would do. Content is current when
    /// it is cached and `wire_seq` is the newest transmission of it.
    fn classify_retx(&self, content: &Content, wire_seq: u32, cause: RetxCause) -> RetxRequest {
        let (latest_ws, loss_attempts, is_p_frame) = match content {
            Content::Tile(key) => match self.cache.get(key) {
                None => return RetxRequest::ContentGone,
                Some(e) => (e.latest_ws, e.loss_attempts, false),
            },
            Content::H264Frag {
                frame_seq,
                frag_idx,
            } => match self.h264.get(*frame_seq, *frag_idx) {
                None => return RetxRequest::ContentGone,
                Some(e) => (
                    e.latest_ws,
                    e.loss_attempts,
                    self.h264.is_idr(*frame_seq) == Some(false),
                ),
            },
        };
        if latest_ws != wire_seq {
            return RetxRequest::NewerInFlight;
        }
        // A NACK repair already waiting will be the pass's newest
        // transmission before this one could be sent.
        if self.pending_loss_set.contains(content)
            || content
                .tile()
                .is_some_and(|k| self.pending_nack_set.contains(&k))
        {
            return RetxRequest::AlreadyQueued;
        }
        if is_p_frame && cause == RetxCause::Loss {
            return RetxRequest::NotRetransmitted;
        }
        if cause == RetxCause::Loss && loss_attempts >= LOSS_RETX_MAX_ATTEMPTS {
            return RetxRequest::Deferred;
        }
        RetxRequest::Queued
    }

    /// Up to `n` deferred passes whose deadline has come, earliest first, each
    /// with the `wire_seq` of the transmission that was judged. Entries for
    /// passes since acknowledged or cancelled, or resent by someone else since
    /// the verdict, are dropped as they surface. O(log n) per entry.
    pub fn take_due_deferred(&mut self, now: Instant, n: usize) -> Vec<(Content, u32)> {
        let mut out = Vec::new();
        while out.len() < n {
            let Some(Reverse(top)) = self.deferred.peek() else {
                break;
            };
            if top.due > now {
                break;
            }
            let Reverse(d) = self.deferred.pop().expect("peeked");
            if self
                .entry_view(&d.key)
                .is_some_and(|(latest_ws, _, _)| latest_ws == d.judged_ws)
            {
                out.push((d.key, d.judged_ws));
            }
        }
        out
    }

    /// When the earliest deferred entry surfaces, for the event loop's wake
    /// deadline. A peek: the entry may by now be moot, in which case the wake
    /// is spurious and `take_due_deferred` discards it.
    pub fn next_deferred_due(&self) -> Option<Instant> {
        self.deferred.peek().map(|Reverse(d)| d.due)
    }

    /// Entries waiting on a deferred deadline, moot ones included.
    pub fn deferred_len(&self) -> usize {
        self.deferred.len()
    }

    /// Leave `key` to its own deadline: `last_sent_at` plus the probe timeout
    /// at the pass's loss-path attempt count, which doubles with each resend.
    fn defer(&mut self, content: Content, judged_ws: u32) {
        let Some((_, loss_attempts, last_sent_at)) = self.entry_view(&content) else {
            return;
        };
        let due = last_sent_at + pto_for_attempt(&self.rtt, loss_attempts);
        self.deferred.push(Reverse(DeferredPass {
            key: content,
            judged_ws,
            due,
        }));
    }

    /// `(latest_ws, loss_attempts, last_sent_at)` of the cached entry for
    /// `content`, whichever cache holds it.
    fn entry_view(&self, content: &Content) -> Option<(u32, u8, Instant)> {
        match content {
            Content::Tile(key) => self
                .cache
                .get(key)
                .map(|e| (e.latest_ws, e.loss_attempts, e.last_sent_at)),
            Content::H264Frag {
                frame_seq,
                frag_idx,
            } => self
                .h264
                .get(*frame_seq, *frag_idx)
                .map(|e| (e.latest_ws, e.loss_attempts, e.last_sent_at)),
        }
    }

    /// Transmission `wire_seq` of pass `key` was declared lost (packet or time
    /// threshold) or is being probed (the timeout expired with it among the
    /// newest outstanding). If its content is current, queue one retransmission
    /// for `tick_loss_budgeted`; otherwise say why not. The verdict itself, and
    /// what it does to the loss estimate, are the caller's.
    pub fn request_retransmit(
        &mut self,
        content: impl Into<Content>,
        wire_seq: u32,
        cause: RetxCause,
    ) -> RetxRequest {
        if crate::config::no_rto() {
            return RetxRequest::Disabled;
        }
        let content = content.into();
        let r = self.classify_retx(&content, wire_seq, cause);
        match r {
            RetxRequest::Queued => {
                self.pending_loss.push_back((content, cause, wire_seq));
                self.pending_loss_set.insert(content);
            }
            RetxRequest::ContentGone => self.stats.verdict_content_gone += 1,
            RetxRequest::NewerInFlight => self.stats.verdict_newer_in_flight += 1,
            RetxRequest::AlreadyQueued => self.stats.verdict_already_queued += 1,
            RetxRequest::Deferred => {
                self.stats.verdict_deferred_to_pto += 1;
                self.defer(content, wire_seq);
            }
            RetxRequest::NotRetransmitted => self.stats.h264_p_verdicts_not_resent += 1,
            RetxRequest::Disabled => {}
        }
        r
    }

    /// Drain emissions to a sender until the queue is empty.
    /// Take everything put on the wire since the last call, for recording in
    /// the transmission ledger.
    ///
    /// Emitted here rather than inferred by the caller because `wire_seq` is
    /// allocated inside this type — including for retransmissions, which each
    /// get their own.
    pub fn take_transmissions(&mut self) -> Vec<EmittedDatagram> {
        std::mem::take(&mut self.transmissions)
    }

    /// Hand queued emissions to the transport until it refuses one.
    ///
    /// On refusal the emission goes back to the head of the queue and the
    /// drain stops. Stopping matters: `Rejected` means quinn's datagram send
    /// buffer is full, so the next emission would be refused too, and
    /// continuing would spin through the whole queue discarding work.
    ///
    /// Re-queueing is what makes `Blocked` survivable as ordinary flow
    /// control. Before this, `DatagramSender::send` returned unit, the
    /// rejected datagram was dropped, and recovery depended on the emitter's
    /// bounded RTO -- which covers a handful of rejections and collapses
    /// under thousands (measured: 4,726 rejections cost a third of delivery
    /// at production scale).
    /// Record the emission queue's high-water depth.
    fn note_emission_queue_depth(&mut self) {
        let d = self.queue.len();
        if d > self.stats.emission_queue_peak {
            self.stats.emission_queue_peak = d;
        }
    }

    pub fn drain<S: DatagramSender>(&mut self, sender: &mut S, now: Instant) {
        while let Some(emission) = self.next_emission(now) {
            let outcome = match &emission {
                Emission::Source(bytes, _) => sender.send(bytes),
                Emission::Parity(bytes, _) => sender.send(bytes),
            };
            // A `DatagramSender` is the wire as far as this method is
            // concerned (a test's collector, the simulator's link), so what it
            // accepted has departed. The bridge does not come through here: it
            // hands the datagram to quinn and starts the timer when quinn's
            // FIFO releases it.
            let departed_ws = match (&emission, outcome) {
                (Emission::Source(b, _), SendOutcome::Sent) if b.len() >= 12 => {
                    Some(u32::from_be_bytes([b[8], b[9], b[10], b[11]]))
                }
                _ => None,
            };
            if !self.settle_emission(emission, outcome) {
                return;
            }
            if let Some(ws) = departed_ws {
                self.on_departed(ws, now);
            }
        }
    }

    /// Take the next emission due to go on the wire. Paired with
    /// [`Self::settle_emission`]: this is [`Self::drain`] split in two so a
    /// caller whose sender needs `&mut` access to the structure that owns this
    /// emitter can send between the halves, with no borrow of the emitter live
    /// across the send.
    pub(crate) fn next_emission(&mut self, now: Instant) -> Option<Emission> {
        let next = self.alloc.peek();
        self.queue.pop(next, now)
    }

    /// Put an emission the admission gate refused back at the head of the
    /// queue. Unlike a refusal by the transport this is not a failed send:
    /// nothing was attempted, so `send_rejected` does not move.
    pub(crate) fn hold_emission(&mut self, emission: Emission) {
        self.queue.push_front(emission);
    }

    /// Record the outcome of sending an emission from [`Self::next_emission`].
    /// Returns false when the caller must stop draining.
    pub(crate) fn settle_emission(&mut self, emission: Emission, outcome: SendOutcome) -> bool {
        match outcome {
            SendOutcome::Sent => {
                if matches!(emission, Emission::Parity(..)) {
                    self.stats.parity_emitted += 1;
                }
                true
            }
            SendOutcome::Rejected => {
                self.queue.push_front(emission);
                self.stats.send_rejected += 1;
                self.note_emission_queue_depth();
                // Stopping is not an optimisation, it is required.
                // `push_front` puts this emission back at the head, so
                // continuing would pop the very same one and offer it again
                // -- an infinite loop. Verified by removing it: the
                // browserless gate stopped terminating.
                false
            }
        }
    }
}

// Test-only count of fingerprints computed on this thread, so a test can
// prove the work is skipped when no subscriber wants the trace line.
#[cfg(test)]
thread_local! {
    static FP_COMPUTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// [H3-DIAG] FNV-1a fingerprint of the datagram's bytes, plus the
/// peeked codec / palrle-flags / palette_id bytes from a tile datagram
/// layout: 16-byte DatagramHeader + 8-byte TileHeader + payload.
///
/// Returns (fp, codec_byte, palette_id_byte, palrle_flags_byte). For
/// non-PalRle codecs the palette_id / flags bytes are meaningless but
/// still surfaced to keep the log shape stable.
fn datagram_fp(f: &[u8]) -> (u32, u8, u8, u8) {
    #[cfg(test)]
    FP_COMPUTED.with(|c| c.set(c.get() + 1));
    // FNV-1a over the whole fragment except the timestamp field at [12..16]
    // (which the io_bridge stamps lazily and we don't want to compare).
    let mut h: u32 = 0x811c_9dc5;
    for (i, &b) in f.iter().enumerate() {
        if (12..16).contains(&i) {
            continue;
        }
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    // TileHeader.codec lives at [18] as (codec << 1) | lz4.
    let codec_byte = f.get(18).copied().map(|b| b >> 1).unwrap_or(0xFF);
    // PalRle payload starts at [24]: [24]=flags, [25]=palette_id.
    let flags_byte = f.get(24).copied().unwrap_or(0xFF);
    let pid_byte = f.get(25).copied().unwrap_or(0xFF);
    (h, codec_byte, pid_byte, flags_byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::reliable_emitter::traits::testing::CollectSender;

    fn fake_source(seq: u32, frag_idx: u16, payload: u8) -> Bytes {
        // 16-byte DatagramHeader + 8-byte TileHeader + 1-byte payload
        let mut v = vec![0u8; 25];
        // frame_seq with TILE_DATAGRAM_FLAG (bit 31)
        let fs = 0x8000_0000 | seq;
        v[0..4].copy_from_slice(&fs.to_be_bytes());
        v[4..6].copy_from_slice(&frag_idx.to_be_bytes());
        v[6..8].copy_from_slice(&1u16.to_be_bytes());
        // wire_seq at 8..12 left as 0 — emitter will overwrite
        v[12..16].copy_from_slice(&0u32.to_be_bytes()); // timestamp
        v[16] = 0;
        v[17] = 0;
        v[18] = 0;
        v[19] = 0; // tile/codec/etc
        v[20..24].copy_from_slice(&1u32.to_be_bytes()); // payload_len
        v[24] = payload;
        Bytes::from(v)
    }

    /// Ask the loss path to resend `key`'s newest transmission, as a verdict
    /// or a probe would.
    fn ask(e: &mut ReliableTileEmitter, key: EmitKey) -> RetxRequest {
        let ws = e.cache.get(&key).map_or(0, |c| c.latest_ws);
        e.request_retransmit(key, ws, RetxCause::Loss)
    }

    /// The stamp is applied in place, so the bytes on the wire must equal what
    /// the old copy-then-stamp implementation produced, for every length the
    /// header branches distinguish (<12, 12..16, >=16 with and without the
    /// tile flag), whether the caller's handle is unique or shared.
    #[test]
    fn stamped_bytes_equal_the_copy_then_stamp_reference() {
        let now = Instant::now();
        let later = now + Duration::from_micros(1234);
        for (len, flag) in [
            (5usize, true),
            (12, true),
            (15, true),
            (16, false),
            (16, true),
            (200, true),
        ] {
            for shared in [false, true] {
                let mut src = vec![0xA5u8; len];
                if len > 0 {
                    src[0] = if flag { 0x80 } else { 0x00 };
                }
                let mut e = ReliableTileEmitter::new(now);
                let key = EmitKey::new(1, 0, 0, 0);
                let source = Bytes::from(src.clone());
                let keep = if shared { Some(source.clone()) } else { None };
                e.submit_one(key, source, now, None, later);
                // Reference: the pre-change algorithm.
                let mut want = src.clone();
                if want.len() >= 12 {
                    want[8..12].copy_from_slice(&0u32.to_be_bytes());
                }
                if want.len() >= 16 && (want[0] & 0x80) != 0 {
                    want[12..16].copy_from_slice(&1234u32.to_be_bytes());
                }
                let mut sender = CollectSender::default();
                e.drain(&mut sender, later);
                assert_eq!(
                    sender.sent,
                    vec![want.clone()],
                    "len={len} flag={flag} shared={shared}"
                );
                assert_eq!(e.cache.get(&key).unwrap().datagram[..], want[..]);
                if let Some(k) = keep {
                    assert_eq!(&k[..], &src[..], "the caller's shared handle was mutated");
                }
            }
        }
    }

    /// The cache and the emission queue must share the one allocation: that is
    /// the whole point of dropping the per-datagram copies.
    /// A uniquely-owned source is stamped where it lies: the cached fragment
    /// is the caller's own allocation, not a copy of it.
    #[test]
    fn a_uniquely_owned_source_is_stamped_in_place() {
        let now = Instant::now();
        let mut e = ReliableTileEmitter::new(now);
        let key = EmitKey::new(1, 0, 0, 0);
        let src = fake_source(1, 0, 0xAA);
        let src_ptr = src.as_ptr();
        e.submit_one(key, src, now, None, now);
        assert_eq!(e.cache.get(&key).unwrap().datagram.as_ptr(), src_ptr);
    }

    /// A shared source must be copied, never stamped through the other handle.
    #[test]
    fn a_shared_source_is_copied_and_the_callers_handle_is_untouched() {
        let now = Instant::now();
        let mut e = ReliableTileEmitter::new(now);
        let key = EmitKey::new(1, 0, 0, 0);
        let src = fake_source(1, 0, 0xAA);
        let keep = src.clone();
        let before = keep.to_vec();
        // Burn wire_seqs so the stamp would differ from the zeros in `src`.
        for _ in 0..3 {
            e.alloc.allocate();
        }
        e.submit_one(key, src, now, None, now);
        assert_eq!(&keep[..], &before[..]);
        assert_ne!(e.cache.get(&key).unwrap().datagram.as_ptr(), keep.as_ptr());
        assert_eq!(
            &e.cache.get(&key).unwrap().datagram[8..12],
            &3u32.to_be_bytes()
        );
    }

    #[test]
    fn cache_and_queue_share_one_allocation() {
        let now = Instant::now();
        let mut e = ReliableTileEmitter::new(now);
        e.set_tile_fec(true);
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0xAA), now, None, now);
        let cached_ptr = e.cache.get(&key).unwrap().datagram.as_ptr();
        let Some(Emission::Source(queued, _)) = e.next_emission(now) else {
            panic!("expected the source emission");
        };
        assert_eq!(queued.as_ptr(), cached_ptr, "queue holds its own copy");
        assert_eq!(
            e.group.sources[0].as_ptr(),
            cached_ptr,
            "the FEC group holds its own copy"
        );
    }

    #[test]
    fn submit_one_then_drain_emits_one_source() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let now = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0xAA), now, None, now);
        e.drain(&mut sender, now);
        assert_eq!(sender.sent.len(), 1);
        assert_eq!(e.stats.source_emitted, 1);
        assert!(e.cache.get(&key).is_some());
    }

    #[test]
    fn a_rejected_datagram_is_re_queued_not_dropped() {
        use crate::transport::reliable_emitter::traits::testing::RejectAfterSender;

        let mut e = ReliableTileEmitter::new(Instant::now());
        let now = Instant::now();
        for i in 0..4u32 {
            e.submit_one(
                EmitKey::new(1, 0, 0, i as u8),
                fake_source(1, 0, i as u8),
                now,
                None,
                now,
            );
        }

        // Accept two, refuse the rest.
        let mut sender = RejectAfterSender::new(2);
        e.drain(&mut sender, now);
        assert_eq!(sender.sent.len(), 2, "two accepted");
        assert_eq!(
            sender.rejected, 1,
            "the drain must stop at the first refusal, not keep offering \
             emissions the transport has no room for"
        );
        assert_eq!(e.stats.send_rejected, 1);

        // The refused emission and everything behind it must still be
        // queued. A sender that now accepts everything drains them.
        let mut sender2 = CollectSender::default();
        e.drain(&mut sender2, now);
        assert_eq!(
            sender2.sent.len(),
            2,
            "the refused emission and the one behind it were dropped rather \
             than re-queued"
        );
    }

    #[test]
    fn a_re_queued_datagram_keeps_its_place_and_its_wire_seq() {
        use crate::transport::reliable_emitter::traits::testing::RejectAfterSender;

        let mut e = ReliableTileEmitter::new(Instant::now());
        let now = Instant::now();
        for i in 0..3u32 {
            e.submit_one(
                EmitKey::new(1, 0, 0, i as u8),
                fake_source(1, 0, i as u8),
                now,
                None,
                now,
            );
        }

        // Refuse everything, then accept everything.
        let mut all_reject = RejectAfterSender::new(0);
        e.drain(&mut all_reject, now);
        assert!(all_reject.sent.is_empty());

        let mut accept = CollectSender::default();
        e.drain(&mut accept, now);
        assert_eq!(accept.sent.len(), 3, "all three survived the refusal");

        // wire_seq lives at bytes [8..12], stamped by `submit_one` before the
        // send. Re-queueing must not renumber or reorder: the queue is in
        // wire order and a refused emission goes back to the head it came
        // from. Out-of-order wire_seqs would desynchronise the receiver's
        // ledger and show up as phantom loss.
        let seqs: Vec<u32> = accept
            .sent
            .iter()
            .map(|d| u32::from_be_bytes(d[8..12].try_into().unwrap()))
            .collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(
            seqs, sorted,
            "wire order changed across a refusal: {seqs:?}"
        );
    }

    #[test]
    fn submit_one_stamps_wire_seq_into_datagram_bytes() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let now = Instant::now();
        e.submit_one(
            EmitKey::new(1, 0, 0, 0),
            fake_source(1, 0, 0),
            now,
            None,
            now,
        );
        e.submit_one(
            EmitKey::new(2, 0, 0, 0),
            fake_source(2, 0, 0),
            now,
            None,
            now,
        );
        e.drain(&mut sender, now);
        let s0 = &sender.sent[0];
        let s1 = &sender.sent[1];
        let ws0 = u32::from_be_bytes(s0[8..12].try_into().unwrap());
        let ws1 = u32::from_be_bytes(s1[8..12].try_into().unwrap());
        assert_eq!(ws0, 0);
        assert_eq!(ws1, 1);
    }

    /// Tile parity is opt-in. The same stream is run with and without it, so
    /// the zero cannot be a stream too short to complete a group.
    #[test]
    fn no_tile_parity_is_emitted_unless_asked_for() {
        let run = |fec: bool| {
            let mut e = ReliableTileEmitter::new(Instant::now());
            if fec {
                e.set_tile_fec(true);
            }
            let mut sender = CollectSender::default();
            let now = Instant::now();
            for i in 0..(FEC_GROUP_SIZE_K as u32 * 5) {
                e.submit_one(
                    EmitKey::new(i, 0, 0, 0),
                    fake_source(i, 0, 0),
                    now,
                    None,
                    now,
                );
            }
            e.drain(&mut sender, now);
            (e.stats.parity_emitted, sender.sent.len())
        };
        let (on_parity, on_sent) = run(true);
        let (off_parity, off_sent) = run(false);
        assert!(on_parity > 0, "premise: opt-in emits parity ({on_parity})");
        assert_eq!(off_parity, 0, "default emits no parity");
        assert_eq!(
            off_sent as u64 + on_parity,
            on_sent as u64,
            "the only difference is the parity datagrams"
        );
    }

    #[test]
    fn submit_k_sources_emits_one_parity_after_offset() {
        use crate::transport::reliable_emitter::FEC_GROUP_SIZE_K;
        let mut e = ReliableTileEmitter::new(Instant::now());
        e.set_tile_fec(true);
        let mut sender = CollectSender::default();
        let now = Instant::now();
        // Submit K sources for group 0, then K sources for group 1 (so the
        // offset-interleaved parity for group 0 is reachable).
        for i in 0..(FEC_GROUP_SIZE_K as u32 * 2) {
            e.submit_one(
                EmitKey::new(i, 0, 0, 0),
                fake_source(i, 0, 0),
                now,
                None,
                now,
            );
        }
        // Drain past wire_seq 20 by submitting one more source so allocator's
        // peek advances.
        e.submit_one(
            EmitKey::new(99, 0, 0, 0),
            fake_source(99, 0, 0),
            now,
            None,
            now,
        );
        e.drain(&mut sender, now);
        // At least one parity datagram should have been emitted
        assert_eq!(
            e.stats.parity_emitted, 1,
            "exactly one parity for first K sources"
        );
        // Total emitted: 2K+1 source + 1 parity
        assert_eq!(
            sender.sent.len(),
            (FEC_GROUP_SIZE_K as u32 * 2 + 1 + 1) as usize
        );
        // The parity datagram starts with TILE_PARITY_ENVELOPE (0x04)
        let parities: Vec<&Vec<u8>> = sender.sent.iter().filter(|b| b[0] == 0x04).collect();
        assert_eq!(parities.len(), 1);
    }

    #[test]
    fn on_ack_removes_cache_entry_and_bumps_ack_hit() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let now = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), now, None, now);
        e.drain(&mut sender, now);
        assert!(e.cache.get(&key).is_some());
        e.on_ack(&[key]);
        assert!(e.cache.get(&key).is_none());
        assert_eq!(e.stats.ack_hit, 1);
        assert_eq!(e.stats.ack_miss, 0);
    }

    #[test]
    fn on_ack_for_unknown_key_bumps_ack_miss() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        e.on_ack(&[EmitKey::new(99, 0, 0, 0)]);
        assert_eq!(e.stats.ack_miss, 1);
        assert_eq!(e.stats.ack_hit, 0);
    }

    #[test]
    fn tick_retransmits_what_the_loss_path_asked_for() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        e.drain(&mut sender, t0);
        assert_eq!(sender.sent.len(), 1);
        let entry_first_sent = e.cache.get(&key).unwrap().first_sent_at;
        assert_eq!(ask(&mut e, key), RetxRequest::Queued);
        assert_eq!(
            ask(&mut e, key),
            RetxRequest::AlreadyQueued,
            "once per wait"
        );
        let t1 = t0 + Duration::from_millis(1_100);
        ask(&mut e, key);
        e.tick(t1, usize::MAX);
        e.drain(&mut sender, t1);
        assert_eq!(sender.sent.len(), 2);
        let entry = e.cache.get(&key).unwrap();
        assert_eq!(entry.attempts, 1);
        assert!(entry.last_sent_at > entry_first_sent);
        assert_eq!(e.stats.loss_path_resends, 1);
        assert_eq!(e.stats.retransmit_attempts_total, 1);
    }

    /// `fake_source` with a generation stamped into the tile header.
    fn fake_source_gen(seq: u32, generation: u8) -> Bytes {
        let mut v = fake_source(seq, 0, 0).to_vec();
        v[19] = generation << 4;
        Bytes::from(v)
    }

    #[test]
    fn a_resend_for_a_generation_the_tile_has_left_is_counted_apart() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        e.submit_one(
            EmitKey::new(1, 0, 0, 0),
            fake_source_gen(1, 3),
            t0,
            None,
            t0,
        );
        // Same tile, newer generation, and the old pass is NOT cancelled: the
        // situation `cancel_for_tile` exists to prevent.
        e.submit_one(
            EmitKey::new(2, 0, 0, 0),
            fake_source_gen(2, 4),
            t0,
            None,
            t0,
        );
        assert_eq!(e.stats.generation_replacements, 1, "premise: tile moved on");
        e.drain(&mut sender, t0);
        ask(&mut e, EmitKey::new(1, 0, 0, 0));
        ask(&mut e, EmitKey::new(2, 0, 0, 0));
        e.tick(t0 + Duration::from_millis(1_100), usize::MAX);
        assert_eq!(e.stats.loss_path_resends, 2, "premise: both timers fired");
        assert_eq!(
            e.stats.loss_path_resends_superseded, 1,
            "only the pass of the generation the tile left is superseded"
        );
    }

    #[test]
    fn cancelling_a_tile_stops_its_old_generation_from_firing() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        e.submit_one(
            EmitKey::new(1, 0, 0, 0),
            fake_source_gen(1, 3),
            t0,
            None,
            t0,
        );
        e.cancel_for_tile(0, 0);
        e.submit_one(
            EmitKey::new(2, 0, 0, 0),
            fake_source_gen(2, 4),
            t0,
            None,
            t0,
        );
        e.drain(&mut sender, t0);
        ask(&mut e, EmitKey::new(1, 0, 0, 0));
        ask(&mut e, EmitKey::new(2, 0, 0, 0));
        e.tick(t0 + Duration::from_millis(1_100), usize::MAX);
        assert_eq!(
            e.stats.loss_path_resends, 1,
            "premise: the current pass fired"
        );
        assert_eq!(e.stats.loss_path_resends_superseded, 0);
    }

    /// Fire the RTO once for a single pass sent as wire_seq 0 (the retransmit
    /// is wire_seq 1).
    fn loss_path_resend_once() -> (ReliableTileEmitter, EmitKey) {
        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        e.drain(&mut sender, t0);
        ask(&mut e, key);
        let t1 = t0 + Duration::from_millis(1_100);
        ask(&mut e, key);
        e.tick(t1, usize::MAX);
        e.drain(&mut sender, t1);
        assert_eq!(e.stats.loss_path_resends, 1, "premise: the loss path fired");
        (e, key)
    }

    #[test]
    fn an_ack_for_the_original_after_a_resend_is_a_spurious_fire() {
        let (mut e, key) = loss_path_resend_once();
        e.note_ack_origin(key, 0);
        assert_eq!(
            (
                e.stats.loss_path_resends_spurious,
                e.stats.loss_path_resends_useful
            ),
            (1, 0)
        );
    }

    #[test]
    fn an_ack_for_the_retransmission_is_a_useful_fire() {
        let (mut e, key) = loss_path_resend_once();
        e.note_ack_origin(key, 1);
        assert_eq!(
            (
                e.stats.loss_path_resends_spurious,
                e.stats.loss_path_resends_useful
            ),
            (0, 1)
        );
    }

    #[test]
    fn a_later_ack_for_the_retransmission_overrides_an_earlier_spurious_verdict() {
        // A multi-fragment pass is acknowledged fragment by fragment: fragment
        // 0's original may be acknowledged first although fragment 1 was lost
        // and only the retransmission delivered it.
        let (mut e, key) = loss_path_resend_once();
        e.note_ack_origin(key, 0);
        assert_eq!(
            (
                e.stats.loss_path_resends_spurious,
                e.stats.loss_path_resends_useful
            ),
            (1, 0)
        );
        e.note_ack_origin(key, 1);
        assert_eq!(
            (
                e.stats.loss_path_resends_spurious,
                e.stats.loss_path_resends_useful
            ),
            (0, 1)
        );
        e.note_ack_origin(key, 1);
        assert_eq!(
            (
                e.stats.loss_path_resends_spurious,
                e.stats.loss_path_resends_useful
            ),
            (0, 1),
            "counted once"
        );
    }

    /// `loss_path_resends_original_first` is the Eifel verdict and it is final: the original
    /// arrived first, so the timer was early, whatever the retransmission's
    /// own acknowledgement does to `loss_path_resends_spurious` afterwards.
    #[test]
    fn the_original_acknowledged_first_stays_a_spurious_fire_after_the_retransmission_is_too() {
        let (mut e, key) = loss_path_resend_once();
        e.note_ack_origin(key, 0);
        assert_eq!(e.stats.loss_path_resends_original_first, 1);
        e.note_ack_origin(key, 1);
        assert_eq!(
            (
                e.stats.loss_path_resends_spurious,
                e.stats.loss_path_resends_useful
            ),
            (0, 1),
            "the older counter takes it back"
        );
        assert_eq!(
            e.stats.loss_path_resends_original_first, 1,
            "this one does not"
        );
    }

    #[test]
    fn the_retransmission_acknowledged_first_is_not_an_original_first_fire() {
        let (mut e, key) = loss_path_resend_once();
        e.note_ack_origin(key, 1);
        // The original's acknowledgement, late: the pass is already settled.
        e.note_ack_origin(key, 0);
        assert_eq!(e.stats.loss_path_resends_original_first, 0);
        assert_eq!(e.stats.loss_path_resends_useful, 1);
    }

    #[test]
    fn a_fire_after_a_spurious_verdict_is_counted_too_and_wrap_is_handled() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        let key = EmitKey::new(1, 0, 0, 0);
        // First retransmission at wire_seq 2; the original (u32::MAX - 1, i.e.
        // 4 earlier across the wrap) is acknowledged.
        e.note_loss_path_resend(key, Some(2), false);
        e.note_ack_origin(key, u32::MAX - 1);
        assert_eq!(
            e.stats.loss_path_resends_spurious, 1,
            "pre-wrap original is older"
        );
        e.note_loss_path_resend(key, Some(9), false);
        assert_eq!(e.stats.loss_path_resends_spurious, 2);
    }

    #[test]
    fn the_resend_bookkeeping_is_bounded_and_counts_its_resets() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        for i in 0..(RESEND_ORIGIN_CAP as u32 + 5) {
            e.note_loss_path_resend(EmitKey::new(i, 0, 0, 0), Some(i), false);
        }
        assert!(e.resend_origin.len() <= RESEND_ORIGIN_CAP);
        assert_eq!(e.stats.resend_origin_resets, 1);
        // The newest fire is still attributable after the reset.
        let newest = EmitKey::new(RESEND_ORIGIN_CAP as u32 + 4, 0, 0, 0);
        e.note_ack_origin(newest, RESEND_ORIGIN_CAP as u32 + 4);
        assert_eq!(e.stats.loss_path_resends_useful, 1);
    }

    #[test]
    fn clear_cache_forgets_resend_history() {
        let (mut e, key) = loss_path_resend_once();
        e.clear_cache();
        e.note_ack_origin(key, 0);
        assert_eq!(
            (
                e.stats.loss_path_resends_spurious,
                e.stats.loss_path_resends_useful
            ),
            (0, 0)
        );
    }

    #[test]
    fn an_ack_for_a_pass_that_never_fired_counts_neither() {
        let (mut e, _) = loss_path_resend_once();
        e.note_ack_origin(EmitKey::new(9, 0, 0, 0), 0);
        assert_eq!(
            (
                e.stats.loss_path_resends_spurious,
                e.stats.loss_path_resends_useful
            ),
            (0, 0)
        );
    }

    #[test]
    fn retransmissions_are_split_by_cause() {
        let (e, _) = loss_path_resend_once();
        assert_eq!(
            (
                e.stats.retransmit_attempts_total,
                e.stats.retransmit_by_nack
            ),
            (1, 0)
        );
        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        e.drain(&mut sender, t0);
        e.on_nack(&[key], t0);
        e.tick(t0, usize::MAX);
        assert_eq!(
            (
                e.stats.retransmit_attempts_total,
                e.stats.retransmit_by_nack,
                e.stats.loss_path_resends
            ),
            (1, 1, 0)
        );
    }

    /// Offered-load accounting: every datagram handed to the transport is
    /// billed to exactly one class, so the classes (plus framing) sum to the
    /// bytes handed over. A scripted run that exercises all five emitter
    /// classes; the premise asserts each is non-zero, so a class the emitter
    /// stopped labelling (or an unbilled one) cannot hide behind a sum that
    /// still balances.
    #[test]
    fn offered_bytes_by_class_sum_to_the_bytes_handed_to_the_transport() {
        use crate::transport::instruments::OfferedBytes;
        use crate::transport::reliable_emitter::FEC_GROUP_SIZE_K;

        let mut e = ReliableTileEmitter::new(Instant::now());
        e.set_tile_fec(true);
        let mut offered = OfferedBytes::default();
        let mut handed = 0u64;
        let t0 = Instant::now();
        let mut hand_over = |e: &mut ReliableTileEmitter, now: Instant| {
            while let Some(em) = e.next_emission(now) {
                let class = em.class();
                let len = match &em {
                    Emission::Source(b, _) | Emission::Parity(b, _) => b.len(),
                };
                // A 2-byte framing prefix per datagram, as the WebTransport
                // layer adds a varint.
                offered.bill(class, len, 2);
                handed += (len + 2) as u64;
                let ws = em.wire_seq();
                assert!(e.settle_emission(em, SendOutcome::Sent));
                // The test's wire takes it at once.
                if let Some(ws) = ws {
                    e.on_departed(ws, now);
                }
            }
        };

        // First sends: critical (pass 0) and refinement (pass 5), enough of
        // them to complete FEC groups and release parity.
        let n = FEC_GROUP_SIZE_K as u32 * 2 + 1;
        for i in 0..n {
            let pass = if i % 3 == 0 { 5 } else { 0 };
            e.submit_one(
                EmitKey::new(i, 0, 0, pass),
                fake_source(i, 0, 0),
                t0,
                None,
                t0,
            );
        }
        hand_over(&mut e, t0);
        // A NACK for one pass.
        e.on_nack(&[EmitKey::new(1, 0, 0, 0)], t0);
        e.tick_budgeted(t0, usize::MAX, usize::MAX);
        hand_over(&mut e, t0);
        // The loss path asks for four of the passes still unacknowledged.
        for i in 2..6 {
            ask(
                &mut e,
                EmitKey::new(i, 0, 0, if i % 3 == 0 { 5 } else { 0 }),
            );
        }
        let t1 = t0 + Duration::from_secs(10);
        e.tick_budgeted(t1, 4, usize::MAX);
        hand_over(&mut e, t1);

        for class in [
            WireClass::Critical,
            WireClass::Refinement,
            WireClass::NackRepair,
            WireClass::LossRetransmit,
            WireClass::Parity,
        ] {
            assert!(offered.of(class) > 0, "{class:?} never billed");
        }
        assert!(e.stats.loss_path_resends > 0 && e.stats.retransmit_by_nack > 0);
        assert!(e.stats.parity_emitted > 0);
        assert_eq!(
            offered.total(),
            handed,
            "classes + framing must account for every byte handed over"
        );
        // Labels, checked against sizes known independently of the billing:
        // every source datagram here is 25 bytes and the parity envelope is
        // recognised by its first byte, not by its class.
        let src = 25u64;
        let crit_sends = (0..n).filter(|i| i % 3 != 0).count() as u64;
        let refine_sends = (0..n).filter(|i| i % 3 == 0).count() as u64;
        assert_eq!(offered.of(WireClass::Critical), crit_sends * src);
        assert_eq!(offered.of(WireClass::Refinement), refine_sends * src);
        assert_eq!(
            offered.of(WireClass::NackRepair),
            e.stats.retransmit_by_nack * src
        );
        assert_eq!(
            offered.of(WireClass::LossRetransmit),
            e.stats.loss_path_resends * src
        );
        assert_eq!(
            offered.datagrams[WireClass::Parity.index()],
            e.stats.parity_emitted
        );
        // And per datagram, not just in aggregate.
        let datagrams: u64 = offered.datagrams.iter().sum();
        assert_eq!(
            datagrams,
            e.stats.source_emitted + e.stats.retransmit_attempts_total + e.stats.parity_emitted
        );
    }

    /// A scheduler re-send enters through `submit_one` like a first send but
    /// is labelled apart, so it is not billed as new critical content.
    #[test]
    fn a_scheduler_retry_is_labelled_apart_from_a_first_send() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        let t0 = Instant::now();
        let item = |i| (EmitKey::new(i, 0, 0, 0), fake_source(i, 0, 0), t0, None);
        e.submit_batch_marking_retries(vec![item(1), item(2), item(3)], &[false, true, false], t0);
        let mut classes = Vec::new();
        while let Some(em) = e.next_emission(t0) {
            classes.push(em.class());
            assert!(e.settle_emission(em, SendOutcome::Sent));
        }
        assert_eq!(
            classes,
            vec![
                WireClass::Critical,
                WireClass::SchedulerRetry,
                WireClass::Critical
            ]
        );
    }

    /// The [H3-SRV] retransmit line needs an FNV pass over the first
    /// fragment. That must only be paid when something is listening at TRACE
    /// on `emitter.h3`: this runs once per retransmit, and a storm is
    /// exactly when retransmits are many.
    #[test]
    fn the_retransmit_fingerprint_is_only_computed_when_traced() {
        fn one_loss_retransmit() {
            let mut e = ReliableTileEmitter::new(Instant::now());
            let mut sender = CollectSender::default();
            let t0 = Instant::now();
            let key = EmitKey::new(1, 0, 0, 0);
            e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
            e.drain(&mut sender, t0);
            let t1 = t0 + Duration::from_millis(1_100);
            ask(&mut e, key);
            e.tick(t1, usize::MAX);
            e.drain(&mut sender, t1);
            assert_eq!(
                e.stats.retransmit_attempts_total, 1,
                "premise: it retransmitted"
            );
        }

        let _pin = crate::tracing_test_support::pin_dispatcher_registry();
        // Nobody listening: no fingerprint.
        FP_COMPUTED.with(|c| c.set(0));
        tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
            one_loss_retransmit()
        });
        assert_eq!(FP_COMPUTED.with(|c| c.get()), 0);

        // Listening at TRACE: fingerprint computed and the line is emitted.
        #[derive(Clone, Default)]
        struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Sink;
            fn make_writer(&'a self) -> Sink {
                self.clone()
            }
        }
        let sink = Sink::default();
        let sub = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(sink.clone())
            .finish();
        tracing::subscriber::with_default(sub, one_loss_retransmit);
        assert_eq!(FP_COMPUTED.with(|c| c.get()), 1);
        let out = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(out.contains("[H3-SRV] retransmit"), "got: {out}");
    }

    #[test]
    fn a_retransmission_gets_its_own_wire_seq() {
        // `wire_seq` has to identify a *transmission*, not a fragment of a
        // pass. Both retransmit paths used to re-queue the cached bytes with
        // the original stamp intact, which left an acknowledgement unable to
        // say which send it referred to — the ambiguity that made a
        // Karn-style filter necessary, and then untenable.
        fn wire_seq_of(b: &[u8]) -> u32 {
            u32::from_be_bytes([b[8], b[9], b[10], b[11]])
        }

        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        e.drain(&mut sender, t0);
        assert_eq!(sender.sent.len(), 1);
        let first = wire_seq_of(&sender.sent[0]);

        // Loss-path retransmit.
        let t1 = t0 + Duration::from_millis(1_100);
        ask(&mut e, key);
        e.tick(t1, usize::MAX);
        e.drain(&mut sender, t1);
        assert_eq!(sender.sent.len(), 2);
        let second = wire_seq_of(&sender.sent[1]);
        assert_ne!(
            second, first,
            "an RTO retransmission must not reuse the original wire_seq"
        );

        // NACK retransmit.
        let t2 = t1 + Duration::from_millis(10);
        e.on_nack(&[key], t2);
        e.tick(t2, usize::MAX);
        e.drain(&mut sender, t2);
        assert_eq!(sender.sent.len(), 3);
        let third = wire_seq_of(&sender.sent[2]);
        assert_ne!(
            third, second,
            "a NACK retransmission must not reuse the previous wire_seq"
        );
        assert_ne!(third, first);
    }

    #[test]
    fn retransmit_drops_probe_cluster_membership_but_keeps_the_entry() {
        // A probe cluster's send rate is its bytes divided by the span
        // between its first and last send. A retransmit re-sends with a
        // fresh emit timestamp, so leaving the cluster id attached stretches
        // that span across the retransmit and corrupts the rate for every
        // packet in the cluster -- not just this one.
        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        let tag = ProbeTag {
            id: 7,
            min_probes: 5,
            min_bytes: 11_250,
            bytes_sent_before: 0,
        };
        e.submit_one(key, fake_source(1, 0, 0), t0, Some(tag), t0);
        e.drain(&mut sender, t0);
        assert_eq!(
            e.cache.get(&key).unwrap().probe.map(|p| p.id),
            Some(7),
            "the first transmission is a genuine member of cluster 7"
        );

        let t1 = t0 + Duration::from_millis(1_100);
        ask(&mut e, key);
        e.tick(t1, usize::MAX);
        e.drain(&mut sender, t1);

        let entry = e.cache.get(&key).expect(
            "the entry must survive: dropping the sample entirely is Karn's \
             algorithm, which starved the delay-based estimator and was reverted",
        );
        assert_eq!(entry.attempts, 1, "it really was retransmitted");
        assert!(
            entry.probe.is_none(),
            "a retransmission belongs to no probe cluster"
        );
    }

    #[test]
    fn tick_keeps_retransmitting_after_many_attempts() {
        // Retirement was removed: the emitter keeps firing RTO retransmits
        // indefinitely until ACKed or cancelled. Verify that after N ticks
        // the entry is still present.
        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        e.drain(&mut sender, t0);
        let mut tn = t0;
        for _ in 0..10 {
            tn += Duration::from_millis(500);
            ask(&mut e, key);
            e.tick(tn, usize::MAX);
            e.drain(&mut sender, tn);
        }
        // Entry must still be in the cache — no retirement.
        assert!(e.cache.get(&key).is_some(), "entry must not be retired");
    }

    #[test]
    fn tick_skips_already_acked_entries() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        e.drain(&mut sender, t0);
        e.on_ack(&[key]);
        let t1 = t0 + Duration::from_millis(1_100);
        ask(&mut e, key);
        e.tick(t1, usize::MAX);
        e.drain(&mut sender, t1);
        assert_eq!(sender.sent.len(), 1, "no retransmit after ACK");
        assert_eq!(e.stats.loss_path_resends, 0);
    }

    #[test]
    fn on_nack_reemits_the_pass() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        e.drain(&mut sender, t0);
        assert_eq!(sender.sent.len(), 1);
        e.on_nack(&[key], t0);
        e.tick(t0, usize::MAX);
        e.drain(&mut sender, t0);
        assert_eq!(sender.sent.len(), 2);
        assert_eq!(e.stats.nack_hit, 1);
        assert_eq!(e.cache.get(&key).unwrap().attempts, 1);
    }

    #[test]
    fn retire_stuck_removes_only_entries_past_the_age() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        e.submit_one(
            EmitKey::new(1, 0, 0, 0),
            fake_source(1, 0, 0xAA),
            t0,
            None,
            t0,
        );
        // 10 s later, a second entry.
        let t10 = t0 + Duration::from_secs(10);
        e.submit_one(
            EmitKey::new(2, 0, 1, 0),
            fake_source(1, 0, 0xBB),
            t10,
            None,
            t10,
        );
        assert_eq!(e.pending_cache_entries(), 2);

        // At t0+20 with a 15 s threshold, only the first is past it.
        let retired = e.retire_stuck(t0 + Duration::from_secs(20), Duration::from_secs(15));
        assert_eq!(retired, vec![EmitKey::new(1, 0, 0, 0)]);
        assert_eq!(e.pending_cache_entries(), 1);
        assert_eq!(e.stats.stuck_entries_retired, 1);
    }

    #[test]
    fn retire_stuck_is_exclusive_at_the_boundary() {
        // `> older_than`, not `>=`: an entry exactly at the threshold has not
        // yet exceeded it, and a boundary that retires early is the direction
        // that abandons still-live content.
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        e.submit_one(
            EmitKey::new(1, 0, 0, 0),
            fake_source(1, 0, 0xAA),
            t0,
            None,
            t0,
        );
        assert!(e
            .retire_stuck(t0 + Duration::from_secs(30), Duration::from_secs(30))
            .is_empty());
        assert_eq!(e.pending_cache_entries(), 1);
        assert_eq!(
            e.retire_stuck(
                t0 + Duration::from_secs(30) + Duration::from_micros(1),
                Duration::from_secs(30)
            )
            .len(),
            1
        );
    }

    #[test]
    fn retire_stuck_bounds_the_retransmit_rate_that_caused_the_storm() {
        // The defect this exists for: entries that are never acknowledged live
        // forever, and each costs one retransmit per RTO_BACKOFF_MAX. Field
        // measurement was 2697 pending -> 549 retransmits/s. Without
        // retirement this cache stays full and the rate scales with it.
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        for i in 0..500u32 {
            e.submit_one(
                EmitKey::new(i, 0, 0, 0),
                fake_source(1, 0, 0xCC),
                t0,
                None,
                t0,
            );
        }
        assert_eq!(e.pending_cache_entries(), 500);

        // Nothing acknowledges them. A minute later they are all stuck.
        let retired = e.retire_stuck(t0 + Duration::from_secs(60), Duration::from_secs(30));
        assert_eq!(retired.len(), 500);
        assert_eq!(
            e.pending_cache_entries(),
            0,
            "an unacknowledged cache must not grow without bound -- this is the \
             immortal-entry defect that starved CDF 5/3 in the field"
        );
    }

    #[test]
    fn retire_stuck_leaves_a_healthy_cache_alone() {
        // Guards the direction that would be worse than the bug: retiring
        // entries that are still legitimately in flight is what 0aa97de
        // removed the attempt cap for.
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        for i in 0..100u32 {
            e.submit_one(
                EmitKey::new(i, 0, 0, 0),
                fake_source(1, 0, 0xCC),
                t0,
                None,
                t0,
            );
        }
        assert!(e
            .retire_stuck(t0 + Duration::from_secs(5), Duration::from_secs(30))
            .is_empty());
        assert_eq!(e.pending_cache_entries(), 100);
        assert_eq!(e.stats.stuck_entries_retired, 0);
    }

    #[test]
    fn on_nack_for_unknown_key_bumps_nack_miss() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        e.on_nack(&[EmitKey::new(99, 0, 0, 0)], Instant::now());
        assert_eq!(e.stats.nack_miss, 1);
    }

    /// The field defect, reproduced at the unit level.
    ///
    /// A tile's CDF 5/3 passes are emitted across different frames, so pass 11
    /// is cached under the frame it went out in. A coverage NACK for pass 11
    /// cannot name that frame -- the client never received the datagram -- so
    /// it sends the tile's last-known `frame_seq`, which is the frame of the
    /// last pass that *did* arrive.
    ///
    /// Before the content index every such NACK missed, the pass was never
    /// resent, and the client gave up after MAX_TAIL_SWEEP_ATTEMPTS. In the
    /// field that read as `nack_hit=0 / nack_miss=6764` from a cold start and
    /// left 478 of 2040 tiles stranded without their three finest bit-planes.
    #[test]
    fn a_coverage_nack_naming_a_stale_frame_seq_still_retransmits() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        let mut sender = CollectSender::default();

        // Pass 10 went out in frame 40; pass 11 in frame 41. The client holds
        // frame 40, because that is the newest pass that reached it.
        let arrived = EmitKey::new(40, 7, 9, 10);
        let missing = EmitKey::new(41, 7, 9, 11);
        e.submit_one(arrived, fake_source(40, 0, 0xA0), t0, None, t0);
        e.submit_one(missing, fake_source(41, 0, 0xB1), t0, None, t0);
        e.drain(&mut sender, t0);
        let sent_before = sender.sent.len();

        // What the client actually puts on the wire: pass 11, frame 40.
        let as_the_client_names_it = EmitKey::new(40, 7, 9, 11);
        assert!(
            e.cache.get(&as_the_client_names_it).is_none(),
            "precondition: the key the client can name is not the key the \
             pass is cached under -- without that there is nothing to fix"
        );

        e.on_nack(&[as_the_client_names_it], t0);

        assert_eq!(
            e.stats.nack_miss, 0,
            "a NACK for a pass that IS cached must not be recorded as a miss \
             just because the client could not name its frame"
        );
        assert_eq!(e.stats.nack_hit, 1, "the pass must be resent");
        assert_eq!(
            e.stats.nack_resolved_by_content, 1,
            "and resolved through the content index, not by luck"
        );

        e.tick(t0, usize::MAX);
        e.drain(&mut sender, t0);
        assert!(
            sender.sent.len() > sent_before,
            "a resolved NACK must put bytes back on the wire"
        );
    }

    /// The fallback must not invent a hit: a tile-pass that was never cached
    /// still misses.
    #[test]
    fn the_content_fallback_does_not_resurrect_uncached_passes() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        let mut sender = CollectSender::default();
        e.submit_one(
            EmitKey::new(40, 7, 9, 10),
            fake_source(40, 0, 0xA0),
            t0,
            None,
            t0,
        );
        e.drain(&mut sender, t0);

        // Same tile, a pass that was never emitted.
        e.on_nack(&[EmitKey::new(40, 7, 9, 13)], t0);
        // A tile that does not exist at all.
        e.on_nack(&[EmitKey::new(40, 3, 3, 0)], t0);

        assert_eq!(e.stats.nack_miss, 2);
        assert_eq!(e.stats.nack_hit, 0);
        assert_eq!(e.stats.nack_resolved_by_content, 0);
    }

    /// Retiring a superseded emission must not unhook the live one.
    ///
    /// The same tile-pass can be emitted more than once -- a re-send, or a
    /// later refinement sweep -- and the content index points at the newest.
    /// When the older entry is then ACKed or retired, clearing the index
    /// unconditionally would strand the newer entry: still cached, but with
    /// no content-addressable route, so every coverage NACK for it would miss
    /// again. That is why the removal is guarded on the slot still pointing
    /// at the key being removed.
    #[test]
    fn retiring_a_superseded_emission_leaves_the_newer_one_reachable() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        let mut sender = CollectSender::default();

        let older = EmitKey::new(41, 7, 9, 11);
        let newer = EmitKey::new(50, 7, 9, 11);
        e.submit_one(older, fake_source(41, 0, 0xB1), t0, None, t0);
        e.submit_one(newer, fake_source(50, 0, 0xB2), t0, None, t0);
        e.drain(&mut sender, t0);

        // The older transmission is acknowledged and leaves the cache.
        assert!(e.cache.remove(&older).is_some());
        assert!(
            e.cache.get(&newer).is_some(),
            "precondition: the newer emission is still cached"
        );

        // A coverage NACK still cannot name either frame.
        e.on_nack(&[EmitKey::new(40, 7, 9, 11)], t0);

        assert_eq!(
            e.stats.nack_miss, 0,
            "removing the superseded entry must not take the live entry's \
             content route with it"
        );
        assert_eq!(e.stats.nack_hit, 1);
        assert_eq!(e.stats.nack_resolved_by_content, 1);
    }

    /// `holds_pass` tracks the cache through every way an entry leaves it.
    #[test]
    fn holds_pass_follows_the_cache() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        let mut sender = CollectSender::default();
        assert!(!e.holds_pass(7, 9, 0));

        let key = EmitKey::new(40, 7, 9, 0);
        e.submit_one(key, fake_source(40, 0, 0xA0), t0, None, t0);
        e.drain(&mut sender, t0);
        assert!(e.holds_pass(7, 9, 0));
        assert!(!e.holds_pass(7, 9, 1), "a different pass of the same tile");
        assert!(!e.holds_pass(9, 7, 0), "a different tile");

        e.on_ack(&[key]);
        assert!(!e.holds_pass(7, 9, 0), "acknowledged");

        e.submit_one(key, fake_source(40, 0, 0xA0), t0, None, t0);
        e.cancel_for_tile(7, 9);
        assert!(!e.holds_pass(7, 9, 0), "superseded");

        e.submit_one(key, fake_source(40, 0, 0xA0), t0, None, t0);
        e.clear_cache();
        assert!(!e.holds_pass(7, 9, 0), "wiped on connection loss");

        e.submit_one(key, fake_source(40, 0, 0xA0), t0, None, t0);
        e.retire_stuck(t0 + Duration::from_secs(60), Duration::from_secs(30));
        assert!(!e.holds_pass(7, 9, 0), "retired by age");
    }

    #[test]
    fn cancel_for_tile_clears_all_gens_and_blocks_retransmit() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let t0 = Instant::now();
        let k1 = EmitKey::new(1, 5, 5, 0);
        let k2 = EmitKey::new(2, 5, 5, 1);
        let k3 = EmitKey::new(1, 5, 6, 0); // different tile
        e.submit_one(k1, fake_source(1, 0, 0), t0, None, t0);
        e.submit_one(k2, fake_source(2, 0, 0), t0, None, t0);
        e.submit_one(k3, fake_source(3, 0, 0), t0, None, t0);
        e.drain(&mut sender, t0);
        e.cancel_for_tile(5, 5);
        assert!(e.cache.get(&k1).is_none());
        assert!(e.cache.get(&k2).is_none());
        assert!(e.cache.get(&k3).is_some());
        // Ask for all three — nothing to send for the cancelled, one for k3.
        let t1 = t0 + Duration::from_millis(1_100);
        for k in [k1, k2, k3] {
            ask(&mut e, k);
        }
        e.tick(t1, usize::MAX);
        e.drain(&mut sender, t1);
        assert_eq!(sender.sent.len(), 4, "3 initial + 1 retransmit for k3 only");
    }

    /// A NACK is a request, not a send.
    ///
    /// `on_nack` used to push the fragment straight onto the emission queue,
    /// and a test here pinned that as "every NACK produces a re-emission".
    /// That is the retransmit storm stated as a requirement: the client's
    /// tail sweep repeats its request for every missing pass of every tile
    /// each 500 ms, so on a 2040-tile screen NACK-driven resends alone
    /// outran an 8 Mbit link -- 15,808 of them in the nine seconds after a
    /// full-screen change, on top of 14,081 from the RTO, filling quinn's
    /// 16 MB datagram buffer and leaving the scheduler no room to emit the
    /// passes the client was actually asking for.
    #[test]
    fn a_nack_waits_for_the_retransmit_budget() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        let mut sender = CollectSender::default();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        e.drain(&mut sender, t0);
        assert_eq!(sender.sent.len(), 1);

        e.on_nack(&[key], t0);
        e.drain(&mut sender, t0);
        assert_eq!(
            sender.sent.len(),
            1,
            "a NACK must not reach the wire on its own; nothing has granted \
             it any budget yet"
        );

        assert_eq!(e.tick_budgeted(t0, usize::MAX, 0), 0);
        e.drain(&mut sender, t0);
        assert_eq!(sender.sent.len(), 1, "a zero budget sends nothing");

        let spent = e.tick_budgeted(t0, usize::MAX, usize::MAX);
        e.drain(&mut sender, t0);
        assert_eq!(sender.sent.len(), 2, "and it is sent once budget allows");
        assert_eq!(spent, sender.sent[1].len(), "billed at its wire size");
    }

    /// The framing the transport adds is part of what a retransmission costs
    /// on the wire, so the bound is in the unit the caller bills in.
    #[test]
    fn retransmissions_are_billed_with_the_transports_framing() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        e.set_wire_overhead(2);
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        e.on_nack(&[key], t0);
        let spent = e.tick_budgeted(t0, usize::MAX, usize::MAX);
        let mut sender = CollectSender::default();
        e.drain(&mut sender, t0);
        let retx = sender.sent.last().unwrap();
        assert_eq!(spent, retx.len() + 2);
    }

    /// The two halves the admission pass serves in priority order are the
    /// halves `tick_budgeted` is made of: NACK repair first, then the timer.
    #[test]
    fn the_nack_half_and_the_loss_half_are_what_tick_budgeted_does() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        let (k0, k1) = (EmitKey::new(1, 0, 0, 0), EmitKey::new(1, 1, 0, 0));
        e.submit_one(k0, fake_source(1, 0, 0), t0, None, t0);
        e.submit_one(k1, fake_source(2, 0, 0), t0, None, t0);
        for t in e.take_transmissions() {
            e.on_departed(t.wire_seq, t0);
        }
        let late = t0 + Duration::from_secs(10);
        e.on_nack(&[k0], late);
        ask(&mut e, k0);
        ask(&mut e, k1);
        let mut n = 0;
        let nacks = e.tick_nacks_budgeted(late, usize::MAX, usize::MAX, &mut n);
        assert_eq!(
            (n, e.stats.retransmit_by_nack, e.stats.loss_path_resends),
            (1, 1, 0)
        );
        let loss = e.tick_loss_budgeted(late, usize::MAX, usize::MAX, &mut n);
        // k0's NACK repair is the newest transmission of k0 now, but the
        // request named the one before it; k1's is current.
        assert!(nacks > 0 && loss > 0);
        // The count carries across, so one `max_retransmits` bounds both.
        assert_eq!(n, 1 + e.stats.loss_path_resends as usize);
    }

    /// An emission the admission gate holds back is not a failed send.
    #[test]
    fn a_held_emission_goes_back_to_the_head_without_counting_a_rejection() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        e.submit_one(EmitKey::new(1, 0, 0, 0), fake_source(1, 0, 0), t0, None, t0);
        e.submit_one(EmitKey::new(1, 1, 0, 0), fake_source(2, 0, 0), t0, None, t0);
        assert_eq!(e.emission_queue_len(), 2);
        let first = e.next_emission(t0).unwrap();
        let first_bytes = match &first {
            Emission::Source(b, _) => b.clone(),
            Emission::Parity(b, _) => b.clone(),
        };
        e.hold_emission(first);
        assert_eq!(e.emission_queue_len(), 2);
        assert_eq!(e.stats.send_rejected, 0);
        match e.next_emission(t0).unwrap() {
            Emission::Source(b, _) => assert_eq!(b, first_bytes, "same datagram, same place"),
            Emission::Parity(..) => panic!("parity ahead of its sources"),
        }
    }

    /// The client repeats a request until the pass arrives. Requests that
    /// pile up while the first is still waiting for budget are one
    /// retransmission, not one each.
    #[test]
    fn a_repeated_nack_is_one_retransmission() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        let mut sender = CollectSender::default();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        e.drain(&mut sender, t0);

        for _ in 0..10 {
            e.on_nack(&[key], t0);
        }
        e.tick_budgeted(t0, usize::MAX, usize::MAX);
        e.drain(&mut sender, t0);

        assert_eq!(sender.sent.len(), 2, "1 original + 1 resend, not 1 + 10");
        assert_eq!(e.stats.nack_hit, 1);
        assert_eq!(e.stats.nack_deduped, 9);
        assert!(e.cache.get(&key).is_some(), "still live until acknowledged");

        // Once it has gone out the request is spent; asking again is a new
        // request and is honoured.
        e.on_nack(&[key], t0);
        e.tick_budgeted(t0, usize::MAX, usize::MAX);
        e.drain(&mut sender, t0);
        assert_eq!(sender.sent.len(), 3);
    }

    /// A queued request for a pass that has since been acknowledged or
    /// superseded is dropped, and costs no budget.
    #[test]
    fn a_queued_nack_for_a_retired_pass_is_dropped() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        let mut sender = CollectSender::default();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        e.drain(&mut sender, t0);

        e.on_nack(&[key], t0);
        e.on_ack(&[key]);
        assert_eq!(e.tick_budgeted(t0, usize::MAX, usize::MAX), 0);
        e.drain(&mut sender, t0);
        assert_eq!(sender.sent.len(), 1);
    }

    /// The byte budget bounds loss-path retransmission, and what it defers stays queued.
    #[test]
    fn the_byte_budget_bounds_loss_replay_and_defers_the_rest() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        let mut sender = CollectSender::default();
        for i in 0..100u32 {
            e.submit_one(EmitKey::new(i, 0, 0, 0), fake_source(i, 0, 0), t0, None, t0);
        }
        e.drain(&mut sender, t0);
        let one = sender.sent[0].len();

        for i in 0..100u32 {
            ask(&mut e, EmitKey::new(i, 0, 0, 0));
        }
        let t1 = t0 + e.pto() + Duration::from_millis(1);
        let spent = e.tick_budgeted(t1, usize::MAX, 10 * one);
        assert_eq!(e.stats.loss_path_resends, 10);
        assert_eq!(spent, 10 * one);

        let spent = e.tick_budgeted(t1, usize::MAX, usize::MAX);
        assert_eq!(e.stats.loss_path_resends, 100);
        assert_eq!(spent, 90 * one);
    }

    /// What was measured on a connection that is gone must not carry over: its
    /// PTO backoff would delay the next connection's first loss recovery, its
    /// srtt would set the next one's deadlines, and its newest ACK would hold
    /// back the next one's passes. `clear_cache` (a dead connection, an
    /// eviction) resets all three. Removing the reset makes this fail.
    #[test]
    fn a_dead_connections_estimate_and_backoff_do_not_carry_over() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        e.rtt.on_sample(Duration::from_millis(40));
        let base = pto_for_attempt(&e.rtt, 0);
        e.note_ack(t0);
        e.on_departed(1, t0);
        e.note_pto_fire(t0);
        e.note_pto_fire(t0 + base * 2);
        assert_eq!(
            e.rtt.pto_count(),
            2,
            "premise: backoff from the old connection"
        );
        assert!(e.last_ack_at.is_some() && e.latest_departure_at.is_some());

        e.clear_cache();

        assert_eq!(e.rtt.pto_count(), 0);
        assert!(!e.rtt.has_sample());
        assert!(e.last_ack_at.is_none() && e.latest_departure_at.is_none());
        assert!(e.last_pto_fire_at.is_none());
        // The next connection's first pass is probed on the initial RTT, not
        // on the old 40 ms estimate with its backoff.
        let t1 = t0 + Duration::from_secs(2);
        let key = EmitKey::new(2, 0, 0, 0);
        e.submit_one(key, fake_source(2, 0, 0), t1, None, t1);
        let ws = e.take_transmissions()[0].wire_seq;
        e.on_departed(ws, t1);
        let initial = pto_for_attempt(&RttEstimator::new(), 0);
        assert_eq!(e.pto_deadline(), Some(t1 + initial));
        assert!(initial > base * 3, "premise: the two estimates differ");
    }

    const MS_1: Duration = Duration::from_millis(1);

    // ---- one repair per loss, whoever asks first ----

    fn one_sent_pass() -> (ReliableTileEmitter, EmitKey, Instant) {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        e.take_transmissions();
        (e, key, t0)
    }

    /// The loss verdict was queued first, then the client's NACK for the same
    /// pass. The NACK is served first and becomes the pass's newest
    /// transmission; the verdict's entry is stale when its turn comes and
    /// sends nothing. One retransmission, not two.
    #[test]
    fn a_nack_after_a_loss_verdict_is_the_only_repair() {
        let (mut e, key, t0) = one_sent_pass();
        assert_eq!(ask(&mut e, key), RetxRequest::Queued);
        e.on_nack(&[key], t0);
        let mut n = 0;
        e.tick_nacks_budgeted(t0, usize::MAX, usize::MAX, &mut n);
        e.tick_loss_budgeted(t0, usize::MAX, usize::MAX, &mut n);
        assert_eq!((n, e.stats.retransmit_attempts_total), (1, 1));
        assert_eq!(
            (e.stats.retransmit_by_nack, e.stats.loss_path_resends),
            (1, 0)
        );
        assert_eq!(
            e.stats.verdict_newer_in_flight, 1,
            "the stale entry is counted"
        );
    }

    /// The other order: the NACK waits for budget when the verdict comes. The
    /// verdict is not queued behind it.
    #[test]
    fn a_loss_verdict_after_a_queued_nack_is_not_a_second_repair() {
        let (mut e, key, t0) = one_sent_pass();
        e.on_nack(&[key], t0);
        assert_eq!(ask(&mut e, key), RetxRequest::AlreadyQueued);
        let mut n = 0;
        e.tick_nacks_budgeted(t0, usize::MAX, usize::MAX, &mut n);
        e.tick_loss_budgeted(t0, usize::MAX, usize::MAX, &mut n);
        assert_eq!((n, e.stats.retransmit_attempts_total), (1, 1));
        assert_eq!(e.stats.loss_path_resends, 0);
    }

    /// After `LOSS_RETX_MAX_ATTEMPTS` resends a verdict leaves the pass to the
    /// probe: no more once-per-round-trip resends (and reports) for a pass the
    /// client never acknowledges. The probe is not limited.
    #[test]
    fn a_pass_resent_three_times_is_left_to_the_probe() {
        let (mut e, key, t0) = one_sent_pass();
        for round in 0..LOSS_RETX_MAX_ATTEMPTS {
            assert_eq!(ask(&mut e, key), RetxRequest::Queued, "resend {round}");
            e.tick(t0, usize::MAX);
            e.take_transmissions();
        }
        assert_eq!(e.cache.get(&key).unwrap().attempts, LOSS_RETX_MAX_ATTEMPTS);
        assert_eq!(ask(&mut e, key), RetxRequest::Deferred);
        assert_eq!(e.stats.verdict_deferred_to_pto, 1);
        let ws = e.cache.get(&key).unwrap().latest_ws;
        assert_eq!(
            e.request_retransmit(key, ws, RetxCause::Probe),
            RetxRequest::Queued,
            "the probe still resends it"
        );
    }

    // ---- the probe timeout (one per connection) ----

    fn one_departed_pass(t0: Instant) -> (ReliableTileEmitter, EmitKey) {
        let mut e = ReliableTileEmitter::new(t0);
        e.rtt.on_sample(Duration::from_millis(40));
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        let ws = e.take_transmissions()[0].wire_seq;
        e.on_departed(ws, t0);
        (e, key)
    }

    /// Not running with nothing cached, nor before anything has left.
    #[test]
    fn the_probe_timeout_runs_only_with_a_departed_pass_outstanding() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        assert_eq!(e.pto_deadline(), None);
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        assert_eq!(e.pto_deadline(), None, "queued, not departed");
        let ws = e.take_transmissions()[0].wire_seq;
        e.on_departed(ws, t0);
        assert!(e.pto_deadline().is_some());
        e.on_ack(&[key]);
        assert_eq!(e.pto_deadline(), None, "nothing left to probe for");
    }

    /// One PTO from the newest departure; an acknowledgement restarts it; each
    /// expiry restarts it and doubles the next; an acknowledgement ends the
    /// doubling.
    #[test]
    fn the_probe_timeout_restarts_on_acks_and_backs_off_on_expiries() {
        let t0 = Instant::now();
        let (mut e, _key) = one_departed_pass(t0);
        let pto = e.pto();
        assert_eq!(e.pto_deadline(), Some(t0 + pto));
        let t_ack = t0 + pto / 2;
        e.note_ack(t_ack);
        assert_eq!(e.pto_deadline(), Some(t_ack + pto), "reset on any ACK");
        // Expires; restarts from the expiry, backed off once.
        let t1 = t_ack + pto;
        assert!(e.pto_due(t1));
        e.note_pto_fire(t1);
        assert!(!e.pto_due(t1));
        assert_eq!(e.pto_deadline(), Some(t1 + pto * 2));
        let t2 = t1 + pto * 2;
        e.note_pto_fire(t2);
        assert_eq!(e.pto_deadline(), Some(t2 + pto * 4));
        e.note_ack(t2 + MS_1);
        assert_eq!(
            e.pto_deadline(),
            Some(t2 + MS_1 + pto),
            "an ACK ends the backoff"
        );
    }

    /// The client saying "this did not arrive" outranks a timer guessing
    /// that it might not have.
    #[test]
    fn nacks_are_served_ahead_of_the_loss_path() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        let mut sender = CollectSender::default();
        let timed_out = EmitKey::new(1, 0, 0, 0);
        let nacked = EmitKey::new(2, 1, 0, 0);
        e.submit_one(timed_out, fake_source(1, 0, 0), t0, None, t0);
        e.drain(&mut sender, t0);
        let one = sender.sent[0].len();
        // Submitted late enough that its own RTO is not yet due at `t1`.
        let t_late = t0 + Duration::from_millis(900);
        e.submit_one(nacked, fake_source(2, 0, 0), t_late, None, t_late);
        e.drain(&mut sender, t_late);

        let t1 = t0 + Duration::from_secs(1);
        e.on_nack(&[nacked], t1);
        ask(&mut e, timed_out);
        // Room for exactly one.
        e.tick_budgeted(t1, usize::MAX, one);
        assert_eq!(e.stats.loss_path_resends, 0, "the loss path waits its turn");
        assert_eq!(e.cache.get(&nacked).unwrap().attempts, 1);
        assert_eq!(e.cache.get(&timed_out).unwrap().attempts, 0);
    }

    #[test]
    fn submit_batch_processes_all_items() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        let mut sender = CollectSender::default();
        let now = Instant::now();
        let items: Vec<(EmitKey, Bytes, Instant, Option<ProbeTag>)> = (0..5)
            .map(|i| (EmitKey::new(i, 0, 0, 0), fake_source(i, 0, 0), now, None))
            .collect();
        e.submit_batch(items, now);
        e.drain(&mut sender, now);
        assert_eq!(sender.sent.len(), 5);
        assert_eq!(e.stats.source_emitted, 5);
    }

    #[test]
    fn tick_keeps_retrying_indefinitely_until_acked() {
        let mut e = ReliableTileEmitter::new(Instant::now());
        let key = EmitKey::new(1, 0, 0, 0);
        let now = Instant::now();
        e.submit_one(key, bytes::Bytes::from(vec![0u8; 16]), now, None, now);
        // Drain the initial emission so the queue is empty.
        let mut sink: Vec<Vec<u8>> = Vec::new();
        struct Sink<'a>(&'a mut Vec<Vec<u8>>);
        impl<'a> crate::transport::reliable_emitter::traits::DatagramSender for Sink<'a> {
            fn send(
                &mut self,
                dg: &[u8],
            ) -> crate::transport::reliable_emitter::traits::SendOutcome {
                self.0.push(dg.to_vec());
                crate::transport::reliable_emitter::traits::SendOutcome::Sent
            }
        }
        e.drain(&mut Sink(&mut sink), now);
        let initial_sends = sink.len();
        assert!(
            initial_sends >= 1,
            "initial submit produced at least one send"
        );

        // Advance time well past the old MAX_RETRANSMITS retirement window
        // and tick many times. Every tick should keep the entry alive (no
        // retirement) and continue scheduling retransmits.
        let mut t = now;
        for _ in 0..30 {
            t += std::time::Duration::from_secs(5);
            ask(&mut e, key);
            e.tick(t, usize::MAX);
            e.drain(&mut Sink(&mut sink), t);
        }
        // Entry must still be live in the cache.
        assert!(
            e.has_cache_entries_for_tile(0, 0),
            "cache entry must survive arbitrarily many RTO firings (no retirement)"
        );
        // ACK the entry — cache entry must disappear.
        e.on_ack(&[key]);
        assert!(
            !e.has_cache_entries_for_tile(0, 0),
            "ACK must remove the entry from the cache"
        );
    }

    #[test]
    fn tick_respects_max_retransmits_budget() {
        // Submit 1000 entries with distinct EmitKeys (so each gets its own
        // RTO heap entry). Advance time past all their initial RTOs.
        // tick(t, budget) must fire at most `budget` retransmits per call,
        // leaving the rest still due in the heap for the next call.
        let mut e = ReliableTileEmitter::new(Instant::now());
        e.set_tile_fec(true);
        let now = Instant::now();
        for i in 0..1000u32 {
            let k = EmitKey::new(i, 0, 0, 0);
            e.submit_one(k, bytes::Bytes::from(vec![0u8; 16]), now, None, now);
        }
        // Drain the initial 1000 submissions.
        struct Sink<'a>(&'a mut Vec<Vec<u8>>);
        impl<'a> crate::transport::reliable_emitter::traits::DatagramSender for Sink<'a> {
            fn send(
                &mut self,
                dg: &[u8],
            ) -> crate::transport::reliable_emitter::traits::SendOutcome {
                self.0.push(dg.to_vec());
                crate::transport::reliable_emitter::traits::SendOutcome::Sent
            }
        }
        let mut sink: Vec<Vec<u8>> = Vec::new();
        e.drain(&mut Sink(&mut sink), now);
        sink.clear();

        for i in 0..1000u32 {
            ask(&mut e, EmitKey::new(i, 0, 0, 0));
        }
        let t1 = now + e.pto() + std::time::Duration::from_millis(1);

        // First tick with budget=64: must fire exactly 64.
        let before = e.stats.loss_path_resends;
        e.tick(t1, 64);
        e.drain(&mut Sink(&mut sink), t1);
        assert_eq!(
            e.stats.loss_path_resends - before,
            64,
            "first tick must fire exactly the budget"
        );
        // Counted from `loss_path_resends` above rather than `sink.len()`: since a
        // retransmission allocates its own `wire_seq`, and (as of the fix
        // that feeds every allocated `wire_seq` into the FEC group builder)
        // that `wire_seq` is now a genuine group member, 64 contiguous
        // retransmit wire_seqs complete floor(64/10) = 6 new groups, each
        // scheduling a parity envelope. Those newly-scheduled parities —
        // plus whichever single not-yet-promoted parity was left over from
        // the initial 1000 submissions — become promotable in this same
        // drain because the allocator's `next` value has advanced past
        // their `emit_after` threshold. So the drain sees 64 retransmitted
        // sources plus 6 parities: 70.
        assert_eq!(
            sink.len(),
            70,
            "64 retransmits + 6 newly-completed FEC groups' parities, got {}",
            sink.len()
        );

        // Second tick at same instant: another 64 should fire (936 still due).
        sink.clear();
        let before = e.stats.loss_path_resends;
        e.tick(t1, 64);
        e.drain(&mut Sink(&mut sink), t1);
        assert_eq!(
            e.stats.loss_path_resends - before,
            64,
            "second tick fires another batch"
        );
        // Same shape as the first batch: the group builder carries a
        // 4-source remainder from the first batch (64 is not a multiple of
        // the group size 10), so this batch again completes 6 full groups
        // (using that remainder plus 56 of its own 64) whose parities are
        // promoted alongside the one left pending from the first batch's
        // drain.
        assert_eq!(sink.len(), 70, "got {}", sink.len());

        // After many ticks at budget=64, all 1000 entries should be popped
        // (each pop takes the entry off the timer, so subsequent ticks
        // at t1 yield None). 1000 / 64 ≈ 16 ticks needed.
        for _ in 0..20 {
            e.tick(t1, 64);
        }
        // All 1000 entries' next RTOs are in the future — heap has nothing due.
        // pending_cache_entries reflects un-ACKed work, still 1000.
        assert_eq!(e.pending_cache_entries(), 1000);
    }

    /// Regression test for the emitter's OWD-stamp clock bug: `submit_one`
    /// stamped `DatagramHeader.timestamp_us` from `SystemTime::now()` (the
    /// real wall clock) rather than from the caller's `now: Instant`, which
    /// in production is `now_std()` — tokio's clock, paused-aware under the
    /// browserless harness. Under a paused runtime real wall-clock time
    /// barely moves while `tokio::time::advance` fast-forwards the virtual
    /// clock, so two tile passes submitted 250ms apart on the virtual clock
    /// would carry emit stamps only microseconds apart — corrupting the
    /// deltas the BWE consumer (`io_bridge.rs`'s `owd_ms_lo16` computation)
    /// depends on.
    ///
    /// This test submits one tile pass, advances tokio's paused clock by
    /// 250ms, submits a second, and asserts the two stamped
    /// `timestamp_us` values differ by ~250ms. Verified by hand that this
    /// discriminates: temporarily reverting `emit_us` to read
    /// `SystemTime::now()` directly (bypassing `time_base`/`now`) makes the
    /// assertion below fail with a delta of a few hundred microseconds
    /// instead of ~250ms — reproducing the exact symptom this commit fixes.
    #[tokio::test(start_paused = true)]
    async fn emit_stamp_tracks_virtual_clock_not_wall_clock() {
        let base = crate::transport::io_bridge::now_std();
        let mut e = ReliableTileEmitter::new(base);
        let mut sender = CollectSender::default();

        let now0 = crate::transport::io_bridge::now_std();
        e.submit_one(
            EmitKey::new(1, 0, 0, 0),
            fake_source(1, 0, 0),
            now0,
            None,
            now0,
        );
        e.drain(&mut sender, now0);

        // Real wall-clock time spent on the next few lines is on the order
        // of microseconds; the paused clock jumps forward 250ms regardless.
        tokio::time::advance(Duration::from_millis(250)).await;

        let now1 = crate::transport::io_bridge::now_std();
        e.submit_one(
            EmitKey::new(2, 0, 0, 0),
            fake_source(2, 0, 0),
            now1,
            None,
            now1,
        );
        e.drain(&mut sender, now1);

        assert_eq!(sender.sent.len(), 2);
        let ts0 = u32::from_be_bytes(sender.sent[0][12..16].try_into().unwrap());
        let ts1 = u32::from_be_bytes(sender.sent[1][12..16].try_into().unwrap());
        let delta = Duration::from_micros(ts1.wrapping_sub(ts0) as u64);
        assert!(
            delta >= Duration::from_millis(240) && delta <= Duration::from_millis(260),
            "expected the two emit stamps to differ by ~250ms of virtual \
             time, got {:?} (ts0={ts0} ts1={ts1}) -- if this fails, the \
             emitter's timestamp_us stamp has regressed back to reading \
             the wall clock instead of the caller's `now: Instant`",
            delta,
        );
    }

    /// The emit stamp is kept at full width: `EmittedDatagram::emit_us` is the
    /// true microseconds since `time_base`, and the header carries its low 32
    /// bits. Swept across the 2^32 boundary and a second wrap.
    #[test]
    fn emit_stamp_is_full_width_across_the_32_bit_wrap() {
        const W: u64 = 1 << 32;
        let now = Instant::now();
        for elapsed_us in [0, 1, W - 1, W, W + 1, 2 * W - 1, 2 * W, 2 * W + 5] {
            let base = now
                .checked_sub(Duration::from_micros(elapsed_us))
                .expect("monotonic clock has this much history");
            let mut e = ReliableTileEmitter::new(base);
            e.submit_one(
                EmitKey::new(1, 0, 0, 0),
                fake_source(1, 0, 0),
                now,
                None,
                now,
            );
            let mut sender = CollectSender::default();
            e.drain(&mut sender, now);
            let sent = e.take_transmissions();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].emit_us, elapsed_us, "full width at {elapsed_us}");
            let header = u32::from_be_bytes(sender.sent[0][12..16].try_into().unwrap());
            assert_eq!(
                header, elapsed_us as u32,
                "header is the low 32 bits at {elapsed_us}"
            );
        }
    }

    /// A retransmission whose first transmission was before the wrap and whose
    /// resend is after it: each carries its own time, and they are ordered.
    #[test]
    fn a_retransmit_straddling_the_wrap_keeps_both_send_times() {
        const W: u64 = 1 << 32;
        let now = Instant::now();
        let first_us = W - 500;
        let base = now.checked_sub(Duration::from_micros(first_us)).unwrap();
        let mut e = ReliableTileEmitter::new(base);
        let mut sender = CollectSender::default();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), now, None, now);
        e.drain(&mut sender, now);
        let later = now + Duration::from_millis(2); // 1.5 ms past the wrap
        e.on_nack(&[key], later);
        e.tick(later, usize::MAX);
        e.drain(&mut sender, later);
        let sent = e.take_transmissions();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].emit_us, first_us);
        assert_eq!(sent[1].emit_us, first_us + 2_000);
        assert!(sent[1].emit_us > W, "the resend is past the wrap");
        let hdr1 = u32::from_be_bytes(sender.sent[1][12..16].try_into().unwrap());
        assert_eq!(hdr1, (first_us + 2_000) as u32);
    }

    /// Regression test for the `on_nack` mixed-clock hazard: `on_nack` used
    /// to stamp `last_sent_at` via a private `smoothed_rtt_clock_now()`
    /// stub that read the real wall clock (`Instant::now()`) instead of a
    /// caller-supplied `now`, while every other cache timestamp
    /// (`first_sent_at`/`last_sent_at` from `submit_one`/`tick`) already
    /// tracked `now_std()` — tokio's clock, paused-aware under the
    /// browserless harness. Under a paused runtime that produced exactly
    /// the defect this branch has already hit twice: `duration_since`
    /// between a wall-clock stamp and a virtual-clock stamp saturates
    /// silently to zero rather than failing loudly.
    ///
    /// This test submits one tile pass, advances tokio's paused clock by
    /// 5 minutes (real wall-clock time spent executing the intervening
    /// lines is microseconds), delivers a NACK for it, and asserts
    /// `last_sent_at` landed at the *virtual* timestamp — reachable only
    /// if `on_nack` reads its `now` argument rather than the wall clock.
    /// Verified by hand that this discriminates: temporarily reverting
    /// `on_nack` to stamp `entry.last_sent_at` from a bare `Instant::now()`
    /// (bypassing the `now` parameter) makes the assertion below fail,
    /// landing `last_sent_at` a few microseconds after `t0` instead of
    /// ~300s after it.
    #[tokio::test(start_paused = true)]
    async fn on_nack_stamps_last_sent_at_from_virtual_clock_not_wall_clock() {
        let t0 = crate::transport::io_bridge::now_std();
        let mut e = ReliableTileEmitter::new(t0);
        let mut sender = CollectSender::default();
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, fake_source(1, 0, 0), t0, None, t0);
        e.drain(&mut sender, t0);

        // Real wall-clock time spent on the next few lines is on the order
        // of microseconds; the paused clock jumps forward 5 minutes
        // regardless.
        tokio::time::advance(Duration::from_secs(300)).await;

        let now1 = crate::transport::io_bridge::now_std();
        e.on_nack(&[key], now1);
        // The resend, and with it the stamp, now happens when budget allows
        // rather than inside `on_nack`. One retransmission only, so it is
        // the NACK branch that stamps and not the RTO behind it.
        e.tick_budgeted(now1, 1, usize::MAX);
        assert_eq!(e.stats.loss_path_resends, 0);

        let last_sent_at = e.cache.get(&key).unwrap().last_sent_at;
        let delta = last_sent_at.duration_since(t0);
        assert!(
            delta >= Duration::from_secs(299) && delta <= Duration::from_secs(301),
            "expected on_nack to stamp last_sent_at from the caller's \
             virtual `now`, ~300s after t0, got {:?} -- if this fails, \
             on_nack has regressed back to reading the wall clock instead \
             of its `now` argument",
            delta,
        );
    }

    /// A tile group's parity emission carries the group's first `wire_seq`,
    /// whether it leaves at the interleave offset or by the end-of-stream flush.
    #[test]
    fn a_parity_emission_names_its_groups_first_wire_seq() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        e.set_tile_fec(true);
        let n = FEC_GROUP_SIZE_K as u32;
        let mut first_ws = Vec::new();
        for i in 0..n {
            e.submit_one(
                EmitKey::new(i, 0, 0, 0),
                Bytes::from(vec![0x80u8; 25]),
                t0,
                None,
                t0,
            );
        }
        for em in e.take_transmissions() {
            first_ws.push(em.wire_seq);
        }
        // Not yet at the interleave offset: the end-of-stream flush releases it.
        let mut groups = Vec::new();
        let mut now = t0;
        for _ in 0..200 {
            now += Duration::from_millis(1);
            while let Some(em) = e.next_emission(now) {
                if let Some(g) = em.parity_group() {
                    groups.push(g);
                }
            }
        }
        assert_eq!(groups, vec![first_ws[0]], "{groups:?} vs {first_ws:?}");
    }
}
