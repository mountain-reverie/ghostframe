//! The wire-facing emission tail: turning scheduled tile work into datagrams
//! on every connected session.
//!
//! Owns, as logic over `IoBridge` fields that stay in `io_bridge.rs`:
//! - `send_to_all_sessions`, the single fan-out point to the live WebTransport
//!   sessions, and `drain_reliable_emitter`, which feeds the reliable
//!   emitter's queue into it through the admission gate;
//! - `drain_scheduler_into_quinn` and `dispatch_dirty_tiles_via_scheduler`,
//!   which feed scheduler output through the emitter;
//! - `emit_frame_dimensions` and `compute_max_datagram_size`;
//! - `apply_injected_frame`, the browserless harness' stand-in for capture.
//!

use super::*;

impl IoBridge {
    /// Put what the reliable emitter has queued (source and parity) on the
    /// wire, through the admission gate: a datagram goes only while quinn's
    /// FIFO holds less than `D_target` and the bucket can afford its wire
    /// bytes, parity included. The first one that cannot go is put back at
    /// the head, untouched, and the drain stops; it is sent by a later pass.
    /// No borrow of the emitter is held across a send: the emission is taken
    /// out first and its outcome handed back afterwards.
    ///
    /// Without a live session, or before the first admission pass has set the
    /// gate, nothing is held back: a bridge driven directly sends as it
    /// always did.
    pub(super) fn drain_reliable_emitter(&mut self, now: std::time::Instant) -> pacing::Stop {
        use crate::transport::admission::{admit_one, Admit, AdmitClass};
        use crate::transport::reliable_emitter::emission_queue::Emission;
        while let Some(emission) = self.reliable_emitter.next_emission(now) {
            let (bytes, class) = match &emission {
                Emission::Source(bytes, class) => (bytes.clone(), *class),
                Emission::Parity(bytes) => (bytes.clone(), WireClass::Parity),
            };
            let mut charged = 0usize;
            if let Some(gate) = self.gate {
                if let Some(depth) = self.live_fifo_depth() {
                    let wire = bytes.len() + self.live_framing_len();
                    let admit_class = match class {
                        WireClass::NackRepair => AdmitClass::NackRepair,
                        WireClass::RtoRetransmit => AdmitClass::Retransmit,
                        WireClass::Parity => AdmitClass::Parity,
                        WireClass::Refinement => AdmitClass::Refinement,
                        // First sends and the scheduler's own retry are the
                        // scheduler's critical tier as far as the bucket is
                        // concerned; H.264 and control datagrams never come
                        // through the emitter.
                        WireClass::Critical
                        | WireClass::SchedulerRetry
                        | WireClass::H264
                        | WireClass::Control => AdmitClass::Critical,
                    };
                    match admit_one(&mut self.admission, admit_class, wire, depth, gate.d_target) {
                        Admit::Go => {
                            charged = wire;
                            self.instr.admission.charged_bytes += wire as u64;
                        }
                        Admit::Depth => {
                            self.reliable_emitter.hold_emission(emission);
                            return pacing::Stop::Depth;
                        }
                        Admit::Tokens => {
                            self.reliable_emitter.hold_emission(emission);
                            return pacing::Stop::Tokens;
                        }
                    }
                }
            }
            let outcome = self.send_counted(&bytes, class, true);
            if matches!(
                outcome,
                crate::transport::reliable_emitter::traits::SendOutcome::Rejected
            ) && charged > 0
            {
                // Refused by quinn: nothing was sent, so nothing is owed.
                self.admission.refund(charged);
                self.instr.admission.charged_bytes -= charged as u64;
            }
            if !self.reliable_emitter.settle_emission(emission, outcome) {
                return pacing::Stop::Rejected;
            }
        }
        pacing::Stop::Empty
    }

    /// Give back what superseded PalRle work was holding on its palette.
    ///
    /// Every superseded item gives back the palette reference Phase A took.
    /// Only a *bundled* one raised `in_flight_carrying`, so only it lowers it.
    /// The emissions in flight are still acknowledged later, and that
    /// acknowledgement would give the same count and reference back again, so
    /// the tile's PalRle coverage is unbundled here: a double release drives
    /// the count below the number of bundled emissions really outstanding.
    pub(super) fn settle_superseded_palettes(
        &mut self,
        tile_x: u8,
        tile_y: u8,
        superseded: &[crate::transport::scheduler::ResolvedTileWork],
    ) {
        let mut any_bundled = false;
        for s in superseded {
            let Some(pid) = s.palette_id else { continue };
            let pid_usize = pid as usize;
            if s.palette_bundled {
                any_bundled = true;
                let cnt = self.palette_table.in_flight_carrying[pid_usize];
                self.palette_table.in_flight_carrying[pid_usize] = cnt.saturating_sub(1);
            }
            // Release the acquire from Phase A. Supersession means the tile
            // work is dropped without ever being ACKed.
            if self.palette_table.ref_count[pid_usize] > 0 {
                self.palette_table.release(pid);
            }
        }
        if any_bundled {
            self.fragment_coverage
                .unbundle_palrle_for_tile(tile_x, tile_y);
        }
    }

    /// The live connection's raw QUIC `datagrams().max_size()`, before any
    /// overhead is subtracted. `None` with no live send handle, no such
    /// connection, or datagrams unsupported. Pure lookup: callers keep their
    /// own side effects and arithmetic.
    pub(super) fn live_max_datagram_size_raw(&mut self) -> Option<usize> {
        let handle = self.live_send_handle()?;
        let conn = self.server.connections.get_mut(&handle)?;
        conn.datagrams().max_size()
    }

