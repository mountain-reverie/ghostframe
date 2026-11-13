//! The connection-stats service: per-session subscription state, the 1 Hz
//! sampler and the frame writes (spec `docs/specs/stats-panel-design.md`
//! section 1.2).
//!
//! It reads counters that already exist and adds no per-datagram or per-tile
//! work: a subscribed session costs one `StatsInputs` read a second. Nothing
//! is sent before a subscribe or after an unsubscribe.

use super::*;
use crate::transport::stats_table::{self, StatsInputs};
use crate::transport::webtransport::StatsWrite;
use ghostframe_protocol::stats::StatsSample;

/// The sampling period.
pub(super) const STATS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// One session's subscription. Keyed by connection handle in
/// `IoBridge::stats_sessions` and removed by `forget_session`: quinn reuses
/// handles, so a leftover entry would alias the next connection.
#[derive(Debug, Default)]
pub(super) struct StatsSession {
    /// The client asked for stats and has not asked to stop.
    pub(super) subscribed: bool,
    /// A schema is owed: set by every `on`, cleared when one is written.
    pub(super) schema_pending: bool,
    /// The id of the last schema written; samples are built against it.
    pub(super) schema_id: Option<u16>,
    /// When the next sample (or schema retry) is due; `None` means now.
    pub(super) next_sample_at: Option<std::time::Instant>,
    /// A malformed subscribe has been logged for this session.
    pub(super) malformed_logged: bool,
    /// A dropped frame has been logged (at INFO) since the last `on`: a stock
    /// deploy logs `ghostframe=info`, and a stream that silently fails (no
    /// stream pinned, permanently blocked, a table that no longer encodes)
    /// must be visible there, once, not only in DEBUG counters.
    pub(super) drop_logged: bool,
}

/// What the service has done, cumulative for the process.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct StatsCounters {
    pub(super) schemas_sent: u64,
    pub(super) samples_sent: u64,
    /// Frames refused because the stream had no room or owed a tail.
    pub(super) frames_blocked: u64,
    /// Frames with nowhere to go: no stream, a gone stream, or no connection.
    pub(super) frames_no_stream: u64,
    /// Frames that could not be built within the wire limits.
    pub(super) encode_failures: u64,
    pub(super) subscribes_malformed: u64,
}

impl IoBridge {
    /// Apply a well-formed `STATS_SUBSCRIBE` from the live session `from`.
    ///
    /// `on` owes a schema (even a repeated `on`: that is how a reopened panel
    /// resynchronises) and makes the first sample due at once. `off` stops
    /// everything and owes nothing.
    pub(super) fn apply_stats_subscribe(&mut self, from: ConnectionHandle, on: bool) {
        let pinned = self
            .wt_sessions
            .get_mut(&from)
            .is_some_and(|wt| wt.note_stats_subscribe());
        let st = self.stats_sessions.entry(from).or_default();
        st.subscribed = on;
        st.schema_pending = on;
        st.next_sample_at = None;
        st.drop_logged = false;
        tracing::debug!(target: LOG_TARGET, ?from, on, pinned, "stats subscription changed");
        if !on {
            let c = self.stats_counters;
            tracing::debug!(
                target: LOG_TARGET,
                ?from,
                schemas_sent = c.schemas_sent,
                samples_sent = c.samples_sent,
                frames_blocked = c.frames_blocked,
                frames_no_stream = c.frames_no_stream,
                encode_failures = c.encode_failures,
                "stats stream stopped"
            );
        }
    }

    /// A `STATS_SUBSCRIBE` that did not decode: counted always, logged once
    /// per session. The message is fixed-length, so the caller still skips it.
    pub(super) fn note_malformed_stats_subscribe(&mut self, from: ConnectionHandle) {
        self.stats_counters.subscribes_malformed += 1;
        let st = self.stats_sessions.entry(from).or_default();
        if !st.malformed_logged {
            st.malformed_logged = true;
            tracing::warn!(
                target: LOG_TARGET,
                ?from,
                "malformed stats subscribe; ignored (logged once per session)"
            );
        }
    }
}

