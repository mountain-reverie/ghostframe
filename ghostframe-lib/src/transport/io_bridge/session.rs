//! Session lifecycle for the single live client: HELLO, takeover, eviction and
//! the deadlines that close a connection.
//!
//! Owns the session bookkeeping fields of `IoBridge` (`wt_sessions`,
//! `last_hello_from`, `close_deadlines`, `last_takeover_at`,
//! `last_disconnect_at`, `has_seen_prior_session`, `session_started_at`,
//! `client_caps`, `display_scale_milli` as set by HELLO, and the
//! connected-session count watch), plus the `CloseDeadline` record and the
//! HELLO deadline, eviction grace, QUIC close codes and takeover interval
//! constants. The fields themselves stay on `IoBridge`. It also sequences the
//! per-session resets (`fire_session_reset`, `forget_session`) that clear
//! state other subsystems keep per connection.
//!
//! Must not: make encode, pacing or bandwidth-estimation decisions; it
//! decides who the live session is and when a connection is closed, and
//! tells the other subsystems to reset, nothing more.

use super::*;

/// Scale before any `DisplayInfo` arrives. 1.0 -- the server's current
/// behaviour, so a client that never negotiates sees no change.
pub(super) const DEFAULT_SCALE_MILLI: u16 = 1000;

/// How long a freshly accepted connection may take to send HELLO before the
/// server closes it. Armed at accept time so a half-open handshake is
/// covered too.
///
/// The budget is the WHOLE setup, not just the HELLO: QUIC handshake, HTTP/3
/// SETTINGS, the WebTransport CONNECT and its 200 (the client sends HELLO
/// only after that), then HELLO itself, all of it reliable and therefore
/// retransmitted with PTO backoff. On a 10 % loss link the browserless
/// scene at seed 0xB17E1000 needs 6.06 s from accept to HELLO (the other
/// four seeds need under a second); 5 s killed a legitimate client there.
/// 20 s leaves ~3x headroom over that and still bounds a silent
/// connection's hold on one of the `MAX_CONNECTIONS` slots.
pub(super) const HELLO_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// How long an evicted session keeps its QUIC connection open so the
/// eviction notice can leave before CONNECTION_CLOSE.
pub(super) const EVICT_CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// QUIC application close codes (ours).
pub(super) const CLOSE_CODE_EVICTED: u32 = 0x6701;

pub(super) const CLOSE_CODE_NO_HELLO: u32 = 0x6702;

pub(super) const CLOSE_CODE_TAKEOVER_RATE_LIMITED: u32 = 0x6703;

/// The peer no longer passes the access rule (see `transport::peer_auth`).
pub(super) const CLOSE_CODE_ACCESS_REVOKED: u32 = 0x6704;

/// Grace before the QUIC close of a revoked session: none. Unlike an
/// eviction there is no notice to flush to a peer that is no longer allowed.
const REVOKE_CLOSE_GRACE: std::time::Duration = std::time::Duration::ZERO;

/// Minimum time between one HELLO displacing a live session and the next.
/// Each takeover evicts the incumbent and runs the session reset (palette,
/// classifier, scheduler, an all-tiles-dirty burst and an IDR), so two
/// clients alternating HELLOs would keep the server permanently resetting.
/// A refused newcomer is closed; the incumbent is untouched. A reconnect
/// after the incumbent has gone is not a takeover and is never limited.
pub(super) const TAKEOVER_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// A pending server-initiated close of one connection.
#[derive(Clone, Copy, Debug)]
pub(super) struct CloseDeadline {
    pub(super) at: std::time::Instant,
    pub(super) code: u32,
}

