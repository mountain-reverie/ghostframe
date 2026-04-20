import {
  DATAGRAM_HEADER_SIZE,
  TILE_HEADER_SIZE,
  TILE_SIZE,
  Codec,
  decodeDatagramHeader,
  decodeTileHeader,
  tileKey,
  TileAssembly,
  H264TileDecoder,
} from './decoder';
import { TileRenderer } from './renderer';

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
    (info) => log(`Transport closed: code=${info.closeCode} reason=${info.reason}`),
    (err) => log(`Transport closed with error: ${err}`)
  );

  await transport.ready;
  log('Connected!');
  statusEl.textContent = 'Connected';

  const renderer = new TileRenderer(canvasEl);
  // Default canvas size; will grow as tiles arrive
  renderer.resize(1280, 720);

  // Per-tile H.264 decoders
  const h264Decoders = new Map<string, H264TileDecoder>();

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

  // Tile assembly state: key -> TileAssembly
  const assemblies = new Map<string, TileAssembly>();
  let latestFrameSeq = 0;
  let firstTileRendered = false;

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
    const dgramHdr = decodeDatagramHeader(view, 0);
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
        assemblies.delete(k);
      }
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

    // Check if all fragments have arrived
    if (asm.received === dgramHdr.fragTotal) {
      assemblies.delete(key);

      // Reassemble payload
      const totalLen = asm.fragments.reduce((acc, f) => acc + (f ? f.byteLength : 0), 0);
      const payload = new Uint8Array(totalLen);
      let offset = 0;
      for (const frag of asm.fragments) {
        if (frag) {
          payload.set(frag, offset);
          offset += frag.byteLength;
        }
      }

      // Ensure canvas is large enough
      const minWidth = (tileHdr.tileX + 1) * TILE_SIZE;
      const minHeight = (tileHdr.tileY + 1) * TILE_SIZE;
      if (canvasEl.width < minWidth || canvasEl.height < minHeight) {
        renderer.resize(
          Math.max(canvasEl.width, minWidth),
          Math.max(canvasEl.height, minHeight)
        );
      }

      // Decode based on codec
      if (asm.header.codec === Codec.Raw) {
        renderer.drawRawTile(tileHdr.tileX, tileHdr.tileY, payload);
      } else if (asm.header.codec === Codec.H264) {
        const dec = getH264Decoder(tileHdr.tileX, tileHdr.tileY);
        dec.decode(payload);
      }

      if (!firstTileRendered) {
        firstTileRendered = true;
        // Log tile diagnostic for E2E debugging
        const sample = Array.from(payload.slice(0, 16))
          .map(b => b.toString(16).padStart(2, '0'))
          .join(' ');
        log(`First tile: (${tileHdr.tileX},${tileHdr.tileY}) ${payload.byteLength}B`);
        log(`First bytes: ${sample}`);
        statusEl.textContent = 'Receiving frames';
      }
    }
  }
}

main().catch((e) => {
  log(`Error: ${e.message}`);
  statusEl.textContent = `Error: ${e.message}`;
});