impl IoBridge {
    /// The next schema id: process-wide, monotonic, never 0.
    fn next_stats_schema_id(&mut self) -> u16 {
        self.stats_schema_seq = self.stats_schema_seq.wrapping_add(1);
        if self.stats_schema_seq == 0 {
            self.stats_schema_seq = 1;
        }
        self.stats_schema_seq
    }

    /// Note that a frame could not be sent. The counters say how often; this
    /// puts one INFO line in a stock journal the first time per subscription
    /// (see `StatsSession::drop_logged`). `true` when it logged.
    fn note_stats_drop(&mut self, handle: ConnectionHandle, why: &str) -> bool {
        let c = self.stats_counters;
        let st = self.stats_sessions.entry(handle).or_default();
        if st.drop_logged {
            return false;
        }
        st.drop_logged = true;
        tracing::info!(
            target: LOG_TARGET,
            ?handle,
            why,
            frames_blocked = c.frames_blocked,
            frames_no_stream = c.frames_no_stream,
            encode_failures = c.encode_failures,
            "stats frame dropped (logged once per subscription; the counters keep running)"
        );
        true
    }

    /// Read the numbers one sample is built from. One place, so the table and
    /// the bridge cannot disagree about a source.
    fn stats_inputs(&self, handle: ConnectionHandle) -> StatsInputs {
        let rtt = if self.reliable_emitter.rtt.has_sample() {
            Some(self.reliable_emitter.rtt.srtt())
        } else {
            // Before the first acknowledgement sample: quinn's own path RTT.
            self.live_path_rtt()
        };
        let h264_backend = if self.h264.availability != h264_refs::H264Availability::Available {
            "unavailable"
        } else if self.h264.backend_kind.is_empty() {
            "none"
        } else {
            self.h264.backend_kind
        };
        StatsInputs {
            rtt_ms: rtt.map_or(0.0, |d| d.as_secs_f64() * 1e3),
            // Real clock on purpose: `session_started_at` is a real-clock
            // stamp (see `on_connection_gone`'s `session_secs`).
            uptime_s: self.session_started_at.map_or(0.0, |t| {
                std::time::Instant::now()
                    .saturating_duration_since(t)
                    .as_secs_f64()
            }),
            bwe_bps: self.bwe.snapshot().bitrate_bps as f64,
            rate_target_bps: f64::from(self.h264.stats.rate_target_bps),
            sent_bytes: self
                .wt_sessions
                .get(&handle)
                .map_or(0, |wt| wt.datagram_bytes_enqueued()) as f64,
            frame_mode: match self.frame_mode {
                crate::tile::FrameMode::H264 => "h264",
                crate::tile::FrameMode::TileCodec => "tile",
            },
            mode_reason: self.classifier.last_decision_reason(),
            h264_backend,
            loss_path_resends: self.reliable_emitter.stats.loss_path_resends as f64,
            loss_verdicts: self.instr.losses_declared as f64,
            nacks_received: (self.reliable_emitter.stats.nack_hit
                + self.reliable_emitter.stats.nack_miss) as f64,
        }
    }

    /// Write one frame, counting what happened. `true` when it is on the
    /// stream.
    fn write_stats_counted(&mut self, handle: ConnectionHandle, frame: &[u8]) -> bool {
        #[cfg(test)]
        if let Some(log) = self.stats_capture_for_test.as_mut() {
            log.push((handle, frame.to_vec()));
            return true;
        }
        let outcome = match (
            self.wt_sessions.get_mut(&handle),
            self.server.connections.get_mut(&handle),
        ) {
            (Some(wt), Some(conn)) => wt.write_stats_frame(conn, frame),
            _ => StatsWrite::NoStream,
        };
        match outcome {
            StatsWrite::Written => true,
            StatsWrite::Blocked => {
                self.stats_counters.frames_blocked += 1;
                self.note_stats_drop(handle, "stream blocked");
                false
            }
            StatsWrite::NoStream | StatsWrite::StreamGone => {
                self.stats_counters.frames_no_stream += 1;
                self.note_stats_drop(handle, "no stats stream");
                false
            }
        }
    }

