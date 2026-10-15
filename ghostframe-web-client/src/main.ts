import { FullFrameDecoder } from './decoder.js';
import { WebGpuRenderer } from './webgpu/renderer.js';
import { WebGpuUnavailableError } from './webgpu/init.js';
import { attachInputCapture } from './input/wire';
import { checkFrameDimensions } from './frame_limits.js';
import { createStatsLogger } from './stats_log.js';
import { installCdf53Debug } from './cdf53_debug.js';
import { initDiagnostics } from './diagnostics.js';
import { bootstrap } from './bootstrap.js';
import { initProtocol, protocol } from './protocol_constants.js';
import { showDisconnectedOverlay } from './disconnect_overlay.js';
import {
  recordProtocolEvent,
  codecDiscriminant,
  type Cdf53ErrorCodes,
} from './cdf53_globals.js';
import * as wasmExports from '../pkg-web/ghostframe_client_wasm.js';
import init, {
  WasmClientCore,
  WasmDecodeErrorBatcher,
  tileNackEnvelope,
  errorCodes,
} from '../pkg-web/ghostframe_client_wasm.js';

/** Microsecond clock for every `now_us` parameter WasmClientCore expects. */
const nowUs = (): bigint => BigInt(Math.round(performance.now() * 1000));

const statusEl = document.getElementById('status')!;
const logEl = document.getElementById('log')!;
const canvasEl = document.getElementById('canvas') as HTMLCanvasElement;

function log(msg: string) {
  const line = document.createElement('div');
  line.textContent = msg;
  logEl.appendChild(line);
  while (logEl.childElementCount > 50) {
    logEl.removeChild(logEl.firstChild!);
  }
}

