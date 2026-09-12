import {
  DATAGRAM_HEADER_SIZE, TILE_HEADER_SIZE, TILE_SIZE, Codec,
  decodeDatagramHeader, decodeTileHeader, tileKey, TileAssembly,
  FRAME_HEADER_SIZE, TILE_DATAGRAM_FLAG, FrameAssembly,
  isTileDatagram, decodeFrameHeader, frameKey, FullFrameDecoder,
  FRAME_DIMENSIONS_SENTINEL_X, FRAME_DIMENSIONS_SENTINEL_Y,
} from './decoder.js';
import { WebGpuRenderer } from './webgpu/renderer.js';
import { WebGpuUnavailableError } from './webgpu/init.js';
import { ParityRecovery } from './fec';
import { ParityDecoder, parseParityEnvelope, TILE_PARITY_ENVELOPE } from './parity_decoder.js';
import { LossTracker } from './feedback';
import { attachInputCapture } from './input/wire';
import { DecodeErrorBatcher } from './decode_error_batcher';
import { AckBatcher } from './ack';
import { NackBatcher } from './nack.js';
import { initDiagnostics } from './diagnostics.js';
import { prevalidateCdf53 } from './prevalidate_cdf53.js';
import { applyCdf53Arrival, type Cdf53CoverageEntry } from './cdf53_coverage.js';
import { bootstrap } from './bootstrap.js';
import { recordProtocolEvent, type Cdf53ErrorCodes } from './cdf53_globals.js';
import init, {
  WasmClientCore,
  tileNackEnvelope,
  errorCodes,
  // Aliased: the TS `prevalidateCdf53` import above (from prevalidate_cdf53.js)
  // stays in scope because the now-orphaned `finishAssembly` still calls it.
  // This is the standalone wasm export the live TilePayload/Cdf53 dispatch
  // path uses instead — see the map doc's "CDF53 is prevalidated twice"
  // section for why a second prevalidation call is required here at all.
  prevalidateCdf53 as prevalidateCdf53Wasm,
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

  const url = new URL(window.location.href);

  // ?e2e=lossless: strip page chrome so a CDP Page.captureScreenshot
  // captures only the canvas, top-left aligned. Used by the lossless-
  // golden e2e to drop the canvas-top-finder heuristic. Status and log
  // divs are still wired up internally; we only hide them visually.
  if (url.searchParams.get('e2e') === 'lossless') {
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

  // M3.3b diagnostic: a URL query param `cdf53watch=X,Y` activates the tile
  // watcher BEFORE the datagram loop starts, so the initial cdf53 emission
  // burst (most of the flow for a static gradient frame) is captured.
  // Without this, set-watcher-via-hook misses the burst that happens between
  // page load and the test's first evaluate.
  const watchParam = url.searchParams.get('cdf53watch');
  if (watchParam) {
    const [wx, wy] = watchParam.split(',').map(s => Number(s.trim()));
    if (Number.isFinite(wx) && Number.isFinite(wy)) {
      renderer.cdf53Pipeline.setTileWatcher(wx, wy);
      log(`Cdf53 tile watcher armed for (${wx},${wy})`);
    }
  }

  // M3.3b diagnostic: test-only hook to drive Cdf53 inverse with hand-supplied
  // coefficients (bypasses the integrate shader). Removed once the wavelet
  // math is verified.
  (window as any).__cdf53TestInverse = async (
    coefficientsI16: number[],  // length 3072 (3 channels × 1024 i16 each)
    targetTileIdx: number = 0,  // optional, default tile 0 for backward compat
  ): Promise<number[]> => {
    const pipe = renderer.cdf53Pipeline;
    const device = renderer.device;

    // Split signed i16 coefficients into magnitudes + signs.
    const coefU32 = new Uint32Array(1536);
    const signU32 = new Uint32Array(96);
    for (let ch = 0; ch < 3; ch++) {
      for (let i = 0; i < 1024; i++) {
        const c = coefficientsI16[ch * 1024 + i];
        const mag = (c < 0 ? -c : c) & 0xFFFF;
        const wordIdx = ch * 512 + (i >> 1);
        if ((i & 1) === 0) {
          coefU32[wordIdx] = (coefU32[wordIdx] & 0xFFFF0000) | mag;
        } else {
          coefU32[wordIdx] = (coefU32[wordIdx] & 0x0000FFFF) | (mag << 16);
        }
        if (c < 0) {
          const signWordIdx = ch * 32 + (i >> 5);
          signU32[signWordIdx] |= 1 << (i & 31);
        }
      }
    }
    // Write into target tile region of each buffer.
    device.queue.writeBuffer(pipe.coefficientBuffer, targetTileIdx * 6144, coefU32);
    device.queue.writeBuffer(pipe.signBuffer, targetTileIdx * 384, signU32);
    device.queue.writeBuffer(pipe.tileGenBuffer, targetTileIdx * 4, new Uint32Array([1]));

    // Run inverse passes.
    const encoder = device.createCommandEncoder();
    pipe.encodeInverse(encoder);
    device.queue.submit([encoder.finish()]);

    // Read back the target tile's pixels via __readPixelRect.
    const cols = Math.ceil(renderer.framebuffer.width / 32);
    const tileX = targetTileIdx % cols;
    const tileY = Math.floor(targetTileIdx / cols);
    return await (window as any).__readPixelRect(tileX * 32, tileY * 32, 32, 32);
  };

  // M3.3b diagnostic: test-only hook to drive the Cdf53 integrate shader
  // directly with hand-supplied RLE-encoded passes for tile 0, then read back
  // the resulting coefficientBuffer + signBuffer. Used to isolate whether the
  // integrate shader (or uploadBatch packing) is correct independently of the
  // wire path.
  (window as any).__cdf53TestIntegrate = async (
    encodedPasses: number[][],  // 14 entries, each is the raw RLE-encoded payload as a number[]
  ): Promise<{ coefficients: number[]; signs: number[] }> => {
    const pipe = renderer.cdf53Pipeline;
    const device = renderer.device;

    // Reset per-tile state for tile 0.
    device.queue.writeBuffer(pipe.coefficientBuffer, 0, new Uint8Array(6144));  // 1536 u32 × 4 B
    device.queue.writeBuffer(pipe.signBuffer, 0, new Uint8Array(384));          // 96 u32 × 4 B
    device.queue.writeBuffer(pipe.tileGenBuffer, 0, new Uint32Array([0]));      // force gen-bump on first pass

    // Build the batch from the 14 encoded passes (all for tile 0, gen=1).
    const entries: Array<{ tileX: number; tileY: number; gen: number; passIdx: number; bitPlanes: Uint8Array }> = [];
    for (let passIdx = 0; passIdx < encodedPasses.length; passIdx++) {
      const payload = new Uint8Array(encodedPasses[passIdx]);
      const r = prevalidateCdf53(payload, 1, passIdx);
      if (!r.ok) throw new Error('prevalidate failed for pass ' + passIdx + ' err=' + r.errorCode);
      r.entry.tileX = 0;
      r.entry.tileY = 0;
      entries.push(r.entry);
    }

    // Upload + integrate.
    pipe.uploadBatch(entries);
    const encoder = device.createCommandEncoder();
    pipe.encodeIntegrate(encoder, entries.length);
    device.queue.submit([encoder.finish()]);

    // Read back coefficientBuffer[0..1536] and signBuffer[0..96] via staging.
    const coefStaging = device.createBuffer({
      size: 6144, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
    });
    const signStaging = device.createBuffer({
      size: 384, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
    });
    const copyEnc = device.createCommandEncoder();
    copyEnc.copyBufferToBuffer(pipe.coefficientBuffer, 0, coefStaging, 0, 6144);
    copyEnc.copyBufferToBuffer(pipe.signBuffer, 0, signStaging, 0, 384);
    device.queue.submit([copyEnc.finish()]);

    await coefStaging.mapAsync(GPUMapMode.READ);
    await signStaging.mapAsync(GPUMapMode.READ);
    const coefArr = Array.from(new Uint32Array(coefStaging.getMappedRange()));
    const signArr = Array.from(new Uint32Array(signStaging.getMappedRange()));
    coefStaging.unmap(); coefStaging.destroy();
    signStaging.unmap(); signStaging.destroy();
    return { coefficients: coefArr, signs: signArr };
  };

  // M3.3b diagnostic: dump per-tile GPU state (tileGen + coefficients + signs)
  // for a single tile index. Used by e2e_cdf53_live_tile_state to inspect the
  // live integrate path under real-world load.
  (window as any).__cdf53DumpTileState = async (
    tileIdx: number,
  ): Promise<{ tileGen: number; coefficients: number[]; signs: number[] }> => {
    const pipe = renderer.cdf53Pipeline;
    const device = renderer.device;

    const coefStaging = device.createBuffer({
      size: 6144, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
    });
    const signStaging = device.createBuffer({
      size: 384, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
    });
    const genStaging = device.createBuffer({
      size: 4, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
    });
    const enc = device.createCommandEncoder();
    enc.copyBufferToBuffer(pipe.coefficientBuffer, tileIdx * 6144, coefStaging, 0, 6144);
    enc.copyBufferToBuffer(pipe.signBuffer, tileIdx * 384, signStaging, 0, 384);
    enc.copyBufferToBuffer(pipe.tileGenBuffer, tileIdx * 4, genStaging, 0, 4);
    device.queue.submit([enc.finish()]);

    await coefStaging.mapAsync(GPUMapMode.READ);
    await signStaging.mapAsync(GPUMapMode.READ);
    await genStaging.mapAsync(GPUMapMode.READ);
    const coefArr = Array.from(new Uint32Array(coefStaging.getMappedRange()));
    const signArr = Array.from(new Uint32Array(signStaging.getMappedRange()));
    const tileGen = new Uint32Array(genStaging.getMappedRange())[0];
    coefStaging.unmap(); coefStaging.destroy();
    signStaging.unmap(); signStaging.destroy();
    genStaging.unmap(); genStaging.destroy();
    return { tileGen, coefficients: coefArr, signs: signArr };
  };

  // M3.3b diagnostic: per-tile JS-side upload watcher. Captures the bytes
  // every uploadBatch hands off to the GPU for the watched (tileX, tileY),
  // along with the (gen, passIdx) the renderer attributed to that entry.
  // Used by `e2e_cdf53_tile_watcher` to verify the JS→GPU handoff is correct
  // independent of the integrate shader's behavior.
  (window as any).__cdf53SetTileWatcher = (tileX: number, tileY: number) => {
    renderer.cdf53Pipeline.setTileWatcher(tileX, tileY);
  };

  // M3.3b queue-identity probe. Reports live state of renderer.cdf53Queue
  // and the pipeline's totals, so we can confirm whether pushes and
  // uploadBatch share the same array.
  (window as any).__cdf53Probe = () => {
    return {
      cdf53QueueLengthNow: renderer.cdf53Queue.length,
      uploadBatchCallsLifetime: renderer.cdf53Pipeline.uploadBatchCalls,
      totalEntriesLifetime: renderer.cdf53Pipeline.totalEntries,
      isQueueAnArray: Array.isArray(renderer.cdf53Queue),
      pipelineCtorName: renderer.cdf53Pipeline.constructor.name,
      // Spot-check: prove the same Cdf53Pipeline instance is bound through
      // the renderer property accessor by storing a token and reading it back.
      sameInstanceCheck: (() => {
        (renderer.cdf53Pipeline as any).__token = 'probe-token-' + Date.now();
        return (renderer.cdf53Pipeline as any).__token;
      })(),
      // Read the watcher state directly through renderer.cdf53Pipeline so we
      // can tell if the watcher was reset somewhere after we set it.
      tileWatcherXNow: (renderer.cdf53Pipeline as any).tileWatcherX,
      tileWatcherYNow: (renderer.cdf53Pipeline as any).tileWatcherY,
      uploadBatchWithWatcherNull: (renderer.cdf53Pipeline as any).uploadBatchWithWatcherNull,
      uploadBatchWithWatcherSet: (renderer.cdf53Pipeline as any).uploadBatchWithWatcherSet,
      entriesWhileWatcherNull: (renderer.cdf53Pipeline as any).entriesWhileWatcherNull,
      entriesWhileWatcherSet: (renderer.cdf53Pipeline as any).entriesWhileWatcherSet,
      seenTilesAddCalls: (renderer.cdf53Pipeline as any).seenTilesAddCalls,
    };
  };
  (window as any).__cdf53GetTileWatcher = () => {
    const pipe = renderer.cdf53Pipeline;
    return {
      captures: pipe.tileWatcherCaptures.map(c => ({
        batchSize: c.batchSize,
        entryIdx: c.entryIdx,
        tileX: c.tileX,
        tileY: c.tileY,
        gen: c.gen,
        passIdx: c.passIdx,
        bitPlanesOffset: c.bitPlanesOffset,
        bitPlanes: Array.from(c.bitPlanes),
      })),
      stats: {
        uploadBatchCalls: pipe.uploadBatchCalls,
        totalEntries: pipe.totalEntries,
        distinctTilesSeen: pipe.seenTiles.size,
        sampleTiles: Array.from(pipe.seenTiles).slice(0, 30),
      },
    };
  };

  // Wire all test/e2e diagnostic globals onto `window` via diagnostics.ts.
  // The getRenderer callback is called lazily (per __readPixel invocation)
  // so it always reflects the current framebuffer texture set by
  // renderer.encodeAndPresentFrame on the previous rAF tick.
  const diag = initDiagnostics({
    canvas: canvasEl,
    getRenderer: () => window.__ghostframeRenderer ?? null,
  });
  const stats = diag.stats;

  const { transport } = await bootstrap();

  // Captured here so onSessionReset can clear them. setInterval keeps firing
  // even after its enclosing stream closes; without explicit clearInterval
  // every reconnect would leak a dead interval handler + its closure state.
  let feedbackInterval: ReturnType<typeof setInterval> | null = null;

  function onSessionReset() {
    // Stop the periodic feedback writer so it doesn't try to write to a
    // closed stream after teardown. Cleared first because it's the only
    // active timer.
    if (feedbackInterval !== null) {
      clearInterval(feedbackInterval);
      feedbackInterval = null;
    }

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

  const parityMap = new Map<string, ParityRecovery>();
  const lossTracker = new LossTracker();

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

  // ACK/NACK datagram writer. Declared here (moved up from its historical
  // position just above ackBatcher, further down) because drainTransmit's
  // Datagram branch needs it immediately: the core queues its HELLO output
  // at construction, and that queue is drained below before anything else
  // touches the feedback stream.
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
  const decodeErrorBatcher = new DecodeErrorBatcher((bytes) => {
    if (feedbackWriter) {
      feedbackWriter.write(bytes).catch((err) => {
        if (!decodeErrorWriteLogged) {
          console.warn('decode-error feedback write failed:', err);
          decodeErrorWriteLogged = true;
        }
      });
    }
  });

  if (feedbackWriter) {
    feedbackInterval = setInterval(async () => {
      try {
        const msg = lossTracker.encodeFeedback();
        await feedbackWriter.write(msg);
      } catch {
        // Stream closed — stop reporting. onSessionReset clears the
        // interval, but a race between the close event and the next tick
        // can fire this branch once before the clear takes effect.
      }
    }, 100);
  }

  // Full-frame decoder and reassembly state.
  let fullFrameDecoder: FullFrameDecoder | null = null;
  const frameAssemblies = new Map<string, FrameAssembly>();
  let latestFullFrameSeq = 0;

  // Batched ACK sender — fire-and-forget unreliable datagrams. `ackWriter`
  // and `ackWriteLogged` are declared earlier now (needed by drainTransmit
  // before this point). The catch logs the first error per writer so a
  // broken ACK path is discoverable; subsequent writes are silent to avoid
  // spamming during normal teardown.
  const ackBatcher = new AckBatcher((dg) => {
    ackWriter.write(dg).catch((err) => {
      if (!ackWriteLogged) {
        console.warn('ACK datagram write failed:', err);
        ackWriteLogged = true;
      }
    });
  });

  // Reliable-tile-emitter FEC counters — RETIRED, not live.
  //
  // These used to be fed by the TS parity branch removed from the receive
  // loop (0x04 TILE_PARITY_ENVELOPE dispatch + ParityDecoder.recordSource).
  // Parity recovery is now internal to `WasmClientCore::handle_datagram`
  // (ghostframe-client-core/src/reassembly.rs) and does not report itself
  // as an event — a recovered source datagram is just folded back into
  // reassembly with no `Event` marking that it happened. There is no
  // `WasmEvent` variant to re-derive per-datagram parity-rx / recovered /
  // unrecoverable counts from (see docs/specs/wasm-cutover-main-ts-map.md
  // Part 2's event list — nothing there carries this).
  //
  // Left frozen at 0 these would misread as "no packet loss recovered" on
  // a session that in fact recovered plenty internally. The `fec-coverage`
  // log line below prints them as an explicit "n/a" instead, so a reader
  // can tell "not measured" from "measured zero" — the same distinction
  // the FEC-counter section of this task's brief calls out. This is a
  // real observability loss versus pre-cutover: `nack_sent` (tile-level,
  // still fed by drainTransmit below) remains the only wire-loss signal
  // visible in this log line post-cutover.
  const FEC_COUNTER_NOT_MEASURED = 'n/a(wasm-internal)';

  // ParityDecoder window: server emits one parity per K=10 source group with
  // +2K interleave offset, so the decoder needs to hold at least the in-flight
  // source datagrams covering 4 × K groups to allow late-source recovery once
  // the offset parity arrives. See parity_decoder.ts (Task 21) and the
  // server-side TileParityEnvelope emitter (Task 35).
  const parityDecoder = new ParityDecoder(40);

  // Reliable-tile-emitter NACK sender — reuses the same datagrams writer.
  // Fire-and-forget; rejection from a closed stream is benign at this point.
  // The wrapper increments `nackSent` by the entry-count in the envelope
  // header (buf[1]) before forwarding to the writer.
  const nackBatcher = new NackBatcher((buf) => {
    if (buf.length >= 2) nackSent += buf[1];
    ackWriter.write(buf).catch(() => {});
  });

  const assemblies = new Map<string, TileAssembly>();
  let latestFrameSeq = 0;
  let firstTileRendered = false;
  let frameDimensionsKnown = false;

  // Serde mirror of `ghostframe-client-wasm/src/boundary.rs`'s `WasmEvent`,
  // confirmed against the generated source and recorded in
  // docs/specs/wasm-cutover-main-ts-map.md Part 2. `handleDatagram` and
  // `onTimeout` return `any` in the .d.ts (serde_wasm_bindgen erases the
  // type at the boundary); this is the real shape crossing it.
  type WasmEvent =
    | { kind: 'TileReady'; frame_seq: number; tile_x: number; tile_y: number; rgba: Uint8Array }
    | {
        kind: 'TilePayload';
        frame_seq: number;
        tile_x: number;
        tile_y: number;
        pass_idx: number;
        generation: number;
        /** `Codec` repr(u8) discriminant: Skip=0, H264=1, PalRle=2, Solid=3, Raw=4, Cdf53=5. */
        codec: number;
        payload: Uint8Array;
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
    | { kind: 'DecodeError'; codec: number; tile_x: number; tile_y: number; code: number };

  // Flat mirror of `WasmPrevalidatedCdf53` (units.rs), the shape returned by
  // the standalone `prevalidateCdf53Wasm` free function. Field names differ
  // from the TS `PrevalidatedCdf53` (prevalidate_cdf53.ts) that
  // `renderer.pushCdf53` expects — adapted at the call site below rather
  // than touching the renderer.
  type WasmPrevalidatedCdf53Result = {
    ok: boolean;
    code: number;
    generation: number;
    pass_idx: number;
    bit_planes: Uint8Array;
  };

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
    recordProtocolEvent(window as any, ev as any, Codec.Cdf53, cdf53ErrorCodes, performance.now());
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
        switch (ev.codec) {
          case Codec.Raw:
            renderer.pushRaw({ tileX: ev.tile_x, tileY: ev.tile_y, bgra: ev.payload });
            break;
          case Codec.Solid:
            // main.ts only ever painted Solid when the payload was exactly
            // 4B — preserved from the old finishAssembly guard.
            if (ev.payload.byteLength === 4) {
              renderer.pushSolid({ tileX: ev.tile_x, tileY: ev.tile_y, bgra: ev.payload });
            }
            break;
          case Codec.PalRle:
            renderer.pushPalRle({ tileX: ev.tile_x, tileY: ev.tile_y, payload: ev.payload });
            break;
          case Codec.Cdf53: {
            // Known double decode (accepted — see the map doc's final
            // section): the core already prevalidated this payload once,
            // internally, to drive coverage/NACK/ACK bookkeeping, then
            // handed back the raw wire payload rather than the bit planes
            // it discarded. `renderer.pushCdf53` needs those bit planes, so
            // they're recomputed here via the standalone wasm export.
            const r = prevalidateCdf53Wasm(
              ev.payload,
              ev.generation,
              ev.pass_idx,
            ) as WasmPrevalidatedCdf53Result;
            if (r.ok) {
              renderer.pushCdf53({
                tileX: ev.tile_x,
                tileY: ev.tile_y,
                gen: r.generation,
                passIdx: r.pass_idx,
                bitPlanes: r.bit_planes,
              });
            } else {
              // The core already validated this exact payload successfully
              // before emitting TilePayload at all — a failure here means
              // the standalone export and the core's internal prevalidation
              // have diverged. That's a real bug, not a wire-loss event.
              console.error(
                `prevalidateCdf53Wasm disagreed with the core's own ` +
                `prevalidation for tile (${ev.tile_x},${ev.tile_y}) ` +
                `gen=${ev.generation} pass=${ev.pass_idx}: code=${r.code}`,
              );
            }
            break;
          }
          default:
            console.error(`TilePayload with unrecognised codec ${ev.codec}`, ev);
        }

        if (!firstTileRendered) {
          firstTileRendered = true;
          const sample = Array.from(ev.payload.slice(0, 16))
            .map(b => b.toString(16).padStart(2, '0'))
            .join(' ');
          log(`First tile: (${ev.tile_x},${ev.tile_y}) ${ev.payload.byteLength}B`);
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
            renderer.pushH264(frame);
          }, 1920, 1080);
        }
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
    }
  }

  // M3.5 bench: per-frame_seq earliest datagram-receive timestamp.
  // Keyed by frameSeq (uint32); cleared after the corresponding frame is painted.
  const firstRecvMs = new Map<number, number>();

  // M3.5 bench: per-frame painted-tile counters + rAF trigger set.
  // A frame enters pendingFramePaintRaf when latestFrameSeq advances past it by
  // >= 2 (matching the stale-eviction threshold), i.e. when the server has moved
  // on and we know all tiles that will arrive have arrived.
  const paintedTilesPerFrame = new Map<number, number>();
  const pendingFramePaintRaf = new Set<number>();

  /** Reassemble completed tile and route it to the renderer's per-codec queue. */
  function finishAssembly(asmKey: string, asm: TileAssembly) {
    assemblies.delete(asmKey);

    // asmKey format is `${frameSeq}:${tileX}:${tileY}` — extract frameSeq for
    // test instrumentation (TileHeader itself doesn't carry frameSeq).
    const frameSeqFromKey = parseInt(asmKey.split(':')[0], 10);

    function recordingResize(
      newW: number,
      newH: number,
      trigger: 'sentinel' | 'fallback-expand',
      seq: number,
    ) {
      const oldW = renderer.framebuffer.width;
      const oldH = renderer.framebuffer.height;
      renderer.resize(newW, newH);
      diag.recordResize({ seq, oldW, oldH, newW, newH, trigger });
    }

    const totalLen = asm.fragments.reduce((acc, f) => acc + (f ? f.byteLength : 0), 0);
    const payload = new Uint8Array(totalLen);
    let off = 0;
    for (const frag of asm.fragments) {
      if (frag) {
        payload.set(frag, off);
        off += frag.byteLength;
      }
    }

    const tX = asm.header.tileX;
    const tY = asm.header.tileY;

    // Frame-dimensions control message — sentinel tile coords (0xFF, 0xFF).
    if (tX === FRAME_DIMENSIONS_SENTINEL_X && tY === FRAME_DIMENSIONS_SENTINEL_Y) {
      if (payload.byteLength >= 8) {
        const view = new DataView(payload.buffer, payload.byteOffset, 8);
        const w = view.getUint32(0, false);
        const h = view.getUint32(4, false);
        recordingResize(w, h, 'sentinel', frameSeqFromKey);
        frameDimensionsKnown = true;
      }
      return;
    }

    // Fallback: expand canvas to fit this tile if we haven't yet received the
    // frame-dimensions sentinel. Once dimensions are known, skip this entirely.
    if (!frameDimensionsKnown) {
      const minWidth = (tX + 1) * TILE_SIZE;
      const minHeight = (tY + 1) * TILE_SIZE;
      if (canvasEl.width < minWidth || canvasEl.height < minHeight) {
        recordingResize(
          Math.max(canvasEl.width, minWidth),
          Math.max(canvasEl.height, minHeight),
          'fallback-expand',
          frameSeqFromKey,
        );
      }
    }

    // Test instrumentation: record codecs for E2E protocol-layer assertions.
    // Per-tile event log adds {tileX, tileY, codec, payloadLen, fbWidth, fbHeight}
    // so e2e_edge_tiles can correlate which tiles arrived against the framebuffer
    // dimensions at the moment they were queued (W5 diagnostic).
    //
    // M3.5 bench: capture firstRecv BEFORE routing to GPU queues and
    // lastPaint AFTER, so the interval spans tile processing time.
    const firstRecv = firstRecvMs.get(frameSeqFromKey);
    // Route tile data into the appropriate renderer queue.

    // Diagnostic counters: every tile increments by codec so the periodic
    // status log can see at a glance whether we're receiving content.
    const w = window as any;
    w.__tileCounts = w.__tileCounts ?? { raw: 0, solid: 0, palrle: 0, cdf53: 0, h264: 0, other: 0 };
    w.__lastTileSeq = frameSeqFromKey;
    // [H5-DIAG] log every tile push with frame_seq + codec. Used by the
    // e2e test to detect OOO arrivals (e.g. Solid(BLACK) for an older
    // frame_seq arriving after PalRle for a newer one — same rAF wipes
    // the PalRle output).
    if (!w.__h5_tilePushLog) w.__h5_tilePushLog = [];
    const h5Buf = w.__h5_tilePushLog as Array<{seq:number,tx:number,ty:number,codec:number,c0:string,ts:number}>;
    // Capture the first up-to-8 payload bytes for any codec. For PalRle
    // bundled this is [flags=1, palette_id, count, c0_b, c0_g, c0_r, c0_a, ...].
    // For PalRle thin: [flags=0, palette_id, rle0, rle1, ...]. For Solid:
    // [b, g, r, a]. The c0 string lets the e2e dump infer which palette
    // slot a PalRle painted from, without instrumenting the renderer.
    const h5c0 = (() => {
      const len = payload?.byteLength ?? -1;
      if (len <= 0) return `LEN=${len}`;
      const n = Math.min(len, 8);
      let s = '';
      for (let i = 0; i < n; i++) s += payload[i].toString(16).padStart(2, '0');
      return s;
    })();
    h5Buf.push({
      seq: frameSeqFromKey,
      tx: tX,
      ty: tY,
      codec: asm.header.codec,
      c0: h5c0,
      ts: performance.now(),
    });
    if (h5Buf.length > 32768) h5Buf.shift();
    if (asm.header.codec === Codec.Raw) {
      w.__tileCounts.raw++;
      renderer.pushRaw({ tileX: tX, tileY: tY, bgra: payload });
    } else if (asm.header.codec === Codec.Solid) {
      w.__tileCounts.solid++;
      if (payload.byteLength === 4) {
        renderer.pushSolid({ tileX: tX, tileY: tY, bgra: payload });
      }
    } else if (asm.header.codec === Codec.PalRle) {
      w.__tileCounts.palrle++;
      renderer.pushPalRle({ tileX: tX, tileY: tY, payload });
    } else if (asm.header.codec === Codec.Cdf53) {
      w.__tileCounts.cdf53++;
      // M3.3b diagnostic counters: track every Cdf53 dispatch branch.
      w.__cdf53DispatchSeen = (w.__cdf53DispatchSeen ?? 0) + 1;

      // Per-(tileX, tileY, generation) pass-coverage bookkeeping.
      // Records the bitmask of received pass indices (0..13) so the
      // periodic stats log can split the cdf53 tile set into
      // "fully refined" (all 14 passes seen) and "partial" (1..13
      // passes seen) buckets. Each tile owns its current generation
      // entry; on bump the prior entry is replaced (the only valid
      // refinement bits are for the most recent generation, per the
      // server-side `bump_generation` + `drop_cdf53_for_tile`
      // contract). Keyed by `tx<<8 | ty` so the Map is keyed by a
      // plain number rather than a string for cheaper lookup; the
      // value is `{ generation, passMask }` where passMask bit i ==
      // pass i received.
      const cdf53Coverage: Map<number, Cdf53CoverageEntry> =
        (w.__cdf53Coverage ??= new Map());
      const tileKey = (tX << 8) | tY;
      const passIdx = asm.header.pass;
      const nowMs = performance.now();

      // Per-tier byte attribution for BWE-side observability (Phase 1
      // Task 6). Counts every received CDF53 pass datagram payload —
      // including prevalidation-failed ones; matches the server's
      // per-tier emit counters at coarse window resolution. The exact
      // match doesn't have to be byte-perfect — we're computing
      // per-second rates for an order-of-magnitude BWE estimate, not
      // for accounting.
      if (passIdx <= 3) {
        bytesRecvCritical += payload.byteLength;
      } else {
        bytesRecvRefinement += payload.byteLength;
      }

      const r = prevalidateCdf53(payload, asm.header.generation, asm.header.pass);

      // Coverage update + NACK decision is shared between the success
      // and failure paths (Phase 1.5-A). Pure logic lives in
      // `applyCdf53Arrival`; this caller wires the resulting NACK list
      // to `queuePassNack` and the per-failure diagnostic counters.
      const update = applyCdf53Arrival(
        cdf53Coverage.get(tileKey),
        asm.header.generation,
        passIdx,
        frameSeqFromKey,
        nowMs,
        r.ok,
      );
      cdf53Coverage.set(tileKey, update.entry);
      for (const p of update.nackPasses) {
        queuePassNack(frameSeqFromKey, tX, tY, p);
      }

      if (!r.ok) {
        w.__cdf53PrevalidateFails = (w.__cdf53PrevalidateFails ?? 0) + 1;
        w.__cdf53LastFailCode = r.errorCode;
        decodeErrorBatcher.report({
          codec: Codec.Cdf53,
          tileX: tX,
          tileY: tY,
          errorCode: r.errorCode,
        });
        // Phase 1.5-C: dump the raw bytes of the first MAX_CDF53_FAIL_DUMPS
        // prevalidation failures so we can inspect the wire-format
        // corruption that causes ERR_CDF53_TRUNCATED / ERR_CDF53_RLE_LENGTH.
        // Each entry: (frameSeq, tileX, tileY, passIdx, gen, errorCode,
        // payloadLen, hex bytes). Stored on `window.__cdf53FailDumps` so
        // it's accessible from devtools without instrumenting the renderer.
        const MAX_CDF53_FAIL_DUMPS = 32;
        const dumps = (w.__cdf53FailDumps ??= [] as Array<{
          frameSeq: number;
          tileX: number;
          tileY: number;
          passIdx: number;
          gen: number;
          errorCode: number;
          payloadLen: number;
          hex: string;
        }>);
        if (dumps.length < MAX_CDF53_FAIL_DUMPS) {
          // Hex-encode up to first 256 bytes — enough to inspect the
          // 6-byte BGR length prefix triple + payload start, where
          // truncation / RLE-length errors occur. Bounded to keep the
          // window object small (a 16-entry dump caps at ~16 KB).
          const len = Math.min(payload.byteLength, 256);
          let hex = '';
          for (let i = 0; i < len; i++) {
            hex += payload[i].toString(16).padStart(2, '0');
            if (i < len - 1) hex += ' ';
          }
          dumps.push({
            frameSeq: frameSeqFromKey,
            tileX: tX,
            tileY: tY,
            passIdx: asm.header.pass,
            gen: asm.header.generation,
            errorCode: r.errorCode,
            payloadLen: payload.byteLength,
            hex,
          });
          // Log the first few to console for visibility without devtools
          // inspection. Useful in production for quick triage.
          if (dumps.length <= 4) {
            log(
              `cdf53.fail#${dumps.length} seq=${frameSeqFromKey} ` +
              `tile=(${tX},${tY}) pass=${asm.header.pass} gen=${asm.header.generation} ` +
              `err=${r.errorCode} len=${payload.byteLength} hex=${hex.slice(0, 96)}${hex.length > 96 ? '…' : ''}`
            );
          }
        }
      } else {
        // Caller-fills tileX/tileY (prevalidate left them at 0).
        r.entry.tileX = tX;
        r.entry.tileY = tY;
        renderer.pushCdf53(r.entry);
        w.__cdf53PushedToQueue = (w.__cdf53PushedToQueue ?? 0) + 1;
        // Phase 1.5-A v2: deferred CDF53 ACK. Fires only after the pass
        // bytes successfully prevalidate (deterministic — runs entirely
        // on bytes already in hand, no GPU round-trip). Server's
        // `record_cdf53_ack` then marks this pass delivered, gating
        // `tile_fully_acked` honestly. A failed-prevalidation pass
        // produces NO ACK — the server's retransmit cache retains the
        // entry for RTO retry, and Phase 1.5-B's stranded escalation
        // can observe `acked < max_passes` as the true client view.
        ackBatcher.add({
          frameSeq: frameSeqFromKey | TILE_DATAGRAM_FLAG,
          tileX: tX,
          tileY: tY,
          passIdx: asm.header.pass,
          arrivalTimeMsLo16: (performance.now() & 0xFFFF) | 0,
        });
      }
    }

    // M3.5 bench: lastPaintMsClient — captured after tile data is handed to
    // the GPU queue (the JS-side "paint" boundary; the actual GPU rasterisation
    // happens on the next rAF tick in encodeAndPresentFrame).
    const lastPaintMs = performance.now();

    diag.recordTile({
      seq: frameSeqFromKey,
      tileX: tX,
      tileY: tY,
      codec: asm.header.codec,
      payloadLen: payload.byteLength,
      fbWidth: renderer.framebuffer.width,
      fbHeight: renderer.framebuffer.height,
      palRleFlag: asm.header.codec === Codec.PalRle ? payload[0] : undefined,
      // M3.5 bench fields:
      firstRecvMsClient: firstRecv,
      lastPaintMsClient: lastPaintMs,
    });

    // M3.5 bench: track painted-tile count per frame. When latestFrameSeq
    // advances past this frame by >= 2 (the stale-eviction threshold), the
    // frame's entry is moved to pendingFramePaintRaf in the datagram loop so
    // the next rAF tick can emit recordFramePainted.
    paintedTilesPerFrame.set(
      frameSeqFromKey,
      (paintedTilesPerFrame.get(frameSeqFromKey) ?? 0) + 1,
    );

    if (!firstTileRendered) {
      firstTileRendered = true;
      const sample = Array.from(payload.slice(0, 16))
        .map(b => b.toString(16).padStart(2, '0'))
        .join(' ');
      log(`First tile: (${tX},${tY}) ${payload.byteLength}B`);
      log(`First bytes: ${sample}`);
      statusEl.textContent = 'Receiving frames';
    }

  }

  // Reliable-tile-emitter: NACK any fragment still missing this long after
  // the assembly's first fragment arrived. Dedup-set on the assembly prevents
  // re-NACKing the same frag_idx on every subsequent rAF tick.
  const ASSEMBLY_TIMEOUT_MS = 30;

  function scanForAssemblyTimeouts(now: number, partialAssemblies: Iterable<TileAssembly>) {
    for (const asm of partialAssemblies) {
      if (asm.received >= asm.fragments.length) continue;
      if (now - asm.partialSince < ASSEMBLY_TIMEOUT_MS) continue;
      for (let i = 0; i < asm.fragments.length; i++) {
        if (asm.fragments[i] === null && !asm.nackedFragIdxs.has(i)) {
          nackBatcher.add(asm.emitKey, i);
          asm.nackedFragIdxs.add(i);
        }
      }
    }
  }

  // Per-tier recv counters (passes 0-3 critical vs 4-13 refinement).
  // Updated in the per-pass receive path below; reported in the periodic
  // stats line (Phase 1 Task 10). Bytes-since-startup; the periodic
  // logger derives per-window rates from snapshot deltas.
  let bytesRecvCritical = 0;
  let bytesRecvRefinement = 0;
  let bytesRecvCriticalSnapshot = 0;
  let bytesRecvRefinementSnapshot = 0;

  // rAF loop — drains queues, flushes one frame per animation tick.
  let __rafTicks = 0;
  // Periodic diagnostic: every ~2 seconds dump tile + queue + framebuffer
  // counters to the page log. Lets us see at a glance whether tiles are
  // arriving, queueing, draining, and the framebuffer is sized.
  let __lastStatsMs = 0;
  let __lastDrainCounts = { raw: 0, solid: 0, palrle: 0, cdf53: 0, h264: 0 };
  // Idle suppression: skip emission when the snapshot is identical to
  // the last one. No heartbeat; silence means nothing changed.
  let __lastStatsLineKey = '';
  // Pass-level NACK strategy: gap-detection on receive + short
  // tail-fallback. CDF53 passes are emitted by the server in pass-major
  // order (0→13 per tile). When the wire delivers pass N for a tile but
  // we don't yet have some pass M<N, M was either lost or is in-flight
  // due to UDP reordering. Gap detection (in the per-pass receive
  // handler above) queues a NACK for M immediately; the debounced
  // flush below re-checks the bitmap a few rAF ticks later so an in-
  // flight M that arrives in the debounce window cancels the NACK.
  //
  // The debounce is the only thing here measured in time. It's a tiny
  // reordering tolerance, not a polling interval.
  //
  // Tail fallback: gap detection can't catch the FINAL pass being lost
  // (no higher pass arrives to trigger detection). If a tile is below
  // FULL_PASS_MASK and its bitmap hasn't gained a new bit in
  // TAIL_FALLBACK_MS, NACK whatever is still missing.
  const NACK_DEBOUNCE_MS = 50;
  const TAIL_FALLBACK_MS = 1500;
  const TAIL_SWEEP_INTERVAL_MS = 500;
  const FULL_PASS_MASK = (1 << 14) - 1;
  // Queue of NACKs awaiting debounce flush, keyed by string so
  // duplicate gap-detections (same pass already pending) coalesce.
  // The deferred flush re-checks the bitmap right before emitting,
  // so an in-flight pass that lands during the debounce cancels
  // itself out without server load.
  type PendingNack = { frameSeq: number; tileX: number; tileY: number; passIdx: number };
  const pendingNacks: Map<string, PendingNack> = new Map();
  let nackFlushTimer: ReturnType<typeof setTimeout> | null = null;
  function flushPendingNacks() {
    nackFlushTimer = null;
    if (pendingNacks.size === 0) return;
    const cov = (window as any).__cdf53Coverage as
      | Map<number, Cdf53CoverageEntry>
      | undefined;
    for (const entry of pendingNacks.values()) {
      // Re-check the live bitmap right before sending. A pass that
      // arrived during the debounce window is now flagged and we skip.
      const tileKey = (entry.tileX << 8) | entry.tileY;
      const v = cov?.get(tileKey);
      if (v && (v.passMask & (1 << entry.passIdx)) !== 0) continue;
      nackBatcher.add(
        {
          frameSeq: entry.frameSeq | TILE_DATAGRAM_FLAG,
          tileX: entry.tileX,
          tileY: entry.tileY,
          passIdx: entry.passIdx,
        },
        0,
      );
    }
    pendingNacks.clear();
  }
  function queuePassNack(frameSeq: number, tileX: number, tileY: number, passIdx: number): void {
    const key = `${tileX}|${tileY}|${passIdx}`;
    if (pendingNacks.has(key)) return;
    pendingNacks.set(key, { frameSeq, tileX, tileY, passIdx });
    if (nackFlushTimer === null) {
      nackFlushTimer = setTimeout(flushPendingNacks, NACK_DEBOUNCE_MS);
    }
  }
  // Make queuePassNack visible to the receive handler above by exposing
  // it on the closure-shared symbol. (TS hoisting: function declarations
  // are hoisted to the enclosing scope, so the per-pass handler above
  // can call queuePassNack even though it lexically appears after.)
  // No additional wiring needed.
  let __lastTailSweepMs = 0;
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
    for (const ev of core.onTimeout(nowUs())) handleEvent(ev);
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
        paintedTilesPerFrame.delete(seq);
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

    renderer.encodeAndPresentFrame((codec, tx, ty, code) => {
      decodeErrorBatcher.report({ codec, tileX: tx, tileY: ty, errorCode: code });
    });

    // Track drained counts (queue clearing = drained this tick).
    const w = window as any;
    w.__rafDrainTotals = w.__rafDrainTotals ?? { raw: 0, solid: 0, palrle: 0, cdf53: 0, h264: 0 };
    w.__rafDrainTotals.raw += beforeDrain.raw;
    w.__rafDrainTotals.solid += beforeDrain.solid;
    w.__rafDrainTotals.palrle += beforeDrain.palrle;
    w.__rafDrainTotals.cdf53 += beforeDrain.cdf53;
    w.__rafDrainTotals.h264 += beforeDrain.h264;
    w.__rafTicks = __rafTicks;

    // Once every ~2 s, log a one-liner summary so the page log shows
    // what's happening without needing devtools. Useful when
    // investigating "canvas is black but tiles seem to be arriving"
    // failure modes. Identical snapshots are dropped entirely — no
    // heartbeat, no liveness ping; if nothing's moving, the log stays
    // silent.
    const nowMs = performance.now();
    if (nowMs - __lastStatsMs > 2000) {
      const prevStatsMs = __lastStatsMs;
      __lastStatsMs = nowMs;
      const counts = w.__tileCounts ?? { raw: 0, solid: 0, palrle: 0, cdf53: 0, h264: 0, other: 0 };
      const drained = w.__rafDrainTotals;
      const drainDelta = {
        raw: drained.raw - __lastDrainCounts.raw,
        solid: drained.solid - __lastDrainCounts.solid,
        palrle: drained.palrle - __lastDrainCounts.palrle,
        cdf53: drained.cdf53 - __lastDrainCounts.cdf53,
        h264: drained.h264 - __lastDrainCounts.h264,
      };
      __lastDrainCounts = { ...drained };
      const fb = renderer.framebuffer;
      const cdf53Fails = w.__cdf53PrevalidateFails ?? 0;
      const cdf53Last = w.__cdf53LastFailCode ?? '-';

      // cdf53 per-tile coverage summary. `refined` = tiles that have
      // received all 14 passes for their current generation; `partial`
      // = tiles with 1..13 passes; `missing` is implicit (never seen,
      // not tracked). Use popcount on a 14-bit mask (passes 0..13).
      // The histogram exposes the *distribution* of pass counts so we
      // can tell e.g. "most are stuck at 8" (LL3 + a few bit-planes)
      // vs "most are at 14 but a handful missing one or two passes".
      const cdf53Cov = (w.__cdf53Coverage ?? new Map<number, Cdf53CoverageEntry>()) as Map<number, Cdf53CoverageEntry>;
      let cdf53Refined = 0;
      let cdf53Partial = 0;
      const cdf53PassHist = new Array(15).fill(0); // bucket index = passes received (0..14)
      const FULL_MASK_14 = (1 << 14) - 1;
      for (const v of cdf53Cov.values()) {
        // Mask off bits >= 14 in case of header corruption.
        const mask = v.passMask & FULL_MASK_14;
        let bits = mask;
        bits = bits - ((bits >> 1) & 0x5555);
        bits = (bits & 0x3333) + ((bits >> 2) & 0x3333);
        bits = (bits + (bits >> 4)) & 0x0f0f;
        const popcount = ((bits * 0x0101) >> 8) & 0xff;
        cdf53PassHist[popcount] = (cdf53PassHist[popcount] ?? 0) + 1;
        if (popcount === 14) cdf53Refined++;
        else if (popcount > 0) cdf53Partial++;
      }
      const cdf53Tiles = cdf53Cov.size;
      const histCompact = cdf53PassHist
        .map((n, i) => (n > 0 ? `${i}:${n}` : ''))
        .filter(Boolean)
        .join(' ');
      // Compose the line + an identity key for idle suppression. The
      // key excludes `raf:` (which always changes) and the cdf53fails
      // counter (also bookkeeping that creeps) — we suppress only when
      // nothing the user cares about has actually moved.
      const statsLine =
        `stats: rx{r:${counts.raw} s:${counts.solid} p:${counts.palrle} c:${counts.cdf53} h:${counts.h264}} ` +
        `Δdrain{r:${drainDelta.raw} s:${drainDelta.solid} p:${drainDelta.palrle} c:${drainDelta.cdf53} h:${drainDelta.h264}} ` +
        `fb:${fb.width}x${fb.height} ` +
        `cdf53fails:${cdf53Fails}(last=${cdf53Last}) ` +
        `raf:${__rafTicks} submit:${(window as unknown as { __gpuSubmitCount?: number }).__gpuSubmitCount ?? 0} lastSeq:${w.__lastTileSeq ?? '-'}`;
      const coverageLine =
        `cdf53-coverage: tiles=${cdf53Tiles} refined=${cdf53Refined}/14p partial=${cdf53Partial} ` +
        `pass-hist{${histCompact}}`;
      // Reliable-tile-emitter coverage: FEC recoveries + parity / NACK traffic.
      // Pairs with the server's reliability counters (Task 35) for side-by-side
      // wire-loss inspection. `parity_rx` / `recovered` / `parity_unrecoverable`
      // print an explicit "not measured" sentinel — see FEC_COUNTER_NOT_MEASURED's
      // definition for why these can no longer be fed post-cutover; `nack_sent`
      // stays live (fed by drainTransmit inspecting the core's own NACK output).
      const fecCoverageLine =
        `fec-coverage: recovered=${FEC_COUNTER_NOT_MEASURED} ` +
        `parity_rx=${FEC_COUNTER_NOT_MEASURED} ` +
        `parity_unrecoverable=${FEC_COUNTER_NOT_MEASURED} ` +
        `nack_sent=${nackSent}`;
      // Phase 1 Task 10: per-tier (passes 0-3 critical vs 4-13 refinement)
      // receive rates over the last stats window, computed from the byte
      // counters added in Task 6. The dt is the existing window duration
      // (already captured by __lastStatsMs); fall back to 1 to avoid
      // division by zero on the very first tick.
      const dtMs = Math.max(1, nowMs - prevStatsMs);
      const bpsCrit = Math.floor(((bytesRecvCritical - bytesRecvCriticalSnapshot) * 8 * 1000) / dtMs);
      const bpsRefn = Math.floor(((bytesRecvRefinement - bytesRecvRefinementSnapshot) * 8 * 1000) / dtMs);
      bytesRecvCriticalSnapshot = bytesRecvCritical;
      bytesRecvRefinementSnapshot = bytesRecvRefinement;
      const bweTierLine =
        `bwe-tier: bps_critical=${bpsCrit} bps_refinement=${bpsRefn} ` +
        `bytes_critical=${bytesRecvCritical} bytes_refinement=${bytesRecvRefinement}`;
      const lineKey =
        `r:${counts.raw}|s:${counts.solid}|p:${counts.palrle}|c:${counts.cdf53}|h:${counts.h264}|` +
        `seq:${w.__lastTileSeq ?? '-'}|cov:${cdf53Refined}/${cdf53Partial}/${cdf53Tiles}|hist:${histCompact}|` +
        // Retired counters dropped from the dedup key entirely (see
        // FEC_COUNTER_NOT_MEASURED) — they're constant now, so keeping them
        // here would only ever contribute a no-op comparison.
        `fec:${nackSent}|` +
        `bwe:${bytesRecvCritical}/${bytesRecvRefinement}`;
      const statsChanged = lineKey !== __lastStatsLineKey;
      if (statsChanged) {
        __lastStatsLineKey = lineKey;
        log(statsLine);
        log(coverageLine);
        log(fecCoverageLine);
        log(bweTierLine);
      }

      // Framebuffer readback: sample 4 known-position pixels and log
      // them. Settles the "is the framebuffer texture actually written
      // or is the blit broken" question.
      //
      //   (16,16)      = top-left background
      //   (960, 540)   = dead center (likely wizard for our test)
      //   (768, 992)   = first-tile area (24,31)*32
      //   (1900,1060)  = bottom-right corner
      //
      // Stays async — we kick off the GPU mapAsync and let the result
      // land in the next log line. Skipped when the stats line is
      // suppressed (nothing's moving → framebuffer is the same →
      // readback would produce identical output we'd then suppress
      // anyway, and the GPU mapAsync isn't free).
      const readPixel = (window as any).__readPixel as
        | ((x: number, y: number) => Promise<number[]>)
        | undefined;
      if (statsChanged && readPixel && fb.width > 0 && fb.height > 0) {
        const xs = [16, 960, 768, 1900];
        const ys = [16, 540, 992, 1060];
        Promise.all(
          xs.map((x, i) =>
            readPixel(Math.min(x, fb.width - 1), Math.min(ys[i], fb.height - 1)),
          ),
        )
          .then(samples => {
            const fmt = (px: number[]) =>
              `[${px[0]},${px[1]},${px[2]},${px[3]}]`;
            log(
              `fb-pixels: tl(16,16)=${fmt(samples[0])} ` +
              `ctr(960,540)=${fmt(samples[1])} ` +
              `tile(768,992)=${fmt(samples[2])} ` +
              `br(1900,1060)=${fmt(samples[3])}`,
            );
          })
          .catch(err => {
            log(`fb-pixels readback failed: ${err}`);
          });
      }
    }

    requestAnimationFrame(tick);
  }
  requestAnimationFrame(tick);

  /**
   * Process a tile-level source datagram (header + tile-header + payload).
   * Extracted into a helper so that XOR-recovered datagrams from the
   * ParityDecoder go through the exact same path as real wire arrivals —
   * the assembler, codec dispatch, and ACK batcher see them indistinguishably.
   * `bytes` is the full datagram including DatagramHeader at offset 0.
   */
  function handleSourceTileDatagram(bytes: Uint8Array) {
    if (bytes.byteLength < DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE) return;
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);

    const dgramHdr = decodeDatagramHeader(view, 0);
    dgramHdr.frameSeq = dgramHdr.frameSeq & ~TILE_DATAGRAM_FLAG;
    lossTracker.onDatagram();
    stats.tileDatagrams++;
    const tileHdr = decodeTileHeader(view, DATAGRAM_HEADER_SIZE);
    // M3.3d rev2: ACK on RECEIPT, per-tile-pass. Server's fragment_coverage
    // map is keyed by (frameSeq, tile_x, tile_y, pass_idx) — unique per
    // tile-pass emission within a frame, avoiding the frag_idx=0 collision
    // that occurs when multiple single-fragment work items share the same
    // frame_seq. Re-set the TILE_DATAGRAM_FLAG bit because the line above
    // already masked it off for downstream logic.
    //
    // Skip the frame-dimensions sentinel (0xFF, 0xFF): it's emitted outside
    // the scheduler and has no coverage entry on the server — ACKing it would
    // always produce an ack_miss log.
    if (tileHdr.tileX !== FRAME_DIMENSIONS_SENTINEL_X || tileHdr.tileY !== FRAME_DIMENSIONS_SENTINEL_Y) {
      // Phase 1.5-A v2: CDF53 ACKs DEFER to the per-pass-completion path
      // (after `prevalidateCdf53` succeeds). Without that gate, a pass
      // whose RLE-encoded bytes are corrupted (`ERR_CDF53_TRUNCATED` /
      // `ERR_CDF53_RLE_LENGTH`) still triggers an ACK on receipt — the
      // server's `record_cdf53_ack` then marks the pass as delivered,
      // `tile_fully_acked` transitions the tile to `PixelPerfect`, and
      // the retransmit cache is emptied. The client is left with a
      // permanently broken tile and no recovery mechanism on the
      // server's side (stranded escalation can't fire because the
      // server thinks every pass is acked).
      //
      // Non-Cdf53 codecs (Solid / PalRle / Raw / H264) don't have a
      // post-assembly prevalidation failure mode, so the legacy
      // ACK-on-receive remains correct for them.
      if (tileHdr.codec !== Codec.Cdf53) {
        ackBatcher.add({
          frameSeq: dgramHdr.frameSeq | TILE_DATAGRAM_FLAG,
          tileX: tileHdr.tileX,
          tileY: tileHdr.tileY,
          passIdx: tileHdr.pass,
          // Low 16 bits of performance.now() in milliseconds — wall-clock
          // monotonic-from-page-load, 65.5 s wrap. The server's BWE consumer
          // only looks at relative arrival differences between passes in the
          // same envelope, so anchor-point + clock skew are irrelevant.
          arrivalTimeMsLo16: (performance.now() & 0xFFFF) | 0,
        });
      }
    }

    // M3.5 bench: record the earliest receive timestamp per frame_seq.
    // Captured here — before any fragment-assembly checks — so it reflects
    // the true first-datagram-arrival time regardless of frag_idx ordering.
    const nowMs = performance.now();
    if (!firstRecvMs.has(dgramHdr.frameSeq)) {
      firstRecvMs.set(dgramHdr.frameSeq, nowMs);
    }

    if (dgramHdr.frameSeq > latestFrameSeq) {
      latestFrameSeq = dgramHdr.frameSeq;
    }

    const staleThreshold = latestFrameSeq - 2;
    for (const [k, asm] of assemblies) {
      const seq = parseInt(k.split(':')[0], 10);
      if (seq < staleThreshold) {
        if (asm.received < asm.fragments.length) {
          lossTracker.onStaleTile(asm.fragments.length, asm.received);
        }
        assemblies.delete(k);
        parityMap.delete(k);
        // M3.5 bench: frame is now stale — mark for recordFramePainted on next
        // rAF tick if we have any bench state for it (i.e. at least one tile
        // was painted). Frames that never produced a painted tile (e.g.
        // dropped entirely) are skipped to avoid spurious FIFO entries.
        if (paintedTilesPerFrame.has(seq)) {
          pendingFramePaintRaf.add(seq);
        }
      }
    }

    // Parity datagram (legacy fragment-level FEC inside a single tile assembly,
    // distinct from the 0x04 TileParityEnvelope routed via ParityDecoder above).
    if (dgramHdr.fragIdx >= dgramHdr.fragTotal) {
      const pKey = tileKey(dgramHdr.frameSeq, tileHdr.tileX, tileHdr.tileY, tileHdr.pass);
      let pr = parityMap.get(pKey);
      if (!pr) {
        pr = new ParityRecovery();
        parityMap.set(pKey, pr);
      }
      const payloadOffset = DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE;
      const parityPayload = new Uint8Array(
        bytes.buffer, bytes.byteOffset + payloadOffset, bytes.byteLength - payloadOffset
      );
      pr.addParity(parityPayload);

      const asmKey = tileKey(dgramHdr.frameSeq, tileHdr.tileX, tileHdr.tileY, tileHdr.pass);
      const pendingAsm = assemblies.get(asmKey);
      if (pendingAsm && pendingAsm.received === dgramHdr.fragTotal - 1) {
        const missingIdx = pendingAsm.fragments.findIndex(f => f === null);
        if (missingIdx >= 0) {
          const recovered = pr.tryRecover(missingIdx, pendingAsm.fragments);
          if (recovered) {
            pendingAsm.fragments[missingIdx] = recovered;
            pendingAsm.received++;
            lossTracker.onFecRecovery();
            finishAssembly(asmKey, pendingAsm);
          }
        }
      }
      return;
    }

    if (tileHdr.tileX === FRAME_DIMENSIONS_SENTINEL_X && tileHdr.tileY === FRAME_DIMENSIONS_SENTINEL_Y) {
      const payloadStart = DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE;
      const payloadBytes = bytes.byteLength - payloadStart;
      if (payloadBytes >= 8) {
        const dimView = new DataView(bytes.buffer, bytes.byteOffset + payloadStart, 8);
        const w = dimView.getUint32(0, false);
        const h = dimView.getUint32(4, false);
        renderer.resize(w, h);
        frameDimensionsKnown = true;
      }
      return;
    }

    if (tileHdr.codec === Codec.Skip) {
      return;
    }

    const key = tileKey(dgramHdr.frameSeq, tileHdr.tileX, tileHdr.tileY, tileHdr.pass);
    const payloadOffset = DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE;
    const fragData = new Uint8Array(bytes.buffer, bytes.byteOffset + payloadOffset, bytes.byteLength - payloadOffset);

    let asm = assemblies.get(key);
    if (!asm) {
      asm = {
        header: tileHdr,
        fragments: new Array(dgramHdr.fragTotal).fill(null),
        received: 0,
        // Server's fragment_coverage map is keyed with TILE_DATAGRAM_FLAG set;
        // mirror the ACK convention above so NACK lookups hit the same entry.
        emitKey: {
          frameSeq: dgramHdr.frameSeq | TILE_DATAGRAM_FLAG,
          tileX: tileHdr.tileX,
          tileY: tileHdr.tileY,
          passIdx: tileHdr.pass,
        },
        partialSince: performance.now(),
        nackedFragIdxs: new Set<number>(),
      };
      assemblies.set(key, asm);
    }

    if (asm.fragments[dgramHdr.fragIdx] === null) {
      asm.fragments[dgramHdr.fragIdx] = fragData.slice();
      asm.received += 1;
    }

    if (asm.received === dgramHdr.fragTotal - 1) {
      const pKey = tileKey(dgramHdr.frameSeq, tileHdr.tileX, tileHdr.tileY, tileHdr.pass);
      const pr = parityMap.get(pKey);
      if (pr) {
        const missingIdx = asm.fragments.findIndex(f => f === null);
        if (missingIdx >= 0) {
          const recovered = pr.tryRecover(missingIdx, asm.fragments);
          if (recovered) {
            asm.fragments[missingIdx] = recovered;
            asm.received++;
            lossTracker.onFecRecovery();
          }
        }
      }
    }

    if (asm.received === dgramHdr.fragTotal) {
      finishAssembly(key, asm);
    }
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

    for (const ev of core.handleDatagram(value, nowUs()) as WasmEvent[]) handleEvent(ev);
    await drainTransmit();
  }
}

main().catch((e) => {
  log(`Error: ${e.message}`);
  statusEl.textContent = `Error: ${e.message}`;
});