impl IoBridge {
    /// Refresh the `connected_session_count` channel the capture loop reads
    /// to decide whether to scrape a frame. Counts only WT sessions whose H3
    /// handshake has completed (`wt.is_connected()`). Emits a single
    /// info-level log on each idle↔active transition.
    ///
    /// Called from `compute_max_datagram_size` (so the count is fresh every
    /// frame submission) AND from the run-loop's event drain (so a freshly
    /// established session unblocks ghostframe-server's gate even before the first
    /// frame is submitted — without this hook, no frame ever flows because
    /// the gate stays at 0 until a frame submission updates it, which can't
    /// happen until the gate opens. Classic bootstrap deadlock).
    ///
    /// `send_if_modified` rather than `send`: the per-frame call site would
    /// otherwise mark the value changed ~30 times a second and wake every
    /// parked `wait_for_client` waiter on each one. Notifying only on an
    /// actual change makes a park cost one wakeup per connect, not one per
    /// frame. It also means `send` can never fail here for want of
    /// receivers — `send_if_modified` ignores the receiver count.
    pub(super) fn refresh_connected_session_count(&mut self) {
        // Only the HELLO'd session counts: a connected-but-unidentified
        // connection must not open the capture gate or receive data.
        let connected = usize::from(
            self.last_hello_from
                .and_then(|h| self.wt_sessions.get(&h))
                .is_some_and(|wt| wt.is_connected()),
        );
        self.connected_session_count.send_if_modified(|current| {
            if *current == connected {
                false
            } else {
                *current = connected;
                true
            }
        });
        let now_idle = connected == 0;
        if now_idle != self.was_idle {
            if now_idle {
                tracing::info!(
                    target: LOG_TARGET,
                    "all clients disconnected; capture loop will sleep until a client returns"
                );
            } else {
                tracing::info!(target: LOG_TARGET, connected, "client connected; resuming frame delivery");
            }
            self.was_idle = now_idle;
        }
    }

    /// Is `handle` the HELLO'd session? The authorisation predicate: only
    /// the live session may have its feedback honoured or receive data.
    pub(super) fn is_live(&self, handle: ConnectionHandle) -> bool {
        self.last_hello_from == Some(handle)
    }

    /// The live session's handle iff it can also be sent to: its WebTransport
    /// session is connected and the quinn connection is still present.
    pub(super) fn live_send_handle(&self) -> Option<ConnectionHandle> {
        self.live_connected_handle()
            .filter(|h| self.server.connections.contains_key(h))
    }

    /// The live session's handle iff its WebTransport session is connected.
    pub(super) fn live_connected_handle(&self) -> Option<ConnectionHandle> {
        let h = self.last_hello_from?;
        self.wt_sessions.get(&h)?.is_connected().then_some(h)
    }

    /// Reset per-session state on a new WebTransport session. Called by
    /// `apply_hello` when a new session takes over (not for the first-ever
    /// session, where the reset body's RECONNECT-specific side effects --
    /// frame_mode -> H264, scheduler clear, force_dirty_frames=20,
    /// request_keyframe -- are no-ops that would break setups capturing
    /// frames before the client connects).
    /// Preserves cross-session palette bytes (warm cache) and, when the
    /// cfg-gated test hook GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1 is
    /// active, also preserves `palette_table.delivered`.
    pub(super) fn fire_session_reset(&mut self, handle: ConnectionHandle) {
        // New WebTransport session just became active.
        // Initialize frame_mode to H264 so the first frame
        // emits a single compact IDR (~50 datagrams) instead
        // of an all-tiles raw burst (~8000 datagrams) that
        // QUIC slow-start would mostly drop. Classifier exits
        // back to TileCodec naturally after exit_sustain frames
        // of empty dirty (Task 17).
        // A frame the worker is still on belongs to the session being reset.
        self.session_epoch += 1;
        self.dirty_tracker.reset();
        self.metrics_tracker.reset();
        self.classifier.reset();
        self.scheduler.clear();
        #[cfg(any(test, feature = "test-loss-injection"))]
        let preserve_delivered = self.skip_palette_session_reset;
        #[cfg(not(any(test, feature = "test-loss-injection")))]
        let preserve_delivered = false;
        self.palette_table.on_session_reset(preserve_delivered);
        if preserve_delivered {
            tracing::info!(
                target: LOG_TARGET,
                "test-hook: preserving palette_table.delivered across session reset (GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1)"
            );
        }
        self.frame_mode = crate::tile::FrameMode::H264;
        // Re-prime the frame-dimensions retransmit counter so
        // the new client receives the sentinel on its first
        // frames even if the screen has been at a stable
        // resolution for >FRAME_DIMENSIONS_RETRANSMITS frames.
        // Without this, the client's per-tile fallback resize
        // (gated on !frameDimensionsKnown) takes over and
        // re-introduces the canvas-resize-clears-tiles bug.
        self.dimensions_retransmits_left = FRAME_DIMENSIONS_RETRANSMITS;
        // Slow-start cushion: 20 frames of all-tiles-dirty so a tile
        // dropped during QUIC slow-start re-emerges as dirty on the
        // next frame. Each path has its own implementation — CPU's
        // `force_dirty_frames` runs dirty_tracker in no-commit mode;
        // GPU's `invalidate_baseline` drops prev_image and runs the
        // no-snapshot first-frame path for N frames.
        //
        // Both are set unconditionally. Without a frame worker (unit tests
        // only) the backend control is a no-op.
        self.force_dirty_frames = 20;
        self.backend_apply(Control::InvalidateBaseline(20));
        // Force IDR on the existing encoder so the new client
        // gets a fresh anchor. Without this, a client connecting
        // mid-stream may receive P-frames referencing GOPs from
        // the prior session and decode garbage until the next
        // natural keyframe (~10 frames later at GOP=11). The
        // first-time-only escape (lazy `FullFrameEncoder::new`
        // with pts=0 → IDR) only covers cold start.
        self.backend_apply(Control::RequestKeyframe);
        // info, not debug: this is the line that says a reconnecting client is
        // getting a clean slate rather than resuming, and at debug it was
        // invisible in exactly the production logs where that matters.
        tracing::info!(
            target: LOG_TARGET,
            ?handle,
            force_dirty_frames = self.force_dirty_frames,
            "session reset: dirty tracker, scheduler and palette cleared; \
             keyframe requested"
        );
    }