    /// Compute the maximum datagram size from the smallest connected session.
    ///
    /// Side effect: calls `refresh_connected_session_count` so the capture
    /// gate's atomic stays current per frame.
    pub(super) fn compute_max_datagram_size(&mut self) -> Option<usize> {
        self.refresh_connected_session_count();
        let min_size = self
            .live_max_datagram_size_raw()
            .map(|sz| sz.saturating_sub(Self::WT_VARINT_OVERHEAD));
        let out = min_size.filter(|&sz| sz > 0);
        // Cached so `tick_budget_floor` can size itself in datagrams from
        // `&self`. Calling `compute_max_datagram_size` there instead would
        // re-run `refresh_connected_session_count` a second time per tick.
        self.last_max_datagram_size = out;
        out
    }

    /// Send a datagram to all connected WebTransport sessions.
    ///
    /// `class` says what the datagram is for; it only labels the bytes in the
    /// offered-load accounting.
    pub(super) fn send_to_all_sessions(
        &mut self,
        dg: &[u8],
        class: WireClass,
    ) -> crate::transport::reliable_emitter::traits::SendOutcome {
        self.send_counted(dg, class, false)
    }

    /// [`Self::send_to_all_sessions`], saying whether the datagram came
    /// through the admission gate (`gated`: it was charged to the bucket
    /// before it got here) or bypassed it (an H.264 frame or a control
    /// datagram, billed to the bucket afterwards).
    pub(super) fn send_counted(
        &mut self,
        dg: &[u8],
        class: WireClass,
        gated: bool,
    ) -> crate::transport::reliable_emitter::traits::SendOutcome {
        use crate::transport::reliable_emitter::traits::SendOutcome;
        #[cfg(test)]
        self.sent_log_for_test.push(dg.to_vec());
        #[cfg(any(test, feature = "test-loss-injection"))]
        if let Some(cap) = self.outbound_bandwidth_cap.as_mut() {
            if !cap.try_consume(dg.len()) {
                // A harness-injected drop *consumes* the datagram. Reporting
                // `Rejected` here would make the emitter re-queue the same
                // bytes forever and the injected loss would never happen.
                return SendOutcome::Sent;
            }
        }
        #[cfg(any(test, feature = "test-loss-injection"))]
        if let Some(inj) = self.outbound_loss.as_mut() {
            if inj.should_drop(dg) {
                return SendOutcome::Sent; // injected loss: consumed, see above
            }
        }
        #[cfg(any(test, feature = "test-loss-injection"))]
        if let Some(plan) = self.drop_plan.as_ref() {
            if plan
                .lock()
                .expect("drop_plan mutex poisoned")
                .should_drop(dg)
            {
                return SendOutcome::Sent; // injected drop: consumed, see above
            }
        }
        // Every datagram goes to every connected session, so one refusal
        // means re-queue. With more than one session that re-sends to the
        // ones that accepted; the duplicate is wasted bandwidth rather than
        // corruption -- the client dedups on wire_seq and tile reassembly is
        // idempotent per frag_idx. Production runs a single session.
        let mut rejected = false;
        // (cum bytes accepted, framing, FIFO depth) after quinn took it.
        let mut accepted: Option<(u64, usize, u64)> = None;
        let send_capacity = self.server.datagram_send_buffer();
        // Only the HELLO'd session receives stream data.
        let live = self.live_send_handle();
        if let Some(handle) = live.as_ref() {
            if let (Some(wt), Some(conn)) = (
                self.wt_sessions.get_mut(handle),
                self.server.connections.get_mut(handle),
            ) {
                let sent = wt.send_datagram(conn, dg);
                if sent.is_ok() {
                    accepted = Some((
                        wt.datagram_bytes_enqueued(),
                        wt.datagram_framing_len(),
                        send_capacity.saturating_sub(conn.datagrams().send_buffer_space()) as u64,
                    ));
                }
                if let Err(e) = sent {
                    rejected = true;
                    self.datagram_send_errs = self.datagram_send_errs.saturating_add(1);
                    if !self.datagram_send_err_first_logged {
                        self.datagram_send_err_first_logged = true;
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?handle,
                            error = ?e,
                            payload_len = dg.len(),
                            "first send_datagram Err — QUIC backpressure or limit; \
                             subsequent send failures will log at debug and a running \
                             total appears in the per-frame `dispatch returned` log"
                        );
                    } else {
                        tracing::debug!(target: LOG_TARGET, ?handle, error = ?e, "datagram send failed");
                    }
                }
            }
        }

        if let (Some(handle), Some((cum, framing, queued))) = (live, accepted) {
            self.note_enqueued(
                handle,
                Enqueued {
                    class,
                    gated,
                    dg_len: dg.len(),
                    framing_len: framing,
                    cum_enqueued: cum,
                    queued,
                },
            );
        }