    /// Send what is due to every subscribed session: the schema it is owed,
    /// then a sample, at most once per [`STATS_INTERVAL`]. A frame that cannot
    /// be written is dropped and counted; a schema that could not be written
    /// stays owed and is retried at the next interval (a sample is never sent
    /// against a schema the client has not received).
    ///
    /// Cheap on every loop turn (turns follow datagram batches, work is due
    /// at most once a second): when no subscribed session is due it returns
    /// after one pass over the (tiny) session map, with no allocation. The
    /// handle list below is built only when something is due.
    pub(super) fn service_stats(&mut self, now: std::time::Instant) {
        let any_due = self
            .stats_sessions
            .values()
            .any(|s| s.subscribed && s.next_sample_at.is_none_or(|t| now >= t));
        if !any_due {
            return;
        }
        let mut handles: Vec<ConnectionHandle> = self
            .stats_sessions
            .iter()
            .filter(|(_, s)| s.subscribed)
            .map(|(h, _)| *h)
            .collect();
        handles.sort_by_key(|h| h.0);
        for h in handles {
            let Some(st) = self.stats_sessions.get_mut(&h) else {
                continue;
            };
            if st.next_sample_at.is_some_and(|t| now < t) {
                continue;
            }
            st.next_sample_at = Some(now + STATS_INTERVAL);
            let schema_pending = st.schema_pending;
            let mut schema_id = st.schema_id;

            if schema_pending {
                let id = self.next_stats_schema_id();
                let Ok(frame) = stats_table::schema(id).try_encode_frame() else {
                    self.stats_counters.encode_failures += 1;
                    self.note_stats_drop(h, "schema does not encode");
                    continue;
                };
                if !self.write_stats_counted(h, &frame) {
                    continue;
                }
                self.stats_counters.schemas_sent += 1;
                schema_id = Some(id);
                if let Some(st) = self.stats_sessions.get_mut(&h) {
                    st.schema_pending = false;
                    st.schema_id = schema_id;
                }
            }
            let Some(schema_id) = schema_id else {
                continue;
            };

            let sample = StatsSample {
                schema_id,
                timestamp_us: now.saturating_duration_since(self.epoch).as_micros() as u64,
                values: self.stats_inputs(h).values(),
            };
            let Ok(frame) = sample.try_encode_frame() else {
                self.stats_counters.encode_failures += 1;
                self.note_stats_drop(h, "sample does not encode");
                continue;
            };
            if self.write_stats_counted(h, &frame) {
                self.stats_counters.samples_sent += 1;
            }
        }
    }