    /// Return the capabilities most recently advertised by the client via HELLO.
    /// Returns `ClientCapabilities::default()` until the first HELLO is received.
    pub fn current_client_caps(&self) -> crate::transport::client_caps::ClientCapabilities {
        self.client_caps
    }

    /// Update capabilities from a parsed HELLO message.
    pub(crate) fn apply_hello(
        &mut self,
        from: ConnectionHandle,
        msg: crate::transport::client_caps::HelloMsg,
    ) {
        // A handle already forgotten (evicted earlier in the same collected
        // feedback batch) may still have a HELLO queued behind it. It has no
        // session to serve, so it must not become the live session.
        if !self.wt_sessions.contains_key(&from) {
            return;
        }
        // Capture the previous value before overwriting it: the eviction
        // check below needs to know who held the session *before* this
        // HELLO, not `from` itself.
        let incumbent = self.last_hello_from;
        if incumbent.is_some_and(|i| i != from) {
            let now = now_std();
            if self.takeover_rate_limited(now) {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?from,
                    ?incumbent,
                    "HELLO would displace the live session again within {:?}; closing the newcomer",
                    TAKEOVER_MIN_INTERVAL
                );
                self.close_deadlines.insert(
                    from,
                    CloseDeadline {
                        at: now,
                        code: CLOSE_CODE_TAKEOVER_RATE_LIMITED,
                    },
                );
                return;
            }
            self.last_takeover_at = Some(now);
        }
        self.last_hello_from = Some(from);
        // The HELLO arrived: the never-HELLO deadline no longer applies.
        self.close_deadlines.remove(&from);

        // A different connection becoming the live session is a takeover
        // (or the first ever). Per-session state of whoever came before
        // must not leak into the new session.
        if incumbent != Some(from) {
            self.reset_session_instruments(from);
            let now = std::time::Instant::now();
            tracing::info!(
                target: LOG_TARGET,
                ?from,
                gap_secs = self
                    .last_disconnect_at
                    .map(|t| now.saturating_duration_since(t).as_secs_f32()),
                carried_pending_entries = self.reliable_emitter.pending_cache_entries(),
                "session live"
            );
            // One client at a time. A second client identifying itself
            // displaces the incumbent, which is told why (M4a design §2, §3).
            //
            // Keyed on HELLO rather than on session accept: a connection that
            // never identifies itself -- a port scan, an abandoned handshake,
            // a client that died mid-negotiation -- must not be able to kill
            // a working session.
            //
            // Evict BEFORE stamping the new session: eviction clears
            // `session_started_at`, which would erase the newcomer's stamp.
            if let Some(incumbent) = incumbent {
                self.evict_session(incumbent, EvictionReason::DisplacedByNewSession);
            }
            self.session_started_at = Some(now);
            self.pending_display_mode = None;
            self.display_scale_milli = DEFAULT_SCALE_MILLI;
        }