async function main() {
  // wasm-bindgen `--target web`: the module must be initialized before any
  // WasmClientCore construction. Awaited here rather than at module scope —
  // a top-level await fails the vite build for the configured target.
  await init();
  // Read every wire constant from Rust once; TS must not retype them.
  initProtocol(wasmExports);

  const url = new URL(window.location.href);

  // ?e2e=lossless: strip page chrome so a CDP Page.captureScreenshot
  // captures only the canvas, top-left aligned. Used by the lossless-
  // golden e2e to drop the canvas-top-finder heuristic. Status and log
  // divs are still wired up internally; we only hide them visually.
  if (url.searchParams.getAll('e2e').includes('lossless')) {
    statusEl.style.display = 'none';
    logEl.style.display = 'none';
    canvasEl.style.position = 'fixed';
    canvasEl.style.top = '0';
    canvasEl.style.left = '0';
  }

  log(`Connecting to ${window.location.origin}...`);

  // WebGPU init — fatal if unavailable per design D2.
  let renderer: WebGpuRenderer;
  try {
    renderer = await WebGpuRenderer.create(canvasEl);
  } catch (e) {
    if (e instanceof WebGpuUnavailableError) {
      statusEl.textContent = 'WebGPU not available in this browser.';
      log(String(e));
      return;
    }
    throw e;
  }
  renderer.resize(0, 0);

  // GPU debug hooks and the `cdf53watch` tile watcher: opt-in only.
  if (url.searchParams.has('debug')) installCdf53Debug(renderer, url, log);

  // Wire all test/e2e diagnostic globals onto `window` via diagnostics.ts.
  // The getRenderer callback is called lazily (per __readPixel invocation)
  // so it always reflects the current framebuffer texture set by
  // renderer.encodeAndPresentFrame on the previous rAF tick.
  const diag = initDiagnostics({
    canvas: canvasEl,
    getRenderer: () => window.__ghostframeRenderer ?? null,
    // Pixel readbacks are for the e2e suite only (`?e2e` on the page URL).
    readbacks: url.searchParams.has('e2e'),
  });
  const stats = diag.stats;

  const { transport } = await bootstrap();

  function onSessionReset() {
    // The periodic feedback writer this used to clear is gone: the core
    // emits ReceiverFeedback from on_timeout, driven by tick()'s rAF loop,
    // which stops on its own when the session tears down. No timer to clear.

    // Drain videoFramesToClose and clear the h264Queue FIRST so that the
    // fullFrameDecoder.close() call below doesn't hit a double-close hazard
    // on its latestFrame (renderer may have moved the same VideoFrame into
    // videoFramesToClose on the previous rAF).
    renderer.onSessionReset();

    // Close the full-frame decoder if one was created this session.
    if (fullFrameDecoder) {
      fullFrameDecoder.close();
      fullFrameDecoder = null;
    }
  }

  transport.closed.then(
    (info) => {
      log(`Transport closed: code=${info.closeCode} reason=${info.reason}`);
      onSessionReset();
    },
    (err) => {
      log(`Transport closed with error: ${err}`);
      onSessionReset();
    }
  );

  await transport.ready;
  log('Connected!');
  statusEl.textContent = 'Connected';

  // Open the feedback bidi stream. Used for: HELLO (one-shot at connect),
  // ReceiverFeedback (periodic), and DECODE_ERROR (rate-limited, on demand).
  // Can be null — construction catches a failure to open the bidi stream
  // and returns null rather than throwing, so every write site below must
  // treat it as optional.
  const feedbackWriter = await (async () => {
    try {
      const bidi = await transport.createBidirectionalStream();
      return bidi.writable.getWriter();
    } catch {
      console.warn('Could not open feedback stream');
      return null;
    }
  })();

  // ACK/NACK datagram writer, needed by drainTransmit immediately: the
  // core queues its HELLO output at construction, and that queue is
  // drained below before anything else touches the feedback stream.
  const ackWriter = transport.datagrams.writable.getWriter();
  let ackWriteLogged = false;

  // Reliable-tile-emitter NACK counter, fed by drainTransmit below when it
  // recognises a NACK envelope among the Datagram outputs. Feeds the
  // periodic fec-coverage log line.
  let nackSent = 0;

  // Construct the wasm protocol core. We hard-require WebGPU, so
  // indices_raw_enabled is unconditionally true. supports_h264 reflects the
  // result of the renderer's startup probe (probeH264 in
  // webgpu/renderer.ts): true on Chrome/Chromium where texture_external +
  // WebCodecs are available, false on Firefox where Naga currently rejects
  // the h264_blit shader for lack of TEXTURE_EXTERNAL capability. The server
  // uses this to gate FrameMode::H264 selection so Firefox never receives
  // unplayable H.264 frames.
  //
  // tile_delivery_payload is unconditionally true: it gives the browser
  // validated-but-undecoded payloads for the GPU (TileDelivery::Payload).
  // Passing false would silently route tiles down the native Decoded path
  // and hand back RGBA instead — wrong, and it would look like it worked.
  const core = new WasmClientCore(true, renderer.h264Supported, true, nowUs());

  // Per-tile Cdf53 coverage, for e2e assertions.
  //
  // The interesting fields are `gave_up` (tiles that exhausted
  // MAX_TAIL_SWEEP_ATTEMPTS and stopped asking, so they are stranded at a
  // partial pass set for the rest of the session) and `partial` (tiles still
  // owed passes). A production-shaped scene that ends with either non-zero
  // has reproduced the stale-tile symptom; `incomplete` then names the
  // coordinates and the missing-pass mask.
  //
  // Exposed here rather than scraped from the log line so a test asserts on
  // structured state instead of parsing prose.
  window.__cdf53CoverageSummary = () => {
    const summary = core.cdf53Coverage();
    return {
      summary,
      incomplete: core.cdf53IncompleteTiles(16),
    };
  };

  // The three CDF53 `DecodeErrorCode` discriminants, read from the wasm
  // export rather than hardcoded 8/9/10 — a drifting discriminant should
  // break `recordProtocolEvent`'s classification loudly, not silently stop
  // matching. See docs/specs/wasm-cutover-main-ts-map.md Part 3.
  const cdf53ErrorCodes = errorCodes() as unknown as Cdf53ErrorCodes;

  type WasmPollOutput = { kind: 'Datagram' | 'Stream'; bytes: Uint8Array };

  /** Drain every pending outbound buffer, routing it to the right wire. */
  async function drainTransmit(): Promise<void> {
    for (;;) {
      const out = core.pollTransmit(nowUs()) as WasmPollOutput | undefined;
      if (out === undefined) break;
      if (out.kind === 'Datagram') {
        // NACK envelopes arrive here too (same writer as ACK batches); a
        // leading TILE_NACK_ENVELOPE discriminator identifies one so the
        // fec-coverage nackSent counter keeps moving.
        if (out.bytes.length >= 2 && out.bytes[0] === tileNackEnvelope()) {
          nackSent += out.bytes[1];
        }
        ackWriter.write(out.bytes).catch((err) => {
          if (!ackWriteLogged) {
            console.warn('ACK/NACK datagram write failed:', err);
            ackWriteLogged = true;
          }
        });
      } else {
        await feedbackWriter?.write(out.bytes);
      }
    }
  }

  // Flush the HELLO the core queued at construction. Must happen before
  // attachInputCapture is wired below, so no input event can beat it onto
  // the feedback stream. Caught locally (rather than left to main()'s
  // top-level catch) so a transient write failure logs a warning instead
  // of aborting the rest of session setup — matching the old manual-HELLO
  // behavior it replaces.
  try {
    await drainTransmit();
  } catch (e) {
    console.warn('HELLO write failed:', e);
  }

  // Browser → server input forwarding. Hooks pointer / wheel / keyboard
  // on the canvas and routes events through the same feedback writer the
  // HELLO above just used. See docs/superpowers/specs/2026-06-13-
  // input-forwarding-design.md.
  if (feedbackWriter) {
    attachInputCapture(canvasEl, feedbackWriter, () => ({
      width: renderer.framebuffer.width,
      height: renderer.framebuffer.height,
    }));
  }

  // Decode-error writer can throw once when the feedback stream closes
  // mid-session. The catch logs the first error per writer so a broken
  // feedback path is discoverable; subsequent writes are silent to avoid
  // spamming the console during normal teardown.
  let decodeErrorWriteLogged = false;
  // Rate limiting (per-tile 1/s, 32/s global) is the wasm core's
  // `DecodeErrorBatcher` (same code as the core's own pre-validation, but a
  // separate instance, so the global cap is per instance).
  const decodeErrorBatcher = new WasmDecodeErrorBatcher();
  const reportDecodeError = (codec: number, tileX: number, tileY: number, code: number) => {
    const bytes = decodeErrorBatcher.report(codec, tileX, tileY, code, nowUs());
    if (bytes && feedbackWriter) {
      feedbackWriter.write(bytes).catch((err) => {
        if (!decodeErrorWriteLogged) {
          console.warn('decode-error feedback write failed:', err);
          decodeErrorWriteLogged = true;
        }
      });
    }
  };

  // Receiver feedback is emitted by the core, not from here. ClientCore::
  // on_timeout queues a ReceiverFeedback Stream output on its own interval
  // (FEEDBACK_INTERVAL_US), and tick()'s core.onTimeout + drainTransmit
  // carries it to the wire.
  //
  // The TS setInterval that used to live here has been removed. It survived
  // 4a and kept firing every 100ms, but nothing fed its LossTracker any more
  // — onDatagram/onStaleTile/onFecRecovery all went with the receive-loop
  // rewrite — so it was sending the server an all-zeros loss report
  // alongside the core's real one. Feedback drives server-side rate
  // adaptation rather than pixels, so the e2e suite could not see it.

  // Full-frame decoder state.
  let fullFrameDecoder: FullFrameDecoder | null = null;
  // frame_seq of each chunk handed to the decoder, oldest first.
  const h264PendingSeqs: number[] = [];
  let h264LastSeq = 0;

  let firstTileRendered = false;
  let frameDimensionsKnown = false;

  // Serde mirror of `ghostframe-client-wasm/src/boundary.rs`'s `WasmEvent`,
  // confirmed against the generated source and recorded in
  // docs/specs/wasm-cutover-main-ts-map.md Part 2. `handleDatagram` and
  // `onTimeout` return `any` in the .d.ts (serde_wasm_bindgen erases the
  // type at the boundary); this is the real shape crossing it.
  // `TileData`'s mirror (boundary.rs's `WasmTileData`), tagged on `codec`.
  // `PalRle` and `Cdf53` arrive prevalidated — the core already computed the
  // GPU-ready product, so there is no second prevalidation call here.
  type WasmTileData =
    | { codec: 'Raw'; bytes: Uint8Array }
    | { codec: 'Solid'; bytes: Uint8Array }
    | {
        codec: 'PalRle';
        palette_id: number;
        count: number;
        indices: Uint8Array;
        /** Payload flags byte as encoded by the server: bit 0 bundled, bit 1 indices-raw. */
        wire_flags: number;
      }
    | { codec: 'Cdf53'; pass_idx: number; bit_planes: Uint8Array; present_passes?: number | null };

  type WasmEvent =
    | { kind: 'TileReady'; frame_seq: number; tile_x: number; tile_y: number; rgba: Uint8Array }
    | {
        kind: 'TilePayload';
        frame_seq: number;
        tile_x: number;
        tile_y: number;
        generation: number;
        data: WasmTileData;
      }
    | { kind: 'PaletteUpdated'; palette_id: number; colors: [number, number, number, number][] }
    | { kind: 'FrameDimensions'; width: number; height: number }
    | {
        kind: 'NeedsH264';
        frame_seq: number;
        timestamp_us: number;
        is_keyframe: boolean;
        payload: Uint8Array;
      }
    | { kind: 'DecodeError'; codec: number; tile_x: number; tile_y: number; code: number }
    | { kind: 'Evicted'; reason: number };

  /**
   * Renders one event out of `core.handleDatagram`/`core.onTimeout`.
   *
   * `TileReady` should never occur: this client always constructs the core
   * with `tile_delivery_payload = true`, and
   * `ghostframe-client-core/tests/tile_delivery.rs` asserts "Payload mode
   * must not emit TileReady". No rendering path is built for it — a build
   * for real would silently mask a wrong core configuration.
   *
   * The five protocol-derived `window.__*` test globals
   * (docs/specs/wasm-cutover-main-ts-map.md Part 3) are re-derived from
   * every event here via `recordProtocolEvent`, in `cdf53_globals.ts` — the
   * same function a Node driver calls in
   * `tests/cdf53_globals.test.ts` against `pkg-node`, so the logic that
   * updates them is never duplicated.
   */
  function handleEvent(ev: WasmEvent): void {
    recordProtocolEvent(window as any, ev as any, cdf53ErrorCodes, performance.now());
    switch (ev.kind) {
      case 'TileReady': {
        console.error(
          'Unexpected TileReady event: core was constructed with ' +
          'tile_delivery_payload=true, so Payload-mode reassembly should ' +
          'never emit this. No rendering path exists for it.',
          ev,
        );
        break;
      }

      case 'TilePayload': {
        const d = ev.data;
        let sampleBytes: Uint8Array;
        switch (d.codec) {
          case 'Raw':
            renderer.pushRaw({ tileX: ev.tile_x, tileY: ev.tile_y, frameSeq: ev.frame_seq, bgra: d.bytes });
            sampleBytes = d.bytes;
            break;
          case 'Solid':
            renderer.pushSolid({ tileX: ev.tile_x, tileY: ev.tile_y, frameSeq: ev.frame_seq, bgra: d.bytes });
            sampleBytes = d.bytes;
            break;
          case 'PalRle':
            renderer.pushPalRle({
              tileX: ev.tile_x,
              tileY: ev.tile_y,
              frameSeq: ev.frame_seq,
              paletteId: d.palette_id,
              count: d.count,
              indices: d.indices,
            });
            sampleBytes = d.indices;
            break;
          case 'Cdf53':
            // Prevalidated by the core (`reassembly.rs`'s `prevalidate_cdf53`
            // call) — no second prevalidation here. See the design doc's
            // "Why the drain-time ordering constraint dissolves".
            renderer.pushCdf53({
              tileX: ev.tile_x,
              tileY: ev.tile_y,
              frameSeq: ev.frame_seq,
              gen: ev.generation,
              passIdx: d.pass_idx,
              bitPlanes: d.bit_planes,
              presentPasses: d.present_passes,
            });
            sampleBytes = d.bit_planes;
            break;
        }

        const tileNowMs = performance.now();
        const firstRecvMsClient = noteFrameTileReceived(ev.frame_seq, tileNowMs);

        // Re-derive the `__ghostframeRecorded*` test globals.
        //
        // `diag.recordTile` lost its only caller when finishAssembly was
        // deleted at the wasm cutover (14aa7d4, 2026-09-11), so
        // `__ghostframeRecordedCodecs` / `Tiles` / `Flags` have been empty
        // ever since. Three e2e tests read them and have been failing on
        // `saw codecs: []` since that day -- the same orphaning that left
        // `__tileCounts` and `__cdf53Coverage` reporting zeros.
        //
        // Fed from the event rather than reimplemented here: recordTile
        // owns the FIFO cap and populates all three globals consistently.
        diag.recordTile({
          seq: ev.frame_seq,
          tileX: ev.tile_x,
          tileY: ev.tile_y,
          codec: codecDiscriminant(d.codec),
          payloadLen: sampleBytes.byteLength,
          fbWidth: renderer.framebuffer.width,
          fbHeight: renderer.framebuffer.height,
          // The wire flags byte, not the palette id. `e2e_indices_raw_handshake`
          // tests bit 1 of this value (indices-raw); it was being handed
          // `d.palette_id`, so the assertion passed or failed on whether a
          // palette id happened to have bit 1 set. `wire_flags` is forwarded
          // verbatim from the payload by the core, so the test now checks the
          // byte the server actually encoded.
          palRleFlag: d.codec === 'PalRle' ? d.wire_flags : undefined,
          firstRecvMsClient,
          lastPaintMsClient: tileNowMs,
        });

        if (!firstTileRendered) {
          firstTileRendered = true;
          const sample = Array.from(sampleBytes.slice(0, 16))
            .map(b => b.toString(16).padStart(2, '0'))
            .join(' ');
          log(`First tile: (${ev.tile_x},${ev.tile_y}) ${sampleBytes.byteLength}B`);
          log(`First bytes: ${sample}`);
          statusEl.textContent = 'Receiving frames';
        }
        break;
      }

      case 'PaletteUpdated': {
        // Not optional: palrle_decode.wgsl decodes against this table, and
        // the shadow driving prevalidation now lives in wasm. Skipping this
        // wiring wouldn't error — it would render wrong colours, the
        // hardest failure mode to trace back to a missing event handler.
        //
        // Same upload path `webgpu/renderer.ts`'s own drain-time
        // `prevalidatePalRle` call already uses for a Bundled entry
        // (renderer.ts: `this.palrlePipeline.upsertPalette(...)`) — so this
        // reaches the exact atlas buffer + `__h2_clientPaletteWrites` log
        // the pre-cutover TS path wrote to, not a parallel copy.
        //
        // `colors` is `Vec<[u8;4]>` in BGRA order (event.rs: "Colours are
        // BGRA, matching the wire and the palette table") — flatten to the
        // packed Uint8Array `upsertPalette` expects.
        const bgra = new Uint8Array(ev.colors.length * 4);
        ev.colors.forEach((c, i) => bgra.set(c, i * 4));
        renderer.palrlePipeline.upsertPalette(ev.palette_id, bgra);
        break;
      }

      case 'FrameDimensions': {
        const verdict = checkFrameDimensions(ev.width, ev.height);
        if (!verdict.ok) {
          // Keep the current framebuffer rather than allocate for a size the
          // server has no business asking for.
          console.error(`FrameDimensions rejected: ${verdict.reason}`);
          log(`FrameDimensions rejected: ${verdict.reason}`);
          break;
        }
        const oldW = renderer.framebuffer.width;
        const oldH = renderer.framebuffer.height;
        renderer.resize(ev.width, ev.height);
        // `seq` is diagnostic-only (fifo-logged to
        // window.__ghostframeRecordedResizes, never asserted on — see the
        // map doc). Unlike the old sentinel-tile path, this event carries
        // no frame_seq to attribute the resize to; 0 is a placeholder.
        diag.recordResize({ seq: 0, oldW, oldH, newW: ev.width, newH: ev.height, trigger: 'sentinel' });
        frameDimensionsKnown = true;
        break;
      }

      case 'NeedsH264': {
        if (!fullFrameDecoder) {
          fullFrameDecoder = new FullFrameDecoder((frame: VideoFrame) => {
            // The decoder is in-order (baseline, no B-frames), so output N is
            // input N. When it outputs more frames than we submitted a seq
            // for (it should not), fall back to the newest known seq.
            const seq = h264PendingSeqs.shift() ?? h264LastSeq;
            renderer.pushH264(frame, seq);
          }, 1920, 1080);
        }
        h264PendingSeqs.push(ev.frame_seq);
        h264LastSeq = ev.frame_seq;
        // A decode error drops an output; do not let the FIFO grow without bound.
        if (h264PendingSeqs.length > 256) h264PendingSeqs.shift();
        fullFrameDecoder.decode(ev.payload, ev.is_keyframe);

        if (!firstTileRendered) {
          firstTileRendered = true;
          log(`First full frame: ${ev.payload.byteLength}B ${ev.is_keyframe ? '(keyframe)' : ''}`);
          statusEl.textContent = 'Receiving frames';
        }
        break;
      }

      case 'DecodeError': {
        // Counters land in a later task (see the doc comment above
        // handleEvent). Reachable now so the switch is exhaustive and the
        // event isn't silently swallowed.
        break;
      }

      case 'Evicted': {
        // Terminal: the server has already dropped this session. Do not
        // reconnect -- two clients with retry logic evict each other
        // indefinitely (design doc §4).
        showDisconnectedOverlay(
          ev.reason === protocol().evictionReasonDisplaced
            ? 'Another session took over this desktop.'
            : 'The server ended this session.',
        );
        break;
      }
    }
  }

  // M3.5 bench: per-frame_seq earliest tile-receive timestamp. Keyed by
  // frame_seq; cleared once the frame's paint has been recorded.
  const firstRecvMs = new Map<number, number>();

  // A frame is "done" once the server has moved two sequences past it (the
  // core's stale-assembly threshold): every tile that will arrive has. It
  // then enters pendingFramePaintRaf and the next rAF tick records its
  // paint time into window.__ghostframe_framePaints, which the bench
  // harness (scene.rs read_client_diagnostics) turns into per-frame latency.
  // Fed from TilePayload events -- the per-datagram path that used to feed
  // it was deleted at the wasm cutover and this stayed empty until now.
  // Not a paint-latency measurement: "painted" is stamped on the first rAF
  // after the server is two sequences ahead, so the sample is at least two
  // frame intervals long (unbounded on a sparse scene). It proves the global
  // is live and gives a coarse upper bound; do not read it as photon latency.
  const MAX_PENDING_FRAME_STAMPS = 1024;
  let latestTileFrameSeq = 0;
  const pendingFramePaintRaf = new Set<number>();
  function noteFrameTileReceived(frameSeq: number, nowMs: number): number {
    // A tile for a frame already two sequences behind was (or will never be)
    // recorded; re-creating its entry would emit a duplicate paint record.
    if (frameSeq + 2 <= latestTileFrameSeq) return nowMs;
    if (!firstRecvMs.has(frameSeq)) {
      firstRecvMs.set(frameSeq, nowMs);
      // rAF (the only drain) stops in a hidden tab while datagrams keep
      // arriving; bound the map so a long background does not grow it, and
      // the scan below, without limit. Map iterates in insertion order.
      if (firstRecvMs.size > MAX_PENDING_FRAME_STAMPS) {
        const oldest = firstRecvMs.keys().next().value as number;
        firstRecvMs.delete(oldest);
        pendingFramePaintRaf.delete(oldest);
      }
    }
    if (frameSeq > latestTileFrameSeq) {
      latestTileFrameSeq = frameSeq;
      for (const seq of firstRecvMs.keys()) {
        if (seq + 2 <= latestTileFrameSeq) pendingFramePaintRaf.add(seq);
      }
    }
    return firstRecvMs.get(frameSeq)!;
  }

  // rAF loop — drains queues, flushes one frame per animation tick.
  let __rafTicks = 0;
  const statsLog = createStatsLogger({ renderer, core, log, getNackSent: () => nackSent });
  function tick() {
    __rafTicks++;
    diag.recordRafTick(__rafTicks);

    // Assembly-timeout NACKs, the tail-fallback sweep, and the periodic
    // feedback/ACK/NACK flush deadlines are now internal to
    // `ClientCore::on_timeout` (ghostframe-client-core/src/lib.rs) — a
    // byte-for-byte port of the scan/sweep this replaced, per the map doc.
    // `drainTransmit` is async; `tick()` is not (it's the rAF callback), so
    // this is a floating promise rather than an awaited call — deliberate,
    // logged rather than left an unhandled rejection.
    // A throw from either must not stop the rAF loop (the draw below still
    // has to run and the next tick has to be scheduled).
    try {
      flushDatagrams();
      for (const ev of core.onTimeout(nowUs())) handleEvent(ev);
    } catch (e) {
      console.error('tick(): intake failed; continuing', e);
    }
    drainTransmit().catch((e) => console.warn('tick(): drainTransmit failed', e));

    // M3.5 bench: emit recordFramePainted for all frames whose last tile was
    // received before this rAF tick. Uses performance.now() at rAF entry so
    // all frames pending in this tick share the same rafMsClient timestamp.
    if (pendingFramePaintRaf.size > 0) {
      const rafMs = performance.now();
      for (const seq of pendingFramePaintRaf) {
        diag.recordFramePainted({ seq, rafMsClient: rafMs });
        // Cleanup per-frame bench state.
        firstRecvMs.delete(seq);
      }
      pendingFramePaintRaf.clear();
    }

    // Snapshot queue depths BEFORE drain so we can log how much each rAF
    // actually consumed.
    const beforeDrain = {
      raw: renderer.rawQueue.length,
      solid: renderer.solidQueue.length,
      palrle: renderer.palRleQueue.length,
      cdf53: renderer.cdf53Queue.length,
      h264: renderer.h264Queue.length,
    };

    // A throw here must not stop the rAF loop: it would freeze the client
    // for the rest of the session on one bad frame.
    try {
      renderer.encodeAndPresentFrame((codec, tx, ty, code) => {
        reportDecodeError(codec, tx, ty, code);
      });
    } catch (e) {
      console.error('encodeAndPresentFrame failed; continuing', e);
    }

    // Drain accounting + the ~2 s stats line live in stats_log.ts.
    statsLog.onTick(beforeDrain, __rafTicks);

    requestAnimationFrame(tick);
  }
  requestAnimationFrame(tick);

  // ---- Inbound datagram batching (PC-3) -------------------------------
  //
  // Each datagram used to cost a wasm crossing with a BigInt timestamp, a
  // serde round trip for its events, and an awaited drainTransmit() that
  // serialised intake behind the ACK/NACK writes. Now the read loop only
  // queues (with the arrival instant, which ACK timing and suspension
  // detection need per datagram), and the queue is handed to the core in one
  // `handleDatagrams` call from the rAF tick, followed by one drainTransmit.
  //
  // rAF is paused in a hidden tab, so two fallbacks keep intake moving:
  // a backlog threshold and a timer. Neither fires in normal foreground use,
  // where the tick runs every ~16 ms.
  const FLUSH_BACKLOG = 32;
  const FLUSH_FALLBACK_MS = 50;
  const pendingDatagrams: Uint8Array[] = [];
  const pendingArrivalsUs: number[] = [];
  let flushTimer: ReturnType<typeof setTimeout> | undefined;

  function flushDatagrams(): void {
    if (flushTimer !== undefined) {
      clearTimeout(flushTimer);
      flushTimer = undefined;
    }
    const n = pendingDatagrams.length;
    if (n === 0) return;
    let total = 0;
    for (const d of pendingDatagrams) total += d.byteLength;
    const concat = new Uint8Array(total);
    const lens = new Uint32Array(n);
    const arrivals = Float64Array.from(pendingArrivalsUs);
    let off = 0;
    for (let i = 0; i < n; i++) {
      const d = pendingDatagrams[i];
      concat.set(d, off);
      lens[i] = d.byteLength;
      off += d.byteLength;
    }
    pendingDatagrams.length = 0;
    pendingArrivalsUs.length = 0;

    for (const ev of core.handleDatagrams(concat, lens, arrivals) as WasmEvent[]) handleEvent(ev);

    // Re-publish the datagram counters the e2e gates read off
    // `window.__ghostframeStats`.
    //
    // `stats` was assigned from `diag.stats` at startup and then never
    // written: the loop that used to increment it was the TypeScript decode
    // path deleted at the wasm cutover (14aa7d4). Five e2e tests read these
    // two numbers and saw 0 on sessions moving thousands of datagrams --
    // `frame_total=0 (expected >= 250)` and `none observed` -- because the
    // readers fall back to `{tileDatagrams:0, frameDatagrams:0}` when the
    // global is missing or stale, which looks exactly like a measurement.
    //
    // Assigned from the core rather than incremented here on purpose: the
    // core classifies tile vs frame in `handle_datagram`'s own dispatch, and
    // a second copy of that discriminator in TypeScript is how the two drift.
    const counts = core.datagramCounts() as {
      tileDatagrams: number;
      frameDatagrams: number;
    };
    stats.tileDatagrams = counts.tileDatagrams;
    stats.frameDatagrams = counts.frameDatagrams;

    drainTransmit().catch((e) => console.warn('flushDatagrams(): drainTransmit failed', e));
  }

  // Receive datagrams. Parity envelopes, ping/pong text, frame-level H.264
  // reassembly and tile-level reassembly all used to be hand-routed here;
  // the wasm core now owns parity recovery and reassembly for both, so the
  // loop reduces to a length guard, the ping/pong backward-compat carve-out
  // (which predates the tile protocol and the core has no concept of it —
  // it must run before handleDatagram ever sees the bytes), and rendering
  // whatever events come back.
  const reader = transport.datagrams.readable.getReader();
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;

    if (!value || value.byteLength === 0) continue;

    // Backward compat: small text datagrams (ping/pong). NOT protocol —
    // this predates the tile protocol and the core has no concept of it,
    // so it must be handled before handleDatagram ever sees the bytes.
    if (value.byteLength < 20) {
      const text = new TextDecoder().decode(value);
      log(`Received: ${text} (${value.byteLength} bytes)`);
      if (text === 'pong') {
        statusEl.textContent = 'Ping/Pong successful!';
        log('M0 COMPLETE: Ping/Pong datagram round-trip verified!');
      }
      continue;
    }

    // Queue, don't process: the batch crosses into wasm once per rAF tick
    // (or sooner under backlog) instead of once per datagram. See
    // flushDatagrams.
    pendingDatagrams.push(value);
    pendingArrivalsUs.push(performance.now() * 1000);
    // Hidden tab: rAF is paused and timers are clamped to >= 1 s, so process
    // immediately (the pre-batching behaviour) to keep ACKs prompt.
    if (pendingDatagrams.length >= FLUSH_BACKLOG || document.hidden) {
      flushDatagrams();
    } else if (flushTimer === undefined) {
      flushTimer = setTimeout(flushDatagrams, FLUSH_FALLBACK_MS);
    }
  }
}

main().catch((e) => {
  log(`Error: ${e.message}`);
  statusEl.textContent = `Error: ${e.message}`;
});

