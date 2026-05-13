import {
  DATAGRAM_HEADER_SIZE, TILE_HEADER_SIZE, TILE_SIZE, Codec,
  decodeDatagramHeader, decodeTileHeader, tileKey, TileAssembly, H264TileDecoder,
  FRAME_HEADER_SIZE, TILE_DATAGRAM_FLAG, FrameAssembly,
  isTileDatagram, decodeFrameHeader, frameKey, FullFrameDecoder,
  FRAME_DIMENSIONS_SENTINEL_X, FRAME_DIMENSIONS_SENTINEL_Y,
  decodePalRle, PalRleDecodeError,
} from './decoder.js';
import { TileRenderer } from './renderer.js';
import { PaletteCache } from './palette.js';
import { ParityRecovery } from './fec';
import { LossTracker } from './feedback';
import { AckBatcher } from './ack';

const statusEl = document.getElementById('status')!;
const logEl = document.getElementById('log')!;
const canvasEl = document.getElementById('canvas') as HTMLCanvasElement;

function log(msg: string) {
  const line = document.createElement('div');
  line.textContent = msg;
  logEl.appendChild(line);
  // Keep log from growing unbounded
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
  // The `host` parameter is expected to already include the port, because
  // the E2E test's loopback UDP forwarder binds to 127.0.0.1:<random>. If no
  // host is passed, fall back to the production tailnet hostname + default
  // port so a developer can load the page manually against a running xdaemon.
  const url = new URL(window.location.href);
  const serverHost = url.searchParams.get('host') ?? 'ghostframe-server:4443';
  const certHash = url.searchParams.get('certHash') ?? '';

  const wtUrl = `https://${serverHost}/`;

  log(`Connecting to ${wtUrl}...`);

  // Per-session palette storage for PalRle codec — declared early so the
  // transport.closed callback (registered below) can close over it safely.
  const paletteCache = new PaletteCache();

  let transport: WebTransport;
  if (certHash) {
    transport = new WebTransport(wtUrl, {
      serverCertificateHashes: [{ algorithm: 'sha-256', value: hexToBuffer(certHash) }],
    });
  } else {
    transport = new WebTransport(wtUrl);
  }

  // Log close reason if the connection fails before ready.
  transport.closed.then(
    (info) => {
      log(`Transport closed: code=${info.closeCode} reason=${info.reason}`);
      paletteCache.clear();
    },
    (err) => {
      log(`Transport closed with error: ${err}`);
      paletteCache.clear();
    }
  );

  await transport.ready;
  log('Connected!');
  statusEl.textContent = 'Connected';

  const parityMap = new Map<string, ParityRecovery>();
  const lossTracker = new LossTracker();

  // Send periodic receiver feedback on a bidi stream
  const feedbackWriter = await (async () => {
    try {
      const bidi = await transport.createBidirectionalStream();
      return bidi.writable.getWriter();
    } catch {
      console.warn('Could not open feedback stream');
      return null;
    }
  })();

  if (feedbackWriter) {
    setInterval(async () => {
      try {
        const msg = lossTracker.encodeFeedback();
        await feedbackWriter.write(msg);
      } catch {
        // Stream closed — stop reporting
      }
    }, 100); // Every 100ms per spec
  }

  const renderer = new TileRenderer(canvasEl);
  // Canvas starts at 0×0 and grows as tiles arrive.  Starting at a non-zero
  // "default" size prevents the canvas from ever shrinking to the actual
  // frame dimensions (tile pipeline uses Math.max), which breaks resolution-
  // change detection in tests.
  renderer.resize(0, 0);

  // Per-tile H.264 decoders
  const h264Decoders = new Map<string, H264TileDecoder>();

  // Full-frame decoder and reassembly state
  let fullFrameDecoder: FullFrameDecoder | null = null;
  const frameAssemblies = new Map<string, FrameAssembly>();
  let latestFullFrameSeq = 0;

  function getH264Decoder(tileX: number, tileY: number): H264TileDecoder {
    const key = `${tileX}:${tileY}`;
    let dec = h264Decoders.get(key);
    if (!dec) {
      dec = new H264TileDecoder((frame: VideoFrame) => {
        renderer.drawVideoFrame(tileX, tileY, frame);
      });
      h264Decoders.set(key, dec);
    }
    return dec;
  }

  // Batched ACK sender — fire-and-forget unreliable datagrams back to server.
  const ackWriter = transport.datagrams.writable.getWriter();
  const ackBatcher = new AckBatcher((dg) => {
    // Fire-and-forget write; failure is acceptable (ACKs are unreliable).
    ackWriter.write(dg).catch(() => {});
  });

  // Tile assembly state: key -> TileAssembly
  const assemblies = new Map<string, TileAssembly>();
  let latestFrameSeq = 0;
  let firstTileRendered = false;
  // Set to true once the frame-dimensions sentinel has been processed.
  // The per-tile fallback resize is only a safety net for pre-sentinel tiles;
  // once dimensions are known, the fallback must not fire because edge-tile
  // coordinates (e.g. tX=21 in a 700px-wide frame) would compute
  // (tX+1)*TILE_SIZE = 704 > 700 and spuriously clear the canvas every frame.
  let frameDimensionsKnown = false;

  /** Reassemble completed tile and decode/render it. */
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

    // Frame-dimensions control message — sentinel tile coords (0xFF, 0xFF)
    // identify a tile-shaped datagram that actually carries (width, height).
    // The server pre-sizes the canvas this way so tiles arrive after the
    // canvas is at final size, avoiding the canvas-resize-clears-tiles bug.
    if (tX === FRAME_DIMENSIONS_SENTINEL_X && tY === FRAME_DIMENSIONS_SENTINEL_Y) {
      if (payload.byteLength >= 8) {
        const view = new DataView(payload.buffer, payload.byteOffset, 8);
        const w = view.getUint32(0, false); // big-endian
        const h = view.getUint32(4, false);
        renderer.resize(w, h);
        frameDimensionsKnown = true;
      }
      return;
    }

    // Fallback: expand canvas to fit this tile if we haven't yet received the
    // frame-dimensions sentinel. Once dimensions are known, skip this entirely —
    // edge tiles compute (tX+1)*TILE_SIZE which can exceed the actual frame
    // width (e.g., tX=21 → 704 in a 700-px-wide frame), causing a spurious
    // resize that clears the canvas on every frame containing that edge tile.
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

    if (asm.header.codec === Codec.Raw) {
      renderer.drawRawTile(tX, tY, payload);
    } else if (asm.header.codec === Codec.H264) {
      const dec = getH264Decoder(tX, tY);
      dec.decode(payload);
    } else if (asm.header.codec === Codec.Solid) {
      // 4-byte BGRA payload.
      if (payload.byteLength === 4) {
        renderer.drawSolidTile(tX, tY, payload);
      }
    } else if (asm.header.codec === Codec.PalRle) {
      try {
        const { bgra } = decodePalRle(payload, paletteCache);
        renderer.drawPalRleTile(tX, tY, bgra);
      } catch (e) {
        if (e instanceof PalRleDecodeError) {
          console.warn("[palrle]", e.message);
        } else {
          throw e;
        }
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

    // Acknowledge this tile completion for the server's scheduler.
    ackBatcher.add({
      tileX: tX,
      tileY: tY,
      generation: asm.header.generation,
      pass: asm.header.pass,
    });
  }

  // Test-instrumentation counters. Polled by `e2e_mode_switch` via Playwright.
  // Reset on every new session so cumulative counts match the test's window.
  // Type comes from src/globals.d.ts.
  window.__ghostframeStats = { tileDatagrams: 0, frameDatagrams: 0 };
  const stats = window.__ghostframeStats;

  // Receive datagrams
  const reader = transport.datagrams.readable.getReader();
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;

    if (!value || value.byteLength === 0) continue;

    // Backward compat: small text datagrams (ping/pong)
    if (value.byteLength < 20) {
      const text = new TextDecoder().decode(value);
      log(`Received: ${text} (${value.byteLength} bytes)`);
      if (text === 'pong') {
        statusEl.textContent = 'Ping/Pong successful!';
        log('M0 COMPLETE: Ping/Pong datagram round-trip verified!');
      }
      continue;
    }

    // Must have at least datagram header + tile header
    if (value.byteLength < DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE) {
      log(`Datagram too short: ${value.byteLength} bytes`);
      continue;
    }

    const view = new DataView(value.buffer, value.byteOffset, value.byteLength);

    // Dispatch: frame-level (bit 31 = 0) or tile-level (bit 31 = 1)
    if (!isTileDatagram(view, 0)) {
      // Frame-level datagram
      if (value.byteLength < FRAME_HEADER_SIZE) continue;

      const frameHdr = decodeFrameHeader(view, 0);
      lossTracker.onDatagram();
      stats.frameDatagrams++;

      // Stale frame discard
      if (frameHdr.frameSeq < latestFullFrameSeq - 2) continue;
      if (frameHdr.frameSeq > latestFullFrameSeq) {
        latestFullFrameSeq = frameHdr.frameSeq;
      }

      // Evict stale frame assemblies
      for (const [k, asm] of frameAssemblies) {
        const seq = parseInt(k.split(':')[1], 10);
        if (seq < latestFullFrameSeq - 2) {
          if (asm.received < asm.fragments.length) {
            lossTracker.onStaleTile(asm.fragments.length, asm.received);
          }
          frameAssemblies.delete(k);
        }
      }

      // Parity datagrams (frag_idx >= frag_total)
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
            renderer.drawFullFrame(frame);
          }, 1920, 1080);
        }

        fullFrameDecoder.decode(payload, asm.header.isKeyframe);

        if (!firstTileRendered) {
          firstTileRendered = true;
          log(`First full frame: ${payload.byteLength}B ${asm.header.isKeyframe ? '(keyframe)' : ''}`);
          statusEl.textContent = 'Receiving frames';
        }
      }

      continue; // Don't fall through to tile processing
    }

    // --- Tile-level datagram processing ---
    const dgramHdr = decodeDatagramHeader(view, 0);
    dgramHdr.frameSeq = dgramHdr.frameSeq & ~TILE_DATAGRAM_FLAG;
    lossTracker.onDatagram();
    stats.tileDatagrams++;
    const tileHdr = decodeTileHeader(view, DATAGRAM_HEADER_SIZE);

    // Track the latest frame sequence number
    if (dgramHdr.frameSeq > latestFrameSeq) {
      latestFrameSeq = dgramHdr.frameSeq;
    }

    // Evict stale assemblies (frames more than 2 behind the latest)
    const staleThreshold = latestFrameSeq - 2;
    for (const [k, asm] of assemblies) {
      const seq = parseInt(k.split(':')[0], 10);
      if (seq < staleThreshold) {
        // Count missing fragments as lost datagrams
        if (asm.received < asm.fragments.length) {
          lossTracker.onStaleTile(asm.fragments.length, asm.received);
        }
        assemblies.delete(k);
        parityMap.delete(k);
      }
    }

    // Parity datagram: store for potential recovery (must precede Skip check)
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

      // Parity arrived — check if a pending assembly can now recover
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
            // Assembly now complete — reassemble and render
            finishAssembly(asmKey, pendingAsm);
          }
        }
      }
      continue; // Don't process as a source fragment
    }

    // Frame-dimensions control message — sentinel tile coords (0xFF, 0xFF) with
    // codec Skip and an 8-byte BE [width, height] payload. Must be handled before
    // the generic Skip guard below (which would silently discard the payload).
    if (tileHdr.tileX === FRAME_DIMENSIONS_SENTINEL_X && tileHdr.tileY === FRAME_DIMENSIONS_SENTINEL_Y) {
      const payloadStart = DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE;
      const payloadBytes = value.byteLength - payloadStart;
      if (payloadBytes >= 8) {
        const dimView = new DataView(value.buffer, value.byteOffset + payloadStart, 8);
        const w = dimView.getUint32(0, false); // big-endian
        const h = dimView.getUint32(4, false);
        renderer.resize(w, h);
        frameDimensionsKnown = true;
      }
      continue;
    }

    // Skip codec: tile unchanged, canvas retains last content
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

    // Store this fragment if not already received
    if (asm.fragments[dgramHdr.fragIdx] === null) {
      asm.fragments[dgramHdr.fragIdx] = fragData.slice(); // copy
      asm.received += 1;
    }

    // Attempt FEC recovery if we have almost all fragments
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

    // Check if all fragments have arrived
    if (asm.received === dgramHdr.fragTotal) {
      finishAssembly(key, asm);
    }
  }
}

main().catch((e) => {
  log(`Error: ${e.message}`);
  statusEl.textContent = `Error: ${e.message}`;
});