    /// When the service next has something to do, for the event loop's timer.
    pub(super) fn stats_next_due(&self) -> Option<std::time::Instant> {
        self.stats_sessions
            .values()
            .filter(|s| s.subscribed)
            .map(|s| s.next_sample_at.unwrap_or_else(now_std))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bridge with one live session, `h`, and the stats writes captured
    /// instead of sent (a unit test has no quinn connection).
    async fn bridge() -> (IoBridge, ConnectionHandle) {
        let (our_end, _peer) = tokio::net::UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("QuicServer::new");
        let mut b = IoBridge::new_with_stream_for_test(our_end, server);
        let h = ConnectionHandle(0);
        b.wt_sessions.insert(h, WebTransportServer::default());
        b.last_hello_from = Some(h);
        b.stats_capture_for_test = Some(Vec::new());
        (b, h)
    }

    #[tokio::test]
    async fn a_subscribe_message_turns_the_subscription_on_and_off() {
        let (mut b, h) = bridge().await;
        assert!(b.stats_sessions.is_empty(), "no state before a subscribe");
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        assert!(b.stats_sessions.get(&h).is_some_and(|s| s.subscribed));
        b.dispatch_feedback_bytes(h, &[0x0B, 0]);
        assert!(b.stats_sessions.get(&h).is_some_and(|s| !s.subscribed));
    }

    #[tokio::test]
    async fn a_subscribe_from_a_session_that_never_said_hello_is_ignored() {
        let (mut b, _h) = bridge().await;
        let stranger = ConnectionHandle(5);
        b.wt_sessions
            .insert(stranger, WebTransportServer::default());
        b.dispatch_feedback_bytes(stranger, &[0x0B, 1]);
        assert!(
            b.stats_sessions.is_empty(),
            "only the live session may switch the stream on: the existing HELLO gate"
        );
    }

    #[tokio::test]
    async fn a_malformed_subscribe_is_ignored_counted_and_keeps_the_stream_in_step() {
        let (mut b, h) = bridge().await;
        // `on` = 7 is malformed; the message is fixed-length, so the valid
        // subscribe behind it in the same read must still be applied.
        b.dispatch_feedback_bytes(h, &[0x0B, 7, 0x0B, 1]);
        assert!(b.stats_sessions.get(&h).is_some_and(|s| s.subscribed));
        assert_eq!(b.stats_counters.subscribes_malformed, 1);
        b.dispatch_feedback_bytes(h, &[0x0B, 9]);
        assert_eq!(b.stats_counters.subscribes_malformed, 2);
        assert!(
            b.stats_sessions.get(&h).is_some_and(|s| s.malformed_logged),
            "the log line is once per session; the count is not"
        );
    }

    #[tokio::test]
    async fn a_subscribe_split_across_reads_is_applied_when_whole() {
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B]);
        assert!(b.stats_sessions.is_empty(), "half a message does nothing");
        b.dispatch_feedback_bytes(h, &[1]);
        assert!(b.stats_sessions.get(&h).is_some_and(|s| s.subscribed));
    }

    #[tokio::test]
    async fn forgetting_a_session_clears_its_subscription() {
        // quinn reuses connection handles: a stale subscription would leak
        // into whichever connection gets this handle next.
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        assert!(b.stats_sessions.contains_key(&h));
        b.forget_session(h);
        assert!(
            !b.stats_sessions.contains_key(&h),
            "forget_session must clear the stats subscription"
        );
    }

    use ghostframe_protocol::stats::{
        MetricValue, ServerFrameReader, StatsSample, StatsSchema, SERVER_MSG_STATS_SAMPLE,
        SERVER_MSG_STATS_SCHEMA,
    };

    /// Everything captured since the last call, decoded as `(type, payload)`.
    fn captured(b: &mut IoBridge) -> Vec<(u8, Vec<u8>)> {
        let raw = std::mem::take(b.stats_capture_for_test.as_mut().expect("capture is on"));
        let mut r = ServerFrameReader::default();
        for (_, f) in raw {
            r.push(&f);
        }
        let mut out = Vec::new();
        while let Some(f) = r.next_frame() {
            out.push(f.expect("the server only writes whole frames"));
        }
        out
    }

    fn decode_schema(f: &(u8, Vec<u8>)) -> StatsSchema {
        assert_eq!(f.0, SERVER_MSG_STATS_SCHEMA);
        StatsSchema::decode_payload(&f.1).expect("schema decodes")
    }

    fn decode_sample(f: &(u8, Vec<u8>), schema: &StatsSchema) -> StatsSample {
        assert_eq!(f.0, SERVER_MSG_STATS_SAMPLE);
        StatsSample::decode_payload(&f.1, schema).expect("sample decodes against its schema")
    }

    fn num(schema: &StatsSchema, s: &StatsSample, name: &str) -> f64 {
        let i = schema
            .metrics
            .iter()
            .position(|m| m.name == name)
            .expect(name);
        match &s.values[i] {
            MetricValue::Num(x) => *x,
            MetricValue::Text(t) => panic!("{name} is text {t:?}"),
        }
    }

    fn text(schema: &StatsSchema, s: &StatsSample, name: &str) -> String {
        let i = schema
            .metrics
            .iter()
            .position(|m| m.name == name)
            .expect(name);
        match &s.values[i] {
            MetricValue::Text(t) => t.clone(),
            MetricValue::Num(x) => panic!("{name} is number {x}"),
        }
    }

    #[tokio::test]
    async fn nothing_is_sent_before_a_subscribe() {
        let (mut b, _h) = bridge().await;
        let now = now_std();
        b.service_stats(now);
        b.service_stats(now + std::time::Duration::from_secs(5));
        assert!(captured(&mut b).is_empty());
        assert_eq!(b.stats_counters, StatsCounters::default());
    }

    #[tokio::test]
    async fn a_subscribe_is_answered_with_a_schema_and_a_first_sample_in_one_turn() {
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        b.service_stats(now_std());
        let frames = captured(&mut b);
        assert_eq!(frames.len(), 2, "schema then sample: {frames:?}");
        let schema = decode_schema(&frames[0]);
        assert_ne!(schema.schema_id, 0);
        assert_eq!(
            schema.metrics.len(),
            crate::transport::stats_table::Stat::ALL.len()
        );
        assert_eq!(
            schema.server_version,
            crate::transport::stats_table::SERVER_VERSION
        );
        let sample = decode_sample(&frames[1], &schema);
        assert_eq!(sample.schema_id, schema.schema_id);
        assert_eq!(sample.values.len(), schema.metrics.len());
        assert_eq!(b.stats_counters.schemas_sent, 1);
        assert_eq!(b.stats_counters.samples_sent, 1);
    }

    #[tokio::test]
    async fn samples_follow_once_a_second_and_never_faster() {
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        let t0 = now_std();
        b.service_stats(t0);
        let _ = captured(&mut b);

        b.service_stats(t0 + std::time::Duration::from_millis(400));
        b.service_stats(t0 + std::time::Duration::from_millis(999));
        assert!(captured(&mut b).is_empty(), "not due yet");

        b.service_stats(t0 + std::time::Duration::from_millis(1000));
        let frames = captured(&mut b);
        assert_eq!(frames.len(), 1, "one sample, no schema: {frames:?}");
        assert_eq!(frames[0].0, SERVER_MSG_STATS_SAMPLE);

        // Called twice at the same instant: still one.
        b.service_stats(t0 + std::time::Duration::from_millis(1000));
        assert!(captured(&mut b).is_empty());
    }

    #[tokio::test]
    async fn an_unsubscribe_stops_everything() {
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        let t0 = now_std();
        b.service_stats(t0);
        let _ = captured(&mut b);
        b.dispatch_feedback_bytes(h, &[0x0B, 0]);
        for s in 1..=5 {
            b.service_stats(t0 + std::time::Duration::from_secs(s));
        }
        assert!(
            captured(&mut b).is_empty(),
            "off means off, and no schema either"
        );
    }

    #[tokio::test]
    async fn a_repeated_subscribe_resends_the_schema_with_a_new_id() {
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        let t0 = now_std();
        b.service_stats(t0);
        let first = decode_schema(&captured(&mut b)[0]);

        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        b.service_stats(t0 + std::time::Duration::from_millis(10));
        let frames = captured(&mut b);
        assert_eq!(frames.len(), 2, "schema and a sample at once: {frames:?}");
        let second = decode_schema(&frames[0]);
        assert_ne!(
            second.schema_id, first.schema_id,
            "the client clears its rings on a new id"
        );
        assert_eq!(
            decode_sample(&frames[1], &second).schema_id,
            second.schema_id
        );
    }

    #[tokio::test]
    async fn a_forgotten_handle_that_quinn_reuses_gets_no_stats() {
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        b.forget_session(h);
        // The same handle, a different connection, which never subscribed.
        b.wt_sessions.insert(h, WebTransportServer::default());
        b.last_hello_from = Some(h);
        b.service_stats(now_std() + std::time::Duration::from_secs(5));
        assert!(
            captured(&mut b).is_empty(),
            "the old subscription must not alias the new connection"
        );
    }

    #[tokio::test]
    async fn a_sample_reads_the_live_counters() {
        let (mut b, h) = bridge().await;
        // Distinct non-zero values, so a swapped source cannot pass.
        b.reliable_emitter.stats.loss_path_resends = 5;
        b.reliable_emitter.stats.nack_hit = 2;
        b.reliable_emitter.stats.nack_miss = 3;
        b.instr.losses_declared = 9;
        b.reliable_emitter
            .rtt
            .on_sample(std::time::Duration::from_millis(40));
        b.session_started_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(5));
        b.frame_mode = crate::tile::FrameMode::TileCodec;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        b.service_stats(now_std());
        let frames = captured(&mut b);
        let schema = decode_schema(&frames[0]);
        let s = decode_sample(&frames[1], &schema);

        assert_eq!(num(&schema, &s, "ghostframe.loss.path_resends"), 5.0);
        assert_eq!(
            num(&schema, &s, "ghostframe.loss.nacks_received"),
            5.0,
            "hit + miss"
        );
        assert_eq!(num(&schema, &s, "ghostframe.loss.verdicts"), 9.0);
        let rtt = num(&schema, &s, "ghostframe.rtt.smoothed");
        assert!(
            (rtt - 40.0).abs() < 0.5,
            "the first sample initialises srtt: {rtt}"
        );
        assert!(num(&schema, &s, "ghostframe.session.uptime") >= 5.0);
        assert_eq!(
            num(&schema, &s, "ghostframe.bwe.estimate"),
            b.bwe.snapshot().bitrate_bps as f64,
            "the estimate is the bridge's own, not a copy that can drift"
        );
        assert!(
            num(&schema, &s, "ghostframe.bwe.estimate") > 0.0,
            "a zero estimate is a dead probe"
        );
        assert_eq!(text(&schema, &s, "ghostframe.frame.mode"), "tile");
        assert_eq!(text(&schema, &s, "ghostframe.frame.mode_reason"), "none");
        assert_eq!(text(&schema, &s, "ghostframe.h264.backend"), "none");
    }

    #[tokio::test]
    async fn the_event_loop_wakes_for_the_next_sample() {
        let (mut b, h) = bridge().await;
        assert!(
            b.next_wake_deadline().is_none(),
            "premise: nothing else is pending"
        );
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        let t0 = now_std();
        b.service_stats(t0);
        let due = b
            .next_wake_deadline()
            .expect("a subscribed session needs a wake");
        // `>=` a whole interval, not `> t0`: without the `next_sample_at`
        // stamp the deadline falls back to "now", which is just after `t0`.
        assert!(
            due >= t0 + STATS_INTERVAL
                && due <= t0 + STATS_INTERVAL + std::time::Duration::from_millis(50),
            "{due:?}"
        );
        b.dispatch_feedback_bytes(h, &[0x0B, 0]);
        assert!(
            b.next_wake_deadline().is_none(),
            "an unsubscribed session needs none"
        );
    }

    #[tokio::test]
    async fn a_dropped_frame_is_logged_once_per_subscription() {
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        assert!(!b.stats_sessions[&h].drop_logged);
        assert!(
            b.note_stats_drop(h, "stream blocked"),
            "the first drop is the one that logs"
        );
        assert!(
            !b.note_stats_drop(h, "stream blocked"),
            "later drops stay quiet"
        );
        assert!(b.stats_sessions[&h].drop_logged);
        // A reopened panel is a new subscription and may log again.
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        assert!(b.note_stats_drop(h, "stream blocked"));
    }
}
