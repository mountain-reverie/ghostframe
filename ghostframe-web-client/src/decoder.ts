export const DATAGRAM_HEADER_SIZE = 12;
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
  Skip = 0, H264 = 1, PalRle = 2, Bc1 = 3, Solid = 4, Raw = 5, Cdf53 = 6,
}

export interface DatagramHeader {
  frameSeq: number;
  fragIdx: number;
  fragTotal: number;
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
    timestampUs: view.getUint32(offset + 8, false),
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

export function tileKey(frameSeq: number, tileX: number, tileY: number): string {
  return `${frameSeq}:${tileX}:${tileY}`;
}

export interface TileAssembly {
  header: TileHeader;
  fragments: (Uint8Array | null)[];
  received: number;
}

/**
 * Per-tile H.264 decoder using WebCodecs VideoDecoder.
 * Each tile position gets its own decoder instance to maintain inter-frame state.
 */
export class H264TileDecoder {
  private decoder: VideoDecoder;
  private latestFrame: VideoFrame | null = null;

  constructor(private onFrame: (frame: VideoFrame) => void) {
    this.decoder = new VideoDecoder({
      output: (frame: VideoFrame) => {
        // M3.2b: the rAF tick is the sole closer of consumed VideoFrames.
        // Do NOT auto-close the previous frame here.
        this.latestFrame = frame;
        this.onFrame(frame);
      },
      error: (e: DOMException) => {
        console.error('H264 decode error:', e.message);
      },
    });

    // The coded dimensions may be larger than the tile size when VA-API
    // pads to the hardware minimum (e.g. 128x128 on AMD).  WebCodecs will
    // read the actual dimensions from the SPS, but we still need a
    // reasonable initial hint.  Using 128 covers the common VA-API case
    // while libx264 at 32x32 also works (decoder auto-adjusts from SPS).
    this.decoder.configure({
      codec: 'avc1.42001e', // Baseline profile, level 3.0
      codedWidth: 128,
      codedHeight: 128,
      optimizeForLatency: true,
    });
  }

  decode(nalData: Uint8Array) {
    if (this.decoder.state === 'closed') return;

    const isKey = this.isKeyframe(nalData);

    const chunk = new EncodedVideoChunk({
      type: isKey ? 'key' : 'delta',
      timestamp: 0,
      data: nalData,
    });

    this.decoder.decode(chunk);
  }

  private isKeyframe(data: Uint8Array): boolean {
    // Scan for NAL start code (0x00 0x00 0x01 or 0x00 0x00 0x00 0x01)
    // then check NAL type: 5 = IDR slice, 7 = SPS (precedes keyframe)
    for (let i = 0; i < data.length - 4; i++) {
      if (data[i] === 0 && data[i + 1] === 0) {
        let nalStart: number;
        if (data[i + 2] === 1) {
          nalStart = i + 3;
        } else if (data[i + 2] === 0 && data[i + 3] === 1) {
          nalStart = i + 4;
        } else {
          continue;
        }
        if (nalStart < data.length) {
          const nalType = data[nalStart] & 0x1f;
          if (nalType === 5 || nalType === 7) return true;
        }
      }
    }
    return false;
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
        if (this.latestFrame) {
          this.latestFrame.close();
        }
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