        // Reset per-session pipeline state for a new live session -- but not
        // for the first one ever (see `fire_session_reset`).
        if incumbent != Some(from) {
            if self.has_seen_prior_session {
                self.fire_session_reset(from);
            }
            self.has_seen_prior_session = true;
        }

        self.client_caps = msg.caps;
        // Mirror the H.264 capability into the adaptation context so the
        // classifier's hard override sees it on the very next decision —
        // without this push, the classifier would still think the client
        // supports H.264 until the next path-stats / feedback ingest.
        self.adaptation_context.supports_h264 = msg.caps.supports_h264;
        self.adaptation_context.last_update_seq =
            self.adaptation_context.last_update_seq.wrapping_add(1);
        self.classifier
            .set_adaptation_context(self.adaptation_context);
        tracing::info!(
            target: LOG_TARGET,
            ?from,
            indices_raw = msg.caps.indices_raw_enabled,
            supports_h264 = msg.caps.supports_h264,
            "HELLO received, client capabilities updated"
        );
    }

    /// Which session most recently sent HELLO. `None` until the first HELLO
    /// arrives. Eviction (`apply_hello`) reads the `last_hello_from` field
    /// directly rather than through this accessor, which exists for tests.
    #[allow(dead_code)]
    pub(crate) fn last_hello_from(&self) -> Option<ConnectionHandle> {
        self.last_hello_from
    }

    /// Cleanup for a connection that is gone, whether quinn reported
    /// `ConnectionLost` or `QuicServer::handle_timeout` reaped it silently.
    ///
    /// Only the live session's loss touches server-wide state: a stale or
    /// never-HELLO'd connection dying (a scanner timing out, an evicted
    /// session finishing its close) must not wipe the live session's
    /// retransmit cache or reset its clocks.
    pub(super) fn on_connection_gone(&mut self, handle: ConnectionHandle, reason: &str) {
        // Must be read before forget_session clears it.
        let was_live = self.last_hello_from == Some(handle);
        // Everything needed to reconstruct the gap later: how long this
        // session lasted, and how much undelivered state died with it.
        // clear_cache() below used to discard all of it silently.
        let discarded = self.reliable_emitter.pending_cache_entries();
        let discarded_bytes = self.reliable_emitter.bytes_in_flight();
        let now = std::time::Instant::now();
        tracing::info!(
            target: LOG_TARGET,
            ?handle,
            reason,
            was_live,
            session_secs = self
                .session_started_at
                .map(|t| now.saturating_duration_since(t).as_secs_f32()),
            discarded_pending_entries = if was_live { discarded } else { 0 },
            discarded_bytes = if was_live { discarded_bytes } else { 0 },
            "connection lost"
        );
        self.forget_session(handle);
        if was_live {
            self.session_epoch += 1;
            self.session_started_at = None;
            self.last_disconnect_at = Some(now);
            // Delivery-guarantee invariant: when the session ends,
            // un-ACKed tile-passes are no longer deliverable and must not
            // occupy memory or fire spurious retransmits for the next
            // client. ReliableTileEmitter is shared across sessions; the
            // design has one live client at a time.
            self.reliable_emitter.clear_cache();
        }
    }

    /// Forget everything keyed by a connection handle. Every site that drops
    /// a session must go through here: quinn reuses handles from a slab, so
    /// a forgotten entry can alias a future connection.
    pub(super) fn forget_session(&mut self, handle: ConnectionHandle) {
        if self.last_hello_from == Some(handle) {
            // The live session is going; a frame in flight is its.
            self.session_epoch += 1;
        }
        // The session-scoped instruments describe this connection; quinn
        // reuses handles, so a stale mirror would alias the next one.
        if self.last_hello_from == Some(handle) || self.departures_handle == Some(handle) {
            self.instr.reset_session_scope(0);
            self.release_departures();
        }
        self.wt_sessions.remove(&handle);
        self.feedback_recv_bufs.remove(&handle);
        self.close_deadlines.remove(&handle);
        self.rechecker.forget(&handle);
        if self.last_hello_from == Some(handle) {
            self.last_hello_from = None;
        }
    }

    /// A new QUIC connection has fully established — start its H3
    /// handshake.
    ///
    /// No stale-session prune here. It used to drop every other session on
    /// a new QUIC connection, which displaced an incumbent before it could
    /// be told why -- and let a connection that never identified itself
    /// kill a working session. Eviction is keyed on HELLO instead (see
    /// `apply_hello`). The page-reload flood the prune was written for is
    /// still covered: the reloaded client's HELLO arrives within a
    /// round-trip of `Connected` and evicts the stale session then -- see
    /// `reconnect_ends_with_one_session_and_a_cleared_emitter_cache`, which
    /// asserts exactly that. Only a connection that never identifies itself
    /// waits for quinn's idle timeout, and such a connection is not the one
    /// flooding you. If stale sessions ever *do* flood, the fix is to close
    /// the QUIC connection on eviction, not to prune on connect.
    ///
    /// Extracted to its own method (rather than inlined in the
    /// `Event::Connected` arm) so a unit test can call it directly and
    /// prove that property: reintroducing the deleted prune here makes
    /// `establishing_a_second_connection_does_not_disturb_an_existing_session`
    /// fail -- verified by hand, then reverted.
    pub(super) fn on_connection_established(&mut self, handle: ConnectionHandle) {
        // Closes the loop opened by the `connection lost` line: together the
        // two bound the interval during which the server was serving nobody.
        // Without `gap_secs` that interval could only be inferred from
        // timestamps, which is exactly the arithmetic you do not want to be
        // doing while reading a field report.
        let now = std::time::Instant::now();
        tracing::info!(
            target: LOG_TARGET,
            ?handle,
            gap_secs = self
                .last_disconnect_at
                .map(|t| now.saturating_duration_since(t).as_secs_f32()),
            carried_pending_entries = self.reliable_emitter.pending_cache_entries(),
            "connection established"
        );
        // Backstop against a reused slab key: quinn recycles handles, so any
        // state a previous holder left behind must not alias this
        // connection. Keep the never-HELLO deadline armed at accept time.
        let armed = self
            .close_deadlines
            .remove(&handle)
            .filter(|d| d.code == CLOSE_CODE_NO_HELLO);
        self.forget_session(handle);
        if let Some(d) = armed {
            self.close_deadlines.insert(handle, d);
        }
        // `forget_session` also dropped the connection's re-check clock that
        // `sweep_close_deadlines` started at accept; start it again.
        self.track_recheck(handle, now_std());
        let origin_policy = self.origin_policy.clone();
        let wt = self
            .wt_sessions
            .entry(handle)
            .or_insert_with(|| WebTransportServer::with_origin_policy(origin_policy));
        if let Some(conn) = self.server.connections.get_mut(&handle) {
            wt.on_new_connection(conn);
        }
    }

    /// Tell `handle` it has been displaced, then drop it.
    ///
    /// Best-effort: the notice is a datagram, so it can be lost, and the
    /// client then falls back to a generic disconnect. Sent three times
    /// because a single loss should not swallow it.
    ///
    /// **Three is a guess, not a measurement, and it buys less than it
    /// looks like.** The three copies are queued back-to-back, and quinn
    /// packs queued DATAGRAM frames into one packet while space remains
    /// (`connection/mod.rs`, the DATAGRAM loop) -- at ~15 bytes each they
    /// normally share a single UDP packet and are lost together. They help
    /// only when other traffic splits them across packets. Real loss
    /// resistance needs a reliable stream (M4a design §3), not a larger
    /// number.
    ///
    /// No grace-period sleep before dropping the session: `forget_session`
    /// only removes our own bookkeeping (`wt_sessions`, etc.) -- it does not
    /// touch `self.server.connections`, so the underlying QUIC connection
    /// stays open and keeps being driven by the normal `poll_transmit`
    /// loop. The eviction datagrams queued below are flushed on that
    /// schedule same as any other datagram; the connection itself is only
    /// reclaimed later, by quinn's idle timeout via `Event::ConnectionLost`.
    ///
    /// **The send below has zero unit-test coverage.** `self.server
    /// .connections.get_mut(&handle)` never yields `Some` in a unit-test
    /// bridge -- the test helpers attach a `wt_sessions` entry without ever
    /// running a real `quinn_proto` handshake, so the send is always
    /// skipped and `EVICTION_REPEATS` could be set to 0 without a single
    /// test failing. Driving it honestly needs a real `Connection`, which
    /// is what the milestone's end-to-end test (last task) exercises; that
    /// is the coverage for this half of the method, not anything here.
    pub(super) fn evict_session(&mut self, handle: ConnectionHandle, reason: EvictionReason) {
        // A guess, not a measurement -- see this method's doc comment.
        const EVICTION_REPEATS: usize = 3;
        self.session_epoch += 1;

        let datagram = build_eviction_datagram(reason);
        let mut notices_sent = 0usize;
        if let Some(conn) = self.server.connections.get_mut(&handle) {
            if let Some(wt) = self.wt_sessions.get_mut(&handle) {
                for _ in 0..EVICTION_REPEATS {
                    // Deliberately discarded: `Disabled` means the session
                    // is already gone (the outcome we are driving towards)
                    // and `Blocked` means quinn's send buffer is full,
                    // which retrying in this loop cannot drain. Count what
                    // left so the log says whether the client could have
                    // learned the reason -- eviction fires exactly when a
                    // session is loaded, so a full send buffer here is the
                    // likely state, not the exotic one (see the
                    // `datagram_send_errs` field note: these failures are
                    // overwhelmingly `Blocked`).
                    if wt.send_datagram(conn, &datagram).is_ok() {
                        notices_sent += 1;
                        // Not put in the departure tracker, though it is in
                        // quinn's FIFO: this session is forgotten just below,
                        // which discards the tracker with it, and the byte
                        // arithmetic reads the session's own counter, which
                        // counts it.
                        // Only the live session's sends are billed: the
                        // notice usually goes to a session that is no longer it.
                        if self.last_hello_from == Some(handle) {
                            let framing = wt.datagram_framing_len();
                            self.instr
                                .offered
                                .bill(WireClass::Control, datagram.len(), framing);
                            // Billed to the bucket after the fact, like every
                            // datagram that does not wait in a queue.
                            self.admission.charge(datagram.len() + framing);
                            self.instr.admission.soft_charged_bytes +=
                                (datagram.len() + framing) as u64;
                        }
                    }
                }
            }
        }
        // Same shape as the `connection lost` line, because an eviction is the
        // other way a session ends and a reader should not have to know which
        // happened to find the same facts. session_secs/discarded_* are read
        // BEFORE clear_cache() below, which is what destroys them.
        let now = std::time::Instant::now();
        tracing::info!(
            target: LOG_TARGET,
            ?handle,
            ?reason,
            notices_sent,
            session_secs = self
                .session_started_at
                .map(|t| now.saturating_duration_since(t).as_secs_f32()),
            discarded_pending_entries = self.reliable_emitter.pending_cache_entries(),
            discarded_bytes = self.reliable_emitter.bytes_in_flight(),
            "evicting session"
        );
        self.session_started_at = None;
        self.last_disconnect_at = Some(now);
        // Moved here from the old connect-time prune: the displaced
        // session's pending entries are un-ACKable, and left in place they
        // flood the link until RTO. `clear_cache()` is server-global, not
        // scoped to `handle` -- clearing it here is correct only because
        // of the one-client-at-a-time invariant this method enforces; a
        // future multi-client design would need a per-session cache.
        self.reliable_emitter.clear_cache();
        self.forget_session(handle);
        // Close the QUIC connection once the notice has had time to leave.
        // Inserted AFTER forget_session, which clears the handle's entry;
        // the sweep closes by handle and does not need `wt_sessions`.
        self.close_deadlines.insert(
            handle,
            CloseDeadline {
                at: now_std() + EVICT_CLOSE_GRACE,
                code: CLOSE_CODE_EVICTED,
            },
        );
    }

    fn takeover_rate_limited(&self, now: std::time::Instant) -> bool {
        self.last_takeover_at
            .is_some_and(|t| now.saturating_duration_since(t) < TAKEOVER_MIN_INTERVAL)
    }

    /// Re-check connected peers that are due, and close those that no longer
    /// pass. The lookup is the gate's: immediate sources answer inline, real
    /// ones are queued for `spawn_peer_lookups` (a blocking WhoIs on a worker
    /// thread, never on this loop) and answered through `apply_peer_lookup`.
    pub(super) fn sweep_rechecks(&mut self, now: std::time::Instant) {
        // Sessions are added when accepted and dropped by `forget_session`;
        // the full reconciliation below catches any that left without one
        // (a drained connection reaped by the server) and is only worth its
        // cost, a map over every connection, when something is due.
        if self.rechecker.next_due().is_none_or(|due| due > now) {
            return;
        }
        self.rechecker.sync(
            self.server
                .connections
                .iter()
                // A connection already closing (revoked, evicted) is not a
                // session to keep checking.
                .filter(|(_, c)| !c.is_closed())
                .map(|(h, c)| (*h, c.remote_address().ip())),
            now,
        );
        for handle in self.rechecker.run_due(self.server.peer_gate_mut(), now) {
            self.revoke_session(handle, now);
        }
    }

    /// Start the re-check clock of a live connection (a no-op for one that
    /// is gone or already tracked at the same address).
    fn track_recheck(&mut self, handle: ConnectionHandle, now: std::time::Instant) {
        if let Some(c) = self.server.connections.get(&handle) {
            self.rechecker.track(handle, c.remote_address().ip(), now);
        }
    }

    /// Close a connected session whose peer no longer passes the access rule.
    pub(super) fn revoke_session(&mut self, handle: ConnectionHandle, now: std::time::Instant) {
        tracing::warn!(
            target: LOG_TARGET,
            ?handle,
            "peer no longer passes the access rule; closing its session"
        );
        // Server-wide cleanup for the live session, then the close itself.
        // Inserted AFTER forget_session, which clears the handle's entry.
        self.on_connection_gone(handle, "access revoked");
        self.close_deadlines.insert(
            handle,
            CloseDeadline {
                at: now + REVOKE_CLOSE_GRACE,
                code: CLOSE_CODE_ACCESS_REVOKED,
            },
        );
    }

    /// Arm the never-HELLO deadline for a freshly accepted connection.
    pub(super) fn arm_hello_deadline(&mut self, handle: ConnectionHandle, now: std::time::Instant) {
        self.close_deadlines.insert(
            handle,
            CloseDeadline {
                at: now + HELLO_DEADLINE,
                code: CLOSE_CODE_NO_HELLO,
            },
        );
    }

    /// Arm deadlines for newly accepted connections, then close every
    /// connection whose deadline has passed. Run before the first
    /// `drain_outbound` of a turn so CONNECTION_CLOSE is transmitted.
    ///
    /// A local close emits no `ConnectionLost`, so the sweep does the
    /// per-handle cleanup itself; the drained connection is later reaped by
    /// `QuicServer::handle_timeout`, which runs `on_connection_gone` (a no-op
    /// for server-wide state, the handle no longer being live).
    pub(super) fn sweep_close_deadlines(&mut self, now: std::time::Instant) {
        for handle in self.server.take_accepted() {
            self.arm_hello_deadline(handle, now);
            // Its re-check clock starts here, so `sweep_rechecks` need not
            // rescan every connection on every turn to find new ones.
            self.track_recheck(handle, now);
        }
        let due: Vec<(ConnectionHandle, CloseDeadline)> = self
            .close_deadlines
            .iter()
            .filter(|(_, d)| d.at <= now)
            .map(|(h, d)| (*h, *d))
            .collect();
        for (handle, d) in due {
            self.close_deadlines.remove(&handle);
            if let Some(conn) = self.server.connections.get_mut(&handle) {
                let reason: &'static [u8] = match d.code {
                    CLOSE_CODE_EVICTED => b"evicted",
                    CLOSE_CODE_NO_HELLO => b"no HELLO before deadline",
                    CLOSE_CODE_TAKEOVER_RATE_LIMITED => b"session takeover rate limit",
                    CLOSE_CODE_ACCESS_REVOKED => b"access revoked",
                    _ => b"closed",
                };
                tracing::info!(target: LOG_TARGET, ?handle, code = d.code, "closing connection at deadline");
                conn.close(
                    now,
                    quinn_proto::VarInt::from_u32(d.code),
                    bytes::Bytes::from_static(reason),
                );
            }
            self.forget_session(handle);
        }
    }
}
