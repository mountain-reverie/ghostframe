//! The connection-stats service: per-session subscription state, the 1 Hz
//! sampler and the frame writes (spec `docs/specs/stats-panel-design.md`
//! section 1.2).
//!
//! It reads counters that already exist and adds no per-datagram or per-tile
//! work: a subscribed session costs one `StatsInputs` read a second. Nothing
//! is sent before a subscribe or after an unsubscribe.

use super::*;

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
}