        // No connected session counts as sent, not rejected: re-queueing a
        // datagram with nowhere to go would spin the drain forever.
        if rejected {
            SendOutcome::Rejected
        } else {
            SendOutcome::Sent
        }
    }

    /// Emit the frame-dimensions datagram on the first frame of each session and
    /// whenever dimensions change, retransmitting `FRAME_DIMENSIONS_RETRANSMITS`
    /// additional times to absorb datagram loss. Called by both
    /// `process_frame_cpu` and `process_frame_gpu`.
    pub(super) fn emit_frame_dimensions(
        &mut self,
        seq: u32,
        timestamp_us: u32,
        width: u32,
        height: u32,
    ) {
        let dims_changed = self.last_emitted_dimensions != Some((width, height));
        if dims_changed {
            self.dimensions_retransmits_left = FRAME_DIMENSIONS_RETRANSMITS;
            self.last_emitted_dimensions = Some((width, height));
        }
        if self.dimensions_retransmits_left > 0 {
            // Not behind the depth gate, like the eviction notice: ~20 bytes
            // that the client cannot do without (it refuses tiles larger than
            // the dimensions it knows). This runs once per frame, before the
            // admission pass, and in a cwnd-limited steady state the FIFO
            // stands at `D_target` whenever a frame arrives, so a gated send
            // would starve for as long as the link stays saturated while
            // tiles were admitted regardless. It is soft-charged to the
            // bucket (`note_enqueued`), so the classes that can wait pay.
            let dg = crate::transport::protocol::build_frame_dimensions_datagram(
                seq,
                timestamp_us,
                width,
                height,
            );
            self.send_to_all_sessions(&dg, WireClass::Control);
            self.dimensions_retransmits_left -= 1;
        }
    }

    /// Send `tiles` as CDF 5/3: forward-transform them on the GPU in one round
    /// trip, encode the passes, and commit each through the same path the
    /// frame's own Cdf53 tiles take. See the caller for which tiles come here.
    ///
    /// Software encoding is not a runtime option. With no GPU processor
    /// (which the GPU policy cannot reach in production) the tiles are left
    /// uncommitted and an error is logged; unit-test builds use the CPU
    /// forward as scaffolding so the routing can be tested without a GPU.
    fn commit_cdf53_fallback(
        &mut self,
        tiles: &[(u32, u32)],
        grid: &crate::tile::TileGrid,
        pixels: &[u8],
        stride: u32,
    ) -> Vec<(u32, u32)> {
        // Tiles the frame worker already transformed and encoded in the frame's
        // own finish; the rest (a PalRle tile that lost its palette at commit)
        // still need the GPU, which is a round trip to the worker.
        let mut encoded: std::collections::HashMap<(u32, u32), super::frame_backend::Cdf53Encoded> =
            std::collections::HashMap::new();
        let mut missing: Vec<(u32, u32)> = Vec::new();
        for &xy in tiles {
            match self.precomputed_fallback.remove(&xy) {
                Some(e) => {
                    encoded.insert(xy, e);
                }
                None => missing.push(xy),
            }
        }
        let mut failed: Vec<(u32, u32)> = Vec::new();
        if !missing.is_empty() {
            let tile_bgra: Vec<Vec<u8>> = missing
                .iter()
                .map(|&(x, y)| grid.extract_tile(pixels, stride, x, y))
                .collect();
            let refs: Vec<&[u8]> = tile_bgra.iter().map(|t| t.as_slice()).collect();
            let n = crate::encoder::cdf53::CDF53_TOTAL_COEFFS;
            // A round trip to the frame worker, which owns the GPU. The loop
            // waits for it (see `FramePipeline::forward_cdf53_tiles`).
            let forwarded = self
                .frame_pipeline
                .as_ref()
                .map(|p| p.forward_cdf53_tiles(&refs));
            let coeffs: Option<Vec<i16>> = match forwarded {
                Some(Ok(c)) => Some(c),
                Some(Err(e)) => {
                    tracing::error!(target: LOG_TARGET, error = %e, tiles = missing.len(),
                        "cdf53 fallback forward failed; tiles not sent this frame");
                    // Report the whole GPU baseline as stale so the next frame
                    // marks these tiles dirty again and they are retried.
                    self.backend_apply(super::frame_backend::Control::InvalidateBaseline(1));
                    None
                }
                #[cfg(test)]
                None => Some(
                    refs.iter()
                        .flat_map(|t| crate::encoder::cdf53::forward(t))
                        .collect(),
                ),
                #[cfg(not(test))]
                None => {
                    tracing::error!(target: LOG_TARGET, tiles = missing.len(),
                        "cdf53 fallback needs the frame backend; tiles not sent this frame");
                    None
                }
            };
            match coeffs {
                Some(coeffs) => {
                    for (i, &xy) in missing.iter().enumerate() {
                        let (present_passes, passes) = crate::encoder::cdf53::encode_passes_sparse(
                            &coeffs[i * n..(i + 1) * n],
                        );
                        encoded.insert(
                            xy,
                            super::frame_backend::Cdf53Encoded {
                                present_passes,
                                passes,
                            },
                        );
                    }
                }
                None => failed = missing,
            }
        }
        for &(x, y) in tiles {
            let Some(e) = encoded.remove(&(x, y)) else {
                continue;
            };
            // Supersede what the tile had in flight the way the dirty loop
            // does: the plain bump in the Cdf53 commit settles nothing, so a
            // PalRle emission still queued would keep its palette reference
            // and carry count for good, and a full table is what sends tiles
            // here.
            self.cancel_in_flight_for_tile(x as u8, y as u8);
            let (_, superseded) = self.scheduler.bump_generation_collecting(x as u8, y as u8);
            self.settle_superseded_palettes(x as u8, y as u8, &superseded);
            self.commit_tile_to_cdf53(
                x as u8,
                y as u8,
                e.present_passes,
                e.passes,
                super::frame::Cdf53Source::Dirty,
            );
        }
        failed
    }

    /// Shared scheduler dispatch: grid-sync → RTT update → bump+encode+enqueue
    /// per dirty tile → tick → fragment+send. Called by both `process_frame_cpu`
    /// and `process_frame_gpu`'s `FrameMode::TileCodec` branch.
    ///
    /// Returns a `FrameSendStats` accumulating wire bytes and codec counts for
    /// all tiles sent this invocation. The caller uses this to emit
    /// `frame.last_send` after the final datagram is handed to QUIC.
    pub(crate) fn dispatch_dirty_tiles_via_scheduler(
        &mut self,
        dirty: &[(u32, u32)],
        grid: &crate::tile::TileGrid,
        frame: TileDispatchFrame<'_>,
        max_frag: usize,
        policy: SchedulerEmissionPolicy,
        mut palrle_payloads: Option<&mut std::collections::HashMap<(u32, u32), Vec<u8>>>,
    ) -> FrameSendStats {
        let TileDispatchFrame {
            pixels,
            stride,
            seq,
            timestamp_us,
        } = frame;
        // Grid sync — keep scheduler in lockstep with the dirty-detection grid.
        if self.scheduler.cols() != grid.cols || self.scheduler.rows() != grid.rows {
            self.scheduler.resize(grid.cols, grid.rows);
        }

        use crate::transport::scheduler::{TileWork, WorkState};

        // A dirty tile the tile encoders cannot carry goes out as CDF 5/3
        // (lossless once every pass lands). The CPU policy has no tile
        // encoders at all: every dirty tile is such a tile. On the GPU policy
        // it is every tile whose state has no tile-local encoder (Skip,
        // H264, PixelPerfect) and a PalRle tile whose payload never got made
        // (palette table full, frame-palette overflow). The frame's own
        // compact pass lists only dirty tiles with more than 16 colours, so
        // these tiles have no coefficients yet: one extra GPU forward for
        // exactly this set, and nothing when the set is empty. Committing
        // sets each tile's state to Cdf53, so the loop below skips it like
        // any other Cdf53 tile.
        let mut failed_fallback: Vec<(u32, u32)> = Vec::new();
        {
            use crate::tile::CodecState;
            let has_payload =
                |palrle: &Option<&mut std::collections::HashMap<(u32, u32), Vec<u8>>>,
                 xy: (u32, u32)| {
                    palrle.as_ref().is_some_and(|m| m.contains_key(&xy))
                };
            let fallback: Vec<(u32, u32)> = match policy {
                #[cfg(test)]
                SchedulerEmissionPolicy::TestCdf53All => dirty.to_vec(),
                SchedulerEmissionPolicy::GpuClassifierDriven => dirty
                    .iter()
                    .copied()
                    .filter(|&(x, y)| match self.metrics_tracker.get(x, y).codec_state {
                        CodecState::Cdf53 { .. } | CodecState::Solid => false,
                        CodecState::PalRle { .. } => !has_payload(&palrle_payloads, (x, y)),
                        CodecState::H264 { .. } | CodecState::PixelPerfect | CodecState::Skip => {
                            true
                        }
                    })
                    .collect(),
            };
            if !fallback.is_empty() {
                failed_fallback = self.commit_cdf53_fallback(&fallback, grid, pixels, stride);
            }
        }

        for &(tile_x, tile_y) in dirty {
            // A fallback tile whose forward failed is retried next frame
            // (the baseline was invalidated); nothing else can carry it.
            if failed_fallback.contains(&(tile_x, tile_y)) {
                continue;
            }
            // The CPU policy sent every dirty tile through the fallback above.
            if policy.is_test_route() {
                continue;
            }
            // For GpuClassifierDriven, check whether this tile is handled by
            // Path B (Cdf53 refinement queue, enqueued above by Phase B) BEFORE
            // calling bump_generation_collecting. bump_generation supersedes all
            // queued work for the tile — so calling it on a Cdf53 tile would
            // invalidate the refinement passes Phase B just enqueued.
            if matches!(policy, SchedulerEmissionPolicy::GpuClassifierDriven) {
                use crate::tile::CodecState;
                let codec_state = self.metrics_tracker.get(tile_x, tile_y).codec_state;
                if matches!(codec_state, CodecState::Cdf53 { .. }) {
                    // Cdf53 state → Phase B handles emission via
                    // the refinement queue (generation already bumped by Phase B).
                    // Skip entirely: do NOT bump generation (would supersede Phase
                    // B's enqueued passes), do NOT enqueue to priority_queue.
                    continue;
                }
            }

            // Drop retransmit/cache state in the reliable emitter in lock-step
            // with scheduler supersession. Paired with every bump_generation*
            // call (replaces the Task 20 set_cancel_callback machinery, which
            // captured a raw pointer to self.reliable_emitter that dangled when
            // the bridge was moved out of `new`).
            self.cancel_in_flight_for_tile(tile_x as u8, tile_y as u8);
            let (gen, superseded) = self
                .scheduler
                .bump_generation_collecting(tile_x as u8, tile_y as u8);
            self.bump_count_accumulator.priority_path =
                self.bump_count_accumulator.priority_path.saturating_add(1);
            // New generation: tile is eligible for re-escalation.
            // Guard with a bounds check to stay safe when metrics_tracker has
            // not yet been sized (e.g. a unit-test bridge).
            if tile_x < self.metrics_tracker.cols() && tile_y < self.metrics_tracker.rows() {
                self.metrics_tracker
                    .get_mut(tile_x, tile_y)
                    .already_escalated_this_gen = false;
            }
            self.settle_superseded_palettes(tile_x as u8, tile_y as u8, &superseded);
            // Only a Solid tile (one pixel) or a PalRle tile (already
            // encoded) is sent here. Everything else went through the CDF 5/3
            // fallback above, so reaching this point with another state means
            // the pre-pass and this match disagree.
            use crate::tile::CodecState;
            let codec_state = self.metrics_tracker.get(tile_x, tile_y).codec_state;
            let (codec, payload) = match codec_state {
                CodecState::Solid => {
                    let solid = grid.first_pixel(pixels, stride, tile_x, tile_y);
                    (Codec::Solid, solid.to_vec())
                }
                CodecState::PalRle { .. } => {
                    match palrle_payloads
                        .as_mut()
                        .and_then(|m| m.remove(&(tile_x, tile_y)))
                    {
                        Some(p) => (Codec::PalRle, p),
                        None => {
                            debug_assert!(
                                false,
                                "PalRle tile without payload was not sent as CDF 5/3"
                            );
                            continue;
                        }
                    }
                }
                CodecState::H264 { .. }
                | CodecState::Cdf53 { .. }
                | CodecState::PixelPerfect
                | CodecState::Skip => {
                    debug_assert!(false, "{codec_state:?} tile was not sent as CDF 5/3");
                    continue;
                }
            };

            let now = now_std();
            self.scheduler.enqueue_at(
                TileWork {
                    tile_x: tile_x as u8,
                    tile_y: tile_y as u8,
                    generation: gen,
                    pass_idx: 0,
                    total_passes: 1,
                    codec,
                    payload,
                    queued_at: now,
                    last_sent_at: None,
                    state: WorkState::Pending,
                },
                now,
            );
        }

        // ---- Admission ----
        //
        // Per-tick rate = measured QUIC bandwidth (bytes/us from path stats)
        // x headroom, billed by elapsed time into the one bucket every byte
        // class draws on, and held behind the depth gate on quinn's FIFO. The
        // scheduler retains what is not admitted, so a 14 MB first-frame
        // burst spreads over as many turns as the rate needs rather than
        // being dumped on quinn in one go. See `pump_admission`.
        //
        // The test route bypasses this with `usize::MAX` so unit fixtures
        // that pre-seed tiles still drain in one tick.
        let (stats, _drained_bytes, _drained_count) = match policy {
            #[cfg(test)]
            SchedulerEmissionPolicy::TestCdf53All => {
                self.drain_scheduler_into_quinn(seq, timestamp_us, max_frag, usize::MAX)
            }
            SchedulerEmissionPolicy::GpuClassifierDriven => {
                self.emit_via_scheduler(seq, timestamp_us, max_frag)
            }
        };

        stats
    }

    /// Drain the scheduler up to `budget_bytes`, fragment+send each
    /// returned `TileWork`, and return the per-codec / wire-byte stats
    /// alongside the total bytes drained and the number of TileWork
    /// items emitted. Shared between the per-frame dispatch entry point
    /// (`dispatch_dirty_tiles_via_scheduler`) and the admission pass that
    /// runs after every `drain_outbound` (`refill_after_drain`) — both want
    /// the same pull-from-scheduler / fragment / record-coverage / send
    /// loop, the only difference being the budget they pass in.
    ///
    /// `seq` / `timestamp_us` tag the outbound fragments with the
    /// *current* logical frame's identity; refinement work queued under
    /// an earlier frame still gets tagged with the current `seq` (which
    /// is what the client expects from a continuous stream).
    pub(super) fn drain_scheduler_into_quinn(
        &mut self,
        seq: u32,
        timestamp_us: u32,
        max_frag: usize,
        budget_bytes: usize,
    ) -> (FrameSendStats, usize, usize) {
        // Record this call's frame identity unconditionally, before any of
        // the budget/queue-dependent logic below, so a later probe-window
        // open always has the most recent valid `seq` to reuse — see
        // `DrainFrameContext` and `open_probe_window_for_burst`.
        self.last_drain_frame_context = Some(DrainFrameContext {
            seq,
            timestamp_us,
            max_frag,
        });
        let now = now_std();
        // The emitter owns recovery for whatever it still has cached; the
        // scheduler re-sends only what the emitter has let go of. See
        // `Scheduler::tick_at_with`.
        let emitter = &self.reliable_emitter;
        let drained = self
            .scheduler
            .tick_at_with(budget_bytes, now, &|x, y, p| emitter.holds_pass(x, y, p));
        let drained_count = drained.len();
        // Which of `drained` are scheduler re-sends (offered-load label only).
        let retry_positions: Vec<usize> = self.scheduler.last_tick_retries().to_vec();
        let mut stats = FrameSendStats::default();
        let mut total_wire_bytes_sent: usize = 0;
        // Build per-fragment (EmitKey, Bytes) items for the reliable
        // emitter. The emitter stamps wire_seq into bytes[8..12], caches
        // for retransmit, schedules RTO, and queues parity per FEC group.
        // The actual wire write happens in `drain_reliable_emitter(now)`
        // below.
        let mut items: Vec<(
            crate::transport::reliable_emitter::EmitKey,
            bytes::Bytes,
            std::time::Instant,
            Option<crate::transport::reliable_emitter::cache::ProbeTag>,
        )> = Vec::with_capacity(drained.len());
        let mut retry_flags: Vec<bool> = Vec::with_capacity(drained.len());
        let mut next_retry = 0usize;
        for (pos, work) in drained.into_iter().enumerate() {
            let is_retry = retry_positions.get(next_retry) == Some(&pos);
            if is_retry {
                next_retry += 1;
            }
            // The identity this tile's generation was first sent under, not
            // this frame's: see `SlotMap::label_or_assign`.
            let frame_seq_with_flag =
                self.scheduler.frame_label(work.tile_x, work.tile_y, seq) | TILE_DATAGRAM_FLAG;
            // A tile pass is one datagram: no payload exceeds `MAX_TILE_PAYLOAD`
            // and frame dispatch refuses a path too small for that, so this
            // check is a backstop for a caller that skipped it.
            if work.payload.len() > max_frag {
                tracing::error!(
                    target: LOG_TARGET,
                    payload = work.payload.len(),
                    max_frag,
                    tile_x = work.tile_x,
                    tile_y = work.tile_y,
                    "tile payload does not fit one datagram; tile not sent"
                );
                continue;
            }
            let datagrams = vec![encode_tile_datagram(
                &TileFragmentInputs {
                    frame_seq: frame_seq_with_flag,
                    tile_x: work.tile_x,
                    tile_y: work.tile_y,
                    codec: work.codec,
                    generation: work.generation,
                    pass: work.pass_idx,
                    timestamp_us,
                },
                &work.payload,
            )];
            // Record fragment coverage: one record per tile-pass (not per
            // fragment), keyed (frame_seq, tile, pass). It is kept
            // alongside the retransmit cache on purpose, not as a
            // leftover: it outlives the cache entry after supersession,
            // `retire_stuck` and `clear_cache`, and late ACKs read it
            // (see the `fragment_coverage` module docs).
            let n = datagrams.len();
            // Declared outside the `if n > 0` block so that the per-tier
            // byte counters below (Phase 1 Task 6) can read it after the
            // block closes.
            let mut tile_wire_bytes_total: u32 = 0;
            if n > 0 {
                let palette_id = if work.codec == crate::transport::protocol::Codec::PalRle {
                    // PalRle payload byte 1 is the persistent palette slot id
                    // (byte 0 is the bundled-flag); guarded against malformed
                    // payloads via .get().
                    work.payload.get(1).copied()
                } else {
                    None
                };
                // Byte 0 bit 0 is the bundled flag (see `pal_rle::decode`).
                // Recorded so the acknowledgement path decrements
                // `in_flight_carrying` only for the emissions that
                // incremented it.
                let palette_bundled = work.codec == crate::transport::protocol::Codec::PalRle
                    && work.payload.first().is_some_and(|f| f & 0x01 != 0);
                let coverage: crate::transport::fragment_coverage::CoverageList =
                    smallvec![crate::transport::fragment_coverage::FragmentCoverage {
                        tile_x: work.tile_x,
                        tile_y: work.tile_y,
                        generation: work.generation,
                        pass_idx: work.pass_idx,
                        codec: work.codec,
                        palette_id,
                        palette_bundled,
                    }];
                let key = (frame_seq_with_flag, work.tile_x, work.tile_y, work.pass_idx);
                self.fragment_coverage.record(key, coverage);
                if work.pass_idx == 0 {
                    stats.tile_count = stats.tile_count.saturating_add(1);
                    match work.codec {
                        Codec::Solid => {
                            stats.codec_histogram.solid =
                                stats.codec_histogram.solid.saturating_add(1);
                        }
                        Codec::PalRle => {
                            stats.codec_histogram.palrle =
                                stats.codec_histogram.palrle.saturating_add(1);
                        }
                        Codec::Cdf53 => {
                            stats.codec_histogram.cdf53 =
                                stats.codec_histogram.cdf53.saturating_add(1);
                        }
                        // No histogram bucket: H264 is counted on the
                        // whole-frame path, Skip emits no datagram. Listed so
                        // a new codec cannot go uncounted without someone
                        // deciding that it should.
                        Codec::H264 | Codec::Skip => {}
                    }
                }
                let tile_wire_bytes: u32 = datagrams.iter().map(|dg| dg.len() as u32).sum();
                tile_wire_bytes_total = tile_wire_bytes;
                stats.total_wire_bytes = stats.total_wire_bytes.saturating_add(tile_wire_bytes);
                total_wire_bytes_sent =
                    total_wire_bytes_sent.saturating_add(tile_wire_bytes as usize);
            }
            // Cumulative per-codec emit counter — bumped once per
            // datagram fragment so the cadence matches the client's
            // per-datagram `rx{...}` accounting. Counted before send so
            // the "emitted - send_errs" subtraction reflects actual
            // wire-attempts; loss between emit and client is then
            // `(emitted - send_errs) - client.rx`.
            let n_dg = datagrams.len() as u64;
            match work.codec {
                Codec::Solid => {
                    self.cumulative_datagrams_emitted.solid =
                        self.cumulative_datagrams_emitted.solid.saturating_add(n_dg);
                }
                Codec::PalRle => {
                    self.cumulative_datagrams_emitted.palrle = self
                        .cumulative_datagrams_emitted
                        .palrle
                        .saturating_add(n_dg);
                }
                Codec::Cdf53 => {
                    self.cumulative_datagrams_emitted.cdf53 =
                        self.cumulative_datagrams_emitted.cdf53.saturating_add(n_dg);
                    // Per-tier byte attribution (Phase 1 Task 6). Uses the
                    // real total wire bytes summed from each datagram fragment
                    // (headers included), set in `tile_wire_bytes_total` above.
                    // Counts every successful emit including retransmits so the
                    // BWE consumer's rate computation reflects actual wire
                    // utilization, not initial-emit accounting.
                    let bytes_this_tile = tile_wire_bytes_total as u64;
                    match pass_tier(work.pass_idx) {
                        PassTier::Critical => {
                            self.bytes_emitted_critical =
                                self.bytes_emitted_critical.saturating_add(bytes_this_tile);
                        }
                        PassTier::Refinement => {
                            self.bytes_emitted_refinement = self
                                .bytes_emitted_refinement
                                .saturating_add(bytes_this_tile);
                        }
                    }
                }
                // Same reasoning as the histogram above: no per-codec
                // datagram counter for these, stated rather than defaulted.
                Codec::H264 | Codec::Skip => {}
            }
            // Hand every fragment to the reliable emitter under the
            // same EmitKey (the per-tile-pass identity used by ACK/NACK).
            // Use `frame_seq_with_flag` (= seq | TILE_DATAGRAM_FLAG) so
            // the cache key matches the wire frame_seq embedded in the
            // datagram header — that's what the client echoes back in
            // every ACK/NACK entry, and what `fragment_coverage` already
            // uses one line up.  Indexing the cache by raw `seq` made
            // every ACK lookup miss, so every entry sat in the cache
            // until RTO fired MAX_RETRANSMITS times and the entry
            // retired, which presents as PixelPerfect-never-fires on
            // an otherwise idle gradient.
            let key = crate::transport::reliable_emitter::EmitKey::new(
                frame_seq_with_flag,
                work.tile_x,
                work.tile_y,
                work.pass_idx,
            );
            for dg in datagrams {
                // BWE Stage 2.4: tag this datagram for the active probe
                // window, if any -- mirrors `queued_at`'s treatment above:
                // the tag travels with the pass from here through
                // `CacheEntry` to the ACK path. `bytes_sent_before` is the
                // cluster's running total *before* this datagram (goog_cc's
                // `probe_cluster_bytes_sent` semantics), so it must be read
                // before `bytes_sent` is advanced by this datagram's size.
                let dg_len = dg.len() as i64;
                let probe_tag = self.active_probe.as_mut().map(|p| {
                    let tag = crate::transport::reliable_emitter::cache::ProbeTag {
                        id: p.id,
                        min_probes: p.min_probes,
                        min_bytes: p.min_bytes,
                        bytes_sent_before: p.bytes_sent,
                    };
                    p.bytes_sent = p.bytes_sent.saturating_add(dg_len);
                    p.packets_sent = p.packets_sent.saturating_add(1);
                    tag
                });
                items.push((key, bytes::Bytes::from(dg), work.queued_at, probe_tag));
                retry_flags.push(is_retry);
            }
        }
        // Submit the batch (stamps wire_seq, caches, schedules RTO + parity).
        self.reliable_emitter
            .submit_batch_marking_retries(items, &retry_flags, now);
        self.drain_reliable_emitter(now);
        (stats, total_wire_bytes_sent, drained_count)
    }

    /// The transport's half of superseding a tile: forget what the retransmit
    /// cache still holds for it and the Cdf53 coverage of passes in flight.
    ///
    /// Every place that replaces a tile's content pairs this with a scheduler
    /// bump (or, in the browserless harness, a plain supersede), so that no
    /// pass of an older generation is retransmitted after a newer one exists.
    /// A client discards such a pass without acknowledging it, so a
    /// retransmit of it can never be settled and repeats until the cache
    /// retires it. The two calls live together so a caller cannot take one
    /// and forget the other, which is how the harness drifted from production.
    ///
    /// Cdf53 coverage is dropped (not PalRle's): the bump marked the matching
    /// refinement-queue work `Superseded`, and coverage of the cancelled
    /// passes must go in lockstep or `pending_refinement_snapshot` keeps
    /// reporting them as pending until LRU eviction. See
    /// `FragmentCoverageMap::drop_cdf53_for_tile`.
    pub(super) fn cancel_in_flight_for_tile(&mut self, tile_x: u8, tile_y: u8) {
        self.reliable_emitter.cancel_for_tile(tile_x, tile_y);
        self.fragment_coverage.drop_cdf53_for_tile(tile_x, tile_y);
    }

    /// Enqueue an injected frame's tile work into the scheduler, then
    /// attempt to drain it to the wire. Used by both the `run()` select
    /// arm and the `drain_injection_for_test` test accessor, so the two
    /// paths cannot drift apart.
    ///
    /// Enqueuing alone would leave the work sitting in the scheduler
    /// forever: `drain_scheduler_into_quinn` is the only thing that turns
    /// queued `TileWork` into datagrams (see its two production callers,
    /// `dispatch_dirty_tiles_via_scheduler` and the admission pass after
    /// each drain). If there's no connected session yet,
    /// `compute_max_datagram_size` returns `None` and the drain is
    /// skipped — the work stays queued for the next opportunity.
    ///
    /// Returns the number of tiles actually enqueued into the scheduler.
    /// This is NOT the number of tiles submitted in `inj.work`: tiles
    /// outside the fixed grid are skipped by the bounds check below, so
    /// the two counts diverge whenever a harness scene mis-sizes its
    /// grid. Callers that need to know work was dropped (as opposed to
    /// merely queued) must use this return value, not `inj.work.len()`.
    pub(super) fn apply_injected_frame(&mut self, inj: InjectedFrame) -> usize {
        // The scheduler grid is normally sized by the real capture path
        // (`dispatch_dirty_tiles_via_scheduler`, keyed off the frame's
        // actual dimensions via `TileGrid`). Injected work bypasses that
        // path entirely, so on a bridge that only ever receives injected
        // frames the scheduler is still at its construction-time (0, 0)
        // and `Scheduler::enqueue`'s bounds `debug_assert` would fire for
        // any real tile coordinate. `new_with_injection_for_test` sizes
        // the grid once, up front, from the scene's known dimensions.
        //
        // Never resize here: `Scheduler::resize` clears the priority
        // queue, the refinement queue, and every CDF53 pass-ACK record,
        // so growing the grid mid-scene would silently discard in-flight
        // work — e.g. a multi-frame scene that rewrites tile (0,0) across
        // several frames would have its queued/refinement/ACK state wiped
        // the moment a later frame first touches a tile outside the
        // original bounding box. That failure mode is invisible at the
        // call site: tiles simply never converge, with no error anywhere.
        // Coordinates outside the fixed grid are a harness-author bug —
        // surface them loudly instead of growing around them.
        let cols = self.scheduler.cols();
        let rows = self.scheduler.rows();
        let mut enqueued_count = 0usize;
        // Accumulate out-of-grid tiles instead of logging per-tile: a
        // mis-sized harness grid injects whole frames of out-of-grid work
        // at scene rate (e.g. 60 tiles/frame x 30 fps), and a five-line
        // `error!` per tile would bury the one summary line that actually
        // matters under ~1800 identical records per second.
        let mut out_of_grid_skipped = 0usize;
        let mut first_out_of_grid: Option<(u8, u8)> = None;

        // Newer content for a tile must invalidate whatever is still queued
        // for it, exactly as the capture path's `bump_generation` does. Skip
        // this and two versions of the same tile sit in the queue at once;
        // whichever drains later is rendered last, and since
        // `drain_scheduler_into_quinn` stamps every tile it drains with the
        // *draining* frame's seq rather than the enqueuing frame's, the older
        // content can arrive labelled newer and win.
        //
        // Done once per distinct tile before the enqueue loop, not per work
        // item: a Cdf53 tile contributes 14 pass items for the same
        // coordinate, and superseding per item would drop this very frame's
        // earlier passes.
        let mut superseded: Vec<(u8, u8)> = Vec::new();
        for work in &inj.work {
            if work.tile_x as u32 >= cols || work.tile_y as u32 >= rows {
                continue;
            }
            let coord = (work.tile_x, work.tile_y);
            if !superseded.contains(&coord) {
                // The scheduler's half of a bump (the harness supplies its own
                // generations, so it does not bump) AND the transport's half.
                // Without the latter the superseded generations' passes sit in
                // the retransmit cache and are re-sent by RTO for up to 30 s.
                self.cancel_in_flight_for_tile(coord.0, coord.1);
                self.scheduler.supersede_pending_for_tile(coord.0, coord.1);
                superseded.push(coord);
            }
        }

        for work in inj.work {
            if work.tile_x as u32 >= cols || work.tile_y as u32 >= rows {
                out_of_grid_skipped += 1;
                first_out_of_grid.get_or_insert((work.tile_x, work.tile_y));
                continue;
            }
            // Route CDF53 passes through `refinement_queue` so they get
            // `drain_refinement_pass_major`'s pass-major drain, exactly as
            // the production capture path does via `enqueue_refinement_at`.
            // Everything else (Solid/PalRle/Raw single-pass work) keeps
            // going through `enqueue_at` -> `priority_queue` -> FIFO drain,
            // unchanged. `enqueue_refinement_work_at` pushes the harness's
            // already-formed `TileWork` as-is, preserving its `pass_idx`/
            // `total_passes` exactly rather than rebuilding them by
            // position — load-bearing when a scene submits a partial or
            // out-of-order set of passes. See
            // docs/specs/bwe-tier-latency-baseline.md's "Correction: the
            // harness never exercises pass-major ordering".
            if work.codec == crate::transport::protocol::Codec::Cdf53 {
                self.scheduler.enqueue_refinement_work_at(work, now_std());
            } else {
                self.scheduler.enqueue_at(work, now_std());
            }
            enqueued_count += 1;
        }
        if let Some((tile_x, tile_y)) = first_out_of_grid {
            tracing::error!(
                target: LOG_TARGET,
                skipped = out_of_grid_skipped,
                first_tile_x = tile_x,
                first_tile_y = tile_y,
                grid_cols = cols,
                grid_rows = rows,
                "apply_injected_frame: tile coordinate(s) outside the \
                 scheduler grid fixed at construction — skipped (first \
                 offender shown, `skipped` is the count for this frame). \
                 Pass the scene's true grid_cols/grid_rows to \
                 new_with_injection_for_test instead of relying on \
                 growth-on-demand."
            );
        }
        // This gate is load-bearing for dimensions, not just for the
        // drain: `emit_frame_dimensions` below only runs inside it. With
        // no session connected yet, `compute_max_datagram_size()` returns
        // `None` and the whole block — including `emit_frame_dimensions`
        // — is skipped for that frame. Emitting dimensions unconditionally
        // here would burn all of `FRAME_DIMENSIONS_RETRANSMITS` into the
        // void with nobody connected to receive them; by the time a client
        // does connect, `dims_changed` in `emit_frame_dimensions` is false
        // (the dimensions haven't changed since the burned attempts) and
        // the retransmit counter is already exhausted, so the new client
        // would never learn the grid size. `fire_session_reset` re-arming
        // `dimensions_retransmits_left` (`:2468`) is what actually covers
        // reconnects — this gate just has to not waste the budget before
        // there is anyone to receive it.
        if let Some(max_frag) = self.compute_max_datagram_size() {
            // Tell the client the canvas / tile-grid size before draining
            // any tile work, mirroring process_frame_cpu (:2388) and
            // process_frame_gpu (:2500) — the injected path bypasses capture
            // entirely, so nothing else on this path would ever emit frame
            // dimensions, and a browserless client would receive tile
            // datagrams for a grid it was never told the size of.
            // `emit_frame_dimensions` only re-arms its retransmit counter
            // when (width, height) actually changes (`last_emitted_dimensions`,
            // :1241-1245), so calling it on every injected frame is safe even
            // though the harness's grid is fixed for the bridge's lifetime.
            // Width/height come from the scheduler grid fixed at construction
            // (`cols`/`rows` above) scaled by the same `crate::tile::TILE_SIZE`
            // (32px) edge the real capture path derives its grid from
            // (`tile::mod.rs`'s `TileGrid::new`).
            self.emit_frame_dimensions(
                inj.seq,
                inj.timestamp_us,
                cols * crate::tile::TILE_SIZE,
                rows * crate::tile::TILE_SIZE,
            );

            // The admission pass -- rate, depth gate, drain -- is shared with
            // `dispatch_dirty_tiles_via_scheduler` via `emit_via_scheduler`.
            // See its doc comment.
            self.emit_via_scheduler(inj.seq, inj.timestamp_us, max_frag);
        }
        // Mirror the frame path's post-dispatch sweep of stuck entries.
        self.sweep_stuck_entries();
        enqueued_count
    }
}
