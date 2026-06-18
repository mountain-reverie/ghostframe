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
import { LossTracker, encodeHello } from './feedback';
import { attachInputCapture } from './input/wire';
import { DecodeErrorBatcher } from './decode_error_batcher';
import { AckBatcher } from './ack';
import { NackBatcher } from './nack.js';
import { initDiagnostics } from './diagnostics.js';
import { prevalidateCdf53 } from './prevalidate_cdf53.js';
import { bootstrap } from './bootstrap.js';

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
  const url = new URL(window.location.href);

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
  const feedbackWriter = await (async () => {
    try {
      const bidi = await transport.createBidirectionalStream();
      return bidi.writable.getWriter();
    } catch {
      console.warn('Could not open feedback stream');
      return null;
    }
  })();

  // Emit HELLO immediately. We hard-require WebGPU, so indicesRawEnabled is
  // unconditional. supportsH264 reflects the result of the renderer's startup
  // probe (probeH264 in webgpu/renderer.ts): true on Chrome/Chromium where
  // texture_external + WebCodecs are available, false on Firefox where Naga
  // currently rejects the h264_blit shader for lack of TEXTURE_EXTERNAL
  // capability. The server uses this to gate FrameMode::H264 selection so
  // Firefox never receives unplayable H.264 frames.
  if (feedbackWriter) {
    try {
      await feedbackWriter.write(
        encodeHello({
          indicesRawEnabled: true,
          supportsH264: renderer.h264Supported,
        }),
      );
    } catch (e) {
      console.warn('HELLO write failed:', e);
    }
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

  // Batched ACK sender — fire-and-forget unreliable datagrams. The catch
  // logs the first error per writer so a broken ACK path is discoverable;
  // subsequent writes are silent to avoid spamming during normal teardown.
  const ackWriter = transport.datagrams.writable.getWriter();
  let ackWriteLogged = false;
  const ackBatcher = new AckBatcher((dg) => {
    ackWriter.write(dg).catch((err) => {
      if (!ackWriteLogged) {
        console.warn('ACK datagram write failed:', err);
        ackWriteLogged = true;
      }
    });
  });

  // Reliable-tile-emitter NACK sender — reuses the same datagrams writer.
  // Fire-and-forget; rejection from a closed stream is benign at this point.
  const nackBatcher = new NackBatcher((buf) => {
    ackWriter.write(buf).catch(() => {});
  });

  const assemblies = new Map<string, TileAssembly>();
  let latestFrameSeq = 0;
  let firstTileRendered = false;
  let frameDimensionsKnown = false;

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
    if (asm.header.codec === Codec.Raw) {
      w.__tileCounts.raw++;
      renderer.rawQueue.push({ tileX: tX, tileY: tY, bgra: payload });
    } else if (asm.header.codec === Codec.Solid) {
      w.__tileCounts.solid++;
      if (payload.byteLength === 4) {
        renderer.solidQueue.push({ tileX: tX, tileY: tY, bgra: payload });
      }
    } else if (asm.header.codec === Codec.PalRle) {
      w.__tileCounts.palrle++;
      renderer.palRleQueue.push({ tileX: tX, tileY: tY, payload });
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
      const cdf53Coverage: Map<number, { generation: number; passMask: number }> =
        (w.__cdf53Coverage ??= new Map());
      const tileKey = (tX << 8) | tY;
      const gen = asm.header.generation;
      const passIdx = asm.header.pass;
      const existing = cdf53Coverage.get(tileKey);
      if (!existing || existing.generation !== gen) {
        // First pass for this (tile, generation) — replaces any prior
        // partial state from the previous generation.
        cdf53Coverage.set(tileKey, { generation: gen, passMask: 1 << passIdx });
      } else {
        existing.passMask |= 1 << passIdx;
      }

      const r = prevalidateCdf53(payload, asm.header.generation, asm.header.pass);
      if (!r.ok) {
        w.__cdf53PrevalidateFails = (w.__cdf53PrevalidateFails ?? 0) + 1;
        w.__cdf53LastFailCode = r.errorCode;
        decodeErrorBatcher.report({
          codec: Codec.Cdf53,
          tileX: tX,
          tileY: tY,
          errorCode: r.errorCode,
        });
      } else {
        // Caller-fills tileX/tileY (prevalidate left them at 0).
        r.entry.tileX = tX;
        r.entry.tileY = tY;
        renderer.cdf53Queue.push(r.entry);
        w.__cdf53PushedToQueue = (w.__cdf53PushedToQueue ?? 0) + 1;
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
  function tick() {
    __rafTicks++;
    diag.recordRafTick(__rafTicks);

    // Reliable-tile-emitter: scan partial assemblies for fragment timeouts
    // and feed missing frag_idxs into the NackBatcher.
    scanForAssemblyTimeouts(performance.now(), assemblies.values());

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
      const cdf53Cov = (w.__cdf53Coverage ?? new Map<number, { generation: number; passMask: number }>()) as Map<number, { generation: number; passMask: number }>;
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
        `raf:${__rafTicks} lastSeq:${w.__lastTileSeq ?? '-'}`;
      const coverageLine =
        `cdf53-coverage: tiles=${cdf53Tiles} refined=${cdf53Refined}/14p partial=${cdf53Partial} ` +
        `pass-hist{${histCompact}}`;
      const lineKey =
        `r:${counts.raw}|s:${counts.solid}|p:${counts.palrle}|c:${counts.cdf53}|h:${counts.h264}|` +
        `seq:${w.__lastTileSeq ?? '-'}|cov:${cdf53Refined}/${cdf53Partial}/${cdf53Tiles}|hist:${histCompact}`;
      const statsChanged = lineKey !== __lastStatsLineKey;
      if (statsChanged) {
        __lastStatsLineKey = lineKey;
        log(statsLine);
        log(coverageLine);
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

  // Receive datagrams.
  const reader = transport.datagrams.readable.getReader();
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;

    if (!value || value.byteLength === 0) continue;

    // Backward compat: small text datagrams (ping/pong).
    if (value.byteLength < 20) {
      const text = new TextDecoder().decode(value);
      log(`Received: ${text} (${value.byteLength} bytes)`);
      if (text === 'pong') {
        statusEl.textContent = 'Ping/Pong successful!';
        log('M0 COMPLETE: Ping/Pong datagram round-trip verified!');
      }
      continue;
    }

    if (value.byteLength < DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE) {
      log(`Datagram too short: ${value.byteLength} bytes`);
      continue;
    }

    const view = new DataView(value.buffer, value.byteOffset, value.byteLength);

    if (!isTileDatagram(view, 0)) {
      // Frame-level datagram.
      if (value.byteLength < FRAME_HEADER_SIZE) continue;

      const frameHdr = decodeFrameHeader(view, 0);
      lossTracker.onDatagram();
      stats.frameDatagrams++;

      if (frameHdr.frameSeq < latestFullFrameSeq - 2) continue;
      if (frameHdr.frameSeq > latestFullFrameSeq) {
        latestFullFrameSeq = frameHdr.frameSeq;
      }

      for (const [k, asm] of frameAssemblies) {
        const seq = parseInt(k.split(':')[1], 10);
        if (seq < latestFullFrameSeq - 2) {
          if (asm.received < asm.fragments.length) {
            lossTracker.onStaleTile(asm.fragments.length, asm.received);
          }
          frameAssemblies.delete(k);
        }
      }

      if (frameHdr.fragIdx >= frameHdr.fragTotal) continue;

      const fKey = frameKey(frameHdr.frameSeq);
      const payloadOffset = FRAME_HEADER_SIZE;
      const fragData = new Uint8Array(
        value.buffer, value.byteOffset + payloadOffset,
        value.byteLength - payloadOffset,
      );

      let asm = frameAssemblies.get(fKey);
      if (!asm) {
        asm = {
          header: frameHdr,
          fragments: new Array(frameHdr.fragTotal).fill(null),
          received: 0,
        };
        frameAssemblies.set(fKey, asm);
      }

      if (asm.fragments[frameHdr.fragIdx] === null) {
        asm.fragments[frameHdr.fragIdx] = fragData.slice();
        asm.received += 1;
      }

      if (asm.received === frameHdr.fragTotal) {
        frameAssemblies.delete(fKey);

        const totalLen = asm.fragments.reduce((acc, f) => acc + (f ? f.byteLength : 0), 0);
        const payload = new Uint8Array(totalLen);
        let off = 0;
        for (const frag of asm.fragments) {
          if (frag) { payload.set(frag, off); off += frag.byteLength; }
        }

        if (!fullFrameDecoder) {
          fullFrameDecoder = new FullFrameDecoder((frame: VideoFrame) => {
            renderer.h264Queue.push(frame);
          }, 1920, 1080);
        }

        fullFrameDecoder.decode(payload, asm.header.isKeyframe);

        if (!firstTileRendered) {
          firstTileRendered = true;
          log(`First full frame: ${payload.byteLength}B ${asm.header.isKeyframe ? '(keyframe)' : ''}`);
          statusEl.textContent = 'Receiving frames';
        }
      }

      continue;
    }

    // --- Tile-level datagram processing ---
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
      ackBatcher.add({
        frameSeq: dgramHdr.frameSeq | TILE_DATAGRAM_FLAG,
        tileX: tileHdr.tileX,
        tileY: tileHdr.tileY,
        passIdx: tileHdr.pass,
      });
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

    // Parity datagram.
    if (dgramHdr.fragIdx >= dgramHdr.fragTotal) {
      const pKey = tileKey(dgramHdr.frameSeq, tileHdr.tileX, tileHdr.tileY);
      let pr = parityMap.get(pKey);
      if (!pr) {
        pr = new ParityRecovery();
        parityMap.set(pKey, pr);
      }
      const payloadOffset = DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE;
      const parityPayload = new Uint8Array(
        value.buffer, value.byteOffset + payloadOffset, value.byteLength - payloadOffset
      );
      pr.addParity(parityPayload);

      const asmKey = tileKey(dgramHdr.frameSeq, tileHdr.tileX, tileHdr.tileY);
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
      continue;
    }

    if (tileHdr.tileX === FRAME_DIMENSIONS_SENTINEL_X && tileHdr.tileY === FRAME_DIMENSIONS_SENTINEL_Y) {
      const payloadStart = DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE;
      const payloadBytes = value.byteLength - payloadStart;
      if (payloadBytes >= 8) {
        const dimView = new DataView(value.buffer, value.byteOffset + payloadStart, 8);
        const w = dimView.getUint32(0, false);
        const h = dimView.getUint32(4, false);
        renderer.resize(w, h);
        frameDimensionsKnown = true;
      }
      continue;
    }

    if (tileHdr.codec === Codec.Skip) {
      continue;
    }

    const key = tileKey(dgramHdr.frameSeq, tileHdr.tileX, tileHdr.tileY);
    const payloadOffset = DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE;
    const fragData = new Uint8Array(value.buffer, value.byteOffset + payloadOffset, value.byteLength - payloadOffset);

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
      const pKey = tileKey(dgramHdr.frameSeq, tileHdr.tileX, tileHdr.tileY);
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
}

main().catch((e) => {
  log(`Error: ${e.message}`);
  statusEl.textContent = `Error: ${e.message}`;
});
