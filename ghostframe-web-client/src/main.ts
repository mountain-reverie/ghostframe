import {
  DATAGRAM_HEADER_SIZE, TILE_HEADER_SIZE, TILE_SIZE, Codec,
  decodeDatagramHeader, decodeTileHeader, tileKey, TileAssembly, H264TileDecoder,
  FRAME_HEADER_SIZE, TILE_DATAGRAM_FLAG, FrameAssembly,
  isTileDatagram, decodeFrameHeader, frameKey, FullFrameDecoder,
  FRAME_DIMENSIONS_SENTINEL_X, FRAME_DIMENSIONS_SENTINEL_Y,
} from './decoder.js';
import debugGradientWgsl from './webgpu/shaders/debug_gradient.wgsl?raw';
import { WebGpuRenderer } from './webgpu/renderer.js';
import { WebGpuUnavailableError } from './webgpu/init.js';
import { ParityRecovery } from './fec';
import { LossTracker, encodeHello } from './feedback';
import { DecodeErrorBatcher } from './decode_error_batcher';
import { AckBatcher } from './ack';

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

  // WebGPU canvases cannot be reliably read via canvas.getContext('2d').getImageData
  // in Chrome 147 when the canvas has been resized after initial configuration
  // (the compositor's copy may lag behind). Instead, read directly from the
  // GPU framebuffer texture via a staging buffer (COPY_SRC → MAP_READ).
  //
  // Both __readPixel and __readPixelRect return Promises; the E2E test
  // framework (chromiumoxide) resolves the returned Promise automatically.
  (window as any).__readPixel = async (x: number, y: number): Promise<number[]> => {
    const rendererRef: any = (window as any).__ghostframeRenderer;
    if (!rendererRef) {
      // Fallback to drawImage if renderer not exposed yet
      const tmp = document.createElement('canvas');
      tmp.width = 1; tmp.height = 1;
      const ctx = tmp.getContext('2d')!;
      ctx.drawImage(canvasEl, x, y, 1, 1, 0, 0, 1, 1);
      return Array.from(ctx.getImageData(0, 0, 1, 1).data);
    }
    const { device, texture } = rendererRef;
    if (!device || !texture) return [0, 0, 0, 0];
    // Row size must be a multiple of 256 bytes.
    const bytesPerRow = 256;
    const staging = device.createBuffer({
      size: bytesPerRow,
      usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
    });
    const enc = device.createCommandEncoder();
    enc.copyTextureToBuffer(
      { texture, origin: { x, y } },
      { buffer: staging, bytesPerRow },
      [1, 1],
    );
    device.queue.submit([enc.finish()]);
    await staging.mapAsync(GPUMapMode.READ);
    const view = new Uint8Array(staging.getMappedRange(0, 4));
    const result = Array.from(view) as number[];
    staging.unmap();
    staging.destroy();
    return result; // [R, G, B, A]
  };

  // Multi-pixel variant for hash-based E2E stability checks.
  (window as any).__readPixelRect = async (x: number, y: number, w: number, h: number): Promise<number[]> => {
    const rendererRef: any = (window as any).__ghostframeRenderer;
    if (!rendererRef) {
      const tmp = document.createElement('canvas');
      tmp.width = w; tmp.height = h;
      const ctx = tmp.getContext('2d')!;
      ctx.drawImage(canvasEl, x, y, w, h, 0, 0, w, h);
      return Array.from(ctx.getImageData(0, 0, w, h).data);
    }
    const { device, texture } = rendererRef;
    if (!device || !texture) return new Array(w * h * 4).fill(0);
    // bytesPerRow must be multiple of 256
    const bytesPerRow = Math.ceil(w * 4 / 256) * 256;
    const staging = device.createBuffer({
      size: bytesPerRow * h,
      usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
    });
    const enc = device.createCommandEncoder();
    enc.copyTextureToBuffer(
      { texture, origin: { x, y } },
      { buffer: staging, bytesPerRow },
      [w, h],
    );
    device.queue.submit([enc.finish()]);
    await staging.mapAsync(GPUMapMode.READ);
    // Compact: remove row padding
    const raw = new Uint8Array(staging.getMappedRange());
    const result: number[] = [];
    for (let row = 0; row < h; row++) {
      for (let col = 0; col < w * 4; col++) {
        result.push(raw[row * bytesPerRow + col]);
      }
    }
    staging.unmap();
    staging.destroy();
    return result;
  };

  // Debug-only: dispatch a compute pipeline that writes a known gradient
  // (r=x&255, g=y&255, b=0, a=255) to the framebuffer, then read back via
  // __readPixel at sample points.  Used by e2e_readpixel_correctness to
  // verify __readPixel before any production-codec pixel assertion is
  // trusted.  See spec/2026-05-15-m3.2c-verification-design.md §W2.
  (window as any).__readGradientGolden = async (): Promise<{
    ok: boolean;
    mismatches: Array<{ x: number; y: number; got: number[]; want: number[] }>;
  }> => {
    const rendererRef: any = (window as any).__ghostframeRenderer;
    if (!rendererRef) {
      return { ok: false, mismatches: [{ x: -1, y: -1, got: [], want: [] }] };
    }
    const { device, texture } = rendererRef;
    if (!device || !texture) {
      return { ok: false, mismatches: [{ x: -1, y: -1, got: [], want: [] }] };
    }
    const fbW = texture.width;
    const fbH = texture.height;
    if (fbW === 0 || fbH === 0) {
      return { ok: false, mismatches: [{ x: -1, y: -1, got: [], want: [] }] };
    }
    // Build the debug compute pipeline (fresh per call — cheap, debug-only).
    const module = device.createShaderModule({ code: debugGradientWgsl });
    const pipeline = device.createComputePipeline({
      layout: 'auto',
      compute: { module, entryPoint: 'debug_gradient' },
    });
    const bindGroup = device.createBindGroup({
      layout: pipeline.getBindGroupLayout(0),
      entries: [{ binding: 0, resource: texture.createView() }],
    });
    const encoder = device.createCommandEncoder();
    const pass = encoder.beginComputePass();
    pass.setPipeline(pipeline);
    pass.setBindGroup(0, bindGroup);
    pass.dispatchWorkgroups(Math.ceil(fbW / 8), Math.ceil(fbH / 8), 1);
    pass.end();
    device.queue.submit([encoder.finish()]);
    await device.queue.onSubmittedWorkDone();

    // Sample 16 points — corners, edges, near-256 wrap boundaries, center.
    const samplePoints: Array<[number, number]> = [
      [0, 0], [1, 0], [255, 0], [256, 0],
      [0, 1], [0, 255], [0, 256],
      [100, 200], [16, 16], [31, 31], [32, 32], [63, 63],
      [Math.floor(fbW / 2), Math.floor(fbH / 2)],
      [fbW - 1, fbH - 1],
      [Math.min(255, fbW - 1), Math.min(255, fbH - 1)],
      [Math.min(256, fbW - 1), Math.min(256, fbH - 1)],
    ];
    const mismatches: Array<{ x: number; y: number; got: number[]; want: number[] }> = [];
    for (const [x, y] of samplePoints) {
      if (x >= fbW || y >= fbH || x < 0 || y < 0) continue;
      const got = await (window as any).__readPixel(x, y);
      const want = [x & 0xFF, y & 0xFF, 0, 255];
      if (got[0] !== want[0] || got[1] !== want[1] || got[2] !== want[2] || got[3] !== want[3]) {
        mismatches.push({ x, y, got, want });
      }
    }
    return { ok: mismatches.length === 0, mismatches };
  };

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

  let transport: WebTransport;
  if (certHash) {
    transport = new WebTransport(wtUrl, {
      serverCertificateHashes: [{ algorithm: 'sha-256', value: hexToBuffer(certHash) }],
    });
  } else {
    transport = new WebTransport(wtUrl);
  }

  transport.closed.then(
    (info) => {
      log(`Transport closed: code=${info.closeCode} reason=${info.reason}`);
      renderer.onSessionReset();
    },
    (err) => {
      log(`Transport closed with error: ${err}`);
      renderer.onSessionReset();
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

  const decodeErrorBatcher = new DecodeErrorBatcher((bytes) => {
    if (feedbackWriter) {
      feedbackWriter.write(bytes).catch(() => {});
    }
  });

  if (feedbackWriter) {
    setInterval(async () => {
      try {
        const msg = lossTracker.encodeFeedback();
        await feedbackWriter.write(msg);
      } catch {
        // Stream closed — stop reporting
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

  // Batched ACK sender — fire-and-forget unreliable datagrams.
  const ackWriter = transport.datagrams.writable.getWriter();
  const ackBatcher = new AckBatcher((dg) => {
    ackWriter.write(dg).catch(() => {});
  });

  const assemblies = new Map<string, TileAssembly>();
  let latestFrameSeq = 0;
  let firstTileRendered = false;
  let frameDimensionsKnown = false;

  /** Reassemble completed tile and route it to the renderer's per-codec queue. */
  function finishAssembly(asmKey: string, asm: TileAssembly) {
    assemblies.delete(asmKey);

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
        renderer.resize(w, h);
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
        renderer.resize(
          Math.max(canvasEl.width, minWidth),
          Math.max(canvasEl.height, minHeight)
        );
      }
    }

    // Test instrumentation: record codecs for E2E protocol-layer assertions.
    if (typeof window !== "undefined") {
      const w = window as unknown as { __ghostframeRecordedCodecs?: number[] };
      if (!w.__ghostframeRecordedCodecs) {
        w.__ghostframeRecordedCodecs = [];
      }
      w.__ghostframeRecordedCodecs.push(asm.header.codec);
    }

    if (asm.header.codec === Codec.Raw) {
      renderer.rawQueue.push({ tileX: tX, tileY: tY, bgra: payload });
    } else if (asm.header.codec === Codec.H264) {
      const dec = getH264Decoder(tX, tY);
      dec.decode(payload);
    } else if (asm.header.codec === Codec.Solid) {
      if (payload.byteLength === 4) {
        renderer.solidQueue.push({ tileX: tX, tileY: tY, bgra: payload });
      }
    } else if (asm.header.codec === Codec.PalRle) {
      renderer.palRleQueue.push({ tileX: tX, tileY: tY, payload });
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
    (window as any).__ghostframeRafTicks = __rafTicks;
    renderer.encodeAndPresentFrame((codec, tx, ty, code) => {
      decodeErrorBatcher.report({ codec, tileX: tx, tileY: ty, errorCode: code });
    });
    requestAnimationFrame(tick);
  }
  requestAnimationFrame(tick);

  // Test-instrumentation counters.
  window.__ghostframeStats = { tileDatagrams: 0, frameDatagrams: 0 };
  const stats = window.__ghostframeStats;

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
            renderer.h264Queue.push({ tileX: -1, tileY: -1, frame });
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
