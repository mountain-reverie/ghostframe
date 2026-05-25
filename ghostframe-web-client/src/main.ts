import {
  DATAGRAM_HEADER_SIZE, TILE_HEADER_SIZE, TILE_SIZE, Codec,
  decodeDatagramHeader, decodeTileHeader, tileKey, TileAssembly, H264TileDecoder,
  FRAME_HEADER_SIZE, TILE_DATAGRAM_FLAG, FrameAssembly,
  isTileDatagram, decodeFrameHeader, frameKey, FullFrameDecoder,
  FRAME_DIMENSIONS_SENTINEL_X, FRAME_DIMENSIONS_SENTINEL_Y,
} from './decoder.js';
import { WebGpuRenderer, type PalRleQueued, type RawQueued } from './webgpu/renderer.js';
import { WebGpuUnavailableError } from './webgpu/init.js';
import type { SolidTile } from './webgpu/solid.js';
import { ParityRecovery } from './fec';
import { LossTracker, encodeHello } from './feedback';
import { DecodeErrorBatcher } from './decode_error_batcher';
import { AckBatcher } from './ack';
import { initDiagnostics } from './diagnostics.js';

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

function hexToBuffer(hex: string): ArrayBuffer {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < hex.length; i += 2) {
    bytes[i / 2] = parseInt(hex.substr(i, 2), 16);
  }
  return bytes.buffer;
}

