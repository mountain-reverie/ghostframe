export const DATAGRAM_HEADER_SIZE = 16;
export const TILE_HEADER_SIZE = 8;
export const TILE_SIZE = 32;

/**
 * Sentinel tile coordinates marking a "tile datagram" that actually carries
 * a frame-dimensions message instead of pixel data. Tile coords are u8;
 * 0xFF (255) is structurally impossible at any sensible resolution
 * (would imply a screen ≥ 8000 px wide), so the receiver routes on the sentinel.
 *
 * Payload: 8 bytes — `[width: u32 BE][height: u32 BE]`.
 */
export const FRAME_DIMENSIONS_SENTINEL_X = 0xFF;
export const FRAME_DIMENSIONS_SENTINEL_Y = 0xFF;

export const enum Codec {
  Skip = 0, H264 = 1, PalRle = 2, Solid = 3, Raw = 4, Cdf53 = 5,
}

export interface DatagramHeader {
  frameSeq: number;
  fragIdx: number;
  fragTotal: number;
  /**
   * Per-QUIC-session monotonic FEC group key, assigned at server emit time.
   * 0 means "not yet stamped" by the emitter (e.g. on synthetic test wire);
   * production tile datagrams always carry a non-zero value. u32 wrap is
   * benign because the dedupe window is short.
   */
  wireSeq: number;
  timestampUs: number;
}

export interface TileHeader {
  tileX: number;
  tileY: number;
  codec: Codec;
  lz4: boolean;
  generation: number; // 4 bits effective (0..=15)
  pass: number;       // 4 bits effective (0..=15)
  payloadLen: number;
}

export function decodeDatagramHeader(view: DataView, offset: number): DatagramHeader {
  return {
    frameSeq: view.getUint32(offset, false),      // big-endian
    fragIdx: view.getUint16(offset + 4, false),
    fragTotal: view.getUint16(offset + 6, false),
    wireSeq: view.getUint32(offset + 8, false),
    timestampUs: view.getUint32(offset + 12, false),
  };
}

export function decodeTileHeader(view: DataView, offset: number): TileHeader {
  const codecLz4 = view.getUint8(offset + 2);
  return {
    tileX: view.getUint8(offset),
    tileY: view.getUint8(offset + 1),
    codec: (codecLz4 >> 1) as Codec,
    lz4: (codecLz4 & 1) !== 0,
    generation: view.getUint8(offset + 3) >> 4,
    pass: view.getUint8(offset + 3) & 0x0F,
    payloadLen: view.getUint32(offset + 4, false),
  };
}

/**
 * Per-tile-pass assembly / parity-map key. Includes `passIdx` because the
 * server emits all 14 CDF53 passes for a single dirty re-encode under
 * the SAME wire `frame_seq` — distinguished only by the tile header's
 * `pass` field. Keying without passIdx caused different-pass fragments
 * to collide in the same assembly bucket: whichever frag_idx slot
 * landed first won, and `finishAssembly` returned a Frankenstein
 * payload mixing bytes from multiple passes. That mismatch surfaced as
 * ERR_CDF53_TRUNCATED / ERR_CDF53_RLE_LENGTH on prevalidation.
 *
 * The frameSeq still comes first in the string so the stale-assembly
 * eviction sweep (which `parseInt(k.split(':')[0], 10)`s) keeps working.
 */
export function tileKey(frameSeq: number, tileX: number, tileY: number, passIdx: number): string {
  return `${frameSeq}:${tileX}:${tileY}:${passIdx}`;
}

export interface TileAssembly {
  header: TileHeader;
  fragments: (Uint8Array | null)[];
  received: number;
  /** NACK key derived from the assembly's source identifiers. */
  emitKey: { frameSeq: number; tileX: number; tileY: number; passIdx: number };
  /** performance.now() of the first fragment received; does not reset. */
  partialSince: number;
  /** Dedup set so the rAF timeout scan NACKs each missing fragment only once. */
  nackedFragIdxs: Set<number>;
}

// ---------------------------------------------------------------------------
// Frame-level protocol (full-frame H.264)
// ---------------------------------------------------------------------------

export const FRAME_HEADER_SIZE = 14;
export const TILE_DATAGRAM_FLAG = 0x80000000;

export interface FrameHeader {
  frameSeq: number;
  fragIdx: number;
  fragTotal: number;
  timestampUs: number;
  isKeyframe: boolean;
}

export function isTileDatagram(view: DataView, offset: number): boolean {
  const firstU32 = view.getUint32(offset, false);
  return (firstU32 & TILE_DATAGRAM_FLAG) !== 0;
}

export function decodeFrameHeader(view: DataView, offset: number): FrameHeader {
  return {
    frameSeq: view.getUint32(offset, false) & ~TILE_DATAGRAM_FLAG,
    fragIdx: view.getUint16(offset + 4, false),
    fragTotal: view.getUint16(offset + 6, false),
    timestampUs: view.getUint32(offset + 8, false),
    isKeyframe: (view.getUint8(offset + 12) & 1) !== 0,
  };
}

export function frameKey(frameSeq: number): string {
  return `frame:${frameSeq}`;
}

export interface FrameAssembly {
  header: FrameHeader;
  fragments: (Uint8Array | null)[];
  received: number;
}

export class FullFrameDecoder {
  private decoder: VideoDecoder;
  private latestFrame: VideoFrame | null = null;

  constructor(
    private onFrame: (frame: VideoFrame) => void,
    width: number,
    height: number,
  ) {
    this.decoder = new VideoDecoder({
      output: (frame: VideoFrame) => {
        // M3.2b: the rAF tick is the sole closer of consumed VideoFrames.
        // Do NOT auto-close the previous frame here.
        this.latestFrame = frame;
        this.onFrame(frame);
      },
      error: (e: DOMException) => {
        console.error('Full-frame H264 decode error:', e.message);
      },
    });

    this.decoder.configure({
      codec: 'avc1.42001e',
      codedWidth: width,
      codedHeight: height,
      optimizeForLatency: true,
    });
  }

  decode(nalData: Uint8Array, isKeyframe: boolean) {
    if (this.decoder.state === 'closed') return;
    const chunk = new EncodedVideoChunk({
      type: isKeyframe ? 'key' : 'delta',
      timestamp: 0,
      data: nalData,
    });
    this.decoder.decode(chunk);
  }

  close() {
    if (this.decoder.state !== 'closed') {
      this.decoder.close();
    }
    if (this.latestFrame) {
      this.latestFrame.close();
      this.latestFrame = null;
    }
  }
}