async function main() {
  const url = new URL(window.location.href);
  const serverHost = url.searchParams.get('host') ?? 'ghostframe-server:4443';
  const certHash = url.searchParams.get('certHash') ?? '';

  const wtUrl = `https://${serverHost}/`;

  log(`Connecting to ${wtUrl}...`);

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
  // Expose renderer type for test diagnostics.
  (window as any).__ghostframeIsSwiftShader = renderer.isSwiftShader;

  // Device-lost recovery: if the GPU device is lost (e.g. SwiftShader crashes
  // when the WebCodecs VideoDecoder is used), recreate the renderer so the
  // WebGPU pipeline can resume. Tiles queued after recreation will be
  // processed against the new device. The recovery handler re-registers
  // itself on the new device so subsequent losses are also handled.
  // True while the WebGPU device is lost and recreation is pending.
  // finishAssembly routes tiles into the stable pending-* queues below
  // instead of the renderer's queues so they survive renderer recreation.
  let deviceLostPending = false;

  // Stable tile buffers that survive renderer recreation. finishAssembly
  // routes tiles here when deviceLostPending=true; the device-lost handler
  // drains them into the new renderer once recreation completes.
  const pendingPalRle: PalRleQueued[] = [];
  const pendingSolid: SolidTile[] = [];
  const pendingRaw: RawQueued[] = [];

  function installDeviceLostHandler(r: WebGpuRenderer) {
    r.device.lost.then(async (info) => {
      console.warn(`WebGPU device lost (reason=${info.reason}): ${info.message}`);
      (window as any).__webgpuDeviceLostCount = ((window as any).__webgpuDeviceLostCount || 0) + 1;
      (window as any).__webgpuDeviceLostReason = info.reason;
      deviceLostPending = true;
      // Capture dimensions before the old renderer is replaced.
      const savedW = r.framebuffer.width;
      const savedH = r.framebuffer.height;
      try {
        (window as any).__webgpuRecreationAttempts = ((window as any).__webgpuRecreationAttempts || 0) + 1;
        const newRenderer = await WebGpuRenderer.create(canvasEl);
        if (savedW > 0 && savedH > 0) {
          newRenderer.resize(savedW, savedH);
        }
        // Drain any tiles that arrived during the device-loss window.
        for (const t of pendingPalRle) newRenderer.palRleQueue.push(t);
        for (const t of pendingSolid) newRenderer.solidQueue.push(t);
        for (const t of pendingRaw) newRenderer.rawQueue.push(t);
        pendingPalRle.length = 0;
        pendingSolid.length = 0;
        pendingRaw.length = 0;
        renderer = newRenderer;
        deviceLostPending = false;
        installDeviceLostHandler(newRenderer);
        console.info('WebGPU device recreated after loss');
        (window as any).__webgpuDeviceRecreated = ((window as any).__webgpuDeviceRecreated || 0) + 1;
      } catch (e) {
        console.error('WebGPU device recreation failed:', e);
        (window as any).__webgpuRecreationError = String(e);
        deviceLostPending = false; // don't block tiles forever on failed recreation
      }
    });
  }
  installDeviceLostHandler(renderer);

  // Wire all test/e2e diagnostic globals onto `window` via diagnostics.ts.
  // The getRenderer callback is called lazily (per __readPixel invocation)
  // so it always reflects the current framebuffer texture set by
  // renderer.encodeAndPresentFrame on the previous rAF tick.
  const diag = initDiagnostics({
    canvas: canvasEl,
    getRenderer: () => window.__ghostframeRenderer ?? null,
  });
  const stats = diag.stats;

  let transport: WebTransport;
  if (certHash) {
    transport = new WebTransport(wtUrl, {
      serverCertificateHashes: [{ algorithm: 'sha-256', value: hexToBuffer(certHash) }],
    });
  } else {
    transport = new WebTransport(wtUrl);
  }

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
    // per-decoder close() calls below don't hit a double-close hazard on
    // latestFrame (renderer may have moved the same VideoFrame into
    // videoFramesToClose on the previous rAF).
    renderer.onSessionReset();

    // Close every per-tile H.264 decoder to release the underlying VideoDecoder
    // instances. Iterate then clear — not clear-then-iterate — so each decoder's
    // close() runs before the reference is dropped.
    for (const dec of h264Decoders.values()) {
      dec.close();
    }
    h264Decoders.clear();

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

  // Emit HELLO immediately. We hard-require WebGPU, so indicesRawEnabled is unconditional.
  if (feedbackWriter) {
    try {
      await feedbackWriter.write(encodeHello({ indicesRawEnabled: true }));
    } catch (e) {
      console.warn('HELLO write failed:', e);
    }
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

  // Per-tile H.264 decoders.
  const h264Decoders = new Map<string, H264TileDecoder>();

  // Full-frame decoder and reassembly state.
  let fullFrameDecoder: FullFrameDecoder | null = null;
  const frameAssemblies = new Map<string, FrameAssembly>();
  let latestFullFrameSeq = 0;

  function getH264Decoder(tileX: number, tileY: number): H264TileDecoder {
    const key = `${tileX}:${tileY}`;
    let dec = h264Decoders.get(key);
    if (!dec) {
      dec = new H264TileDecoder((frame: VideoFrame) => {
        renderer.h264Queue.push({ tileX, tileY, frame });
      });
      h264Decoders.set(key, dec);
    }
    return dec;
  }

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

  const assemblies = new Map<string, TileAssembly>();
  let latestFrameSeq = 0;
  let firstTileRendered = false;
  let frameDimensionsKnown = false;

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
    diag.recordTile({
      seq: frameSeqFromKey,
      tileX: tX,
      tileY: tY,
      codec: asm.header.codec,
      payloadLen: payload.byteLength,
      fbWidth: renderer.framebuffer.width,
      fbHeight: renderer.framebuffer.height,
      palRleFlag: asm.header.codec === Codec.PalRle ? payload[0] : undefined,
    });

    if (asm.header.codec === Codec.Raw) {
      if (deviceLostPending) {
        pendingRaw.push({ tileX: tX, tileY: tY, bgra: payload });
      } else {
        renderer.rawQueue.push({ tileX: tX, tileY: tY, bgra: payload });
      }
    } else if (asm.header.codec === Codec.H264) {
      const dec = getH264Decoder(tX, tY);
      dec.decode(payload);
    } else if (asm.header.codec === Codec.Solid) {
      if (payload.byteLength === 4) {
        if (deviceLostPending) {
          pendingSolid.push({ tileX: tX, tileY: tY, bgra: payload });
        } else {
          renderer.solidQueue.push({ tileX: tX, tileY: tY, bgra: payload });
        }
      }
    } else if (asm.header.codec === Codec.PalRle) {
      if (deviceLostPending) {
        pendingPalRle.push({ tileX: tX, tileY: tY, payload });
      } else {
        renderer.palRleQueue.push({ tileX: tX, tileY: tY, payload });
      }
    }

    if (!firstTileRendered) {
      firstTileRendered = true;
      const sample = Array.from(payload.slice(0, 16))
        .map(b => b.toString(16).padStart(2, '0'))
        .join(' ');
      log(`First tile: (${tX},${tY}) ${payload.byteLength}B`);
      log(`First bytes: ${sample}`);
      statusEl.textContent = 'Receiving frames';
    }

    ackBatcher.add({
      tileX: tX,
      tileY: tY,
      generation: asm.header.generation,
      pass: asm.header.pass,
    });
  }

  // rAF loop — drains queues, flushes one frame per animation tick.
  let __rafTicks = 0;
  function tick() {
    __rafTicks++;
    diag.recordRafTick(__rafTicks);
    renderer.encodeAndPresentFrame((codec, tx, ty, code) => {
      decodeErrorBatcher.report({ codec, tileX: tx, tileY: ty, errorCode: code });
    });
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

        // Skip full-frame H.264 decode on SwiftShader: WebCodecs VideoDecoder
        // crashes the GPU process on SwiftShader, permanently destroying the
        // WebGPU device. TileCodec (PalRle) will repaint everything after the
        // H.264→TileCodec transition anyway, so dropping H.264 frames here is
        // acceptable in the software-renderer path.
        if (!renderer.isSwiftShader) {
          if (!fullFrameDecoder) {
            const fbW = renderer.framebuffer.width || 1920;
            const fbH = renderer.framebuffer.height || 1080;
            (window as any).__fullFrameDecoderDims = { w: fbW, h: fbH };
            fullFrameDecoder = new FullFrameDecoder((frame: VideoFrame) => {
              const cnt = ((window as any).__fullFrameOutputCount || 0) + 1;
              (window as any).__fullFrameOutputCount = cnt;
              const frameW = frame.displayWidth;
              const frameH = frame.displayHeight;
              // Use copyTo() to extract RGBA pixels and write directly to the
              // framebuffer texture. This avoids importExternalTexture which
              // causes SwiftShader device loss in the e2e headless environment.
              const rgba = new Uint8Array(new ArrayBuffer(frameW * frameH * 4));
              frame.copyTo(rgba, { format: 'RGBA' }).then(() => {
                // Sample pixel at (100,100) for diagnostics
                const pixOff = (100 * frameW + 100) * 4;
                if (!((window as any).__fullFramePixelSamples)) (window as any).__fullFramePixelSamples = [];
                (window as any).__fullFramePixelSamples.push({
                  n: cnt, w: frameW, h: frameH,
                  r: rgba[pixOff], g: rgba[pixOff+1], b: rgba[pixOff+2],
                  fbW: renderer.framebuffer.width, fbH: renderer.framebuffer.height,
                });
                renderer.writeFullFrameRgba(rgba, frameW, frameH);
                frame.close();
              }).catch((e) => {
                if (!((window as any).__fullFrameCopyToErrors)) (window as any).__fullFrameCopyToErrors = [];
                (window as any).__fullFrameCopyToErrors.push({ n: cnt, err: String(e) });
                frame.close();
              });
            }, fbW, fbH);
          }

          if (!(window as any).__fullFramePayloadSizes) (window as any).__fullFramePayloadSizes = [];
          // Capture first 16 bytes of first IDR as hex for analysis
          const hex16 = Array.from(payload.slice(0, 16)).map(b => b.toString(16).padStart(2,'0')).join(' ');
          (window as any).__fullFramePayloadSizes.push({ size: payload.byteLength, isKeyframe: asm.header.isKeyframe, hex16 });
          fullFrameDecoder.decode(payload, asm.header.isKeyframe, asm.header.timestampUs);
        }

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
